//! Background one-shot device operations with an explicit pre-write gate.
use crate::instance::{request_daemon_command, OwnerInfo};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use crate::job_protocol::READY;
const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Waiting,
    Writing,
    Restoring,
    Finished,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Preparing => "Preparing",
            Self::Waiting => "Waiting for device",
            Self::Writing => "Writing — cancellation unavailable",
            Self::Restoring => "Restoring daemon",
            Self::Finished => "Finished",
        }
    }
    pub fn cancellable(self) -> bool {
        matches!(self, Self::Preparing | Self::Waiting)
    }
}
struct State {
    phase: Phase,
    cancel: bool,
}
pub struct Outcome {
    pub operation: Result<(), String>,
    pub restoration: Result<(), String>,
    pub output: String,
}
pub struct Job {
    pub label: String,
    state: Arc<Mutex<State>>,
    pub receiver: mpsc::Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
}
impl Job {
    pub fn start(
        binary: PathBuf,
        args: Vec<String>,
        label: String,
        owner: Option<OwnerInfo>,
    ) -> Self {
        let state = Arc::new(Mutex::new(State {
            phase: Phase::Preparing,
            cancel: false,
        }));
        let worker_state = state.clone();
        let (tx, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(binary, args, owner, &worker_state)
            }))
            .unwrap_or_else(|_| Outcome {
                operation: Err(
                    "Device worker failed unexpectedly; inspect device and daemon state".into(),
                ),
                restoration: Err("Check daemon state after worker failure".into()),
                output: String::new(),
            });
            worker_state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .phase = Phase::Finished;
            let _ = tx.send(outcome);
        });
        Self {
            label,
            state,
            receiver,
            worker: Some(worker),
        }
    }
    pub fn phase(&self) -> Phase {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .phase
    }
    pub fn cancel(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.phase.cancellable() {
            return false;
        }
        state.cancel = true;
        true
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// Drain every byte, retaining a bounded tail. The readiness marker is recognized
// across arbitrary read boundaries without allowing unbounded line allocations.
fn drain(mut reader: impl Read, ready: Option<mpsc::Sender<()>>) -> String {
    let mut tail = Vec::new();
    let mut line = Vec::new();
    let mut oversized = false;
    let mut chunk = [0u8; 4096];
    while let Ok(count) = reader.read(&mut chunk) {
        if count == 0 {
            break;
        }
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                if !oversized && line == READY.as_bytes() {
                    if let Some(tx) = &ready {
                        let _ = tx.send(());
                    }
                }
                line.clear();
                oversized = false;
            } else if line.len() < 256 && !oversized {
                line.push(*byte);
            } else {
                oversized = true;
            }
        }
        tail.extend_from_slice(&chunk[..count]);
        if tail.len() > OUTPUT_LIMIT {
            tail.drain(..tail.len() - OUTPUT_LIMIT);
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

fn exited_without_reaping(child: &Child) -> std::io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() != 0 })
}

struct Restore<F: FnMut(&str) -> Result<String, String>> {
    control: F,
    paused: bool,
}
impl<F: FnMut(&str) -> Result<String, String>> Restore<F> {
    fn restore(&mut self) -> Result<(), String> {
        if !std::mem::take(&mut self.paused) {
            return Ok(());
        }
        (self.control)("resume").and_then(|response| {
            if response.starts_with("running") {
                Ok(())
            } else {
                Err(format!("Unexpected resume response: {response}"))
            }
        })
    }
}
impl<F: FnMut(&str) -> Result<String, String>> Drop for Restore<F> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

struct Helper {
    child: Child,
    writing: bool,
    reaped: bool,
}
impl Helper {
    fn cancel_preparation(&mut self) {
        if self.reaped {
            return;
        }
        // Preparation never reaps early: even an exited leader reserves its
        // PID, so its group cannot be reused before descendants are stopped.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
        self.reaped = true;
    }
}
impl std::ops::Deref for Helper {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl std::ops::DerefMut for Helper {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        if self.writing {
            let _ = self.child.wait();
        } else {
            self.cancel_preparation();
        }
    }
}

fn run(
    binary: PathBuf,
    args: Vec<String>,
    owner: Option<OwnerInfo>,
    state: &Mutex<State>,
) -> Outcome {
    run_with_control(binary, args, state, |command| {
        if let Some(owner) = &owner {
            request_daemon_command(owner, command, Duration::from_secs(5))
        } else {
            Ok("paused".into())
        }
    })
}

