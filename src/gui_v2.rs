mod config;
mod instance;
mod profile;
mod renderer;
mod sensors;
mod service_manager;

use clap::Parser;
use config::{
    default_config_path, Config, ImageFit, MediaTransform, Transform2D, WidgetInstance,
    WidgetOverrides, WidgetPlacement, BUILTIN_SENSOR_TEMPLATE_ID,
};
use eframe::egui;
use image::codecs::gif::GifDecoder;
use image::AnimationDecoder;
use instance::{
    current_owner, refresh_owner, replace_and_acquire, request_daemon_command, request_graceful,
    signal_owner, AcquireError, InstanceGuard, InstanceKind, OwnerInfo, ReplaceExisting,
};
use profile::ProfileStore;
use renderer::{
    arc_pointer_angle, centered_grid_coordinates, load_media_frame_at, media_duration,
    transform_media_image, DragSnapState, MediaFootprint, PanRegion, Renderer, RotationDragState,
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
    Profiles,
    Diagnostics,
    Settings,
}

impl Page {
    const ALL: [(Page, &'static str); 6] = [
        (Page::Overview, "Overview"),
        (Page::LiveDisplay, "Live Display"),
        (Page::StandbySettings, "Standby Settings"),
        (Page::Profiles, "Profiles"),
        (Page::Diagnostics, "Diagnostics"),
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
    Standby,
}

impl DevicePreviewMode {
    fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::BootLoop => "Boot (loop)",
            Self::Standby => "Standby",
        }
    }

    fn is_boot(self) -> bool {
        matches!(self, Self::BootLoop)
    }

