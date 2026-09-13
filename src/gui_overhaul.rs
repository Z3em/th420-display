mod config;
mod profile;
mod renderer;
mod sensors;
mod service_manager;

use config::{
    default_config_path, from_f32, interpolate_color, to_f32, ColorPoint, Config, ImageFit,
    LayoutPreset, LayoutSlot,
};
use eframe::egui;
use image::codecs::gif::GifDecoder;
use image::AnimationDecoder;
use profile::ProfileStore;
use renderer::{format_value, Renderer};
use sensors::SensorValues;
use service_manager::ServiceManager;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Device,
    Display,
    Sensors,
    Media,
    Profiles,
    Diagnostics,
}

impl Page {
    const ALL: [(Page, &'static str); 6] = [
        (Page::Device, "Device"),
        (Page::Display, "Display"),
        (Page::Sensors, "Sensors"),
        (Page::Media, "Media"),
        (Page::Profiles, "Profiles"),
        (Page::Diagnostics, "Diagnostics"),
    ];
}

#[derive(Clone, Debug, Default)]
struct DeviceInfo {
    connected: bool,
    vid: String,
    pid: String,
    product: Option<String>,
    manufacturer: Option<String>,
    serial: Option<String>,
    revision: Option<String>,
    control_hidraw: Option<PathBuf>,
    image_hidraw: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct GifInfo {
    frames: usize,
    uniform_delay_ms: Option<u32>,
    min_delay_ms: u32,
    max_delay_ms: u32,
    file_size: u64,
}

#[derive(Debug)]
struct JobResult {
    success: bool,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
enum JobMessage {
    Started(u32),
    Finished(JobResult),
}

struct CliJob {
    label: String,
    rx: Receiver<JobMessage>,
    pid: Option<u32>,
    restore_daemon: bool,
    cancel_requested: bool,
}

struct App {
    page: Page,
    committed: Config,
    working: Config,
    config_path: PathBuf,
    sensors: sensors::SensorReader,
    sensor_values: SensorValues,
    last_sensor_update: Instant,
    renderer: Renderer,
    preview_texture: Option<egui::TextureHandle>,
    preview_config: Option<Config>,
    last_preview: Instant,
    selected_sensor: Option<String>,

    service_manager: ServiceManager,
    daemon_running: bool,
    autostart_enabled: bool,
    last_daemon_check: Instant,
    last_autostart_check: Instant,

    device_info: DeviceInfo,
    last_device_scan: Instant,
    device_coolant: Option<f32>,
    device_pump_rpm: Option<u16>,

    undo: Vec<Config>,
    redo: Vec<Config>,
    pending_undo: Option<Config>,
    suppress_history_once: bool,
    confirm_reset_all: bool,

    profiles: ProfileStore,
    profile_names: Vec<String>,
    selected_profile: Option<String>,
    profile_name_edit: String,
    confirm_delete_profile: Option<String>,

    brightness: u8,
    pump_color: [f32; 3],
    standby_path: Option<PathBuf>,
    boot_path: Option<PathBuf>,
    live_gif_path: Option<PathBuf>,
    live_frame_paths: Vec<PathBuf>,
    boot_info: Option<Result<GifInfo, String>>,
    live_info: Option<Result<GifInfo, String>>,
    live_fps: u8,
    live_loops: usize,
    live_forever: bool,
    media_job: Option<CliJob>,
    media_status: String,
    last_error: Option<String>,
}

fn main() -> eframe::Result<()> {
    let icon = load_icon();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1120.0, 760.0])
        .with_min_inner_size([900.0, 620.0])
        .with_resizable(true)
        .with_title("TH420 Display");
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "TH420 Display",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

fn load_icon() -> Option<egui::IconData> {
    let image = image::load_from_memory(include_bytes!("../assets/th420-config.png"))
        .ok()?
        .into_rgba8();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width: 128,
        height: 128,
    })
}

impl App {
    fn new() -> Self {
        let config_path = default_config_path();
        let config = Config::load(&config_path).unwrap_or_else(|_| {
            let cfg = Config::default();
            let _ = cfg.save(&config_path);
            cfg
        });
        let profiles = ProfileStore::new();
        let profile_names = profiles.list().unwrap_or_default();
        let selected_profile = profiles
            .active_name()
            .filter(|name| profile_names.contains(name));
        let service_manager = ServiceManager::detect();
        let mut app = Self {
            page: Page::Device,
            committed: config.clone(),
            working: config,
            config_path,
            sensors: sensors::SensorReader::new(),
            sensor_values: SensorValues {
                readings: HashMap::new(),
            },
            last_sensor_update: Instant::now() - Duration::from_secs(10),
            renderer: Renderer::new(),
            preview_texture: None,
            preview_config: None,
            last_preview: Instant::now() - Duration::from_secs(10),
            selected_sensor: None,
            daemon_running: service_manager.daemon_running(),
            autostart_enabled: service_manager.autostart_enabled(),
            service_manager,
            last_daemon_check: Instant::now(),
            last_autostart_check: Instant::now(),
            device_info: detect_device_info(),
            last_device_scan: Instant::now(),
            device_coolant: None,
            device_pump_rpm: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pending_undo: None,
            suppress_history_once: false,
            confirm_reset_all: false,
            profiles,
            profile_names,
            selected_profile,
            profile_name_edit: String::new(),
            confirm_delete_profile: None,
            brightness: 80,
            pump_color: [1.0, 1.0, 1.0],
            standby_path: None,
            boot_path: None,
            live_gif_path: None,
            live_frame_paths: Vec::new(),
            boot_info: None,
            live_info: None,
            live_fps: 24,
            live_loops: 1,
            live_forever: false,
            media_job: None,
            media_status: "Idle".to_string(),
            last_error: None,
        };
        app.refresh_sensors();
        app
    }

    fn refresh_sensors(&mut self) {
        let mut values = self.sensors.read();
        if let Some(temp) = self.device_coolant {
            values.readings.insert("coolant".to_string(), temp);
        }
        self.sensor_values = values;
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        let img = self
            .renderer
            .render_preview(&self.working, &self.sensor_values);
        let pixels = img
            .pixels()
            .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
            .collect();
        self.preview_texture = Some(ctx.load_texture(
            "display-preview",
            egui::ColorImage {
                size: [img.width() as usize, img.height() as usize],
                pixels,
            },
            egui::TextureOptions::LINEAR,
        ));
        self.preview_config = Some(self.working.clone());
        self.last_preview = Instant::now();
    }

    fn is_dirty(&self) -> bool {
        self.working != self.committed
    }

