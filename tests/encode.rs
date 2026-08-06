use avifenc_rust::{
    ColorInformation, EncodeOptions, EncoderError, EncoderOptions, FrameBuffers, ImageBuffer,
    ImageInformation, Metadata, NclxColorInformation, PickCallback, PickOptions, PixelInformation,
    PlaneBuffer, PlaneLayout, Rgba16ImageBuffer, encode, encode_bytes, encode_frame_bytes,
    encode_rgba16_bytes,
};
use std::collections::HashMap;

fn plane(
    plane: u8,
    width: usize,
    height: usize,
    subsampling_x: u8,
    subsampling_y: u8,
    samples: Vec<u16>,
) -> PlaneBuffer {
    PlaneBuffer {
        layout: PlaneLayout {
            plane,
            width,
            height,
            subsampling_x,
            subsampling_y,
            sample_count: width * height,
        },
        samples,
    }
}

#[test]
fn rgba_alpha_is_written_as_an_auxiliary_item() {
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![
            255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 255, 255, 255, 0,
        ],
    };
    let encoded = encode_bytes(&image, &EncoderOptions::default()).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"auxC"));
    assert!(encoded.windows(4).any(|window| window == b"iref"));
    assert!(encoded.windows(4).any(|window| window == b"mdat"));
}

#[test]
fn parallel_complete_keyframe_search_matches_serial_search() {
    let width = 16;
    let height = 16;
    let rgba = (0..width * height)
        .flat_map(|index| {
            let value = (index as u8).wrapping_mul(17);
            [value, value.rotate_left(1), value.rotate_left(2), u8::MAX]
        })
        .collect();
    let image = ImageBuffer {
        width,
        height,
        rgba,
    };
    let parallel = EncoderOptions {
        quality: 90,
        quality_alpha: 90,
        speed: 10,
        jobs: 0,
        ..EncoderOptions::default()
    };
    let serial = EncoderOptions {
        jobs: 1,
        ..parallel.clone()
    };

    assert_eq!(
        encode_bytes(&image, &parallel).unwrap(),
        encode_bytes(&image, &serial).unwrap()
    );
}

#[test]
fn grayscale_rgba_uses_a_monochrome_primary_item() {
    let rgba = [0u8, 32, 96, 255, 255, 128, 64, 16]
        .into_iter()
        .flat_map(|value| [value, value, value, u8::MAX])
        .collect();
    let image = ImageBuffer {
        width: 4,
        height: 2,
        rgba,
    };
    let options = EncoderOptions {
        lossless: true,
        ..EncoderOptions::default()
    };
    let encoded = encode_bytes(&image, &options).unwrap();
    let pixi = encoded
        .windows(4)
        .position(|window| window == b"pixi")
        .expect("primary pixi box");
    assert_eq!(encoded[pixi + 8], 1);
}

#[test]
fn monochrome_bilevel_blocks_use_compact_lossless_palette_coding() {
    let width: usize = 128;
    let height: usize = 128;
    let rgba = (0..width * height)
        .flat_map(|index| {
            let value = if (index / width / 8 + index % width / 8).is_multiple_of(2usize) {
                0
            } else {
                u8::MAX
            };
            [value, value, value, u8::MAX]
        })
        .collect();
    let image = ImageBuffer {
        width,
        height,
        rgba,
    };
    let encoded = encode_bytes(
        &image,
        &EncoderOptions {
            lossless: true,
            ..EncoderOptions::default()
        },
    )
    .unwrap();
    assert!(encoded.len() < 8 * 1024);
}

#[test]
fn full_resolution_color_blocks_can_use_lossless_palette_coding() {
    let width: usize = 128;
    let height: usize = 128;
    let rgba = (0..width * height)
        .flat_map(|index| {
            let green = if (index / width / 8 + index % width / 8).is_multiple_of(2usize) {
                0
            } else {
                u8::MAX
            };
            [32, green, 128, u8::MAX]
        })
        .collect();
    let encoded = encode_bytes(
        &ImageBuffer {
            width,
            height,
            rgba,
        },
        &EncoderOptions {
            lossless: true,
            ..EncoderOptions::default()
        },
    )
    .unwrap();
    assert!(encoded.len() < 1024);
}

#[test]
fn lossless_palette_coding_supports_more_than_two_colors() {
    let width: usize = 128;
    let height: usize = 128;
    let rgba = (0..width * height)
        .flat_map(|index| {
            let green = ((index / width / 8 + index % width / 8) % 4) as u8 * 85;
            [32, green, 128, u8::MAX]
        })
        .collect();
    let encoded = encode_bytes(
        &ImageBuffer {
            width,
            height,
            rgba,
        },
        &EncoderOptions {
            lossless: true,
            ..EncoderOptions::default()
        },
    )
    .unwrap();
    assert!(encoded.len() < 2 * 1024);
}

