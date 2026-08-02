# avifenc-rust

Pure Rust AVIF still-image encoding backed by the Rust `rav1e` AV1 encoder.

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

The public-name comparison against `avif-rust` is recorded in
[`API_COMPATIBILITY.md`](API_COMPATIBILITY.md).
