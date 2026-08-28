use avifenc_rust::{
    EncoderOptions, FrameBuffers, NativeColorInformation, NativeEncodeOptions, NativeSubsampling,
    NclxColorInformation, PlaneBuffer, PlaneLayout,
};

fn plane(
    id: u8,
    width: usize,
    height: usize,
    subsampling_x: u8,
    subsampling_y: u8,
    value: u16,
) -> PlaneBuffer {
    let sample_count = width.checked_mul(height).unwrap();
    PlaneBuffer {
        layout: PlaneLayout {
            plane: id,
            width,
            height,
            subsampling_x,
            subsampling_y,
            sample_count,
        },
        samples: vec![value; sample_count],
    }
}

fn options(bit_depth: u8, subsampling: NativeSubsampling) -> NativeEncodeOptions {
    NativeEncodeOptions {
        bit_depth,
        subsampling,
        encoder: EncoderOptions {
            quality: 80,
            quality_alpha: 80,
            color_information: None,
            ..EncoderOptions::default()
        },
        color_information: NativeColorInformation::from_nclx(NclxColorInformation {
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: if matches!(
                subsampling,
                NativeSubsampling::Cs400 | NativeSubsampling::Cs420 | NativeSubsampling::Cs422
            ) {
                6
            } else {
                0
            },
            full_range_flag: true,
        }),
    }
}

fn rgb_odd(
    width: usize,
    height: usize,
    subsampling: NativeSubsampling,
    value: u16,
) -> FrameBuffers {
    let (sx, sy) = match subsampling {
        NativeSubsampling::Cs420 => (1, 1),
        NativeSubsampling::Cs422 => (1, 0),
        NativeSubsampling::Cs444 => (0, 0),
        NativeSubsampling::Cs400 => unreachable!(),
    };
    let chroma_width = width.div_ceil(1usize << sx);
    let chroma_height = height.div_ceil(1usize << sy);
    FrameBuffers {
        width,
        height,
        planes: vec![
            plane(0, width, height, 0, 0, value),
            plane(1, chroma_width, chroma_height, sx, sy, value),
            plane(2, chroma_width, chroma_height, sx, sy, value),
        ],
    }
}

#[test]
fn odd_dimensions_420_keep_luma_full_resolution() {
    let buffers = rgb_odd(3, 5, NativeSubsampling::Cs420, 32);
    let result = avifenc_rust::encode_native_bytes(&buffers, &options(8, NativeSubsampling::Cs420));
    assert!(result.is_ok(), "4:2:0 native geometry rejected: {result:?}");
}

#[test]
fn odd_dimensions_422_keep_luma_full_resolution() {
    let buffers = rgb_odd(5, 3, NativeSubsampling::Cs422, 32);
    let result = avifenc_rust::encode_native_bytes(&buffers, &options(8, NativeSubsampling::Cs422));
    assert!(result.is_ok(), "4:2:2 native geometry rejected: {result:?}");
}

#[test]
fn four_four_four_and_gray_alpha_use_full_resolution() {
    let rgb = rgb_odd(3, 5, NativeSubsampling::Cs444, 32);
    assert!(avifenc_rust::encode_native_bytes(&rgb, &options(8, NativeSubsampling::Cs444)).is_ok());

    let gray = FrameBuffers {
        width: 3,
        height: 5,
        planes: vec![plane(0, 3, 5, 0, 0, 32), plane(3, 3, 5, 0, 0, 255)],
    };
    assert!(
        avifenc_rust::encode_native_bytes(&gray, &options(8, NativeSubsampling::Cs400)).is_ok()
    );
}

#[test]
fn explicit_dark_twelve_bit_samples_are_accepted() {
    for bit_depth in [8, 10, 12] {
        let buffers = rgb_odd(3, 3, NativeSubsampling::Cs420, 1);
        assert!(
            avifenc_rust::encode_native_bytes(
                &buffers,
                &options(bit_depth, NativeSubsampling::Cs420)
            )
            .is_ok(),
            "explicit {bit_depth}-bit dark samples were rejected"
        );
    }
}

#[test]
fn luma_subsampling_id_count_and_depth_are_rejected() {
    let mut buffers = rgb_odd(3, 5, NativeSubsampling::Cs420, 32);
    buffers.planes[0].layout.subsampling_x = 1;
    assert!(
        avifenc_rust::encode_native_bytes(&buffers, &options(8, NativeSubsampling::Cs420)).is_err()
    );

    let mut buffers = rgb_odd(3, 5, NativeSubsampling::Cs420, 32);
    buffers.planes[1].layout.plane = 2;
    assert!(
        avifenc_rust::encode_native_bytes(&buffers, &options(8, NativeSubsampling::Cs420)).is_err()
    );

    let mut buffers = rgb_odd(3, 5, NativeSubsampling::Cs420, 32);
    buffers.planes[1].layout.sample_count -= 1;
    assert!(
        avifenc_rust::encode_native_bytes(&buffers, &options(8, NativeSubsampling::Cs420)).is_err()
    );

    let buffers = rgb_odd(3, 5, NativeSubsampling::Cs420, 32);
    assert!(
        avifenc_rust::encode_native_bytes(&buffers, &options(9, NativeSubsampling::Cs420)).is_err()
    );

    let mut buffers = rgb_odd(3, 5, NativeSubsampling::Cs420, 32);
    buffers.planes[0].samples[0] = 4096;
    assert!(
        avifenc_rust::encode_native_bytes(&buffers, &options(12, NativeSubsampling::Cs420))
            .is_err()
    );
}
