use rav1e::prelude::{
    ChromaSampling, ColorDescription, ColorPrimaries, Config, EncoderConfig as Rav1eEncoderConfig,
    EncoderStatus, MatrixCoefficients, PixelRange, PredictionModesSetting, Rational,
    SegmentationLevel, TransferCharacteristics, Tune,
};

use crate::backend_capabilities::{validate_lossless_controls, validate_native_pixi_extensions};
use crate::compat::{DataMap, EncodeOptions, EncoderOptions, Metadata, PickOptions};
use crate::container::{NativeColorSource, write_avif, write_avif_with_native_color_source};
use crate::error::EncoderError;
use crate::lossless_backend::encode_still_lossless;
use crate::lossless_types::LosslessPlanes;
use crate::native_color_plan::{
    NativeColorPlan, checked_native_color_plan, native_nclx_color_information,
};
use crate::native_plan::validate_native_plane;
use crate::profile;
use crate::profile::{Guard, Kind};
use crate::{
    FrameBuffers, ImageBuffer, NativeColorInformation, PixelInformation, PlaneBuffer,
    Rgba16ImageBuffer,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sampling {
    Cs400,
    Cs420,
    Cs422,
    Cs444,
}

/// Explicit chroma layout for native encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSubsampling {
    /// Monochrome (4:0:0).
    Cs400,
    /// 4:2:0.
    Cs420,
    /// 4:2:2.
    Cs422,
    /// 4:4:4.
    Cs444,
}

impl NativeSubsampling {
    fn sampling(self) -> Sampling {
        match self {
            Self::Cs400 => Sampling::Cs400,
            Self::Cs420 => Sampling::Cs420,
            Self::Cs422 => Sampling::Cs422,
            Self::Cs444 => Sampling::Cs444,
        }
    }
}

/// Options for explicit native 8/10/12-bit AVIF encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeEncodeOptions {
    /// Explicit coded sample depth. Zero is invalid and forces the caller to
    /// choose 8, 10, or 12 bits instead of inferring from sample maxima.
    pub bit_depth: u8,
    /// Explicit native chroma layout.
    pub subsampling: NativeSubsampling,
    /// Backend quality and execution controls.
    pub encoder: EncoderOptions,
    /// Both ICC and CICP source metadata may be retained.
    pub color_information: NativeColorInformation,
}

impl Default for NativeEncodeOptions {
    fn default() -> Self {
        Self {
            bit_depth: 0,
            subsampling: NativeSubsampling::Cs444,
            encoder: EncoderOptions::default(),
            color_information: NativeColorInformation::default(),
        }
    }
}

struct PlaneInput<'a> {
    samples: &'a [u16],
    width: usize,
    height: usize,
}

