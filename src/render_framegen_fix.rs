//! Content-aware correction of generated frames, and GPU timing.
//!
//! TackleCast only sees the flattened HDMI image, and NVIDIA FRUC does not
//! expose its optical flow (NvOFFRUC.h: `NvOFFRUCProcess` returns the frame
//! and `bHasFrameRepetitionOccurred`, nothing else). So instead of masking
//! regions, every generated pixel G is checked against what the two real
//! frames A (previous) and B (next) say it must be, using predictions that
//! are reliable where they apply:
//!
//! * static:  A(p) == B(p)            -> the midpoint is that pixel.
//! * motion:  A(p-a) == B(p+b), a+b=g -> the midpoint is that content
//!   (g = camera motion measured from luma profiles, see `framegen::motion`).
//! * edge:    p-a lies outside A      -> content entering the screen exists
//!   only in B; the midpoint is B(p+b) (and symmetric for content leaving).
//!   The band is exactly as wide as the motion and disappears when it stops.
//! * occlusion: A(p-a) is a static overlay pixel (HUD) while B(p+b) is
//!   moving scenery -> the scenery was hidden behind the HUD in A; the
//!   midpoint is B(p+b) (and symmetric). Repairs HUD ghosts dragged by flow.
//! * fade:    A and B have the same structure at p with a different
//!   brightness (UI fading in/out) -> the midpoint is their average.
//!
//! G is replaced only where a prediction applies AND G clearly contradicts
//! it (HUD warped or dragged, smeared edges, half-built menu). The
//! replacement is itself a midpoint image, not a copy of a real frame, so
//! corrected pixels still move at the output rate. Decisions are gated per
//! 8x8 tile (a cluster is required, with hysteresis) so isolated noisy
//! pixels are ignored and corrections do not flicker on and off.
//!
//! Where no prediction applies (independent object motion, parallax,
//! zoom) FRUC's output is kept unchanged.

use super::*;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Correction settings for one generated frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FgProtection {
    pub fix_static: bool,
    pub fix_motion: bool,
    pub fix_edges: bool,
    pub fix_fades: bool,
    /// Advanced: paint detected static HUD out of FRUC's input.
    pub hud_input_removal: bool,
    /// HUD guard: small static on-screen elements (crosshair, icons, text)
    /// that stay put while the camera moves are painted out of the
    /// interpolator's input (filled from the previous frame along the
    /// camera motion) and drawn back from the real frame afterwards, so
    /// they neither vanish nor leave dragged copies in generated frames.
    pub hud_guard: bool,
    /// What to show: [`VIEW_FINAL`], [`VIEW_RAW`] or [`VIEW_OVERLAY`].
    pub view: u32,
    /// Optional manual zones (bit 0 TL, 1 TR, 2 BL, 3 BR), shown from the
    /// real frame. Troubleshooting only; off by default.
    pub zones: u32,
    pub zone_width: f32,
    pub zone_height: f32,
    /// Camera motion of the next real frame relative to the previous one
    /// (px), set per generated frame by the controller.
    pub motion: Option<[f32; 2]>,
}

impl FgProtection {
    pub fn corrections(self) -> bool {
        self.fix_static || self.fix_motion || self.fix_edges || self.fix_fades || self.hud_guard
    }

    /// Whether the capture side paints the HUD out of the interpolator input.
    pub fn removes_hud(self) -> bool {
        self.hud_input_removal || self.hud_guard
    }

    /// Whether the correction pass has to run at all.
    pub fn any(self) -> bool {
        self.corrections() || self.view != VIEW_FINAL || self.zones != 0
    }
}

pub const VIEW_FINAL: u32 = 0;
pub const VIEW_RAW: u32 = 1;
pub const VIEW_OVERLAY: u32 = 2;

pub const ZONE_TOP_LEFT: u32 = 1;
pub const ZONE_TOP_RIGHT: u32 = 2;
pub const ZONE_BOTTOM_LEFT: u32 = 4;
pub const ZONE_BOTTOM_RIGHT: u32 = 8;

const FLAG_STATIC: u32 = 1;
const FLAG_MOTION: u32 = 2;
const FLAG_EDGE: u32 = 4;
const FLAG_FADE: u32 = 8;
const FLAG_HUD: u32 = 16;

/// Tile size of the correction gate (must match the shader workgroup).
pub const FIX_TILE: u32 = 8;

/// Statistics of the last measured generated frame (read back async).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct FixStats {
    /// Percent of the frame corrected (weight > 0.5), total and by type.
    pub corrected_pct: f32,
    pub static_pct: f32,
    pub motion_pct: f32,
    pub edge_pct: f32,
    pub fade_pct: f32,
    /// Percent of the frame covered by manual zones.
    pub zone_pct: f32,
    /// Tiles whose correction gate was open.
    pub active_tiles: u32,
}

