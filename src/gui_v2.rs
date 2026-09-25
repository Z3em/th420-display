mod config;
mod renderer;
mod sensors;
mod service_manager;

use config::{default_config_path, Config, ImageFit, LayoutPreset, LayoutSlot};
use eframe::egui;
use image::codecs::gif::GifDecoder;
use image::{imageops, AnimationDecoder};
use renderer::Renderer;
use sensors::SensorValues;
use service_manager::ServiceManager;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
    Boot,
    StandbyLoop,
    StandbyOnce,
}

impl DevicePreviewMode {
    const ALL: [Self; 4] = [
        Self::Off,
        Self::Boot,
        Self::StandbyLoop,
        Self::StandbyOnce,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Boot => "Boot",
            Self::StandbyLoop => "Standby (loop)",
            Self::StandbyOnce => "Standby (once)",
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

struct App {
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

    brightness: u8,
    boot_path: Option<PathBuf>,
    standby_path: Option<PathBuf>,
    stream_path: Option<PathBuf>,
    device_preview_job: Option<DevicePreviewJob>,
    status_text: String,
    last_error: Option<String>,
}

fn main() -> eframe::Result<()> {
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
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
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
    fn new() -> Self {
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

        let mut app = Self {
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
            brightness: 80,
            boot_path: None,
            standby_path: None,
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
    }

    fn poll_device_preview_job(&mut self) {
        let finished = self
            .device_preview_job
            .as_mut()
            .and_then(|job| job.child.try_wait().ok().flatten())
            .is_some();
        if !finished {
            return;
        }
        let job = self.device_preview_job.take().unwrap();
        self.device_preview_mode = DevicePreviewMode::Off;
        if job.restore_live_display {
            self.service_manager.start(&daemon_binary_path());
            self.daemon_running = true;
        } else {
            self.daemon_running = false;
        }
        self.status_text = format!("{} preview finished", job.mode.label());
    }

    fn is_dirty(&self) -> bool {
        self.working != self.committed
    }

    fn apply(&mut self) {
        self.working.background.image_path = match self.background_source {
            BackgroundSource::File => self.background_file_path.clone(),
            BackgroundSource::Stream | BackgroundSource::SolidColor => None,
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
            self.service_manager.start(&daemon_binary_path());
            self.daemon_running = true;
        } else {
            self.service_manager.stop();
            self.daemon_running = false;
        }
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
                            self.device_info.product.as_deref().unwrap_or("TH420 V2 Ultra EX")
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
                if self.page == Page::LiveDisplay && self.live_tab == LiveTab::Overlay {
                    self.interactive_preview(ui);
                } else {
                    self.static_preview(ui);
                }
                if self.page == Page::StandbySettings {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Preview: {}",
                            self.preview_mode.label()
                        ))
                        .small()
                        .color(egui::Color32::GRAY),
                    );
                }
            });
    }

    fn update_preview_texture(&mut self, ctx: &egui::Context) {
        if self.last_preview_update.elapsed() < Duration::from_millis(300)
            && self.preview_texture.is_some()
        {
            return;
        }

        let image = if self.page == Page::StandbySettings {
            match self.preview_mode {
                PreviewMode::Boot => self.boot_path.as_deref().and_then(load_media_frame),
                PreviewMode::Standby => self.standby_path.as_deref().and_then(load_media_frame),
            }
        } else if self.page == Page::LiveDisplay
            && self.background_source == BackgroundSource::Stream
        {
            self.stream_path.as_deref().and_then(load_media_frame)
        } else {
            None
        };

        let image = image.unwrap_or_else(|| {
            self.renderer
                .render_preview(&self.working, &self.sensor_values)
        });
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
            egui::Stroke::new(1.0, egui::Color32::GRAY),
        );
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
            egui::Stroke::new(1.0, egui::Color32::GRAY),
        );

        if self.show_grid {
            let step = rect.width() * self.grid_size / 480.0;
            let mut x = rect.left();
            while x <= rect.right() {
                ui.painter().line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(0.5, egui::Color32::from_white_alpha(30)),
                );
                x += step.max(2.0);
            }
            let mut y = rect.top();
            while y <= rect.bottom() {
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    egui::Stroke::new(0.5, egui::Color32::from_white_alpha(30)),
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
                    egui::Stroke::new(2.0, egui::Color32::YELLOW),
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
        if let Some(sensor) = self.working.sensors.iter_mut().find(|sensor| sensor.id == id) {
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
            egui::Grid::new("overview-device").num_columns(2).show(ui, |ui| {
                ui.label("Model");
                ui.label(self.device_info.product.as_deref().unwrap_or("TH420 V2 Ultra EX"));
                ui.end_row();
                ui.label("USB ID");
                ui.label(format!("{}:{}", self.device_info.vid, self.device_info.pid));
                ui.end_row();
                ui.label("Firmware / revision");
                ui.label(self.device_info.revision.as_deref().unwrap_or("—"));
                ui.end_row();
                ui.label("Live Display");
                ui.label(if self.live_display_enabled { "Enabled" } else { "Disabled" });
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
            egui::Grid::new("overview-metrics").num_columns(2).show(ui, |ui| {
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
                BackgroundSource::Stream | BackgroundSource::SolidColor => None,
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
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter(
                                "Media",
                                &["png", "jpg", "jpeg", "webp", "bmp", "gif"],
                            )
                            .pick_file()
                        {
                            let path = path.to_string_lossy().into_owned();
                            self.background_file_path = Some(path.clone());
                            self.working.background.image_path = Some(path);
                        }
                    }
                });
                egui::ComboBox::from_label("Fit")
                    .selected_text(format!("{:?}", self.working.background.fit))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.working.background.fit, ImageFit::Cover, "Cover");
                        ui.selectable_value(&mut self.working.background.fit, ImageFit::Contain, "Contain");
                        ui.selectable_value(&mut self.working.background.fit, ImageFit::Stretch, "Stretch");
                    });
                ui.add(
                    egui::Slider::new(&mut self.working.background.zoom, 0.25..=4.0)
                        .text("Zoom"),
                );
            }
            BackgroundSource::Stream => {
                ui.horizontal(|ui| {
                    ui.label("Stream source");
                    ui.label(file_name(self.stream_path.as_ref()));
                    if ui.button("Browse…").clicked() {
                        self.stream_path = rfd::FileDialog::new()
                            .add_filter("Animated media", &["gif"])
                            .pick_file();
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "The current backend can preview direct stream media here; overlay composition over streamed animation remains a backend follow-up.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
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
                        self.working.background.background_color = color.map(|c| {
                            (c * 255.0).round().clamp(0.0, 255.0) as u8
                        });
                        self.working.background.image_path = None;
                    }
                });
            }
        }
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
            if let Some(index) = self.working.sensors.iter().position(|sensor| sensor.id == id) {
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
                        egui::Slider::new(&mut slot.label_font_size, 8.0..=64.0)
                            .text("Label size"),
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
            ui.selectable_value(&mut self.standby_tab, StandbyTab::Boot, "Boot");
            ui.selectable_value(&mut self.standby_tab, StandbyTab::Standby, "Standby");
        });
        ui.add_space(8.0);

        egui::ComboBox::from_label("Preview")
            .selected_text(self.preview_mode.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.preview_mode, PreviewMode::Boot, "Boot");
                ui.selectable_value(&mut self.preview_mode, PreviewMode::Standby, "Standby");
            });

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
                self.boot_path = rfd::FileDialog::new().add_filter("GIF", &["gif"]).pick_file();
            }
        });
        if ui
            .add_enabled(
                self.device_info.connected && self.boot_path.is_some(),
                egui::Button::new("Upload to device"),
            )
            .clicked()
        {
            let path = self.boot_path.as_ref().unwrap().to_string_lossy().into_owned();
            self.run_short_device_command("Upload boot animation", vec!["--upload-boot".into(), path]);
        }
    }

    fn show_standby_media_settings(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Standby").strong());
        ui.horizontal(|ui| {
            ui.label("File");
            ui.label(file_name(self.standby_path.as_ref()));
            if ui.button("Browse…").clicked() {
                self.standby_path = rfd::FileDialog::new()
                    .add_filter(
                        "Media",
                        &["png", "jpg", "jpeg", "webp", "bmp", "gif"],
                    )
                    .pick_file();
            }
        });
        let static_image = self
            .standby_path
            .as_deref()
            .map(|path| !is_gif(path))
            .unwrap_or(false);
        if ui
            .add_enabled(
                self.device_info.connected && static_image,
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
            self.run_short_device_command("Upload standby image", vec!["--upload-standby".into(), path]);
        }
        if self.standby_path.as_deref().map(is_gif) == Some(true) {
            ui.label(
                egui::RichText::new(
                    "Animated standby media can be previewed/tested; persistent device standby upload currently accepts static images.",
                )
                .small()
                .color(egui::Color32::GRAY),
            );
        }
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
            if ui.checkbox(&mut autostart, "Start Live Display on login").changed() {
                if autostart {
                    self.service_manager.enable_autostart(&daemon_binary_path());
                } else {
                    self.service_manager.disable_autostart();
                }
                self.autostart_enabled = self.service_manager.autostart_enabled();
            }
            ui.label(format!("Service manager: {}", self.service_manager.kind().name()));
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
        if restore {
            self.service_manager.stop();
            self.daemon_running = false;
            std::thread::sleep(Duration::from_millis(250));
        }
        let result = Command::new(daemon_binary_path()).arg("--status").output();
        if restore {
            self.service_manager.start(&daemon_binary_path());
            self.daemon_running = true;
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
        if restore {
            self.service_manager.stop();
            self.daemon_running = false;
            std::thread::sleep(Duration::from_millis(250));
        }
        let result = Command::new(daemon_binary_path()).args(args).output();
        if restore {
            self.service_manager.start(&daemon_binary_path());
            self.daemon_running = true;
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
        self.stop_device_preview();
        if mode == DevicePreviewMode::Off {
            return;
        }
        let path = match mode {
            DevicePreviewMode::Boot => self.boot_path.clone(),
            DevicePreviewMode::StandbyLoop | DevicePreviewMode::StandbyOnce => {
                self.standby_path.clone()
            }
            DevicePreviewMode::Off => None,
        };
        let Some(path) = path else {
            self.last_error = Some(format!("Select a {} file first", mode.label()));
            return;
        };

        let restore_live_display = self.live_display_enabled;
        if self.daemon_running {
            self.service_manager.stop();
            self.daemon_running = false;
            std::thread::sleep(Duration::from_millis(250));
        }

        let loops = match mode {
            DevicePreviewMode::StandbyOnce => 1,
            _ => 0,
        };
        let mut args = if is_gif(&path) {
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
        args.extend([
            "--live-brightness".to_string(),
            self.brightness.to_string(),
        ]);

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
                    self.service_manager.start(&daemon_binary_path());
                    self.daemon_running = true;
                }
            }
        }
    }

    fn stop_device_preview(&mut self) {
        let Some(mut job) = self.device_preview_job.take() else {
            self.device_preview_mode = DevicePreviewMode::Off;
            return;
        };
        let _ = job.child.kill();
        let _ = job.child.wait();
        if job.restore_live_display {
            self.service_manager.start(&daemon_binary_path());
            self.daemon_running = true;
        } else {
            self.daemon_running = false;
        }
        self.device_preview_mode = DevicePreviewMode::Off;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_device_preview();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        ctx.request_repaint_after(Duration::from_millis(150));
    }
}

fn snap(value: f32, grid: f32) -> f32 {
    if grid <= 0.0 {
        value
    } else {
        (value / grid).round() * grid
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

fn load_media_frame(path: &Path) -> Option<image::RgbImage> {
    let rgb = if is_gif(path) {
        let decoder = GifDecoder::new(BufReader::new(File::open(path).ok()?)).ok()?;
        let frame = decoder.into_frames().next()?.ok()?;
        image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8()
    } else {
        image::open(path).ok()?.into_rgb8()
    };
    Some(imageops::resize(
        &rgb,
        480,
        480,
        imageops::FilterType::Lanczos3,
    ))
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
