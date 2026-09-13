# TH420 V2 LCD protocol reference

This is a capture-based interoperability reference for the Thermaltake TH420
V2 Ultra EX ARGB LCD (USB VID `264a`, PID `233c`). It is not a vendor
specification. Statements labelled *confirmed* were observed in USB captures
and, where stated, exercised successfully from this project on hardware. Other
behaviour is deliberately identified as unknown rather than guessed.

All multi-byte numeric fields below are little-endian unless another byte order
is stated. HID reports are zero-padded to their interface report size unless
noted.

## Interfaces

| Linux interface | Report size | Captured endpoints | Purpose |
| --- | ---: | --- | --- |
| `:1.0` | 440 bytes | OUT `0x01`, IN `0x82` | Control, configuration, persistent media, sensor query |
| `:1.1` | 1024 bytes | OUT `0x03`, IN `0x84` | Transient live JPEG frames |

`Device::open()` identifies interfaces through their hidraw sysfs links, not
fixed `/dev/hidrawN` numbers.

## Initialization and coolant query

Control commands use the prefix `command 01 00 80`. Before a live stream or a
persistent upload, send the following reports and read one response after each:

```text
85 01 00 80
87 01 00 80
85 01 00 80
87 01 00 80
84 01 00 80
81 01 00 80
```

The project uses a 50 ms delay between commands. The vendor application used
the same sequence when attaching. Response semantics are not decoded, but a
response is read and drained after each command; skipping those reads is
untested.

Coolant query: `80 01 00 80`. Response bytes 6--7 are a big-endian unsigned
millidegrees-C value; divide by `1000.0`.

## Live display streaming

This is the original project functionality. Frames are transient: the daemon
renders and resends a 480 x 480 JPEG every ~800 ms by default. A captured
vendor live stream ran at about 21.5 fps (median inter-frame spacing 47.7 ms),
and the project was visually verified at 24 fps. Neither observation is known
to be a firmware maximum.

1. Write `12 01 00 80 brightness` on the control interface. The legacy
   `Device::send_frame()` writes `64` hex (100 decimal) before every frame.
   Continuous playback sets brightness once and then sends image data directly
   so a control read does not limit the frame rate.
2. Split the JPEG into 1020-byte pieces and write a 1024-byte report per piece
   on the image interface:

   ```text
   First: 08 total_chunks 00 80 jpeg[0..1020]
   Later: 08 chunk_index  00 00 jpeg[...]
   ```

   `chunk_index` starts at 1 for the second piece. The final report is padded;
   no separate end-of-frame command was observed.

The first chunk's `total_chunks` and subsequent `chunk_index` fields are one
byte. The current implementation does not yet reject a JPEG needing more than
255 chunks; callers should keep a frame below 260,100 bytes until that guard is
added.

Live content is not persistent. A subsequent live frame replaces the visible
frame; a reset displays the separately stored boot/standby media.

## Persistent media envelope

Persistent media uses the 440-byte control interface, not live-image reports.
For a source blob of `total_bytes`:

1. Send `82 01 00 80`, then read its response.
2. Send `0a 01 00 80 kind 00 00 00 total_bytes_le32`, then read its response.
3. Send `0x0b` logical chunks, reading one response per complete chunk.
4. Finish as specified for the selected `kind` below.

| `kind` | Meaning | Status |
| ---: | --- | --- |
| `00` | Standby picture JPEG | Implemented and hardware-tested |
| `01` | Boot-animation container | Implemented and hardware-tested |

### `0x0b` chunk layout

The vendor sends at most 10,240 source bytes per logical chunk at `offset`:

```text
First 440-byte report:
0b packet_count 00 80 offset_le32 chunk_len_le32 progress_le32 data[0..424]

Subsequent 440-byte reports:
0b report_index 00 00 data[...up to 436 bytes]
```

- `packet_count = ceil((chunk_len + 16) / 436)`.
- `report_index` starts at 1.
- `progress` is `((offset + chunk_len) * 100 / total_bytes)` as a `u32`.
- Unused bytes in the final physical report are zero.

The first report reserves 16 bytes for metadata and carries 424 data bytes;
later reports reserve four bytes and carry 436. The exact firmware-level
meaning of the initial and final `0x82` reports is unknown.

## Standby picture

The vendor UI uses JPEG. The current CLI accepts an image, converts it to RGB,
resizes it to 480 x 480 using Lanczos3, encodes JPEG, and uploads `kind = 0`.
The transfer completes with `82 01 00 80` and a read response.

It can combine an image upload with overlay color and brightness in one device
session:

```sh
th420-display --upload-standby IMAGE --pump-temp-color '#0000ff' --standby-brightness 80
```