    fn loops(self) -> usize {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundSource {
    File,
    Stream,
    SolidColor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransformPreset {
    OneToOne,
    Fit,
    Cover,
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SnapSettings {
    enabled: bool,
    show_grid: bool,
    pan_grid: f32,
    rotation_grid: f32,
    edge_snap: bool,
    keep_covered: bool,
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            show_grid: true,
            pan_grid: 8.0,
            rotation_grid: 15.0,
            edge_snap: true,
            keep_covered: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct PendingMediaPreset {
    path: PathBuf,
    transform_at_selection: Transform2D,
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
    spec: DevicePreviewSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DevicePreviewSpec {
    mode: DevicePreviewMode,
    source_path: PathBuf,
    args: Vec<String>,
}

const DEVICE_PREVIEW_RESTART_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Clone, Debug, PartialEq)]
struct BootInspectionKey {
    path: PathBuf,
    transform: MediaTransform,
    start_frame: usize,
    end_frame: usize,
    frame_delay_ms: u32,
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct DeviceTelemetry {
    coolant_temp_c: f32,
    pump_rpm: u16,
    age_ms: u128,
}

struct App {
    shutdown_requested: Arc<AtomicBool>,
    page: Page,
    live_tab: LiveTab,
    standby_tab: StandbyTab,
    preview_mode: PreviewMode,
    device_preview_mode: DevicePreviewMode,
    device_preview_enabled: bool,
    preview_paused_daemon: Option<OwnerInfo>,
    preview_missing_reported: bool,
    background_source: BackgroundSource,
    background_file_path: Option<String>,

    committed: Config,
    working: Config,
    config_path: PathBuf,
    profiles: ProfileStore,
    selected_profile: Option<String>,
    profile_name_edit: String,
    confirm_delete_profile: bool,
    undo: Vec<Config>,
    redo: Vec<Config>,
    pending_undo: Option<Config>,
    suppress_history_once: bool,
    renderer: Renderer,
    sensors: sensors::SensorReader,
    sensor_values: SensorValues,
    last_sensor_update: Instant,

    service_manager: ServiceManager,
    live_display_enabled: bool,
    daemon_running: bool,
    daemon_paused: bool,
    autostart_enabled: bool,
    last_daemon_check: Instant,

    device_info: DeviceInfo,
    device_coolant: Option<f32>,
    device_pump_rpm: Option<u16>,
    device_status_error: Option<String>,
    device_status_pending: bool,
    device_status_tx: Sender<Result<DeviceTelemetry, String>>,
    device_status_rx: Receiver<Result<DeviceTelemetry, String>>,
    last_device_status_request: Instant,
    last_device_scan: Instant,

    preview_texture: Option<egui::TextureHandle>,
    last_preview_update: Instant,

    selected_sensor: Option<String>,
    add_widget_template: String,
    editing_widget_template: Option<String>,
    widget_snap: SnapSettings,
    widget_drag: Option<DragSnapState>,
    widget_rotation_drag: Option<RotationDragState>,

    live_snap: SnapSettings,
    boot_snap: SnapSettings,
    standby_snap: SnapSettings,
    background_drag: Option<DragSnapState>,
    boot_drag: Option<DragSnapState>,
    standby_drag: Option<DragSnapState>,
    background_rotation_drag: Option<RotationDragState>,
    boot_rotation_drag: Option<RotationDragState>,
    standby_rotation_drag: Option<RotationDragState>,
    boot_transform: MediaTransform,
    standby_transform: MediaTransform,
    boot_default_preset: Option<PendingMediaPreset>,
    standby_default_preset: Option<PendingMediaPreset>,

    brightness: u8,
    boot_path: Option<PathBuf>,
    standby_path: Option<PathBuf>,
    boot_source_cache: Option<(PathBuf, image::RgbImage)>,
    boot_animation: Option<(PathBuf, TimedAnimation)>,
    boot_animation_pending: Option<PathBuf>,
    boot_animation_tx: Sender<(PathBuf, Result<TimedAnimation, String>)>,
    boot_animation_rx: Receiver<(PathBuf, Result<TimedAnimation, String>)>,
    boot_trim_start: usize,
    boot_trim_end: usize,
    boot_scrub_frame: usize,
    boot_preview_playing: bool,
    boot_frame_delay_ms: u32,
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
    pending_device_preview_spec: Option<DevicePreviewSpec>,
    last_device_preview_spec_change: Instant,
    device_preview_refresh_now: bool,
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
        let daemon_paused = current_owner(InstanceKind::Daemon)
            .and_then(|owner| request_daemon_command(&owner, "state", Duration::from_secs(1)).ok())
            .is_some_and(|state| state == "paused");
        let autostart_enabled = service_manager.autostart_enabled();
        let background_source = if config.background.image_path.is_some() {
            BackgroundSource::File
        } else {
            BackgroundSource::SolidColor
        };
        let profiles = ProfileStore::new();
        let selected_profile = profiles.active_name();

        let (boot_inspection_tx, boot_inspection_rx) = mpsc::channel();
        let (boot_animation_tx, boot_animation_rx) = mpsc::channel();
        let (standby_worker_tx, standby_worker_rx) = mpsc::channel();
        let (device_status_tx, device_status_rx) = mpsc::channel();
        let mut app = Self {
            shutdown_requested,
            page: Page::Overview,
            live_tab: LiveTab::Background,
            standby_tab: StandbyTab::Boot,
            preview_mode: PreviewMode::Boot,
            device_preview_mode: DevicePreviewMode::Off,
            device_preview_enabled: false,
            preview_paused_daemon: None,
            preview_missing_reported: false,
            background_source,
            background_file_path: config.background.image_path.clone(),
            committed: config.clone(),
            working: config,
            config_path,
            profiles,
            selected_profile,
            profile_name_edit: String::new(),
            confirm_delete_profile: false,
            undo: Vec::new(),
            redo: Vec::new(),
            pending_undo: None,
            suppress_history_once: false,
            renderer: Renderer::new(),
            sensors: sensors::SensorReader::new(),
            sensor_values: SensorValues {
                readings: HashMap::new(),
            },
            last_sensor_update: Instant::now() - Duration::from_secs(10),
            service_manager,
            live_display_enabled: daemon_running,
            daemon_running,
            daemon_paused,
            autostart_enabled,
            last_daemon_check: Instant::now(),
            device_info: detect_device_info(),
            device_coolant: None,
            device_pump_rpm: None,
            device_status_error: None,
            device_status_pending: false,
            device_status_tx,
            device_status_rx,
            last_device_status_request: Instant::now() - Duration::from_secs(10),
            last_device_scan: Instant::now(),
            preview_texture: None,
            last_preview_update: Instant::now() - Duration::from_secs(10),
            selected_sensor: None,
            add_widget_template: BUILTIN_SENSOR_TEMPLATE_ID.to_string(),
            editing_widget_template: None,
            widget_snap: SnapSettings::default(),
            widget_drag: None,
            widget_rotation_drag: None,
            live_snap: SnapSettings::default(),
            boot_snap: SnapSettings::default(),
            standby_snap: SnapSettings::default(),
            background_drag: None,
            boot_drag: None,
            standby_drag: None,
            background_rotation_drag: None,
            boot_rotation_drag: None,
            standby_rotation_drag: None,
            boot_transform: MediaTransform::default(),
            standby_transform: MediaTransform::default(),
            boot_default_preset: None,
            standby_default_preset: None,
            brightness: 80,
            boot_path: None,
            standby_path: None,
            boot_source_cache: None,
            boot_animation: None,
            boot_animation_pending: None,
            boot_animation_tx,
            boot_animation_rx,
            boot_trim_start: 0,
            boot_trim_end: 0,
            boot_scrub_frame: 0,
            boot_preview_playing: true,
            boot_frame_delay_ms: 100,
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
            pending_device_preview_spec: None,
            last_device_preview_spec_change: Instant::now(),
            device_preview_refresh_now: false,
            status_text: String::new(),
            last_error: None,
        };
        app.refresh_sensors();
        app.start_device_status_refresh();
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
        while let Ok(result) = self.device_status_rx.try_recv() {
            self.device_status_pending = false;
            match result {
                Ok(telemetry) => {
                    self.device_coolant = Some(telemetry.coolant_temp_c);
                    self.device_pump_rpm = Some(telemetry.pump_rpm);
                    self.device_status_error = if telemetry.age_ms > 3_000 {
                        Some(format!(
                            "Device telemetry is stale ({} ms old)",
                            telemetry.age_ms
                        ))
                    } else {
                        None
                    };
                    self.refresh_sensors();
                }
                Err(error) => self.device_status_error = Some(error),
            }
        }
        if !self.device_status_pending
            && self.last_device_status_request.elapsed() >= Duration::from_secs(1)
        {
            self.start_device_status_refresh();
        }
        if self.last_sensor_update.elapsed() >= Duration::from_secs(1) {
            self.refresh_sensors();
            self.last_sensor_update = Instant::now();
        }
        if self.last_daemon_check.elapsed() >= Duration::from_secs(2) {
            self.daemon_running = self.service_manager.daemon_running();
            self.daemon_paused = current_owner(InstanceKind::Daemon)
                .and_then(|owner| {
                    request_daemon_command(&owner, "state", Duration::from_secs(1)).ok()
                })
                .is_some_and(|state| state == "paused");
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
                    Ok(animation) => {
                        let dimensions = animation
                            .frame(0)
                            .map(|frame| (frame.width(), frame.height()));
                        if let Some((width, height)) = dimensions {
                            if let Err(error) = apply_pending_cover(
                                self.boot_path.as_deref(),
                                &path,
                                &mut self.boot_transform,
                                &mut self.boot_default_preset,
                                width,
                                height,
                            ) {
                                self.last_error = Some(error);
                            }
                        }
                        self.boot_trim_start = 0;
                        self.boot_trim_end = animation.frame_count().saturating_sub(1);
                        self.boot_scrub_frame = 0;
                        self.boot_preview_playing = true;
                        self.boot_frame_delay_ms = animation.first_delay_ms();
                        self.boot_animation = Some((path, animation));
                    }
                    Err(error) => {
                        if self
                            .boot_default_preset
                            .as_ref()
                            .is_some_and(|request| request.path == path)
                        {
                            self.boot_default_preset = None;
                        }
                        self.last_error = Some(error);
                    }
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
                                if let Err(error) = apply_pending_cover(
                                    self.standby_path.as_deref(),
                                    &key.path,
                                    &mut self.standby_transform,
                                    &mut self.standby_default_preset,
                                    frame.width(),
                                    frame.height(),
                                ) {
                                    self.last_error = Some(error);
                                }
                                self.standby_source_cache = Some((key.path, key.time_bits, frame));
                            }
                            Err(error) => {
                                if self
                                    .standby_default_preset
                                    .as_ref()
                                    .is_some_and(|request| request.path == key.path)
                                {
                                    self.standby_default_preset = None;
                                }
                                self.last_error = Some(error);
                            }
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
        let current = self.boot_path.as_ref().and_then(|path| {
            self.boot_animation
                .as_ref()
                .filter(|(animation_path, _)| animation_path == path)
                .map(|_| BootInspectionKey {
                    path: path.clone(),
                    transform: self.boot_transform.clone(),
                    start_frame: self.boot_trim_start,
                    end_frame: self.boot_trim_end,
                    frame_delay_ms: self.boot_frame_delay_ms,
                })
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
            args.extend(boot_edit_args(
                key.start_frame,
                key.end_frame,
                key.frame_delay_ms,
            ));
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
        self.device_preview_enabled = false;
        self.resume_preview_daemon();
        if status.success() {
            self.status_text = format!("{} preview finished", job.spec.mode.label());
        } else {
            self.last_error = Some(format!(
                "{} preview process exited with {status}",
                job.spec.mode.label()
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
        if let Some(source) = self.live_source_dimensions() {
            if let Some(error) = media_placement_error(
                Some(source),
                &self.working.background.fit,
                &self.working.background.transform(),
                self.live_snap,
            ) {
                self.last_error = Some(format!("Live background placement is invalid: {error}"));
                return;
            }
        }
        match self.working.save(&self.config_path) {
            Ok(()) => {
                self.committed = self.working.clone();
                self.undo.clear();
                self.redo.clear();
                self.pending_undo = None;
                self.status_text = "Configuration applied".to_string();
                if let Some(name) = self.selected_profile.clone() {
                    if let Err(error) = self
                        .profiles
                        .save(&name, &self.working)
                        .and_then(|_| self.profiles.set_active(&name))
                    {
                        self.last_error = Some(format!(
                            "Configuration applied, but profile update failed: {error}"
                        ));
                    }
                }
            }
            Err(err) => self.last_error = Some(format!("Failed to save config: {err}")),
        }
    }

    fn revert(&mut self) {
        self.working = self.committed.clone();
        self.undo.clear();
        self.redo.clear();
        self.pending_undo = None;
        self.suppress_history_once = true;
        self.sync_background_source();
        self.selected_sensor = None;
    }

    fn sync_background_source(&mut self) {
        self.background_file_path = self.working.background.image_path.clone();
        self.background_source = match self.background_file_path.as_deref() {
            None => BackgroundSource::SolidColor,
            Some(path)
                if self
                    .stream_path
                    .as_ref()
                    .is_some_and(|stream| stream.to_string_lossy() == path) =>
            {
                BackgroundSource::Stream
            }
            Some(_) => BackgroundSource::File,
        };
        self.preview_texture = None;
    }

    fn push_undo(&mut self, before: Config) {
        if before == self.working {
            return;
        }
        if self.undo.last() != Some(&before) {
            self.undo.push(before);
            if self.undo.len() > 64 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    fn undo(&mut self) {
        if let Some(previous) = self.undo.pop() {
            self.redo.push(self.working.clone());
            self.working = previous;
            self.sync_background_source();
            self.pending_undo = None;
            self.suppress_history_once = true;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.working.clone());
            self.working = next;
            self.sync_background_source();
            self.pending_undo = None;
            self.suppress_history_once = true;
        }
    }

    fn capture_history(&mut self, before: Config, ctx: &egui::Context) {
        if self.suppress_history_once {
            self.suppress_history_once = false;
            self.pending_undo = None;
            return;
        }
        let pointer_down = ctx.input(|input| input.pointer.any_down());
        if before != self.working {
            if pointer_down {
                if self.pending_undo.is_none() {
                    self.pending_undo = Some(before);
                }
            } else {
                let base = self.pending_undo.take().unwrap_or(before);
                self.push_undo(base);
            }
        } else if !pointer_down {
            if let Some(base) = self.pending_undo.take() {
                self.push_undo(base);
            }
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let undo = ctx.input(|input| {
            input.modifiers.command && input.key_pressed(egui::Key::Z) && !input.modifiers.shift
        });
        let redo = ctx.input(|input| {
            input.modifiers.command
                && ((input.key_pressed(egui::Key::Z) && input.modifiers.shift)
                    || input.key_pressed(egui::Key::Y))
        });
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
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
                        if ui
                            .add_enabled(!self.redo.is_empty(), egui::Button::new("Redo"))
                            .clicked()
                        {
                            self.redo();
                        }
                        if ui
                            .add_enabled(!self.undo.is_empty(), egui::Button::new("Undo"))
                            .clicked()
                        {
                            self.undo();
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
                    let frame = if self.boot_preview_playing {
                        animation.current_uniform_frame(
                            self.boot_trim_start,
                            self.boot_trim_end,
                            self.boot_frame_delay_ms,
                        )
                    } else {
                        animation.frame(self.boot_scrub_frame)?
                    };
                    return transform_media_image(frame, &self.boot_transform);
                }
                if self
                    .boot_source_cache
                    .as_ref()
                    .map(|(cached_path, _)| cached_path)
                    != Some(path)
                {
                    if let Some(source) = load_media_source_frame(path) {
                        if let Err(error) = apply_pending_cover(
                            self.boot_path.as_deref(),
                            path,
                            &mut self.boot_transform,
                            &mut self.boot_default_preset,
                            source.width(),
                            source.height(),
                        ) {
                            self.last_error = Some(error);
                        }
                        self.boot_source_cache = Some((path.clone(), source));
                    }
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
        if self.live_snap.show_grid {
            paint_centered_grid(ui, rect, self.live_snap.pan_grid);
        }
        ui.painter().circle_stroke(
            rect.center(),
            rect.width() / 2.0,
            egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
        );
        if response.drag_started_by(egui::PointerButton::Primary) {
            self.background_drag = Some(DragSnapState::new([
                self.working.background.pan_x,
                self.working.background.pan_y,
            ]));
        }
        if response.dragged_by(egui::PointerButton::Primary) {
            let delta = response.drag_delta();
            let canvas_delta = [
                delta.x * 480.0 / rect.width(),
                delta.y * 480.0 / rect.height(),
            ];
            let source = self.live_source_dimensions();
            let drag = self.background_drag.get_or_insert_with(|| {
                DragSnapState::new([self.working.background.pan_x, self.working.background.pan_y])
            });
            drag.update(canvas_delta, false, self.live_snap.pan_grid);
            let transform = self.working.background.transform();
            if let Ok(preview) = constrained_media_pan(
                source,
                &self.working.background.fit,
                &transform,
                drag.raw,
                self.live_snap,
                true,
            ) {
                drag.preview = preview;
            }
            self.working.background.pan_x = drag.preview[0];
            self.working.background.pan_y = drag.preview[1];
        }
        if response.drag_stopped_by(egui::PointerButton::Primary) {
            if let Some(drag) = self.background_drag.take() {
                let committed = if drag.moved() {
                    drag.preview
                } else {
                    drag.origin
                };
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
        if response.drag_started_by(egui::PointerButton::Secondary) {
            self.background_rotation_drag = response.interact_pointer_pos().and_then(|pointer| {
                arc_pointer_angle(
                    [pointer.x, pointer.y],
                    [rect.center().x, rect.center().y],
                    rect.width() * 0.08,
                )
                .map(|angle| RotationDragState::new(self.working.background.rotation, angle))
            });
        }
        if response.dragged_by(egui::PointerButton::Secondary) {
            let source = self.live_source_dimensions();
            if let (Some(pointer), Some(drag)) = (
                response.interact_pointer_pos(),
                self.background_rotation_drag.as_mut(),
            ) {
                if let Some(angle) = arc_pointer_angle(
                    [pointer.x, pointer.y],
                    [rect.center().x, rect.center().y],
                    rect.width() * 0.08,
                ) {
                    drag.update(angle, self.live_snap.enabled, self.live_snap.rotation_grid);
                    let mut candidate = self.working.background.transform();
                    candidate.rotation = drag.preview;
                    if let Some(error) = media_placement_error(
                        source,
                        &self.working.background.fit,
                        &candidate,
                        self.live_snap,
                    ) {
                        self.last_error = Some(error);
                    } else {
                        self.working.background.rotation = drag.preview;
                    }
                }
            }
        }
        if response.drag_stopped_by(egui::PointerButton::Secondary) {
            if let Some(drag) = self.background_rotation_drag.take() {
                let mut candidate = self.working.background.transform();
                candidate.rotation =
                    drag.finish(self.live_snap.enabled, self.live_snap.rotation_grid);
                if media_placement_error(
                    self.live_source_dimensions(),
                    &self.working.background.fit,
                    &candidate,
                    self.live_snap,
                )
                .is_none()
                {
                    self.working.background.rotation = candidate.rotation;
                }
            }
        }
        if let Some(drag) = self.background_rotation_drag {
            ui.label(
                egui::RichText::new(format!(
                    "Rotate: {:.1}°    Preview: {:.1}°",
                    drag.raw, drag.preview
                ))
                .small()
                .monospace(),
            );
        }
        if response.hovered() {
            let scroll = ctx.input(|input| input.raw_scroll_delta.y);
            if scroll != 0.0 {
                let mut candidate = self.working.background.transform();
                candidate.zoom = (candidate.zoom * (1.0 + scroll * 0.001)).clamp(0.05, 8.0);
                if let Some(error) = media_placement_error(
                    self.live_source_dimensions(),
                    &self.working.background.fit,
                    &candidate,
                    self.live_snap,
                ) {
                    self.last_error = Some(error);
                } else {
                    self.working.background.zoom = candidate.zoom;
                }
            }
        }
    }

    fn active_media_transform(&self) -> &MediaTransform {
        match self.preview_mode {
            PreviewMode::Boot => &self.boot_transform,
            PreviewMode::Standby => &self.standby_transform,
        }
    }

    fn live_source_dimensions(&self) -> Option<(u32, u32)> {
        self.working
            .background
            .image_path
            .as_deref()
            .and_then(|path| self.renderer.background_source_dimensions(path))
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

    fn active_media_rotation_drag_mut(&mut self) -> &mut Option<RotationDragState> {
        match self.preview_mode {
            PreviewMode::Boot => &mut self.boot_rotation_drag,
            PreviewMode::Standby => &mut self.standby_rotation_drag,
        }
    }

    fn active_media_snap(&self) -> SnapSettings {
        match self.preview_mode {
            PreviewMode::Boot => self.boot_snap,
            PreviewMode::Standby => self.standby_snap,
        }
    }

    fn boot_source_dimensions_all(&self) -> Vec<(u32, u32)> {
        let Some(path) = self.boot_path.as_ref() else {
            return Vec::new();
        };
        if let Some((_, animation)) = self
            .boot_animation
            .as_ref()
            .filter(|(animation_path, _)| animation_path == path)
        {
            return animation.frame_dimensions();
        }
        self.boot_source_cache
            .as_ref()
            .filter(|(cached_path, _)| cached_path == path)
            .map(|(_, frame)| vec![(frame.width(), frame.height())])
            .unwrap_or_default()
    }

    fn active_media_source_dimensions(&self) -> Vec<(u32, u32)> {
        match self.preview_mode {
            PreviewMode::Boot => self.boot_source_dimensions_all(),
            PreviewMode::Standby => self.standby_source_dimensions().into_iter().collect(),
        }
    }

    fn standby_source_dimensions(&self) -> Option<(u32, u32)> {
        let path = self.standby_path.as_ref()?;
        self.standby_source_cache
            .as_ref()
            .filter(|(cached_path, _, _)| cached_path == path)
            .map(|(_, _, frame)| (frame.width(), frame.height()))
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
        let active_snap = self.active_media_snap();
        if active_snap.show_grid {
            paint_centered_grid(ui, rect, active_snap.pan_grid);
        }
        ui.painter().circle_stroke(
            rect.center(),
            rect.width() / 2.0,
            egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
        );

        if response.drag_started_by(egui::PointerButton::Primary) {
            let transform = &self.active_media_transform().transform;
            *self.active_media_drag_mut() =
                Some(DragSnapState::new([transform.pan_x, transform.pan_y]));
        }
        if response.dragged_by(egui::PointerButton::Primary) {
            let delta = response.drag_delta();
            let canvas_delta = [
                delta.x * 480.0 / rect.width(),
                delta.y * 480.0 / rect.height(),
            ];
            let origin = {
                let transform = &self.active_media_transform().transform;
                [transform.pan_x, transform.pan_y]
            };
            let snap = self.active_media_snap();
            let raw = {
                let drag = self
                    .active_media_drag_mut()
                    .get_or_insert_with(|| DragSnapState::new(origin));
                drag.update(canvas_delta, false, snap.pan_grid);
                drag.raw
            };
            let sources = self.active_media_source_dimensions();
            let media = self.active_media_transform();
            let preview =
                constrained_media_pan_many(&sources, &media.fit, &media.transform, raw, snap, true)
                    .unwrap_or(raw);
            self.active_media_drag_mut().as_mut().unwrap().preview = preview;
            let transform = &mut self.active_media_transform_mut().transform;
            transform.pan_x = preview[0];
            transform.pan_y = preview[1];
        }
        if response.drag_stopped_by(egui::PointerButton::Primary) {
            if let Some(drag) = self.active_media_drag_mut().take() {
                let committed = if drag.moved() {
                    drag.preview
                } else {
                    drag.origin
                };
                let transform = &mut self.active_media_transform_mut().transform;
                transform.pan_x = committed[0];
                transform.pan_y = committed[1];
                self.device_preview_refresh_now = true;
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
        if response.drag_started_by(egui::PointerButton::Secondary) {
            let origin = self.active_media_transform().transform.rotation;
            *self.active_media_rotation_drag_mut() =
                response.interact_pointer_pos().and_then(|pointer| {
                    arc_pointer_angle(
                        [pointer.x, pointer.y],
                        [rect.center().x, rect.center().y],
                        rect.width() * 0.08,
                    )
                    .map(|angle| RotationDragState::new(origin, angle))
                });
        }
        if response.dragged_by(egui::PointerButton::Secondary) {
            if let Some(pointer) = response.interact_pointer_pos() {
                if let Some(angle) = arc_pointer_angle(
                    [pointer.x, pointer.y],
                    [rect.center().x, rect.center().y],
                    rect.width() * 0.08,
                ) {
                    let snap = self.active_media_snap();
                    let preview = if let Some(drag) = self.active_media_rotation_drag_mut() {
                        drag.update(angle, snap.enabled, snap.rotation_grid);
                        Some(drag.preview)
                    } else {
                        None
                    };
                    if let Some(preview) = preview {
                        let sources = self.active_media_source_dimensions();
                        let media = self.active_media_transform().clone();
                        let mut candidate = media.transform;
                        candidate.rotation = preview;
                        if let Some(error) =
                            media_placement_error_many(&sources, &media.fit, &candidate, snap)
                        {
                            self.last_error = Some(error);
                        } else {
                            self.active_media_transform_mut().transform.rotation = preview;
                        }
                    }
                }
            }
        }
        if response.drag_stopped_by(egui::PointerButton::Secondary) {
            let snap = self.active_media_snap();
            if let Some(drag) = self.active_media_rotation_drag_mut().take() {
                let committed = drag.finish(snap.enabled, snap.rotation_grid);
                let sources = self.active_media_source_dimensions();
                let media = self.active_media_transform().clone();
                let mut candidate = media.transform;
                candidate.rotation = committed;
                if media_placement_error_many(&sources, &media.fit, &candidate, snap).is_none() {
                    self.active_media_transform_mut().transform.rotation = committed;
                }
                self.device_preview_refresh_now = true;
            }
        }
        if let Some(drag) = *self.active_media_rotation_drag_mut() {
            ui.label(
                egui::RichText::new(format!(
                    "Rotate: {:.1}°    Preview: {:.1}°",
                    drag.raw, drag.preview
                ))
                .small()
                .monospace(),
            );
        }
        if response.hovered() {
            let scroll = ctx.input(|input| input.raw_scroll_delta.y);
            if scroll != 0.0 {
                let sources = self.active_media_source_dimensions();
                let media = self.active_media_transform().clone();
                let mut candidate = media.transform;
                candidate.zoom = (candidate.zoom * (1.0 + scroll * 0.001)).clamp(0.05, 8.0);
                if let Some(error) = media_placement_error_many(
                    &sources,
                    &media.fit,
                    &candidate,
                    self.active_media_snap(),
                ) {
                    self.last_error = Some(error);
                } else {
                    self.active_media_transform_mut().transform.zoom = candidate.zoom;
                }
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

        if self.widget_snap.show_grid {
            paint_centered_grid(ui, rect, self.widget_snap.pan_grid);
        }

        let pointer_canvas = response.interact_pointer_pos().map(|pointer| {
            [
                (pointer.x - rect.center().x) * 480.0 / rect.width(),
                (pointer.y - rect.center().y) * 480.0 / rect.height(),
            ]
        });
        if response.clicked_by(egui::PointerButton::Primary) {
            self.selected_sensor =
                pointer_canvas.and_then(|point| self.widget_at(point, 4.0 * 480.0 / rect.width()));
        }
        if response.drag_started_by(egui::PointerButton::Primary) {
            if let Some(point) = pointer_canvas {
                if let Some(id) = self.widget_at(point, 4.0 * 480.0 / rect.width()) {
                    self.selected_sensor = Some(id.clone());
                    if let Some(instance) = self
                        .working
                        .widget_instances
                        .iter()
                        .find(|instance| instance.id == id)
                    {
                        self.widget_drag = Some(DragSnapState::new([
                            instance.transform.pan_x,
                            instance.transform.pan_y,
                        ]));
                    }
                }
            }
        }
        if response.dragged_by(egui::PointerButton::Primary) {
            let delta = response.drag_delta() * 480.0 / rect.width();
            if let (Some(id), Some(drag)) =
                (self.selected_sensor.clone(), self.widget_drag.as_mut())
            {
                drag.update(
                    [delta.x, delta.y],
                    self.widget_snap.enabled,
                    self.widget_snap.pan_grid,
                );
                if let Some(instance) = self
                    .working
                    .widget_instances
                    .iter_mut()
                    .find(|instance| instance.id == id)
                {
                    instance.transform.pan_x = drag.preview[0];
                    instance.transform.pan_y = drag.preview[1];
                }
            }
        }
        if response.drag_stopped_by(egui::PointerButton::Primary) {
            if let (Some(id), Some(drag)) = (self.selected_sensor.clone(), self.widget_drag.take())
            {
                let committed = drag.finish(self.widget_snap.enabled, self.widget_snap.pan_grid);
                if let Some(instance) = self
                    .working
                    .widget_instances
                    .iter_mut()
                    .find(|instance| instance.id == id)
                {
                    instance.transform.pan_x = committed[0];
                    instance.transform.pan_y = committed[1];
                }
            }
        }
        if response.drag_started_by(egui::PointerButton::Secondary) {
            if let (Some(id), Some(pointer)) = (
                self.selected_sensor.clone(),
                response.interact_pointer_pos(),
            ) {
                if let Some(instance) = self
                    .working
                    .widget_instances
                    .iter()
                    .find(|instance| instance.id == id)
                {
                    let center = egui::pos2(
                        rect.center().x + instance.transform.pan_x * rect.width() / 480.0,
                        rect.center().y + instance.transform.pan_y * rect.height() / 480.0,
                    );
                    self.widget_rotation_drag = arc_pointer_angle(
                        [pointer.x, pointer.y],
                        [center.x, center.y],
                        rect.width() * 0.04,
                    )
                    .map(|angle| RotationDragState::new(instance.transform.rotation, angle));
                }
            }
        }
        if response.dragged_by(egui::PointerButton::Secondary) {
            if let (Some(id), Some(pointer), Some(drag)) = (
                self.selected_sensor.clone(),
                response.interact_pointer_pos(),
                self.widget_rotation_drag.as_mut(),
            ) {
                if let Some(instance) = self
                    .working
                    .widget_instances
                    .iter_mut()
                    .find(|instance| instance.id == id)
                {
                    let center = egui::pos2(
                        rect.center().x + instance.transform.pan_x * rect.width() / 480.0,
                        rect.center().y + instance.transform.pan_y * rect.height() / 480.0,
                    );
                    if let Some(angle) = arc_pointer_angle(
                        [pointer.x, pointer.y],
                        [center.x, center.y],
                        rect.width() * 0.04,
                    ) {
                        drag.update(
                            angle,
                            self.widget_snap.enabled,
                            self.widget_snap.rotation_grid,
                        );
                        instance.transform.rotation = drag.preview;
                    }
                }
            }
        }
        if response.drag_stopped_by(egui::PointerButton::Secondary) {
            if let (Some(id), Some(drag)) = (
                self.selected_sensor.clone(),
                self.widget_rotation_drag.take(),
            ) {
                if let Some(instance) = self
                    .working
                    .widget_instances
                    .iter_mut()
                    .find(|instance| instance.id == id)
                {
                    instance.transform.rotation =
                        drag.finish(self.widget_snap.enabled, self.widget_snap.rotation_grid);
                }
            }
        }

        if let Some(id) = &self.selected_sensor {
            if let Some(instance) = self
                .working
                .widget_instances
                .iter()
                .find(|instance| &instance.id == id)
            {
                let point = egui::pos2(
                    rect.center().x + instance.transform.pan_x * rect.width() / 480.0,
                    rect.center().y + instance.transform.pan_y * rect.height() / 480.0,
                );
                let _ = point;
                if let Some(corners) = self.renderer.widget_corners(&self.working, id) {
                    let points: Vec<egui::Pos2> = corners
                        .iter()
                        .map(|[x, y]| {
                            egui::pos2(
                                rect.center().x + x * rect.width() / 480.0,
                                rect.center().y + y * rect.height() / 480.0,
                            )
                        })
                        .collect();
                    ui.painter().add(egui::Shape::closed_line(
                        points,
                        egui::Stroke::new(2.0_f32, egui::Color32::YELLOW),
                    ));
                }
            }
        }
        if let Some(drag) = self.widget_drag {
            ui.label(
                egui::RichText::new(format!(
                    "Drag: {:.1}, {:.1} px    Preview: {:.1}, {:.1} px",
                    drag.raw[0], drag.raw[1], drag.preview[0], drag.preview[1]
                ))
                .small()
                .monospace(),
            );
        }
        if let Some(drag) = self.widget_rotation_drag {
            ui.label(
                egui::RichText::new(format!(
                    "Rotate: {:.1}°    Preview: {:.1}°",
                    drag.raw, drag.preview
                ))
                .small()
                .monospace(),
            );
        }
    }

    fn widget_at(&self, point: [f32; 2], tolerance: f32) -> Option<String> {
        self.working
            .widget_instances
            .iter()
            .rev()
            .filter(|instance| instance.visible && self.working.overlay_enabled)
            .filter_map(|instance| {
                let widget = self.working.resolved_widget(instance)?;
                if !self.sensor_values.readings.contains_key(&widget.source_id)
                    && !widget.style.show_missing
                {
                    return None;
                }
                self.renderer
                    .widget_contains(&self.working, &instance.id, point, tolerance)
                    .then(|| instance.id.clone())
            })
            .next()
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
        if self.daemon_paused && self.preview_paused_daemon.is_none() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Live daemon is paused").color(egui::Color32::YELLOW));
                if ui.button("Resume daemon").clicked() {
                    if let Some(owner) = current_owner(InstanceKind::Daemon) {
                        match request_daemon_command(&owner, "resume", Duration::from_secs(5)) {
                            Ok(response) if response.starts_with("running") => {
                                self.daemon_paused = false;
                                self.status_text = "Live daemon resumed".into();
                            }
                            Ok(response) => {
                                self.last_error = Some(format!(
                                    "Daemon returned an unexpected resume state: {response}"
                                ));
                            }
                            Err(error) => {
                                self.last_error = Some(format!("Failed to resume daemon: {error}"));
                            }
                        }
                    }
                }
            });
        }
        ui.add_space(10.0);
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Device metrics").strong());
                if ui
                    .add_enabled(
                        self.device_info.connected
                            && self.device_preview_job.is_none()
                            && !self.device_status_pending,
                        egui::Button::new("Refresh"),
                    )
                    .clicked()
                {
                    self.start_device_status_refresh();
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
            if let Some(error) = &self.device_status_error {
                ui.label(
                    egui::RichText::new(error)
                        .small()
                        .color(egui::Color32::YELLOW),
                );
            }
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
                                apply_background_preset_from_source(
                                    &mut self.working.background,
                                    TransformPreset::Cover,
                                )
                                .unwrap_or_else(|error| self.last_error = Some(error));
                            }
                        }
                    }
                });
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
                                apply_background_preset_from_source(
                                    &mut self.working.background,
                                    TransformPreset::Cover,
                                )
                                .unwrap_or_else(|error| self.last_error = Some(error));
                            }
                        }
                    }
                });
            }
            BackgroundSource::SolidColor => {}
        }
        ui.horizontal(|ui| {
            ui.label("Canvas color");
            rgb_editor(
                ui,
                "live-canvas",
                &mut self.working.background.background_color,
            );
        });
        self.background_transform_controls(ui);
    }

    fn background_transform_controls(&mut self, ui: &mut egui::Ui) {
        let before = self.working.background.transform();
        let covered_before = self.live_snap.keep_covered;
        let source_dimensions = self.live_source_dimensions();
        let mut pan_edited = false;
        ui.separator();
        ui.label(egui::RichText::new("Transform").strong());
        if self.background_source != BackgroundSource::SolidColor {
            ui.horizontal(|ui| {
                ui.label("Presets");
                if ui
                    .button("1:1")
                    .on_hover_text("Display source pixels at native size")
                    .clicked()
                {
                    apply_background_preset_from_source(
                        &mut self.working.background,
                        TransformPreset::OneToOne,
                    )
                    .unwrap_or_else(|error| self.last_error = Some(error));
                    self.background_drag = None;
                }
                if ui
                    .button("Fit")
                    .on_hover_text("Contain the whole source inside the display")
                    .clicked()
                {
                    apply_background_preset_from_source(
                        &mut self.working.background,
                        TransformPreset::Fit,
                    )
                    .unwrap_or_else(|error| self.last_error = Some(error));
                    self.background_drag = None;
                }
                if ui
                    .button("Cover")
                    .on_hover_text("Fill the display while preserving aspect ratio")
                    .clicked()
                {
                    apply_background_preset_from_source(
                        &mut self.working.background,
                        TransformPreset::Cover,
                    )
                    .unwrap_or_else(|error| self.last_error = Some(error));
                    self.background_drag = None;
                }
                if ui
                    .button("Stretch")
                    .on_hover_text("Fill the display without preserving aspect ratio")
                    .clicked()
                {
                    apply_background_preset_from_source(
                        &mut self.working.background,
                        TransformPreset::Stretch,
                    )
                    .unwrap_or_else(|error| self.last_error = Some(error));
                    self.background_drag = None;
                }
            });
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.live_snap.enabled, "Snap transforms");
            ui.checkbox(&mut self.live_snap.show_grid, "Show grid");
            ui.add(
                egui::DragValue::new(&mut self.live_snap.pan_grid)
                    .range(1.0..=120.0)
                    .suffix(" px"),
            );
            ui.add(
                egui::DragValue::new(&mut self.live_snap.rotation_grid)
                    .range(1.0..=90.0)
                    .suffix(" deg"),
            );
        });
        if self.background_source != BackgroundSource::SolidColor {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.live_snap.edge_snap, "Snap media edges (8 px)");
                ui.checkbox(&mut self.live_snap.keep_covered, "Keep viewport covered");
                if ui.small_button("Center image").clicked() {
                    self.working.background.pan_x = 0.0;
                    self.working.background.pan_y = 0.0;
                    self.background_drag = None;
                    pan_edited = true;
                }
            });
        }
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
                pan_edited = true;
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
        if self.background_source != BackgroundSource::SolidColor {
            let current = self.working.background.transform();
            let geometry_changed = current.zoom != before.zoom
                || current.stretch_x != before.stretch_x
                || current.stretch_y != before.stretch_y
                || current.rotation != before.rotation;
            if let Some(source) = source_dimensions {
                if pan_edited {
                    let transform = self.working.background.transform();
                    if let Ok([x, y]) = constrained_media_pan(
                        Some(source),
                        &self.working.background.fit,
                        &transform,
                        [transform.pan_x, transform.pan_y],
                        self.live_snap,
                        false,
                    ) {
                        self.working.background.pan_x = x;
                        self.working.background.pan_y = y;
                    }
                }
                let current = self.working.background.transform();
                let placement_error = (geometry_changed
                    || covered_before != self.live_snap.keep_covered)
                    .then(|| {
                        media_placement_error(
                            Some(source),
                            &self.working.background.fit,
                            &current,
                            self.live_snap,
                        )
                    })
                    .flatten();
                if let Some(error) = placement_error {
                    if geometry_changed {
                        self.working.background.zoom = before.zoom;
                        self.working.background.stretch_x = before.stretch_x;
                        self.working.background.stretch_y = before.stretch_y;
                        self.working.background.rotation = before.rotation;
                    }
                    self.live_snap.keep_covered = covered_before;
                    self.last_error = Some(error);
                }
            }
        }
    }

