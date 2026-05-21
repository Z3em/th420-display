use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::{Rgb, RgbImage, imageops};
use imageproc::drawing::{draw_filled_circle_mut, draw_line_segment_mut, draw_text_mut};
use imageproc::geometric_transformations::{rotate_about_center, Interpolation};
use std::io::Cursor;

use crate::config::{Config, ImageFit};
use crate::sensors::SensorValues;

const FONT_BYTES: &[u8] = include_bytes!("../assets/NotoSans-Bold.ttf");

const W: u32 = 480;
const H: u32 = 480;

const BG:        Rgb<u8> = Rgb([10, 10, 20]);
const CIRCLE_BG: Rgb<u8> = Rgb([15, 15, 28]);
const DIVIDER:   Rgb<u8> = Rgb([45, 45, 68]);

pub struct Renderer {
    font_bytes: Vec<u8>,
    /// Cached background image: (source_path, loaded_480x480_image)
    bg_cache: Option<(String, RgbImage)>,
}

impl Renderer {
    pub fn new() -> Self {
        Self { font_bytes: FONT_BYTES.to_vec(), bg_cache: None }
    }

    /// Render with rotation applied — used for GUI preview.
    #[allow(dead_code)]
    pub fn render_preview(&mut self, config: &Config, v: &SensorValues) -> RgbImage {
        self.render_image(config, v)
    }

    /// Render with rotation applied — used for sending to device.
    pub fn render_image(&mut self, config: &Config, v: &SensorValues) -> RgbImage {
        self.update_bg_cache(config);
        let img = self.render_base(config, v);
        apply_rotation(img, config.rotation)
    }

    /// Encode to JPEG for the device.
    #[allow(dead_code)]
    pub fn render(&mut self, config: &Config, v: &SensorValues) -> Vec<u8> {
        let img = self.render_image(config, v);
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg).expect("jpeg encode failed");
        buf.into_inner()
    }

    fn update_bg_cache(&mut self, config: &Config) {
        let current_path = config.background.image_path.as_deref().unwrap_or("");
        let cached_path = self.bg_cache.as_ref().map(|(p, _)| p.as_str()).unwrap_or("");
        if current_path != cached_path {
            self.bg_cache = config.background.image_path.as_ref().and_then(|path| {
                load_background_image(path, &config.background.fit, config.background.overlay_alpha)
                    .map(|img| (path.clone(), img))
            });
        }
    }

    fn render_base(&self, config: &Config, v: &SensorValues) -> RgbImage {
        let font = FontRef::try_from_slice(&self.font_bytes).expect("invalid font");

        // ── Base layer ────────────────────────────────────────────────────────
        let mut img = if let Some((_, ref bg)) = self.bg_cache {
            bg.clone()
        } else {
            RgbImage::from_pixel(W, H, BG)
        };

        // Only fill the circle with solid color when there is no background image —
        // otherwise the fill would overwrite the loaded image.
        if self.bg_cache.is_none() {
            draw_filled_circle_mut(&mut img, (240, 240), 238, CIRCLE_BG);
        }

        // ── Divider line (only in Classic layout) ────────────────────────────
        if config.layout.preset == crate::config::LayoutPreset::Classic {
            draw_line_segment_mut(&mut img, (68.0, 262.0), (412.0, 262.0), DIVIDER);
        }

        // ── Sensors ───────────────────────────────────────────────────────────
        let enabled_ids: Vec<String> = config.enabled_sensor_ids();
        let id_refs: Vec<&str> = enabled_ids.iter().map(|s| s.as_str()).collect();
        let slots = config.layout.preset_slots(&id_refs);

        for slot in &slots {
            let Some(sc) = config.sensor_by_id(&slot.sensor_id) else { continue };
            if !sc.enabled { continue; }
            let Some(&raw) = v.readings.get(&slot.sensor_id) else { continue };

            let vc = Rgb(sc.value_color(raw));
            let lc = Rgb(sc.label_color);
            let val_str = format_value(&sc.unit, raw);

            draw_centered(&mut img, &font, &val_str, slot.value_cx, slot.value_y, slot.value_fs, vc);
            draw_centered(&mut img, &font, &sc.label, slot.label_cx, slot.label_y, slot.label_fs, lc);
        }

        img
    }
}

// ── Background image loading ──────────────────────────────────────────────────

