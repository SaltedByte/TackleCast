# TackleCast FrameGen (my research and tries)

This is TackleCast 2.3.0 with frame generation built in. I play on a PS5 Pro through a capture card, and most console games run at 30 FPS. I was using Lossless Scaling on top of TackleCast to get 60, but I wanted it inside the app itself, so this is what came out of a lot of evenings of trying stuff.

It doubles the frame rate (30 -> 60). It's off by default; everything else works the same as normal TackleCast.

My setup, so you know what it was actually tested on:

- PS5 Pro -> Unitek SAV2503 capture card (2560x1440 @ 30, NV12)
- RTX 5080, Windows 11
- Samsung Odyssey G6 OLED, 240 Hz
- Red Dead Redemption 2 for basically all the testing

I haven't tried other GPUs or other games much, so no promises there.

## How to use it

1. Unzip the release somewhere and run `TackleCast-FrameGen.exe`. Keep the `rife` folder next to the exe.
2. Open the **Frame Gen** tab and tick Enable Frame Generation. The defaults are what I use:
   - Backend: RIFE (DirectML)
   - RIFE model: RIFE 4.26, half-res motion
   - Mode: 2x
   - Output target: 60 FPS
   - Keep HUD steady: on
   - Artifact reduction: Off
3. The overlay (top right) shows what it's doing: captured / generated / displayed FPS, how long each generated frame takes, and the added latency.

If something looks wrong, there's a **Record diagnostic clip (10 s)** button. It saves the raw captured frames so they can be replayed later and you can compare settings on the exact same footage (see "Replays" below).

## What I tried, in order

### 1. NVIDIA FRUC (Optical Flow SDK)

My first try was NVIDIA's FRUC from the Optical Flow SDK, since it runs on the optical flow hardware. It works, but in RDR2 the HUD was tearing and doubling, there was a band at the screen edge when turning the camera, and the sky got blocky. I spent a long time writing correction passes for it (fixed HUD zones first, then detecting artifacts dynamically). It got better but never good. When I replayed recordings through NVIDIA's own sample, the artifacts were the same, so it's just how FRUC behaves on this kind of content.

FRUC is still in the app as a backend if you want to try it, but you need your own copy of the Optical Flow SDK 5.0.7. NVIDIA's DLLs can't be redistributed, so they're not included. The app looks for them in an `Optical_Flow_SDK_5.0.7` folder next to the app's folder (or a level or two up), or in the folder set in `TACKLECAST_FRUC_DIR`. Artifact reduction was made for FRUC; with RIFE leave it off.

### 2. RIFE on DirectML

Next I tried RIFE (Practical-RIFE by hzwer), exported to ONNX and running through ONNX Runtime + DirectML. It runs on the same D3D12 device as the renderer, so the frames never leave the GPU. The HUD and minimap looked way better than with FRUC straight away.

Problems I hit after that, and what fixed them:

**Distant landscape shimmering, edges looking "filtered".** RIFE 4.6's blend mask has a 2-pixel checkerboard pattern, so every generated frame had a fine mesh on it. Real frame, mesh frame, real frame... at 60 FPS that looks like shimmer. The fix was to stop using the network's output image. The models now output only the motion and the blend mask, and TackleCast builds the in-between frame itself (upscale the motion, sample both real frames along it, blend). The mesh is completely gone.

**Crosshair flickering.** When the camera turns, the interpolator moves every pixel together with the background, including the crosshair, so it gets dragged or half disappears in the generated frame. "Keep HUD steady" looks for small elements that stay in place while everything around them moves (only during camera turns and only around the screen center / minimap). It paints them out of what RIFE sees and draws the real ones back afterwards. It also remembers the crosshair between turns, so it doesn't flicker when the background behind it changes from sky to trees.

**Gun barrel and hat going see-through in fast turns.** Thin things that move differently from the background behind them (the rifle on Arthur's back, the hat brim) turned into ghosts. I tried the newer RIFE versions (4.25, 4.26, the heavy ones) at full 1440p, but on DirectML they took 50-135 ms per frame, way too slow for 30 -> 60. What actually worked was letting the network estimate motion on a half-resolution copy of the frames (1280x720). The barrel and hat stay solid, and the final frame is still built from the full-res frames, so you don't lose sharpness. It's also a lot faster.

**Sound coming late.** The audio buffer could build up a big backlog (in my log it was 848 ms at one point) and it never caught up, so you'd shoot and hear it way too late. Now it drops the backlog whenever more than 100 ms builds up. Note that Bluetooth headphones add their own delay on top, a cable is better.

## Models

All measured on my RTX 5080 at 1440p:

| Model | Time per generated frame | Notes |
|---|---|---|
| RIFE 4.26, half-res motion | ~10 ms | the one I use, best on thin objects |
| RIFE 4.6, half-res motion | ~5 ms | fastest, slightly worse on thin stuff |
| RIFE 4.6, full-res motion | ~18 ms | the older version, loses thin objects in fast turns |

Models are included for 2560x1440, 1920x1080 and 1280x720 capture. `scripts/rife/export_models.py` makes them from the Practical-RIFE weights if you want to rebuild them.

## Known issues

- Generated frames are a bit softer than real ones in motion. That's normal for interpolation, but you might notice it.
- In very fast turns a thin light line can show up for one frame at the top edge of the hat against the sky.
- Super fast camera pans can still look a bit doubled sometimes.
- Only 2x. RIFE needs Windows and DirectX 12.
- Added latency is roughly half a source frame (~17 ms at 30 FPS) plus the processing time; the overlay shows the real number. With the old full-res model it was around 38 ms for me.

## Replays

The diagnostic clips can be replayed offline with any engine/model, which is how I compared everything:

```
TackleCast-FrameGen.exe --fg-replay=recordings\clip_XXXX --fg-engine=rife --fg-rife-model=rife426fmh --fg-replay-modes=raw,hud --fg-replay-every=6 --fg-replay-out=replay_out
```

It writes a `summary.txt` with timings and PNGs of the real frames and the generated frame in between, so you can look at artifacts frame by frame.

## Building

Same as normal TackleCast (see BUILD.md). The frame generation code is in `src/framegen/`, `src/render_framegen.rs` and `src/render_framegen_fix.rs`. The ONNX Runtime and DirectML DLLs are loaded at runtime from the `rife` folder, so take that folder from the release zip. The exe in my release was cross-compiled with MinGW.

## Credits and licenses

- TackleCast and everyone who worked on it
- RIFE / Practical-RIFE by hzwer (Zhewei Huang), MIT
- vs-rife by HolyWu (network code used for the export), MIT
- ONNX Runtime (Microsoft), MIT
- DirectML (Microsoft), redistributed under its license
- FFmpeg, LGPL

License texts are in `licenses/` and `rife/licenses/` in the release.