    fn show_overlay_editor(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Widgets").strong());
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("add-widget-template")
                .selected_text(
                    self.working
                        .widget_template(&self.add_widget_template)
                        .map(|template| template.name.as_str())
                        .unwrap_or("Select template"),
                )
                .show_ui(ui, |ui| {
                    for template in &self.working.widget_templates {
                        ui.selectable_value(
                            &mut self.add_widget_template,
                            template.id.clone(),
                            &template.name,
                        );
                    }
                });
            if ui.button("Add widget").clicked() {
                if let Some(source) = self.working.sensors.first() {
                    let id = next_unique_id(
                        "widget",
                        self.working
                            .widget_instances
                            .iter()
                            .map(|instance| instance.id.as_str()),
                    );
                    self.working.widget_instances.push(WidgetInstance {
                        id: id.clone(),
                        name: format!("{} widget", source.label),
                        template_id: self.add_widget_template.clone(),
                        source_id: source.id.clone(),
                        visible: true,
                        transform: WidgetPlacement::default(),
                        overrides: WidgetOverrides::default(),
                    });
                    self.selected_sensor = Some(id);
                }
            }
        });

        let mut remove = None;
        let mut duplicate = None;
        let mut move_by = None;
        let instance_count = self.working.widget_instances.len();
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                for (index, instance) in self.working.widget_instances.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut instance.visible, "");
                        if ui
                            .selectable_label(
                                self.selected_sensor.as_deref() == Some(&instance.id),
                                &instance.name,
                            )
                            .clicked()
                        {
                            self.selected_sensor = Some(instance.id.clone());
                        }
                        if ui.small_button("↑").clicked() && index > 0 {
                            move_by = Some((index, index - 1));
                        }
                        if ui.small_button("↓").clicked() && index + 1 < instance_count {
                            move_by = Some((index, index + 1));
                        }
                        if ui.small_button("Duplicate").clicked() {
                            duplicate = Some(index);
                        }
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                }
            });
        if let Some((from, to)) = move_by {
            self.working.widget_instances.swap(from, to);
        }
        if let Some(index) = duplicate {
            let mut copy = self.working.widget_instances[index].clone();
            copy.id = next_unique_id(
                &copy.id,
                self.working
                    .widget_instances
                    .iter()
                    .map(|instance| instance.id.as_str()),
            );
            copy.name = format!("{} copy", copy.name);
            copy.transform.pan_x += self.widget_snap.pan_grid;
            copy.transform.pan_y += self.widget_snap.pan_grid;
            self.selected_sensor = Some(copy.id.clone());
            self.working.widget_instances.insert(index + 1, copy);
        }
        if let Some(index) = remove {
            let removed = self.working.widget_instances.remove(index);
            if self.selected_sensor.as_deref() == Some(&removed.id) {
                self.selected_sensor = None;
            }
        }

        if let Some(id) = self.selected_sensor.clone() {
            if let Some(index) = self
                .working
                .widget_instances
                .iter()
                .position(|instance| instance.id == id)
            {
                ui.separator();
                ui.label(egui::RichText::new("Selected instance").strong());
                let template_names: Vec<(String, String)> = self
                    .working
                    .widget_templates
                    .iter()
                    .map(|template| (template.id.clone(), template.name.clone()))
                    .collect();
                let source_names: Vec<(String, String)> = self
                    .working
                    .sensors
                    .iter()
                    .map(|source| (source.id.clone(), source.label.clone()))
                    .collect();
                let resolved = self
                    .working
                    .resolved_widget(&self.working.widget_instances[index]);
                let instance = &mut self.working.widget_instances[index];
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut instance.name);
                });
                egui::ComboBox::from_label("Template")
                    .selected_text(
                        template_names
                            .iter()
                            .find(|(id, _)| id == &instance.template_id)
                            .map(|(_, name)| name.as_str())
                            .unwrap_or("Missing template"),
                    )
                    .show_ui(ui, |ui| {
                        for (id, name) in &template_names {
                            ui.selectable_value(&mut instance.template_id, id.clone(), name);
                        }
                    });
                egui::ComboBox::from_label("Data source")
                    .selected_text(
                        source_names
                            .iter()
                            .find(|(id, _)| id == &instance.source_id)
                            .map(|(_, name)| name.as_str())
                            .unwrap_or("Missing source"),
                    )
                    .show_ui(ui, |ui| {
                        for (id, name) in &source_names {
                            ui.selectable_value(&mut instance.source_id, id.clone(), name);
                        }
                    });
                if let Some(resolved) = resolved {
                    ui.horizontal(|ui| {
                        ui.label("Box size");
                        let mut width = instance.overrides.width.unwrap_or(resolved.style.width);
                        let mut height = instance.overrides.height.unwrap_or(resolved.style.height);
                        if ui
                            .add(
                                egui::DragValue::new(&mut width)
                                    .range(1.0..=4096.0)
                                    .prefix("W: ")
                                    .suffix(" px"),
                            )
                            .changed()
                            && width.is_finite()
                        {
                            instance.overrides.width = Some(width);
                        }
                        if ui
                            .add(
                                egui::DragValue::new(&mut height)
                                    .range(1.0..=4096.0)
                                    .prefix("H: ")
                                    .suffix(" px"),
                            )
                            .changed()
                            && height.is_finite()
                        {
                            instance.overrides.height = Some(height);
                        }
                        if (instance.overrides.width.is_some()
                            || instance.overrides.height.is_some())
                            && ui.small_button("Use template size").clicked()
                        {
                            instance.overrides.width = None;
                            instance.overrides.height = None;
                        }
                    });
                    let current = self
                        .sensor_values
                        .readings
                        .get(&resolved.source_id)
                        .copied()
                        .map(|value| renderer::format_value(&resolved.unit, value))
                        .unwrap_or_else(|| "--".to_string());
                    let representative = renderer::format_value(&resolved.unit, 100.0);
                    let required = [current.as_str(), representative.as_str(), "--"]
                        .into_iter()
                        .filter_map(|value| self.renderer.widget_content_size(&resolved, value))
                        .fold([0.0_f32, 0.0_f32], |size, item| {
                            [size[0].max(item[0]), size[1].max(item[1])]
                        });
                    if ui.small_button("Fit to content").clicked() {
                        instance.overrides.width = Some(required[0].ceil().clamp(1.0, 4096.0));
                        instance.overrides.height = Some(required[1].ceil().clamp(1.0, 4096.0));
                    }
                    if required[0] > resolved.style.width || required[1] > resolved.style.height {
                        ui.label(
                            egui::RichText::new("Text may overflow the widget box")
                                .color(egui::Color32::YELLOW),
                        );
                    }
                    override_text_row(ui, "Label", &resolved.label, &mut instance.overrides.label);
                    override_text_row(ui, "Unit", &resolved.unit, &mut instance.overrides.unit);
                    override_f32_row(
                        ui,
                        "Value size",
                        resolved.style.value_font_size,
                        &mut instance.overrides.value_font_size,
                        8.0..=180.0,
                    );
                    override_f32_row(
                        ui,
                        "Label size",
                        resolved.style.label_font_size,
                        &mut instance.overrides.label_font_size,
                        6.0..=96.0,
                    );
                    override_rgb_row(
                        ui,
                        "Label text color",
                        resolved.style.label_color,
                        &mut instance.overrides.label_color,
                        &instance.id,
                    );
                    ui.collapsing("Background", |ui| {
                        override_rgb_row(
                            ui,
                            "Color",
                            resolved.style.background_color,
                            &mut instance.overrides.background_color,
                            &instance.id,
                        );
                        override_background_transparency_row(
                            ui,
                            resolved.style.background_opacity,
                            &mut instance.overrides.background_opacity,
                        );
                        ui.label("100% transparency hides the background.");
                    });
                    ui.collapsing("Value text colors", |ui| {
                        if instance.overrides.color_map.is_none()
                            && ui.button("Override template thresholds").clicked()
                        {
                            instance.overrides.color_map = Some(resolved.style.color_map.clone());
                        }
                        if let Some(map) = &mut instance.overrides.color_map {
                            color_map_editor(ui, map, &instance.id);
                            if ui.small_button("Use template thresholds").clicked() {
                                instance.overrides.color_map = None;
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("Inherited from template")
                                    .small()
                                    .color(egui::Color32::GRAY),
                            );
                        }
                    });
                }
                ui.collapsing("Transform", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Pan");
                        ui.add(egui::DragValue::new(&mut instance.transform.pan_x).prefix("X: "));
                        ui.add(egui::DragValue::new(&mut instance.transform.pan_y).prefix("Y: "));
                    });
                    ui.add(
                        egui::DragValue::new(&mut instance.transform.rotation)
                            .suffix(" deg")
                            .speed(0.25),
                    );
                });
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.widget_snap.enabled, "Snap transforms");
            ui.checkbox(&mut self.widget_snap.show_grid, "Show grid");
            ui.add(
                egui::DragValue::new(&mut self.widget_snap.pan_grid)
                    .range(1.0..=120.0)
                    .suffix(" px"),
            );
            ui.add(
                egui::DragValue::new(&mut self.widget_snap.rotation_grid)
                    .range(1.0..=90.0)
                    .suffix(" deg"),
            );
        });

        self.show_template_manager(ui);
        self.show_source_editor(ui);
    }

    fn show_source_editor(&mut self, ui: &mut egui::Ui) {
        ui.collapsing("Data sources", |ui| {
            ui.label(egui::RichText::new("Source names and units provide widget defaults. Widget visibility and order are controlled by the instance list; colors are controlled by templates or instance overrides.").small().color(egui::Color32::GRAY));
            let defaults = Config::default();
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for source in &mut self.working.sensors {
                    let current = self.sensor_values.readings.get(&source.id).copied();
                    ui.collapsing(format!("{} · {}", source.label, source.id), |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Label");
                            ui.text_edit_singleline(&mut source.label);
                            ui.label("Unit");
                            ui.text_edit_singleline(&mut source.unit);
                            if ui.small_button("Reset source").clicked() {
                                if let Some(default) = defaults.sensor_by_id(&source.id) {
                                    source.label = default.label.clone();
                                    source.unit = default.unit.clone();
                                }
                            }
                        });
                        ui.label(current.map(|value| renderer::format_value(&source.unit, value)).unwrap_or_else(|| "No reading yet".into()));
                    });
                }
            });
        });
    }

    fn show_template_manager(&mut self, ui: &mut egui::Ui) {
        ui.collapsing("Template manager", |ui| {
            if self.editing_widget_template.is_none() {
                self.editing_widget_template = self
                    .working
                    .widget_templates
                    .first()
                    .map(|template| template.id.clone());
            }
            egui::ComboBox::from_label("Edit template")
                .selected_text(
                    self.editing_widget_template
                        .as_deref()
                        .and_then(|id| self.working.widget_template(id))
                        .map(|template| template.name.as_str())
                        .unwrap_or("Select template"),
                )
                .show_ui(ui, |ui| {
                    for template in &self.working.widget_templates {
                        ui.selectable_value(
                            &mut self.editing_widget_template,
                            Some(template.id.clone()),
                            &template.name,
                        );
                    }
                });
            let Some(id) = self.editing_widget_template.clone() else {
                return;
            };
            let Some(index) = self
                .working
                .widget_templates
                .iter()
                .position(|template| template.id == id)
            else {
                return;
            };
            let built_in = self.working.widget_templates[index].built_in;
            if built_in {
                ui.label(
                    egui::RichText::new("Built-in templates are read-only")
                        .small()
                        .color(egui::Color32::GRAY),
                );
            } else {
                let template = &mut self.working.widget_templates[index];
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut template.name);
                });
                ui.add(
                    egui::Slider::new(&mut template.style.value_font_size, 8.0..=180.0)
                        .text("Value size"),
                );
                ui.add(
                    egui::Slider::new(&mut template.style.label_font_size, 6.0..=96.0)
                        .text("Label size"),
                );
                ui.horizontal(|ui| {
                    ui.label("Box size");
                    ui.add(
                        egui::DragValue::new(&mut template.style.width)
                            .range(1.0..=4096.0)
                            .prefix("W: ")
                            .suffix(" px"),
                    );
                    ui.add(
                        egui::DragValue::new(&mut template.style.height)
                            .range(1.0..=4096.0)
                            .prefix("H: ")
                            .suffix(" px"),
                    );
                });
                ui.add(
                    egui::DragValue::new(&mut template.style.label_offset_y)
                        .prefix("Label Y: ")
                        .suffix(" px"),
                );
                ui.horizontal(|ui| {
                    ui.label("Label color");
                    rgb_editor(ui, (&template.id, "label"), &mut template.style.label_color);
                });
                ui.collapsing("Background", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Color");
                        rgb_editor(
                            ui,
                            (&template.id, "background"),
                            &mut template.style.background_color,
                        );
                    });
                    background_transparency_slider(ui, &mut template.style.background_opacity);
                });
                ui.collapsing("Default threshold colors", |ui| {
                    color_map_editor(ui, &mut template.style.color_map, &template.id);
                });
            }
            ui.horizontal(|ui| {
                if ui.button("Duplicate as user template").clicked() {
                    let mut copy = self.working.widget_templates[index].clone();
                    copy.id = next_unique_id(
                        "template",
                        self.working
                            .widget_templates
                            .iter()
                            .map(|template| template.id.as_str()),
                    );
                    copy.name = format!("{} copy", copy.name);
                    copy.built_in = false;
                    self.editing_widget_template = Some(copy.id.clone());
                    self.working.widget_templates.push(copy);
                }
                let referenced = self
                    .working
                    .widget_instances
                    .iter()
                    .any(|instance| instance.template_id == id);
                if ui
                    .add_enabled(
                        !built_in && !referenced,
                        egui::Button::new("Delete template"),
                    )
                    .clicked()
                {
                    self.working.widget_templates.remove(index);
                    self.editing_widget_template = None;
                }
            });
        });
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
                self.preview_missing_reported = false;
            }
            if ui
                .selectable_value(&mut self.standby_tab, StandbyTab::Standby, "Standby")
                .clicked()
            {
                self.preview_mode = PreviewMode::Standby;
                self.preview_missing_reported = false;
            }
        });
        ui.add_space(8.0);

        if ui
            .checkbox(&mut self.device_preview_enabled, "Show on device")
            .changed()
        {
            self.preview_missing_reported = false;
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
                    self.boot_default_preset = Some(PendingMediaPreset {
                        path: path.clone(),
                        transform_at_selection: self.boot_transform.transform,
                    });
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
        if let Some((frame_count, scrub_time)) = self
            .boot_animation
            .as_ref()
            .filter(|(path, _)| Some(path) == self.boot_path.as_ref())
            .map(|(_, animation)| {
                (
                    animation.frame_count(),
                    animation.frame_start_seconds(self.boot_scrub_frame),
                )
            })
        {
            let last_frame = frame_count.saturating_sub(1);
            ui.separator();
            ui.label(egui::RichText::new("Edit animation").strong());
            ui.horizontal(|ui| {
                ui.label("Trim frames");
                let start_changed = ui
                    .add(
                        egui::DragValue::new(&mut self.boot_trim_start)
                            .range(0..=last_frame)
                            .prefix("Start "),
                    )
                    .changed();
                let end_changed = ui
                    .add(
                        egui::DragValue::new(&mut self.boot_trim_end)
                            .range(0..=last_frame)
                            .prefix("End "),
                    )
                    .changed();
                if start_changed && self.boot_trim_start > self.boot_trim_end {
                    self.boot_trim_end = self.boot_trim_start;
                } else if end_changed && self.boot_trim_end < self.boot_trim_start {
                    self.boot_trim_start = self.boot_trim_end;
                }
                self.boot_scrub_frame = self
                    .boot_scrub_frame
                    .clamp(self.boot_trim_start, self.boot_trim_end);
                ui.label(format!(
                    "{} selected",
                    self.boot_trim_end - self.boot_trim_start + 1
                ));
            });
            ui.horizontal(|ui| {
                ui.label("Uniform timing");
                ui.add(
                    egui::DragValue::new(&mut self.boot_frame_delay_ms)
                        .range(80..=5000)
                        .suffix(" ms/frame"),
                );
                let mut fps = 1000.0 / self.boot_frame_delay_ms as f32;
                if ui
                    .add(
                        egui::DragValue::new(&mut fps)
                            .range(0.2..=12.5)
                            .speed(0.1)
                            .suffix(" FPS"),
                    )
                    .changed()
                {
                    self.boot_frame_delay_ms = (1000.0 / fps).round().max(80.0) as u32;
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.boot_preview_playing, "Play preview");
                let scrubbed = ui
                    .add(
                        egui::Slider::new(
                            &mut self.boot_scrub_frame,
                            self.boot_trim_start..=self.boot_trim_end,
                        )
                        .text("Timeline frame"),
                    )
                    .changed();
                if scrubbed {
                    self.boot_preview_playing = false;
                }
            });
            ui.label(
                egui::RichText::new(format!(
                    "Source frame {} of {} · source time {:.3} s",
                    self.boot_scrub_frame + 1,
                    frame_count,
                    scrub_time
                ))
                .small()
                .color(egui::Color32::GRAY),
            );
        }
        let source_dimensions = self.boot_source_dimensions_all();
        if let Some(error) = Self::media_transform_controls(
            ui,
            "boot",
            &mut self.boot_transform,
            &mut self.boot_drag,
            &mut self.boot_rotation_drag,
            &mut self.boot_snap,
            &source_dimensions,
        ) {
            self.last_error = Some(error);
        }
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
            if let Some(error) = media_placement_error_many(
                &source_dimensions,
                &self.boot_transform.fit,
                &self.boot_transform.transform,
                self.boot_snap,
            ) {
                self.last_error = Some(format!("Boot upload placement is invalid: {error}"));
                return;
            }
            let path = self
                .boot_path
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mut args = vec!["--upload-boot".into(), path];
            args.extend(media_transform_args(&self.boot_transform));
            args.extend(boot_edit_args(
                self.boot_trim_start,
                self.boot_trim_end,
                self.boot_frame_delay_ms,
            ));
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
                    self.standby_path = Some(path.clone());
                    self.standby_default_preset = Some(PendingMediaPreset {
                        path,
                        transform_at_selection: self.standby_transform.transform,
                    });
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
        let source_dimensions: Vec<_> = self.standby_source_dimensions().into_iter().collect();
        if let Some(error) = Self::media_transform_controls(
            ui,
            "standby",
            &mut self.standby_transform,
            &mut self.standby_drag,
            &mut self.standby_rotation_drag,
            &mut self.standby_snap,
            &source_dimensions,
        ) {
            self.last_error = Some(error);
        }
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
            if let Some(error) = media_placement_error_many(
                &source_dimensions,
                &self.standby_transform.fit,
                &self.standby_transform.transform,
                self.standby_snap,
            ) {
                self.last_error = Some(format!("Standby upload placement is invalid: {error}"));
                return;
            }
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
        target: &str,
        media: &mut MediaTransform,
        drag: &mut Option<DragSnapState>,
        rotation_drag: &mut Option<RotationDragState>,
        snap: &mut SnapSettings,
        source_dimensions_all: &[(u32, u32)],
    ) -> Option<String> {
        let source_dimensions = source_dimensions_all.first().copied();
        let mut error = None;
        let before = media.transform;
        let covered_before = snap.keep_covered;
        let mut pan_edited = false;
        ui.separator();
        ui.label(egui::RichText::new("Transform").strong());
        ui.horizontal(|ui| {
            ui.label("Presets");
            for (label, tooltip, preset) in [
                (
                    "1:1",
                    "Display source pixels at native size",
                    TransformPreset::OneToOne,
                ),
                (
                    "Fit",
                    "Contain the whole source inside the display",
                    TransformPreset::Fit,
                ),
                (
                    "Cover",
                    "Fill the display while preserving aspect ratio",
                    TransformPreset::Cover,
                ),
                (
                    "Stretch",
                    "Fill the display without preserving aspect ratio",
                    TransformPreset::Stretch,
                ),
            ] {
                if ui
                    .add_enabled(source_dimensions.is_some(), egui::Button::new(label))
                    .on_hover_text(tooltip)
                    .clicked()
                {
                    let (width, height) = source_dimensions.expect("enabled only with dimensions");
                    if let Err(message) = apply_media_preset(media, preset, width, height) {
                        error = Some(message);
                    }
                    *drag = None;
                    *rotation_drag = None;
                }
            }
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut snap.enabled, "Snap transforms");
            ui.checkbox(&mut snap.show_grid, "Show grid");
            ui.add(
                egui::DragValue::new(&mut snap.pan_grid)
                    .range(1.0..=120.0)
                    .suffix(" px"),
            );
            ui.add(
                egui::DragValue::new(&mut snap.rotation_grid)
                    .range(1.0..=90.0)
                    .suffix(" deg"),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut snap.edge_snap, "Snap media edges (8 px)");
            ui.checkbox(&mut snap.keep_covered, "Keep viewport covered");
            if ui.small_button("Center image").clicked() {
                media.transform.pan_x = 0.0;
                media.transform.pan_y = 0.0;
                *drag = None;
                pan_edited = true;
            }
        });
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
                pan_edited = true;
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
                reset_media_transform(media);
                *drag = None;
                *rotation_drag = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Canvas color");
            rgb_editor(ui, (target, "canvas"), &mut media.canvas_color);
        });
        if !source_dimensions_all.is_empty() {
            let current = media.transform;
            let geometry_changed = current.zoom != before.zoom
                || current.stretch_x != before.stretch_x
                || current.stretch_y != before.stretch_y
                || current.rotation != before.rotation;
            if pan_edited {
                let transform = media.transform;
                if let Ok([x, y]) = constrained_media_pan_many(
                    source_dimensions_all,
                    &media.fit,
                    &transform,
                    [transform.pan_x, transform.pan_y],
                    *snap,
                    false,
                ) {
                    media.transform.pan_x = x;
                    media.transform.pan_y = y;
                }
            }
            let current = media.transform;
            let placement_error = (geometry_changed || covered_before != snap.keep_covered)
                .then(|| {
                    media_placement_error_many(source_dimensions_all, &media.fit, &current, *snap)
                })
                .flatten();
            if let Some(message) = placement_error {
                if geometry_changed {
                    media.transform.zoom = before.zoom;
                    media.transform.stretch_x = before.stretch_x;
                    media.transform.stretch_y = before.stretch_y;
                    media.transform.rotation = before.rotation;
                }
                snap.keep_covered = covered_before;
                error = Some(message);
            }
        }
        error
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
            ui.add(
                egui::Slider::new(&mut self.working.rotation, 0.0..=359.9)
                    .text("Fine rotation")
                    .suffix("°"),
            );
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
            if self.daemon_running && ui.button("Restart Live Display daemon").clicked() {
                self.service_manager.restart(&daemon_binary_path());
                self.daemon_running = self.service_manager.daemon_running();
            }
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
            }
        });
    }

    fn show_profiles(&mut self, ui: &mut egui::Ui) {
        ui.heading("Profiles");
        ui.label("Profiles store runtime display configuration; Boot and Standby uploads remain separate.");
        let names = match self.profiles.list() {
            Ok(names) => names,
            Err(error) => {
                self.last_error = Some(format!("Failed to list profiles: {error}"));
                Vec::new()
            }
        };
        egui::ComboBox::from_label("Profile")
            .selected_text(self.selected_profile.as_deref().unwrap_or("None"))
            .show_ui(ui, |ui| {
                for name in &names {
                    ui.selectable_value(&mut self.selected_profile, Some(name.clone()), name);
                }
            });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.selected_profile.is_some(), egui::Button::new("Load"))
                .clicked()
            {
                if let Some(name) = &self.selected_profile {
                    match self.profiles.load(name) {
                        Ok(config) => {
                            self.working = config;
                            self.selected_sensor = None;
                            self.sync_background_source();
                        }
                        Err(error) => {
                            self.last_error = Some(format!("Failed to load profile: {error}"))
                        }
                    }
                }
            }
            if ui
                .add_enabled(
                    self.selected_profile.is_some(),
                    egui::Button::new("Delete…"),
                )
                .clicked()
            {
                self.confirm_delete_profile = true;
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut self.profile_name_edit);
            if ui.button("Save as new").clicked() {
                let name = ProfileStore::sanitize_name(&self.profile_name_edit);
                if names.contains(&name) {
                    self.last_error = Some(format!("Profile {name} already exists"));
                } else {
                    match self.profiles.save(&name, &self.working) {
                        Ok(()) => {
                            self.selected_profile = Some(name.clone());
                            self.profile_name_edit = name;
                        }
                        Err(error) => {
                            self.last_error = Some(format!("Failed to save profile: {error}"))
                        }
                    }
                }
            }
            if ui
                .add_enabled(
                    self.selected_profile.is_some(),
                    egui::Button::new("Rename selected"),
                )
                .clicked()
            {
                if let Some(old) = self.selected_profile.clone() {
                    let name = ProfileStore::sanitize_name(&self.profile_name_edit);
                    if name != old && names.contains(&name) {
                        self.last_error = Some(format!("Profile {name} already exists"));
                    } else if name != old {
                        match self.profiles.rename(&old, &name) {
                            Ok(()) => self.selected_profile = Some(name),
                            Err(error) => {
                                self.last_error = Some(format!("Failed to rename profile: {error}"))
                            }
                        }
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Import…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("TH420 profile", &["toml"])
                    .pick_file()
                {
                    match self.profiles.import(&path) {
                        Ok((name, config)) => {
                            self.selected_profile = Some(name);
                            self.working = config;
                            self.selected_sensor = None;
                            self.sync_background_source();
                        }
                        Err(error) => {
                            self.last_error = Some(format!("Profile import failed: {error}"))
                        }
                    }
                }
            }
            if ui
                .add_enabled(
                    self.selected_profile.is_some(),
                    egui::Button::new("Export…"),
                )
                .clicked()
            {
                if let Some(name) = &self.selected_profile {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name(format!("{name}.toml"))
                        .save_file()
                    {
                        if let Err(error) = self.profiles.export(name, &path) {
                            self.last_error = Some(format!("Profile export failed: {error}"));
                        }
                    }
                }
            }
            if ui.button("Open profile directory").clicked() {
                open_local_path(self.profiles.dir());
            }
        });
        if let Some(active) = self.profiles.active_name() {
            ui.label(format!("Active profile: {active}"));
        }
        ui.label(
            egui::RichText::new("Applying while a profile is selected also updates that profile.")
                .small()
                .color(egui::Color32::GRAY),
        );
        if self.confirm_delete_profile {
            egui::Window::new("Delete profile?")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "Delete profile {}?",
                        self.selected_profile.as_deref().unwrap_or("")
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_delete_profile = false;
                        }
                        if ui.button("Delete").clicked() {
                            if let Some(name) = self.selected_profile.clone() {
                                match self.profiles.delete(&name) {
                                    Ok(()) => self.selected_profile = None,
                                    Err(error) => {
                                        self.last_error =
                                            Some(format!("Failed to delete profile: {error}"))
                                    }
                                }
                            }
                            self.confirm_delete_profile = false;
                        }
                    });
                });
        }
    }

    fn diagnostics_text(&self) -> String {
        let mut output = String::from("TH420 Display diagnostics\n\n");
        output.push_str(&format!(
            "device.connected={}\ndevice.usb_id={}:{}\n",
            self.device_info.connected, self.device_info.vid, self.device_info.pid
        ));
        output.push_str(&format!(
            "device.product={}\ndevice.revision={}\n",
            self.device_info.product.as_deref().unwrap_or("unknown"),
            self.device_info.revision.as_deref().unwrap_or("unknown")
        ));
        output.push_str(&format!(
            "device.control_hid={}\ndevice.image_hid={}\n",
            path_or_dash(self.device_info.control_hidraw.as_ref()),
            path_or_dash(self.device_info.image_hidraw.as_ref())
        ));
        output.push_str(&format!(
            "service.manager={}\nservice.daemon_running={}\nservice.daemon_paused={}\n",
            self.service_manager.kind().name(),
            self.daemon_running,
            self.daemon_paused
        ));
        output.push_str(&format!(
            "runtime.config={}\nruntime.profile={}\nruntime.unsaved_changes={}\n",
            self.config_path.display(),
            self.selected_profile.as_deref().unwrap_or("none"),
            self.is_dirty()
        ));
        if let Some(value) = self.device_coolant {
            output.push_str(&format!("device.coolant_c={value:.1}\n"));
        }
        if let Some(value) = self.device_pump_rpm {
            output.push_str(&format!("device.pump_rpm={value}\n"));
        }
        let mut readings: Vec<_> = self.sensor_values.readings.iter().collect();
        readings.sort_by_key(|(name, _)| *name);
        output.push_str("\nsensors:\n");
        for (name, value) in readings {
            output.push_str(&format!("  {name}={value:.2}\n"));
        }
        if let Some(error) = &self.last_error {
            output.push_str(&format!(
                "runtime.last_error={}\n",
                error.replace('\n', " | ")
            ));
        }
        output
    }

    fn show_diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.heading("Diagnostics");
        let text = self.diagnostics_text();
        egui::ScrollArea::vertical()
            .max_height(440.0)
            .show(ui, |ui| {
                ui.monospace(&text);
            });
        ui.horizontal(|ui| {
            if ui.button("Copy diagnostics").clicked() {
                ui.ctx().copy_text(text);
            }
            if ui.button("Open config directory").clicked() {
                if let Some(parent) = self.config_path.parent() {
                    open_local_path(parent);
                }
            }
            if ui.button("Open profile directory").clicked() {
                open_local_path(self.profiles.dir());
            }
            if ui
                .add_enabled(
                    self.device_info.connected && !self.device_status_pending,
                    egui::Button::new("Refresh device telemetry"),
                )
                .clicked()
            {
                self.start_device_status_refresh();
            }
        });
    }

    fn start_device_status_refresh(&mut self) {
        if self.device_status_pending {
            return;
        }
        self.device_status_pending = true;
        self.last_device_status_request = Instant::now();
        let tx = self.device_status_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(query_device_telemetry());
        });
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

    fn desired_device_preview_mode(&self) -> DevicePreviewMode {
        desired_device_preview_mode_for(self.device_preview_enabled, self.page, self.standby_tab)
    }

    fn device_preview_spec(&self, mode: DevicePreviewMode, path: &Path) -> DevicePreviewSpec {
        let transform = match mode {
            DevicePreviewMode::BootLoop => &self.boot_transform,
            DevicePreviewMode::Standby => &self.standby_transform,
            DevicePreviewMode::Off => unreachable!(),
        };
        build_device_preview_spec(
            mode,
            path,
            self.brightness,
            transform,
            mode.is_boot().then_some((
                self.boot_trim_start,
                self.boot_trim_end,
                self.boot_frame_delay_ms,
            )),
            matches!(mode, DevicePreviewMode::Standby).then_some(self.standby_time),
        )
    }

    fn reconcile_device_preview(&mut self) {
        let desired = self.desired_device_preview_mode();
        if desired == DevicePreviewMode::Off {
            self.pending_device_preview_spec = None;
            self.device_preview_refresh_now = false;
            self.stop_device_preview_child();
            self.resume_preview_daemon();
            return;
        }
        let path = match desired {
            DevicePreviewMode::BootLoop => self.boot_path.clone(),
            DevicePreviewMode::Standby => self.standby_path.clone(),
            DevicePreviewMode::Off => None,
        };
        let Some(path) = path else {
            self.pending_device_preview_spec = None;
            self.device_preview_refresh_now = false;
            self.stop_device_preview_child();
            if !self.preview_missing_reported {
                self.last_error = Some(format!("Select a {} file first", desired.label()));
                self.preview_missing_reported = true;
            }
            return;
        };
        self.preview_missing_reported = false;

        let sources = match desired {
            DevicePreviewMode::BootLoop => self.boot_source_dimensions_all(),
            DevicePreviewMode::Standby => self.standby_source_dimensions().into_iter().collect(),
            DevicePreviewMode::Off => Vec::new(),
        };
        if sources.is_empty() {
            self.stop_device_preview_child();
            self.resume_preview_daemon();
            return;
        }
        let (media, snap) = match desired {
            DevicePreviewMode::BootLoop => (&self.boot_transform, self.boot_snap),
            DevicePreviewMode::Standby => (&self.standby_transform, self.standby_snap),
            DevicePreviewMode::Off => unreachable!(),
        };
        if let Some(error) =
            media_placement_error_many(&sources, &media.fit, &media.transform, snap)
        {
            self.last_error = Some(format!("Device preview placement is invalid: {error}"));
            self.stop_device_preview_child();
            self.resume_preview_daemon();
            return;
        }

        let spec = self.device_preview_spec(desired, &path);
        if self
            .device_preview_job
            .as_ref()
            .is_some_and(|job| job.spec == spec)
        {
            self.pending_device_preview_spec = None;
            self.device_preview_refresh_now = false;
            return;
        }
        let same_target = self
            .device_preview_job
            .as_ref()
            .is_some_and(|job| job.spec.mode == desired && job.spec.source_path == path);
        if same_target {
            if self.pending_device_preview_spec.as_ref() != Some(&spec) {
                self.pending_device_preview_spec = Some(spec.clone());
                self.last_device_preview_spec_change = Instant::now();
            }
            if !std::mem::take(&mut self.device_preview_refresh_now)
                && self.last_device_preview_spec_change.elapsed() < DEVICE_PREVIEW_RESTART_INTERVAL
            {
                return;
            }
        } else {
            self.pending_device_preview_spec = None;
            self.device_preview_refresh_now = false;
        }
        if self.device_preview_job.is_some() {
            self.stop_device_preview_child();
        }

        if self.preview_paused_daemon.is_none() {
            if let Some(owner) = current_owner(InstanceKind::Daemon) {
                match request_daemon_command(&owner, "pause", Duration::from_secs(5)) {
                    Ok(response) if response == "paused changed" => {
                        self.preview_paused_daemon = Some(owner);
                        self.daemon_paused = true;
                    }
                    Ok(response) if response.starts_with("paused") => {
                        self.daemon_paused = true;
                    }
                    Ok(response) => {
                        self.last_error = Some(format!(
                            "Daemon returned an unexpected pause state: {response}"
                        ));
                        self.device_preview_enabled = false;
                        return;
                    }
                    Err(error) => {
                        self.last_error = Some(format!("Failed to pause live daemon: {error}"));
                        self.device_preview_enabled = false;
                        return;
                    }
                }
            }
        }

        match Command::new(daemon_binary_path())
            .args(&spec.args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.device_preview_mode = desired;
                self.pending_device_preview_spec = None;
                self.device_preview_job = Some(DevicePreviewJob { child, spec });
            }
            Err(err) => {
                self.pending_device_preview_spec = None;
                self.last_error = Some(format!("Failed to start device preview: {err}"));
                self.device_preview_enabled = false;
                self.resume_preview_daemon();
            }
        }
    }

    fn stop_device_preview(&mut self) {
        self.device_preview_enabled = false;
        self.stop_device_preview_child();
        self.resume_preview_daemon();
        self.device_preview_mode = DevicePreviewMode::Off;
    }

    fn stop_device_preview_child(&mut self) {
        let Some(mut job) = self.device_preview_job.take() else {
            self.device_preview_mode = DevicePreviewMode::Off;
            return;
        };
        let _ = job.child.kill();
        let _ = job.child.wait();
        self.device_preview_mode = DevicePreviewMode::Off;
    }

    fn resume_preview_daemon(&mut self) {
        let Some(owner) = self.preview_paused_daemon.take() else {
            return;
        };
        match request_daemon_command(&owner, "resume", Duration::from_secs(5)) {
            Ok(response) if response.starts_with("running") => {
                self.daemon_running = true;
                self.daemon_paused = false;
            }
            Ok(response) => {
                self.last_error = Some(format!(
                    "Daemon returned an unexpected resume state: {response}"
                ));
            }
            Err(error) => {
                self.last_error = Some(format!("Failed to resume live daemon: {error}"));
            }
        }
    }
}

