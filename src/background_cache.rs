//! Bounded target-resolution backgrounds. Native frames live only in the worker.
use super::{compose_background_image, prepare_background_geometry, H, MAX_DEVICE_FRAME_RATE, W};
use crate::config::{
    BackgroundCacheCompression, BackgroundConfig, DEFAULT_BACKGROUND_CACHE_MIB,
    MAX_BACKGROUND_CACHE_MIB, MIN_BACKGROUND_CACHE_MIB,
};
use image::{AnimationDecoder, RgbImage};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

// The configured payload budget excludes the streaming queue, current/render
// copies, and decoder/transform working memory.
const QUEUE_FRAMES: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Key {
    background: BackgroundConfig,
    compression: BackgroundCacheCompression,
    size_mib: u32,
    canonical_path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
    identity: (u64, u64),
}
impl Key {
    pub fn new(background: &BackgroundConfig) -> Self {
        use std::os::unix::fs::MetadataExt;
        let path = Path::new(background.image_path.as_deref().unwrap_or_default());
        let metadata = path.metadata().ok();
        Self {
            background: background.clone(),
            compression: BackgroundCacheCompression::Raw,
            size_mib: DEFAULT_BACKGROUND_CACHE_MIB,
            canonical_path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            len: metadata.as_ref().map_or(0, |m| m.len()),
            identity: metadata.as_ref().map_or((0, 0), |m| (m.dev(), m.ino())),
        }
    }
    pub fn with_compression(mut self, compression: BackgroundCacheCompression) -> Self {
        self.compression = compression;
        self
    }
    pub fn with_size_mib(mut self, size_mib: u32) -> Self {
        self.size_mib = size_mib.clamp(MIN_BACKGROUND_CACHE_MIB, MAX_BACKGROUND_CACHE_MIB);
        self
    }
    fn budget_bytes(&self) -> usize {
        self.size_mib as usize * 1024 * 1024
    }
}

#[derive(Clone)]
struct Frame {
    image: Arc<RgbImage>,
    delay: Duration,
    dimensions: (u32, u32),
}
// Cached payloads are independent blocks. Raw streaming never invokes HC.
enum CachedPixels {
    Raw(Arc<RgbImage>),
    Lz4(Box<[u8]>),
}
struct DecodeBuffers {
    images: [Arc<RgbImage>; 2],
    next: usize,
}
impl DecodeBuffers {
    fn new() -> Self {
        Self {
            images: std::array::from_fn(|_| Arc::new(RgbImage::new(W, H))),
            next: 0,
        }
    }
}
impl CachedPixels {
    fn stored_bytes(&self) -> usize {
        match self {
            Self::Raw(image) => image.as_raw().len(),
            Self::Lz4(bytes) => bytes.len(),
        }
    }
    fn decode(&self, buffers: &mut Option<DecodeBuffers>) -> Result<Arc<RgbImage>, String> {
        match self {
            Self::Raw(image) => Ok(Arc::clone(image)),
            Self::Lz4(bytes) => {
                // Alternate reusable canvases so the renderer can keep the
                // previous Arc while we decode the next frame without allocation.
                let buffers = buffers.get_or_insert_with(DecodeBuffers::new);
                let index = buffers.next;
                buffers.next = 1 - index;
                let image = Arc::make_mut(&mut buffers.images[index]);
                let decoded = lz4::block::decompress_to_buffer(
                    bytes,
                    Some((W * H * 3) as i32),
                    image.as_mut(),
                )
                .map_err(|error| format!("Cannot decode cached background: {error}"))?;
                if decoded != (W * H * 3) as usize {
                    return Err("Invalid cached background size".into());
                }
                Ok(Arc::clone(&buffers.images[index]))
            }
        }
    }
}
struct CachedFrame {
    pixels: CachedPixels,
    delay: Duration,
    dimensions: (u32, u32),
}
impl CachedFrame {
    fn prepare(frame: Frame, compression: BackgroundCacheCompression) -> Result<Self, String> {
        let pixels = if compression == BackgroundCacheCompression::Lz4Hc9 {
            let compressed = lz4::block::compress(
                frame.image.as_raw(),
                Some(lz4::block::CompressionMode::HIGHCOMPRESSION(9)),
                false,
            )
            .map_err(|error| format!("Cannot compress background: {error}"))?;
            if compressed.len() < frame.image.as_raw().len() {
                // Discard compress_bound spare capacity before budget accounting.
                CachedPixels::Lz4(compressed.into_boxed_slice())
            } else {
                CachedPixels::Raw(frame.image)
            }
        } else {
            CachedPixels::Raw(frame.image)
        };
        Ok(Self {
            pixels,
            delay: frame.delay,
            dimensions: frame.dimensions,
        })
    }
    fn into_stream(self, buffers: &mut Option<DecodeBuffers>) -> Result<Frame, String> {
        Ok(Frame {
            image: self.pixels.decode(buffers)?,
            delay: self.delay,
            dimensions: self.dimensions,
        })
    }
}

