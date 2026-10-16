use crate::encoder::EncodedFrame;
use crate::error::EncoderError;
use crate::profile::{Guard, Kind};
use crate::{
    ColorInformation, EncoderOptions, NativeColorInformation, NclxColorInformation,
    PixelInformation,
};

const PRIMARY_ITEM_ID: u16 = 1;
const ALPHA_ITEM_ID: u16 = 2;

struct MetaBoxInput<'a> {
    width: u32,
    height: u32,
    primary_offset: u32,
    primary_length: u32,
    alpha_length: u32,
    primary: &'a EncodedFrame,
    alpha: Option<&'a EncodedFrame>,
    item_count: u16,
    options: &'a EncoderOptions,
    rich_color: Option<&'a NativeColorInformation>,
    native_color: Option<&'a NativeColorSource<'a>>,
}

/// Borrowed native colour data passed from the checked encoder plan to both
/// metadata passes. The ICC slice is never cloned while sizing or assembling
/// the container; the final `ipco` payload is the owning output copy.
pub(crate) struct NativeColorSource<'a> {
    pub(crate) nclx: NclxColorInformation,
    pub(crate) icc_profile: Option<&'a [u8]>,
    pub(crate) icc_color_type: Option<[u8; 4]>,
}

pub(crate) fn write_avif(
    width: usize,
    height: usize,
    primary: &EncodedFrame,
    alpha: Option<&EncodedFrame>,
    options: &EncoderOptions,
) -> Result<Vec<u8>, EncoderError> {
    write_avif_with_color_set(width, height, primary, alpha, options, None)
}

pub(crate) fn write_avif_with_color_set(
    width: usize,
    height: usize,
    primary: &EncodedFrame,
    alpha: Option<&EncodedFrame>,
    options: &EncoderOptions,
    rich_color: Option<&NativeColorInformation>,
) -> Result<Vec<u8>, EncoderError> {
    write_avif_with_sources(width, height, primary, alpha, options, rich_color, None)
}

pub(crate) fn write_avif_with_native_color_source(
    width: usize,
    height: usize,
    primary: &EncodedFrame,
    alpha: Option<&EncodedFrame>,
    options: &EncoderOptions,
    native_color: &NativeColorSource<'_>,
) -> Result<Vec<u8>, EncoderError> {
    write_avif_with_sources(
        width,
        height,
        primary,
        alpha,
        options,
        None,
        Some(native_color),
    )
}

fn write_avif_with_sources(
    width: usize,
    height: usize,
    primary: &EncodedFrame,
    alpha: Option<&EncodedFrame>,
    options: &EncoderOptions,
    rich_color: Option<&NativeColorInformation>,
    native_color: Option<&NativeColorSource<'_>>,
) -> Result<Vec<u8>, EncoderError> {
    let _timing = Guard::new(Kind::Container);
    let width = u32::try_from(width)
        .map_err(|_| EncoderError::InvalidParam("image width exceeds u32".to_string()))?;
    let height = u32::try_from(height)
        .map_err(|_| EncoderError::InvalidParam("image height exceeds u32".to_string()))?;
    let ftyp = box_bytes(b"ftyp", &file_type_payload())?;
    let primary_length = u32::try_from(primary.frame_payload.len())
        .map_err(|_| EncoderError::InvalidParam("AVIF item payload exceeds u32".to_string()))?;
    let alpha_length = alpha
        .map(|frame| u32::try_from(frame.frame_payload.len()))
        .transpose()
        .map_err(|_| EncoderError::InvalidParam("AVIF alpha payload exceeds u32".to_string()))?;
    let item_count = if alpha.is_some() { 2 } else { 1 };
    let meta_size = meta_box(&MetaBoxInput {
        width,
        height,
        primary_offset: 0,
        primary_length,
        alpha_length: alpha_length.unwrap_or(0),
        primary,
        alpha,
        item_count,
        options,
        rich_color,
        native_color,
    })?
    .len();
    let primary_offset = ftyp
        .len()
        .checked_add(meta_size)
        .and_then(|offset| offset.checked_add(8))
        .ok_or_else(|| EncoderError::InvalidParam("AVIF offset overflows usize".to_string()))?;
    let primary_offset = u32::try_from(primary_offset)
        .map_err(|_| EncoderError::InvalidParam("AVIF offset exceeds u32".to_string()))?;
    let alpha_offset = primary_offset
        .checked_add(primary_length)
        .ok_or_else(|| EncoderError::InvalidParam("AVIF alpha offset overflows u32".to_string()))?;
    let meta = meta_box(&MetaBoxInput {
        width,
        height,
        primary_offset,
        primary_length,
        alpha_length: alpha_length.unwrap_or(0),
        primary,
        alpha,
        item_count,
        options,
        rich_color,
        native_color,
    })?;
    let mut output = Vec::with_capacity(
        ftyp.len()
            + meta.len()
            + 8
            + primary.frame_payload.len()
            + alpha.map_or(0, |frame| frame.frame_payload.len()),
    );
    output.extend_from_slice(&ftyp);
    output.extend_from_slice(&meta);
    let mut mdat_payload = Vec::with_capacity(
        primary.frame_payload.len() + alpha.map_or(0, |frame| frame.frame_payload.len()),
    );
    mdat_payload.extend_from_slice(&primary.frame_payload);
    if let Some(frame) = alpha {
        mdat_payload.extend_from_slice(&frame.frame_payload);
    }
    output.extend_from_slice(&box_bytes(b"mdat", &mdat_payload)?);
    let _ = alpha_offset;
    Ok(output)
}