fn run_with_control(
    binary: PathBuf,
    args: Vec<String>,
    state: &Mutex<State>,
    control: impl FnMut(&str) -> Result<String, String>,
) -> Outcome {
    let mut command = Command::new(binary);
    command
        .args(args)
        .arg("--gui-device-job")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome {
                operation: Err(format!("Cannot start device helper: {error}")),
                restoration: Ok(()),
                output: String::new(),
            }
        }
    };
    let mut daemon = Restore {
        control,
        paused: false,
    };
    // Declared after the restore guard: unwind reaps the helper before resume.
    let mut child = Helper {
        child,
        writing: false,
        reaped: false,
    };
    let (ready_tx, ready_rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let output_reader = thread::spawn(move || drain(stdout, Some(ready_tx)));
    let error_reader = thread::spawn(move || drain(stderr, None));
    let mut writing = false;
    let operation = (|| -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if state.lock().unwrap().cancel {
                return Err("Cancelled before device writes".into());
            }
            if ready_rx.try_recv().is_ok() {
                break;
            }
            if exited_without_reaping(&child).map_err(|error| error.to_string())? {
                return Err("Preparation exited without readiness acknowledgement".into());
            }
            if Instant::now() >= deadline {
                return Err("Media preparation timed out before device writes".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
        state.lock().unwrap().phase = Phase::Waiting;
        if exited_without_reaping(&child).map_err(|error| error.to_string())? {
            return Err("Helper exited before device handoff".into());
        }
        {
            let previous = (daemon.control)("state")?;
            if previous == "running" {
                let response = (daemon.control)("pause").map_err(|error| {
                    format!("Daemon pause failed; it may require manual resume: {error}")
                })?;
                if response == "paused changed" {
                    daemon.paused = true;
                } else if response != "paused unchanged" {
                    return Err(format!("Unexpected pause acknowledgement: {response}"));
                }
            } else if previous != "paused" {
                return Err(format!(
                    "Daemon is transitioning ({previous}); try again when ready"
                ));
            }
        }
        {
            // Cancel and write permission use the same mutex. Once this gate
            // wins, cancellation can never kill a helper performing writes.
            let mut gate = state.lock().unwrap();
            if gate.cancel {
                return Err("Cancelled before device writes".into());
            }
            let mut input = child
                .stdin
                .take()
                .ok_or("Device helper input is unavailable")?;
            input
                .write_all(b"begin\n")
                .and_then(|_| input.flush())
                .map_err(|error| error.to_string())?;
            gate.phase = Phase::Writing;
            writing = true;
            child.writing = true;
        }
        let status = child.wait().map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "Device write failed ({status}); device state may be incomplete"
            ))
        }
    })();
    if !writing {
        child.cancel_preparation();
    } else {
        let _ = child.wait();
    }
    state.lock().unwrap().phase = Phase::Restoring;
    let restoration = daemon.restore();
    let output = output_reader.join().unwrap_or_default();
    let errors = error_reader.join().unwrap_or_default();
    let operation = operation.map_err(|error| {
        if errors.trim().is_empty() {
            error
        } else {
            format!("{error}: {}", errors.trim())
        }
    });
    Outcome {
        operation,
        restoration,
        output: format!("{output}\n{errors}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_drain_is_bounded_and_finds_split_ready_lines() {
        let data = format!("{}\n{READY}\n{}", "x".repeat(8000), "y".repeat(100_000));
        let (tx, rx) = mpsc::channel();
        let output = drain(std::io::Cursor::new(data), Some(tx));
        assert_eq!(output.len(), OUTPUT_LIMIT);
        assert!(rx.try_recv().is_ok());
    }
    #[test]
    fn write_and_restoration_phases_cannot_be_cancelled() {
        for phase in [Phase::Writing, Phase::Restoring, Phase::Finished] {
            assert!(!phase.cancellable());
        }
        assert!(Phase::Preparing.cancellable());
        assert!(Phase::Waiting.cancellable());
    }

    fn fake_job(script: &str, path: &std::path::Path) -> Job {
        Job::start(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                script.into(),
                "fake-helper".into(),
                path.to_string_lossy().into_owned(),
            ],
            "test operation".into(),
            None,
        )
    }

    #[test]
    fn cancellation_before_readiness_never_writes_and_reaps_helper() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("written");
        let job = fake_job(
            "sleep 1; printf 'TH420_DEVICE_JOB_READY_V1\\n'; read command; echo write > \"$1\"",
            &path,
        );
        assert!(job.cancel());
        let result = job.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(result.operation.unwrap_err().contains("Cancelled"));
        assert!(!path.exists());
        assert!(result.restoration.is_ok());
    }

    #[test]
    fn writing_cannot_be_cancelled_and_waits_for_completion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("written");
        let job = fake_job("printf 'TH420_DEVICE_JOB_READY_V1\\n'; read command; test \"$command\" = begin || exit 9; sleep 0.2; echo write > \"$1\"", &path);
        let deadline = Instant::now() + Duration::from_secs(5);
        while job.phase() != Phase::Writing && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(job.phase(), Phase::Writing);
        assert!(!job.cancel());
        assert!(job
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .operation
            .is_ok());
        assert!(path.exists());
    }

    #[test]
    fn helper_failure_reports_error_without_authorizing_writes() {
        let dir = tempfile::tempdir().unwrap();
        let job = fake_job("echo 'bad input' >&2; exit 3", &dir.path().join("unused"));
        let result = job.receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(result.operation.unwrap_err().contains("bad input"));
    }

    #[test]
    fn cancellation_after_pause_restores_only_the_daemon_it_paused() {
        let state = Mutex::new(State {
            phase: Phase::Preparing,
            cancel: false,
        });
        let mut calls = Vec::new();
        let result = run_with_control(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf 'TH420_DEVICE_JOB_READY_V1\\n'; read command; exit 7".into(),
            ],
            &state,
            |command| {
                calls.push(command.to_string());
                match command {
                    "state" => Ok("running".into()),
                    "pause" => {
                        state.lock().unwrap().cancel = true;
                        Ok("paused changed".into())
                    }
                    "resume" => Ok("running changed".into()),
                    _ => unreachable!(),
                }
            },
        );
        assert!(result.operation.unwrap_err().contains("Cancelled"));
        assert!(result.restoration.is_ok());
        assert_eq!(calls, ["state", "pause", "resume"]);
    }

    #[test]
    fn already_paused_daemon_is_preserved_on_write_failure() {
        let state = Mutex::new(State {
            phase: Phase::Preparing,
            cancel: false,
        });
        let mut calls = Vec::new();
        let result = run_with_control(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf 'TH420_DEVICE_JOB_READY_V1\\n'; read command; echo failure >&2; exit 7"
                    .into(),
            ],
            &state,
            |command| {
                calls.push(command.to_string());
                Ok("paused".into())
            },
        );
        assert!(result
            .operation
            .unwrap_err()
            .contains("state may be incomplete"));
        assert_eq!(calls, ["state"]);
    }

    #[test]
    fn operation_and_restoration_failures_are_independent() {
        let state = Mutex::new(State {
            phase: Phase::Preparing,
            cancel: false,
        });
        let result = run_with_control(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf 'TH420_DEVICE_JOB_READY_V1\\n'; read command; exit 0".into(),
            ],
            &state,
            |command| match command {
                "state" => Ok("running".into()),
                "pause" => Ok("paused changed".into()),
                "resume" => Err("daemon was replaced".into()),
                _ => unreachable!(),
            },
        );
        assert!(result.operation.is_ok());
        assert_eq!(result.restoration.unwrap_err(), "daemon was replaced");
    }

    #[test]
    fn exited_preparation_leader_does_not_orphan_a_pipe_holding_descendant() {
        let state = Mutex::new(State {
            phase: Phase::Preparing,
            cancel: false,
        });
        let started = Instant::now();
        let result = run_with_control(
            "/bin/sh".into(),
            vec!["-c".into(), "sleep 30 & exit 3".into()],
            &state,
            |_| panic!("must not contact daemon"),
        );
        assert!(result.operation.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn pause_failure_never_grants_write_permission_or_blindly_resumes() {
        let state = Mutex::new(State {
            phase: Phase::Preparing,
            cancel: false,
        });
        let mut calls = Vec::new();
        let result = run_with_control(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf 'TH420_DEVICE_JOB_READY_V1\\n'; read command; exit 7".into(),
            ],
            &state,
            |command| {
                calls.push(command.to_string());
                if command == "state" {
                    Ok("running".into())
                } else {
                    Err("pause timeout".into())
                }
            },
        );
        assert!(result.operation.unwrap_err().contains("manual resume"));
        assert_eq!(calls, ["state", "pause"]);
    }
}
