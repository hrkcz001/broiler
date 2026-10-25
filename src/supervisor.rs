//! Supervision of the isolated DDC worker.
//!
//! One worker at a time, one operation at a time, every operation bounded. When the bound expires
//! the worker is killed and its termination is *confirmed* before anything else may use the device;
//! if confirmation fails the supervisor quarantines itself and refuses further traffic, reporting a
//! degraded backend instead of starting a second writer against a possibly wedged driver.
//!
//! Repeated failures trip a circuit breaker so a broken DDC path is not hammered in a loop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::ipc::{self, Failure, Request, Response};
use crate::win::process::{self, Child, Handle, Job};

/// Bound for one worker operation. The architecture starts with a 2 s watchdog.
pub const OPERATION_TIMEOUT: Duration = Duration::from_millis(2000);

/// How long a killed worker may take to disappear before the backend is quarantined.
pub const TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Consecutive worker failures that open the circuit breaker.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Idle time after which an on-demand worker is retired.
pub const IDLE_RETIRE_AFTER: Duration = Duration::from_secs(30);

/// The executable and arguments that start a worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl WorkerCommand {
    /// The real worker: this same binary in its internal worker mode.
    pub fn internal(tray_executable: PathBuf) -> WorkerCommand {
        WorkerCommand {
            program: tray_executable,
            args: vec![String::from("--internal-ddc-worker")],
        }
    }
}

/// Why a supervised request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisorError {
    /// The worker could not be started.
    Spawn(String),
    /// No response within the bound. The hardware outcome is unknown.
    Timeout { waited_ms: u64 },
    /// The worker exited or closed the channel.
    WorkerGone(String),
    /// The channel carried something that is not a valid frame.
    Protocol(String),
    /// The supervisor already killed a worker it could not confirm dead.
    Quarantined(String),
    /// The worker answered with a typed device failure.
    Device(Failure),
}

impl SupervisorError {
    pub fn message(&self) -> String {
        match self {
            SupervisorError::Spawn(reason) => format!("cannot start the DDC worker: {reason}"),
            SupervisorError::Timeout { waited_ms } => format!(
                "the DDC worker did not answer within {waited_ms} ms; the worker was killed and the hardware outcome is unknown"
            ),
            SupervisorError::WorkerGone(reason) => format!("the DDC worker stopped: {reason}"),
            SupervisorError::Protocol(reason) => format!("worker protocol error: {reason}"),
            SupervisorError::Quarantined(reason) => format!(
                "the DDC backend is quarantined after an unconfirmed worker termination: {reason}"
            ),
            SupervisorError::Device(failure) => {
                format!("{}: {}", failure.kind, failure.message)
            }
        }
    }

    /// True when the hardware outcome cannot be determined from this result.
    pub fn outcome_unknown(&self) -> bool {
        matches!(
            self,
            SupervisorError::Timeout { .. } | SupervisorError::WorkerGone(_)
        )
    }
}

/// Record of one worker termination, kept so callers can state what was confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Termination {
    pub pid: u32,
    pub confirmed: bool,
    pub waited_ms: u64,
}

struct Running {
    child: Child,
    _job: Job,
    to_worker: Handle,
    from_worker: Handle,
    last_used: Instant,
}

impl Running {
    fn pid(&self) -> u32 {
        self.child.pid()
    }
}

/// Owns at most one worker and serializes every operation through it.
pub struct Supervisor {
    command: WorkerCommand,
    running: Option<Running>,
    failures: u32,
    quarantine: Option<String>,
    next_id: u64,
    operation_timeout: Duration,
    termination_timeout: Duration,
    last_termination: Option<Termination>,
}

impl Supervisor {
    pub fn new(command: WorkerCommand) -> Supervisor {
        Supervisor {
            command,
            running: None,
            failures: 0,
            quarantine: None,
            next_id: 0,
            operation_timeout: OPERATION_TIMEOUT,
            termination_timeout: TERMINATION_TIMEOUT,
            last_termination: None,
        }
    }

