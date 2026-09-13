use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use nvml_wrapper::Nvml;
use nvml_wrapper_sys::bindings::nvmlGpuThermalSettings_t;

pub struct SensorValues {
    pub readings: HashMap<String, f32>,
}

pub struct SensorReader {
    prev_idle: u64,
    prev_total: u64,
    prev_energy: u64,
    prev_time: Instant,
    // sysfs millidegree temperature files (CPU, NVMe, DIMM; GPU temp for AMD-only systems)
    temp_paths: HashMap<String, String>,
    // Primary GPU (discrete) sysfs paths — used on AMD-only systems; cleared when NVML is present
    gpu_power_path: Option<String>,
    gpu_busy_path: Option<String>,
    vram_used_path: Option<String>,
    vram_total_path: Option<String>,
    // AMD iGPU sysfs paths — always populated when amdgpu hwmon is detected
    igpu_power_path: Option<String>,
    igpu_busy_path: Option<String>,
    igpu_vram_used_path: Option<String>,
    igpu_vram_total_path: Option<String>,
    // CPU energy counter
    zenergy_path: String,
    // NVIDIA — populated when NVML initialises successfully
    nvidia_nvml: Option<Nvml>,
    // GPU T.Limit (HotSpot) via nvidia hwmon temp2, if driver exposes it
    nvidia_hotspot_path: Option<String>,
}

