//! Opt-in native Media Foundation GPU capture. Pixel data stays GPU-resident.
use crate::{
    capture::{CaptureFrame, CaptureStats},
    color::ColorInfo,
    frame_rate::FrameRate,
    triple_buffer::Producer,
    AppEvent,
};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use windows::{core::Interface, Win32::Graphics::Direct3D12::ID3D12Resource};

pub struct GpuFrame {
    pub buffer: wgpu::Buffer,
    pub width: u32,
    pub height: u32,
    pub uv_offset: u64,
    pub y_pitch: u32,
    pub uv_pitch: u32,
    pub color_info: ColorInfo,
    _lease: Arc<SlotLease>,
}

// A slot returns to the producer only after the last frame owner, including
// the queue-completion callback, releases its permit.
struct SlotLease(Arc<AtomicBool>);
impl SlotLease {
    fn acquire(busy: &Arc<AtomicBool>) -> Option<Arc<Self>> {
        busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Arc::new(Self(busy.clone())))
    }
}
impl Drop for SlotLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
struct PoolSlot {
    buffer: wgpu::Buffer,
    resource: ID3D12Resource,
    layout: Output,
    busy: Arc<AtomicBool>,
}
#[repr(C)]
#[derive(Default)]
struct Output {
    resource: *mut c_void,
    size: u64,
    uv_offset: u64,
    y_pitch: u32,
    uv_pitch: u32,
}
type Open =
    unsafe extern "C" fn(*mut c_void, *const u16, u32, u32, u32, u32, *mut *mut c_void) -> i32;
type Allocate = unsafe extern "C" fn(*mut c_void, *mut Output) -> i32;
type Next = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut Output) -> i32;
type Close = unsafe extern "C" fn(*mut c_void);
type Metadata = unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32, *mut u32, *mut u32) -> i32;

pub fn eligible(format: crate::settings::VideoFormat, nv12_advertised: bool) -> bool {
    use crate::settings::VideoFormat;
    match format {
        VideoFormat::Auto => nv12_advertised,
        VideoFormat::Nv12 => true,
        _ => false,
    }
}