    /// Alternate bounds, for tests that must not wait seconds.
    pub fn with_timeouts(
        command: WorkerCommand,
        operation_timeout: Duration,
        termination_timeout: Duration,
    ) -> Supervisor {
        let mut supervisor = Supervisor::new(command);
        supervisor.operation_timeout = operation_timeout;
        supervisor.termination_timeout = termination_timeout;
        supervisor
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn pid(&self) -> Option<u32> {
        self.running.as_ref().map(Running::pid)
    }

    pub fn last_termination(&self) -> Option<&Termination> {
        self.last_termination.as_ref()
    }

    pub fn is_quarantined(&self) -> bool {
        self.quarantine.is_some()
    }

    /// Retire an idle worker. Called by the coordinator's idle policy, never from a UI thread.
    pub fn retire_if_idle(&mut self) {
        let idle = self
            .running
            .as_ref()
            .map(|running| running.last_used.elapsed() >= IDLE_RETIRE_AFTER)
            .unwrap_or(false);
        if idle {
            let _ = self.kill_and_confirm();
        }
    }

    /// Send one request and wait for its response within the operation bound.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, SupervisorError> {
        if let Some(reason) = self.quarantine.clone() {
            return Err(SupervisorError::Quarantined(reason));
        }

        self.ensure_worker()?;

        self.next_id += 1;
        let id = self.next_id;
        let request = Request {
            id,
            method: method.to_string(),
            params,
        };
        let frame = ipc::encode_frame(&request)
            .map_err(|error| SupervisorError::Protocol(error.to_string()))?;

        let (write_handle, read_handle) = match self.running.as_ref() {
            Some(running) => (running.to_worker.raw(), running.from_worker.raw()),
            None => return Err(SupervisorError::WorkerGone(String::from("worker is not running"))),
        };

        if let Err(error) = process::write_all(write_handle, &frame) {
            self.record_failure();
            let reason = error.to_string();
            self.kill_and_confirm().ok();
            return Err(SupervisorError::WorkerGone(reason));
        }

        let deadline = Instant::now() + self.operation_timeout;
        let response = match process::read_frame_deadline(read_handle, deadline) {
            Ok(Some(frame)) => match ipc::decode_frame::<Response>(&frame) {
                Ok(response) => response,
                Err(error) => {
                    self.record_failure();
                    self.kill_and_confirm().ok();
                    return Err(SupervisorError::Protocol(error.to_string()));
                }
            },
            Ok(None) => {
                self.record_failure();
                let reason = String::from("the worker closed the channel");
                self.kill_and_confirm().ok();
                return Err(SupervisorError::WorkerGone(reason));
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                self.record_failure();
                let waited = self.operation_timeout.as_millis().min(u64::MAX as u128) as u64;
                let killed = self.kill_and_confirm();
                if let Err(reason) = killed {
                    return Err(SupervisorError::Quarantined(reason));
                }
                return Err(SupervisorError::Timeout { waited_ms: waited });
            }
            Err(error) => {
                self.record_failure();
                let reason = error.to_string();
                self.kill_and_confirm().ok();
                return Err(SupervisorError::WorkerGone(reason));
            }
        };

        if let Some(running) = self.running.as_mut() {
            running.last_used = Instant::now();
        }

        if response.id != id {
            self.record_failure();
            return Err(SupervisorError::Protocol(format!(
                "response id {} does not match request id {id}",
                response.id
            )));
        }

        if response.ok {
            self.failures = 0;
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            self.record_failure();
            Err(SupervisorError::Device(
                response
                    .error
                    .unwrap_or_else(|| Failure::new("unknown", "worker reported failure")),
            ))
        }
    }

    /// Terminate the worker and confirm it is gone. Returns `Err` with the reason when the
    /// termination could not be confirmed; the supervisor is then quarantined.
    fn kill_and_confirm(&mut self) -> Result<(), String> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        let pid = running.pid();
        let started = Instant::now();

        if let Err(error) = running.child.terminate() {
            // A worker that already exited makes TerminateProcess fail; check before concluding.
            if !running.child.wait(Duration::from_millis(0)) {
                let reason = error.to_string();
                self.quarantine = Some(reason.clone());
                return Err(reason);
            }
        }

        let exited = running.child.wait(self.termination_timeout);
        let waited_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.last_termination = Some(Termination {
            pid,
            confirmed: exited,
            waited_ms,
        });

