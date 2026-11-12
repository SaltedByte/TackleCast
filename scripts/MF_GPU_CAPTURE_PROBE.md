# Native NV12 GPU capture investigation

> **Note (2026-11):** The native bridge is now compiled into TackleCast.exe, so
> `tacklecast_mf_gpu.dll`, `scripts/build_mf_bridge.ps1` and
> `scripts/run_mf_gpu_viewer.ps1` no longer exist. The steps that use them below
> are kept as a record of how the investigation was done.

This standalone Media Foundation probe does not change TackleCast. Its default
mode enumerates device names without activating a source. That mode found
`Elgato 4K X` on this computer on 2026-10-31.

```powershell
.\scripts\probe_mf_gpu_capture.ps1
```

Close TackleCast and other apps using the 4K X before running the capture test:

```powershell
.\scripts\probe_mf_gpu_capture.ps1 -Capture
.\scripts\probe_mf_gpu_capture.ps1 -Capture -Frames 1200
```

The probe selects the 9070 adapter, supplies a D3D11 DXGI device manager to the
Source Reader, disables converters, and requires native NV12 2560x1440
at nominal 120 FPS (within half a 100 ns capture interval of 120/1). It
preserves the advertised rational rate. It inspects 60 samples for IMFDXGIBuffer/ID3D11Texture2D without
mapping pixels or calling ConvertToContiguousBuffer. There is no resolution or
rate downgrade. The wrapper terminates only its own probe after 30 seconds and
records output in `.deps/mf-gpu-probe/probe.log`.

GPU surfaces would establish a candidate surface-delivery API, not prove that
the USB driver performs no internal staging or that wgpu/DX12 can consume the
resources safely. CPU buffers mean this particular Source Reader configuration
has not provided GPU-only capture, not that every Windows capture API or driver
configuration is incapable of it. The existing CPU-backed retained-frame path
remains available.

## Local result — 2026-10-31

Live capture passed with TackleCast closed:

- Advertised native rate: `10000000/83333` (approximately 120.00048 FPS).
- Received 60 GPU-backed samples and zero CPU-backed samples.
- All textures were NV12 2560x1440, array size 1, subresource 0, on the
  explicitly supplied RX 9070 XT device (adapter identity checked).
- Bind flags: 1064; miscellaneous flags: 2050 (includes shared NT handle).
- Every native capture texture exported a shared handle and opened successfully
  as an ID3D12Resource on a DX12 device on the same adapter. Handles were closed
  immediately after import. No pixel readback or application pixel copy.

This is a candidate GPU-surface capture path, not an integrated viewer backend.
Cross-API GPU producer/consumer synchronization, lifetime under asynchronous
rendering, wgpu wrapping/plane consumption, image correctness, sustained delivery,
and latency still require implementation and testing. No conclusion is drawn
about hidden USB-driver staging. Successful import alone does not prove that
sampling those resources without further synchronization is safe.

## Synchronized consumer validation

The probe now creates a shared D3D11 producer fence, signals and flushes the
immediate context, waits on the imported fence from a DX12 consumer queue,
transitions the capture resource from COMMON to COPY_SOURCE and back, and
retains the IMFSample until consumer completion. Both NV12 planes are compared
against an independent D3D11 staging readback. The mappings are diagnostic
verification only, and must be removed from a production GPU rendering path.

An initial 1,200-frame run verified all pixels but crashed with heap corruption;
another crashed during capture. The probe shared the immediate context with
Media Foundation without enabling D3D11 multithread protection. This required
protection is now enabled before handing the device to Media Foundation.
With protection enabled, two consecutive 1,200-frame runs verified both planes
of every frame and exited normally. This is evidence of improvement, not a sustained-playback
or latency benchmark. Reader flushing and source shutdown are explicit while
the device manager and graphics devices remain alive.

The native consumer is still separate from TackleCast. Integration must keep
device multithread protection, producer/consumer synchronization, and sample
ownership, reuse allocated resources, and feed wgpu without pixel readback.

## Experimental viewer integration

This section is a dated record. The bridge has since become automatic and is
packaged; see `MF_GPU_SETTINGS_AUDIT.md` for current behavior.

The viewer now has an opt-in native bridge, `src/mf_gpu_native.cpp`, loaded by
`src/mf_gpu.rs`. Build and launch with the card free:

```powershell
.\scripts\run_mf_gpu_viewer.ps1 -Build
```