fn desired_device_preview_mode_for(
    enabled: bool,
    page: Page,
    standby_tab: StandbyTab,
) -> DevicePreviewMode {
    if !enabled || page != Page::StandbySettings {
        return DevicePreviewMode::Off;
    }
    match standby_tab {
        StandbyTab::Boot => DevicePreviewMode::BootLoop,
        StandbyTab::Standby => DevicePreviewMode::Standby,
    }
}

fn build_device_preview_spec(
    mode: DevicePreviewMode,
    path: &Path,
    brightness: u8,
    transform: &MediaTransform,
    boot_edit: Option<(usize, usize, u32)>,
    standby_time: Option<f64>,
) -> DevicePreviewSpec {
    let mut args = if mode.is_boot() && is_gif(path) {
        vec![
            "--play-live-gif".to_string(),
            path.to_string_lossy().into_owned(),
            "--live-loops".to_string(),
            mode.loops().to_string(),
        ]
    } else {
        vec![
            "--play-live-frames".to_string(),
            path.to_string_lossy().into_owned(),
            "--live-fps".to_string(),
            "1".to_string(),
            "--live-loops".to_string(),
            mode.loops().to_string(),
        ]
    };
    args.extend(["--live-brightness".to_string(), brightness.to_string()]);
    args.extend(media_transform_args(transform));
    if let Some((start, end, delay)) = boot_edit {
        args.extend(boot_edit_args(start, end, delay));
    }
    if let Some(time) = standby_time {
        args.extend(["--media-time".into(), time.to_string()]);
    }
    DevicePreviewSpec {
        mode,
        source_path: path.to_path_buf(),
        args,
    }
}

