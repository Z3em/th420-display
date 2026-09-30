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

- [ ] **Decide transient Live playback parity** — `gui_overhaul` exposed finite
  GIF playback and frame-sequence streaming with loop/FPS controls. `gui_v2`
  supports animated/streamed backgrounds, but does not yet expose the same
  finite playback workflow. Decide whether that old workflow is still required
  or explicitly superseded before closing the GUI parity audit.

## Confirmed

## Planned

## In progress

The user approved implementation of these scenarios. Widget sizing, precise
selection, color editing, media placement, and `gui_v2` parity are being worked
through together. Automated tests do not establish GUI or hardware acceptance.

- [ ] **Maintain `gui_v2` as the active GUI reimplementation** — `th420-config`
  builds from `src/gui_v2.rs`. Retain `src/gui_overhaul.rs` for comparison until
  `gui_v2` has reimplemented every required capability; do not merge the two UIs
  blindly or remove the old implementation prematurely.

  Inventory every user workflow in `gui_overhaul`, every supported runtime
  config field, and relevant CLI-only device operations. Record the equivalent
  `gui_v2` control or mark the old workflow as intentionally superseded, with a
  reason and user-visible replacement. Examine sensor-source editing,
  Undo/Redo, Profiles, Diagnostics, persistent-media controls, and service
  management explicitly; do not assume the new widget editor replaces them.
  Create separate tasks for genuine gaps and add the GUI/config parity check
  already tracked in Identified. Close this umbrella task only when required
  gaps have been implemented and the checklist has been reviewed. Keep the
  old GUI source until that point.

  Progress: `gui_v2` now has source label/unit editing, Profiles, Diagnostics,
  Undo/Redo, fine display rotation, and daemon restart. Widget instance order
  supersedes the old layout/sensor-order workflow; template/instance colors
  supersede sensor-specific colors. The detailed parity inventory, transient
  Live playback decision, and GUI validation remain open.

- [ ] **Simplify widget sizing and keep geometry stable** — use explicit local
  box width and height, with template defaults and optional per-instance
  overrides. Changing telemetry must not resize the text area, background, or
  selection box. Center the value and label group inside the box, retain their
  individual font sizes and relative placement, and provide numerical size
  controls plus a deliberate Fit to content action and overflow warning. Widget
  placement needs pan and rotation; remove widget-specific zoom and X/Y stretch
  from the new model and editor. Backwards compatibility for existing widget
  scale/stretch settings is not required. Background, Boot, and Standby media
  transforms are unaffected.

  Add width and height to template style and sparse optional width/height
  overrides to instances. Resolve them once per render from the template and
  overrides, never from the current telemetry string. Replace the widget's
  shared `Transform2D` with widget-only pan/rotation placement while retaining
  `Transform2D` for backgrounds and persistent media. Use one local rectangle
  for text layout, optional color fill, rendering pivot, and selection. Center
  the value-and-label group inside the rectangle while retaining its internal
  label offsets; preserve half-pixel centers through rotation and rasterization.
  Set built-in dimensions for representative formatted readings. On Add widget,
  use the selected template dimensions; duplicated instances retain their own
  overrides. A Fit to content action should measure the label, the current
  value (or missing-data placeholder), and a representative value for the
  bound unit, then add explicit padding and set the instance overrides.

  Expose precise numerical width/height controls, inherited/overridden state,
  Reset to template, and a non-blocking overflow warning when either rendered
  text exceeds the box. Reject non-finite or non-positive dimensions, and cap
  raster dimensions before allocation. Widget configs using the old scale and
  stretch representation need no compatibility migration; document the config
  schema change. Test inheritance, independent sibling sizing, value changes
  such as `99` to `100`, Fit to content, overflow, labels above/below values,
  transparent and opaque backgrounds, rotation pivot, and serialization.

  Progress: template box dimensions, sparse instance overrides, pan/rotation-
  only widget placement, stable render bounds, numerical size controls,
  inheritance reset, Fit to content, and overflow warning are implemented.
  Visual fit/overflow checks and stronger representative-value coverage remain.

