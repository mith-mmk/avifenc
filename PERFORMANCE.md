# Performance and memory, 2026-10-20

Lossless RGBA8 encoding staged interleaved RGB8, interleaved RGB16, then planar GBR16. It now writes exact-capacity GBR16 planes directly. The shared final backend call, headers and quality settings are unchanged.

The allocation counters identified avoidable temporary storage; these are allocation reductions rather than a demonstrated general throughput improvement. Detailed before/after raw trials are in `performance/*.json`. Baseline source: `13ef4e46b32569d33fece0b4ed29c994754eff2c`. `performance/environment.json` records Rust, CPU, flags, source fingerprints and measurement scope. All measured inputs are 8-bit; 10/12-bit correctness continues to be covered by regression tests, not a performance claim.

## Measurement

Release builds, default features, generic Rust target, no custom RUSTFLAGS. Five fresh processes per version, alternating before/after order, one warm-up then nine timed API calls per process. Report median of the five process medians. No builds or other benchmarks ran concurrently with the recorded trials. Input preparation, checksum computation and output destruction are outside timings. The encoder uses deterministic procedural pixels, speed 10, jobs 0 with threading disabled, equal primary/alpha quality. The decoder materializes all native sequence frames; this does not measure streaming memory.

Allocation counters run in a separate call with timing disabled. Counts include alloc, alloc_zeroed and realloc; cumulative requested bytes count the full realloc target, not just growth. This is not peak/live heap. The allocator wrapper is present with counters disabled during timings in both binaries. Peak RSS is Linux process VmHWM, including setup/warm-up/allocator retention; sub-page and process-noise differences are not proof of an RSS improvement. The comparator requires identical output hashes across every trial. Decoder hashes every native u16 sample; encoder hashes every encoded byte.

## Reproduce

Keep before/after release binaries separate. From this repository, create the baseline source without changing the work branch:

```sh
mkdir -p /tmp/encode-baseline
git archive 13ef4e46b32569d33fece0b4ed29c994754eff2c | tar -x -C /tmp/encode-baseline
mkdir -p /tmp/encode-baseline/examples
cp examples/performance.rs /tmp/encode-baseline/examples/
CARGO_TARGET_DIR=/tmp/encode-before-build cargo build --locked --release --manifest-path /tmp/encode-baseline/Cargo.toml --example performance
cp /tmp/encode-before-build/release/examples/performance /tmp/avif-encode-before
CARGO_TARGET_DIR=/tmp/encode-after-build cargo build --locked --release --example performance
cp /tmp/encode-after-build/release/examples/performance /tmp/avif-encode-after
```

Then run the comparator with the workload arguments recorded in each JSON file, e.g.:

```sh
python3 scripts/compare-performance.py --before /tmp/avif-encode-before --after /tmp/avif-encode-after --output /tmp/encode.json 256 256 100 alpha 9
```

The added differential regression compares the former interleaved route with direct planes at three dimensions and speeds 0/6/10, including odd edges. All encoded bytes match in benchmark trials. Lossy encoding is a control workload; its backend and quality choices are unchanged. Remaining candidates include the four per-chroma accumulators in RGBA-to-YUV420 conversion, requiring exact rounding preservation and separate measurement.

## Results

| Workload | Median ms, before → after | Allocation calls | Requested bytes | Peak RSS KiB |
|---|---:|---:|---:|---:|
| encode-lossless128 | 43.034 → 41.540 | 2,993 → 2,978 | 4,957,453 → 4,728,085 | 2,892 → 2,832 |
| encode-lossless256 | 169.936 → 168.963 | 10,611 → 10,594 | 19,381,721 → 18,464,225 | 4,292 → 3,900 |
| encode-lossy128 | 1463.155 → 1484.267 | 342,102 → 342,102 | 149,976,851 → 149,976,851 | 5,588 → 5,592 |

## Validation

Default and all-features release suites:95 passed each, 0 failed, 0 ignored. Direct/staged differential regression includes nine dimension/speed cases. Format passes. Strict all-target/all-feature Clippy fails on the same three pre-existing `collapsible_else_if` diagnostics at baseline and after; no lint gate is changed. A separate diagnostic run allowing only that lint finds no other warnings. This repository has no hosted CI workflow.

## Remaining CPU bottleneck

A separate release build with the existing `profile` feature runs the same128×128, quality 60, speed 10, alpha workload and produces the same encoded-byte hash. Across its three timed calls, median rav1e guard time is1494.750ms (four backend calls: primary/alpha, each base/complete search), RGBA-to-YUV0.079ms and container0.014ms. These diagnostic timings are not mixed into the default-build comparison. Raw guard data are in `performance/encode-lossy-stage-profile.json`. Rav1e search is the measured CPU target for subsequent work; the current search/quality policy is retained.

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

