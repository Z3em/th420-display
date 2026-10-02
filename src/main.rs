mod config;
mod device;
mod instance;
mod job_protocol;
mod renderer;
mod sensors;

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use config::{default_config_path, Config, ImageFit, MediaTransform, Transform2D};
use image::codecs::gif::GifDecoder;
use image::{imageops, AnimationDecoder};
use instance::{
    current_owner, replace_and_acquire_daemon, request_daemon_command, AcquireError, DaemonControl,
    DeviceGuard, InstanceGuard, InstanceKind, ReplaceExisting,
};
use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEncoder, SamplingFactor};
use std::fs::File;
use std::io::BufReader;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

// Observed in the reverse-engineered official Windows application.
const MAX_BOOT_CONTAINER_SIZE: usize = 10 * 1024 * 1024;
const MIN_BOOT_FRAME_DELAY_MS: u32 = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BootEdit {
    start_frame: usize,
    end_frame: Option<usize>,
    frame_delay_ms: Option<u32>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PumpTempOverlay {
    Show,
    Hide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DaemonControlArg {
    Pause,
    Resume,
    State,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MediaFitArg {
    Cover,
    Contain,
    Stretch,
    Native,
}

impl From<MediaFitArg> for ImageFit {
    fn from(value: MediaFitArg) -> Self {
        match value {
            MediaFitArg::Cover => Self::Cover,
            MediaFitArg::Contain => Self::Contain,
            MediaFitArg::Stretch => Self::Stretch,
            MediaFitArg::Native => Self::Native,
        }
    }
}

impl PumpTempOverlay {
    fn visible(self) -> bool {
        matches!(self, Self::Show)
    }
}

#[derive(Parser)]
#[command(
    name = "th420-display",
    about = "CPU/GPU monitor for Thermaltake TH420 V2 LCD"
)]
struct Cli {
    /// Replace an existing live daemon, escalating no further than this level.
    #[arg(long, value_enum, value_name = "graceful|term|kill")]
    replace_existing: Option<ReplaceExisting>,

    /// Control device ownership of the running daemon.
    #[arg(long, value_enum, value_name = "pause|resume|state")]
    daemon_control: Option<DaemonControlArg>,

    /// Sensor update interval in milliseconds
    #[arg(short, long, default_value = "800")]
    interval: u64,

    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Stream a temporary live configuration without owning the daemon instance.
    #[arg(long, requires = "config")]
    preview_live: bool,

    /// Internal GUI preparation/write handshake; stdin must authorize writes.
    #[arg(long, hide = true)]
    gui_device_job: bool,

    /// Print device coolant temperature and pump RPM, then exit.
    #[arg(long)]
    status: bool,

    /// Upload one image as the persistent standby picture, then exit.
    #[arg(long, value_name = "IMAGE")]
    upload_standby: Option<PathBuf>,

    /// Set persistent LCD brightness (0 through 100), then exit.
    #[arg(long, value_name = "PERCENT", value_parser = clap::value_parser!(u8).range(0..=100))]
    standby_brightness: Option<u8>,

    /// Set pump-temperature text color as #RRGGBB; requires --upload-standby.
    #[arg(long, value_name = "#RRGGBB")]
    pump_temp_color: Option<String>,

    /// Show or hide the coolant-temperature text on the standby image.
    #[arg(long, value_name = "show|hide")]
    pump_temp_overlay: Option<PumpTempOverlay>,

    /// Upload an animated GIF as the persistent boot animation, then exit.
    #[arg(long, value_name = "GIF")]
    upload_boot: Option<PathBuf>,

    /// Prepare and report boot-container metadata without opening the device.
    #[arg(long, value_name = "GIF")]
    inspect_boot: Option<PathBuf>,

    /// First boot-animation frame to include (zero-based).
    #[arg(long, default_value_t = 0)]
    boot_start_frame: usize,

    /// Last boot-animation frame to include (zero-based, inclusive).
    #[arg(long)]
    boot_end_frame: Option<usize>,

    /// Override source timing with one uniform boot-animation frame delay.
    #[arg(long, value_parser = clap::value_parser!(u32).range(80..))]
    boot_frame_delay_ms: Option<u32>,

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

    /// Fit policy applied to uploaded or transiently previewed media.
    #[arg(long, value_enum, default_value_t = MediaFitArg::Stretch)]
    media_fit: MediaFitArg,

    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    media_pan_x: f32,
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    media_pan_y: f32,
    #[arg(long, default_value_t = 1.0, allow_hyphen_values = true)]
    media_zoom: f32,
    #[arg(long, default_value_t = 1.0, allow_hyphen_values = true)]
    media_stretch_x: f32,
    #[arg(long, default_value_t = 1.0, allow_hyphen_values = true)]
    media_stretch_y: f32,
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    media_rotation: f32,
    #[arg(long, default_value = "#0a0a14")]
    media_canvas: String,
    /// Select one timestamp for single-frame media preparation.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    media_time: f64,
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

fn media_transform(cli: &Cli) -> Result<MediaTransform> {
    let values = [
        cli.media_pan_x,
        cli.media_pan_y,
        cli.media_zoom,
        cli.media_stretch_x,
        cli.media_stretch_y,
        cli.media_rotation,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        bail!("media transform values must be finite");
    }
    if !cli.media_time.is_finite() || cli.media_time < 0.0 {
        bail!("media time must be a finite non-negative number");
    }
    if cli.media_zoom <= 0.0 || cli.media_stretch_x <= 0.0 || cli.media_stretch_y <= 0.0 {
        bail!("media zoom and stretch must be greater than zero");
    }
    Ok(MediaTransform {
        fit: cli.media_fit.into(),
        transform: Transform2D {
            pan_x: cli.media_pan_x,
            pan_y: cli.media_pan_y,
            zoom: cli.media_zoom,
            stretch_x: cli.media_stretch_x,
            stretch_y: cli.media_stretch_y,
            rotation: cli.media_rotation,
        },
        canvas_color: parse_rgb(&cli.media_canvas)?,
    })
}

fn encode_jpeg(path: &Path, transform: &MediaTransform, media_time: f64) -> Result<Vec<u8>> {
    let source = renderer::load_media_frame_at(path, media_time).map_err(anyhow::Error::msg)?;
    let transformed = renderer::transform_media_image(&source, transform)
        .ok_or_else(|| anyhow::anyhow!("media frame is empty"))?;
    encode_rgb_jpeg(transformed)
}

fn encode_rgb_jpeg(source: image::RgbImage) -> Result<Vec<u8>> {
    let image = imageops::resize(&source, 480, 480, imageops::FilterType::Lanczos3);
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, image::ImageFormat::Jpeg)?;
    Ok(encoded.into_inner())
}

fn decode_gif(
    path: &Path,
    transform: &MediaTransform,
    edit: BootEdit,
) -> Result<Vec<(Vec<u8>, Duration)>> {
    let decoder = GifDecoder::new(BufReader::new(File::open(path)?))?;
    let frames = decoder
        .into_frames()
        .collect_frames()?
        .into_iter()
        .map(|frame| {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let delay = if denominator == 0 {
                Duration::from_millis(100)
            } else {
                Duration::from_secs_f64(f64::from(numerator) / f64::from(denominator) / 1000.0)
            };
            let rgb = image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8();
            let transformed = renderer::transform_media_image(&rgb, transform)
                .ok_or_else(|| anyhow::anyhow!("media frame is empty"))?;
            Ok((encode_rgb_jpeg(transformed)?, delay))
        })
        .collect::<Result<Vec<_>>>()?;
    let range = boot_frame_range(frames.len(), edit)?;
    Ok(frames[range]
        .iter()
        .map(|(frame, source_delay)| {
            (
                frame.clone(),
                edit.frame_delay_ms
                    .map(|delay| Duration::from_millis(u64::from(delay)))
                    .unwrap_or(*source_delay),
            )
        })
        .collect())
}

fn encode_boot_jpeg(source: image::RgbImage) -> Result<Vec<u8>> {
    let image = imageops::resize(&source, 480, 480, imageops::FilterType::Lanczos3);
    let mut encoded = Vec::new();
    let mut encoder = JpegEncoder::new(&mut encoded, 75);
    encoder.set_sampling_factor(SamplingFactor::F_2_2); // 4:2:0, as captured from the vendor app
    encoder.encode(image.as_raw(), 480, 480, JpegColorType::Rgb)?;
    Ok(encoded)
}

fn boot_frame_range(frame_count: usize, edit: BootEdit) -> Result<std::ops::Range<usize>> {
    if frame_count == 0 {
        bail!("boot GIF has no frames");
    }
    let end = edit.end_frame.unwrap_or(frame_count - 1);
    if edit.start_frame >= frame_count || end >= frame_count {
        bail!("boot frame range is outside the source animation");
    }
    if edit.start_frame > end {
        bail!("boot start frame must not be after the end frame");
    }
    if edit
        .frame_delay_ms
        .is_some_and(|delay| delay < MIN_BOOT_FRAME_DELAY_MS)
    {
        bail!("boot frame delay must be at least 80 ms");
    }
    Ok(edit.start_frame..end + 1)
}

fn decode_boot_gif(
    path: &Path,
    transform: &MediaTransform,
    edit: BootEdit,
) -> Result<(Vec<Vec<u8>>, u32)> {
    let decoder = GifDecoder::new(BufReader::new(File::open(path)?))?;
    let source_frames = decoder.into_frames().collect_frames()?;
    let range = boot_frame_range(source_frames.len(), edit)?;
    let mut source_delay_ms = None;
    let mut frames = Vec::with_capacity(range.len());

    for frame in source_frames
        .into_iter()
        .skip(range.start)
        .take(range.len())
    {
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        if denominator == 0 || numerator % denominator != 0 {
            bail!("boot GIF frame delays must be an exact number of milliseconds");
        }
        let delay_ms = numerator / denominator;
        if edit.frame_delay_ms.is_none() && delay_ms < MIN_BOOT_FRAME_DELAY_MS {
            bail!("boot GIF frame delay must be at least 80 ms");
        }
        if let Some(expected) = source_delay_ms {
            if edit.frame_delay_ms.is_none() && delay_ms != expected {
                bail!("boot GIF must use one uniform frame delay");
            }
        } else {
            source_delay_ms = Some(delay_ms);
        }

        let rgb = image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8();
        let transformed = renderer::transform_media_image(&rgb, transform)
            .ok_or_else(|| anyhow::anyhow!("boot frame is empty"))?;
        frames.push(encode_boot_jpeg(transformed)?);
    }

    Ok((
        frames,
        edit.frame_delay_ms
            .or(source_delay_ms)
            .ok_or_else(|| anyhow::anyhow!("boot GIF has no frames"))?,
    ))
}

fn validate_cli(cli: &Cli) -> Result<()> {
    if cli.gui_device_job
        && (cli.status
            || cli.inspect_boot.is_some()
            || cli.preview_live
            || cli.replace_existing.is_some()
            || cli.daemon_control.is_some()
            || cli.play_live_gif.is_some()
            || !cli.play_live_frames.is_empty()
            || !(cli.upload_boot.is_some()
                || cli.upload_standby.is_some()
                || cli.standby_brightness.is_some()
                || cli.pump_temp_overlay.is_some()
                || cli.pump_temp_color.is_some()))
    {
        bail!("--gui-device-job requires a supported one-shot write operation");
    }
    if cli.preview_live && cli.config.is_none() {
        bail!("--preview-live requires --config");
    }
    if cli.preview_live
        && (cli.replace_existing.is_some()
            || cli.daemon_control.is_some()
            || cli.status
            || cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some()
            || cli.pump_temp_overlay.is_some()
            || cli.upload_boot.is_some()
            || cli.inspect_boot.is_some()
            || !cli.play_live_frames.is_empty()
            || cli.play_live_gif.is_some())
    {
        bail!("--preview-live cannot be combined with another device operation");
    }
    if cli.replace_existing.is_some() && cli.is_one_shot() {
        bail!("--replace-existing applies only to the continuous live daemon");
    }
    if cli.pump_temp_color.is_some() && cli.upload_standby.is_none() {
        bail!("--pump-temp-color requires --upload-standby to commit the color");
    }
    if cli.daemon_control.is_some()
        && (cli.status
            || cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some()
            || cli.pump_temp_overlay.is_some()
            || cli.upload_boot.is_some()
            || cli.inspect_boot.is_some()
            || !cli.play_live_frames.is_empty()
            || cli.play_live_gif.is_some())
    {
        bail!("daemon control cannot be combined with display operations");
    }
    if cli.status
        && (cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some()
            || cli.pump_temp_overlay.is_some()
            || cli.upload_boot.is_some()
            || cli.inspect_boot.is_some()
            || !cli.play_live_frames.is_empty()
            || cli.play_live_gif.is_some())
    {
        bail!("--status cannot be combined with display operations");
    }
    if cli.upload_boot.is_some()
        && (cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some()
            || cli.pump_temp_overlay.is_some()
            || !cli.play_live_frames.is_empty()
            || cli.play_live_gif.is_some())
    {
        bail!("--upload-boot cannot be combined with other display operations");
    }
    if cli.inspect_boot.is_some()
        && (cli.upload_boot.is_some()
            || cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some()
            || cli.pump_temp_overlay.is_some()
            || !cli.play_live_frames.is_empty()
            || cli.play_live_gif.is_some())
    {
        bail!("--inspect-boot cannot be combined with display operations");
    }
    if !cli.play_live_frames.is_empty() && cli.play_live_gif.is_some() {
        bail!("--play-live-frames and --play-live-gif are mutually exclusive");
    }
    if (!cli.play_live_frames.is_empty() || cli.play_live_gif.is_some())
        && (cli.upload_standby.is_some()
            || cli.standby_brightness.is_some()
            || cli.pump_temp_color.is_some()
            || cli.pump_temp_overlay.is_some())
    {
        bail!("live playback cannot be combined with persistent settings");
    }
    Ok(())
}

impl Cli {
    fn is_one_shot(&self) -> bool {
        self.status
            || self.upload_standby.is_some()
            || self.standby_brightness.is_some()
            || self.pump_temp_color.is_some()
            || self.pump_temp_overlay.is_some()
            || self.upload_boot.is_some()
            || self.inspect_boot.is_some()
            || !self.play_live_frames.is_empty()
            || self.play_live_gif.is_some()
            || self.daemon_control.is_some()
            || self.preview_live
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_cli(&cli)?;
    if let Some(command) = cli.daemon_control {
        let command = match command {
            DaemonControlArg::Pause => "pause",
            DaemonControlArg::Resume => "resume",
            DaemonControlArg::State => "state",
        };
        let owner = current_owner(InstanceKind::Daemon)
            .ok_or_else(|| anyhow::anyhow!("the live daemon is not running"))?;
        let state = request_daemon_command(&owner, command, Duration::from_secs(5))
            .map_err(anyhow::Error::msg)?;
        println!("daemon_state={state}");
        return Ok(());
    }
    let media_transform = media_transform(&cli)?;
    let boot_edit = BootEdit {
        start_frame: cli.boot_start_frame,
        end_frame: cli.boot_end_frame,
        frame_delay_ms: cli.boot_frame_delay_ms,
    };
    let interval = Duration::from_millis(cli.interval);
    let config_path = cli.config.unwrap_or_else(default_config_path);
    let preview_live = cli.preview_live;
    let preview_brightness = cli.live_brightness;

    if cli.status {
        let _device_guard = DeviceGuard::acquire().map_err(anyhow::Error::msg)?;
        let mut dev = device::Device::open()?;
        dev.init()?;
        let status = dev.read_status()?;
        // Stable machine-readable output used by the GUI and useful in scripts.
        println!("coolant_temp_c={:.2}", status.coolant_temp_c);
        println!("pump_rpm={}", status.pump_rpm);
        return Ok(());
    }

    if let Some(path) = cli.inspect_boot {
        let (frames, frame_delay_ms) = decode_boot_gif(&path, &media_transform, boot_edit)?;
        let container = device::build_boot_container(&frames)?;
        println!("boot_frames={}", frames.len());
        println!("boot_delay_ms={frame_delay_ms}");
        println!("boot_container_bytes={}", container.len());
        println!("boot_limit_bytes={MAX_BOOT_CONTAINER_SIZE}");
        println!(
            "boot_within_limit={}",
            container.len() <= MAX_BOOT_CONTAINER_SIZE
        );
        return Ok(());
    }

    if let Some(path) = cli.upload_boot {
        let (frames, frame_delay_ms) = decode_boot_gif(&path, &media_transform, boot_edit)?;
        let container = device::build_boot_container(&frames)?;
        if container.len() > MAX_BOOT_CONTAINER_SIZE {
            bail!("boot container exceeds the 10 MB vendor-app limit");
        }

        job_protocol::wait_for_gui(cli.gui_device_job)?;
        let _device_guard = DeviceGuard::acquire().map_err(anyhow::Error::msg)?;
        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        println!(
            "Uploading boot animation: {} frame(s), {} ms/frame, {} bytes.",
            frames.len(),
            frame_delay_ms,
            container.len(),
        );
        dev.upload_boot(&container, frame_delay_ms)?;
        println!("Boot animation upload complete.");
        return Ok(());
    }

    if let Some(path) = cli.play_live_gif {
        let frames = decode_gif(&path, &media_transform, boot_edit)?;
        if frames.is_empty() {
            bail!("GIF contains no frames");
        }

        let _device_guard = DeviceGuard::acquire().map_err(anyhow::Error::msg)?;
        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        println!(
            "Streaming {} GIF frame(s) for {} loop(s) at {}% brightness.",
            frames.len(),
            cli.live_loops,
            cli.live_brightness,
        );
        dev.set_brightness(cli.live_brightness)?;
        let mut completed = 0usize;
        while cli.live_loops == 0 || completed < cli.live_loops {
            for (frame, delay) in &frames {
                let tick = Instant::now();
                dev.send_frame_data(frame)?;
                let elapsed = tick.elapsed();
                if elapsed < *delay {
                    std::thread::sleep(*delay - elapsed);
                }
            }
            completed = completed.saturating_add(1);
        }
        return Ok(());
    }

    if !cli.play_live_frames.is_empty() {
        let frames: Result<Vec<_>> = cli
            .play_live_frames
            .iter()
            .map(|path| encode_jpeg(path, &media_transform, cli.media_time))
            .collect();
        let frames = frames?;
        let period = Duration::from_secs_f64(1.0 / f64::from(cli.live_fps));

        let _device_guard = DeviceGuard::acquire().map_err(anyhow::Error::msg)?;
        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        println!(
            "Streaming {} frame(s) at {} FPS for {} loop(s) at {}% brightness.",
            frames.len(),
            cli.live_fps,
            cli.live_loops,
            cli.live_brightness,
        );
        dev.set_brightness(cli.live_brightness)?;
        let mut completed = 0usize;
        while cli.live_loops == 0 || completed < cli.live_loops {
            for frame in &frames {
                let tick = Instant::now();
                dev.send_frame_data(frame)?;
                let elapsed = tick.elapsed();
                if elapsed < period {
                    std::thread::sleep(period - elapsed);
                }
            }
            completed = completed.saturating_add(1);
        }
        return Ok(());
    }

    if cli.upload_standby.is_some()
        || cli.standby_brightness.is_some()
        || cli.pump_temp_color.is_some()
        || cli.pump_temp_overlay.is_some()
    {
        let encoded = if let Some(path) = &cli.upload_standby {
            Some((path, encode_jpeg(path, &media_transform, cli.media_time)?))
        } else {
            None
        };
        let color = cli.pump_temp_color.as_deref().map(parse_rgb).transpose()?;

        job_protocol::wait_for_gui(cli.gui_device_job)?;
        let _device_guard = DeviceGuard::acquire().map_err(anyhow::Error::msg)?;
        println!("Opening Thermaltake TH420 V2...");
        let mut dev = device::Device::open()?;
        dev.init()?;
        // The vendor app stages the overlay color before it begins the standby
        // upload; preserve that ordering so the upload commits the staged color.
        if let Some(color) = color {
            dev.set_pump_temperature_color(color)?;
            println!(
                "Pump-temperature text color set to #{:02x}{:02x}{:02x}.",
                color[0], color[1], color[2]
            );
        }
        if let Some((path, encoded)) = encoded {
            println!(
                "Uploading standby image: {} ({} bytes)",
                path.display(),
                encoded.len()
            );
            dev.upload_standby(&encoded)?;
            println!("Standby image upload complete.");
        }
        if let Some(overlay) = cli.pump_temp_overlay {
            dev.set_pump_temperature_visible(overlay.visible())?;
            println!(
                "Pump-temperature text {}.",
                if overlay.visible() { "shown" } else { "hidden" }
            );
        }
        if let Some(brightness) = cli.standby_brightness {
            dev.set_brightness(brightness)?;
            println!("LCD brightness set to {brightness}%.");
        }
        return Ok(());
    }

    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let daemon_control = DaemonControl::new_starting();
    let _instance_guard = if preview_live {
        None
    } else if let Some(level) = cli.replace_existing {
        Some(
            replace_and_acquire_daemon(shutdown_requested.clone(), level, daemon_control.clone())
                .map_err(anyhow::Error::msg)?,
        )
    } else {
        match InstanceGuard::try_acquire_daemon(shutdown_requested.clone(), daemon_control.clone())
        {
            Ok(guard) => Some(guard),
            Err(AcquireError::Conflict(owner)) => {
                bail!("the live daemon is already running:\n{}", owner.describe())
            }
            Err(AcquireError::Other(error)) => bail!("{error}"),
        }
    };
    let mut device_guard = Some(DeviceGuard::acquire().map_err(anyhow::Error::msg)?);

    let mut cfg = if preview_live {
        Config::load(&config_path)?
    } else {
        Config::load(&config_path).unwrap_or_else(|_| {
            let default = Config::default();
            let _ = default.save(&config_path);
            default
        })
    };
    let mut cfg_mtime = file_mtime(&config_path);

    println!("Config: {}", config_path.display());
    println!("Opening Thermaltake TH420 V2...");

    let mut initial_device = device::Device::open()?;
    initial_device.init()?;
    // Brightness is persistent on the device.  Set it once before the live
    // stream so the control endpoint is reserved for the coolant query below.
    // Re-sending brightness for every frame can leave an ACK queued, which
    // would then be mistaken for a temperature response on the next tick.
    initial_device.set_brightness(if preview_live {
        preview_brightness
    } else {
        100
    })?;
    let mut dev = Some(initial_device);
    if !daemon_control.pause_requested() {
        daemon_control.mark_running();
    }
    println!("Device ready. Displaying stats (Ctrl+C to stop).");

    let mut sensors = sensors::SensorReader::new();
    let mut renderer = renderer::Renderer::new();
    let mut values = sensors.read();
    match dev.as_mut().unwrap().read_status() {
        Ok(status) => {
            values
                .readings
                .insert("coolant".to_string(), status.coolant_temp_c);
            daemon_control.update_telemetry(status.coolant_temp_c, status.pump_rpm);
        }
        Err(error) => daemon_control.mark_telemetry_error(error.to_string()),
    }
    let mut last_sensor_update = Instant::now();
    let mut daemon_is_paused = false;

    while !shutdown_requested.load(Ordering::SeqCst) {
        if daemon_control.pause_requested() {
            if !daemon_is_paused {
                dev.take();
                device_guard.take();
                daemon_is_paused = true;
                daemon_control.mark_paused();
                println!("Device preview pause active.");
            }
            while daemon_control.pause_requested() && !shutdown_requested.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(25));
            }
            if shutdown_requested.load(Ordering::SeqCst) {
                break;
            }
            match DeviceGuard::acquire()
                .map_err(anyhow::Error::msg)
                .and_then(|guard| {
                    let mut reopened = device::Device::open()?;
                    reopened.init()?;
                    reopened.set_brightness(if preview_live {
                        preview_brightness
                    } else {
                        100
                    })?;
                    Ok((guard, reopened))
                }) {
                Ok((guard, reopened)) => {
                    device_guard = Some(guard);
                    dev = Some(reopened);
                    daemon_is_paused = false;
                    daemon_control.mark_running();
                    last_sensor_update = Instant::now() - interval;
                    println!("Device preview pause ended.");
                }
                Err(error) => {
                    daemon_control.mark_resume_failed(error.to_string());
                    continue;
                }
            }
        }
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

        if last_sensor_update.elapsed() >= interval {
            values = sensors.read();
            match dev.as_mut().unwrap().read_status() {
                Ok(status) => {
                    values
                        .readings
                        .insert("coolant".to_string(), status.coolant_temp_c);
                    daemon_control.update_telemetry(status.coolant_temp_c, status.pump_rpm);
                }
                Err(error) => {
                    daemon_control.mark_telemetry_error(error.to_string());
                }
            }
            last_sensor_update = Instant::now();
        }

        let jpeg = renderer.render(&cfg, &values);
        dev.as_mut().unwrap().send_frame_data(&jpeg)?;

        let frame_interval = renderer.animation_frame_interval().unwrap_or(interval);
        let elapsed = tick.elapsed();
        if elapsed < frame_interval {
            let deadline = Instant::now() + (frame_interval - elapsed);
            while Instant::now() < deadline
                && !shutdown_requested.load(Ordering::SeqCst)
                && !daemon_control.pause_requested()
            {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50)),
                );
            }
        }
    }
    println!("Graceful shutdown complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_write_gate_only_accepts_supported_one_shot_operations() {
        for args in [
            vec!["th420-display", "--gui-device-job"],
            vec!["th420-display", "--gui-device-job", "--status"],
            vec![
                "th420-display",
                "--gui-device-job",
                "--play-live-gif",
                "file.gif",
            ],
        ] {
            assert!(validate_cli(&Cli::try_parse_from(args).unwrap()).is_err());
        }
        for args in [
            vec![
                "th420-display",
                "--gui-device-job",
                "--standby-brightness",
                "50",
            ],
            vec![
                "th420-display",
                "--gui-device-job",
                "--upload-boot",
                "file.gif",
            ],
        ] {
            assert!(validate_cli(&Cli::try_parse_from(args).unwrap()).is_ok());
        }
    }

    fn cli() -> Cli {
        Cli {
            replace_existing: None,
            daemon_control: None,
            interval: 800,
            config: None,
            preview_live: false,
            gui_device_job: false,
            status: false,
            upload_standby: None,
            standby_brightness: None,
            pump_temp_color: None,
            pump_temp_overlay: None,
            upload_boot: None,
            inspect_boot: None,
            boot_start_frame: 0,
            boot_end_frame: None,
            boot_frame_delay_ms: None,
            play_live_frames: vec![],
            play_live_gif: None,
            live_fps: 24,
            live_loops: 1,
            live_brightness: 80,
            media_fit: MediaFitArg::Stretch,
            media_pan_x: 0.0,
            media_pan_y: 0.0,
            media_zoom: 1.0,
            media_stretch_x: 1.0,
            media_stretch_y: 1.0,
            media_rotation: 0.0,
            media_canvas: "#0a0a14".into(),
            media_time: 0.0,
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
    fn rejects_invalid_media_transform_before_device_work() {
        let mut options = cli();
        options.media_zoom = 0.0;
        assert!(media_transform(&options).is_err());

        options.media_zoom = 1.0;
        options.media_pan_x = f32::NAN;
        assert!(media_transform(&options).is_err());

        options.media_pan_x = 0.0;
        options.media_time = -1.0;
        assert!(media_transform(&options).is_err());
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

        let mut options = cli();
        options.inspect_boot = Some("boot.gif".into());
        options.upload_boot = Some("boot.gif".into());
        assert!(validate_cli(&options).is_err());

        let mut options = cli();
        options.preview_live = true;
        assert!(validate_cli(&options).is_err());
        options.config = Some("preview.toml".into());
        assert!(validate_cli(&options).is_ok());
        options.replace_existing = Some(ReplaceExisting::Term);
        assert!(validate_cli(&options).is_err());
    }

    #[test]
    fn cli_validation_accepts_one_operation_at_a_time() {
        let mut options = cli();
        options.upload_standby = Some("standby.png".into());
        options.pump_temp_color = Some("#0055ff".into());
        options.standby_brightness = Some(80);
        options.pump_temp_overlay = Some(PumpTempOverlay::Hide);
        assert!(validate_cli(&options).is_ok());
    }

    #[test]
    fn replace_existing_parses_all_escalation_ceilings() {
        for (value, expected) in [
            ("graceful", ReplaceExisting::Graceful),
            ("term", ReplaceExisting::Term),
            ("kill", ReplaceExisting::Kill),
        ] {
            let options = Cli::try_parse_from(["th420-display", "--replace-existing", value])
                .expect("replacement level should parse");
            assert_eq!(options.replace_existing, Some(expected));
        }
    }

    #[test]
    fn transform_cli_accepts_negative_numeric_values() {
        let options = Cli::try_parse_from([
            "th420-display",
            "--inspect-boot",
            "boot.gif",
            "--media-pan-x",
            "-12.5",
            "--media-pan-y",
            "-1",
            "--media-rotation",
            "-45",
        ])
        .expect("signed transform values should parse");
        assert_eq!(options.media_pan_x, -12.5);
        assert_eq!(options.media_pan_y, -1.0);
        assert_eq!(options.media_rotation, -45.0);
    }

    #[test]
    fn boot_frame_range_is_inclusive_and_validated() {
        let edit = BootEdit {
            start_frame: 2,
            end_frame: Some(4),
            frame_delay_ms: Some(100),
        };
        assert_eq!(boot_frame_range(8, edit).unwrap(), 2..5);

        assert!(boot_frame_range(
            8,
            BootEdit {
                start_frame: 5,
                end_frame: Some(4),
                frame_delay_ms: Some(100),
            }
        )
        .is_err());
        assert!(boot_frame_range(
            8,
            BootEdit {
                start_frame: 0,
                end_frame: None,
                frame_delay_ms: Some(79),
            }
        )
        .is_err());
    }

    #[test]
    fn boot_decode_applies_trim_and_uniform_timing() {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, Rgba, RgbaImage};

        let path = std::env::temp_dir().join(format!(
            "th420-boot-edit-{}-{:?}.gif",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = File::create(&path).unwrap();
        let mut encoder = GifEncoder::new(file);
        for (red, delay) in [(10, 80), (20, 90), (30, 100), (40, 110)] {
            encoder
                .encode_frame(Frame::from_parts(
                    RgbaImage::from_pixel(2, 2, Rgba([red, 0, 0, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay, 1),
                ))
                .unwrap();
        }
        drop(encoder);

        let (frames, delay) = decode_boot_gif(
            &path,
            &MediaTransform::default(),
            BootEdit {
                start_frame: 1,
                end_frame: Some(2),
                frame_delay_ms: Some(125),
            },
        )
        .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(delay, 125);

        let preview_frames = decode_gif(
            &path,
            &MediaTransform::default(),
            BootEdit {
                start_frame: 1,
                end_frame: Some(2),
                frame_delay_ms: Some(125),
            },
        )
        .unwrap();
        assert_eq!(preview_frames.len(), frames.len());
        assert!(preview_frames
            .iter()
            .all(|(_, delay)| *delay == Duration::from_millis(125)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn replacement_is_rejected_for_one_shot_commands() {
        let mut options = cli();
        options.replace_existing = Some(ReplaceExisting::Kill);
        options.status = true;
        assert!(validate_cli(&options).is_err());
    }

    #[test]
    fn daemon_control_is_a_one_shot_exclusive_operation() {
        let options = Cli::try_parse_from(["th420-display", "--daemon-control", "pause"]).unwrap();
        assert_eq!(options.daemon_control, Some(DaemonControlArg::Pause));
        assert!(options.is_one_shot());

        let mut options = cli();
        options.daemon_control = Some(DaemonControlArg::State);
        options.upload_boot = Some("boot.gif".into());
        assert!(validate_cli(&options).is_err());
    }

    #[test]
    fn status_is_exclusive_with_display_operations() {
        let mut options = cli();
        options.status = true;
        options.upload_standby = Some("standby.png".into());
        assert!(validate_cli(&options).is_err());
    }
}
