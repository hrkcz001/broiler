//! The local IPC endpoint.
//!
//! Naming and access control follow the documented hardening rules: the endpoint name carries a hash
//! of the logon SID, which both routes a client to the coordinator of its own logon session and keeps
//! another terminal session out; the pipe rejects remote clients; the first instance is created with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` so an unexpected existing server is detected instead of joined; and
//! the DACL grants only the logon SID, never `Everyone` and never a null DACL.
//!
//! Endpoint naming is routing, not authentication: a process already inside the same logon session is
//! inside this boundary by construction.

use std::ffi::c_void;
use std::io;
use std::time::Instant;

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
};
use windows_sys::Win32::Security::{
    AddAccessAllowedAce, GetLengthSid, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
    SetSecurityDescriptorDacl, ACL, ACL_REVISION, SECURITY_DESCRIPTOR, TOKEN_GROUPS, TOKEN_QUERY,
    TokenLogonSid,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::ipc::{decode_frame, encode_frame, Request, Response};
use crate::win::process::{self, Handle};

const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

/// A local endpoint: the pipe name plus the security descriptor that protects it.
pub struct Endpoint {
    pub name: String,
    /// Backing storage for the descriptor and its ACL; the pointer handed to `CreateNamedPipeW` must
    /// stay valid for the lifetime of this value.
    _descriptor: Box<SECURITY_DESCRIPTOR>,
    _acl: Vec<u8>,
    attributes: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
}

// The descriptor and its ACL are owned by this value and move with it; nothing is shared and no
// thread reads those buffers concurrently. Windows consumes them during `CreateNamedPipeW`, which
// runs on whichever thread owns the endpoint, so moving the endpoint to the serving thread is sound.
// The declaration exists so the daemon can live on a worker thread and leave the UI thread free for
// the tray message loop.
unsafe impl Send for Endpoint {}

impl Endpoint {
    fn attributes(&self) -> *const windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        &self.attributes
    }

    /// Reuse this endpoint's security descriptor under a different pipe name.
    ///
    /// Only tests need this: it keeps concurrent test cases from colliding on the real endpoint.
    pub fn with_name(self, name: &str) -> Endpoint {
        Endpoint {
            name: name.to_string(),
            _descriptor: self._descriptor,
            _acl: self._acl,
            attributes: self.attributes,
        }
    }
}

fn last_error(call: &str) -> io::Error {
    let code = unsafe { GetLastError() };
    io::Error::new(
        io::ErrorKind::Other,
        format!("{call} failed with Win32 error {code}"),
    )
}

/// Read the logon SID of this process and hash it into an endpoint token.
fn logon_sid_token() -> io::Result<(String, Vec<u8>)> {
    let mut token: HANDLE = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let token = Handle::from_raw(token);

    let mut needed: u32 = 0;
    unsafe {
        GetTokenInformation(
            token.raw(),
            TokenLogonSid,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(last_error("GetTokenInformation(size)"));
    }

    let mut buffer = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token.raw(),
            TokenLogonSid,
            buffer.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(last_error("GetTokenInformation(TokenLogonSid)"));
    }

    // TOKEN_GROUPS is a variable-length struct: at least one entry for a logon SID.
    let groups = unsafe { &*(buffer.as_ptr() as *const TOKEN_GROUPS) };
    if groups.GroupCount == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "the process token carries no logon SID",
        ));
    }
    let sid = groups.Groups[0].Sid;
    let length = unsafe { GetLengthSid(sid) } as usize;
    if length == 0 {
        return Err(last_error("GetLengthSid"));
    }
    let bytes = unsafe { std::slice::from_raw_parts(sid as *const u8, length) }.to_vec();

    let mut hasher = Sha256::new();
    hasher.update(b"broiler.endpoint.v1|");
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let token: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    Ok((token, bytes))
}

/// Build the endpoint for this logon session.
///
/// This is the only endpoint a production process can reach. A test binary can point its own thread
/// at a fixture namespace through [`fixture`], which is compiled out of every non-test build.
pub fn local_endpoint() -> io::Result<Endpoint> {
    #[cfg(test)]
    if let Some(name) = fixture::current() {
        let (_, sid_bytes) = logon_sid_token()?;
        return endpoint_named(name, &sid_bytes);
    }
    let (token, sid_bytes) = logon_sid_token()?;
    endpoint_named(format!("\\\\.\\pipe\\broiler.v1.{token}"), &sid_bytes)
}

