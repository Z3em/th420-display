use crate::instance::{current_owner, request_daemon_command, request_graceful, InstanceKind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const SERVICE_NAME: &str = "th420-display";
const RUNIT_SERVICE_DIR: &str = "/etc/sv/th420-display";
const RUNIT_ACTIVE_SERVICE: &str = "/var/service/th420-display";

/// The init system responsible for managing the display daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Systemd,
    Runit,
    Unmanaged,
}

impl BackendKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Runit => "runit",
            Self::Unmanaged => "unmanaged",
        }
    }

    pub const fn supports_autostart(self) -> bool {
        !matches!(self, Self::Unmanaged)
    }
}

/// Select a backend from testable PID 1 metadata.  `/proc/1/comm` normally
/// contains just the executable name, while `/proc/1/exe` is a symlink.
pub fn backend_from_pid1(comm: Option<&str>, exe: Option<&Path>) -> BackendKind {
    let comm_name = comm.and_then(init_name);
    match comm_name.and_then(backend_from_name) {
        Some(backend) => backend,
        None => exe
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .and_then(backend_from_name)
            .unwrap_or(BackendKind::Unmanaged),
    }
}

fn init_name(value: &str) -> Option<&str> {
    let value = value.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
    (!value.is_empty()).then_some(value)
}

fn backend_from_name(name: &str) -> Option<BackendKind> {
    match name {
        "systemd" => Some(BackendKind::Systemd),
        "runit" => Some(BackendKind::Runit),
        _ => None,
    }
}

pub fn detect_backend() -> BackendKind {
    let comm = std::fs::read_to_string("/proc/1/comm").ok();
    let exe = std::fs::read_link("/proc/1/exe").ok();
    backend_from_pid1(comm.as_deref(), exe.as_deref())
}

pub struct ServiceManager {
    kind: BackendKind,
}

impl ServiceManager {
    pub fn detect() -> Self {
        Self {
            kind: detect_backend(),
        }
    }

    #[cfg(test)]
    fn for_kind(kind: BackendKind) -> Self {
        Self { kind }
    }

    pub const fn kind(&self) -> BackendKind {
        self.kind
    }

    pub fn daemon_running(&self) -> bool {
        current_owner(InstanceKind::Daemon).is_some()
    }

    pub fn autostart_enabled(&self) -> bool {
        match self.kind {
            BackendKind::Systemd => {
                command_succeeds("systemctl", &["--user", "is-enabled", SERVICE_NAME])
            }
            BackendKind::Runit => runit_service_enabled(Path::new(RUNIT_ACTIVE_SERVICE)),
            BackendKind::Unmanaged => false,
        }
    }

    pub fn start(&self, daemon: &Path) -> bool {
        if self.daemon_running() {
            return true;
        }
        let requested = match self.kind {
            BackendKind::Systemd => {
                if install_systemd_unit(daemon) {
                    run_command("systemctl", &["--user", "start", SERVICE_NAME])
                } else {
                    false
                }
            }
            BackendKind::Runit => {
                if !runit_service_enabled(Path::new(RUNIT_ACTIVE_SERVICE))
                    || !run_command("sv", &["up", RUNIT_ACTIVE_SERVICE])
                {
                    spawn_reaped(daemon)
                } else {
                    true
                }
            }
            BackendKind::Unmanaged => spawn_reaped(daemon),
        };
        requested && wait_for_daemon_start(Duration::from_secs(5))
    }

    pub fn stop(&self) -> bool {
        match self.kind {
            BackendKind::Systemd => {
                let _ = run_command("systemctl", &["--user", "stop", SERVICE_NAME]);
            }
            BackendKind::Runit => {
                if !runit_service_enabled(Path::new(RUNIT_ACTIVE_SERVICE))
                    || !run_command("sv", &["down", RUNIT_ACTIVE_SERVICE])
                {
                    return stop_direct_daemon();
                }
            }
            BackendKind::Unmanaged => {
                return stop_direct_daemon();
            }
        }
        wait_for_daemon_stop(Duration::from_secs(5))
    }

    pub fn restart(&self, daemon: &Path) {
        match self.kind {
            BackendKind::Systemd => {
                if install_systemd_unit(daemon) {
                    run_command("systemctl", &["--user", "restart", SERVICE_NAME]);
                }
            }
            BackendKind::Runit => {
                if !runit_service_enabled(Path::new(RUNIT_ACTIVE_SERVICE))
                    || !run_command("sv", &["restart", RUNIT_ACTIVE_SERVICE])
                {
                    stop_direct_daemon();
                    spawn_reaped(daemon);
                }
            }
            BackendKind::Unmanaged => {
                self.stop();
                let _ = self.start(daemon);
            }
        }
    }

