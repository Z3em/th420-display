mod config;
mod device;
mod renderer;
mod sensors;

use anyhow::Result;
use clap::Parser;
use config::{Config, default_config_path};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

#[derive(Parser)]
#[command(name = "th420-display", about = "CPU/GPU monitor for Thermaltake TH420 V2 LCD")]
struct Cli {
    /// Update interval in milliseconds
    #[arg(short, long, default_value = "800")]
    interval: u64,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,
}

fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let interval = Duration::from_millis(cli.interval);
    let config_path = cli.config.unwrap_or_else(default_config_path);

    let mut cfg = Config::load(&config_path).unwrap_or_else(|_| {
        let default = Config::default();
        let _ = default.save(&config_path);
        default
    });
    let mut cfg_mtime = file_mtime(&config_path);

    println!("Config: {}", config_path.display());
    println!("Opening Thermaltake TH420 V2...");

    let mut dev = device::Device::open()?;
    dev.init()?;
    println!("Device ready. Displaying stats (Ctrl+C to stop).");

    let mut sensors = sensors::SensorReader::new();
    let mut renderer = renderer::Renderer::new();

    loop {
        let tick = Instant::now();

        // Reload config if file changed
        let mtime = file_mtime(&config_path);
        if mtime != cfg_mtime {
            if let Ok(new_cfg) = Config::load(&config_path) {
                cfg = new_cfg;
                cfg_mtime = mtime;
                println!("Config reloaded.");
            }
        }

        let mut values = sensors.read();
        values.readings.insert(
            "coolant".to_string(),
            dev.read_liquid_temp().unwrap_or(0.0),
        );

        let jpeg = renderer.render(&cfg, &values);
        dev.send_frame(&jpeg)?;

        let elapsed = tick.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}
