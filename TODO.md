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

The validation notes under older completed tasks describe the checks performed
at that time; their test counts are not current suite totals. `Complete` does not
imply user verification or a physical-device test unless the note says so.

## Identified

- [ ] **Synchronize window and device animation playback** — coordinate animation
  timing between the GUI preview and the physical display for both boot preview
  and ordinary live mode, avoiding independent starts and accumulated drift.
  Implementation details, clock/handshake strategy, acceptable hardware latency,
  resynchronization behavior, and failure handling are TBD.

- [ ] **Add GUI/config parity coverage** — test that every supported runtime TOML
  setting has an intentional GUI control or an explicitly documented reason to be
  configuration-only.

- [ ] **Decide transient Live playback parity** — `gui_overhaul` exposed finite
  GIF playback and frame-sequence streaming with loop/FPS controls. `gui_v2`
  supports animated/streamed backgrounds, but does not yet expose the same
  finite playback workflow. The user is unsure whether this is still needed;
  decide whether to restore it or explicitly supersede it before closing the
  GUI parity audit.

- [ ] **Decide desktop integration workflow** — the old `gui.rs` installed a
  desktop entry and icon on launch. Decide whether `gui_v2` should offer a
  separate, explicit install/update workflow, leave integration to packaging,
  or intentionally omit it. Do not silently write desktop files on startup.

## Confirmed

## Planned

## In progress

Automated tests do not establish GUI or hardware acceptance.

- [ ] **Correct coolant telemetry and verify it against the standby readout** —
  replace the inferred `0x80` temperature decoding with the same-device
  FanControl plugin's `0x82` query and little-endian centidegrees. Keep the pump
  query separate. Failed reads invalidate daemon telemetry and remove coolant
  from live rendering; GUI errors/stale responses clear displayed values.
  Parser and simulated control-exchange tests cover changing temperatures,
  including 31 °C, invalid replies, and telemetry recovery. Compare the rebuilt
  daemon and GUI with the built-in standby readout at multiple temperatures
  before marking hardware verification complete. Broader validation of the
  existing pump-speed interpretation also remains pending.
  Validation: all 265 daemon/GUI tests pass; both release binaries rebuilt.
  Broad hardware accuracy verification is pending. The user will compare the
  software and built-in standby readout once the temperature changes
  significantly; keep this task out of Verified until that comparison.
  Follow-up: the user reports 30.9 °C in Overview, widget 31 °C, and standby
  30 °C. The widget difference is explained by rounding to the nearest integer;
  standby is consistent with truncating the fraction in this observation.
  Preserve centidegree precision through telemetry, CLI status, Overview, and
  diagnostics to distinguish integer rounding from a persistent discrepancy.
  Widget temperatures currently round to the nearest integer; the standby
  readout's handling of fractional temperatures has not been established.
  Host follow-up: a fresh rebuilt `--status` query returned 30.86 °C and 2320 RPM
  while the user reported standby 30 °C. This confirms the `0x82` response
  prefix works on this device and is consistent with standby truncation. The
  running GUI was still the replaced 19:10 binary; no daemon was running, so
  its Overview value was a retained one-time snapshot. Multiple-temperature
  comparisons remain pending; do not introduce a temperature offset.

- [ ] **Maintain `gui_v2` as the active GUI reimplementation** — `th420-config`
  builds from `src/gui_v2.rs`. Both `src/gui.rs` and `src/gui_overhaul.rs` are
  non-functional artifacts of the old GUI; retain them for comparison until
  `gui_v2` has reimplemented every required capability. Do not merge the old
  UIs blindly or remove their source prematurely.

  Inventory every user workflow in `gui.rs` and `gui_overhaul.rs`, every
  supported runtime config field, and relevant CLI-only device operations.
  Record the equivalent
  `gui_v2` control or mark the old workflow as intentionally superseded, with a
  reason and user-visible replacement. Examine sensor-source editing,
  Undo/Redo, Profiles, Diagnostics, persistent-media controls, and service
  management explicitly; do not assume the new widget editor replaces them.
  Create separate tasks for genuine gaps and add the GUI/config parity check
  already tracked in Identified. Close this umbrella task only when required
  gaps have been implemented and the checklist has been reviewed. Keep the
  old GUI sources until that point.

  Progress: `gui_v2` now has source label/unit editing, Profiles under Settings,
  Diagnostics,
  Undo/Redo, fine display rotation, and daemon restart. Widget instance order
  supersedes the old layout/sensor-order workflow; template/instance colors
  supersede sensor-specific colors. Still needed: a documented old-GUI/config/
  CLI workflow inventory, disposition of the transient Live playback workflow
  (tracked in Identified), and interactive validation of the new controls.