    pub fn enable_autostart(&self, daemon: &Path) {
        match self.kind {
            BackendKind::Systemd => {
                if install_systemd_unit(daemon) {
                    run_command("systemctl", &["--user", "enable", SERVICE_NAME]);
                }
            }
            BackendKind::Runit => enable_runit(),
            BackendKind::Unmanaged => {}
        }
    }

    pub fn disable_autostart(&self) {
        match self.kind {
            BackendKind::Systemd => {
                let _ = run_command("systemctl", &["--user", "disable", SERVICE_NAME]);
            }
            BackendKind::Runit => disable_runit(),
            BackendKind::Unmanaged => {}
        }
    }
}

fn spawn_reaped(daemon: &Path) -> bool {
    let Ok(mut child) = Command::new(daemon).spawn() else {
        return false;
    };
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    true
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn run_command(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn install_systemd_unit(daemon: &Path) -> bool {
    let service_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("systemd/user");
    if std::fs::create_dir_all(&service_dir).is_err() {
        return false;
    }
    let content = format!(
        "[Unit]\nDescription=Thermaltake TH420 V2 LCD display daemon\nAfter=graphical-session.target\n\n\
         [Service]\nExecStart={}\nRestart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=default.target\n",
        daemon.to_string_lossy()
    );
    if std::fs::write(service_dir.join("th420-display.service"), content).is_err() {
        return false;
    }
    let _ = run_command("systemctl", &["--user", "daemon-reload"]);
    true
}

fn runit_service_enabled(active_service: &Path) -> bool {
    std::fs::symlink_metadata(active_service)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn enable_runit() {
    let service_dir = Path::new(RUNIT_SERVICE_DIR);
    let active_service = Path::new(RUNIT_ACTIVE_SERVICE);
    if !service_dir.is_dir()
        || active_service.exists()
        || std::fs::symlink_metadata(active_service).is_ok()
    {
        return;
    }
    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(service_dir, active_service);
    }
}

fn disable_runit() {
    let active_service = Path::new(RUNIT_ACTIVE_SERVICE);
    if runit_service_enabled(active_service) {
        let _ = std::fs::remove_file(active_service);
    }
}

fn stop_direct_daemon() -> bool {
    let Some(owner) = current_owner(InstanceKind::Daemon) else {
        return true;
    };
    request_graceful(&owner, Duration::from_secs(5)).is_ok()
}

fn wait_for_daemon_stop(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if current_owner(InstanceKind::Daemon).is_none() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_daemon_start(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Some(owner) = current_owner(InstanceKind::Daemon) {
            if request_daemon_command(&owner, "state", Duration::from_millis(200))
                .is_ok_and(|state| daemon_state_is_ready(&state))
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn daemon_state_is_ready(state: &str) -> bool {
    state == "running"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_waits_for_running_state_not_just_an_instance_lock() {
        for state in ["starting", "pausing", "paused", "stopping"] {
            assert!(!daemon_state_is_ready(state));
        }
        assert!(daemon_state_is_ready("running"));
    }

    #[test]
    fn pid1_comm_selects_supported_backends() {
        assert_eq!(
            backend_from_pid1(Some("systemd\n"), None),
            BackendKind::Systemd
        );
        assert_eq!(backend_from_pid1(Some("runit\0"), None), BackendKind::Runit);
    }

    #[test]
    fn pid1_exe_is_used_when_comm_is_missing_or_unknown() {
        assert_eq!(
            backend_from_pid1(None, Some(Path::new("/usr/lib/systemd/systemd"))),
            BackendKind::Systemd
        );
        assert_eq!(
            backend_from_pid1(Some("init"), Some(Path::new("/sbin/runit"))),
            BackendKind::Runit
        );
    }

    #[test]
    fn unsupported_or_malformed_pid1_uses_unmanaged() {
        assert_eq!(backend_from_pid1(Some(""), None), BackendKind::Unmanaged);
        assert_eq!(
            backend_from_pid1(Some("busybox"), Some(Path::new("/sbin/init"))),
            BackendKind::Unmanaged
        );
    }

    #[test]
    fn unmanaged_backend_has_no_autostart() {
        let manager = ServiceManager::for_kind(BackendKind::Unmanaged);
        assert!(!manager.kind().supports_autostart());
        assert!(!manager.autostart_enabled());
    }
}
