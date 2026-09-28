use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::codecs::gif::GifDecoder;
use image::{imageops, AnimationDecoder, Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_line_segment_mut, draw_text_mut};
use imageproc::geometric_transformations::{rotate_about_center, Interpolation};
use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use crate::config::{
    effective_label_offset_y, BackgroundConfig, Config, ImageFit, MediaTransform, Transform2D,
};
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
        // egui's Response::drag_delta() is the pointer movement for the current
        // frame, so preserve the unsnapped accumulator across frames.
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
    if !grid.is_finite() || grid <= 0.0 || !value.is_finite() {
        value
    } else {
        (value / grid).round() * grid
    }
}

pub fn centered_grid_coordinates(grid: f32) -> Vec<f32> {
    if !grid.is_finite() || grid <= 0.0 {
        return vec![240.0];
    }
    let mut coordinates = vec![240.0];
    let mut offset = grid;
    while offset <= 240.0 + f32::EPSILON {
        coordinates.push(240.0 - offset);
        coordinates.push(240.0 + offset);
        offset += grid;
    }
    coordinates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    coordinates
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RotationDragState {
    pub origin: f32,
    pub raw: f32,
    pub preview: f32,
    last_pointer_angle: f32,
    moved: bool,
}

impl RotationDragState {
    pub fn new(origin: f32, pointer_angle: f32) -> Self {
        Self {
            origin,
            raw: origin,
            preview: origin,
            last_pointer_angle: pointer_angle,
            moved: false,
        }
    }

    pub fn update(&mut self, pointer_angle: f32, snapping: bool, step: f32) {
        let delta = normalize_angle(pointer_angle - self.last_pointer_angle);
        self.last_pointer_angle = pointer_angle;
        self.moved |= delta.abs() > f32::EPSILON;
        self.raw += delta;
        self.preview = normalize_angle(if snapping {
            snap_value(self.raw, step)
        } else {
            self.raw
        });
    }

    pub fn finish(self, snapping: bool, step: f32) -> f32 {
        if !self.moved {
            self.origin
        } else {
            normalize_angle(if snapping {
                snap_value(self.raw, step)
            } else {
                self.raw
            })
        }
    }
}

pub fn normalize_angle(angle: f32) -> f32 {
    let normalized = (angle + 180.0).rem_euclid(360.0) - 180.0;
    if normalized == -180.0 && angle > 0.0 {
        180.0
    } else {
        normalized
    }
}

pub fn arc_pointer_angle(
    pointer: [f32; 2],
    center: [f32; 2],
    dead_zone_radius: f32,
) -> Option<f32> {
    let dx = pointer[0] - center[0];
    let dy = pointer[1] - center[1];
    (dx * dx + dy * dy >= dead_zone_radius.max(0.0).powi(2)).then(|| dy.atan2(dx).to_degrees())
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
    started: Instant,
}

impl TimedAnimation {
    pub fn load_gif(path: &Path) -> Result<Self, String> {
        let decoder = GifDecoder::new(BufReader::new(File::open(path).map_err(|e| e.to_string())?))
            .map_err(|e| e.to_string())?;
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
            started: Instant::now(),
        })
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, index: usize) -> Option<&RgbImage> {
        self.frames.get(index).map(|(frame, _)| frame)
    }

    pub fn frame_start_seconds(&self, index: usize) -> f64 {
        self.frames
            .iter()
            .take(index.min(self.frames.len()))
            .map(|(_, delay)| delay.as_secs_f64())
            .sum()
    }

    pub fn first_delay_ms(&self) -> u32 {
        self.frames
            .first()
            .map(|(_, delay)| delay.as_millis().clamp(80, u128::from(u32::MAX)) as u32)
            .unwrap_or(100)
    }

    pub fn current_uniform_frame(&self, start: usize, end: usize, delay_ms: u32) -> &RgbImage {
        let start = start.min(self.frames.len() - 1);
        let end = end.clamp(start, self.frames.len() - 1);
        let count = end - start + 1;
        let elapsed_ms = self.started.elapsed().as_millis();
        let index = start + (elapsed_ms / u128::from(delay_ms.max(1)) % count as u128) as usize;
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
            .map_err(ffmpeg_start_error)?;
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

    pub fn widget_bounds(
        &self,
        config: &Config,
        instance_id: &str,
        values: &SensorValues,
    ) -> Option<[f32; 2]> {
        let instance = config
            .widget_instances
            .iter()
            .find(|instance| instance.id == instance_id)?;
        let widget = config.resolved_widget(instance)?;
        let font = FontRef::try_from_slice(&self.font_bytes).ok()?;
        let raw = values.readings.get(&widget.source_id).copied();
        let value = raw
            .map(|value| format_value(&widget.unit, value))
            .unwrap_or_else(|| "--".to_string());
        let [width, height] = widget_layer_dimensions(&font, &widget, &value);
        let width = width * widget.transform.zoom * widget.transform.stretch_x;
        let height = height * widget.transform.zoom * widget.transform.stretch_y;
        let radians = widget.transform.rotation.to_radians();
        Some([
            width * radians.cos().abs() + height * radians.sin().abs(),
            width * radians.sin().abs() + height * radians.cos().abs(),
        ])
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

        if config.widget_model_version > 0 {
            for widget in config.resolved_widgets() {
                if !widget.visible {
                    continue;
                }
                let raw = v.readings.get(&widget.source_id).copied();
                if raw.is_none() && !widget.style.show_missing {
                    continue;
                }
                draw_widget_instance(&mut img, &font, &widget, raw);
            }
            return img;
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

fn draw_widget_instance(
    canvas: &mut RgbImage,
    font: &FontRef,
    widget: &crate::config::ResolvedWidget,
    raw: Option<f32>,
) {
    let value = raw
        .map(|value| format_value(&widget.unit, value))
        .unwrap_or_else(|| "--".to_string());
    let value_color = raw
        .map(|value| crate::config::interpolate_color(value, &widget.style.color_map))
        .unwrap_or([150, 150, 150]);
    let [logical_width, logical_height] = widget_layer_dimensions(font, widget, &value);
    let label_offset_y = effective_label_offset_y(&widget.style);
    let min_y = label_offset_y.min(0.0);
    let width = logical_width.ceil().max(1.0) as u32;
    let height = logical_height.ceil().max(1.0) as u32;
    let mut layer = RgbaImage::new(width, height);
    draw_centered_rgba(
        &mut layer,
        font,
        &value,
        width as i32 / 2,
        (4.0 - min_y).round() as i32,
        widget.style.value_font_size,
        Rgba([value_color[0], value_color[1], value_color[2], 255]),
    );
    draw_centered_rgba(
        &mut layer,
        font,
        &widget.label,
        (width as f32 / 2.0 + widget.style.label_offset_x).round() as i32,
        (4.0 + label_offset_y - min_y).round() as i32,
        widget.style.label_font_size,
        Rgba([
            widget.style.label_color[0],
            widget.style.label_color[1],
            widget.style.label_color[2],
            255,
        ]),
    );

    let transform = widget.transform;
    let scaled_width = (layer.width() as f32 * transform.zoom * transform.stretch_x)
        .round()
        .clamp(1.0, 4096.0) as u32;
    let scaled_height = (layer.height() as f32 * transform.zoom * transform.stretch_y)
        .round()
        .clamp(1.0, 4096.0) as u32;
    let scaled = imageops::resize(
        &layer,
        scaled_width,
        scaled_height,
        imageops::FilterType::Lanczos3,
    );
    let transformed = rotate_rgba_expanded(&scaled, transform.rotation);
    let left = (240.0 + transform.pan_x - transformed.width() as f32 / 2.0).round() as i64;
    let top = (240.0 + transform.pan_y - transformed.height() as f32 / 2.0).round() as i64;
    overlay_rgba(canvas, &transformed, left, top);
}

fn widget_layer_dimensions(
    font: &FontRef,
    widget: &crate::config::ResolvedWidget,
    value: &str,
) -> [f32; 2] {
    let value_width = measure_width(font, PxScale::from(widget.style.value_font_size), value);
    let label_width = measure_width(
        font,
        PxScale::from(widget.style.label_font_size),
        &widget.label,
    );
    let half_width = (value_width / 2.0).max(widget.style.label_offset_x.abs() + label_width / 2.0);
    let label_offset_y = effective_label_offset_y(&widget.style);
    let min_y = label_offset_y.min(0.0);
    let max_y = widget
        .style
        .value_font_size
        .max(label_offset_y + widget.style.label_font_size);
    [
        (half_width * 2.0).ceil().max(1.0) + 8.0,
        (max_y - min_y).ceil().max(1.0) + 8.0,
    ]
}

fn rotate_rgba_expanded(image: &RgbaImage, degrees: f32) -> RgbaImage {
    if degrees.abs() <= 0.01 {
        return image.clone();
    }
    let diagonal = ((image.width() as f32).hypot(image.height() as f32))
        .ceil()
        .max(1.0) as u32;
    let mut padded = RgbaImage::new(diagonal, diagonal);
    imageops::overlay(
        &mut padded,
        image,
        (diagonal as i64 - image.width() as i64) / 2,
        (diagonal as i64 - image.height() as i64) / 2,
    );
    rotate_about_center(
        &padded,
        degrees.to_radians(),
        Interpolation::Bilinear,
        Rgba([0, 0, 0, 0]),
    )
}

fn overlay_rgba(canvas: &mut RgbImage, layer: &RgbaImage, left: i64, top: i64) {
    for (x, y, pixel) in layer.enumerate_pixels() {
        let cx = left + x as i64;
        let cy = top + y as i64;
        if cx < 0 || cy < 0 || cx >= canvas.width() as i64 || cy >= canvas.height() as i64 {
            continue;
        }
        let alpha = pixel[3] as f32 / 255.0;
        if alpha <= 0.0 {
            continue;
        }
        let target = canvas.get_pixel_mut(cx as u32, cy as u32);
        for channel in 0..3 {
            target[channel] = (pixel[channel] as f32 * alpha
                + target[channel] as f32 * (1.0 - alpha))
                .round() as u8;
        }
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
        .map_err(ffmpeg_start_error)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    image::load_from_memory(&output.stdout)
        .map(|image| image.into_rgb8())
        .map_err(|error| error.to_string())
}

fn ffmpeg_start_error(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        "This media type requires FFmpeg. Install ffmpeg and try again.".to_string()
    } else {
        format!("failed to start FFmpeg: {error}")
    }
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
        ImageFit::Native => (sw as f32 * zoom, sh as f32 * zoom),
    };
    let nw = (base_w * stretch_x).round().clamp(1.0, 8192.0) as u32;
    let nh = (base_h * stretch_y).round().clamp(1.0, 8192.0) as u32;
    let scaled = imageops::resize(src, nw, nh, imageops::FilterType::Lanczos3);
    let canvas_color = Rgb(canvas_color);
    Some(if transform.rotation.abs() > 0.01 {
        rotate_with_expanded_bounds(&scaled, transform.rotation, canvas_color)
    } else {
        scaled
    })
}

/// Rotate without clipping against the source image's unrotated rectangle.
///
/// `imageproc::rotate_about_center` preserves its input dimensions, so rotating
/// a non-square image directly would discard the parts which extend beyond the
/// original bounds. Padding to the rotated bounding box first gives the output
/// enough room while retaining the same center pivot used by pan/composition.
fn rotate_with_expanded_bounds(image: &RgbImage, degrees: f32, fill: Rgb<u8>) -> RgbImage {
    let radians = degrees.to_radians();
    let sin = radians.sin().abs();
    let cos = radians.cos().abs();
    let width = image.width() as f32;
    let height = image.height() as f32;
    let bound_dimension = |value: f32| {
        let rounded = value.round();
        let dimension = if (value - rounded).abs() < 0.001 {
            rounded
        } else {
            value.ceil()
        };
        dimension.max(1.0) as u32
    };
    let rotated_width = bound_dimension(width * cos + height * sin);
    let rotated_height = bound_dimension(width * sin + height * cos);

    // The input must fit before rotation as well as after it. For rotations near
    // 90 degrees, the final bounding dimensions are effectively swapped, so
    // padding directly to only the final dimensions would clip while copying.
    let padded_width = rotated_width.max(image.width());
    let padded_height = rotated_height.max(image.height());
    let mut padded = RgbImage::from_pixel(padded_width, padded_height, fill);
    let x = (padded_width as i64 - image.width() as i64) / 2;
    let y = (padded_height as i64 - image.height() as i64) / 2;
    imageops::overlay(&mut padded, image, x, y);
    let rotated = rotate_about_center(&padded, radians, Interpolation::Bilinear, fill);
    let crop_x = (padded_width - rotated_width) / 2;
    let crop_y = (padded_height - rotated_height) / 2;
    imageops::crop_imm(&rotated, crop_x, crop_y, rotated_width, rotated_height).to_image()
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

fn draw_centered_rgba(
    img: &mut RgbaImage,
    font: &FontRef,
    text: &str,
    cx: i32,
    y_top: i32,
    size: f32,
    color: Rgba<u8>,
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
    fn rotation_expands_non_square_bounds_instead_of_clipping() {
        let source = RgbImage::from_pixel(120, 300, Rgb([240, 10, 20]));
        let rotated = rotate_with_expanded_bounds(&source, 90.0, Rgb([0, 0, 0]));

        assert_eq!((rotated.width(), rotated.height()), (300, 120));
        let retained_source_pixels = rotated
            .pixels()
            .filter(|pixel| pixel[0] > 200 && pixel[1] < 30 && pixel[2] < 30)
            .count();
        assert!(
            retained_source_pixels > 35_000,
            "retained {retained_source_pixels} of 36000 source pixels"
        );
    }

    #[test]
    fn shared_rotation_keeps_center_pivot_and_clips_to_canvas() {
        let canvas = [2, 4, 6];
        let mut source = RgbImage::from_pixel(120, 60, Rgb([200, 20, 30]));
        source.put_pixel(60, 30, Rgb([0, 255, 0]));
        let media = MediaTransform {
            fit: ImageFit::Native,
            transform: Transform2D {
                rotation: 45.0,
                ..Default::default()
            },
            canvas_color: canvas,
        };

        let rendered = transform_media_image(&source, &media).unwrap();
        assert_eq!((rendered.width(), rendered.height()), (480, 480));
        assert_ne!(rendered.get_pixel(240, 240), &Rgb(canvas));
        for corner in [(0, 0), (479, 0), (0, 479), (479, 479)] {
            assert_eq!(rendered.get_pixel(corner.0, corner.1), &Rgb(canvas));
        }
    }

    #[test]
    fn native_transform_exposes_canvas_fill_around_source() {
        let source = RgbImage::from_pixel(20, 10, Rgb([200, 100, 50]));
        let media = MediaTransform {
            fit: ImageFit::Native,
            transform: Transform2D::default(),
            canvas_color: [7, 11, 13],
        };
        let rendered = transform_media_image(&source, &media).unwrap();
        assert_eq!(rendered.get_pixel(0, 0), &Rgb([7, 11, 13]));
        assert_eq!(rendered.get_pixel(240, 240), &Rgb([200, 100, 50]));
    }

    #[test]
    fn background_cache_invalidates_only_the_affected_layer() {
        let path = std::env::temp_dir().join(format!(
            "th420-transform-cache-{}-{:?}.png",
            std::process::id(),
            std::thread::current().id()
        ));
        RgbImage::from_pixel(24, 12, Rgb([90, 120, 150]))
            .save(&path)
            .unwrap();
        let mut config = Config::default();
        config.background.image_path = Some(path.to_string_lossy().into_owned());
        config.background.fit = ImageFit::Native;
        config.background.overlay_alpha = 0;
        let mut renderer = Renderer::new();

        renderer.update_bg_cache(&config);
        let original_geometry = renderer.bg_geometry_cache.as_ref().unwrap().clone();
        let original_composed = renderer.bg_cache.as_ref().unwrap().1.clone();

        config.background.pan_x = 40.0;
        renderer.update_bg_cache(&config);
        assert_eq!(
            renderer.bg_geometry_cache.as_ref().unwrap(),
            &original_geometry
        );
        assert_ne!(renderer.bg_cache.as_ref().unwrap().1, original_composed);

        config.background.background_color = [1, 2, 3];
        renderer.update_bg_cache(&config);
        assert_ne!(
            renderer.bg_geometry_cache.as_ref().unwrap().0,
            original_geometry.0
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_ffmpeg_error_identifies_optional_dependency() {
        let message = ffmpeg_start_error(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(message.contains("requires FFmpeg"));
        assert!(message.contains("Install ffmpeg"));
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
    fn rotation_drag_unwraps_across_angle_boundary() {
        let mut drag = RotationDragState::new(0.0, 179.0);
        drag.update(-179.0, false, 15.0);
        assert!((drag.raw - 2.0).abs() < 0.001);
        assert!((drag.preview - 2.0).abs() < 0.001);
        assert!((drag.finish(false, 15.0) - 2.0).abs() < 0.001);
    }

    #[test]
    fn rotation_drag_commits_snapped_normalized_angle() {
        let mut drag = RotationDragState::new(170.0, 0.0);
        drag.update(20.0, true, 15.0);
        assert_eq!(drag.raw, 190.0);
        assert_eq!(drag.preview, -165.0);
        let committed = drag.finish(true, 15.0);
        assert_eq!(committed, -165.0);
        assert_ne!(committed, snap_value(committed, 30.0));
    }

    #[test]
    fn rotation_click_and_target_states_remain_independent() {
        let background = RotationDragState::new(37.5, 10.0);
        let mut boot = RotationDragState::new(-10.0, 0.0);
        let standby = RotationDragState::new(80.0, 90.0);
        boot.update(32.0, true, 15.0);

        assert_eq!(background.finish(true, 15.0), 37.5);
        assert_eq!(boot.finish(true, 15.0), 15.0);
        assert_eq!(standby.finish(false, 1.0), 80.0);
    }

    #[test]
    fn rotation_arc_ignores_unstable_center_dead_zone() {
        assert_eq!(arc_pointer_angle([5.0, 5.0], [5.0, 5.0], 10.0), None);
        assert_eq!(arc_pointer_angle([15.0, 5.0], [5.0, 5.0], 10.0), Some(0.0));
        assert_eq!(arc_pointer_angle([5.0, 15.0], [5.0, 5.0], 10.0), Some(90.0));
    }

    #[test]
    fn centered_grid_is_symmetric_for_non_divisor_sizes() {
        for grid in [7.0, 8.0, 12.0, 20.0] {
            let coordinates = centered_grid_coordinates(grid);
            assert!(coordinates.contains(&240.0));
            for coordinate in &coordinates {
                let mirror = 480.0 - coordinate;
                assert!(coordinates
                    .iter()
                    .any(|other| (other - mirror).abs() < 0.001));
            }
        }
    }

    #[test]
    fn invalid_grid_values_do_not_create_invalid_snaps() {
        assert_eq!(snap_value(13.0, f32::NAN), 13.0);
        assert_eq!(snap_value(13.0, 0.0), 13.0);
        assert_eq!(centered_grid_coordinates(f32::NAN), [240.0]);
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
