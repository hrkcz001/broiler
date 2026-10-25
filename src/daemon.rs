//! The resident coordinator process.
//!
//! `broiler-tray.exe` with no arguments becomes this daemon until the tray and its windows exist.
//! It takes the per-logon single-instance lock, serves the ACL-protected pipe, and serialises every
//! command through one [`Coordinator`]. Connections are handled on their own threads, but the
//! coordinator is behind a mutex, so no two hardware operations can overlap and no second writer is
//! ever created for the same logon session.
//!
//! A command's response is written on the connection that asked for it; a long hardware call only
//! blocks that connection, never the accept loop.

use std::io;
use std::sync::{Arc, Condvar, Mutex};

use crate::coordinator::Coordinator;
use crate::ipc::{Failure, Request, Response};
use crate::pipe::{Endpoint, ServerConnection};
use crate::transport::Transport;

/// Bound for reading one request and writing one response.
pub const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Handle one already connected client: read one request, dispatch, write one response.
pub fn handle_connection<T: Transport>(
    connection: ServerConnection,
    coordinator: Arc<Mutex<Coordinator<T>>>,
) {
    let deadline = std::time::Instant::now() + CONNECTION_TIMEOUT;

    let request = match connection.read_request(deadline) {
        Ok(Some(request)) => request,
        Ok(None) => {
            connection.disconnect();
            return;
        }
        Err(error) => {
            // The frame was unusable (oversized, length mismatch, malformed JSON). Answer with a
            // typed protocol error rather than panicking or staying silent.
            let _ = connection.write_response(&Response {
                id: 0,
                ok: false,
                result: None,
                error: Some(Failure::new("protocol_error", error.to_string())),
            });
            connection.disconnect();
            return;
        }
    };

    // A coordinator that has already been told to stop must not serve another command: the client
    // would otherwise see a live answer from a process that is already on its way out.
    let stopping = coordinator
        .lock()
        .map(|guard| guard.shutdown_requested())
        .unwrap_or(true);

    let response = if stopping {
        Response {
            id: request.id,
            ok: false,
            result: None,
            error: Some(Failure::new(
                "shutting_down",
                "the coordinator is stopping and accepts no further commands",
            )),
        }
    } else {
        dispatch(&coordinator, &request)
    };

    let _ = connection.write_response(&response);
    connection.disconnect();
}

fn dispatch<T: Transport>(coordinator: &Arc<Mutex<Coordinator<T>>>, request: &Request) -> Response {
    let mut guard = match coordinator.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Response {
                id: request.id,
                ok: false,
                result: None,
                error: Some(Failure::new(
                    "internal",
                    "the coordinator lock was poisoned by an earlier panic",
                )),
            }
        }
    };

    match guard.handle(&request.method, &request.params) {
        Ok(result) => Response {
            id: request.id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            id: request.id,
            ok: false,
            result: None,
            error: Some(error),
        },
    }
}

/// Serves one endpoint until a shutdown command arrives.
pub struct Daemon<T: Transport> {
    endpoint: Endpoint,
    coordinator: Arc<Mutex<Coordinator<T>>>,
}