Subsequent launches can omit `-Build`. The helper scopes the enable flag and
absolute DLL path to the viewer launch. The normal executable remains usable
without this flag. The native bridge is not part of release packaging yet.

The backend requires a unique Media Foundation friendly-name match and a native
NV12 mode matching the selected dimensions and timestamp-rounded rate. It
selects the graphics adapter by the actual wgpu DX12 device's LUID. It does not
silently fall back to a different rate or a CPU decoder. The overlay identifies
the active path as `NV12 (MF GPU)`. Color Auto now reads negotiated native matrix/range metadata; missing values retain explicitly labeled defaults and manual overrides remain available.

An asynchronous Source Reader permits stop checks during missing input. The
bridge signals a D3D11 fence, waits on DX12, copies Y and UV into a GPU buffer,
restores the capture resource state, and waits for completion before releasing
the capture sample. Rust wraps the transferred COM reference as a wgpu buffer
on the same device. The renderer copies its planes to R8/RG8 textures and holds
the frame until submission completion. No pixel mappings, diagnostic readbacks,
CPU chroma processing, or queue.write_texture calls occur on this path.

This is GPU-resident pixel processing with GPU-side copies, not literally
copy-free transport. Buffers/command allocators are currently created per frame
and the native copy has a CPU completion wait, so latency/performance still need
optimization. An initial live build opened 1440p120 but delivered about 60–65 FPS.
Its polling sleep has been replaced with a GPU-fence completion event; the
revised bridge was reloaded and the user's live screenshot showed
`2560x1440 | NV12 (MF GPU) | 120 nominal FPS | 119.8 delivered FPS`.
This confirms near-target delivery at that moment; it does not measure display
latency, frame pacing over time, or hidden driver staging. Standard unit tests,
strict Clippy, and software-only checks pass. The integrated path is experimental
and requires visual, delivery, shutdown, and longer-run validation.

### Overlapped delivery revision

The bridge now keeps at most one asynchronous read outstanding while the current
sample is copied, and reuses one command allocator/list after its completion
fence. This overlaps capture delivery and copy work without an application FIFO
or resetting in-flight command memory. The current sample remains referenced
until the copy completes. The output buffer is still allocated per frame.

The revision builds with MinGW `-Wall -Wextra -Werror`. The prior working bridge
is retained locally as `.deps/mf-gpu-probe/tacklecast_mf_gpu_pre_overlap.dll`.
Live delivery and film-based latency must be compared before claiming a gain.
Remaining candidates are bounded GPU-buffer pooling, queue-to-queue completion
instead of the CPU fence wait, and direct shared-texture sampling to remove
copies. These require consumer-completion lifetime tracking and broader tests.

### Bounded output-buffer pool revision

Build/launch the isolated pooled variant with
`scripts/run_mf_gpu_viewer.ps1 -Pooled -Build`; omit `-Build` once compiled.
The new Rust client requires native ABI version 2 and fails clearly if paired
with the older bridge. The bridge retains the old entry points for comparison.

Six DEFAULT-heap buffers are allocated and wrapped in wgpu once per session.
Every published frame owns a pool permit. The permit returns only when the
last frame reference drops, including the wgpu submission-completion callback.
Frames discarded before upload release their slots too. Pool exhaustion polls
GPU completions and waits rather than allocating additional buffers. The frame
storage remains bounded. Closing capture can drop the session while GPU frame
references retain their own buffer ownership; permit release requires no DLL
callback after unload.

The bridge still waits for its copy fence before returning, so this revision
does not remove the CPU fence wait. Output buffers are transitioned from COMMON
to COPY_DEST and back. Prior wgpu COPY_SRC use is complete before a pool permit
can be acquired again; D3D12 buffers decay to COMMON after execution.

Native compilation passes `-Wall -Wextra -Werror`; 45 Rust unit tests pass,
including final-consumer and unsubmitted-frame permit release. Live validation
and latency measurements remain required before claiming an improvement.

Local pooled-viewer run: startup confirms six 5,529,600-byte buffers at
2560x1440. Consecutive steady 30-second intervals logged 120.0 rendered FPS,
120.0 uploaded FPS, and approximately 120 captured FPS. No capture/renderer
errors were logged in these intervals. Visual confirmation and film-based
latency comparison remain separate acceptance checks; no latency gain is claimed.

References:
- https://learn.microsoft.com/en-us/windows/win32/medfound/audio-video-capture-in-media-foundation
- https://learn.microsoft.com/en-us/windows/win32/medfound/processing-media-data-with-the-source-reader
