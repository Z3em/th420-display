use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::codecs::gif::GifDecoder;
use image::{imageops, AnimationDecoder, Rgb, RgbImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_line_segment_mut, draw_text_mut};
use imageproc::geometric_transformations::{rotate_about_center, Interpolation};
use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use crate::config::{BackgroundConfig, Config, ImageFit, MediaTransform, Transform2D};
use crate::sensors::SensorValues;

const FONT_BYTES: &[u8] = include_bytes!("../assets/NotoSans-Bold.ttf");

const W: u32 = 480;
const H: u32 = 480;
const MAX_DEVICE_FRAME_RATE: f64 = 24.0;

const BG: Rgb<u8> = Rgb([10, 10, 20]);
const CIRCLE_BG: Rgb<u8> = Rgb([15, 15, 28]);
const DIVIDER: Rgb<u8> = Rgb([45, 45, 68]);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragSnapState {
    pub origin: [f32; 2],
    pub raw: [f32; 2],
    pub preview: [f32; 2],
    moved: bool,
}

impl DragSnapState {
    pub fn new(origin: [f32; 2]) -> Self {
        Self {
            origin,
            raw: origin,
            preview: origin,
            moved: false,
        }
    }

    pub fn update(&mut self, delta: [f32; 2], snapping: bool, grid: f32) {
        self.raw[0] += delta[0];
        self.raw[1] += delta[1];
        self.moved |= delta[0] != 0.0 || delta[1] != 0.0;
        self.preview = if snapping {
            [snap_value(self.raw[0], grid), snap_value(self.raw[1], grid)]
        } else {
            self.raw
        };
    }

    pub fn finish(self, snapping: bool, grid: f32) -> [f32; 2] {
        if !self.moved {
            self.origin
        } else if snapping {
            [snap_value(self.raw[0], grid), snap_value(self.raw[1], grid)]
        } else {
            self.raw
        }
    }
}

pub fn snap_value(value: f32, grid: f32) -> f32 {
    if grid <= 0.0 {
        value
    } else {
        (value / grid).round() * grid
    }
}

#[derive(Clone, Debug, PartialEq)]
struct BackgroundGeometryKey {
    fit: ImageFit,
    zoom: f32,
    stretch_x: f32,
    stretch_y: f32,
    rotation: f32,
    background_color: [u8; 3],
}

enum AnimatedSource {
    Static {
        image: RgbImage,
        delivered: bool,
    },
    Gif {
        frames: Vec<(RgbImage, Duration)>,
        total: Duration,
        started: Instant,
        current: usize,
    },
    Ffmpeg {
        child: Child,
        frames: Receiver<RgbImage>,
        current: Option<RgbImage>,
    },
}

pub struct TimedAnimation {
    frames: Vec<(RgbImage, Duration)>,
    total: Duration,
    started: Instant,
}

impl TimedAnimation {
    pub fn load_gif(path: &Path) -> Result<Self, String> {
        let decoder = GifDecoder::new(BufReader::new(File::open(path).map_err(|e| e.to_string())?))
            .map_err(|e| e.to_string())?;
        let mut total = Duration::ZERO;
        let frames = decoder
            .into_frames()
            .collect_frames()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|frame| {
                let (numerator, denominator) = frame.delay().numer_denom_ms();
                let delay = if denominator == 0 {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs_f64(f64::from(numerator) / f64::from(denominator) / 1000.0)
                }
                .max(Duration::from_millis(10));
                total += delay;
                (
                    image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8(),
                    delay,
                )
            })
            .collect::<Vec<_>>();
        if frames.is_empty() {
            return Err("boot animation contains no frames".into());
        }
        Ok(Self {
            frames,
            total,
            started: Instant::now(),
        })
    }

    pub fn current_frame(&self) -> &RgbImage {
        let elapsed = self.started.elapsed().as_secs_f64() % self.total.as_secs_f64();
        let mut accumulated = 0.0;
        let index = self
            .frames
            .iter()
            .position(|(_, delay)| {
                accumulated += delay.as_secs_f64();
                elapsed < accumulated
            })
            .unwrap_or(self.frames.len() - 1);
        &self.frames[index].0
    }
}