fn load_background_image(path: &str, fit: &ImageFit, overlay_alpha: u8) -> Option<RgbImage> {
    let src = image::open(path).ok()?.into_rgb8();
    let (sw, sh) = (src.width(), src.height());

    let resized: RgbImage = match fit {
        ImageFit::Cover => {
            let scale = (W as f32 / sw as f32).max(H as f32 / sh as f32);
            let nw = (sw as f32 * scale) as u32;
            let nh = (sh as f32 * scale) as u32;
            let scaled = imageops::resize(&src, nw, nh, imageops::FilterType::Lanczos3);
            let ox = (nw.saturating_sub(W)) / 2;
            let oy = (nh.saturating_sub(H)) / 2;
            imageops::crop_imm(&scaled, ox, oy, W, H).to_image()
        }
        ImageFit::Contain => {
            let scale = (W as f32 / sw as f32).min(H as f32 / sh as f32);
            let nw = (sw as f32 * scale) as u32;
            let nh = (sh as f32 * scale) as u32;
            let scaled = imageops::resize(&src, nw, nh, imageops::FilterType::Lanczos3);
            let mut canvas = RgbImage::from_pixel(W, H, BG);
            let ox = (W.saturating_sub(nw)) / 2;
            let oy = (H.saturating_sub(nh)) / 2;
            imageops::overlay(&mut canvas, &scaled, ox as i64, oy as i64);
            canvas
        }
        ImageFit::Stretch => {
            imageops::resize(&src, W, H, imageops::FilterType::Lanczos3)
        }
    };

    let mut out = resized;
    if overlay_alpha > 0 {
        let factor = (255 - overlay_alpha as u16) as f32 / 255.0;
        for p in out.pixels_mut() {
            p[0] = (p[0] as f32 * factor) as u8;
            p[1] = (p[1] as f32 * factor) as u8;
            p[2] = (p[2] as f32 * factor) as u8;
        }
    }
    Some(out)
}

// ── Value formatting ──────────────────────────────────────────────────────────

pub fn format_value(unit: &str, value: f32) -> String {
    match unit {
        "°C"  => format!("{:.0}°C", value),
        "%"   => format!("{:.0}%", value),
        "GHz" => format!("{:.2}G", value),
        "W"   => format!("{:.0}W", value.min(999.0)),
        "GB"  => format!("{:.1}G", value),
        _     => format!("{:.1}", value),
    }
}

// ── Geometry helpers ──────────────────────────────────────────────────────────

fn apply_rotation(img: RgbImage, degrees: f32) -> RgbImage {
    if degrees == 0.0 { return img; }
    rotate_about_center(&img, degrees.to_radians(), Interpolation::Bilinear, Rgb([0, 0, 0]))
}

fn draw_centered(img: &mut RgbImage, font: &FontRef, text: &str,
                 cx: i32, y_top: i32, size: f32, color: Rgb<u8>) {
    let scale = PxScale::from(size);
    let width = measure_width(font, scale, text);
    let x = cx - (width / 2.0) as i32;
    draw_text_mut(img, color, x, y_top, scale, font, text);
}