fn apply_background_preset_from_source(
    background: &mut config::BackgroundConfig,
    preset: TransformPreset,
) -> Result<(), String> {
    let path = background
        .image_path
        .as_deref()
        .ok_or_else(|| "Select a background source before applying a preset".to_string())?;
    let source = load_media_frame_at(Path::new(path), 0.0)?;
    apply_background_preset(background, preset, source.width(), source.height())
}

fn apply_background_preset(
    background: &mut config::BackgroundConfig,
    preset: TransformPreset,
    source_width: u32,
    source_height: u32,
) -> Result<(), String> {
    let transform = preset_transform(preset, source_width, source_height)?;
    background.fit = ImageFit::Native;
    background.zoom = transform.zoom;
    background.stretch_x = transform.stretch_x;
    background.stretch_y = transform.stretch_y;
    background.pan_x = transform.pan_x;
    background.pan_y = transform.pan_y;
    background.rotation = transform.rotation;
    Ok(())
}

fn apply_media_preset(
    media: &mut MediaTransform,
    preset: TransformPreset,
    source_width: u32,
    source_height: u32,
) -> Result<(), String> {
    media.fit = ImageFit::Native;
    media.transform = preset_transform(preset, source_width, source_height)?;
    Ok(())
}

fn reset_media_transform(media: &mut MediaTransform) {
    media.fit = ImageFit::Native;
    media.transform = Transform2D::default();
}

