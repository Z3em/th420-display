# TH420 Display work queue

This file tracks work that needs to be reworked, implemented, or reimplemented.
Keep each task in exactly one state:

- `identified` — found by the AI; awaiting user confirmation or reprioritisation.
- `confirmed` — approved by the user and ready to start.
- `planned` — confirmed by the user, with an explicitly approved implementation
  scenario; awaiting scheduling or start.
- `in progress` — actively being implemented.
- `complete` — implemented and validated at the appropriate boundary (tests, GUI, or hardware).
- `verified` — a completed task whose result has been explicitly tested or
  reviewed and accepted by the user.
- `denied` — identified by the AI and explicitly rejected by the user; retain it
  as a record rather than re-adding it as new work.

Move a task between sections instead of duplicating it. Keep a short validation note
when moving work to `complete`.

## Identified

- [ ] **Synchronize window and device animation playback** — coordinate animation
  timing between the GUI preview and the physical display for both boot preview
  and ordinary live mode, avoiding independent starts and accumulated drift.
  Implementation details, clock/handshake strategy, acceptable hardware latency,
  resynchronization behavior, and failure handling are TBD.

- [ ] **Add GUI/config parity coverage** — test that every supported runtime TOML
  setting has an intentional GUI control or an explicitly documented reason to be
  configuration-only.

## Confirmed

- [ ] **Replace duplicate Live Display background controls with presets** — the
  Background editor currently exposes legacy Fit and Zoom controls in addition
  to the unified transform controls. Remove the old duplicates and provide quick
  Fit/Contain, Cover, Stretch, and any other useful presentation presets instead.
  A preset updates the canonical shared fit/transform state; the unified Zoom,
  Stretch, Pan, and Rotation controls remain available for fine adjustment after
  applying it.

- [ ] **Maintain `gui_v2` as the active GUI reimplementation** — `th420-config`
  builds from `src/gui_v2.rs`. Retain `src/gui_overhaul.rs` for comparison until
  `gui_v2` has reimplemented every required capability; do not merge the two UIs
  blindly or remove the old implementation prematurely.

- [ ] **Reimplement widgets as templates and instances** — define reusable widget
  templates, then permit multiple independently configured instances of each
  template. The Live Display tab provides one Add widget control backed by a
  template dropdown (or equivalent), plus a list of existing instances for
  selection, configuration, reordering, and removal. Rework layout, placement,
  typography, label/value colours, and thresholds within this instance model.

## Planned

- [ ] **Apply shared transforms to widget instances** — adapt each future widget
  instance to the common transform and drag/snap contract while keeping its state
  independent from its template and sibling instances. Transform around measured
  widget bounds, retain the `Drag`/`Preview` readout and exact numeric controls,
  and keep widget anchoring, clipping, visibility, and selection handles outside
  the shared geometry engine. This task depends on the widget template/instance
  model being implemented.

- [ ] **Close the remaining cross-target transform validation gaps** — existing
  coverage already exercises shared background/standby/boot geometry, independent
  target state, animation cadence, and upload gating. Add the missing focused
  cases for rotation pivot/clipping, canvas fill and cache invalidation, trimmed
  boot timelines/timing normalization, and optional FFmpeg behavior. Add widget
  adapter equivalence only after the template/instance model exists. Finish with
  a visual GUI check; any persistent device upload remains a separate test that
  requires explicit user approval.

<!-- Move planned tasks here only after their implementation scenario is approved. -->

## In progress

- [ ] **Add boot-animation editing controls** — add source-range trimming, a
  timeline scrubber, and explicit uniform delay/FPS controls. The scrubber must
  preview the transformed frame at the selected source time without changing
  committed playback timing. Trimming and timing changes must feed the same
  debounced container inspection used by upload, retain the minimum 80 ms delay,
  255-frame and 10 MiB limits, and never perform a persistent device write until
  the user explicitly presses Upload.

## Complete

- [x] **Stabilize preview helper and daemon lifecycles** — switching between
  Boot and Standby device previews now stops and reaps the old helper without
  restarting the live daemon between modes. Directly launched daemons are reaped,
  daemon stop/start results are checked before device operations continue, and
  failed preview exits are reported. Boot-container inspection is serialized so
  rapid transform edits cannot accumulate CPU-heavy `th420-display` helpers.
  Device choices are now Off, Boot (loop), Boot (once), and Standby; loop/once
  semantics apply only to boot animation. Validated by the GUI-enabled tests,
  Clippy, format/diff checks, process inspection, and release builds.