## Complete

- [x] **Restore standby coolant-text controls in `gui_v2`** — Standby now has
  explicit Keep unchanged / Show / Hide visibility, a separate Apply visibility
  action, and optional text color committed only by an explicit standby upload.
  The device exposes no readback for these settings, so the GUI never presents
  a guessed state as current. The streamed preview does not simulate firmware
  coolant text. Upload/apply controls are disabled while device preview owns
  the device. Tests cover omitted defaults and exact color/visibility CLI
  arguments; physical-device behavior awaits user verification.

- [x] **Add media file drag-and-drop to `gui_v2`** — one local file dropped on
  Live Background, Boot, or Standby selects that editor's media via the same
  path as Browse. Boot accepts GIF; Live/Standby accept image or FFmpeg-backed
  media. Unsupported pages and multi-file drops give explicit errors instead
  of changing another tab. Tests cover target routing and file validation;
  interactive drag-and-drop awaits user verification.

- [x] **Move Profiles into Settings and preview unsaved Live Display edits on
  device** — Profiles now lives in Settings. The global device-preview switch
  streams the current working configuration on Live Display, follows the Boot
  or Standby tab for media preview, and remains enabled but idle on unrelated
  pages. Live preview writes only an atomically replaced private temporary
  snapshot, bypasses the daemon instance lock while retaining the device lock,
  and preserves the daemon pause/resume handoff. Tests cover page routing,
  transient CLI exclusivity, snapshot updates without altering committed
  config, and the existing Boot/Standby preview paths. GUI and daemon suites,
  formatting, non-strict Clippy, and release build passed; interactive GUI and
  device behavior await user verification.

- [x] **Stop automatic offline telemetry helpers and coordinate live startup** —
  automatic coolant/pump updates now use the running daemon's control socket
  only. With the daemon off, the GUI labels readings as non-live and offers an
  explicit one-time snapshot using a bounded `--status` helper. Starting Live
  Display waits for an in-flight snapshot to finish or time out; the daemon is
  considered started only after its control state reports `running`, not merely
  after publishing its instance lock. Coolant is injected into live widgets
  only from fresh daemon telemetry. Any visible widget with an unavailable or
  invalid reading now shows `--` and remains selectable; the old `show_missing`
  setting no longer silently hides it. Automated tests cover offline poll suppression, snapshot
  timeout, startup readiness, placeholder rendering, and selection. The full
  138-test GUI binary and 122-test daemon binary suites, formatting, non-strict
  Clippy, and release builds passed; GUI/device behavior awaits user testing.

- [x] **Improve color editing across the GUI** — one larger swatch, visual RGB
  picker, and editable `#RRGGBB` field serves widget labels, thresholds,
  backgrounds, and Live/Boot/Standby canvas colors. Per-control drafts leave
  invalid or incomplete input uncommitted; template overrides and a separate
  0–100% transparency control remain available. Validated by hex formatting/
  parsing, draft/commit, independent-color, inheritance/serialization, and
  opacity endpoint/round-trip tests. Interactive popup, keyboard, and preview
  appearance await user verification.

- [x] **Select transformed widgets by their actual box** — shared rotated box
  geometry drives the outline and inverse-transform hit testing. Selection is
  topmost-first, ignores hidden/unrendered instances, and keeps an active drag
  after the pointer leaves the box. Tests cover rotated empty corners,
  0/90/180-degree angles, fractional centers, overlapping siblings,
  preview-scaled pointer tolerance, and drag continuity. Interactive selection
  appearance awaits user verification.

- [x] **Bound and align Live, Boot, and Standby media placement** — shared
  source-footprint geometry constrains relaxed and strict pan, with optional
  edge-before-grid snapping and Center image on each media target. Boot uses
  the intersection of all frame dimensions. GUI drag/numeric controls, Apply,
  device preview, and explicit upload validate against the same placement
  rules and pass the same transform arguments. Tests cover exact contact versus
  one-pixel gaps, all sides and corners, rotated source-edge snapping,
  impossible strict coverage, multi-frame intersection, and shared preview/
  upload arguments. Optional edge/coverage preferences are intentionally
  GUI-only; direct CLI transforms stay unconstrained for scripts, as documented
  in README. Desktop appearance and physical-device behavior await user
  verification; no persistent device write was performed.

  Final validation for these three tasks: all 132 GUI-binary and 120 daemon-
  binary tests passed with the GUI feature enabled; formatting, non-strict
  Clippy, diff checks, and release builds of both binaries passed.