struct Loop {
    frames: Vec<CachedFrame>,
    total: Duration,
    animated: bool,
}
enum Message {
    Cached(Loop),
    Stream(Frame),
    Error(String),
}
struct Request {
    key: Key,
    generation: u64,
    tx: SyncSender<Message>,
}
#[derive(Default)]
struct Shared {
    pending: Mutex<Option<Request>>,
    wake: Condvar,
    generation: AtomicU64,
    stopped: AtomicBool,
    child: Mutex<Option<Child>>,
}
impl Shared {
    fn cancelled(&self, generation: u64) -> bool {
        self.stopped.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) != generation
    }
    fn kill(&self) {
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
    fn reap(&self) {
        // Never wait while holding the mutex needed by cancellation.
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(super) struct BackgroundPlayer {
    shared: Arc<Shared>,
    receiver: Option<Receiver<Message>>,
    cached: Option<Loop>,
    decode_buffers: Option<DecodeBuffers>,
    started: Option<Instant>,
    current: Option<usize>,
    next_due: Option<Instant>,
    interval: Option<Duration>,
    pub dimensions: Option<(u32, u32)>,
    pub error: Option<String>,
}
impl BackgroundPlayer {
    pub fn new() -> Self {
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        std::thread::spawn(move || loop {
            let request = {
                let mut pending = worker.pending.lock().unwrap();
                while pending.is_none() && !worker.stopped.load(Ordering::Acquire) {
                    pending = worker.wake.wait(pending).unwrap();
                }
                if worker.stopped.load(Ordering::Acquire) {
                    break;
                }
                pending.take().unwrap()
            };
            if let Err(error) = prepare(&request, &worker, request.key.budget_bytes()) {
                if !worker.cancelled(request.generation) {
                    let _ = request.tx.send(Message::Error(error));
                }
            }
            worker.reap();
        });
        Self {
            shared,
            receiver: None,
            cached: None,
            decode_buffers: None,
            started: None,
            current: None,
            next_due: None,
            interval: None,
            dimensions: None,
            error: None,
        }
    }

    pub fn clear(&mut self) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.receiver.take();
        self.cached = None;
        self.decode_buffers = None;
        self.started = None;
        self.current = None;
        self.next_due = None;
        self.interval = None;
        self.dimensions = None;
        self.error = None;
        self.shared.kill();
        self.shared.pending.lock().unwrap().take();
    }

    pub fn request(&mut self, key: Key) {
        // Publish cancellation before waking a worker blocked on the old queue.
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.receiver.take();
        self.cached = None;
        self.decode_buffers = None;
        self.started = None;
        self.current = None;
        self.next_due = None;
        self.dimensions = None;
        self.error = None;
        self.interval = Some(video_period());
        self.shared.kill();
        let (tx, receiver) = mpsc::sync_channel(QUEUE_FRAMES);
        self.receiver = Some(receiver);
        *self.shared.pending.lock().unwrap() = Some(Request {
            key,
            generation,
            tx,
        });
        self.shared.wake.notify_one();
    }

    pub fn interval(&self) -> Option<Duration> {
        self.interval
    }

    pub fn next_frame(&mut self, now: Instant) -> Option<Arc<RgbImage>> {
        if let Some(cached) = &self.cached {
            let start = *self.started.get_or_insert(now);
            let elapsed = now.saturating_duration_since(start).as_nanos() % cached.total.as_nanos();
            let mut end = 0;
            let index = cached
                .frames
                .iter()
                .position(|frame| {
                    end += frame.delay.as_nanos();
                    elapsed < end
                })
                .unwrap_or(cached.frames.len() - 1);
            if self.current == Some(index) {
                return None;
            }
            self.current = Some(index);
            let frame = &cached.frames[index];
            self.interval = cached.animated.then_some(frame.delay.max(video_period()));
            self.dimensions = Some(frame.dimensions);
            return match frame.pixels.decode(&mut self.decode_buffers) {
                Ok(image) => Some(image),
                Err(error) => {
                    self.error = Some(error);
                    self.cached = None;
                    self.interval = None;
                    None
                }
            };
        }
        let mut newest = None;
        for _ in 0..QUEUE_FRAMES {
            if self.next_due.is_some_and(|due| now < due) {
                break;
            }
            let Some(message) = self.receiver.as_ref().and_then(|rx| rx.try_recv().ok()) else {
                break;
            };
            match message {
                Message::Cached(cached) => {
                    self.cached = Some(cached);
                    self.started = Some(now);
                    return self.next_frame(now);
                }
                Message::Stream(frame) => {
                    self.dimensions = Some(frame.dimensions);
                    self.interval = Some(frame.delay.max(video_period()));
                    self.next_due = Some(self.next_due.unwrap_or(now) + frame.delay);
                    newest = Some(frame.image);
                }
                Message::Error(error) => {
                    self.error = Some(error);
                    self.interval = None;
                    break;
                }
            }
        }
        // Bound catch-up after suspend or a blocked device; never spin through
        // the entire missed interval on the UI/render thread.
        if let Some(due) = self.next_due {
            if now.saturating_duration_since(due) > video_period() * QUEUE_FRAMES as u32 {
                self.next_due = Some(now + self.interval.unwrap_or_else(video_period));
            }
        }
        newest
    }
}
impl Drop for BackgroundPlayer {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.receiver.take();
        self.shared.kill();
        // Pair the predicate update with the condvar mutex to avoid a lost wake.
        let mut pending = self.shared.pending.lock().unwrap();
        pending.take();
        self.shared.wake.notify_one();
    }
}
fn video_period() -> Duration {
    Duration::from_secs_f64(1.0 / MAX_DEVICE_FRAME_RATE)
}