pub(crate) struct EncodedFrame {
    pub sequence_header: Vec<u8>,
    pub frame_payload: Vec<u8>,
    pub pixel_channels: u8,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeBackendKind {
    Primary,
    Alpha,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeBackendEntries {
    primary: u8,
    alpha: u8,
    order: [NativeBackendKind; 2],
    order_len: usize,
}

#[cfg(test)]
impl Default for NativeBackendEntries {
    fn default() -> Self {
        Self {
            primary: 0,
            alpha: 0,
            order: [NativeBackendKind::Primary; 2],
            order_len: 0,
        }
    }
}

#[cfg(test)]
thread_local! {
    static NATIVE_BACKEND_ENTRIES: std::cell::Cell<NativeBackendEntries> =
        const { std::cell::Cell::new(NativeBackendEntries {
            primary: 0,
            alpha: 0,
            order: [NativeBackendKind::Primary; 2],
            order_len: 0,
        }) };
}

#[cfg(test)]
pub(crate) fn reset_native_backend_entries() {
    NATIVE_BACKEND_ENTRIES.with(|entries| entries.set(NativeBackendEntries::default()));
}

#[cfg(test)]
fn native_backend_entries() -> NativeBackendEntries {
    NATIVE_BACKEND_ENTRIES.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn native_backend_entry_count() -> usize {
    let entries = native_backend_entries();
    usize::from(entries.primary) + usize::from(entries.alpha)
}

#[cfg(test)]
fn record_native_backend_entry(kind: NativeBackendKind) {
    NATIVE_BACKEND_ENTRIES.with(|entries| {
        let mut current = entries.get();
        match kind {
            NativeBackendKind::Primary => current.primary = current.primary.saturating_add(1),
            NativeBackendKind::Alpha => current.alpha = current.alpha.saturating_add(1),
        }
        if current.order_len < current.order.len() {
            current.order[current.order_len] = kind;
            current.order_len += 1;
        }
        entries.set(current);
    });
}

/// Encodes one image supplied by a `PickCallback`.
pub fn encode(option: &mut EncodeOptions<'_>) -> Result<Vec<u8>, EncoderError> {
    let profile = option.drawer.encode_start()?;
    let result = (|| {
        let callback_metadata = option.drawer.metadata()?;
        if profile.width == 0 || profile.height == 0 {
            return Err(EncoderError::InvalidParam(
                "image dimensions must be non-zero".to_string(),
            ));
        }
        let data =
            option
                .drawer
                .encode_pick(0, 0, profile.width, profile.height, Some(PickOptions {}))?;
        let mut metadata = callback_metadata.unwrap_or_default();
        if let Some(profile_metadata) = profile.metadata.as_ref() {
            metadata.extend(profile_metadata.clone());
        }
        if let Some(option_metadata) = option.options.as_ref() {
            metadata.extend(option_metadata.clone());
        }
        encode_bytes(
            &ImageBuffer {
                width: profile.width,
                height: profile.height,
                rgba: data,
            },
            &options_from_metadata((!metadata.is_empty()).then_some(&metadata))?,
        )
    })();
    let end_result = option.drawer.encode_end();
    match (result, end_result) {
        (Ok(data), Ok(())) => Ok(data),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

/// Encodes an RGBA8 image into an AVIF still image.
pub fn encode_bytes(
    image: &ImageBuffer,
    options: &EncoderOptions,
) -> Result<Vec<u8>, EncoderError> {
    validate_rgba8(image)?;
    validate_options(options)?;
    validate_pixel_information(options, 3, Some(8))?;
    if color_lossless_requested(options) || alpha_lossless_requested(options) {
        let result = encode_rgba8_with_lossless_items(
            image,
            options,
            color_lossless_requested(options),
            alpha_lossless_requested(options),
        );
        profile::report();
        return result;
    }
    let grayscale = rgba8_is_grayscale(image);
    let (primary, alpha, channels) = if grayscale {
        let (gray, alpha) = rgba_to_gray(image);
        let planes = [PlaneInput {
            samples: &gray,
            width: image.width,
            height: image.height,
        }];
        let (primary, alpha) = encode_planes_with_alpha(
            image.width,
            image.height,
            &planes,
            Sampling::Cs400,
            8,
            options,
            alpha.as_deref(),
        )?;
        (primary, alpha, 1)
    } else {
        let (y, u, v, alpha) = rgba_to_yuv420(image);
        let planes = [
            PlaneInput {
                samples: &y,
                width: image.width,
                height: image.height,
            },
            PlaneInput {
                samples: &u,
                width: chroma_width(image.width),
                height: chroma_height(image.height),
            },
            PlaneInput {
                samples: &v,
                width: chroma_width(image.width),
                height: chroma_height(image.height),
            },
        ];
        let (primary, alpha) = encode_planes_with_alpha(
            image.width,
            image.height,
            &planes,
            Sampling::Cs420,
            8,
            options,
            alpha.as_deref(),
        )?;
        (primary, alpha, 3)
    };
    let effective = effective_image_options(options, channels, 8);
    let result = write_avif(
        image.width,
        image.height,
        &primary,
        alpha.as_ref(),
        &effective,
    );
    profile::report();
    result
}

/// Encodes native YUV planes into an AVIF still image.
pub fn encode_frame_bytes(
    buffers: &FrameBuffers,
    options: &EncoderOptions,
) -> Result<Vec<u8>, EncoderError> {
    validate_dimensions(buffers.width, buffers.height)?;
    validate_options(options)?;
    let sampling = sampling_from_planes(buffers)?;
    validate_pixel_information(options, buffers.planes.len(), None)?;
    let bit_depth = native_bit_depth(buffers, options)?;
    let planes = buffers
        .planes
        .iter()
        .map(plane_input)
        .collect::<Result<Vec<_>, _>>()?;
    if color_lossless_requested(options) {
        return encode_lossless_native_full(buffers, options, sampling, bit_depth, &planes);
    }
    let primary = encode_planes(
        buffers.width,
        buffers.height,
        &planes,
        sampling,
        bit_depth,
        options,
    )?;
    let effective = effective_options(options, buffers.planes.len(), bit_depth);
    write_avif(buffers.width, buffers.height, &primary, None, &effective)
}

/// Encodes explicit native planes with validated depth, chroma layout, alpha,
/// and colour metadata.
pub(crate) fn validate_shared_native_options(
    width: usize,
    height: usize,
    color_channels: usize,
    has_alpha: bool,
    options: &NativeEncodeOptions,
) -> Result<(), EncoderError> {
    validate_dimensions(width, height)?;
    validate_options(&options.encoder)?;
    if !matches!(options.bit_depth, 8 | 10 | 12) {
        return Err(EncoderError::InvalidParam(
            "native AVIF encoding requires8/10/12 bits".into(),
        ));
    }
    let subsampling = match options.subsampling {
        NativeSubsampling::Cs400 | NativeSubsampling::Cs444 => (0, 0),
        NativeSubsampling::Cs420 => (1, 1),
        NativeSubsampling::Cs422 => (1, 0),
    };
    let plan = checked_native_color_plan(
        &options.color_information,
        options.encoder.color_information.as_ref(),
        color_channels == 1,
        subsampling,
    )?;
    let effective = native_backend_options(options, plan);
    validate_pixel_information(&effective, color_channels, Some(options.bit_depth))?;
    validate_native_pixi_extensions(effective.pixel_information.as_ref(), "shared")?;
    validate_native_backend_settings(
        width,
        height,
        options.subsampling.sampling(),
        options.bit_depth,
        has_alpha,
        &effective,
    )?;
    Ok(())
}

/// Encodes explicit native planes with validated depth, chroma layout, alpha,
/// and colour metadata.
pub fn encode_native_bytes(
    buffers: &FrameBuffers,
    options: &NativeEncodeOptions,
) -> Result<Vec<u8>, EncoderError> {
    validate_dimensions(buffers.width, buffers.height)?;
    if !matches!(options.bit_depth, 8 | 10 | 12) {
        return Err(EncoderError::InvalidParam(
            "native AVIF encoding requires an explicit 8, 10, or 12-bit depth".to_string(),
        ));
    }
    validate_options(&options.encoder)?;
    let sampling = options.subsampling.sampling();
    let color_channels = if sampling == Sampling::Cs400 { 1 } else { 3 };
    let color_plan = checked_native_color_plan(
        &options.color_information,
        options.encoder.color_information.as_ref(),
        sampling == Sampling::Cs400,
        match sampling {
            Sampling::Cs400 | Sampling::Cs444 => (0, 0),
            Sampling::Cs420 => (1, 1),
            Sampling::Cs422 => (1, 0),
        },
    )?;
    if buffers.planes.len() != color_channels && buffers.planes.len() != color_channels + 1 {
        return Err(EncoderError::InvalidParam(format!(
            "native AVIF requires {color_channels} color planes plus optional alpha"
        )));
    }
    let expected_subsampling = match sampling {
        Sampling::Cs400 | Sampling::Cs444 => (0, 0),
        Sampling::Cs420 => (1, 1),
        Sampling::Cs422 => (1, 0),
    };
    let mut color_planes = Vec::with_capacity(color_channels);
    for (index, plane) in buffers.planes.iter().take(color_channels).enumerate() {
        let expected_width = if index == 0 {
            buffers.width
        } else {
            buffers.width.div_ceil(1usize << expected_subsampling.0)
        };
        let expected_height = if index == 0 {
            buffers.height
        } else {
            buffers.height.div_ceil(1usize << expected_subsampling.1)
        };
        validate_native_plane(
            plane,
            index as u8,
            expected_width,
            expected_height,
            if index == 0 {
                (0, 0)
            } else {
                expected_subsampling
            },
            options.bit_depth,
        )?;
        color_planes.push(PlaneInput {
            samples: &plane.samples,
            width: expected_width,
            height: expected_height,
        });
    }
    let alpha = if buffers.planes.len() == color_channels + 1 {
        let plane = &buffers.planes[color_channels];
        validate_native_plane(
            plane,
            3,
            buffers.width,
            buffers.height,
            (0, 0),
            options.bit_depth,
        )?;
        Some(plane.samples.as_slice())
    } else {
        None
    };

    let mut effective = native_backend_options(options, color_plan);
    if effective.pixel_information.is_none() {
        effective.pixel_information = Some(PixelInformation {
            bits_per_channel: vec![options.bit_depth; color_channels],
            extended_channels: None,
        });
    }
    validate_pixel_information(&effective, color_channels, Some(options.bit_depth))?;
    let primary_lossless = color_lossless_requested(&effective);
    validate_native_backend_settings(
        buffers.width,
        buffers.height,
        sampling,
        options.bit_depth,
        alpha.is_some(),
        &effective,
    )?;
    validate_native_pixi_extensions(effective.pixel_information.as_ref(), "native")?;
    #[cfg(test)]
    record_native_backend_entry(NativeBackendKind::Primary);
    let primary = if primary_lossless {
        encode_native_lossless_frame(
            buffers.width,
            buffers.height,
            &color_planes,
            sampling,
            options.bit_depth,
            &color_plan,
            effective.speed,
        )?
    } else {
        encode_planes(
            buffers.width,
            buffers.height,
            &color_planes,
            sampling,
            options.bit_depth,
            &effective,
        )?
    };
    let alpha_frame = alpha
        .map(|samples| {
            let alpha_options = native_alpha_backend_options(&effective, options.bit_depth);
            #[cfg(test)]
            record_native_backend_entry(NativeBackendKind::Alpha);
            let frame = if color_lossless_requested(&alpha_options) {
                let alpha_color = NativeColorInformation::from_nclx(crate::NclxColorInformation {
                    color_primaries: 1,
                    transfer_characteristics: 13,
                    matrix_coefficients: 6,
                    full_range_flag: true,
                });
                let alpha_plan = checked_native_color_plan(&alpha_color, None, true, (0, 0))?;
                encode_native_lossless_frame(
                    buffers.width,
                    buffers.height,
                    &[PlaneInput {
                        samples,
                        width: buffers.width,
                        height: buffers.height,
                    }],
                    Sampling::Cs400,
                    options.bit_depth,
                    &alpha_plan,
                    alpha_options.speed,
                )
            } else {
                encode_planes(
                    buffers.width,
                    buffers.height,
                    &[PlaneInput {
                        samples,
                        width: buffers.width,
                        height: buffers.height,
                    }],
                    Sampling::Cs400,
                    options.bit_depth,
                    &alpha_options,
                )
            }?;
            Ok::<_, EncoderError>(frame)
        })
        .transpose()?;
    let native_color = NativeColorSource {
        nclx: color_plan.nclx,
        icc_profile: color_plan.icc_profile,
        icc_color_type: color_plan.icc_color_type,
    };
    write_avif_with_native_color_source(
        buffers.width,
        buffers.height,
        &primary,
        alpha_frame.as_ref(),
        &effective,
        &native_color,
    )
}

fn normalize_container_frame(mut frame: EncodedFrame, sampling: Sampling) -> EncodedFrame {
    if sampling == Sampling::Cs400 && frame.sequence_header.len() >= 3 {
        // AV1's monochrome color_config infers both chroma subsampling flags
        // as one.  rav1e's compact container header may retain the luma-plane
        // layout (1,0) in lossy mode; every API must make av1C agree with
        // the actual sequence header without changing the supplied plane.
        frame.sequence_header[2] |= 0x0c;
    }
    frame
}

fn encode_native_lossless_frame(
    width: usize,
    height: usize,
    planes: &[PlaneInput<'_>],
    sampling: Sampling,
    bit_depth: u8,
    color: &NativeColorPlan<'_>,
    speed: u8,
) -> Result<EncodedFrame, EncoderError> {
    let (width, height) = validate_lossless_dimensions(width, height)?;
    let nclx = color.nclx;
    let plane_refs = planes.iter().map(|plane| plane.samples).collect();
    let lossless = LosslessPlanes {
        width,
        height,
        bit_depth,
        monochrome: sampling == Sampling::Cs400,
        subsampling_x: if matches!(sampling, Sampling::Cs420 | Sampling::Cs422) {
            1
        } else {
            0
        },
        subsampling_y: if sampling == Sampling::Cs420 { 1 } else { 0 },
        color_primaries: nclx.color_primaries,
        transfer_characteristics: nclx.transfer_characteristics,
        matrix_coefficients: nclx.matrix_coefficients,
        full_range: nclx.full_range_flag,
        planes: plane_refs,
    };
    encode_lossless_plane_set(&lossless, speed)
}

/// Encodes an RGBA16 image as a native 12-bit 4:2:0 image by default.
pub fn encode_rgba16_bytes(
    image: &Rgba16ImageBuffer,
    options: &EncoderOptions,
) -> Result<Vec<u8>, EncoderError> {
    validate_rgba16(image)?;
    validate_options(options)?;
    let bit_depth = options
        .pixel_information
        .as_ref()
        .and_then(|value| value.bits_per_channel.first().copied())
        .unwrap_or(12);
    if !matches!(bit_depth, 8 | 10 | 12) {
        return Err(EncoderError::InvalidParam(
            "RGBA16 encoding supports 8, 10, or 12 bits".to_string(),
        ));
    }
    validate_pixel_information(options, 3, Some(bit_depth))?;
    if color_lossless_requested(options) || alpha_lossless_requested(options) {
        return encode_rgba16_with_lossless_items(
            image,
            options,
            bit_depth,
            color_lossless_requested(options),
            alpha_lossless_requested(options),
        );
    }
    let grayscale = rgba16_is_grayscale(image);
    let (primary, alpha, channels) = if grayscale {
        let (gray, alpha) = rgba16_to_gray(image, bit_depth);
        let planes = [PlaneInput {
            samples: &gray,
            width: image.width,
            height: image.height,
        }];
        let (primary, alpha) = encode_planes_with_alpha(
            image.width,
            image.height,
            &planes,
            Sampling::Cs400,
            bit_depth,
            options,
            alpha.as_deref(),
        )?;
        (primary, alpha, 1)
    } else {
        let (y, u, v, alpha) = rgba16_to_yuv420(image, bit_depth);
        let planes = [
            PlaneInput {
                samples: &y,
                width: image.width,
                height: image.height,
            },
            PlaneInput {
                samples: &u,
                width: chroma_width(image.width),
                height: chroma_height(image.height),
            },
            PlaneInput {
                samples: &v,
                width: chroma_width(image.width),
                height: chroma_height(image.height),
            },
        ];
        let (primary, alpha) = encode_planes_with_alpha(
            image.width,
            image.height,
            &planes,
            Sampling::Cs420,
            bit_depth,
            options,
            alpha.as_deref(),
        )?;
        (primary, alpha, 3)
    };
    let effective = effective_image_options(options, channels, bit_depth);
    write_avif(
        image.width,
        image.height,
        &primary,
        alpha.as_ref(),
        &effective,
    )
}

fn encode_planes(
    width: usize,
    height: usize,
    planes: &[PlaneInput<'_>],
    sampling: Sampling,
    bit_depth: u8,
    options: &EncoderOptions,
) -> Result<EncodedFrame, EncoderError> {
    if !should_try_complete_keyframe_search(options) {
        return encode_planes_with_prediction(
            width, height, planes, sampling, bit_depth, options, false,
        );
    }
    #[cfg(feature = "multithread")]
    let (base, complete) = if options.jobs == 0 {
        encode_planes_parallel(width, height, planes, sampling, bit_depth, options)?
    } else {
        encode_planes_serial(width, height, planes, sampling, bit_depth, options)?
    };
    #[cfg(not(feature = "multithread"))]
    let (base, complete) =
        encode_planes_serial(width, height, planes, sampling, bit_depth, options)?;
    if complete.sequence_header.len() + complete.frame_payload.len()
        < base.sequence_header.len() + base.frame_payload.len()
    {
        Ok(complete)
    } else {
        Ok(base)
    }
}

fn encode_planes_serial(
    width: usize,
    height: usize,
    planes: &[PlaneInput<'_>],
    sampling: Sampling,
    bit_depth: u8,
    options: &EncoderOptions,
) -> Result<(EncodedFrame, EncodedFrame), EncoderError> {
    Ok((
        encode_planes_with_prediction(width, height, planes, sampling, bit_depth, options, false)?,
        encode_planes_with_prediction(width, height, planes, sampling, bit_depth, options, true)?,
    ))
}

fn encode_planes_with_alpha(
    width: usize,
    height: usize,
    planes: &[PlaneInput<'_>],
    sampling: Sampling,
    bit_depth: u8,
    options: &EncoderOptions,
    alpha: Option<&[u16]>,
) -> Result<(EncodedFrame, Option<EncodedFrame>), EncoderError> {
    let mut alpha_options = options.clone();
    alpha_options.quality = options.quality_alpha;
    alpha_options.color_information = None;
    let alpha_planes = alpha.map(|samples| {
        [PlaneInput {
            samples,
            width,
            height,
        }]
    });

    #[cfg(feature = "multithread")]
    if options.jobs == 0 {
        return std::thread::scope(|scope| {
            let primary_handle =
                scope.spawn(|| encode_planes(width, height, planes, sampling, bit_depth, options));
            let alpha_handle = alpha_planes.as_ref().map(|alpha_planes| {
                scope.spawn(|| {
                    encode_planes(
                        width,
                        height,
                        alpha_planes,
                        Sampling::Cs400,
                        bit_depth,
                        &alpha_options,
                    )
                })
            });
            let primary = primary_handle
                .join()
                .map_err(|_| EncoderError::Bitstream("color encode panicked".to_string()))??;
            let alpha = alpha_handle
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| EncoderError::Bitstream("alpha encode panicked".to_string()))?
                })
                .transpose()?;
            Ok((primary, alpha))
        });
    }

    let primary = encode_planes(width, height, planes, sampling, bit_depth, options)?;
    let alpha = alpha_planes
        .as_ref()
        .map(|alpha_planes| {
            encode_planes(
                width,
                height,
                alpha_planes,
                Sampling::Cs400,
                bit_depth,
                &alpha_options,
            )
        })
        .transpose()?;
    Ok((primary, alpha))
}

#[cfg(feature = "multithread")]
fn encode_planes_parallel(
    width: usize,
    height: usize,
    planes: &[PlaneInput<'_>],
    sampling: Sampling,
    bit_depth: u8,
    options: &EncoderOptions,
) -> Result<(EncodedFrame, EncodedFrame), EncoderError> {
    std::thread::scope(|scope| {
        let base_handle = scope.spawn(|| {
            encode_planes_with_prediction(
                width, height, planes, sampling, bit_depth, options, false,
            )
        });
        let complete_handle = scope.spawn(|| {
            encode_planes_with_prediction(width, height, planes, sampling, bit_depth, options, true)
        });
        let base = base_handle
            .join()
            .map_err(|_| EncoderError::Bitstream("base keyframe search panicked".to_string()))?;
        let complete = complete_handle.join().map_err(|_| {
            EncoderError::Bitstream("complete keyframe search panicked".to_string())
        })?;
        Ok::<_, EncoderError>((base?, complete?))
    })
}

fn encode_planes_with_prediction(
    width: usize,
    height: usize,
    planes: &[PlaneInput<'_>],
    sampling: Sampling,
    bit_depth: u8,
    options: &EncoderOptions,
    complete_keyframe_search: bool,
) -> Result<EncodedFrame, EncoderError> {
    let _timing = Guard::new(Kind::Rav1eEncode);
    let _variant = Guard::new(if complete_keyframe_search {
        Kind::Rav1eComplete
    } else {
        Kind::Rav1eBase
    });
    {
        let _validation = Guard::new(Kind::Rav1eValidation);
        if planes.len() != if sampling == Sampling::Cs400 { 1 } else { 3 } {
            return Err(EncoderError::InvalidParam(
                "plane count does not match chroma sampling".to_string(),
            ));
        }
        for (index, plane) in planes.iter().enumerate() {
            let expected = if index == 0 {
                (width, height)
            } else {
                chroma_dimensions(width, height, sampling)
            };
            if (plane.width, plane.height) != expected {
                return Err(EncoderError::InvalidParam(format!(
                    "plane {index} has {}x{}, expected {}x{}",
                    plane.width, plane.height, expected.0, expected.1
                )));
            }
            let expected_samples = plane.width.checked_mul(plane.height).ok_or_else(|| {
                EncoderError::InvalidParam("plane size overflows usize".to_string())
            })?;
            if plane.samples.len() != expected_samples {
                return Err(EncoderError::InvalidParam(format!(
                    "plane {index} has {} samples, expected {expected_samples}",
                    plane.samples.len()
                )));
            }
            let max_sample = (1u32 << u32::from(bit_depth)) - 1;
            if plane
                .samples
                .iter()
                .any(|&sample| u32::from(sample) > max_sample)
            {
                return Err(EncoderError::InvalidParam(format!(
                    "plane {index} contains a sample outside the {bit_depth}-bit range"
                )));
            }
        }
    }
    let config = {
        let _config = Guard::new(Kind::Rav1eConfig);
        validated_rav1e_config(
            width,
            height,
            sampling,
            bit_depth,
            options,
            complete_keyframe_search,
        )?
    };
    let mut context = {
        let _context = Guard::new(Kind::Rav1eContext);
        config
            .new_context::<u16>()
            .map_err(|error| EncoderError::InvalidParam(error.to_string()))?
    };
    let mut frame = {
        let _frame = Guard::new(Kind::Rav1eFrame);
        context.new_frame()
    };
    {
        let _copy = Guard::new(Kind::Rav1eCopy);
        for (index, plane) in planes.iter().enumerate() {
            let destination_stride = frame.planes[index].cfg.stride;
            let destination = frame.planes[index].data_origin_mut();
            for (destination_row, source_row) in destination
                .chunks_exact_mut(destination_stride)
                .take(plane.height)
                .zip(plane.samples.chunks_exact(plane.width))
            {
                destination_row[..plane.width].copy_from_slice(source_row);
            }
            // rav1e's plane padding takes the visible luma dimensions and applies
            // the plane's subsampling internally. Passing the already-subsampled
            // chroma dimensions leaves the right/bottom edge under-padded for odd
            // image sizes.
            frame.planes[index].pad(width, height);
        }
    }
    {
        let _submit = Guard::new(Kind::Rav1eSubmit);
        context
            .send_frame(frame)
            .map_err(|error| EncoderError::Bitstream(error.to_string()))?;
        context.flush();
    }

    let mut packet = None;
    {
        let _packets = Guard::new(Kind::Rav1ePackets);
        loop {
            match context.receive_packet() {
                Ok(value) => packet = Some(value.data),
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::LimitReached) => break,
                Err(error) => return Err(EncoderError::Bitstream(error.to_string())),
            }
        }
    }
    let frame_payload = packet.ok_or_else(|| {
        EncoderError::NotEnoughData("rav1e produced no AV1 frame packet".to_string())
    })?;
    Ok(normalize_container_frame(
        EncodedFrame {
            sequence_header: context.container_sequence_header().to_vec(),
            frame_payload,
            pixel_channels: planes.len() as u8,
        },
        sampling,
    ))
}

// This builds and validates settings only: no context, threads, frame or samples.
// Keep it identical for preflight and the actual rav1e path.
fn validated_rav1e_config(
    width: usize,
    height: usize,
    sampling: Sampling,
    bit_depth: u8,
    options: &EncoderOptions,
    complete_keyframe_search: bool,
) -> Result<Config, EncoderError> {
    let chroma_sampling = match sampling {
        Sampling::Cs400 => ChromaSampling::Cs400,
        Sampling::Cs420 => ChromaSampling::Cs420,
        Sampling::Cs422 => ChromaSampling::Cs422,
        Sampling::Cs444 => ChromaSampling::Cs444,
    };
    let mut config = Rav1eEncoderConfig::with_speed_preset(options.speed);
    config.width = width;
    config.height = height;
    config.bit_depth = usize::from(bit_depth);
    config.chroma_sampling = chroma_sampling;
    config.pixel_range = pixel_range(options);
    config.color_description = color_description(options);
    config.time_base = Rational::new(1, 1);
    config.still_picture = true;
    config.quantizer = quality_to_quantizer_for_options(options.quality, options.speed);
    config.min_quantizer = config.quantizer as u8;
    config.set_key_frame_interval(1, 1);
    config.tile_cols = 1usize << options.tile_cols_log2;
    config.tile_rows = 1usize << options.tile_rows_log2;
    apply_compression_defaults(&mut config, options);
    if complete_keyframe_search {
        config.speed_settings.prediction.prediction_modes =
            PredictionModesSetting::ComplexKeyframes;
    }
    apply_advanced(&mut config, &options.advanced)?;

    let config = Config::new()
        .with_encoder_config(config)
        .with_threads(options.jobs);
    config
        .validate()
        .map_err(|error| EncoderError::InvalidParam(error.to_string()))?;
    Ok(config)
}

fn validate_lossless_dimensions(width: usize, height: usize) -> Result<(u32, u32), EncoderError> {
    let width = u32::try_from(width)
        .map_err(|_| EncoderError::InvalidParam("image width exceeds u32".to_string()))?;
    let height = u32::try_from(height)
        .map_err(|_| EncoderError::InvalidParam("image height exceeds u32".to_string()))?;
    crate::lossless_headers::pick_level(width, height)
        .map_err(|error| EncoderError::Bitstream(error.to_string()))?;
    Ok((width, height))
}

fn native_alpha_backend_options(options: &EncoderOptions, bit_depth: u8) -> EncoderOptions {
    EncoderOptions {
        quality: options.quality_alpha,
        quality_alpha: options.quality_alpha,
        color_information: None,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![bit_depth],
            extended_channels: None,
        }),
        ..options.clone()
    }
}

fn validate_native_backend_settings(
    width: usize,
    height: usize,
    sampling: Sampling,
    bit_depth: u8,
    has_alpha: bool,
    options: &EncoderOptions,
) -> Result<(), EncoderError> {
    if color_lossless_requested(options) {
        validate_lossless_controls(options, "native primary")?;
        validate_lossless_dimensions(width, height)?;
    } else {
        validated_rav1e_config(width, height, sampling, bit_depth, options, false)?;
    }
    if has_alpha {
        if alpha_lossless_requested(options) {
            validate_lossless_controls(options, "native alpha")?;
            validate_lossless_dimensions(width, height)?;
        } else {
            let alpha_options = native_alpha_backend_options(options, bit_depth);
            validated_rav1e_config(
                width,
                height,
                Sampling::Cs400,
                bit_depth,
                &alpha_options,
                false,
            )?;
        }
    }
    Ok(())
}

fn apply_compression_defaults(config: &mut Rav1eEncoderConfig, options: &EncoderOptions) {
    // Match libavif's still-image comparison metric with rav1e's pixel-domain
    // PSNR RDO by default. The public advanced `tune` option is applied after
    // this function and can explicitly restore psychovisual tuning. Bottom-up
    // partition decisions and the compression-oriented transform settings are
    // kept on all presets unless the caller selects backend alternatives.
    config.tune = Tune::Psnr;
    let has_rdo_setting = options
        .advanced
        .keys()
        .any(|key| matches!(key.as_str(), "rdo-tx-decision" | "rdo_tx_decision"));
    let has_reduced_tx_setting = options
        .advanced
        .keys()
        .any(|key| matches!(key.as_str(), "reduced-tx-set" | "reduced_tx_set"));
    let has_tx_distortion_setting = options.advanced.keys().any(|key| {
        matches!(
            key.as_str(),
            "tx-domain-distortion" | "tx_domain_distortion"
        )
    });
    let tx_rate_enabled = options.advanced.iter().any(|(key, value)| {
        matches!(key.as_str(), "tx-domain-rate" | "tx_domain_rate")
            && match value {
                DataMap::UInt(value) => *value != 0,
                DataMap::Ascii(value) => value.eq_ignore_ascii_case("true"),
                _ => false,
            }
    });
    let has_bottomup_setting = options
        .advanced
        .keys()
        .any(|key| matches!(key.as_str(), "encode-bottomup" | "encode_bottomup"));
    if !has_rdo_setting {
        config.speed_settings.transform.rdo_tx_decision = true;
    }
    if !has_reduced_tx_setting {
        config.speed_settings.transform.reduced_tx_set = false;
    }
    config.speed_settings.segmentation = SegmentationLevel::Full;
    // rav1e's temporal-RDO path uses pixel-domain distortion. This is the
    // more compression-oriented choice for still images, and also matches
    // the libavif rate-distortion path used by the comparison benchmark.
    // Keep rav1e's default when tx-domain-rate is explicitly requested
    // because that backend option is not valid together with temporal RDO.
    if !has_tx_distortion_setting && !tx_rate_enabled {
        config.speed_settings.transform.tx_domain_distortion = false;
    }
    if !has_bottomup_setting {
        config.speed_settings.partition.encode_bottomup = true;
    }
}

fn should_try_complete_keyframe_search(options: &EncoderOptions) -> bool {
    if options.speed < 7 {
        return false;
    }
    for (key, value) in &options.advanced {
        if matches!(key.as_str(), "rdo-tx-decision" | "rdo_tx_decision")
            && let Ok(enabled) = metadata_bool(value, key)
            && !enabled
        {
            return false;
        }
        if matches!(key.as_str(), "reduced-tx-set" | "reduced_tx_set")
            && let Ok(enabled) = metadata_bool(value, key)
            && enabled
        {
            return false;
        }
    }
    true
}

fn validate_rgba8(image: &ImageBuffer) -> Result<(), EncoderError> {
    validate_dimensions(image.width, image.height)?;
    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            EncoderError::InvalidParam("RGBA buffer size overflows usize".to_string())
        })?;
    if image.rgba.len() != expected {
        return Err(EncoderError::InvalidParam(format!(
            "RGBA buffer has {} bytes, expected {expected}",
            image.rgba.len()
        )));
    }
    Ok(())
}

fn validate_rgba16(image: &Rgba16ImageBuffer) -> Result<(), EncoderError> {
    validate_dimensions(image.width, image.height)?;
    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            EncoderError::InvalidParam("RGBA16 buffer size overflows usize".to_string())
        })?;
    if image.rgba.len() != expected {
        return Err(EncoderError::InvalidParam(format!(
            "RGBA16 buffer has {} samples, expected {expected}",
            image.rgba.len()
        )));
    }
    Ok(())
}

fn validate_dimensions(width: usize, height: usize) -> Result<(), EncoderError> {
    if width == 0 || height == 0 {
        return Err(EncoderError::InvalidParam(
            "image dimensions must be non-zero".to_string(),
        ));
    }
    Ok(())
}

fn validate_options(options: &EncoderOptions) -> Result<(), EncoderError> {
    if options.quality > 100 || options.quality_alpha > 100 {
        return Err(EncoderError::InvalidParam(
            "quality and quality_alpha must be in 0..=100".to_string(),
        ));
    }
    if options.speed > 10 {
        return Err(EncoderError::InvalidParam(
            "speed must be in 0..=10".to_string(),
        ));
    }
    if options.tile_rows_log2 > 6 || options.tile_cols_log2 > 6 {
        return Err(EncoderError::InvalidParam(
            "tile log2 values must be in 0..=6".to_string(),
        ));
    }
    if let Some(pixi) = &options.pixel_information {
        if pixi.bits_per_channel.is_empty() || pixi.bits_per_channel.len() > 255 {
            return Err(EncoderError::InvalidParam(
                "pixi must contain between one and 255 channels".to_string(),
            ));
        }
        if pixi
            .bits_per_channel
            .iter()
            .any(|bits| !matches!(bits, 8 | 10 | 12))
        {
            return Err(EncoderError::Unsupported(
                "only 8, 10, and 12-bit AVIF input is supported".to_string(),
            ));
        }
        if pixi
            .bits_per_channel
            .windows(2)
            .any(|bits| bits[0] != bits[1])
        {
            return Err(EncoderError::InvalidParam(
                "all AVIF channels must use the same bit depth".to_string(),
            ));
        }
    }
    Ok(())
}

fn color_lossless_requested(options: &EncoderOptions) -> bool {
    options.lossless || options.quality == 100
}

fn alpha_lossless_requested(options: &EncoderOptions) -> bool {
    options.lossless || options.quality_alpha == 100
}

fn validate_lossless_options(
    options: &EncoderOptions,
    channels: usize,
    bit_depth: u8,
    input_kind: &str,
) -> Result<(), EncoderError> {
    validate_lossless_controls(options, input_kind)?;
    if let Some(pixi) = &options.pixel_information
        && (pixi.bits_per_channel.len() != channels
            || pixi.bits_per_channel.iter().any(|&bits| bits != bit_depth)
            || pixi.extended_channels.is_some())
    {
        return Err(EncoderError::Unsupported(format!(
            "the lossless {input_kind} backend emits {channels} {bit_depth}-bit channels without extended channels"
        )));
    }
    if let Some(color) = &options.color_information {
        if color.icc_profile().is_some() {
            return Err(EncoderError::Unsupported(format!(
                "the lossless {input_kind} backend does not preserve ICC profiles"
            )));
        }
        if let Some(nclx) = color.nclx()
            && !matches!(
                (
                    nclx.color_primaries,
                    nclx.transfer_characteristics,
                    nclx.matrix_coefficients,
                    nclx.full_range_flag,
                ),
                (1, 13, 0, true) | (1, 13, 6, true)
            )
        {
            return Err(EncoderError::Unsupported(format!(
                "the lossless {input_kind} backend requires full-range BT.709/sRGB CICP metadata"
            )));
        }
    }
    Ok(())
}

fn encode_rgba8_with_lossless_items(
    image: &ImageBuffer,
    options: &EncoderOptions,
    color_lossless: bool,
    alpha_lossless: bool,
) -> Result<Vec<u8>, EncoderError> {
    let has_alpha = image.rgba.chunks_exact(4).any(|pixel| pixel[3] != u8::MAX);
    let grayscale = rgba8_is_grayscale(image);
    if color_lossless {
        validate_lossless_options(options, if grayscale { 1 } else { 3 }, 8, "RGBA8 color")?;
    }

    let width = u32::try_from(image.width)
        .map_err(|_| EncoderError::InvalidParam("image width exceeds u32".to_string()))?;
    let height = u32::try_from(image.height)
        .map_err(|_| EncoderError::InvalidParam("image height exceeds u32".to_string()))?;
    #[cfg(feature = "multithread")]
    if has_alpha && color_lossless && alpha_lossless && options.jobs == 0 {
        return encode_rgba8_lossless_items_parallel(image, options, grayscale, width, height);
    }
    let primary = if color_lossless {
        if grayscale {
            encode_rgba8_gray_lossless(image, width, height, options.speed)?
        } else {
            encode_rgba8_rgb_lossless(image, width, height, options.speed)?
        }
    } else {
        if grayscale {
            let gray = image
                .rgba
                .chunks_exact(4)
                .map(|pixel| u16::from(pixel[0]))
                .collect::<Vec<_>>();
            let planes = [PlaneInput {
                samples: &gray,
                width: image.width,
                height: image.height,
            }];
            encode_planes(
                image.width,
                image.height,
                &planes,
                Sampling::Cs400,
                8,
                options,
            )?
        } else {
            let (y, u, v, _) = rgba_to_yuv420(image);
            let planes = [
                PlaneInput {
                    samples: &y,
                    width: image.width,
                    height: image.height,
                },
                PlaneInput {
                    samples: &u,
                    width: chroma_width(image.width),
                    height: chroma_height(image.height),
                },
                PlaneInput {
                    samples: &v,
                    width: chroma_width(image.width),
                    height: chroma_height(image.height),
                },
            ];
            encode_planes(
                image.width,
                image.height,
                &planes,
                Sampling::Cs420,
                8,
                options,
            )?
        }
    };
    let alpha = if has_alpha {
        if alpha_lossless {
            Some(encode_rgba8_alpha_lossless(image, options, width, height)?)
        } else {
            let alpha_samples = image
                .rgba
                .chunks_exact(4)
                .map(|pixel| u16::from(pixel[3]))
                .collect::<Vec<_>>();
            let alpha_options = {
                let mut value = options.clone();
                value.quality = options.quality_alpha;
                value.color_information = None;
                value
            };
            let alpha_plane = [PlaneInput {
                samples: &alpha_samples,
                width: image.width,
                height: image.height,
            }];
            Some(encode_planes(
                image.width,
                image.height,
                &alpha_plane,
                Sampling::Cs400,
                8,
                &alpha_options,
            )?)
        }
    } else {
        None
    };
    let mut effective = effective_image_options(options, if grayscale { 1 } else { 3 }, 8);
    if color_lossless {
        effective.color_information = Some(lossless_color_information());
    }
    write_avif(
        width as usize,
        height as usize,
        &primary,
        alpha.as_ref(),
        &effective,
    )
}

fn encode_rgba8_gray_lossless(
    image: &ImageBuffer,
    width: u32,
    height: u32,
    speed: u8,
) -> Result<EncodedFrame, EncoderError> {
    let gray = image
        .rgba
        .chunks_exact(4)
        .map(|pixel| u16::from(pixel[0]))
        .collect::<Vec<_>>();
    encode_lossless_gray_samples(width, height, &gray, 8, speed)
}

fn encode_rgba8_rgb_lossless(
    image: &ImageBuffer,
    width: u32,
    height: u32,
    speed: u8,
) -> Result<EncodedFrame, EncoderError> {
    // RGBA8 has already been validated. Build the identity-matrix GBR
    // planes directly instead of staging interleaved RGB at two precisions.
    let sample_count = image.rgba.len() / 4;
    let mut g = Vec::with_capacity(sample_count);
    let mut b = Vec::with_capacity(sample_count);
    let mut r = Vec::with_capacity(sample_count);
    for pixel in image.rgba.chunks_exact(4) {
        r.push(u16::from(pixel[0]));
        g.push(u16::from(pixel[1]));
        b.push(u16::from(pixel[2]));
    }
    encode_lossless_gbr_planes(width, height, 8, [&g, &b, &r], speed)
}

fn encode_rgba8_alpha_lossless(
    image: &ImageBuffer,
    options: &EncoderOptions,
    width: u32,
    height: u32,
) -> Result<EncodedFrame, EncoderError> {
    let alpha_samples = image
        .rgba
        .chunks_exact(4)
        .map(|pixel| u16::from(pixel[3]))
        .collect::<Vec<_>>();
    let mut alpha_options = options.clone();
    alpha_options.quality = options.quality_alpha;
    alpha_options.quality_alpha = options.quality_alpha;
    alpha_options.color_information = None;
    alpha_options.pixel_information = Some(PixelInformation {
        bits_per_channel: vec![8],
        extended_channels: None,
    });
    validate_lossless_options(&alpha_options, 1, 8, "RGBA8 alpha")?;
    encode_lossless_gray_samples(width, height, &alpha_samples, 8, options.speed)
}

#[cfg(feature = "multithread")]
fn encode_rgba8_lossless_items_parallel(
    image: &ImageBuffer,
    options: &EncoderOptions,
    grayscale: bool,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, EncoderError> {
    let (primary, alpha) = std::thread::scope(|scope| {
        let primary_handle = scope.spawn(|| {
            if grayscale {
                encode_rgba8_gray_lossless(image, width, height, options.speed)
            } else {
                encode_rgba8_rgb_lossless(image, width, height, options.speed)
            }
        });
        let alpha_handle =
            scope.spawn(|| encode_rgba8_alpha_lossless(image, options, width, height));
        let primary = primary_handle
            .join()
            .map_err(|_| EncoderError::Bitstream("color encode panicked".to_string()))??;
        let alpha = alpha_handle
            .join()
            .map_err(|_| EncoderError::Bitstream("alpha encode panicked".to_string()))??;
        Ok::<_, EncoderError>((primary, alpha))
    })?;
    let mut effective = effective_image_options(options, if grayscale { 1 } else { 3 }, 8);
    effective.color_information = Some(lossless_color_information());
    write_avif(
        width as usize,
        height as usize,
        &primary,
        Some(&alpha),
        &effective,
    )
}

fn encode_rgba16_with_lossless_items(
    image: &Rgba16ImageBuffer,
    options: &EncoderOptions,
    bit_depth: u8,
    color_lossless: bool,
    alpha_lossless: bool,
) -> Result<Vec<u8>, EncoderError> {
    let max = ((1u32 << u32::from(bit_depth)) - 1) as u16;
    let mut rgb = Vec::with_capacity(image.rgba.len().saturating_sub(image.rgba.len() / 4));
    let mut alpha_samples = Vec::with_capacity(image.rgba.len() / 4);
    let mut has_alpha = false;
    for pixel in image.rgba.chunks_exact(4) {
        let red = scale_u16(pixel[0], u32::from(max)) as u16;
        let green = scale_u16(pixel[1], u32::from(max)) as u16;
        let blue = scale_u16(pixel[2], u32::from(max)) as u16;
        let alpha = scale_u16(pixel[3], u32::from(max)) as u16;
        rgb.extend_from_slice(&[red, green, blue]);
        alpha_samples.push(alpha);
        has_alpha |= alpha != max;
    }

    let grayscale = rgba16_is_grayscale(image);
    if color_lossless {
        validate_lossless_options(
            options,
            if grayscale { 1 } else { 3 },
            bit_depth,
            "RGBA16 color",
        )?;
    }
    let width = u32::try_from(image.width)
        .map_err(|_| EncoderError::InvalidParam("image width exceeds u32".to_string()))?;
    let height = u32::try_from(image.height)
        .map_err(|_| EncoderError::InvalidParam("image height exceeds u32".to_string()))?;
    let primary = if color_lossless {
        if grayscale {
            let gray = rgb
                .chunks_exact(3)
                .map(|pixel| pixel[0])
                .collect::<Vec<_>>();
            encode_lossless_gray_samples(width, height, &gray, bit_depth, options.speed)?
        } else {
            encode_gamut_lossless_samples(width, height, &rgb, bit_depth, options.speed)?
        }
    } else {
        if grayscale {
            let gray = rgb
                .chunks_exact(3)
                .map(|pixel| pixel[0])
                .collect::<Vec<_>>();
            let planes = [PlaneInput {
                samples: &gray,
                width: image.width,
                height: image.height,
            }];
            encode_planes(
                image.width,
                image.height,
                &planes,
                Sampling::Cs400,
                bit_depth,
                options,
            )?
        } else {
            let (y, u, v, _) = rgba16_to_yuv420(image, bit_depth);
            let planes = [
                PlaneInput {
                    samples: &y,
                    width: image.width,
                    height: image.height,
                },
                PlaneInput {
                    samples: &u,
                    width: chroma_width(image.width),
                    height: chroma_height(image.height),
                },
                PlaneInput {
                    samples: &v,
                    width: chroma_width(image.width),
                    height: chroma_height(image.height),
                },
            ];
            encode_planes(
                image.width,
                image.height,
                &planes,
                Sampling::Cs420,
                bit_depth,
                options,
            )?
        }
    };
    let alpha = if has_alpha {
        if alpha_lossless {
            let alpha_options = {
                let mut value = options.clone();
                value.quality = options.quality_alpha;
                value.quality_alpha = options.quality_alpha;
                value.color_information = None;
                value.pixel_information = Some(PixelInformation {
                    bits_per_channel: vec![bit_depth],
                    extended_channels: None,
                });
                value
            };
            validate_lossless_options(&alpha_options, 1, bit_depth, "RGBA16 alpha")?;
            Some(encode_lossless_gray_samples(
                width,
                height,
                &alpha_samples,
                bit_depth,
                options.speed,
            )?)
        } else {
            let alpha_options = {
                let mut value = options.clone();
                value.quality = options.quality_alpha;
                value.color_information = None;
                value
            };
            let alpha_plane = [PlaneInput {
                samples: &alpha_samples,
                width: image.width,
                height: image.height,
            }];
            Some(encode_planes(
                image.width,
                image.height,
                &alpha_plane,
                Sampling::Cs400,
                bit_depth,
                &alpha_options,
            )?)
        }
    } else {
        None
    };
    let mut effective = effective_image_options(options, if grayscale { 1 } else { 3 }, bit_depth);
    if color_lossless {
        effective.color_information = Some(lossless_color_information());
    }
    write_avif(
        image.width,
        image.height,
        &primary,
        alpha.as_ref(),
        &effective,
    )
}

fn lossless_color_information() -> crate::ColorInformation {
    crate::NclxColorInformation {
        color_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients: 0,
        full_range_flag: true,
    }
    .to_color_information()
}

fn encode_lossless_native_full(
    buffers: &FrameBuffers,
    options: &EncoderOptions,
    sampling: Sampling,
    bit_depth: u8,
    planes: &[PlaneInput<'_>],
) -> Result<Vec<u8>, EncoderError> {
    let channels = if sampling == Sampling::Cs400 { 1 } else { 3 };
    validate_lossless_options(options, channels, bit_depth, "native lossless")?;
    if planes.len() != channels {
        return Err(EncoderError::InvalidParam(
            "lossless native plane count does not match chroma sampling".to_string(),
        ));
    }
    let (subsampling_x, subsampling_y) = match sampling {
        Sampling::Cs400 | Sampling::Cs444 => (0, 0),
        Sampling::Cs420 => (1, 1),
        Sampling::Cs422 => (1, 0),
    };
    let color = options
        .color_information
        .as_ref()
        .and_then(crate::ColorInformation::nclx)
        .unwrap_or(crate::NclxColorInformation {
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range_flag: true,
        });
    if matches!(sampling, Sampling::Cs420 | Sampling::Cs422) && color.matrix_coefficients == 0 {
        return Err(EncoderError::Unsupported(
            "lossless native 4:2:0 and 4:2:2 require non-identity CICP matrix coefficients"
                .to_string(),
        ));
    }
    for (index, plane) in planes.iter().enumerate() {
        let expected = if index == 0 {
            (buffers.width, buffers.height)
        } else {
            chroma_dimensions(buffers.width, buffers.height, sampling)
        };
        let expected_samples = expected
            .0
            .checked_mul(expected.1)
            .ok_or_else(|| EncoderError::InvalidParam("plane size overflows usize".to_string()))?;
        if (plane.width, plane.height) != expected
            || plane.samples.len() != expected_samples
            || plane
                .samples
                .iter()
                .any(|&sample| sample >= (1u16 << bit_depth))
        {
            return Err(EncoderError::InvalidParam(format!(
                "lossless native plane {index} must be a {}x{} {bit_depth}-bit plane",
                expected.0, expected.1
            )));
        }
    }

    let lossless_planes = LosslessPlanes {
        width: u32::try_from(buffers.width)
            .map_err(|_| EncoderError::InvalidParam("image width exceeds u32".to_string()))?,
        height: u32::try_from(buffers.height)
            .map_err(|_| EncoderError::InvalidParam("image height exceeds u32".to_string()))?,
        bit_depth,
        monochrome: sampling == Sampling::Cs400,
        subsampling_x,
        subsampling_y,
        color_primaries: color.color_primaries,
        transfer_characteristics: color.transfer_characteristics,
        matrix_coefficients: if matches!(sampling, Sampling::Cs400 | Sampling::Cs444) {
            0
        } else {
            color.matrix_coefficients
        },
        full_range: color.full_range_flag,
        planes: planes.iter().map(|plane| plane.samples).collect(),
    };
    let primary = encode_lossless_plane_set(&lossless_planes, options.speed)?;
    let mut effective = effective_options(options, channels, bit_depth);
    if matches!(sampling, Sampling::Cs400 | Sampling::Cs444) {
        effective.color_information = Some(lossless_color_information());
    }
    write_avif(buffers.width, buffers.height, &primary, None, &effective)
}

fn encode_gamut_lossless_samples(
    width: u32,
    height: u32,
    rgb: &[u16],
    bit_depth: u8,
    speed: u8,
) -> Result<EncodedFrame, EncoderError> {
    let sample_count = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| {
            EncoderError::InvalidParam("lossless image dimensions overflow usize".to_string())
        })?;
    let expected_len = sample_count.checked_mul(3).ok_or_else(|| {
        EncoderError::InvalidParam("lossless RGB buffer size overflows usize".to_string())
    })?;
    if rgb.len() != expected_len {
        return Err(EncoderError::InvalidParam(
            "lossless RGB buffer length does not match image dimensions".to_string(),
        ));
    }
    if !matches!(bit_depth, 8 | 10 | 12) {
        return Err(EncoderError::InvalidParam(
            "lossless RGB samples do not match the selected bit depth".to_string(),
        ));
    }
    let max_sample = 1u16 << bit_depth;
    if rgb.iter().any(|&sample| sample >= max_sample) {
        return Err(EncoderError::InvalidParam(
            "lossless RGB samples do not match the selected bit depth".to_string(),
        ));
    }
    let mut g = Vec::with_capacity(sample_count);
    let mut b = Vec::with_capacity(sample_count);
    let mut r = Vec::with_capacity(sample_count);
    for pixel in rgb.chunks_exact(3) {
        r.push(pixel[0]);
        g.push(pixel[1]);
        b.push(pixel[2]);
    }
    encode_lossless_gbr_planes(width, height, bit_depth, [&g, &b, &r], speed)
}

fn encode_lossless_gbr_planes(
    width: u32,
    height: u32,
    bit_depth: u8,
    gbr: [&[u16]; 3],
    speed: u8,
) -> Result<EncodedFrame, EncoderError> {
    let planes = LosslessPlanes {
        width,
        height,
        bit_depth,
        monochrome: false,
        subsampling_x: 0,
        subsampling_y: 0,
        color_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients: 0,
        full_range: true,
        planes: gbr.to_vec(),
    };
    encode_lossless_plane_set(&planes, speed)
}

fn encode_lossless_gray_samples(
    width: u32,
    height: u32,
    samples: &[u16],
    bit_depth: u8,
    speed: u8,
) -> Result<EncodedFrame, EncoderError> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| {
            EncoderError::InvalidParam("lossless image dimensions overflow usize".to_string())
        })?;
    if samples.len() != expected || !matches!(bit_depth, 8 | 10 | 12) {
        return Err(EncoderError::InvalidParam(
            "lossless gray samples do not match the selected bit depth".to_string(),
        ));
    }
    let max_sample = 1u16 << bit_depth;
    if samples.iter().any(|&sample| sample >= max_sample) {
        return Err(EncoderError::InvalidParam(
            "lossless gray samples do not match the selected bit depth".to_string(),
        ));
    }
    let planes = LosslessPlanes {
        width,
        height,
        bit_depth,
        monochrome: true,
        subsampling_x: 0,
        subsampling_y: 0,
        color_primaries: 1,
        transfer_characteristics: 13,
        matrix_coefficients: 0,
        full_range: true,
        planes: vec![samples],
    };
    encode_lossless_plane_set(&planes, speed)
}