#[test]
fn native_444_uv_palette_coding_is_available() {
    let width = 64;
    let height = 64;
    let y = vec![128; width * height];
    let pairs = [(32u16, 64u16), (96, 128), (160, 192), (224, 32)];
    let mut u = Vec::with_capacity(width * height);
    let mut v = Vec::with_capacity(width * height);
    for y_pos in 0..height {
        for x_pos in 0..width {
            let (u_value, v_value) = pairs[(x_pos / 8 + y_pos / 8) % pairs.len()];
            u.push(u_value);
            v.push(v_value);
        }
    }
    let encoded = encode_frame_bytes(
        &FrameBuffers {
            width,
            height,
            planes: vec![
                plane(0, width, height, 0, 0, y),
                plane(1, width, height, 0, 0, u),
                plane(2, width, height, 0, 0, v),
            ],
        },
        &EncoderOptions {
            lossless: true,
            pixel_information: Some(PixelInformation {
                bits_per_channel: vec![8, 8, 8],
                extended_channels: None,
            }),
            ..EncoderOptions::default()
        },
    )
    .unwrap();
    assert!(encoded.len() < 2 * 1024);
}

#[test]
fn native_444_superblock_edge_uv_rdo_stays_compact() {
    let width = 96;
    let height = 64;
    let y = vec![128; width * height];
    let make_plane = |ax: i32, ay: i32| {
        (0..width * height)
            .map(|index| {
                let x = (index % width) as i32;
                let y = (index / width) as i32;
                let texture = (x * 3 + y * 5).rem_euclid(7) - 3;
                (128 + ax * (x - 48) / 4 + ay * (y - 32) / 4 + texture).clamp(0, 255) as u16
            })
            .collect::<Vec<_>>()
    };
    let encoded = encode_frame_bytes(
        &FrameBuffers {
            width,
            height,
            planes: vec![
                plane(0, width, height, 0, 0, y),
                plane(1, width, height, 0, 0, make_plane(-3, -3)),
                plane(2, width, height, 0, 0, make_plane(-2, -3)),
            ],
        },
        &EncoderOptions {
            lossless: true,
            pixel_information: Some(PixelInformation {
                bits_per_channel: vec![8, 8, 8],
                extended_channels: None,
            }),
            ..EncoderOptions::default()
        },
    )
    .unwrap();
    eprintln!("superblock-edge fixture bytes={}", encoded.len());
    assert!(encoded.len() < 32 * 1024);
}

#[test]
fn rgba_odd_dimensions_encode_without_chroma_padding_bias() {
    let image = ImageBuffer {
        width: 1,
        height: 1,
        rgba: vec![255, 0, 0, 255],
    };
    let encoded = encode_bytes(&image, &EncoderOptions::default()).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"av1C"));
}

#[test]
fn native_10_bit_422_planes_are_accepted() {
    let options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![10, 10, 10],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let buffers = FrameBuffers {
        width: 4,
        height: 2,
        planes: vec![
            plane(0, 4, 2, 0, 0, vec![0, 64, 128, 1023, 10, 20, 30, 40]),
            plane(1, 2, 2, 1, 0, vec![512, 512, 512, 512]),
            plane(2, 2, 2, 1, 0, vec![512, 512, 512, 512]),
        ],
    };
    let encoded = encode_frame_bytes(&buffers, &options).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"av1C"));
}

#[test]
fn native_8_bit_444_lossless_planes_are_accepted() {
    let width = 5;
    let height = 3;
    let y = (0..width * height)
        .map(|index| (index * 7) as u16)
        .collect();
    let u = (0..width * height)
        .map(|index| (128 + index) as u16)
        .collect();
    let v = (0..width * height)
        .map(|index| (255 - index) as u16)
        .collect();
    let buffers = FrameBuffers {
        width,
        height,
        planes: vec![
            plane(0, width, height, 0, 0, y),
            plane(1, width, height, 0, 0, u),
            plane(2, width, height, 0, 0, v),
        ],
    };
    let options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![8, 8, 8],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    assert!(!encode_frame_bytes(&buffers, &options).unwrap().is_empty());
}

#[test]
fn native_plane_shape_errors_are_reported() {
    let buffers = FrameBuffers {
        width: 3,
        height: 3,
        planes: vec![plane(0, 3, 3, 0, 0, vec![0; 8])],
    };
    let error = encode_frame_bytes(&buffers, &EncoderOptions::default()).unwrap_err();
    assert!(error.to_string().contains("samples"));
}