fn preset_transform(
    preset: TransformPreset,
    source_width: u32,
    source_height: u32,
) -> Result<Transform2D, String> {
    if source_width == 0 || source_height == 0 {
        return Err("Media source has invalid dimensions".into());
    }
    let fit_zoom = (480.0 / source_width as f32)
        .min(480.0 / source_height as f32)
        .clamp(0.05, 8.0);
    let cover_zoom = (480.0 / source_width as f32)
        .max(480.0 / source_height as f32)
        .clamp(0.05, 8.0);
    let (zoom, stretch_x, stretch_y) = match preset {
        TransformPreset::OneToOne => (1.0, 1.0, 1.0),
        TransformPreset::Fit => (fit_zoom, 1.0, 1.0),
        TransformPreset::Cover => (cover_zoom, 1.0, 1.0),
        TransformPreset::Stretch => (
            1.0,
            (480.0 / source_width as f32).clamp(0.05, 8.0),
            (480.0 / source_height as f32).clamp(0.05, 8.0),
        ),
    };

    Ok(Transform2D {
        zoom,
        stretch_x,
        stretch_y,
        ..Transform2D::default()
    })
}

fn apply_pending_cover(
    selected_path: Option<&Path>,
    loaded_path: &Path,
    media: &mut MediaTransform,
    pending: &mut Option<PendingMediaPreset>,
    source_width: u32,
    source_height: u32,
) -> Result<bool, String> {
    if selected_path != Some(loaded_path)
        || pending
            .as_ref()
            .is_none_or(|request| request.path != loaded_path)
    {
        return Ok(false);
    }
    let request = pending.take().expect("pending request checked above");
    if media.transform != request.transform_at_selection {
        return Ok(false);
    }
    apply_media_preset(media, TransformPreset::Cover, source_width, source_height)?;
    Ok(true)
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
        self.handle_keyboard(ctx);
        let before = self.working.clone();
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
                    Page::Profiles => self.show_profiles(ui),
                    Page::Diagnostics => self.show_diagnostics(ui),
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
        self.capture_history(before, ctx);
        self.reconcile_device_preview();
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

#[cfg(test)]
fn snap(value: f32, grid: f32) -> f32 {
    if grid <= 0.0 {
        value
    } else {
        (value / grid).round() * grid
    }
}

fn query_device_telemetry() -> Result<DeviceTelemetry, String> {
    if let Some(owner) = current_owner(InstanceKind::Daemon) {
        let response = request_daemon_command(&owner, "telemetry", Duration::from_secs(1))?;
        return parse_device_telemetry(&response);
    }

    let output = Command::new(daemon_binary_path())
        .arg("--status")
        .output()
        .map_err(|error| format!("Failed to read device status: {error}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if error.is_empty() {
            format!("Device status command exited with {}", output.status)
        } else {
            error
        });
    }
    parse_device_telemetry(&String::from_utf8_lossy(&output.stdout))
}

