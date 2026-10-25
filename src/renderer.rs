use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use image::codecs::gif::GifDecoder;
use image::{imageops, AnimationDecoder, Rgb, RgbImage, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_line_segment_mut, draw_text_mut};
use imageproc::geometric_transformations::{rotate_about_center, Interpolation};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{
    effective_label_offset_y, BackgroundConfig, Config, ImageFit, MediaTransform, Transform2D,
};
use crate::sensors::SensorValues;

#[path = "background_cache.rs"]
mod background_cache;

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
    pub fn moved(&self) -> bool {
        self.moved
    }
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

/// The actual transformed source footprint, before rotation padding is filled
/// with canvas color. Coordinates are relative to the canvas center.
#[derive(Clone, Debug)]
pub struct MediaFootprint {
    pub corners: [[f32; 2]; 4],
}

impl MediaFootprint {
    pub fn new(source: (u32, u32), fit: &ImageFit, transform: &Transform2D) -> Option<Self> {
        let (sw, sh) = source;
        if sw == 0 || sh == 0 {
            return None;
        }
        let zoom = transform.zoom.clamp(0.05, 8.0);
        let stretch_x = transform.stretch_x.clamp(0.05, 8.0);
        let stretch_y = transform.stretch_y.clamp(0.05, 8.0);
        let (base_w, base_h) = match fit {
            ImageFit::Cover => {
                let scale = (480.0 / sw as f32).max(480.0 / sh as f32) * zoom;
                (sw as f32 * scale, sh as f32 * scale)
            }
            ImageFit::Contain => {
                let scale = (480.0 / sw as f32).min(480.0 / sh as f32) * zoom;
                (sw as f32 * scale, sh as f32 * scale)
            }
            ImageFit::Stretch => (480.0 * zoom, 480.0 * zoom),
            ImageFit::Native => (sw as f32 * zoom, sh as f32 * zoom),
        };
        let half_w = (base_w * stretch_x).round().clamp(1.0, 8192.0) / 2.0;
        let half_h = (base_h * stretch_y).round().clamp(1.0, 8192.0) / 2.0;
        let (sin, cos) = transform.rotation.to_radians().sin_cos();
        Some(Self {
            corners: [
                [-half_w, -half_h],
                [half_w, -half_h],
                [half_w, half_h],
                [-half_w, half_h],
            ]
            .map(|[x, y]| [x * cos - y * sin, x * sin + y * cos]),
        })
    }

    pub fn pan_region(&self, keep_covered: bool) -> Result<PanRegion, &'static str> {
        const CANVAS: [[f32; 2]; 4] = [
            [-240.0, -240.0],
            [240.0, -240.0],
            [240.0, 240.0],
            [-240.0, 240.0],
        ];
        if !keep_covered {
            let mut sums = Vec::with_capacity(16);
            for canvas in CANVAS {
                for media in self.corners {
                    sums.push([canvas[0] - media[0], canvas[1] - media[1]]);
                }
            }
            return Ok(PanRegion {
                vertices: convex_hull(sums),
            });
        }
        let edge = [
            self.corners[1][0] - self.corners[0][0],
            self.corners[1][1] - self.corners[0][1],
        ];
        let length = edge[0].hypot(edge[1]);
        let u = [edge[0] / length, edge[1] / length];
        let v = [-u[1], u[0]];
        let half_w = length / 2.0;
        let half_h = (self.corners[2][0] - self.corners[1][0])
            .hypot(self.corners[2][1] - self.corners[1][1])
            / 2.0;
        let mut polygon = vec![
            [-20000.0, -20000.0],
            [20000.0, -20000.0],
            [20000.0, 20000.0],
            [-20000.0, 20000.0],
        ];
        for (axis, half) in [(u, half_w), (v, half_h)] {
            let projections = CANVAS.map(|corner| dot(corner, axis));
            let min = projections.into_iter().fold(f32::INFINITY, f32::min);
            let max = projections.into_iter().fold(f32::NEG_INFINITY, f32::max);
            if max - min > 2.0 * half + 0.001 {
                return Err("This size and rotation cannot cover the canvas; increase zoom or stretch first");
            }
            polygon = clip_half_plane(&polygon, axis, min + half);
            polygon = clip_half_plane(&polygon, [-axis[0], -axis[1]], half - max);
        }
        if polygon.is_empty() {
            return Err(
                "This size and rotation cannot cover the canvas; increase zoom or stretch first",
            );
        }
        Ok(PanRegion {
            vertices: convex_hull(polygon),
        })
    }

    pub fn edge_snap(&self, pan: [f32; 2], distance: f32) -> [Option<f32>; 2] {
        let mut result = [None, None];
        for axis in 0..2 {
            let min = self
                .corners
                .iter()
                .map(|corner| corner[axis])
                .fold(f32::INFINITY, f32::min);
            let max = self
                .corners
                .iter()
                .map(|corner| corner[axis])
                .fold(f32::NEG_INFINITY, f32::max);
            let targets = [-240.0 - min, 240.0 - min, -240.0 - max, 240.0 - max];
            result[axis] = targets
                .into_iter()
                .filter(|target| (target - pan[axis]).abs() <= distance)
                .min_by(|a, b| (a - pan[axis]).abs().total_cmp(&(b - pan[axis]).abs()));
        }
        result
    }
}

