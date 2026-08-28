use crate::PixelInformation;
use crate::compat::EncoderOptions;
use crate::error::EncoderError;

/// Checks controls that the pure-Rust lossless backend cannot represent.
///
/// This is deliberately independent of colour metadata: legacy callers keep
/// their existing colour restrictions, while the native route can preserve
/// its validated CICP/ICC authority and still share these backend limits.
pub(crate) fn validate_lossless_controls(
    options: &EncoderOptions,
    input_kind: &str,
) -> Result<(), EncoderError> {
    if options.jobs != 0
        || options.tile_rows_log2 != 0
        || options.tile_cols_log2 != 0
        || !options.advanced.is_empty()
    {
        return Err(EncoderError::Unsupported(format!(
            "the lossless {input_kind} backend does not expose jobs, tiling, or advanced controls"
        )));
    }
    Ok(())
}

/// Rejects semantic `pixi` extensions that the current native container
/// writer cannot retain. The legacy writer continues to use its historical
/// depth-only projection.
pub(crate) fn validate_native_pixi_extensions(
    pixi: Option<&PixelInformation>,
    input_kind: &str,
) -> Result<(), EncoderError> {
    if pixi.is_some_and(|pixi| pixi.extended_channels.is_some()) {
        return Err(EncoderError::Unsupported(format!(
            "native {input_kind} encoding does not preserve extended pixi channels"
        )));
    }
    Ok(())
}
