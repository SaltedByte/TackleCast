use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tracing::{info, warn};

/// Real-time audio connection status reported to the UI and logger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStatus {
    Connected,
    NoInput,
    NoOutput,
    Error(String),
    Stopped,
}

impl std::fmt::Display for AudioStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => write!(f, "Connected"),
            Self::NoInput => write!(f, "No Input Device"),
            Self::NoOutput => write!(f, "No Output Device"),
            Self::Error(msg) => write!(f, "Error: {msg}"),
            Self::Stopped => write!(f, "Stopped"),
        }
    }
}

/// Metadata about an active audio stream (for logging and UI inspection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl std::fmt::Display for AudioStreamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' ({} ch, {} Hz, {:?})",
            self.device_name, self.channels, self.sample_rate, self.sample_format
        )
    }
}

/// Diagnostic state for monitoring the audio pipeline.
pub struct AudioDiagnostics {
    status_code: AtomicU32, // 0: Stopped, 1: Connected, 2: NoInput, 3: NoOutput, 4: Error
    last_error: RwLock<Option<String>>,
    input_info: RwLock<Option<AudioStreamInfo>>,
    output_info: RwLock<Option<AudioStreamInfo>>,
    underrun_counter: AtomicUsize,
    overrun_counter: AtomicUsize,
    last_underrun_log: Mutex<Option<Instant>>,
    last_overrun_log: Mutex<Option<Instant>>,
    resync_counter: AtomicUsize,
}

impl AudioDiagnostics {
    pub fn new() -> Self {
        Self {
            status_code: AtomicU32::new(0),
            last_error: RwLock::new(None),
            input_info: RwLock::new(None),
            output_info: RwLock::new(None),
            underrun_counter: AtomicUsize::new(0),
            overrun_counter: AtomicUsize::new(0),
            last_underrun_log: Mutex::new(None),
            last_overrun_log: Mutex::new(None),
            resync_counter: AtomicUsize::new(0),
        }
    }

    pub fn set_status(&self, status: AudioStatus) {
        let code = match &status {
            AudioStatus::Stopped => 0,
            AudioStatus::Connected => 1,
            AudioStatus::NoInput => 2,
            AudioStatus::NoOutput => 3,
            AudioStatus::Error(err) => {
                if let Ok(mut lock) = self.last_error.write() {
                    *lock = Some(err.clone());
                }
                4
            }
        };
        if code != 4 {
            if let Ok(mut lock) = self.last_error.write() {
                *lock = None;
            }
        }
        self.status_code.store(code, Ordering::Release);
    }

    pub fn status(&self) -> AudioStatus {
        match self.status_code.load(Ordering::Acquire) {
            1 => AudioStatus::Connected,
            2 => AudioStatus::NoInput,
            3 => AudioStatus::NoOutput,
            4 => AudioStatus::Error(
                self.last_error
                    .read()
                    .ok()
                    .and_then(|lock| lock.clone())
                    .unwrap_or_else(|| "unknown error".to_string()),
            ),
            _ => AudioStatus::Stopped,
        }
    }

    pub fn set_input_info(&self, info: Option<AudioStreamInfo>) {
        if let Ok(mut lock) = self.input_info.write() {
            *lock = info;
        }
    }

    pub fn set_output_info(&self, info: Option<AudioStreamInfo>) {
        if let Ok(mut lock) = self.output_info.write() {
            *lock = info;
        }
    }

    #[allow(dead_code)]
    pub fn input_info(&self) -> Option<AudioStreamInfo> {
        self.input_info.read().ok().and_then(|lock| lock.clone())
    }

    #[allow(dead_code)]
    pub fn output_info(&self) -> Option<AudioStreamInfo> {
        self.output_info.read().ok().and_then(|lock| lock.clone())
    }