impl FixStats {
    fn from_counters(c: &[u32; 8], pixels: f32) -> Self {
        let pct = |v: u32| v as f32 * 100.0 / pixels.max(1.0);
        Self {
            static_pct: pct(c[0]),
            motion_pct: pct(c[1]),
            edge_pct: pct(c[2]),
            fade_pct: pct(c[3]),
            zone_pct: pct(c[4]),
            active_tiles: c[5],
            corrected_pct: pct(c[6]),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct FixParams {
    width: u32,
    height: u32,
    tiles_x: u32,
    tiles_y: u32,
    flags: u32,
    view: u32,
    zones: u32,
    motion_valid: u32,
    ax: i32,
    ay: i32,
    bx: i32,
    by: i32,
    zone_w: f32,
    zone_h: f32,
    zone_feather: f32,
    tau_lo: f32,
    tau_hi: f32,
    tau_static: f32,
    tau_motion: f32,
    tile_on: f32,
    tile_keep: f32,
    ncc_min: f32,
    flat_std: f32,
    /// Word offset of the next real frame's HUD mask in `hud_bits`.
    hud_offset: u32,
    /// Word offset of the previous real frame's HUD mask.
    hud_prev_offset: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

/// Split the camera motion `g` into the two half-way offsets: the content
/// at p in the midpoint frame is at p - a in A and p + b in B (a + b = g).
pub fn half_offsets(g: [f32; 2]) -> ([i32; 2], [i32; 2]) {
    let gx = g[0].round() as i32;
    let gy = g[1].round() as i32;
    let ax = (gx as f32 / 2.0).round() as i32;
    let ay = (gy as f32 / 2.0).round() as i32;
    ([ax, ay], [gx - ax, gy - ay])
}

/// Non-blocking GPU readback ring: a buffer is mapped after the submission
/// that filled it and collected on a later frame; when all are busy the
/// measurement for that frame is skipped (never waits).
pub(super) struct ReadbackRing {
    buffers: Vec<(wgpu::Buffer, Arc<AtomicU8>)>,
    /// Buffer filled by the current encoder, to be mapped after submit.
    to_map: Option<usize>,
}

const RB_FREE: u8 = 0;
const RB_IN_USE: u8 = 1;
const RB_READY: u8 = 2;
const RB_FAILED: u8 = 3;

impl ReadbackRing {
    pub(super) fn new(device: &wgpu::Device, label: &str, size: u64, count: usize) -> Self {
        let buffers = (0..count)
            .map(|_| {
                (
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some(label),
                        size,
                        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    }),
                    Arc::new(AtomicU8::new(RB_FREE)),
                )
            })
            .collect();
        Self {
            buffers,
            to_map: None,
        }
    }

    /// Collect every finished readback (oldest first is not guaranteed).
    pub(super) fn collect(&mut self, mut f: impl FnMut(&[u8])) {
        for (buffer, state) in &self.buffers {
            match state.load(Ordering::Acquire) {
                RB_READY => {
                    {
                        let data = buffer.slice(..).get_mapped_range();
                        f(&data);
                    }
                    buffer.unmap();
                    state.store(RB_FREE, Ordering::Release);
                }
                RB_FAILED => state.store(RB_FREE, Ordering::Release),
                _ => {}
            }
        }
    }

    /// Reserve a free buffer for this frame's copy.
    pub(super) fn reserve(&mut self) -> Option<&wgpu::Buffer> {
        if self.to_map.is_some() {
            return None;
        }
        let index = self
            .buffers
            .iter()
            .position(|(_, s)| s.load(Ordering::Acquire) == RB_FREE)?;
        self.buffers[index].1.store(RB_IN_USE, Ordering::Release);
        self.to_map = Some(index);
        Some(&self.buffers[index].0)
    }

    /// Call after the encoder that used [`reserve`] was submitted.
    pub(super) fn after_submit(&mut self) {
        if let Some(index) = self.to_map.take() {
            let (buffer, state) = &self.buffers[index];
            let state = state.clone();
            buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                state.store(
                    if r.is_ok() { RB_READY } else { RB_FAILED },
                    Ordering::Release,
                );
            });
        }
    }
}

/// GPU time of a group of compute passes, via timestamp queries
/// (only when the adapter supports `TIMESTAMP_QUERY`).
pub(super) struct GpuTimer {
    query_set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    ring: ReadbackRing,
    period_ns: f32,
    armed: bool,
    pub(super) samples_ms: std::collections::VecDeque<f32>,
}

impl GpuTimer {
    pub(super) fn new(device: &wgpu::Device, queue: &wgpu::Queue, label: &str) -> Option<Self> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        Some(Self {
            query_set: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some(label),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }),
            resolve: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            ring: ReadbackRing::new(device, label, 16, 4),
            period_ns: queue.get_timestamp_period(),
            armed: false,
            samples_ms: std::collections::VecDeque::with_capacity(256),
        })
    }

    /// Start a measurement; returns false if no readback slot is free.
    pub(super) fn arm(&mut self) -> bool {
        self.collect();
        self.armed = self.ring.reserve().is_some();
        self.armed
    }

    /// Take in finished measurements (never waits).
    pub(super) fn collect(&mut self) {
        let period = self.period_ns;
        let samples = &mut self.samples_ms;
        self.ring.collect(|data| {
            let t0 = u64::from_le_bytes(data[0..8].try_into().expect("8 bytes"));
            let t1 = u64::from_le_bytes(data[8..16].try_into().expect("8 bytes"));
            if t1 > t0 {
                if samples.len() == 256 {
                    samples.pop_front();
                }
                samples.push_back((t1 - t0) as f32 * period / 1.0e6);
            }
        });
    }

    /// Timestamp writes for a pass: `first` writes the start, `last` the end.
    pub(super) fn writes(
        &self,
        first: bool,
        last: bool,
    ) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
        if !self.armed || !(first || last) {
            return None;
        }
        Some(wgpu::ComputePassTimestampWrites {
            query_set: &self.query_set,
            beginning_of_pass_write_index: first.then_some(0),
            end_of_pass_write_index: last.then_some(1),
        })
    }

    /// Resolve into the reserved readback buffer (end of the encoder).
    pub(super) fn finish(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if !self.armed {
            return;
        }
        encoder.resolve_query_set(&self.query_set, 0..2, &self.resolve, 0);
        if let Some(index) = self.ring.to_map {
            encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.ring.buffers[index].0, 0, 16);
        }
        self.armed = false;
    }

    pub(super) fn after_submit(&mut self) {
        self.ring.after_submit();
    }
}