- [x] **Simplify widget sizing and keep geometry stable** — widget templates
  define a fixed local width/height; instances may override either dimension
  independently, and the renderer keeps that box unchanged as readings change.
  Widgets use center-relative pan/rotation without widget zoom or stretch.
  The GUI has numerical size controls, Use template size, Fit to content, and
  a non-blocking overflow warning. The fit calculation includes the current
  reading, a representative formatted value, the missing-data placeholder,
  label placement, and padding. The config schema and inheritance rules are
  documented in README. Validated by config serialization/inheritance tests,
  `99`/`100` stable-geometry and rotation checks, Fit/overflow tests for labels
  above and below the value, transparent/opaque raster checks, label-position
  pixel checks, the full GUI-enabled suite, formatting, non-strict Clippy, and
  a release build of both binaries.
  Interactive GUI appearance has not been user-verified; no device write was
  performed.

- [x] **Keep instance replacement working after executable rebuilds** — owner
  validation now uses the running executable's device/inode identity in addition
  to PID, UID, and process start time; legacy lock records also accept Linux's
  ` (deleted)` suffix. Instance owner metadata is published atomically beside
  the lock, and competing launches retry briefly during publication. Focused
  identity, publication-race, and graceful-handoff tests passed, as did the
  GUI-enabled suite. This follow-up has not been user-verified; the broader
  single-instance behavior remains separately recorded under Verified.

- [x] **Use center-origin snapping and center-drawn grids for transformed
  content** — formalize `(240, 240)` as the 480x480 canvas origin and represent
  placement as an object-center offset from that point. Snap with symmetric
  multiples of the active pan grid around zero, using the transformed media
  center or the measured center of a widget instance as the pivot. Scaling and
  rotation must preserve that pivot, odd-sized widget bounds must retain
  half-pixel centers internally, and circular display clipping must remain a
  visual concern rather than changing the square canvas coordinate system.

  Extract shared center-snap and canvas-to-preview grid helpers for Live
  backgrounds, Boot media, Standby media, the existing widget-placement adapter
  where practical, and the future widget instance system. Draw the primary axes
  through the exact preview center and additional lines outward at
  `center +/- N * grid size`, with stronger center axes and clipping to the
  preview. Grid sizes that do not divide 240 evenly must not shift the origin or
  cause the drawn grid and actual snap points to disagree.

  Extend each target's independent snap settings with its own `Show grid` value,
  alongside snap enabled, pan-grid size, and rotation interval. Enable snapping
  and grid visibility by default while allowing the grid to be hidden without
  disabling snapping. Changing grid size, visibility, or enabled state must not
  move or resnap committed positions; only a moving drag commits against the
  active grid, and a click without motion preserves an unsnapped position.
  Numerical controls and the retained `Drag`/`Preview` readout remain
  center-relative and use identical terminology across editors.

  Cover exact-center alignment, positive/negative symmetry, non-divisor grid
  sizes, canvas-to-preview agreement, stable scale/rotation pivots, unchanged
  committed values after grid changes, per-target independence, click-without-
  motion behavior, and half-pixel widget centers with regression tests. Validate
  with the full GUI-enabled suite, Clippy, formatting, diff checks, release
  builds, visual checks at representative 7/8/12/20 px grids, and a separately
  reported hardware boundary for device placement.

