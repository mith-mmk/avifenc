use std::collections::HashMap;

use crate::error::EncoderError;

/// Metadata/options map using the vocabulary shared with `avif-rust`.
pub type Metadata = HashMap<String, DataMap>;

/// A typed compatibility metadata value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataMap {
    /// An unsigned integer.
    UInt(u64),
    /// An array of unsigned integers.
    UIntAllay(Vec<u64>),
    /// An opaque byte payload.
    Raw(Vec<u8>),
    /// A text value.
    Ascii(String),
    /// An explicitly absent value.
    None,
}

/// Command returned by a compatibility callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseCommand {
    /// Stop encoding.
    Abort,
    /// Continue encoding.
    Continue,
}

/// Callback response wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackResponse {
    /// Requested callback command.
    pub response: ResponseCommand,
}

impl CallbackResponse {
    /// Creates an abort response.
    pub fn abort() -> Self {
        Self {
            response: ResponseCommand::Abort,
        }
    }

    /// Creates a continue response.
    pub fn cont() -> Self {
        Self {
            response: ResponseCommand::Continue,
        }
    }
}

/// Placeholder options passed to `encode_pick`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PickOptions {}

/// Image dimensions and metadata returned by `PickCallback::encode_start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInformation {
    /// Image width in pixels.
    pub width: usize,
    /// Image height in pixels.
    pub height: usize,
    /// Optional encoder metadata.
    pub metadata: Option<Metadata>,
}

/// Supplies image data to the callback-based [`crate::encode`] entry point.
pub trait PickCallback: Sync + Send {
    /// Begins an encode operation and returns image properties.
    fn encode_start(&mut self) -> Result<ImageInformation, EncoderError>;
    /// Supplies one requested image rectangle as interleaved RGBA8 bytes.
    fn encode_pick(
        &mut self,
        start_x: usize,
        start_y: usize,
        width: usize,
        height: usize,
        option: Option<PickOptions>,
    ) -> Result<Vec<u8>, EncoderError>;
    /// Ends an encode operation.
    fn encode_end(&mut self) -> Result<(), EncoderError>;
    /// Supplies additional compatibility metadata.
    fn metadata(&mut self) -> Result<Option<Metadata>, EncoderError>;
}

/// Encoder options shared by typed and callback entry points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderOptions {
    /// Color quality in the libavif 0–100 scale.
    pub quality: u8,
    /// Alpha quality in the libavif 0–100 scale.
    pub quality_alpha: u8,
    /// libavif-compatible speed preset, 0–10. Lossless encoding keeps the
    /// complete search through 6 and uses fewer RDO candidates above 6.
    pub speed: u8,
    /// Request mathematically lossless encoding.
    pub lossless: bool,
    /// Number of worker threads; zero uses the backend default.
    pub jobs: usize,
    /// Vertical tile grid exponent.
    pub tile_rows_log2: u8,
    /// Horizontal tile grid exponent.
    pub tile_cols_log2: u8,
    /// Advanced backend options, including `cdef`, `lrf`, and transform
    /// settings supported by the selected rav1e backend.
    pub advanced: Metadata,
    /// `colr` metadata.
    pub color_information: Option<crate::ColorInformation>,
    /// `pixi` metadata and bit depth declaration.
    pub pixel_information: Option<crate::PixelInformation>,
}

impl Default for EncoderOptions {
    fn default() -> Self {
        Self {
            quality: 60,
            quality_alpha: 60,
            speed: 6,
            lossless: false,
            jobs: 0,
            tile_rows_log2: 0,
            tile_cols_log2: 0,
            advanced: Metadata::new(),
            color_information: Some(
                crate::NclxColorInformation {
                    color_primaries: 1,
                    transfer_characteristics: 13,
                    matrix_coefficients: 6,
                    full_range_flag: true,
                }
                .to_color_information(),
            ),
            pixel_information: None,
        }
    }
}

impl EncoderOptions {
    /// Creates options with the default libavif-compatible values.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Callback-based encoder options, paired with `avif-rust::DecodeOptions`.
pub struct EncodeOptions<'a> {
    /// Compatibility debug flag.
    pub debug_flag: usize,
    /// Image callback implementation.
    pub drawer: &'a mut dyn PickCallback,
    /// Optional compatibility metadata.
    pub options: Option<Metadata>,
}

impl<'a> EncodeOptions<'a> {
    /// Creates callback options with default metadata and debug settings.
    pub fn new(drawer: &'a mut dyn PickCallback) -> Self {
        Self {
            debug_flag: 0,
            drawer,
            options: None,
        }
    }
}