pub(super) struct FixResources {
    layout: wgpu::BindGroupLayout,
    score: wgpu::ComputePipeline,
    compose: wgpu::ComputePipeline,
    uniforms: wgpu::Buffer,
    pixels: wgpu::Buffer,
    tile_state: wgpu::Buffer,
    counters: wgpu::Buffer,
    stats_ring: ReadbackRing,
    /// Raw FRUC output per output slot (copied from the shared buffer).
    raw: Vec<(wgpu::Texture, wgpu::TextureView)>,
    output_storage_views: Vec<wgpu::TextureView>,
    bind_groups: HashMap<(usize, usize, usize), wgpu::BindGroup>,
    tiles_x: u32,
    tiles_y: u32,
    pub(super) timer: Option<GpuTimer>,
    pub(super) last_stats: Option<FixStats>,
    pub(super) stats_history: std::collections::VecDeque<FixStats>,
}

impl FgGpu {
    /// Create the correction passes on first use. Returns false (and stays
    /// false) if the device rejects them; frames are then shown raw.
    pub(super) fn ensure_fix(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        if self.fix.is_some() {
            return true;
        }
        if self.fix_failed {
            return false;
        }
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let storage_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tacklecast-fg-fix-layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: FG_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                storage_entry(4),
                storage_entry(5),
                storage_entry(6),
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tacklecast-fg-fix-shader"),
            source: wgpu::ShaderSource::Wgsl(FIX_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tacklecast-fg-fix-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("tacklecast-fg-fix-pipeline"),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let (width, height) = (self.width, self.height);
        let tiles_x = width.div_ceil(FIX_TILE);
        let tiles_y = height.div_ceil(FIX_TILE);
        let storage = |label: &str, size: u64, extra: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | extra,
                mapped_at_creation: false,
            })
        };
        let raw = (0..self.outputs.len())
            .map(|_| {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("tacklecast-fg-raw-output"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: FG_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_DST
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                (texture, view)
            })
            .collect();
        let output_storage_views = self
            .outputs
            .iter()
            .map(|o| o.texture.create_view(&Default::default()))
            .collect();
        self.fix = Some(FixResources {
            score: pipeline("score"),
            compose: pipeline("compose"),
            layout,
            uniforms: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tacklecast-fg-fix-uniforms"),
                size: std::mem::size_of::<FixParams>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            pixels: storage(
                "tacklecast-fg-fix-pixels",
                width as u64 * height as u64 * 4,
                wgpu::BufferUsages::empty(),
            ),
            tile_state: storage(
                "tacklecast-fg-fix-tiles",
                tiles_x as u64 * tiles_y as u64 * 4,
                wgpu::BufferUsages::empty(),
            ),
            counters: storage(
                "tacklecast-fg-fix-counters",
                32,
                wgpu::BufferUsages::COPY_SRC,
            ),
            stats_ring: ReadbackRing::new(device, "tacklecast-fg-fix-stats", 32, 4),
            raw,
            output_storage_views,
            bind_groups: HashMap::new(),
            tiles_x,
            tiles_y,
            timer: GpuTimer::new(device, queue, "tacklecast-fg-fix-timer"),
            last_stats: None,
            stats_history: std::collections::VecDeque::with_capacity(512),
        });
        if let Some(error) = pollster::block_on(device.pop_error_scope()) {
            tracing::warn!(
                "frame generation artifact correction unavailable on this GPU \
                 (generated frames shown raw): {error}"
            );
            self.fix = None;
            self.fix_failed = true;
            return false;
        }
        tracing::info!(
            "frame generation artifact correction ready (GPU timing {})",
            if self.fix.as_ref().is_some_and(|f| f.timer.is_some()) {
                "available"
            } else {
                "unavailable on this adapter"
            }
        );
        true
    }

    /// Publish output `slot` (raw FRUC result between real frames `prev`
    /// and `next`) through the correction passes into the output texture.
    pub fn encode_publish_corrected(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        (slot, prev, next): (usize, usize, usize),
        protection: FgProtection,
    ) {
        if !self.ensure_fix(device, queue) {
            self.encode_publish_output(encoder, slot);
            return;
        }
        // Collect finished statistics from earlier frames (non-blocking).
        self.poll_fix_stats();
        let (width, height, pitch) = (self.width, self.height, self.pitch);
        let fix = self.fix.as_mut().expect("ensured");

        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &self.outputs[slot].buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pitch),
                    rows_per_image: Some(height),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture: &fix.raw[slot].0,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let flags = if protection.view == VIEW_RAW && !protection.corrections() {
            0
        } else {
            (u32::from(protection.fix_static) * FLAG_STATIC)
                | (u32::from(protection.fix_motion) * FLAG_MOTION)
                | (u32::from(protection.fix_edges) * FLAG_EDGE)
                | (u32::from(protection.fix_fades) * FLAG_FADE)
                | (u32::from(protection.hud_guard) * FLAG_HUD)
        };
        let hud_offset = next * self.hud_words_per_slot;
        let hud_prev_offset = prev * self.hud_words_per_slot;
        // Motion is only usable while it stays well inside the frame.
        let motion = protection
            .motion
            .filter(|g| g[0].abs() < width as f32 * 0.3 && g[1].abs() < height as f32 * 0.3);
        let ([ax, ay], [bx, by]) = half_offsets(motion.unwrap_or([0.0, 0.0]));
        let params = FixParams {
            width,
            height,
            tiles_x: fix.tiles_x,
            tiles_y: fix.tiles_y,
            flags,
            view: protection.view,
            zones: protection.zones,
            motion_valid: u32::from(motion.is_some()),
            ax,
            ay,
            bx,
            by,
            zone_w: (protection.zone_width.clamp(0.0, 0.5) * width as f32).round(),
            zone_h: (protection.zone_height.clamp(0.0, 0.5) * height as f32).round(),
            zone_feather: (width as f32 * 0.01).max(4.0),
            tau_lo: 8.0,
            tau_hi: 20.0,
            tau_static: 3.0,
            tau_motion: 5.0,
            tile_on: 0.10,
            tile_keep: 0.03,
            ncc_min: 0.92,
            flat_std: 0.75,
            hud_offset: hud_offset as u32,
            hud_prev_offset: hud_prev_offset as u32,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
        };
        queue.write_buffer(&fix.uniforms, 0, bytemuck::bytes_of(&params));
        encoder.clear_buffer(&fix.counters, 0, None);
        let sources = &self.sources;
        let layout = &fix.layout;
        let (raw, outputs) = (&fix.raw, &fix.output_storage_views);
        let (pixel_buf, tiles, counters, uniforms) =
            (&fix.pixels, &fix.tile_state, &fix.counters, &fix.uniforms);
        let hud_bits = &self.hud_bits;
        let bind_group = fix
            .bind_groups
            .entry((slot, prev, next))
            .or_insert_with(|| {
                let entries = [
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&sources[prev].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&sources[next].view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&raw[slot].1),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&outputs[slot]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: pixel_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: tiles.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 6,
                        resource: counters.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 7,
                        resource: uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 8,
                        resource: hud_bits.as_entire_binding(),
                    },
                ];
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("tacklecast-fg-fix-bind-group"),
                    layout,
                    entries: &entries,
                })
            })
            .clone();
        if let Some(timer) = fix.timer.as_mut() {
            timer.arm();
        }
        let groups = (width.div_ceil(8), height.div_ceil(8));
        for (index, (pipeline, label)) in [
            (&fix.score, "tacklecast-fg-fix-score"),
            (&fix.compose, "tacklecast-fg-fix-compose"),
        ]
        .into_iter()
        .enumerate()
        {
            let timestamp_writes = fix
                .timer
                .as_ref()
                .and_then(|t| t.writes(index == 0, index == 1));
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some(label),
                timestamp_writes,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(groups.0, groups.1, 1);
        }
        if let Some(timer) = fix.timer.as_mut() {
            timer.finish(encoder);
        }
        if let Some(buffer) = fix.stats_ring.reserve() {
            encoder.copy_buffer_to_buffer(&fix.counters, 0, buffer, 0, 32);
        }
    }

    /// Take in correction statistics whose readback has completed (never
    /// waits; replay and tests call it after `device.poll(Wait)`).
    pub fn poll_fix_stats(&mut self) {
        let pixels = (self.width * self.height) as f32;
        let Some(fix) = self.fix.as_mut() else {
            return;
        };
        let (last, history) = (&mut fix.last_stats, &mut fix.stats_history);
        fix.stats_ring.collect(|data| {
            let mut c = [0u32; 8];
            for (i, v) in c.iter_mut().enumerate() {
                *v = u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
            }
            let stats = FixStats::from_counters(&c, pixels);
            *last = Some(stats);
            if history.len() == 512 {
                history.pop_front();
            }
            history.push_back(stats);
        });
        if let Some(timer) = fix.timer.as_mut() {
            timer.collect();
        }
    }

    /// Map readbacks used by the encoder just submitted.
    pub fn after_publish_submit(&mut self) {
        if let Some(fix) = self.fix.as_mut() {
            fix.stats_ring.after_submit();
            if let Some(timer) = fix.timer.as_mut() {
                timer.after_submit();
            }
        }
    }

    /// Correction statistics and GPU times (ms) gathered so far.
    pub fn fix_report(&self) -> FixReport {
        let fix = self.fix.as_ref();
        let summary = |s: Option<&std::collections::VecDeque<f32>>| {
            s.filter(|v| !v.is_empty())
                .map(|v| crate::framegen::stats::summarize(v.iter().copied()))
        };
        FixReport {
            last: fix.and_then(|f| f.last_stats),
            mean_corrected_pct: fix
                .filter(|f| !f.stats_history.is_empty())
                .map(|f| {
                    f.stats_history.iter().map(|s| s.corrected_pct).sum::<f32>()
                        / f.stats_history.len() as f32
                })
                .unwrap_or(0.0),
            max_corrected_pct: fix
                .map(|f| {
                    f.stats_history
                        .iter()
                        .map(|s| s.corrected_pct)
                        .fold(0.0, f32::max)
                })
                .unwrap_or(0.0),
            fix_gpu: summary(fix.and_then(|f| f.timer.as_ref()).map(|t| &t.samples_ms)),
            prep_gpu: summary(self.prep_timer.as_ref().map(|t| &t.samples_ms)),
            timing_available: self
                .fix
                .as_ref()
                .map(|f| f.timer.is_some())
                .unwrap_or(self.prep_timer.is_some()),
            fix_failed: self.fix_failed,
        }
    }
}