## Third pass: opt-in upstream SIMD

Baseline: `6a7ba0a8ae42425fff4b2e4cacddd3380d6edbfb`. Add `asm = ["rav1e/asm"]` with `default = []` unchanged. This is the existing pinned rav1e 0.8.1 implementation, not a version change, vendored fork or source override. x86_64 builds require NASM>=2.15 and the native toolchain; NASM 2.16.03 was built only under /tmp for this evaluation. `nasm-rs 0.3.2` is the only new locked package. Quality, quantizer calibration, speed, base/complete search, color metadata and threading settings are unchanged. No merge or publication occurs.

### Dispatch correctness and the new pixel-storage rationale

The portable build's CPU-feature type exposes only RUST. With rav1e ASM enabled on x86_64, coefficient CDF updates gain SSE2 and transforms use runtime-selected native kernels. Raw enabling of ASM while retaining u16 for 8-bit input produced a different 32×24 bitstream (FNV 6582551f1122d55f versus baseline 6666dc53b980be23). With CPU target RUST/SSE2, that raw variant matched baseline. The inverse dispatch source selects its HBD12 table for u16 depths other than 10; that is a concrete path observation, not a complete isolated root-cause claim.

Consequently, the opt-in build uses the supported u8 context for 8-bit samples, and u16 for 10/12-bit. This is materially different from the second-pass rejected u8-only scalar specialization: native dispatch selects its low/high-depth kernels using pixel type, and the combined SIMD candidate provides substantial measured gains. Portable builds retain u16. All sample ranges are checked before the cast; no precision reduction is introduced. The incorrect raw u16/ASM variant is not shipped. Frozen complete AVIF references make this observed incompatibility a mandatory regression.

### Repeated measurements

Linux x86_64 AMD EPYC 9V74, Rust 1.91.0, generic release target, no custom RUSTFLAGS, single-threaded dependency feature sets: baseline default versus asm only. Same inputs/settings for each pair, quality 60, speed 10, jobs 0, equal color/alpha quality; only build dispatch changes. Five fresh process pairs alternate AB/BA. 128/64 alpha use nine timed calls; other ASM cases use three. Each process includes one warm-up and a separate allocation-only call. Preparation, output hashes and output destruction are outside timing; no builds or other benchmarks run concurrently. Allocation bytes are cumulative requested bytes, not peak/live heap; peak RSS is process VmHWM including code pages and allocator retention. Full raw trials preserve every per-process median, extrema and hash.

| Workload | Median ms | Time reduction | Allocation calls | Requested bytes | Peak RSS KiB |
|---|---:|---:|---:|---:|---:|
| asm-128-alpha | 1547.122 → 838.189 | 45.8% | 342,102 → 869 | 149,976,851 → 19,216,147 | 5,652 → 5,808 |
| asm-64-alpha | 475.751 → 229.481 | 51.8% | 99,722 → 643 | 54,934,879 → 16,281,951 | 4,612 → 5,600 |
| asm-65x63-opaque | 279.376 → 136.851 | 51.0% | 63,608 → 403 | 32,090,756 → 8,146,948 | 4,380 → 5,320 |
| asm-10bit | 85.208 → 48.367 | 43.2% | 18,654 → 568 | 20,677,415 → 13,050,663 | 4,432 → 5,412 |
| asm-12bit | 81.252 → 48.141 | 40.8% | 18,654 → 718 | 20,677,448 → 13,665,096 | 4,412 → 5,384 |
| asm-natural64 | 92.990 → 46.237 | 50.3% | 17,018 → 363 | 15,573,284 → 8,033,316 | 4,440 → 5,316 |

The 128×128 case falls from 1547.122 to 838.189 ms with identical complete encoded output. Across six measured workloads time falls 41–52%; this is scoped to the measured host/inputs. Peak RSS increases: 5652→5808 KiB for 128 alpha, and by about 0.9–1.0 MiB in small cases. The benchmark binary grows from 2,410,304 to 4,158,968 bytes (+72.6%). These are explicit optional-build tradeoffs, not hidden changes to the portable default. `multithread` may be combined, but its throughput/memory effects are not included in these single-threaded comparisons.

The committed `natural64.rgba` comes from avif-rust's existing plum-blossom fixture using FFmpeg 7.1.5, bilinear 64×64 conversion, RGBA8, opaque alpha. Original and derived SHA256 plus the exact command are recorded in environment.json. The benchmark accepts `AVIF_PERF_INPUT` for raw RGBA; without it, the prior procedural input is unchanged. Optional depth/speed arguments remain available. The frozen 8/10/12-bit reference files were generated with the immutable portable baseline and the committed reference-generator.rs, not with the candidate.

