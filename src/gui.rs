mod config;
mod renderer;
mod sensors;
mod service_manager;

use config::{
    interpolate_color, ColorPoint, Config, ImageFit, LayoutPreset, LayoutSlot,
    SensorConfig, default_config_path, from_f32, to_f32,
};
use eframe::egui;
use renderer::{Renderer, format_value};
use sensors::SensorValues;
use service_manager::ServiceManager;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn icon_rgba() -> Vec<u8> {
    const N: u32 = 128;
    let cx = N as f32 / 2.0;
    let cy = N as f32 / 2.0;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let r = (dx * dx + dy * dy).sqrt();
            let i = ((y * N + x) * 4) as usize;
            if r > 62.0 { continue; }
            let angle = (dy.atan2(dx).to_degrees() + 90.0).rem_euclid(360.0);
            let color: [u8; 4] = if r > 57.0 {
                [50, 100, 185, 255]
            } else if r >= 34.0 && r <= 51.0 {
                if angle < 100.0 {
                    [70, 200, 95, 255]
                } else if angle >= 120.0 && angle < 220.0 {
                    [220, 125, 45, 255]
                } else if angle >= 240.0 && angle < 340.0 {
                    [45, 140, 220, 255]
                } else {
                    [10, 10, 20, 255]
                }
            } else {
                [10, 10, 20, 255]
            };
            rgba[i..i + 4].copy_from_slice(&color);
        }
    }
    rgba
}

fn make_app_icon() -> egui::IconData {
    egui::IconData { rgba: icon_rgba(), width: 128, height: 128 }
}

/// Install icon PNG + .desktop file so the taskbar shows the correct icon.
/// Uses an absolute path for Icon= so KDE doesn't need an icon-theme cache lookup.
fn install_app_resources() {
    let home = match std::env::var_os("HOME") {
        Some(h) => std::path::PathBuf::from(h),
        None => return,
    };

    let mut changed = false;

    let icon_dir = home.join(".local/share/icons/hicolor/128x128/apps");
    let icon_path = icon_dir.join("th420-config.png");
    if !icon_path.exists() {
        if std::fs::create_dir_all(&icon_dir).is_ok() {
            if let Some(img) = image::RgbaImage::from_raw(128, 128, icon_rgba()) {
                if img.save(&icon_path).is_ok() {
                    changed = true;
                }
            }
        }
    }

    let apps_dir = home.join(".local/share/applications");
    if std::fs::create_dir_all(&apps_dir).is_ok() {
        // When running from AppImage, point Exec at the AppImage file itself (stable path).
        let exec_path = std::env::var_os("APPIMAGE")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("th420-config"));
        // Use absolute path for Icon= — KDE loads it directly without theme cache.
        let content = format!(
"[Desktop Entry]\n\
Version=1.0\n\
Name=TH420 Display Config\n\
Comment=CPU/GPU monitor configurator for Thermaltake TH420 V2 LCD\n\
Exec={}\n\
Icon={}\n\
Type=Application\n\
Categories=System;Monitor;\n\
Terminal=false\n\
StartupNotify=true\n\
StartupWMClass=th420-config\n",
            exec_path.to_string_lossy(),
            icon_path.to_string_lossy(),
        );
        let desktop_path = apps_dir.join("th420-config.desktop");
        let existing = std::fs::read_to_string(&desktop_path).unwrap_or_default();
        if existing != content {
            if std::fs::write(&desktop_path, &content).is_ok() {
                let _ = std::process::Command::new("update-desktop-database")
                    .arg(apps_dir.to_str().unwrap_or(""))
                    .output();
                changed = true;
            }
        }
    }

    if changed {
        // Rebuild KDE service database so plasmashell can match our app-id
        // to the .desktop file before the window appears.
        let _ = std::process::Command::new("kbuildsycoca6")
            .arg("--noincremental")
            .output();
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }
}

fn main() -> eframe::Result<()> {
    install_app_resources();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([820.0, 660.0])
            .with_resizable(true)
            .with_title("TH420 Display Config")
            .with_app_id("th420-config")
            .with_icon(make_app_icon()),
        ..Default::default()
    };
    eframe::run_native("TH420 Config", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}

