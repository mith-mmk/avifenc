//! Shared pixel inputs with explicit depth, color policy and preflight.
use crate::buffer::*;
use crate::{
    EncoderError, FrameBuffers, NativeEncodeOptions, NativeSubsampling, PlaneBuffer, PlaneLayout,
};
use std::fmt;

/// Color metadata preservation is distinct from applying a color transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorPolicyEx {
    /// Require source CICP and ICC bytes to match the explicit encoder options.
    #[default]
    Preserve,
    /// Permit only the individually selected signalling changes. No pixels transform.
    Explicit {
        /// Allow caller-prepared output CICP to differ or fill an absent declaration.
        allow_cicp_change: bool,
        /// Allow explicitly prepared output ICC bytes, including removal.
        allow_icc_change: bool,
    },
}
/// Additive options; output depth is `native.bit_depth`, never inferred from U16.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeOptionsEx {
    /// Existing validated native backend and output color/depth settings.
    pub native: NativeEncodeOptions,
    /// Normalized packed precision policy; native depth conversion is separate.
    pub precision: PrecisionPolicy,
    /// Explicit per-field output signalling permission, not an ICC engine.
    pub color: ColorPolicyEx,
}
/// Performed operations only; lossless codec mode does not undo these operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionEx {
    /// Full-range normalized packed scaling selected by the caller.
    PackedPrecision {
        /// Original sample depth.
        from: u8,
        /// Output coded depth.
        to: u8,
        /// Selected integer policy.
        policy: PrecisionPolicy,
    },
    /// CICP metadata was explicitly changed; no color transform was applied.
    CicpSignalling,
    /// ICC bytes were explicitly changed or removed; no ICC transform was applied.
    IccSignalling,
}
/// Exact input/output choices and adapter copying; not a total codec allocation report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeReportEx {
    /// False for a preflight preview; true only after a complete successful encode.
    pub encoded: bool,
    /// Input color precision.
    pub input_depth: u8,
    /// Actual coded precision selected for this supported backend path.
    pub output_depth: u8,
    /// Native layout used by the encoder.
    pub model: NativeModel,
    /// Alpha association supplied to the encoder.
    pub alpha: AlphaMode,
    /// Explicit output CICP tuple.
    pub output_cicp: Cicp,
    /// Whether opaque ICC bytes are written.
    pub output_icc: bool,
    /// Planned operations in a preview; actually applied only when `encoded` is true.
    pub conversions: Vec<ConversionEx>,
    /// Pixel sample values copied to the legacy compact native entry point.
    pub adapter_sample_copies: usize,
    /// Whether the primary backend is in lossless mode.
    pub primary_lossless: bool,
    /// Whether an existing alpha plane is encoded losslessly.
    pub alpha_lossless: bool,
}
/// A successful transactional in-memory encode.
#[derive(Debug)]
pub struct EncodedImageEx {
    /// Complete AVIF bytes; never a partial success on error.
    pub bytes: Vec<u8>,
    /// Applied choices and conversions.
    pub report: EncodeReportEx,
}
/// Shared validation and explicit conversion errors remain distinguishable.
#[derive(Debug)]
pub enum EncodeErrorEx {
    /// Invalid storage/sample layout or allocation/precision failure.
    Buffer(BufferError),
    /// Caller must perform/select a supported explicit conversion first.
    ConversionRequired(&'static str),
    /// Output metadata conflicts with the declared source meaning.
    ColorConflict(&'static str),
    /// A representation is retained by the model but unsupported by this backend.
    Unsupported(&'static str),
    /// Native codec validation or encode failure.
    Codec(EncoderError),
}
impl fmt::Display for EncodeErrorEx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffer(e) => write!(f, "{e}"),
            Self::Codec(e) => write!(f, "{e}"),
            Self::ConversionRequired(s) => write!(f, "explicit conversion required: {s}"),
            Self::ColorConflict(s) => write!(f, "source/output color conflict: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported shared encode: {s}"),
        }
    }
}
impl std::error::Error for EncodeErrorEx {}
impl From<BufferError> for EncodeErrorEx {
    fn from(e: BufferError) -> Self {
        Self::Buffer(e)
    }
}
impl From<EncoderError> for EncodeErrorEx {
    fn from(e: EncoderError) -> Self {
        Self::Codec(e)
    }
}

