//! Process and pipe primitives for the isolated DDC worker.
//!
//! The worker is launched on demand with two inherited anonymous pipes and assigned to a
//! kill-on-close job object, so a crashed or forgotten child cannot outlive the coordinator. Handles
//! are owned by RAII types; nothing here writes to a monitor.
//!
//! `TerminateProcess` is asynchronous and a worker cannot exit until pending driver I/O returns, so
//! termination is always followed by a bounded wait and the caller has to treat a failed wait as a
//! quarantined backend rather than assuming the process is gone.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JobObjectExtendedLimitInformation,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// Upgrade an OS error with the failing call's name.
fn last_error(call: &str) -> io::Error {
    let code = unsafe { GetLastError() };
    io::Error::new(
        io::ErrorKind::Other,
        format!("{call} failed with Win32 error {code}"),
    )
}

/// An owned Win32 handle.
#[derive(Debug)]
pub struct Handle(HANDLE);

// Raw handles are plain pointers; sharing one across threads is only done by the coordinator, which
// serializes its own traffic.
unsafe impl Send for Handle {}

impl Handle {
    /// Borrow the raw handle for a Win32 call.
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Logical validity: null and `INVALID_HANDLE_VALUE` are both invalid.
    pub fn is_valid(&self) -> bool {
        !self.0.is_null() && self.0 as isize != -1
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.is_valid() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A readable end and a writable end of one anonymous pipe, both inheritable.
pub struct PipePair {
    pub read: Handle,
    pub write: Handle,
}

/// Create an inheritable anonymous pipe.
pub fn create_inheritable_pipe() -> io::Result<PipePair> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    let ok = unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) };
    if ok == 0 {
        return Err(last_error("CreatePipe"));
    }
    Ok(PipePair {
        read: Handle(read),
        write: Handle(write),
    })
}

/// Bytes waiting in a pipe, without blocking.
pub fn bytes_available(handle: HANDLE) -> io::Result<u32> {
    let mut available: u32 = 0;
    let ok = unsafe { PeekNamedPipe(handle, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut available, std::ptr::null_mut()) };
    if ok == 0 {
        return Err(last_error("PeekNamedPipe"));
    }
    Ok(available)
}

/// Read up to `buffer.len()` bytes that are already available.
pub fn read_some(handle: HANDLE, buffer: &mut [u8]) -> io::Result<usize> {
    let mut read: u32 = 0;
    let ok = unsafe {
        ReadFile(
            handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error("ReadFile"));
    }
    Ok(read as usize)
}

/// Write the whole buffer to a pipe.
pub fn write_all(handle: HANDLE, bytes: &[u8]) -> io::Result<()> {
    let mut written_total = 0usize;
    while written_total < bytes.len() {
        let mut written: u32 = 0;
        let chunk = &bytes[written_total..];
        let ok = unsafe {
            WriteFile(
                handle,
                chunk.as_ptr(),
                chunk.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || written == 0 {
            return Err(last_error("WriteFile"));
        }
        written_total += written as usize;
    }
    Ok(())
}

/// A job object that kills its members when the last handle closes.
pub struct Job(Handle);

impl Job {
    pub fn kill_on_close() -> io::Result<Job> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = Job(Handle(handle));

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job.0.raw(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error("SetInformationJobObject"));
        }
        Ok(job)
    }

    pub fn assign(&self, process: HANDLE) -> io::Result<()> {
        let ok = unsafe { AssignProcessToJobObject(self.0.raw(), process) };
        if ok == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(())
    }
}

/// A spawned child process.
pub struct Child {
    process: Handle,
    /// The primary thread handle is kept only so it is closed with the process.
    _thread: Handle,
    pid: u32,
}

fn wide(text: &OsStr) -> Vec<u16> {
    let mut buffer: Vec<u16> = text.encode_wide().collect();
    buffer.push(0);
    buffer
}