- [x] **Reimplement widgets as templates and instances** — separate telemetry
  data sources, reusable presentation templates, and placed widget instances.
  Data sources own technical identity, availability, current value, and default
  unit without owning layout or appearance. Templates own a stable ID, name,
  widget kind, default typography, colours, thresholds, spacing, alignment,
  sizing, and missing-data presentation without containing a sensor binding,
  position, or z-order. Ship read-only built-in templates and allow user
  templates to be duplicated, renamed, edited, and removed only when unused.

  Each placed instance must have its own stable ID, template reference, data-
  source binding, visibility, center-relative transform, and list position as
  its z-order. Permit multiple instances of one template and multiple instances
  bound to one source. Use live template inheritance with sparse per-instance
  overrides rather than copying template values at creation: unresolved fields
  follow later template edits, overridden fields remain local, and the GUI
  exposes clear inherited/overridden state plus `Reset to template` actions.

  Rebuild the Overlay workflow around one `Add widget` template selector and an
  ordered instance list supporting selection, visibility, rename, duplicate,
  reorder, and remove. Separate instance editing into Data, Appearance,
  Transform, and Template/override sections; keep template editing in a distinct
  mode so an instance edit cannot accidentally modify all siblings. Prevent
  deletion of referenced templates until their instances are reassigned or
  removed.

  Resolve each visible instance in list order by combining its template and
  overrides, reading the bound data source, measuring and rendering local widget
  bounds, and then handing those bounds to the shared center-origin transform
  and compositing pipeline. Templates remain position-independent. Define a
  deterministic missing-data behavior and cache static layout/style work without
  preventing dynamic value or inherited-template updates.

  Add an idempotent compatibility migration: each currently enabled sensor
  becomes one instance at its resolved preset/custom-slot position; existing
  label, unit, colour map, font sizes, and other customized presentation become
  overrides; disabled sensors remain available as data sources without creating
  instances. Preserve old `sensors`, layout presets, `max_visible`, and custom
  slots as readable compatibility input until migration and rendering parity are
  proven. Cover inheritance, override reset, duplicate bindings and templates,
  ordering, deletion constraints, stable IDs, missing sources, exact legacy
  migration, serialization round trips, and renderer output with regression
  tests before retiring the legacy widget path.

- [x] **Apply shared transforms to widget instances** — adapt each future widget
  instance to the common transform and drag/snap contract while keeping its state
  independent from its template and sibling instances. Transform around measured
  widget bounds using the shared center-origin pivot and center-drawn grid,
  retain the `Drag`/`Preview` readout and exact numeric controls, and keep widget
  anchoring, clipping, visibility, and selection handles outside the shared
  geometry engine. This task depended on both the center-origin foundation and
  the widget template/instance model. Subsequent widget-sizing work intentionally
  narrowed widget placement to pan and rotation; media keeps zoom and stretch.

- [x] **Refresh Boot and Standby device previews after live edits** — device
  preview helpers are now keyed by mode, source path, brightness, the complete
  media transform and canvas colour, Boot trim/timing, and Standby frame time.
  Changed transforms replace the immutable helper after a 150 ms trailing
  debounce, while pan and rotation release force an immediate refresh. Mode and
  source changes remain immediate. Intentional replacement kills and reaps the
  old child before spawning its successor, keeps the live daemon paused across
  the handoff, and clears pending work when preview is disabled. Regression tests
  cover Boot and Standby helper-input invalidation. The full 194-test GUI-enabled
  suite, Clippy, formatting, diff checks, and release builds passed at
  completion; perceived device latency and lock handoff still await hardware
  verification.

- [x] **Bring Boot and Standby transform controls to Live Display parity** —
  removed the legacy Fit dropdown and gave both media tabs the same `1:1`, `Fit`,
  `Cover`, and `Stretch` visible-value presets as Live Display. All three targets
  now share native-baseline preset math while retaining their appropriate
  media-specific controls. Live, Boot, and Standby each own independent snap
  enabled, pan-grid, and rotation-grid settings, and their preview gestures route
  exclusively through the active target's settings without resnapping committed
  transforms. New Boot and Standby sources receive Cover once dimensions arrive;
  path and transform guards reject stale results and preserve intervening edits.
  Presets remain disabled until dimensions are known, and Reset establishes a
  native 1:1 transform without changing canvas colour or snapping preferences.
  Regression tests cover cross-target preset equivalence, hidden-fit removal,
  guarded Cover defaults, reset semantics, snap independence, and grid changes.
  The full 192-test GUI-enabled suite, Clippy, formatting, diff checks, and release
  builds passed at completion; visual GUI and physical-device behavior await
  user verification.

- [x] **Rotate transformed media with a natural right-button arc gesture** —
  right-button dragging now rotates Live Display backgrounds, boot media, and
  standby media by following the pointer's angular path around the preview
  center. The reusable rotation state unwraps boundary crossings, maintains
  independent raw and snapped-preview angles, commits the appropriate normalized
  angle on release, and ignores unstable starts in a small center dead zone.
  Primary drag remains pan, the wheel remains zoom, exact numeric controls are
  retained, and the live `Rotate`/`Preview` readout remains visible. Later snap
  interval changes do not alter committed rotations. Focused tests cover arc
  unwrapping, dead-zone handling, snapping, normalization, release semantics,
  later interval changes, and target-state independence. The full 186-test
  GUI-enabled suite, Clippy, formatting, diff checks, and release builds passed;
  visual interaction and physical-device behavior await user verification.