fn sampling(model: NativeModel) -> NativeSubsampling {
    match model {
        NativeModel::Monochrome => NativeSubsampling::Cs400,
        NativeModel::Yuv420 => NativeSubsampling::Cs420,
        NativeModel::Yuv422 => NativeSubsampling::Cs422,
        _ => NativeSubsampling::Cs444,
    }
}

impl crate::ImageBuffer {
    /// Borrow unchanged legacy RGBA8 storage with caller-declared color metadata.
    pub fn view_ex<'a>(
        &'a self,
        color: ColorMetadataRef<'a>,
    ) -> Result<ImageViewEx<'a>, BufferError> {
        packed_view(
            self.width,
            self.height,
            8,
            SamplesRef::U8(&self.rgba),
            color,
        )
    }
}
impl crate::Rgba16ImageBuffer {
    /// Borrow full-range16-bit samples; this never infers native12-bit precision.
    pub fn view_ex<'a>(
        &'a self,
        color: ColorMetadataRef<'a>,
    ) -> Result<ImageViewEx<'a>, BufferError> {
        packed_view(
            self.width,
            self.height,
            16,
            SamplesRef::U16(&self.rgba),
            color,
        )
    }
}
fn packed_view<'a>(
    width: usize,
    height: usize,
    bits: u8,
    samples: SamplesRef<'a>,
    color: ColorMetadataRef<'a>,
) -> Result<ImageViewEx<'a>, BufferError> {
    let row = width
        .checked_mul(4)
        .and_then(|n| n.checked_mul(samples.storage().bytes()))
        .ok_or(BufferError::Overflow)?;
    if samples.length_bytes() != Ok(row.checked_mul(height).ok_or(BufferError::Overflow)?) {
        return Err(BufferError::Layout("legacy RGBA exact length"));
    }
    let view = ImageViewEx::Packed(PackedImageViewEx {
        plane: PlaneViewEx {
            samples,
            layout: PlaneLayoutEx {
                width,
                height,
                valid_bits: bits,
                offset_bytes: 0,
                row_stride_bytes: row,
            },
        },
        channels: PackedChannels::Rgba,
        alpha: AlphaMode::Straight,
        color,
    });
    view.validate()?;
    Ok(view)
}
impl FrameBuffers {
    /// Borrow existing compact native planes with explicit depth, roles and color.
    pub fn view_ex<'a>(
        &'a self,
        descriptor: NativeDescriptor,
        bits: u8,
        color: ColorMetadataRef<'a>,
    ) -> Result<ImageViewEx<'a>, BufferError> {
        if self.width != descriptor.width || self.height != descriptor.height {
            return Err(BufferError::Layout("native frame/context dimensions"));
        }
        let mut planes = [None; 4];
        for p in &self.planes {
            let role = usize::from(p.layout.plane);
            if role >= 4 || planes[role].is_some() {
                return Err(BufferError::Layout("invalid/duplicate native role"));
            }
            let count = p
                .layout
                .width
                .checked_mul(p.layout.height)
                .ok_or(BufferError::Overflow)?;
            if p.layout.sample_count != count || p.samples.len() != count {
                return Err(BufferError::Layout("legacy native exact sample count"));
            }
            let expected = if role == 1 || role == 2 {
                descriptor.model.chroma_subsampling().unwrap_or((0, 0))
            } else {
                (0, 0)
            };
            if (
                usize::from(p.layout.subsampling_x),
                usize::from(p.layout.subsampling_y),
            ) != expected
            {
                return Err(BufferError::Layout("native frame/context subsampling"));
            }
            planes[role] = Some(PlaneViewEx {
                samples: SamplesRef::U16(&p.samples),
                layout: PlaneLayoutEx {
                    width: p.layout.width,
                    height: p.layout.height,
                    valid_bits: bits,
                    offset_bytes: 0,
                    row_stride_bytes: p.layout.width.checked_mul(2).ok_or(BufferError::Overflow)?,
                },
            });
        }
        let view = ImageViewEx::NativePlanar(NativePlanarViewEx {
            descriptor,
            planes,
            color,
        });
        view.validate()?;
        Ok(view)
    }
}
fn description(view: ImageViewEx<'_>) -> (NativeModel, AlphaMode, u8) {
    match view {
        ImageViewEx::Packed(p) => (
            if p.channels == PackedChannels::Gray {
                NativeModel::Monochrome
            } else {
                NativeModel::IdentityGbr
            },
            p.alpha,
            p.plane.layout.valid_bits,
        ),
        ImageViewEx::NativePlanar(n) => (
            n.descriptor.model,
            n.descriptor.alpha,
            n.planes[0].expect("validated primary").layout.valid_bits,
        ),
    }
}