- [ ] **Improve color editing across the GUI** — provide a consistent, larger
  swatch, editable `#RRGGBB` value, and visual picker for widget labels,
  thresholds, widget backgrounds, and canvas colors. Keep transparency separate
  where applicable, preserve template inheritance/reset, and handle incomplete
  or invalid hex input without changing the saved color.

  Implement a reusable RGB editor in `gui_v2` with a larger swatch, a visual
  hue/saturation/value popup, and a `#RRGGBB` text field. Keep a per-control
  draft string keyed by a stable widget/template/threshold identity. Accept a
  complete six-digit hex value on Enter or focus loss; retain invalid or
  incomplete drafts with inline feedback while leaving the working color
  unchanged. Changes from the popup update the desktop preview immediately;
  Apply retains its existing configuration-save semantics. Reuse the editor
  for template and instance label colors, threshold rows, widget background
  colors, and Live/Boot/Standby canvas colors. Keep background transparency
  as its existing separate 0–100% control, converting only at the UI boundary
  to the stored 0–255 opacity. A color change to an inherited instance creates
  only that field's override; Use template clears it.

  Test hex parsing/formatting, draft/commit behavior, RGB conversion, opacity
  endpoint and round-trip behavior, independent fields and threshold rows,
  template inheritance, and config save/load. Visually check popup sizing,
  keyboard editing, and preview updates at the GUI boundary.

  Progress: the reusable swatch/hex/popup editor is wired to widget labels,
  threshold rows, widget backgrounds, and all three canvas colors. Hex parsing
  has automated tests; keyboard, popup, inheritance, and transparency behavior
  still need GUI validation.

- [ ] **Select transformed widgets by their actual box** — hit-test the widget's
  rotated local box rather than its axis-aligned bounding box.
  Share geometry with rendering and the selection outline, retain topmost-first
  behavior for overlaps, and allow a small pointer tolerance. This depends on
  stable widget geometry.

  Derive the four transformed box corners from the resolved local dimensions
  and widget pan/rotation in one shared geometry helper. Draw the selection
  outline from those corners. For pointer hit-testing, map the canvas point
  through the inverse transform into local box coordinates, using a small
  tolerance measured in preview pixels. Ignore hidden or unrendered instances,
  inspect remaining instances in reverse compositing order, and keep a drag
  active once it starts even if the pointer leaves the box. An instance may
  still be selected from the list when its media is missing or off-screen.
  Test rotated empty corners, 0/90/180-degree rotations, fractional centers,
  edge tolerance at different preview sizes, overlaps, and drag continuity.

  Progress: the outline uses rotated box corners; hit testing inverse-rotates
  the pointer, uses preview-pixel tolerance, and respects visible/rendered
  instances and reverse z-order. Full interactive selection checks remain.