impl AnimatedSource {
    fn frame_interval(&self) -> Option<Duration> {
        match self {
            Self::Static { .. } => None,
            Self::Gif {
                frames, current, ..
            } => Some(
                frames
                    .get(*current)
                    .or_else(|| frames.first())
                    .map(|(_, delay)| *delay)
                    .unwrap_or(Duration::from_millis(100))
                    .max(Duration::from_secs_f64(1.0 / MAX_DEVICE_FRAME_RATE)),
            ),
            // FFmpeg preserves source timing with -re. Poll often enough for
            // smooth video without exceeding the hardware-tested 24 FPS.
            Self::Ffmpeg { .. } => Some(Duration::from_secs_f64(1.0 / MAX_DEVICE_FRAME_RATE)),
        }
    }

    fn open(path: &str) -> Result<Self, String> {
        if Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
        {
            let decoder =
                GifDecoder::new(BufReader::new(File::open(path).map_err(|e| e.to_string())?))
                    .map_err(|e| e.to_string())?;
            let mut total = Duration::ZERO;
            let frames = decoder
                .into_frames()
                .collect_frames()
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|frame| {
                    let (numerator, denominator) = frame.delay().numer_denom_ms();
                    let delay = if denominator == 0 {
                        Duration::from_millis(100)
                    } else {
                        Duration::from_secs_f64(
                            f64::from(numerator) / f64::from(denominator) / 1000.0,
                        )
                    }
                    .max(Duration::from_millis(10));
                    total += delay;
                    (
                        image::DynamicImage::ImageRgba8(frame.into_buffer()).into_rgb8(),
                        delay,
                    )
                })
                .collect::<Vec<_>>();
            if frames.is_empty() {
                return Err("animated image contains no frames".into());
            }
            return Ok(Self::Gif {
                frames,
                total,
                started: Instant::now(),
                current: usize::MAX,
            });
        }
        if let Ok(image) = image::open(path) {
            return Ok(Self::Static {
                image: image.into_rgb8(),
                delivered: false,
            });
        }
        Self::open_ffmpeg(path)
    }

    fn open_ffmpeg(path: &str) -> Result<Self, String> {
        let mut child = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-stream_loop",
                "-1",
                "-re",
                "-i",
                path,
                "-f",
                "image2pipe",
                "-vcodec",
                "mjpeg",
                "-q:v",
                "2",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "This media type requires FFmpeg. Install ffmpeg and try again.".to_string()
                } else {
                    format!("failed to start FFmpeg: {error}")
                }
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "FFmpeg stdout unavailable".to_string())?;
        let (tx, frames) = mpsc::sync_channel(1);
        std::thread::spawn(move || read_mjpeg_frames(stdout, tx));
        Ok(Self::Ffmpeg {
            child,
            frames,
            current: None,
        })
    }

    fn next_frame(&mut self) -> Option<RgbImage> {
        match self {
            Self::Static { image, delivered } => {
                if *delivered {
                    None
                } else {
                    *delivered = true;
                    Some(image.clone())
                }
            }
            Self::Gif {
                frames,
                total,
                started,
                current,
            } => {
                let elapsed = started.elapsed().as_secs_f64() % total.as_secs_f64();
                let mut accumulated = 0.0;
                let index = frames
                    .iter()
                    .position(|(_, delay)| {
                        accumulated += delay.as_secs_f64();
                        elapsed < accumulated
                    })
                    .unwrap_or(frames.len() - 1);
                if index == *current {
                    None
                } else {
                    *current = index;
                    Some(frames[index].0.clone())
                }
            }
            Self::Ffmpeg {
                frames, current, ..
            } => {
                let mut newest = None;
                while let Ok(frame) = frames.try_recv() {
                    newest = Some(frame);
                }
                if let Some(frame) = newest {
                    *current = Some(frame.clone());
                    Some(frame)
                } else {
                    None
                }
            }
        }
    }
}