### Reproduction

Build an archived baseline as in the first-pass procedure, copying the current performance example so both binaries accept identical arguments. Save a default release binary and build the current checkout with `cargo build --locked --release --features asm --example performance`; save it separately. NASM must be on PATH for the latter on x86_64. Use the commands/arguments and process counts in each JSON file with `scripts/compare-performance.py`. For the natural case, set `AVIF_PERF_INPUT=performance/third-pass/natural64.rgba` in the comparator's environment. Do not run compilation or another benchmark concurrently.

For frozen references, copy performance/third-pass/reference-generator.rs into the archived 6a7ba0a baseline's examples directory and run its default release build with a temporary output directory. The resulting three AVIFs must match tests/fixtures/portable-8bit.avif, portable-10bit.avif and portable-12bit.avif by SHA256. tests/simd_output.rs always compares full encoded bytes; it cannot skip for missing tools or external checkouts. Raw fixtures are marked binary in Git.

### Rejected inverse-scratch experiment and maintenance

The Rust inverse path allocates a zeroed Vec on each call. A reversible dependency-copy experiment replaced areas<=1024 with an initialized 4 KiB stack buffer, retaining heap fallback and identical padding/math for larger transforms. Output hashes matched at 8/10/12 bits and 64 alpha allocations fell 99,722→643, requested bytes 54,934,879→17,599,839. Throughput was not consistently better: 64 alpha 444.884→451.595 ms, 10 bit 87.340→89.353 ms, 12 bit 87.654→82.476 ms. It adds stack usage and shipping it would require maintaining a rav1e source fork (~11MB source) or an upstream change. No override/fork is adopted; the exact zero-context patch and raw trials remain for review. No upstream communication is sent.

The optional existing SIMD backend avoids that maintenance cost and improves measured throughput. Users needing the smaller portable binary or avoiding native tools can leave asm off. A portable control has identical allocation counts, requested bytes and output hashes (442.352→441.277ms, timing noise). A lossless-path control also has identical allocation counts/bytes/output; its 46.150→43.741 ms shift is not attributed to SIMD because the private lossless backend is unchanged.

### Validation and scope

Default, asm and all-features release suites each pass 96 tests, including the frozen 8/10/12-bit bitstreams and existing FFmpeg oracles. The mandatory reference test also passes under all seven CPU caps actually resolved on this host: RUST, SSE2, SSSE3, SSE4_1, AVX2, AVX512 and AVX512ICL. Format and the scoped Clippy diagnostic pass; the three pre-existing collapsible_else_if warnings remain, with no source suppression or weakened gate. New Linux CI covers portable, asm and all-features at MSRV 1.88, runs the mandatory references before full regressions, and installs NASM only for native-feature jobs. CI outcomes are linked in the PR after push. Windows/macOS/aarch64 performance is not measured here.

Decoder code remains at d3ea4e6; no dependency pins or feature forwarding there are changed. Its existing 60-frame exactness evidence remains valid. This encoder feature is enabled explicitly in this checkout; it does not implicitly enable SIMD through the decoder's currently pinned encoder dependency.

## Fourth pass: remove retained RGBA16 RGB staging

Baseline: `f125e0966286304569b75e5ddea6c5a5456ad88f`. The earlier direct-GBR optimization covered RGBA8, while `encode_rgba16_with_lossless_items` still created a scaled interleaved RGB16 vector. It then copied that vector into GBR or gray planes, or did not use it at all when only alpha was lossless. The RGB vector remained alive through primary/alpha encoding and container construction.

This pass scales directly from validated RGBA16 into the final lossless GBR or gray planes; the alpha-only-lossless path no longer constructs unused RGB. It removes one allocation and exactly **6 bytes per pixel** of cumulative allocation requests from affected API calls. Alpha extraction/association, full-range integer rounding, all source validations, quality, speed, metadata and backend choice are preserved. It adds no feature, dependency, fork or public API. Default Pure Rust and optional asm remain as before; versions are unchanged.

### Measurements

Compare default-before/default-after and asm-before/asm-after independently. Separate Cargo target directories are used for each source checkout to prevent shared-output cache confusion; initial shared-target probes were discarded before retained trials. All measurements below use isolated fresh release artifacts and the identical enhanced harness. Five process pairs alternate AB/BA, with one warmup and a separate allocation-only call. The two 1024 cases use 9 timed calls,512/128lossless cases 5, others 3. No concurrent builds or benchmarks ran during timing. Per-case environment variables and exact arguments are in environment.json and individual JSON files. All eight cases retain identical complete encoded-output hashes.