struct App {
    config: Config,
    config_path: PathBuf,
    sensors: sensors::SensorReader,
    sensor_values: SensorValues,
    last_sensor_update: Instant,
    liquid_temp: f32,
    renderer: Renderer,
    preview_texture: Option<egui::TextureHandle>,
    last_preview: Instant,
    config_dirty: bool,
    service_manager: ServiceManager,
    daemon_running: bool,
    last_daemon_check: Instant,
    autostart_enabled: bool,
    last_autostart_check: Instant,
}

impl App {
    fn new() -> Self {
        let config_path = default_config_path();
        let config = Config::load(&config_path).unwrap_or_default();
        let mut app = Self {
            config,
            config_path,
            sensors: sensors::SensorReader::new(),
            sensor_values: SensorValues { readings: HashMap::new() },
            last_sensor_update: Instant::now() - Duration::from_secs(10),
            liquid_temp: 0.0,
            renderer: Renderer::new(),
            preview_texture: None,
            last_preview: Instant::now() - Duration::from_secs(10),
            config_dirty: false,
            service_manager: ServiceManager::detect(),
            daemon_running: false,
            last_daemon_check: Instant::now(),
            autostart_enabled: false,
            last_autostart_check: Instant::now(),
        };
        app.daemon_running = app.service_manager.daemon_running();
        app.autostart_enabled = app.service_manager.autostart_enabled();
        app.refresh_sensors();
        app
    }

    #[cfg(test)]
    fn new_for_test(values: SensorValues) -> Self {
        Self {
            config: Config::default(),
            config_path: std::env::temp_dir().join("th420_test.toml"),
            sensors: sensors::SensorReader::new(),
            sensor_values: values,
            last_sensor_update: Instant::now(),
            liquid_temp: 0.0,
            renderer: Renderer::new(),
            preview_texture: None,
            last_preview: Instant::now() - Duration::from_secs(10),
            config_dirty: false,
            service_manager: ServiceManager::detect(),
            daemon_running: false,
            last_daemon_check: Instant::now(),
            autostart_enabled: false,
            last_autostart_check: Instant::now(),
        }
    }

    fn refresh_sensors(&mut self) {
        let mut v = self.sensors.read();
        v.readings.insert("coolant".to_string(), self.liquid_temp);
        self.sensor_values = v;
    }

    #[cfg(test)]
    fn current_readings(&self) -> &HashMap<String, f32> {
        &self.sensor_values.readings
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        let img = self.renderer.render_preview(&self.config, &self.sensor_values);
        let pixels: Vec<egui::Color32> = img.pixels()
            .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
            .collect();
        self.preview_texture = Some(ctx.load_texture(
            "preview",
            egui::ColorImage { size: [img.width() as usize, img.height() as usize], pixels },
            egui::TextureOptions::LINEAR,
        ));
    }

    fn save_config(&mut self) {
        let _ = self.config.save(&self.config_path);
        self.config_dirty = false;
    }
}

/// When running from an AppImage, extract the daemon binary to ~/.local/bin/
/// so it has a stable path usable by systemd services and direct invocation.
fn extract_daemon_from_appimage() -> Option<PathBuf> {
    let appdir = std::env::var_os("APPDIR")?;
    let src = PathBuf::from(appdir).join("usr/bin/th420-display");
    if !src.exists() { return None; }
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
    // AppImage mode: prefer stable extracted copy in ~/.local/bin/
    if std::env::var_os("APPIMAGE").is_some() {
        let local = dirs::home_dir()
            .map(|h| h.join(".local/bin/th420-display"))
            .filter(|p| p.exists());
        if let Some(p) = local { return p; }
        if let Some(p) = extract_daemon_from_appimage() { return p; }
    }
    // Dev/installed mode: sibling binary next to the GUI
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("th420-display")))
        .unwrap_or_else(|| PathBuf::from("th420-display"))
}

// ── Gradient bar ──────────────────────────────────────────────────────────────

fn draw_gradient_bar(ui: &mut egui::Ui, map: &[ColorPoint], width: f32, height: f32) {
    if map.len() < 2 { return; }
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter();
    let n = 80usize;
    let min_v = map.first().unwrap().value;
    let max_v = map.last().unwrap().value;
    let seg_w = rect.width() / n as f32 + 1.0;
    for i in 0..n {
        let t = i as f32 / (n - 1) as f32;
        let v = min_v + t * (max_v - min_v);
        let c = interpolate_color(v, map);
        let x = rect.left() + t * rect.width();
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(seg_w, height)),
            0.0, egui::Color32::from_rgb(c[0], c[1], c[2]),
        );
    }
    painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, egui::Color32::from_gray(60)),
        egui::StrokeKind::Middle);
}