/// Build one endpoint with an explicit pipe name and the DACL of this logon session.
///
/// The name is the whole identity of the endpoint: the pipe, and the instance mutex derived from it,
/// are both functions of this string. Test fixtures therefore get their own namespace without a
/// production code path that could name a second one.
fn endpoint_named(name: String, sid_bytes: &[u8]) -> io::Result<Endpoint> {
    // A DACL granting only this logon SID read and write access.
    let mut descriptor: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
    let ok = unsafe {
        InitializeSecurityDescriptor(
            descriptor.as_mut() as *mut SECURITY_DESCRIPTOR as *mut c_void,
            1,
        )
    };
    if ok == 0 {
        return Err(last_error("InitializeSecurityDescriptor"));
    }

    let mut acl: Vec<u8> = vec![0u8; 128];
    let ok = unsafe {
        InitializeAcl(
            acl.as_mut_ptr() as *mut ACL,
            acl.len() as u32,
            ACL_REVISION as u32,
        )
    };
    if ok == 0 {
        return Err(last_error("InitializeAcl"));
    }

    // The ACE copies the SID, so an aligned local copy is enough; it must outlive the call only.
    let mut sid_storage: Vec<u32> = vec![0u32; sid_bytes.len().div_ceil(4)];
    if sid_bytes.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the logon SID has an unexpected shape",
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            sid_bytes.as_ptr(),
            sid_storage.as_mut_ptr() as *mut u8,
            sid_bytes.len(),
        );
    }
    let sid = sid_storage.as_mut_ptr() as *mut c_void;

    let ok = unsafe {
        AddAccessAllowedAce(
            acl.as_mut_ptr() as *mut ACL,
            ACL_REVISION as u32,
            GENERIC_READ_WRITE,
            sid,
        )
    };
    if ok == 0 {
        return Err(last_error("AddAccessAllowedAce"));
    }

    let ok = unsafe {
        SetSecurityDescriptorDacl(
            descriptor.as_mut() as *mut SECURITY_DESCRIPTOR as *mut c_void,
            1,
            acl.as_ptr() as *const ACL,
            0,
        )
    };
    if ok == 0 {
        return Err(last_error("SetSecurityDescriptorDacl"));
    }

    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_mut() as *mut SECURITY_DESCRIPTOR as *mut c_void,
        bInheritHandle: 0,
    };

    Ok(Endpoint {
        name,
        _descriptor: descriptor,
        _acl: acl,
        attributes,
    })
}

fn wide(text: &str) -> Vec<u16> {
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    buffer.push(0);
    buffer
}

/// One server-side pipe instance.
pub struct ServerConnection {
    handle: Handle,
}

impl ServerConnection {
    /// Create one instance. `first` adds `FILE_FLAG_FIRST_PIPE_INSTANCE`, which fails when another
    /// server already owns the name.
    pub fn create(endpoint: &Endpoint, first: bool) -> io::Result<ServerConnection> {
        let name = wide(&endpoint.name);
        let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                open_mode,
                pipe_mode,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                endpoint.attributes(),
            )
        };
        // One owner: a temporary wrapper here would close the handle as soon as it is dropped.
        let handle = Handle::from_raw(handle);
        if !handle.is_valid() {
            return Err(last_error("CreateNamedPipeW"));
        }
        Ok(ServerConnection { handle })
    }

    /// Wait for a client. Returns false when the connection was already satisfied.
    pub fn connect(&self) -> io::Result<bool> {
        let ok = unsafe { ConnectNamedPipe(self.handle.raw(), std::ptr::null_mut()) };
        if ok != 0 {
            return Ok(true);
        }
        match unsafe { GetLastError() } {
            ERROR_PIPE_CONNECTED => Ok(false),
            _ => Err(last_error("ConnectNamedPipe")),
        }
    }

    pub fn read_request(&self, deadline: Instant) -> io::Result<Option<Request>> {
        match process::read_frame_deadline(self.handle.raw(), deadline)? {
            None => Ok(None),
            Some(frame) => decode_frame(&frame)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        }
    }

    pub fn write_response(&self, response: &Response) -> io::Result<()> {
        let frame = encode_frame(response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        process::write_all(self.handle.raw(), &frame)?;
        process::flush(self.handle.raw())
    }

    pub fn write_raw(&self, bytes: &[u8]) -> io::Result<()> {
        process::write_all(self.handle.raw(), bytes)
    }

    pub fn disconnect(&self) {
        unsafe {
            DisconnectNamedPipe(self.handle.raw());
        }
    }
}

