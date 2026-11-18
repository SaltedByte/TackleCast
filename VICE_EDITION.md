# TackleCast v3.VI — local edition notes (draft)

This is a local, unannounced edition. The application/package version is
**3.0.0**, branded **v3.VI**. No publication or release
is implied by this document.

## Interface

- Vice is the default theme: deep aubergine surfaces, warm text, peach/pink
  accents, and a GTA VI skyline screenshot supplied by the owner, confined to the settings menu.
  The full-bleed header trims excess sky and foreground water while retaining
  the full skyline width at the existing 4:1 banner ratio. Its height adapts to the menu width; settings scroll below.
- Display → Theme previews Vice or Classic immediately. Escape or clicking the
  backdrop saves the draft, including the theme. Classic retains the original
  UI palette; the runtime window icon follows the selected theme.
- Video, Picture, Audio, Frame Gen, and Display share the themed settings menu. The settings list uses
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

## Performance overlay

Display offers three modes: Hidden, Compact and Custom. New installations
start in Compact. Hidden suppresses performance statistics; capture/reconnection
errors remain visible independently.

Compact is a fixed top-left preset, with no statistics/position editor:
`2560×1440 · 30 → 60 FPS · FG 2×` while interpolation is active, or
`2560×1440 · 60 FPS` while off/paused. The arrow uses unique source FPS and
application presented FPS, not transport FPS or measured panel refresh.

Only Custom exposes the position selector and collapsible statistics editor.
Resolution has no prefix. Capture/requested/unique/generated/presented rates use
Cap/Req/Src/Gen/Out. Whole values omit decimal zeroes; fractional values retain
up to two decimals. Requested rates are stored rationally without duplicating
that ratio in the overlay. Fields retain their selected order, share one readable
font size and wrap to available width, without forced two-field rows.

Custom supports resolution, capture/requested rates, format, filter, color space,
range, FG state and source/generated/presented rates. Drag the handle or use
Up/Down to reorder rows with their checkbox state. All-off hides Custom's pill.
Switching modes leaves the saved custom selections/order/corner intact.

## Saved-settings compatibility

`theme` stores vice/classic. `overlay_mode` stores hidden/compact/custom.
The old `show_overlay` flag is retained for compatibility and synchronized on
save; older files without a mode migrate to Hidden when their master was off,
or Custom when it was on, retaining their previous layout. `overlay` retains its
corner and ordered stats list. Duplicate IDs keep their first entry; missing
statistics append disabled. An explicitly empty custom list remains all off.

For old files without `overlay`, the old compact preference becomes the new
compact default; `detailed_overlay: true` enables all seven statistics. The old
master switch is preserved. Once migrated, the explicit list takes precedence
and the legacy detail flag is no longer edited by the menu.

## Synthetic UI screenshot previews

Startup controls are honored **only** alongside an existing synthetic test flag
(`--test`, `--test-alt`, `--test-nv12`, `--test-yuvj422p`, or `--test-mjpeg`):

- `--test-ui=video|picture|audio|display` opens the corresponding settings tab.
- `--test-ui=overlay` opens Display, expands Customize statistics once, and
  scrolls the editor into view. The statistics editor scrolls to the additional frame-generation rows.
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

## Experimental frame generation integration

Frame Gen adds the optional2x RIFE backend from Wiktor's PR28, off by default,
with4.26 and4.6 half-resolution-motion choices bundled for720p/1080p/1440p.
The controls use the same shared columns, switches and selectors as VI's other
pages, with no per-setting subtitles. It does not change the theme/banner,
audio preview behavior or zero-capture-frame redraw scheduling.

The performance pill now also offers frame-generation state, unique source FPS,
generated FPS and presented FPS. These rows can be enabled, reordered and placed
using the existing Display editor; new choices are disabled during migration.
Delivered capture FPS retains its original meaning. Presented counters do not
measure panel refresh or absolute latency. Capture errors still suppress stale
statistics and remain visible when the overlay master is off.

Local integration candidates are private and require owner testing before any
GitHub publication. Previous candidates and source branches remain separate.
