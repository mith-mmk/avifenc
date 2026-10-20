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

## Second pass: rav1e search investigation

Baseline is `008956dc09b52bc793ffa0dc00b5f7cc2f964b5f`. This pass ships finer optional profiling and an extended benchmark harness, with no default encoder algorithm change. The `profile` feature now reports validation, config, context, frame allocation, copy/pad, submit/flush, receive packets, and base/complete totals. Diagnostic stderr gains those labels. Default features, dependency pins, u16 backend storage, search settings, quality and speed presets remain unchanged.

At 64×64 / 8-bit / quality 60 / speed 10 / alpha, wrapper medians across three timed diagnostic calls were: validation 0.009 ms, config 0.008 ms, context 0.015 ms, frame 0.057 ms, copy/pad 0.010 ms, submit/flush 0.524 ms and receive packets 485.160 ms. The whole backend guard was 485.907 ms. Eliminating setup/copies cannot provide a substantial speed gain for this workload. Guard intervals are nested and their independent medians must not be added.

A temporary copy of registry rav1e 0.8.1 was instrumented, never used as a shipping dependency. Across three calls, median tile time was 446.610 ms, including encode_tx_block 426.344 ms; nested transform selection took 398.025 ms. The narrower disjoint block phases included coefficient entropy 167.705 ms, inverse transform 105.211 ms, forward transform 60.686 ms and quantization 41.341 ms. This points to repeated transform/coefficient evaluation inside dependency RDO, rather than wrapper staging. Timers can perturb code generation and execution; these diagnostic values are not substituted into uninstrumented before/after results. Full source checksum, patch, call counts and raw records are in `performance/second-pass/`.

Base and complete searches are useful distinct work. In the measured input their primary packets were 1149 versus 1129 bytes and alpha 707 versus 661 bytes. Dropping complete search would change compression/output. No search or quality tradeoff is applied.

### Rejected candidate

Using Context<u8> for 8-bit input (Context<u16> retained for 10/12-bit) preserved every encoded-byte hash in five measured workloads. It reduced requested allocation bytes slightly, but its 0.7–1.8% timing differences were comparable to unchanged 10/12-bit control variation. The benchmark executable grew from 2407776 to 2997008 bytes (+24.5%); small-image RSS increased. The candidate was removed. It is retained only as `rejected-u8-context.patch` so the measurements are reviewable and reproducible. `git apply --unidiff-zero --check` validates it against the shipped profiling source; do not treat the candidate JSON `after` columns as shipped behavior.

| Rejected candidate workload | Median ms | Requested bytes | Peak RSS KiB |
|---|---:|---:|---:|
| u8-128-alpha | 1479.941 → 1453.240 | 149,976,851 → 147,761,939 | 5,576 → 5,092 |
| u8-64-alpha | 438.842 → 435.648 | 54,934,879 → 53,616,991 | 4,716 → 5,008 |
| u8-65x63-opaque | 271.012 → 267.374 | 32,090,756 → 31,458,436 | 4,372 → 4,712 |
| u8-control10 | 81.216 → 83.170 | 20,677,415 → 20,677,415 | 4,420 → 4,880 |
| u8-control12 | 82.812 → 81.442 | 20,677,448 → 20,677,448 | 4,500 → 4,844 |

128/64 alpha use five fresh processes per version, alternating AB/BA, with one warm-up, nine timed calls and one allocation-only call per process. Odd 65×63 opaque uses five processes and three timed calls; 10/12-bit controls use three processes and three timed calls. Input generation and options are identical within each pair. No other builds or benchmarks run concurrently. Raw per-process trials retain dimensions/depth and all hashes.

### Reproduction and next work

The existing `examples/performance.rs` arguments remain width, height, quality, alpha/opaque and iterations; optional depth (8/10/12) and speed now follow. For example `performance 32 24 60 alpha 3 12 10`. The immutable baseline can be built using the first-pass archive procedure, copying the current harness into its examples directory. The rejected candidate patch applies to a separate checkout of this profiling change. Build and save both default-feature release executables, then run the same `scripts/compare-performance.py` commands/arguments recorded in each JSON file.

For dependency diagnostics, copy the cached rav1e 0.8.1 registry source into a temporary directory, apply `performance/second-pass/rav1e-diagnostic-only.patch` there using `patch -p1`, and add a temporary `[patch.crates-io] rav1e = { path = "..." }` to an archived baseline wrapper. Build that temporary wrapper with `--release --features profile`, allowing only its temporary Cargo.lock to update. Run `performance 64 64 60 alpha 3`. Do not apply that override to the user repository. Its output hash must match the uninstrumented 64 alpha baseline. The shipped Cargo.toml and Cargo.lock are unchanged.

No worthwhile default speed improvement is claimed from this pass. The measured next target is upstream rav1e transform/coefficient RDO work, especially repeated entropy-rate evaluation and inverse reconstruction. Changing those internals needs an upstream patch with bitstream/quality regression evidence; an invasive vendored fork is not introduced. Assembly support is currently disabled by this package's existing `default-features=false` dependency; making it opt-in would add native build requirements and requires separate evaluation. Hardware sampling was unavailable (`perf` absent); built-in and diagnostic source guards supplied the measurements above.

Final shipped-code control (profiling disabled, 64 alpha, five process pairs with three timed calls each): 431.277→432.519 ms; allocations 99,722 and requested bytes 54,934,879 are identical, as is the full encoded-byte hash. No speed gain is claimed. Default/all-features release suites each pass 95 tests; 10/12-bit final hashes also match baseline. Format, diff check and rejected-patch apply check pass. Strict Clippy retains the same three pre-existing collapsible_else_if diagnostics; the scoped diagnostic check finds no other warning. Decoder remains unchanged at d3ea4e6 with its prior full 60-frame exact reference validation.