/// Validate representation, precision, source/output color and backend controls
/// before allocating compact planes or starting an encoder. No input is changed.
pub fn preflight_ex(
    view: ImageViewEx<'_>,
    options: &EncodeOptionsEx,
) -> Result<EncodeReportEx, EncodeErrorEx> {
    view.validate()?;
    let (model, alpha, bits) = description(view);
    if matches!(alpha, AlphaMode::Premultiplied(_)) {
        return Err(EncodeErrorEx::ConversionRequired(
            "premultiplied alpha needs explicit unpremultiplication",
        ));
    }
    let depth = options.native.bit_depth;
    if !matches!(depth, 8 | 10 | 12) {
        return Err(EncodeErrorEx::Unsupported("coded depth must be8/10/12"));
    }
    if sampling(model) != options.native.subsampling {
        return Err(EncodeErrorEx::ConversionRequired(
            "native subsampling/plane roles differ; explicit resampling/color conversion required",
        ));
    }
    let nclx = options
        .native
        .color_information
        .nclx
        .ok_or(EncodeErrorEx::ColorConflict("explicit output nclx missing"))?;
    let out = Cicp {
        primaries: nclx.color_primaries,
        transfer: nclx.transfer_characteristics,
        matrix: nclx.matrix_coefficients,
    };
    if model != NativeModel::Monochrome && (model == NativeModel::IdentityGbr) != (out.matrix == 0)
    {
        return Err(EncodeErrorEx::ColorConflict(
            "identity GBR and YUV are different native roles",
        ));
    }
    let (cicp_allowed, icc_allowed) = match options.color {
        ColorPolicyEx::Preserve => (false, false),
        ColorPolicyEx::Explicit {
            allow_cicp_change,
            allow_icc_change,
        } => (allow_cicp_change, allow_icc_change),
    };
    let mut conversions = Vec::new();
    conversions
        .try_reserve_exact(3)
        .map_err(|_| BufferError::Allocation)?;
    if view.color().cicp != Some(out) {
        if !cicp_allowed {
            return Err(EncodeErrorEx::ColorConflict(
                "source CICP differs or is absent",
            ));
        }
        conversions.push(ConversionEx::CicpSignalling);
    }
    if view.color().icc != options.native.color_information.icc_profile.as_deref() {
        if !icc_allowed {
            return Err(EncodeErrorEx::ColorConflict(
                "source ICC differs or would be dropped",
            ));
        }
        conversions.push(ConversionEx::IccSignalling);
    }
    match view {
        ImageViewEx::NativePlanar(n) => {
            if n.descriptor.range
                != Some(if nclx.full_range_flag {
                    ColorRange::Full
                } else {
                    ColorRange::Limited
                })
            {
                return Err(EncodeErrorEx::ColorConflict(
                    "native range differs or is unknown",
                ));
            }
            if n.descriptor.chroma_location.is_some_and(|v| v != 0) {
                return Err(EncodeErrorEx::Unsupported(
                    "native chroma location signalling is not exposed by the backend",
                ));
            }
            if bits != depth || n.planes[3].is_some_and(|p| p.layout.valid_bits != depth) {
                return Err(EncodeErrorEx::ConversionRequired(
                    "native/alpha depth conversion needs an explicit native-range policy",
                ));
            }
        }
        ImageViewEx::Packed(p) => {
            if !nclx.full_range_flag {
                return Err(EncodeErrorEx::ColorConflict(
                    "packed normalized samples require full range",
                ));
            }
            if bits != depth {
                for y in 0..view.height() {
                    for x in 0..view.width() {
                        for c in 0..p.channels.count() {
                            scale_full_range(
                                p.plane.sample(x, y, c, p.channels.count())?,
                                bits,
                                depth,
                                options.precision,
                            )?;
                        }
                    }
                }
                conversions.push(ConversionEx::PackedPrecision {
                    from: bits,
                    to: depth,
                    policy: options.precision,
                });
            }
        }
    }
    crate::encoder::validate_shared_native_options(
        view.width(),
        view.height(),
        if model == NativeModel::Monochrome {
            1
        } else {
            3
        },
        alpha != AlphaMode::Absent,
        &options.native,
    )?;
    let count = match view {
        ImageViewEx::Packed(p) => view
            .width()
            .checked_mul(view.height())
            .and_then(|v| v.checked_mul(p.channels.count()))
            .ok_or(BufferError::Overflow)?,
        ImageViewEx::NativePlanar(n) => {
            n.planes.iter().flatten().try_fold(0usize, |total, p| {
                p.layout
                    .width
                    .checked_mul(p.layout.height)
                    .and_then(|v| total.checked_add(v))
                    .ok_or(BufferError::Overflow)
            })?
        }
    };
    Ok(EncodeReportEx {
        encoded: false,
        input_depth: bits,
        output_depth: depth,
        model,
        alpha,
        output_cicp: out,
        output_icc: options.native.color_information.icc_profile.is_some(),
        conversions,
        adapter_sample_copies: count,
        primary_lossless: options.native.encoder.lossless || options.native.encoder.quality == 100,
        alpha_lossless: alpha != AlphaMode::Absent
            && (options.native.encoder.lossless || options.native.encoder.quality_alpha == 100),
    })
}