#[test]
fn native_pixi_channel_count_must_match_plane_count() {
    let buffers = FrameBuffers {
        width: 2,
        height: 2,
        planes: vec![plane(0, 2, 2, 0, 0, vec![0; 4])],
    };
    let options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![8, 8, 8],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let error = encode_frame_bytes(&buffers, &options).unwrap_err();
    assert!(error.to_string().contains("encoded image has 1"));
}

#[test]
fn rgba8_pixi_bit_depth_must_match_the_encoded_samples() {
    let options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![10, 10, 10],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    };
    let error = encode_bytes(&image, &options).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("encoded image uses 8-bit samples")
    );
}

#[test]
fn native_12_bit_444_and_gray_planes_are_accepted() {
    let options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12, 12, 12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let buffers = FrameBuffers {
        width: 3,
        height: 2,
        planes: vec![
            plane(0, 3, 2, 0, 0, vec![0, 1024, 4095, 100, 200, 300]),
            plane(1, 3, 2, 0, 0, vec![2048; 6]),
            plane(2, 3, 2, 0, 0, vec![2048; 6]),
        ],
    };
    let encoded = encode_frame_bytes(&buffers, &options).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"av1C"));

    let gray_options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let gray = FrameBuffers {
        width: 3,
        height: 2,
        planes: vec![plane(0, 3, 2, 0, 0, vec![0, 1024, 4095, 100, 200, 300])],
    };
    let gray_encoded = encode_frame_bytes(&gray, &gray_options).unwrap();
    assert!(gray_encoded.windows(4).any(|window| window == b"av1C"));
}

#[test]
fn native_high_bit_depth_lossless_planes_are_accepted() {
    let ten_options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![10, 10, 10],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let ten_buffers = FrameBuffers {
        width: 5,
        height: 3,
        planes: vec![
            plane(0, 5, 3, 0, 0, (0..15).map(|sample| sample * 71).collect()),
            plane(
                1,
                5,
                3,
                0,
                0,
                (0..15).map(|sample| 1023 - sample * 31).collect(),
            ),
            plane(2, 5, 3, 0, 0, (0..15).map(|sample| sample * 19).collect()),
        ],
    };
    assert!(
        !encode_frame_bytes(&ten_buffers, &ten_options)
            .unwrap()
            .is_empty()
    );

    let twelve_options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let twelve_buffers = FrameBuffers {
        width: 5,
        height: 3,
        planes: vec![plane(
            0,
            5,
            3,
            0,
            0,
            (0..15).map(|sample| sample * 271).collect(),
        )],
    };
    assert!(
        !encode_frame_bytes(&twelve_buffers, &twelve_options)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn native_subsampled_lossless_planes_are_accepted() {
    let options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![8, 8, 8],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    for (subsampling_x, subsampling_y, chroma_width, chroma_height) in [(1, 1, 4, 4), (1, 0, 4, 8)]
    {
        let buffers = FrameBuffers {
            width: 8,
            height: 8,
            planes: vec![
                plane(0, 8, 8, 0, 0, (0..64).map(|v| (v * 3) as u16).collect()),
                plane(
                    1,
                    chroma_width,
                    chroma_height,
                    subsampling_x,
                    subsampling_y,
                    (0..chroma_width * chroma_height)
                        .map(|v| (64 + v) as u16)
                        .collect(),
                ),
                plane(
                    2,
                    chroma_width,
                    chroma_height,
                    subsampling_x,
                    subsampling_y,
                    (0..chroma_width * chroma_height)
                        .map(|v| (192 - v) as u16)
                        .collect(),
                ),
            ],
        };
        let encoded = encode_frame_bytes(&buffers, &options).unwrap();
        assert!(encoded.windows(4).any(|window| window == b"av1C"));
    }
}

#[test]
fn rgba16_encoding_uses_the_high_bit_path() {
    let image = Rgba16ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![
            0, 16384, 32768, 65535, 65535, 32768, 16384, 32768, 1024, 2048, 4096, 8192, 8192, 4096,
            2048, 4096,
        ],
    };
    let options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12, 12, 12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let encoded = encode_rgba16_bytes(&image, &options).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"av1C"));
    assert!(encoded.windows(4).any(|window| window == b"auxC"));
}

#[test]
fn rgba16_lossless_encoding_uses_the_pure_rust_path() {
    let image = Rgba16ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![
            0, 16384, 32768, 65535, 65535, 32768, 16384, 32768, 1024, 2048, 4096, 8192, 8192, 4096,
            2048, 4096,
        ],
    };
    let options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12, 12, 12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let encoded = encode_rgba16_bytes(&image, &options).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"av1C"));
    assert!(encoded.windows(4).any(|window| window == b"auxC"));
}

