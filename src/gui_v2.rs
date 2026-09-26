mod config;
mod instance;
mod renderer;
mod sensors;
mod service_manager;

use clap::Parser;
use config::{
    default_config_path, Config, ImageFit, LayoutPreset, LayoutSlot, MediaTransform, Transform2D,
};
use eframe::egui;
use image::codecs::gif::GifDecoder;
use image::AnimationDecoder;
use instance::{
    refresh_owner, replace_and_acquire, request_graceful, signal_owner, AcquireError,
    InstanceGuard, InstanceKind, OwnerInfo, ReplaceExisting,
};
use renderer::{
    load_media_frame_at, media_duration, transform_media_image, DragSnapState, Renderer,
    TimedAnimation,
};
use sensors::SensorValues;
use service_manager::ServiceManager;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Overview,
    LiveDisplay,
    StandbySettings,
    Settings,
}

impl Page {
    const ALL: [(Page, &'static str); 4] = [
        (Page::Overview, "Overview"),
        (Page::LiveDisplay, "Live Display"),
        (Page::StandbySettings, "Standby Settings"),
        (Page::Settings, "Settings"),
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveTab {
    Background,
    Overlay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StandbyTab {
    Boot,
    Standby,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewMode {
    Boot,
    Standby,
}

impl PreviewMode {
    fn label(self) -> &'static str {
        match self {
            Self::Boot => "Boot",
            Self::Standby => "Standby",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevicePreviewMode {
    Off,
    BootLoop,
    BootOnce,
    Standby,
}

impl DevicePreviewMode {
    const ALL: [Self; 4] = [Self::Off, Self::BootLoop, Self::BootOnce, Self::Standby];

    fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::BootLoop => "Boot (loop)",
            Self::BootOnce => "Boot (once)",
            Self::Standby => "Standby",
        }
    }

    fn is_boot(self) -> bool {
        matches!(self, Self::BootLoop | Self::BootOnce)
    }

    fn loops(self) -> usize {
        if matches!(self, Self::BootOnce) {
            1
        } else {
            0
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundSource {
    File,
    Stream,
    SolidColor,
}

impl BackgroundSource {
    const ALL: [Self; 3] = [Self::File, Self::Stream, Self::SolidColor];

    fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Stream => "Stream",
            Self::SolidColor => "Solid Color",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct DeviceInfo {
    connected: bool,
    vid: String,
    pid: String,
    product: Option<String>,
    manufacturer: Option<String>,
    revision: Option<String>,
    control_hidraw: Option<PathBuf>,
    image_hidraw: Option<PathBuf>,
}

struct DevicePreviewJob {
    child: Child,
    mode: DevicePreviewMode,
    restore_live_display: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct BootInspectionKey {
    path: PathBuf,
    transform: MediaTransform,
}

#[derive(Clone, Copy, Debug)]
struct BootEstimate {
    frames: usize,
    delay_ms: u32,
    container_bytes: usize,
    limit_bytes: usize,
    within_limit: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct StandbyFrameKey {
    path: PathBuf,
    time_bits: u64,
}

enum StandbyWorkerResult {
    Metadata(PathBuf, Result<f64, String>),
    Frame(StandbyFrameKey, Result<image::RgbImage, String>),
}

struct App {
    shutdown_requested: Arc<AtomicBool>,
    page: Page,
    live_tab: LiveTab,
    standby_tab: StandbyTab,
    preview_mode: PreviewMode,
    device_preview_mode: DevicePreviewMode,
    background_source: BackgroundSource,
    background_file_path: Option<String>,

    committed: Config,
    working: Config,
    config_path: PathBuf,
    renderer: Renderer,
    sensors: sensors::SensorReader,
    sensor_values: SensorValues,
    last_sensor_update: Instant,

    service_manager: ServiceManager,
    live_display_enabled: bool,
    daemon_running: bool,
    autostart_enabled: bool,
    last_daemon_check: Instant,

    device_info: DeviceInfo,
    device_coolant: Option<f32>,
    device_pump_rpm: Option<u16>,
    last_device_scan: Instant,

    preview_texture: Option<egui::TextureHandle>,
    last_preview_update: Instant,

    selected_sensor: Option<String>,
    placing_sensor: Option<String>,
    snap_to_grid: bool,
    show_grid: bool,
    grid_size: f32,

    background_snap: bool,
    background_snap_px: f32,
    background_snap_degrees: f32,
    background_drag: Option<DragSnapState>,
    boot_drag: Option<DragSnapState>,
    standby_drag: Option<DragSnapState>,
    boot_transform: MediaTransform,
    standby_transform: MediaTransform,

    brightness: u8,
    boot_path: Option<PathBuf>,
    standby_path: Option<PathBuf>,
    boot_source_cache: Option<(PathBuf, image::RgbImage)>,
    boot_animation: Option<(PathBuf, TimedAnimation)>,
    boot_animation_pending: Option<PathBuf>,
    boot_animation_tx: Sender<(PathBuf, Result<TimedAnimation, String>)>,
    boot_animation_rx: Receiver<(PathBuf, Result<TimedAnimation, String>)>,
    standby_source_cache: Option<(PathBuf, u64, image::RgbImage)>,
    standby_time: f64,
    standby_duration: Option<f64>,
    standby_metadata_path: Option<PathBuf>,
    standby_frame_key: Option<StandbyFrameKey>,
    standby_frame_pending: bool,
    standby_worker_tx: Sender<StandbyWorkerResult>,
    standby_worker_rx: Receiver<StandbyWorkerResult>,
    last_standby_frame_change: Instant,
    boot_inspection_key: Option<BootInspectionKey>,
    boot_inspection_pending: Option<BootInspectionKey>,
    boot_estimate: Option<Result<BootEstimate, String>>,
    boot_inspection_tx: Sender<(BootInspectionKey, Result<BootEstimate, String>)>,
    boot_inspection_rx: Receiver<(BootInspectionKey, Result<BootEstimate, String>)>,
    last_boot_inspection_change: Instant,
    stream_path: Option<PathBuf>,
    device_preview_job: Option<DevicePreviewJob>,
    status_text: String,
    last_error: Option<String>,
}

#[derive(Parser)]
#[command(name = "th420-config", about = "TH420 Display configuration")]
struct GuiCli {
    /// Replace an existing GUI, escalating no further than this level.
    #[arg(long, value_enum, value_name = "graceful|term|kill")]
    replace_existing: Option<ReplaceExisting>,
}

fn main() -> eframe::Result<()> {
    let cli = GuiCli::parse();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let Some(_instance_guard) = acquire_gui_instance(&cli, shutdown_requested.clone()) else {
        return Ok(());
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([980.0, 660.0])
        .with_resizable(true)
        .with_title("TH420 Display");
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    eframe::run_native(
        "TH420 Display",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(move |_cc| Ok(Box::new(App::new(shutdown_requested)))),
    )
}

fn acquire_gui_instance(
    cli: &GuiCli,
    shutdown_requested: Arc<AtomicBool>,
) -> Option<InstanceGuard> {
    if let Some(level) = cli.replace_existing {
        return match replace_and_acquire(InstanceKind::Gui, shutdown_requested, level) {
            Ok(guard) => Some(guard),
            Err(error) => {
                show_instance_error(&error);
                None
            }
        };
    }
    let owner = match InstanceGuard::try_acquire(InstanceKind::Gui, shutdown_requested.clone()) {
        Ok(guard) => return Some(guard),
        Err(AcquireError::Conflict(owner)) => owner,
        Err(AcquireError::Other(error)) => {
            show_instance_error(&error);
            return None;
        }
    };
    let choice = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("TH420 Display is already running")
        .set_description(format!(
            "Another GUI instance is active.\n\n{}\n\nTerminate it and continue?",
            owner.describe()
        ))
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "Terminate other instance".into(),
            "Exit".into(),
        ))
        .show();
    let terminate = matches!(choice, rfd::MessageDialogResult::Ok)
        || matches!(
            choice,
            rfd::MessageDialogResult::Custom(ref label)
                if label == "Terminate other instance"
        );
    if !terminate {
        return None;
    }
    if request_graceful(&owner, Duration::from_secs(3)).is_ok() {
        return acquire_gui_after_replacement(shutdown_requested);
    }
    acquire_gui_with_escalation(owner, shutdown_requested)
}

fn acquire_gui_with_escalation(
    owner: OwnerInfo,
    shutdown_requested: Arc<AtomicBool>,
) -> Option<InstanceGuard> {
    let current = refresh_owner(&owner).unwrap_or(owner);
    let choice = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("GUI did not exit gracefully")
        .set_description(format!(
            "The process did not respond to graceful shutdown.\n\n{}\n\nChoose the maximum escalation to attempt.",
            current.describe()
        ))
        .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
            "SIGTERM".into(),
            "SIGKILL".into(),
            "Exit".into(),
        ))
        .show();
    let level = match choice {
        rfd::MessageDialogResult::Yes => ReplaceExisting::Term,
        rfd::MessageDialogResult::No => ReplaceExisting::Kill,
        rfd::MessageDialogResult::Custom(ref label) if label == "SIGTERM" => ReplaceExisting::Term,
        rfd::MessageDialogResult::Custom(ref label) if label == "SIGKILL" => ReplaceExisting::Kill,
        _ => return None,
    };
    match replace_after_graceful(current, shutdown_requested, level) {
        Ok(guard) => Some(guard),
        Err(error) => {
            show_instance_error(&error);
            None
        }
    }
}

fn replace_after_graceful(
    owner: OwnerInfo,
    shutdown_requested: Arc<AtomicBool>,
    level: ReplaceExisting,
) -> Result<InstanceGuard, String> {
    let mut attempts = vec!["graceful shutdown"];
    attempts.push("SIGTERM");
    if signal_owner(&owner, libc::SIGTERM, Duration::from_secs(3)).is_ok() {
        return acquire_gui_after_replacement(shutdown_requested)
            .ok_or_else(|| "another GUI acquired ownership during replacement".into());
    }
    if matches!(level, ReplaceExisting::Kill) {
        attempts.push("SIGKILL");
        signal_owner(&owner, libc::SIGKILL, Duration::from_secs(3))?;
        return acquire_gui_after_replacement(shutdown_requested)
            .ok_or_else(|| "another GUI acquired ownership during replacement".into());
    }
    Err(format!(
        "Could not replace the existing GUI.\nAttempted: {}\n\n{}",
        attempts.join(", "),
        refresh_owner(&owner).unwrap_or(owner).describe()
    ))
}

fn acquire_gui_after_replacement(shutdown_requested: Arc<AtomicBool>) -> Option<InstanceGuard> {
    InstanceGuard::try_acquire(InstanceKind::Gui, shutdown_requested).ok()
}

fn show_instance_error(error: &str) {
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Could not start TH420 Display")
        .set_description(error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

fn load_icon() -> Option<egui::IconData> {
    let image = image::load_from_memory(include_bytes!("../assets/th420-config.png"))
        .ok()?
        .into_rgba8();
    Some(egui::IconData {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

impl App {
    fn new(shutdown_requested: Arc<AtomicBool>) -> Self {
        let config_path = default_config_path();
        let config = Config::load(&config_path).unwrap_or_else(|_| {
            let config = Config::default();
            let _ = config.save(&config_path);
            config
        });
        let service_manager = ServiceManager::detect();
        let daemon_running = service_manager.daemon_running();
        let autostart_enabled = service_manager.autostart_enabled();
        let background_source = if config.background.image_path.is_some() {
            BackgroundSource::File
        } else {
            BackgroundSource::SolidColor
        };

        let (boot_inspection_tx, boot_inspection_rx) = mpsc::channel();
        let (boot_animation_tx, boot_animation_rx) = mpsc::channel();
        let (standby_worker_tx, standby_worker_rx) = mpsc::channel();
        let mut app = Self {
            shutdown_requested,
            page: Page::Overview,
            live_tab: LiveTab::Background,
            standby_tab: StandbyTab::Boot,
            preview_mode: PreviewMode::Boot,
            device_preview_mode: DevicePreviewMode::Off,
            background_source,
            background_file_path: config.background.image_path.clone(),
            committed: config.clone(),
            working: config,
            config_path,
            renderer: Renderer::new(),
            sensors: sensors::SensorReader::new(),
            sensor_values: SensorValues {
                readings: HashMap::new(),
            },
            last_sensor_update: Instant::now() - Duration::from_secs(10),
            service_manager,
            live_display_enabled: daemon_running,
            daemon_running,
            autostart_enabled,
            last_daemon_check: Instant::now(),
            device_info: detect_device_info(),
            device_coolant: None,
            device_pump_rpm: None,
            last_device_scan: Instant::now(),
            preview_texture: None,
            last_preview_update: Instant::now() - Duration::from_secs(10),
            selected_sensor: None,
            placing_sensor: None,
            snap_to_grid: true,
            show_grid: false,
            grid_size: 8.0,
            background_snap: true,
            background_snap_px: 8.0,
            background_snap_degrees: 15.0,
            background_drag: None,
            boot_drag: None,
            standby_drag: None,
            boot_transform: MediaTransform::default(),
            standby_transform: MediaTransform::default(),
            brightness: 80,
            boot_path: None,
            standby_path: None,
            boot_source_cache: None,
            boot_animation: None,
            boot_animation_pending: None,
            boot_animation_tx,
            boot_animation_rx,
            standby_source_cache: None,
            standby_time: 0.0,
            standby_duration: None,
            standby_metadata_path: None,
            standby_frame_key: None,
            standby_frame_pending: false,
            standby_worker_tx,
            standby_worker_rx,
            last_standby_frame_change: Instant::now(),
            boot_inspection_key: None,
            boot_inspection_pending: None,
            boot_estimate: None,
            boot_inspection_tx,
            boot_inspection_rx,
            last_boot_inspection_change: Instant::now(),
            stream_path: None,
            device_preview_job: None,
            status_text: String::new(),
            last_error: None,
        };
        app.refresh_sensors();
        app
    }

    fn refresh_sensors(&mut self) {
        let mut values = self.sensors.read();
        if let Some(value) = self.device_coolant {
            values.readings.insert("coolant".to_string(), value);
        }
        self.sensor_values = values;
    }

    fn poll(&mut self) {
        if self.last_sensor_update.elapsed() >= Duration::from_secs(1) {
            self.refresh_sensors();
            self.last_sensor_update = Instant::now();
        }
        if self.last_daemon_check.elapsed() >= Duration::from_secs(2) {
            self.daemon_running = self.service_manager.daemon_running();
            self.last_daemon_check = Instant::now();
        }
        if self.last_device_scan.elapsed() >= Duration::from_secs(4) {
            self.device_info = detect_device_info();
            self.last_device_scan = Instant::now();
        }
        self.poll_device_preview_job();
        self.poll_boot_inspection();
        self.poll_boot_animation();
        self.poll_standby_worker();
    }

    fn poll_boot_animation(&mut self) {
        while let Ok((path, result)) = self.boot_animation_rx.try_recv() {
            if self.boot_path.as_ref() == Some(&path) {
                self.boot_animation_pending = None;
                match result {
                    Ok(animation) => self.boot_animation = Some((path, animation)),
                    Err(error) => self.last_error = Some(error),
                }
            }
        }
    }

    fn start_boot_animation_decode(&mut self, path: PathBuf) {
        self.boot_animation = None;
        self.boot_animation_pending = Some(path.clone());
        let tx = self.boot_animation_tx.clone();
        std::thread::spawn(move || {
            let result = TimedAnimation::load_gif(&path);
            let _ = tx.send((path, result));
        });
    }

    fn poll_standby_worker(&mut self) {
        while let Ok(result) = self.standby_worker_rx.try_recv() {
            match result {
                StandbyWorkerResult::Metadata(path, result) => {
                    if self.standby_path.as_ref() == Some(&path) {
                        self.standby_metadata_path = None;
                        match result {
                            Ok(duration) => {
                                self.standby_duration = Some(duration.max(0.0));
                                self.standby_time = self.standby_time.min(duration.max(0.0));
                                self.standby_frame_key = None;
                                self.last_standby_frame_change = Instant::now();
                            }
                            Err(error) => self.last_error = Some(error),
                        }
                    }
                }
                StandbyWorkerResult::Frame(key, result) => {
                    let current = self.standby_path.as_ref().map(|path| StandbyFrameKey {
                        path: path.clone(),
                        time_bits: self.standby_time.to_bits(),
                    });
                    if current.as_ref() == Some(&key) {
                        self.standby_frame_pending = false;
                        match result {
                            Ok(frame) => {
                                self.standby_source_cache = Some((key.path, key.time_bits, frame));
                            }
                            Err(error) => self.last_error = Some(error),
                        }
                    }
                }
            }
        }

        let Some(path) = self.standby_path.clone() else {
            return;
        };
        if self.standby_duration.is_none() && self.standby_metadata_path.as_ref() != Some(&path) {
            self.standby_metadata_path = Some(path.clone());
            let tx = self.standby_worker_tx.clone();
            std::thread::spawn(move || {
                let result = media_duration(&path);
                let _ = tx.send(StandbyWorkerResult::Metadata(path, result));
            });
            return;
        }
        if self.standby_duration.is_none() {
            return;
        }

        let key = StandbyFrameKey {
            path,
            time_bits: self.standby_time.to_bits(),
        };
        if self.standby_frame_key.as_ref() != Some(&key) {
            self.standby_frame_key = Some(key.clone());
            self.standby_frame_pending = false;
            self.last_standby_frame_change = Instant::now();
        }
        let cached = self
            .standby_source_cache
            .as_ref()
            .is_some_and(|(path, time, _)| path == &key.path && *time == key.time_bits);
        if cached
            || self.standby_frame_pending
            || self.last_standby_frame_change.elapsed() < Duration::from_millis(150)
        {
            return;
        }
        self.standby_frame_pending = true;
        let tx = self.standby_worker_tx.clone();
        std::thread::spawn(move || {
            let result = load_media_frame_at(&key.path, f64::from_bits(key.time_bits));
            let _ = tx.send(StandbyWorkerResult::Frame(key, result));
        });
    }

    fn poll_boot_inspection(&mut self) {
        let current = self.boot_path.as_ref().map(|path| BootInspectionKey {
            path: path.clone(),
            transform: self.boot_transform.clone(),
        });
        if current != self.boot_inspection_key {
            self.boot_inspection_key = current.clone();
            self.boot_estimate = None;
            self.last_boot_inspection_change = Instant::now();
        }

        while let Ok((key, result)) = self.boot_inspection_rx.try_recv() {
            if self.boot_inspection_pending.as_ref() == Some(&key) {
                self.boot_inspection_pending = None;
            }
            if Some(&key) == self.boot_inspection_key.as_ref() {
                self.boot_estimate = Some(result);
            }
        }

        let Some(key) = current else {
            return;
        };
        if self.boot_inspection_pending.is_some()
            || self.boot_estimate.is_some()
            || self.last_boot_inspection_change.elapsed() < Duration::from_millis(300)
        {
            return;
        }

        self.boot_inspection_pending = Some(key.clone());
        let tx = self.boot_inspection_tx.clone();
        std::thread::spawn(move || {
            let mut args = vec![
                "--inspect-boot".to_string(),
                key.path.to_string_lossy().into_owned(),
            ];
            args.extend(media_transform_args(&key.transform));
            let result = Command::new(daemon_binary_path())
                .args(args)
                .output()
                .map_err(|error| format!("failed to inspect boot animation: {error}"))
                .and_then(parse_boot_estimate);
            let _ = tx.send((key, result));
        });
    }

    fn poll_device_preview_job(&mut self) {
        let status = self
            .device_preview_job
            .as_mut()
            .and_then(|job| job.child.try_wait().ok().flatten());
        let Some(status) = status else {
            return;
        };
        let job = self.device_preview_job.take().unwrap();
        self.device_preview_mode = DevicePreviewMode::Off;
        if job.restore_live_display {
            self.start_live_daemon();
        } else {
            self.daemon_running = false;
        }
        if status.success() {
            self.status_text = format!("{} preview finished", job.mode.label());
        } else {
            self.last_error = Some(format!(
                "{} preview process exited with {status}",
                job.mode.label()
            ));
        }
    }

    fn is_dirty(&self) -> bool {
        self.working != self.committed
    }

    fn apply(&mut self) {
        self.working.background.image_path = match self.background_source {
            BackgroundSource::File => self.background_file_path.clone(),
            BackgroundSource::Stream => self
                .stream_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            BackgroundSource::SolidColor => None,
        };
        match self.working.save(&self.config_path) {
            Ok(()) => {
                self.committed = self.working.clone();
                self.status_text = "Configuration applied".to_string();
            }
            Err(err) => self.last_error = Some(format!("Failed to save config: {err}")),
        }
    }

    fn revert(&mut self) {
        self.working = self.committed.clone();
        self.selected_sensor = None;
        self.placing_sensor = None;
    }

    fn set_live_display(&mut self, enabled: bool) {
        self.live_display_enabled = enabled;
        if self.device_preview_job.is_some() {
            return;
        }
        if enabled {
            self.start_live_daemon();
        } else {
            self.stop_live_daemon();
        }
    }

    fn start_live_daemon(&mut self) -> bool {
        let started = self.service_manager.start(&daemon_binary_path());
        self.daemon_running = started;
        if !started {
            self.last_error = Some("Live display daemon failed to start".into());
        }
        started
    }

    fn stop_live_daemon(&mut self) -> bool {
        let stopped = self.service_manager.stop();
        self.daemon_running = !stopped;
        if !stopped {
            self.last_error = Some(
                "Live display daemon did not release ownership; device operation cancelled".into(),
            );
        }
        stopped
    }

    fn show_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top-bar")
            .exact_height(44.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("TH420 Display");
                    ui.separator();
                    let connected = self.device_info.connected;
                    ui.label(
                        egui::RichText::new(if connected {
                            "● Connected"
                        } else {
                            "○ Disconnected"
                        })
                        .color(if connected {
                            egui::Color32::LIGHT_GREEN
                        } else {
                            egui::Color32::GRAY
                        }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(self.is_dirty(), egui::Button::new("Apply"))
                            .clicked()
                        {
                            self.apply();
                        }
                        if ui
                            .add_enabled(self.is_dirty(), egui::Button::new("Revert"))
                            .clicked()
                        {
                            self.revert();
                        }
                        if self.is_dirty() {
                            ui.label(
                                egui::RichText::new("Unsaved changes")
                                    .small()
                                    .color(egui::Color32::YELLOW),
                            );
                        }
                    });
                });
            });
    }

    fn show_brightness_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("brightness-bar")
            .exact_height(48.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label("Display brightness");
                    ui.add(egui::Slider::new(&mut self.brightness, 0..=100).suffix("%"));
                    if ui
                        .add_enabled(
                            self.device_info.connected && self.device_preview_job.is_none(),
                            egui::Button::new("Set"),
                        )
                        .clicked()
                    {
                        self.run_short_device_command(
                            "Set brightness",
                            vec!["--standby-brightness".into(), self.brightness.to_string()],
                        );
                    }
                });
            });
    }

    fn show_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("navigation")
            .exact_width(165.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(12.0);
                for (page, label) in Page::ALL {
                    if ui.selectable_label(self.page == page, label).clicked() {
                        self.page = page;
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} · 480 × 480",
                            self.device_info
                                .product
                                .as_deref()
                                .unwrap_or("TH420 V2 Ultra EX")
                        ))
                        .small()
                        .color(egui::Color32::GRAY),
                    );
                });
            });
    }

    fn show_preview_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("display-preview")
            .exact_width(410.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(10.0);
                ui.heading("Display Preview");
                ui.add_space(8.0);
                self.update_preview_texture(ctx);
                if self.page == Page::LiveDisplay && self.live_tab == LiveTab::Background {
                    self.interactive_background_preview(ui, ctx);
                } else if self.page == Page::LiveDisplay && self.live_tab == LiveTab::Overlay {
                    self.interactive_preview(ui);
                } else if self.page == Page::StandbySettings {
                    self.interactive_media_preview(ui, ctx);
                } else {
                    self.static_preview(ui);
                }
                if self.page == Page::StandbySettings {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(format!("Preview: {}", self.preview_mode.label()))
                            .small()
                            .color(egui::Color32::GRAY),
                    );
                }
            });
    }

    fn update_preview_texture(&mut self, ctx: &egui::Context) {
        let refresh_interval = if self.page == Page::StandbySettings
            && self.preview_mode == PreviewMode::Boot
            && self.boot_animation.is_some()
        {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(50)
        };
        if self.last_preview_update.elapsed() < refresh_interval && self.preview_texture.is_some() {
            return;
        }

        let image = if self.page == Page::StandbySettings {
            let Some(image) = self.transformed_persistent_preview() else {
                self.last_preview_update = Instant::now();
                return;
            };
            image
        } else {
            self.renderer
                .render_preview(&self.working, &self.sensor_values)
        };
        if let Some(error) = self.renderer.take_background_error() {
            self.last_error = Some(error);
        }
        let pixels = image
            .pixels()
            .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
            .collect();
        self.preview_texture = Some(ctx.load_texture(
            "gui-v2-preview",
            egui::ColorImage {
                size: [image.width() as usize, image.height() as usize],
                pixels,
            },
            egui::TextureOptions::LINEAR,
        ));
        self.last_preview_update = Instant::now();
    }

    fn transformed_persistent_preview(&mut self) -> Option<image::RgbImage> {
        match self.preview_mode {
            PreviewMode::Boot => {
                let path = self.boot_path.as_ref()?;
                if is_gif(path) {
                    let (_, animation) = self
                        .boot_animation
                        .as_ref()
                        .filter(|(animation_path, _)| animation_path == path)?;
                    return transform_media_image(animation.current_frame(), &self.boot_transform);
                }
                if self
                    .boot_source_cache
                    .as_ref()
                    .map(|(cached_path, _)| cached_path)
                    != Some(path)
                {
                    self.boot_source_cache =
                        load_media_source_frame(path).map(|source| (path.clone(), source));
                }
                self.boot_source_cache
                    .as_ref()
                    .and_then(|(_, source)| transform_media_image(source, &self.boot_transform))
            }
            PreviewMode::Standby => {
                let path = self.standby_path.as_ref()?;
                let time_key = self.standby_time.to_bits();
                let matches = self.standby_source_cache.as_ref().is_some_and(
                    |(cached_path, cached_time, _)| cached_path == path && *cached_time == time_key,
                );
                if !matches {
                    return None;
                }
                self.standby_source_cache
                    .as_ref()
                    .and_then(|(_, _, source)| {
                        transform_media_image(source, &self.standby_transform)
                    })
            }
        }
    }

    fn preview_rect(&self, ui: &mut egui::Ui, sense: egui::Sense) -> (egui::Rect, egui::Response) {
        let side = ui.available_width().min(380.0);
        ui.allocate_exact_size(egui::vec2(side, side), sense)
    }

    fn static_preview(&mut self, ui: &mut egui::Ui) {
        let Some(texture) = &self.preview_texture else {
            ui.spinner();
            return;
        };
        let (rect, _) = self.preview_rect(ui, egui::Sense::hover());
        ui.painter().image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        ui.painter().circle_stroke(
            rect.center(),
            rect.width() / 2.0,
            egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
        );
    }

    fn interactive_background_preview(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(texture) = &self.preview_texture else {
            ui.spinner();
            return;
        };
        let texture_id = texture.id();
        let (rect, response) = self.preview_rect(ui, egui::Sense::drag());
        ui.painter().image(
            texture_id,
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        ui.painter().circle_stroke(
            rect.center(),
            rect.width() / 2.0,
            egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
        );
        if response.drag_started() {
            self.background_drag = Some(DragSnapState::new([
                self.working.background.pan_x,
                self.working.background.pan_y,
            ]));
        }
        if response.dragged() {
            let delta = response.drag_delta();
            let canvas_delta = [
                delta.x * 480.0 / rect.width(),
                delta.y * 480.0 / rect.height(),
            ];
            let drag = self.background_drag.get_or_insert_with(|| {
                DragSnapState::new([self.working.background.pan_x, self.working.background.pan_y])
            });
            drag.update(canvas_delta, self.background_snap, self.background_snap_px);
            self.working.background.pan_x = drag.preview[0];
            self.working.background.pan_y = drag.preview[1];
        }
        if response.drag_stopped() {
            if let Some(drag) = self.background_drag.take() {
                let committed = drag.finish(self.background_snap, self.background_snap_px);
                self.working.background.pan_x = committed[0];
                self.working.background.pan_y = committed[1];
            }
        }
        if let Some(drag) = self.background_drag {
            ui.label(
                egui::RichText::new(format!(
                    "Drag: {:.1}, {:.1} px    Preview: {:.1}, {:.1} px",
                    drag.raw[0], drag.raw[1], drag.preview[0], drag.preview[1]
                ))
                .small()
                .monospace(),
            );
        }
        if response.hovered() {
            let scroll = ctx.input(|input| input.raw_scroll_delta.y);
            if scroll != 0.0 {
                self.working.background.zoom =
                    (self.working.background.zoom * (1.0 + scroll * 0.001)).clamp(0.05, 8.0);
            }
        }
    }

    fn active_media_transform(&self) -> &MediaTransform {
        match self.preview_mode {
            PreviewMode::Boot => &self.boot_transform,
            PreviewMode::Standby => &self.standby_transform,
        }
    }

    fn active_media_transform_mut(&mut self) -> &mut MediaTransform {
        match self.preview_mode {
            PreviewMode::Boot => &mut self.boot_transform,
            PreviewMode::Standby => &mut self.standby_transform,
        }
    }

    fn active_media_drag_mut(&mut self) -> &mut Option<DragSnapState> {
        match self.preview_mode {
            PreviewMode::Boot => &mut self.boot_drag,
            PreviewMode::Standby => &mut self.standby_drag,
        }
    }

    fn interactive_media_preview(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(texture) = &self.preview_texture else {
            ui.spinner();
            return;
        };
        let texture_id = texture.id();
        let (rect, response) = self.preview_rect(ui, egui::Sense::drag());
        ui.painter().image(
            texture_id,
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        ui.painter().circle_stroke(
            rect.center(),
            rect.width() / 2.0,
            egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
        );

        if response.drag_started() {
            let transform = &self.active_media_transform().transform;
            *self.active_media_drag_mut() =
                Some(DragSnapState::new([transform.pan_x, transform.pan_y]));
        }
        if response.dragged() {
            let delta = response.drag_delta();
            let canvas_delta = [
                delta.x * 480.0 / rect.width(),
                delta.y * 480.0 / rect.height(),
            ];
            let origin = {
                let transform = &self.active_media_transform().transform;
                [transform.pan_x, transform.pan_y]
            };
            let snapping = self.background_snap;
            let grid = self.background_snap_px;
            let drag = self
                .active_media_drag_mut()
                .get_or_insert_with(|| DragSnapState::new(origin));
            drag.update(canvas_delta, snapping, grid);
            let preview = drag.preview;
            let transform = &mut self.active_media_transform_mut().transform;
            transform.pan_x = preview[0];
            transform.pan_y = preview[1];
        }
        if response.drag_stopped() {
            if let Some(drag) = self.active_media_drag_mut().take() {
                let committed = drag.finish(self.background_snap, self.background_snap_px);
                let transform = &mut self.active_media_transform_mut().transform;
                transform.pan_x = committed[0];
                transform.pan_y = committed[1];
            }
        }
        if let Some(drag) = *self.active_media_drag_mut() {
            ui.label(
                egui::RichText::new(format!(
                    "Drag: {:.1}, {:.1} px    Preview: {:.1}, {:.1} px",
                    drag.raw[0], drag.raw[1], drag.preview[0], drag.preview[1]
                ))
                .small()
                .monospace(),
            );
        }
        if response.hovered() {
            let scroll = ctx.input(|input| input.raw_scroll_delta.y);
            if scroll != 0.0 {
                let transform = &mut self.active_media_transform_mut().transform;
                transform.zoom = (transform.zoom * (1.0 + scroll * 0.001)).clamp(0.05, 8.0);
            }
        }
    }

    fn interactive_preview(&mut self, ui: &mut egui::Ui) {
        let Some(texture) = &self.preview_texture else {
            ui.spinner();
            return;
        };
        let texture_id = texture.id();
        let (rect, response) = self.preview_rect(ui, egui::Sense::click_and_drag());
        ui.painter().image(
            texture_id,
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        ui.painter().circle_stroke(
            rect.center(),
            rect.width() / 2.0,
            egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
        );

        if self.show_grid {
            let step = rect.width() * self.grid_size / 480.0;
            let mut x = rect.left();
            while x <= rect.right() {
                ui.painter().line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(0.5_f32, egui::Color32::from_white_alpha(30)),
                );
                x += step.max(2.0);
            }
            let mut y = rect.top();
            while y <= rect.bottom() {
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    egui::Stroke::new(0.5_f32, egui::Color32::from_white_alpha(30)),
                );
                y += step.max(2.0);
            }
        }

        if let Some(pointer) = response.interact_pointer_pos() {
            let mut x = ((pointer.x - rect.left()) / rect.width() * 480.0).clamp(0.0, 480.0);
            let mut y = ((pointer.y - rect.top()) / rect.height() * 480.0).clamp(0.0, 480.0);
            if self.snap_to_grid {
                x = snap(x, self.grid_size);
                y = snap(y, self.grid_size);
            }

            if response.clicked() {
                if let Some(id) = self.placing_sensor.take() {
                    self.place_sensor(&id, x, y);
                    self.selected_sensor = Some(id);
                } else {
                    self.selected_sensor = self.sensor_at(x, y);
                }
            }
            if response.dragged() {
                if let Some(id) = self.selected_sensor.clone() {
                    self.place_sensor(&id, x, y);
                }
            }
        }

        if let Some(id) = &self.selected_sensor {
            if let Some((x, y)) = self.sensor_position(id) {
                let point = egui::pos2(
                    rect.left() + x / 480.0 * rect.width(),
                    rect.top() + y / 480.0 * rect.height(),
                );
                ui.painter().circle_stroke(
                    point,
                    30.0,
                    egui::Stroke::new(2.0_f32, egui::Color32::YELLOW),
                );
            }
        }
    }

    fn ensure_custom_layout(&mut self) {
        if self.working.layout.preset == LayoutPreset::Custom {
            return;
        }
        let ids = self.working.enabled_sensor_ids();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let resolved = self.working.layout.preset_slots(&refs);
        self.working.layout.custom_slots = resolved
            .into_iter()
            .map(|slot| LayoutSlot {
                sensor_id: slot.sensor_id,
                value_cx_norm: slot.value_cx as f32 / 480.0,
                value_cy_norm: slot.value_y as f32 / 480.0,
                label_cx_norm: slot.label_cx as f32 / 480.0,
                label_cy_norm: slot.label_y as f32 / 480.0,
                value_font_size: slot.value_fs,
                label_font_size: slot.label_fs,
            })
            .collect();
        self.working.layout.preset = LayoutPreset::Custom;
    }

    fn place_sensor(&mut self, id: &str, x: f32, y: f32) {
        self.ensure_custom_layout();
        if let Some(sensor) = self
            .working
            .sensors
            .iter_mut()
            .find(|sensor| sensor.id == id)
        {
            sensor.enabled = true;
        }
        let nx = (x / 480.0).clamp(0.0, 1.0);
        let ny = (y / 480.0).clamp(0.0, 1.0);
        if let Some(slot) = self
            .working
            .layout
            .custom_slots
            .iter_mut()
            .find(|slot| slot.sensor_id == id)
        {
            slot.value_cx_norm = nx;
            slot.value_cy_norm = ny;
            slot.label_cx_norm = nx;
            slot.label_cy_norm = (ny + 0.08).clamp(0.0, 1.0);
        } else {
            self.working.layout.custom_slots.push(LayoutSlot {
                sensor_id: id.to_string(),
                value_cx_norm: nx,
                value_cy_norm: ny,
                label_cx_norm: nx,
                label_cy_norm: (ny + 0.08).clamp(0.0, 1.0),
                value_font_size: 52.0,
                label_font_size: 24.0,
            });
        }
    }

    fn sensor_position(&self, id: &str) -> Option<(f32, f32)> {
        let ids = self.working.enabled_sensor_ids();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let slot = self
            .working
            .layout
            .preset_slots(&refs)
            .into_iter()
            .find(|slot| slot.sensor_id == id)?;
        Some((slot.value_cx as f32, slot.value_y as f32))
    }

    fn sensor_at(&self, x: f32, y: f32) -> Option<String> {
        let ids = self.working.enabled_sensor_ids();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.working
            .layout
            .preset_slots(&refs)
            .into_iter()
            .map(|slot| {
                let dx = slot.value_cx as f32 - x;
                let dy = slot.value_y as f32 - y;
                (slot.sensor_id, (dx * dx + dy * dy).sqrt())
            })
            .filter(|(_, distance)| *distance <= 70.0)
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|(id, _)| id)
    }

    fn show_overview(&mut self, ui: &mut egui::Ui) {
        ui.heading("Overview");
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("Device").strong());
            egui::Grid::new("overview-device")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Model");
                    ui.label(
                        self.device_info
                            .product
                            .as_deref()
                            .unwrap_or("TH420 V2 Ultra EX"),
                    );
                    ui.end_row();
                    ui.label("USB ID");
                    ui.label(format!("{}:{}", self.device_info.vid, self.device_info.pid));
                    ui.end_row();
                    ui.label("Firmware / revision");
                    ui.label(self.device_info.revision.as_deref().unwrap_or("—"));
                    ui.end_row();
                    ui.label("Live Display");
                    ui.label(if self.live_display_enabled {
                        "Enabled"
                    } else {
                        "Disabled"
                    });
                    ui.end_row();
                });
        });
        ui.add_space(10.0);
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Device metrics").strong());
                if ui
                    .add_enabled(
                        self.device_info.connected && self.device_preview_job.is_none(),
                        egui::Button::new("Refresh"),
                    )
                    .clicked()
                {
                    self.read_device_status();
                }
            });
            egui::Grid::new("overview-metrics")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Coolant");
                    ui.label(
                        self.device_coolant
                            .map(|v| format!("{v:.1} °C"))
                            .unwrap_or_else(|| "—".to_string()),
                    );
                    ui.end_row();
                    ui.label("Pump");
                    ui.label(
                        self.device_pump_rpm
                            .map(|v| format!("{v} RPM"))
                            .unwrap_or_else(|| "—".to_string()),
                    );
                    ui.end_row();
                    ui.label("Brightness");
                    ui.label(format!("{}%", self.brightness));
                    ui.end_row();
                });
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Configure Live Display").clicked() {
                self.page = Page::LiveDisplay;
            }
            if ui.button("Configure Standby").clicked() {
                self.page = Page::StandbySettings;
            }
        });
    }

    fn show_live_display(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Live Display");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut enabled = self.live_display_enabled;
                if ui.checkbox(&mut enabled, "Enabled").changed() {
                    self.set_live_display(enabled);
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.working.background.enabled, "Background");
            ui.checkbox(&mut self.working.overlay_enabled, "Overlay");
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.live_tab, LiveTab::Background, "Background");
            ui.selectable_value(&mut self.live_tab, LiveTab::Overlay, "Overlay");
        });
        ui.separator();
        match self.live_tab {
            LiveTab::Background => self.show_background_editor(ui),
            LiveTab::Overlay => self.show_overlay_editor(ui),
        }
    }

    fn show_background_editor(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        let previous_source = self.background_source;
        egui::ComboBox::from_label("Source")
            .selected_text(self.background_source.label())
            .show_ui(ui, |ui| {
                for source in BackgroundSource::ALL {
                    ui.selectable_value(&mut self.background_source, source, source.label());
                }
            });
        if previous_source != self.background_source {
            self.working.background.image_path = match self.background_source {
                BackgroundSource::File => self.background_file_path.clone(),
                BackgroundSource::Stream => self
                    .stream_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                BackgroundSource::SolidColor => None,
            };
        }

        match self.background_source {
            BackgroundSource::File => {
                ui.horizontal(|ui| {
                    ui.label("File");
                    let label = self
                        .working
                        .background
                        .image_path
                        .as_deref()
                        .and_then(|p| Path::new(p).file_name())
                        .and_then(|s| s.to_str())
                        .unwrap_or("None");
                    ui.label(label);
                    if ui.button("Browse…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            if image::open(&path).is_err()
                                && Command::new("ffmpeg").arg("-version").output().is_err()
                            {
                                self.last_error = Some("This media type requires FFmpeg. Install ffmpeg and try again.".to_string());
                            } else {
                                let path = path.to_string_lossy().into_owned();
                                self.background_file_path = Some(path.clone());
                                self.working.background.image_path = Some(path);
                            }
                        }
                    }
                });
                egui::ComboBox::from_label("Fit")
                    .selected_text(format!("{:?}", self.working.background.fit))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.working.background.fit,
                            ImageFit::Cover,
                            "Cover",
                        );
                        ui.selectable_value(
                            &mut self.working.background.fit,
                            ImageFit::Contain,
                            "Contain",
                        );
                        ui.selectable_value(
                            &mut self.working.background.fit,
                            ImageFit::Stretch,
                            "Stretch",
                        );
                    });
                ui.add(
                    egui::Slider::new(&mut self.working.background.zoom, 0.25..=4.0).text("Zoom"),
                );
            }
            BackgroundSource::Stream => {
                ui.horizontal(|ui| {
                    ui.label("Stream source");
                    ui.label(file_name(self.stream_path.as_ref()));
                    if ui.button("Browse…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            if image::open(&path).is_err()
                                && Command::new("ffmpeg").arg("-version").output().is_err()
                            {
                                self.last_error = Some("This media type requires FFmpeg. Install ffmpeg and try again.".to_string());
                            } else {
                                self.stream_path = Some(path.clone());
                                self.working.background.image_path =
                                    Some(path.to_string_lossy().into_owned());
                            }
                        }
                    }
                });
                egui::ComboBox::from_label("Fit")
                    .selected_text(format!("{:?}", self.working.background.fit))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.working.background.fit,
                            ImageFit::Cover,
                            "Cover",
                        );
                        ui.selectable_value(
                            &mut self.working.background.fit,
                            ImageFit::Contain,
                            "Contain",
                        );
                        ui.selectable_value(
                            &mut self.working.background.fit,
                            ImageFit::Stretch,
                            "Stretch",
                        );
                    });
            }
            BackgroundSource::SolidColor => {
                ui.horizontal(|ui| {
                    ui.label("Color");
                    let mut color = self
                        .working
                        .background
                        .background_color
                        .map(|c| c as f32 / 255.0);
                    if egui::color_picker::color_edit_button_rgb(ui, &mut color).changed() {
                        self.working.background.background_color =
                            color.map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8);
                        self.working.background.image_path = None;
                    }
                });
            }
        }
        self.background_transform_controls(ui);
    }

    fn background_transform_controls(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label(egui::RichText::new("Transform").strong());
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.background_snap, "Snap transforms");
            ui.add(
                egui::DragValue::new(&mut self.background_snap_px)
                    .range(1.0..=120.0)
                    .suffix(" px"),
            );
            ui.add(
                egui::DragValue::new(&mut self.background_snap_degrees)
                    .range(1.0..=90.0)
                    .suffix(" deg"),
            );
        });
        ui.add(egui::Slider::new(&mut self.working.background.zoom, 0.05..=8.0).text("Zoom"));
        ui.horizontal(|ui| {
            ui.label("Stretch");
            ui.add(
                egui::DragValue::new(&mut self.working.background.stretch_x)
                    .range(0.05..=8.0)
                    .prefix("X: "),
            );
            ui.add(
                egui::DragValue::new(&mut self.working.background.stretch_y)
                    .range(0.05..=8.0)
                    .prefix("Y: "),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Pan");
            let pan_x_changed = ui
                .add(
                    egui::DragValue::new(&mut self.working.background.pan_x)
                        .speed(0.5)
                        .prefix("X: ")
                        .suffix(" px"),
                )
                .changed();
            let pan_y_changed = ui
                .add(
                    egui::DragValue::new(&mut self.working.background.pan_y)
                        .speed(0.5)
                        .prefix("Y: ")
                        .suffix(" px"),
                )
                .changed();
            if pan_x_changed || pan_y_changed {
                self.background_drag = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Background rotation");
            ui.add(
                egui::DragValue::new(&mut self.working.background.rotation)
                    .speed(0.25)
                    .suffix(" deg"),
            );
            if ui.small_button("Reset transforms").clicked() {
                self.background_drag = None;
                self.working.background.zoom = 1.0;
                self.working.background.stretch_x = 1.0;
                self.working.background.stretch_y = 1.0;
                self.working.background.pan_x = 0.0;
                self.working.background.pan_y = 0.0;
                self.working.background.rotation = 0.0;
            }
        });
        ui.add(
            egui::Slider::new(&mut self.working.background.opacity, 0..=255).text("Image opacity"),
        );
        ui.add(
            egui::Slider::new(&mut self.working.background.overlay_alpha, 0..=255).text("Darken"),
        );
        ui.add(egui::Slider::new(&mut self.working.background.blur_sigma, 0.0..=50.0).text("Blur"));
    }

    fn show_overlay_editor(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Widgets").strong());
        egui::ScrollArea::vertical()
            .max_height(250.0)
            .show(ui, |ui| {
                for sensor in self.working.sensors.clone() {
                    ui.horizontal(|ui| {
                        let selected = self.selected_sensor.as_deref() == Some(&sensor.id);
                        if ui.selectable_label(selected, &sensor.label).clicked() {
                            self.selected_sensor = Some(sensor.id.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if sensor.enabled {
                                if ui.small_button("Remove").clicked() {
                                    if let Some(entry) = self
                                        .working
                                        .sensors
                                        .iter_mut()
                                        .find(|entry| entry.id == sensor.id)
                                    {
                                        entry.enabled = false;
                                    }
                                }
                            } else if ui.small_button("Place").clicked() {
                                self.placing_sensor = Some(sensor.id.clone());
                                self.selected_sensor = Some(sensor.id.clone());
                            }
                        });
                    });
                }
            });

        if let Some(id) = self.selected_sensor.clone() {
            ui.separator();
            ui.label(egui::RichText::new("Selected widget").strong());
            if let Some(index) = self
                .working
                .sensors
                .iter()
                .position(|sensor| sensor.id == id)
            {
                ui.horizontal(|ui| {
                    ui.label("Label");
                    ui.text_edit_singleline(&mut self.working.sensors[index].label);
                });
                self.ensure_custom_layout();
                if let Some(slot) = self
                    .working
                    .layout
                    .custom_slots
                    .iter_mut()
                    .find(|slot| slot.sensor_id == id)
                {
                    ui.add(
                        egui::Slider::new(&mut slot.value_font_size, 12.0..=140.0)
                            .text("Value size"),
                    );
                    ui.add(
                        egui::Slider::new(&mut slot.label_font_size, 8.0..=64.0).text("Label size"),
                    );
                }
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.snap_to_grid, "Snap to grid");
            ui.checkbox(&mut self.show_grid, "Show grid");
        });
        ui.add(egui::Slider::new(&mut self.grid_size, 2.0..=48.0).text("Grid size"));
        if let Some(id) = &self.placing_sensor {
            ui.label(
                egui::RichText::new(format!("Click the preview to place {id}"))
                    .small()
                    .color(egui::Color32::YELLOW),
            );
        }
    }

    fn show_standby_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Standby Settings");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .selectable_value(&mut self.standby_tab, StandbyTab::Boot, "Boot")
                .clicked()
            {
                self.preview_mode = PreviewMode::Boot;
            }
            if ui
                .selectable_value(&mut self.standby_tab, StandbyTab::Standby, "Standby")
                .clicked()
            {
                self.preview_mode = PreviewMode::Standby;
            }
        });
        ui.add_space(8.0);

        let mut requested = self.device_preview_mode;
        egui::ComboBox::from_label("Show on device")
            .selected_text(requested.label())
            .show_ui(ui, |ui| {
                for mode in DevicePreviewMode::ALL {
                    ui.selectable_value(&mut requested, mode, mode.label());
                }
            });
        if requested != self.device_preview_mode {
            self.set_device_preview(requested);
        }

        ui.separator();
        match self.standby_tab {
            StandbyTab::Boot => self.show_boot_settings(ui),
            StandbyTab::Standby => self.show_standby_media_settings(ui),
        }
    }

    fn show_boot_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Boot animation").strong());
        ui.horizontal(|ui| {
            ui.label("File");
            ui.label(file_name(self.boot_path.as_ref()));
            if ui.button("Browse…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("GIF", &["gif"])
                    .pick_file()
                {
                    self.boot_path = Some(path.clone());
                    self.boot_source_cache = None;
                    self.preview_texture = None;
                    self.start_boot_animation_decode(path);
                }
            }
        });
        if self.boot_animation_pending.is_some() {
            ui.label(
                egui::RichText::new("Decoding boot preview…")
                    .small()
                    .color(egui::Color32::GRAY),
            );
        }
        Self::media_transform_controls(
            ui,
            &mut self.boot_transform,
            &mut self.boot_drag,
            self.background_snap,
            self.background_snap_px,
            self.background_snap_degrees,
        );
        let boot_upload_ready = match &self.boot_estimate {
            Some(Ok(estimate)) => {
                ui.label(format!(
                    "Encoded container: {:.2} MiB / {:.2} MiB · {} frames · {} ms/frame",
                    estimate.container_bytes as f64 / (1024.0 * 1024.0),
                    estimate.limit_bytes as f64 / (1024.0 * 1024.0),
                    estimate.frames,
                    estimate.delay_ms
                ));
                if !estimate.within_limit {
                    ui.label(
                        egui::RichText::new("Boot container exceeds the 10 MiB limit")
                            .color(egui::Color32::LIGHT_RED),
                    );
                }
                estimate.within_limit
            }
            Some(Err(error)) => {
                ui.label(egui::RichText::new(error).color(egui::Color32::LIGHT_RED));
                false
            }
            None if self.boot_path.is_some() => {
                ui.label(
                    egui::RichText::new("Calculating encoded container size…")
                        .small()
                        .color(egui::Color32::GRAY),
                );
                false
            }
            None => false,
        };
        if ui
            .add_enabled(
                self.device_info.connected && self.boot_path.is_some() && boot_upload_ready,
                egui::Button::new("Upload to device"),
            )
            .clicked()
        {
            let path = self
                .boot_path
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mut args = vec!["--upload-boot".into(), path];
            args.extend(media_transform_args(&self.boot_transform));
            self.run_short_device_command("Upload boot animation", args);
        }
    }

    fn show_standby_media_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Standby").strong());
        ui.horizontal(|ui| {
            ui.label("File");
            ui.label(file_name(self.standby_path.as_ref()));
            if ui.button("Browse…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    self.standby_path = Some(path);
                    self.standby_time = 0.0;
                    self.standby_duration = None;
                    self.standby_metadata_path = None;
                    self.standby_frame_key = None;
                    self.standby_frame_pending = false;
                    self.standby_source_cache = None;
                    self.preview_texture = None;
                }
            }
        });
        if let Some(duration) = self.standby_duration.filter(|duration| *duration > 0.0) {
            ui.add(
                egui::Slider::new(&mut self.standby_time, 0.0..=duration)
                    .text("Frame time")
                    .suffix(" s"),
            );
        } else if self.standby_path.is_some() && self.standby_duration.is_none() {
            ui.label(
                egui::RichText::new("Reading media metadata…")
                    .small()
                    .color(egui::Color32::GRAY),
            );
        }
        Self::media_transform_controls(
            ui,
            &mut self.standby_transform,
            &mut self.standby_drag,
            self.background_snap,
            self.background_snap_px,
            self.background_snap_degrees,
        );
        let standby_frame_ready = self.standby_path.as_ref().is_some_and(|path| {
            self.standby_source_cache
                .as_ref()
                .is_some_and(|(cached_path, time, _)| {
                    cached_path == path && *time == self.standby_time.to_bits()
                })
        });
        if ui
            .add_enabled(
                self.device_info.connected && standby_frame_ready,
                egui::Button::new("Upload persistent standby image"),
            )
            .clicked()
        {
            let path = self
                .standby_path
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mut args = vec!["--upload-standby".into(), path];
            args.extend(media_transform_args(&self.standby_transform));
            args.extend(["--media-time".into(), self.standby_time.to_string()]);
            self.run_short_device_command("Upload standby image", args);
        }
    }

    fn media_transform_controls(
        ui: &mut egui::Ui,
        media: &mut MediaTransform,
        drag: &mut Option<DragSnapState>,
        snapping: bool,
        pan_grid: f32,
        rotation_grid: f32,
    ) {
        ui.separator();
        ui.label(egui::RichText::new("Transform").strong());
        egui::ComboBox::from_label("Fit")
            .selected_text(match media.fit {
                ImageFit::Cover => "Cover",
                ImageFit::Contain => "Contain",
                ImageFit::Stretch => "Stretch",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut media.fit, ImageFit::Cover, "Cover");
                ui.selectable_value(&mut media.fit, ImageFit::Contain, "Contain");
                ui.selectable_value(&mut media.fit, ImageFit::Stretch, "Stretch");
            });
        ui.label(
            egui::RichText::new(format!(
                "Snapping: {} px / {}° ({})",
                pan_grid,
                rotation_grid,
                if snapping { "on" } else { "off" }
            ))
            .small()
            .color(egui::Color32::GRAY),
        );
        ui.add(egui::Slider::new(&mut media.transform.zoom, 0.05..=8.0).text("Zoom"));
        ui.horizontal(|ui| {
            ui.label("Stretch");
            ui.add(
                egui::DragValue::new(&mut media.transform.stretch_x)
                    .range(0.05..=8.0)
                    .prefix("X: "),
            );
            ui.add(
                egui::DragValue::new(&mut media.transform.stretch_y)
                    .range(0.05..=8.0)
                    .prefix("Y: "),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Pan");
            let x_changed = ui
                .add(
                    egui::DragValue::new(&mut media.transform.pan_x)
                        .speed(0.5)
                        .prefix("X: ")
                        .suffix(" px"),
                )
                .changed();
            let y_changed = ui
                .add(
                    egui::DragValue::new(&mut media.transform.pan_y)
                        .speed(0.5)
                        .prefix("Y: ")
                        .suffix(" px"),
                )
                .changed();
            if x_changed || y_changed {
                *drag = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Rotation");
            ui.add(
                egui::DragValue::new(&mut media.transform.rotation)
                    .speed(0.25)
                    .suffix(" deg"),
            );
            if ui.small_button("Reset transforms").clicked() {
                media.transform = Transform2D::default();
                *drag = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Canvas");
            let mut color = media.canvas_color.map(|channel| channel as f32 / 255.0);
            if egui::color_picker::color_edit_button_rgb(ui, &mut color).changed() {
                media.canvas_color =
                    color.map(|channel| (channel * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        });
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("Device").strong());
            egui::ComboBox::from_label("Orientation")
                .selected_text(format!("{:.0}°", self.working.rotation))
                .show_ui(ui, |ui| {
                    for rotation in [0.0f32, 90.0, 180.0, 270.0] {
                        ui.selectable_value(
                            &mut self.working.rotation,
                            rotation,
                            format!("{rotation:.0}°"),
                        );
                    }
                });
        });
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("Application / service").strong());
            let mut autostart = self.autostart_enabled;
            if ui
                .checkbox(&mut autostart, "Start Live Display on login")
                .changed()
            {
                if autostart {
                    self.service_manager.enable_autostart(&daemon_binary_path());
                } else {
                    self.service_manager.disable_autostart();
                }
                self.autostart_enabled = self.service_manager.autostart_enabled();
            }
            ui.label(format!(
                "Service manager: {}",
                self.service_manager.kind().name()
            ));
        });
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("Advanced").strong());
            ui.label(format!("Config: {}", self.config_path.display()));
            ui.label(format!(
                "Control HID: {}",
                path_or_dash(self.device_info.control_hidraw.as_ref())
            ));
            ui.label(format!(
                "Image HID: {}",
                path_or_dash(self.device_info.image_hidraw.as_ref())
            ));
            if ui.button("Reset working configuration").clicked() {
                self.working = Config::default();
                self.selected_sensor = None;
                self.placing_sensor = None;
            }
        });
    }

    fn read_device_status(&mut self) {
        let restore = self.daemon_running;
        if restore && !self.stop_live_daemon() {
            return;
        }
        let result = Command::new(daemon_binary_path()).arg("--status").output();
        if restore {
            self.start_live_daemon();
        }
        match result {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                for line in text.lines() {
                    if let Some(value) = line.strip_prefix("coolant_temp_c=") {
                        self.device_coolant = value.trim().parse().ok();
                    }
                    if let Some(value) = line.strip_prefix("pump_rpm=") {
                        self.device_pump_rpm = value.trim().parse().ok();
                    }
                }
                self.refresh_sensors();
            }
            Ok(output) => {
                self.last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Err(err) => self.last_error = Some(format!("Failed to read device status: {err}")),
        }
    }

    fn run_short_device_command(&mut self, label: &str, args: Vec<String>) {
        if self.device_preview_job.is_some() {
            return;
        }
        let restore = self.daemon_running;
        if restore && !self.stop_live_daemon() {
            return;
        }
        let result = Command::new(daemon_binary_path()).args(args).output();
        if restore {
            self.start_live_daemon();
        }
        match result {
            Ok(output) if output.status.success() => self.status_text = format!("{label} complete"),
            Ok(output) => {
                self.last_error = Some(format!(
                    "{label} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(err) => self.last_error = Some(format!("{label} failed: {err}")),
        }
    }

    fn set_device_preview(&mut self, mode: DevicePreviewMode) {
        let inherited_restore = self.stop_device_preview_child();
        if mode == DevicePreviewMode::Off {
            if inherited_restore {
                self.start_live_daemon();
            }
            return;
        }
        let path = match mode {
            DevicePreviewMode::BootLoop | DevicePreviewMode::BootOnce => self.boot_path.clone(),
            DevicePreviewMode::Standby => self.standby_path.clone(),
            DevicePreviewMode::Off => None,
        };
        let Some(path) = path else {
            self.last_error = Some(format!("Select a {} file first", mode.label()));
            if inherited_restore {
                self.start_live_daemon();
            }
            return;
        };

        let restore_live_display = inherited_restore || self.live_display_enabled;
        if self.daemon_running && !self.stop_live_daemon() {
            if inherited_restore {
                self.live_display_enabled = true;
            }
            return;
        }

        let loops = mode.loops();
        let mut args = if mode.is_boot() && is_gif(&path) {
            vec![
                "--play-live-gif".to_string(),
                path.to_string_lossy().into_owned(),
                "--live-loops".to_string(),
                loops.to_string(),
            ]
        } else {
            vec![
                "--play-live-frames".to_string(),
                path.to_string_lossy().into_owned(),
                "--live-fps".to_string(),
                "1".to_string(),
                "--live-loops".to_string(),
                loops.to_string(),
            ]
        };
        args.extend(["--live-brightness".to_string(), self.brightness.to_string()]);
        let transform = match mode {
            DevicePreviewMode::BootLoop | DevicePreviewMode::BootOnce => &self.boot_transform,
            DevicePreviewMode::Standby => &self.standby_transform,
            DevicePreviewMode::Off => unreachable!(),
        };
        args.extend(media_transform_args(transform));
        if matches!(mode, DevicePreviewMode::Standby) {
            args.extend(["--media-time".into(), self.standby_time.to_string()]);
        }

        match Command::new(daemon_binary_path())
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.device_preview_mode = mode;
                self.device_preview_job = Some(DevicePreviewJob {
                    child,
                    mode,
                    restore_live_display,
                });
            }
            Err(err) => {
                self.last_error = Some(format!("Failed to start device preview: {err}"));
                if restore_live_display {
                    self.start_live_daemon();
                }
            }
        }
    }

    fn stop_device_preview(&mut self) {
        let restore = self.stop_device_preview_child();
        if restore {
            self.start_live_daemon();
        } else {
            self.daemon_running = false;
        }
        self.device_preview_mode = DevicePreviewMode::Off;
    }

    fn stop_device_preview_child(&mut self) -> bool {
        let Some(mut job) = self.device_preview_job.take() else {
            self.device_preview_mode = DevicePreviewMode::Off;
            return false;
        };
        let _ = job.child.kill();
        let _ = job.child.wait();
        self.device_preview_mode = DevicePreviewMode::Off;
        job.restore_live_display
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_device_preview();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.shutdown_requested.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.poll();
        self.show_top_bar(ctx);
        self.show_brightness_bar(ctx);
        self.show_sidebar(ctx);
        self.show_preview_panel(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(10.0);
                match self.page {
                    Page::Overview => self.show_overview(ui),
                    Page::LiveDisplay => self.show_live_display(ui),
                    Page::StandbySettings => self.show_standby_settings(ui),
                    Page::Settings => self.show_settings(ui),
                }
                if !self.status_text.is_empty() {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(&self.status_text)
                            .small()
                            .color(egui::Color32::GRAY),
                    );
                }
                if let Some(error) = self.last_error.clone() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(error).color(egui::Color32::LIGHT_RED));
                        if ui.small_button("Dismiss").clicked() {
                            self.last_error = None;
                        }
                    });
                }
            });
        });
        let repaint_interval = if self.page == Page::StandbySettings
            && self.preview_mode == PreviewMode::Boot
            && self.boot_animation.is_some()
        {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(150)
        };
        ctx.request_repaint_after(repaint_interval);
    }
}