fn file_type_payload() -> Vec<u8> {
    let mut payload = Vec::with_capacity(20);
    payload.extend_from_slice(b"avif");
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(b"avif");
    payload.extend_from_slice(b"mif1");
    payload.extend_from_slice(b"miaf");
    payload
}

fn meta_box(input: &MetaBoxInput<'_>) -> Result<Vec<u8>, EncoderError> {
    let hdlr = full_box(
        b"hdlr",
        0,
        0,
        &[
            0, 0, 0, 0, b'p', b'i', b'c', b't', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'P', 0,
        ],
    )?;
    let pitm = full_box(b"pitm", 0, 0, &PRIMARY_ITEM_ID.to_be_bytes())?;
    let iinf = item_info_box(input.item_count, input.alpha.is_some())?;
    let iloc = item_location_box(
        input.primary_offset,
        input.primary_length,
        input.primary_offset + input.primary_length,
        input.alpha_length,
        input.alpha.is_some(),
    )?;
    let iprp = item_properties_box(
        input.width,
        input.height,
        input.primary,
        input.alpha,
        input.options,
        input.rich_color,
        input.native_color,
    )?;
    let iref = if input.alpha.is_some() {
        Some(auxiliary_reference_box()?)
    } else {
        None
    };
    let mut payload = Vec::new();
    payload.extend_from_slice(&hdlr);
    payload.extend_from_slice(&pitm);
    payload.extend_from_slice(&iinf);
    payload.extend_from_slice(&iloc);
    if let Some(iref) = iref {
        payload.extend_from_slice(&iref);
    }
    payload.extend_from_slice(&iprp);
    full_box(b"meta", 0, 0, &payload)
}

fn item_info_box(item_count: u16, has_alpha: bool) -> Result<Vec<u8>, EncoderError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&item_count.to_be_bytes());
    payload.extend_from_slice(&item_info_entry(PRIMARY_ITEM_ID, b"Color")?);
    if has_alpha {
        payload.extend_from_slice(&item_info_entry(ALPHA_ITEM_ID, b"Alpha")?);
    }
    full_box(b"iinf", 0, 0, &payload)
}

fn item_info_entry(item_id: u16, name: &[u8]) -> Result<Vec<u8>, EncoderError> {
    let mut payload = vec![0, item_id as u8, 0, 0, b'a', b'v', b'0', b'1'];
    payload.extend_from_slice(name);
    payload.push(0);
    full_box(b"infe", 2, 0, &payload)
}

fn item_location_box(
    primary_offset: u32,
    primary_length: u32,
    alpha_offset: u32,
    alpha_length: u32,
    has_alpha: bool,
) -> Result<Vec<u8>, EncoderError> {
    let mut payload = vec![0x44, 0];
    payload.extend_from_slice(&(if has_alpha { 2u16 } else { 1u16 }).to_be_bytes());
    append_item_location(
        &mut payload,
        PRIMARY_ITEM_ID,
        primary_offset,
        primary_length,
    );
    if has_alpha {
        append_item_location(&mut payload, ALPHA_ITEM_ID, alpha_offset, alpha_length);
    }
    full_box(b"iloc", 0, 0, &payload)
}

fn append_item_location(payload: &mut Vec<u8>, item_id: u16, offset: u32, length: u32) {
    payload.extend_from_slice(&item_id.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&offset.to_be_bytes());
    payload.extend_from_slice(&length.to_be_bytes());
}

fn auxiliary_reference_box() -> Result<Vec<u8>, EncoderError> {
    let mut reference_payload = Vec::new();
    reference_payload.extend_from_slice(&ALPHA_ITEM_ID.to_be_bytes());
    reference_payload.extend_from_slice(&1u16.to_be_bytes());
    reference_payload.extend_from_slice(&PRIMARY_ITEM_ID.to_be_bytes());
    let reference = box_bytes(b"auxl", &reference_payload)?;
    full_box(b"iref", 0, 0, &reference)
}