The result was verified to survive a physical reconnect.

## Standby overlay and brightness

Each command below was captured and its visible result verified. Brightness and
temperature-overlay visibility were independently verified to survive a
physical reconnect and reboot; text color was verified rendered and persistent
when committed with a standby upload.

| Function | Report prefix | Confirmed semantics |
| --- | --- | --- |
| Brightness | `12 01 00 80 value` | 0--100 percent (`00` dark; `64` = 100%) |
| UI on/off switch | same `0x12` | Off writes 0; on restores the app's remembered brightness; no separate power command |
| Pump-temperature overlay | `18 01 00 80 value` | `00` show; `02` hide |
| Pump-temperature text color | `16 01 00 80 R G B ff` | RGBA; green is `00 ff 00 ff`; vendor repeats the write after about 280 ms |

The Windows "Keep the LCD screen on during standby" checkbox is expressed as
brightness: app exit writes 100 when enabled and 0 when disabled. It is not a
separate boolean in the observed protocol. Likewise, the UI's LCD on/off switch
is a brightness wrapper, not a separate power command.

Text color is staged, not committed by `0x16` alone. A native color-only write
was ignored; the verified vendor-compatible transaction is two `0x16` writes,
then a standby upload, then optional brightness. The CLI therefore requires
`--upload-standby` whenever `--pump-temp-color` is supplied.

## Boot animation

The vendor application uses the persistent-media envelope with `kind = 1` for
boot media. It converts a supplied GIF into a `Update_Boot_GIF` container; the
transmitted blob is not the GIF. The project produces the same observed format
and successfully installed a native animation on hardware.

The container layout is:

```text
0x00 i32  -(data_end + table_len + trailer_len + frame_count)
0x04 u32  data_end (end of the final JPEG; excludes trailer)
0x08 u32  table_len = frame_count * 16
0x0c u16  trailer_len = 16
0x0e u8   frame_count
0x0f u8   'P'
0x10 char[16] "Update_Boot_GIF\\0"
0x20 frame_count × {
       i32 -(record_offset + jpeg_size + 8),
       u32 record_offset,
       u32 jpeg_size,
       u32 0x40000008
     }
records: ASCII `NNN.jpg\\0` (eight bytes) immediately followed by one
         480 x 480 JPEG
trailer: 15 zero bytes followed by `0x10`
```

The vendor's captured JPEGs were in source order, quality 75, and used 4:2:0
chroma subsampling. The native encoder deliberately uses the same 4:2:0,
quality-75 JPEG parameters. Frame names are currently `000.jpg` through
`NNN.jpg`.

After the final boot `0x0b` chunk ACK, wait briefly, send the
fire-and-forget commit report, wait briefly, then send final `0x82` and read
its response:

```text
14 01 00 80 (frame_delay_ms - 1)_le32
82 01 00 80
```

The `0x14` value is one global frame delay, not an observed per-frame field.
This was confirmed by two otherwise byte-identical vendor containers: an
800 ms GIF sent `0x14 ... 1f 03` (799), while a 200 ms GIF sent
`0x14 ... c7 00` (199). The six-frame, 200 ms vendor animation played for
approximately 1.2 seconds, and a native six-frame, 250 ms animation also
played successfully. The earlier apparent 40 ms spacing was flash-chunk
transfer cadence, not animation timing.

### Native boot-upload limits

The CLI command is:

```sh
th420-display --upload-boot ANIMATION.gif
```

It accepts GIF input only, converts each frame to a 480 x 480 RGB JPEG using
Lanczos3, and builds the container above. It rejects an empty GIF, more than
255 frames, non-integral-millisecond frame delays, non-uniform frame delays,
delays below 80 ms, and containers over the project's conservative 5 MiB
limit. The 80 ms and 5 MiB bounds are project safety guards, not documented
firmware limits. `--upload-boot` is intentionally exclusive of live playback
and standby-setting arguments.

The uniform-delay restriction follows the only timing field observed in the
container: the global `0x14` commit value. It is not evidence that firmware
could never support a different, undiscovered variable-timing format.

## Confidence and safety boundary

Confirmed: interface split, initialization sequence, coolant query, live JPEG
stream, standby JPEG persistence, persistent brightness and pump-temperature
visibility, staged RGBA text color, boot container encoding, boot commit timing,
and successful native boot installation.

Still unknown: response semantics, firmware storage limits, the maximum safe
live frame rate, the maximum number/size of boot frames, variable boot timing,
and controls outside the observed Standby page. Keep persistent writes bounded,
preserve the logical-chunk response rhythm, and test new protocol variants with
an easily recognizable disposable image or animation.
