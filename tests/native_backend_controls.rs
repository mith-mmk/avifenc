use std::collections::HashMap;

use avifenc_rust::{
    EncoderOptions, FrameBuffers, NativeColorInformation, NativeEncodeOptions, NativeSubsampling,
    NclxColorInformation, PixelChannelInformation, PixelInformation, PlaneBuffer, PlaneLayout,
};

fn buffers() -> FrameBuffers {
    let mut planes = (0..3)
        .map(|plane| PlaneBuffer {
            layout: PlaneLayout {
                plane,
                width: 2,
                height: 2,
                subsampling_x: 0,
                subsampling_y: 0,
                sample_count: 4,
            },
            samples: vec![32 + u16::from(plane); 4],
        })
        .collect::<Vec<_>>();
    planes.push(PlaneBuffer {
        layout: PlaneLayout {
            plane: 3,
            width: 2,
            height: 2,
            subsampling_x: 0,
            subsampling_y: 0,
            sample_count: 4,
        },
        samples: vec![128; 4],
    });
    FrameBuffers {
        width: 2,
        height: 2,
        planes,
    }
}

fn options() -> NativeEncodeOptions {
    NativeEncodeOptions {
        bit_depth: 8,
        subsampling: NativeSubsampling::Cs444,
        encoder: EncoderOptions {
            quality: 80,
            quality_alpha: 80,
            color_information: None,
            ..EncoderOptions::default()
        },
        color_information: NativeColorInformation::from_nclx(NclxColorInformation {
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range_flag: true,
        }),
    }
}

#[test]
fn native_lossless_controls_are_rejected_before_the_selected_backend() {
    for (quality, quality_alpha, expected_kind) in [
        (100, 80, "native primary"),
        (80, 100, "native alpha"),
        (100, 100, "native primary"),
    ] {
        for control in ["jobs", "tiling", "advanced"] {
            let mut native = options();
            native.encoder.quality = quality;
            native.encoder.quality_alpha = quality_alpha;
            match control {
                "jobs" => native.encoder.jobs = 1,
                "tiling" => native.encoder.tile_rows_log2 = 1,
                "advanced" => {
                    native.encoder.advanced = HashMap::from([(
                        "tune".to_string(),
                        avifenc_rust::DataMap::Ascii("psnr".to_string()),
                    )]);
                }
                _ => unreachable!(),
            }
            let error = avifenc_rust::encode_native_bytes(&buffers(), &native).unwrap_err();
            assert!(
                error.to_string().contains(expected_kind),
                "control={control}, error={error}"
            );
            assert!(matches!(error, avifenc_rust::EncoderError::Unsupported(_)));
        }
    }
}

#[test]
fn native_extended_pixi_is_rejected_without_starting_a_backend() {
    for (quality, quality_alpha) in [(80, 80), (100, 80), (80, 100)] {
        let mut native = options();
        native.encoder.quality = quality;
        native.encoder.quality_alpha = quality_alpha;
        native.encoder.pixel_information = Some(PixelInformation {
            bits_per_channel: vec![8, 8, 8],
            extended_channels: Some(vec![PixelChannelInformation {
                channel_idc: 1,
                component_format: 0,
                subsampling: None,
            }]),
        });
        let error = avifenc_rust::encode_native_bytes(&buffers(), &native).unwrap_err();
        assert!(matches!(error, avifenc_rust::EncoderError::Unsupported(_)));
        assert!(error.to_string().contains("extended pixi"));
    }
}

#[test]
fn native_lossless_and_mixed_backend_routes_remain_encodable() {
    for (quality, quality_alpha) in [(100, 80), (80, 100), (100, 100)] {
        let mut native = options();
        native.encoder.quality = quality;
        native.encoder.quality_alpha = quality_alpha;
        let source = buffers();
        let before = source.clone();
        let encoded = avifenc_rust::encode_native_bytes(&source, &native).unwrap();
        assert!(!encoded.is_empty());
        assert_eq!(source, before);
    }
}
