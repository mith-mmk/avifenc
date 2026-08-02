//! Pure-Rust AV1 lossless still-image backend.

use crate::lossless_headers as headers;
use crate::lossless_tile::FrameEncoder;
use crate::lossless_types::LosslessPlanes;
use gamut_core::{Error, Result};

/// The AV1 temporal unit and sequence-header fields needed by the AVIF writer.
#[derive(Debug, Clone)]
pub(crate) struct EncodedStill {
    pub obus: Vec<u8>,
    pub config: headers::Av1StillConfig,
}

/// Encodes a full-resolution lossless plane set as one AV1 intra frame.
pub(crate) fn encode_still_lossless(planes: &LosslessPlanes<'_>) -> Result<EncodedStill> {
    if planes.width == 0 || planes.height == 0 {
        return Err(Error::InvalidInput("image has a zero dimension"));
    }
    if !matches!(planes.bit_depth, 8 | 10 | 12) {
        return Err(Error::Unsupported(
            "only 8, 10, and 12-bit lossless input is supported",
        ));
    }

    let seq_profile =
        if planes.bit_depth == 12 || planes.subsampling_x == 1 && planes.subsampling_y == 0 {
            2
        } else if planes.monochrome || planes.subsampling_x == 1 && planes.subsampling_y == 1 {
            0
        } else {
            1
        };
    let config = headers::Av1StillConfig {
        seq_profile,
        seq_level_idx_0: headers::pick_level(planes.width, planes.height)?,
        seq_tier_0: 0,
        high_bitdepth: planes.bit_depth > 8,
        twelve_bit: planes.bit_depth == 12,
        monochrome: planes.monochrome,
        chroma_subsampling_x: if planes.monochrome {
            1
        } else {
            planes.subsampling_x
        },
        chroma_subsampling_y: if planes.monochrome {
            1
        } else {
            planes.subsampling_y
        },
        chroma_sample_position: 0,
        color_primaries: planes.color_primaries,
        transfer_characteristics: planes.transfer_characteristics,
        matrix_coefficients: planes.matrix_coefficients,
        full_range: planes.full_range,
    };

    let mi_cols = 2 * ((planes.width + 7) >> 3);
    let mi_rows = 2 * ((planes.height + 7) >> 3);
    let seq_payload = headers::sequence_header_payload(&config, planes.width, planes.height);
    let mut frame_payload = headers::frame_header_payload(
        planes.width,
        planes.height,
        mi_cols,
        mi_rows,
        planes.monochrome,
    );
    frame_payload.extend_from_slice(&FrameEncoder::new(planes).encode());

    Ok(EncodedStill {
        obus: headers::assemble_temporal_unit(&seq_payload, &frame_payload),
        config,
    })
}
