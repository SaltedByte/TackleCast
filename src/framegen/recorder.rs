//! Diagnostic clip recording and loading.
//!
//! A clip is the raw captured NV12 frames of a few seconds of real gameplay,
//! with their arrival timestamps, written to `recordings/clip_<time>/`:
//!
//! * `frame_00000.nv12` ... — Y plane followed by the interleaved UV plane,
//!   exactly as the capture card delivered them;
//! * `index.csv` — `index,arrival_us,width,height` per stored frame (arrival
//!   time relative to the first frame, from the same monotonic clock the
//!   live pipeline paces with);
//! * `clip.json` — format, colour space/range, counts, settings.
//!
//! `--fg-replay` feeds a clip through the real interpolation pipeline, so
//! different artifact-reduction modes can be compared on identical frames.
//!
//! Recording never blocks the UI thread: frames are copied into a bounded
//! queue drained by a writer thread; if the disk cannot keep up the frame is
//! dropped (and counted) instead.

use crate::capture::{CaptureFrame, PixelFormat};
use crate::settings::{ColorRange, ColorSpace};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Frames buffered between the UI thread and the writer (~0.3 GB at 1440p).
const QUEUE_FRAMES: usize = 48;
pub const CLIP_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipMeta {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub format: String,
    /// `rec709`, `bt601` or `bt2020` (as resolved by the live renderer).
    pub color_space: String,
    /// `limited` or `full`.
    pub color_range: String,
    pub nominal_fps: f64,
    pub frames_stored: u32,
    /// Frames that arrived while recording but could not be queued.
    pub frames_dropped: u32,
    pub duration_s: f64,
    pub recorded_at: String,
    pub app_version: String,
    /// Frame-generation settings at recording time (for reference only).
    pub framegen_settings: String,
}

/// Fixed facts of a recording, known when it starts.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub color_space: ColorSpace,
    pub color_range: ColorRange,
    pub nominal_fps: f64,
    pub framegen_settings: String,
}

enum Msg {
    Frame {
        index: u32,
        arrival_us: u64,
        width: u32,
        height: u32,
        y: Vec<u8>,
        uv: Vec<u8>,
    },
    Finish(Box<ClipMeta>),
}

struct Active {
    tx: Sender<Msg>,
    done: Receiver<Result<String, String>>,
    dir: PathBuf,
    info: ClipInfo,
    started: Instant,
    until: Instant,
    first: Option<Instant>,
    size: Option<(u32, u32)>,
    stored: u32,
    dropped: u32,
    unsupported: bool,
}

/// Records diagnostic clips; owned by the UI thread.
#[derive(Default)]
pub struct ClipRecorder {
    active: Option<Active>,
    /// Writer still finishing a stopped recording.
    finishing: Option<(Receiver<Result<String, String>>, PathBuf)>,
    status: String,
}

impl ClipRecorder {
    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn is_busy(&self) -> bool {
        self.active.is_some() || self.finishing.is_some()
    }

    /// Start recording the next `duration` of captured frames into a new
    /// folder under `base`.
    pub fn start(&mut self, base: &Path, duration: Duration, info: ClipInfo, now: Instant) {
        if self.is_busy() {
            return;
        }
        let stamp = time::OffsetDateTime::now_local()
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
            .format(time::macros::format_description!(
                "[year][month][day]_[hour][minute][second]"
            ))
            .unwrap_or_else(|_| "clip".into());
        let dir = base.join(format!("clip_{stamp}"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.status = format!("Recording failed: cannot create {}: {e}", dir.display());
            return;
        }
        let (tx, rx) = bounded::<Msg>(QUEUE_FRAMES);
        let (done_tx, done_rx) = bounded(1);
        let writer_dir = dir.clone();
        let spawned = std::thread::Builder::new()
            .name("tacklecast-clip-writer".into())
            .spawn(move || {
                let _ = done_tx.send(write_clip(&writer_dir, rx));
            });
        if let Err(e) = spawned {
            self.status = format!("Recording failed: {e}");
            return;
        }
        tracing::info!(
            "diagnostic clip recording started: {} ({:.0} s)",
            dir.display(),
            duration.as_secs_f64()
        );
        self.status = format!("Recording to {} ...", dir.display());
        self.active = Some(Active {
            tx,
            done: done_rx,
            dir,
            info,
            started: now,
            until: now + duration,
            first: None,
            size: None,
            stored: 0,
            dropped: 0,
            unsupported: false,
        });
    }

    /// Offer a captured frame (call for every uploaded frame).
    pub fn push(&mut self, frame: &CaptureFrame, arrival: Instant) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if arrival >= active.until {
            self.stop(arrival);
            return;
        }
        #[allow(unreachable_patterns)] // Only one variant without gpu-decode.
        let (width, height, format, y_data, u_data) = match frame {
            CaptureFrame::Cpu {
                width,
                height,
                format,
                y_data,
                u_data,
                ..
            } => (width, height, format, y_data, u_data),
            _ => {
                active.unsupported = true;
                active.dropped += 1;
                return;
            }
        };
        if *format != PixelFormat::Nv12 {
            active.unsupported = true;
            active.dropped += 1;
            return;
        }
        if active.size.is_some_and(|s| s != (*width, *height)) {
            // Resolution change mid-clip: stop rather than mix sizes.
            self.stop(arrival);
            return;
        }
        active.size = Some((*width, *height));
        let first = *active.first.get_or_insert(arrival);
        let y_len = (*width as usize) * (*height as usize);
        let uv_len = y_len / 2;
        if y_data.len() < y_len || u_data.len() < uv_len {
            active.dropped += 1;
            return;
        }
        let msg = Msg::Frame {
            index: active.stored,
            arrival_us: arrival.saturating_duration_since(first).as_micros() as u64,
            width: *width,
            height: *height,
            y: y_data[..y_len].to_vec(),
            uv: u_data[..uv_len].to_vec(),
        };
        match active.tx.try_send(msg) {
            Ok(()) => active.stored += 1,
            Err(TrySendError::Full(_)) => active.dropped += 1,
            Err(TrySendError::Disconnected(_)) => {
                self.status = "Recording failed: writer stopped".into();
                self.active = None;
            }
        }
    }