struct Accumulator<'a> {
    request: &'a Request,
    shared: &'a Shared,
    frames: Vec<CachedFrame>,
    bytes: usize,
    budget: usize,
    streaming: bool,
    seen: usize,
}
impl Accumulator<'_> {
    fn emit(&mut self, source: RgbImage, delay: Duration) -> Result<bool, String> {
        if self.shared.cancelled(self.request.generation) {
            return Ok(false);
        }
        let dimensions = source.dimensions();
        let prepared = prepare_background_geometry(&source, &self.request.key.background)
            .ok_or("invalid background dimensions")?;
        let image = compose_background_image(&prepared, &self.request.key.background);
        drop(prepared);
        drop(source);
        debug_assert_eq!(image.dimensions(), (W, H));
        let frame = Frame {
            image: Arc::new(image),
            delay,
            dimensions,
        };
        self.seen += 1;
        if self.streaming {
            return Ok(self.send(Message::Stream(frame)));
        }
        let cached = CachedFrame::prepare(frame, self.request.key.compression)?;
        let bytes = cached.pixels.stored_bytes();
        if bytes <= self.budget.saturating_sub(self.bytes) {
            self.bytes += bytes;
            self.frames.push(cached);
            return Ok(true);
        }
        self.streaming = true;
        let mut buffers = None;
        for prefix in std::mem::take(&mut self.frames) {
            if !self.send(Message::Stream(prefix.into_stream(&mut buffers)?)) {
                return Ok(false);
            }
        }
        self.bytes = 0;
        Ok(self.send(Message::Stream(cached.into_stream(&mut buffers)?)))
    }

    fn send(&self, message: Message) -> bool {
        !self.shared.cancelled(self.request.generation) && self.request.tx.send(message).is_ok()
    }
}

