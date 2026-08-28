/// RGBA8 image input and output shape compatible with `avif-rust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBuffer {
    /// Image width in pixels.
    pub width: usize,
    /// Image height in pixels.
    pub height: usize,
    /// Interleaved, non-premultiplied RGBA8 samples.
    pub rgba: Vec<u8>,
}

/// RGBA16 image shape compatible with `avif-rust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba16ImageBuffer {
    /// Image width in pixels.
    pub width: usize,
    /// Image height in pixels.
    pub height: usize,
    /// Interleaved, non-premultiplied RGBA16 samples in the full `u16` range.
    pub rgba: Vec<u16>,
}

/// Native AV1 plane geometry compatible with `avif-rust`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    /// AV1 plane index: 0 for Y or gray, 1 for U, and 2 for V in the legacy
    /// native frame API. `encode_native_bytes` also accepts plane 3 as an
    /// optional alpha plane and uses 0=G, 1=B, 2=R for the native identity
    /// layout.
    pub plane: u8,
    /// Plane width in samples.
    pub width: usize,
    /// Plane height in samples.
    pub height: usize,
    /// Horizontal chroma subsampling exponent.
    pub subsampling_x: u8,
    /// Vertical chroma subsampling exponent.
    pub subsampling_y: u8,
    /// Number of entries in the corresponding sample buffer.
    pub sample_count: usize,
}

impl PlaneLayout {
    /// Returns the tightly packed sample stride.
    pub fn stride(&self) -> usize {
        self.width
    }
}

/// Native AV1 plane samples compatible with `avif-rust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneBuffer {
    /// Geometry of this plane.
    pub layout: PlaneLayout,
    /// Unsigned native samples, stored row-major.
    pub samples: Vec<u16>,
}

/// Native AV1 frame planes compatible with `avif-rust`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBuffers {
    /// Luma frame width in pixels.
    pub width: usize,
    /// Luma frame height in pixels.
    pub height: usize,
    /// One gray/luma plane or three YUV planes for the legacy native frame
    /// API. `encode_native_bytes` additionally accepts an optional plane 3
    /// alpha buffer and its explicit native identity GBR layout.
    pub planes: Vec<PlaneBuffer>,
}

/// `pixi` pixel information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelInformation {
    /// Bit depth of each channel in the `pixi` property.
    pub bits_per_channel: Vec<u8>,
    /// Optional extended channel descriptors.
    pub extended_channels: Option<Vec<PixelChannelInformation>>,
}

/// Optional extended `pixi` channel descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelChannelInformation {
    /// ISO/IEC 23008-12 channel identifier.
    pub channel_idc: u8,
    /// ISO/IEC 23001-17 component format.
    pub component_format: u8,
    /// Optional chroma sample location.
    pub subsampling: Option<PixelSubsampling>,
}

/// Chroma subsampling information carried by an extended `pixi` channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelSubsampling {
    /// Chroma subsampling type.
    pub subsampling_type: u8,
    /// Chroma subsampling location.
    pub subsampling_location: u8,
}

/// `colr` color information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorInformation {
    /// Four-byte `colr` colour type, such as `nclx` or `prof`.
    pub color_type: [u8; 4],
    /// Type-specific `colr` payload.
    pub payload: Vec<u8>,
}

impl ColorInformation {
    /// Parses the payload as CICP values when this is an `nclx` property.
    pub fn nclx(&self) -> Option<NclxColorInformation> {
        if &self.color_type != b"nclx" || self.payload.len() < 7 {
            return None;
        }
        Some(NclxColorInformation {
            color_primaries: u16::from_be_bytes([self.payload[0], self.payload[1]]),
            transfer_characteristics: u16::from_be_bytes([self.payload[2], self.payload[3]]),
            matrix_coefficients: u16::from_be_bytes([self.payload[4], self.payload[5]]),
            full_range_flag: self.payload[6] & 0x80 != 0,
        })
    }

    /// Returns the raw ICC payload for `prof` or `rICC` properties.
    pub fn icc_profile(&self) -> Option<&[u8]> {
        if &self.color_type == b"prof" || &self.color_type == b"rICC" {
            Some(&self.payload)
        } else {
            None
        }
    }
}

/// Structured CICP values carried by an `nclx` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NclxColorInformation {
    /// CICP colour primaries code.
    pub color_primaries: u16,
    /// CICP transfer characteristics code.
    pub transfer_characteristics: u16,
    /// CICP matrix coefficients code.
    pub matrix_coefficients: u16,
    /// Whether the encoded samples use full range.
    pub full_range_flag: bool,
}

/// Colour properties to preserve on a native AVIF item.
///
/// Native encoding requires an explicit [`NativeColorInformation::nclx`]
/// tuple. ICC is opaque to this codec and is never interpreted or replaced by
/// an implicit sRGB profile; CICP and ICC may be emitted together. If ICC is
/// supplied, `icc_profile` and `icc_color_type` must be supplied as a pair,
/// with the type set to `prof` or `rICC`. Native CICP is authoritative. A
/// nested [`EncoderOptions`](crate::EncoderOptions) `ColorInformation` contains
/// one `colr` property, so it must match the native CICP property exactly when
/// it is `nclx`, or match the native ICC type and bytes when it is `prof`/`rICC`;
/// it cannot represent both properties at once.
///
/// The native writer currently emits the primary item followed by an
/// optional alpha auxiliary item. The alpha item has its own monochrome,
/// full-range CICP description and no ICC/`colr`; callers should not use the
/// primary identity tuple as an alpha description. Identity CICP is admitted
/// only for the explicit full-range BT.709/sRGB 4:4:4 GBR layout, with the
/// AV1-required GBR plane order (`plane 0 = G`, `plane 1 = B`, `plane 2 = R`).
/// Other identity tuples, including monochrome, are rejected by the native
/// policy. Auxiliary item identifiers are container-owned.
/// `PlaneLayout::plane == 3` identifies an input alpha plane; it is not a
/// promise that the container will use item ID 3 (the current writer uses
/// item 2).
///
/// `NativeEncodeOptions::default` retains the legacy nested colour default.
/// When supplying a different native tuple, callers should set
/// `EncoderOptions::color_information` to `None`, or provide an exactly
/// matching nested `nclx`/ICC property.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NativeColorInformation {
    /// Explicit CICP `nclx` property required by native encoding. ICC cannot
    /// infer this AV1 colour configuration.
    pub nclx: Option<NclxColorInformation>,
    /// Raw ICC profile bytes. Must be paired with [`Self::icc_color_type`].
    pub icc_profile: Option<Vec<u8>>,
    /// ICC property type, either `prof` or `rICC`; must be paired with
    /// [`Self::icc_profile`].
    pub icc_color_type: Option<[u8; 4]>,
}

impl NativeColorInformation {
    /// Constructs CICP-only native colour information.
    pub fn from_nclx(nclx: NclxColorInformation) -> Self {
        Self {
            nclx: Some(nclx),
            ..Self::default()
        }
    }
}

impl NclxColorInformation {
    /// Converts the CICP values into a `colr` property payload.
    pub fn to_color_information(self) -> ColorInformation {
        let mut payload = Vec::with_capacity(7);
        payload.extend_from_slice(&self.color_primaries.to_be_bytes());
        payload.extend_from_slice(&self.transfer_characteristics.to_be_bytes());
        payload.extend_from_slice(&self.matrix_coefficients.to_be_bytes());
        payload.push(if self.full_range_flag { 0x80 } else { 0 });
        ColorInformation {
            color_type: *b"nclx",
            payload,
        }
    }
}
