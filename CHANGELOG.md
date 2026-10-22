# Changelog

## 0.1.2 - 2026-10-22

- Fix terminal HORZ/VERT rectangular partition syntax and rectangular palette
  presence/context signaling, including 12-bit lossless streams rejected by
  independent decoders. Corrected encoded bytes intentionally differ from
  affected 0.1.0 output; valid native sample values are preserved exactly.
- Normalize monochrome av1C subsampling to match sequence headers across native,
  RGBA/RGBA16, primary and alpha encoding routes.
- Add opt-in `asm` for rav1e native SIMD, selecting u8 storage for 8-bit kernels
  and u16 for higher depths. Portable defaults and quality/search presets stay
  unchanged. x86 asm builds require NASM and native build tools.
- Remove redundant RGBA8/RGBA16 lossless staging; affected RGBA16 paths save one
  allocation and six requested bytes per pixel while preserving output bytes
  aside from the independent syntax/header corrections above.
- Preserve public APIs, MSRV Rust 1.88 and shared avif-image-buffer 0.1.0.
- Package source and synthetic regression fixtures; exclude benchmark input
  data and measurement artifacts from the published archive.

## 0.1.0 - 2026-10-17

- Add shared image-buffer views and explicit precision/color encode preflight.
- Correct alpha auxiliary references to point from alpha to primary.
- Publish versioned shared-buffer registry dependency and package metadata.

## 0.0.7 - 2026-08-31

### Changed

- Prepared the typed U8/U16 8/10/12-bit input path for WML2 high-bit-depth
  integration without inferring output depth or treating U16 as a 16-bit AV1
  stream.
- Kept explicit quantization and ICC/nclx metadata replacement separate from
  pixel conversion.

## 0.0.6 - 2026-08-07

- Added a Pure Rust AVIF still-image encoder backed by `rav1e`.
- Added compatible RGBA, native-plane, metadata, and callback APIs.
- Added 8-, 10-, and 12-bit 4:0:0, 4:2:0, 4:2:2, and 4:4:4 encoding.
- Added AVIF auxiliary alpha item output and `wml2` integration.
- Improved lossy still-image rate/distortion defaults with full segmentation
  search and adaptive keyframe intra-mode search, pixel-domain transform
  decisions, and bottom-up partition search; added validation for an
  incompatible rav1e transform-option combination.
- Added a limited qcolor/qalpha rate-distortion calibration for rav1e's
  slower middle-quality presets, bringing the q60 benchmark rate in line
  with current libavif without changing q75/q90 or speed-10 mapping.
- Added a private Pure Rust lossless AV1 intra backend for 8/10/12-bit
  4:0:0/4:2:0/4:2:2/4:4:4 native planes, including FFmpeg bit-exact
  verification for subsampled planes, RGBA16 scaling, and an auxiliary alpha
  item; unsupported metadata combinations remain explicit errors.
- Added the opt-in `multithread` feature. It enables rav1e's native threading
  backend and parallelizes the independent base and complete keyframe
  searches while keeping default/WASM builds single-threaded.
- Added lossless `EncoderOptions::speed` presets from 7 through 10. They reduce
  the lossless RDO search while preserving pixel-exact output; the default
  speed-6 search and its encoded bytes remain unchanged.
- Verified the crate with FFmpeg, the existing `avif-rust` decoder, Wasm checks,
  Clippy, and the `wml2` AVIF integration tests.