    /// Periodic update: ends a recording whose time is up even if frames
    /// stopped arriving, and reports when the writer has finished.
    pub fn tick(&mut self, now: Instant) {
        if let Some(active) = &self.active {
            if now >= active.until {
                self.stop(now);
            } else {
                self.status = format!(
                    "Recording... {:.1} s, {} frames{}",
                    now.saturating_duration_since(active.started).as_secs_f64(),
                    active.stored,
                    if active.dropped > 0 {
                        format!(", {} not stored", active.dropped)
                    } else {
                        String::new()
                    }
                );
            }
        }
        if let Some((done, dir)) = &self.finishing {
            match done.try_recv() {
                Ok(Ok(summary)) => {
                    tracing::info!("diagnostic clip saved: {summary}");
                    self.status = format!("Saved: {summary}");
                    self.finishing = None;
                }
                Ok(Err(e)) => {
                    tracing::warn!("diagnostic clip failed: {e}");
                    self.status = format!("Recording failed: {e}");
                    self.finishing = None;
                }
                Err(_) => {
                    self.status = format!("Writing {} ...", dir.display());
                }
            }
        }
    }

    fn stop(&mut self, now: Instant) {
        let Some(active) = self.active.take() else {
            return;
        };
        let (width, height) = active.size.unwrap_or((0, 0));
        let meta = ClipMeta {
            version: CLIP_VERSION,
            width,
            height,
            format: "nv12".into(),
            color_space: match active.info.color_space {
                ColorSpace::Bt601 => "bt601",
                ColorSpace::Bt2020 => "bt2020",
                _ => "rec709",
            }
            .into(),
            color_range: match active.info.color_range {
                ColorRange::Full => "full",
                _ => "limited",
            }
            .into(),
            nominal_fps: active.info.nominal_fps,
            frames_stored: active.stored,
            frames_dropped: active.dropped,
            duration_s: now.saturating_duration_since(active.started).as_secs_f64(),
            recorded_at: time::OffsetDateTime::now_local()
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
                .to_string(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            framegen_settings: active.info.framegen_settings.clone(),
        };
        if active.unsupported {
            tracing::warn!(
                "diagnostic clip: {} frames were not in CPU NV12 form (GPU decode path) and \
                 could not be recorded",
                active.dropped
            );
        }
        // A blocking send is fine: the writer drains the queue continuously.
        let _ = active.tx.send(Msg::Finish(Box::new(meta)));
        self.status = format!("Writing {} ...", active.dir.display());
        self.finishing = Some((active.done, active.dir));
    }
}

fn write_clip(dir: &Path, rx: Receiver<Msg>) -> Result<String, String> {
    let mut index = std::io::BufWriter::new(
        std::fs::File::create(dir.join("index.csv")).map_err(|e| e.to_string())?,
    );
    writeln!(index, "index,arrival_us,width,height").map_err(|e| e.to_string())?;
    let mut bytes = 0u64;
    for msg in rx {
        match msg {
            Msg::Frame {
                index: i,
                arrival_us,
                width,
                height,
                y,
                uv,
            } => {
                let path = dir.join(frame_file(i));
                let mut file = std::io::BufWriter::with_capacity(
                    1 << 20,
                    std::fs::File::create(&path).map_err(|e| e.to_string())?,
                );
                file.write_all(&y).map_err(|e| e.to_string())?;
                file.write_all(&uv).map_err(|e| e.to_string())?;
                file.flush().map_err(|e| e.to_string())?;
                bytes += (y.len() + uv.len()) as u64;
                writeln!(index, "{i},{arrival_us},{width},{height}").map_err(|e| e.to_string())?;
            }
            Msg::Finish(meta) => {
                index.flush().map_err(|e| e.to_string())?;
                let json = serde_json::to_string_pretty(&*meta).map_err(|e| e.to_string())?;
                std::fs::write(dir.join("clip.json"), json).map_err(|e| e.to_string())?;
                return Ok(format!(
                    "{} frames ({:.0} MB, {} not stored) in {}",
                    meta.frames_stored,
                    bytes as f64 / 1.0e6,
                    meta.frames_dropped,
                    dir.display()
                ));
            }
        }
    }
    Err("recording ended without finishing".into())
}

pub fn frame_file(index: u32) -> String {
    format!("frame_{index:05}.nv12")
}

/// A recorded clip on disk.
pub struct Clip {
    pub dir: PathBuf,
    pub meta: ClipMeta,
    /// (index, arrival in seconds) per stored frame.
    pub frames: Vec<(u32, f64)>,
}

impl Clip {
    pub fn open(dir: &Path) -> Result<Self, String> {
        let meta: ClipMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.join("clip.json"))
                .map_err(|e| format!("{}: {e}", dir.join("clip.json").display()))?,
        )
        .map_err(|e| format!("clip.json: {e}"))?;
        if meta.format != "nv12" || meta.width == 0 || meta.height == 0 {
            return Err(format!(
                "unsupported clip: {} {}x{}",
                meta.format, meta.width, meta.height
            ));
        }
        let index = std::fs::read_to_string(dir.join("index.csv"))
            .map_err(|e| format!("index.csv: {e}"))?;
        let mut frames = Vec::new();
        for line in index.lines().skip(1) {
            let mut parts = line.split(',');
            let i: u32 = parts
                .next()
                .and_then(|v| v.trim().parse().ok())
                .ok_or_else(|| format!("index.csv: bad line '{line}'"))?;
            let t: u64 = parts
                .next()
                .and_then(|v| v.trim().parse().ok())
                .ok_or_else(|| format!("index.csv: bad line '{line}'"))?;
            frames.push((i, t as f64 / 1.0e6));
        }
        if frames.len() < 3 {
            return Err("clip has fewer than 3 frames".into());
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            meta,
            frames,
        })
    }

    /// Load stored frame `index` and convert it to RGBA8 the way the live
    /// renderer does (neutral picture adjustments).
    pub fn load_rgba(&self, index: u32) -> Result<Vec<u8>, String> {
        let path = self.dir.join(frame_file(index));
        let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (w, h) = (self.meta.width as usize, self.meta.height as usize);
        if data.len() < w * h * 3 / 2 {
            return Err(format!("{}: truncated", path.display()));
        }
        let (y, uv) = data.split_at(w * h);
        Ok(nv12_to_rgba(
            y,
            uv,
            self.meta.width,
            self.meta.height,
            &self.meta.color_space,
            self.meta.color_range == "full",
        ))
    }
}