/// Correction statistics for diagnostics.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FixReport {
    pub last: Option<FixStats>,
    /// Mean / max corrected area over the last ~512 generated frames.
    pub mean_corrected_pct: f32,
    pub max_corrected_pct: f32,
    /// GPU time of the correction passes (publish) and of the capture-side
    /// passes (profiles, optional HUD removal).
    pub fix_gpu: Option<crate::framegen::stats::DurationSummary>,
    pub prep_gpu: Option<crate::framegen::stats::DurationSummary>,
    pub timing_available: bool,
    pub fix_failed: bool,
}

/// See the module documentation.
pub(super) const FIX_SHADER: &str = r#"
struct Params {
    width: u32, height: u32, tiles_x: u32, tiles_y: u32,
    flags: u32, view: u32, zones: u32, motion_valid: u32,
    ax: i32, ay: i32, bx: i32, by: i32,
    zone_w: f32, zone_h: f32, zone_feather: f32, tau_lo: f32,
    tau_hi: f32, tau_static: f32, tau_motion: f32, tile_on: f32,
    tile_keep: f32, ncc_min: f32, flat_std: f32, hud_offset: u32,
    hud_prev_offset: u32, _pad0: u32, _pad1: u32, _pad2: u32,
};

const FLAG_STATIC: u32 = 1u;
const FLAG_MOTION: u32 = 2u;
const FLAG_EDGE: u32 = 4u;
const FLAG_FADE: u32 = 8u;
const FLAG_HUD: u32 = 16u;