#[derive(Clone, Debug)]
pub struct PanRegion {
    pub vertices: Vec<[f32; 2]>,
}

impl PanRegion {
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        if self.vertices.is_empty() || other.vertices.is_empty() {
            return None;
        }
        if self.vertices.len() == 1 {
            let point = self.vertices[0];
            return (distance_sq(other.clamp(point), point) < 0.0001).then(|| self.clone());
        }
        if other.vertices.len() == 1 {
            return other.intersect(self);
        }
        let (base, clip) = if self.vertices.len() <= 2 && other.vertices.len() > 2 {
            (self, other)
        } else if other.vertices.len() <= 2 && self.vertices.len() > 2 {
            (other, self)
        } else {
            (self, other)
        };
        if clip.vertices.len() <= 2 {
            let intersections: Vec<_> = base
                .vertices
                .iter()
                .copied()
                .filter(|point| distance_sq(clip.clamp(*point), *point) < 0.0001)
                .collect();
            return (!intersections.is_empty()).then(|| Self {
                vertices: convex_hull(intersections),
            });
        }
        let mut polygon = base.vertices.clone();
        for (index, start) in clip.vertices.iter().enumerate() {
            let end = clip.vertices[(index + 1) % clip.vertices.len()];
            let edge = [end[0] - start[0], end[1] - start[1]];
            let normal = [edge[1], -edge[0]];
            polygon = clip_half_plane(&polygon, normal, dot(normal, *start));
            if polygon.is_empty() {
                return None;
            }
        }
        Some(Self {
            vertices: convex_hull(polygon),
        })
    }

    pub fn clamp(&self, point: [f32; 2]) -> [f32; 2] {
        if self.vertices.is_empty() || !point.iter().all(|value| value.is_finite()) {
            return point;
        }
        if self.vertices.len() == 1 {
            return self.vertices[0];
        }
        let area: f32 = self
            .vertices
            .iter()
            .enumerate()
            .map(|(index, start)| {
                let end = self.vertices[(index + 1) % self.vertices.len()];
                start[0] * end[1] - start[1] * end[0]
            })
            .sum();
        if area.abs() > 0.001
            && self.vertices.iter().enumerate().all(|(index, start)| {
                let end = self.vertices[(index + 1) % self.vertices.len()];
                cross(*start, end, point) >= -0.001
            })
        {
            return point;
        }
        self.vertices
            .iter()
            .enumerate()
            .map(|(index, start)| {
                let end = self.vertices[(index + 1) % self.vertices.len()];
                let delta = [end[0] - start[0], end[1] - start[1]];
                let length_sq = dot(delta, delta);
                let t = if length_sq <= f32::EPSILON {
                    0.0
                } else {
                    (dot([point[0] - start[0], point[1] - start[1]], delta) / length_sq)
                        .clamp(0.0, 1.0)
                };
                [start[0] + t * delta[0], start[1] + t * delta[1]]
            })
            .min_by(|a, b| distance_sq(*a, point).total_cmp(&distance_sq(*b, point)))
            .unwrap_or(point)
    }
}

fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}
fn cross(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}
fn distance_sq(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)
}