// ── Color map editor ──────────────────────────────────────────────────────────

fn edit_color_map(ui: &mut egui::Ui, map: &mut Vec<ColorPoint>, unit: &str, current_value: f32) -> bool {
    let mut changed = false;
    ui.add_space(4.0);
    draw_gradient_bar(ui, map, 280.0, 14.0);
    ui.add_space(6.0);

    let map_len = map.len();
    let mut to_remove: Option<usize> = None;
    for (i, point) in map.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{}", i + 1)).color(egui::Color32::GRAY).small());
            if ui.add(egui::DragValue::new(&mut point.value)
                .speed(0.5).suffix(format!(" {unit}"))
                .min_decimals(0).max_decimals(1)
            ).changed() { changed = true; }
            let mut f = to_f32(point.color);
            if egui::color_picker::color_edit_button_rgb(ui, &mut f).changed() {
                point.color = from_f32(f);
                changed = true;
            }
            if map_len > 2 {
                if ui.small_button("x").clicked() { to_remove = Some(i); changed = true; }
            }
        });
    }
    if let Some(i) = to_remove { map.remove(i); }

    ui.horizontal(|ui| {
        if ui.small_button("+ Add point").clicked() {
            let new_val = if map.len() >= 2 {
                let last = map.last().unwrap().value;
                let prev = map[map.len() - 2].value;
                last + (last - prev)
            } else {
                map.last().map(|p| p.value + 10.0).unwrap_or(50.0)
            };
            map.push(ColorPoint { value: new_val, color: [200, 200, 200] });
            changed = true;
        }
    });

    if changed { map.sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap()); }

    if current_value > 0.0 && map.len() >= 2 {
        let c = interpolate_color(current_value, map);
        ui.label(
            egui::RichText::new(format!("Current: {current_value:.1} {unit}"))
                .color(egui::Color32::from_rgb(c[0], c[1], c[2])).small().strong(),
        );
    }
    changed
}

// ── Sensor row ────────────────────────────────────────────────────────────────

struct SensorRowData<'a> {
    value_str: String,
    raw_value: f32,
    available: bool,
    entry: &'a mut SensorConfig,
    default_entry: SensorConfig,
}

fn sensor_row(ui: &mut egui::Ui, d: SensorRowData<'_>) -> bool {
    let mut changed = false;

    let val_color = if d.available {
        let c = d.entry.value_color(d.raw_value);
        egui::Color32::from_rgb(c[0], c[1], c[2])
    } else {
        egui::Color32::DARK_GRAY
    };
    let label_color = {
        let [r, g, b] = d.entry.label_color;
        egui::Color32::from_rgb(r, g, b)
    };
    let name_color = if d.entry.enabled { label_color } else { egui::Color32::DARK_GRAY };

    let SensorRowData { value_str, raw_value, available, entry, default_entry } = d;
    let id = ui.make_persistent_id(&entry.id);

    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
        .show_header(ui, |ui| {
            let sq = if available { val_color } else { egui::Color32::DARK_GRAY };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2.0, sq);
            ui.label(egui::RichText::new(&entry.label).color(name_color));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let txt = if available {
                    egui::RichText::new(&value_str).color(val_color).strong()
                } else {
                    egui::RichText::new("N/A").color(egui::Color32::DARK_GRAY)
                };
                ui.label(txt);
            });
        })
        .body(|ui| {
            ui.horizontal(|ui| {
                if ui.checkbox(&mut entry.enabled, "Enabled").changed() { changed = true; }
                let mut f = entry.label_color_f32();
                ui.label("Color:");
                if egui::color_picker::color_edit_button_rgb(ui, &mut f).changed() {
                    entry.set_label_from_f32(f);
                    changed = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("↺ Reset").on_hover_text("Reset to defaults").clicked() {
                        *entry = default_entry.clone();
                        changed = true;
                    }
                });
            });

            // Custom label editor
            ui.horizontal(|ui| {
                ui.label("Label:");
                if ui.text_edit_singleline(&mut entry.label).changed() { changed = true; }
            });

            ui.add_space(4.0);
            ui.label(egui::RichText::new("Value thresholds:").small().color(egui::Color32::GRAY));
            if edit_color_map(ui, &mut entry.color_map, &entry.unit.clone(), raw_value) {
                changed = true;
            }
        });

    changed
}