const CAT_NONE: u32 = 0u;
const CAT_STATIC: u32 = 1u;
const CAT_MOTION: u32 = 2u;
const CAT_EDGE_B: u32 = 3u;
const CAT_EDGE_A: u32 = 4u;
const CAT_FADE: u32 = 5u;
const CAT_OCCL_B: u32 = 6u;
const CAT_OCCL_A: u32 = 7u;

@group(0) @binding(0) var a_tex: texture_2d<f32>;
@group(0) @binding(1) var b_tex: texture_2d<f32>;
@group(0) @binding(2) var g_tex: texture_2d<f32>;
@group(0) @binding(3) var out_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(4) var<storage, read_write> pix: array<u32>;
@group(0) @binding(5) var<storage, read_write> tile_state: array<u32>;
@group(0) @binding(6) var<storage, read_write> counters: array<atomic<u32>, 8>;
@group(0) @binding(7) var<uniform> p: Params;
@group(0) @binding(8) var<storage, read> hud_bits: array<u32>;

// HUD-guard mask of a real frame (written by the capture-side fill).
fn hud_bit(offset: u32, q: vec2<i32>) -> f32 {
    if q.x < 0 || q.y < 0 || q.x >= i32(p.width) || q.y >= i32(p.height) {
        return 0.0;
    }
    let i = u32(q.y) * p.width + u32(q.x);
    return f32((hud_bits[offset + i / 32u] >> (i % 32u)) & 1u);
}

fn hud_at(q: vec2<i32>) -> f32 {
    return hud_bit(p.hud_offset, q);
}

fn clampi(q: vec2<i32>) -> vec2<i32> {
    return clamp(q, vec2<i32>(0, 0), vec2<i32>(i32(p.width) - 1, i32(p.height) - 1));
}

fn inside(q: vec2<i32>) -> bool {
    return q.x >= 0 && q.y >= 0 && q.x < i32(p.width) && q.y < i32(p.height);
}

fn lum(t: texture_2d<f32>, q: vec2<i32>) -> f32 {
    return dot(textureLoad(t, clampi(q), 0).rgb, vec3<f32>(0.299, 0.587, 0.114)) * 255.0;
}

fn rgb(t: texture_2d<f32>, q: vec2<i32>) -> vec3<f32> {
    return textureLoad(t, clampi(q), 0).rgb;
}

// Cross-shaped patch (centre + 4 neighbours at 2 px): robust against single
// noisy pixels, cheap enough for every pixel at 1440p.
fn cross_offset(i: i32) -> vec2<i32> {
    var cross = array<vec2<i32>, 5>(
        vec2<i32>(0, 0), vec2<i32>(2, 0), vec2<i32>(-2, 0), vec2<i32>(0, 2), vec2<i32>(0, -2)
    );
    return cross[i];
}

// Patch samples outside the image are skipped (clamping one side but not
// the other would invent differences along the screen border).
fn diff2(t1: texture_2d<f32>, p1: vec2<i32>, t2: texture_2d<f32>, p2: vec2<i32>) -> f32 {
    var s = 0.0;
    var n = 0.0;
    for (var i = 0; i < 5; i = i + 1) {
        let o = cross_offset(i);
        if inside(p1 + o) && inside(p2 + o) {
            s = s + abs(lum(t1, p1 + o) - lum(t2, p2 + o));
            n = n + 1.0;
        }
    }
    return s / max(n, 1.0);
}