fn encode_lossless_plane_set(
    planes: &LosslessPlanes<'_>,
    speed: u8,
) -> Result<EncodedFrame, EncoderError> {
    let still = encode_still_lossless(planes, speed)
        .map_err(|error| EncoderError::Bitstream(error.to_string()))?;
    Ok(EncodedFrame {
        sequence_header: vec![
            0x81,
            (still.config.seq_profile << 5) | (still.config.seq_level_idx_0 & 0x1f),
            (still.config.seq_tier_0 << 7)
                | (u8::from(still.config.high_bitdepth) << 6)
                | (u8::from(still.config.twelve_bit) << 5)
                | (u8::from(still.config.monochrome) << 4)
                | (still.config.chroma_subsampling_x << 3)
                | (still.config.chroma_subsampling_y << 2)
                | (still.config.chroma_sample_position & 0x3),
            0,
        ],
        frame_payload: still.obus,
        pixel_channels: if still.config.monochrome { 1 } else { 3 },
    })
}

fn validate_pixel_information(
    options: &EncoderOptions,
    channels: usize,
    expected_bit_depth: Option<u8>,
) -> Result<(), EncoderError> {
    if let Some(pixi) = &options.pixel_information {
        if pixi.bits_per_channel.len() != channels {
            return Err(EncoderError::InvalidParam(format!(
                "pixi contains {} channels, but the encoded image has {channels}",
                pixi.bits_per_channel.len()
            )));
        }
        if let Some(expected_bit_depth) = expected_bit_depth
            && pixi.bits_per_channel[0] != expected_bit_depth
        {
            return Err(EncoderError::InvalidParam(format!(
                "pixi declares {}-bit channels, but the encoded image uses {expected_bit_depth}-bit samples",
                pixi.bits_per_channel[0]
            )));
        }
    }
    Ok(())
}