// ── App::update ───────────────────────────────────────────────────────────────

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        if self.preview_texture.is_none()
            || self.last_preview.elapsed() > Duration::from_millis(800)
            || self.config_dirty
        {
            self.update_preview(ctx);
            self.last_preview = Instant::now();
        }
        if self.config_dirty { self.save_config(); }

        // ── LEFT: settings ──
        egui::SidePanel::left("settings")
            .exact_width(400.0)
            .resizable(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);

                    // Daemon + Autostart management
                    let running = self.daemon_running;
                    ui.horizontal(|ui| {
                        let color = if running {
                            egui::Color32::from_rgb(80, 220, 80)
                        } else {
                            egui::Color32::from_rgb(160, 80, 80)
                        };
                        let label = if running { "● Daemon: running" } else { "● Daemon: stopped" };
                        ui.label(egui::RichText::new(label).color(color).small());
                        if running {
                            if ui.small_button("■ Stop").clicked() {
                                self.service_manager.stop();
                                self.daemon_running = false;
                            }
                            if ui.small_button("↻ Restart").clicked() {
                                self.service_manager.restart(&daemon_binary_path());
                            }
                        } else if ui.small_button("▶ Start").clicked() {
                            self.service_manager.start(&daemon_binary_path());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Reset all").on_hover_text("Reset entire config to defaults").clicked() {
                                self.config = Config::default();
                                self.config_dirty = true;
                            }
                        });
                    });
                    ui.label(egui::RichText::new(format!(
                        "Service manager: {}", self.service_manager.kind().name()
                    )).small().color(egui::Color32::GRAY));
                    if self.service_manager.kind().supports_autostart() {
                    ui.horizontal(|ui| {
                        let (label, color) = if self.autostart_enabled {
                            ("● Autostart: enabled", egui::Color32::from_rgb(80, 220, 80))
                        } else {
                            ("○ Autostart: disabled", egui::Color32::DARK_GRAY)
                        };
                        ui.label(egui::RichText::new(label).color(color).small());
                        if self.autostart_enabled {
                            if ui.small_button("Disable").clicked() {
                                self.service_manager.disable_autostart();
                                self.autostart_enabled = self.service_manager.autostart_enabled();
                            }
                        } else if ui.small_button("Enable").clicked() {
                            self.service_manager.enable_autostart(&daemon_binary_path());
                            self.autostart_enabled = self.service_manager.autostart_enabled();
                        }
                    });
                    } else {
                        ui.label(egui::RichText::new(
                            "Automatic service and autostart management is unavailable."
                        ).small().color(egui::Color32::DARK_GRAY));
                    }
                    ui.separator();
                    ui.add_space(4.0);

                    // ── Section: Screen ────────────────────────────────────────────────
                    ui.group(|ui| {
                        ui.label(egui::RichText::new("Screen").strong());
                        ui.add_space(2.0);
                        if ui.add(
                            egui::Slider::new(&mut self.config.rotation, 0.0..=359.9)
                                .suffix("°").step_by(0.5)
                        ).changed() { self.config_dirty = true; }
                        ui.horizontal(|ui| {
                            for deg in [0.0f32, 90.0, 180.0, 270.0] {
                                if ui.small_button(format!("{deg}°")).clicked() {
                                    self.config.rotation = deg;
                                    self.config_dirty = true;
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);

                    // ── Section: Background ────────────────────────────────────────────
                    ui.group(|ui| {
                        ui.label(egui::RichText::new("Background Image").strong());
                        ui.add_space(2.0);

                        let has_image = self.config.background.image_path.is_some();
                        ui.horizontal(|ui| {
                            let path_str = self.config.background.image_path
                                .as_deref().unwrap_or("(none)");
                            ui.label(egui::RichText::new(path_str)
                                .small()
                                .color(if has_image {
                                    egui::Color32::LIGHT_GRAY
                                } else {
                                    egui::Color32::DARK_GRAY
                                }));
                        });

                        ui.horizontal(|ui| {
                            if ui.button("Browse…").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "webp", "tiff"])
                                    .pick_file()
                                {
                                    self.config.background.image_path =
                                        Some(path.to_string_lossy().into_owned());
                                    self.config_dirty = true;
                                }
                            }
                            if has_image && ui.small_button("Clear").clicked() {
                                self.config.background.image_path = None;
                                self.config_dirty = true;
                            }
                        });

                        if has_image {
                            // Fit mode
                            ui.horizontal(|ui| {
                                ui.label("Fit:");
                                for (label, fit) in [
                                    ("Cover", ImageFit::Cover),
                                    ("Contain", ImageFit::Contain),
                                    ("Stretch", ImageFit::Stretch),
                                ] {
                                    if ui.selectable_label(
                                        self.config.background.fit == fit, label
                                    ).clicked() {
                                        self.config.background.fit = fit;
                                        self.config_dirty = true;
                                    }
                                }
                            });

                            // Overlay alpha
                            ui.horizontal(|ui| {
                                ui.label("Darken:");
                                let mut alpha = self.config.background.overlay_alpha as f32;
                                if ui.add(
                                    egui::Slider::new(&mut alpha, 0.0..=240.0)
                                        .show_value(false)
                                        .step_by(4.0)
                                ).changed() {
                                    self.config.background.overlay_alpha = alpha as u8;
                                    self.config_dirty = true;
                                }
                                ui.label(
                                    egui::RichText::new(
                                        format!("{:.0}%", self.config.background.overlay_alpha as f32 / 255.0 * 100.0)
                                    ).small()
                                );
                            });
                        }
                    });
                    ui.add_space(6.0);

                    // ── Section: Layout ────────────────────────────────────────────────
                    ui.group(|ui| {
                        ui.label(egui::RichText::new("Layout").strong());
                        ui.add_space(2.0);

                        // Preset selector
                        ui.horizontal(|ui| {
                            ui.label("Preset:");
                            for (label, preset) in [
                                ("Classic",  LayoutPreset::Classic),
                                ("Grid 2×3", LayoutPreset::Grid2x3),
                                ("Big Top",  LayoutPreset::BigTop),
                                ("Custom",   LayoutPreset::Custom),
                            ] {
                                if ui.selectable_label(
                                    self.config.layout.preset == preset, label
                                ).clicked() {
                                    self.config.layout.preset = preset;
                                    self.config_dirty = true;
                                }
                            }
                        });

                        // Max visible
                        ui.horizontal(|ui| {
                            ui.label("Max shown:");
                            let mut mv = self.config.layout.max_visible;
                            if ui.add(
                                egui::DragValue::new(&mut mv).range(1..=8)
                            ).changed() {
                                self.config.layout.max_visible = mv;
                                self.config_dirty = true;
                            }
                        });

                        // Custom slot editor
                        if self.config.layout.preset == LayoutPreset::Custom {
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new("Custom positions (0–1 normalized):").small().color(egui::Color32::GRAY));

                            let enabled_ids: Vec<String> = self.config.sensors.iter()
                                .filter(|s| s.enabled)
                                .take(self.config.layout.max_visible)
                                .map(|s| s.id.clone())
                                .collect();

                            // Ensure a slot exists for each enabled sensor
                            for id in &enabled_ids {
                                if !self.config.layout.custom_slots.iter().any(|s| &s.sensor_id == id) {
                                    self.config.layout.custom_slots.push(LayoutSlot {
                                        sensor_id: id.clone(),
                                        value_cx_norm: 0.5, value_cy_norm: 0.45,
                                        label_cx_norm: 0.5, label_cy_norm: 0.52,
                                        value_font_size: 58.0, label_font_size: 27.0,
                                    });
                                }
                            }

                            let mut slot_changed = false;
                            for slot in &mut self.config.layout.custom_slots {
                                if !enabled_ids.contains(&slot.sensor_id) { continue; }
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(&slot.sensor_id).small().strong());
                                });
                                ui.horizontal(|ui| {
                                    ui.label("V cx:"); changed_drag(ui, &mut slot.value_cx_norm, 0.0, 1.0, 0.005, &mut slot_changed);
                                    ui.label("cy:");   changed_drag(ui, &mut slot.value_cy_norm, 0.0, 1.0, 0.005, &mut slot_changed);
                                    ui.label("fs:");   changed_drag(ui, &mut slot.value_font_size, 10.0, 140.0, 1.0, &mut slot_changed);
                                });
                                ui.horizontal(|ui| {
                                    ui.label("L cx:"); changed_drag(ui, &mut slot.label_cx_norm, 0.0, 1.0, 0.005, &mut slot_changed);
                                    ui.label("cy:");   changed_drag(ui, &mut slot.label_cy_norm, 0.0, 1.0, 0.005, &mut slot_changed);
                                    ui.label("fs:");   changed_drag(ui, &mut slot.label_font_size, 8.0, 60.0, 0.5, &mut slot_changed);
                                });
                                ui.add_space(2.0);
                            }
                            if slot_changed { self.config_dirty = true; }
                        }
                    });
                    ui.add_space(6.0);

                    // ── Section: Sensors ───────────────────────────────────────────────
                    let enabled_count = self.config.sensors.iter().filter(|s| s.enabled).count();
                    let max_v = self.config.layout.max_visible;
                    ui.label(egui::RichText::new("Sensors").strong());
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{enabled_count} enabled / {max_v} max shown"))
                                .small()
                                .color(if enabled_count > max_v {
                                    egui::Color32::YELLOW
                                } else {
                                    egui::Color32::GRAY
                                }),
                        );
                        ui.label(egui::RichText::new("— click to expand").small().color(egui::Color32::GRAY));
                    });
                    ui.add_space(4.0);

                    let defs = Config::default();
                    let readings = self.sensor_values.readings.clone();

                    for entry in &mut self.config.sensors {
                        let raw = readings.get(&entry.id).copied().unwrap_or(0.0);
                        let available = readings.contains_key(&entry.id);
                        let val_str = if available {
                            format_value(&entry.unit, raw)
                        } else {
                            "N/A".to_string()
                        };
                        let default_entry = defs.sensor_by_id(&entry.id)
                            .cloned()
                            .unwrap_or_else(|| entry.clone());

                        if sensor_row(ui, SensorRowData {
                            value_str: val_str,
                            raw_value: raw,
                            available,
                            entry,
                            default_entry,
                        }) {
                            self.config_dirty = true;
                        }
                        ui.separator();
                    }

                    ui.add_space(4.0);
                    let apply_msg = if self.daemon_running {
                        "Config saved — daemon picks up changes instantly."
                    } else {
                        "Config saved — start daemon to apply to device."
                    };
                    ui.label(egui::RichText::new(apply_msg).small().color(egui::Color32::GRAY));
                });
            });

        // ── RIGHT: preview ──
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(16.0);
                ui.label(egui::RichText::new("Preview").strong());
                ui.add_space(8.0);

                if let Some(tex) = &self.preview_texture {
                    let size = egui::vec2(300.0, 300.0);
                    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                    ui.painter().image(
                        tex.id(), rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                    ui.painter().circle_stroke(
                        rect.center(), rect.width() / 2.0,
                        egui::Stroke::new(2.0, egui::Color32::from_gray(80)),
                    );
                } else {
                    ui.spinner();
                }

                ui.add_space(12.0);
                if !self.daemon_running {
                    ui.label(
                        egui::RichText::new("Coolant N/A — start daemon to see it.")
                            .small().color(egui::Color32::GRAY),
                    );
                }
            });
        });

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn changed_drag(ui: &mut egui::Ui, val: &mut f32, min: f32, max: f32, speed: f64, changed: &mut bool) {
    if ui.add(egui::DragValue::new(val).range(min..=max).speed(speed).max_decimals(3)).changed() {
        *changed = true;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "gui"))]
