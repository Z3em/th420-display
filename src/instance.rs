use clap::ValueEnum;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ReplaceExisting {
    Graceful,
    Term,
    Kill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Each binary uses one variant from this shared source module.
pub enum InstanceKind {
    Gui,
    Daemon,
}

impl InstanceKind {
    fn name(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Daemon => "daemon",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerInfo {
    pub pid: u32,
    pub uid: u32,
    pub start_ticks: u64,
    pub executable: PathBuf,
    pub executable_identity: Option<(u64, u64)>,
    pub command_line: String,
    pub lock_path: PathBuf,
    pub socket_path: PathBuf,
}

impl OwnerInfo {
    pub fn describe(&self) -> String {
        format!(
            "PID: {}\nUser/UID: {} ({})\nExecutable: {}\nStart time (clock ticks): {}\nCommand: {}\nLock: {}\nControl socket: {}",
            self.pid,
            user_name(self.uid),
            self.uid,
            self.executable.display(),
            self.start_ticks,
            self.command_line,
            self.lock_path.display(),
            self.socket_path.display(),
        )
    }
}

#[derive(Debug)]
pub enum AcquireError {
    Conflict(OwnerInfo),
    Other(String),
}

pub struct InstanceGuard {
    lock_file: File,
    owner_path: PathBuf,
    socket_path: PathBuf,
    listener_stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    daemon_control: Option<Arc<DaemonControl>>,
}

#[allow(dead_code)] // Device ownership is used by the daemon binary, not the GUI binary.
pub struct DeviceGuard {
    lock_file: File,
}

#[derive(Debug)]
struct DaemonControlStatus {
    desired_paused: bool,
    actual_paused: bool,
    transition_error: Option<String>,
    stopping: bool,
    starting: bool,
    telemetry: Option<DaemonTelemetry>,
    telemetry_error: Option<String>,
}

#[derive(Clone, Debug)]
struct DaemonTelemetry {
    coolant_temp_c: f32,
    pump_rpm: u16,
    updated_at: Instant,
}

#[derive(Debug)]
pub struct DaemonControl {
    status: Mutex<DaemonControlStatus>,
    changed: Condvar,
}

impl DaemonControl {
    pub fn new_starting() -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(DaemonControlStatus {
                desired_paused: false,
                actual_paused: false,
                transition_error: None,
                stopping: false,
                starting: true,
                telemetry: None,
                telemetry_error: None,
            }),
            changed: Condvar::new(),
        })
    }

    pub fn pause_requested(&self) -> bool {
        self.status.lock().unwrap().desired_paused
    }

    pub fn mark_paused(&self) {
        let mut status = self.status.lock().unwrap();
        status.actual_paused = true;
        status.starting = false;
        status.transition_error = None;
        self.changed.notify_all();
    }

    pub fn mark_running(&self) {
        let mut status = self.status.lock().unwrap();
        status.actual_paused = false;
        status.starting = false;
        status.transition_error = None;
        self.changed.notify_all();
    }

    pub fn mark_resume_failed(&self, error: String) {
        let mut status = self.status.lock().unwrap();
        status.desired_paused = true;
        status.actual_paused = true;
        status.starting = false;
        status.transition_error = Some(error);
        self.changed.notify_all();
    }

    pub fn update_telemetry(&self, coolant_temp_c: f32, pump_rpm: u16) {
        let mut status = self.status.lock().unwrap();
        status.telemetry = Some(DaemonTelemetry {
            coolant_temp_c,
            pump_rpm,
            updated_at: Instant::now(),
        });
        status.telemetry_error = None;
    }

    pub fn mark_telemetry_error(&self, error: String) {
        let mut status = self.status.lock().unwrap();
        status.telemetry = None;
        status.telemetry_error = Some(error);
    }

    fn stop(&self) {
        let mut status = self.status.lock().unwrap();
        status.stopping = true;
        self.changed.notify_all();
    }

    fn command(&self, command: &str, timeout: Duration) -> Result<String, String> {
        let mut status = self.status.lock().unwrap();
        if status.stopping {
            return Err("daemon is stopping".into());
        }
        let changed = match command {
            "pause" => {
                let changed = !status.desired_paused && !status.actual_paused;
                status.desired_paused = true;
                changed
            }
            "resume" => {
                let changed = status.desired_paused && status.actual_paused;
                status.desired_paused = false;
                status.transition_error = None;
                changed
            }
            "state" => return Ok(control_state(&status).into()),
            "telemetry" => {
                let Some(telemetry) = &status.telemetry else {
                    return Err(status
                        .telemetry_error
                        .clone()
                        .unwrap_or_else(|| "device telemetry is not available yet".into()));
                };
                return Ok(format!(
                    "coolant_temp_c={:.2}\npump_rpm={}\nage_ms={}",
                    telemetry.coolant_temp_c,
                    telemetry.pump_rpm,
                    telemetry.updated_at.elapsed().as_millis()
                ));
            }
            _ => return Err(format!("unsupported daemon command: {command}")),
        };
        let desired = status.desired_paused;
        let (status, wait) = self
            .changed
            .wait_timeout_while(status, timeout, |status| {
                status.actual_paused != desired
                    && status.transition_error.is_none()
                    && !status.stopping
            })
            .unwrap();
        if let Some(error) = &status.transition_error {
            return Err(error.clone());
        }
        if status.stopping {
            return Err("daemon is stopping".into());
        }
        if wait.timed_out() && status.actual_paused != desired {
            return Err(format!(
                "daemon did not enter {} state",
                if desired { "paused" } else { "running" }
            ));
        }
        Ok(format!(
            "{} {}",
            control_state(&status),
            if changed { "changed" } else { "unchanged" }
        ))
    }
}