fn item_properties_box(
    width: u32,
    height: u32,
    primary: &EncodedFrame,
    alpha: Option<&EncodedFrame>,
    options: &EncoderOptions,
    rich_color: Option<&NativeColorInformation>,
    native_color: Option<&NativeColorSource<'_>>,
) -> Result<Vec<u8>, EncoderError> {
    let primary_pixi = options
        .pixel_information
        .clone()
        .unwrap_or(PixelInformation {
            bits_per_channel: vec![8, 8, 8],
            extended_channels: None,
        });
    validate_pixi(&primary_pixi)?;
    let ispe = image_spatial_extents(width, height)?;
    let primary_pixi_box = pixel_information_box(&primary_pixi)?;
    let primary_av1c = box_bytes(b"av1C", &primary.sequence_header)?;
    let mut ipco_payload = Vec::new();
    let mut property_count = 0u8;
    let ispe_index = append_property(&mut ipco_payload, &ispe, &mut property_count)?;
    let primary_pixi_index =
        append_property(&mut ipco_payload, &primary_pixi_box, &mut property_count)?;
    let color_indices = append_color_properties(
        &mut ipco_payload,
        &mut property_count,
        options.color_information.as_ref(),
        rich_color,
        native_color,
    )?;
    let primary_av1c_index =
        append_property(&mut ipco_payload, &primary_av1c, &mut property_count)?;
    let mut primary_associations = vec![ispe_index, primary_pixi_index];
    primary_associations.extend(color_indices.iter().copied());
    primary_associations.push(primary_av1c_index);
    let mut alpha_associations = Vec::new();
    if let Some(alpha) = alpha {
        let alpha_pixi = PixelInformation {
            bits_per_channel: vec![primary_pixi.bits_per_channel[0]; alpha.pixel_channels as usize],
            extended_channels: None,
        };
        let alpha_pixi_box = pixel_information_box(&alpha_pixi)?;
        let alpha_av1c = box_bytes(b"av1C", &alpha.sequence_header)?;
        let auxc = full_box(
            b"auxC",
            0,
            0,
            b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0",
        )?;
        let alpha_pixi_index =
            append_property(&mut ipco_payload, &alpha_pixi_box, &mut property_count)?;
        let alpha_av1c_index =
            append_property(&mut ipco_payload, &alpha_av1c, &mut property_count)?;
        let auxc_index = append_property(&mut ipco_payload, &auxc, &mut property_count)?;
        alpha_associations.extend_from_slice(&[
            essential_property_index(ispe_index),
            essential_property_index(alpha_pixi_index),
            essential_property_index(alpha_av1c_index),
            essential_property_index(auxc_index),
        ]);
    }
    let ipco = box_bytes(b"ipco", &ipco_payload)?;
    let mut ipma_payload = Vec::new();
    ipma_payload.extend_from_slice(&(if alpha.is_some() { 2u32 } else { 1u32 }).to_be_bytes());
    ipma_payload.extend_from_slice(&PRIMARY_ITEM_ID.to_be_bytes());
    ipma_payload.push(
        u8::try_from(primary_associations.len())
            .map_err(|_| EncoderError::InvalidParam("too many primary properties".to_string()))?,
    );
    for index in primary_associations {
        ipma_payload.push(essential_property_index(index));
    }
    if alpha.is_some() {
        ipma_payload.extend_from_slice(&ALPHA_ITEM_ID.to_be_bytes());
        ipma_payload.push(alpha_associations.len() as u8);
        ipma_payload.extend_from_slice(&alpha_associations);
    }
    let ipma = full_box(b"ipma", 0, 0, &ipma_payload)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&ipco);
    payload.extend_from_slice(&ipma);
    box_bytes(b"iprp", &payload)
}

fn append_property(
    payload: &mut Vec<u8>,
    property: &[u8],
    property_count: &mut u8,
) -> Result<u8, EncoderError> {
    let index = property_count
        .checked_add(1)
        .ok_or_else(|| EncoderError::InvalidParam("too many item properties".to_string()))?;
    payload.extend_from_slice(property);
    *property_count = index;
    Ok(index)
}

fn essential_property_index(index: u8) -> u8 {
    index | 0x80
}

fn image_spatial_extents(width: u32, height: u32) -> Result<Vec<u8>, EncoderError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&width.to_be_bytes());
    payload.extend_from_slice(&height.to_be_bytes());
    full_box(b"ispe", 0, 0, &payload)
}

fn validate_pixi(pixi: &PixelInformation) -> Result<(), EncoderError> {
    if pixi.bits_per_channel.is_empty() || pixi.bits_per_channel.len() > u8::MAX as usize {
        return Err(EncoderError::InvalidParam(
            "pixi must contain between one and 255 channels".to_string(),
        ));
    }
    Ok(())
}