fn effective_options(options: &EncoderOptions, channels: usize, bit_depth: u8) -> EncoderOptions {
    let mut effective = options.clone();
    if effective.pixel_information.is_none() {
        effective.pixel_information = Some(crate::PixelInformation {
            bits_per_channel: vec![bit_depth; channels],
            extended_channels: None,
        });
    }
    effective
}

fn native_backend_options(
    options: &NativeEncodeOptions,
    color_plan: NativeColorPlan<'_>,
) -> EncoderOptions {
    // Do not clone the caller's nested color property: native color metadata is
    // authoritative and is carried by the borrowed plan into the container
    // writer.  In particular, an opaque ICC payload can be several kilobytes.
    EncoderOptions {
        quality: options.encoder.quality,
        quality_alpha: options.encoder.quality_alpha,
        speed: options.encoder.speed,
        lossless: options.encoder.lossless,
        jobs: options.encoder.jobs,
        tile_rows_log2: options.encoder.tile_rows_log2,
        tile_cols_log2: options.encoder.tile_cols_log2,
        advanced: options.encoder.advanced.clone(),
        color_information: Some(native_nclx_color_information(color_plan)),
        pixel_information: options.encoder.pixel_information.clone(),
    }
}

fn effective_image_options(
    options: &EncoderOptions,
    channels: usize,
    bit_depth: u8,
) -> EncoderOptions {
    let mut effective = options.clone();
    if channels == 1
        && let Some(pixel_information) = effective.pixel_information.as_mut()
        && pixel_information.bits_per_channel.len() == 3
    {
        pixel_information.bits_per_channel.truncate(1);
        pixel_information.extended_channels = None;
    }
    effective_options(&effective, channels, bit_depth)
}