fn control_state(status: &DaemonControlStatus) -> &'static str {
    if status.stopping {
        return "stopping";
    }
    if status.starting && !status.desired_paused {
        return "starting";
    }
    match (status.desired_paused, status.actual_paused) {
        (false, false) => "running",
        (true, true) => "paused",
        (true, false) => "pausing",
        (false, true) => "resuming",
    }
}

impl DeviceGuard {
    #[allow(dead_code)]
    pub fn acquire() -> Result<Self, String> {
        let (lock_path, _) = instance_paths(InstanceKind::Daemon)?;
        let lock_path = lock_path.with_file_name("th420-display-device.lock");
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(|error| format!("cannot open device lock {}: {error}", lock_path.display()))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let owner = read_device_owner(&lock_path)
                .map(|owner| format!("\n{}", owner.describe()))
                .unwrap_or_default();
            return Err(format!(
                "the TH420 device is already owned by another process{owner}"
            ));
        }
        write_owner_record(&mut lock)
            .map_err(|error| format!("cannot record device ownership: {error}"))?;
        Ok(Self { lock_file: lock })
    }
}

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        let _ = self.lock_file.set_len(0);
        let _ = unsafe { libc::flock(self.lock_file.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl InstanceGuard {
    pub fn try_acquire(
        kind: InstanceKind,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Result<Self, AcquireError> {
        let (lock_path, socket_path) = instance_paths(kind).map_err(AcquireError::Other)?;
        Self::try_acquire_paths(lock_path, socket_path, shutdown_requested, None)
    }

    pub fn try_acquire_daemon(
        shutdown_requested: Arc<AtomicBool>,
        control: Arc<DaemonControl>,
    ) -> Result<Self, AcquireError> {
        let (lock_path, socket_path) =
            instance_paths(InstanceKind::Daemon).map_err(AcquireError::Other)?;
        Self::try_acquire_paths(lock_path, socket_path, shutdown_requested, Some(control))
    }

    fn try_acquire_paths(
        lock_path: PathBuf,
        socket_path: PathBuf,
        shutdown_requested: Arc<AtomicBool>,
        daemon_control: Option<Arc<DaemonControl>>,
    ) -> Result<Self, AcquireError> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(|error| {
                AcquireError::Other(format!(
                    "cannot open instance lock {}: {error}",
                    lock_path.display()
                ))
            })?;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(AcquireError::Other(format!(
                    "cannot lock {}: {error}",
                    lock_path.display()
                )));
            }
            if let Some(owner) = read_owner(&lock_path, &socket_path).filter(validate_owner) {
                return Err(AcquireError::Conflict(owner));
            }
            if Instant::now() >= deadline {
                return Err(AcquireError::Other(format!(
                    "instance lock {} is held but its owner metadata could not be validated; no process was signaled",
                    lock_path.display()
                )));
            }
            thread::sleep(Duration::from_millis(25));
        }
        {
            let owner_path = owner_path(&lock_path);
            let _ = fs::remove_file(&owner_path);

            let _ = fs::remove_file(&socket_path);
            let listener = match UnixListener::bind(&socket_path) {
                Ok(listener) => listener,
                Err(error) => {
                    return Err(AcquireError::Other(format!(
                        "cannot create control socket {}: {error}",
                        socket_path.display()
                    )));
                }
            };
            let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
            if let Err(error) = listener.set_nonblocking(true) {
                let _ = fs::remove_file(&socket_path);
                return Err(AcquireError::Other(format!(
                    "cannot configure control socket: {error}"
                )));
            }
            if let Err(error) = write_instance_owner(&owner_path) {
                let _ = fs::remove_file(&socket_path);
                return Err(AcquireError::Other(format!(
                    "cannot publish instance owner: {error}"
                )));
            }
            let listener_stop = Arc::new(AtomicBool::new(false));
            let thread_stop = listener_stop.clone();
            let thread_control = daemon_control.clone();
            let listener_thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if let Ok(command) = read_control_command(&mut stream) {
                                if command == "shutdown" {
                                    shutdown_requested.store(true, Ordering::SeqCst);
                                    let _ = stream.write_all(b"ok\n");
                                } else if let Some(control) = &thread_control {
                                    let response =
                                        match control.command(&command, Duration::from_secs(5)) {
                                            Ok(state) => format!("ok {state}\n"),
                                            Err(error) => format!("error {error}\n"),
                                        };
                                    let _ = stream.write_all(response.as_bytes());
                                } else {
                                    let _ = stream.write_all(b"error unsupported command\n");
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(25));
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(Self {
                lock_file: lock,
                owner_path,
                socket_path,
                listener_stop,
                listener: Some(listener_thread),
                daemon_control,
            })
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(control) = &self.daemon_control {
            control.stop();
        }
        self.listener_stop.store(true, Ordering::Relaxed);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.owner_path);
        let _ = unsafe { libc::flock(self.lock_file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn request_graceful(owner: &OwnerInfo, timeout: Duration) -> Result<(), String> {
    let verified = revalidate_owner(owner)?;
    let mut stream = UnixStream::connect(&verified.socket_path).map_err(|error| {
        format!(
            "cannot connect to {}: {error}",
            verified.socket_path.display()
        )
    })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"shutdown\n")
        .map_err(|error| format!("cannot request graceful shutdown: {error}"))?;
    wait_for_release(&verified, timeout)
}

pub fn request_daemon_command(
    owner: &OwnerInfo,
    command: &str,
    timeout: Duration,
) -> Result<String, String> {
    if !matches!(command, "pause" | "resume" | "state" | "telemetry") {
        return Err(format!("unsupported daemon command: {command}"));
    }
    let verified = revalidate_owner(owner)?;
    let mut stream = UnixStream::connect(&verified.socket_path).map_err(|error| {
        format!(
            "cannot connect to {}: {error}",
            verified.socket_path.display()
        )
    })?;
    stream
        .set_read_timeout(Some(timeout + Duration::from_secs(1)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|error| format!("cannot request daemon {command}: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("cannot read daemon response: {error}"))?;
    let response = response.trim();
    if let Some(state) = response.strip_prefix("ok ") {
        Ok(state.to_string())
    } else if let Some(error) = response.strip_prefix("error ") {
        Err(error.to_string())
    } else {
        Err(format!("invalid daemon response: {response}"))
    }
}

pub fn signal_owner(owner: &OwnerInfo, signal: i32, timeout: Duration) -> Result<(), String> {
    let verified = revalidate_owner(owner)?;
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, verified.pid as i32, 0) as i32 };
    let result = if pidfd >= 0 {
        revalidate_owner(&verified)?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd,
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            ) as i32
        };
        unsafe { libc::close(pidfd) };
        result
    } else {
        revalidate_owner(&verified)?;
        unsafe { libc::kill(verified.pid as i32, signal) }
    };
    if result != 0 {
        return Err(format!(
            "failed to signal PID {}: {}",
            verified.pid,
            std::io::Error::last_os_error()
        ));
    }
    wait_for_release(&verified, timeout)
}

pub fn replace_and_acquire(
    kind: InstanceKind,
    shutdown_requested: Arc<AtomicBool>,
    level: ReplaceExisting,
) -> Result<InstanceGuard, String> {
    replace_and_acquire_controlled(kind, shutdown_requested, level, None)
}

pub fn replace_and_acquire_daemon(
    shutdown_requested: Arc<AtomicBool>,
    level: ReplaceExisting,
    control: Arc<DaemonControl>,
) -> Result<InstanceGuard, String> {
    replace_and_acquire_controlled(
        InstanceKind::Daemon,
        shutdown_requested,
        level,
        Some(control),
    )
}

fn replace_and_acquire_controlled(
    kind: InstanceKind,
    shutdown_requested: Arc<AtomicBool>,
    level: ReplaceExisting,
    control: Option<Arc<DaemonControl>>,
) -> Result<InstanceGuard, String> {
    let owner = match try_acquire_controlled(kind, shutdown_requested.clone(), control.clone()) {
        Ok(guard) => return Ok(guard),
        Err(AcquireError::Conflict(owner)) => owner,
        Err(AcquireError::Other(error)) => return Err(error),
    };
    let mut attempts = Vec::new();
    attempts.push("graceful shutdown".to_string());
    if request_graceful(&owner, Duration::from_secs(3)).is_ok() {
        return acquire_after_exit(kind, shutdown_requested, control);
    }
    if matches!(level, ReplaceExisting::Graceful) {
        return Err(replacement_failure(&owner, &attempts));
    }
    attempts.push("SIGTERM".to_string());
    if signal_owner(&owner, libc::SIGTERM, Duration::from_secs(3)).is_ok() {
        return acquire_after_exit(kind, shutdown_requested, control);
    }
    if matches!(level, ReplaceExisting::Term) {
        return Err(replacement_failure(&owner, &attempts));
    }
    attempts.push("SIGKILL".to_string());
    signal_owner(&owner, libc::SIGKILL, Duration::from_secs(3))
        .map_err(|_| replacement_failure(&owner, &attempts))?;
    acquire_after_exit(kind, shutdown_requested, control)
}

fn acquire_after_exit(
    kind: InstanceKind,
    shutdown_requested: Arc<AtomicBool>,
    control: Option<Arc<DaemonControl>>,
) -> Result<InstanceGuard, String> {
    try_acquire_controlled(kind, shutdown_requested, control).map_err(|error| match error {
        AcquireError::Conflict(owner) => format!(
            "another instance acquired ownership during replacement:\n{}",
            owner.describe()
        ),
        AcquireError::Other(error) => error,
    })
}

fn try_acquire_controlled(
    kind: InstanceKind,
    shutdown_requested: Arc<AtomicBool>,
    control: Option<Arc<DaemonControl>>,
) -> Result<InstanceGuard, AcquireError> {
    match control {
        Some(control) => InstanceGuard::try_acquire_daemon(shutdown_requested, control),
        None => InstanceGuard::try_acquire(kind, shutdown_requested),
    }
}

fn replacement_failure(owner: &OwnerInfo, attempts: &[String]) -> String {
    let owner = refresh_owner(owner).unwrap_or_else(|_| owner.clone());
    format!(
        "could not replace the existing instance.\nAttempted: {}\n\n{}",
        attempts.join(", "),
        owner.describe()
    )
}

pub fn refresh_owner(owner: &OwnerInfo) -> Result<OwnerInfo, String> {
    revalidate_owner(owner)
}

#[allow(dead_code)] // Used by the GUI's service manager.
pub fn current_owner(kind: InstanceKind) -> Option<OwnerInfo> {
    let (lock_path, socket_path) = instance_paths(kind).ok()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .ok()?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    read_owner(&lock_path, &socket_path).filter(validate_owner)
}

fn wait_for_release(owner: &OwnerInfo, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if lock_is_available(&owner.lock_path) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "PID {} did not release ownership within {:?}",
        owner.pid, timeout
    ))
}

fn read_control_command(mut reader: impl Read) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(16);
    loop {
        let mut byte = [0_u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => return Err("control command ended before newline".into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                if bytes.len() == 32 {
                    return Err("control command is too long".into());
                }
                bytes.push(byte[0]);
            }
            Err(error) => return Err(format!("cannot read control command: {error}")),
        }
    }
    String::from_utf8(bytes)
        .map(|command| command.trim_end_matches('\r').to_string())
        .map_err(|_| "control command is not UTF-8".into())
}