/// Encode the shared immutable view through the existing native backend.
/// Compact native vectors currently copy samples once; no zero-copy backend claim.
pub fn encode_ex(
    view: ImageViewEx<'_>,
    options: &EncodeOptionsEx,
) -> Result<EncodedImageEx, EncodeErrorEx> {
    let mut report = preflight_ex(view, options)?;
    let mut planes = Vec::new();
    planes
        .try_reserve_exact(4)
        .map_err(|_| BufferError::Allocation)?;
    for role in 0..4usize {
        let (w, h, sx, sy) = match view {
            ImageViewEx::NativePlanar(n) => {
                let Some(p) = n.planes[role] else { continue };
                let (sx, sy) = if role == 1 || role == 2 {
                    n.descriptor.model.chroma_subsampling().unwrap_or((0, 0))
                } else {
                    (0, 0)
                };
                (p.layout.width, p.layout.height, sx, sy)
            }
            ImageViewEx::Packed(p) => {
                if p.channels == PackedChannels::Gray && role != 0 {
                    continue;
                }
                (view.width(), view.height(), 0, 0)
            }
        };
        let count = w.checked_mul(h).ok_or(BufferError::Overflow)?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(count)
            .map_err(|_| BufferError::Allocation)?;
        for y in 0..h {
            for x in 0..w {
                let value = match view {
                    ImageViewEx::NativePlanar(n) => {
                        n.planes[role].expect("selected plane").sample(x, y, 0, 1)?
                    }
                    ImageViewEx::Packed(p) => {
                        let channel = if p.channels == PackedChannels::Gray {
                            0
                        } else {
                            [1, 2, 0, 3][role]
                        };
                        scale_full_range(
                            p.plane.sample(x, y, channel, p.channels.count())?,
                            p.plane.layout.valid_bits,
                            options.native.bit_depth,
                            options.precision,
                        )?
                    }
                };
                samples.push(value);
            }
        }
        planes.push(PlaneBuffer {
            layout: PlaneLayout {
                plane: role as u8,
                width: w,
                height: h,
                subsampling_x: sx as u8,
                subsampling_y: sy as u8,
                sample_count: count,
            },
            samples,
        });
    }
    let bytes = crate::encode_native_bytes(
        &FrameBuffers {
            width: view.width(),
            height: view.height(),
            planes,
        },
        &options.native,
    )?;
    report.encoded = true;
    Ok(EncodedImageEx { bytes, report })
}