impl SensorReader {
    pub fn new() -> Self {
        let mut temp_paths: HashMap<String, String> = HashMap::new();
        let mut gpu_power_path = None;
        let mut gpu_busy_path = None;
        let mut vram_used_path = None;
        let mut vram_total_path = None;
        let mut igpu_power_path = None;
        let mut igpu_busy_path = None;
        let mut igpu_vram_used_path = None;
        let mut igpu_vram_total_path = None;
        let mut nvidia_hotspot_path = None;
        let mut nvme_idx = 0usize;
        let mut dimm_idx = 0usize;

        if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
            let mut sorted: Vec<_> = entries.flatten().collect();
            sorted.sort_by_key(|e| e.file_name());
            for entry in sorted {
                let hwmon = entry.path();
                let name = fs::read_to_string(hwmon.join("name"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match name.as_str() {
                    "k10temp" => {
                        maybe_insert(&mut temp_paths, "cpu_temp", &hwmon, "temp1_input");
                    }
                    "amdgpu" => {
                        // Always store iGPU paths under igpu_* keys so they can be
                        // displayed alongside NVIDIA when both GPUs are present.
                        // On AMD-only systems (no NVML) these are also copied to gpu_* below.
                        maybe_insert(&mut temp_paths, "igpu_temp", &hwmon, "temp1_input");
                        igpu_power_path = probe(&hwmon, "power1_input");
                        if let Ok(real) = fs::canonicalize(&hwmon) {
                            if let Some(dev) = real.parent().and_then(|p| p.parent()) {
                                let bp = dev.join("gpu_busy_percent");
                                let up = dev.join("mem_info_vram_used");
                                let tp = dev.join("mem_info_vram_total");
                                if bp.exists() {
                                    igpu_busy_path = Some(bp.to_string_lossy().into_owned());
                                }
                                if up.exists() && tp.exists() {
                                    igpu_vram_used_path = Some(up.to_string_lossy().into_owned());
                                    igpu_vram_total_path = Some(tp.to_string_lossy().into_owned());
                                }
                            }
                        }
                    }
                    // NVIDIA driver 520+ / open kernel module: temp1=core, temp2=T.Limit, power1=draw
                    "nvidia" => {
                        maybe_insert(&mut temp_paths, "gpu_temp", &hwmon, "temp1_input");
                        nvidia_hotspot_path = probe(&hwmon, "temp2_input");
                        if let Some(p) = probe(&hwmon, "power1_input") {
                            gpu_power_path = Some(p);
                        }
                    }
                    "nvme" => {
                        if let Some(p) = probe(&hwmon, "temp1_input") {
                            temp_paths.insert(format!("nvme{nvme_idx}_temp"), p);
                            nvme_idx += 1;
                        }
                    }
                    "spd5118" => {
                        if let Some(p) = probe(&hwmon, "temp1_input") {
                            temp_paths.insert(format!("dimm{dimm_idx}_temp"), p);
                            dimm_idx += 1;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Try NVML for discrete NVIDIA GPU metrics.
        // When NVML is absent (AMD-only), promote the iGPU paths to the primary gpu_* keys.
        let nvidia_nvml = match Nvml::init() {
            Ok(nvml) => {
                // Discrete NVIDIA GPU confirmed — NVML owns gpu_* keys.
                // iGPU paths stay in igpu_* only.
                temp_paths.remove("gpu_temp"); // NVML will provide this
                eprintln!(
                    "Sensors: NVIDIA NVML ready (driver {})",
                    nvml.sys_driver_version().unwrap_or_default()
                );
                Some(nvml)
            }
            Err(e) => {
                // No discrete GPU — promote iGPU to primary gpu_* keys so existing
                // sensor IDs in user configs continue to work on AMD-only systems.
                if let Some(p) = temp_paths.get("igpu_temp").cloned() {
                    temp_paths.insert("gpu_temp".to_string(), p);
                }
                gpu_busy_path = igpu_busy_path.clone();
                vram_used_path = igpu_vram_used_path.clone();
                vram_total_path = igpu_vram_total_path.clone();
                gpu_power_path = igpu_power_path.clone();
                eprintln!("Sensors: NVML unavailable ({e}), AMD iGPU as primary GPU");
                None
            }
        };

        let zenergy_path = find_zenergy_socket()
            .unwrap_or_else(|| "/sys/class/hwmon/hwmon8/energy9_input".to_string());

        let (idle, total) = read_proc_stat().unwrap_or((0, 1));
        let energy = read_u64(&zenergy_path).unwrap_or(0);

        eprintln!(
            "Sensors: temps={:?} nvidia={} igpu_busy={:?}",
            temp_paths.keys().collect::<Vec<_>>(),
            nvidia_nvml.is_some(),
            igpu_busy_path,
        );

        Self {
            prev_idle: idle,
            prev_total: total,
            prev_energy: energy,
            prev_time: Instant::now(),
            temp_paths,
            gpu_power_path,
            gpu_busy_path,
            vram_used_path,
            vram_total_path,
            igpu_power_path,
            igpu_busy_path,
            igpu_vram_used_path,
            igpu_vram_total_path,
            zenergy_path,
            nvidia_nvml,
            nvidia_hotspot_path,
        }
    }

    pub fn read(&mut self) -> SensorValues {
        let mut r: HashMap<String, f32> = HashMap::new();

        // CPU, NVMe, DIMM temps; iGPU temp (igpu_temp key); GPU temp on AMD-only (gpu_temp key)
        for (id, path) in &self.temp_paths {
            if let Some(v) = read_millidegree(path) {
                r.insert(id.clone(), v as f32);
            }
        }

        // CPU frequency (GHz)
        r.insert("cpu_freq".to_string(), read_cpu_freq_avg());

        // CPU utilization (%)
        let (idle, total) = read_proc_stat().unwrap_or((self.prev_idle, self.prev_total));
        let d_total = total.saturating_sub(self.prev_total);
        let d_idle = idle.saturating_sub(self.prev_idle);
        let cpu_util = d_total
            .saturating_sub(d_idle)
            .saturating_mul(100)
            .checked_div(d_total)
            .unwrap_or(0);
        r.insert("cpu_util".to_string(), cpu_util.min(100) as f32);
        self.prev_idle = idle;
        self.prev_total = total;

        // CPU power (W) via energy counter delta
        let energy = read_u64(&self.zenergy_path).unwrap_or(self.prev_energy);
        let elapsed = self.prev_time.elapsed().as_secs_f64();
        if elapsed > 0.05 {
            let delta = energy.wrapping_sub(self.prev_energy) as f64;
            let power = (delta / elapsed / 1_000_000.0).max(0.0) as f32;
            r.insert("cpu_power".to_string(), power);
        }
        self.prev_energy = energy;
        self.prev_time = Instant::now();

        // ── AMD iGPU ──────────────────────────────────────────────────────────
        if let Some(ref p) = self.igpu_busy_path {
            if let Some(v) = read_u64(p) {
                r.insert("igpu_util".to_string(), v as f32);
            }
        }
        if let Some(ref p) = self.igpu_power_path {
            if let Some(v) = read_u64(p) {
                r.insert("igpu_power".to_string(), v as f32 / 1_000_000.0);
            }
        }
        if let (Some(ref up), Some(ref tp)) =
            (&self.igpu_vram_used_path, &self.igpu_vram_total_path)
        {
            if let (Some(used), Some(total)) = (read_u64(up), read_u64(tp)) {
                if total > 0 {
                    r.insert(
                        "igpu_vram_pct".to_string(),
                        used as f32 / total as f32 * 100.0,
                    );
                }
            }
        }

        // ── Discrete NVIDIA GPU via NVML ──────────────────────────────────────
        if let Some(ref nvml) = self.nvidia_nvml {
            if let Ok(device) = nvml.device_by_index(0) {
                if let Ok(t) = device.temperature(TemperatureSensor::Gpu) {
                    r.insert("gpu_temp".to_string(), t as f32);
                }
                // GPU T.Limit (HotSpot) via nvmlDeviceGetThermalSettings
                unsafe {
                    let mut ts: nvmlGpuThermalSettings_t = std::mem::zeroed();
                    let sym = nvml.lib().nvmlDeviceGetThermalSettings.as_ref();
                    if let Ok(f) = sym {
                        let raw_handle = device.handle();
                        if f(raw_handle, 0, &mut ts) == 0 && ts.count > 0 {
                            let hot = ts.sensor[0].currentTemp;
                            if hot > 0 {
                                r.insert("gpu_hotspot_temp".to_string(), hot as f32);
                            }
                        }
                    }
                }
                if let Ok(rates) = device.utilization_rates() {
                    r.insert("gpu_util".to_string(), rates.gpu as f32);
                }
                if let Ok(mem) = device.memory_info() {
                    if mem.total > 0 {
                        r.insert(
                            "gpu_vram_pct".to_string(),
                            mem.used as f32 / mem.total as f32 * 100.0,
                        );
                    }
                }
                if let Ok(mw) = device.power_usage() {
                    r.insert("gpu_power".to_string(), mw as f32 / 1000.0);
                }
            }
        }

        // Hotspot via nvidia hwmon temp2 (NVIDIA open kernel module / driver 520+)
        if let Some(ref p) = self.nvidia_hotspot_path {
            if let Some(v) = read_millidegree(p) {
                r.insert("gpu_hotspot_temp".to_string(), v as f32);
            }
        }

        // ── AMD-only fallback: primary GPU keys from iGPU sysfs ───────────────
        // On AMD-only systems these paths mirror the igpu_* paths set during init.
        if let Some(ref p) = self.gpu_busy_path {
            if let Some(v) = read_u64(p) {
                r.insert("gpu_util".to_string(), v as f32);
            }
        }
        if let Some(ref p) = self.gpu_power_path {
            if let Some(v) = read_u64(p) {
                r.insert("gpu_power".to_string(), v as f32 / 1_000_000.0);
            }
        }
        if let (Some(ref up), Some(ref tp)) = (&self.vram_used_path, &self.vram_total_path) {
            if let (Some(used), Some(total)) = (read_u64(up), read_u64(tp)) {
                if total > 0 {
                    r.insert(
                        "gpu_vram_pct".to_string(),
                        used as f32 / total as f32 * 100.0,
                    );
                }
            }
        }

        // RAM usage (%)
        if let Some(pct) = read_ram_used_pct() {
            r.insert("ram_used_pct".to_string(), pct);
        }

        SensorValues { readings: r }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn probe(hwmon: &Path, file: &str) -> Option<String> {
    let p = hwmon.join(file);
    if p.exists() {
        Some(p.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn maybe_insert(map: &mut HashMap<String, String>, id: &str, hwmon: &Path, file: &str) {
    if let Some(p) = probe(hwmon, file) {
        map.insert(id.to_string(), p);
    }
}

fn find_zenergy_socket() -> Option<String> {
    for entry in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let path = entry.path();
        let name = fs::read_to_string(path.join("name"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if name != "zenergy" {
            continue;
        }
        for i in 1..=16 {
            let label = fs::read_to_string(path.join(format!("energy{i}_label")))
                .unwrap_or_default()
                .trim()
                .to_string();
            if label == "Esocket0" {
                return Some(
                    path.join(format!("energy{i}_input"))
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    None
}

fn read_millidegree(path: &str) -> Option<i32> {
    let v = read_u64(path)? as i64;
    Some((v / 1000) as i32)
}

fn read_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_proc_stat() -> Option<(u64, u64)> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    parse_proc_stat(&content)
}

fn parse_proc_stat(content: &str) -> Option<(u64, u64)> {
    let line = content.lines().next()?.strip_prefix("cpu ")?;
    let mut p = line.split_whitespace();
    let user: u64 = p.next()?.parse().ok()?;
    let nice: u64 = p.next()?.parse().ok()?;
    let system: u64 = p.next()?.parse().ok()?;
    let idle: u64 = p.next()?.parse().ok()?;
    let iowait: u64 = p.next()?.parse().ok()?;
    let irq: u64 = p.next()?.parse().ok()?;
    let softirq: u64 = p.next()?.parse().ok()?;
    let total = user + nice + system + idle + iowait + irq + softirq;
    Some((idle + iowait, total))
}

fn read_cpu_freq_avg() -> f32 {
    let mut total = 0u64;
    let mut count = 0u32;
    let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
        return 0.0;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("cpufreq/scaling_cur_freq");
        if let Ok(v) = fs::read_to_string(&path) {
            if let Ok(khz) = v.trim().parse::<u64>() {
                total += khz;
                count += 1;
            }
        }
    }
    if count > 0 {
        total as f32 / count as f32 / 1_000_000.0
    } else {
        0.0
    }
}

fn read_ram_used_pct() -> Option<f32> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    parse_ram_used_pct(&content)
}

fn parse_ram_used_pct(content: &str) -> Option<f32> {
    let mut total_kb = 0u64;
    let mut avail_kb = 0u64;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest.split_whitespace().next()?.parse().ok()?;
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = rest.split_whitespace().next()?.parse().ok()?;
        }
    }
    if total_kb == 0 {
        return None;
    }
    Some(total_kb.saturating_sub(avail_kb) as f32 / total_kb as f32 * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat_and_counts_iowait_as_idle() {
        let (idle, total) =
            parse_proc_stat("cpu  100 20 30 400 50 10 20 0 0 0\n").expect("valid proc stat");
        assert_eq!(idle, 450);
        assert_eq!(total, 630);
    }

    #[test]
    fn rejects_malformed_proc_stat() {
        assert!(parse_proc_stat("cpu  1 2 3\n").is_none());
        assert!(parse_proc_stat("intr 1 2 3\n").is_none());
    }

    #[test]
    fn parses_ram_used_percentage_and_handles_missing_total() {
        let pct = parse_ram_used_pct("MemTotal:       1000 kB\nMemAvailable:    250 kB\n")
            .expect("valid meminfo");
        assert!((pct - 75.0).abs() < f32::EPSILON);
        assert!(parse_ram_used_pct("MemAvailable: 250 kB\n").is_none());
    }

    #[test]
    fn probe_and_numeric_readers_handle_valid_and_invalid_files() {
        let root = std::env::temp_dir().join(format!("th420-sensors-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let input = root.join("temp1_input");
        fs::write(&input, "42500\n").unwrap();

        assert_eq!(
            probe(&root, "temp1_input"),
            Some(input.to_string_lossy().into_owned())
        );
        assert_eq!(read_u64(input.to_str().unwrap()), Some(42_500));
        assert_eq!(read_millidegree(input.to_str().unwrap()), Some(42));
        fs::write(&input, "not-a-number").unwrap();
        assert_eq!(read_u64(input.to_str().unwrap()), None);
        assert_eq!(probe(&root, "missing"), None);

        fs::remove_dir_all(root).unwrap();
    }
}