fn prepare(request: &Request, shared: &Shared, budget: usize) -> Result<(), String> {
    let path = &request.key.canonical_path;
    let gif = path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|v| v.eq_ignore_ascii_case("gif"));
    let animated = gif || image::image_dimensions(path).is_err();
    let alpha_decoder = if animated && !gif {
        super::video_alpha_decoder(path)?
    } else {
        None
    };
    let mut output = Accumulator {
        request,
        shared,
        frames: Vec::new(),
        bytes: 0,
        budget,
        streaming: false,
        seen: 0,
    };
    loop {
        if shared.cancelled(request.generation) {
            return Ok(());
        }
        let before = output.seen;
        if gif {
            let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(
                File::open(path).map_err(|e| e.to_string())?,
            ))
            .map_err(|e| e.to_string())?;
            for frame in decoder.into_frames() {
                let frame = frame.map_err(|e| e.to_string())?;
                let (n, d) = frame.delay().numer_denom_ms();
                let delay = if d == 0 {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs_f64(f64::from(n) / f64::from(d) / 1000.0)
                }
                .max(Duration::from_millis(10));
                let image = super::flatten_rgba_on_canvas(
                    &frame.into_buffer(),
                    request.key.background.background_color,
                );
                if !output.emit(image, delay)? {
                    return Ok(());
                }
            }
        } else if !animated {
            let image = super::flatten_rgba_on_canvas(
                &image::open(path).map_err(|e| e.to_string())?.into_rgba8(),
                request.key.background.background_color,
            );
            if !output.emit(image, video_period())? {
                return Ok(());
            }
        } else {
            let filter = super::video_matte_filter(request.key.background.background_color);
            let mut command = Command::new("ffmpeg");
            command.args(["-v", "error"]);
            if let Some(decoder) = alpha_decoder {
                command.args(["-c:v", decoder]);
            }
            let mut child = command
                .arg("-i")
                .arg(path)
                .args([
                    "-an",
                    "-filter_complex",
                    &filter,
                    "-map",
                    "[out]",
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
                .map_err(super::ffmpeg_start_error)?;
            let stdout = child.stdout.take().ok_or("FFmpeg stdout unavailable")?;
            {
                let mut active = shared.child.lock().unwrap();
                if shared.cancelled(request.generation) {
                    let _ = child.kill();
                    drop(active);
                    let _ = child.wait();
                    return Ok(());
                }
                *active = Some(child);
            }
            let complete = decode_mjpeg(stdout, |image| output.emit(image, video_period()))?;
            let child = shared.child.lock().unwrap().take();
            if let Some(mut child) = child {
                if !complete || shared.cancelled(request.generation) {
                    let _ = child.kill();
                }
                let status = child.wait().map_err(|e| e.to_string())?;
                if !status.success() && !shared.cancelled(request.generation) {
                    return Err("FFmpeg could not decode the background".into());
                }
            }
        }
        if shared.cancelled(request.generation) {
            return Ok(());
        }
        if output.seen == before {
            return Err("background contains no frames".into());
        }
        if !output.streaming {
            let total = output.frames.iter().map(|f| f.delay).sum();
            let frames = std::mem::take(&mut output.frames);
            output.send(Message::Cached(Loop {
                frames,
                total,
                animated,
            }));
            return Ok(());
        }
        // Over-budget clips keep only the prepared queue. Restart decoding at
        // EOF while the queue covers startup, preserving the playback clock.
    }
}