    pub fn record_underrun(&self) {
        let count = self.underrun_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut last_log) = self.last_underrun_log.lock() {
            let now = Instant::now();
            let should_log = match *last_log {
                Some(t) => now.duration_since(t).as_secs() >= 5,
                None => true,
            };
            if should_log {
                warn!(
                    "audio output underrun (buffer starvation, total: {} occurrences)",
                    count
                );
                *last_log = Some(now);
            }
        }
    }

    /// Resyncs are rare (only after a stall), so each one is logged.
    pub fn record_resync(&self, dropped_ms: f64) {
        let count = self.resync_counter.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(
            "audio resync: dropped {:.0} ms of backlog to restore sync (total: {} occurrences)",
            dropped_ms, count
        );
    }

    #[allow(dead_code)]
    pub fn resync_count(&self) -> usize {
        self.resync_counter.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub fn underrun_count(&self) -> usize {
        self.underrun_counter.load(Ordering::Relaxed)
    }

    pub fn record_overrun(&self) {
        let count = self.overrun_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut last_log) = self.last_overrun_log.lock() {
            let now = Instant::now();
            let should_log = match *last_log {
                Some(t) => now.duration_since(t).as_secs() >= 5,
                None => true,
            };
            if should_log {
                warn!(
                    "audio input overrun (buffer overflow, dropped samples, total: {} occurrences)",
                    count
                );
                *last_log = Some(now);
            }
        }
    }
}

/// Convert any supported cpal sample to f32 in [-1.0, 1.0].
#[allow(dead_code)]
#[inline]
pub fn sample_to_f32<T: Sample>(sample: T) -> f32
where
    f32: FromSample<T>,
{
    f32::from_sample(sample)
}

/// Convert f32 in [-1.0, 1.0] to target sample type.
#[allow(dead_code)]
#[inline]
pub fn f32_to_sample<T: Sample + SizedSample + FromSample<f32>>(sample: f32) -> T {
    T::from_sample(sample.clamp(-1.0, 1.0))
}

/// Convert interleaved multi-channel audio from `in_channels` to `out_channels`.
/// Handles:
/// - mono -> stereo (duplicates sample to left and right)
/// - stereo -> mono (averages left and right: (L + R) * 0.5)
/// - pass-through when in_channels == out_channels
/// - multi-channel downmixing (N -> 2 or N -> 1)
pub fn convert_channels(
    input: &[f32],
    in_channels: usize,
    out_channels: usize,
    output: &mut Vec<f32>,
) {
    if in_channels == 0 || out_channels == 0 {
        return;
    }

    if in_channels == out_channels {
        output.extend_from_slice(input);
        return;
    }

    let frame_count = input.len() / in_channels;
    output.reserve(frame_count * out_channels);

    match (in_channels, out_channels) {
        // Mono to stereo: duplicate each sample
        (1, 2) => {
            for &s in input.iter().take(frame_count) {
                output.push(s);
                output.push(s);
            }
        }
        // Stereo to mono: average left and right
        (2, 1) => {
            for chunk in input.as_chunks::<2>().0.iter().take(frame_count) {
                output.push(0.5 * (chunk[0] + chunk[1]));
            }
        }
        // Mono to multi-channel (>2): replicate mono to all front channels
        (1, n) => {
            for &s in input.iter().take(frame_count) {
                for _ in 0..n {
                    output.push(s);
                }
            }
        }
        // Fold center, LFE and surround speakers into normalized stereo.
        (n, 2) if n > 2 => {
            for chunk in input.chunks_exact(n).take(frame_count) {
                let [left, right] = downmix_stereo(chunk);
                output.push(left);
                output.push(right);
            }
        }
        // Downmix multi-channel (>2) to mono: average all channels
        (n, 1) if n > 2 => {
            let inv = 1.0 / n as f32;
            for chunk in input.chunks_exact(n).take(frame_count) {
                let sum: f32 = chunk.iter().sum();
                output.push(sum * inv);
            }
        }
        // General arbitrary conversion: copy minimum channels, pad remaining with 0.0
        (n, m) => {
            let min_ch = n.min(m);
            for chunk in input.chunks_exact(n).take(frame_count) {
                output.extend_from_slice(&chunk[..min_ch]);
                for _ in min_ch..m {
                    output.push(0.0);
                }
            }
        }
    }
}

/// Uses the conventional speaker order: FL, FR, FC, LFE, BL, BR, SL, SR.
/// Quad omits FC/LFE; 5.0 omits LFE; 6.1 uses BC, SL, SR after LFE.
/// CPAL does not expose channel masks. Devices with a different ordering should
/// be routed to this order or configured for stereo PCM at the source.
fn downmix_stereo(frame: &[f32]) -> [f32; 2] {
    let surround = std::f32::consts::FRAC_1_SQRT_2;
    let mut mix = [frame[0], frame[1]];
    let mut weight = [1.0, 1.0];
    for (index, &sample) in frame.iter().enumerate().skip(2) {
        let gains = match (frame.len(), index) {
            (4, 2) | (5, 3) => [surround, 0.0],
            (4, 3) | (5, 4) => [0.0, surround],
            (_, 2) => [surround, surround], // center
            (7, 4) => [surround, surround], // back center
            (7, 5) => [surround, 0.0],
            (7, 6) => [0.0, surround],
            (n, 3) if n >= 6 => [0.5, 0.5], // LFE
            (_, i) if i % 2 == 0 => [surround, 0.0],
            _ => [0.0, surround],
        };
        for channel in 0..2 {
            mix[channel] += sample * gains[channel];
            weight[channel] += gains[channel];
        }
    }
    [mix[0] / weight[0], mix[1] / weight[1]]
}