fn measure_width(font: &FontRef, scale: PxScale, text: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    text.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LayoutPreset};
    use std::collections::HashMap;

    fn make_values(pairs: &[(&str, f32)]) -> SensorValues {
        SensorValues { readings: pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect() }
    }

    fn normal() -> SensorValues {
        make_values(&[
            ("cpu_temp", 55.0), ("coolant", 28.5), ("cpu_freq", 3.8),
            ("cpu_util", 42.0), ("cpu_power", 65.0), ("gpu_temp", 62.0),
        ])
    }

    fn triple_digit() -> SensorValues {
        make_values(&[
            ("cpu_temp", 105.0), ("coolant", 55.0), ("cpu_freq", 5.9),
            ("cpu_util", 99.0), ("cpu_power", 250.0), ("gpu_temp", 112.0),
        ])
    }

    fn with_extended() -> SensorValues {
        let mut v = normal();
        v.readings.insert("gpu_util".to_string(), 85.0);
        v.readings.insert("gpu_power".to_string(), 180.0);
        v.readings.insert("gpu_vram_pct".to_string(), 60.0);
        v.readings.insert("ram_used_pct".to_string(), 45.0);
        v.readings.insert("nvme0_temp".to_string(), 44.0);
        v.readings.insert("dimm0_temp".to_string(), 38.0);
        v
    }

    // ── render_preview ────────────────────────────────────────────────────────

    #[test]
    fn render_normal_values_produces_480x480() {
        let img = Renderer::new().render_preview(&Config::default(), &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_triple_digit_temps_no_panic() {
        let img = Renderer::new().render_preview(&Config::default(), &triple_digit());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_empty_readings_no_panic() {
        let img = Renderer::new().render_preview(&Config::default(),
            &SensorValues { readings: HashMap::new() });
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_all_sensors_disabled_no_panic() {
        let mut config = Config::default();
        for s in &mut config.sensors { s.enabled = false; }
        let img = Renderer::new().render_preview(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_negative_sensor_values_no_panic() {
        let img = Renderer::new().render_preview(&Config::default(),
            &make_values(&[("cpu_temp", -5.0), ("gpu_temp", -3.0), ("cpu_power", -10.0)]));
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_extended_sensors_enabled_no_panic() {
        let mut config = Config::default();
        for s in &mut config.sensors { s.enabled = true; }
        config.layout.max_visible = 8;
        let img = Renderer::new().render_preview(&config, &with_extended());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    // NOTE: render_preview now takes &mut self — temporary is auto-reborrowed as mut

    // ── render_image (with rotation) ─────────────────────────────────────────

    #[test]
    fn render_image_rotation_0_no_panic() {
        let mut config = Config::default();
        config.rotation = 0.0;
        let img = Renderer::new().render_image(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_image_rotation_180_no_panic() {
        let mut config = Config::default();
        config.rotation = 180.0;
        let img = Renderer::new().render_image(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    // ── layout presets ────────────────────────────────────────────────────────

    #[test]
    fn render_grid2x3_preset_no_panic() {
        let mut config = Config::default();
        config.layout.preset = LayoutPreset::Grid2x3;
        let img = Renderer::new().render_preview(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_bigtop_preset_no_panic() {
        let mut config = Config::default();
        config.layout.preset = LayoutPreset::BigTop;
        let img = Renderer::new().render_preview(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_preview_matches_render_image() {
        let mut config = Config::default();
        config.rotation = 90.0;
        let mut r = Renderer::new();
        let preview = r.render_preview(&config, &normal());
        let device = r.render_image(&config, &normal());
        assert!(preview.pixels().zip(device.pixels()).all(|(a, b)| a == b));
    }

    #[test]
    fn render_preview_rotation_90_differs_from_0() {
        let mut config = Config::default();
        let base = Renderer::new().render_preview(&config, &normal());
        config.rotation = 90.0;
        let rotated = Renderer::new().render_preview(&config, &normal());
        assert!(!base.pixels().zip(rotated.pixels()).all(|(a, b)| a == b));
    }

    // ── format_value ─────────────────────────────────────────────────────────

    #[test]
    fn format_value_celsius() {
        assert_eq!(format_value("°C", 55.6), "56°C");
        assert_eq!(format_value("°C", 105.0), "105°C");
    }

    #[test]
    fn format_value_percent() {
        assert_eq!(format_value("%", 42.7), "43%");
    }

    #[test]
    fn format_value_ghz() {
        assert_eq!(format_value("GHz", 3.8), "3.80G");
    }

    #[test]
    fn format_value_watts_capped_at_999() {
        assert_eq!(format_value("W", 9999.0), "999W");
        assert_eq!(format_value("W", 65.0),   "65W");
    }

    // ── background image loading ──────────────────────────────────────────────

    #[test]
    fn load_background_cover_produces_480x480() {
        let tmp = std::env::temp_dir().join("th420_bg_test.png");
        image::RgbImage::from_pixel(800, 600, Rgb([100, 150, 200]))
            .save(&tmp).unwrap();
        let img = load_background_image(tmp.to_str().unwrap(), &ImageFit::Cover, 0).unwrap();
        assert_eq!((img.width(), img.height()), (480, 480));
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn load_background_contain_produces_480x480() {
        let tmp = std::env::temp_dir().join("th420_bg_contain.png");
        image::RgbImage::from_pixel(300, 300, Rgb([50, 100, 150]))
            .save(&tmp).unwrap();
        let img = load_background_image(tmp.to_str().unwrap(), &ImageFit::Contain, 128).unwrap();
        assert_eq!((img.width(), img.height()), (480, 480));
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn load_background_invalid_path_returns_none() {
        assert!(load_background_image("/nonexistent/file.png", &ImageFit::Cover, 0).is_none());
    }

    #[test]
    fn render_with_background_image_no_panic() {
        let tmp = std::env::temp_dir().join("th420_bg_render.png");
        image::RgbImage::from_pixel(480, 480, Rgb([30, 30, 40]))
            .save(&tmp).unwrap();
        let mut config = Config::default();
        config.background.image_path = Some(tmp.to_str().unwrap().to_string());
        // update_bg_cache is called only by render_image, so call it manually here
        let mut r = Renderer::new();
        r.update_bg_cache(&config);
        let img = r.render_preview(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
        let _ = std::fs::remove_file(tmp);
    }
}