/// The client side of the endpoint.
pub struct Client {
    handle: Handle,
}

impl Client {
    /// Connect, retrying while the server is busy, until the deadline.
    pub fn connect(endpoint_name: &str, deadline: Instant) -> io::Result<Client> {
        let name = wide(endpoint_name);
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            let candidate = Handle::from_raw(handle);
            if candidate.is_valid() {
                return Ok(Client { handle: candidate });
            }
            let error = unsafe { GetLastError() };
            if error == 2 {
                // ERROR_FILE_NOT_FOUND: no server has ever created this endpoint.
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no broiler coordinator endpoint exists in this session",
                ));
            }
            if error != ERROR_PIPE_BUSY {
                return Err(last_error("CreateFileW"));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no broiler coordinator answered on the local endpoint",
                ));
            }
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(100) as u32;
            unsafe {
                WaitNamedPipeW(name.as_ptr(), wait_ms);
            }
        }
    }

    /// One request, one response. The CLI uses one connection per command.
    pub fn request(&self, request: &Request, deadline: Instant) -> io::Result<Response> {
        let frame = encode_frame(request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        process::write_all(self.handle.raw(), &frame)?;
        let response = process::read_frame_deadline(self.handle.raw(), deadline)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the coordinator closed the pipe"))?;
        decode_frame(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }

    /// Send bytes verbatim, used by tests that must exercise malformed input.
    pub fn send_raw(&self, bytes: &[u8]) -> io::Result<()> {
        process::write_all(self.handle.raw(), bytes)
    }

    pub fn read_raw_response(&self, deadline: Instant) -> io::Result<Option<Vec<u8>>> {
        process::read_frame_deadline(self.handle.raw(), deadline)
    }
}

/// Render the DACL of a live pipe instance as SDDL.
///
/// This is the check that can actually be made on a single-session machine: it shows which principal
/// the kernel holds in the pipe's access control list. It proves the DACL is scoped and that no
/// `Everyone` or anonymous entry exists; it does not simulate a second logon session.
pub fn dacl_sddl(connection: &ServerConnection) -> io::Result<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            connection.handle.raw(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("GetSecurityInfo failed with Win32 error {status}"),
        ));
    }

    let mut text: *mut u16 = std::ptr::null_mut();
    let mut length: u32 = 0;
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            &mut length,
        )
    };
    if ok == 0 {
        unsafe { LocalFree(descriptor as *mut c_void) };
        return Err(last_error("ConvertSecurityDescriptorToStringSecurityDescriptorW"));
    }

    let mut end = 0usize;
    unsafe {
        while *text.add(end) != 0 {
            end += 1;
        }
    }
    let sddl = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(text, end)) };
    unsafe {
        LocalFree(text as *mut c_void);
        LocalFree(descriptor as *mut c_void);
    }
    Ok(sddl)
}

/// True when a coordinator already owns this endpoint.
pub fn server_exists(endpoint_name: &str) -> bool {
    matches!(Client::connect(endpoint_name, Instant::now() + std::time::Duration::from_millis(50)), Ok(_))
}

/// A private endpoint namespace for tests, compiled only into a test build.
///
/// The product must have exactly one endpoint per logon session, and a test must not be able to reach
/// it, be reached by it, or disturb a running owner. Both properties come from the same fact: this
/// module exists only under `cfg(test)`, so no shipped binary contains a way to choose another
/// namespace, and a fixture name can never equal the logon-SID token the product derives.
#[cfg(test)]
pub mod fixture {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        /// The fixture endpoint this thread's `local_endpoint` resolves to, if any.
        static NAMESPACE: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh namespace, unique inside this process and marked as a fixture.
    pub fn namespace(label: &str) -> String {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!(
            "\\\\.\\pipe\\broiler.v1.fixture.{label}.{}.{count}",
            std::process::id()
        )
    }

    /// A namespace shared by one test binary, for cases that only need "somewhere else".
    pub fn stable(label: &str) -> String {
        format!(
            "\\\\.\\pipe\\broiler.v1.fixture.{label}.{}.stable",
            std::process::id()
        )
    }

