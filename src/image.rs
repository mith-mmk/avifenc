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
    /// AV1 plane index: 0 for Y or gray, 1 for U, and 2 for V.
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
    /// One gray/luma plane or three YUV planes.
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
