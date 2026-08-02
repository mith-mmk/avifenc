# avif-rust API vocabulary review

`avifenc-rust` keeps the public vocabulary aligned with the sibling
`avif-rust` crate. The following table records the review for the 0.1.0 API.

| Concept | `avif-rust` | `avifenc-rust` | Compatibility decision |
| --- | --- | --- | --- |
| Callback options | `DecodeOptions` | `EncodeOptions` | Same `*Options` pairing; encode-specific callback type is explicit. |
| Callback entry point | `decode` | `encode` | Same operation verb. |
| Byte entry points | `decode_bytes` | `encode_bytes`, `encode_frame_bytes`, `encode_rgba16_bytes` | Existing `*_bytes` vocabulary is retained. |
| RGBA image | `ImageBuffer` | `ImageBuffer` | Same field names and RGBA byte layout. |
| High-bit RGBA image | `Rgba16ImageBuffer` | `Rgba16ImageBuffer` | Same field names and full-range `u16` layout. |
| Native planes | `PlaneLayout`, `PlaneBuffer`, `FrameBuffers` | `PlaneLayout`, `PlaneBuffer`, `FrameBuffers` | Same native-plane vocabulary and geometry fields. |
| Color metadata | `ColorInformation`, `NclxColorInformation` | `ColorInformation`, `NclxColorInformation` | Same `*Information` names and CICP representation. |
| Pixel metadata | `PixelInformation`, `PixelChannelInformation`, `PixelSubsampling` | Same names | No parallel aliases were introduced. |
| Metadata values | `DataMap`, `Metadata` | `DataMap`, `Metadata` | Same variants, including the existing `UIntAllay` spelling. |
| Callback response | `CallbackResponse`, `ResponseCommand` | Same names | Same abort/continue vocabulary. |
| Error type | `DecoderError` | `EncoderError` | Same classification and display categories for the opposite operation. |

The encoder-specific pull callback is named `PickCallback`, matching the
existing `encode_pick` operation used by the `wml2` adapter. No public
`EncoderConfig`, `encode_image`, or `ColorProfile` aliases are provided.