/// Sub-frame resolution of the resampler's read position. A finer grid than
/// `1 / out_rate` lets the step be nudged by a few parts per million.
const RESAMPLER_PRECISION: u64 = 1 << 20;

/// Streaming Catmull-Rom interpolation with two frames of lookahead.
/// Fixed-point positions preserve the timeline across arbitrary callback boundaries.
/// The four-frame history is allocated once, rather than on every callback.
pub struct AudioResampler {
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    /// Interpolate even when the nominal rates match, so the step can be adjusted.
    adaptive: bool,
    /// Input frames advanced per output frame, in units of 1 / `denominator`.
    step: u64,
    denominator: u64,
    next_frame: u64,
    next_fraction: u64, // always < denominator
    frames_seen: u64,
    history: Vec<f32>,
}

impl AudioResampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        assert!(
            in_rate > 0 && out_rate > 0,
            "audio sample rates must be positive"
        );
        let channels = channels.max(1);
        Self {
            in_rate,
            out_rate,
            channels,
            adaptive: false,
            step: in_rate as u64 * RESAMPLER_PRECISION,
            denominator: out_rate as u64 * RESAMPLER_PRECISION,
            next_frame: 0,
            next_fraction: 0,
            frames_seen: 0,
            history: vec![0.0; channels * 4],
        }
    }

    /// A resampler whose ratio can be trimmed with [`Self::set_rate_adjustment`] to
    /// follow the real clocks of two devices, including at equal nominal rates.
    pub fn adaptive(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        Self {
            adaptive: true,
            ..Self::new(in_rate, out_rate, channels)
        }
    }

    /// Consume input `1 + adjustment` times faster than nominal. A positive value
    /// yields fewer output frames, draining a buffer that is filling up.
    pub fn set_rate_adjustment(&mut self, adjustment: f64) {
        debug_assert!(self.adaptive, "only adaptive resamplers can be adjusted");
        let nominal = (self.in_rate as u64 * RESAMPLER_PRECISION) as f64;
        self.step = (nominal * (1.0 + adjustment)).round().max(1.0) as u64;
    }

    #[allow(dead_code)]
    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    #[allow(dead_code)]
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    #[allow(dead_code)]
    pub fn channels(&self) -> usize {
        self.channels
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.next_frame = 0;
        self.next_fraction = 0;
        self.frames_seen = 0;
        self.history.fill(0.0);
    }

    /// Resample interleaved multi-channel input samples into the output buffer.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.in_rate == self.out_rate && !self.adaptive {
            // Identity: zero resampling required!
            output.extend_from_slice(input);
            return;
        }

        let channels = self.channels;
        for frame in input.chunks_exact(channels) {
            if self.frames_seen == 0 {
                for previous in self.history.chunks_exact_mut(channels) {
                    previous.copy_from_slice(frame);
                }
            } else {
                self.history.copy_within(channels.., 0);
                self.history[3 * channels..].copy_from_slice(frame);
            }
            while self.next_frame + 2 <= self.frames_seen {
                let t = (self.next_fraction as f64 / self.denominator as f64) as f32;
                for ch in 0..channels {
                    let y0 = self.history[ch];
                    let y1 = self.history[channels + ch];
                    let y2 = self.history[2 * channels + ch];
                    let y3 = self.history[3 * channels + ch];
                    let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
                    let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
                    let c = -0.5 * y0 + 0.5 * y2;
                    output.push(((a * t + b) * t + c) * t + y1);
                }
                self.next_fraction += self.step;
                self.next_frame += self.next_fraction / self.denominator;
                self.next_fraction %= self.denominator;
            }
            self.frames_seen += 1;
        }
    }
}

/// A thread-safe, lock-free ring buffer for real-time audio sample streaming.
/// Samples are stored as bitwise f32 in AtomicU32.
/// Audio buffered between capture and playback in steady state.
pub const PLAYBACK_TARGET_SECONDS: f64 = 0.040;
/// A backlog beyond this (a stalled device, say) is dropped back to the target.
pub const PLAYBACK_RESYNC_SECONDS: f64 = 0.150;

