use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

// ── Color primitives ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ColorPoint {
    pub value: f32,
    pub color: [u8; 3],
}

/// Linear interpolation between color_map anchor points.
pub fn interpolate_color(value: f32, map: &[ColorPoint]) -> [u8; 3] {
    if map.is_empty() { return [200, 200, 200]; }
    if map.len() == 1 || value <= map[0].value { return map[0].color; }
    if value >= map.last().unwrap().value { return map.last().unwrap().color; }
    for w in map.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if value >= a.value && value <= b.value {
            let t = (value - a.value) / (b.value - a.value);
            return lerp_color(a.color, b.color, t.clamp(0.0, 1.0));
        }
    }
    map.last().unwrap().color
}

fn lerp_color(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t).round() as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t).round() as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t).round() as u8,
    ]
}

#[allow(dead_code)]
pub fn to_f32(c: [u8; 3]) -> [f32; 3] {
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0]
}
#[allow(dead_code)]
pub fn from_f32(c: [f32; 3]) -> [u8; 3] {
    [(c[0] * 255.0).clamp(0.0, 255.0) as u8,
     (c[1] * 255.0).clamp(0.0, 255.0) as u8,
     (c[2] * 255.0).clamp(0.0, 255.0) as u8]
}

// ── SensorConfig ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SensorConfig {
    pub id: String,
    pub enabled: bool,
    pub label: String,
    pub unit: String,
    pub label_color: [u8; 3],
    pub color_map: Vec<ColorPoint>,
}

impl SensorConfig {
    #[allow(dead_code)]
    pub fn label_color_f32(&self) -> [f32; 3] { to_f32(self.label_color) }
    #[allow(dead_code)]
    pub fn set_label_from_f32(&mut self, c: [f32; 3]) { self.label_color = from_f32(c); }
    pub fn value_color(&self, value: f32) -> [u8; 3] {
        interpolate_color(value, &self.color_map)
    }
}

// ── BackgroundConfig ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ImageFit {
    #[default]
    Cover,
    Contain,
    Stretch,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BackgroundConfig {
    pub image_path: Option<String>,
    pub fit: ImageFit,
    pub overlay_alpha: u8,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self { image_path: None, fit: ImageFit::Cover, overlay_alpha: 80 }
    }
}

// ── LayoutConfig ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutPreset {
    Classic,
    Grid2x3,
    BigTop,
    Custom,
}

