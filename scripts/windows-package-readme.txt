TackleCast v3.VI - private integrated Windows test candidate
=======================================================

Extract the entire ZIP to a writable folder and run TackleCast.exe. Keep the
DLLs, rife folder and other files together. Windows 10/11 x64 and a compatible
graphics driver are required. No Python, CUDA toolkit or model download is needed.

Escape opens settings and saves/closes the menu. F11 toggles fullscreen.
Video and Audio choose capture/playback devices. Reinitialize devices refreshes
device discovery after changing a card, cable or USB port. Picture adjusts the
live image. Audio volume/mute apply immediately while settings remain open.
Display selects Vice/Classic and Hidden, Compact or Custom overlay mode.
Compact shows resolution and FPS, plus source-to-presented FPS and FG state when
interpolating. Only Custom exposes the statistics/position editor. Switching
modes retains its custom selections. Capture FPS remains independent of FG FPS.

Experimental frame generation
-----------------------------
Frame generation starts OFF. In Frame Gen, enable it and choose a model, then
Apply & close. RIFE 4.26 and the faster RIFE 4.6 half-resolution motion models
are included for 1280x720, 1920x1080 and 2560x1440 capture. These estimate motion
at half resolution and compose the output at capture resolution.

The 60 FPS target is intended for 30/29.97 FPS game content. Always 2x also
allows 60-to-120 operation when GPU and display performance permit. One frame
is generated between adjacent unique source frames. This adds latency and may
produce visual artifacts. Scheduling delay is an internal estimate, not a
measurement of total HDMI-to-screen latency. Presented FPS is an application
counter, not proof of the panel's physical refresh rate.

This bundle does not include 4K models, RIFE 4.25 lite, full-resolution-motion
models, TensorRT, or the NVIDIA Optical Flow SDK. Normal capture remains usable
when FG is disabled or unavailable. Choose a bundled capture resolution if the
selected model reports an unsupported size. GPU performance varies by hardware.

Settings are saved beside the executable. Logs are in logs/. Neither saved
settings nor developer logs are included in the ZIP. Prior candidates remain
separate; copy your settings manually only if you want to retain those choices.
This is a local test candidate, not a published release.