fn center_static(q: vec2<i32>) -> bool {
    return inside(q) && abs(lum(a_tex, q) - lum(b_tex, q)) < p.tau_static;
}

// Consecutive generated frames for which pixel q was unchanged (kept in
// bits 16..23 of `pix`, from the previous score pass).
fn streak_at(q: vec2<i32>) -> u32 {
    if !inside(q) {
        return 0u;
    }
    return (pix[u32(q.y) * p.width + u32(q.x)] >> 16u) & 255u;
}

// Unchanged between A and B at q, robustly: a HUD pixel stays unchanged
// frame after frame, a chance match in moving texture does not. So either
// q has been unchanged for 3+ frames (and a neighbour now), or 3 of its 4
// patch neighbours are unchanged now (a HUD that just appeared; its edge
// and stroke pixels border on scenery, so all 4 cannot be required).
fn is_static(q: vec2<i32>) -> bool {
    if !center_static(q) {
        return false;
    }
    var n = 0;
    for (var i = 1; i < 5; i = i + 1) {
        if center_static(q + cross_offset(i)) {
            n = n + 1;
        }
    }
    return (streak_at(q) >= 3u && n >= 1) || n >= 3;
}

// |G - B| over the patch samples that are unchanged between A and B (the
// static prediction says nothing about moving neighbours).
fn diff_static(q: vec2<i32>) -> f32 {
    var s = 0.0;
    var n = 0.0;
    for (var i = 0; i < 5; i = i + 1) {
        let o = q + cross_offset(i);
        if inside(o) && abs(lum(a_tex, o) - lum(b_tex, o)) < p.tau_static {
            s = s + abs(lum(g_tex, o) - lum(b_tex, o));
            n = n + 1.0;
        }
    }
    return s / max(n, 1.0);
}

// Error of a generated pixel against a prediction: the smaller of the
// centre error and the patch mean. A lone noisy pixel (centre only) and a
// patch that merely touches other content (neighbours only) both stay low;
// a real artifact is wrong at the centre and around it.
fn err_vs(t: texture_2d<f32>, pt: vec2<i32>, q: vec2<i32>) -> f32 {
    return min(abs(lum(g_tex, q) - lum(t, pt)), diff2(g_tex, q, t, pt));
}

fn err_mid(q: vec2<i32>, pa: vec2<i32>, pb: vec2<i32>) -> f32 {
    let centre = abs(lum(g_tex, q) - 0.5 * (lum(a_tex, pa) + lum(b_tex, pb)));
    return min(centre, diff_mid(q, pa, pb));
}

// |G(p) - midpoint of A(pa) and B(pb)| over the cross patch.
fn diff_mid(q: vec2<i32>, pa: vec2<i32>, pb: vec2<i32>) -> f32 {
    var s = 0.0;
    var n = 0.0;
    for (var i = 0; i < 5; i = i + 1) {
        let o = cross_offset(i);
        if inside(q + o) && inside(pa + o) && inside(pb + o) {
            s = s + abs(lum(g_tex, q + o) - 0.5 * (lum(a_tex, pa + o) + lum(b_tex, pb + o)));
            n = n + 1.0;
        }
    }
    return s / max(n, 1.0);
}

// Same structure at the same place, different brightness: a fade.
fn in_place_change(q: vec2<i32>) -> bool {
    var sa = 0.0; var sb = 0.0; var saa = 0.0; var sbb = 0.0; var sab = 0.0;
    for (var dy = -2; dy <= 2; dy = dy + 1) {
        for (var dx = -2; dx <= 2; dx = dx + 1) {
            let la = lum(a_tex, q + vec2<i32>(dx, dy));
            let lb = lum(b_tex, q + vec2<i32>(dx, dy));
            sa = sa + la; sb = sb + lb;
            saa = saa + la * la; sbb = sbb + lb * lb; sab = sab + la * lb;
        }
    }
    let n = 25.0;
    let ma = sa / n;
    let mb = sb / n;
    let sda = sqrt(max(saa / n - ma * ma, 0.0));
    let sdb = sqrt(max(sbb / n - mb * mb, 0.0));
    if sda < p.flat_std && sdb < p.flat_std {
        return abs(ma - mb) > p.tau_static;
    }
    if sda < p.flat_std || sdb < p.flat_std {
        return false;
    }
    let cov = sab / n - ma * mb;
    let ncc = cov / (sda * sdb);
    if ncc <= p.ncc_min {
        return false;
    }
    // A fade changes brightness/contrast in place: B = alpha*A + beta must
    // also hold at the centre. (A window straddling a static HUD correlates
    // well while its centre is moving scenery: that is not a fade.)
    let alpha = cov / (sda * sda);
    let beta = mb - alpha * ma;
    return abs(lum(b_tex, q) - (alpha * lum(a_tex, q) + beta)) < 2.0 * p.tau_static;
}

var<workgroup> wg_bad: atomic<u32>;

