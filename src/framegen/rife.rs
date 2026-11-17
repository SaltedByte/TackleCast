//! RIFE frame-interpolation backend (Windows, ONNX Runtime + DirectML).
//!
//! Drop-in alternative to the NVIDIA FRUC worker with the same job protocol.
//! Runs RIFE (Practical-RIFE weights, MIT) exported to ONNX. It reads the
//! frame-generation source slots and writes the output slots directly, with
//! no copies and no CPU round trip:
//!
//! * DirectML runs on the renderer's own `ID3D12Device` (wgpu's), on a
//!   dedicated compute queue;
//! * the shared slot buffers are opened on that device and wrapped as
//!   DirectML tensors once at start-up;
//! * per job the worker waits for the capture submission (CPU fence, like
//!   the FRUC worker), runs the network on (previous unique frame, this
//!   frame) and waits for the GPU before reporting, so the renderer only
//!   ever reads finished output.
//!
//! Runtime files (folder `rife` next to the executable):
//! `onnxruntime.dll`, `onnxruntime_providers_shared.dll`, `DirectML.dll`
//! and `models/<model>_<W>x<H>.onnx` (`rife426fmh`, `rife46fmh`, `rife46fm`).
//!
//! Models whose stem ends in `fm` / `fmh` output motion + blend mask instead
//! of an image: fp16 `[1, H/2, W/2, 8]` (flow to frame 0, flow to frame 1,
//! mask, padding), which fills the output slot exactly. `fmh` models run the
//! network on the frames scaled down 2x. The renderer builds the frame from
//! it (`FgGpu::encode_flow_warp`). Models that output an RGBA image directly
//! are still supported.

use super::duplicate::is_duplicate;
use super::worker::{read_stats, spawn_thread, wait_for_gpu, Command};
use super::{Backend, BackendEvent, BackendInfo, JobResult, Outcome};
use crate::render::FgImport;
use crossbeam_channel::Receiver;
use ort::memory::{AllocationDevice, AllocatorType, MemoryInfo, MemoryType};
use ort::session::Session;
use ort::value::{Shape, TensorRefMut};
use ort::AsPointer;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use wgpu::hal::api::Dx12;
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;

/// IID of `IDMLDevice` (DirectML.h).
const IID_IDML_DEVICE: GUID = GUID::from_u128(0x6dbd6437_96fd_423f_a98c_ae5e7c2a573f);

type DmlCreateDevice =
    unsafe extern "system" fn(*mut c_void, u32, *const GUID, *mut *mut c_void) -> HRESULT;

pub struct RifeConfig {
    /// Folder with the ONNX Runtime / DirectML DLLs and `models/`.
    pub dir: PathBuf,
    /// Model file stem (`rife426fmh`, ...).
    pub model: String,
}

/// `rife` next to the executable.
pub fn default_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default()
        .join("rife")
}

/// The process-wide ONNX Runtime environment. ONNX Runtime allows only one
/// per process and ort cannot build a second one after the first is
/// dropped, so it is created on first use and kept for the process
/// lifetime (backend restarts, replay modes) — dropping it would make every
/// later RIFE start fail.
fn environment(dir: &Path) -> Result<ort::environment::Environment, String> {
    static ENV: std::sync::Mutex<Option<ort::environment::Environment>> =
        std::sync::Mutex::new(None);
    let mut guard = ENV.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(env) = guard.as_ref() {
        return Ok(env.clone());
    }
    let env = ort::init_from(dir.join("onnxruntime.dll"))
        .map_err(|e| format!("ONNX Runtime: {e}"))?
        .with_name("tacklecast-rife")
        .build()
        .map_err(|e| format!("ONNX Runtime: {e}"))?;
    *guard = Some(env.clone());
    Ok(env)
}

pub fn model_path(dir: &Path, model: &str, width: u32, height: u32) -> PathBuf {
    dir.join("models")
        .join(format!("{model}_{width}x{height}.onnx"))
}

pub fn spawn(
    import: FgImport,
    device: wgpu::Device,
    config: RifeConfig,
    waker: Box<dyn Fn() + Send>,
) -> Box<dyn Backend> {
    spawn_thread(
        "tacklecast-framegen-rife",
        move |commands, send| run(import, device, config, commands, send),
        waker,
    )
}

/// The renderer's D3D12 device (wgpu DX12 backend).
fn raw_device(device: &wgpu::Device) -> Option<ID3D12Device> {
    unsafe { device.as_hal::<Dx12>().map(|d| d.raw_device().clone()) }
}