impl Drop for AnimatedSource {
    fn drop(&mut self) {
        if let Self::Ffmpeg { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_mjpeg_frames(mut reader: impl Read, tx: SyncSender<RgbImage>) {
    let mut pending = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    while let Ok(count) = reader.read(&mut chunk) {
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..count]);
        while let Some(end) = pending.windows(2).position(|bytes| bytes == [0xff, 0xd9]) {
            let jpeg = pending.drain(..end + 2).collect::<Vec<_>>();
            if let Ok(image) = image::load_from_memory(&jpeg) {
                let _ = tx.try_send(image.into_rgb8());
            }
        }
    }
}

impl From<&BackgroundConfig> for BackgroundGeometryKey {
    fn from(config: &BackgroundConfig) -> Self {
        Self {
            fit: config.fit.clone(),
            zoom: config.zoom,
            stretch_x: config.stretch_x,
            stretch_y: config.stretch_y,
            rotation: config.rotation,
            background_color: config.background_color,
        }
    }
}

pub struct Renderer {
    font_bytes: Vec<u8>,
    /// Decoded source media, independent of presentation transforms.
    bg_source: Option<(String, AnimatedSource)>,
    bg_source_cache: Option<(String, RgbImage)>,
    /// Scaled and rotated source, independent of pan and post-processing.
    bg_geometry_cache: Option<(BackgroundGeometryKey, RgbImage)>,
    /// Cached background image together with all transformation parameters.
    bg_cache: Option<(BackgroundConfig, RgbImage)>,
    background_error: Option<String>,
    background_failed_path: Option<String>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            font_bytes: FONT_BYTES.to_vec(),
            bg_source: None,
            bg_source_cache: None,
            bg_geometry_cache: None,
            bg_cache: None,
            background_error: None,
            background_failed_path: None,
        }
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
        img.write_to(&mut buf, image::ImageFormat::Jpeg)
            .expect("jpeg encode failed");
        buf.into_inner()
    }

    /// Recommended device refresh cadence for the active animated background.
    /// Sensor collection remains independently controlled by the daemon interval.
    pub fn animation_frame_interval(&self) -> Option<Duration> {
        self.bg_source
            .as_ref()
            .and_then(|(_, source)| source.frame_interval())
    }

    fn update_bg_cache(&mut self, config: &Config) {
        if !config.background.enabled {
            self.bg_source = None;
            self.bg_cache = None;
            return;
        }

        let Some(path) = config.background.image_path.as_deref() else {
            self.bg_source = None;
            self.bg_source_cache = None;
            self.bg_geometry_cache = None;
            self.bg_cache = None;
            return;
        };
        let source_matches = self
            .bg_source
            .as_ref()
            .is_some_and(|(cached_path, _)| cached_path == path);
        if !source_matches {
            if self.background_failed_path.as_deref() == Some(path) {
                return;
            }
            match AnimatedSource::open(path) {
                Ok(source) => {
                    self.bg_source = Some((path.to_string(), source));
                    self.bg_source_cache = None;
                    self.background_error = None;
                    self.background_failed_path = None;
                }
                Err(error) => {
                    self.background_error = Some(error);
                    self.background_failed_path = Some(path.to_string());
                    return;
                }
            }
            self.bg_geometry_cache = None;
            self.bg_cache = None;
        }

        let frame_changed = self
            .bg_source
            .as_mut()
            .and_then(|(_, source)| source.next_frame())
            .map(|frame| {
                self.bg_source_cache = Some((path.to_string(), frame));
                self.bg_geometry_cache = None;
                self.bg_cache = None;
            })
            .is_some();
        if !frame_changed
            && self.bg_cache.as_ref().map(|(background, _)| background) == Some(&config.background)
        {
            return;
        }

        let geometry_key = BackgroundGeometryKey::from(&config.background);
        let geometry_matches = self
            .bg_geometry_cache
            .as_ref()
            .is_some_and(|(cached_key, _)| cached_key == &geometry_key);
        if !geometry_matches {
            self.bg_geometry_cache = self.bg_source_cache.as_ref().and_then(|(_, source)| {
                prepare_background_geometry(source, &config.background)
                    .map(|image| (geometry_key, image))
            });
        }

        self.bg_cache = self.bg_geometry_cache.as_ref().map(|(_, prepared)| {
            (
                config.background.clone(),
                compose_background_image(prepared, &config.background),
            )
        });
    }

    pub fn take_background_error(&mut self) -> Option<String> {
        self.background_error.take()
    }

    fn render_base(&self, config: &Config, v: &SensorValues) -> RgbImage {
        let font = FontRef::try_from_slice(&self.font_bytes).expect("invalid font");

        // ── Base layer ────────────────────────────────────────────────────────
        let mut img = if config.background.enabled {
            if let Some((_, ref bg)) = self.bg_cache {
                bg.clone()
            } else {
                RgbImage::from_pixel(W, H, BG)
            }
        } else {
            RgbImage::from_pixel(W, H, BG)
        };

        // When enabled without an image, the configured canvas color becomes
        // the solid-color source used by the GUI.
        if config.background.enabled && self.bg_cache.is_none() {
            draw_filled_circle_mut(
                &mut img,
                (240, 240),
                238,
                Rgb(config.background.background_color),
            );
        }

        if !config.overlay_enabled {
            return img;
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
            let Some(sc) = config.sensor_by_id(&slot.sensor_id) else {
                continue;
            };
            if !sc.enabled {
                continue;
            }
            let Some(&raw) = v.readings.get(&slot.sensor_id) else {
                continue;
            };

            let vc = Rgb(sc.value_color(raw));
            let lc = Rgb(sc.label_color);
            let val_str = format_value(&sc.unit, raw);

            draw_centered(
                &mut img,
                &font,
                &val_str,
                slot.value_cx,
                slot.value_y,
                slot.value_fs,
                vc,
            );
            draw_centered(
                &mut img,
                &font,
                &sc.label,
                slot.label_cx,
                slot.label_y,
                slot.label_fs,
                lc,
            );
        }

        img
    }
}