@compute @workgroup_size(8, 8, 1)
fn score(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    if li == 0u {
        atomicStore(&wg_bad, 0u);
    }
    workgroupBarrier();
    if gid.x < p.width && gid.y < p.height {
        let q = vec2<i32>(gid.xy);
        var cat = CAT_NONE;
        var e = 0.0;
        let static_here = is_static(q);
        if (p.flags & FLAG_STATIC) != 0u && static_here {
            cat = CAT_STATIC;
            e = min(abs(lum(g_tex, q) - lum(b_tex, q)), diff_static(q));
        }
        if cat == CAT_NONE && p.motion_valid != 0u {
            let pa = q - vec2<i32>(p.ax, p.ay);
            let pb = q + vec2<i32>(p.bx, p.by);
            let in_a = inside(pa);
            let in_b = inside(pb);
            let moving = abs(p.ax) + abs(p.ay) + abs(p.bx) + abs(p.by) >= 2;
            if (p.flags & FLAG_MOTION) != 0u && moving && in_a && in_b
                && diff2(a_tex, pa, b_tex, pb) < p.tau_motion {
                cat = CAT_MOTION;
                e = err_mid(q, pa, pb);
            } else if (p.flags & FLAG_MOTION) != 0u && moving && in_a && in_b
                && !center_static(q) {
                // (A pixel unchanged between A and B is never replaced by
                // moving scenery: it is overlay or still content.)
                // Occlusion by a static overlay (HUD, prompt, panel): the
                // scenery at the midpoint is hidden behind the overlay in
                // one real frame and visible in the other. Rebuild it from
                // the frame where it is visible.
                let occluded_in_a = is_static(pa);
                let occluded_in_b = is_static(pb);
                if occluded_in_a && !occluded_in_b {
                    cat = CAT_OCCL_B;
                    e = err_vs(b_tex, pb, q);
                } else if occluded_in_b && !occluded_in_a {
                    cat = CAT_OCCL_A;
                    e = err_vs(a_tex, pa, q);
                }
            }
            if cat == CAT_NONE && (p.flags & FLAG_EDGE) != 0u && moving && !in_a && in_b {
                cat = CAT_EDGE_B;
                e = err_vs(b_tex, pb, q);
            } else if cat == CAT_NONE && (p.flags & FLAG_EDGE) != 0u && moving && in_a && !in_b {
                cat = CAT_EDGE_A;
                e = err_vs(a_tex, pa, q);
            }
        }
        if cat == CAT_NONE && (p.flags & FLAG_FADE) != 0u && in_place_change(q) {
            cat = CAT_FADE;
            e = err_mid(q, q, q);
        }
        var s = 0.0;
        if cat != CAT_NONE {
            s = smoothstep(p.tau_lo, p.tau_hi, e);
        }
        if s > 0.5 {
            atomicAdd(&wg_bad, 1u);
        }
        // The streak bits are left as they are: other invocations may read
        // them (occlusion test at p-a / p+b). The compose pass updates them.
        let idx = gid.y * p.width + gid.x;
        pix[idx] = u32(round(s * 255.0)) | (cat << 8u) | (pix[idx] & 0xFF0000u);
    }
    workgroupBarrier();
    if li == 0u {
        let t = wid.y * p.tiles_x + wid.x;
        let frac = f32(atomicLoad(&wg_bad)) / 64.0;
        let was_on = tile_state[t] != 0u;
        let on = frac >= p.tile_on || (was_on && frac >= p.tile_keep);
        tile_state[t] = select(0u, 1u, on);
        if on {
            atomicAdd(&counters[5], 1u);
        }
    }
}

fn tile_on_at(tx: i32, ty: i32) -> f32 {
    if tx < 0 || ty < 0 || tx >= i32(p.tiles_x) || ty >= i32(p.tiles_y) {
        return 0.0;
    }
    return f32(tile_state[u32(ty) * p.tiles_x + u32(tx)]);
}

fn zone_weight(q: vec2<u32>) -> f32 {
    if p.zones == 0u || p.zone_w <= 0.0 || p.zone_h <= 0.0 {
        return 0.0;
    }
    let x = f32(q.x) + 0.5;
    let y = f32(q.y) + 0.5;
    let fw = f32(p.width);
    let fh = f32(p.height);
    let l = 1.0 - smoothstep(p.zone_w - p.zone_feather, p.zone_w, x);
    let r = smoothstep(fw - p.zone_w, fw - p.zone_w + p.zone_feather, x);
    let t = 1.0 - smoothstep(p.zone_h - p.zone_feather, p.zone_h, y);
    let b = smoothstep(fh - p.zone_h, fh - p.zone_h + p.zone_feather, y);
    var w = 0.0;
    if (p.zones & 1u) != 0u { w = max(w, min(l, t)); }
    if (p.zones & 2u) != 0u { w = max(w, min(r, t)); }
    if (p.zones & 4u) != 0u { w = max(w, min(l, b)); }
    if (p.zones & 8u) != 0u { w = max(w, min(r, b)); }
    return w;
}

var<workgroup> wg_cat: array<atomic<u32>, 8>;