fn open_shared(device: &ID3D12Device, handle: *mut c_void) -> Result<ID3D12Resource, String> {
    let mut resource: Option<ID3D12Resource> = None;
    unsafe { device.OpenSharedHandle(HANDLE(handle), &mut resource) }
        .map_err(|e| format!("OpenSharedHandle: {e}"))?;
    resource.ok_or_else(|| "OpenSharedHandle returned null".into())
}

/// A slot buffer as a DirectML tensor `[1, H, W, 4]` u8.
struct SlotTensor {
    // Field order = drop order: tensor, DML allocation, D3D12 resource.
    tensor: TensorRefMut<'static, u8>,
    _allocation: ort::ep::directml::DMLResource,
    _resource: ID3D12Resource,
}

fn slot_tensor(resource: ID3D12Resource, width: u32, height: u32) -> Result<SlotTensor, String> {
    let allocation = unsafe { ort::ep::directml::resource_from_d3d(resource.as_raw().cast()) }
        .map_err(|e| format!("DirectML allocation from slot buffer: {e}"))?;
    let info = MemoryInfo::new(
        AllocationDevice::DIRECTML,
        0,
        AllocatorType::Device,
        MemoryType::Default,
    )
    .map_err(|e| e.to_string())?;
    let tensor = unsafe {
        TensorRefMut::<u8>::from_raw(
            info,
            *allocation,
            Shape::new([1, height as i64, width as i64, 4]),
        )
    }
    .map_err(|e| format!("DirectML tensor: {e}"))?;
    Ok(SlotTensor {
        tensor,
        _allocation: allocation,
        _resource: resource,
    })
}

/// An output slot as the flow + mask tensor `[1, H/2, W/2, 8]` fp16 of the
/// `fm` models (created through the C API: ort's typed constructor needs
/// the `half` feature).
struct FlowTensor {
    value: *mut ort::sys::OrtValue,
    _allocation: ort::ep::directml::DMLResource,
    _resource: ID3D12Resource,
}

impl Drop for FlowTensor {
    fn drop(&mut self) {
        // Release the tensor before the DML allocation / resource fields.
        unsafe { (ort::api().ReleaseValue)(self.value) };
    }
}

fn flow_tensor(resource: ID3D12Resource, width: u32, height: u32) -> Result<FlowTensor, String> {
    let allocation = unsafe { ort::ep::directml::resource_from_d3d(resource.as_raw().cast()) }
        .map_err(|e| format!("DirectML allocation from slot buffer: {e}"))?;
    let info = MemoryInfo::new(
        AllocationDevice::DIRECTML,
        0,
        AllocatorType::Device,
        MemoryType::Default,
    )
    .map_err(|e| e.to_string())?;
    let shape = [1_i64, (height / 2) as i64, (width / 2) as i64, 8];
    let bytes = (width / 2) as usize * (height / 2) as usize * 8 * 2;
    let mut value: *mut ort::sys::OrtValue = std::ptr::null_mut();
    unsafe {
        ort::Error::result_from_status((ort::api().CreateTensorWithDataAsOrtValue)(
            info.ptr(),
            *allocation,
            bytes,
            shape.as_ptr(),
            shape.len(),
            ort::sys::ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT16,
            &mut value,
        ))
    }
    .map_err(|e| format!("DirectML flow tensor: {e}"))?;
    if value.is_null() {
        return Err("DirectML flow tensor: null".into());
    }
    Ok(FlowTensor {
        value,
        _allocation: allocation,
        _resource: resource,
    })
}

/// An output slot bound either as an image or as flow + mask.
enum OutTensor {
    Image(SlotTensor),
    Flow(FlowTensor),
}

impl OutTensor {
    fn ptr(&self) -> *const ort::sys::OrtValue {
        match self {
            Self::Image(t) => t.tensor.ptr(),
            Self::Flow(t) => t.value,
        }
    }
}

