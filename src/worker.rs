//! The isolated DDC worker.
//!
//! `broiler-tray.exe --internal-ddc-worker` enters this module. The process owns the physical
//! monitor handles for the duration of one operation, speaks the private length-prefixed JSON
//! protocol over inherited standard handles, and exits when the coordinator closes the channel.
//!
//! It exists because Dxva2 calls block inside the display driver with no cancellation contract. The
//! coordinator can therefore abandon or kill this process without endangering its own responsiveness.
//! No monitor handle ever crosses the process boundary: the worker receives a monitor identifier and
//! resolves it to a handle itself.

use std::io::{BufReader, BufWriter};

use serde_json::{json, Value};

use crate::discovery;
use crate::ipc::{self, Failure, Request, Response};
use crate::win::ddc;

/// Control names accepted by the worker. There is no numeric VCP code in the protocol.
pub const CONTROL_BRIGHTNESS: &str = "brightness";
pub const CONTROL_CONTRAST: &str = "contrast";

fn ok(id: u64, result: Value) -> Response {
    Response {
        id,
        ok: true,
        result: Some(result),
        error: None,
    }
}

fn fail(id: u64, kind: &str, message: impl Into<String>) -> Response {
    Response {
        id,
        ok: false,
        result: None,
        error: Some(Failure::new(kind, message)),
    }
}

fn string_param(params: &Value, key: &str) -> Result<String, Response> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(|text| text.to_string())
        .ok_or_else(|| fail(0, "invalid_params", format!("`{key}` must be a string")))
}

/// Resolve a monitor identifier to its current handle, refusing built-in panels.
fn resolve_handle(id: &str) -> Result<isize, Failure> {
    let targets = discovery::target_handles().map_err(|failure| {
        Failure::new("topology_error", failure.message())
    })?;
    match targets.iter().find(|target| target.id == id) {
        Some(target) if target.builtin => Err(Failure::new(
            "unsupported",
            "built-in panels are never controlled",
        )),
        Some(target) => Ok(target.hmonitor),
        None => Err(Failure::new(
            "unknown_monitor",
            format!("monitor {id} is not present in this session"),
        )),
    }
}

fn control_of(params: &Value) -> Result<bool, Response> {
    let control = string_param(params, "control")?;
    match control.as_str() {
        CONTROL_BRIGHTNESS => Ok(false),
        CONTROL_CONTRAST => Ok(true),
        other => Err(fail(
            0,
            "unsupported",
            format!("`{other}` is not a worker control; only brightness and contrast exist"),
        )),
    }
}