fn decode_mjpeg(
    mut reader: impl Read,
    mut emit: impl FnMut(RgbImage) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut pending = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(true);
        }
        pending.extend_from_slice(&chunk[..count]);
        while let Some(end) = pending.windows(2).position(|v| v == [0xff, 0xd9]) {
            let jpeg = pending.drain(..end + 2).collect::<Vec<_>>();
            let image = image::load_from_memory(&jpeg)
                .map_err(|e| e.to_string())?
                .into_rgb8();
            if !emit(image)? {
                return Ok(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, Frame as GifFrame, Rgb, Rgba, RgbaImage};

    fn config(path: &Path) -> BackgroundConfig {
        BackgroundConfig {
            image_path: Some(path.to_string_lossy().into_owned()),
            overlay_alpha: 0,
            ..Default::default()
        }
    }
    fn frame(color: u8, delay: Duration) -> Frame {
        Frame {
            image: Arc::new(RgbImage::from_pixel(W, H, Rgb([color, 0, 0]))),
            delay,
            dimensions: (640, 360),
        }
    }
    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "th420-cache-{}-{:?}-{name}",
            std::process::id(),
            std::thread::current().id()
        ))
    }
    fn gif(path: &Path) {
        let mut encoder = image::codecs::gif::GifEncoder::new(File::create(path).unwrap());
        for (color, delay) in [(10, 70), (200, 130)] {
            encoder
                .encode_frame(GifFrame::from_parts(
                    RgbaImage::from_pixel(640, 360, Rgba([color, 0, 0, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay, 1),
                ))
                .unwrap();
        }
    }
    fn wait(player: &mut BackgroundPlayer) -> Arc<RgbImage> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = player.next_frame(Instant::now()) {
                return frame;
            }
            assert!(player.error.is_none(), "{:?}", player.error);
            assert!(Instant::now() < until, "preparation timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn png_and_gif_transparency_use_canvas_in_raw_and_compressed_caches() {
        for extension in ["png", "gif"] {
            let path = path(&format!("alpha.{extension}"));
            let rgba = RgbaImage::from_pixel(16, 8, Rgba([255, 0, 0, 0]));
            if extension == "gif" {
                image::codecs::gif::GifEncoder::new(File::create(&path).unwrap())
                    .encode_frame(GifFrame::from_parts(
                        rgba,
                        0,
                        0,
                        Delay::from_numer_denom_ms(100, 1),
                    ))
                    .unwrap();
            } else {
                rgba.save(&path).unwrap();
            }
            for compression in [
                BackgroundCacheCompression::Raw,
                BackgroundCacheCompression::Lz4Hc9,
            ] {
                let mut cfg = config(&path);
                cfg.background_color = [17, 61, 113];
                let mut player = BackgroundPlayer::new();
                player.request(Key::new(&cfg).with_compression(compression));
                let first = wait(&mut player);
                assert_eq!(first.get_pixel(240, 240).0, cfg.background_color);
                cfg.background_color = [0, 200, 0];
                player.request(Key::new(&cfg).with_compression(compression));
                let second = wait(&mut player);
                assert_eq!(second.get_pixel(240, 240).0, cfg.background_color);
                assert_eq!(second.dimensions(), (W, H));
                assert_eq!(player.dimensions, Some((16, 8)));
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    #[ignore = "requires FFmpeg and ffprobe; no device access"]
    fn transparent_video_ffmpeg_paths_keep_alpha_and_matte_before_mjpeg() {
        use std::io::Write;
        let sources = [[255, 0, 0, 0], [255, 0, 0, 128], [255, 0, 0, 255]];
        for (extension, encoder, options) in [
            ("mkv", "ffv1", vec!["-pix_fmt", "bgra"]),
            (
                "webm",
                "libvpx-vp9",
                vec![
                    "-pix_fmt",
                    "yuva420p",
                    "-lossless",
                    "1",
                    "-auto-alt-ref",
                    "0",
                ],
            ),
        ] {
            let path = path(&format!("alpha-video.{extension}"));
            let mut encode = Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "rawvideo",
                    "-pixel_format",
                    "rgba",
                    "-video_size",
                    "16x16",
                    "-framerate",
                    "24",
                    "-i",
                    "pipe:0",
                    "-c:v",
                    encoder,
                ])
                .args(&options)
                .arg(&path)
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut raw = Vec::new();
            for pixel in sources {
                for _ in 0..16 * 16 {
                    raw.extend_from_slice(&pixel);
                }
            }
            encode.stdin.take().unwrap().write_all(&raw).unwrap();
            let result = encode.wait_with_output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            if extension == "webm" {
                assert_eq!(
                    super::super::video_alpha_decoder(&path).unwrap(),
                    Some("libvpx-vp9")
                );
            }
            let extracted = super::super::load_media_frame_at(&path, 0.0).unwrap();
            assert_eq!(extracted.get_pixel(0, 0)[3], 0);
            let mut cfg = config(&path);
            cfg.background_color = [0, 0, 255];
            let mut player = BackgroundPlayer::new();
            player.request(Key::new(&cfg).with_compression(BackgroundCacheCompression::Lz4Hc9));
            wait(&mut player);
            let cached = player.cached.as_ref().unwrap();
            assert_eq!(cached.frames.len(), 3);
            for (frame, source) in cached.frames.iter().zip(sources) {
                let expected = super::super::flatten_rgba_on_canvas(
                    &RgbaImage::from_pixel(1, 1, Rgba(source)),
                    cfg.background_color,
                );
                let decoded = frame.pixels.decode(&mut None).unwrap();
                for channel in 0..3 {
                    assert!(
                        decoded.get_pixel(240, 240)[channel]
                            .abs_diff(expected.get_pixel(0, 0)[channel])
                            <= 4,
                        "{extension}: alpha={} got={:?} expected={:?}",
                        source[3],
                        decoded.get_pixel(240, 240),
                        expected.get_pixel(0, 0)
                    );
                }
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn hc_cache_is_lossless_and_reuses_two_decode_canvases() {
        let source = frame(123, video_period());
        let cached =
            CachedFrame::prepare(source.clone(), BackgroundCacheCompression::Lz4Hc9).unwrap();
        assert!(matches!(cached.pixels, CachedPixels::Lz4(_)));
        assert!(cached.pixels.stored_bytes() < source.image.as_raw().len());
        let mut buffers = None;
        let mut previous = None;
        let mut pointers = Vec::new();
        for _ in 0..8 {
            let decoded = cached.pixels.decode(&mut buffers).unwrap();
            assert_eq!(*decoded, *source.image);
            pointers.push(Arc::as_ptr(&decoded));
            previous = Some(decoded);
        }
        assert!(previous.is_some());
        assert_ne!(pointers[0], pointers[1]);
        for index in 2..8 {
            assert_eq!(pointers[index], pointers[index % 2]);
        }
        let raw = CachedFrame::prepare(source.clone(), BackgroundCacheCompression::Raw).unwrap();
        assert_eq!(raw.pixels.stored_bytes(), source.image.as_raw().len());
        assert!(Arc::ptr_eq(
            &raw.pixels.decode(&mut None).unwrap(),
            &source.image
        ));
    }

    #[test]
    fn cache_size_changes_the_key_and_byte_budget_with_bounded_values() {
        let cfg = config(Path::new("unused"));
        let default = Key::new(&cfg);
        assert_eq!(default.budget_bytes(), 32 * 1024 * 1024);
        let larger = Key::new(&cfg).with_size_mib(64);
        assert_ne!(default, larger);
        assert_eq!(larger.budget_bytes(), 64 * 1024 * 1024);
        assert_eq!(Key::new(&cfg).with_size_mib(0).budget_bytes(), 1024 * 1024);
        assert_eq!(
            Key::new(&cfg).with_size_mib(u32::MAX).budget_bytes(),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn changing_cache_size_switches_between_streaming_and_full_loop_cache() {
        let path = path("sized.gif");
        gif(&path);
        let cfg = config(&path);
        let mut player = BackgroundPlayer::new();
        player.request(Key::new(&cfg).with_size_mib(1));
        wait(&mut player);
        assert!(player.cached.is_none(), "two raw canvases exceed 1 MiB");
        player.request(Key::new(&cfg).with_size_mib(2));
        wait(&mut player);
        assert_eq!(player.cached.as_ref().unwrap().frames.len(), 2);
        player.request(Key::new(&cfg).with_size_mib(1));
        wait(&mut player);
        assert!(
            player.cached.is_none(),
            "shrinking the limit must discard the full cache"
        );
        player.clear();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn compression_mode_invalidates_the_cache_key() {
        let cfg = config(Path::new("unused"));
        assert_ne!(
            Key::new(&cfg),
            Key::new(&cfg).with_compression(BackgroundCacheCompression::Lz4Hc9)
        );
    }

    #[test]
    fn compressed_budget_counts_stored_bytes_and_overflow_streams_in_order() {
        let shared = Shared::default();
        let (tx, rx) = mpsc::sync_channel(QUEUE_FRAMES);
        let request = Request {
            key: Key::new(&config(Path::new("unused")))
                .with_compression(BackgroundCacheCompression::Lz4Hc9),
            generation: 0,
            tx,
        };
        let size = CachedFrame::prepare(
            frame(10, video_period()),
            BackgroundCacheCompression::Lz4Hc9,
        )
        .unwrap()
        .pixels
        .stored_bytes();
        let mut output = Accumulator {
            request: &request,
            shared: &shared,
            frames: vec![],
            bytes: 0,
            budget: size * 2,
            streaming: false,
            seen: 0,
        };
        for color in [10, 20] {
            assert!(output
                .emit(
                    RgbImage::from_pixel(1, 1, Rgb([color, 0, 0])),
                    video_period()
                )
                .unwrap());
        }
        assert_eq!(
            output.bytes,
            output
                .frames
                .iter()
                .map(|f| f.pixels.stored_bytes())
                .sum::<usize>()
        );
        assert!(output.bytes <= size * 2);
        assert!(output.bytes < (W * H * 3) as usize);
        for color in [30, 40] {
            assert!(output
                .emit(
                    RgbImage::from_pixel(1, 1, Rgb([color, 0, 0])),
                    video_period()
                )
                .unwrap());
        }
        assert!(output.streaming);
        assert!(output.frames.is_empty());
        assert_eq!(output.bytes, 0);
        for color in [10, 20, 30, 40] {
            let Message::Stream(frame) = rx.try_recv().unwrap() else {
                panic!("expected raw streaming fallback");
            };
            assert_eq!(frame.image.get_pixel(240, 240)[0], color);
        }
    }

    #[test]
    fn invalid_compressed_payload_reports_an_error_without_displaying_a_frame() {
        let mut player = BackgroundPlayer::new();
        player.cached = Some(Loop {
            frames: vec![CachedFrame {
                pixels: CachedPixels::Lz4(vec![0].into_boxed_slice()),
                delay: video_period(),
                dimensions: (W, H),
            }],
            total: video_period(),
            animated: true,
        });
        assert!(player.next_frame(Instant::now()).is_none());
        assert!(player.error.is_some());
        assert!(player.cached.is_none());
        assert!(player.interval().is_none());
    }

    #[test]
    fn cached_clock_preserves_variable_delays_and_reuses_frames_at_loop_seam() {
        let mut player = BackgroundPlayer::new();
        let (tx, rx) = mpsc::sync_channel(1);
        let red = frame(10, Duration::from_millis(70));
        let blue = frame(200, Duration::from_millis(130));
        tx.send(Message::Cached(Loop {
            frames: vec![
                CachedFrame::prepare(red.clone(), BackgroundCacheCompression::Raw).unwrap(),
                CachedFrame::prepare(blue.clone(), BackgroundCacheCompression::Raw).unwrap(),
            ],
            total: Duration::from_millis(200),
            animated: true,
        }))
        .ok()
        .unwrap();
        player.receiver = Some(rx);
        let start = Instant::now();
        assert!(Arc::ptr_eq(&player.next_frame(start).unwrap(), &red.image));
        assert!(player
            .next_frame(start + Duration::from_millis(69))
            .is_none());
        assert!(Arc::ptr_eq(
            &player
                .next_frame(start + Duration::from_millis(70))
                .unwrap(),
            &blue.image
        ));
        assert_eq!(player.interval(), Some(Duration::from_millis(130)));
        assert!(player
            .next_frame(start + Duration::from_millis(199))
            .is_none());
        assert!(Arc::ptr_eq(
            &player
                .next_frame(start + Duration::from_millis(200))
                .unwrap(),
            &red.image
        ));
    }

    #[test]
    fn memory_overflow_moves_cached_prefix_into_ordered_stream_without_extra_retention() {
        let shared = Shared::default();
        let (tx, rx) = mpsc::sync_channel(QUEUE_FRAMES);
        let request = Request {
            key: Key::new(&config(Path::new("unused"))),
            generation: 0,
            tx,
        };
        let bytes = (W * H * 3) as usize;
        let mut output = Accumulator {
            request: &request,
            shared: &shared,
            frames: vec![],
            bytes: 0,
            budget: bytes * 2,
            streaming: false,
            seen: 0,
        };
        for color in [10, 20] {
            assert!(output
                .emit(
                    RgbImage::from_pixel(1, 1, Rgb([color, 0, 0])),
                    video_period()
                )
                .unwrap());
        }
        assert_eq!(output.bytes, bytes * 2);
        assert!(rx.try_recv().is_err());
        assert!(output
            .emit(RgbImage::from_pixel(1, 1, Rgb([30, 0, 0])), video_period())
            .unwrap());
        assert!(output.streaming);
        assert!(output.frames.is_empty());
        assert_eq!(output.bytes, 0);
        for color in [10, 20, 30] {
            let Message::Stream(frame) = rx.try_recv().unwrap() else {
                panic!("expected streaming fallback");
            };
            assert_eq!(frame.image.dimensions(), (W, H));
            assert_eq!(frame.image.get_pixel(W / 2, H / 2)[0], color);
        }
        drop(rx);
        assert!(!output.emit(RgbImage::new(1, 1), video_period()).unwrap());
    }

    #[test]
    fn gif_prepares_only_target_canvases_and_retains_original_dimensions() {
        let path = path("prepared.gif");
        gif(&path);
        let mut cfg = config(&path);
        cfg.fit = crate::config::ImageFit::Native;
        cfg.zoom = 0.5;
        cfg.pan_x = 30.0;
        let mut player = BackgroundPlayer::new();
        player.request(Key::new(&cfg));
        let image = wait(&mut player);
        assert_eq!(image.dimensions(), (W, H));
        assert_eq!(player.dimensions, Some((640, 360)));
        let cached = player.cached.as_ref().unwrap();
        assert_eq!(cached.frames.len(), 2);
        assert_eq!(cached.total, Duration::from_millis(200));
        assert_eq!(cached.frames[0].delay, Duration::from_millis(70));
        assert_eq!(cached.frames[1].delay, Duration::from_millis(130));
        assert!(cached
            .frames
            .iter()
            .all(|f| f.pixels.decode(&mut None).unwrap().dimensions() == (W, H)));
        let native = RgbImage::from_pixel(640, 360, Rgb([10, 0, 0]));
        assert_eq!(
            *image,
            super::super::transform_background_image(&native, &cfg).unwrap()
        );
        assert!(player.shared.child.lock().unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn over_budget_gif_reopens_in_order_and_cancellation_unblocks_the_worker() {
        let path = path("stream.gif");
        gif(&path);
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        let (tx, rx) = mpsc::sync_channel(QUEUE_FRAMES);
        let request = Request {
            key: Key::new(&config(&path)),
            generation: 0,
            tx,
        };
        let thread = std::thread::spawn(move || prepare(&request, &worker, 0));
        for color in [10, 200, 10, 200] {
            let Message::Stream(frame) = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
                panic!("expected stream");
            };
            assert_eq!(frame.image.get_pixel(240, 240)[0], color);
        }
        shared.generation.fetch_add(1, Ordering::AcqRel);
        drop(rx);
        thread.join().unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rapid_edits_keep_one_worker_and_cannot_publish_stale_results() {
        let path = path("latest.png");
        RgbImage::from_pixel(16, 8, Rgb([200, 0, 0]))
            .save(&path)
            .unwrap();
        let mut player = BackgroundPlayer::new();
        let worker = Arc::clone(&player.shared);
        let mut cfg = config(&path);
        for offset in 0..80 {
            cfg.pan_x = offset as f32;
            player.request(Key::new(&cfg));
            assert!(Arc::ptr_eq(&player.shared, &worker));
        }
        cfg.opacity = 0;
        cfg.background_color = [1, 2, 3];
        player.request(Key::new(&cfg));
        let image = wait(&mut player);
        assert!(image.pixels().all(|p| p.0 == [1, 2, 3]));
        assert_eq!(player.shared.generation.load(Ordering::Acquire), 81);
        player.clear();
        assert!(player.cached.is_none());
        assert!(player.interval().is_none());
        assert!(player.next_frame(Instant::now()).is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_replacement_changes_identity_even_when_path_and_size_match() {
        let path = path("replace.png");
        let replacement = path.with_extension("new.png");
        RgbImage::from_pixel(16, 8, Rgb([100, 0, 0]))
            .save(&path)
            .unwrap();
        let cfg = config(&path);
        let before = Key::new(&cfg);
        std::fs::copy(&path, &replacement).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let after = Key::new(&cfg);
        assert_eq!(before.len, after.len);
        assert_ne!(before, after);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn streaming_clock_handles_fast_gifs_and_does_not_accelerate_at_gui_refresh_rate() {
        let mut player = BackgroundPlayer::new();
        let (tx, rx) = mpsc::sync_channel(QUEUE_FRAMES);
        for index in 0..QUEUE_FRAMES {
            tx.send(Message::Stream(frame(
                index as u8,
                Duration::from_millis(10),
            )))
            .ok()
            .unwrap();
        }
        player.receiver = Some(rx);
        let now = Instant::now();
        assert_eq!(player.next_frame(now).unwrap().get_pixel(0, 0)[0], 0);
        assert!(player.next_frame(now + Duration::from_millis(9)).is_none());
        assert_eq!(
            player
                .next_frame(now + Duration::from_millis(41))
                .unwrap()
                .get_pixel(0, 0)[0],
            4
        );
        assert!(player.next_frame(now + Duration::from_millis(41)).is_none());
        assert_eq!(
            player
                .next_frame(now + Duration::from_secs(3600))
                .unwrap()
                .get_pixel(0, 0)[0],
            7
        );
        assert!(player.next_frame(now + Duration::from_secs(3600)).is_none());
    }

    #[test]
    #[ignore = "manual live cache performance diagnostic; no device access"]
    fn live_background_cache_budget_and_cadence() {
        let path = std::env::var("TH420_BENCH_CONFIG").expect("set TH420_BENCH_CONFIG");
        let cfg = crate::config::Config::load(&PathBuf::from(path)).unwrap();
        let mut player = BackgroundPlayer::new();
        let start = Instant::now();
        player.request(
            Key::new(&cfg.background)
                .with_compression(cfg.background_cache_compression)
                .with_size_mib(cfg.background_cache_size_mib),
        );
        wait(&mut player);
        let cached = player
            .cached
            .as_ref()
            .expect("diagnostic clip must fit cache");
        assert!(
            cached.animated && cached.frames.len() > 1,
            "diagnostic requires a multi-frame animation"
        );
        let bytes: usize = cached.frames.iter().map(|f| f.pixels.stored_bytes()).sum();
        assert!(
            bytes
                <= Key::new(&cfg.background)
                    .with_size_mib(cfg.background_cache_size_mib)
                    .budget_bytes()
        );
        assert!(player.shared.child.lock().unwrap().is_none());
        if let Some(directory) = std::env::var_os("TH420_BENCH_EXPORT_DIR") {
            let directory = PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            for (index, frame) in cached.frames.iter().enumerate() {
                std::fs::write(
                    directory.join(format!("frame-{index:04}.rgb")),
                    frame.pixels.decode(&mut None).unwrap().as_raw(),
                )
                .unwrap();
            }
            std::fs::write(
                directory.join("manifest.toml"),
                format!(
                    "width = {W}\nheight = {H}\nchannels = 3\nframes = {}\n",
                    cached.frames.len()
                ),
            )
            .unwrap();
        }
        eprintln!(
            "prepared {} frames in {:.1}ms; retained {:.2}MiB",
            cached.frames.len(),
            start.elapsed().as_secs_f64() * 1000.0,
            bytes as f64 / 1048576.0
        );
        let mut updates = 0;
        let mut deadline = Instant::now();
        for _ in 0..72 {
            deadline += video_period();
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            if player.next_frame(Instant::now()).is_some() {
                updates += 1;
            }
        }
        eprintln!("cached playback: {updates}/72 updates");
        assert!(updates >= 68);
    }
}