impl Default for LayoutPreset {
    fn default() -> Self { Self::Classic }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LayoutSlot {
    pub sensor_id: String,
    pub value_cx_norm: f32,
    pub value_cy_norm: f32,
    pub label_cx_norm: f32,
    pub label_cy_norm: f32,
    pub value_font_size: f32,
    pub label_font_size: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LayoutConfig {
    pub preset: LayoutPreset,
    pub max_visible: usize,
    pub custom_slots: Vec<LayoutSlot>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self { preset: LayoutPreset::Classic, max_visible: 6, custom_slots: vec![] }
    }
}

/// Concrete slot with pixel coordinates, ready for the renderer.
#[derive(Clone, Debug)]
pub struct ResolvedSlot {
    pub sensor_id: String,
    pub value_cx: i32,
    pub value_y: i32,
    pub value_fs: f32,
    pub label_cx: i32,
    pub label_y: i32,
    pub label_fs: f32,
}

impl LayoutConfig {
    /// Returns rendering slots for the given enabled sensor IDs (in order),
    /// capped at `max_visible`.
    pub fn preset_slots(&self, enabled_ids: &[&str]) -> Vec<ResolvedSlot> {
        let take = enabled_ids.len().min(self.max_visible);
        let ids = &enabled_ids[..take];
        match self.preset {
            LayoutPreset::Classic  => classic_slots(ids),
            LayoutPreset::Grid2x3  => grid_slots(ids, 2, 3),
            LayoutPreset::BigTop   => big_top_slots(ids),
            LayoutPreset::Custom   => custom_slots_resolved(ids, &self.custom_slots),
        }
    }
}

// Classic: exact pixel positions from the original renderer.
fn classic_slots(ids: &[&str]) -> Vec<ResolvedSlot> {
    const P: [(i32, i32, f32, i32, i32, f32); 6] = [
        (240,  83, 118.0, 240,  35,  46.0),  // top center
        (240, 210,  44.0, 240, 196,  22.0),  // middle center (coolant)
        (145, 272,  58.0, 145, 334,  27.0),  // left mid
        (335, 272,  58.0, 335, 334,  27.0),  // right mid
        (145, 368,  58.0, 145, 430,  27.0),  // left bottom
        (335, 368,  58.0, 335, 430,  27.0),  // right bottom
    ];
    ids.iter().enumerate().filter_map(|(i, &id)| {
        P.get(i).map(|&(vc, vy, vf, lc, ly, lf)| ResolvedSlot {
            sensor_id: id.to_string(),
            value_cx: vc, value_y: vy, value_fs: vf,
            label_cx: lc, label_y: ly, label_fs: lf,
        })
    }).collect()
}

// Grid: evenly distribute cols × rows within the circular area (r ≈ 200).
fn grid_slots(ids: &[&str], cols: usize, rows: usize) -> Vec<ResolvedSlot> {
    let col_xs: Vec<i32> = match cols {
        1 => vec![240],
        2 => vec![145, 335],
        3 => vec![110, 240, 370],
        _ => (0..cols).map(|i| 90 + (i * (300) / cols.saturating_sub(1).max(1)) as i32).collect(),
    };
    let row_ys: Vec<i32> = match rows {
        1 => vec![210],
        2 => vec![150, 340],
        3 => vec![110, 240, 370],
        4 => vec![90, 190, 300, 400],
        _ => (0..rows).map(|i| 80 + (i * 320 / rows.saturating_sub(1).max(1)) as i32).collect(),
    };
    let value_fs = 46.0f32;
    let label_fs = 22.0f32;
    let label_dy = 36i32;
    ids.iter().enumerate().filter_map(|(i, &id)| {
        let col = i % cols;
        let row = i / cols;
        row_ys.get(row).zip(col_xs.get(col)).map(|(&vy, &cx)| ResolvedSlot {
            sensor_id: id.to_string(),
            value_cx: cx, value_y: vy, value_fs,
            label_cx: cx, label_y: vy + label_dy, label_fs,
        })
    }).collect()
}

// BigTop: first sensor large at top, rest in a 2×2 grid in the lower half.
fn big_top_slots(ids: &[&str]) -> Vec<ResolvedSlot> {
    let mut slots = Vec::new();
    if let Some(&first) = ids.first() {
        slots.push(ResolvedSlot {
            sensor_id: first.to_string(),
            value_cx: 240, value_y: 100, value_fs: 80.0,
            label_cx: 240, label_y:  50, label_fs: 36.0,
        });
    }
    if ids.len() > 1 {
        let rest: Vec<&str> = ids[1..].to_vec();
        let mut sub = grid_slots(&rest, 2, 2);
        for s in &mut sub { s.value_y += 140; s.label_y += 140; }
        slots.extend(sub);
    }
    slots
}

// Custom: convert normalized positions to pixels.
fn custom_slots_resolved(ids: &[&str], custom: &[LayoutSlot]) -> Vec<ResolvedSlot> {
    ids.iter().filter_map(|&id| {
        custom.iter().find(|s| s.sensor_id == id).map(|s| ResolvedSlot {
            sensor_id: id.to_string(),
            value_cx: (s.value_cx_norm * 480.0) as i32,
            value_y:  (s.value_cy_norm * 480.0) as i32,
            value_fs:  s.value_font_size,
            label_cx: (s.label_cx_norm * 480.0) as i32,
            label_y:  (s.label_cy_norm * 480.0) as i32,
            label_fs:  s.label_font_size,
        })
    }).collect()
}

// ── Default color maps ────────────────────────────────────────────────────────

fn temp_map(warm: f32, hot: f32) -> Vec<ColorPoint> {
    vec![
        ColorPoint { value: warm - 20.0, color: [80, 220, 80] },
        ColorPoint { value: warm,        color: [255, 200, 0] },
        ColorPoint { value: hot,         color: [255, 55, 55] },
    ]
}

fn pct_map() -> Vec<ColorPoint> {
    vec![
        ColorPoint { value: 20.0, color: [80, 220, 80] },
        ColorPoint { value: 60.0, color: [255, 200, 0] },
        ColorPoint { value: 90.0, color: [255, 55, 55] },
    ]
}

fn power_map(mid: f32, high: f32) -> Vec<ColorPoint> {
    vec![
        ColorPoint { value: mid * 0.4, color: [80, 220, 80] },
        ColorPoint { value: mid,       color: [255, 200, 0] },
        ColorPoint { value: high,      color: [255, 55, 55] },
    ]
}

fn make_sensor(id: &str, label: &str, unit: &str, enabled: bool, map: Vec<ColorPoint>) -> SensorConfig {
    SensorConfig {
        id: id.to_string(),
        enabled,
        label: label.to_string(),
        unit: unit.to_string(),
        label_color: [150, 150, 185],
        color_map: map,
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub rotation: f32,
    pub background: BackgroundConfig,
    pub layout: LayoutConfig,
    pub sensors: Vec<SensorConfig>,
}

/// Extract a short CPU model string from /proc/cpuinfo, e.g. "Ryzen 7 7800X3D".
fn default_cpu_label() -> String {
    let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") else {
        return "CPU".to_string();
    };
    for line in cpuinfo.lines() {
        let Some(rest) = line.strip_prefix("model name") else { continue };
        let name = rest.trim_start_matches(|c: char| c == ' ' || c == ':');
        let words: Vec<&str> = name.split_whitespace().collect();
        // Find the first brand keyword and take up to 4 words from there
        let anchors = ["Ryzen", "Core", "EPYC", "Threadripper", "Xeon", "Athlon", "Celeron"];
        if let Some(pos) = words.iter().position(|&w| anchors.contains(&w)) {
            return words[pos..].iter().take(4).copied().collect::<Vec<_>>().join(" ");
        }
        // Fallback: last 3 tokens
        if words.len() >= 3 {
            return words[words.len() - 3..].join(" ");
        }
        return words.join(" ");
    }
    "CPU".to_string()
}

impl Default for Config {
    fn default() -> Self {
        let cpu_label = default_cpu_label();
        Self {
            rotation: 180.0,
            background: BackgroundConfig::default(),
            layout: LayoutConfig::default(),
            sensors: vec![
                // --- original 6, enabled by default ---
                make_sensor("cpu_temp",    &cpu_label,   "°C",  true,  temp_map(65.0, 85.0)),
                make_sensor("coolant",     "COOLANT",    "°C",  true,  vec![
                    ColorPoint { value: 25.0, color: [80, 200, 255] },
                    ColorPoint { value: 35.0, color: [255, 200, 0] },
                    ColorPoint { value: 45.0, color: [255, 55, 55] },
                ]),
                make_sensor("cpu_freq",    "CPU FREQ",   "GHz", true,  vec![
                    ColorPoint { value: 2.0, color: [80, 160, 255] },
                    ColorPoint { value: 4.0, color: [200, 200, 220] },
                    ColorPoint { value: 5.5, color: [255, 220, 80] },
                ]),
                make_sensor("cpu_util",    "CPU UTIL",   "%",   true,  pct_map()),
                make_sensor("cpu_power",   "CPU PWR",    "W",   true,  power_map(90.0, 150.0)),
                make_sensor("gpu_temp",    "GPU TEMP",   "°C",  true,  temp_map(70.0, 90.0)),
                // --- new sensors, disabled by default ---
                make_sensor("gpu_util",        "GPU UTIL",   "%",   false, pct_map()),
                make_sensor("gpu_power",       "GPU PWR",    "W",   false, power_map(100.0, 200.0)),
                make_sensor("gpu_vram_pct",    "VRAM",       "%",   false, pct_map()),
                make_sensor("gpu_hotspot_temp","GPU HOT",    "°C",  false, temp_map(80.0, 100.0)),
                // AMD iGPU (integrated graphics) — visible alongside NVIDIA discrete GPU
                make_sensor("igpu_temp",       "iGPU TEMP",  "°C",  false, temp_map(60.0, 80.0)),
                make_sensor("igpu_util",       "iGPU UTIL",  "%",   false, pct_map()),
                make_sensor("igpu_power",      "iGPU PWR",   "W",   false, power_map(15.0, 35.0)),
                make_sensor("igpu_vram_pct",   "iVRAM",      "%",   false, pct_map()),
                make_sensor("ram_used_pct",    "RAM",        "%",   false, pct_map()),
                make_sensor("nvme0_temp",  "NVMe 0",     "°C",  false, temp_map(50.0, 65.0)),
                make_sensor("nvme1_temp",  "NVMe 1",     "°C",  false, temp_map(50.0, 65.0)),
                make_sensor("nvme2_temp",  "NVMe 2",     "°C",  false, temp_map(50.0, 65.0)),
                make_sensor("dimm0_temp",  "DIMM 0",     "°C",  false, temp_map(40.0, 55.0)),
                make_sensor("dimm1_temp",  "DIMM 1",     "°C",  false, temp_map(40.0, 55.0)),
            ],
        }
    }
}

impl Config {
    /// Load from TOML; on any error fall back to default.
    /// Automatically appends sensors that exist in the default config but not in the
    /// loaded one (forward migration) and saves the result so the GUI sees them too.
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&text).map_err(|e| anyhow::anyhow!(e))?;
        if config.migrate_sensors() {
            let _ = config.save(path);
        }
        Ok(config)
    }

    /// Append any sensors present in `Config::default()` but absent here.
    /// Returns `true` if at least one sensor was added.
    pub fn migrate_sensors(&mut self) -> bool {
        let defaults = Config::default();
        let mut added = false;
        for def in defaults.sensors {
            if !self.sensors.iter().any(|s| s.id == def.id) {
                self.sensors.push(def);
                added = true;
            }
        }
        added
    }

    pub fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Enabled sensor IDs in config order, capped at `layout.max_visible`.
    pub fn enabled_sensor_ids(&self) -> Vec<String> {
        self.sensors.iter()
            .filter(|s| s.enabled)
            .take(self.layout.max_visible)
            .map(|s| s.id.clone())
            .collect()
    }

    pub fn sensor_by_id(&self, id: &str) -> Option<&SensorConfig> {
        self.sensors.iter().find(|s| s.id == id)
    }


}

