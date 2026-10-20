# Performance and memory, 2026-10-20

Lossless RGBA8 encoding staged interleaved RGB8, interleaved RGB16, then planar GBR16. It now writes exact-capacity GBR16 planes directly. The shared final backend call, headers and quality settings are unchanged.

The allocation counters identified avoidable temporary storage; these are allocation reductions rather than a demonstrated general throughput improvement. Detailed before/after raw trials are in `performance/*.json`. Baseline source: `13ef4e46b32569d33fece0b4ed29c994754eff2c`. `performance/environment.json` records Rust, CPU, flags, source fingerprints and measurement scope. All measured inputs are 8-bit; 10/12-bit correctness continues to be covered by regression tests, not a performance claim.

## Measurement

Release builds, default features, generic Rust target, no custom RUSTFLAGS. Five fresh processes per version, alternating before/after order, one warm-up then nine timed API calls per process. Report median of the five process medians. No builds or other benchmarks ran concurrently with the recorded trials. Input preparation, checksum computation and output destruction are outside timings. The encoder uses deterministic procedural pixels, speed10, jobs0 with threading disabled, equal primary/alpha quality. The decoder materializes all native sequence frames; this does not measure streaming memory.

Allocation counters run in a separate call with timing disabled. Counts include alloc, alloc_zeroed and realloc; cumulative requested bytes count the full realloc target, not just growth. This is not peak/live heap. The allocator wrapper is present with counters disabled during timings in both binaries. Peak RSS is Linux process VmHWM, including setup/warm-up/allocator retention; sub-page and process-noise differences are not proof of an RSS improvement. The comparator requires identical output hashes across every trial. Decoder hashes every native u16 sample; encoder hashes every encoded byte.

## Reproduce

Keep before/after release binaries separate. From this repository, create the baseline source without changing the work branch:

```sh
mkdir -p /tmp/encode-baseline
git archive 13ef4e46b32569d33fece0b4ed29c994754eff2c | tar -x -C /tmp/encode-baseline
mkdir -p /tmp/encode-baseline/examples
cp examples/performance.rs /tmp/encode-baseline/examples/
CARGO_TARGET_DIR=/tmp/encode-build cargo build --locked --release --manifest-path /tmp/encode-baseline/Cargo.toml --example performance
cp /tmp/encode-build/release/examples/performance /tmp/avif-encode-before
CARGO_TARGET_DIR=/tmp/encode-build cargo build --locked --release --example performance
cp /tmp/encode-build/release/examples/performance /tmp/avif-encode-after
```

Then run the comparator with the workload arguments recorded in each JSON file, e.g.:

```sh
python3 scripts/compare-performance.py --before /tmp/avif-encode-before --after /tmp/avif-encode-after --output /tmp/encode.json 256 256 100 alpha 9
```

The added differential regression compares the former interleaved route with direct planes at three dimensions and speeds0/6/10, including odd edges. All encoded bytes match in benchmark trials. Lossy encoding is a control workload; its backend and quality choices are unchanged. Remaining candidates include the four per-chroma accumulators in RGBA-to-YUV420 conversion, requiring exact rounding preservation and separate measurement.

## Results

| Workload | Median ms, before → after | Allocation calls | Requested bytes | Peak RSS KiB |
|---|---:|---:|---:|---:|
| encode-lossless128 | 43.034 → 41.540 | 2,993 → 2,978 | 4,957,453 → 4,728,085 | 2,892 → 2,832 |
| encode-lossless256 | 169.936 → 168.963 | 10,611 → 10,594 | 19,381,721 → 18,464,225 | 4,292 → 3,900 |
| encode-lossy128 | 1463.155 → 1484.267 | 342,102 → 342,102 | 149,976,851 → 149,976,851 | 5,588 → 5,592 |

## Validation

Default and all-features release suites:95 passed each, 0 failed, 0 ignored. Direct/staged differential regression includes nine dimension/speed cases. Format passes. Strict all-target/all-feature Clippy fails on the same three pre-existing `collapsible_else_if` diagnostics at baseline and after; no lint gate is changed. A separate diagnostic run allowing only that lint finds no other warnings. This repository has no hosted CI workflow.

## Remaining CPU bottleneck

A separate release build with the existing `profile` feature runs the same128×128, quality60, speed10, alpha workload and produces the same encoded-byte hash. Across its three timed calls, median rav1e guard time is1494.750ms (four backend calls: primary/alpha, each base/complete search), RGBA-to-YUV0.079ms and container0.014ms. These diagnostic timings are not mixed into the default-build comparison. Raw guard data are in `performance/encode-lossy-stage-profile.json`. Rav1e search is the measured CPU target for subsequent work; the current search/quality policy is retained.
