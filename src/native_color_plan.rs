use crate::error::EncoderError;
use crate::{ColorInformation, NativeColorInformation, NclxColorInformation};

/// Borrowed, validated colour authority for one native encode.
///
/// The CICP tuple is copied because it is a small value and is consumed by
/// both the AV1 header and container writers. ICC remains borrowed and opaque;
/// this plan never interprets or rewrites profile bytes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeColorPlan<'a> {
    pub(crate) nclx: NclxColorInformation,
    pub(crate) icc_profile: Option<&'a [u8]>,
    pub(crate) icc_color_type: Option<[u8; 4]>,
}

pub(crate) fn checked_native_color_plan<'a>(
    native: &'a NativeColorInformation,
    nested: Option<&ColorInformation>,
    monochrome: bool,
    subsampling: (u8, u8),
) -> Result<NativeColorPlan<'a>, EncoderError> {
    let nclx = native.nclx.ok_or_else(|| {
        EncoderError::InvalidParam(
            "native encoding requires explicit nclx colour information; ICC cannot infer AV1 CICP"
                .to_string(),
        )
    })?;
    validate_nclx(nclx, monochrome, subsampling)?;

    let (icc_profile, icc_color_type) = match (&native.icc_profile, native.icc_color_type) {
        (Some(profile), Some(color_type)) => {
            if profile.is_empty() {
                return Err(EncoderError::InvalidParam(
                    "native ICC profile must not be empty".to_string(),
                ));
            }
            if color_type != *b"prof" && color_type != *b"rICC" {
                return Err(EncoderError::InvalidParam(
                    "native ICC color type must be prof or rICC".to_string(),
                ));
            }
            (Some(profile.as_slice()), Some(color_type))
        }
        (Some(_), None) => {
            return Err(EncoderError::InvalidParam(
                "native ICC profile requires an explicit prof or rICC type".to_string(),
            ));
        }
        (None, Some(_)) => {
            return Err(EncoderError::InvalidParam(
                "native ICC color type requires profile bytes".to_string(),
            ));
        }
        (None, None) => (None, None),
    };

    if let Some(explicit) = nested
        && !matches_native_color(explicit, nclx, icc_profile, icc_color_type)
    {
        return Err(EncoderError::InvalidParam(
            "nested EncoderOptions color_information conflicts with native colour authority"
                .to_string(),
        ));
    }

    Ok(NativeColorPlan {
        nclx,
        icc_profile,
        icc_color_type,
    })
}

fn validate_nclx(
    nclx: NclxColorInformation,
    monochrome: bool,
    subsampling: (u8, u8),
) -> Result<(), EncoderError> {
    if nclx.color_primaries > u8::MAX as u16
        || nclx.transfer_characteristics > u8::MAX as u16
        || nclx.matrix_coefficients > u8::MAX as u16
    {
        return Err(EncoderError::Unsupported(
            "native CICP values cannot be represented by the AV1 color_config fields".to_string(),
        ));
    }
    if nclx.color_primaries == 2
        || nclx.transfer_characteristics == 2
        || nclx.matrix_coefficients == 2
    {
        return Err(EncoderError::Unsupported(
            "CICP code 2 is the defined Unspecified value, but native encoding requires an explicit colour policy"
                .to_string(),
        ));
    }
    if !matches!(
        nclx.color_primaries,
        1 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 22
    ) {
        return Err(EncoderError::Unsupported(
            "native CICP color primaries are not representable by the encoder".to_string(),
        ));
    }
    if !matches!(
        nclx.transfer_characteristics,
        1 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18
    ) {
        return Err(EncoderError::Unsupported(
            "native CICP transfer characteristics are not representable by the encoder".to_string(),
        ));
    }
    if !matches!(
        nclx.matrix_coefficients,
        0 | 1 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14
    ) {
        return Err(EncoderError::Unsupported(
            "native CICP matrix coefficients are not representable by the encoder".to_string(),
        ));
    }
    if nclx.matrix_coefficients == 0 {
        if monochrome {
            return Err(EncoderError::Unsupported(
                "identity CICP is not valid in the AV1 monochrome color_config".to_string(),
            ));
        }
        if !nclx.full_range_flag || subsampling != (0, 0) {
            return Err(EncoderError::Unsupported(
                "identity CICP requires full range and a full-resolution native layout".to_string(),
            ));
        }
        if (nclx.color_primaries, nclx.transfer_characteristics) != (1, 13) {
            return Err(EncoderError::Unsupported(
                "the native identity shortcut requires the complete BT.709/sRGB tuple".to_string(),
            ));
        }
    }
    Ok(())
}

fn matches_native_color(
    explicit: &ColorInformation,
    nclx: NclxColorInformation,
    icc_profile: Option<&[u8]>,
    icc_color_type: Option<[u8; 4]>,
) -> bool {
    if explicit.color_type == *b"nclx" && explicit.payload == nclx.to_color_information().payload {
        return true;
    }
    icc_profile.is_some_and(|profile| {
        Some(explicit.color_type) == icc_color_type && explicit.payload == profile
    })
}

/// Native options use this only to seed the legacy backend's typed CICP
/// argument. The richer ICC bytes remain on `NativeColorPlan` for the
/// container writer.
pub(crate) fn native_nclx_color_information(plan: NativeColorPlan<'_>) -> ColorInformation {
    plan.nclx.to_color_information()
}