| Workload | Median ms | Cumulative requested bytes | Peak RSS KiB |
|---|---:|---:|---:|
| portable-1024-flat12-alpha | 85.240 → 81.699 | 74,316,653 → 68,025,197 | 37,768 → 31,496 |
| asm-1024-flat12-alpha | 82.168 → 77.588 | 74,316,653 → 68,025,197 | 37,792 → 31,780 |
| portable-512-flat10-opaque | 18.257 → 17.629 | 17,073,240 → 15,500,376 | 11,432 → 9,756 |
| asm-128-procedural10-alpha | 92.844 → 84.572 | 6,141,555 → 6,043,251 | 3,272 → 3,272 |
| asm-64-primary60-alpha100-12bit | 109.035 → 104.426 | 9,940,045 → 9,915,469 | 5,672 → 5,764 |
| portable-64-primary100-alpha60-8bit | 249.785 → 250.132 | 31,436,624 → 31,412,048 | 4,820 → 4,700 |
| asm-128-lossy8-control | 788.388 → 793.586 | 19,216,147 → 19,216,147 | 5,816 → 5,824 |
| portable-64-lossy12-control | 404.372 → 395.816 | 53,268,575 → 53,268,575 | 4,996 → 5,016 |

1024×1024, 12-bit, alpha, quality 100 primary/alpha, speed 10 reduces portable peak RSS 37,768→31,496 KiB (~16.6%), and asm-build peak 37,792→31,780 KiB (~15.9%). The requested-byte delta is exactly 6,291,456 bytes (6 MiB). Portable time 85.240→81.699 ms and asm-build time 82.168→77.588 ms are measured medians; these lossless items use the private backend even when the optional asm feature is built. Smaller cases can have unchanged/noisy RSS, so the peak-memory claim is scoped to these larger representative memory workloads.

The mixed item cases exercise color 100/alpha 60 and color 60/alpha 100. Their output depths are explicitly 8/12 bits, using RGBA16 input; the 10-bit procedural case covers nonflat fully lossless data and the 10-bit opaque case covers no-alpha output. Lossy-only controls are unaffected by this path: allocation counts/bytes and hashes are identical. The normal 8-bit asm lossy control 788.388→793.586 ms (~0.7%) remains within timing variation; its existing SIMD improvement is retained. No speed gain is claimed for controls.

Binary size does not grow: portable 2,411,512→2,411,416 bytes and asm 4,159,944→4,158,880 bytes. There is no new native-tool requirement beyond the existing opt-in asm feature. The test-only former interleaved helper remains as a differential oracle, not shipping code.

### Reproduce and verify

Archive f125e09 and copy the current examples/performance.rs into its examples directory. Build baseline default/asm under one target directory exclusive to the archive, and current default/asm under a different target directory exclusive to the working checkout. Save each executable separately before comparing. Do not share a Cargo target directory between checkouts with the same package name/version. The two source fingerprints and four binary SHA256/size values are in environment.json.

For example, run the comparator with `AVIF_PERF_RGBA16=1 AVIF_PERF_PATTERN=flat` and arguments `1024 1024 100 alpha 9 12`, using before/after binaries with the same feature set. `AVIF_PERF_RGBA16` forces the RGBA16 entry point even at 8-bit output; it also sets explicit depth metadata. `AVIF_PERF_ALPHA_QUALITY` overrides only the benchmark's alpha quality for mixed-item cases (same value before/after). `AVIF_PERF_PATTERN=flat` generates a constant colored image with alpha 128 or 255, while the default procedural pattern and existing raw-input option are unchanged. These are benchmark inputs/settings, not changes to production presets.

The committed reference-generator.rs runs only against the immutable baseline to produce 14 complete AVIF references: 8/10/12-bit color, lossless primary and/or alpha, opaque output and 12-bit grayscale. Baseline portable/asm generations match byte-for-byte. Tests compare full bytes with no external-tool or fixture skip. A separate direct/helper differential test covers 27 dimension/depth/speed combinations, including odd edges, full u16 input precision and speeds 0/6/10. Existing preflight, lossless limits, native headers and independent FFmpeg regressions remain required.

Default, asm and all-features release regression suites each pass **98 tests**, 0 failures/ignored. New required CI references include both the existing SIMD fixtures and 14 RGBA16 staging cases before full regressions. Format and scoped Clippy pass; strict Clippy still reports exactly the same three pre-existing collapsible_else_if warnings. CI outcomes are linked in the PR. Decoder remains unchanged at d3ea4e6.