fn lock_is_available(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    else {
        return false;
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return false;
    }
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    true
}

fn revalidate_owner(expected: &OwnerInfo) -> Result<OwnerInfo, String> {
    let current = read_owner(&expected.lock_path, &expected.socket_path)
        .ok_or_else(|| "the recorded instance no longer exists".to_string())?;
    if current.pid != expected.pid
        || current.uid != expected.uid
        || current.start_ticks != expected.start_ticks
        || current.executable != expected.executable
        || current.executable_identity != expected.executable_identity
        || !validate_owner(&current)
    {
        return Err("instance identity changed; refusing to signal it".into());
    }
    Ok(current)
}

fn validate_owner(owner: &OwnerInfo) -> bool {
    owner.uid == effective_uid()
        && process_uid(owner.pid) == Some(owner.uid)
        && process_start_ticks(owner.pid) == Some(owner.start_ticks)
        && match owner.executable_identity {
            Some(identity) => process_executable_identity(owner.pid) == Some(identity),
            None => process_executable(owner.pid)
                .as_ref()
                .is_some_and(|current| legacy_executable_matches(&owner.executable, current)),
        }
}

fn legacy_executable_matches(recorded: &Path, current: &Path) -> bool {
    current == recorded
        || current
            .as_os_str()
            .as_encoded_bytes()
            .strip_suffix(b" (deleted)")
            == Some(recorded.as_os_str().as_encoded_bytes())
}