fn handle_request(request: &Request) -> Response {
    match request.method.as_str() {
        "hello" => ok(
            request.id,
            json!({"protocol": ipc::PROTOCOL_VERSION, "pid": std::process::id()}),
        ),
        "monitors" => match discovery::target_handles() {
            Ok(targets) => ok(request.id, json!({ "monitors": targets })),
            Err(failure) => fail(request.id, "topology_error", failure.message()),
        },
        "capabilities" => {
            let monitor = match string_param(&request.params, "monitor") {
                Ok(value) => value,
                Err(mut response) => {
                    response.id = request.id;
                    return response;
                }
            };
            let hmonitor = match resolve_handle(&monitor) {
                Ok(handle) => handle,
                Err(failure) => return fail(request.id, &failure.kind, failure.message),
            };
            match ddc::probe(hmonitor, true, crate::discovery::CAPABILITIES_PROBE_BUDGET) {
                Ok(outcome) => match outcome.capabilities {
                    Ok(raw) => ok(
                        request.id,
                        json!({
                            "raw": raw.text,
                            "declared_length": raw.declared_length,
                            "physical_monitor": outcome.descriptions,
                        }),
                    ),
                    Err(error) => fail(request.id, error.state(), error.message()),
                },
                Err(error) => fail(request.id, error.state(), error.message()),
            }
        }
        "read" => {
            let monitor = match string_param(&request.params, "monitor") {
                Ok(value) => value,
                Err(mut response) => {
                    response.id = request.id;
                    return response;
                }
            };
            let contrast = match control_of(&request.params) {
                Ok(value) => value,
                Err(mut response) => {
                    response.id = request.id;
                    return response;
                }
            };
            let hmonitor = match resolve_handle(&monitor) {
                Ok(handle) => handle,
                Err(failure) => return fail(request.id, &failure.kind, failure.message),
            };
            match ddc::with_physical_monitor(hmonitor, |handle| ddc::read_control(handle, contrast))
            {
                Ok(Ok(reading)) => ok(
                    request.id,
                    json!({
                        "value": reading.current,
                        "minimum": reading.minimum,
                        "maximum": reading.maximum,
                        "backend": reading.backend,
                    }),
                ),
                Ok(Err(error)) => fail(request.id, error.state(), error.message()),
                Err(error) => fail(request.id, error.state(), error.message()),
            }
        }
        "write" => {
            let monitor = match string_param(&request.params, "monitor") {
                Ok(value) => value,
                Err(mut response) => {
                    response.id = request.id;
                    return response;
                }
            };
            let contrast = match control_of(&request.params) {
                Ok(value) => value,
                Err(mut response) => {
                    response.id = request.id;
                    return response;
                }
            };
            let value = match request.params.get("value").and_then(Value::as_u64) {
                Some(value) if value <= u32::MAX as u64 => value as u32,
                _ => return fail(request.id, "invalid_params", "`value` must be a 32-bit integer"),
            };
            let hmonitor = match resolve_handle(&monitor) {
                Ok(handle) => handle,
                Err(failure) => return fail(request.id, &failure.kind, failure.message),
            };

            let written = ddc::with_physical_monitor(hmonitor, |handle| {
                ddc::write_control(handle, contrast, value)
            });
            let assigned_backend = match written {
                Ok(Ok(backend)) => backend,
                Ok(Err(error)) => return fail(request.id, error.state(), error.message()),
                Err(error) => return fail(request.id, error.state(), error.message()),
            };

            // Read back inside the same process so the verified value belongs to this operation.
            match ddc::with_physical_monitor(hmonitor, |handle| ddc::read_control(handle, contrast))
            {
                Ok(Ok(reading)) => ok(
                    request.id,
                    json!({
                        "requested": value,
                        "observed": reading.current,
                        "minimum": reading.minimum,
                        "maximum": reading.maximum,
                        "write_backend": assigned_backend,
                        "read_backend": reading.backend,
                    }),
                ),
                Ok(Err(error)) => fail(request.id, error.state(), error.message()),
                Err(error) => fail(request.id, error.state(), error.message()),
            }
        }
        other => fail(
            request.id,
            "unknown_method",
            format!("`{other}` is not a worker method"),
        ),
    }
}

/// Serve requests until the coordinator closes the channel. Returns the process exit code.
pub fn serve() -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    loop {
        let frame = match ipc::read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return 0,
            Err(_) => return 2,
        };

        let request: Request = match ipc::decode_frame(&frame) {
            Ok(request) => request,
            Err(error) => {
                let response = fail(0, "protocol_error", error.to_string());
                let frame = match ipc::encode_frame(&response) {
                    Ok(frame) => frame,
                    Err(_) => return 2,
                };
                if ipc::write_frame(&mut writer, &frame).is_err() {
                    return 2;
                }
                continue;
            }
        };

        let response = handle_request(&request);
        let frame = match ipc::encode_frame(&response) {
            Ok(frame) => frame,
            Err(_) => return 2,
        };
        if ipc::write_frame(&mut writer, &frame).is_err() {
            return 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_reports_the_protocol_and_pid() {
        let response = handle_request(&Request {
            id: 5,
            method: String::from("hello"),
            params: Value::Null,
        });
        assert!(response.ok);
        let result = response.result.expect("result");
        assert_eq!(result["protocol"], ipc::PROTOCOL_VERSION);
        assert!(result["pid"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn unknown_methods_are_refused_without_touching_hardware() {
        let response = handle_request(&Request {
            id: 6,
            method: String::from("set-vcp"),
            params: json!({"monitor": "mon-aa", "code": 0xD6, "value": 5}),
        });
        assert!(!response.ok);
        assert_eq!(response.error.expect("error").kind, "unknown_method");
    }

    #[test]
    fn control_names_are_an_allowlist() {
        let response = handle_request(&Request {
            id: 7,
            method: String::from("write"),
            params: json!({"monitor": "mon-aa", "control": "sharpness", "value": 5}),
        });
        assert!(!response.ok);
        assert_eq!(response.error.expect("error").kind, "unsupported");
    }

    #[test]
    fn a_write_without_a_value_is_refused_before_any_handle_is_opened() {
        let response = handle_request(&Request {
            id: 8,
            method: String::from("write"),
            params: json!({"monitor": "mon-aa", "control": "brightness"}),
        });
        assert!(!response.ok);
        assert_eq!(response.error.expect("error").kind, "invalid_params");
    }
}