fn quality_to_quantizer(quality: u8) -> usize {
    // libavif maps its public 0..=100 quality scale to the codec's quantizer
    // range. rav1e uses the full 0..=255 QP range, so keep the same mapping as
    // libavif's rav1e backend instead of treating the public quality as a
    // rav1e-native quantizer.
    let quality = u16::from(quality);
    ((100 - quality) * 255 + 50) as usize / 100
}

fn quality_to_quantizer_for_options(quality: u8, _speed: u8) -> usize {
    // libavif's public quality scale is calibrated against libaom's still
    // image curve, while this crate's lossy path uses rav1e.  The direct
    // 0..=100-to-0..=255 mapping therefore spends too many bits at low and
    // medium-low public qcolor values.  Use a monotone, piecewise-linear
    // codec-scale correction derived from a cross-image rate/distortion
    // curve.  The anchors are public-quality values, not image-specific
    // thresholds; interpolation keeps every input on the same curve and
    // avoids a discontinuity at the quality presets used by libavif.
    const CALIBRATION: [(u8, u8); 15] = [
        (0, 0),
        (10, 10),
        (15, 13),
        (20, 16),
        (25, 18),
        (30, 23),
        (35, 27),
        (40, 30),
        (45, 35),
        (50, 40),
        (75, 65),
        (85, 80),
        (90, 85),
        (95, 90),
        (100, 100),
    ];
    let calibrated_quality = CALIBRATION
        .windows(2)
        .find_map(|window| {
            let [
                (low_quality, low_calibrated),
                (high_quality, high_calibrated),
            ] = window
            else {
                unreachable!("quality calibration windows have two entries")
            };
            if quality > *high_quality {
                return None;
            }
            let quality_span = u16::from(*high_quality - *low_quality);
            let calibrated_span = u16::from(*high_calibrated - *low_calibrated);
            let quality_offset = u16::from(quality - *low_quality);
            let interpolated = u16::from(*low_calibrated)
                + (calibrated_span * quality_offset + quality_span / 2) / quality_span;
            Some(interpolated as u8)
        })
        .unwrap_or(100);
    quality_to_quantizer(calibrated_quality)
}

