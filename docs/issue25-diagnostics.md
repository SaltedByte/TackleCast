# Issue #25 Linux diagnostics

Diagnostics only, based on v2.2.0 commit
`542ffb76f7f32d1a44cb5f4fed3641bc4a5a53c3`. Version remains 2.2.0; the
startup event embeds the diagnostic source commit (with `-dirty` on modified
tracked sources). The branch is `diagnostics/linux-audio-drift-25`.

## Evidence recheck, 2026-11-15

[Issue #25](https://github.com/SaltedByte/TackleCast/issues/25) had no comments
or newer attachments at the recheck. Both attached logs select generic
`pipewire` input and output, F32, stereo, 44100 Hz. The clip's enumeration
includes the named capture-card ALSA input, but saved input index 0 still
selects `pipewire`. Its single logged underrun is at startup. The longer log
ends at 32 legacy underruns with approximately 60 FPS and no packet errors
in its final video stream.

The longer log also contains several video restarts (including resolution
changes) and audio restarts. Legacy underruns accumulate across those audio
streams, and warnings are throttled. Neither the 32 count nor the single
startup warning measures missing audio duration. These are clues, not a cause.

The reporter's executable/settings folder name matches the standard extracted
`TackleCast-Linux-x86_64` archive, in two different parent folders. Neither the
issue nor logs explicitly identifies the original downloaded file format. Use
the existing Linux tar.gz workflow, the closest evidenced standard format;
the parent folder named AppImages does not establish an AppImage package.

The original checkout was clean. Main was `62babb55d48c92f18033737792ff680c0b241e44`;
its differences from the tag were documentation and release-smoke setup only.
The tag is used to eliminate ambiguity about the reproduction base.

## Activation and privacy

Run the extracted archive with:

```sh
TACKLECAST_DIAGNOSTICS=1 ./run.sh
```

Only the exact value `1` enables telemetry. The existing launcher inherits the
environment and forwards arguments; no desktop-launcher edits or persistent
settings changes are required. Without this variable, telemetry is off.
Release logs are in `logs/` beside the extracted executable.

While enabled, the file logger accepts only the dedicated diagnostic target,
even if `RUST_LOG` is set. This suppresses legacy events that contain settings
paths and USB by-id serials. New events record routing device names and numeric
configuration, never raw media, USB serial IDs or settings/home paths. Video
uses its display label; an unresolved raw device ID is replaced with a fixed
label. Backend error strings are omitted. The normal off-mode logging filter
is retained. Logs retain the existing pruning policy; normal periodic volume
is two compact lines per two seconds, plus lifecycle events (a few MB/hour).

## Measurements

Audio counters restart at zero for each `id`. Startup/reinitialization events
record the build, version, CPAL host (ALSA on Linux; `pipewire` is an ALSA
compatibility route), selected input/output names, rates, channels, formats,
requested CPAL buffer settings and ring capacity. `Default` is a backend-selected
buffer setting, not a measured hardware period or latency. Hardware/server
buffer capacity and latency are unknown. No latency control exists in this base.
Ring capacity remains 192000 interleaved samples: 96000 frames for stereo,
about 2177 ms at 44100 Hz. No prefill or queue-size change is made.

`drift audio` reports `total` and `interval` arrays in this order:

| Index | Stage | Unit/rate |
| --- | --- | --- |
| 0 | CPAL capture callback received, before conversion | frames at `capture_hz` |
| 1 | After channel conversion and resampling, offered to ring | frames at `playback_hz` |
| 2 | Accepted by ring | frames at `playback_hz` |
| 3 | Requested by output through ring pops | frames at `playback_hz` |
| 4 | Real ring data delivered to output conversion | frames at `playback_hz` |
| 5 | Silence inserted for missing ring data | frames at `playback_hz` |
| 6 | Ring overflow drops | frames at `playback_hz` |

Frames mean one sample per channel. Internally, counters use interleaved
samples and convert only when reporting, preserving fractional frames when
the output's existing 2048-sample chunks split a multichannel frame. Real data
can itself contain silence; mute/gain does not turn real data into an underrun.
At quiescence: offered = accepted + dropped; requested = real + inserted silence;
accepted - real = fill (before the ring is discarded on stop).

`total_events` and `interval_events` contain counts of: all short ring pops
(partial or empty), legacy underruns (empty pops only), rejected ring pushes,
and backend stream errors. An event is not a missing frame and is not necessarily
one callback: output callbacks may perform multiple ring pops. Existing session
underrun/overrun counters are retained; per-stream counters clarify their scope.
Stop events separately record buffered samples discarded during reset.

Input callback entry and output callback entry each report
`(cumulative_callbacks, last_host_us, interval_max_gap_us)`. Error counts also
have their latest host timestamps. All host timestamps share the diagnostic
process's monotonic `Instant` origin (`t_us`); wall-clock log prefixes are for
readability. Intervals are actual elapsed time, not assumed to be exactly 2 s.
The summary thread continues during callback stalls. Ring current/min/max are
approximate concurrent observations after ring operations and at summary time;
the extrema reset each summary. Independent relaxed loads may straddle a
callback, so neighboring summaries can differ by a block and cross-counter
equations need not hold exactly during active streaming. No observation feeds
back into playback.

Video stage tuples are `(cumulative_frames, last_host_us, interval_max_gap_us)`:
FFmpeg video packet returned, decoded frame published to the triple buffer,
UI frame upload returned, and wgpu present call returned. These are independent
counts, not paired frames. Synthetic video has no packet-return stage. Video
open/negotiation/stop/error and settings-change events mark lifecycle boundaries.
Video cumulative counts span resets; audio counts use stream IDs.

All timing markers are **host-side proxies**. There are no collected hardware
audio timestamps or actual speaker/display presentation timestamps. CPAL's
backend timestamps and FFmpeg PTS are not assumed to share a clock; the FFmpeg
time base and its unknown-origin PTS domain are labeled at negotiation. Host
markers can locate delivery interruptions but cannot establish HDMI source A/V
alignment, physical output delay, an A/V content offset, or PipeWire's internal
resampling/clock behavior. No cross-domain A/V subtraction is reported.

The new callback bookkeeping is fixed atomic work and monotonic-clock reads:
no formatting, logging, allocation, locking, disk writes, channels or waits.
Legacy underrun/overrun warnings and backend error status reporting are moved
to UI housekeeping so they no longer lock/log from callbacks. Error payloads
are replaced by direction flags and per-stream counts/timestamps. Existing
conversion buffers and resampling code, including their preexisting potential
Vec growth, are unchanged. Device selection, audio/video defaults, buffer size,
underrun zero-fill, resampling, gain, playback and recovery are unchanged.

## Build and verification

On Ubuntu 24.04 x86_64 with Rust 1.93.1 and rustfmt/clippy:

```sh
sudo apt-get update
sudo apt-get install -y build-essential nasm libasound2-dev libclang-dev pkg-config \
  curl ca-certificates git python3 xvfb xauth libxkbcommon-x11-0 libgl1-mesa-dri mesa-vulkan-drivers
bash scripts/setup_linux_dependencies.sh
export PKG_CONFIG_PATH="$PWD/.deps/ffmpeg/lib/pkgconfig"
export LD_LIBRARY_PATH="$PWD/.deps/ffmpeg/lib"
cargo fmt --all --check
git diff --check
python3 -m py_compile scripts/verify_package.py scripts/smoke_test.py scripts/verify_drift.py
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-default-features
cargo clippy --locked --all-targets --no-default-features -- -D warnings
cargo build --release --locked --no-default-features
# Commit the diagnostic source before packaging, so embedded identity is clean.
bash scripts/package_linux_release.sh
CHECK_DIR="$(mktemp -d)"
tar -xzf dist/TackleCast-Linux-x86_64.tar.gz -C "$CHECK_DIR"
python3 scripts/verify_package.py --package "$CHECK_DIR/TackleCast-Linux-x86_64" --commit "$(git rev-parse HEAD)"
env -u TACKLECAST_DIAGNOSTICS xvfb-run -a python3 scripts/smoke_test.py --binary "$CHECK_DIR/TackleCast-Linux-x86_64/tacklecast"
xvfb-run -a python3 scripts/verify_drift.py --package "$CHECK_DIR/TackleCast-Linux-x86_64" --render
cp dist/TackleCast-Linux-x86_64.tar.gz dist/TackleCast-Linux-x86_64-issue25.tar.gz
cd dist
sha256sum TackleCast-Linux-x86_64-issue25.tar.gz > TackleCast-Linux-x86_64-issue25.tar.gz.sha256
```

The existing release workflow resolves a version tag. Do not dispatch it with
`v2.2.0` to build this branch: that would build the original tag. The commands
above build the local diagnostic branch without publishing, pushing or tagging.

The extra verifier invokes the **extracted `run.sh`**, from an unrelated working
directory without development library paths, with diagnostics off and on. It
checks build identity, privacy filtering, periodic output and final conservation
equations. Its explicit `--diagnostic-audio-fixture` CLI exercises the production
ring/resampler with seeded steady flow, sustained excess capture delivery,
forced capture interruption, and 44100-to-48000 conversion. Fixture metadata
labels its accelerated host schedule and lack of CPAL. These are synthetic
distinguishability checks, not capture-card or operating-system validation.
The optional `--render` check also launches synthetic NV12 video through run.sh
and requires positive publish/upload/present proxy counts.

## Interpretation

Compare within an uninterrupted audio stream ID and across many intervals.
Normalize capture and playback frames by their own rates before comparing
durations. A steady fill trend with diverging normalized delivery/request rates
points toward clock/rate mismatch. Sharp fill drops, larger callback gaps,
silence bursts, backend errors or stream resets around the symptom point toward
interruptions. Neither pattern is conclusive by itself. Healthy app-side
counters also cannot rule out drift upstream/downstream inside generic PipeWire.

Retest once with the original generic PipeWire input and all other settings
unchanged. Collect the log and the elapsed time/direction of the first symptom;
do not combine this evidence run with device switching or tuning.