    fn apply(&mut self) {
        match self.working.save(&self.config_path) {
            Ok(()) => {
                self.committed = self.working.clone();
                self.undo.clear();
                self.redo.clear();
                if let Some(name) = self.selected_profile.clone() {
                    if let Err(err) = self
                        .profiles
                        .save(&name, &self.working)
                        .and_then(|_| self.profiles.set_active(&name))
                    {
                        self.last_error =
                            Some(format!("Config applied, but profile update failed: {err}"));
                    }
                }
                self.media_status = "Configuration applied".to_string();
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
    }

    fn reset_all(&mut self) {
        self.push_undo(self.working.clone());
        self.working = Config::default();
        self.selected_sensor = None;
    }

    fn push_undo(&mut self, config: Config) {
        if config == self.working {
            return;
        }
        if self.undo.last() != Some(&config) {
            self.undo.push(config);
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
            self.pending_undo = None;
            self.suppress_history_once = true;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.working.clone());
            self.working = next;
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
        let pointer_down = ctx.input(|i| i.pointer.any_down());
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

    fn poll_background_state(&mut self) {
        if self.last_sensor_update.elapsed() > Duration::from_secs(1) {
            self.refresh_sensors();
            self.last_sensor_update = Instant::now();
        }
        if self.last_daemon_check.elapsed() > Duration::from_secs(2) {
            self.daemon_running = self.service_manager.daemon_running();
            self.last_daemon_check = Instant::now();
        }
        if self.last_autostart_check.elapsed() > Duration::from_secs(5) {
            self.autostart_enabled = self.service_manager.autostart_enabled();
            self.last_autostart_check = Instant::now();
        }
        if self.last_device_scan.elapsed() > Duration::from_secs(4) {
            self.device_info = detect_device_info();
            self.last_device_scan = Instant::now();
        }
        self.poll_cli_job();
    }

    fn poll_cli_job(&mut self) {
        let mut finished: Option<JobResult> = None;
        if let Some(job) = self.media_job.as_mut() {
            while let Ok(message) = job.rx.try_recv() {
                match message {
                    JobMessage::Started(pid) => job.pid = Some(pid),
                    JobMessage::Finished(result) => finished = Some(result),
                }
            }
        }
        let Some(result) = finished else {
            return;
        };
        let job = self.media_job.take().unwrap();
        if job.restore_daemon {
            self.service_manager.start(&daemon_binary_path());
            self.daemon_running = true;
        }
        if result.success {
            self.media_status = format!("{} complete", job.label);
            if job.label == "Read device status" {
                self.parse_device_status(&result.stdout);
            }
        } else if job.cancel_requested {
            self.media_status = format!("{} stopped", job.label);
        } else {
            let detail = if result.stderr.trim().is_empty() {
                result.stdout
            } else {
                result.stderr
            };
            self.last_error = Some(format!("{} failed: {}", job.label, detail.trim()));
            self.media_status = format!("{} failed", job.label);
        }
    }

    fn parse_device_status(&mut self, text: &str) {
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

    fn start_cli_job(&mut self, label: impl Into<String>, args: Vec<String>) {
        if self.media_job.is_some() {
            return;
        }
        let label = label.into();
        let restore_daemon = self.service_manager.daemon_running();
        if restore_daemon {
            self.service_manager.stop();
            self.daemon_running = false;
        }
        let binary = daemon_binary_path();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            if restore_daemon {
                std::thread::sleep(Duration::from_millis(350));
            }
            let child = Command::new(binary)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            match child {
                Ok(child) => {
                    let pid = child.id();
                    let _ = tx.send(JobMessage::Started(pid));
                    match child.wait_with_output() {
                        Ok(output) => {
                            let _ = tx.send(JobMessage::Finished(JobResult {
                                success: output.status.success(),
                                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                            }));
                        }
                        Err(err) => {
                            let _ = tx.send(JobMessage::Finished(JobResult {
                                success: false,
                                stdout: String::new(),
                                stderr: err.to_string(),
                            }));
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(JobMessage::Finished(JobResult {
                        success: false,
                        stdout: String::new(),
                        stderr: err.to_string(),
                    }));
                }
            }
        });
        self.media_status = label.clone();
        self.media_job = Some(CliJob {
            label,
            rx,
            pid: None,
            restore_daemon,
            cancel_requested: false,
        });
    }

    fn stop_cli_job(&mut self) {
        if let Some(job) = self.media_job.as_mut() {
            job.cancel_requested = true;
            if let Some(pid) = job.pid {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
        }
    }

    fn ensure_custom_layout(&mut self) {
        if self.working.layout.preset != LayoutPreset::Custom {
            let enabled = self.working.enabled_sensor_ids();
            let refs: Vec<&str> = enabled.iter().map(String::as_str).collect();
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
        let enabled = self.working.enabled_sensor_ids();
        for (index, id) in enabled.iter().enumerate() {
            if self
                .working
                .layout
                .custom_slots
                .iter()
                .any(|s| &s.sensor_id == id)
            {
                continue;
            }
            let angle = index as f32 * std::f32::consts::TAU / enabled.len().max(1) as f32;
            self.working.layout.custom_slots.push(LayoutSlot {
                sensor_id: id.clone(),
                value_cx_norm: (0.5 + angle.cos() * 0.22).clamp(0.05, 0.95),
                value_cy_norm: (0.5 + angle.sin() * 0.22).clamp(0.05, 0.95),
                label_cx_norm: (0.5 + angle.cos() * 0.22).clamp(0.05, 0.95),
                label_cy_norm: (0.56 + angle.sin() * 0.22).clamp(0.05, 0.95),
                value_font_size: 52.0,
                label_font_size: 24.0,
            });
        }
    }

    fn move_selected(&mut self, dx: f32, dy: f32) {
        let Some(id) = self.selected_sensor.clone() else {
            return;
        };
        self.ensure_custom_layout();
        if let Some(slot) = self
            .working
            .layout
            .custom_slots
            .iter_mut()
            .find(|s| s.sensor_id == id)
        {
            let nx = dx / 480.0;
            let ny = dy / 480.0;
            slot.value_cx_norm = (slot.value_cx_norm + nx).clamp(0.0, 1.0);
            slot.value_cy_norm = (slot.value_cy_norm + ny).clamp(0.0, 1.0);
            slot.label_cx_norm = (slot.label_cx_norm + nx).clamp(0.0, 1.0);
            slot.label_cy_norm = (slot.label_cy_norm + ny).clamp(0.0, 1.0);
        }
    }

    fn show_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("TH420 Display");
                ui.separator();
                let dirty = self.is_dirty();
                if dirty {
                    ui.label(egui::RichText::new("Unsaved changes").color(egui::Color32::YELLOW));
                } else {
                    ui.label(egui::RichText::new("Saved").color(egui::Color32::GRAY));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(dirty, egui::Button::new("Apply")).clicked() {
                        self.apply();
                    }
                    if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                        self.revert();
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

    fn show_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .exact_width(150.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                for (page, label) in Page::ALL {
                    if ui.selectable_label(self.page == page, label).clicked() {
                        self.page = page;
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Service: {}",
                            self.service_manager.kind().name()
                        ))
                        .small()
                        .color(egui::Color32::GRAY),
                    );
                    let status = if self.device_info.connected {
                        "Display connected"
                    } else {
                        "Display disconnected"
                    };
                    ui.label(egui::RichText::new(status).small().color(
                        if self.device_info.connected {
                            egui::Color32::LIGHT_GREEN
                        } else {
                            egui::Color32::LIGHT_RED
                        },
                    ));
                });
            });
    }

    fn show_device_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Device");
        ui.add_space(8.0);
        egui::Grid::new("device-grid")
            .num_columns(2)
            .striped(true)
            .show(ui, |ui| {
                ui.label("Connection");
                ui.label(if self.device_info.connected {
                    "Connected"
                } else {
                    "Disconnected"
                });
                ui.end_row();
                ui.label("USB ID");
                ui.label(if self.device_info.connected {
                    format!("{}:{}", self.device_info.vid, self.device_info.pid)
                } else {
                    "264a:233c".to_string()
                });
                ui.end_row();
                ui.label("Product");
                ui.label(self.device_info.product.as_deref().unwrap_or("Unknown"));
                ui.end_row();
                ui.label("Manufacturer");
                ui.label(
                    self.device_info
                        .manufacturer
                        .as_deref()
                        .unwrap_or("Unknown"),
                );
                ui.end_row();
                ui.label("Revision");
                ui.label(self.device_info.revision.as_deref().unwrap_or("Unknown"));
                ui.end_row();
                ui.label("Control HID");
                ui.label(path_or_dash(self.device_info.control_hidraw.as_ref()));
                ui.end_row();
                ui.label("Image HID");
                ui.label(path_or_dash(self.device_info.image_hidraw.as_ref()));
                ui.end_row();
            });
        ui.add_space(12.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("Daemon").strong());
            ui.label(format!("Service manager: {}", self.service_manager.kind().name()));
            ui.horizontal(|ui| {
                ui.label(if self.daemon_running { "● Running" } else { "○ Stopped" });
                if self.daemon_running {
                    if ui.button("Stop").clicked() { self.service_manager.stop(); self.daemon_running = false; }
                    if ui.button("Restart").clicked() { self.service_manager.restart(&daemon_binary_path()); }
                } else if ui.button("Start").clicked() {
                    self.service_manager.start(&daemon_binary_path());
                    self.daemon_running = true;
                }
            });
            if self.service_manager.kind().supports_autostart() {
                ui.horizontal(|ui| {
                    ui.label(if self.autostart_enabled { "Autostart enabled" } else { "Autostart disabled" });
                    if self.autostart_enabled {
                        if ui.button("Disable").clicked() { self.service_manager.disable_autostart(); self.autostart_enabled = false; }
                    } else if ui.button("Enable").clicked() {
                        self.service_manager.enable_autostart(&daemon_binary_path());
                        self.autostart_enabled = self.service_manager.autostart_enabled();
                    }
                });
            } else {
                ui.label(egui::RichText::new("Autostart management is unavailable for this PID 1; direct process control remains available.").small().color(egui::Color32::GRAY));
            }
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Refresh device info").clicked() {
                self.device_info = detect_device_info();
            }
            if ui
                .add_enabled(
                    self.device_info.connected && self.media_job.is_none(),
                    egui::Button::new("Read device telemetry"),
                )
                .clicked()
            {
                self.start_cli_job("Read device status", vec!["--status".to_string()]);
            }
        });
        if let Some(temp) = self.device_coolant {
            ui.label(format!("Coolant: {temp:.0} °C"));
        }
        if let Some(rpm) = self.device_pump_rpm {
            ui.label(format!("Pump: {rpm} RPM"));
        }
    }

    fn show_display_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_min_width(420.0);
                ui.heading("Display");
                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Rotation").strong());
                    ui.add(
                        egui::Slider::new(&mut self.working.rotation, 0.0..=359.9)
                            .suffix("°")
                            .step_by(0.5),
                    );
                    ui.horizontal(|ui| {
                        for deg in [0.0f32, 90.0, 180.0, 270.0] {
                            if ui.small_button(format!("{deg:.0}°")).clicked() {
                                self.working.rotation = deg;
                            }
                        }
                    });
                });
                ui.add_space(6.0);
                self.background_editor(ui, ctx);
                ui.add_space(6.0);
                self.layout_editor(ui);
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.heading("Preview");
                ui.add_space(6.0);
                self.interactive_preview(ui);
                self.selected_layout_inspector(ui);
            });
        });
    }

    fn background_editor(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.group(|ui| {
            ui.label(egui::RichText::new("Background").strong());
            let has_image = self.working.background.image_path.is_some();
            ui.horizontal(|ui| {
                if ui.button("Browse…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "webp", "tiff"])
                        .pick_file()
                    {
                        self.working.background.image_path =
                            Some(path.to_string_lossy().into_owned());
                    }
                }
                if has_image && ui.button("Clear").clicked() {
                    self.working.background.image_path = None;
                }
            });
            if let Some(path) = &self.working.background.image_path {
                ui.label(egui::RichText::new(path).small().color(egui::Color32::GRAY));
            }
            for file in ctx.input(|i| i.raw.dropped_files.clone()) {
                if let Some(path) = file.path {
                    if image::open(&path).is_ok() {
                        self.working.background.image_path =
                            Some(path.to_string_lossy().into_owned());
                        break;
                    }
                }
            }
            ui.horizontal(|ui| {
                ui.label("Fit:");
                for (label, fit) in [
                    ("Cover", ImageFit::Cover),
                    ("Contain", ImageFit::Contain),
                    ("Stretch", ImageFit::Stretch),
                ] {
                    if ui
                        .selectable_label(self.working.background.fit == fit, label)
                        .clicked()
                    {
                        self.working.background.fit = fit;
                    }
                }
            });
            ui.add(egui::Slider::new(&mut self.working.background.zoom, 0.25..=3.0).text("Zoom"));
            ui.add(
                egui::Slider::new(&mut self.working.background.offset_x, -1.0..=1.0)
                    .text("Horizontal position"),
            );
            ui.add(
                egui::Slider::new(&mut self.working.background.offset_y, -1.0..=1.0)
                    .text("Vertical position"),
            );
            ui.add(
                egui::Slider::new(&mut self.working.background.opacity, 0..=255)
                    .text("Image opacity"),
            );
            ui.add(
                egui::Slider::new(&mut self.working.background.overlay_alpha, 0..=240)
                    .text("Darken"),
            );
            ui.add(
                egui::Slider::new(&mut self.working.background.blur_sigma, 0.0..=20.0).text("Blur"),
            );
            ui.horizontal(|ui| {
                ui.label("Canvas color:");
                let mut color = to_f32(self.working.background.background_color);
                if egui::color_picker::color_edit_button_rgb(ui, &mut color).changed() {
                    self.working.background.background_color = from_f32(color);
                }
            });
            if ui.small_button("Reset background settings").clicked() {
                let image_path = self.working.background.image_path.clone();
                self.working.background = config::BackgroundConfig::default();
                self.working.background.image_path = image_path;
            }
        });
    }

    fn layout_editor(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.label(egui::RichText::new("Layout").strong());
            ui.horizontal_wrapped(|ui| {
                for (label, preset) in [
                    ("Classic", LayoutPreset::Classic),
                    ("Grid 2×3", LayoutPreset::Grid2x3),
                    ("Big Top", LayoutPreset::BigTop),
                    ("Custom", LayoutPreset::Custom),
                ] {
                    if ui.selectable_label(self.working.layout.preset == preset, label).clicked() {
                        if preset == LayoutPreset::Custom { self.ensure_custom_layout(); }
                        else { self.working.layout.preset = preset; }
                    }
                }
            });
            ui.add(egui::Slider::new(&mut self.working.layout.max_visible, 1..=12).text("Display first enabled sensors"));
            ui.label(egui::RichText::new("For Custom layouts, drag sensors directly in the preview. Exact coordinates remain available in the inspector.").small().color(egui::Color32::GRAY));
        });
    }

    fn selected_layout_inspector(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected_sensor.clone() else {
            ui.label(
                egui::RichText::new("Click a sensor in the preview to edit its position.")
                    .small()
                    .color(egui::Color32::GRAY),
            );
            return;
        };
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new(format!("Selected: {id}")).strong());
            if ui.button("Switch to custom layout").clicked() {
                self.ensure_custom_layout();
            }
            if self.working.layout.preset != LayoutPreset::Custom {
                ui.label(
                    egui::RichText::new("Position editing requires Custom layout.")
                        .small()
                        .color(egui::Color32::GRAY),
                );
                return;
            }
            if let Some(slot) = self
                .working
                .layout
                .custom_slots
                .iter_mut()
                .find(|s| s.sensor_id == id)
            {
                egui::Grid::new("slot-inspector")
                    .num_columns(3)
                    .show(ui, |ui| {
                        ui.label("Value X");
                        ui.add(
                            egui::DragValue::new(&mut slot.value_cx_norm)
                                .range(0.0..=1.0)
                                .speed(0.002),
                        );
                        ui.label("normalized");
                        ui.end_row();
                        ui.label("Value Y");
                        ui.add(
                            egui::DragValue::new(&mut slot.value_cy_norm)
                                .range(0.0..=1.0)
                                .speed(0.002),
                        );
                        ui.label("normalized");
                        ui.end_row();
                        ui.label("Value size");
                        ui.add(
                            egui::DragValue::new(&mut slot.value_font_size)
                                .range(10.0..=140.0)
                                .speed(1.0),
                        );
                        ui.label("px");
                        ui.end_row();
                        ui.label("Label X");
                        ui.add(
                            egui::DragValue::new(&mut slot.label_cx_norm)
                                .range(0.0..=1.0)
                                .speed(0.002),
                        );
                        ui.label("normalized");
                        ui.end_row();
                        ui.label("Label Y");
                        ui.add(
                            egui::DragValue::new(&mut slot.label_cy_norm)
                                .range(0.0..=1.0)
                                .speed(0.002),
                        );
                        ui.label("normalized");
                        ui.end_row();
                        ui.label("Label size");
                        ui.add(
                            egui::DragValue::new(&mut slot.label_font_size)
                                .range(8.0..=80.0)
                                .speed(0.5),
                        );
                        ui.label("px");
                        ui.end_row();
                    });
            }
        });
    }

    fn interactive_preview(&mut self, ui: &mut egui::Ui) {
        let Some(texture) = self.preview_texture.clone() else {
            ui.spinner();
            return;
        };
        let size = egui::vec2(420.0, 420.0);
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        ui.painter().image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        ui.painter().circle_stroke(
            rect.center(),
            rect.width() / 2.0,
            egui::Stroke::new(1.5_f32, egui::Color32::from_gray(100)),
        );

        let enabled = self.working.enabled_sensor_ids();
        let refs: Vec<&str> = enabled.iter().map(String::as_str).collect();
        let slots = self.working.layout.preset_slots(&refs);

        if response.clicked() || response.drag_started() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let dx = (pointer.x - rect.left()) / rect.width() * 480.0;
                let dy = (pointer.y - rect.top()) / rect.height() * 480.0;
                let (lx, ly) = rotate_point(dx, dy, -self.working.rotation);
                self.selected_sensor = slots
                    .iter()
                    .map(|slot| {
                        let cx = (slot.value_cx + slot.label_cx) as f32 / 2.0;
                        let cy = (slot.value_y + slot.label_y) as f32 / 2.0;
                        let dist = ((cx - lx).powi(2) + (cy - ly).powi(2)).sqrt();
                        (slot.sensor_id.clone(), dist)
                    })
                    .filter(|(_, dist)| *dist <= 70.0)
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                    .map(|(id, _)| id);
            }
        }

        if response.dragged() && self.selected_sensor.is_some() {
            self.ensure_custom_layout();
            let incremental = response.drag_delta();
            let scale = 480.0 / rect.width();
            let (dx, dy) = rotate_vector(
                incremental.x * scale,
                incremental.y * scale,
                -self.working.rotation,
            );
            self.move_selected(dx, dy);
        }

        if let Some(selected) = &self.selected_sensor {
            let enabled = self.working.enabled_sensor_ids();
            let refs: Vec<&str> = enabled.iter().map(String::as_str).collect();
            if let Some(slot) = self
                .working
                .layout
                .preset_slots(&refs)
                .into_iter()
                .find(|slot| &slot.sensor_id == selected)
            {
                let cx = (slot.value_cx + slot.label_cx) as f32 / 2.0;
                let cy = (slot.value_y + slot.label_y) as f32 / 2.0;
                let (rx, ry) = rotate_point(cx, cy, self.working.rotation);
                let point = egui::pos2(
                    rect.left() + rx / 480.0 * rect.width(),
                    rect.top() + ry / 480.0 * rect.height(),
                );
                ui.painter().circle_stroke(
                    point,
                    28.0,
                    egui::Stroke::new(2.0_f32, egui::Color32::YELLOW),
                );
            }
        }
    }

    fn show_sensors_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_min_width(520.0);
                ui.heading("Sensors");
                ui.label(egui::RichText::new("Order determines display priority. Use the arrows to reorder sensors; only the first enabled entries up to the layout limit are shown.").small().color(egui::Color32::GRAY));
                ui.add_space(6.0);
                let readings = self.sensor_values.readings.clone();
                let defaults = Config::default();
                let mut move_request: Option<(usize, isize)> = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for index in 0..self.working.sensors.len() {
                        let id = self.working.sensors[index].id.clone();
                        let raw = readings.get(&id).copied();
                        let default = defaults.sensor_by_id(&id).cloned().unwrap_or_else(|| self.working.sensors[index].clone());
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                if ui.small_button("↑").on_hover_text("Move up").clicked() && index > 0 { move_request = Some((index, -1)); }
                                if ui.small_button("↓").on_hover_text("Move down").clicked() && index + 1 < self.working.sensors.len() { move_request = Some((index, 1)); }
                                ui.checkbox(&mut self.working.sensors[index].enabled, "");
                                let entry = &self.working.sensors[index];
                                ui.label(egui::RichText::new(&entry.label).strong());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(raw.map(|v| format_value(&entry.unit, v)).unwrap_or_else(|| "N/A".to_string()));
                                });
                            });
                            let open_id = ui.make_persistent_id(format!("sensor-details-{id}"));
                            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), open_id, false)
                                .show_header(ui, |ui| { ui.label("Settings"); })
                                .body(|ui| {
                                    let entry = &mut self.working.sensors[index];
                                    ui.horizontal(|ui| {
                                        ui.label("Label:"); ui.text_edit_singleline(&mut entry.label);
                                        let mut color = to_f32(entry.label_color);
                                        ui.label("Label color:");
                                        if egui::color_picker::color_edit_button_rgb(ui, &mut color).changed() { entry.label_color = from_f32(color); }
                                        if ui.small_button("Reset").clicked() { *entry = default.clone(); }
                                    });
                                    edit_color_map(ui, &mut entry.color_map, &entry.unit, raw);
                                });
                        });
                        ui.add_space(4.0);
                    }
                });
                if let Some((index, delta)) = move_request {
                    let target = (index as isize + delta) as usize;
                    self.working.sensors.swap(index, target);
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.heading("Preview");
                self.interactive_preview(ui);
            });
        });
    }

    fn show_media_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Media");
        ui.horizontal(|ui| {
            ui.label(format!("Status: {}", self.media_status));
            if self.media_job.is_some() {
                ui.spinner();
                if ui.button("Stop").clicked() {
                    self.stop_cli_job();
                }
            }
        });
        if let Some(err) = &self.last_error {
            ui.label(egui::RichText::new(err).color(egui::Color32::LIGHT_RED));
        }
        ui.add_space(8.0);

        ui.columns(2, |columns| {
            columns[0].group(|ui| {
                ui.label(egui::RichText::new("Brightness").strong());
                ui.add(egui::Slider::new(&mut self.brightness, 0..=100).suffix("%"));
                if ui
                    .add_enabled(
                        self.media_job.is_none() && self.device_info.connected,
                        egui::Button::new("Apply to device"),
                    )
                    .clicked()
                {
                    self.start_cli_job(
                        "Set brightness",
                        vec!["--standby-brightness".into(), self.brightness.to_string()],
                    );
                }
            });
            columns[1].group(|ui| {
                ui.label(egui::RichText::new("Persistent standby image").strong());
                if ui.button("Choose image…").clicked() {
                    self.standby_path = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "webp", "tiff"])
                        .pick_file();
                }
                if let Some(path) = &self.standby_path {
                    ui.label(
                        path.file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("(image)"),
                    );
                }
                ui.horizontal(|ui| {
                    ui.label("Temperature color:");
                    egui::color_picker::color_edit_button_rgb(ui, &mut self.pump_color);
                });
                ui.label(
                    egui::RichText::new("Writes the persistent standby image to device flash.")
                        .small()
                        .color(egui::Color32::YELLOW),
                );
                if ui
                    .add_enabled(
                        self.media_job.is_none()
                            && self.standby_path.is_some()
                            && self.device_info.connected,
                        egui::Button::new("Upload standby image"),
                    )
                    .clicked()
                {
                    let path = self
                        .standby_path
                        .as_ref()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    let color = from_f32(self.pump_color);
                    self.start_cli_job(
                        "Upload standby image",
                        vec![
                            "--upload-standby".into(),
                            path,
                            "--pump-temp-color".into(),
                            format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]),
                        ],
                    );
                }
            });
        });
        ui.add_space(8.0);
        ui.columns(2, |columns| {
            columns[0].group(|ui| {
                ui.label(egui::RichText::new("Persistent boot animation").strong());
                if ui.button("Choose GIF…").clicked() {
                    self.boot_path = rfd::FileDialog::new().add_filter("GIF", &["gif"]).pick_file();
                    self.boot_info = self.boot_path.as_ref().map(|p| inspect_gif(p));
                }
                if let Some(path) = &self.boot_path { ui.label(path.file_name().and_then(|s| s.to_str()).unwrap_or("(GIF)")); }
                show_gif_info(ui, self.boot_info.as_ref(), true);
                ui.label(egui::RichText::new("Replaces the persistent boot animation stored in device flash.").small().color(egui::Color32::YELLOW));
                let valid = matches!(self.boot_info, Some(Ok(ref info)) if info.uniform_delay_ms.is_some() && info.min_delay_ms >= 80);
                if ui.add_enabled(self.media_job.is_none() && valid && self.device_info.connected, egui::Button::new("Upload boot animation")).clicked() {
                    let path = self.boot_path.as_ref().unwrap().to_string_lossy().into_owned();
                    self.start_cli_job("Upload boot animation", vec!["--upload-boot".into(), path]);
                }
            });
            columns[1].group(|ui| {
                ui.label(egui::RichText::new("Live GIF").strong());
                if ui.button("Choose GIF…").clicked() {
                    self.live_gif_path = rfd::FileDialog::new().add_filter("GIF", &["gif"]).pick_file();
                    self.live_info = self.live_gif_path.as_ref().map(|p| inspect_gif(p));
                }
                if let Some(path) = &self.live_gif_path { ui.label(path.file_name().and_then(|s| s.to_str()).unwrap_or("(GIF)")); }
                show_gif_info(ui, self.live_info.as_ref(), false);
                ui.checkbox(&mut self.live_forever, "Loop until stopped");
                if !self.live_forever { ui.add(egui::DragValue::new(&mut self.live_loops).range(1..=10000).prefix("Loops: ")); }
                if ui.add_enabled(self.media_job.is_none() && self.live_gif_path.is_some() && self.device_info.connected, egui::Button::new("Play live GIF")).clicked() {
                    let path = self.live_gif_path.as_ref().unwrap().to_string_lossy().into_owned();
                    self.start_cli_job("Play live GIF", vec![
                        "--play-live-gif".into(), path,
                        "--live-loops".into(), if self.live_forever { "0".into() } else { self.live_loops.to_string() },
                        "--live-brightness".into(), self.brightness.to_string(),
                    ]);
                }
            });
        });
        ui.add_space(8.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("Live frame sequence").strong());
            ui.horizontal(|ui| {
                if ui.button("Choose frames…").clicked() {
                    self.live_frame_paths = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "webp", "tiff"])
                        .pick_files()
                        .unwrap_or_default();
                }
                if ui.button("Clear").clicked() {
                    self.live_frame_paths.clear();
                }
                ui.add(egui::Slider::new(&mut self.live_fps, 1..=60).text("FPS"));
                ui.checkbox(&mut self.live_forever, "Loop until stopped");
                if !self.live_forever {
                    ui.add(
                        egui::DragValue::new(&mut self.live_loops)
                            .range(1..=10000)
                            .prefix("Loops: "),
                    );
                }
            });
            ui.label(format!("{} frame(s) selected", self.live_frame_paths.len()));
            if ui
                .add_enabled(
                    self.media_job.is_none()
                        && !self.live_frame_paths.is_empty()
                        && self.device_info.connected,
                    egui::Button::new("Play frame sequence"),
                )
                .clicked()
            {
                let mut args = vec!["--play-live-frames".to_string()];
                args.extend(
                    self.live_frame_paths
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned()),
                );
                args.extend([
                    "--live-fps".into(),
                    self.live_fps.to_string(),
                    "--live-loops".into(),
                    if self.live_forever {
                        "0".into()
                    } else {
                        self.live_loops.to_string()
                    },
                    "--live-brightness".into(),
                    self.brightness.to_string(),
                ]);
                self.start_cli_job("Play frame sequence", args);
            }
        });

        // Drop a file anywhere on this page: GIF -> live GIF, otherwise standby image.
        for file in ctx.input(|i| i.raw.dropped_files.clone()) {
            let Some(path) = file.path else {
                continue;
            };
            if path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("gif"))
                .unwrap_or(false)
            {
                self.live_gif_path = Some(path.clone());
                self.live_info = Some(inspect_gif(&path));
            } else if image::open(&path).is_ok() {
                self.standby_path = Some(path);
            }
        }
    }

    fn show_profiles_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Profiles");
        ui.label(egui::RichText::new("Profiles store complete runtime display configurations. Device-flash media is intentionally kept separate.").small().color(egui::Color32::GRAY));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Profile")
                .selected_text(self.selected_profile.as_deref().unwrap_or("None"))
                .show_ui(ui, |ui| {
                    for name in self.profile_names.clone() {
                        ui.selectable_value(&mut self.selected_profile, Some(name.clone()), name);
                    }
                });
            if ui
                .add_enabled(self.selected_profile.is_some(), egui::Button::new("Load"))
                .clicked()
            {
                let name = self.selected_profile.clone().unwrap();
                match self.profiles.load(&name) {
                    Ok(config) => {
                        self.working = config;
                        self.selected_sensor = None;
                    }
                    Err(err) => self.last_error = Some(format!("Failed to load profile: {err}")),
                }
            }
            if ui
                .add_enabled(self.selected_profile.is_some(), egui::Button::new("Delete"))
                .clicked()
            {
                self.confirm_delete_profile = self.selected_profile.clone();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut self.profile_name_edit);
            if ui.button("Save as new profile").clicked() {
                let name = ProfileStore::sanitize_name(&self.profile_name_edit);
                match self.profiles.save(&name, &self.working) {
                    Ok(()) => {
                        self.refresh_profiles();
                        self.selected_profile = Some(name.clone());
                        self.profile_name_edit = name;
                    }
                    Err(err) => self.last_error = Some(format!("Failed to create profile: {err}")),
                }
            }
            if ui
                .add_enabled(
                    self.selected_profile.is_some() && !self.profile_name_edit.trim().is_empty(),
                    egui::Button::new("Rename selected"),
                )
                .clicked()
            {
                let old = self.selected_profile.clone().unwrap();
                let new = ProfileStore::sanitize_name(&self.profile_name_edit);
                match self.profiles.rename(&old, &new) {
                    Ok(()) => {
                        self.refresh_profiles();
                        self.selected_profile = Some(new);
                    }
                    Err(err) => self.last_error = Some(format!("Failed to rename profile: {err}")),
                }
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Import…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("TH420 profile", &["toml"])
                    .pick_file()
                {
                    match self.profiles.import(&path) {
                        Ok((name, config)) => {
                            self.refresh_profiles();
                            self.selected_profile = Some(name);
                            self.working = config;
                        }
                        Err(err) => self.last_error = Some(format!("Profile import failed: {err}")),
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
                let name = self.selected_profile.clone().unwrap();
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(format!("{name}.toml"))
                    .save_file()
                {
                    if let Err(err) = self.profiles.export(&name, &path) {
                        self.last_error = Some(format!("Profile export failed: {err}"));
                    }
                }
            }
            if ui.button("Open profile directory").clicked() {
                open_path(self.profiles.dir());
            }
        });
        if let Some(active) = self.profiles.active_name() {
            ui.label(format!("Active profile: {active}"));
        }
        ui.label(egui::RichText::new("Applying a configuration while a profile is selected updates that profile and makes it active.").small().color(egui::Color32::GRAY));
    }

    fn refresh_profiles(&mut self) {
        self.profile_names = self.profiles.list().unwrap_or_default();
    }

    fn show_diagnostics_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Diagnostics");
        let text = self.diagnostics_text();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.monospace(&text);
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Copy diagnostics").clicked() {
                ctx.copy_text(text);
            }
            if ui.button("Open config directory").clicked() {
                if let Some(parent) = self.config_path.parent() {
                    open_path(parent);
                }
            }
            if ui.button("Open profile directory").clicked() {
                open_path(self.profiles.dir());
            }
            if ui
                .add_enabled(
                    self.media_job.is_none() && self.device_info.connected,
                    egui::Button::new("Refresh device telemetry"),
                )
                .clicked()
            {
                self.start_cli_job("Read device status", vec!["--status".into()]);
            }
        });
    }

    fn diagnostics_text(&self) -> String {
        let mut out = String::new();
        out.push_str("TH420 Display diagnostics\n\n");
        out.push_str(&format!(
            "device.connected={}\n",
            self.device_info.connected
        ));
        out.push_str(&format!(
            "device.usb_id={}:{}\n",
            self.device_info.vid, self.device_info.pid
        ));
        out.push_str(&format!(
            "device.product={}\n",
            self.device_info.product.as_deref().unwrap_or("unknown")
        ));
        out.push_str(&format!(
            "device.revision={}\n",
            self.device_info.revision.as_deref().unwrap_or("unknown")
        ));
        out.push_str(&format!(
            "device.control_hid={}\n",
            path_or_dash(self.device_info.control_hidraw.as_ref())
        ));
        out.push_str(&format!(
            "device.image_hid={}\n",
            path_or_dash(self.device_info.image_hidraw.as_ref())
        ));
        out.push_str(&format!(
            "service.manager={}\n",
            self.service_manager.kind().name()
        ));
        out.push_str(&format!("service.daemon_running={}\n", self.daemon_running));
        out.push_str(&format!("service.autostart={}\n", self.autostart_enabled));
        out.push_str(&format!(
            "runtime.daemon_binary={}\n",
            daemon_binary_path().display()
        ));
        out.push_str(&format!("runtime.config={}\n", self.config_path.display()));
        out.push_str(&format!(
            "runtime.profile={}\n",
            self.selected_profile.as_deref().unwrap_or("none")
        ));
        out.push_str(&format!("runtime.unsaved_changes={}\n", self.is_dirty()));
        if let Some(temp) = self.device_coolant {
            out.push_str(&format!("device.coolant_c={temp:.1}\n"));
        }
        if let Some(rpm) = self.device_pump_rpm {
            out.push_str(&format!("device.pump_rpm={rpm}\n"));
        }
        out.push_str("\nsensors:\n");
        let mut readings: Vec<_> = self.sensor_values.readings.iter().collect();
        readings.sort_by_key(|(name, _)| *name);
        for (name, value) in readings {
            out.push_str(&format!("  {name}={value:.2}\n"));
        }
        if let Some(job) = &self.media_job {
            out.push_str(&format!("runtime.device_job={}\n", job.label));
        }
        if let Some(error) = &self.last_error {
            out.push_str(&format!(
                "runtime.last_error={}\n",
                error.replace('\n', " | ")
            ));
        }
        out
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        let undo =
            ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Z) && !i.modifiers.shift);
        let redo = ctx.input(|i| {
            (i.modifiers.ctrl && i.modifiers.shift && i.key_pressed(egui::Key::Z))
                || (i.modifiers.ctrl && i.key_pressed(egui::Key::Y))
        });
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }

        if !matches!(self.page, Page::Display | Page::Sensors) || self.selected_sensor.is_none() {
            return;
        }
        let (mut dx, mut dy) = (0.0f32, 0.0f32);
        let coarse = ctx.input(|i| i.modifiers.shift);
        let step = if coarse { 5.0 } else { 1.0 };
        ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) {
                dx -= step;
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                dx += step;
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                dy -= step;
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                dy += step;
            }
        });
        if dx != 0.0 || dy != 0.0 {
            self.move_selected(dx, dy);
        }
    }

    fn confirmation_windows(&mut self, ctx: &egui::Context) {
        if self.confirm_reset_all {
            egui::Window::new("Reset all settings?").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label("This resets the working configuration to defaults. It will not be written until Apply is pressed.");
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { self.confirm_reset_all = false; }
                    if ui.button("Reset all").clicked() { self.reset_all(); self.confirm_reset_all = false; }
                });
            });
        }
        if let Some(name) = self.confirm_delete_profile.clone() {
            egui::Window::new("Delete profile?")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!("Delete profile '{name}'?"));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_delete_profile = None;
                        }
                        if ui.button("Delete").clicked() {
                            match self.profiles.delete(&name) {
                                Ok(()) => {
                                    self.refresh_profiles();
                                    if self.selected_profile.as_deref() == Some(&name) {
                                        self.selected_profile = None;
                                    }
                                }
                                Err(err) => {
                                    self.last_error =
                                        Some(format!("Failed to delete profile: {err}"))
                                }
                            }
                            self.confirm_delete_profile = None;
                        }
                    });
                });
        }
        if let Some(error) = self.last_error.clone() {
            egui::TopBottomPanel::bottom("error-banner").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(error).color(egui::Color32::LIGHT_RED));
                    if ui.button("Dismiss").clicked() {
                        self.last_error = None;
                    }
                });
            });
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background_state();
        self.handle_keyboard(ctx);

        if self.preview_texture.is_none()
            || self.preview_config.as_ref() != Some(&self.working)
            || self.last_preview.elapsed() > Duration::from_secs(1)
        {
            self.update_preview(ctx);
        }

        let before = self.working.clone();
        self.show_top_bar(ctx);
        self.show_sidebar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            match self.page {
                Page::Device => self.show_device_page(ui),
                Page::Display => self.show_display_page(ui, ctx),
                Page::Sensors => self.show_sensors_page(ui),
                Page::Media => self.show_media_page(ui, ctx),
                Page::Profiles => self.show_profiles_page(ui),
                Page::Diagnostics => self.show_diagnostics_page(ui, ctx),
            }
            ui.add_space(8.0);
            if !matches!(self.page, Page::Media | Page::Profiles | Page::Diagnostics) {
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Reset all…").clicked() {
                        self.confirm_reset_all = true;
                    }
                    if self.is_dirty() {
                        ui.label(
                            egui::RichText::new("Changes are preview-only until Apply.")
                                .small()
                                .color(egui::Color32::GRAY),
                        );
                    }
                });
            }
        });
        self.capture_history(before, ctx);
        self.confirmation_windows(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

fn edit_color_map(ui: &mut egui::Ui, map: &mut Vec<ColorPoint>, unit: &str, current: Option<f32>) {
    if map.len() >= 2 {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(300.0, 16.0), egui::Sense::hover());
        let min = map.first().unwrap().value;
        let max = map.last().unwrap().value.max(min + f32::EPSILON);
        for i in 0..100 {
            let t0 = i as f32 / 100.0;
            let t1 = (i + 1) as f32 / 100.0;
            let value = min + t0 * (max - min);
            let c = interpolate_color(value, map);
            let r = egui::Rect::from_min_max(
                egui::pos2(rect.left() + rect.width() * t0, rect.top()),
                egui::pos2(rect.left() + rect.width() * t1 + 1.0, rect.bottom()),
            );
            ui.painter()
                .rect_filled(r, 0.0, egui::Color32::from_rgb(c[0], c[1], c[2]));
        }
        if let Some(value) = current {
            let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
            let x = rect.left() + rect.width() * t;
            ui.painter().line_segment(
                [
                    egui::pos2(x, rect.top() - 3.0),
                    egui::pos2(x, rect.bottom() + 3.0),
                ],
                egui::Stroke::new(2.0_f32, egui::Color32::WHITE),
            );
        }
    }
    let mut remove = None;
    let map_len = map.len();
    for (index, point) in map.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut point.value)
                    .speed(0.5)
                    .suffix(format!(" {unit}")),
            );
            let mut color = to_f32(point.color);
            if egui::color_picker::color_edit_button_rgb(ui, &mut color).changed() {
                point.color = from_f32(color);
            }
            if map_len > 2 && ui.small_button("Remove").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        map.remove(index);
    }
    if ui.small_button("Add threshold").clicked() {
        let value = map.last().map(|p| p.value + 10.0).unwrap_or(50.0);
        map.push(ColorPoint {
            value,
            color: [200, 200, 200],
        });
    }
    map.sort_by(|a, b| {
        a.value
            .partial_cmp(&b.value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn inspect_gif(path: &Path) -> Result<GifInfo, String> {
    let decoder = GifDecoder::new(BufReader::new(File::open(path).map_err(|e| e.to_string())?))
        .map_err(|e| e.to_string())?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(|e| e.to_string())?;
    if frames.is_empty() {
        return Err("GIF contains no frames".to_string());
    }
    let mut delays = Vec::with_capacity(frames.len());
    for frame in &frames {
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let ms = numerator.checked_div(denominator).unwrap_or(0);
        delays.push(ms);
    }
    let first = delays[0];
    let uniform = delays.iter().all(|delay| *delay == first).then_some(first);
    Ok(GifInfo {
        frames: frames.len(),
        uniform_delay_ms: uniform,
        min_delay_ms: *delays.iter().min().unwrap_or(&0),
        max_delay_ms: *delays.iter().max().unwrap_or(&0),
        file_size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    })
}

fn show_gif_info(ui: &mut egui::Ui, info: Option<&Result<GifInfo, String>>, boot: bool) {
    match info {
        Some(Ok(info)) => {
            ui.label(format!(
                "{} frames · {} KiB",
                info.frames,
                info.file_size / 1024
            ));
            if let Some(delay) = info.uniform_delay_ms {
                ui.label(format!("Frame delay: {delay} ms"));
            } else {
                ui.label(format!(
                    "Variable delays: {}–{} ms",
                    info.min_delay_ms, info.max_delay_ms
                ));
            }
            if boot {
                if info.uniform_delay_ms.is_none() {
                    ui.label(
                        egui::RichText::new("Boot animation requires one uniform frame delay.")
                            .color(egui::Color32::LIGHT_RED),
                    );
                }
                if info.min_delay_ms < 80 {
                    ui.label(
                        egui::RichText::new("Boot animation frame delay must be at least 80 ms.")
                            .color(egui::Color32::LIGHT_RED),
                    );
                }
            }
        }
        Some(Err(err)) => {
            ui.label(egui::RichText::new(err).color(egui::Color32::LIGHT_RED));
        }
        None => {}
    }
}

fn rotate_point(x: f32, y: f32, degrees: f32) -> (f32, f32) {
    let angle = degrees.to_radians();
    let (sin, cos) = angle.sin_cos();
    let dx = x - 240.0;
    let dy = y - 240.0;
    (240.0 + dx * cos - dy * sin, 240.0 + dx * sin + dy * cos)
}

fn rotate_vector(x: f32, y: f32, degrees: f32) -> (f32, f32) {
    let angle = degrees.to_radians();
    let (sin, cos) = angle.sin_cos();
    (x * cos - y * sin, x * sin + y * cos)
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
                info.serial = read_trimmed(ancestor.join("serial"));
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
    path.map(|p| p.display().to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn open_path(path: &Path) {
    let _ = Command::new("xdg-open").arg(path).spawn();
}

fn extract_daemon_from_appimage() -> Option<PathBuf> {
    let appdir = std::env::var_os("APPDIR")?;
    let src = PathBuf::from(appdir).join("usr/bin/th420-display");
    if !src.exists() {
        return None;
    }
    let target_dir = dirs::home_dir()?.join(".local/bin");
    std::fs::create_dir_all(&target_dir).ok()?;
    let target = target_dir.join("th420-display");
    std::fs::copy(&src, &target).ok()?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&target).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&target, perms).ok()?;
    Some(target)
}

fn daemon_binary_path() -> PathBuf {
    if std::env::var_os("APPIMAGE").is_some() {
        let local = dirs::home_dir()
            .map(|h| h.join(".local/bin/th420-display"))
            .filter(|p| p.exists());
        if let Some(path) = local {
            return path;
        }
        if let Some(path) = extract_daemon_from_appimage() {
            return path;
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("th420-display")))
        .unwrap_or_else(|| PathBuf::from("th420-display"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_rotation_roundtrip() {
        let point = rotate_point(100.0, 140.0, 73.0);
        let back = rotate_point(point.0, point.1, -73.0);
        assert!((back.0 - 100.0).abs() < 0.01);
        assert!((back.1 - 140.0).abs() < 0.01);
    }

    #[test]
    fn vector_rotation_roundtrip() {
        let point = rotate_vector(12.0, -4.0, 120.0);
        let back = rotate_vector(point.0, point.1, -120.0);
        assert!((back.0 - 12.0).abs() < 0.01);
        assert!((back.1 + 4.0).abs() < 0.01);
    }
}