fn owner_path(lock_path: &Path) -> PathBuf {
    lock_path.with_extension("owner")
}

fn instance_paths(kind: InstanceKind) -> Result<(PathBuf, PathBuf), String> {
    let uid = effective_uid();
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| runtime_dir_is_safe(path, uid))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/th420-display-{uid}")));
    if !base.exists() {
        fs::create_dir(&base)
            .map_err(|error| format!("cannot create {}: {error}", base.display()))?;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("cannot secure {}: {error}", base.display()))?;
    }
    if !runtime_dir_is_safe(&base, uid) {
        return Err(format!("unsafe runtime directory: {}", base.display()));
    }
    let stem = format!("th420-display-{}", kind.name());
    Ok((
        base.join(format!("{stem}.lock")),
        base.join(format!("{stem}.sock")),
    ))
}

fn runtime_dir_is_safe(path: &Path, uid: u32) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o022 == 0)
        .unwrap_or(false)
}

fn read_owner(lock_path: &Path, socket_path: &Path) -> Option<OwnerInfo> {
    let sidecar = read_owner_file(&owner_path(lock_path), lock_path, socket_path);
    if sidecar.as_ref().is_some_and(validate_owner) {
        return sidecar;
    }
    read_owner_file(lock_path, lock_path, socket_path)
}