fn native_color_info(matrix: u32, range: u32) -> ColorInfo {
    use crate::settings::{ColorRange, ColorSpace};
    ColorInfo {
        space: match matrix {
            1 => Some(ColorSpace::Rec709),
            2 => Some(ColorSpace::Bt601),
            4 | 5 => Some(ColorSpace::Bt2020),
            _ => None,
        },
        range: match range {
            1 => Some(ColorRange::Full),
            2 => Some(ColorRange::Limited),
            _ => None,
        },
        ..ColorInfo::default()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    device: wgpu::Device,
    name: String,
    width: u32,
    height: u32,
    rate: FrameRate,
    stop: Arc<AtomicBool>,
    mut producer: Producer<CaptureFrame>,
    stats: crossbeam_channel::Sender<CaptureStats>,
    errors: crossbeam_channel::Sender<String>,
    events: winit::event_loop::EventLoopProxy<AppEvent>,
) {
    let result = (|| -> Result<(), String> {
        let path =
            std::env::var_os("TACKLECAST_MF_GPU_DLL").ok_or("TACKLECAST_MF_GPU_DLL is required")?;
        if !std::path::Path::new(&path).is_absolute() {
            return Err("Native bridge DLL path must be absolute".into());
        }
        // SAFETY: explicit opt-in to the locally built bridge with this C ABI.
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| e.to_string())?;
        let version: unsafe extern "C" fn() -> u32 = unsafe {
            *lib.get(b"tc_mf_abi_version\0")
                .map_err(|_| "Rebuild the native GPU bridge (ABI 2 required)")?
        };
        if unsafe { version() } != 2 {
            return Err("Native GPU bridge ABI mismatch".into());
        }
        let open: Open = unsafe { *lib.get(b"tc_mf_open\0").map_err(|e| e.to_string())? };
        let allocate: Allocate =
            unsafe { *lib.get(b"tc_mf_allocate\0").map_err(|e| e.to_string())? };
        let next: Next = unsafe { *lib.get(b"tc_mf_next_reuse\0").map_err(|e| e.to_string())? };
        let close: Close = unsafe { *lib.get(b"tc_mf_close\0").map_err(|e| e.to_string())? };
        let metadata: Metadata = unsafe {
            *lib.get(b"tc_mf_metadata\0")
                .map_err(|_| "Rebuild the native GPU bridge for color metadata support")?
        };
        let raw = unsafe {
            device.as_hal::<wgpu::hal::api::Dx12, _, _>(|d| d.map(|d| d.raw_device().clone()))
        }
        .ok_or("DX12 device required")?;
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut session = std::ptr::null_mut();
        let hr = unsafe {
            open(
                raw.as_raw(),
                wide.as_ptr(),
                width,
                height,
                rate.numerator,
                rate.denominator,
                &mut session,
            )
        };
        if hr < 0 {
            return Err(format!("MF GPU open failed: 0x{:08x}", hr as u32));
        }
        struct Session {
            p: *mut c_void,
            close: Close,
        }
        impl Drop for Session {
            fn drop(&mut self) {
                unsafe { (self.close)(self.p) }
            }
        }
        let session = Session { p: session, close };
        let (mut matrix, mut range, mut actual_n, mut actual_d) = (0, 0, 0, 0);
        let hr = unsafe {
            metadata(
                session.p,
                &mut matrix,
                &mut range,
                &mut actual_n,
                &mut actual_d,
            )
        };
        if hr < 0 {
            return Err(format!("MF metadata query failed: 0x{:08x}", hr as u32));
        }
        let color_info = native_color_info(matrix, range);
        tracing::info!("MF native metadata: rate={actual_n}/{actual_d}, matrix={matrix}, range={range}, interpreted={color_info:?}");
        let mut pool = Vec::with_capacity(6);
        for _ in 0..6 {
            let mut output = Output::default();
            let hr = unsafe { allocate(session.p, &mut output) };
            if hr < 0 || output.resource.is_null() {
                return Err(format!(
                    "MF GPU pool allocation failed: 0x{:08x}",
                    hr as u32
                ));
            }
            let resource = unsafe { ID3D12Resource::from_raw(output.resource) };
            let buffer = unsafe {
                let hal = wgpu::hal::dx12::Device::buffer_from_raw(resource.clone(), output.size);
                device.create_buffer_from_hal::<wgpu::hal::api::Dx12>(
                    hal,
                    &wgpu::BufferDescriptor {
                        label: Some("mf-nv12-pooled-planes"),
                        size: output.size,
                        usage: wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    },
                )
            };
            pool.push(PoolSlot {
                buffer,
                resource,
                layout: output,
                busy: Arc::new(AtomicBool::new(false)),
            });
        }
        tracing::info!(
            "MF GPU buffer pool ready: {} slots, {} bytes each",
            pool.len(),
            pool[0].layout.size
        );
        let mut started = Instant::now();
        let mut frames = 0;
        tracing::info!("experimental MF GPU NV12 capture opened: {width}x{height} @ {rate}");
        while !stop.load(Ordering::Relaxed) {
            // Bounded storage: poll completed callbacks rather than allocating
            // extra buffers when the renderer temporarily falls behind.
            device.poll(wgpu::Maintain::Poll);
            let Some((slot, lease)) = pool
                .iter()
                .find_map(|slot| SlotLease::acquire(&slot.busy).map(|lease| (slot, lease)))
            else {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            let mut output = Output::default();
            let hr = unsafe { next(session.p, slot.resource.as_raw(), &mut output) };
            if hr == 1 {
                continue;
            }
            if hr < 0 {
                return Err(format!("MF GPU capture failed: 0x{:08x}", hr as u32));
            }
            if output.resource.is_null() {
                return Err("Native bridge returned no GPU resource".into());
            }
            // Release the bridge's temporary reference. The cached slot owns
            // the allocation and wgpu wrapper; no GPU allocation occurs here.
            let returned = unsafe { ID3D12Resource::from_raw(output.resource) };
            if returned.as_raw() != slot.resource.as_raw()
                || output.size != slot.layout.size
                || output.uv_offset != slot.layout.uv_offset
                || output.y_pitch != slot.layout.y_pitch
                || output.uv_pitch != slot.layout.uv_pitch
            {
                return Err("MF GPU pooled resource/layout mismatch".into());
            }
            drop(returned);
            producer.write(CaptureFrame::MfGpu(Arc::new(GpuFrame {
                buffer: slot.buffer.clone(),
                width,
                height,
                uv_offset: output.uv_offset,
                y_pitch: output.y_pitch,
                uv_pitch: output.uv_pitch,
                color_info,
                _lease: lease,
            })));
            let _ = events.send_event(AppEvent::FrameReady);
            frames += 1;
            if started.elapsed().as_millis() >= 300 {
                let _ = stats.send(CaptureStats {
                    format_label: "NV12 (MF GPU)".into(),
                    nominal_rate: Some(rate),
                    fps: frames as f32 / started.elapsed().as_secs_f32(),
                    width,
                    height,
                });
                started = Instant::now();
                frames = 0;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = errors.send(error);
        let _ = events.send_event(AppEvent::FrameReady);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backend_selection_respects_explicit_pixel_formats() {
        use crate::settings::VideoFormat;
        for format in VideoFormat::ALL {
            assert_eq!(
                eligible(format, true),
                matches!(format, VideoFormat::Auto | VideoFormat::Nv12)
            );
            assert_eq!(eligible(format, false), format == VideoFormat::Nv12);
        }
    }
    #[test]
    fn mf_metadata_preserves_known_and_unknown_values() {
        use crate::settings::{ColorRange, ColorSpace};
        assert_eq!(native_color_info(1, 2).space, Some(ColorSpace::Rec709));
        assert_eq!(native_color_info(1, 2).range, Some(ColorRange::Limited));
        assert_eq!(native_color_info(2, 1).space, Some(ColorSpace::Bt601));
        assert_eq!(native_color_info(2, 1).range, Some(ColorRange::Full));
        assert_eq!(native_color_info(4, 2).space, Some(ColorSpace::Bt2020));
        assert_eq!(native_color_info(5, 2).space, Some(ColorSpace::Bt2020));
        for unsupported in [0, 3, 99] {
            assert_eq!(native_color_info(unsupported, unsupported).space, None);
            assert_eq!(native_color_info(unsupported, unsupported).range, None);
        }
        let info = native_color_info(1, 2);
        assert_eq!(
            info.resolve_space(ColorSpace::Bt601, 1440),
            ColorSpace::Bt601
        );
        assert_eq!(info.resolve_range(ColorRange::Full), ColorRange::Full);
    }
    #[test]
    fn pool_slot_waits_for_final_consumer() {
        let busy = Arc::new(AtomicBool::new(false));
        let frame = SlotLease::acquire(&busy).unwrap();
        let gpu_completion = frame.clone();
        drop(frame);
        assert!(SlotLease::acquire(&busy).is_none());
        std::thread::spawn(move || drop(gpu_completion))
            .join()
            .unwrap();
        assert!(SlotLease::acquire(&busy).is_some());
    }
    #[test]
    fn unsubmitted_frame_releases_slot() {
        let busy = Arc::new(AtomicBool::new(false));
        drop(SlotLease::acquire(&busy).unwrap());
        assert!(SlotLease::acquire(&busy).is_some());
    }
}