/// NV12 to RGBA8 with the live shader's matrices (Rec. 709 / BT.601 /
/// BT.2020 coefficients; the 601/2020 gamut conversion to the display is
/// omitted — irrelevant for artifact analysis). Uses all CPU cores.
pub fn nv12_to_rgba(
    y: &[u8],
    uv: &[u8],
    width: u32,
    height: u32,
    space: &str,
    full_range: bool,
) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let (kr_v, kg_u, kg_v, kb_u) = match space {
        "bt601" => (1.402, 0.344136, 0.714136, 1.772),
        "bt2020" => (1.4746, 0.164553, 0.571353, 1.8814),
        _ => (1.5748, 0.187324, 0.468124, 1.8556),
    };
    let mut out = vec![0u8; w * h * 4];
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 32);
    let rows_per = h.div_ceil(threads).next_multiple_of(2).max(2);
    std::thread::scope(|scope| {
        for (chunk_index, chunk) in out.chunks_mut(rows_per * w * 4).enumerate() {
            scope.spawn(move || {
                let row0 = chunk_index * rows_per;
                for (r, row) in chunk.chunks_mut(w * 4).enumerate() {
                    let yy = row0 + r;
                    let uv_row = (yy / 2) * w;
                    for x in 0..w {
                        let luma = y[yy * w + x] as f32 / 255.0;
                        let u = uv[uv_row + (x & !1)] as f32 / 255.0;
                        let v = uv[uv_row + (x & !1) + 1] as f32 / 255.0;
                        let (l, cb, cr) = if full_range {
                            (luma, u - 128.0 / 255.0, v - 128.0 / 255.0)
                        } else {
                            (
                                ((luma - 16.0 / 255.0) * (255.0 / 219.0)).clamp(0.0, 1.0),
                                (u - 128.0 / 255.0) * (255.0 / 224.0),
                                (v - 128.0 / 255.0) * (255.0 / 224.0),
                            )
                        };
                        let rgb = [l + kr_v * cr, l - kg_u * cb - kg_v * cr, l + kb_u * cb];
                        let px = &mut row[x * 4..x * 4 + 4];
                        for c in 0..3 {
                            px[c] = (rgb[c].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                        }
                        px[3] = 255;
                    }
                }
            });
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorInfo;

    fn nv12_frame(w: u32, h: u32, shade: u8) -> CaptureFrame {
        CaptureFrame::Cpu {
            width: w,
            height: h,
            format: PixelFormat::Nv12,
            color_info: ColorInfo::default(),
            y_data: vec![shade; (w * h) as usize],
            u_data: vec![128; (w * h / 2) as usize],
            v_data: Vec::new(),
        }
    }

    fn wait_saved(rec: &mut ClipRecorder) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while rec.is_busy() && Instant::now() < deadline {
            rec.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!rec.is_busy(), "writer did not finish: {}", rec.status());
    }

    #[test]
    fn records_and_reloads_a_clip_with_timestamps() {
        let base = std::env::temp_dir().join(format!("tc-clip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut rec = ClipRecorder::default();
        let t0 = Instant::now();
        rec.start(
            &base,
            Duration::from_secs(10),
            ClipInfo {
                color_space: ColorSpace::Rec709,
                color_range: ColorRange::Limited,
                nominal_fps: 30.0,
                framegen_settings: "test".into(),
            },
            t0,
        );
        assert!(rec.is_busy());
        for k in 0..5u32 {
            let shade = 16 + 40 * k as u8;
            rec.push(
                &nv12_frame(64, 32, shade),
                t0 + Duration::from_micros(33_333 * k as u64),
            );
        }
        // Time is up: the next frame ends the recording.
        rec.push(&nv12_frame(64, 32, 0), t0 + Duration::from_secs(11));
        wait_saved(&mut rec);
        assert!(
            rec.status().starts_with("Saved: 5 frames"),
            "{}",
            rec.status()
        );

        let dir = std::fs::read_dir(&base)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let clip = Clip::open(&dir).unwrap();
        assert_eq!(clip.meta.frames_stored, 5);
        assert_eq!(clip.frames.len(), 5);
        assert!((clip.frames[3].1 - 0.1).abs() < 1e-3, "{:?}", clip.frames);
        // Limited-range Y=16 is black, Y=176 is (176-16)/219 = 73% grey.
        assert_eq!(&clip.load_rgba(0).unwrap()[..4], &[0, 0, 0, 255]);
        let grey = clip.load_rgba(4).unwrap()[0];
        assert!((grey as i32 - 186).abs() <= 1, "grey {grey}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_full_queue_drops_instead_of_blocking() {
        let (tx, _rx_kept_full) = bounded::<Msg>(1);
        let (_done_tx, done) = bounded(1);
        let mut rec = ClipRecorder::default();
        let now = Instant::now();
        rec.active = Some(Active {
            tx,
            done,
            dir: PathBuf::from("unused"),
            info: ClipInfo {
                color_space: ColorSpace::Rec709,
                color_range: ColorRange::Limited,
                nominal_fps: 30.0,
                framegen_settings: String::new(),
            },
            started: now,
            until: now + Duration::from_secs(5),
            first: None,
            size: None,
            stored: 0,
            dropped: 0,
            unsupported: false,
        });
        let started = Instant::now();
        for k in 0..10 {
            rec.push(&nv12_frame(64, 32, 50), now + Duration::from_millis(k));
        }
        assert!(started.elapsed() < Duration::from_millis(500));
        let active = rec.active.as_ref().unwrap();
        assert_eq!(active.stored, 1);
        assert_eq!(active.dropped, 9);
    }

    #[test]
    fn rgb_conversion_matches_the_shader_for_rec709_limited() {
        // Limited-range Rec. 709, 4x2 pixels = two 2x2 chroma blocks: the
        // left one white/black luma with neutral chroma, the right one red.
        let y = [235u8, 16, 63, 63, 235, 16, 63, 63];
        let uv = [128u8, 128, 102, 240];
        let rgba = nv12_to_rgba(&y, &uv, 4, 2, "rec709", false);
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 255]);
        // Y=63, Cb=102, Cr=240 is Rec. 709 red (~255, 0, 0).
        assert!(
            rgba[8] > 245 && rgba[9] < 12 && rgba[10] < 12,
            "{:?}",
            &rgba[8..12]
        );
    }
}