fn pixel_information_box(pixi: &PixelInformation) -> Result<Vec<u8>, EncoderError> {
    let mut payload = vec![pixi.bits_per_channel.len() as u8];
    payload.extend_from_slice(&pixi.bits_per_channel);
    full_box(b"pixi", 0, 0, &payload)
}

fn append_color_properties(
    output: &mut Vec<u8>,
    property_count: &mut u8,
    legacy: Option<&ColorInformation>,
    rich: Option<&NativeColorInformation>,
    native: Option<&NativeColorSource<'_>>,
) -> Result<Vec<u8>, EncoderError> {
    if let Some(native) = native {
        let mut indices = Vec::new();
        let nclx_payload = [
            (native.nclx.color_primaries >> 8) as u8,
            native.nclx.color_primaries as u8,
            (native.nclx.transfer_characteristics >> 8) as u8,
            native.nclx.transfer_characteristics as u8,
            (native.nclx.matrix_coefficients >> 8) as u8,
            native.nclx.matrix_coefficients as u8,
            if native.nclx.full_range_flag { 0x80 } else { 0 },
        ];
        indices.push(append_color_property(
            output,
            property_count,
            *b"nclx",
            &nclx_payload,
        )?);
        if let Some(profile) = native.icc_profile {
            let color_type = native.icc_color_type.unwrap_or(*b"prof");
            if color_type != *b"prof" && color_type != *b"rICC" {
                return Err(EncoderError::InvalidParam(
                    "native ICC color type must be prof or rICC".to_string(),
                ));
            }
            indices.push(append_color_property(
                output,
                property_count,
                color_type,
                profile,
            )?);
        }
        return Ok(indices);
    }
    let Some(rich) = rich else {
        let default_payload = [0, 1, 0, 13, 0, 6, 0x80];
        let (color_type, color_payload) = legacy
            .map_or((*b"nclx", &default_payload[..]), |color| {
                (color.color_type, color.payload.as_slice())
            });
        return append_color_property(output, property_count, color_type, color_payload)
            .map(|index| vec![index]);
    };
    let mut indices = Vec::new();
    if let Some(nclx) = rich.nclx {
        let nclx_payload = [
            (nclx.color_primaries >> 8) as u8,
            nclx.color_primaries as u8,
            (nclx.transfer_characteristics >> 8) as u8,
            nclx.transfer_characteristics as u8,
            (nclx.matrix_coefficients >> 8) as u8,
            nclx.matrix_coefficients as u8,
            if nclx.full_range_flag { 0x80 } else { 0 },
        ];
        indices.push(append_color_property(
            output,
            property_count,
            *b"nclx",
            &nclx_payload,
        )?);
    }
    if let Some(profile) = &rich.icc_profile {
        let color_type = rich.icc_color_type.unwrap_or(*b"prof");
        if color_type != *b"prof" && color_type != *b"rICC" {
            return Err(EncoderError::InvalidParam(
                "native ICC color type must be prof or rICC".to_string(),
            ));
        }
        indices.push(append_color_property(
            output,
            property_count,
            color_type,
            profile,
        )?);
    }
    if indices.is_empty() {
        let default_payload = [0, 1, 0, 13, 0, 6, 0x80];
        let (color_type, color_payload) = legacy
            .map_or((*b"nclx", &default_payload[..]), |color| {
                (color.color_type, color.payload.as_slice())
            });
        indices.push(append_color_property(
            output,
            property_count,
            color_type,
            color_payload,
        )?);
    }
    Ok(indices)
}

fn append_color_property(
    output: &mut Vec<u8>,
    property_count: &mut u8,
    color_type: [u8; 4],
    color_payload: &[u8],
) -> Result<u8, EncoderError> {
    let index = property_count
        .checked_add(1)
        .ok_or_else(|| EncoderError::InvalidParam("too many item properties".to_string()))?;
    let size = color_payload
        .len()
        .checked_add(12)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| EncoderError::InvalidParam("AVIF colr box exceeds u32".to_string()))?;
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(b"colr");
    output.extend_from_slice(&color_type);
    output.extend_from_slice(color_payload);
    *property_count = index;
    Ok(index)
}

fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Result<Vec<u8>, EncoderError> {
    let size = u32::try_from(payload.len() + 8)
        .map_err(|_| EncoderError::InvalidParam("AVIF box exceeds u32".to_string()))?;
    let mut output = Vec::with_capacity(payload.len() + 8);
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(payload);
    Ok(output)
}

fn full_box(
    kind: &[u8; 4],
    version: u8,
    flags: u32,
    payload: &[u8],
) -> Result<Vec<u8>, EncoderError> {
    let mut full_payload = Vec::with_capacity(4 + payload.len());
    full_payload.push(version);
    full_payload.extend_from_slice(&[(flags >> 16) as u8, (flags >> 8) as u8, flags as u8]);
    full_payload.extend_from_slice(payload);
    box_bytes(kind, &full_payload)
}