mod tests {
    use super::*;
    use config::ColorPoint;

    fn mock_normal() -> SensorValues {
        SensorValues {
            readings: [
                ("cpu_temp", 55.0), ("coolant", 28.5), ("cpu_freq", 3.8),
                ("cpu_util", 42.0), ("cpu_power", 65.0), ("gpu_temp", 62.0),
            ].iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    fn mock_triple_digit() -> SensorValues {
        SensorValues {
            readings: [
                ("cpu_temp", 105.0), ("coolant", 55.0), ("cpu_freq", 5.9),
                ("cpu_util", 99.0), ("cpu_power", 250.0), ("gpu_temp", 112.0),
            ].iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    fn run_ui<F: FnOnce(&mut egui::Ui)>(f: F) {
        let ctx = egui::Context::default();
        let mut f_opt = Some(f);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some(f) = f_opt.take() { f(ui); }
            });
        });
    }

    // ── edit_color_map ────────────────────────────────────────────────────────

    #[test]
    fn edit_color_map_no_input_returns_false() {
        let mut map = vec![
            ColorPoint { value: 45.0, color: [80, 220, 80] },
            ColorPoint { value: 85.0, color: [255, 55, 55] },
        ];
        run_ui(|ui| { assert!(!edit_color_map(ui, &mut map, "°C", 60.0)); });
    }