fn read_owner_file(path: &Path, lock_path: &Path, socket_path: &Path) -> Option<OwnerInfo> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.uid() != effective_uid() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
    };
    let pid = field("pid")?.parse().ok()?;
    let uid = field("uid")?.parse().ok()?;
    let start_ticks = field("start")?.parse().ok()?;
    let executable = PathBuf::from(unsafe {
        std::ffi::OsString::from_encoded_bytes_unchecked(hex_decode(field("exe")?).ok()?)
    });
    let executable_identity = match (field("exe_dev"), field("exe_ino")) {
        (Some(dev), Some(ino)) => Some((dev.parse().ok()?, ino.parse().ok()?)),
        (None, None) => None,
        _ => return None,
    };
    Some(OwnerInfo {
        pid,
        uid,
        start_ticks,
        executable,
        executable_identity,
        command_line: process_command_line(pid),
        lock_path: lock_path.to_owned(),
        socket_path: socket_path.to_owned(),
    })
}

#[allow(dead_code)]
fn read_device_owner(lock_path: &Path) -> Option<OwnerInfo> {
    read_owner_file(lock_path, lock_path, Path::new("<none>"))
}

fn write_owner_record(lock: &mut File) -> std::io::Result<()> {
    let record = owner_record()?;
    lock.set_len(0)?;
    lock.rewind()?;
    lock.write_all(record.as_bytes())?;
    lock.sync_all()
}

