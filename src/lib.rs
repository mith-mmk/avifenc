//! Pure Rust AVIF encoding entry points.
//!
//! The public vocabulary intentionally mirrors `avif-rust`: image buffers,
//! frame buffers, `*Information` metadata, `*Options` configuration, and
//! `encode`/`encode_bytes` entry points are kept stable and explicit.
#![deny(missing_docs)]

mod compat;
mod container;
mod encoder;
mod error;
mod image;
mod lossless_backend;
mod lossless_cdf;
mod lossless_headers;
mod lossless_tile;
mod lossless_types;

pub use compat::{
    CallbackResponse, DataMap, EncodeOptions, EncoderOptions, ImageInformation, Metadata,
    PickCallback, PickOptions, ResponseCommand,
};
pub use error::EncoderError;
pub use image::{
    ColorInformation, FrameBuffers, ImageBuffer, NclxColorInformation, PixelChannelInformation,
    PixelInformation, PixelSubsampling, PlaneBuffer, PlaneLayout, Rgba16ImageBuffer,
};

pub use encoder::{encode, encode_bytes, encode_frame_bytes, encode_rgba16_bytes};