fn pixel_range(options: &EncoderOptions) -> PixelRange {
    options
        .color_information
        .as_ref()
        .and_then(crate::ColorInformation::nclx)
        .map_or(PixelRange::Full, |color| {
            if color.full_range_flag {
                PixelRange::Full
            } else {
                PixelRange::Limited
            }
        })
}

fn color_description(options: &EncoderOptions) -> Option<ColorDescription> {
    let color = options.color_information.as_ref()?.nclx()?;
    Some(ColorDescription {
        color_primaries: color_primaries(color.color_primaries),
        transfer_characteristics: transfer_characteristics(color.transfer_characteristics),
        matrix_coefficients: matrix_coefficients(color.matrix_coefficients),
    })
}

fn color_primaries(value: u16) -> ColorPrimaries {
    match value {
        1 => ColorPrimaries::BT709,
        4 => ColorPrimaries::BT470M,
        5 => ColorPrimaries::BT470BG,
        6 => ColorPrimaries::BT601,
        7 => ColorPrimaries::SMPTE240,
        8 => ColorPrimaries::GenericFilm,
        9 => ColorPrimaries::BT2020,
        10 => ColorPrimaries::XYZ,
        11 => ColorPrimaries::SMPTE431,
        12 => ColorPrimaries::SMPTE432,
        22 => ColorPrimaries::EBU3213,
        _ => ColorPrimaries::Unspecified,
    }
}

fn transfer_characteristics(value: u16) -> TransferCharacteristics {
    match value {
        1 => TransferCharacteristics::BT709,
        4 => TransferCharacteristics::BT470M,
        5 => TransferCharacteristics::BT470BG,
        6 => TransferCharacteristics::BT601,
        7 => TransferCharacteristics::SMPTE240,
        8 => TransferCharacteristics::Linear,
        9 => TransferCharacteristics::Log100,
        10 => TransferCharacteristics::Log100Sqrt10,
        11 => TransferCharacteristics::IEC61966,
        12 => TransferCharacteristics::BT1361,
        13 => TransferCharacteristics::SRGB,
        14 => TransferCharacteristics::BT2020_10Bit,
        15 => TransferCharacteristics::BT2020_12Bit,
        16 => TransferCharacteristics::SMPTE2084,
        17 => TransferCharacteristics::SMPTE428,
        18 => TransferCharacteristics::HLG,
        _ => TransferCharacteristics::Unspecified,
    }
}

fn matrix_coefficients(value: u16) -> MatrixCoefficients {
    match value {
        0 => MatrixCoefficients::Identity,
        1 => MatrixCoefficients::BT709,
        4 => MatrixCoefficients::FCC,
        5 => MatrixCoefficients::BT470BG,
        6 => MatrixCoefficients::BT601,
        7 => MatrixCoefficients::SMPTE240,
        8 => MatrixCoefficients::YCgCo,
        9 => MatrixCoefficients::BT2020NCL,
        10 => MatrixCoefficients::BT2020CL,
        11 => MatrixCoefficients::SMPTE2085,
        12 => MatrixCoefficients::ChromatNCL,
        13 => MatrixCoefficients::ChromatCL,
        14 => MatrixCoefficients::ICtCp,
        _ => MatrixCoefficients::Unspecified,
    }
}

fn rgba8_is_grayscale(image: &ImageBuffer) -> bool {
    image
        .rgba
        .chunks_exact(4)
        .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2])
}

fn rgba16_is_grayscale(image: &Rgba16ImageBuffer) -> bool {
    image
        .rgba
        .chunks_exact(4)
        .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2])
}

fn rgba_to_gray(image: &ImageBuffer) -> (Vec<u16>, Option<Vec<u16>>) {
    let mut gray = Vec::with_capacity(image.width * image.height);
    let mut alpha = Vec::with_capacity(image.width * image.height);
    let mut has_alpha = false;
    for pixel in image.rgba.chunks_exact(4) {
        gray.push(u16::from(pixel[0]));
        alpha.push(u16::from(pixel[3]));
        has_alpha |= pixel[3] != u8::MAX;
    }
    (gray, has_alpha.then_some(alpha))
}

fn rgba16_to_gray(image: &Rgba16ImageBuffer, bit_depth: u8) -> (Vec<u16>, Option<Vec<u16>>) {
    let max = (1u32 << u32::from(bit_depth)) - 1;
    let mut gray = Vec::with_capacity(image.width * image.height);
    let mut alpha = Vec::with_capacity(image.width * image.height);
    let mut has_alpha = false;
    for pixel in image.rgba.chunks_exact(4) {
        gray.push(scale_u16(pixel[0], max) as u16);
        let value = scale_u16(pixel[3], max) as u16;
        alpha.push(value);
        has_alpha |= value != max as u16;
    }
    (gray, has_alpha.then_some(alpha))
}

fn rgba_to_yuv420(image: &ImageBuffer) -> (Vec<u16>, Vec<u16>, Vec<u16>, Option<Vec<u16>>) {
    let _timing = Guard::new(Kind::RgbaToYuv);
    let chroma_width = chroma_width(image.width);
    let chroma_height = chroma_height(image.height);
    let mut y = vec![0u16; image.width * image.height];
    let mut red_sum = vec![0u32; chroma_width * chroma_height];
    let mut green_sum = vec![0u32; chroma_width * chroma_height];
    let mut blue_sum = vec![0u32; chroma_width * chroma_height];
    let mut chroma_count = vec![0u32; chroma_width * chroma_height];
    let mut alpha = vec![0u16; image.width * image.height];
    let mut has_alpha = false;
    for row in 0..image.height {
        for column in 0..image.width {
            let offset = (row * image.width + column) * 4;
            let red = image.rgba[offset];
            let green = image.rgba[offset + 1];
            let blue = image.rgba[offset + 2];
            let yy = rgb_to_y_jpeg(red, green, blue);
            y[row * image.width + column] = u16::from(yy);
            let chroma_index = (row / 2) * chroma_width + column / 2;
            red_sum[chroma_index] += u32::from(red);
            green_sum[chroma_index] += u32::from(green);
            blue_sum[chroma_index] += u32::from(blue);
            chroma_count[chroma_index] += 1;
            let a = image.rgba[offset + 3];
            alpha[row * image.width + column] = u16::from(a);
            has_alpha |= a != u8::MAX;
        }
    }
    let (u, v): (Vec<u16>, Vec<u16>) = red_sum
        .into_iter()
        .zip(green_sum)
        .zip(blue_sum)
        .zip(chroma_count)
        .map(|(((red, green), blue), count)| {
            let red = ((red + count / 2) / count) as u8;
            let green = ((green + count / 2) / count) as u8;
            let blue = ((blue + count / 2) / count) as u8;
            let (u, v) = rgb_to_uv_jpeg(red, green, blue);
            (u as u16, v as u16)
        })
        .unzip();
    (y, u, v, has_alpha.then_some(alpha))
}

