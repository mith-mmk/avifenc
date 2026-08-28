use super::{
    NativeBackendKind, NativeEncodeOptions, encode_native_bytes, native_backend_entries,
    reset_native_backend_entries,
};
use crate::{
    EncoderError, EncoderOptions, FrameBuffers, NativeColorInformation, NclxColorInformation,
    PixelInformation, PlaneBuffer, PlaneLayout,
};

fn buffers(with_alpha: bool) -> FrameBuffers {
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
    if with_alpha {
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
    }
    FrameBuffers {
        width: 2,
        height: 2,
        planes,
    }
}

fn options() -> NativeEncodeOptions {
    NativeEncodeOptions {
        bit_depth: 8,
        subsampling: super::NativeSubsampling::Cs444,
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

fn assert_unsupported_before_backend(options: &NativeEncodeOptions, with_alpha: bool) {
    reset_native_backend_entries();
    let error = encode_native_bytes(&buffers(with_alpha), options).unwrap_err();
    assert!(
        matches!(error, EncoderError::Unsupported(_)),
        "error={error}"
    );
    let entries = native_backend_entries();
    assert_eq!(entries.primary, 0, "primary entry observed: {entries:?}");
    assert_eq!(entries.alpha, 0, "alpha entry observed: {entries:?}");
    assert_eq!(entries.order_len, 0, "backend order observed: {entries:?}");
}

#[test]
fn native_lossless_routes_are_rejected_before_any_backend_entry() {
    for (quality, quality_alpha, lossless) in [
        (100, 80, false),
        (80, 100, false),
        (100, 100, false),
        (80, 80, true),
    ] {
        for control in 0..4 {
            let mut native = options();
            native.encoder.quality = quality;
            native.encoder.quality_alpha = quality_alpha;
            native.encoder.lossless = lossless;
            match control {
                0 => native.encoder.jobs = 1,
                1 => native.encoder.tile_rows_log2 = 1,
                2 => native.encoder.tile_cols_log2 = 1,
                3 => {
                    native.encoder.advanced.insert(
                        "tune".to_string(),
                        crate::DataMap::Ascii("psnr".to_string()),
                    );
                }
                _ => unreachable!(),
            }
            assert_unsupported_before_backend(&native, true);
        }
    }
}

#[test]
fn native_backend_entry_order_is_primary_then_selected_alpha() {
    let mut native = options();
    native.encoder.quality = 100;
    native.encoder.quality_alpha = 100;
    reset_native_backend_entries();
    assert!(
        !encode_native_bytes(&buffers(true), &native)
            .unwrap()
            .is_empty()
    );
    let entries = native_backend_entries();
    assert_eq!(entries.primary, 1);
    assert_eq!(entries.alpha, 1);
    assert_eq!(entries.order_len, 2);
    assert_eq!(
        entries.order,
        [NativeBackendKind::Primary, NativeBackendKind::Alpha]
    );
}

#[test]
fn absent_alpha_does_not_probe_alpha_lossless_backend() {
    let mut native = options();
    native.encoder.quality = 80;
    native.encoder.quality_alpha = 100;
    native.encoder.jobs = 1;
    reset_native_backend_entries();
    assert!(
        !encode_native_bytes(&buffers(false), &native)
            .unwrap()
            .is_empty()
    );
    let entries = native_backend_entries();
    assert_eq!(entries.primary, 1);
    assert_eq!(entries.alpha, 0);
    assert_eq!(entries.order, [NativeBackendKind::Primary; 2]);
}

#[test]
fn native_pixi_depth_mismatch_is_rejected_before_backend_entry() {
    let mut native = options();
    native.encoder.pixel_information = Some(PixelInformation {
        bits_per_channel: vec![8, 10, 8],
        extended_channels: None,
    });
    reset_native_backend_entries();
    let error = encode_native_bytes(&buffers(true), &native).unwrap_err();
    assert!(
        matches!(error, EncoderError::InvalidParam(_)),
        "error={error}"
    );
    let entries = native_backend_entries();
    assert_eq!(entries.order_len, 0, "backend order observed: {entries:?}");
}