fn convex_hull(mut points: Vec<[f32; 2]>) -> Vec<[f32; 2]> {
    points.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    points.dedup();
    if points.len() <= 2 {
        return points;
    }
    let mut lower = Vec::new();
    for point in &points {
        while lower.len() >= 2
            && cross(lower[lower.len() - 2], lower[lower.len() - 1], *point) <= 0.0
        {
            lower.pop();
        }
        lower.push(*point);
    }
    let mut upper = Vec::new();
    for point in points.iter().rev() {
        while upper.len() >= 2
            && cross(upper[upper.len() - 2], upper[upper.len() - 1], *point) <= 0.0
        {
            upper.pop();
        }
        upper.push(*point);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

fn clip_half_plane(polygon: &[[f32; 2]], normal: [f32; 2], limit: f32) -> Vec<[f32; 2]> {
    if polygon.is_empty() {
        return Vec::new();
    }
    let mut output = Vec::new();
    for (index, current) in polygon.iter().enumerate() {
        let next = polygon[(index + 1) % polygon.len()];
        let current_d = dot(*current, normal) - limit;
        let next_d = dot(next, normal) - limit;
        if current_d <= 0.0 {
            output.push(*current);
        }
        if (current_d < 0.0 && next_d > 0.0) || (current_d > 0.0 && next_d < 0.0) {
            let t = current_d / (current_d - next_d);
            output.push([
                current[0] + t * (next[0] - current[0]),
                current[1] + t * (next[1] - current[1]),
            ]);
        }
    }
    output
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

pub struct TimedAnimation {
    frames: Vec<(RgbaImage, Duration)>,
    dimensions: Vec<(u32, u32)>,
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
                (frame.into_buffer(), delay)
            })
            .collect::<Vec<_>>();
        if frames.is_empty() {
            return Err("boot animation contains no frames".into());
        }
        let mut dimensions = Vec::new();
        for (frame, _) in &frames {
            let size = (frame.width(), frame.height());
            if !dimensions.contains(&size) {
                dimensions.push(size);
            }
        }
        Ok(Self {
            frames,
            dimensions,
            started: Instant::now(),
        })
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, index: usize) -> Option<&RgbaImage> {
        self.frames.get(index).map(|(frame, _)| frame)
    }

    pub fn frame_dimensions(&self) -> Vec<(u32, u32)> {
        self.dimensions.clone()
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

    pub fn current_uniform_frame(&self, start: usize, end: usize, delay_ms: u32) -> &RgbaImage {
        let start = start.min(self.frames.len() - 1);
        let end = end.clamp(start, self.frames.len() - 1);
        let count = end - start + 1;
        let elapsed_ms = self.started.elapsed().as_millis();
        let index = start + (elapsed_ms / u128::from(delay_ms.max(1)) % count as u128) as usize;
        &self.frames[index].0
    }
}

pub struct Renderer {
    font_bytes: Vec<u8>,
    bg_player: Option<background_cache::BackgroundPlayer>,
    bg_key: Option<background_cache::Key>,
    /// Prepared background only; live sensor overlays are drawn afterward.
    bg_cache: Option<(BackgroundConfig, Arc<RgbImage>)>,
    background_error: Option<String>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            font_bytes: FONT_BYTES.to_vec(),
            bg_player: None,
            bg_key: None,
            bg_cache: None,
            background_error: None,
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
        let mut encoded = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut encoded, 75);
        // Match the vendor's 4:2:0 format to reduce USB packets per frame.
        encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
        encoder
            .encode(
                img.as_raw(),
                W as u16,
                H as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("jpeg encode failed");
        encoded
    }

    /// Recommended device refresh cadence for the active animated background.
    /// Sensor collection remains independently controlled by the daemon interval.
    pub fn animation_frame_interval(&self) -> Option<Duration> {
        self.bg_player.as_ref().and_then(|player| player.interval())
    }

    pub fn background_source_dimensions(&self, path: &str) -> Option<(u32, u32)> {
        self.bg_key.as_ref()?;
        self.bg_cache
            .as_ref()
            .filter(|(background, _)| background.image_path.as_deref() == Some(path))?;
        self.bg_player.as_ref()?.dimensions
    }

    fn update_bg_cache(&mut self, config: &Config) {
        if !config.background.enabled || config.background.image_path.is_none() {
            if self.bg_key.is_some() {
                if let Some(player) = &mut self.bg_player {
                    player.clear();
                }
            }
            self.bg_key = None;
            self.bg_cache = None;
            self.background_error = None;
            return;
        }
        let key = background_cache::Key::new(&config.background)
            .with_compression(config.background_cache_compression)
            .with_size_mib(config.background_cache_size_mib);
        if self.bg_key.as_ref() != Some(&key) {
            self.bg_player
                .get_or_insert_with(background_cache::BackgroundPlayer::new)
                .request(key.clone());
            self.bg_key = Some(key);
            self.background_error = None;
        }
        let player = self.bg_player.as_mut().unwrap();
        if let Some(frame) = player.next_frame(Instant::now()) {
            self.bg_cache = Some((config.background.clone(), frame));
        }
        if let Some(error) = player.error.take() {
            self.bg_cache = None;
            self.background_error = Some(error);
        }
    }

    pub fn take_background_error(&mut self) -> Option<String> {
        self.background_error.take()
    }

    pub fn widget_bounds(
        &self,
        config: &Config,
        instance_id: &str,
        _values: &SensorValues,
    ) -> Option<[f32; 2]> {
        let instance = config
            .widget_instances
            .iter()
            .find(|instance| instance.id == instance_id)?;
        let widget = config.resolved_widget(instance)?;
        let width = widget.style.width;
        let height = widget.style.height;
        let radians = widget.transform.rotation.to_radians();
        Some([
            width * radians.cos().abs() + height * radians.sin().abs(),
            width * radians.sin().abs() + height * radians.cos().abs(),
        ])
    }

    pub fn widget_corners(&self, config: &Config, instance_id: &str) -> Option<[[f32; 2]; 4]> {
        let instance = config
            .widget_instances
            .iter()
            .find(|item| item.id == instance_id)?;
        let widget = config.resolved_widget(instance)?;
        if !widget.style.width.is_finite()
            || !widget.style.height.is_finite()
            || widget.style.width <= 0.0
            || widget.style.height <= 0.0
        {
            return None;
        }
        let half_width = widget.style.width / 2.0;
        let half_height = widget.style.height / 2.0;
        let (sin, cos) = widget.transform.rotation.to_radians().sin_cos();
        Some(
            [
                [-half_width, -half_height],
                [half_width, -half_height],
                [half_width, half_height],
                [-half_width, half_height],
            ]
            .map(|[x, y]| {
                [
                    widget.transform.pan_x + x * cos - y * sin,
                    widget.transform.pan_y + x * sin + y * cos,
                ]
            }),
        )
    }

    pub fn widget_contains(
        &self,
        config: &Config,
        instance_id: &str,
        point: [f32; 2],
        tolerance: f32,
    ) -> bool {
        let Some(instance) = config
            .widget_instances
            .iter()
            .find(|item| item.id == instance_id)
        else {
            return false;
        };
        let Some(widget) = config.resolved_widget(instance) else {
            return false;
        };
        if !widget.style.width.is_finite()
            || !widget.style.height.is_finite()
            || widget.style.width <= 0.0
            || widget.style.height <= 0.0
        {
            return false;
        }
        let dx = point[0] - widget.transform.pan_x;
        let dy = point[1] - widget.transform.pan_y;
        let (sin, cos) = widget.transform.rotation.to_radians().sin_cos();
        let local_x = dx * cos + dy * sin;
        let local_y = -dx * sin + dy * cos;
        let tolerance = tolerance.max(0.0);
        local_x.abs() <= widget.style.width / 2.0 + tolerance
            && local_y.abs() <= widget.style.height / 2.0 + tolerance
    }

    pub fn widget_fit_size(
        &self,
        widget: &crate::config::ResolvedWidget,
        current: &str,
        representative: &str,
    ) -> Option<[f32; 2]> {
        let font = FontRef::try_from_slice(&self.font_bytes).ok()?;
        let mut required = [0.0_f32, 0.0_f32];
        for value in [current, representative, "--"] {
            let size = widget_layer_dimensions(&font, widget, value);
            required[0] = required[0].max(size[0]);
            required[1] = required[1].max(size[1]);
        }
        Some(required)
    }

    pub fn widget_content_overflows(
        &self,
        widget: &crate::config::ResolvedWidget,
        required: [f32; 2],
    ) -> bool {
        required[0] > widget.style.width || required[1] > widget.style.height
    }

    fn render_base(&self, config: &Config, v: &SensorValues) -> RgbImage {
        let font = FontRef::try_from_slice(&self.font_bytes).expect("invalid font");

        // ── Base layer ────────────────────────────────────────────────────────
        let mut img = if config.background.enabled {
            if let Some((_, ref bg)) = self.bg_cache {
                bg.as_ref().clone()
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
                let raw = v
                    .readings
                    .get(&widget.source_id)
                    .copied()
                    .filter(|value| value.is_finite());
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
    let logical_width = widget.style.width;
    let logical_height = widget.style.height;
    if !logical_width.is_finite()
        || !logical_height.is_finite()
        || logical_width <= 0.0
        || logical_height <= 0.0
    {
        return;
    }
    let label_offset_y = effective_label_offset_y(&widget.style);
    let min_y = label_offset_y.min(0.0);
    let max_y = widget
        .style
        .value_font_size
        .max(label_offset_y + widget.style.label_font_size);
    let group_top = ((logical_height - (max_y - min_y)) / 2.0).round();
    let width = logical_width.ceil().clamp(1.0, 4096.0) as u32;
    let height = logical_height.ceil().clamp(1.0, 4096.0) as u32;
    let mut text_layer = RgbaImage::new(width, height);
    draw_centered_rgba(
        &mut text_layer,
        font,
        &value,
        width as i32 / 2,
        (group_top - min_y).round() as i32,
        widget.style.value_font_size,
        Rgba([value_color[0], value_color[1], value_color[2], 255]),
    );
    draw_centered_rgba(
        &mut text_layer,
        font,
        &widget.label,
        (width as f32 / 2.0 + widget.style.label_offset_x).round() as i32,
        (group_top + label_offset_y - min_y).round() as i32,
        widget.style.label_font_size,
        Rgba([
            widget.style.label_color[0],
            widget.style.label_color[1],
            widget.style.label_color[2],
            255,
        ]),
    );
    let layer = if widget.style.background_opacity > 0 {
        let [red, green, blue] = widget.style.background_color;
        let mut background = RgbaImage::from_pixel(
            width,
            height,
            Rgba([red, green, blue, widget.style.background_opacity]),
        );
        imageops::overlay(&mut background, &text_layer, 0, 0);
        background
    } else {
        text_layer
    };

    let transform = widget.transform;
    let transformed = rotate_rgba_expanded(&layer, transform.rotation);
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
    let source = flatten_rgba_on_canvas(&source, config.background_color);
    transform_background_image(&source, config)
}

fn load_background_source(path: &str) -> Option<RgbaImage> {
    image::open(path)
        .map(|image| image.into_rgba8())
        .ok()
        .or_else(|| load_ffmpeg_first_frame(path))
}

fn load_ffmpeg_first_frame(path: &str) -> Option<RgbaImage> {
    load_media_frame_at(Path::new(path), 0.0).ok()
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

pub fn load_media_frame_at(path: &Path, seconds: f64) -> Result<RgbaImage, String> {
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
        return Ok(frames[index].buffer().clone());
    }
    if let Ok(image) = image::open(path) {
        return Ok(image.into_rgba8());
    }
    let decoder = video_alpha_decoder(path)?;
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-ss", &seconds.max(0.0).to_string()]);
    if let Some(decoder) = decoder {
        command.args(["-c:v", decoder]);
    }
    let output = command
        .arg("-i")
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .output()
        .map_err(ffmpeg_start_error)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    image::load_from_memory(&output.stdout)
        .map(|image| image.into_rgba8())
        .map_err(|error| error.to_string())
}

fn ffmpeg_start_error(error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        "This media type requires FFmpeg. Install ffmpeg and try again.".to_string()
    } else {
        format!("failed to start FFmpeg: {error}")
    }
}

/// Composite straight alpha onto the selected matte before RGB geometry.
/// Flattening before interpolation prevents hidden transparent RGB from bleeding
/// into scaled/rotated edges. Cache and USB formats remain opaque RGB/JPEG.
pub fn flatten_rgba_on_canvas(source: &RgbaImage, color: [u8; 3]) -> RgbImage {
    RgbImage::from_fn(source.width(), source.height(), |x, y| {
        let pixel = source.get_pixel(x, y);
        let alpha = u32::from(pixel[3]);
        Rgb(std::array::from_fn(|channel| {
            ((u32::from(pixel[channel]) * alpha + u32::from(color[channel]) * (255 - alpha) + 127)
                / 255) as u8
        }))
    })
}

pub fn transform_rgba_media_image(source: &RgbaImage, media: &MediaTransform) -> Option<RgbImage> {
    transform_media_image(&flatten_rgba_on_canvas(source, media.canvas_color), media)
}

/// WebM alpha is an auxiliary VP8/VP9 bitstream that native FFmpeg decoders
/// commonly ignore. Select libvpx explicitly when the stream declares alpha.
fn video_alpha_decoder(path: &Path) -> Result<Option<&'static str>, String> {
    let container = path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default();
    if !container.eq_ignore_ascii_case("webm") && !container.eq_ignore_ascii_case("mkv") {
        return Ok(None);
    }
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name:stream_tags=alpha_mode",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .map_err(|error| {
            format!("Inspecting WebM/Matroska transparency requires ffprobe: {error}")
        })?;
    if !probe.status.success() {
        return Err("Cannot inspect video transparency with ffprobe".into());
    }
    let metadata = String::from_utf8_lossy(&probe.stdout);
    let mut codec = "";
    let mut alpha = false;
    for line in metadata.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if key == "codec_name" {
                codec = value;
            }
            if key.eq_ignore_ascii_case("TAG:alpha_mode") && value == "1" {
                alpha = true;
            }
        }
    }
    let decoder = match (alpha, codec) {
        (true, "vp9") => "libvpx-vp9",
        (true, "vp8") => "libvpx",
        _ => return Ok(None),
    };
    let decoders = Command::new("ffmpeg")
        .args(["-hide_banner", "-decoders"])
        .output()
        .map_err(ffmpeg_start_error)?;
    if !decoders.status.success()
        || !String::from_utf8_lossy(&decoders.stdout)
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(decoder))
    {
        return Err(format!(
            "Transparent {codec} video requires an FFmpeg build with the {decoder} decoder"
        ));
    }
    Ok(Some(decoder))
}

