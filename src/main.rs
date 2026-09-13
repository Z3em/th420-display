mod config;
mod device;
mod renderer;
mod sensors;

use anyhow::{bail, Result};
use clap::Parser;
use config::{Config, default_config_path};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use std::io::Cursor;
use image::{AnimationDecoder, imageops};
use image::codecs::gif::GifDecoder;
use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEncoder, SamplingFactor};
use std::fs::File;
use std::io::BufReader;

const MAX_BOOT_CONTAINER_SIZE: usize = 5 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "th420-display", about = "CPU/GPU monitor for Thermaltake TH420 V2 LCD")]
struct Cli {
    /// Update interval in milliseconds
    #[arg(short, long, default_value = "800")]
    interval: u64,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Upload one image as the persistent standby picture, then exit.
    #[arg(long, value_name = "IMAGE")]
    upload_standby: Option<PathBuf>,

    /// Set persistent LCD brightness (0 through 100), then exit.
    #[arg(long, value_name = "PERCENT", value_parser = clap::value_parser!(u8).range(0..=100))]
    standby_brightness: Option<u8>,

    /// Set pump-temperature text color as #RRGGBB; requires --upload-standby.
    #[arg(long, value_name = "#RRGGBB")]
    pump_temp_color: Option<String>,

    /// Upload an animated GIF as the persistent boot animation, then exit.
    #[arg(long, value_name = "GIF")]
    upload_boot: Option<PathBuf>,

    /// Stream IMAGE frames through the live display endpoint, then exit.
    #[arg(long, value_name = "IMAGE", num_args = 1..)]
    play_live_frames: Vec<PathBuf>,

    /// Stream an animated GIF through the live display endpoint using its frame delays.
    #[arg(long, value_name = "GIF")]
    play_live_gif: Option<PathBuf>,

    /// Frame rate for --play-live-frames.
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u8).range(1..=60))]
    live_fps: u8,

    /// Number of complete --play-live-frames loops.
    #[arg(long, default_value_t = 1)]
    live_loops: usize,

    /// Brightness to hold while streaming --play-live-frames.
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(0..=100))]
    live_brightness: u8,
}

fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn parse_rgb(value: &str) -> Result<[u8; 3]> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("color must use #RRGGBB format");
    }
    Ok([
        u8::from_str_radix(&value[0..2], 16)?,
        u8::from_str_radix(&value[2..4], 16)?,
        u8::from_str_radix(&value[4..6], 16)?,
    ])
}

fn encode_jpeg(path: &Path) -> Result<Vec<u8>> {
    let source = image::open(path)?.into_rgb8();
    encode_rgb_jpeg(source)
}

fn encode_rgb_jpeg(source: image::RgbImage) -> Result<Vec<u8>> {
    let image = imageops::resize(&source, 480, 480, imageops::FilterType::Lanczos3);
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, image::ImageFormat::Jpeg)?;
    Ok(encoded.into_inner())
}

fn decode_gif(path: &Path) -> Result<Vec<(Vec<u8>, Duration)>> {
    let decoder = GifDecoder::new(BufReader::new(File::open(path)?))?;
    decoder.into_frames().collect_frames()?.into_iter().map(|frame| {
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay = if denominator == 0 {
            Duration::from_millis(100)
        } else {
            Duration::from_secs_f64(f64::from(numerator) / f64::from(denominator) / 1000.0)
        };
        let rgb = image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8();
        Ok((encode_rgb_jpeg(rgb)?, delay))
    }).collect()
}

fn encode_boot_jpeg(source: image::RgbImage) -> Result<Vec<u8>> {
    let image = imageops::resize(&source, 480, 480, imageops::FilterType::Lanczos3);
    let mut encoded = Vec::new();
    let mut encoder = JpegEncoder::new(&mut encoded, 75);
    encoder.set_sampling_factor(SamplingFactor::F_2_2); // 4:2:0, as captured from the vendor app
    encoder.encode(image.as_raw(), 480, 480, JpegColorType::Rgb)?;
    Ok(encoded)
}