@compute @workgroup_size(8, 8, 1)
fn compose(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    if li < 8u {
        atomicStore(&wg_cat[li], 0u);
    }
    workgroupBarrier();
    if gid.x < p.width && gid.y < p.height {
        let q = vec2<i32>(gid.xy);
        let idx = gid.y * p.width + gid.x;
        let packed = pix[idx];
        // Score has finished for every pixel: update this pixel's streak.
        let streak = select(0u, min(((packed >> 16u) & 255u) + 1u, 255u), center_static(q));
        pix[idx] = (packed & 0xFFFFu) | (streak << 16u);
        let s = f32(packed & 255u) / 255.0;
        let cat = (packed >> 8u) & 7u;
        // Open where this tile or a neighbour holds a cluster of errors.
        let tx = i32(wid.x);
        let ty = i32(wid.y);
        var gate = 0.0;
        for (var dy = -1; dy <= 1; dy = dy + 1) {
            for (var dx = -1; dx <= 1; dx = dx + 1) {
                gate = max(gate, tile_on_at(tx + dx, ty + dy));
            }
        }
        var w = 0.0;
        if cat != CAT_NONE {
            w = s * gate;
        }
        let g = rgb(g_tex, q);
        let pa = q - vec2<i32>(p.ax, p.ay);
        let pb = q + vec2<i32>(p.bx, p.by);
        var c = g;
        switch cat {
            case 1u: { c = rgb(b_tex, q); }
            case 2u: { c = 0.5 * (rgb(a_tex, pa) + rgb(b_tex, pb)); }
            case 3u: { c = rgb(b_tex, pb); }
            case 4u: { c = rgb(a_tex, pa); }
            case 5u: { c = 0.5 * (rgb(a_tex, q) + rgb(b_tex, q)); }
            case 6u: { c = rgb(b_tex, pb); }
            case 7u: { c = rgb(a_tex, pa); }
            default: {}
        }
        var color = mix(g, c, w);
        // HUD guard: the interpolator never saw the static HUD (painted out
        // of its input), so draw it from the real frame, feathered by 1 px.
        var wh = 0.0;
        if (p.flags & FLAG_HUD) != 0u {
            var wa = 0.0;
            var wb = 0.0;
            for (var dy = -1; dy <= 1; dy = dy + 1) {
                for (var dx = -1; dx <= 1; dx = dx + 1) {
                    wb = wb + hud_at(q + vec2<i32>(dx, dy));
                    wa = wa + hud_bit(p.hud_prev_offset, q + vec2<i32>(dx, dy));
                }
            }
            wb = wb / 9.0;
            wa = wa / 9.0;
            // Guarded in the previous frame (the guard can drop out for a
            // frame): the interpolator saw the element in one input only and
            // made a half-faded copy. Where both real frames agree the
            // element is still there, so draw it from them.
            if wa > 0.0 {
                let ra = rgb(a_tex, q);
                let rb = rgb(b_tex, q);
                let d = max(abs(ra.r - rb.r), max(abs(ra.g - rb.g), abs(ra.b - rb.b)));
                let same = 1.0 - smoothstep(4.0 / 255.0, 16.0 / 255.0, d);
                color = mix(color, 0.5 * (ra + rb), wa * same);
                wh = wa * same;
            }
            if wb > 0.0 {
                color = mix(color, rgb(b_tex, q), wb);
                wh = max(wh, wb);
            }
        }
        let wz = zone_weight(gid.xy);
        if wz > 0.0 {
            color = mix(color, rgb(b_tex, q), wz);
        }
        if p.view == 1u {
            color = g;
        } else if p.view == 2u {
            var tint = vec3<f32>(0.0);
            switch cat {
                case 1u: { tint = vec3<f32>(1.0, 0.15, 0.15); }
                case 2u, 6u, 7u: { tint = vec3<f32>(0.15, 1.0, 0.15); }
                case 3u, 4u: { tint = vec3<f32>(0.2, 0.4, 1.0); }
                case 5u: { tint = vec3<f32>(1.0, 0.9, 0.1); }
                default: {}
            }
            color = mix(color, tint, 0.6 * w);
            color = mix(color, vec3<f32>(1.0, 0.1, 1.0), 0.35 * wz);
            color = mix(color, vec3<f32>(0.1, 0.9, 1.0), 0.6 * wh);
        }
        textureStore(out_tex, q, vec4<f32>(color, 1.0));
        if w > 0.5 {
            // Counters: 0 static, 1 motion (incl. occlusion), 2 edge, 3 fade.
            var slot = 3u;
            if cat == 1u {
                slot = 0u;
            } else if cat == 2u || cat == 6u || cat == 7u {
                slot = 1u;
            } else if cat == 3u || cat == 4u {
                slot = 2u;
            }
            atomicAdd(&wg_cat[slot], 1u);
            atomicAdd(&wg_cat[6], 1u);
        }
        if wz > 0.5 {
            atomicAdd(&wg_cat[4], 1u);
        }
    }
    workgroupBarrier();
    if li < 8u && li != 5u {
        let v = atomicLoad(&wg_cat[li]);
        if v > 0u {
            atomicAdd(&counters[li], v);
        }
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_offsets_sum_to_motion() {
        for g in [
            [0.0, 0.0],
            [7.0, -3.0],
            [-120.0, 1.0],
            [1.0, 1.0],
            [-5.4, 9.6],
        ] {
            let (a, b) = half_offsets(g);
            assert_eq!(a[0] + b[0], g[0].round() as i32);
            assert_eq!(a[1] + b[1], g[1].round() as i32);
            assert!((a[0] - b[0]).abs() <= 1 && (a[1] - b[1]).abs() <= 1);
        }
    }

    #[test]
    fn params_layout_is_16_byte_multiple() {
        assert_eq!(std::mem::size_of::<FixParams>() % 16, 0);
    }
}