/// Flatten video frames before the MJPEG transport removes their alpha.
fn video_matte_filter(color: [u8; 3]) -> String {
    let [r, g, b] = color;
    format!(
        "[0:v]fps={MAX_DEVICE_FRAME_RATE},format=rgba,split=2[fg][matte];\
        [matte]drawbox=x=0:y=0:w=iw:h=ih:t=fill:color=0x{r:02x}{g:02x}{b:02x}@1:replace=1[bg];\
        [bg][fg]overlay=shortest=1:alpha=straight:format=rgb[out]"
    )
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
    fn alpha_is_flattened_on_canvas_with_partial_and_opaque_pixels_preserved() {
        let mut source = RgbaImage::new(3, 1);
        source.put_pixel(0, 0, Rgba([255, 99, 77, 0]));
        source.put_pixel(1, 0, Rgba([255, 0, 0, 128]));
        source.put_pixel(2, 0, Rgba([12, 34, 56, 255]));
        let rgb = flatten_rgba_on_canvas(&source, [0, 0, 255]);
        assert_eq!(rgb.get_pixel(0, 0), &Rgb([0, 0, 255]));
        assert_eq!(rgb.get_pixel(1, 0), &Rgb([128, 0, 127]));
        assert_eq!(rgb.get_pixel(2, 0), &Rgb([12, 34, 56]));
    }

    #[test]
    fn transparent_hidden_colors_do_not_bleed_during_scaling_or_rotation() {
        let source = RgbaImage::from_fn(7, 5, |x, _| {
            Rgba([255, 0, if x % 2 == 0 { 255 } else { 0 }, 0])
        });
        let mut media = MediaTransform {
            canvas_color: [0, 200, 0],
            fit: ImageFit::Contain,
            ..Default::default()
        };
        media.transform.rotation = 37.0;
        media.transform.zoom = 1.2;
        media.transform.pan_x = 13.0;
        let rendered = transform_rgba_media_image(&source, &media).unwrap();
        assert!(rendered
            .pixels()
            .all(|pixel| pixel[0] == 0 && pixel[2] == 0 && pixel[1].abs_diff(200) <= 1));
    }

    #[test]
    fn retained_rgba_media_can_change_canvas_color_without_redecoding() {
        let source = RgbaImage::from_pixel(16, 8, Rgba([255, 0, 0, 0]));
        let mut media = MediaTransform {
            canvas_color: [17, 61, 113],
            ..Default::default()
        };
        let first = transform_rgba_media_image(&source, &media).unwrap();
        assert_eq!(first.get_pixel(240, 240), &Rgb([17, 61, 113]));
        media.canvas_color = [0, 200, 0];
        let second = transform_rgba_media_image(&source, &media).unwrap();
        assert_eq!(second.get_pixel(240, 240), &Rgb([0, 200, 0]));
        assert_eq!(source.get_pixel(0, 0), &Rgba([255, 0, 0, 0]));
    }

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
    fn widget_background_uses_fixed_box() {
        let config = Config::default();
        let mut widget = config.resolved_widget(&config.widget_instances[0]).unwrap();
        widget.style.background_color = [200, 0, 0];
        widget.style.background_opacity = 128;
        widget.label.clear();
        widget.transform.pan_x = 0.0;
        widget.transform.pan_y = 0.0;
        let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
        let [width, height] = [widget.style.width, widget.style.height];
        let corner_x = (240.0 - width.ceil() / 2.0).round() as u32 + 1;
        let corner_y = (240.0 - height.ceil() / 2.0).round() as u32 + 1;
        let mut canvas = RgbImage::from_pixel(480, 480, Rgb([0, 0, 0]));
        draw_widget_instance(&mut canvas, &font, &widget, Some(25.0));
        let first = canvas.get_pixel(corner_x, corner_y);
        assert_eq!(*first, Rgb([100, 0, 0]));

        widget.style.width *= 2.0;
        widget.style.height *= 2.0;
        let mut enlarged = RgbImage::from_pixel(480, 480, Rgb([0, 0, 0]));
        draw_widget_instance(&mut enlarged, &font, &widget, Some(25.0));
        let count_red = |image: &RgbImage| image.pixels().filter(|pixel| pixel[0] > 0).count();
        assert!(count_red(&enlarged) > count_red(&canvas) * 2);
    }

    #[test]
    fn widget_fit_accounts_for_current_representative_and_missing_values() {
        let config = Config::default();
        let mut widget = config.resolved_widget(&config.widget_instances[0]).unwrap();
        let renderer = Renderer::new();
        let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
        for label_above in [false, true] {
            widget.style.label_offset_y = if label_above { -10.0 } else { 10.0 };
            let fit = renderer.widget_fit_size(&widget, "99", "100").unwrap();
            for value in ["99", "100", "--"] {
                let measured = widget_layer_dimensions(&font, &widget, value);
                assert!(fit[0] >= measured[0] && fit[1] >= measured[1]);
            }
            widget.style.width = fit[0].ceil();
            widget.style.height = fit[1].ceil();
            assert!(!renderer.widget_content_overflows(&widget, fit));
            widget.style.width = fit[0] - 1.0;
            assert!(renderer.widget_content_overflows(&widget, fit));
            widget.style.width = fit[0].ceil();
            widget.style.height = fit[1] - 1.0;
            assert!(renderer.widget_content_overflows(&widget, fit));
        }
    }

    #[test]
    fn widget_box_rendering_keeps_transparent_and_opaque_edges() {
        let config = Config::default();
        let mut widget = config.resolved_widget(&config.widget_instances[0]).unwrap();
        widget.label.clear();
        widget.style.width = 101.0;
        widget.style.height = 81.0;
        widget.style.background_color = [240, 0, 0];
        widget.transform.pan_x = 0.0;
        widget.transform.pan_y = 0.0;
        widget.transform.rotation = 0.0;
        let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
        let mut render = |opacity| {
            widget.style.background_opacity = opacity;
            let mut canvas = RgbImage::from_pixel(480, 480, Rgb([0, 0, 0]));
            draw_widget_instance(&mut canvas, &font, &widget, Some(25.0));
            canvas
        };
        let transparent = render(0);
        let opaque = render(255);
        assert_eq!(*transparent.get_pixel(191, 201), Rgb([0, 0, 0]));
        assert_eq!(*opaque.get_pixel(191, 201), Rgb([240, 0, 0]));
    }

    #[test]
    fn widget_label_stays_on_requested_side_of_value() {
        let config = Config::default();
        let mut widget = config.resolved_widget(&config.widget_instances[0]).unwrap();
        widget.label = "LABEL".to_string();
        widget.style.width = 220.0;
        widget.style.height = 160.0;
        widget.style.label_color = [255, 0, 0];
        widget.style.color_map = vec![crate::config::ColorPoint {
            value: 0.0,
            color: [0, 255, 0],
        }];
        widget.transform.pan_x = 0.0;
        widget.transform.pan_y = 0.0;
        widget.transform.rotation = 0.0;
        let font = FontRef::try_from_slice(FONT_BYTES).unwrap();
        for (offset, above) in [(-10.0, true), (10.0, false)] {
            widget.style.label_offset_y = offset;
            let mut canvas = RgbImage::from_pixel(480, 480, Rgb([0, 0, 0]));
            draw_widget_instance(&mut canvas, &font, &widget, Some(25.0));
            let color_center_y = |is_label: bool| {
                let ys: Vec<u32> = canvas
                    .enumerate_pixels()
                    .filter_map(|(_, y, pixel)| {
                        let selected = if is_label {
                            pixel[0] > 100 && pixel[1] < 30
                        } else {
                            pixel[1] > 100 && pixel[0] < 30
                        };
                        selected.then_some(y)
                    })
                    .collect();
                assert!(!ys.is_empty());
                ys.iter().sum::<u32>() as f32 / ys.len() as f32
            };
            let label_y = color_center_y(true);
            let value_y = color_center_y(false);
            assert_eq!(label_y < value_y, above);
            assert!((label_y + value_y) / 2.0 > 190.0);
            assert!((label_y + value_y) / 2.0 < 290.0);
        }
    }

    #[test]
    fn every_missing_widget_renders_placeholder_instead_of_disappearing() {
        let mut renderer = Renderer::new();
        for source in ["coolant", "cpu_temp"] {
            let mut config = Config::default();
            config
                .widget_instances
                .retain(|instance| instance.source_id == source);
            assert_eq!(config.widget_instances.len(), 1);
            let missing = SensorValues {
                readings: HashMap::new(),
            };
            let placeholder = renderer.render_preview(&config, &missing);
            let instance = config.widget_instances[0].clone();
            config.widget_instances.clear();
            let hidden = renderer.render_preview(&config, &missing);
            assert_ne!(placeholder, hidden, "missing {source} widget disappeared");

            let mut live = SensorValues {
                readings: HashMap::new(),
            };
            live.readings.insert(source.into(), 28.5);
            let mut invalid = SensorValues {
                readings: HashMap::new(),
            };
            invalid.readings.insert(source.into(), f32::NAN);
            config.widget_instances.push(instance);
            assert_eq!(placeholder, renderer.render_preview(&config, &invalid));
            assert_ne!(placeholder, renderer.render_preview(&config, &live));
        }
    }

    #[test]
    fn widget_geometry_does_not_change_with_telemetry() {
        let mut config = Config::default();
        let id = config.widget_instances[0].id.clone();
        let renderer = Renderer::new();
        let mut values = SensorValues {
            readings: HashMap::new(),
        };
        values
            .readings
            .insert(config.widget_instances[0].source_id.clone(), 99.0);
        let before = renderer.widget_bounds(&config, &id, &values).unwrap();
        values
            .readings
            .insert(config.widget_instances[0].source_id.clone(), 100.0);
        assert_eq!(
            renderer.widget_bounds(&config, &id, &values).unwrap(),
            before
        );
        config.widget_instances[0].transform.rotation = 45.0;
        let corners = renderer.widget_corners(&config, &id).unwrap();
        let edge = [corners[1][0] - corners[0][0], corners[1][1] - corners[0][1]];
        assert!(
            (edge[0].hypot(edge[1]) - config.widget_instances[0].overrides.width.unwrap()).abs()
                < 0.01
        );
    }

    #[test]
    fn widget_hit_test_excludes_rotated_bounding_box_corners() {
        let mut config = Config::default();
        let id = config.widget_instances[0].id.clone();
        config.widget_instances[0].transform.pan_x = 0.0;
        config.widget_instances[0].transform.pan_y = 0.0;
        config.widget_instances[0].transform.rotation = 45.0;
        let renderer = Renderer::new();
        assert!(renderer.widget_contains(&config, &id, [0.0, 0.0], 0.0));
        assert!(!renderer.widget_contains(&config, &id, [95.0, 95.0], 0.0));
    }

    #[test]
    fn media_pan_regions_use_actual_source_edges() {
        let transform = Transform2D::default();
        let footprint = MediaFootprint::new((480, 480), &ImageFit::Native, &transform).unwrap();
        let relaxed = footprint.pan_region(false).unwrap();
        assert_eq!(relaxed.clamp([960.0, 0.0]), [480.0, 0.0]);
        assert_eq!(relaxed.clamp([479.0, 0.0]), [479.0, 0.0]);
        let strict = footprint.pan_region(true).unwrap();
        assert_eq!(strict.clamp([10.0, 0.0]), [0.0, 0.0]);
        assert_eq!(footprint.edge_snap([474.0, 0.0], 8.0)[0], Some(480.0));
        let rotated = MediaFootprint::new(
            (480, 480),
            &ImageFit::Native,
            &Transform2D {
                rotation: 45.0,
                ..transform
            },
        )
        .unwrap();
        assert!(rotated.pan_region(true).is_err());
    }

    #[test]
    fn relaxed_media_limits_allow_contact_but_not_a_one_pixel_gap() {
        let footprint =
            MediaFootprint::new((480, 480), &ImageFit::Native, &Transform2D::default()).unwrap();
        let region = footprint.pan_region(false).unwrap();
        for candidate in [
            [480.0, 0.0],
            [-480.0, 0.0],
            [0.0, 480.0],
            [0.0, -480.0],
            [480.0, 480.0],
            [480.0, -480.0],
            [-480.0, 480.0],
            [-480.0, -480.0],
        ] {
            assert_eq!(region.clamp(candidate), candidate);
        }
        for (outside, expected) in [
            ([481.0, 0.0], [480.0, 0.0]),
            ([-481.0, 0.0], [-480.0, 0.0]),
            ([0.0, 481.0], [0.0, 480.0]),
            ([0.0, -481.0], [0.0, -480.0]),
            ([481.0, 481.0], [480.0, 480.0]),
        ] {
            assert_eq!(region.clamp(outside), expected);
        }
    }

    #[test]
    fn rotated_media_edge_snap_uses_source_corners_and_strict_coverage() {
        let transform = Transform2D {
            rotation: 30.0,
            ..Transform2D::default()
        };
        let footprint = MediaFootprint::new((300, 200), &ImageFit::Native, &transform).unwrap();
        let max_x = footprint
            .corners
            .iter()
            .map(|corner| corner[0])
            .fold(f32::NEG_INFINITY, f32::max);
        let right_contact = 240.0 - max_x;
        assert_eq!(
            footprint.edge_snap([right_contact - 2.0, 0.0], 8.0)[0],
            Some(right_contact)
        );
        let min_x = footprint
            .corners
            .iter()
            .map(|corner| corner[0])
            .fold(f32::INFINITY, f32::min);
        let outer_contact = 240.0 - min_x;
        let region = footprint.pan_region(false).unwrap();
        let clamped = region.clamp([outer_contact + 1.0, 0.0]);
        assert!((clamped[0] - outer_contact).abs() < 0.01);

        assert!(MediaFootprint::new(
            (480, 480),
            &ImageFit::Native,
            &Transform2D {
                rotation: 45.0,
                ..Transform2D::default()
            }
        )
        .unwrap()
        .pan_region(true)
        .is_err());
        let enlarged = MediaFootprint::new(
            (480, 480),
            &ImageFit::Native,
            &Transform2D {
                zoom: 1.5,
                rotation: 45.0,
                ..Transform2D::default()
            },
        )
        .unwrap();
        let covered = enlarged.pan_region(true).unwrap();
        let centered = covered.clamp([0.0, 0.0]);
        assert!(centered[0].abs() < 0.01 && centered[1].abs() < 0.01);
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

    fn wait_for_background(renderer: &mut Renderer, config: &Config) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            renderer.update_bg_cache(config);
            if renderer
                .bg_cache
                .as_ref()
                .is_some_and(|(key, _)| key == &config.background)
                && renderer.bg_player.as_ref().unwrap().dimensions.is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "background preparation timed out: {:?}",
                renderer.background_error
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn failed_replacement_clears_the_previous_background() {
        let path = std::env::temp_dir().join(format!(
            "th420-cache-error-{}-{:?}.png",
            std::process::id(),
            std::thread::current().id()
        ));
        RgbImage::from_pixel(16, 8, Rgb([100, 0, 0]))
            .save(&path)
            .unwrap();
        let mut config = Config::default();
        config.background.image_path = Some(path.to_string_lossy().into_owned());
        let mut renderer = Renderer::new();
        wait_for_background(&mut renderer, &config);
        config.background.image_path = Some(
            path.with_extension("missing.gif")
                .to_string_lossy()
                .into_owned(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while renderer.background_error.is_none() {
            renderer.update_bg_cache(&config);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(renderer.bg_cache.is_none());
        assert!(renderer
            .background_source_dimensions(path.to_str().unwrap())
            .is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn prepared_background_invalidates_on_visual_edits_but_not_sensor_edits() {
        let path = std::env::temp_dir().join(format!(
            "th420-prepared-{}-{:?}.png",
            std::process::id(),
            std::thread::current().id()
        ));
        let source = RgbImage::from_pixel(24, 12, Rgb([90, 120, 150]));
        source.save(&path).unwrap();
        let mut config = Config::default();
        config.background.image_path = Some(path.to_string_lossy().into_owned());
        config.background.fit = ImageFit::Native;
        config.background.overlay_alpha = 0;
        let mut renderer = Renderer::new();
        wait_for_background(&mut renderer, &config);
        assert_eq!(
            renderer.background_source_dimensions(path.to_str().unwrap()),
            Some((24, 12))
        );
        let original = Arc::clone(&renderer.bg_cache.as_ref().unwrap().1);
        assert_eq!(original.dimensions(), (480, 480));
        assert_eq!(
            *original,
            load_background_image(&config.background).unwrap()
        );
        let first = renderer.render(&config, &normal());
        let second = renderer.render(&config, &make_values(&[("cpu_temp", 99.0)]));
        assert_ne!(first, second);
        assert!(Arc::ptr_eq(
            &original,
            &renderer.bg_cache.as_ref().unwrap().1
        ));
        config.background_cache_compression = crate::config::BackgroundCacheCompression::Lz4Hc9;
        renderer.update_bg_cache(&config);
        wait_for_background(&mut renderer, &config);
        assert_eq!(*renderer.bg_cache.as_ref().unwrap().1, *original);
        assert!(!Arc::ptr_eq(
            &original,
            &renderer.bg_cache.as_ref().unwrap().1
        ));
        config.background.pan_x = 40.0;
        wait_for_background(&mut renderer, &config);
        assert_ne!(*renderer.bg_cache.as_ref().unwrap().1, *original);
        assert_eq!(
            *renderer.bg_cache.as_ref().unwrap().1,
            load_background_image(&config.background).unwrap()
        );
        config.background.background_color = [1, 2, 3];
        wait_for_background(&mut renderer, &config);
        assert_eq!(
            *renderer.bg_cache.as_ref().unwrap().1,
            load_background_image(&config.background).unwrap()
        );
        std::fs::remove_file(path).unwrap();
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
    fn live_jpeg_uses_decodable_420_sampling() {
        let bytes = Renderer::new().render(&Config::default(), &normal());
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (W, H));
        // Baseline SOF: marker, length, precision, dimensions, component count,
        // then component ID and the luma sampling byte (2x2 for 4:2:0).
        let sof = bytes
            .windows(2)
            .position(|marker| marker == [0xff, 0xc0])
            .unwrap();
        assert_eq!(bytes[sof + 11], 0x22);
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
            &Rgba([0, 0, 255, 255])
        );
        let _ = std::fs::remove_file(path);
    }
}