/// Playback buffer thresholds in interleaved samples of the output format.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackLatency {
    pub sample_rate: u32,
    pub channels: u16,
    pub target_samples: usize,
    pub resync_samples: usize,
    pub samples_per_second: f64,
}

impl PlaybackLatency {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let channels = channels.max(1) as usize;
        let samples = |seconds: f64| (sample_rate as f64 * seconds).round() as usize * channels;
        Self {
            target_samples: samples(PLAYBACK_TARGET_SECONDS),
            resync_samples: samples(PLAYBACK_RESYNC_SECONDS),
            sample_rate,
            channels: channels as u16,
            samples_per_second: sample_rate as f64 * channels as f64,
        }
    }

    pub fn seconds(&self, samples: usize) -> f64 {
        samples as f64 / self.samples_per_second
    }
}

const DRIFT_SMOOTHING_SECONDS: f64 = 1.0;
/// Rate correction per second of buffer error. With the integral gain this
/// gives a critically damped loop that settles in a few minutes.
const DRIFT_PROPORTIONAL_GAIN: f64 = 0.05;
const DRIFT_INTEGRAL_GAIN: f64 = 6e-4;
/// ±0.2% (about 3.5 cents) is inaudible and covers any sane pair of clocks.
const MAX_RATE_ADJUSTMENT: f64 = 0.002;
const DRIFT_REPORT_SECONDS: f64 = 30.0;

/// Keeps the playback buffer at its target by trimming the capture resampler.
/// The capture and playback devices run on separate crystals, so a fixed
/// ratio lets the buffer grow or shrink without bound over a long session.
pub struct DriftController {
    target_seconds: f64,
    smoothed: Option<f64>,
    integral: f64,
    adjustment: f64,
    report_elapsed: f64,
    report_min: f64,
    report_max: f64,
}

impl DriftController {
    pub fn new(target_seconds: f64) -> Self {
        Self {
            target_seconds,
            smoothed: None,
            integral: 0.0,
            adjustment: 0.0,
            report_elapsed: 0.0,
            report_min: f64::INFINITY,
            report_max: 0.0,
        }
    }

    /// Feed the buffer level after a capture callback covering `elapsed_seconds`.
    /// Returns the rate adjustment for [`AudioResampler::set_rate_adjustment`].
    pub fn update(&mut self, buffered_seconds: f64, elapsed_seconds: f64) -> f64 {
        let smoothed = match self.smoothed {
            None => buffered_seconds,
            Some(previous) => {
                let alpha = 1.0 - (-elapsed_seconds / DRIFT_SMOOTHING_SECONDS).exp();
                previous + (buffered_seconds - previous) * alpha
            }
        };
        self.smoothed = Some(smoothed);

        let error = smoothed - self.target_seconds;
        self.integral = (self.integral + DRIFT_INTEGRAL_GAIN * error * elapsed_seconds)
            .clamp(-MAX_RATE_ADJUSTMENT, MAX_RATE_ADJUSTMENT);
        self.adjustment = (DRIFT_PROPORTIONAL_GAIN * error + self.integral)
            .clamp(-MAX_RATE_ADJUSTMENT, MAX_RATE_ADJUSTMENT);

        self.report_min = self.report_min.min(buffered_seconds);
        self.report_max = self.report_max.max(buffered_seconds);
        self.report_elapsed += elapsed_seconds;
        if self.report_elapsed >= DRIFT_REPORT_SECONDS {
            info!(
                "audio sync: buffer {:.1} ms (range {:.1}-{:.1}, target {:.0}), clock correction {:+.0} ppm",
                smoothed * 1000.0,
                self.report_min * 1000.0,
                self.report_max * 1000.0,
                self.target_seconds * 1000.0,
                self.adjustment * 1e6,
            );
            self.report_elapsed = 0.0;
            self.report_min = f64::INFINITY;
            self.report_max = 0.0;
        }
        self.adjustment
    }

    /// Playback is waiting for audio; keep the learned clock offset but
    /// restart smoothing once it resumes.
    pub fn pause(&mut self) {
        self.smoothed = None;
    }

    #[allow(dead_code)]
    pub fn adjustment(&self) -> f64 {
        self.adjustment
    }
}

