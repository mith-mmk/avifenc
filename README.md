[![crates.io](https://img.shields.io/crates/v/avifenc-rust)](https://crates.io/crates/avifenc-rust) ![license](https://img.shields.io/crates/l/avifenc-rust)
[Japanese](./README.ja.md)

# avifenc-rust

Pure Rust AVIF still-image encoding backed by the Rust `rav1e` AV1 encoder.
Version 0.1.2 fixes rectangular lossless syntax and monochrome headers, adds
optional SIMD, and reduces lossless staging allocations. Public APIs, portable
defaults and Rust 1.88 remain unchanged; the shared buffer stays at published
0.1.0. See [the changelog](CHANGELOG.md) and [distribution notes](SHARED_BUFFER.md).

The public vocabulary follows `avif-rust`: use `ImageBuffer` and
`Rgba16ImageBuffer` for RGBA input, `FrameBuffers` for native YUV input,
`ColorInformation` and `PixelInformation` for AVIF metadata, and
`ImageInformation`, `EncodeOptions`, and `encode` for callback-based
operation. Byte-oriented callers use `encode_bytes`, `encode_frame_bytes`, and
`encode_rgba16_bytes`.

The standalone crate accepts 8-, 10-, and 12-bit 4:0:0, 4:2:0, 4:2:2, and
4:4:4 native planes. RGBA alpha is emitted as an AVIF auxiliary alpha item.
The `wml2` adapter intentionally accepts only RGBA8 with standard sRGB
metadata; ICC and non-sRGB CICP requests are reported as errors there.

The compatibility metadata map accepts `quality`/`qcolor`/`min`/`max`,
`quality_alpha`/`qalpha`/`minalpha`/`maxalpha`, `speed`, `jobs`, `lossless`,
`tile_rows_log2`/`tilerowslog2`, and `tile_cols_log2`/`tilecolslog2`.
`advanced` accepts a comma- or semicolon-separated `key=value` list. The
backend supports rav1e controls such as `cdef`, `lrf`, `reduced-tx-set`,
`rdo-lookahead-frames`, `tune`, `low-latency`, and the corresponding partition,
transform, prediction, and motion boolean controls. These libavif spellings
are metadata aliases only; the Rust API keeps the `EncoderOptions` and
`EncodeOptions` naming used by `avif-rust`.

The `lossless` option uses the crate's private Pure Rust AV1 intra backend and
is verified bit-exact through FFmpeg. The primary and optional alpha items use
lossless 4:4:4 identity coding. Native 8/10/12-bit 4:0:0, 4:2:0, 4:2:2, and
4:4:4 planes are supported. `Rgba16ImageBuffer` values are scaled to the
selected 8/10/12-bit range and can use the same lossless path. ICC and
non-default CICP metadata combinations remain explicit
`EncoderError::Unsupported` cases for lossless native/identity coding. Lossy
paths use `rav1e`.

The existing `EncoderOptions::speed` field also controls the lossless RDO
search. Presets 0 through 6 retain the complete search used by the default;
7 through 10 progressively reduce intra-mode, angle, and palette probes.
All of these presets remain mathematically lossless, but faster presets can
produce larger AVIF files, matching libavif's speed/size trade-off.

The public-name comparison against `avif-rust` is recorded in
[`API_COMPATIBILITY.md`](API_COMPATIBILITY.md).

## Optional native SIMD

The default build remains portable Pure Rust. This branch adds an opt-in
`asm` feature forwarding to rav1e's existing native SIMD kernels:

```sh
cargo build --release --features asm
cargo run --release --features asm --example performance -- 128 128 60 alpha 9
```

On x86_64, NASM 2.15 or newer must be on PATH, together with the native
compiler/linker toolchain (tested with NASM 2.16.03 on Linux x86_64). Other
native targets follow rav1e's platform build requirements. No vendored
backend is introduced. The feature can be combined with `multithread`;
benchmark comparisons isolate SIMD with threading disabled.

SIMD dispatch detects CPU capabilities at runtime. With `asm`, 8-bit images
use rav1e's u8 context; 10/12-bit images keep u16. This matters because rav1e
selects its low/high depth kernels using the context's pixel type. The
portable path retains its existing u16 context. Search presets, quality,
color metadata and the lossless backend are unchanged.

On the measured AMD EPYC host, six lossy workloads took 41–52% less time
with identical complete encoded-byte hashes, including 8/10/12-bit inputs
and a natural-image fixture. This is an explicit build tradeoff: the example
executable grew from 2.41 to 4.16 MB and peak RSS rose despite fewer temporary
allocations. See [PERFORMANCE.md](PERFORMANCE.md) for raw repetitions, memory,
binary size, frozen reference tests and rejected experiments. These results
are scoped to the measured inputs and host; the feature is not enabled by
default.

## Encoding benchmark

The benchmark example accepts an RGBA8 raw input and reports encoding time,
output size, size ratio, and FFmpeg-decoded RGB/alpha error. It compares
several AVIF quality/speed presets with FFmpeg's `libwebp` encoder at matching
quality values and libavif's `avifenc` CLI:

```text
  cargo run --release --features multithread --example benchmark -- \
  input.rgba 1152 896 ffmpeg avifenc benchmark-output 1
```

The output is CSV on stdout with one row per `codec,case` pair (`avif`,
`libavif`, and `webp`). `mae_rgb`,
`max_rgb`, and `psnr_rgb` describe RGB degradation against the raw input;
`size_ratio` is encoded bytes divided by raw RGBA bytes. WebP encode timing
includes the FFmpeg process invocation, so it is an end-to-end reference and
not a direct comparison of the AVIF encoder's in-process core timing. The
libavif timing similarly includes the `avifenc` process invocation. The
libavif row uses `--yuv 420`, `--jobs all`, and matching `--qcolor`/`--qalpha`.
The default path enables PSNR-tuned pixel-domain RDO, full transform RDO, and
full segmentation search, then adaptively compares the preset's keyframe
intra-mode search with the complete keyframe search on faster presets. It also
uses bottom-up partition search for compression at every speed; the explicit
`tune=psychovisual` metadata option restores rav1e's perceptual tuning, while
`rdo_tx_decision=false,reduced_tx_set=true` restores the faster rav1e
transform preset. The benchmark also includes `q60-s6-fast`,
`q60-s6-psychovisual`, `q90-s10-rdo`, `q90-s10-rdo-psychovisual`, and
`q90-s9-rdo` to separate rav1e preset and tuning differences from the public
quality mapping when comparing against libavif.
The lossy path applies a content-independent codec-scale calibration to the
public `qcolor`/`qalpha` values at every speed. This compensates for the
different rav1e/libaom still-image rate/distortion curves; `q=100` remains the
separate mathematically-lossless path.
Grayscale RGBA input is encoded as a single YUV400 plane automatically; this
avoids three-channel coding for monochrome images while preserving the AVIF
container contract.
Use executable paths for FFmpeg and libavif as the fourth and fifth arguments
when they are not available on `PATH`.

For release hotspot profiling, add the `profile` feature to the same benchmark
command. The report is written to stderr after each encode and separates the
RGBA conversion, rav1e, container, and Pure Rust lossless-RDO phases. Nested
phase times are inclusive, so they must not be summed:

```text
  cargo run --release --features "multithread,profile" --example benchmark -- \
  input.rgba 1152 896 ffmpeg avifenc benchmark-output 1
```

The optional `multithread` feature enables rav1e's native threading backend
and parallelizes the independent base and complete keyframe candidates when
the default `jobs=all` setting is used. The feature is disabled by default so
targets such as WASM do not acquire native threading support implicitly.
Explicit `jobs` settings retain the serial comparison path; WML2 users can
opt in with its existing `multithread` feature together with `avifenc`.