    /// Point this thread at `name` for the duration of `body`.
    pub fn with_namespace<R>(name: &str, body: impl FnOnce() -> R) -> R {
        NAMESPACE.with(|slot| *slot.borrow_mut() = Some(name.to_string()));
        let result = body();
        NAMESPACE.with(|slot| *slot.borrow_mut() = None);
        result
    }

    /// The fixture endpoint this thread currently resolves to.
    pub fn current() -> Option<String> {
        NAMESPACE.with(|slot| slot.borrow().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_endpoint_name_is_stable_and_scoped_to_the_logon_sid() {
        let first = local_endpoint().expect("endpoint");
        let second = local_endpoint().expect("endpoint");
        assert_eq!(first.name, second.name);
        assert!(first.name.starts_with("\\\\.\\pipe\\broiler.v1."));
        // The token is a hash, not the SID itself.
        assert_eq!(first.name.rsplit('.').next().map(str::len), Some(16));
    }

    #[test]
    fn the_pipe_dacl_names_only_the_logon_sid() {
        let endpoint = local_endpoint().expect("endpoint");
        let unique = format!("{}.sddl{}", endpoint.name, std::process::id());
        let server = ServerConnection::create(&endpoint.with_name(&unique), true).expect("server");

        let sddl = dacl_sddl(&server).expect("sddl");
        // Exactly one access-allowed ACE, and it names the logon SID rather than a group.
        assert_eq!(
            sddl.matches("(A;;").count(),
            1,
            "expected exactly one ACE: {sddl}"
        );
        assert!(sddl.contains("S-1-5-5-"), "expected a logon SID ACE: {sddl}");
        // No Everyone (WD), Anonymous (AN), Authenticated Users (AU) or Users (BU) grant.
        for trustee in [";WD;", ";AN;", ";AU;", ";BU;", ";OW;"] {
            assert!(!sddl.contains(trustee), "{trustee} must not appear: {sddl}");
        }
        // Not a null DACL: a null DACL renders as "D:NO_ACCESS_CONTROL".
        assert!(!sddl.contains("NO_ACCESS_CONTROL"), "null DACL: {sddl}");
        println!("pipe DACL: {sddl}");
    }

    #[test]
    fn first_instance_flag_detects_an_existing_server() {
        let endpoint = local_endpoint().expect("endpoint");
        let unique = format!("{}.test{}", endpoint.name, std::process::id());
        // The public name is used by a real daemon; the unique suffix keeps this test isolated.
        let server = ServerConnection::create(&shared_endpoint(&unique), true).expect("first instance");
        let second = ServerConnection::create(&shared_endpoint(&unique), true);
        assert!(second.is_err(), "a second first-instance must fail");
        drop(server);
    }

    /// Build an endpoint value with a different name but the same security descriptor.
    fn shared_endpoint(name: &str) -> Endpoint {
        let endpoint = local_endpoint().expect("endpoint");
        Endpoint {
            name: name.to_string(),
            _descriptor: endpoint._descriptor,
            _acl: endpoint._acl,
            attributes: endpoint.attributes,
        }
    }

    #[test]
    fn a_request_round_trips_over_the_pipe() {
        let endpoint = shared_endpoint(&format!(
            "\\\\.\\pipe\\broiler.v1.test{}.rt",
            std::process::id()
        ));
        let server = ServerConnection::create(&endpoint, true).expect("server");
        let name = endpoint.name.clone();

        let client = std::thread::spawn(move || {
            let client = Client::connect(&name, Instant::now() + Duration::from_secs(5)).expect("connect");
            client
                .request(
                    &Request {
                        id: 1,
                        method: String::from("ping"),
                        params: serde_json::Value::Null,
                        storage_root: None,
                    },
                    Instant::now() + Duration::from_secs(5),
                )
                .expect("response")
        });

        server.connect().expect("connect server side");
        let request = server
            .read_request(Instant::now() + Duration::from_secs(5))
            .expect("read")
            .expect("a request");
        assert_eq!(request.method, "ping");
        server
            .write_response(&Response {
                id: request.id,
                ok: true,
                result: Some(serde_json::json!({"protocol": 1})),
                error: None,
            })
            .expect("write");

        let response = client.join().expect("client thread");
        assert!(response.ok);
    }
}
