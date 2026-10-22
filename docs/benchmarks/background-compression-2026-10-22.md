# Prepared background compression benchmark — 2026-10-22

The selected live background was exported from the application’s actual preparation worker. The corpus contains 23 independently prepared 480×480 RGB frames: 15.16 MiB uncompressed. Background transforms and darkening are included; live sensor overlays are excluded. All compressed frames were decoded and compared byte-for-byte with the originals.

Host: AMD Ryzen 9 9950X3D 16-Core Processor; Linux x86_64; pinned to CPU 0. Python 3.14.8, liblz4 1.10.0, libzstd 1.5.7.

Each codec received three warm-up passes, 10 compression passes (230 frame samples), and 200 decompression passes (4,600 frame samples). Codec order was randomized each round. Input/output buffers were preallocated and reused; Zstd contexts were reused. Native library calls and Python ctypes overhead are timed; disk I/O, initial decoding, transforms, and buffer allocation are excluded. Compression is independent per frame, without dictionaries or temporal deltas.

| Codec | Payload MiB | Raw/compressed | Compress whole clip, median | Decode/frame, median | Decode/frame, p95 |
|---|---:|---:|---:|---:|---:|
| LZ4 default | 6.78 | 2.24× | 11.9 ms | 0.122 ms | 0.178 ms |
| LZ4 HC 9 | 5.77 | 2.63× | 186.1 ms | 0.115 ms | 0.170 ms |
| Zstd 1 | 5.71 | 2.66× | 19.5 ms | 0.306 ms | 0.437 ms |
| Zstd 3 | 5.38 | 2.82× | 30.6 ms | 0.318 ms | 0.452 ms |

For this corpus, Zstd level 3 reduced payload memory by 64.5%, to 5.38 MiB. Median decompression took 0.318 ms per frame, about 0.76% of a 24 FPS frame period. LZ4 default used 6.78 MiB but decoded faster (0.122 ms median). LZ4 HC saved additional space over default LZ4, but its preparation cost was substantially higher.

Zstd level 3 is the strongest memory-saving candidate from this measurement; the LZ4 variants are candidates for lower playback CPU cost. These results apply to this clip and host. Payload sizes exclude indexing, working buffers, and codec contexts. This is an offline compression microbenchmark, not an end-to-end LCD playback test or a compressed-cache implementation. Different Rust codec backends, scenes, and hardware can change the results.

## Reproduce

The script needs Python 3.11+ and system liblz4/libzstd. The export directory contains raw media pixels; keep it private and remove it after benchmarking. No source-media path or raw frames are included in this report.

```sh
export_dir=$(mktemp -d /tmp/th420-compression-XXXXXXXX)
TH420_BENCH_CONFIG=/path/to/config.toml TH420_BENCH_EXPORT_DIR="$export_dir" \
  cargo test --release --bin th420-display live_background_cache_budget_and_cadence \
  -- --ignored --nocapture
python3 scripts/benchmark-background-compression.py "$export_dir" \
  --cpu 0 --output /tmp/th420-compression-results.json
```

Choose a CPU permitted by the current affinity mask, or omit `--cpu`. The diagnostic expects the selected clip to fit the existing raw cache budget. The JSON companion records exact sizes, timing summaries, library versions, and a hash of the prepared corpus.