/// Pushes and discards whole interleaved frames, so channels never swap.
pub struct AudioRingBuffer {
    data: Box<[AtomicU32]>,
    capacity: usize,
    channels: usize,
    read_index: AtomicUsize,
    write_index: AtomicUsize,
    /// Set by the consumer once enough audio is buffered to start playback.
    primed: AtomicBool,
    diagnostics: Arc<AudioDiagnostics>,
}

impl AudioRingBuffer {
    pub fn new(capacity: usize, channels: usize, diagnostics: Arc<AudioDiagnostics>) -> Self {
        let channels = channels.max(1);
        let capacity = (capacity / channels).max(1) * channels;
        let mut values = Vec::with_capacity(capacity);
        values.resize_with(capacity, || AtomicU32::new(0));
        Self {
            data: values.into_boxed_slice(),
            capacity,
            channels,
            read_index: AtomicUsize::new(0),
            write_index: AtomicUsize::new(0),
            primed: AtomicBool::new(false),
            diagnostics,
        }
    }

    pub fn is_primed(&self) -> bool {
        self.primed.load(Ordering::Acquire)
    }

    /// Drop the oldest samples so at most `keep` remain. Consumer side only.
    pub fn discard_to(&self, keep: usize) -> usize {
        let read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        let available = write.wrapping_sub(read);
        if available <= keep {
            return 0;
        }
        let dropped = (available - keep) / self.channels * self.channels;
        if dropped > 0 {
            self.read_index
                .store(read.wrapping_add(dropped), Ordering::Release);
        }
        dropped
    }

    /// Fill `output` for playback. Consumer side only. Holds silence until the
    /// target latency is buffered, trims a backlog beyond the resync limit back
    /// to the target, and re-primes after the buffer runs dry.
    pub fn pull_playback(&self, output: &mut [f32], latency: &PlaybackLatency) -> usize {
        if !self.is_primed() {
            if self.available() < latency.target_samples {
                output.fill(0.0);
                return 0;
            }
            self.primed.store(true, Ordering::Release);
        }
        if self.available() > latency.resync_samples {
            let dropped = self.discard_to(latency.target_samples);
            self.diagnostics
                .record_resync(dropped as f64 / latency.samples_per_second * 1000.0);
        }
        let filled = self.pop_samples(output);
        if filled < output.len() {
            self.primed.store(false, Ordering::Release);
        }
        filled
    }

    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[allow(dead_code)]
    pub fn available(&self) -> usize {
        let write = self.write_index.load(Ordering::Acquire);
        let read = self.read_index.load(Ordering::Relaxed);
        write.wrapping_sub(read)
    }

    /// Pushes samples into the ring buffer. If buffer overflows, drops excess and logs.
    pub fn push_samples(&self, samples: &[f32]) -> usize {
        let mut written = 0;
        let mut write = self.write_index.load(Ordering::Relaxed);
        let read = self.read_index.load(Ordering::Acquire);
        let occupied = write.wrapping_sub(read);

        if occupied >= self.capacity {
            self.diagnostics.record_overrun();
            return 0;
        }

        let space = self.capacity - occupied;
        let to_write = samples.len().min(space) / self.channels * self.channels;

        if to_write < samples.len() {
            self.diagnostics.record_overrun();
        }

        while written < to_write {
            let slot = write % self.capacity;
            self.data[slot].store(samples[written].to_bits(), Ordering::Relaxed);
            write = write.wrapping_add(1);
            written += 1;
        }

        if written > 0 {
            self.write_index.store(write, Ordering::Release);
        }

        written
    }

    /// Pops samples into output slice. If buffer underruns, fills missing samples with 0.0.
    pub fn pop_samples(&self, output: &mut [f32]) -> usize {
        let mut read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        let available = write.wrapping_sub(read);
        let count = available.min(output.len());

        for sample in output.iter_mut().take(count) {
            let slot = read % self.capacity;
            *sample = f32::from_bits(self.data[slot].load(Ordering::Relaxed));
            read = read.wrapping_add(1);
        }

        if count < output.len() {
            // Buffer starvation / underrun
            for sample in &mut output[count..] {
                *sample = 0.0;
            }
            if available == 0 {
                self.diagnostics.record_underrun();
            }
        }

        if count > 0 {
            self.read_index.store(read, Ordering::Release);
        }

        count
    }
}

/// Volume and mute controller that can be safely updated from any thread and read in the audio callback.
#[derive(Clone)]
pub struct VolumeController {
    volume_bits: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
}