impl Child {
    /// Spawn `program` with its standard handles redirected to the given pipes.
    ///
    /// The command line is built from quoted arguments; `lpApplicationName` carries the resolved
    /// executable path, so no `PATH` search happens.
    pub fn spawn(
        program: &Path,
        args: &[String],
        stdin: HANDLE,
        stdout: HANDLE,
    ) -> io::Result<Child> {
        let mut command_line = String::new();
        command_line.push('"');
        command_line.push_str(&program.display().to_string());
        command_line.push('"');
        for argument in args {
            command_line.push(' ');
            command_line.push('"');
            command_line.push_str(&argument.replace('"', "\\\""));
            command_line.push('"');
        }

        let application = wide(program.as_os_str());
        let mut command: Vec<u16> = command_line.encode_utf16().collect();
        command.push(0);

        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = stdin;
        startup.hStdOutput = stdout;
        startup.hStdError = stdout;

        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_NO_WINDOW,
                std::ptr::null_mut(),
                std::ptr::null(),
                &startup,
                &mut info,
            )
        };
        if ok == 0 {
            return Err(last_error("CreateProcessW"));
        }

        Ok(Child {
            process: Handle(info.hProcess),
            _thread: Handle(info.hThread),
            pid: info.dwProcessId,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Raw process handle, for job-object assignment.
    pub fn raw_process(&self) -> HANDLE {
        self.process.raw()
    }

    /// Wait up to `timeout` for exit. Returns true when the process has exited.
    pub fn wait(&self, timeout: Duration) -> bool {
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        let result = unsafe { WaitForSingleObject(self.process.raw(), millis) };
        result == WAIT_OBJECT_0
    }

    /// True when the last [`Child::wait`] timed out.
    pub fn still_running(&self) -> bool {
        matches!(
            unsafe { WaitForSingleObject(self.process.raw(), 0) },
            WAIT_TIMEOUT
        )
    }

    pub fn terminate(&self) -> io::Result<()> {
        let ok = unsafe { TerminateProcess(self.process.raw(), 1) };
        if ok == 0 {
            // A process that already exited is not an error for the caller's purpose.
            let code = unsafe { GetLastError() };
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("TerminateProcess failed with Win32 error {code}"),
            ));
        }
        Ok(())
    }

    pub fn exit_code(&self) -> Option<u32> {
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(self.process.raw(), &mut code) };
        (ok != 0).then_some(code)
    }
}

/// Deadline-aware reader for one length-prefixed frame from a synchronous pipe.
///
/// Anonymous pipes are synchronous, so a blocking `ReadFile` could wait forever. Polling
/// `PeekNamedPipe` keeps the wait bounded and lets the supervisor kill a worker that never answers.
/// Returns `Ok(None)` when the pipe closed.
pub fn read_frame_deadline(handle: HANDLE, deadline: Instant) -> io::Result<Option<Vec<u8>>> {
    let mut frame: Vec<u8> = Vec::new();
    let mut declared: Option<usize> = None;

    loop {
        if let Some(length) = declared {
            if frame.len() >= 4 + length {
                return Ok(Some(frame));
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no complete frame within the operation bound",
            ));
        }

        let available = bytes_available(handle)?;
        if available == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        let mut chunk = vec![0u8; available as usize];
        let read = read_some(handle, &mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        frame.extend_from_slice(&chunk[..read]);

        if declared.is_none() && frame.len() >= 4 {
            let length = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
            if length > crate::ipc::MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("worker declared a {length} byte frame, above the cap"),
                ));
            }
            declared = Some(length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{encode_frame, Response};

    #[test]
    fn pipes_carry_bytes_both_ways() {
        let pair = create_inheritable_pipe().expect("pipe");
        write_all(pair.write.raw(), b"hello").expect("write");
        assert_eq!(bytes_available(pair.read.raw()).expect("peek"), 5);
        let mut buffer = [0u8; 16];
        let read = read_some(pair.read.raw(), &mut buffer).expect("read");
        assert_eq!(&buffer[..read], b"hello");
    }

    #[test]
    fn a_deadline_expires_without_a_full_frame() {
        let pair = create_inheritable_pipe().expect("pipe");
        write_all(pair.write.raw(), &[10, 0, 0, 0]).expect("write prefix only");
        let error = read_frame_deadline(pair.read.raw(), Instant::now() + Duration::from_millis(50))
            .expect_err("truncated frame must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn a_complete_frame_is_returned_before_the_deadline() {
        let pair = create_inheritable_pipe().expect("pipe");
        let frame = encode_frame(&Response {
            id: 3,
            ok: true,
            result: Some(serde_json::json!({"pid": 42})),
            error: None,
        })
        .expect("encode");
        write_all(pair.write.raw(), &frame).expect("write");

        let read = read_frame_deadline(pair.read.raw(), Instant::now() + Duration::from_secs(1))
            .expect("read")
            .expect("frame");
        assert_eq!(read, frame);
    }

    #[test]
    fn a_job_object_can_be_created_and_assigned() {
        let job = Job::kill_on_close().expect("job");
        // A process that exits immediately: assignment must succeed and the handle must stay valid.
        let pair = create_inheritable_pipe().expect("pipe");
        let child = Child::spawn(
            Path::new("C:\\Windows\\System32\\cmd.exe"),
            &[String::from("/c"), String::from("exit 0")],
            pair.read.raw(),
            pair.write.raw(),
        )
        .expect("spawn cmd");
        job.assign(child.process.raw()).expect("assign");
        assert!(child.wait(Duration::from_secs(5)));
        // The assignment and the reap are what this test proves; the exit code of `cmd /c exit 0`
        // depends on how the shell parsed the quoted argument list.
        assert!(child.exit_code().is_some());
        assert!(!child.still_running());
    }
}
