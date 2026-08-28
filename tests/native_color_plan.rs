use avifenc_rust::{
    EncoderOptions, FrameBuffers, NativeColorInformation, NativeEncodeOptions, NativeSubsampling,
    NclxColorInformation, PlaneBuffer, PlaneLayout,
};

fn nclx(matrix_coefficients: u16) -> NclxColorInformation {
    NclxColorInformation {
        color_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients,
        full_range_flag: true,
    }
}

fn buffers() -> FrameBuffers {
    FrameBuffers {
        width: 2,
        height: 2,
        planes: (0..3)
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
            .collect(),
    }
}

fn gray_buffers() -> FrameBuffers {
    FrameBuffers {
        width: 2,
        height: 2,
        planes: vec![PlaneBuffer {
            layout: PlaneLayout {
                plane: 0,
                width: 2,
                height: 2,
                subsampling_x: 0,
                subsampling_y: 0,
                sample_count: 4,
            },
            samples: vec![32; 4],
        }],
    }
}

fn options(color_information: NativeColorInformation) -> NativeEncodeOptions {
    NativeEncodeOptions {
        bit_depth: 8,
        subsampling: NativeSubsampling::Cs444,
        encoder: EncoderOptions {
            quality: 80,
            quality_alpha: 80,
            color_information: None,
            ..EncoderOptions::default()
        },
        color_information,
    }
}

#[test]
fn native_color_information_requires_explicit_cicp() {
    let profile_only = NativeColorInformation {
        nclx: None,
        icc_profile: Some(vec![1, 2, 3]),
        icc_color_type: Some(*b"prof"),
    };
    let error = avifenc_rust::encode_native_bytes(&buffers(), &options(profile_only)).unwrap_err();
    assert!(error.to_string().contains("explicit nclx"));
}

#[test]
fn nested_color_conflict_is_rejected_and_icc_is_preserved() {
    let native = NativeColorInformation {
        nclx: Some(nclx(6)),
        icc_profile: Some(vec![9, 8, 7, 6]),
        icc_color_type: Some(*b"rICC"),
    };
    let mut conflicting = options(native.clone());
    conflicting.encoder.color_information = Some(nclx(1).to_color_information());
    assert!(avifenc_rust::encode_native_bytes(&buffers(), &conflicting).is_err());

    let output = avifenc_rust::encode_native_bytes(&buffers(), &options(native)).unwrap();
    assert!(output.windows(4).any(|window| window == b"rICC"));
    assert!(output.windows(4).any(|window| window == [9, 8, 7, 6]));
}

#[test]
fn native_color_plan_rejects_unrepresentable_or_unsupported_cicp() {
    let mut too_wide = nclx(6);
    too_wide.color_primaries = 256;
    assert!(
        avifenc_rust::encode_native_bytes(
            &buffers(),
            &options(NativeColorInformation::from_nclx(too_wide)),
        )
        .is_err()
    );

    let mut unsupported = nclx(6);
    unsupported.transfer_characteristics = 3;
    assert!(
        avifenc_rust::encode_native_bytes(
            &buffers(),
            &options(NativeColorInformation::from_nclx(unsupported)),
        )
        .is_err()
    );

    let wrong_identity = NativeColorInformation::from_nclx(NclxColorInformation {
        color_primaries: 9,
        ..nclx(0)
    });
    assert!(avifenc_rust::encode_native_bytes(&buffers(), &options(wrong_identity)).is_err());

    let limited_identity = NativeColorInformation::from_nclx(NclxColorInformation {
        full_range_flag: false,
        ..nclx(0)
    });
    assert!(avifenc_rust::encode_native_bytes(&buffers(), &options(limited_identity)).is_err());

    let mut subsampled_identity = options(NativeColorInformation::from_nclx(nclx(0)));
    subsampled_identity.subsampling = NativeSubsampling::Cs420;
    assert!(avifenc_rust::encode_native_bytes(&buffers(), &subsampled_identity).is_err());

    let gray_identity = NativeColorInformation::from_nclx(nclx(0));
    let mut gray_options = options(gray_identity);
    gray_options.subsampling = NativeSubsampling::Cs400;
    assert!(avifenc_rust::encode_native_bytes(&gray_buffers(), &gray_options).is_err());
}