- [x] **Restore animated live-display cadence** — animated background rendering
  is independent of the default 800 ms sensor interval. GIF source delays are
  retained and video/device output is capped at the hardware-tested 24 FPS,
  while sensors and coolant telemetry remain on the configured slower cadence.
  Validated by timing regression tests, the full GUI-enabled suite, Clippy,
  format/diff checks, and release builds; device behavior awaits user acceptance.

- [x] **Complete transformed boot preview and upload preparation** — one
  fit/pan/zoom/stretch/rotation/canvas transform is applied consistently to every
  decoded boot frame, desktop animation preview, transient device preview,
  container inspection, and explicit upload. Signed transform values are accepted
  by the helper CLI. Inspection validates non-empty input, uniform delays of at
  least 80 ms, at most 255 frames, and the 10 MiB container limit before enabling
  Upload. Live daemon ownership is released and restored around device work.
  Validated by CLI/parser, container, transform, preview-mode, and GUI tests plus
  release builds; no persistent device write was performed during validation.

- [x] **Finish transformed standby-media preparation** — static and animated
  sources expose an exact-frame timeline; metadata probing and frame extraction
  run in debounced workers with stale-result rejection, so FFmpeg never blocks
  GUI refresh. The cached selected frame drives transformed desktop preview,
  transient device preview, and explicit JPEG upload with identical timestamp
  and geometry. Upload stays disabled until that frame is ready and retains the
  live-daemon stop/restore boundary. Validated by 142 tests, Clippy, format and
  diff checks; no persistent device write was performed.

- [x] **Build the shared animated-media decoder and live compositor** — static
  media uses `image`, GIF timing is retained in-process, and other supported
  formats use one real-time-paced FFmpeg MJPEG pipe per selected source. Static
  and current animated frames are cached; frame changes invalidate transform
  geometry and are composited with widgets before final device orientation.
  Stream backgrounds now use the same renderer and preserve the old source on a
  non-fatal decoder error. Validated by frame-timing, cache, MJPEG-pipe, renderer,
  and GUI tests; no persistent write path is involved.

- [x] **Show the encoded boot-container size before upload** — the GUI now
  asynchronously prepares the transformed boot container through a read-only CLI
  inspection mode, displays encoded size, limit, frame count, and delay, and
  disables upload while validation is pending, invalid, or over 10 MiB. Transform
  changes are debounced so dragging remains responsive. Inspection never opens
  the device. Validated by parser/exclusivity tests and the 134-test GUI suite;
  no device write was performed.

- [x] **Implement configurable live-background transforms** — persist fit,
  pan, zoom, independent stretch, background-only rotation, opacity, darken,
  blur, and canvas colour with migration from legacy normalized offsets. The
  renderer applies background transforms before widgets and final device
  orientation. The Background tab provides direct drag and wheel interaction,
  snapping, exact numeric controls, Reset, and the retained raw `Drag`/snapped
  `Preview` coordinate readout. Validated by config and renderer tests plus the
  GUI-enabled suite and release build.

- [x] **Extract universal transform and drag/snap foundations** — shared
  `Transform2D`, media geometry, and portable drag state now drive backgrounds
  plus independent Boot and Standby editors. Desktop preview, transient device
  preview, and explicit upload commands receive identical fit, pan, zoom/stretch,
  rotation, and canvas values; boot applies one transform to every encoded frame.
  Preview sources are cached and target states remain isolated. Validated by 133
  automated tests, Clippy, diff/format checks, and both release builds; no device
  upload was performed.

- [x] **Raise the boot-animation size limit to 10 MiB** — encoded boot containers
  are rejected above `10 * 1024 * 1024` bytes before the device is opened. The
  error text, README, and protocol documentation identify the reverse-engineered
  official Windows application as the source of this limit rather than claiming
  firmware validation. Validated by the GUI-enabled test suite and release build;
  no persistent device write was performed.

<!-- Move finished tasks here and add the validation performed. -->

## Verified

- [x] **Prevent conflicting GUI and daemon instances** — `th420-config` and the
  continuous `th420-display` daemon use separate per-user advisory locks and
  local graceful-shutdown sockets. Both binaries accept
  `--replace-existing graceful|term|kill`, with verified process identity and a
  strict escalation ceiling. A separate device lock serializes HID ownership
  across the daemon and one-shot helpers. Automated validation covered 152 tests,
  formatting, diff checks, release builds, and both CLI surfaces; the user
  subsequently tested the behavior and confirmed it works correctly.

<!-- Move completed tasks here only after explicit user verification. Record
what the user verified when useful. -->

## Denied

<!-- Move explicitly rejected AI-identified tasks here, including the reason when useful. -->