    #[test]
    fn edit_color_map_one_point_no_remove_button_no_panic() {
        let mut map = vec![ColorPoint { value: 50.0, color: [100, 150, 200] }];
        run_ui(|ui| {
            let _ = edit_color_map(ui, &mut map, "°C", 50.0);
            assert_eq!(map.len(), 1);
        });
    }

    #[test]
    fn edit_color_map_triple_digit_current_value_no_panic() {
        let mut map = vec![
            ColorPoint { value: 45.0, color: [80, 220, 80] },
            ColorPoint { value: 85.0, color: [255, 55, 55] },
        ];
        run_ui(|ui| { let _ = edit_color_map(ui, &mut map, "°C", 105.0); });
    }

    #[test]
    fn edit_color_map_zero_current_value_hides_indicator_no_panic() {
        let mut map = vec![
            ColorPoint { value: 45.0, color: [80, 220, 80] },
            ColorPoint { value: 85.0, color: [255, 55, 55] },
        ];
        run_ui(|ui| { let _ = edit_color_map(ui, &mut map, "°C", 0.0); });
    }

    #[test]
    fn edit_color_map_all_sensor_units_no_panic() {
        for unit in ["°C", "GHz", "%", "W"] {
            let mut map = vec![
                ColorPoint { value: 0.0,   color: [80, 220, 80] },
                ColorPoint { value: 100.0, color: [255, 55, 55] },
            ];
            run_ui(|ui| { let _ = edit_color_map(ui, &mut map, unit, 50.0); });
        }
    }

