//! Input description shared by the private lossless AV1 header and tile modules.

/// Plane data for the all-intra lossless layouts.
pub(crate) struct LosslessPlanes<'a> {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub monochrome: bool,
    pub subsampling_x: u8,
    pub subsampling_y: u8,
    pub color_primaries: u16,
    pub transfer_characteristics: u16,
    pub matrix_coefficients: u16,
    pub full_range: bool,
    pub planes: Vec<&'a [u16]>,
}