/// Bind (img0, img1) -> out and run; waits for the GPU.
fn interpolate(
    session: &mut Session,
    prev: &SlotTensor,
    next: &SlotTensor,
    out: &OutTensor,
) -> Result<(), String> {
    let mut binding = session.create_binding().map_err(|e| e.to_string())?;
    // Bind through the C API: ort's safe wrapper clones non-owned device
    // input tensors (a copy of every frame) and refuses borrowed outputs.
    let api = ort::api();
    for (name, tensor) in [(c"img0", &prev.tensor), (c"img1", &next.tensor)] {
        unsafe {
            ort::Error::result_from_status((api.BindInput)(
                binding.ptr_mut(),
                name.as_ptr(),
                tensor.ptr(),
            ))
        }
        .map_err(|e| format!("bind input: {e}"))?;
    }
    unsafe {
        ort::Error::result_from_status((api.BindOutput)(
            binding.ptr_mut(),
            c"out".as_ptr(),
            out.ptr(),
        ))
    }
    .map_err(|e| format!("bind output: {e}"))?;
    {
        let _outputs = session
            .run_binding(&binding)
            .map_err(|e| format!("RIFE inference: {e}"))?;
    }
    binding
        .synchronize_outputs()
        .map_err(|e| format!("RIFE GPU sync: {e}"))
}