pub fn default_config_path() -> PathBuf {
    let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push("th420-display");
    p.push("config.toml");
    p
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn three_point_map() -> Vec<ColorPoint> {
        vec![
            ColorPoint { value: 45.0, color: [80, 220, 80] },
            ColorPoint { value: 65.0, color: [255, 200, 0] },
            ColorPoint { value: 85.0, color: [255, 55, 55] },
        ]
    }

    fn test_sensor() -> SensorConfig {
        SensorConfig {
            id: "test".to_string(), enabled: true,
            label: "Test".to_string(), unit: "°C".to_string(),
            label_color: [150, 150, 185], color_map: three_point_map(),
        }
    }

    // ── interpolate_color ─────────────────────────────────────────────────────

    #[test]
    fn interpolate_empty_map_returns_default_gray() {
        assert_eq!(interpolate_color(50.0, &[]), [200, 200, 200]);
    }

    #[test]
    fn interpolate_single_point_always_returns_that_color() {
        let map = vec![ColorPoint { value: 50.0, color: [100, 150, 200] }];
        assert_eq!(interpolate_color(0.0,   &map), [100, 150, 200]);
        assert_eq!(interpolate_color(50.0,  &map), [100, 150, 200]);
        assert_eq!(interpolate_color(999.0, &map), [100, 150, 200]);
    }

    #[test]
    fn interpolate_below_min_clamps_to_first_color() {
        let map = three_point_map();
        assert_eq!(interpolate_color(0.0,   &map), [80, 220, 80]);
        assert_eq!(interpolate_color(-50.0, &map), [80, 220, 80]);
    }

    #[test]
    fn interpolate_above_max_clamps_to_last_color() {
        let map = three_point_map();
        assert_eq!(interpolate_color(100.0, &map), [255, 55, 55]);
        assert_eq!(interpolate_color(150.0, &map), [255, 55, 55]);
        assert_eq!(interpolate_color(999.0, &map), [255, 55, 55]);
    }

    #[test]
    fn interpolate_at_exact_anchors_returns_anchor_color() {
        let map = three_point_map();
        assert_eq!(interpolate_color(45.0, &map), [80, 220, 80]);
        assert_eq!(interpolate_color(65.0, &map), [255, 200, 0]);
        assert_eq!(interpolate_color(85.0, &map), [255, 55, 55]);
    }

    #[test]
    fn interpolate_midpoint_is_between_neighbor_colors() {
        let map = three_point_map();
        let mid = interpolate_color(55.0, &map);
        assert!(mid[0] > 80 && mid[0] < 255, "R={}", mid[0]);
        assert!(mid[1] >= 200 && mid[1] <= 220, "G={}", mid[1]);
        assert!(mid[2] < 80, "B={}", mid[2]);
    }

    // ── to_f32 / from_f32 ─────────────────────────────────────────────────────

    #[test]
    fn to_f32_from_f32_roundtrip_within_one_lsb() {
        for v in [0u8, 1, 64, 127, 128, 200, 254, 255] {
            let orig = [v, v.wrapping_add(13), v.wrapping_add(47)];
            let f = to_f32(orig);
            let back = from_f32(f);
            for i in 0..3 {
                let diff = (orig[i] as i16 - back[i] as i16).abs();
                assert!(diff <= 1, "channel {i}: {} → {}", orig[i], back[i]);
            }
        }
    }

    #[test]
    fn from_f32_clamps_out_of_range_values() {
        assert_eq!(from_f32([2.0, -0.5, 1.0]), [255, 0, 255]);
    }

    // ── SensorConfig ──────────────────────────────────────────────────────────

    #[test]
    fn value_color_at_normal_temp() {
        let sc = test_sensor();
        assert_eq!(sc.value_color(45.0), [80, 220, 80]);
        assert_eq!(sc.value_color(85.0), [255, 55, 55]);
    }

    #[test]
    fn value_color_triple_digit_temp_clamps_to_max_no_panic() {
        let sc = test_sensor();
        assert_eq!(sc.value_color(105.0), [255, 55, 55]);
        assert_eq!(sc.value_color(200.0), [255, 55, 55]);
    }

    #[test]
    fn value_color_zero_and_negative_clamp_to_min_no_panic() {
        let sc = test_sensor();
        assert_eq!(sc.value_color(0.0),  [80, 220, 80]);
        assert_eq!(sc.value_color(-5.0), [80, 220, 80]);
    }

    // ── color_map ordering ────────────────────────────────────────────────────

    #[test]
    fn color_map_sort_preserves_gradient_order() {
        let mut map = vec![
            ColorPoint { value: 85.0, color: [255, 55, 55] },
            ColorPoint { value: 45.0, color: [80, 220, 80] },
            ColorPoint { value: 65.0, color: [255, 200, 0] },
        ];
        map.sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap());
        let values: Vec<f32> = map.iter().map(|p| p.value).collect();
        assert_eq!(values, [45.0, 65.0, 85.0]);
    }

    // ── Config defaults ───────────────────────────────────────────────────────

    #[test]
    fn default_config_color_maps_are_sorted() {
        let cfg = Config::default();
        for sc in &cfg.sensors {
            for w in sc.color_map.windows(2) {
                assert!(w[0].value <= w[1].value,
                    "sensor '{}' map not sorted: {} > {}", sc.id, w[0].value, w[1].value);
            }
        }
    }

    #[test]
    fn default_config_original_sensors_enabled() {
        let cfg = Config::default();
        for id in ["cpu_temp", "coolant", "cpu_freq", "cpu_util", "cpu_power", "gpu_temp"] {
            assert!(
                cfg.sensors.iter().find(|s| s.id == id).map(|s| s.enabled).unwrap_or(false),
                "sensor '{id}' should be enabled by default"
            );
        }
    }

    #[test]
    fn default_config_new_sensors_disabled() {
        let cfg = Config::default();
        for id in ["gpu_util", "gpu_power", "gpu_vram_pct", "gpu_hotspot_temp",
                   "igpu_temp", "igpu_util", "igpu_power", "igpu_vram_pct",
                   "ram_used_pct", "nvme0_temp", "dimm0_temp"] {
            assert!(
                !cfg.sensors.iter().find(|s| s.id == id).map(|s| s.enabled).unwrap_or(true),
                "sensor '{id}' should be disabled by default"
            );
        }
    }

    #[test]
    fn enabled_sensor_ids_respects_max_visible() {
        let mut cfg = Config::default();
        cfg.layout.max_visible = 3;
        let ids = cfg.enabled_sensor_ids();
        assert!(ids.len() <= 3);
    }

    #[test]
    fn sensor_by_id_finds_existing() {
        let cfg = Config::default();
        assert!(cfg.sensor_by_id("cpu_temp").is_some());
        assert!(cfg.sensor_by_id("nonexistent").is_none());
    }

    // ── LayoutConfig::preset_slots ────────────────────────────────────────────

    #[test]
    fn classic_preset_6_sensors_returns_6_slots() {
        let layout = LayoutConfig::default();
        let ids = ["cpu_temp", "coolant", "cpu_freq", "cpu_util", "cpu_power", "gpu_temp"];
        assert_eq!(layout.preset_slots(&ids).len(), 6);
    }

    #[test]
    fn classic_preset_first_slot_matches_original_cpu_temp_position() {
        let layout = LayoutConfig::default();
        let slots = layout.preset_slots(&["cpu_temp", "coolant"]);
        assert_eq!(slots[0].value_cx, 240);
        assert_eq!(slots[0].value_y,   83);
        assert!((slots[0].value_fs - 118.0).abs() < 0.1);
    }

    #[test]
    fn grid2x3_preset_returns_correct_slot_count() {
        let mut layout = LayoutConfig::default();
        layout.preset = LayoutPreset::Grid2x3;
        let ids = ["a", "b", "c", "d", "e", "f"];
        assert_eq!(layout.preset_slots(&ids).len(), 6);
    }

    #[test]
    fn big_top_first_slot_has_larger_font_than_rest() {
        let mut layout = LayoutConfig::default();
        layout.preset = LayoutPreset::BigTop;
        let ids = ["cpu_temp", "gpu_temp", "cpu_util", "cpu_power"];
        let slots = layout.preset_slots(&ids);
        assert!(!slots.is_empty());
        let max_rest = slots[1..].iter().map(|s| s.value_fs as i32).max().unwrap_or(0);
        assert!(slots[0].value_fs as i32 > max_rest);
    }

    #[test]
    fn max_visible_limits_slot_count() {
        let layout = LayoutConfig { preset: LayoutPreset::Grid2x3, max_visible: 2, custom_slots: vec![] };
        let ids = ["a", "b", "c", "d", "e", "f"];
        assert_eq!(layout.preset_slots(&ids).len(), 2);
    }

    #[test]
    fn custom_slots_map_normalized_to_pixels() {
        let layout = LayoutConfig {
            preset: LayoutPreset::Custom,
            max_visible: 1,
            custom_slots: vec![LayoutSlot {
                sensor_id: "x".to_string(),
                value_cx_norm: 0.5, value_cy_norm: 0.5,
                label_cx_norm: 0.5, label_cy_norm: 0.6,
                value_font_size: 60.0, label_font_size: 24.0,
            }],
        };
        let slots = layout.preset_slots(&["x"]);
        assert_eq!(slots[0].value_cx, 240);
        assert_eq!(slots[0].value_y,  240);
        assert_eq!(slots[0].label_y,  288);
    }

    // ── migrate_sensors ───────────────────────────────────────────────────────

    #[test]
    fn migrate_adds_sensors_missing_from_old_config() {
        let mut cfg = Config::default();
        cfg.sensors.retain(|s| s.id != "igpu_temp" && s.id != "gpu_hotspot_temp");
        assert!(cfg.sensor_by_id("igpu_temp").is_none());
        assert!(cfg.sensor_by_id("gpu_hotspot_temp").is_none());

        let migrated = cfg.migrate_sensors();

        assert!(migrated, "should report sensors were added");
        assert!(cfg.sensor_by_id("igpu_temp").is_some());
        assert!(cfg.sensor_by_id("gpu_hotspot_temp").is_some());
    }

    #[test]
    fn migrate_idempotent_on_complete_config() {
        let mut cfg = Config::default();
        let count_before = cfg.sensors.len();
        assert!(!cfg.migrate_sensors(), "complete config needs no migration");
        assert_eq!(cfg.sensors.len(), count_before);
    }

    #[test]
    fn migrate_preserves_existing_sensor_customisation() {
        let mut cfg = Config::default();
        // Simulate a user who renamed cpu_temp and disabled it
        if let Some(s) = cfg.sensors.iter_mut().find(|s| s.id == "cpu_temp") {
            s.label   = "MY CPU".to_string();
            s.enabled = false;
        }
        // Drop a new sensor to force migration
        cfg.sensors.retain(|s| s.id != "igpu_temp");

        cfg.migrate_sensors();

        let cpu = cfg.sensor_by_id("cpu_temp").unwrap();
        assert_eq!(cpu.label, "MY CPU", "existing label must be preserved");
        assert!(!cpu.enabled,           "existing enabled flag must be preserved");
        assert!(cfg.sensor_by_id("igpu_temp").is_some(), "missing sensor must be added");
    }
}