fn parse_device_telemetry(text: &str) -> Result<DeviceTelemetry, String> {
    let mut coolant_temp_c = None;
    let mut pump_rpm = None;
    let mut age_ms = 0;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("coolant_temp_c=") {
            coolant_temp_c = value.trim().parse::<f32>().ok();
        } else if let Some(value) = line.strip_prefix("pump_rpm=") {
            pump_rpm = value.trim().parse::<u16>().ok();
        } else if let Some(value) = line.strip_prefix("age_ms=") {
            age_ms = value.trim().parse::<u128>().unwrap_or(0);
        }
    }
    let coolant_temp_c = coolant_temp_c
        .filter(|value| value.is_finite())
        .ok_or_else(|| "Device status did not contain a valid coolant temperature".to_string())?;
    let pump_rpm =
        pump_rpm.ok_or_else(|| "Device status did not contain a valid pump RPM".to_string())?;
    Ok(DeviceTelemetry {
        coolant_temp_c,
        pump_rpm,
        age_ms,
    })
}

fn paint_centered_grid(ui: &egui::Ui, rect: egui::Rect, grid: f32) {
    for coordinate in centered_grid_coordinates(grid) {
        let offset = coordinate / 480.0 * rect.width();
        let x = rect.left() + offset;
        let y = rect.top() + offset;
        let center = (coordinate - 240.0).abs() < f32::EPSILON;
        let stroke = if center {
            egui::Stroke::new(1.25_f32, egui::Color32::from_white_alpha(85))
        } else {
            egui::Stroke::new(0.5_f32, egui::Color32::from_white_alpha(32))
        };
        ui.painter().line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            stroke,
        );
        ui.painter().line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            stroke,
        );
    }
}

fn constrained_media_pan(
    source: Option<(u32, u32)>,
    fit: &ImageFit,
    transform: &Transform2D,
    raw: [f32; 2],
    settings: SnapSettings,
    apply_snap: bool,
) -> Result<[f32; 2], String> {
    constrained_media_pan_many(
        &source.into_iter().collect::<Vec<_>>(),
        fit,
        transform,
        raw,
        settings,
        apply_snap,
    )
}

fn media_pan_region_for_sources(
    sources: &[(u32, u32)],
    fit: &ImageFit,
    transform: &Transform2D,
    keep_covered: bool,
) -> Result<Option<(MediaFootprint, PanRegion)>, String> {
    let mut result: Option<(MediaFootprint, PanRegion)> = None;
    for &source in sources {
        let Some(footprint) = MediaFootprint::new(source, fit, transform) else {
            continue;
        };
        let region = footprint.pan_region(keep_covered).map_err(str::to_owned)?;
        result = Some(match result {
            None => (footprint, region),
            Some((first, accumulated)) => {
                let intersection = accumulated
                    .intersect(&region)
                    .ok_or_else(|| "No placement satisfies every animation frame".to_string())?;
                (first, intersection)
            }
        });
    }
    Ok(result)
}

fn constrained_media_pan_many(
    sources: &[(u32, u32)],
    fit: &ImageFit,
    transform: &Transform2D,
    raw: [f32; 2],
    settings: SnapSettings,
    apply_snap: bool,
) -> Result<[f32; 2], String> {
    let Some((footprint, region)) =
        media_pan_region_for_sources(sources, fit, transform, settings.keep_covered)?
    else {
        return Ok(raw);
    };
    let mut candidate = raw;
    if apply_snap && settings.enabled {
        let edge = if settings.edge_snap {
            footprint.edge_snap(raw, 8.0)
        } else {
            [None, None]
        };
        for axis in 0..2 {
            candidate[axis] =
                edge[axis].unwrap_or_else(|| renderer::snap_value(raw[axis], settings.pan_grid));
        }
    }
    Ok(region.clamp(candidate))
}

fn media_placement_error(
    source: Option<(u32, u32)>,
    fit: &ImageFit,
    transform: &Transform2D,
    settings: SnapSettings,
) -> Option<String> {
    media_placement_error_many(
        &source.into_iter().collect::<Vec<_>>(),
        fit,
        transform,
        settings,
    )
}

fn media_placement_error_many(
    sources: &[(u32, u32)],
    fit: &ImageFit,
    transform: &Transform2D,
    settings: SnapSettings,
) -> Option<String> {
    let region = match media_pan_region_for_sources(sources, fit, transform, settings.keep_covered)
    {
        Ok(Some((_, region))) => region,
        Ok(None) => return None,
        Err(error) => return Some(error),
    };
    let pan = [transform.pan_x, transform.pan_y];
    let legal = region.clamp(pan);
    ((legal[0] - pan[0]).abs() > 0.01 || (legal[1] - pan[1]).abs() > 0.01).then(|| {
        "Current pan would be outside the media bounds; move or center the image first".to_string()
    })
}

fn next_unique_id<'a>(base: &str, ids: impl Iterator<Item = &'a str>) -> String {
    let existing: std::collections::HashSet<&str> = ids.collect();
    let stem: String = base
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    for suffix in 1.. {
        let candidate = format!("{stem}-{suffix}");
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

fn open_local_path(path: &Path) {
    let _ = Command::new("xdg-open").arg(path).spawn();
}

fn override_text_row(ui: &mut egui::Ui, label: &str, resolved: &str, value: &mut Option<String>) {
    ui.horizontal(|ui| {
        ui.label(label);
        if let Some(local) = value {
            ui.text_edit_singleline(local);
            if ui.small_button("Use template").clicked() {
                *value = None;
            }
        } else {
            ui.label(egui::RichText::new(resolved).color(egui::Color32::GRAY));
            if ui.small_button("Override").clicked() {
                *value = Some(resolved.to_string());
            }
        }
    });
}

fn override_f32_row(
    ui: &mut egui::Ui,
    label: &str,
    resolved: f32,
    value: &mut Option<f32>,
    range: std::ops::RangeInclusive<f32>,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        if let Some(local) = value {
            ui.add(egui::Slider::new(local, range));
            if ui.small_button("Use template").clicked() {
                *value = None;
            }
        } else {
            ui.label(
                egui::RichText::new(format!("{resolved:.1} (template)")).color(egui::Color32::GRAY),
            );
            if ui.small_button("Override").clicked() {
                *value = Some(resolved);
            }
        }
    });
}

fn override_rgb_row(
    ui: &mut egui::Ui,
    label: &str,
    resolved: [u8; 3],
    value: &mut Option<[u8; 3]>,
    instance_id: &str,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut color = value.unwrap_or(resolved);
        if rgb_editor(ui, (instance_id, label), &mut color) {
            *value = Some(color);
        }
        if value.is_some() {
            if ui.small_button("Use template").clicked() {
                *value = None;
            }
        } else {
            ui.label(
                egui::RichText::new("Template")
                    .small()
                    .color(egui::Color32::GRAY),
            );
        }
    });
}

fn parse_rgb_hex(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ])
}

#[derive(Clone)]
struct ColorDraft {
    source: [u8; 3],
    text: String,
    dirty: bool,
}

fn rgb_editor(ui: &mut egui::Ui, key: impl std::hash::Hash, color: &mut [u8; 3]) -> bool {
    let id = ui.id().with(key);
    let mut state = ui
        .ctx()
        .data_mut(|data| data.get_temp::<ColorDraft>(id))
        .unwrap_or_else(|| ColorDraft {
            source: *color,
            text: format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2]),
            dirty: false,
        });
    if !state.dirty && state.source != *color {
        state.source = *color;
        state.text = format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2]);
    }
    let mut changed = false;
    let mut picker = color.map(|channel| channel as f32 / 255.0);
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 28.0;
        if egui::color_picker::color_edit_button_rgb(ui, &mut picker).changed() {
            *color = picker.map(|channel| (channel * 255.0).round().clamp(0.0, 255.0) as u8);
            state.source = *color;
            state.text = format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2]);
            state.dirty = false;
            changed = true;
        }
        let response = ui.add(egui::TextEdit::singleline(&mut state.text).desired_width(78.0));
        state.dirty |= response.changed();
        if response.lost_focus()
            || (response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)))
        {
            if let Some(parsed) = parse_rgb_hex(&state.text) {
                changed |= parsed != *color;
                *color = parsed;
                state.source = parsed;
                state.text = format!("#{:02X}{:02X}{:02X}", color[0], color[1], color[2]);
                state.dirty = false;
            }
        }
        if state.dirty && parse_rgb_hex(&state.text).is_none() {
            response.on_hover_text("Enter a color as #RRGGBB");
            ui.label(
                egui::RichText::new("Invalid hex")
                    .small()
                    .color(egui::Color32::LIGHT_RED),
            );
        }
    });
    ui.ctx().data_mut(|data| data.insert_temp(id, state));
    changed
}

fn background_transparency_slider(ui: &mut egui::Ui, opacity: &mut u8) {
    let mut transparency = 100 - (u16::from(*opacity) * 100 / 255) as u8;
    if ui
        .add(egui::Slider::new(&mut transparency, 0..=100).text("Transparency"))
        .changed()
    {
        *opacity = (((100 - transparency) as u16 * 255 + 50) / 100) as u8;
    }
}

fn override_background_transparency_row(
    ui: &mut egui::Ui,
    inherited_opacity: u8,
    override_opacity: &mut Option<u8>,
) {
    ui.horizontal(|ui| {
        if let Some(opacity) = override_opacity {
            background_transparency_slider(ui, opacity);
            if ui.small_button("Use template").clicked() {
                *override_opacity = None;
            }
        } else {
            let transparency = 100 - (u16::from(inherited_opacity) * 100 / 255) as u8;
            ui.label(format!("Transparency: {transparency}% (template)"));
            if ui.small_button("Override").clicked() {
                *override_opacity = Some(inherited_opacity);
            }
        }
    });
}