fn run(
    import: FgImport,
    device: wgpu::Device,
    config: RifeConfig,
    commands: &Receiver<Command>,
    send: &dyn Fn(BackendEvent),
) -> Result<(), String> {
    let init_started = Instant::now();
    let fail = |e: String| {
        send(BackendEvent::InitFailed(e));
        Ok(())
    };
    let FgImport {
        width,
        height,
        pitch,
        source_handles,
        output_handles,
        readbacks,
        ..
    } = import;
    if pitch != width * 4 {
        return fail(format!(
            "RIFE needs packed slots (pitch {pitch}, width {width})"
        ));
    }
    let dir = config.dir;
    let model_name = config.model;
    let model = model_path(&dir, &model_name, width, height);
    if !model.is_file() {
        return fail(format!(
            "RIFE has no model for {width}x{height} ({} not found). Available: 2560x1440, \
             1920x1080, 1280x720",
            model.display()
        ));
    }

    // --- D3D12: the renderer's device + a compute queue for DirectML ------
    let Some(d3d) = raw_device(&device) else {
        return fail("RIFE needs the DirectX 12 renderer".into());
    };
    let queue: ID3D12CommandQueue = match unsafe {
        d3d.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
            Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
            Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
            NodeMask: 0,
        })
    } {
        Ok(q) => q,
        Err(e) => return fail(format!("D3D12 compute queue: {e}")),
    };

    // --- DirectML (our copy, loaded first so ONNX Runtime uses it too) ----
    let dml_lib = match unsafe { libloading::Library::new(dir.join("DirectML.dll")) } {
        Ok(lib) => lib,
        Err(e) => return fail(format!("{}: {e}", dir.join("DirectML.dll").display())),
    };
    let dml_device: windows::core::IUnknown = unsafe {
        let create: libloading::Symbol<DmlCreateDevice> = match dml_lib.get(b"DMLCreateDevice\0") {
            Ok(f) => f,
            Err(e) => return fail(format!("DMLCreateDevice: {e}")),
        };
        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = create(d3d.as_raw(), 0, &IID_IDML_DEVICE, &mut raw);
        if hr.is_err() || raw.is_null() {
            return fail(format!("DMLCreateDevice failed: {hr:?}"));
        }
        windows::core::IUnknown::from_raw(raw)
    };

    // --- ONNX Runtime session on that device/queue -------------------------
    let env = match environment(&dir) {
        Ok(env) => env,
        Err(e) => return fail(e),
    };
    let Some(dml_api) = ort::ep::directml::api() else {
        return fail("this onnxruntime.dll has no DirectML support".into());
    };
    let session = (|| -> Result<Session, String> {
        let mut builder = Session::builder(&env)
            .map_err(|e| e.to_string())?
            .with_memory_pattern(false)
            .map_err(|e| e.to_string())?
            .with_parallel_execution(false)
            .map_err(|e| e.to_string())?;
        // Diagnostics: per-operator ONNX Runtime profile (which execution
        // provider runs each node) when TACKLECAST_RIFE_PROFILE names a file.
        if let Ok(path) = std::env::var("TACKLECAST_RIFE_PROFILE") {
            builder = builder.with_profiling(path).map_err(|e| e.to_string())?;
        }
        unsafe {
            ort::Error::result_from_status((dml_api.SessionOptionsAppendExecutionProvider_DML1)(
                builder.ptr_mut(),
                dml_device.as_raw(),
                queue.as_raw(),
            ))
        }
        .map_err(|e| format!("DirectML provider: {e}"))?;
        builder
            .commit_from_file(&model)
            .map_err(|e| format!("{}: {e}", model.display()))
    })();
    let mut session = match session {
        Ok(s) => s,
        Err(e) => return fail(e),
    };

    // --- Slot buffers as tensors -------------------------------------------
    let flow_output = model_name.ends_with("fm") || model_name.ends_with("fmh");
    let mut sources = Vec::new();
    for handle in source_handles.iter() {
        match open_shared(&d3d, handle.handle).and_then(|r| slot_tensor(r, width, height)) {
            Ok(t) => sources.push(t),
            Err(e) => return fail(e),
        }
    }
    let mut outputs = Vec::new();
    for handle in output_handles.iter() {
        let tensor = open_shared(&d3d, handle.handle).and_then(|r| {
            if flow_output {
                flow_tensor(r, width, height).map(OutTensor::Flow)
            } else {
                slot_tensor(r, width, height).map(OutTensor::Image)
            }
        });
        match tensor {
            Ok(t) => outputs.push(t),
            Err(e) => return fail(e),
        }
    }
    drop(source_handles);
    drop(output_handles);
    if sources.len() < 2 || outputs.is_empty() {
        return fail("RIFE needs at least two source slots and one output slot".into());
    }

    // --- Warm-up: compiles DirectML operators for this shape -------------
    let warm_started = Instant::now();
    for _ in 0..3 {
        let (a, rest) = sources.split_at(1);
        if let Err(e) = interpolate(&mut session, &a[0], &rest[0], &outputs[0]) {
            return fail(format!("RIFE warm-up failed: {e}"));
        }
    }
    let warm = warm_started.elapsed();
    let description = format!(
        "RIFE {}{} on DirectML ({}x{}), runtime {}",
        model_name,
        if flow_output {
            " + TackleCast blend"
        } else {
            ""
        },
        width,
        height,
        dir.display()
    );
    info!(
        "framegen RIFE ready in {:.0} ms (warm-up {:.0} ms)",
        init_started.elapsed().as_secs_f64() * 1000.0,
        warm.as_secs_f64() * 1000.0
    );
    send(BackendEvent::Ready(BackendInfo {
        description,
        warmup: warm,
        flow_output,
    }));

    // --- Job loop (same protocol as the FRUC worker) ------------------------
    let mut prev: Option<usize> = None;
    let mut need_skip = true;
    for command in commands.iter() {
        let job = match command {
            Command::Process(job) => job,
            Command::Shutdown => break,
        };
        let started = Instant::now();
        wait_for_gpu(&device, job.fence.0);
        let gpu_wait = started.elapsed();

        let (stats, motion) = if job.has_reference {
            match readbacks
                .get(job.source)
                .map(|b| read_stats(&device, b, width as usize, height as usize))
            {
                Some(Ok((stats, motion))) => (Some(stats), motion),
                Some(Err(e)) => {
                    warn!("duplicate check unavailable for this frame: {e}");
                    (None, None)
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let done = |outcome: Outcome, fruc_time: Duration| {
            send(BackendEvent::Done(JobResult {
                seq: job.seq,
                outcome,
                gpu_wait,
                fruc_time,
                motion,
            }))
        };
        if let Some(stats) = stats {
            if is_duplicate(&stats, &job.thresholds) {
                done(Outcome::Duplicate(stats), Duration::ZERO);
                continue;
            }
        }
        if !job.fruc {
            need_skip = true;
            done(Outcome::Unique { stats }, Duration::ZERO);
            continue;
        }
        let interp_from = prev.filter(|p| *p != job.source && !job.skip_warp && !need_skip);
        let Some(from) = interp_from else {
            // Discontinuity / first frame: this frame becomes the reference.
            prev = Some(job.source);
            need_skip = false;
            done(Outcome::Unique { stats }, Duration::ZERO);
            continue;
        };
        if job.source >= sources.len() || job.output >= outputs.len() || from >= sources.len() {
            done(
                Outcome::Failed("slot index out of range".into()),
                Duration::ZERO,
            );
            continue;
        }
        let t = Instant::now();
        let result = interpolate(
            &mut session,
            &sources[from],
            &sources[job.source],
            &outputs[job.output],
        );
        let elapsed = t.elapsed();
        match result {
            Ok(()) => {
                prev = Some(job.source);
                done(
                    Outcome::Interpolated {
                        repeated: false,
                        stats,
                    },
                    elapsed,
                );
            }
            Err(e) => {
                need_skip = true;
                done(Outcome::Failed(e), elapsed);
            }
        }
    }

    // --- Teardown: tensors and allocations before the device --------------
    drop(outputs);
    drop(sources);
    drop(session);
    drop(env);
    drop(dml_device);
    drop(queue);
    drop(dml_lib);
    info!("framegen RIFE worker stopped");
    Ok(())
}