fn rgba16_to_yuv420(
    image: &Rgba16ImageBuffer,
    bit_depth: u8,
) -> (Vec<u16>, Vec<u16>, Vec<u16>, Option<Vec<u16>>) {
    let _timing = Guard::new(Kind::RgbaToYuv);
    let max = (1u32 << u32::from(bit_depth)) - 1;
    let chroma_width = chroma_width(image.width);
    let chroma_height = chroma_height(image.height);
    let mut y = vec![0u16; image.width * image.height];
    let mut u_sum = vec![0u64; chroma_width * chroma_height];
    let mut v_sum = vec![0u64; chroma_width * chroma_height];
    let mut chroma_count = vec![0u64; chroma_width * chroma_height];
    let mut alpha = vec![0u16; image.width * image.height];
    let mut has_alpha = false;
    for row in 0..image.height {
        for column in 0..image.width {
            let offset = (row * image.width + column) * 4;
            let red = scale_u16(image.rgba[offset], max);
            let green = scale_u16(image.rgba[offset + 1], max);
            let blue = scale_u16(image.rgba[offset + 2], max);
            let (yy, uu, vv) = rgb_to_yuv_u32(red, green, blue, max);
            y[row * image.width + column] = yy as u16;
            let chroma_index = (row / 2) * chroma_width + column / 2;
            u_sum[chroma_index] += u64::from(uu);
            v_sum[chroma_index] += u64::from(vv);
            chroma_count[chroma_index] += 1;
            let a = scale_u16(image.rgba[offset + 3], max);
            alpha[row * image.width + column] = a as u16;
            has_alpha |= a != max;
        }
    }
    let u = u_sum
        .into_iter()
        .zip(chroma_count.iter().copied())
        .map(|(sum, count)| ((sum + count / 2) / count) as u16)
        .collect();
    let v = v_sum
        .into_iter()
        .zip(chroma_count.iter().copied())
        .map(|(sum, count)| ((sum + count / 2) / count) as u16)
        .collect();
    (y, u, v, has_alpha.then_some(alpha))
}

fn plane_input(plane: &PlaneBuffer) -> Result<PlaneInput<'_>, EncoderError> {
    if plane.samples.len() != plane.layout.sample_count {
        return Err(EncoderError::InvalidParam(format!(
            "plane {} has {} samples, expected {}",
            plane.layout.plane,
            plane.samples.len(),
            plane.layout.sample_count
        )));
    }
    Ok(PlaneInput {
        samples: &plane.samples,
        width: plane.layout.width,
        height: plane.layout.height,
    })
}

fn sampling_from_planes(buffers: &FrameBuffers) -> Result<Sampling, EncoderError> {
    match buffers.planes.len() {
        1 => Ok(Sampling::Cs400),
        3 => {
            let u = &buffers.planes[1].layout;
            let v = &buffers.planes[2].layout;
            if u.subsampling_x != v.subsampling_x || u.subsampling_y != v.subsampling_y {
                return Err(EncoderError::InvalidParam(
                    "U and V plane subsampling must match".to_string(),
                ));
            }
            match (u.subsampling_x, u.subsampling_y) {
                (1, 1) => Ok(Sampling::Cs420),
                (1, 0) => Ok(Sampling::Cs422),
                (0, 0) => Ok(Sampling::Cs444),
                _ => Err(EncoderError::Unsupported(
                    "unsupported native plane subsampling".to_string(),
                )),
            }
        }
        _ => Err(EncoderError::InvalidParam(
            "native AVIF input requires one or three planes".to_string(),
        )),
    }
}

fn native_bit_depth(buffers: &FrameBuffers, options: &EncoderOptions) -> Result<u8, EncoderError> {
    if let Some(bits) = options
        .pixel_information
        .as_ref()
        .and_then(|value| value.bits_per_channel.first().copied())
    {
        if matches!(bits, 8 | 10 | 12) {
            return Ok(bits);
        }
        return Err(EncoderError::Unsupported(
            "only 8, 10, and 12-bit AVIF input is supported".to_string(),
        ));
    }
    let max_sample = buffers
        .planes
        .iter()
        .flat_map(|plane| plane.samples.iter().copied())
        .max()
        .unwrap_or(0);
    Ok(if max_sample <= 255 {
        8
    } else if max_sample <= 1023 {
        10
    } else if max_sample <= 4095 {
        12
    } else {
        return Err(EncoderError::InvalidParam(
            "native samples exceed the supported 12-bit range".to_string(),
        ));
    })
}

fn options_from_metadata(metadata: Option<&Metadata>) -> Result<EncoderOptions, EncoderError> {
    let mut options = EncoderOptions::default();
    let Some(metadata) = metadata else {
        return Ok(options);
    };
    for (key, value) in metadata {
        match key.as_str() {
            // libavif deprecated the old min/max quantizer pair in favor of
            // qcolor/qalpha. rav1e's still-picture path has one constant
            // quality value, so both legacy bounds are accepted as aliases
            // for that value rather than being silently ignored.
            "quality" | "qcolor" | "min" | "max" => options.quality = metadata_u8(value, key)?,
            "quality_alpha" | "qalpha" | "minalpha" | "maxalpha" => {
                options.quality_alpha = metadata_u8(value, key)?
            }
            "speed" => options.speed = metadata_speed(value, key)?,
            "jobs" => options.jobs = metadata_jobs(value, key)?,
            "lossless" => options.lossless = metadata_bool(value, key)?,
            "tile_rows_log2" | "tilerowslog2" => options.tile_rows_log2 = metadata_u8(value, key)?,
            "tile_cols_log2" | "tilecolslog2" => options.tile_cols_log2 = metadata_u8(value, key)?,
            "advanced" => parse_advanced(value, &mut options.advanced, key)?,
            _ => {}
        }
    }
    Ok(options)
}

fn parse_advanced(value: &DataMap, advanced: &mut Metadata, key: &str) -> Result<(), EncoderError> {
    let DataMap::Ascii(value) = value else {
        return Err(EncoderError::InvalidParam(format!(
            "{key} must be an ASCII comma-separated key=value list"
        )));
    };
    for entry in value.split([',', ';']) {
        let Some((name, setting)) = entry.split_once('=') else {
            return Err(EncoderError::InvalidParam(format!(
                "advanced option {entry} must use key=value"
            )));
        };
        advanced.insert(
            name.trim().to_string(),
            DataMap::Ascii(setting.trim().to_string()),
        );
    }
    Ok(())
}

fn apply_advanced(
    config: &mut Rav1eEncoderConfig,
    advanced: &Metadata,
) -> Result<(), EncoderError> {
    for (key, value) in advanced {
        match key.as_str() {
            "multiref" => config.speed_settings.multiref = metadata_bool(value, key)?,
            "fast-deblock" | "fast_deblock" => {
                config.speed_settings.fast_deblock = metadata_bool(value, key)?
            }
            "lru-on-skip" | "lru_on_skip" => {
                config.speed_settings.lru_on_skip = metadata_bool(value, key)?
            }
            "cdef" => config.speed_settings.cdef = metadata_bool(value, key)?,
            "lrf" | "restoration" => config.speed_settings.lrf = metadata_bool(value, key)?,
            "tx-domain-distortion" | "tx_domain_distortion" => {
                config.speed_settings.transform.tx_domain_distortion = metadata_bool(value, key)?
            }
            "tx-domain-rate" | "tx_domain_rate" => {
                config.speed_settings.transform.tx_domain_rate = metadata_bool(value, key)?
            }
            "rdo-tx-decision" | "rdo_tx_decision" => {
                config.speed_settings.transform.rdo_tx_decision = metadata_bool(value, key)?
            }
            "enable-inter-tx-split" | "enable_inter_tx_split" => {
                config.speed_settings.transform.enable_inter_tx_split = metadata_bool(value, key)?
            }
            "encode-bottomup" | "encode_bottomup" => {
                config.speed_settings.partition.encode_bottomup = metadata_bool(value, key)?
            }
            "reduced-tx-set" | "reduced_tx_set" => {
                config.speed_settings.transform.reduced_tx_set = metadata_bool(value, key)?
            }
            "include-near-mvs" | "include_near_mvs" => {
                config.speed_settings.motion.include_near_mvs = metadata_bool(value, key)?
            }
            "use-satd-subpel" | "use_satd_subpel" => {
                config.speed_settings.motion.use_satd_subpel = metadata_bool(value, key)?
            }
            "me-allow-full-search" | "me_allow_full_search" => {
                config.speed_settings.motion.me_allow_full_search = metadata_bool(value, key)?
            }
            "fine-directional-intra" | "fine_directional_intra" => {
                config.speed_settings.prediction.fine_directional_intra = metadata_bool(value, key)?
            }
            "rdo-lookahead-frames" | "rdo_lookahead_frames" => {
                config.speed_settings.rdo_lookahead_frames = metadata_usize(value, key)?
            }
            "tune" => config.tune = metadata_tune(value, key)?,
            "low-latency" | "low_latency" => config.low_latency = metadata_bool(value, key)?,
            _ => {
                return Err(EncoderError::Unsupported(format!(
                    "unsupported advanced codec option {key}"
                )));
            }
        }
    }
    if config.speed_settings.transform.tx_domain_rate
        && !config.speed_settings.transform.tx_domain_distortion
    {
        return Err(EncoderError::InvalidParam(
            "tx-domain-rate cannot be combined with tx-domain-distortion=false".to_string(),
        ));
    }
    Ok(())
}

fn metadata_u8(value: &DataMap, key: &str) -> Result<u8, EncoderError> {
    match value {
        DataMap::UInt(value) => u8::try_from(*value)
            .map_err(|_| EncoderError::InvalidParam(format!("{key} must fit in u8"))),
        _ => Err(EncoderError::InvalidParam(format!("{key} must be UInt"))),
    }
}

fn metadata_speed(value: &DataMap, key: &str) -> Result<u8, EncoderError> {
    match value {
        DataMap::UInt(_) => metadata_u8(value, key),
        DataMap::Ascii(value) if value.eq_ignore_ascii_case("default") || value == "d" => {
            Ok(EncoderOptions::default().speed)
        }
        _ => Err(EncoderError::InvalidParam(format!(
            "{key} must be a numeric speed or default"
        ))),
    }
}

fn metadata_jobs(value: &DataMap, key: &str) -> Result<usize, EncoderError> {
    match value {
        DataMap::UInt(value) => usize::try_from(*value)
            .map_err(|_| EncoderError::InvalidParam(format!("{key} must fit in usize"))),
        DataMap::Ascii(value) if value.eq_ignore_ascii_case("all") => Ok(0),
        _ => Err(EncoderError::InvalidParam(format!(
            "{key} must be a numeric worker count or all"
        ))),
    }
}