#[test]
fn advanced_codec_options_are_forwarded_to_rav1e() {
    let options = EncoderOptions {
        advanced: HashMap::from([
            (
                "reduced-tx-set".to_string(),
                avifenc_rust::DataMap::Ascii("false".to_string()),
            ),
            (
                "fast-deblock".to_string(),
                avifenc_rust::DataMap::Ascii("false".to_string()),
            ),
            (
                "tune".to_string(),
                avifenc_rust::DataMap::Ascii("psnr".to_string()),
            ),
        ]),
        ..EncoderOptions::default()
    };
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    };
    encode_bytes(&image, &options).unwrap();
}

#[test]
fn opaque_rgba8_lossless_encoding_is_available() {
    let options = EncoderOptions {
        lossless: true,
        ..EncoderOptions::default()
    };
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    };
    assert!(!encode_bytes(&image, &options).unwrap().is_empty());
}

#[test]
fn rgba8_lossless_alpha_is_written_as_an_auxiliary_item() {
    let options = EncoderOptions {
        lossless: true,
        ..EncoderOptions::default()
    };
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: [255, 255, 255, 0].repeat(4),
    };
    let encoded = encode_bytes(&image, &options).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"auxC"));
}

#[test]
fn quality_99_remains_encodable_without_entering_rav1e_qindex_zero() {
    let options = EncoderOptions {
        quality: 99,
        quality_alpha: 99,
        ..EncoderOptions::default()
    };
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    };
    encode_bytes(&image, &options).unwrap();
}

#[test]
fn quality_100_uses_the_opaque_rgba8_lossless_path() {
    let options = EncoderOptions {
        quality: 100,
        ..EncoderOptions::default()
    };
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![255; 16],
    };
    assert!(!encode_bytes(&image, &options).unwrap().is_empty());
}

struct CallbackFixture {
    ended: bool,
    profile_metadata: Option<Metadata>,
    callback_metadata: Option<Metadata>,
}

impl PickCallback for CallbackFixture {
    fn encode_start(&mut self) -> Result<ImageInformation, EncoderError> {
        Ok(ImageInformation {
            width: 2,
            height: 2,
            metadata: self.profile_metadata.clone(),
        })
    }

    fn encode_pick(
        &mut self,
        _start_x: usize,
        _start_y: usize,
        width: usize,
        height: usize,
        _option: Option<PickOptions>,
    ) -> Result<Vec<u8>, EncoderError> {
        Ok(vec![255; width * height * 4])
    }

    fn encode_end(&mut self) -> Result<(), EncoderError> {
        self.ended = true;
        Ok(())
    }

    fn metadata(&mut self) -> Result<Option<Metadata>, EncoderError> {
        Ok(self.callback_metadata.clone())
    }
}

#[test]
fn callback_encode_keeps_the_encode_start_pick_end_order() {
    let mut callback = CallbackFixture {
        ended: false,
        profile_metadata: None,
        callback_metadata: None,
    };
    let mut options = EncodeOptions {
        debug_flag: 0,
        drawer: &mut callback,
        options: None,
    };
    encode(&mut options).unwrap();
    assert!(callback.ended);
}

#[test]
fn callback_metadata_layers_are_merged_with_explicit_options() {
    let mut callback = CallbackFixture {
        ended: false,
        profile_metadata: Some(HashMap::from([(
            "lossless".to_string(),
            avifenc_rust::DataMap::UInt(1),
        )])),
        callback_metadata: Some(HashMap::from([(
            "speed".to_string(),
            avifenc_rust::DataMap::UInt(6),
        )])),
    };
    let mut options = EncodeOptions {
        debug_flag: 0,
        drawer: &mut callback,
        options: Some(HashMap::from([(
            "qcolor".to_string(),
            avifenc_rust::DataMap::UInt(80),
        )])),
    };
    assert!(!encode(&mut options).unwrap().is_empty());
    assert!(callback.ended);
}

#[test]
fn standalone_color_metadata_is_written_without_wml2_srgb_restriction() {
    let options = EncoderOptions {
        color_information: Some(
            NclxColorInformation {
                color_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                full_range_flag: false,
            }
            .to_color_information(),
        ),
        ..EncoderOptions::default()
    };
    let image = ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![128; 16],
    };
    let encoded = encode_bytes(&image, &options).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"nclx"));

    let icc_options = EncoderOptions {
        color_information: Some(ColorInformation {
            color_type: *b"prof",
            payload: vec![1, 2, 3, 4],
        }),
        ..EncoderOptions::default()
    };
    let encoded = encode_bytes(&image, &icc_options).unwrap();
    assert!(encoded.windows(4).any(|window| window == b"prof"));
}