fn snap(value: f32, grid: f32) -> f32 {
    if grid <= 0.0 {
        value
    } else {
        (value / grid).round() * grid
    }
}

#[cfg(test)]
mod transform_drag_tests {
    use super::*;

    #[test]
    fn tracks_raw_coordinates_separately_from_snapped_preview() {
        let mut drag = DragSnapState::new([0.0, 0.0]);
        drag.update([3.0, 5.0], true, 8.0);
        drag.update([2.0, 1.0], true, 8.0);

        assert_eq!(drag.raw, [5.0, 6.0]);
        assert_eq!(drag.preview, [8.0, 8.0]);
    }

    #[test]
    fn release_commits_a_fully_snapped_coordinate_pair() {
        let mut drag = DragSnapState::new([2.0, 3.0]);
        drag.update([9.0, 14.0], true, 8.0);

        assert_eq!(drag.finish(true, 8.0), [8.0, 16.0]);
    }

    #[test]
    fn click_without_motion_preserves_existing_unsnapped_coordinates() {
        let drag = DragSnapState::new([5.5, 7.5]);

        assert_eq!(drag.finish(true, 8.0), [5.5, 7.5]);
    }

    #[test]
    fn later_grid_changes_do_not_resnap_an_already_committed_position() {
        let mut first_drag = DragSnapState::new([0.0, 0.0]);
        first_drag.update([10.0, 10.0], true, 8.0);
        let stored = first_drag.finish(true, 8.0);

        let new_grid = 12.0;
        assert_eq!(stored, [8.0, 8.0]);
        assert_ne!(
            stored,
            [snap(stored[0], new_grid), snap(stored[1], new_grid)]
        );
    }