- [x] **Make device preview tab-driven and pause the daemon while it owns the
  device** — replaced the GUI mode dropdown with one persistent `Show on device`
  switch. Boot now selects looping boot preview, Standby selects standby preview,
  and other pages stop the helper while retaining enabled intent. Cross-tab
  handoff reaps and replaces helpers without resuming between modes; missing
  sources remain dormant and start when selected, and unexpected exits disable
  preview with an error. The daemon control socket now supports bounded,
  acknowledged `pause`, `resume`, and `state` commands. Pause keeps daemon and
  instance ownership alive while dropping HID and the device lock; resume fully
  reacquires and initializes them, reports failures while remaining paused, and
  is idempotent. The GUI resumes only the exact process it changed to paused.
  `--daemon-control pause|resume|state` provides recovery, startup detects paused
  state, and Overview exposes a Resume button when no GUI preview owns the pause.
  Validated by control-state, idempotency, failure, shutdown wake-up, bounded
  command framing, owner/lifecycle, navigation mapping, CLI parsing, and full
  GUI-enabled tests. Clippy, formatting, diff checks, and release builds passed;
  live hardware handoff remains for user verification.

- [x] **Close the current cross-target transform validation gaps** — added
  focused regression coverage for center-pivot rotation and canvas clipping,
  native-source canvas fill, transform/cache-layer invalidation, identical
  trimmed-frame and normalized-timing behavior between boot upload preparation
  and transient device preview, and the explicit nonfatal error for media that
  requires a missing optional FFmpeg installation. The full GUI-enabled suite
  passed 168 tests at completion. Widget adapters were subsequently implemented
  with the template/instance model and later narrowed to pan/rotation placement.
  Formatting, Clippy, diff checks, and release builds passed; visual GUI testing
  and any persistent device upload remain user-controlled validation boundaries.

- [x] **Add boot-animation editing controls** — added inclusive start/end frame
  trimming, a source-frame timeline scrubber with source-time readout, and linked
  uniform delay/FPS controls. Scrubbing pauses only the desktop preview and does
  not alter committed timing. The same trim range, uniform delay, and transform
  arguments now drive debounced container inspection, transient device preview,
  and explicit upload. The helper validates nonempty in-range selections and the
  80 ms minimum; existing container validation retains the 255-frame and 10 MiB
  limits. Persistent writes still occur only through Upload. Validated by focused
  trim/timing/argument tests, the full 160-test GUI-enabled suite, Clippy,
  formatting and diff checks, and rebuilt release binaries; visual GUI and
  physical-device behavior await user testing.

- [x] **Replace duplicate Live Display background controls with presets** —
  removed the File/Stream source-level Fit dropdowns and the legacy 0.25–4× Zoom
  slider. The shared Transform section now provides 1:1, Fit, Cover, and Stretch
  presets calculated from the selected source dimensions. All presets use native
  source pixels as their common baseline: 1:1 is the neutral transform, Fit and
  Cover set visible Zoom, and Stretch sets visible independent X/Y Stretch.
  Newly selected File or Stream
  sources apply Cover by default. Pan and rotation reset to a predictable
  baseline, and all unified transform, snapping, and exact controls remain
  available for fine adjustment. Validated for landscape, portrait, and
  below-canvas native sources by preset-state regression coverage, GUI-enabled
  tests, source inspection confirming one canonical background Zoom control,
  formatting and diff checks.

- [x] **Stabilize preview helper and daemon lifecycles** — switching between
  Boot and Standby device previews now stops and reaps the old helper without
  restarting the live daemon between modes. Directly launched daemons are reaped,
  daemon stop/start results are checked before device operations continue, and
  failed preview exits are reported. Boot-container inspection is serialized so
  rapid transform edits cannot accumulate CPU-heavy `th420-display` helpers.
  At that stage, device choices were Off, Boot (loop), Boot (once), and Standby;
  the later tab-driven preview task replaced this selector with one On/Off switch.
  Loop/once semantics apply only to boot animation. Validated by GUI-enabled
  tests, Clippy, format/diff checks, process inspection, and release builds.

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