        if !exited {
            let reason = format!(
                "worker {pid} did not exit within {} ms after TerminateProcess",
                self.termination_timeout.as_millis()
            );
            self.quarantine = Some(reason.clone());
            return Err(reason);
        }
        Ok(())
    }

    fn record_failure(&mut self) {
        self.failures += 1;
        if self.failures >= MAX_CONSECUTIVE_FAILURES && self.quarantine.is_none() {
            self.quarantine = Some(format!(
                "circuit breaker opened after {} consecutive worker failures",
                self.failures
            ));
        }
    }

    fn ensure_worker(&mut self) -> Result<(), SupervisorError> {
        if self.running.is_some() {
            return Ok(());
        }

        let to_worker_pipe = process::create_inheritable_pipe()
            .map_err(|error| SupervisorError::Spawn(error.to_string()))?;
        let from_worker_pipe = process::create_inheritable_pipe()
            .map_err(|error| SupervisorError::Spawn(error.to_string()))?;

        let child = match Child::spawn(
            &self.command.program,
            &self.command.args,
            to_worker_pipe.read.raw(),
            from_worker_pipe.write.raw(),
        ) {
            Ok(child) => child,
            Err(error) => {
                self.record_failure();
                return Err(SupervisorError::Spawn(error.to_string()));
            }
        };

        let job = match Job::kill_on_close() {
            Ok(job) => job,
            Err(error) => {
                let _ = child.terminate();
                child.wait(self.termination_timeout);
                self.record_failure();
                return Err(SupervisorError::Spawn(error.to_string()));
            }
        };
        if let Err(error) = job.assign(child.raw_process()) {
            let _ = child.terminate();
            child.wait(self.termination_timeout);
            self.record_failure();
            return Err(SupervisorError::Spawn(error.to_string()));
        }

        // The child holds the other ends now; keeping our copies would stop the pipe from ever
        // signalling end-of-stream.
        drop(to_worker_pipe.read);
        drop(from_worker_pipe.write);

        self.running = Some(Running {
            child,
            _job: job,
            to_worker: to_worker_pipe.write,
            from_worker: from_worker_pipe.read,
            last_used: Instant::now(),
        });
        Ok(())
    }

    /// Stop the worker and wait for it. Used by the coordinator at shutdown.
    pub fn shutdown(&mut self) {
        let _ = self.kill_and_confirm();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hang_command() -> WorkerCommand {
        // A process that produces no output and never answers: the coordinator's bound has to be what
        // ends the wait.
        WorkerCommand {
            program: PathBuf::from(
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
            ),
            args: vec![
                String::from("-NoProfile"),
                String::from("-NonInteractive"),
                String::from("-Command"),
                String::from("Start-Sleep -Seconds 20"),
            ],
        }
    }

    fn dying_command() -> WorkerCommand {
        WorkerCommand {
            program: PathBuf::from("C:\\Windows\\System32\\cmd.exe"),
            args: vec![String::from("/c"), String::from("exit 0")],
        }
    }

    #[test]
    fn a_hung_worker_is_contained_and_reaped() {
        let mut supervisor = Supervisor::with_timeouts(
            hang_command(),
            Duration::from_millis(1200),
            Duration::from_secs(5),
        );

        let error = supervisor
            .request("hello", Value::Null)
            .expect_err("a silent worker must time out");
        assert!(matches!(error, SupervisorError::Timeout { .. }), "{error:?}");
        assert!(error.outcome_unknown());

        // The supervisor must not leave a worker behind.
        assert!(!supervisor.is_running());
        let termination = supervisor.last_termination().expect("termination record");
        assert!(termination.confirmed, "{termination:?}");
        assert!(!supervisor.is_quarantined());
    }

    #[test]
    fn a_dead_worker_is_reported_and_the_circuit_breaker_opens() {
        let mut supervisor = Supervisor::with_timeouts(
            dying_command(),
            Duration::from_millis(300),
            Duration::from_secs(5),
        );

        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            let _ = supervisor.request("hello", Value::Null);
        }
        assert!(supervisor.is_quarantined());

        let error = supervisor
            .request("hello", Value::Null)
            .expect_err("circuit breaker is open");
        assert!(matches!(error, SupervisorError::Quarantined(_)));
    }

    #[test]
    fn a_missing_executable_fails_to_spawn_without_panicking() {
        let mut supervisor = Supervisor::with_timeouts(
            WorkerCommand {
                program: PathBuf::from("C:\\broiler\\does-not-exist\\broiler-tray.exe"),
                args: vec![String::from("--internal-ddc-worker")],
            },
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        let error = supervisor
            .request("hello", Value::Null)
            .expect_err("missing executable");
        assert!(matches!(error, SupervisorError::Spawn(_)), "{error:?}");
    }
}
