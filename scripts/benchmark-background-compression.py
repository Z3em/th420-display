#!/usr/bin/env python3
"""Benchmark independent prepared RGB frames using system liblz4/libzstd.

Export the app's actual prepared frames using the ignored release diagnostic:
  TH420_BENCH_CONFIG=/path/to/config.toml TH420_BENCH_EXPORT_DIR=/private/tmp/frames \
    cargo test --release --bin th420-display live_background_cache_budget_and_cadence \
    -- --ignored --nocapture
Then run this script with that export directory and --output results.json.
No codecs are added to the application's playback path.
"""
import argparse
import ctypes as C
import ctypes.util
import hashlib
import json
import math
import os
import platform
import random
import statistics
import time
import tomllib
from pathlib import Path


def bind(lib, name, args, result):
    fn = getattr(lib, name)
    fn.argtypes = args
    fn.restype = result
    return fn


class Lz4:
    def __init__(self, high=False):
        self.lib = C.CDLL(ctypes.util.find_library("lz4") or "liblz4.so")
        self.name = "LZ4 HC 9" if high else "LZ4 default"
        self.version = bind(self.lib, "LZ4_versionString", [], C.c_char_p)().decode()
        self.bound = bind(self.lib, "LZ4_compressBound", [C.c_int], C.c_int)
        self.high = high
        self.encode = bind(self.lib, "LZ4_compress_HC" if high else "LZ4_compress_default",
                           [C.c_void_p, C.c_void_p, C.c_int, C.c_int] + ([C.c_int] if high else []), C.c_int)
        self.decode = bind(self.lib, "LZ4_decompress_safe", [C.c_void_p, C.c_void_p, C.c_int, C.c_int], C.c_int)

    def compress(self, src, dst, size, capacity):
        args = (src, dst, size, capacity)
        result = self.encode(*args, 9) if self.high else self.encode(*args)
        if result <= 0:
            raise RuntimeError("LZ4 compression failed")
        return result

    def decompress(self, src, dst, size, capacity):
        result = self.decode(src, dst, size, capacity)
        if result < 0:
            raise RuntimeError("LZ4 decompression failed")
        return result

    def close(self):
        pass


class Zstd:
    def __init__(self, level):
        self.lib = C.CDLL(ctypes.util.find_library("zstd") or "libzstd.so")
        self.name = f"Zstd {level}"
        self.version = bind(self.lib, "ZSTD_versionString", [], C.c_char_p)().decode()
        self.bound = bind(self.lib, "ZSTD_compressBound", [C.c_size_t], C.c_size_t)
        self.is_error = bind(self.lib, "ZSTD_isError", [C.c_size_t], C.c_uint)
        self.error_name = bind(self.lib, "ZSTD_getErrorName", [C.c_size_t], C.c_char_p)
        self.encode = bind(self.lib, "ZSTD_compressCCtx",
                           [C.c_void_p, C.c_void_p, C.c_size_t, C.c_void_p, C.c_size_t, C.c_int], C.c_size_t)
        self.decode = bind(self.lib, "ZSTD_decompressDCtx",
                           [C.c_void_p, C.c_void_p, C.c_size_t, C.c_void_p, C.c_size_t], C.c_size_t)
        self.free_c = bind(self.lib, "ZSTD_freeCCtx", [C.c_void_p], C.c_size_t)
        self.free_d = bind(self.lib, "ZSTD_freeDCtx", [C.c_void_p], C.c_size_t)
        self.cctx = bind(self.lib, "ZSTD_createCCtx", [], C.c_void_p)()
        self.dctx = bind(self.lib, "ZSTD_createDCtx", [], C.c_void_p)()
        if not self.cctx or not self.dctx:
            self.close()
            raise MemoryError("Zstd context allocation failed")
        self.level = level

    def check(self, result):
        if self.is_error(result):
            raise RuntimeError(self.error_name(result).decode())
        return result

    def compress(self, src, dst, size, capacity):
        return self.check(self.encode(self.cctx, dst, capacity, src, size, self.level))

    def decompress(self, src, dst, size, capacity):
        return self.check(self.decode(self.dctx, dst, capacity, src, size))

    def close(self):
        if self.cctx:
            self.free_c(self.cctx)
            self.cctx = None
        if self.dctx:
            self.free_d(self.dctx)
            self.dctx = None