    #[test]
    fn parses_machine_readable_boot_estimate() {
        let estimate = parse_boot_estimate_text(
            "boot_frames=12\nboot_delay_ms=100\nboot_container_bytes=123456\nboot_limit_bytes=10485760\nboot_within_limit=true\n",
        )
        .unwrap();

        assert_eq!(estimate.frames, 12);
        assert_eq!(estimate.delay_ms, 100);
        assert_eq!(estimate.container_bytes, 123456);
        assert_eq!(estimate.limit_bytes, 10 * 1024 * 1024);
        assert!(estimate.within_limit);
    }
}

#[cfg(test)]
mod device_preview_mode_tests {
    use super::DevicePreviewMode;

    #[test]
    fn loop_and_once_apply_to_boot_only() {
        assert_eq!(DevicePreviewMode::BootLoop.label(), "Boot (loop)");
        assert_eq!(DevicePreviewMode::BootLoop.loops(), 0);
        assert_eq!(DevicePreviewMode::BootOnce.label(), "Boot (once)");
        assert_eq!(DevicePreviewMode::BootOnce.loops(), 1);
        assert_eq!(DevicePreviewMode::Standby.label(), "Standby");
        assert_eq!(DevicePreviewMode::Standby.loops(), 0);
        assert!(DevicePreviewMode::BootLoop.is_boot());
        assert!(DevicePreviewMode::BootOnce.is_boot());
        assert!(!DevicePreviewMode::Standby.is_boot());
    }
}