- [ ] **Bound and align Live, Boot, and Standby media placement** — use the square
  480×480 canvas for movement limits and edge alignment; the circular display
  mask remains visual clipping. In the default relaxed mode, allow the actual
  transformed image or video frame to move completely out of view, but stop
  when its outer edge touches the canvas edge: do not allow a gap between them.
  Exclude rotation padding filled with canvas color when determining the media
  edge. Provide a **Center image** action that resets pan to `(0, 0)` without
  changing zoom, stretch, or rotation.

  Offer independent, optional edge snapping for Live backgrounds, Boot media,
  and Standby media. Within its snap distance, alignment of a transformed
  media edge with a canvas edge takes precedence over pan-grid snapping on the
  affected axis; otherwise retain grid snapping. Preview coordinates and
  committed coordinates must obey the same rule. Also offer an optional
  **Keep viewport covered** mode that prevents any canvas pixel from falling
  outside the actual transformed media. If the current scale or rotation
  cannot cover the canvas, explain the conflict and require an explicit size
  change rather than silently modifying the transform. Apply the limits to
  pointer drags and numeric pan controls, with consistent bounds across an
  animation's frames.

  Extract shared media-footprint geometry from the fit, zoom, stretch, and
  rotation calculations before canvas-colored rotation padding is added. Work
  with the transformed source rectangle in center-relative canvas coordinates.
  In relaxed mode, a candidate pan is valid when that rectangle intersects or
  touches the square canvas; clamp an invalid candidate to the nearest valid
  contact position so a fully off-screen image cannot move farther away. In
  strict mode, require all four canvas corners to lie inside that rectangle.
  If strict coverage is geometrically impossible, leave the current transform
  intact and explain which size/rotation change is needed. While strict mode
  is active, reject scale or rotation edits that would make coverage impossible
  rather than silently changing zoom. Solid-color Live backgrounds have no
  media edge and should not show these controls.

  Provide separate edge-snap and strict-coverage toggles for Live, Boot, and
  Standby, preserving the existing independent grid settings. Use an 8 canvas
  pixel default edge-snap distance. For a rotated rectangle, align its extreme
  point on the relevant axis to a canvas edge (tangent contact), not its
  canvas-colored bounding-box padding. Evaluate edge candidates before grid
  candidates on each axis, then validate the combined candidate against the
  selected placement mode. If a grid point is illegal, choose the nearest
  legal boundary position; the final Drag/Preview readout must equal the stored
  coordinates on release. Changing snap options must never resnap a committed
  pan. Center image sets only pan to `(0, 0)`.

  Use the same constraint function for desktop dragging, numeric pan, and
  device-preview/upload arguments. Derive one legal pan region for every
  animation frame; if source dimensions can differ, use the intersection of
  their legal regions. Recompute on source or transform changes without silently
  moving a committed image. Test exact edge contact, one-pixel gaps, all four
  sides and corners, rotated contact, strict-mode impossibility, edge-versus-
  grid precedence, snap changes, animation-frame consistency, and desktop/
  device-preview geometry equivalence. Follow automated checks with visual
  preview and physical-device validation; do not mark hardware behavior
  verified from unit tests alone.

  Progress: source-footprint geometry, relaxed/strict pan regions, edge-before-
  grid snapping, per-target toggles, Center image, drag/numeric bounds, and
  geometry-edit rejection are implemented. Boot placement now intersects legal
  regions across differing animation-frame dimensions. GUI Apply, device
  preview, and persistent-media uploads now validate placement before sending
  arguments. Remaining: decide whether direct CLI media transforms should
  enforce the same optional GUI modes, complete coverage tests, and run
  visual/device checks.

## Complete

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
  geometry engine. This task depends on both the center-origin foundation and
  the widget template/instance model being implemented.

- [x] **Refresh Boot and Standby device previews after live edits** — device
  preview helpers are now keyed by mode, source path, brightness, the complete
  media transform and canvas colour, Boot trim/timing, and Standby frame time.
  Changed transforms replace the immutable helper after a 150 ms trailing
  debounce, while pan and rotation release force an immediate refresh. Mode and
  source changes remain immediate. Intentional replacement kills and reaps the
  old child before spawning its successor, keeps the live daemon paused across
  the handoff, and clears pending work when preview is disabled. Regression tests
  cover Boot and Standby helper-input invalidation. The full 194-test GUI-enabled
  suite, Clippy, formatting, diff checks, and release builds pass; perceived
  device latency and lock handoff await hardware verification.

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
  builds pass; visual GUI and physical-device behavior await user verification.

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
  GUI-enabled suite, Clippy, formatting, diff checks, and release builds pass;
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
  requires a missing optional FFmpeg installation. The full GUI-enabled suite now
  passes 168 tests. Widget adapter equivalence remains part of the planned shared
  widget-transform task because the template/instance model does not exist yet.
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