fn color_map_editor(ui: &mut egui::Ui, map: &mut Vec<config::ColorPoint>, owner_id: &str) {
    let ids_key = ui.id().with((owner_id, "threshold-row-ids"));
    let mut row_ids = ui
        .ctx()
        .data_mut(|data| data.get_temp::<Vec<u64>>(ids_key))
        .unwrap_or_default();
    row_ids.truncate(map.len());
    let mut next_id = row_ids.iter().copied().max().unwrap_or(0) + 1;
    while row_ids.len() < map.len() {
        row_ids.push(next_id);
        next_id += 1;
    }
    let mut remove = None;
    for (index, point) in map.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut point.value).speed(0.5));
            rgb_editor(
                ui,
                (owner_id, "threshold", row_ids[index]),
                &mut point.color,
            );
            if ui.small_button("Remove").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        map.remove(index);
        row_ids.remove(index);
    }
    if ui.small_button("Add threshold").clicked() {
        map.push(config::ColorPoint {
            value: map.last().map_or(0.0, |point| point.value + 10.0),
            color: [255, 255, 255],
        });
        row_ids.push(next_id);
    }
    let mut rows: Vec<_> = std::mem::take(map).into_iter().zip(row_ids).collect();
    rows.sort_by(|left, right| left.0.value.total_cmp(&right.0.value));
    let (points, row_ids): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    *map = points;
    ui.ctx().data_mut(|data| data.insert_temp(ids_key, row_ids));
}

#[cfg(test)]
mod transform_drag_tests {
    use super::*;

    #[test]
    fn rgb_hex_requires_complete_hash_prefixed_color() {
        assert_eq!(parse_rgb_hex("#aBcD09"), Some([0xab, 0xcd, 0x09]));
        for invalid in ["abcd09", "#abcd", "#abcd0x", "#abcd0900"] {
            assert_eq!(parse_rgb_hex(invalid), None);
        }
    }

    #[test]
    fn media_edge_snapping_precedes_grid_and_clamps() {
        let transform = Transform2D::default();
        let snap = SnapSettings {
            pan_grid: 10.0,
            ..SnapSettings::default()
        };
        assert_eq!(
            constrained_media_pan(
                Some((480, 480)),
                &ImageFit::Native,
                &transform,
                [474.0, 0.0],
                snap,
                true
            )
            .unwrap(),
            [480.0, 0.0]
        );
        assert_eq!(
            constrained_media_pan(
                Some((480, 480)),
                &ImageFit::Native,
                &transform,
                [600.0, 0.0],
                snap,
                true
            )
            .unwrap(),
            [480.0, 0.0]
        );
        let strict = SnapSettings {
            keep_covered: true,
            ..snap
        };
        assert_eq!(
            constrained_media_pan(
                Some((480, 480)),
                &ImageFit::Native,
                &transform,
                [10.0, 0.0],
                strict,
                false
            )
            .unwrap(),
            [0.0, 0.0]
        );
        assert_eq!(
            constrained_media_pan_many(
                &[(480, 480), (300, 300)],
                &ImageFit::Native,
                &transform,
                [500.0, 0.0],
                snap,
                false,
            )
            .unwrap(),
            [390.0, 0.0]
        );
    }

    #[test]
    fn parses_daemon_and_direct_device_telemetry() {
        let telemetry =
            parse_device_telemetry("coolant_temp_c=28.5\npump_rpm=2320\nage_ms=17\n").unwrap();
        assert_eq!(telemetry.coolant_temp_c, 28.5);
        assert_eq!(telemetry.pump_rpm, 2320);
        assert_eq!(telemetry.age_ms, 17);

        assert!(parse_device_telemetry("pump_rpm=2320\n").is_err());
    }

    #[test]
    fn per_frame_pointer_deltas_accumulate_across_drag() {
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
    fn background_presets_use_visible_transform_values() {
        let mut background = config::BackgroundConfig {
            zoom: 2.5,
            stretch_x: 0.75,
            stretch_y: 1.5,
            pan_x: -42.0,
            pan_y: 19.0,
            rotation: -30.0,
            ..Default::default()
        };

        apply_background_preset(&mut background, TransformPreset::OneToOne, 1600, 900).unwrap();
        assert_eq!(background.fit, ImageFit::Native);
        assert_eq!(background.zoom, 1.0);
        assert_eq!(background.stretch_x, 1.0);
        assert_eq!(background.stretch_y, 1.0);

        apply_background_preset(&mut background, TransformPreset::Fit, 1600, 900).unwrap();
        assert_eq!(background.fit, ImageFit::Native);
        assert!((background.zoom - 0.3).abs() < 0.001);
        assert_eq!(background.stretch_x, 1.0);
        assert_eq!(background.stretch_y, 1.0);

        apply_background_preset(&mut background, TransformPreset::Cover, 1600, 900).unwrap();
        assert_eq!(background.fit, ImageFit::Native);
        assert!((background.zoom - 480.0 / 900.0).abs() < 0.001);
        assert_eq!(background.stretch_x, 1.0);
        assert_eq!(background.stretch_y, 1.0);

        apply_background_preset(&mut background, TransformPreset::Stretch, 1600, 900).unwrap();
        assert_eq!(background.fit, ImageFit::Native);
        assert_eq!(background.zoom, 1.0);
        assert!((background.stretch_x - 0.3).abs() < 0.001);
        assert!((background.stretch_y - 480.0 / 900.0).abs() < 0.001);

        apply_background_preset(&mut background, TransformPreset::Stretch, 900, 1600).unwrap();
        assert!((background.stretch_x - 480.0 / 900.0).abs() < 0.001);
        assert!((background.stretch_y - 0.3).abs() < 0.001);
        assert_eq!(background.pan_x, 0.0);
        assert_eq!(background.pan_y, 0.0);
        assert_eq!(background.rotation, 0.0);

        apply_background_preset(&mut background, TransformPreset::OneToOne, 240, 240).unwrap();
        assert_eq!(background.zoom, 1.0);
    }

    #[test]
    fn media_and_background_presets_share_visible_value_math() {
        for preset in [
            TransformPreset::OneToOne,
            TransformPreset::Fit,
            TransformPreset::Cover,
            TransformPreset::Stretch,
        ] {
            let mut background = config::BackgroundConfig::default();
            let mut media = MediaTransform::default();

            apply_background_preset(&mut background, preset, 1600, 900).unwrap();
            apply_media_preset(&mut media, preset, 1600, 900).unwrap();

            assert_eq!(background.fit, ImageFit::Native);
            assert_eq!(media.fit, ImageFit::Native);
            assert_eq!(background.transform(), media.transform);
        }
    }

    #[test]
    fn pending_cover_applies_only_to_the_current_untouched_selection() {
        let selected = PathBuf::from("selected.gif");
        let stale = PathBuf::from("stale.gif");
        let mut media = MediaTransform::default();
        let original = media.transform;
        let mut pending = Some(PendingMediaPreset {
            path: selected.clone(),
            transform_at_selection: original,
        });

        assert!(
            !apply_pending_cover(Some(&selected), &stale, &mut media, &mut pending, 1600, 900,)
                .unwrap()
        );
        assert!(pending.is_some());

        media.transform.zoom = 2.0;
        assert!(!apply_pending_cover(
            Some(&selected),
            &selected,
            &mut media,
            &mut pending,
            1600,
            900,
        )
        .unwrap());
        assert_eq!(media.transform.zoom, 2.0);
        assert!(pending.is_none());

        media.transform = original;
        pending = Some(PendingMediaPreset {
            path: selected.clone(),
            transform_at_selection: original,
        });
        assert!(apply_pending_cover(
            Some(&selected),
            &selected,
            &mut media,
            &mut pending,
            1600,
            900,
        )
        .unwrap());
        assert_eq!(media.fit, ImageFit::Native);
        assert!((media.transform.zoom - 480.0 / 900.0).abs() < 0.001);
    }

    #[test]
    fn reset_uses_one_to_one_without_changing_canvas() {
        let mut media = MediaTransform {
            fit: ImageFit::Cover,
            transform: Transform2D {
                pan_x: 13.0,
                pan_y: -7.0,
                zoom: 2.0,
                stretch_x: 0.5,
                stretch_y: 1.5,
                rotation: 45.0,
            },
            canvas_color: [4, 5, 6],
        };

        reset_media_transform(&mut media);

        assert_eq!(media.fit, ImageFit::Native);
        assert_eq!(media.transform, Transform2D::default());
        assert_eq!(media.canvas_color, [4, 5, 6]);
    }

    #[test]
    fn target_snap_settings_and_committed_values_are_independent() {
        let live = SnapSettings::default();
        let mut boot = SnapSettings::default();
        let mut standby = SnapSettings::default();
        boot.pan_grid = 12.0;
        boot.rotation_grid = 30.0;
        standby.enabled = false;

        assert_eq!(live.pan_grid, 8.0);
        assert_eq!(live.rotation_grid, 15.0);
        assert_eq!(boot.pan_grid, 12.0);
        assert!(!standby.enabled);

        let mut drag = DragSnapState::new([0.0, 0.0]);
        drag.update([10.0, 10.0], boot.enabled, boot.pan_grid);
        let committed = drag.finish(boot.enabled, boot.pan_grid);
        boot.pan_grid = 20.0;
        assert_eq!(committed, [12.0, 12.0]);
        assert_ne!(
            committed,
            [
                snap(committed[0], boot.pan_grid),
                snap(committed[1], boot.pan_grid)
            ]
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

    #[test]
    fn boot_edit_arguments_are_shared_with_helper_commands() {
        assert_eq!(
            boot_edit_args(2, 7, 125),
            [
                "--boot-start-frame",
                "2",
                "--boot-end-frame",
                "7",
                "--boot-frame-delay-ms",
                "125",
            ]
        );
    }
}

#[cfg(test)]
mod device_preview_mode_tests {
    use super::{
        build_device_preview_spec, desired_device_preview_mode_for, DevicePreviewMode,
        MediaTransform, Page, StandbyTab,
    };
    use std::path::Path;

    #[test]
    fn gui_uses_looping_boot_and_standby_modes() {
        assert_eq!(DevicePreviewMode::BootLoop.label(), "Boot (loop)");
        assert_eq!(DevicePreviewMode::BootLoop.loops(), 0);
        assert_eq!(DevicePreviewMode::Standby.label(), "Standby");
        assert_eq!(DevicePreviewMode::Standby.loops(), 0);
        assert!(DevicePreviewMode::BootLoop.is_boot());
        assert!(!DevicePreviewMode::Standby.is_boot());
    }

    #[test]
    fn enabled_preview_is_derived_from_page_and_standby_tab() {
        assert_eq!(
            desired_device_preview_mode_for(true, Page::StandbySettings, StandbyTab::Boot),
            DevicePreviewMode::BootLoop
        );
        assert_eq!(
            desired_device_preview_mode_for(true, Page::StandbySettings, StandbyTab::Standby),
            DevicePreviewMode::Standby
        );
        for page in [
            Page::Overview,
            Page::LiveDisplay,
            Page::Profiles,
            Page::Diagnostics,
            Page::Settings,
        ] {
            assert_eq!(
                desired_device_preview_mode_for(true, page, StandbyTab::Boot),
                DevicePreviewMode::Off
            );
        }
        assert_eq!(
            desired_device_preview_mode_for(false, Page::StandbySettings, StandbyTab::Boot),
            DevicePreviewMode::Off
        );
    }

    #[test]
    fn preview_spec_tracks_every_boot_helper_input() {
        let path = Path::new("boot.gif");
        let transform = MediaTransform::default();
        let base = build_device_preview_spec(
            DevicePreviewMode::BootLoop,
            path,
            80,
            &transform,
            Some((0, 5, 100)),
            None,
        );

        let mut moved = transform.clone();
        moved.transform.pan_x = 8.0;
        assert_ne!(
            base,
            build_device_preview_spec(
                DevicePreviewMode::BootLoop,
                path,
                80,
                &moved,
                Some((0, 5, 100)),
                None,
            )
        );
        assert_ne!(
            base,
            build_device_preview_spec(
                DevicePreviewMode::BootLoop,
                path,
                70,
                &transform,
                Some((0, 5, 100)),
                None,
            )
        );
        assert_ne!(
            base,
            build_device_preview_spec(
                DevicePreviewMode::BootLoop,
                path,
                80,
                &transform,
                Some((1, 5, 125)),
                None,
            )
        );
    }

    #[test]
    fn preview_spec_tracks_standby_path_transform_and_time() {
        let transform = MediaTransform::default();
        let base = build_device_preview_spec(
            DevicePreviewMode::Standby,
            Path::new("standby.mp4"),
            80,
            &transform,
            None,
            Some(1.0),
        );
        let mut rotated = transform.clone();
        rotated.transform.rotation = 15.0;

        assert_ne!(
            base,
            build_device_preview_spec(
                DevicePreviewMode::Standby,
                Path::new("other.mp4"),
                80,
                &transform,
                None,
                Some(1.0),
            )
        );
        assert_ne!(
            base,
            build_device_preview_spec(
                DevicePreviewMode::Standby,
                Path::new("standby.mp4"),
                80,
                &rotated,
                None,
                Some(1.0),
            )
        );
        assert_ne!(
            base,
            build_device_preview_spec(
                DevicePreviewMode::Standby,
                Path::new("standby.mp4"),
                80,
                &transform,
                None,
                Some(2.0),
            )
        );
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
        ImageFit::Native => "native",
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

fn boot_edit_args(start_frame: usize, end_frame: usize, frame_delay_ms: u32) -> Vec<String> {
    vec![
        "--boot-start-frame".into(),
        start_frame.to_string(),
        "--boot-end-frame".into(),
        end_frame.to_string(),
        "--boot-frame-delay-ms".into(),
        frame_delay_ms.to_string(),
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
