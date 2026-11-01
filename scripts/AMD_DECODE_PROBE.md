# AMD MJPEG decode probe

Run from the repository root in **64-bit Windows PowerShell**:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\probe_amd_decode.ps1
```

Requires the installed AMD graphics driver and either Visual Studio C++ build
tools with the Windows SDK or MSYS2 MinGW at `C:\msys64\mingw64`.
It does not require Rust, FFmpeg, CUDA, or an AMF runtime download.
The script downloads AMD's official AMF SDK source into the ignored `.deps`
directory. It resolves the upstream revision once, records the revision in the
cached directory name, and logs the archive SHA256. Subsequent runs reuse it.
Pass `-SdkRoot C:\path\to\AMF` to use an existing SDK checkout instead.

The probe selects a Radeon 9070 adapter when present (otherwise the first AMD
adapter), creates a D3D11 video device, loads the installed System32 AMF runtime,
and attempts MJPEG decoding into both NV12 and YUY2. It reports acceleration type,
AMF result codes, and output dimensions, and attempts a host-memory readback.
Each output query has a five-second deadline; a driver call itself could still
block. Success requires at least one accelerated output format to work.

By default it generates a synthetic 1920x1080 JPEG using Windows System.Drawing.
The current pattern includes a gradient, rectangles, and a diagonal line.
To test another JPEG, including a frame extracted from a capture stream:

```powershell
.\scripts\probe_amd_decode.ps1 -JpegPath C:\path\to\frame.jpg
```

Additional native GPU interoperability probes (independent of wgpu):

```powershell
.\scripts\probe_amd_decode.ps1 -Dx12
.\scripts\probe_amd_decode.ps1 -Dx12Bridge
.\scripts\probe_amd_decode.ps1 -Dx12Bridge -Frames 120 -SyntheticWidth 3840 -SyntheticHeight 2160
```

`-Dx12` tests direct DX12 decoding. `-Dx12Bridge` starts with DX11 hardware
decode, initializes DX12 after output arrives, and requests AMF's DX12 conversion.
It checks resource device identity, waits on AMF's producer fence from a separate
DX12 queue, copies to a GPU texture, restores the decoder resource state, and
waits for consumer completion while retaining the AMF surface. Diagnostic
readbacks compare every visible plane of the consumer texture against AMF's
readback. These readbacks are validation, not the proposed runtime path. The
probe exits successfully if at least one format passes; inspect per-format
results as well. A successful native copy does not establish wgpu integration
or prove that AMF's internal conversion has no hidden staging.

Results and build output are saved in `.deps\amf-probe\probe.log`.
A failure on one JPEG does not prove every JPEG profile is unsupported. In
particular the existing `assets\tests\nvjpeg-422.jpg` is only 16x8, which may be
below hardware minimum dimensions. A successful synthetic decode establishes a
candidate backend; it does not validate capture-card JPEG variants (including
omitted Huffman tables), image accuracy, sustained frame rate, latency, or
zero-copy interoperability with wgpu. The viewer is not modified by this probe.

API references:

- [AMD AMF decoder guide](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/doc/AMF_Video_Decode_API.md)
- [AMD AMF capability interfaces](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/public/include/components/ComponentCaps.h)

## Local result — 2026-10-31

The standalone probe passed while TackleCast remained running; it did not open
the capture card or change the application.

- Adapter: AMD Radeon RX 9070 XT; driver `32.0.32015.7029`.
- AMF runtime: `0x1000500030000`.
- Headers: official AMF revision `57ee0cfb0e1a08d9041f77435653794c1b012be0`.
- Source archive SHA256: `382A9F3559DD978F50F172E597286AC4E4EDD7A4FA3C693815383D2EF59D4B1B`.
- Compiler: installed MSYS2 MinGW g++, statically linked diagnostic.
- Input: System.Drawing-generated 1920x1080 JPEG.
- NV12 and YUY2: component creation, initialization, submission, output, and
  host readback all succeeded. Both reported `AMF_ACCEL_HARDWARE`.
- Output memory type was `3` (DX11); host pitches were 2048 and 4096 bytes,
  respectively.

This establishes hardware decode availability, not zero-copy rendering. Next
validate realistic 4:2:2 capture frames and missing DHT handling, then prove
GPU-only interop with the renderer's DX12 device on the same adapter. Resource
lifetime, producer/consumer fencing, color accuracy, and sustained latency must
be verified before enabling an AMD backend. A host readback is diagnostic only
and must not be part of the final zero-copy path.

## DX12 handoff result — 2026-10-31

- Direct DX12: context initialization succeeds, but MJPEG capabilities report
  unsupported acceleration. Initial testing also failed decoder initialization.
- DX11 decode followed by AMF DX12 conversion: both NV12 and YUY2 produce
  native resources on the explicitly supplied DX12 device. Producer-fence wait,
  GPU texture copy, and consumer-fence completion succeed for both formats.
- YUY2: every visible output byte matches AMF's own readback on the varied
  1080p pattern. This is the preferred candidate for a renderer prototype.
- NV12: luma comparison passes, but chroma differs at plane 1, row 0. Root cause
  is unresolved; do not enable this bridge format on the strength of submission
  success alone. The earlier constant-color test did not expose the mismatch.
- Native allocation: 1920x1088, while visible image is 1920x1080. Preserve crop
  and allocation dimensions separately.
- Initializing DX12 changes subsequent capability queries in the same AMF
  context. The probe uses a fresh context for each format. A persistent stream
  and separate decode/interop contexts still need investigation.

The next implementation should prototype YUY2 consumption on the actual wgpu
device, unpack packed 4:2:2 on the GPU, and preserve the existing color metadata.
It must also test continuous decoder reuse, asynchronous surface lifetime and
queue synchronization, and actual capture-card JPEGs before becoming a viewer
backend. No application source or running capture session was changed.

## Continuous 4K result — 2026-10-31

The target experience is interactive play with an Elgato 4K X and RX 9070 XT.
Acceptance must prioritize delivered frame rate, input-to-display latency,
consistent frame pacing, and picture accuracy rather than GPU usage alone.

The repeated-frame test passed 120 consecutive 3840x2160 synthetic JPEGs
through one YUY2 decoder in low-latency reorder mode, without per-frame drain
or decoder recreation. Every frame completed the DX11-to-DX12 transfer,
producer-fence wait, GPU consumer copy, and consumer-fence wait. All visible
consumer bytes matched AMF readback on every frame. Initializing DX12 after
the first output did not prevent subsequent decoding in the same component.
The native texture allocation was 3840x2176; visible dimensions remained
3840x2160.

Elapsed time was 2610 ms including per-frame resource/queue allocations,
verbose logging, CPU waits, and diagnostic pixel readbacks. This is a
correctness test, not evidence of 4K120 capability or gameplay latency. It
repeats one synthetic JPEG and does not validate varied live frames or the
4K X's actual JPEG stream. `-Frames` greater than one currently tests only
YUY2, the bridge format with verified pixel contents.

Remaining gates: reusable GPU resource pools and an actual wgpu consumer;
capture-card JPEG compatibility; sustained real-stream frame delivery; and
latency comparison against native NV12 at equivalent modes. The final path
must avoid diagnostic host readbacks and avoid queuing stale frames.