    // ── sensor_row ────────────────────────────────────────────────────────────

    #[test]
    fn sensor_row_available_normal_value_no_panic() {
        let mut entry = Config::default().sensors[0].clone();
        let def = Config::default().sensors[0].clone();
        run_ui(|ui| {
            sensor_row(ui, SensorRowData {
                value_str: "55°C".to_string(), raw_value: 55.0,
                available: true, entry: &mut entry, default_entry: def,
            });
        });
    }

    #[test]
    fn sensor_row_unavailable_shows_na_no_panic() {
        let mut entry = Config::default().sensors[5].clone(); // gpu_temp
        let def = Config::default().sensors[5].clone();
        run_ui(|ui| {
            sensor_row(ui, SensorRowData {
                value_str: "N/A".to_string(), raw_value: 0.0,
                available: false, entry: &mut entry, default_entry: def,
            });
        });
    }

    #[test]
    fn sensor_row_triple_digit_cpu_temp_no_panic() {
        let mut entry = Config::default().sensors[0].clone();
        let def = Config::default().sensors[0].clone();
        run_ui(|ui| {
            sensor_row(ui, SensorRowData {
                value_str: "105°C".to_string(), raw_value: 105.0,
                available: true, entry: &mut entry, default_entry: def,
            });
        });
    }

    #[test]
    fn sensor_row_disabled_entry_no_panic() {
        let mut entry = Config::default().sensors[0].clone();
        entry.enabled = false;
        let def = Config::default().sensors[0].clone();
        run_ui(|ui| {
            sensor_row(ui, SensorRowData {
                value_str: "55°C".to_string(), raw_value: 55.0,
                available: true, entry: &mut entry, default_entry: def,
            });
        });
    }

    // ── App with mocked sensor values ─────────────────────────────────────────

    #[test]
    fn app_sensor_values_reflects_mock_data() {
        let app = App::new_for_test(mock_triple_digit());
        assert_eq!(app.current_readings().get("cpu_temp").copied().map(|v| v as i32), Some(105));
        assert_eq!(app.current_readings().get("gpu_temp").copied().map(|v| v as i32), Some(112));
    }

    #[test]
    fn app_render_preview_with_normal_mock_no_panic() {
        let mut app = App::new_for_test(mock_normal());
        let img = app.renderer.render_preview(&app.config, &app.sensor_values);
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn app_render_preview_with_triple_digit_mock_no_panic() {
        let mut app = App::new_for_test(mock_triple_digit());
        let img = app.renderer.render_preview(&app.config, &app.sensor_values);
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    // ── daemon management ─────────────────────────────────────────────────────

    #[test]
    fn daemon_binary_path_returns_without_panic() {
        let p = daemon_binary_path();
        assert!(p.file_name().is_some());
    }
}
