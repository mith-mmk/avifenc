use std::error::Error;
use std::fmt::{self, Display, Formatter};

/// Errors returned by AVIF encoding.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderError {
    /// An input or option value is invalid.
    InvalidParam(String),
    /// The supplied image or callback did not provide enough data.
    NotEnoughData(String),
    /// The AV1 bitstream or AVIF container could not be produced.
    Bitstream(String),
    /// The requested feature is not supported by this backend.
    Unsupported(String),
    /// An input/output operation failed.
    Io(String),
}

impl Display for EncoderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidParam(message) => write!(f, "invalid parameter: {message}"),
            Self::NotEnoughData(message) => write!(f, "not enough data: {message}"),
            Self::Bitstream(message) => write!(f, "bitstream error: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported feature: {message}"),
            Self::Io(message) => write!(f, "io error: {message}"),
        }
    }
}

impl Error for EncoderError {}