fn write_instance_owner(path: &Path) -> std::io::Result<()> {
    let record = owner_record()?;
    let temporary = path.with_extension(format!("owner.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(record.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn owner_record() -> std::io::Result<String> {
    let pid = std::process::id();
    let uid = effective_uid();
    let start_ticks = process_start_ticks(pid)
        .ok_or_else(|| std::io::Error::other("cannot read own process start time"))?;
    let executable = process_executable(pid)
        .ok_or_else(|| std::io::Error::other("cannot read own executable"))?;
    let (exe_dev, exe_ino) = process_executable_identity(pid)
        .ok_or_else(|| std::io::Error::other("cannot read own executable identity"))?;
    let token = format!(
        "{}-{}-{}",
        pid,
        start_ticks,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    Ok(format!(
        "pid={pid}\nuid={uid}\nstart={start_ticks}\nexe={}\nexe_dev={exe_dev}\nexe_ino={exe_ino}\ntoken={token}\n",
        hex_encode(executable.as_os_str().as_encoded_bytes())
    ))
}

fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    after_name.split_whitespace().nth(19)?.parse().ok()
}

fn process_uid(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn process_executable(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

fn process_executable_identity(pid: u32) -> Option<(u64, u64)> {
    let metadata = fs::metadata(format!("/proc/{pid}/exe")).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

fn process_command_line(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(sanitize)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| "<unavailable>".into())
}

fn sanitize(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect()
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn user_name(uid: u32) -> String {
    std::env::var("USER").unwrap_or_else(|_| uid.to_string())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(value: &str) -> Result<Vec<u8>, ()> {
    if !value.len().is_multiple_of(2) {
        return Err(());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_parser_reads_current_process_identity() {
        let pid = std::process::id();
        assert!(process_start_ticks(pid).is_some());
        assert_eq!(process_uid(pid), Some(effective_uid()));
        assert!(process_executable(pid).is_some());
        assert!(process_executable_identity(pid).is_some());
    }

    #[test]
    fn replaced_executable_keeps_valid_owner_identity() {
        let pid = std::process::id();
        let original = process_executable(pid).unwrap();
        let owner = OwnerInfo {
            pid,
            uid: effective_uid(),
            start_ticks: process_start_ticks(pid).unwrap(),
            executable: PathBuf::from("/tmp/old-executable-path"),
            executable_identity: process_executable_identity(pid),
            command_line: String::new(),
            lock_path: PathBuf::new(),
            socket_path: PathBuf::new(),
        };
        assert!(validate_owner(&owner));
        let mut wrong_identity = owner.clone();
        wrong_identity.executable_identity = Some((0, 0));
        assert!(!validate_owner(&wrong_identity));
        assert!(legacy_executable_matches(&original, &original));
        let deleted = PathBuf::from(format!("{} (deleted)", original.display()));
        assert!(legacy_executable_matches(&original, &deleted));
        assert!(!legacy_executable_matches(
            Path::new("/tmp/unrelated"),
            &deleted
        ));
    }

    #[test]
    fn contention_waits_for_owner_metadata_publication() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "th420-owner-publication-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let lock_path = directory.join("test.lock");
        let socket_path = directory.join("test.sock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let metadata_path = owner_path(&lock_path);
        let publisher = thread::spawn(move || {
            thread::sleep(Duration::from_millis(75));
            write_instance_owner(&metadata_path).unwrap();
            metadata_path
        });
        let result = InstanceGuard::try_acquire_paths(
            lock_path.clone(),
            socket_path,
            Arc::new(AtomicBool::new(false)),
            None,
        );
        assert!(matches!(result, Err(AcquireError::Conflict(_))));
        let metadata_path = publisher.join().unwrap();
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        fs::remove_file(metadata_path).unwrap();
        fs::remove_file(lock_path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn hex_round_trip_handles_arbitrary_path_bytes() {
        let bytes = b"/tmp/path with spaces/and-newline\n";
        assert_eq!(hex_decode(&hex_encode(bytes)).unwrap(), bytes);
    }

    #[test]
    fn command_line_sanitizer_removes_control_characters() {
        assert_eq!(sanitize(b"hello\nworld\t"), "hello?world?");
    }

    #[test]
    fn daemon_pause_resume_is_acknowledged_and_idempotent() {
        let control = DaemonControl::new_starting();
        control.mark_running();
        let requester = {
            let control = control.clone();
            thread::spawn(move || control.command("pause", Duration::from_secs(1)))
        };
        while !control.pause_requested() {
            thread::yield_now();
        }
        control.mark_paused();
        assert_eq!(requester.join().unwrap().unwrap(), "paused changed");
        assert_eq!(
            control.command("pause", Duration::from_millis(10)).unwrap(),
            "paused unchanged"
        );

        let requester = {
            let control = control.clone();
            thread::spawn(move || control.command("resume", Duration::from_secs(1)))
        };
        while control.pause_requested() {
            thread::yield_now();
        }
        control.mark_running();
        assert_eq!(requester.join().unwrap().unwrap(), "running changed");
        assert_eq!(
            control
                .command("resume", Duration::from_millis(10))
                .unwrap(),
            "running unchanged"
        );
    }

    #[test]
    fn daemon_failed_resume_stays_paused_and_reports_error() {
        let control = DaemonControl::new_starting();
        control.mark_running();
        let pause = {
            let control = control.clone();
            thread::spawn(move || control.command("pause", Duration::from_secs(1)))
        };
        while !control.pause_requested() {
            thread::yield_now();
        }
        control.mark_paused();
        pause.join().unwrap().unwrap();

        let resume = {
            let control = control.clone();
            thread::spawn(move || control.command("resume", Duration::from_secs(1)))
        };
        while control.pause_requested() {
            thread::yield_now();
        }
        control.mark_resume_failed("device remains busy".into());
        assert_eq!(resume.join().unwrap().unwrap_err(), "device remains busy");
        assert_eq!(control.command("state", Duration::ZERO).unwrap(), "paused");
    }

    #[test]
    fn daemon_telemetry_invalidates_failed_reading_and_recovers() {
        let control = DaemonControl::new_starting();
        assert!(control
            .command("telemetry", Duration::ZERO)
            .unwrap_err()
            .contains("not available"));

        control.update_telemetry(30.97, 2320);
        let sample = control.command("telemetry", Duration::ZERO).unwrap();
        assert!(sample.contains("coolant_temp_c=30.97\n"));
        assert!(sample.contains("pump_rpm=2320"));

        control.mark_telemetry_error("temporary read failure".into());
        assert_eq!(
            control.command("telemetry", Duration::ZERO).unwrap_err(),
            "temporary read failure"
        );
        control.update_telemetry(31.0, 2400);
        let recovered = control.command("telemetry", Duration::ZERO).unwrap();
        assert!(recovered.contains("coolant_temp_c=31.0"));
        assert!(recovered.contains("pump_rpm=2400"));
    }

    #[test]
    fn control_command_requires_complete_bounded_line() {
        assert_eq!(
            read_control_command(std::io::Cursor::new(b"pause\n")).unwrap(),
            "pause"
        );
        assert!(read_control_command(std::io::Cursor::new(b"pau")).is_err());
        assert!(read_control_command(std::io::Cursor::new([b'x'; 34])).is_err());
    }

    #[test]
    fn daemon_stop_wakes_a_pending_transition() {
        let control = DaemonControl::new_starting();
        control.mark_paused();
        let requester = {
            let control = control.clone();
            thread::spawn(move || control.command("resume", Duration::from_secs(5)))
        };
        while control.pause_requested() {
            thread::yield_now();
        }
        control.stop();
        assert_eq!(requester.join().unwrap().unwrap_err(), "daemon is stopping");
    }

    #[test]
    fn duplicate_is_rejected_and_graceful_shutdown_hands_off_lock() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "th420-instance-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let lock_path = directory.join("test.lock");
        let socket_path = directory.join("test.sock");
        let shutdown = Arc::new(AtomicBool::new(false));
        let guard = InstanceGuard::try_acquire_paths(
            lock_path.clone(),
            socket_path.clone(),
            shutdown.clone(),
            None,
        )
        .unwrap();
        assert!(owner_path(&lock_path).exists());
        assert!(fs::read_to_string(&lock_path).unwrap().is_empty());
        let owner = match InstanceGuard::try_acquire_paths(
            lock_path.clone(),
            socket_path.clone(),
            Arc::new(AtomicBool::new(false)),
            None,
        ) {
            Err(AcquireError::Conflict(owner)) => owner,
            _ => panic!("second acquisition must report the verified owner"),
        };
        let dropper = thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            drop(guard);
        });
        request_graceful(&owner, Duration::from_secs(2)).unwrap();
        dropper.join().unwrap();
        let replacement = InstanceGuard::try_acquire_paths(
            lock_path.clone(),
            socket_path.clone(),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .unwrap();
        drop(replacement);
        let _ = fs::remove_file(lock_path);
        fs::remove_dir(directory).unwrap();
    }
}