def summary(samples):
    ordered = sorted(samples)
    return {"median_ms": statistics.median(ordered) / 1e6,
            "p95_ms": ordered[math.ceil(len(ordered) * .95) - 1] / 1e6,
            "max_ms": ordered[-1] / 1e6}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cpu", type=int, help="pin benchmark to an available Linux CPU")
    parser.add_argument("--compression-rounds", type=int, default=10)
    parser.add_argument("--decompression-rounds", type=int, default=200)
    args = parser.parse_args()
    if min(args.compression_rounds, args.decompression_rounds) < 1:
        parser.error("round counts must be positive")
    if args.cpu is not None:
        if args.cpu not in os.sched_getaffinity(0):
            parser.error("requested CPU is unavailable")
        os.sched_setaffinity(0, {args.cpu})
    manifest = tomllib.loads((args.directory / "manifest.toml").read_text())
    frame_size = manifest["width"] * manifest["height"] * manifest["channels"]
    if (manifest["width"], manifest["height"], manifest["channels"]) != (480, 480, 3):
        parser.error("expected prepared 480x480 RGB frames")
    raw = [(args.directory / f"frame-{i:04}.rgb").read_bytes() for i in range(manifest["frames"])]
    if not raw or any(len(frame) != frame_size for frame in raw):
        parser.error("frame count or frame size is invalid")
    sources = [C.create_string_buffer(frame) for frame in raw]
    output = C.create_string_buffer(frame_size)
    codecs = [Lz4(), Lz4(high=True), Zstd(1), Zstd(3)]
    states = []
    try:
        for codec in codecs:
            capacity = codec.bound(frame_size)
            destinations = [C.create_string_buffer(capacity) for _ in raw]
            lengths = [codec.compress(src, dst, frame_size, capacity) for src, dst in zip(sources, destinations)]
            compressed = [C.create_string_buffer(dst.raw[:length]) for dst, length in zip(destinations, lengths)]
            for original, src, length in zip(raw, compressed, lengths):
                decoded_size = codec.decompress(src, output, length, frame_size)
                if decoded_size != frame_size or output.raw != original:
                    raise RuntimeError(f"{codec.name}: lossless round-trip failed")
            states.append(dict(codec=codec, capacity=capacity, destinations=destinations,
                               lengths=lengths, compressed=compressed,
                               compress_ns=[], decompress_ns=[], loop_compress_ns=[]))
        rng = random.Random(420)
        # Warm both paths before measurement. No file I/O or buffer allocation is timed.
        for state in states:
            for _ in range(3):
                for src, dst, packed, length in zip(sources, state["destinations"], state["compressed"], state["lengths"]):
                    state["codec"].compress(src, dst, frame_size, state["capacity"])
                    state["codec"].decompress(packed, output, length, frame_size)
        for phase, rounds in [("compress", args.compression_rounds), ("decompress", args.decompression_rounds)]:
            for _ in range(rounds):
                order = states.copy()
                rng.shuffle(order)
                for state in order:
                    codec = state["codec"]
                    loop_start = time.perf_counter_ns()
                    if phase == "compress":
                        for src, dst in zip(sources, state["destinations"]):
                            start = time.perf_counter_ns()
                            codec.compress(src, dst, frame_size, state["capacity"])
                            state["compress_ns"].append(time.perf_counter_ns() - start)
                        state["loop_compress_ns"].append(time.perf_counter_ns() - loop_start)
                    else:
                        for src, length in zip(state["compressed"], state["lengths"]):
                            start = time.perf_counter_ns()
                            size = codec.decompress(src, output, length, frame_size)
                            state["decompress_ns"].append(time.perf_counter_ns() - start)
                            if size != frame_size:
                                raise RuntimeError(f"{codec.name}: invalid decoded length")
        raw_bytes = frame_size * len(raw)
        results = []
        for state in states:
            total = sum(state["lengths"])
            results.append(dict(codec=state["codec"].name, version=state["codec"].version,
                                compressed_bytes=total, compressed_mib=total / 1048576,
                                compression_ratio=raw_bytes / total, savings_percent=100 * (1 - total / raw_bytes),
                                compression_per_frame=summary(state["compress_ns"]),
                                decompression_per_frame=summary(state["decompress_ns"]),
                                compression_whole_clip_median_ms=statistics.median(state["loop_compress_ns"]) / 1e6))
        cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                    if line.startswith("model name")), platform.machine())
        report = dict(cpu=cpu, platform=platform.system() + " " + platform.machine(), python=platform.python_version(),
                      cpu_affinity=sorted(os.sched_getaffinity(0)), frames=len(raw), frame_bytes=frame_size, raw_bytes=raw_bytes, raw_mib=raw_bytes / 1048576,
                      corpus_sha256=hashlib.sha256(b"".join(raw)).hexdigest(),
                      compression_rounds=args.compression_rounds, decompression_rounds=args.decompression_rounds,
                      method="Single thread; independent frames; reusable output buffers and Zstd contexts; ctypes call overhead included; exact round-trips verified; no disk I/O timed; randomized codec order each round; no dictionaries or frame deltas.",
                      results=results)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(f"{len(raw)} frames, {raw_bytes / 1048576:.2f} MiB raw; {cpu}")
        print("Codec | MiB | Ratio | Compress ms/frame | Decode median/p95 ms/frame | Prepare whole clip ms")
        for row in results:
            enc = row["compression_per_frame"]
            dec = row["decompression_per_frame"]
            print(f'{row["codec"]} | {row["compressed_mib"]:.2f} | {row["compression_ratio"]:.2f}x | '
                  f'{enc["median_ms"]:.3f} | {dec["median_ms"]:.3f}/{dec["p95_ms"]:.3f} | '
                  f'{row["compression_whole_clip_median_ms"]:.1f}')
    finally:
        for codec in codecs:
            codec.close()


if __name__ == "__main__":
    main()
