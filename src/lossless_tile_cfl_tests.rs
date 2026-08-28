use super::{FrameEncoder, LosslessPlanes};

#[test]
fn cfl_luma_deltas_keep_q3_precision_on_padded_420_edges() {
    let luma = vec![
        0, 37, 74, 111, 148, 202, 239, 20, 57, 94, 148, 185, 222, 3, 40,
    ];
    let chroma = vec![0u16; 6];
    let planes: Vec<&[u16]> = vec![&luma, &chroma, &chroma];
    let input = LosslessPlanes {
        width: 5,
        height: 3,
        bit_depth: 8,
        monochrome: false,
        subsampling_x: 1,
        subsampling_y: 1,
        color_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients: 6,
        full_range: true,
        planes,
    };
    let encoder = FrameEncoder::new(&input, false, false, 10);
    assert_eq!(
        encoder.cfl_luma_deltas(0, 0),
        [
            204, -228, 216, 216, 580, 148, -432, -432, 580, 148, -432, -432, 580, 148, -432, -432,
        ]
    );
}