// ── Background image loading ──────────────────────────────────────────────────
fn load_background_image(config: &BackgroundConfig) -> Option<RgbImage> {
    let path = config.image_path.as_deref()?;
    let source = load_background_source(path)?;
    transform_background_image(&source, config)
}

fn load_background_source(path: &str) -> Option<RgbImage> {
    image::open(path)
        .map(|image| image.into_rgb8())
        .ok()
        .or_else(|| load_ffmpeg_first_frame(path))
}

/// Decode one frame through the optional FFmpeg runtime when image does not
/// support the selected file. A missing or failing binary simply leaves the
/// background unchanged; the GUI presents the actionable error at selection.
fn load_ffmpeg_first_frame(path: &str) -> Option<RgbImage> {
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            path,
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ])
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    image::load_from_memory(&output.stdout)
        .ok()
        .map(|image| image.into_rgb8())
}

pub fn media_duration(path: &Path) -> Result<f64, String> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
    {
        let decoder = GifDecoder::new(BufReader::new(File::open(path).map_err(|e| e.to_string())?))
            .map_err(|e| e.to_string())?;
        let mut duration = 0.0;
        for frame in decoder
            .into_frames()
            .collect_frames()
            .map_err(|e| e.to_string())?
        {
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            if denominator != 0 {
                duration += f64::from(numerator) / f64::from(denominator) / 1000.0;
            }
        }
        return Ok(duration);
    }
    if image::open(path).is_ok() {
        return Ok(0.0);
    }
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .map_err(|error| format!("failed to run ffprobe: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| "ffprobe returned an invalid duration".to_string())
}

pub fn load_media_frame_at(path: &Path, seconds: f64) -> Result<RgbImage, String> {
    let is_gif = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"));
    if is_gif {
        let decoder = GifDecoder::new(BufReader::new(File::open(path).map_err(|e| e.to_string())?))
            .map_err(|e| e.to_string())?;
        let frames = decoder
            .into_frames()
            .collect_frames()
            .map_err(|e| e.to_string())?;
        if frames.is_empty() {
            return Err("animated image contains no frames".into());
        }
        let target = seconds.max(0.0);
        let mut elapsed = 0.0;
        let index = frames
            .iter()
            .position(|frame| {
                let (numerator, denominator) = frame.delay().numer_denom_ms();
                if denominator != 0 {
                    elapsed += f64::from(numerator) / f64::from(denominator) / 1000.0;
                }
                target < elapsed
            })
            .unwrap_or(frames.len() - 1);
        return Ok(image::DynamicImage::ImageRgba8(frames[index].buffer().clone()).into_rgb8());
    }
    if let Ok(image) = image::open(path) {
        return Ok(image.into_rgb8());
    }
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-ss", &seconds.max(0.0).to_string(), "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ])
        .output()
        .map_err(|error| format!("failed to run FFmpeg: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    image::load_from_memory(&output.stdout)
        .map(|image| image.into_rgb8())
        .map_err(|error| error.to_string())
}

/// Transform one decoded frame into the 480x480 background canvas.
pub fn transform_background_image(src: &RgbImage, config: &BackgroundConfig) -> Option<RgbImage> {
    let prepared = prepare_background_geometry(src, config)?;
    Some(compose_background_image(&prepared, config))
}

/// Apply the shared fit/zoom/stretch/rotation/pan geometry to one media frame.
pub fn transform_media_image(src: &RgbImage, media: &MediaTransform) -> Option<RgbImage> {
    let prepared =
        prepare_transform_geometry(src, &media.fit, &media.transform, media.canvas_color)?;
    Some(compose_transform_image(
        &prepared,
        &media.transform,
        media.canvas_color,
    ))
}

fn prepare_background_geometry(src: &RgbImage, config: &BackgroundConfig) -> Option<RgbImage> {
    prepare_transform_geometry(
        src,
        &config.fit,
        &config.transform(),
        config.background_color,
    )
}

fn prepare_transform_geometry(
    src: &RgbImage,
    fit: &ImageFit,
    transform: &Transform2D,
    canvas_color: [u8; 3],
) -> Option<RgbImage> {
    let (sw, sh) = (src.width(), src.height());
    if sw == 0 || sh == 0 {
        return None;
    }
    let zoom = transform.zoom.clamp(0.05, 8.0);
    let stretch_x = transform.stretch_x.clamp(0.05, 8.0);
    let stretch_y = transform.stretch_y.clamp(0.05, 8.0);
    let (base_w, base_h) = match fit {
        ImageFit::Cover => {
            let scale = (W as f32 / sw as f32).max(H as f32 / sh as f32) * zoom;
            (sw as f32 * scale, sh as f32 * scale)
        }
        ImageFit::Contain => {
            let scale = (W as f32 / sw as f32).min(H as f32 / sh as f32) * zoom;
            (sw as f32 * scale, sh as f32 * scale)
        }
        ImageFit::Stretch => (W as f32 * zoom, H as f32 * zoom),
    };
    let nw = (base_w * stretch_x).round().clamp(1.0, 8192.0) as u32;
    let nh = (base_h * stretch_y).round().clamp(1.0, 8192.0) as u32;
    let scaled = imageops::resize(src, nw, nh, imageops::FilterType::Lanczos3);
    let canvas_color = Rgb(canvas_color);
    Some(if transform.rotation.abs() > 0.01 {
        rotate_about_center(
            &scaled,
            transform.rotation.to_radians(),
            Interpolation::Bilinear,
            canvas_color,
        )
    } else {
        scaled
    })
}

fn compose_background_image(prepared: &RgbImage, config: &BackgroundConfig) -> RgbImage {
    let mut out = compose_transform_image(prepared, &config.transform(), config.background_color);

    if config.blur_sigma > 0.01 {
        out = imageops::blur(&out, config.blur_sigma.clamp(0.0, 50.0));
    }
    let opacity = config.opacity as f32 / 255.0;
    let darken = (255 - config.overlay_alpha as u16) as f32 / 255.0;
    for pixel in out.pixels_mut() {
        for channel in 0..3 {
            let blended = pixel[channel] as f32 * opacity
                + config.background_color[channel] as f32 * (1.0 - opacity);
            pixel[channel] = (blended * darken).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn compose_transform_image(
    prepared: &RgbImage,
    transform: &Transform2D,
    canvas_color: [u8; 3],
) -> RgbImage {
    let canvas_color = Rgb(canvas_color);
    let mut out = RgbImage::from_pixel(W, H, canvas_color);
    let ox = ((W as f32 - prepared.width() as f32) / 2.0 + transform.pan_x).round() as i64;
    let oy = ((H as f32 - prepared.height() as f32) / 2.0 + transform.pan_y).round() as i64;
    imageops::overlay(&mut out, prepared, ox, oy);
    out
}

// ── Value formatting ──────────────────────────────────────────────────────────

pub fn format_value(unit: &str, value: f32) -> String {
    match unit {
        "°C" => format!("{:.0}°C", value),
        "%" => format!("{:.0}%", value),
        "GHz" => format!("{:.2}G", value),
        "W" => format!("{:.0}W", value.min(999.0)),
        "GB" => format!("{:.1}G", value),
        _ => format!("{:.1}", value),
    }
}

// ── Geometry helpers ──────────────────────────────────────────────────────────

fn apply_rotation(img: RgbImage, degrees: f32) -> RgbImage {
    if degrees == 0.0 {
        return img;
    }
    rotate_about_center(
        &img,
        degrees.to_radians(),
        Interpolation::Bilinear,
        Rgb([0, 0, 0]),
    )
}

fn draw_centered(
    img: &mut RgbImage,
    font: &FontRef,
    text: &str,
    cx: i32,
    y_top: i32,
    size: f32,
    color: Rgb<u8>,
) {
    let scale = PxScale::from(size);
    let width = measure_width(font, scale, text);
    let x = cx - (width / 2.0) as i32;
    draw_text_mut(img, color, x, y_top, scale, font, text);
}

fn measure_width(font: &FontRef, scale: PxScale, text: &str) -> f32 {
    let scaled = font.as_scaled(scale);
    text.chars()
        .map(|c| scaled.h_advance(font.glyph_id(c)))
        .sum()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LayoutPreset};
    use std::collections::HashMap;

    fn make_values(pairs: &[(&str, f32)]) -> SensorValues {
        SensorValues {
            readings: pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    fn normal() -> SensorValues {
        make_values(&[
            ("cpu_temp", 55.0),
            ("coolant", 28.5),
            ("cpu_freq", 3.8),
            ("cpu_util", 42.0),
            ("cpu_power", 65.0),
            ("gpu_temp", 62.0),
        ])
    }

    fn triple_digit() -> SensorValues {
        make_values(&[
            ("cpu_temp", 105.0),
            ("coolant", 55.0),
            ("cpu_freq", 5.9),
            ("cpu_util", 99.0),
            ("cpu_power", 250.0),
            ("gpu_temp", 112.0),
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
        let img = Renderer::new().render_preview(
            &Config::default(),
            &SensorValues {
                readings: HashMap::new(),
            },
        );
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_all_sensors_disabled_no_panic() {
        let mut config = Config::default();
        for s in &mut config.sensors {
            s.enabled = false;
        }
        let img = Renderer::new().render_preview(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_negative_sensor_values_no_panic() {
        let img = Renderer::new().render_preview(
            &Config::default(),
            &make_values(&[("cpu_temp", -5.0), ("gpu_temp", -3.0), ("cpu_power", -10.0)]),
        );
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_extended_sensors_enabled_no_panic() {
        let mut config = Config::default();
        for s in &mut config.sensors {
            s.enabled = true;
        }
        config.layout.max_visible = 8;
        let img = Renderer::new().render_preview(&config, &with_extended());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    // NOTE: render_preview now takes &mut self — temporary is auto-reborrowed as mut

    // ── render_image (with rotation) ─────────────────────────────────────────

    #[test]
    fn render_image_rotation_0_no_panic() {
        let config = Config {
            rotation: 0.0,
            ..Default::default()
        };
        let img = Renderer::new().render_image(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
    }

    #[test]
    fn render_image_rotation_180_no_panic() {
        let config = Config {
            rotation: 180.0,
            ..Default::default()
        };
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
        let config = Config {
            rotation: 90.0,
            ..Default::default()
        };
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
        assert_eq!(format_value("W", 65.0), "65W");
    }

    // ── background image loading ──────────────────────────────────────────────

    #[test]
    fn load_background_cover_produces_480x480() {
        let tmp = std::env::temp_dir().join("th420_bg_test.png");
        image::RgbImage::from_pixel(800, 600, Rgb([100, 150, 200]))
            .save(&tmp)
            .unwrap();
        let background = BackgroundConfig {
            image_path: Some(tmp.to_string_lossy().into_owned()),
            fit: ImageFit::Cover,
            ..Default::default()
        };
        let img = load_background_image(&background).unwrap();
        assert_eq!((img.width(), img.height()), (480, 480));
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn load_background_contain_produces_480x480() {
        let tmp = std::env::temp_dir().join("th420_bg_contain.png");
        image::RgbImage::from_pixel(300, 300, Rgb([50, 100, 150]))
            .save(&tmp)
            .unwrap();
        let background = BackgroundConfig {
            image_path: Some(tmp.to_string_lossy().into_owned()),
            fit: ImageFit::Contain,
            overlay_alpha: 128,
            ..Default::default()
        };
        let img = load_background_image(&background).unwrap();
        assert_eq!((img.width(), img.height()), (480, 480));
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn load_background_invalid_path_returns_none() {
        let background = BackgroundConfig {
            image_path: Some("/nonexistent/file.png".to_string()),
            ..Default::default()
        };
        assert!(load_background_image(&background).is_none());
    }

    #[test]
    fn render_with_background_image_no_panic() {
        let tmp = std::env::temp_dir().join("th420_bg_render.png");
        image::RgbImage::from_pixel(480, 480, Rgb([30, 30, 40]))
            .save(&tmp)
            .unwrap();
        let mut config = Config::default();
        config.background.image_path = Some(tmp.to_str().unwrap().to_string());
        // update_bg_cache is called only by render_image, so call it manually here
        let mut r = Renderer::new();
        r.update_bg_cache(&config);
        let img = r.render_preview(&config, &normal());
        assert_eq!((img.width(), img.height()), (480, 480));
        let _ = std::fs::remove_file(tmp);
    }
    #[test]
    fn transformed_background_applies_pixel_pan() {
        let source = RgbImage::from_fn(480, 480, |x, _| {
            if x < 240 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        });
        let mut background = BackgroundConfig::default();
        background.fit = ImageFit::Stretch;
        background.overlay_alpha = 0;
        background.pan_x = 120.0;
        let rendered = transform_background_image(&source, &background).unwrap();
        assert_eq!(rendered.get_pixel(120, 240), &Rgb([255, 0, 0]));
        assert_eq!(rendered.get_pixel(360, 240), &Rgb([0, 0, 255]));
    }

    #[test]
    fn transformed_background_accepts_rotation_and_stretch() {
        let source = RgbImage::from_pixel(32, 64, Rgb([80, 120, 160]));
        let background = BackgroundConfig {
            fit: ImageFit::Contain,
            stretch_x: 1.5,
            stretch_y: 0.75,
            rotation: 30.0,
            overlay_alpha: 0,
            ..Default::default()
        };
        let rendered = transform_background_image(&source, &background).unwrap();
        assert_eq!((rendered.width(), rendered.height()), (480, 480));
    }

    #[test]
    fn media_and_background_adapters_share_transform_geometry() {
        let source = RgbImage::from_fn(120, 80, |x, y| {
            Rgb([(x % 255) as u8, (y % 255) as u8, ((x + y) % 255) as u8])
        });
        let mut background = BackgroundConfig::default();
        background.fit = ImageFit::Contain;
        background.overlay_alpha = 0;
        background.pan_x = 17.0;
        background.pan_y = -11.0;
        background.zoom = 1.25;
        background.stretch_x = 0.8;
        background.stretch_y = 1.1;
        background.rotation = 19.0;
        background.background_color = [3, 7, 11];

        let media = MediaTransform {
            fit: background.fit.clone(),
            transform: background.transform(),
            canvas_color: background.background_color,
        };
        assert_eq!(
            transform_background_image(&source, &background).unwrap(),
            transform_media_image(&source, &media).unwrap()
        );
    }

    #[test]
    fn drag_snap_state_is_target_independent() {
        let mut boot = DragSnapState::new([0.0, 0.0]);
        let standby = DragSnapState::new([5.5, 7.5]);
        boot.update([10.0, 6.0], true, 8.0);

        assert_eq!(boot.raw, [10.0, 6.0]);
        assert_eq!(boot.preview, [8.0, 8.0]);
        assert_eq!(boot.finish(true, 8.0), [8.0, 8.0]);
        assert_eq!(standby.finish(true, 12.0), [5.5, 7.5]);
    }

    #[test]
    fn animated_source_advances_frames_from_elapsed_time() {
        let red = RgbImage::from_pixel(1, 1, Rgb([255, 0, 0]));
        let blue = RgbImage::from_pixel(1, 1, Rgb([0, 0, 255]));
        let mut source = AnimatedSource::Gif {
            frames: vec![
                (red.clone(), Duration::from_millis(100)),
                (blue.clone(), Duration::from_millis(100)),
            ],
            total: Duration::from_millis(200),
            started: Instant::now(),
            current: usize::MAX,
        };

        assert_eq!(source.next_frame().unwrap(), red);
        if let AnimatedSource::Gif { started, .. } = &mut source {
            *started = Instant::now() - Duration::from_millis(150);
        }
        assert_eq!(source.next_frame().unwrap(), blue);
        assert!(source.next_frame().is_none());
    }

    #[test]
    fn animated_source_reports_its_frame_cadence() {
        let gif = AnimatedSource::Gif {
            frames: vec![(
                RgbImage::from_pixel(1, 1, Rgb([0, 0, 0])),
                Duration::from_millis(80),
            )],
            total: Duration::from_millis(80),
            started: Instant::now(),
            current: 0,
        };
        assert_eq!(gif.frame_interval(), Some(Duration::from_millis(80)));

        let still = AnimatedSource::Static {
            image: RgbImage::new(1, 1),
            delivered: false,
        };
        assert_eq!(still.frame_interval(), None);

        let fast_gif = AnimatedSource::Gif {
            frames: vec![(
                RgbImage::from_pixel(1, 1, Rgb([0, 0, 0])),
                Duration::from_millis(10),
            )],
            total: Duration::from_millis(10),
            started: Instant::now(),
            current: 0,
        };
        assert_eq!(
            fast_gif.frame_interval(),
            Some(Duration::from_secs_f64(1.0 / MAX_DEVICE_FRAME_RATE))
        );
    }

    #[test]
    fn static_source_delivers_only_one_cache_invalidation() {
        let image = RgbImage::from_pixel(1, 1, Rgb([1, 2, 3]));
        let mut source = AnimatedSource::Static {
            image: image.clone(),
            delivered: false,
        };

        assert_eq!(source.next_frame().unwrap(), image);
        assert!(source.next_frame().is_none());
    }

    #[test]
    fn mjpeg_reader_delivers_decodable_frames() {
        let source = RgbImage::from_pixel(3, 2, Rgb([20, 40, 60]));
        let mut encoded = Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        let (tx, rx) = mpsc::sync_channel(1);

        read_mjpeg_frames(Cursor::new(encoded.into_inner()), tx);

        let decoded = rx.try_recv().unwrap();
        assert_eq!((decoded.width(), decoded.height()), (3, 2));
    }

    #[test]
    fn gif_duration_and_frame_selection_use_source_timing() {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, Rgba, RgbaImage};

        let path =
            std::env::temp_dir().join(format!("th420-standby-timeline-{}.gif", std::process::id()));
        let file = File::create(&path).unwrap();
        let mut encoder = GifEncoder::new(file);
        encoder.set_repeat(Repeat::Infinite).unwrap();
        encoder
            .encode_frame(Frame::from_parts(
                RgbaImage::from_pixel(2, 1, Rgba([255, 0, 0, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            ))
            .unwrap();
        encoder
            .encode_frame(Frame::from_parts(
                RgbaImage::from_pixel(2, 1, Rgba([0, 0, 255, 255])),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            ))
            .unwrap();
        drop(encoder);

        assert!((media_duration(&path).unwrap() - 0.2).abs() < 0.001);
        assert_eq!(
            load_media_frame_at(&path, 0.15).unwrap().get_pixel(0, 0),
            &Rgb([0, 0, 255])
        );
        let _ = std::fs::remove_file(path);
    }
}