impl<T: Transport + Send + 'static> Daemon<T> {
    pub fn new(endpoint: Endpoint, coordinator: Arc<Mutex<Coordinator<T>>>) -> Daemon<T> {
        Daemon {
            endpoint,
            coordinator,
        }
    }

    /// Accept connections until `shutdown`. Returns the number of connections served.
    ///
    /// The accept loop creates a fresh instance per connection so a second client can connect while
    /// the first one is still being served; the coordinator mutex is what serialises execution.
    pub fn run(&self) -> io::Result<u64> {
        let stopped = Arc::new((Mutex::new(false), Condvar::new()));
        let mut first = true;
        let mut served = 0u64;

        loop {
            if self.stopping() {
                break;
            }
            let connection = match ServerConnection::create(&self.endpoint, first) {
                Ok(connection) => connection,
                Err(error) => {
                    if first {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!("another coordinator owns the endpoint: {error}"),
                        ));
                    }
                    return Err(error);
                }
            };
            first = false;

            connection.connect()?;
            served += 1;

            let coordinator = Arc::clone(&self.coordinator);
            std::thread::spawn(move || handle_connection(connection, coordinator));

            // Give the handler a chance to register a shutdown before accepting the next client.
            if self.coordinator.lock().map(|guard| guard.shutdown_requested()).unwrap_or(true) {
                break;
            }
        }

        let _ = stopped;
        Ok(served)
    }

    fn stopping(&self) -> bool {
        self.coordinator
            .lock()
            .map(|guard| guard.shutdown_requested())
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::registry::ProfileRegistry;
    use crate::config::settings::Settings;
    use crate::ipc::{encode_frame, Request};
    use crate::pipe::{local_endpoint, Client};
    use crate::transport::FakeTransport;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn shared_endpoint(name: &str) -> Endpoint {
        local_endpoint().expect("endpoint").with_name(name)
    }

    fn coordinator() -> Arc<Mutex<Coordinator<FakeTransport>>> {
        let transport = FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 40);
        Arc::new(Mutex::new(Coordinator::new(
            transport,
            Settings::default(),
            ProfileRegistry::default(),
            PathBuf::from("C:\\test\\settings.toml"),
        )))
    }

    fn serve_one(
        endpoint: &Endpoint,
        coordinator: Arc<Mutex<Coordinator<FakeTransport>>>,
    ) -> (std::thread::JoinHandle<()>, String) {
        let server = ServerConnection::create(endpoint, true).expect("server instance");
        let name = endpoint.name.clone();
        let handle = std::thread::spawn(move || {
            server.connect().expect("client connected");
            handle_connection(server, coordinator);
        });
        (handle, name)
    }

    #[test]
    fn two_concurrent_clients_share_one_executor() {
        let name = format!("\\\\.\\pipe\\broiler.v1.test{}.concurrent", std::process::id());
        let endpoint = shared_endpoint(&name);
        let coordinator_state = coordinator();

        // Two instances of one endpoint, so both clients connect at the same time.
        let first = ServerConnection::create(&endpoint, true).expect("first instance");
        let second = ServerConnection::create(&endpoint, false).expect("second instance");

        let shared = Arc::clone(&coordinator_state);
        let server_thread = std::thread::spawn(move || {
            let a = std::thread::spawn({
                let shared = Arc::clone(&shared);
                move || {
                    first.connect().expect("connect");
                    handle_connection(first, shared);
                }
            });
            let b = std::thread::spawn({
                let shared = Arc::clone(&shared);
                move || {
                    second.connect().expect("connect");
                    handle_connection(second, shared);
                }
            });
            a.join().expect("handler a");
            b.join().expect("handler b");
        });

        let client_threads: Vec<_> = (0..2)
            .map(|index| {
                let name = name.clone();
                std::thread::spawn(move || {
                    let client = Client::connect(&name, Instant::now() + Duration::from_secs(5))
                        .expect("client connect");
                    let response = client
                        .request(
                            &Request {
                                id: index + 1,
                                method: String::from("parameters.set"),
                                params: json!({
                                    "monitor": "mon-a",
                                    "control": "brightness",
                                    "value": 40 + index,
                                    "unit": "native"
                                }),
                            },
                            Instant::now() + Duration::from_secs(5),
                        )
                        .expect("response");
                    assert!(response.ok, "{response:?}");
                    response.id == index + 1
                })
            })
            .collect();

        for thread in client_threads {
            assert!(thread.join().expect("client thread"));
        }
        server_thread.join().expect("server thread");

        let guard = coordinator_state.lock().expect("coordinator");
        // One executor: no two transport operations were ever inside the backend at the same time.
        assert_eq!(guard.transport_peak_concurrency(), Some(1));
    }

    #[test]
    fn a_malformed_frame_gets_a_protocol_error_instead_of_a_panic() {
        let name = format!("\\\\.\\pipe\\broiler.v1.test{}.malformed", std::process::id());
        let endpoint = shared_endpoint(&name);
        let (handle, _name) = serve_one(&endpoint, coordinator());

        let client = Client::connect(&name, Instant::now() + Duration::from_secs(5)).expect("connect");
        client.send_raw(&[3, 0, 0, 0, b'{', b'x', b'}']).expect("send");

        let frame = client
            .read_raw_response(Instant::now() + Duration::from_secs(5))
            .expect("read")
            .expect("a frame");
        let response: Response = crate::ipc::decode_frame(&frame).expect("decode");
        assert!(!response.ok);
        assert_eq!(response.error.expect("error").kind, "protocol_error");
        handle.join().expect("server thread");
    }

    #[test]
    fn an_oversized_frame_is_refused_without_allocating_it() {
        let name = format!("\\\\.\\pipe\\broiler.v1.test{}.oversized", std::process::id());
        let endpoint = shared_endpoint(&name);
        let (handle, _name) = serve_one(&endpoint, coordinator());

        let client = Client::connect(&name, Instant::now() + Duration::from_secs(5)).expect("connect");
        let mut frame = ((crate::ipc::MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(b"{}");
        client.send_raw(&frame).expect("send");

        let response = client.read_raw_response(Instant::now() + Duration::from_secs(5));
        match response {
            Ok(Some(frame)) => {
                let response: Response = crate::ipc::decode_frame(&frame).expect("decode");
                assert_eq!(response.error.expect("error").kind, "protocol_error");
            }
            // A closed connection is also acceptable here: the point is that nothing was allocated
            // and the server did not crash.
            Ok(None) | Err(_) => {}
        }
        handle.join().expect("server thread");
    }

    #[test]
    fn a_ping_over_the_pipe_reports_the_protocol() {
        let name = format!("\\\\.\\pipe\\broiler.v1.test{}.ping", std::process::id());
        let endpoint = shared_endpoint(&name);
        let (handle, _name) = serve_one(&endpoint, coordinator());

        let client = Client::connect(&name, Instant::now() + Duration::from_secs(5)).expect("connect");
        let request = Request {
            id: 9,
            method: String::from("ping"),
            params: serde_json::Value::Null,
        };
        let _ = encode_frame(&request).expect("encode");
        let response = client
            .request(&request, Instant::now() + Duration::from_secs(5))
            .expect("response");
        assert!(response.ok);
        handle.join().expect("server thread");
    }
}