fn metadata_bool(value: &DataMap, key: &str) -> Result<bool, EncoderError> {
    match value {
        DataMap::UInt(value) => Ok(*value != 0),
        DataMap::Ascii(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        DataMap::Ascii(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(EncoderError::InvalidParam(format!("{key} must be boolean"))),
    }
}

fn metadata_tune(value: &DataMap, key: &str) -> Result<Tune, EncoderError> {
    match value {
        DataMap::Ascii(value) if value.eq_ignore_ascii_case("psnr") => Ok(Tune::Psnr),
        DataMap::Ascii(value)
            if value.eq_ignore_ascii_case("psychovisual") || value.eq_ignore_ascii_case("psy") =>
        {
            Ok(Tune::Psychovisual)
        }
        _ => Err(EncoderError::InvalidParam(format!(
            "{key} must be psnr or psychovisual"
        ))),
    }
}

fn metadata_usize(value: &DataMap, key: &str) -> Result<usize, EncoderError> {
    match value {
        DataMap::UInt(value) => usize::try_from(*value)
            .map_err(|_| EncoderError::InvalidParam(format!("{key} must fit in usize"))),
        DataMap::Ascii(value) => value
            .parse()
            .map_err(|_| EncoderError::InvalidParam(format!("{key} must be an integer"))),
        _ => Err(EncoderError::InvalidParam(format!(
            "{key} must be an integer"
        ))),
    }
}

fn chroma_dimensions(width: usize, height: usize, sampling: Sampling) -> (usize, usize) {
    match sampling {
        Sampling::Cs400 => (width, height),
        Sampling::Cs420 => (width.div_ceil(2), height.div_ceil(2)),
        Sampling::Cs422 => (width.div_ceil(2), height),
        Sampling::Cs444 => (width, height),
    }
}

fn chroma_width(width: usize) -> usize {
    width.div_ceil(2)
}
fn chroma_height(height: usize) -> usize {
    height.div_ceil(2)
}

fn rgb_to_y_jpeg(red: u8, green: u8, blue: u8) -> u8 {
    ((77 * u32::from(red) + 150 * u32::from(green) + 29 * u32::from(blue) + 128) >> 8) as u8
}

fn rgb_to_uv_jpeg(red: u8, green: u8, blue: u8) -> (u8, u8) {
    let u = (128i32 * i32::from(blue) - 85 * i32::from(green) - 43 * i32::from(red) + 0x8000) >> 8;
    let v = (128i32 * i32::from(red) - 107 * i32::from(green) - 21 * i32::from(blue) + 0x8000) >> 8;
    (u as u8, v as u8)
}

fn rgb_to_yuv_u32(red: u32, green: u32, blue: u32, max: u32) -> (u32, u32, u32) {
    let red = red as f64;
    let green = green as f64;
    let blue = blue as f64;
    let max_f = max as f64;
    let y = (0.299 * red + 0.587 * green + 0.114 * blue).round();
    let u = (-0.168736 * red - 0.331264 * green + 0.5 * blue + max_f / 2.0).round();
    let v = (0.5 * red - 0.418688 * green - 0.081312 * blue + max_f / 2.0).round();
    (
        y.clamp(0.0, max_f) as u32,
        u.clamp(0.0, max_f) as u32,
        v.clamp(0.0, max_f) as u32,
    )
}

fn scale_u16(value: u16, max: u32) -> u32 {
    ((u32::from(value) * max + 32767) / 65535).min(max)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_advanced, apply_compression_defaults, quality_to_quantizer,
        quality_to_quantizer_for_options, rgb_to_uv_jpeg, rgb_to_y_jpeg,
        should_try_complete_keyframe_search,
    };
    use crate::EncoderOptions;
    use crate::compat::DataMap;
    use crate::compat::Metadata;
    use rav1e::prelude::{EncoderConfig as Rav1eEncoderConfig, SegmentationLevel, Tune};

    #[test]
    fn quality_mapping_matches_libavif_rav1e_backend() {
        let expected = [
            (0, 255),
            (6, 240),
            (7, 237),
            (28, 184),
            (29, 181),
            (53, 120),
            (54, 117),
            (60, 102),
            (75, 64),
            (90, 26),
            (99, 3),
            (100, 0),
        ];
        for (quality, quantizer) in expected {
            assert_eq!(
                quality_to_quantizer(quality),
                quantizer,
                "quality={quality}"
            );
        }
    }

    #[test]
    fn quality_calibration_is_content_independent_across_speed_presets() {
        assert_eq!(quality_to_quantizer_for_options(20, 6), 214);
        assert_eq!(quality_to_quantizer_for_options(30, 10), 196);
        assert_eq!(quality_to_quantizer_for_options(60, 6), 128);
        assert_eq!(quality_to_quantizer_for_options(60, 10), 128);
        assert_eq!(quality_to_quantizer_for_options(75, 6), 89);
        assert_eq!(quality_to_quantizer_for_options(90, 6), 38);
        assert_eq!(quality_to_quantizer_for_options(90, 10), 38);

        for quality in 0..=100 {
            assert!(
                quality_to_quantizer_for_options(quality, 10)
                    >= quality_to_quantizer_for_options(quality.saturating_add(1), 10),
                "quality calibration must remain monotone at quality={quality}"
            );
        }
    }

    #[test]
    fn rgba8_yuv_conversion_matches_libyuv_jpeg_coefficients() {
        assert_eq!(rgb_to_y_jpeg(255, 0, 0), 77);
        assert_eq!(rgb_to_uv_jpeg(255, 0, 0), (85, 255));
        assert_eq!(rgb_to_y_jpeg(255, 255, 255), 255);
        assert_eq!(rgb_to_uv_jpeg(255, 255, 255), (128, 128));
    }

    #[test]
    fn compression_defaults_can_be_overridden_by_advanced_options() {
        let options = EncoderOptions::default();
        let mut config = Rav1eEncoderConfig::with_speed_preset(options.speed);
        apply_compression_defaults(&mut config, &options);
        assert_eq!(config.tune, Tune::Psnr);
        assert!(config.speed_settings.transform.rdo_tx_decision);
        assert!(!config.speed_settings.transform.reduced_tx_set);
        assert!(!config.speed_settings.transform.tx_domain_distortion);
        assert!(config.speed_settings.partition.encode_bottomup);
        assert_eq!(config.speed_settings.segmentation, SegmentationLevel::Full);

        let options = EncoderOptions {
            speed: 10,
            ..EncoderOptions::default()
        };
        let mut config = Rav1eEncoderConfig::with_speed_preset(options.speed);
        apply_compression_defaults(&mut config, &options);
        assert_eq!(config.tune, Tune::Psnr);
        assert!(config.speed_settings.transform.rdo_tx_decision);
        assert!(!config.speed_settings.transform.reduced_tx_set);
        assert!(!config.speed_settings.transform.tx_domain_distortion);
        assert!(config.speed_settings.partition.encode_bottomup);
        assert_eq!(config.speed_settings.segmentation, SegmentationLevel::Full);

        let mut options = EncoderOptions::default();
        options.advanced.insert(
            "rdo_tx_decision".to_string(),
            DataMap::Ascii("false".to_string()),
        );
        options.advanced.insert(
            "reduced_tx_set".to_string(),
            DataMap::Ascii("true".to_string()),
        );
        let mut config = Rav1eEncoderConfig::with_speed_preset(options.speed);
        apply_compression_defaults(&mut config, &options);
        assert!(!config.speed_settings.transform.rdo_tx_decision);
        assert!(config.speed_settings.transform.reduced_tx_set);
    }

    #[test]
    fn complete_keyframe_search_is_adaptive_and_respects_fast_overrides() {
        let mut options = EncoderOptions::default();
        assert!(!should_try_complete_keyframe_search(&options));

        options.speed = 10;
        assert!(should_try_complete_keyframe_search(&options));

        options.advanced.insert(
            "rdo_tx_decision".to_string(),
            DataMap::Ascii("false".to_string()),
        );
        assert!(!should_try_complete_keyframe_search(&options));
    }

    #[test]
    fn explicit_tune_overrides_the_psnr_compression_default() {
        let options = Metadata::from([(
            "tune".to_string(),
            DataMap::Ascii("psychovisual".to_string()),
        )]);
        let mut config = Rav1eEncoderConfig::with_speed_preset(6);
        apply_compression_defaults(&mut config, &EncoderOptions::default());
        apply_advanced(&mut config, &options).unwrap();
        assert_eq!(config.tune, Tune::Psychovisual);
    }

    #[test]
    fn tx_domain_rate_rejects_the_temporal_rdo_combination() {
        let advanced = Metadata::from([
            (
                "tx-domain-rate".to_string(),
                DataMap::Ascii("true".to_string()),
            ),
            (
                "tx-domain-distortion".to_string(),
                DataMap::Ascii("false".to_string()),
            ),
        ]);
        let mut config = Rav1eEncoderConfig::with_speed_preset(6);
        assert!(apply_advanced(&mut config, &advanced).is_err());
    }

    #[test]
    fn disabled_tx_domain_rate_keeps_the_compression_default() {
        let options = EncoderOptions {
            advanced: Metadata::from([(
                "tx-domain-rate".to_string(),
                DataMap::Ascii("false".to_string()),
            )]),
            ..EncoderOptions::default()
        };
        let mut config = Rav1eEncoderConfig::with_speed_preset(options.speed);
        apply_compression_defaults(&mut config, &options);
        assert!(!config.speed_settings.transform.tx_domain_distortion);
    }
}

#[cfg(test)]
#[path = "encoder_backend_tests.rs"]
mod backend_tests;

#[cfg(test)]
mod preflight_backend_tests {
    use super::*;

    #[test]
    fn selected_backend_dimensions_include_alpha_and_all_lossless_level_limits() {
        let mut options = EncoderOptions {
            lossless: true,
            color_information: None,
            ..Default::default()
        };
        for (width, height, valid) in [
            (1, 1, true),
            (16384, 1, true),
            (16385, 1, false),
            (1, 8704, true),
            (1, 8705, false),
            (4096, 8704, true),
            (4097, 8704, false),
        ] {
            assert_eq!(
                validate_native_backend_settings(
                    width,
                    height,
                    Sampling::Cs400,
                    8,
                    false,
                    &options
                )
                .is_ok(),
                valid,
                "{width}x{height}"
            );
        }
        options.lossless = false;
        options.quality = 60;
        options.quality_alpha = 60;
        // Lossy is governed by rav1e's settings, not the lossless level table.
        for (width, height, valid) in [
            (16385, 1, true),
            (65535, 1, true),
            (65536, 1, false),
            (1, 65535, true),
            (1, 65536, false),
        ] {
            assert_eq!(
                validate_native_backend_settings(width, height, Sampling::Cs400, 8, true, &options)
                    .is_ok(),
                valid,
                "lossy {width}x{height}"
            );
        }
        options.quality_alpha = 100;
        assert!(
            validate_native_backend_settings(16385, 1, Sampling::Cs400, 8, false, &options).is_ok()
        );
        assert!(
            validate_native_backend_settings(16385, 1, Sampling::Cs400, 8, true, &options).is_err()
        );
        options.quality = 100;
        options.quality_alpha = 60;
        assert!(
            validate_native_backend_settings(16385, 1, Sampling::Cs400, 8, true, &options).is_err()
        );
    }
}

#[cfg(test)]
mod rgba8_planar_lossless_tests {
    use super::*;

    #[test]
    fn direct_rgba8_gbr_matches_interleaved_rgb_route() {
        for (width, height) in [(1, 1), (5, 3), (16, 9)] {
            let rgba = (0..width * height)
                .flat_map(|i| {
                    [
                        (i * 37) as u8,
                        (255 - i % 256) as u8,
                        (i * 101) as u8,
                        (i * 17) as u8,
                    ]
                })
                .collect();
            let image = ImageBuffer {
                width,
                height,
                rgba,
            };
            let rgb = image
                .rgba
                .chunks_exact(4)
                .flat_map(|pixel| pixel[..3].iter().copied().map(u16::from))
                .collect::<Vec<_>>();
            for speed in [0, 6, 10] {
                let direct =
                    encode_rgba8_rgb_lossless(&image, width as u32, height as u32, speed).unwrap();
                let staged =
                    encode_gamut_lossless_samples(width as u32, height as u32, &rgb, 8, speed)
                        .unwrap();
                assert_eq!(direct.sequence_header, staged.sequence_header);
                assert_eq!(
                    direct.frame_payload, staged.frame_payload,
                    "{width}x{height} speed{speed}"
                );
                assert_eq!(direct.pixel_channels, staged.pixel_channels);
            }
        }
    }
}