impl VolumeController {
    pub fn new(volume: f64, muted: bool) -> Self {
        let clamped = volume.clamp(0.0, 1.0) as f32;
        Self {
            volume_bits: Arc::new(AtomicU32::new(clamped.to_bits())),
            muted: Arc::new(AtomicBool::new(muted)),
        }
    }

    pub fn set_volume(&self, volume: f64) {
        let clamped = volume.clamp(0.0, 1.0) as f32;
        self.volume_bits.store(clamped.to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    /// Multiplier to apply to output samples (0.0 if muted, otherwise volume).
    #[inline]
    pub fn gain(&self) -> f32 {
        if self.is_muted() {
            0.0
        } else {
            self.volume()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_conversion_mono_to_stereo() {
        let input = [0.25f32, -0.5, 0.75];
        let mut output = Vec::new();
        convert_channels(&input, 1, 2, &mut output);
        assert_eq!(output.len(), 6);
        assert_eq!(output, vec![0.25, 0.25, -0.5, -0.5, 0.75, 0.75]);
    }

    #[test]
    fn channel_conversion_stereo_to_mono() {
        let input = [0.2f32, 0.4, -0.6, 0.2];
        let mut output = Vec::new();
        convert_channels(&input, 2, 1, &mut output);
        assert_eq!(output.len(), 2);
        assert!((output[0] - 0.3).abs() < 1e-5);
        assert!((output[1] - -0.2).abs() < 1e-5);
    }

    #[test]
    fn channel_conversion_identical_passthrough() {
        let input = [0.1f32, 0.2, 0.3, 0.4];
        let mut output = Vec::new();
        convert_channels(&input, 2, 2, &mut output);
        assert_eq!(output, input);
    }

    #[test]
    fn channel_conversion_multichannel_downmix() {
        for channels in [3, 4, 5, 6, 7, 8] {
            // Every speaker must remain audible, including center and LFE.
            for speaker in 0..channels {
                let mut input = vec![0.0; channels];
                input[speaker] = 1.0;
                let mut output = Vec::new();
                convert_channels(&input, channels, 2, &mut output);
                assert!(output.iter().any(|sample| *sample > 0.0));
                if speaker == 2 && channels != 4 {
                    assert_eq!(output[0], output[1], "center dialogue should be centered");
                }
            }
            let mut output = Vec::new();
            convert_channels(&vec![1.0; channels], channels, 2, &mut output);
            assert!(output.iter().all(|sample| *sample <= 1.0));
        }
    }

    #[test]
    fn quad_surround_preserves_left_and_right() {
        let mut output = Vec::new();
        convert_channels(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0], 4, 2, &mut output);
        assert!(output[0] > 0.0 && output[1] == 0.0);
        assert!(output[2] == 0.0 && output[3] > 0.0);
    }

    #[test]
    fn resampling_is_independent_of_callback_boundaries() {
        for (in_rate, out_rate) in [
            (44100, 48000),
            (48000, 44100),
            (96000, 48000),
            (48000, 96000),
        ] {
            for channels in [1, 2] {
                let input: Vec<f32> = (0..9600 * channels)
                    .map(|i| (i as f32 * 0.17).sin() * 0.9)
                    .collect();
                let mut expected = Vec::new();
                AudioResampler::new(in_rate, out_rate, channels).process(&input, &mut expected);
                for frames_per_callback in [1, 2, 3, 127, 256, 441, 1024] {
                    let mut resampler = AudioResampler::new(in_rate, out_rate, channels);
                    let mut actual = Vec::new();
                    for chunk in input.chunks(frames_per_callback * channels) {
                        resampler.process(chunk, &mut actual);
                    }
                    assert_eq!(actual, expected, "{in_rate}->{out_rate}, {channels}ch, {frames_per_callback} frames/callback");
                }
            }
        }
    }

    #[test]
    fn resampling_retains_exact_downsample_timeline() {
        let input: Vec<f32> = (0..9600).map(|i| i as f32).collect();
        let mut resampler = AudioResampler::new(96000, 48000, 1);
        let mut output = Vec::new();
        for chunk in input.chunks(256) {
            resampler.process(chunk, &mut output);
        }
        for (frame, sample) in output.iter().enumerate() {
            assert_eq!(*sample, (frame * 2) as f32);
        }
    }

    #[test]
    fn resampler_reset_discards_previous_stream() {
        let mut resampler = AudioResampler::new(44100, 48000, 2);
        resampler.process(&[0.9; 256], &mut Vec::new());
        resampler.reset();
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        resampler.process(&[0.2; 256], &mut actual);
        AudioResampler::new(44100, 48000, 2).process(&[0.2; 256], &mut expected);
        assert_eq!(actual, expected);
    }

    #[test]
    fn output_conversion_clamps_interpolation_overshoot() {
        assert_eq!(f32_to_sample::<i16>(1.25), i16::MAX);
        assert_eq!(f32_to_sample::<i16>(-1.25), i16::MIN);
    }

    #[test]
    fn resampler_identity_returns_exact_input() {
        let mut resampler = AudioResampler::new(48000, 48000, 2);
        let input = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        assert_eq!(output, input);
    }

    #[test]
    fn resampler_44100_to_48000_ratio_matches() {
        let mut resampler = AudioResampler::new(44100, 48000, 2);
        // Feed 4410 frames (100 ms of 44.1kHz audio)
        let in_frames = 4410;
        let mut input = Vec::with_capacity(in_frames * 2);
        for i in 0..in_frames {
            let t = i as f32 / 44100.0;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            input.push(sample);
            input.push(sample);
        }

        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        let out_frames = output.len() / 2;
        // Expected ~4800 frames
        assert!(
            (out_frames as i32 - 4800).abs() <= 5,
            "expected ~4800 frames, got {out_frames}"
        );
    }

    #[test]
    fn resampler_96000_to_48000_ratio_matches() {
        let mut resampler = AudioResampler::new(96000, 48000, 2);
        let in_frames = 9600;
        let mut input = Vec::with_capacity(in_frames * 2);
        for i in 0..in_frames {
            let s = (i % 100) as f32 / 100.0;
            input.push(s);
            input.push(s);
        }

        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        let out_frames = output.len() / 2;
        // Expected ~4800 frames
        assert!(
            (out_frames as i32 - 4800).abs() <= 5,
            "expected ~4800 frames, got {out_frames}"
        );
    }

    #[test]
    fn ring_buffer_push_pop_order() {
        let diag = Arc::new(AudioDiagnostics::new());
        let ring = AudioRingBuffer::new(16, 1, diag);
        assert_eq!(ring.push_samples(&[1.0, 2.0, 3.0, 4.0]), 4);
        assert_eq!(ring.available(), 4);

        let mut out = [0.0f32; 4];
        assert_eq!(ring.pop_samples(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn ring_buffer_underrun_zero_fills() {
        let diag = Arc::new(AudioDiagnostics::new());
        let ring = AudioRingBuffer::new(8, 1, diag.clone());
        assert_eq!(ring.push_samples(&[0.5, 0.6]), 2);

        let mut out = [1.0f32; 4];
        let count = ring.pop_samples(&mut out);
        assert_eq!(count, 2);
        assert_eq!(out[0], 0.5);
        assert_eq!(out[1], 0.6);
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 0.0);
    }

    #[test]
    fn ring_buffer_overrun_limits_to_capacity() {
        let diag = Arc::new(AudioDiagnostics::new());
        let ring = AudioRingBuffer::new(4, 1, diag.clone());
        assert_eq!(ring.push_samples(&[0.1, 0.2, 0.3, 0.4, 0.5]), 4);
        assert_eq!(ring.available(), 4);
    }

    #[test]
    fn ring_buffer_overrun_keeps_whole_frames() {
        let diag = Arc::new(AudioDiagnostics::new());
        let ring = AudioRingBuffer::new(5, 2, diag);
        // Capacity rounds down to 2 frames; a third frame must not be split.
        assert_eq!(ring.push_samples(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
        assert_eq!(ring.discard_to(1), 2, "discard drops whole frames only");
        let mut out = [0.0; 2];
        ring.pop_samples(&mut out);
        assert_eq!(out, [3.0, 4.0]);
    }

    #[test]
    fn adaptive_resampler_trims_output_rate() {
        let input = vec![0.25f32; 48_000 * 2];
        for (adjustment, expected) in [(0.0, 48_000.0), (0.001, 47_952.0), (-0.001, 48_048.0)] {
            let mut resampler = AudioResampler::adaptive(48_000, 48_000, 2);
            resampler.set_rate_adjustment(adjustment);
            let mut output = Vec::new();
            resampler.process(&input, &mut output);
            let frames = (output.len() / 2) as f64;
            assert!(
                (frames - expected).abs() <= 3.0,
                "adjustment {adjustment}: expected ~{expected} frames, got {frames}"
            );
            assert!(output.iter().all(|s| (s - 0.25).abs() < 1e-6));
        }
    }

    #[test]
    fn playback_waits_for_target_then_resyncs_backlog() {
        let diag = Arc::new(AudioDiagnostics::new());
        let latency = PlaybackLatency::new(48_000, 2);
        let ring = AudioRingBuffer::new(48_000 * 2 * 2, 2, diag.clone());
        let mut out = vec![1.0f32; 960];

        ring.push_samples(&vec![0.5; latency.target_samples / 2]);
        assert_eq!(ring.pull_playback(&mut out, &latency), 0);
        assert!(out.iter().all(|s| *s == 0.0), "silence while priming");
        assert_eq!(diag.underrun_count(), 0);

        ring.push_samples(&vec![0.5; latency.target_samples]);
        assert_eq!(ring.pull_playback(&mut out, &latency), 960);
        assert!(ring.is_primed());

        // A stall leaves a 300 ms backlog; playback snaps back to the target.
        ring.push_samples(&vec![0.5; 48_000 * 2 * 3 / 10]);
        ring.pull_playback(&mut out, &latency);
        assert_eq!(diag.resync_count(), 1);
        assert_eq!(ring.available(), latency.target_samples - 960);
    }

    /// Runs capture and playback on independent clocks, as a capture card and
    /// an audio interface do, and returns the worst buffer error once settled.
    fn simulate_clock_mismatch(capture_rate: f64, compensate: bool) -> (f64, usize) {
        const BLOCK: usize = 480; // 10 ms callbacks
        const MINUTES: usize = 12;
        let diag = Arc::new(AudioDiagnostics::new());
        let latency = PlaybackLatency::new(48_000, 2);
        let ring = AudioRingBuffer::new(48_000 * 2 * 2, 2, diag.clone());
        let mut resampler = AudioResampler::adaptive(48_000, 48_000, 2);
        let mut drift = DriftController::new(PLAYBACK_TARGET_SECONDS);
        let input = vec![0.1f32; BLOCK * 2];
        let mut resampled = Vec::new();
        let mut output = vec![0.0f32; BLOCK * 2];
        let (mut capture_clock, mut playback_clock) = (0.0, 0.0);
        let mut worst: f64 = 0.0;

        for ms in 0..MINUTES * 60_000 {
            capture_clock += capture_rate / 1000.0;
            while capture_clock >= BLOCK as f64 {
                capture_clock -= BLOCK as f64;
                resampled.clear();
                resampler.process(&input, &mut resampled);
                ring.push_samples(&resampled);
                if !ring.is_primed() {
                    drift.pause();
                    continue;
                }
                let buffered = latency.seconds(ring.available());
                if compensate {
                    resampler.set_rate_adjustment(drift.update(buffered, BLOCK as f64 / 48_000.0));
                }
                if ms > 4 * 60_000 {
                    worst = worst.max((buffered - PLAYBACK_TARGET_SECONDS).abs());
                }
            }
            playback_clock += 48.0;
            while playback_clock >= BLOCK as f64 {
                playback_clock -= BLOCK as f64;
                ring.pull_playback(&mut output, &latency);
            }
        }
        (worst, diag.resync_count() + diag.underrun_count())
    }

    #[test]
    fn drift_control_holds_latency_across_clock_mismatch() {
        // ±300 ppm is well beyond typical USB audio crystals.
        for ppm in [-300.0, -50.0, 50.0, 300.0] {
            let (worst, glitches) = simulate_clock_mismatch(48_000.0 * (1.0 + ppm * 1e-6), true);
            assert!(
                worst < 0.012,
                "{ppm} ppm: buffer strayed {:.1} ms from target",
                worst * 1000.0
            );
            assert_eq!(glitches, 0, "{ppm} ppm: unexpected resync or underrun");
        }
    }

    #[test]
    fn uncompensated_clock_mismatch_drifts() {
        // Guards the simulation itself: without control, 300 ppm piles up audio.
        let (worst, _) = simulate_clock_mismatch(48_000.0 * 1.0003, false);
        assert!(worst > 0.05, "expected drift, got {:.1} ms", worst * 1000.0);
    }

    #[test]
    fn volume_controller_gain() {
        let vol = VolumeController::new(0.8, false);
        assert!((vol.gain() - 0.8).abs() < 1e-4);

        vol.set_muted(true);
        assert_eq!(vol.gain(), 0.0);

        vol.set_muted(false);
        assert!((vol.gain() - 0.8).abs() < 1e-4);

        vol.set_volume(0.5);
        assert!((vol.gain() - 0.5).abs() < 1e-4);
    }
}
