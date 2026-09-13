use std::path::{Path, PathBuf};
use std::process::Command;

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
        match self.kind {
            BackendKind::Systemd => command_succeeds(
                "systemctl",
                &["--user", "is-active", "--quiet", SERVICE_NAME],
            ),
            // A runit host may have the service files installed but leave the
            // service disabled. In that case the GUI's Start button uses the
            // same direct-launch behavior as unmanaged mode.
            BackendKind::Runit => runit_is_running() || daemon_pid().is_some(),
            BackendKind::Unmanaged => daemon_pid().is_some(),
        }
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

    pub fn start(&self, daemon: &Path) {
        match self.kind {
            BackendKind::Systemd => {
                if install_systemd_unit(daemon) {
                    run_command("systemctl", &["--user", "start", SERVICE_NAME]);
                }
            }
            BackendKind::Runit => {
                if !runit_service_enabled(Path::new(RUNIT_ACTIVE_SERVICE))
                    || !run_command("sv", &["up", RUNIT_ACTIVE_SERVICE])
                {
                    let _ = Command::new(daemon).spawn();
                }
            }
            BackendKind::Unmanaged => {
                let _ = Command::new(daemon).spawn();
            }
        }
    }

    pub fn stop(&self) {
        match self.kind {
            BackendKind::Systemd => {
                let _ = run_command("systemctl", &["--user", "stop", SERVICE_NAME]);
            }
            BackendKind::Runit => {
                if !runit_service_enabled(Path::new(RUNIT_ACTIVE_SERVICE))
                    || !run_command("sv", &["down", RUNIT_ACTIVE_SERVICE])
                {
                    stop_direct_daemon();
                }
            }
            BackendKind::Unmanaged => {
                stop_direct_daemon();
            }
        }
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
                    let _ = Command::new(daemon).spawn();
                }
            }
            BackendKind::Unmanaged => {
                self.stop();
                self.start(daemon);
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

fn runit_is_running() -> bool {
    let output = match Command::new("sv")
        .args(["status", RUNIT_ACTIVE_SERVICE])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with("run:")
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

fn daemon_pid() -> Option<u32> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cmdline = std::fs::read_to_string(entry.path().join("cmdline")).unwrap_or_default();
        if cmdline
            .split('\0')
            .next()
            .unwrap_or("")
            .ends_with(SERVICE_NAME)
        {
            return name.parse().ok();
        }
    }
    None
}

fn stop_direct_daemon() {
    if let Some(pid) = daemon_pid() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