fn decode_boot_gif(path: &Path) -> Result<(Vec<Vec<u8>>, u32)> {
    let decoder = GifDecoder::new(BufReader::new(File::open(path)?))?;
    let mut frame_delay_ms = None;
    let mut frames = Vec::new();

    for frame in decoder.into_frames().collect_frames()? {
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        if denominator == 0 || numerator % denominator != 0 {
            bail!("boot GIF frame delays must be an exact number of milliseconds");
        }
        let delay_ms = numerator / denominator;
        if delay_ms < 80 {
            bail!("boot GIF frame delay must be at least 80 ms");
        }
        if let Some(expected) = frame_delay_ms {
            if delay_ms != expected {
                bail!("boot GIF must use one uniform frame delay");
            }
        } else {
            frame_delay_ms = Some(delay_ms);
        }

        let rgb = image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8();
        frames.push(encode_boot_jpeg(rgb)?);
    }

    Ok((frames, frame_delay_ms.ok_or_else(|| anyhow::anyhow!("boot GIF has no frames"))?))
}

fn validate_cli(cli: &Cli) -> Result<()> {
    if cli.pump_temp_color.is_some() && cli.upload_standby.is_none() {
        bail!("--pump-temp-color requires --upload-standby to commit the color");
    }
    if cli.upload_boot.is_some()
        && (cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some()
            || !cli.play_live_frames.is_empty()
            || cli.play_live_gif.is_some())
    {
        bail!("--upload-boot cannot be combined with other display operations");
    }
    if !cli.play_live_frames.is_empty() && cli.play_live_gif.is_some() {
        bail!("--play-live-frames and --play-live-gif are mutually exclusive");
    }
    if (!cli.play_live_frames.is_empty() || cli.play_live_gif.is_some())
        && (cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some())
    {
        bail!("live playback cannot be combined with persistent settings");
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_cli(&cli)?;
    let interval = Duration::from_millis(cli.interval);
    let config_path = cli.config.unwrap_or_else(default_config_path);

    if let Some(path) = cli.upload_boot {
        let (frames, frame_delay_ms) = decode_boot_gif(&path)?;
        let container = device::build_boot_container(&frames)?;
        if container.len() > MAX_BOOT_CONTAINER_SIZE {
            bail!("boot container exceeds the conservative 5 MiB limit");
        }

        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        println!(
            "Uploading boot animation: {} frame(s), {} ms/frame, {} bytes.",
            frames.len(), frame_delay_ms, container.len(),
        );
        dev.upload_boot(&container, frame_delay_ms)?;
        println!("Boot animation upload complete.");
        return Ok(());
    }

    if let Some(path) = cli.play_live_gif {
        let frames = decode_gif(&path)?;
        if frames.is_empty() {
            bail!("GIF contains no frames");
        }

        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        println!(
            "Streaming {} GIF frame(s) for {} loop(s) at {}% brightness.",
            frames.len(), cli.live_loops, cli.live_brightness,
        );
        dev.set_brightness(cli.live_brightness)?;
        for _ in 0..cli.live_loops {
            for (frame, delay) in &frames {
                let tick = Instant::now();
                dev.send_frame_data(frame)?;
                let elapsed = tick.elapsed();
                if elapsed < *delay {
                    std::thread::sleep(*delay - elapsed);
                }
            }
        }
        return Ok(());
    }

    if !cli.play_live_frames.is_empty() {
        let frames: Result<Vec<_>> = cli.play_live_frames.iter()
            .map(|path| encode_jpeg(path))
            .collect();
        let frames = frames?;
        let period = Duration::from_secs_f64(1.0 / f64::from(cli.live_fps));

        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        println!(
            "Streaming {} frame(s) at {} FPS for {} loop(s) at {}% brightness.",
            frames.len(), cli.live_fps, cli.live_loops, cli.live_brightness,
        );
        dev.set_brightness(cli.live_brightness)?;
        for _ in 0..cli.live_loops {
            for frame in &frames {
                let tick = Instant::now();
                dev.send_frame_data(frame)?;
                let elapsed = tick.elapsed();
                if elapsed < period {
                    std::thread::sleep(period - elapsed);
                }
            }
        }
        return Ok(());
    }

    if cli.upload_standby.is_some()
        || cli.standby_brightness.is_some()
        || cli.pump_temp_color.is_some()
    {
        let encoded = if let Some(path) = &cli.upload_standby {
            Some((path, encode_jpeg(path)?))
        } else {
            None
        };
        let color = cli.pump_temp_color.as_deref().map(parse_rgb).transpose()?;

        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        // The vendor app stages the overlay color before it begins the standby
        // upload; preserve that ordering so the upload commits the staged color.
        if let Some(color) = color {
            dev.set_pump_temperature_color(color)?;
            println!("Pump-temperature text color set to #{:02x}{:02x}{:02x}.", color[0], color[1], color[2]);
        }
        if let Some((path, encoded)) = encoded {
            println!("Uploading standby image: {} ({} bytes)", path.display(), encoded.len());
            dev.upload_standby(&encoded)?;
            println!("Standby image upload complete.");
        }
        if let Some(brightness) = cli.standby_brightness {
            dev.set_brightness(brightness)?;
            println!("LCD brightness set to {brightness}%.");
        }
        return Ok(());
    }

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
    // Brightness is persistent on the device.  Set it once before the live
    // stream so the control endpoint is reserved for the coolant query below.
    // Re-sending brightness for every frame can leave an ACK queued, which
    // would then be mistaken for a temperature response on the next tick.
    dev.set_brightness(100)?;
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
        dev.send_frame_data(&jpeg)?;

        let elapsed = tick.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> Cli {
        Cli {
            interval: 800, config: None, upload_standby: None, standby_brightness: None,
            pump_temp_color: None, upload_boot: None, play_live_frames: vec![],
            play_live_gif: None, live_fps: 24, live_loops: 1, live_brightness: 80,
        }
    }

    #[test]
    fn parses_rgb_with_or_without_hash() {
        assert_eq!(parse_rgb("#0055ff").unwrap(), [0, 0x55, 0xff]);
        assert_eq!(parse_rgb("ff0000").unwrap(), [0xff, 0, 0]);
    }

    #[test]
    fn rejects_invalid_rgb() {
        assert!(parse_rgb("#0f0").is_err());
        assert!(parse_rgb("#00ff0z").is_err());
    }

    #[test]
    fn jpeg_encoders_produce_decodable_480_square_images() {
        let source = image::RgbImage::from_pixel(16, 8, image::Rgb([20, 40, 60]));
        for encoded in [encode_rgb_jpeg(source.clone()), encode_boot_jpeg(source)] {
            let decoded = image::load_from_memory(&encoded.unwrap()).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (480, 480));
        }
    }

    #[test]
    fn cli_validation_rejects_conflicting_operations() {
        let mut options = cli();
        options.pump_temp_color = Some("#ffffff".into());
        assert!(validate_cli(&options).is_err());

        let mut options = cli();
        options.upload_boot = Some("boot.gif".into());
        options.upload_standby = Some("standby.png".into());
        assert!(validate_cli(&options).is_err());

        let mut options = cli();
        options.play_live_frames.push("one.png".into());
        options.play_live_gif = Some("live.gif".into());
        assert!(validate_cli(&options).is_err());
    }

    #[test]
    fn cli_validation_accepts_one_operation_at_a_time() {
        let mut options = cli();
        options.upload_standby = Some("standby.png".into());
        options.pump_temp_color = Some("#0055ff".into());
        options.standby_brightness = Some(80);
        assert!(validate_cli(&options).is_ok());
    }
}