fn is_gif(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("gif"))
        .unwrap_or(false)
}

fn file_name(path: Option<&PathBuf>) -> String {
    path.and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("None")
        .to_string()
}

fn load_media_source_frame(path: &Path) -> Option<image::RgbImage> {
    Some(if is_gif(path) {
        let decoder = GifDecoder::new(BufReader::new(File::open(path).ok()?)).ok()?;
        let frame = decoder.into_frames().next()?.ok()?;
        image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8()
    } else {
        image::open(path).ok()?.into_rgb8()
    })
}

fn detect_device_info() -> DeviceInfo {
    let mut info = DeviceInfo {
        vid: "264a".into(),
        pid: "233c".into(),
        ..Default::default()
    };
    let Ok(entries) = std::fs::read_dir("/sys/class/hidraw") else {
        return info;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let device_link = entry.path().join("device");
        let uevent = std::fs::read_to_string(device_link.join("uevent"))
            .unwrap_or_default()
            .to_ascii_uppercase();
        if !uevent.contains("264A") || !uevent.contains("233C") {
            continue;
        }
        info.connected = true;
        let dev = PathBuf::from(format!("/dev/{name}"));
        let link = std::fs::canonicalize(&device_link).unwrap_or(device_link.clone());
        let link_text = link.to_string_lossy();
        if link_text.contains(":1.0") {
            info.control_hidraw = Some(dev.clone());
        }
        if link_text.contains(":1.1") {
            info.image_hidraw = Some(dev);
        }
        for ancestor in link.ancestors() {
            let vid = std::fs::read_to_string(ancestor.join("idVendor")).ok();
            let pid = std::fs::read_to_string(ancestor.join("idProduct")).ok();
            if vid
                .as_deref()
                .map(str::trim)
                .map(|v| v.eq_ignore_ascii_case("264a"))
                == Some(true)
                && pid
                    .as_deref()
                    .map(str::trim)
                    .map(|v| v.eq_ignore_ascii_case("233c"))
                    == Some(true)
            {
                info.product = read_trimmed(ancestor.join("product"));
                info.manufacturer = read_trimmed(ancestor.join("manufacturer"));
                info.revision = read_trimmed(ancestor.join("bcdDevice"));
                break;
            }
        }
    }
    info
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn path_or_dash(path: Option<&PathBuf>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn media_transform_args(media: &MediaTransform) -> Vec<String> {
    let fit = match media.fit {
        ImageFit::Cover => "cover",
        ImageFit::Contain => "contain",
        ImageFit::Stretch => "stretch",
    };
    vec![
        "--media-fit".into(),
        fit.into(),
        "--media-pan-x".into(),
        media.transform.pan_x.to_string(),
        "--media-pan-y".into(),
        media.transform.pan_y.to_string(),
        "--media-zoom".into(),
        media.transform.zoom.to_string(),
        "--media-stretch-x".into(),
        media.transform.stretch_x.to_string(),
        "--media-stretch-y".into(),
        media.transform.stretch_y.to_string(),
        "--media-rotation".into(),
        media.transform.rotation.to_string(),
        "--media-canvas".into(),
        format!(
            "#{:02x}{:02x}{:02x}",
            media.canvas_color[0], media.canvas_color[1], media.canvas_color[2]
        ),
    ]
}

fn parse_boot_estimate(output: std::process::Output) -> Result<BootEstimate, String> {
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    parse_boot_estimate_text(&String::from_utf8_lossy(&output.stdout))
}

fn parse_boot_estimate_text(text: &str) -> Result<BootEstimate, String> {
    let value = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| format!("boot inspection omitted {name}"))
    };
    Ok(BootEstimate {
        frames: value("boot_frames=")?
            .parse()
            .map_err(|_| "invalid boot frame count".to_string())?,
        delay_ms: value("boot_delay_ms=")?
            .parse()
            .map_err(|_| "invalid boot frame delay".to_string())?,
        container_bytes: value("boot_container_bytes=")?
            .parse()
            .map_err(|_| "invalid boot container size".to_string())?,
        limit_bytes: value("boot_limit_bytes=")?
            .parse()
            .map_err(|_| "invalid boot container limit".to_string())?,
        within_limit: value("boot_within_limit=")?
            .parse()
            .map_err(|_| "invalid boot size verdict".to_string())?,
    })
}

fn daemon_binary_path() -> PathBuf {
    if std::env::var_os("APPIMAGE").is_some() {
        if let Some(path) = dirs::home_dir()
            .map(|home| home.join(".local/bin/th420-display"))
            .filter(|path| path.exists())
        {
            return path;
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("th420-display")))
        .unwrap_or_else(|| PathBuf::from("th420-display"))
}
