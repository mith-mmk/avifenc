use crate::PlaneBuffer;
use crate::error::EncoderError;

/// Validates one explicitly supplied native plane before it reaches the AV1
/// backend.  Luma/gray and alpha callers pass `(0, 0)`; only chroma planes
/// pass the selected subsampling exponents.
pub(crate) fn validate_native_plane(
    plane: &PlaneBuffer,
    expected_plane: u8,
    width: usize,
    height: usize,
    subsampling: (u8, u8),
    bit_depth: u8,
) -> Result<(), EncoderError> {
    if plane.layout.plane != expected_plane
        || plane.layout.width != width
        || plane.layout.height != height
        || plane.layout.subsampling_x != subsampling.0
        || plane.layout.subsampling_y != subsampling.1
    {
        return Err(EncoderError::InvalidParam(format!(
            "native plane {expected_plane} layout does not match the selected image layout"
        )));
    }
    let expected_samples = width.checked_mul(height).ok_or_else(|| {
        EncoderError::InvalidParam("native plane size overflows usize".to_string())
    })?;
    if plane.samples.len() != expected_samples || plane.layout.sample_count != expected_samples {
        return Err(EncoderError::InvalidParam(format!(
            "native plane {expected_plane} sample count does not match its dimensions"
        )));
    }
    let max_sample = 1u16 << bit_depth;
    if plane.samples.iter().any(|&sample| sample >= max_sample) {
        return Err(EncoderError::InvalidParam(format!(
            "native plane {expected_plane} contains a sample outside the {bit_depth}-bit range"
        )));
    }
    Ok(())
}
