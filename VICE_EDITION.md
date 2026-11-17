# TackleCast VI — local edition notes (draft)

This is a local, unannounced edition. The application/package version remains
**2.3.0**; “VI” is display branding, not a version tag. No publication or release
is implied by this document.

## Interface

- Vice is the default theme: deep aubergine surfaces, warm text, peach/pink
  accents, and an original coastal header confined to the settings menu.
  The full-bleed header scales to the image's original aspect ratio so the entire
  banner stays visible. Its height adapts to the menu width; settings scroll below.
- Display → Theme previews Vice or Classic immediately. Escape or clicking the
  backdrop saves the draft, including the theme. Classic retains the original
  UI palette; the runtime window icon follows the selected theme.
- Video, Picture, Audio, and Display remain the four tabs. The settings list uses
  roomier aligned label/control rows, bounded library controls and simple
  section headings. Every page shares fixed columns; Picture sliders and numeric
  fields also share their own columns. There are no per-setting subtitles.
  Content scrolls inside a fixed-size opaque menu; picture sliders retain the undimmed
  live-image preview. Apply & close or Escape saves the selection.
- Volume and mute preview immediately while settings stay open. Audio controls
  keep the image dimmed; only picture/video adjustments reveal the live image.
- Theme and performance-pill changes do not require a capture restart.
- Reinitialize devices replaces the separate capture/audio restart actions.
  It refreshes device lists, pairing and capabilities, then reopens the selected
  routes while preserving the settings window. Windows warns when it measures
  a USB 2-or-slower connection that may restrict available capture modes.
- Automatic capture selection pairs video friendly names with capture audio
  inputs; it never uses generic name tokens to choose an unrelated camera or
  microphone. Explicit selections are preserved. No unique match prompts manual
  selection in Video and Audio. Playback output remains separate.

## Performance pill

Display → On-screen overlay contains the master switch, four-corner position,
and a collapsible **Customize statistics** editor. Drag the visible handle or
use each row’s up/down buttons to reorder the whole statistic with its checkbox
state intact. Changes preview while the menu is open and save on close.

Available values are resolution, measured delivered FPS, target/nominal FPS,
active capture pixel format, actual scaling-filter label (including renderer
fallbacks), color space, and color range. Fractional nominal rates retain their
exact ratio alongside their decimal display. No latency or decode-duration
measurements are claimed. The compact default enables resolution, delivered
FPS, target/nominal FPS, and pixel format. Scaling and color details are optional.

Turning every statistic off hides the pill. Turning the master off does the
same without losing selections. Capture/reconnection status remains separate
and visible regardless of these choices; errors suppress stale performance data.

## Saved-settings compatibility

`theme` stores `vice` or `classic`. `show_overlay` remains the master switch.
`overlay` stores a `corner` and an ordered `stats` list, with a statistic ID and
an `enabled` flag for each row. On load, duplicate IDs retain the first entry;
missing IDs are appended disabled. An explicitly empty list means all off.

For old files without `overlay`, the old compact preference becomes the new
compact default; `detailed_overlay: true` enables all seven statistics. The old
master switch is preserved. Once migrated, the explicit list takes precedence
and the legacy detail flag is no longer edited by the menu.

## Synthetic UI screenshot previews

Startup controls are honored **only** alongside an existing synthetic test flag
(`--test`, `--test-alt`, `--test-nv12`, `--test-yuvj422p`, or `--test-mjpeg`):

- `--test-ui=video|picture|audio|display` opens the corresponding settings tab.
- `--test-ui=overlay` opens Display, expands Customize statistics once, and
  scrolls the editor into view. All seven rows fit at the default 1280×720 size.
- `--test-ui=none` leaves gameplay visible. Omitting the page does the same.
- `--test-theme=classic|vice` optionally overrides the theme in memory.

A valid preview selection disables settings writes for the entire process,
including menu apply and normal exit. It skips device enumeration/capability
queries and opens no real capture or audio streams; device lists remain empty,
not fabricated. Frames and statistics still come from the existing scrolling
synthetic test-pattern capture and actual renderer. Without preview flags,
existing startup/test behavior is unchanged. Invalid values are ignored; the
last occurrence of each option wins. Flags are case-insensitive.

Example (prefer an optimized binary for meaningful synthetic delivered FPS):

```sh
tacklecast --test-nv12 --test-ui=overlay --test-theme=vice
```

Runtime theme changes update both the small window icon and large taskbar icon
on Windows. Window-icon APIs are a no-op under Wayland; X11/window-manager icon
presentation remains desktop-dependent.

## Validation boundaries

Settings/migration/normalization, row moves, formatting, recovery-status
separation, icon decoding, headless small-screen layout, and actual egui
checkbox/arrow/drag events have automated coverage. Synthetic rendering and
screenshots are separate from physical capture-card validation. Windows icon
behavior, real device negotiation/recovery, GPU-specific filter fallbacks, and
long hardware sessions still need their platform/hardware checks from BUILD.md.
