//! The single-tile, all-intra, lossless encoder: superblock/partition iteration (§5.11.2/.4),
//! intra prediction (§7.11.2), the forward 4×4 WHT (via `gamut-dsp`), and coefficient coding
//! with full context derivation (§5.11.39, §8.3.2).
//!
//! All coded blocks are square intra blocks using lossless AV1 intra prediction;
//! full-resolution color blocks use deterministic recursive splitting to 4×4 DC leaves, while
//! monochrome and subsampled inputs keep the compact `PARTITION_NONE` path; right/bottom frame edges use
//! the spec's forced splitting. The frame is coded on
//! the MI-unit grid (`mi_cols*4 × mi_rows*4`, i.e. dimensions rounded up to a multiple of 8); the
//! out-of-frame padding is edge-replicated and cropped away on decode.

use crate::lossless_cdf as cdf;
use crate::lossless_types::LosslessPlanes;
use gamut_bitstream::SymbolEncoder as RangeEncoder;
use std::collections::HashMap;

const CFL_PRED: u8 = 13;

/// AV1's range coder state used only for fractional-rate estimation.
///
/// `gamut-bitstream` intentionally keeps the arithmetic state private.  The
/// encoder therefore mirrors the small §8.2.6 update here so local RDO can
/// compare candidates before the byte flush rounds their sizes to the same
/// boundary.  The real `RangeEncoder` remains the sole bitstream writer.
#[derive(Clone)]
struct RateEstimator {
    low: u64,
    rng: u32,
    cnt: i32,
    stream_bits: u32,
    rate_q10: u64,
}

impl RateEstimator {
    const EC_PROB_SHIFT: u32 = 6;
    const EC_MIN_PROB: u32 = 4;
    const CDF_PROB_TOP: u32 = 1 << 15;

    fn new() -> Self {
        Self {
            low: 0,
            rng: Self::CDF_PROB_TOP,
            cnt: -9,
            stream_bits: 0,
            rate_q10: 0,
        }
    }

    fn encode_symbol(&mut self, symbol: usize, cdf: &[u16]) {
        let nsyms = cdf.len() as u32;
        let fl = if symbol > 0 {
            Self::CDF_PROB_TOP - u32::from(cdf[symbol - 1])
        } else {
            Self::CDF_PROB_TOP
        };
        let fh = Self::CDF_PROB_TOP - u32::from(cdf[symbol]);
        self.encode_q15(fl, fh, symbol as u32, nsyms);
    }

    fn encode_literal(&mut self, value: u32, bits: u32) {
        const BOOL_CDF: [u16; 2] = [1 << 14, 1 << 15];
        for bit in (0..bits).rev() {
            self.encode_symbol(((value >> bit) & 1) as usize, &BOOL_CDF);
        }
    }

    fn encode_q15(&mut self, fl: u32, fh: u32, symbol: u32, nsyms: u32) {
        let mut low = self.low;
        let mut range = self.rng;
        let n = nsyms - 1;
        if fl < Self::CDF_PROB_TOP {
            let u = (((range >> 8) * (fl >> Self::EC_PROB_SHIFT)) >> (7 - Self::EC_PROB_SHIFT))
                + Self::EC_MIN_PROB * (n - (symbol - 1));
            let v = (((range >> 8) * (fh >> Self::EC_PROB_SHIFT)) >> (7 - Self::EC_PROB_SHIFT))
                + Self::EC_MIN_PROB * (n - symbol);
            low += u64::from(range - u);
            range = u - v;
        } else {
            let v = (((range >> 8) * (fh >> Self::EC_PROB_SHIFT)) >> (7 - Self::EC_PROB_SHIFT))
                + Self::EC_MIN_PROB * (n - symbol);
            range -= v;
        }

        self.normalize(low, range);
        self.rate_q10 = self.tell_frac_q10();
    }

    fn normalize(&mut self, mut low: u64, range: u32) {
        let shift = range.leading_zeros() - 16;
        let mut cnt = self.cnt;
        let mut s = cnt + shift as i32;
        if s >= 0 {
            cnt += 16;
            let mut mask = (1u64 << cnt) - 1;
            if s >= 8 {
                self.stream_bits += 8;
                low &= mask;
                cnt -= 8;
                mask = (1u64 << cnt) - 1;
            }
            self.stream_bits += 8;
            s = cnt + shift as i32 - 24;
            low &= mask;
        }
        self.low = low << shift;
        self.rng = range << shift;
        self.cnt = s;
    }

    /// Returns the fractional number of bits used by the current arithmetic
    /// interval, matching libaom/rav1e's `tell_frac` convention. The final
    /// range is accounted for instead of rounding every symbol's probability
    /// independently, which makes close RDO candidates deterministic.
    fn tell_frac_q10(&self) -> u64 {
        let total_bits =
            u64::from(self.stream_bits).saturating_add_signed(i64::from(self.cnt) + 10);
        let mut range = self.rng;
        let mut fractional = 0u64;
        for _ in 0..10 {
            range = (((u64::from(range) * u64::from(range)) >> 15) as u32).max(1);
            let bit = range >> 16;
            fractional = (fractional << 1) | u64::from(bit);
            range >>= bit;
        }
        total_bits * 1024 - fractional
    }
}

#[derive(Clone)]
struct AdaptiveSymbolEncoder {
    inner: Option<RangeEncoder>,
    cdfs: HashMap<usize, (Vec<u16>, u16)>,
    rate: RateEstimator,
}

impl AdaptiveSymbolEncoder {
    fn new() -> Self {
        Self {
            inner: Some(RangeEncoder::new()),
            cdfs: HashMap::new(),
            rate: RateEstimator::new(),
        }
    }

    fn encode_symbol(&mut self, symbol: usize, cdf: &'static [u16]) {
        self.encode_symbol_with_key(symbol, cdf, cdf.as_ptr() as usize);
    }

    fn encode_symbol_with_key(&mut self, symbol: usize, cdf: &[u16], key: usize) {
        let state = self.cdfs.entry(key).or_insert_with(|| (cdf.to_vec(), 0));
        self.rate.encode_symbol(symbol, &state.0);
        if let Some(inner) = self.inner.as_mut() {
            inner.encode_symbol_adapt(symbol, &mut state.0, &mut state.1);
        } else {
            adapt_cdf(&mut state.0, symbol, &mut state.1);
        }
    }

    /// Encodes a symbol with a temporary CDF. AV1's edge-restricted
    /// partition CDF is rebuilt for each partition and is not written back to
    /// the frame CDF state by the decoder.
    fn encode_symbol_once(&mut self, symbol: usize, cdf: &[u16]) {
        let mut state = cdf.to_vec();
        let mut count = 0;
        self.rate.encode_symbol(symbol, &state);
        if let Some(inner) = self.inner.as_mut() {
            inner.encode_symbol_adapt(symbol, &mut state, &mut count);
        } else {
            adapt_cdf(&mut state, symbol, &mut count);
        }
    }

    fn current_cdf(&mut self, cdf: &'static [u16]) -> Vec<u16> {
        let key = cdf.as_ptr() as usize;
        self.cdfs
            .entry(key)
            .or_insert_with(|| (cdf.to_vec(), 0))
            .0
            .clone()
    }

    fn encode_literal(&mut self, value: u32, bits: u32) {
        self.rate.encode_literal(value, bits);
        if let Some(inner) = self.inner.as_mut() {
            inner.encode_literal(value, bits);
        }
    }

    fn finish(self) -> Vec<u8> {
        self.inner
            .expect("the final encoder must retain its range writer")
            .finish()
    }
}

fn adapt_cdf(cdf: &mut [u16], symbol: usize, count: &mut u16) {
    let n = cdf.len();
    let rate = 3
        + u32::from(*count > 15)
        + u32::from(*count > 31)
        + (31 - (n as u32).leading_zeros()).min(2);
    let (_top, body) = cdf.split_last_mut().expect("a CDF has at least one entry");
    for value in &mut body[..symbol] {
        *value -= *value >> rate;
    }
    for value in &mut body[symbol..] {
        *value += ((1u16 << 15) - *value) >> rate;
    }
    if *count < 32 {
        *count += 1;
    }
}

#[derive(Clone)]
struct PaletteBlock {
    /// Sorted Y palette and its full-resolution color map.  Empty means that
    /// this block uses only the UV palette.
    colors: Vec<u16>,
    index_map: Vec<u8>,
    /// UV palette colors are stored as all U values followed by all V values,
    /// matching AV1's palette_uv_color syntax.
    uv_colors: Option<Vec<u16>>,
    uv_index_map: Option<Vec<u8>>,
}

impl PaletteBlock {
    fn has_y(&self) -> bool {
        !self.colors.is_empty()
    }

    fn has_uv(&self) -> bool {
        self.uv_colors.is_some()
    }
}

/// `NUM_BASE_LEVELS` (§3).
const NUM_BASE_LEVELS: i32 = 2;
/// `NUM_BASE_LEVELS + COEFF_BASE_RANGE`, the golomb threshold (§5.11.39).
const COEFF_BASE_PLUS_RANGE: i32 = 14;

const DC_PRED: u8 = 0;
const V_PRED: u8 = 1;
const H_PRED: u8 = 2;
const D45_PRED: u8 = 3;
const D135_PRED: u8 = 4;
const D113_PRED: u8 = 5;
const D157_PRED: u8 = 6;
const D203_PRED: u8 = 7;
const D67_PRED: u8 = 8;
const SMOOTH_PRED: u8 = 9;
const SMOOTH_V_PRED: u8 = 10;
const SMOOTH_H_PRED: u8 = 11;
const PAETH_PRED: u8 = 12;

const SMOOTH_WEIGHTS_4: [i32; 4] = [255, 149, 85, 64];
const SMOOTH_WEIGHTS_8: [i32; 8] = [255, 197, 146, 105, 73, 50, 37, 32];
const SMOOTH_WEIGHTS_16: [i32; 16] = [
    255, 225, 196, 170, 145, 123, 102, 84, 68, 54, 43, 33, 26, 20, 17, 16,
];
const SMOOTH_WEIGHTS_32: [i32; 32] = [
    255, 240, 225, 210, 196, 182, 169, 157, 145, 133, 122, 111, 101, 92, 83, 74, 66, 59, 52, 45,
    39, 34, 29, 25, 21, 17, 14, 12, 10, 9, 8, 8,
];
const SMOOTH_WEIGHTS_64: [i32; 64] = [
    255, 248, 240, 233, 225, 218, 210, 203, 196, 189, 182, 176, 169, 163, 156, 150, 144, 138, 133,
    127, 121, 116, 111, 106, 101, 96, 91, 86, 82, 77, 73, 69, 65, 61, 57, 54, 50, 47, 44, 41, 38,
    35, 32, 29, 27, 25, 22, 20, 18, 16, 15, 13, 12, 10, 9, 8, 7, 6, 6, 5, 5, 4, 4, 4,
];

/// `Partition_Context` values for the terminal partition types emitted by this encoder.
/// The table is `above/left × block-level (128..8) × NONE/HORZ/VERT/SPLIT` (§5.11.4).
const AL_PART_CTX: [[[u8; 4]; 5]; 2] = [
    [
        [0x00, 0x00, 0x10, 0x00],
        [0x10, 0x10, 0x18, 0x00],
        [0x18, 0x18, 0x1c, 0x00],
        [0x1c, 0x1c, 0x1e, 0x00],
        [0x1e, 0x1e, 0x1f, 0x1f],
    ],
    [
        [0x00, 0x10, 0x00, 0x00],
        [0x10, 0x18, 0x10, 0x00],
        [0x18, 0x1c, 0x18, 0x00],
        [0x1c, 0x1e, 0x1c, 0x00],
        [0x1e, 0x1f, 0x1e, 0x1f],
    ],
];

/// Row stride for the superblock-local `BlockDecoded` map. The guarded range
/// is `-1..=16` in both directions (§5.11.3).
const BD_STRIDE: usize = 18;

/// Returns whether a color frame contains a block that can use the narrow
/// lossless palette path.  Screen-content syntax is enabled at frame scope,
/// so avoid paying its per-block signaling cost when no such block exists.
pub(crate) fn needs_screen_content_tools(planes: &LosslessPlanes<'_>) -> bool {
    if planes.monochrome {
        return true;
    }
    if planes.subsampling_x != 0
        || planes.subsampling_y != 0
        || planes.width < 8
        || planes.height < 8
    {
        return false;
    }
    let width = planes.width as usize;
    let height = planes.height as usize;
    for &block_width in &[8usize, 16, 32, 64] {
        if block_width > width || block_width > height {
            continue;
        }
        for sy in (0..=height - block_width).step_by(block_width) {
            for sx in (0..=width - block_width).step_by(block_width) {
                if color_palette_candidate(planes, sx, sy, block_width) {
                    return true;
                }
            }
        }
    }
    false
}

fn color_palette_candidate(
    planes: &LosslessPlanes<'_>,
    sx: usize,
    sy: usize,
    block_width: usize,
) -> bool {
    let width = planes.width as usize;
    let y_candidate =
        palette_values_candidate(planes.planes[0], width, sx, sy, block_width, block_width);
    let uv_candidate = !planes.monochrome
        && planes.subsampling_x == 0
        && planes.subsampling_y == 0
        && planes.planes.len() >= 3
        && palette_pairs_candidate(
            planes.planes[1],
            planes.planes[2],
            width,
            sx,
            sy,
            block_width,
        );
    y_candidate || uv_candidate
}

fn palette_values_candidate(
    samples: &[u16],
    width: usize,
    sx: usize,
    sy: usize,
    block_width: usize,
    block_height: usize,
) -> bool {
    let mut colors = Vec::with_capacity(8);
    for y in 0..block_height {
        for x in 0..block_width {
            let value = samples[(sy + y) * width + sx + x];
            if !colors.contains(&value) {
                colors.push(value);
                if colors.len() > 8 {
                    return false;
                }
            }
        }
    }
    colors.len() >= 2
}

fn palette_pairs_candidate(
    u: &[u16],
    v: &[u16],
    width: usize,
    sx: usize,
    sy: usize,
    block_width: usize,
) -> bool {
    let mut pairs = Vec::with_capacity(8);
    for y in 0..block_width {
        for x in 0..block_width {
            let pair = (u[(sy + y) * width + sx + x], v[(sy + y) * width + sx + x]);
            if !pairs.contains(&pair) {
                pairs.push(pair);
                if pairs.len() > 8 {
                    return false;
                }
            }
        }
    }
    pairs.len() >= 2
        && pairs
            .iter()
            .all(|(u, _)| pairs.iter().filter(|(v, _)| v == u).count() == 1)
}

fn needs_uv_palette_tools(planes: &LosslessPlanes<'_>) -> bool {
    if planes.monochrome
        || planes.subsampling_x != 0
        || planes.subsampling_y != 0
        || planes.planes.len() < 3
        || planes.width < 8
        || planes.height < 8
    {
        return false;
    }
    let width = planes.width as usize;
    let height = planes.height as usize;
    for block_width in [8usize, 16, 32, 64] {
        if block_width > width || block_width > height {
            continue;
        }
        for sy in (0..=height - block_width).step_by(block_width) {
            for sx in (0..=width - block_width).step_by(block_width) {
                if palette_pairs_candidate(
                    planes.planes[1],
                    planes.planes[2],
                    width,
                    sx,
                    sy,
                    block_width,
                ) {
                    return true;
                }
            }
        }
    }
    false
}

/// Encoder for the single tile that spans the whole frame.
pub(crate) struct FrameEncoder<'a> {
    planes: Vec<&'a [u16]>,
    bit_depth: u8,
    monochrome: bool,
    subsampling_x: usize,
    subsampling_y: usize,
    plane_widths: Vec<usize>,
    plane_heights: Vec<usize>,
    plane_coded_widths: Vec<usize>,
    plane_coded_heights: Vec<usize>,
    /// Decoder-visible reconstructed samples on the aligned coded grid.
    reconstructed: Vec<Vec<u16>>,
    mi_cols: usize,
    mi_rows: usize,
    sym: AdaptiveSymbolEncoder,
    above_level: Vec<Vec<u8>>,
    above_dc: Vec<Vec<u8>>,
    left_level: Vec<Vec<u8>>,
    left_dc: Vec<Vec<u8>>,
    /// `Mi_Width_Log2` of the block covering each MI cell (for the partition context).
    mi_bsl: Vec<u8>,
    /// Whether the block covering each MI cell was coded with `skip = 1`.
    mi_skip: Vec<bool>,
    /// Y intra mode of the block covering each MI cell.
    mi_ymode: Vec<u8>,
    /// AV1 AbovePartitionContext/LeftPartitionContext bitmasks (§5.11.4).
    above_partition: Vec<u8>,
    left_partition: Vec<u8>,
    /// Superblock-local luma transform availability for directional edge
    /// extension (§5.11.34).
    sb_r: usize,
    sb_c: usize,
    block_decoded: Vec<Vec<u8>>,
    /// Palette size of the block covering each MI cell (zero for no palette).
    mi_palette_size: Vec<u8>,
    /// Sorted luma palette colours of the block covering each MI cell.
    mi_palette_colors: Vec<[u16; 8]>,
    /// UV palette U colours of the block covering each MI cell.
    mi_uv_palette_size: Vec<u8>,
    mi_uv_palette_colors: Vec<[u16; 8]>,
    uv_palette_content: bool,
    screen_content_tools: bool,
}

type MiContext = (usize, u8, bool, u8, u8, [u16; 8], u8, [u16; 8]);

struct BlockContextSnapshot {
    planes: Vec<PlaneBlockContextSnapshot>,
    above_partition_start: usize,
    above_partition: Vec<u8>,
    left_partition_start: usize,
    left_partition: Vec<u8>,
    mi: Vec<MiContext>,
}

struct PlaneBlockContextSnapshot {
    above_start: usize,
    above_level: Vec<u8>,
    above_dc: Vec<u8>,
    left_start: usize,
    left_level: Vec<u8>,
    left_dc: Vec<u8>,
    reconstruction: PlaneReconstructionSnapshot,
}

struct PlaneReconstructionSnapshot {
    x: usize,
    y: usize,
    width: usize,
    samples: Vec<u16>,
    block_decoded: Vec<(usize, u8)>,
}

#[derive(Clone, Copy)]
struct PlaneBlockRegion {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    width4: usize,
    height4: usize,
}

#[derive(Clone)]
struct LeafPlaneSnapshot {
    above_index: usize,
    above_level: u8,
    above_dc: u8,
    left_index: usize,
    left_level: u8,
    left_dc: u8,
    block_decoded_index: usize,
    block_decoded: u8,
    reconstructed_x: usize,
    reconstructed_y: usize,
    reconstructed: [u16; 16],
}

#[derive(Clone)]
struct LeafContextSnapshot {
    planes: Vec<LeafPlaneSnapshot>,
    mi: Option<MiContext>,
}

impl<'a> FrameEncoder<'a> {
    /// Creates an encoder over full-resolution planes (Y=G, U=B, V=R for identity CICP).
    pub(crate) fn new(planes: &'a LosslessPlanes<'a>, screen_content_tools: bool) -> Self {
        let width = planes.width as usize;
        let height = planes.height as usize;
        let mi_cols = 2 * ((width + 7) >> 3);
        let mi_rows = 2 * ((height + 7) >> 3);
        let plane_count = planes.planes.len();
        let plane_widths: Vec<usize> = planes
            .planes
            .iter()
            .enumerate()
            .map(|(plane, _)| {
                if plane == 0 || planes.monochrome {
                    width
                } else {
                    width.div_ceil(1usize << usize::from(planes.subsampling_x))
                }
            })
            .collect();
        let plane_heights: Vec<usize> = planes
            .planes
            .iter()
            .enumerate()
            .map(|(plane, _)| {
                if plane == 0 || planes.monochrome {
                    height
                } else {
                    height.div_ceil(1usize << usize::from(planes.subsampling_y))
                }
            })
            .collect();
        let plane_coded_widths: Vec<usize> = planes
            .planes
            .iter()
            .enumerate()
            .map(|(plane, _)| {
                if plane == 0 || planes.monochrome {
                    mi_cols * 4
                } else {
                    (mi_cols * 4).div_ceil(1usize << usize::from(planes.subsampling_x))
                }
            })
            .collect();
        let plane_coded_heights: Vec<usize> = planes
            .planes
            .iter()
            .enumerate()
            .map(|(plane, _)| {
                if plane == 0 || planes.monochrome {
                    mi_rows * 4
                } else {
                    (mi_rows * 4).div_ceil(1usize << usize::from(planes.subsampling_y))
                }
            })
            .collect();
        let reconstructed = (0..plane_count)
            .map(|plane| {
                let coded_width = plane_coded_widths[plane];
                let coded_height = plane_coded_heights[plane];
                let source_width = plane_widths[plane];
                let source_height = plane_heights[plane];
                let source = planes.planes[plane];
                (0..coded_height)
                    .flat_map(|y| {
                        let source_y = y.min(source_height - 1);
                        (0..coded_width).map(move |x| {
                            let source_x = x.min(source_width - 1);
                            source[source_y * source_width + source_x]
                        })
                    })
                    .collect()
            })
            .collect();
        Self {
            planes: planes.planes.clone(),
            bit_depth: planes.bit_depth,
            monochrome: planes.monochrome,
            subsampling_x: usize::from(planes.subsampling_x),
            subsampling_y: usize::from(planes.subsampling_y),
            plane_widths,
            plane_heights,
            plane_coded_widths,
            plane_coded_heights,
            reconstructed,
            mi_cols,
            mi_rows,
            sym: AdaptiveSymbolEncoder::new(),
            above_level: (0..plane_count)
                .map(|plane| {
                    vec![
                        0;
                        mi_cols.div_ceil(
                            1usize
                                << if plane == 0 {
                                    0
                                } else {
                                    usize::from(planes.subsampling_x)
                                }
                        )
                    ]
                })
                .collect(),
            above_dc: (0..plane_count)
                .map(|plane| {
                    vec![
                        0;
                        mi_cols.div_ceil(
                            1usize
                                << if plane == 0 {
                                    0
                                } else {
                                    usize::from(planes.subsampling_x)
                                }
                        )
                    ]
                })
                .collect(),
            left_level: (0..plane_count)
                .map(|plane| {
                    vec![
                        0;
                        mi_rows.div_ceil(
                            1usize
                                << if plane == 0 {
                                    0
                                } else {
                                    usize::from(planes.subsampling_y)
                                }
                        )
                    ]
                })
                .collect(),
            left_dc: (0..plane_count)
                .map(|plane| {
                    vec![
                        0;
                        mi_rows.div_ceil(
                            1usize
                                << if plane == 0 {
                                    0
                                } else {
                                    usize::from(planes.subsampling_y)
                                }
                        )
                    ]
                })
                .collect(),
            mi_bsl: vec![0; mi_cols * mi_rows],
            mi_skip: vec![false; mi_cols * mi_rows],
            mi_ymode: vec![DC_PRED; mi_cols * mi_rows],
            above_partition: vec![0; mi_cols],
            left_partition: vec![0; mi_rows],
            sb_r: 0,
            sb_c: 0,
            block_decoded: vec![vec![0; BD_STRIDE * BD_STRIDE]; plane_count],
            mi_palette_size: vec![0; mi_cols * mi_rows],
            mi_palette_colors: vec![[0; 8]; mi_cols * mi_rows],
            mi_uv_palette_size: vec![0; mi_cols * mi_rows],
            mi_uv_palette_colors: vec![[0; 8]; mi_cols * mi_rows],
            uv_palette_content: needs_uv_palette_tools(planes),
            screen_content_tools,
        }
    }

    /// Encodes the tile and returns the symbol-coded bytes (`decode_tile`, §5.11.2).
    pub(crate) fn encode(mut self) -> Vec<u8> {
        const SB4: usize = 16; // 64×64 superblock in MI units
        // Monochrome planes (including the auxiliary alpha item) have no
        // chroma partition work, so the deeper generic partition RDO is
        // affordable there. Keep the color path at the shallower search to
        // avoid multiplying the cost of large RGB images without changing
        // their selected partitions.
        let rdo_depth = if self.monochrome { 4 } else { 1 };
        let mut r = 0;
        while r < self.mi_rows {
            for plane in 0..self.planes.len() {
                self.left_level[plane].iter_mut().for_each(|v| *v = 0);
                self.left_dc[plane].iter_mut().for_each(|v| *v = 0);
            }
            let mut c = 0;
            while c < self.mi_cols {
                self.sb_r = r;
                self.sb_c = c;
                self.clear_block_decoded(r, c);
                // One local NONE/SPLIT probe per superblock avoids cloning the
                // full-frame range coder while still accounting for the
                // current adaptive CDFs and block contexts.
                self.encode_partition_depth(r, c, 64, rdo_depth);
                c += SB4;
            }
            r += SB4;
        }
        let mut output = self.sym.finish();
        // The AV1 range decoder needs the two-byte initial refill window even
        // when a flat subsampled plane emits almost no symbols.
        output.resize(output.len().max(2), 0);
        output
    }

    /// Padded (edge-replicated) source sample of `plane` at coded-grid position `(x, y)`.
    fn sample(&self, plane: usize, x: usize, y: usize) -> i32 {
        let width = self.plane_widths[plane];
        let height = self.plane_heights[plane];
        let xx = x.min(width - 1);
        let yy = y.min(height - 1);
        i32::from(self.planes[plane][yy * width + xx])
    }

    fn reconstructed_sample(&self, plane: usize, x: usize, y: usize) -> i32 {
        let width = self.plane_coded_widths[plane];
        let height = self.plane_coded_heights[plane];
        i32::from(self.reconstructed[plane][y.min(height - 1) * width + x.min(width - 1)])
    }

    fn reconstruct_source_block(&mut self, plane: usize, sx: usize, sy: usize) {
        let width = self.plane_coded_widths[plane];
        let height = self.plane_coded_heights[plane];
        for y in sy..(sy + 4).min(height) {
            for x in sx..(sx + 4).min(width) {
                self.reconstructed[plane][y * width + x] = self.sample(plane, x, y) as u16;
            }
        }
    }

    fn encode_partition_depth(&mut self, r: usize, c: usize, bw: usize, rdo_depth: u8) {
        if r >= self.mi_rows || c >= self.mi_cols {
            return;
        }
        let num4x4 = bw / 4;
        let half = num4x4 >> 1;
        let has_rows = r + half < self.mi_rows;
        let has_cols = c + half < self.mi_cols;
        let fine_partition = self.screen_content_tools
            && !self.monochrome
            && self.subsampling_x == 0
            && self.subsampling_y == 0
            && self.uv_palette_content;
        if fine_partition
            && bw > 8
            && has_rows
            && has_cols
            && self.palette_block(r, c, bw).is_none()
        {
            let bsl = num4x4.trailing_zeros() as usize;
            let ctx = self.partition_ctx(r, c, bsl);
            self.sym.encode_symbol(3, partition_cdf(bsl, ctx));
            if bsl == 1 {
                self.update_partition_ctx(r, c, bsl, 3);
            }
            let h = bw / 2;
            self.encode_partition_depth(r, c, h, rdo_depth);
            self.encode_partition_depth(r, c + half, h, rdo_depth);
            self.encode_partition_depth(r + half, c, h, rdo_depth);
            self.encode_partition_depth(r + half, c + half, h, rdo_depth);
            return;
        }

        if rdo_depth > 0
            && bw >= 16
            && has_rows
            && has_cols
            && self.subsampling_x == 0
            && self.subsampling_y == 0
            && !self.block_is_skippable(r, c, bw, DC_PRED)
        {
            self.encode_partition_rdo(r, c, bw, rdo_depth);
            return;
        }

        self.encode_partition_heuristic(r, c, bw, rdo_depth);
    }

    fn encode_partition_rdo(&mut self, r: usize, c: usize, bw: usize, rdo_depth: u8) {
        let num4x4 = bw / 4;
        let half = num4x4 >> 1;
        let has_rows = r + half < self.mi_rows;
        let has_cols = c + half < self.mi_cols;
        let child_depth = rdo_depth.saturating_sub(1);
        let crosses_visible_edge =
            r * 4 + bw > self.plane_heights[0] || c * 4 + bw > self.plane_widths[0];
        let mut best = (self.measure_partition_local(r, c, bw, 0, 0, false), 0usize);
        if has_rows && has_cols {
            // Rectangular children are especially useful at 16×16, where
            // libaom frequently keeps one dimension at 8 pixels. Keep the
            // first implementation bounded to that size until rectangular
            // RDO has enough coverage for larger blocks.
            if bw == 16 && !crosses_visible_edge {
                for partition in [1usize, 2] {
                    let size = self.measure_partition_local(r, c, bw, partition, 0, false);
                    if size < best.0 {
                        best = (size, partition);
                    }
                }
            }
            let partition = 3;
            let size = self.measure_partition_local(r, c, bw, partition, child_depth, false);
            if size < best.0 {
                best = (size, partition);
            }
        }
        self.encode_partition_choice(r, c, bw, best.1, rdo_depth.saturating_sub(1), false);
    }

    fn encode_partition_choice(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        partition: usize,
        child_rdo_depth: u8,
        force_dc: bool,
    ) {
        let num4x4 = bw / 4;
        let half = num4x4 >> 1;
        let bsl = num4x4.trailing_zeros() as usize;
        let ctx = self.partition_ctx(r, c, bsl);
        self.sym.encode_symbol(partition, partition_cdf(bsl, ctx));
        if bw >= 8 && (partition == 0 || partition == 3 && bsl == 1) {
            self.update_partition_ctx(r, c, bsl, partition);
        }

        let h = bw / 2;
        match partition {
            0 => {
                if force_dc {
                    let uv_mode = if self.monochrome {
                        DC_PRED
                    } else {
                        self.select_uv_mode_for_y(r, c, bw, DC_PRED)
                    };
                    self.encode_block_with_mode(r, c, bw, None, DC_PRED, uv_mode);
                } else {
                    self.encode_block(r, c, bw);
                }
            }
            1 => {
                self.encode_rectangular_none(r, c, bw, h);
                self.encode_rectangular_none(r + half, c, bw, h);
            }
            2 => {
                self.encode_rectangular_none(r, c, h, bw);
                self.encode_rectangular_none(r, c + half, h, bw);
            }
            3 => {
                self.encode_partition_depth(r, c, h, child_rdo_depth);
                self.encode_partition_depth(r, c + half, h, child_rdo_depth);
                self.encode_partition_depth(r + half, c, h, child_rdo_depth);
                self.encode_partition_depth(r + half, c + half, h, child_rdo_depth);
            }
            _ => unreachable!("lossless partition RDO selected an invalid partition"),
        }
        if bw >= 8 && matches!(partition, 1 | 2) {
            // The decoder updates the parent partition context only after all
            // HORZ/VERT children have been decoded.
            if partition == 1 {
                self.update_partition_ctx_shape(r, c, bw, bw, bw, h);
            } else {
                self.update_partition_ctx_shape(r, c, bw, bw, h, bw);
            }
        }
    }

    fn encode_partition_heuristic(&mut self, r: usize, c: usize, bw: usize, child_rdo_depth: u8) {
        let num4x4 = bw / 4;
        let half = num4x4 >> 1;
        let has_rows = r + half < self.mi_rows;
        let has_cols = c + half < self.mi_cols;
        let bsl = num4x4.trailing_zeros() as usize; // Mi_Width_Log2

        let partition = if bw < 8 {
            0 // PARTITION_NONE forced, no symbol
        } else if !self.monochrome
            && self.subsampling_x == 0
            && self.subsampling_y == 0
            && bw >= 8
            && has_rows
            && has_cols
            && !self.block_is_skippable(r, c, bw, DC_PRED)
            && self.exact_skip_mode(r, c, bw).is_none()
            // Large heuristic blocks still split to expose local predictors.
            // At 16x16 and 8x8, compare the complete NONE and SPLIT syntax;
            // forcing every textured block down to 4x4 spends far more mode
            // and partition symbols than the residual reduction can repay.
            && (bw > 16 || self.should_split_square_local(r, c, bw))
        {
            let ctx = self.partition_ctx(r, c, bsl);
            self.sym.encode_symbol(3, partition_cdf(bsl, ctx)); // split for per-block DC prediction
            3
        } else if has_rows && has_cols {
            let ctx = self.partition_ctx(r, c, bsl);
            self.sym.encode_symbol(0, partition_cdf(bsl, ctx)); // PARTITION_NONE
            0
        } else if has_cols {
            let ctx = self.partition_ctx(r, c, bsl);
            let partition = partition_cdf(bsl, ctx);
            let current = self.sym.current_cdf(partition);
            let cdf2 = split_or_horz_cdf(&current);
            self.sym.encode_symbol_once(1, &cdf2);
            3
        } else if has_rows {
            let ctx = self.partition_ctx(r, c, bsl);
            let partition = partition_cdf(bsl, ctx);
            let current = self.sym.current_cdf(partition);
            let cdf2 = split_or_vert_cdf(&current);
            self.sym.encode_symbol_once(1, &cdf2);
            3
        } else {
            3 // forced PARTITION_SPLIT, no symbol
        };

        if bw >= 8 && (partition != 3 || bsl == 1) {
            self.update_partition_ctx(r, c, bsl, partition);
        }

        if partition == 0 {
            self.encode_block(r, c, bw);
        } else {
            let h = bw / 2;
            self.encode_partition_depth(r, c, h, child_rdo_depth);
            self.encode_partition_depth(r, c + half, h, child_rdo_depth);
            self.encode_partition_depth(r + half, c, h, child_rdo_depth);
            self.encode_partition_depth(r + half, c + half, h, child_rdo_depth);
        }
    }

    /// Emits the implicit leaf partition for a rectangular child of a
    /// HORZ/VERT split. Rectangular blocks still carry PARTITION_NONE when
    /// both edges are available; the partition CDF is selected by the
    /// child's width, as required by AV1's `BlockSize::width_mi_log2`.
    fn encode_rectangular_none(&mut self, r: usize, c: usize, bw: usize, bh: usize) {
        if bw >= 8 && bh >= 8 {
            let bsl = (bw / 4).trailing_zeros() as usize;
            let ctx = self.partition_ctx(r, c, bsl);
            self.sym.encode_symbol(0, partition_cdf(bsl, ctx));
            self.update_partition_ctx_shape(r, c, bw, bh, bw, bh);
        }
        self.encode_block_shape(r, c, bw, bh);
    }

    fn update_partition_ctx_shape(
        &mut self,
        r: usize,
        c: usize,
        context_width: usize,
        context_height: usize,
        sub_width: usize,
        sub_height: usize,
    ) {
        let (above, left) = partition_contexts_for_dimensions(sub_width, sub_height);
        for k in 0..(context_width / 4) {
            if c + k < self.mi_cols {
                self.above_partition[c + k] = above;
            }
        }
        for k in 0..(context_height / 4) {
            if r + k < self.mi_rows {
                self.left_partition[r + k] = left;
            }
        }
    }

    fn partition_ctx(&self, r: usize, c: usize, bsl: usize) -> usize {
        // §8.3.2 reads bit `bsl - 1` from the partition context of the
        // neighbour, rather than comparing the neighbour's terminal block
        // size.  The distinction matters after a split partition.
        let bit = bsl - 1;
        let above = (self.above_partition[c] >> bit) & 1;
        let left = if c > 0 {
            (self.left_partition[r] >> bit) & 1
        } else {
            0
        };
        usize::from(left) * 2 + usize::from(above)
    }

    fn update_partition_ctx(&mut self, r: usize, c: usize, bsl: usize, partition: usize) {
        let block_level = 5 - bsl;
        let n4 = 1usize << bsl;
        let above = AL_PART_CTX[0][block_level][partition];
        let left = AL_PART_CTX[1][block_level][partition];
        for k in 0..n4 {
            if c + k < self.mi_cols {
                self.above_partition[c + k] = above;
            }
            if r + k < self.mi_rows {
                self.left_partition[r + k] = left;
            }
        }
    }

    /// Clears `BlockDecoded` for the current 64×64 superblock while retaining
    /// the already reconstructed row above and column to the left.
    fn clear_block_decoded(&mut self, r: usize, c: usize) {
        const SB4: isize = 16;
        let sb_width4 = (self.mi_cols - c).min(SB4 as usize) as isize;
        let sb_height4 = (self.mi_rows - r).min(SB4 as usize) as isize;
        for plane in &mut self.block_decoded {
            for by in -1..=SB4 {
                for bx in -1..=SB4 {
                    let above = by < 0 && r > 0 && bx >= 0 && bx < sb_width4;
                    let left = bx < 0 && c > 0 && by >= 0 && by < sb_height4;
                    plane[(by + 1) as usize * BD_STRIDE + (bx + 1) as usize] =
                        u8::from(above || left);
                }
            }
            // The below-left guard is explicitly unavailable at a
            // superblock edge.
            plane[(SB4 + 1) as usize * BD_STRIDE] = 0;
        }
    }

    fn block_decoded_at(&self, plane: usize, by: isize, bx: isize) -> bool {
        if !(-1..=16).contains(&by) || !(-1..=16).contains(&bx) {
            return false;
        }
        self.block_decoded[plane][(by + 1) as usize * BD_STRIDE + (bx + 1) as usize] != 0
    }

    fn mark_block_decoded_cell(&mut self, plane: usize, r: usize, c: usize) {
        let by = r as isize - self.sb_r as isize;
        let bx = c as isize - self.sb_c as isize;
        if (0..16).contains(&by) && (0..16).contains(&bx) {
            self.block_decoded[plane][(by + 1) as usize * BD_STRIDE + (bx + 1) as usize] = 1;
        }
    }

    fn mark_block_decoded_shape(&mut self, r: usize, c: usize, bw: usize, bh: usize) {
        for plane in 0..self.planes.len() {
            let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
            let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
            let n4_x = (bw >> shift_x).max(4) / 4;
            let n4_y = (bh >> shift_y).max(4) / 4;
            let start_x = (c * 4) >> shift_x;
            let start_y = (r * 4) >> shift_y;
            for y in 0..n4_y {
                for x in 0..n4_x {
                    self.mark_block_decoded_cell(plane, (start_y >> 2) + y, (start_x >> 2) + x);
                    self.reconstruct_source_block(plane, start_x + x * 4, start_y + y * 4);
                }
            }
        }
    }

    fn encode_block(&mut self, r: usize, c: usize, bw: usize) {
        self.encode_block_shape(r, c, bw, bw);
    }

    fn encode_block_shape(&mut self, r: usize, c: usize, bw: usize, bh: usize) {
        let palette_candidate = (bw == bh).then(|| self.palette_block(r, c, bw)).flatten();
        let normal_y_mode = if bw == bh {
            self.exact_skip_mode(r, c, bw)
        } else {
            None
        }
        .unwrap_or_else(|| self.select_y_mode_shape(r, c, bw, bh));
        let normal_uv_mode = if self.monochrome {
            DC_PRED
        } else {
            self.select_uv_mode_for_y_shape(r, c, bw, bh, normal_y_mode)
        };
        let (normal_y_angle_delta, normal_uv_angle_delta) =
            self.select_angle_deltas(r, c, bw, normal_y_mode, normal_uv_mode);
        let palette = palette_candidate.and_then(|candidate| {
            // Larger palettes are worthwhile only when their complete syntax
            // (palette size, cache/color signaling, and map tokens) beats the
            // ordinary intra block. Keep this probe local so textured blocks
            // do not inherit a palette merely because they have <= 8 colors.
            if bw < 8 {
                return None;
            }
            let palette_y_mode = if candidate.has_y() {
                DC_PRED
            } else {
                normal_y_mode
            };
            let palette_uv_mode = if candidate.has_uv() {
                DC_PRED
            } else {
                normal_uv_mode
            };
            let palette_rate = self.measure_block_modes_local(
                r,
                c,
                bw,
                palette_y_mode,
                palette_uv_mode,
                Some(candidate.clone()),
            );
            let normal_rate =
                self.measure_block_modes_local(r, c, bw, normal_y_mode, normal_uv_mode, None);
            (palette_rate < normal_rate).then_some(candidate)
        });
        let y_mode = if palette.is_some() {
            palette
                .as_ref()
                .filter(|candidate| candidate.has_y())
                .map_or(normal_y_mode, |_| DC_PRED)
        } else {
            normal_y_mode
        };
        let normal_uv_mode = if palette.is_some() {
            palette
                .as_ref()
                .filter(|candidate| candidate.has_uv())
                .map_or(normal_uv_mode, |_| DC_PRED)
        } else {
            normal_uv_mode
        };
        let y_angle_delta = if palette.as_ref().is_some_and(PaletteBlock::has_y) {
            0
        } else {
            normal_y_angle_delta
        };
        let uv_angle_delta = if palette.as_ref().is_some_and(PaletteBlock::has_uv) {
            0
        } else {
            normal_uv_angle_delta
        };
        let cfl = if palette.is_none() {
            self.select_cfl_alpha(r, c, bw, bh, normal_uv_mode)
        } else {
            None
        };
        let uv_mode = cfl.map_or(normal_uv_mode, |_| CFL_PRED);
        self.encode_block_with_mode_shape_cfl(
            r,
            c,
            bw,
            bh,
            palette,
            y_mode,
            uv_mode,
            y_angle_delta,
            uv_angle_delta,
            cfl,
        );
    }

    fn encode_block_with_mode(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        palette: Option<PaletteBlock>,
        y_mode: u8,
        uv_mode: u8,
    ) {
        self.encode_block_with_mode_shape(r, c, bw, bw, palette, y_mode, uv_mode, 0, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_block_with_mode_shape(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        palette: Option<PaletteBlock>,
        y_mode: u8,
        uv_mode: u8,
        y_angle_delta: i8,
        uv_angle_delta: i8,
    ) {
        self.encode_block_with_mode_shape_cfl(
            r,
            c,
            bw,
            bh,
            palette,
            y_mode,
            uv_mode,
            y_angle_delta,
            uv_angle_delta,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_block_with_mode_shape_cfl(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        palette: Option<PaletteBlock>,
        y_mode: u8,
        uv_mode: u8,
        y_angle_delta: i8,
        uv_angle_delta: i8,
        cfl: Option<(i8, i8)>,
    ) {
        let n4_x = bw / 4;
        let n4_y = bh / 4;
        let bsl = n4_x.trailing_zeros() as u8;
        let chroma_reference = !self.monochrome && self.is_chroma_reference_shape(r, c, bw, bh);
        let has_luma_palette = palette.as_ref().is_some_and(PaletteBlock::has_y);
        let has_uv_palette = palette.as_ref().is_some_and(PaletteBlock::has_uv);
        let skip = uv_mode != CFL_PRED
            && self.block_is_skippable_with_modes_shape(r, c, bw, bh, y_mode, uv_mode);
        // intra_frame_mode_info: skip, then the selected y_mode.  The mode
        // context is derived from the already-coded above/left MI cells.
        let skip_ctx = self.skip_ctx(r, c);
        self.sym
            .encode_symbol(usize::from(skip), &cdf::SKIP[skip_ctx]);
        let above_mode = if r > 0 {
            self.mi_ymode[(r - 1) * self.mi_cols + c]
        } else {
            DC_PRED
        };
        let left_mode = if c > 0 {
            self.mi_ymode[r * self.mi_cols + c - 1]
        } else {
            DC_PRED
        };
        let above_context = intra_mode_context(above_mode);
        let left_context = intra_mode_context(left_mode);
        let y_mode_cdf = intra_frame_y_mode_cdf(above_context, left_context);
        self.sym.encode_symbol(usize::from(y_mode), y_mode_cdf);
        if bw >= 8 && is_directional_mode(y_mode) {
            self.sym
                .encode_symbol(angle_delta_symbol(y_angle_delta), angle_delta_cdf(y_mode));
        }
        if chroma_reference {
            // In coded-lossless mode CFL is allowed when the subsampled chroma
            // block is exactly 4x4.  This includes an 8x8 luma block in 4:2:0.
            let chroma_width = (bw >> self.subsampling_x).max(4);
            let chroma_height = (bh >> self.subsampling_y).max(4);
            let cfl_allowed = chroma_width == 4 && chroma_height == 4;
            if cfl_allowed {
                let uv = uv_mode_cfl_allowed_cdf(y_mode);
                self.sym.encode_symbol(usize::from(uv_mode), uv);
            } else {
                let uv = uv_mode_cfl_not_allowed_cdf(y_mode);
                self.sym.encode_symbol(usize::from(uv_mode), uv);
            }
            if uv_mode == CFL_PRED {
                let (alpha_u, alpha_v) = cfl.expect("CFL mode requires alpha parameters");
                self.encode_cfl_params(alpha_u, alpha_v);
            } else if bw >= 8 && is_directional_mode(uv_mode) {
                self.sym
                    .encode_symbol(angle_delta_symbol(uv_angle_delta), angle_delta_cdf(uv_mode));
            }
        }

        // Palette syntax is present only when screen-content tools are enabled.
        // The frame-level flag is gated by a source scan so ordinary color
        // images do not pay the per-block has_palette signaling cost.
        if self.screen_content_tools && bw == bh && bw >= 8 {
            let block_size_context = palette_block_size_context(bw);
            let palette_context = self.palette_context(r, c);
            if y_mode == DC_PRED {
                let has_palette = has_luma_palette;
                self.sym.encode_symbol(
                    usize::from(has_palette),
                    &cdf::PALETTE_Y_MODE[block_size_context][palette_context],
                );
                if let Some(palette) = palette.as_ref().filter(|palette| palette.has_y()) {
                    self.sym.encode_symbol(
                        palette.colors.len() - 2,
                        &cdf::PALETTE_Y_SIZE[block_size_context],
                    );
                    let cache = self.palette_cache(r, c);
                    self.signal_palette_colors(&palette.colors, &cache);
                }
            }
            if chroma_reference && uv_mode == DC_PRED {
                // has_palette_uv is signalled for every DC chroma block, even
                // when the luma mode is directional.  Directional UV blocks
                // do not enter palette_mode_info at all.
                self.sym.encode_symbol(
                    usize::from(has_uv_palette),
                    &cdf::PALETTE_UV_MODE[usize::from(has_luma_palette)],
                );
                if let Some(palette) = palette.as_ref().filter(|palette| palette.has_uv()) {
                    let colors = palette.uv_colors.as_ref().expect("UV palette colors");
                    self.sym.encode_symbol(
                        colors.len() / 2 - 2,
                        &cdf::PALETTE_UV_SIZE[block_size_context],
                    );
                    let cache = self.uv_palette_cache(r, c);
                    self.signal_palette_uv_colors(colors, &cache);
                }
            }
            if y_mode == DC_PRED
                && let Some(palette) = palette.as_ref().filter(|palette| palette.has_y())
            {
                // Palette color-map tokens follow the complete
                // palette_mode_info syntax, including has_palette_uv.
                self.signal_palette_tokens(&palette.index_map, bw, palette.colors.len(), false);
            }
            if let Some(palette) = palette.as_ref().filter(|palette| palette.has_uv())
                && uv_mode == DC_PRED
            {
                let colors = palette.uv_colors.as_ref().expect("UV palette colors");
                let index_map = palette.uv_index_map.as_ref().expect("UV palette color map");
                self.signal_palette_tokens(index_map, bw, colors.len() / 2, true);
            }
        }

        for y in 0..n4_y {
            for x in 0..n4_x {
                let (rr, cc) = (r + y, c + x);
                if rr < self.mi_rows && cc < self.mi_cols {
                    self.mi_bsl[rr * self.mi_cols + cc] = bsl;
                    self.mi_skip[rr * self.mi_cols + cc] = skip;
                    self.mi_ymode[rr * self.mi_cols + cc] = y_mode;
                    self.mi_palette_size[rr * self.mi_cols + cc] = palette
                        .as_ref()
                        .filter(|value| value.has_y())
                        .map_or(0, |value| value.colors.len() as u8);
                    let colors = &mut self.mi_palette_colors[rr * self.mi_cols + cc];
                    colors.fill(0);
                    if let Some(palette) = palette.as_ref().filter(|palette| palette.has_y()) {
                        colors[..palette.colors.len()].copy_from_slice(&palette.colors);
                    }
                    self.mi_uv_palette_size[rr * self.mi_cols + cc] = palette
                        .as_ref()
                        .filter(|value| value.has_uv())
                        .map_or(0, |value| {
                            value.uv_colors.as_ref().expect("UV palette colors").len() as u8 / 2
                        });
                    let uv_colors = &mut self.mi_uv_palette_colors[rr * self.mi_cols + cc];
                    uv_colors.fill(0);
                    if let Some(palette) = palette.as_ref().filter(|palette| palette.has_uv()) {
                        let colors = palette.uv_colors.as_ref().expect("UV palette colors");
                        uv_colors[..colors.len() / 2].copy_from_slice(&colors[..colors.len() / 2]);
                    }
                }
            }
        }

        if skip {
            self.mark_block_decoded_shape(r, c, bw, bh);
            self.clear_plane_contexts_shape(r, c, bw, bh);
            return;
        }

        // residual(): decoder order is luma, then U/V only for chroma-reference blocks.
        if has_luma_palette {
            self.encode_palette_plane_unit(0, r, c, bw, bh);
        } else {
            self.encode_plane_unit(0, r, c, bw, bh, y_mode, y_angle_delta, None);
        }
        if chroma_reference {
            if has_uv_palette {
                self.encode_palette_plane_unit(1, r, c, bw, bh);
                self.encode_palette_plane_unit(2, r, c, bw, bh);
            } else {
                if let Some((alpha_u, alpha_v)) = cfl {
                    self.encode_plane_unit(1, r, c, bw, bh, DC_PRED, uv_angle_delta, Some(alpha_u));
                    self.encode_plane_unit(2, r, c, bw, bh, DC_PRED, uv_angle_delta, Some(alpha_v));
                } else {
                    self.encode_plane_unit(1, r, c, bw, bh, uv_mode, uv_angle_delta, None);
                    self.encode_plane_unit(2, r, c, bw, bh, uv_mode, uv_angle_delta, None);
                }
            }
        }
    }

    fn is_chroma_reference_shape(&self, r: usize, c: usize, bw: usize, bh: usize) -> bool {
        let mi_col = c;
        let mi_row = r;
        let block_mi_width = bw / 4;
        let block_mi_height = bh / 4;
        (!mi_row.is_multiple_of(2) || block_mi_height.is_multiple_of(2) || self.subsampling_y == 0)
            && (!mi_col.is_multiple_of(2)
                || block_mi_width.is_multiple_of(2)
                || self.subsampling_x == 0)
    }

    fn skip_ctx(&self, r: usize, c: usize) -> usize {
        let above = r > 0 && self.mi_skip[(r - 1) * self.mi_cols + c];
        let left = c > 0 && self.mi_skip[r * self.mi_cols + (c - 1)];
        usize::from(above) + usize::from(left)
    }

    fn block_is_skippable(&mut self, r: usize, c: usize, bw: usize, y_mode: u8) -> bool {
        self.block_is_skippable_with_modes(r, c, bw, y_mode, DC_PRED)
    }

    fn block_is_skippable_with_modes(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        y_mode: u8,
        uv_mode: u8,
    ) -> bool {
        self.block_is_skippable_with_modes_shape(r, c, bw, bw, y_mode, uv_mode)
    }

    fn block_is_skippable_with_modes_shape(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        y_mode: u8,
        uv_mode: u8,
    ) -> bool {
        if bw == 4 && bh == 4 {
            for plane in 0..self.planes.len() {
                let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
                let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
                let px = (c * 4) >> shift_x;
                let py = (r * 4) >> shift_y;
                if px >= self.plane_coded_widths[plane] || py >= self.plane_coded_heights[plane] {
                    continue;
                }
                let mode = if plane == 0 { y_mode } else { uv_mode };
                let predictions = self.predict_block_4x4(plane, px, py, mode, 0);
                for y in 0..4 {
                    for x in 0..4 {
                        if self.sample(plane, px + x, py + y) != predictions[y * 4 + x] {
                            return false;
                        }
                    }
                }
            }
            return true;
        }
        // Probe against a private reconstruction-availability region. Decoder
        // edge extension advances after every 4x4 transform, including the
        // transforms earlier in this same block; checking the whole block
        // against the caller's map would therefore accept unsafe non-DC skip.
        let snapshot = self.snapshot_reconstruction_context(r, c, bw, bh);
        let mut skippable = true;
        'planes: for plane in 0..self.planes.len() {
            let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
            let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
            let sx = (c * 4) >> shift_x;
            let sy = (r * 4) >> shift_y;
            let unit_w = (bw >> shift_x).max(4);
            let unit_h = (bh >> shift_y).max(4);
            for ty in 0..unit_h.div_ceil(4) {
                for tx in 0..unit_w.div_ceil(4) {
                    let px = sx + tx * 4;
                    let py = sy + ty * 4;
                    if px >= self.plane_coded_widths[plane] || py >= self.plane_coded_heights[plane]
                    {
                        continue;
                    }
                    let predictions = self.predict_block_4x4(
                        plane,
                        px,
                        py,
                        if plane == 0 { y_mode } else { uv_mode },
                        0,
                    );
                    for y in 0..4 {
                        for x in 0..4 {
                            if self.sample(plane, px + x, py + y) != predictions[y * 4 + x] {
                                skippable = false;
                                break 'planes;
                            }
                        }
                    }
                    self.reconstruct_source_block(plane, px, py);
                    self.mark_block_decoded_cell(plane, py >> 2, px >> 2);
                }
            }
        }
        self.restore_reconstruction_context(snapshot);
        skippable
    }

    fn exact_skip_mode(&mut self, r: usize, c: usize, bw: usize) -> Option<u8> {
        self.block_is_skippable(r, c, bw, DC_PRED)
            .then_some(DC_PRED)
    }

    fn should_split_square_local(&mut self, r: usize, c: usize, bw: usize) -> bool {
        let none_rate = self.measure_partition_local(r, c, bw, 0, 0, false);
        let split_rate = self.measure_partition_local(r, c, bw, 3, 0, false);
        split_rate < none_rate
    }

    fn snapshot_block_context(&self, r: usize, c: usize, bw: usize) -> BlockContextSnapshot {
        let n4 = bw / 4;
        let reconstructions = self.snapshot_reconstruction_context(r, c, bw, bw);
        let planes = reconstructions
            .into_iter()
            .enumerate()
            .map(|(plane, reconstruction)| {
                let region = self.plane_block_region(plane, r, c, bw, bw);
                let above_start = region.x >> 2;
                let above_end = (above_start + region.width4).min(self.above_level[plane].len());
                let left_start = region.y >> 2;
                let left_end = (left_start + region.height4).min(self.left_level[plane].len());
                PlaneBlockContextSnapshot {
                    above_start,
                    above_level: self.above_level[plane][above_start..above_end].to_vec(),
                    above_dc: self.above_dc[plane][above_start..above_end].to_vec(),
                    left_start,
                    left_level: self.left_level[plane][left_start..left_end].to_vec(),
                    left_dc: self.left_dc[plane][left_start..left_end].to_vec(),
                    reconstruction,
                }
            })
            .collect();
        let above_partition_start = c.min(self.mi_cols);
        let above_partition_end = (c + n4).min(self.mi_cols);
        let left_partition_start = r.min(self.mi_rows);
        let left_partition_end = (r + n4).min(self.mi_rows);
        let mut mi = Vec::with_capacity(n4 * n4);
        for y in 0..n4 {
            for x in 0..n4 {
                let (rr, cc) = (r + y, c + x);
                if rr < self.mi_rows && cc < self.mi_cols {
                    let index = rr * self.mi_cols + cc;
                    mi.push((
                        index,
                        self.mi_bsl[index],
                        self.mi_skip[index],
                        self.mi_ymode[index],
                        self.mi_palette_size[index],
                        self.mi_palette_colors[index],
                        self.mi_uv_palette_size[index],
                        self.mi_uv_palette_colors[index],
                    ));
                }
            }
        }
        BlockContextSnapshot {
            planes,
            above_partition_start,
            above_partition: self.above_partition[above_partition_start..above_partition_end]
                .to_vec(),
            left_partition_start,
            left_partition: self.left_partition[left_partition_start..left_partition_end].to_vec(),
            mi,
        }
    }

    fn restore_block_context(&mut self, snapshot: BlockContextSnapshot) {
        for (plane, saved) in snapshot.planes.into_iter().enumerate() {
            let above_end = saved.above_start + saved.above_level.len();
            self.above_level[plane][saved.above_start..above_end]
                .copy_from_slice(&saved.above_level);
            self.above_dc[plane][saved.above_start..above_end].copy_from_slice(&saved.above_dc);
            let left_end = saved.left_start + saved.left_level.len();
            self.left_level[plane][saved.left_start..left_end].copy_from_slice(&saved.left_level);
            self.left_dc[plane][saved.left_start..left_end].copy_from_slice(&saved.left_dc);
            self.restore_plane_reconstruction(plane, saved.reconstruction);
        }
        let above_partition_end = snapshot.above_partition_start + snapshot.above_partition.len();
        self.above_partition[snapshot.above_partition_start..above_partition_end]
            .copy_from_slice(&snapshot.above_partition);
        let left_partition_end = snapshot.left_partition_start + snapshot.left_partition.len();
        self.left_partition[snapshot.left_partition_start..left_partition_end]
            .copy_from_slice(&snapshot.left_partition);
        for (index, bsl, skip, ymode, palette_size, colors, uv_palette_size, uv_colors) in
            snapshot.mi
        {
            self.mi_bsl[index] = bsl;
            self.mi_skip[index] = skip;
            self.mi_ymode[index] = ymode;
            self.mi_palette_size[index] = palette_size;
            self.mi_palette_colors[index] = colors;
            self.mi_uv_palette_size[index] = uv_palette_size;
            self.mi_uv_palette_colors[index] = uv_colors;
        }
    }

    fn plane_block_region(
        &self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
    ) -> PlaneBlockRegion {
        let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
        let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
        let x = (c * 4) >> shift_x;
        let y = (r * 4) >> shift_y;
        let unit_width = (bw >> shift_x).max(4);
        let unit_height = (bh >> shift_y).max(4);
        let width = unit_width.min(self.plane_coded_widths[plane].saturating_sub(x));
        let height = unit_height.min(self.plane_coded_heights[plane].saturating_sub(y));
        PlaneBlockRegion {
            x,
            y,
            width,
            height,
            width4: width.div_ceil(4),
            height4: height.div_ceil(4),
        }
    }

    fn snapshot_reconstruction_context(
        &self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
    ) -> Vec<PlaneReconstructionSnapshot> {
        (0..self.planes.len())
            .map(|plane| {
                let region = self.plane_block_region(plane, r, c, bw, bh);
                let coded_width = self.plane_coded_widths[plane];
                let mut samples = Vec::with_capacity(region.width * region.height);
                for y in region.y..region.y + region.height {
                    let start = y * coded_width + region.x;
                    samples
                        .extend_from_slice(&self.reconstructed[plane][start..start + region.width]);
                }
                let mut block_decoded = Vec::with_capacity(region.width4 * region.height4);
                for y in 0..region.height4 {
                    for x in 0..region.width4 {
                        let cell_r = (region.y >> 2) + y;
                        let cell_c = (region.x >> 2) + x;
                        let by = cell_r as isize - self.sb_r as isize;
                        let bx = cell_c as isize - self.sb_c as isize;
                        if (0..16).contains(&by) && (0..16).contains(&bx) {
                            let index = (by + 1) as usize * BD_STRIDE + (bx + 1) as usize;
                            block_decoded.push((index, self.block_decoded[plane][index]));
                        }
                    }
                }
                PlaneReconstructionSnapshot {
                    x: region.x,
                    y: region.y,
                    width: region.width,
                    samples,
                    block_decoded,
                }
            })
            .collect()
    }

    fn restore_reconstruction_context(&mut self, snapshots: Vec<PlaneReconstructionSnapshot>) {
        for (plane, snapshot) in snapshots.into_iter().enumerate() {
            self.restore_plane_reconstruction(plane, snapshot);
        }
    }

    fn restore_plane_reconstruction(
        &mut self,
        plane: usize,
        snapshot: PlaneReconstructionSnapshot,
    ) {
        for (index, value) in snapshot.block_decoded {
            self.block_decoded[plane][index] = value;
        }
        let coded_width = self.plane_coded_widths[plane];
        if snapshot.width > 0 {
            for (row, samples) in snapshot.samples.chunks(snapshot.width).enumerate() {
                let start = (snapshot.y + row) * coded_width + snapshot.x;
                self.reconstructed[plane][start..start + snapshot.width].copy_from_slice(samples);
            }
        }
    }

    fn measure_block_modes_local(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        y_mode: u8,
        uv_mode: u8,
        palette: Option<PaletteBlock>,
    ) -> usize {
        self.measure_block_modes_local_with_angles(r, c, bw, y_mode, uv_mode, palette, 0, 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn measure_block_modes_local_with_angles(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        y_mode: u8,
        uv_mode: u8,
        palette: Option<PaletteBlock>,
        y_angle_delta: i8,
        uv_angle_delta: i8,
    ) -> usize {
        if bw == 4 {
            return self.measure_leaf_block_modes_local(r, c, y_mode, uv_mode, palette);
        }
        let snapshot = self.snapshot_block_context(r, c, bw);
        let cdfs = self.sym.cdfs.clone();
        let saved_rate = self.sym.rate.clone();
        let saved_sym = std::mem::replace(
            &mut self.sym,
            AdaptiveSymbolEncoder {
                inner: None,
                cdfs,
                rate: saved_rate.clone(),
            },
        );
        self.encode_block_with_mode_shape_cfl(
            r,
            c,
            bw,
            bw,
            palette,
            y_mode,
            uv_mode,
            y_angle_delta,
            uv_angle_delta,
            None,
        );
        let size = (self.sym.rate.rate_q10 - saved_rate.rate_q10) as usize;
        self.restore_block_context(snapshot);
        self.sym = saved_sym;
        size
    }

    fn measure_leaf_block_modes_local(
        &mut self,
        r: usize,
        c: usize,
        y_mode: u8,
        uv_mode: u8,
        palette: Option<PaletteBlock>,
    ) -> usize {
        let snapshot = self.snapshot_leaf_context(r, c);
        let cdfs = self.sym.cdfs.clone();
        let saved_rate = self.sym.rate.clone();
        let saved_sym = std::mem::replace(
            &mut self.sym,
            AdaptiveSymbolEncoder {
                inner: None,
                cdfs,
                rate: saved_rate.clone(),
            },
        );
        self.encode_block_with_mode(r, c, 4, palette, y_mode, uv_mode);
        let size = (self.sym.rate.rate_q10 - saved_rate.rate_q10) as usize;
        self.restore_leaf_context(snapshot, r, c);
        self.sym = saved_sym;
        size
    }

    fn snapshot_leaf_context(&self, r: usize, c: usize) -> LeafContextSnapshot {
        let planes = (0..self.planes.len())
            .map(|plane| {
                let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
                let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
                let sx = (c * 4) >> shift_x;
                let sy = (r * 4) >> shift_y;
                let above_index = sx >> 2;
                let left_index = sy >> 2;
                let cell_r = sy >> 2;
                let cell_c = sx >> 2;
                let by = cell_r as isize - self.sb_r as isize;
                let bx = cell_c as isize - self.sb_c as isize;
                let block_decoded_index = (by + 1) as usize * BD_STRIDE + (bx + 1) as usize;
                let mut reconstructed = [0; 16];
                let width = self.plane_coded_widths[plane];
                let height = self.plane_coded_heights[plane];
                for y in 0..4 {
                    for x in 0..4 {
                        if sx + x < width && sy + y < height {
                            reconstructed[y * 4 + x] =
                                self.reconstructed[plane][(sy + y) * width + sx + x];
                        }
                    }
                }
                LeafPlaneSnapshot {
                    above_index,
                    above_level: self.above_level[plane][above_index],
                    above_dc: self.above_dc[plane][above_index],
                    left_index,
                    left_level: self.left_level[plane][left_index],
                    left_dc: self.left_dc[plane][left_index],
                    block_decoded_index,
                    block_decoded: self.block_decoded[plane][block_decoded_index],
                    reconstructed_x: sx,
                    reconstructed_y: sy,
                    reconstructed,
                }
            })
            .collect();
        let index = r * self.mi_cols + c;
        LeafContextSnapshot {
            planes,
            mi: (r < self.mi_rows && c < self.mi_cols).then(|| {
                (
                    index,
                    self.mi_bsl[index],
                    self.mi_skip[index],
                    self.mi_ymode[index],
                    self.mi_palette_size[index],
                    self.mi_palette_colors[index],
                    self.mi_uv_palette_size[index],
                    self.mi_uv_palette_colors[index],
                )
            }),
        }
    }

    fn restore_leaf_context(&mut self, snapshot: LeafContextSnapshot, r: usize, c: usize) {
        for (plane, saved) in snapshot.planes.into_iter().enumerate() {
            self.above_level[plane][saved.above_index] = saved.above_level;
            self.above_dc[plane][saved.above_index] = saved.above_dc;
            self.left_level[plane][saved.left_index] = saved.left_level;
            self.left_dc[plane][saved.left_index] = saved.left_dc;
            self.block_decoded[plane][saved.block_decoded_index] = saved.block_decoded;
            let width = self.plane_coded_widths[plane];
            let height = self.plane_coded_heights[plane];
            for y in 0..4 {
                for x in 0..4 {
                    if saved.reconstructed_x + x < width && saved.reconstructed_y + y < height {
                        self.reconstructed[plane]
                            [(saved.reconstructed_y + y) * width + saved.reconstructed_x + x] =
                            saved.reconstructed[y * 4 + x];
                    }
                }
            }
        }
        if let Some((index, bsl, skip, ymode, palette_size, colors, uv_palette_size, uv_colors)) =
            snapshot.mi
        {
            debug_assert_eq!(index, r * self.mi_cols + c);
            self.mi_bsl[index] = bsl;
            self.mi_skip[index] = skip;
            self.mi_ymode[index] = ymode;
            self.mi_palette_size[index] = palette_size;
            self.mi_palette_colors[index] = colors;
            self.mi_uv_palette_size[index] = uv_palette_size;
            self.mi_uv_palette_colors[index] = uv_colors;
        }
    }

    fn measure_partition_local(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        partition: usize,
        child_rdo_depth: u8,
        force_dc: bool,
    ) -> usize {
        let snapshot = self.snapshot_block_context(r, c, bw);
        let cdfs = self.sym.cdfs.clone();
        let saved_rate = self.sym.rate.clone();
        let saved_sym = std::mem::replace(
            &mut self.sym,
            AdaptiveSymbolEncoder {
                inner: None,
                cdfs,
                rate: saved_rate.clone(),
            },
        );
        self.encode_partition_choice(r, c, bw, partition, child_rdo_depth, force_dc);
        let size = (self.sym.rate.rate_q10 - saved_rate.rate_q10) as usize;
        self.restore_block_context(snapshot);
        self.sym = saved_sym;
        size
    }

    fn select_y_mode_shape(&mut self, r: usize, c: usize, bw: usize, bh: usize) -> u8 {
        if bw != bh {
            return DC_PRED;
        }
        // Keep subsampled lossless planes on the proven DC path for now.  In
        // those modes the luma/chroma reference grid has additional
        // alignment rules; the directional search is enabled first for
        // full-resolution identity planes where the reference is direct.
        if self.subsampling_x != 0 || self.subsampling_y != 0 {
            return DC_PRED;
        }
        // Boundary references use the decoder-visible reconstructed coded
        // grid, including its edge extension. They are safe to search just
        // like interior blocks; restricting them to DC leaves avoidable
        // residuals on the final partial superblock row/column.
        let candidates = [
            DC_PRED,
            V_PRED,
            H_PRED,
            D45_PRED,
            D135_PRED,
            D113_PRED,
            D157_PRED,
            D67_PRED,
            SMOOTH_PRED,
            SMOOTH_V_PRED,
            SMOOTH_H_PRED,
            PAETH_PRED,
        ];
        // Probe the complete block syntax for every square size. In
        // particular, 4×4 leaves used to choose by coefficient magnitude
        // only, which could prefer a mode with a larger entropy cost.
        let candidate_modes = if bw == 4 {
            let mut ranked = candidates
                .iter()
                .copied()
                .filter(|&mode| mode != DC_PRED)
                .map(|mode| (self.mode_cost_plane(0, r, c, bw, mode), mode))
                .collect::<Vec<_>>();
            ranked.sort_unstable_by_key(|&(cost, _)| cost);
            ranked
                .into_iter()
                .take(5)
                .map(|(_, mode)| mode)
                .collect::<Vec<_>>()
        } else {
            candidates.to_vec()
        };
        let mut best_mode = DC_PRED;
        let initial_uv = if self.monochrome {
            DC_PRED
        } else {
            self.select_uv_mode_for_y(r, c, bw, DC_PRED)
        };
        let mut best_size = self.measure_block_modes_local(r, c, bw, DC_PRED, initial_uv, None);
        for mode in candidate_modes {
            let uv_mode = if self.monochrome {
                DC_PRED
            } else {
                self.select_uv_mode_for_y(r, c, bw, mode)
            };
            let size = self.measure_block_modes_local(r, c, bw, mode, uv_mode, None);
            if size < best_size {
                best_mode = mode;
                best_size = size;
            }
        }
        best_mode
    }

    fn select_uv_mode_for_y(&mut self, r: usize, c: usize, bw: usize, y_mode: u8) -> u8 {
        self.select_uv_mode_for_y_shape(r, c, bw, bw, y_mode)
    }

    fn select_uv_mode_for_y_shape(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        y_mode: u8,
    ) -> u8 {
        if bw != bh {
            return DC_PRED;
        }
        // The first UV RDO pass targets the full-resolution lossless path.
        // Subsampled chroma has different edge geometry and is kept on the
        // proven DC path until its reference construction is audited.
        if self.subsampling_x != 0 || self.subsampling_y != 0 {
            return DC_PRED;
        }
        if bw < 8 {
            let candidates = [
                DC_PRED,
                V_PRED,
                H_PRED,
                D45_PRED,
                D135_PRED,
                D113_PRED,
                D157_PRED,
                D67_PRED,
                SMOOTH_PRED,
                SMOOTH_V_PRED,
                SMOOTH_H_PRED,
                PAETH_PRED,
            ];
            return candidates
                .iter()
                .copied()
                .min_by_key(|&mode| self.mode_cost_plane(1, r, c, bw, mode))
                .unwrap_or(DC_PRED);
        }
        // Directional modes can read extended edges across the superblock
        // guard. Keep those modes out of the exact probe at a superblock
        // edge, but still rate the local-only predictors with the real AV1
        // syntax. The old coefficient-magnitude proxy could choose a UV mode
        // whose residual was slightly smaller but substantially more
        // expensive once EOB, coefficient contexts, and mode signaling were
        // included.
        if bw < 8 {
            let candidates = [
                DC_PRED,
                V_PRED,
                H_PRED,
                D45_PRED,
                D135_PRED,
                D113_PRED,
                D157_PRED,
                D67_PRED,
                SMOOTH_PRED,
                SMOOTH_V_PRED,
                SMOOTH_H_PRED,
                PAETH_PRED,
            ];
            return candidates
                .iter()
                .copied()
                .min_by_key(|&mode| self.mode_cost_plane(1, r, c, bw, mode))
                .unwrap_or(DC_PRED);
        }
        if r.is_multiple_of(16) || c.is_multiple_of(16) {
            let candidates = [
                DC_PRED,
                V_PRED,
                H_PRED,
                SMOOTH_PRED,
                SMOOTH_V_PRED,
                SMOOTH_H_PRED,
                PAETH_PRED,
            ];
            return candidates
                .iter()
                .copied()
                .min_by_key(|&mode| self.measure_block_modes_local(r, c, bw, y_mode, mode, None))
                .unwrap_or(DC_PRED);
        }
        let candidates = if bw < 16 {
            vec![
                DC_PRED,
                V_PRED,
                H_PRED,
                D45_PRED,
                D135_PRED,
                D113_PRED,
                D157_PRED,
                D67_PRED,
                SMOOTH_PRED,
                SMOOTH_V_PRED,
                SMOOTH_H_PRED,
                PAETH_PRED,
            ]
        } else {
            vec![
                DC_PRED,
                V_PRED,
                H_PRED,
                D45_PRED,
                D135_PRED,
                D113_PRED,
                D157_PRED,
                D67_PRED,
                SMOOTH_PRED,
                SMOOTH_V_PRED,
                SMOOTH_H_PRED,
            ]
        };
        candidates
            .iter()
            .copied()
            .min_by_key(|&mode| self.measure_block_modes_local(r, c, bw, y_mode, mode, None))
            .unwrap_or(DC_PRED)
    }

    fn select_angle_deltas(
        &mut self,
        r: usize,
        c: usize,
        bw: usize,
        y_mode: u8,
        uv_mode: u8,
    ) -> (i8, i8) {
        // AV1 signals angle_delta only for blocks at least 8x8.  Keeping the
        // guard here is important: a non-zero value would alter prediction
        // for a 4x4 transform without a corresponding syntax element.
        if bw < 8 {
            return (0, 0);
        }
        let y_angle_delta = if is_directional_mode(y_mode) {
            (-3i8..=3)
                .min_by_key(|&angle_delta| {
                    self.measure_block_modes_local_with_angles(
                        r,
                        c,
                        bw,
                        y_mode,
                        uv_mode,
                        None,
                        angle_delta,
                        0,
                    )
                })
                .unwrap_or(0)
        } else {
            0
        };
        let uv_angle_delta = if !self.monochrome && is_directional_mode(uv_mode) {
            (-3i8..=3)
                .min_by_key(|&angle_delta| {
                    self.measure_block_modes_local_with_angles(
                        r,
                        c,
                        bw,
                        y_mode,
                        uv_mode,
                        None,
                        y_angle_delta,
                        angle_delta,
                    )
                })
                .unwrap_or(0)
        } else {
            0
        };
        (y_angle_delta, uv_angle_delta)
    }

    fn mode_cost_plane(&self, plane: usize, r: usize, c: usize, bw: usize, mode: u8) -> u64 {
        self.mode_cost_plane_angle(plane, r, c, bw, mode, 0)
    }

    fn mode_cost_plane_angle(
        &self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        mode: u8,
        angle_delta: i8,
    ) -> u64 {
        let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
        let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
        let sx = (c * 4) >> shift_x;
        let sy = (r * 4) >> shift_y;
        let unit_w = (bw >> shift_x).max(4);
        let unit_h = (bw >> shift_y).max(4);
        let mut cost = 0u64;
        for ty in 0..unit_h.div_ceil(4) {
            for tx in 0..unit_w.div_ceil(4) {
                let px = sx + tx * 4;
                let py = sy + ty * 4;
                let predictions = self.predict_block_4x4(plane, px, py, mode, angle_delta);
                let mut residual = [0i32; 16];
                for y in 0..4 {
                    for x in 0..4 {
                        residual[y * 4 + x] =
                            self.sample(plane, px + x, py + y) - predictions[y * 4 + x];
                    }
                }
                cost += gamut_dsp::fwht4x4(&residual)
                    .into_iter()
                    .map(|coefficient| u64::from(coefficient.unsigned_abs()))
                    .sum::<u64>();
            }
        }
        cost
    }

    fn select_cfl_alpha(
        &self,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        normal_uv_mode: u8,
    ) -> Option<(i8, i8)> {
        if self.monochrome
            || self.planes.len() < 3
            || !self.is_chroma_reference_shape(r, c, bw, bh)
            || (bw >> self.subsampling_x).max(4) != 4
            || (bh >> self.subsampling_y).max(4) != 4
        {
            return None;
        }
        let normal_cost = self
            .mode_cost_plane(1, r, c, bw, normal_uv_mode)
            .saturating_add(self.mode_cost_plane(2, r, c, bw, normal_uv_mode));
        let sx = (c * 4) >> self.subsampling_x;
        let sy = (r * 4) >> self.subsampling_y;
        let luma_deltas = self.cfl_luma_deltas(sx, sy);
        let best_u = (-16i8..=16)
            .min_by_key(|&alpha| self.cfl_mode_cost(1, sx, sy, alpha, &luma_deltas))
            .unwrap_or(0);
        let best_v = (-16i8..=16)
            .min_by_key(|&alpha| self.cfl_mode_cost(2, sx, sy, alpha, &luma_deltas))
            .unwrap_or(0);
        if best_u == 0 && best_v == 0 {
            return None;
        }
        let cfl_cost = self
            .cfl_mode_cost(1, sx, sy, best_u, &luma_deltas)
            .saturating_add(self.cfl_mode_cost(2, sx, sy, best_v, &luma_deltas));
        (cfl_cost < normal_cost).then_some((best_u, best_v))
    }

    fn cfl_mode_cost(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        alpha_q3: i8,
        luma_deltas: &[i32; 16],
    ) -> u64 {
        let mut residual = [0i32; 16];
        for y in 0..4 {
            for x in 0..4 {
                let index = y * 4 + x;
                residual[index] = self.sample(plane, sx + x, sy + y)
                    - self.predict_cfl_sample(plane, sx + x, sy + y, alpha_q3, luma_deltas);
            }
        }
        gamut_dsp::fwht4x4(&residual)
            .into_iter()
            .map(|coefficient| u64::from(coefficient.unsigned_abs()))
            .sum()
    }

    fn encode_cfl_params(&mut self, alpha_u_q3: i8, alpha_v_q3: i8) {
        let sign_u = cfl_alpha_sign(alpha_u_q3);
        let sign_v = cfl_alpha_sign(alpha_v_q3);
        let joint_sign = (0..8)
            .find(|&joint| cfl_joint_signs(joint) == (sign_u, sign_v))
            .expect("CFL alpha signs must have a joint-sign representation");
        self.sym.encode_symbol(joint_sign, &cdf::CFL_SIGN);
        if sign_u != 0 {
            let context = joint_sign.saturating_sub(2);
            self.sym.encode_symbol(
                usize::from(alpha_u_q3.unsigned_abs()).saturating_sub(1),
                &cdf::CFL_ALPHA[context],
            );
        }
        if sign_v != 0 {
            let context = {
                let (sign_u, sign_v) = cfl_joint_signs(joint_sign);
                sign_v * 3 + sign_u - 3
            };
            self.sym.encode_symbol(
                usize::from(alpha_v_q3.unsigned_abs()).saturating_sub(1),
                &cdf::CFL_ALPHA[context],
            );
        }
    }

    fn cfl_luma_deltas(&self, block_sx: usize, block_sy: usize) -> [i32; 16] {
        let sample_count = 1usize << (self.subsampling_x + self.subsampling_y);
        let luma_x = block_sx << self.subsampling_x;
        let luma_y = block_sy << self.subsampling_y;
        let luma_q3_at = |row: usize, col: usize| {
            let source_x = (luma_x + (col << self.subsampling_x)).min(self.plane_widths[0] - 1);
            let source_y = (luma_y + (row << self.subsampling_y)).min(self.plane_heights[0] - 1);
            let mut sum = 0u32;
            for luma_row in 0..(1usize << self.subsampling_y) {
                for luma_col in 0..(1usize << self.subsampling_x) {
                    sum += self.reconstructed_sample(
                        0,
                        (source_x + luma_col).min(self.plane_widths[0] - 1),
                        (source_y + luma_row).min(self.plane_heights[0] - 1),
                    ) as u32;
                }
            }
            ((sum + (sample_count as u32 / 2)) / sample_count as u32) as i32 * 8
        };
        let average_q3 = ((0..4)
            .flat_map(|row| (0..4).map(move |col| luma_q3_at(row, col)))
            .sum::<i32>()
            + 8)
        .div_euclid(16);
        let mut deltas = [0i32; 16];
        for row in 0..4 {
            for col in 0..4 {
                deltas[row * 4 + col] = luma_q3_at(row, col) - average_q3;
            }
        }
        deltas
    }

    fn predict_cfl_sample(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        alpha_q3: i8,
        luma_deltas: &[i32; 16],
    ) -> i32 {
        debug_assert!(plane > 0);
        let block_sx = sx & !3;
        let block_sy = sy & !3;
        let base = self.dc_avg(plane, block_sx, block_sy);
        let value_sum = luma_deltas[(sy - block_sy) * 4 + sx - block_sx];
        let scaled = round_power_of_two_signed(i32::from(alpha_q3) * value_sum, 6);
        (base + scaled).clamp(0, (1i32 << self.bit_depth) - 1)
    }

    /// Selects compact Y and/or UV palette paths for a full-resolution block.
    /// UV palettes are initially restricted to unique U values so the
    /// decoder's cached-color merge has an unambiguous pair ordering.
    fn palette_block(&self, r: usize, c: usize, bw: usize) -> Option<PaletteBlock> {
        if bw < 8
            || !self.screen_content_tools
            || (!self.monochrome && (self.subsampling_x != 0 || self.subsampling_y != 0))
        {
            return None;
        }
        let sx = c * 4;
        let sy = r * 4;
        let mut colors = Vec::with_capacity(8);
        for y in 0..bw {
            for x in 0..bw {
                let value = self.sample(0, sx + x, sy + y) as u16;
                if colors.contains(&value) {
                    continue;
                }
                colors.push(value);
                if colors.len() > 8 {
                    colors.clear();
                    break;
                }
            }
            if colors.is_empty() {
                break;
            }
        }
        let y_palette = if (2..=8).contains(&colors.len()) {
            colors.sort_unstable();
            let mut index_map = Vec::with_capacity(bw * bw);
            for y in 0..bw {
                for x in 0..bw {
                    let value = self.sample(0, sx + x, sy + y) as u16;
                    let index = colors
                        .binary_search(&value)
                        .expect("palette source sample must be present");
                    index_map.push(index as u8);
                }
            }
            Some((colors, index_map))
        } else {
            None
        };

        let uv_palette = if !self.monochrome && self.subsampling_x == 0 && self.subsampling_y == 0 {
            let mut pairs = Vec::with_capacity(8);
            for y in 0..bw {
                for x in 0..bw {
                    let pair = (
                        self.sample(1, sx + x, sy + y) as u16,
                        self.sample(2, sx + x, sy + y) as u16,
                    );
                    if !pairs.contains(&pair) {
                        pairs.push(pair);
                        if pairs.len() > 8 {
                            pairs.clear();
                            break;
                        }
                    }
                }
                if pairs.is_empty() {
                    break;
                }
            }
            if (2..=8).contains(&pairs.len())
                && pairs
                    .iter()
                    .all(|(u, _)| pairs.iter().filter(|(other, _)| other == u).count() == 1)
            {
                pairs.sort_unstable();
                let mut colors = pairs.iter().map(|(u, _)| *u).collect::<Vec<_>>();
                colors.extend(pairs.iter().map(|(_, v)| *v));
                let mut index_map = Vec::with_capacity(bw * bw);
                for y in 0..bw {
                    for x in 0..bw {
                        let pair = (
                            self.sample(1, sx + x, sy + y) as u16,
                            self.sample(2, sx + x, sy + y) as u16,
                        );
                        let index = pairs
                            .binary_search(&pair)
                            .expect("UV palette source sample must be present");
                        index_map.push(index as u8);
                    }
                }
                Some((colors, index_map))
            } else {
                None
            }
        } else {
            None
        };

        if y_palette.is_none() && uv_palette.is_none() {
            return None;
        }
        let (colors, index_map) = y_palette.unwrap_or_default();
        Some(PaletteBlock {
            colors,
            index_map,
            uv_colors: uv_palette.as_ref().map(|(colors, _)| colors.clone()),
            uv_index_map: uv_palette.map(|(_, index_map)| index_map),
        })
    }

    fn palette_context(&self, r: usize, c: usize) -> usize {
        let above = r > 0 && self.mi_palette_size[(r - 1) * self.mi_cols + c] != 0;
        let left = c > 0 && self.mi_palette_size[r * self.mi_cols + (c - 1)] != 0;
        usize::from(above) + usize::from(left)
    }

    fn palette_cache(&self, r: usize, c: usize) -> Vec<u16> {
        let above_n = if r.is_multiple_of(16) {
            0
        } else {
            usize::from(self.mi_palette_size[(r - 1) * self.mi_cols + c])
        };
        let left_n = if c == 0 {
            0
        } else {
            usize::from(self.mi_palette_size[r * self.mi_cols + (c - 1)])
        };
        let above = self.mi_palette_colors[(r.saturating_sub(1)) * self.mi_cols + c];
        let left = self.mi_palette_colors[r * self.mi_cols + c.saturating_sub(1)];
        let mut cache = Vec::new();
        for color in above[..above_n.min(8)]
            .iter()
            .chain(left[..left_n.min(8)].iter())
        {
            if !cache.contains(color) {
                cache.push(*color);
            }
        }
        cache.sort_unstable();
        cache
    }

    fn uv_palette_cache(&self, r: usize, c: usize) -> Vec<u16> {
        let above_n = if r.is_multiple_of(16) {
            0
        } else {
            usize::from(self.mi_uv_palette_size[(r - 1) * self.mi_cols + c])
        };
        let left_n = if c == 0 {
            0
        } else {
            usize::from(self.mi_uv_palette_size[r * self.mi_cols + (c - 1)])
        };
        let above = self.mi_uv_palette_colors[(r.saturating_sub(1)) * self.mi_cols + c];
        let left = self.mi_uv_palette_colors[r * self.mi_cols + c.saturating_sub(1)];
        let mut cache = Vec::new();
        for color in above[..above_n.min(8)]
            .iter()
            .chain(left[..left_n.min(8)].iter())
        {
            if !cache.contains(color) {
                cache.push(*color);
            }
        }
        cache.sort_unstable();
        cache
    }

    fn signal_palette_colors(&mut self, colors: &[u16], cache: &[u16]) {
        let mut cached = vec![false; cache.len()];
        let mut cached_count = 0;
        for (index, &color) in cache.iter().enumerate() {
            if cached_count == colors.len() {
                break;
            }
            let use_color = colors.contains(&color);
            self.sym.encode_literal(u32::from(use_color), 1);
            if use_color {
                cached[index] = true;
                cached_count += 1;
            }
        }
        let transmitted = colors
            .iter()
            .copied()
            .filter(|color| {
                !cache
                    .iter()
                    .enumerate()
                    .any(|(index, cached_color)| cached[index] && cached_color == color)
            })
            .collect::<Vec<_>>();
        if transmitted.is_empty() {
            return;
        }
        let bit_depth = u32::from(self.bit_depth);
        self.sym
            .encode_literal(u32::from(transmitted[0]), bit_depth);
        if transmitted.len() > 1 {
            // palette_num_extra_bits_y = 3, hence the initial palette bits
            // equal bit_depth; subsequent deltas use the shrinking range.
            self.sym.encode_literal(3, 2);
            let mut bits = bit_depth;
            let mut range = (1usize << usize::from(self.bit_depth))
                .saturating_sub(usize::from(transmitted[0]) + 1);
            let mut previous = transmitted[0];
            for &current in transmitted.iter().skip(1) {
                self.sym
                    .encode_literal(u32::from(current - previous - 1), bits);
                range = range.saturating_sub(usize::from(current - previous));
                bits = bits.min(palette_ceil_log2(range));
                previous = current;
            }
        }
    }

    fn signal_palette_tokens(
        &mut self,
        index_map: &[u8],
        bw: usize,
        palette_size: usize,
        uv: bool,
    ) {
        encode_ns(&mut self.sym, usize::from(index_map[0]), palette_size);
        for diagonal in 1..(2 * bw - 1) {
            let start = diagonal.min(bw - 1);
            let end = diagonal.saturating_sub(bw - 1);
            let mut col = start;
            loop {
                let row = diagonal - col;
                let (order, context) = palette_color_context(index_map, bw, row, col, palette_size);
                let actual = usize::from(index_map[row * bw + col]);
                let symbol = order[..palette_size]
                    .iter()
                    .position(|&value| value == actual)
                    .unwrap_or(0);
                let color_index_cdf = if uv {
                    &cdf::PALETTE_UV_COLOR_INDEX[palette_size - 2][context][..palette_size]
                } else {
                    &cdf::PALETTE_Y_COLOR_INDEX[palette_size - 2][context][..palette_size]
                };
                self.sym.encode_symbol(symbol, color_index_cdf);
                if col == end {
                    break;
                }
                col -= 1;
            }
        }
    }

    fn signal_palette_uv_colors(&mut self, colors: &[u16], cache: &[u16]) {
        let palette_size = colors.len() / 2;
        let u_colors = &colors[..palette_size];
        let v_colors = &colors[palette_size..];
        let mut cached = vec![false; cache.len()];
        let mut cached_count = 0;
        for (index, &color) in cache.iter().enumerate() {
            if cached_count == palette_size {
                break;
            }
            let use_color = u_colors.contains(&color);
            self.sym.encode_literal(u32::from(use_color), 1);
            if use_color {
                cached[index] = true;
                cached_count += 1;
            }
        }
        let transmitted = u_colors
            .iter()
            .copied()
            .filter(|color| {
                !cache
                    .iter()
                    .enumerate()
                    .any(|(index, cached_color)| cached[index] && cached_color == color)
            })
            .collect::<Vec<_>>();
        if !transmitted.is_empty() {
            let bit_depth = u32::from(self.bit_depth);
            self.sym
                .encode_literal(u32::from(transmitted[0]), bit_depth);
            if transmitted.len() > 1 {
                self.sym.encode_literal(3, 2);
                let mut bits = bit_depth;
                let mut range = (1usize << usize::from(self.bit_depth))
                    .saturating_sub(usize::from(transmitted[0]));
                let mut previous = transmitted[0];
                for &current in transmitted.iter().skip(1) {
                    let delta = current - previous;
                    self.sym.encode_literal(u32::from(delta), bits);
                    range = range.saturating_sub(usize::from(delta));
                    bits = bits.min(palette_ceil_log2(range));
                    previous = current;
                }
            }
        }

        let bit_depth = u32::from(self.bit_depth);
        let max_value = 1i32 << self.bit_depth;
        let mut deltas = Vec::with_capacity(palette_size.saturating_sub(1));
        let mut max_delta = 0usize;
        for pair in v_colors.windows(2) {
            let forward = (i32::from(pair[1]) - i32::from(pair[0])).rem_euclid(max_value);
            let signed = if forward > max_value / 2 {
                forward - max_value
            } else {
                forward
            };
            let magnitude = signed.unsigned_abs() as usize;
            max_delta = max_delta.max(magnitude);
            deltas.push(signed);
        }
        let mut extra_bits = 0u32;
        while extra_bits < 3 && max_delta >= (1usize << (bit_depth.saturating_sub(4) + extra_bits))
        {
            extra_bits += 1;
        }
        let delta_bits = bit_depth.saturating_sub(4) + extra_bits;
        let delta_cost = 1
            + 2
            + bit_depth
            + deltas
                .iter()
                .map(|delta| u32::from(delta.unsigned_abs() != 0) + delta_bits)
                .sum::<u32>();
        let raw_cost = palette_size as u32 * bit_depth;
        let use_delta =
            !deltas.is_empty() && max_delta < (1usize << delta_bits) && delta_cost < raw_cost;
        self.sym.encode_literal(u32::from(use_delta), 1);
        if use_delta {
            self.sym.encode_literal(extra_bits, 2);
            self.sym.encode_literal(u32::from(v_colors[0]), bit_depth);
            for delta in deltas {
                let magnitude = delta.unsigned_abs();
                self.sym.encode_literal(magnitude, delta_bits);
                if magnitude != 0 {
                    self.sym.encode_literal(u32::from(delta < 0), 1);
                }
            }
        } else {
            for &color in v_colors {
                self.sym.encode_literal(u32::from(color), bit_depth);
            }
        }
    }

    fn clear_plane_contexts_shape(&mut self, r: usize, c: usize, bw: usize, bh: usize) {
        for plane in 0..self.planes.len() {
            let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
            let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
            let sx = (c * 4) >> shift_x;
            let sy = (r * 4) >> shift_y;
            let unit_w = (bw >> shift_x).max(4);
            let unit_h = (bh >> shift_y).max(4);
            let n_x = unit_w.div_ceil(4);
            let n_y = unit_h.div_ceil(4);
            for x in 0..n_x {
                let index = (sx >> 2) + x;
                if index < self.above_level[plane].len() {
                    self.above_level[plane][index] = 0;
                    self.above_dc[plane][index] = 0;
                }
            }
            for y in 0..n_y {
                let index = (sy >> 2) + y;
                if index < self.left_level[plane].len() {
                    self.left_level[plane][index] = 0;
                    self.left_dc[plane][index] = 0;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_plane_unit(
        &mut self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
        mode: u8,
        angle_delta: i8,
        cfl_alpha_q3: Option<i8>,
    ) {
        let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
        let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
        let sx = (c * 4) >> shift_x;
        let sy = (r * 4) >> shift_y;
        let unit_w = (bw >> shift_x).max(4);
        let unit_h = (bh >> shift_y).max(4);
        let n_x = unit_w.div_ceil(4);
        let n_y = unit_h.div_ceil(4);
        for ty in 0..n_y {
            for tx in 0..n_x {
                let px = sx + tx * 4;
                let py = sy + ty * 4;
                if px >= self.plane_coded_widths[plane] || py >= self.plane_coded_heights[plane] {
                    continue;
                }
                self.transform_block(
                    plane,
                    px,
                    py,
                    unit_w,
                    unit_w * unit_h,
                    mode,
                    angle_delta,
                    sx,
                    sy,
                    cfl_alpha_q3,
                );
                self.mark_block_decoded_cell(plane, py >> 2, px >> 2);
            }
        }
    }

    fn encode_palette_plane_unit(
        &mut self,
        plane: usize,
        r: usize,
        c: usize,
        bw: usize,
        bh: usize,
    ) {
        let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
        let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
        let sx = (c * 4) >> shift_x;
        let sy = (r * 4) >> shift_y;
        let unit_w = (bw >> shift_x).max(4);
        let unit_h = (bh >> shift_y).max(4);
        let n_x = unit_w.div_ceil(4);
        let n_y = unit_h.div_ceil(4);
        for ty in 0..n_y {
            for tx in 0..n_x {
                let px = sx + tx * 4;
                let py = sy + ty * 4;
                if px >= self.plane_coded_widths[plane] || py >= self.plane_coded_heights[plane] {
                    continue;
                }
                self.code_coeffs(plane, px >> 2, py >> 2, unit_w, unit_w * unit_h, &[0; 16]);
                self.reconstruct_source_block(plane, px, py);
                self.mark_block_decoded_cell(plane, py >> 2, px >> 2);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_block(
        &mut self,
        plane: usize,
        sx: usize,
        sy: usize,
        block_width: usize,
        block_area: usize,
        mode: u8,
        angle_delta: i8,
        _block_sx: usize,
        _block_sy: usize,
        cfl_alpha_q3: Option<i8>,
    ) {
        let mut res = [0i32; 16];
        let cfl_luma_deltas = cfl_alpha_q3.map(|_| self.cfl_luma_deltas(sx, sy));
        let predictions = cfl_alpha_q3
            .is_none()
            .then(|| self.predict_block_4x4(plane, sx, sy, mode, angle_delta));
        for i in 0..4 {
            for j in 0..4 {
                let sample = self.sample(plane, sx + j, sy + i);
                let prediction = if let Some(alpha_q3) = cfl_alpha_q3 {
                    self.predict_cfl_sample(
                        plane,
                        sx + j,
                        sy + i,
                        alpha_q3,
                        cfl_luma_deltas.as_ref().expect("CFL luma deltas"),
                    )
                } else {
                    predictions.as_ref().expect("intra predictions")[i * 4 + j]
                };
                res[i * 4 + j] = sample - prediction;
            }
        }
        let quant = gamut_dsp::fwht4x4(&res);
        self.code_coeffs(plane, sx >> 2, sy >> 2, block_width, block_area, &quant);
        self.reconstruct_source_block(plane, sx, sy);
    }

    /// Builds one lossless 4×4 intra predictor while sharing its edge
    /// references across all 16 samples.
    fn predict_block_4x4(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        mode: u8,
        angle_delta: i8,
    ) -> [i32; 16] {
        let block_sx = sx & !3;
        let block_sy = sy & !3;
        if mode == DC_PRED {
            return [self.dc_avg(plane, block_sx, block_sy); 16];
        }
        let above = self.reference_above(plane, block_sx, block_sy);
        let left = self.reference_left(plane, block_sx, block_sy);
        let top_left = self.reference_top_left(plane, block_sx, block_sy);
        let top_right = above[3];
        let bottom_left = left[3];
        let mut predictions = [0; 16];
        for y in 0..4 {
            for x in 0..4 {
                let above_sample = above[x];
                let left_sample = left[y];
                predictions[y * 4 + x] = match mode {
                    V_PRED => above_sample,
                    H_PRED => left_sample,
                    D45_PRED..=D67_PRED => self.predict_directional_sample_from_edges(
                        &above,
                        &left,
                        top_left,
                        x,
                        y,
                        mode,
                        angle_delta,
                    ),
                    SMOOTH_V_PRED => {
                        let wy = smooth_weight(4, y);
                        (wy * above_sample + (256 - wy) * bottom_left + 128) >> 8
                    }
                    SMOOTH_H_PRED => {
                        let wx = smooth_weight(4, x);
                        (wx * left_sample + (256 - wx) * top_right + 128) >> 8
                    }
                    SMOOTH_PRED => {
                        let wx = smooth_weight(4, x);
                        let wy = smooth_weight(4, y);
                        (wy * above_sample
                            + (256 - wy) * bottom_left
                            + wx * left_sample
                            + (256 - wx) * top_right
                            + 256)
                            >> 9
                    }
                    PAETH_PRED => {
                        let base = above_sample + left_sample - top_left;
                        let p_left = (base - left_sample).abs();
                        let p_top = (base - above_sample).abs();
                        let p_top_left = (base - top_left).abs();
                        if p_left <= p_top && p_left <= p_top_left {
                            left_sample
                        } else if p_top <= p_top_left {
                            above_sample
                        } else {
                            top_left
                        }
                    }
                    _ => unreachable!("lossless mode search only emits AV1 intra modes"),
                };
            }
        }
        predictions
    }

    #[allow(clippy::too_many_arguments)]
    fn predict_directional_sample_from_edges(
        &self,
        above: &[i32],
        left: &[i32],
        top_left: i32,
        x: usize,
        y: usize,
        mode: u8,
        angle_delta: i8,
    ) -> i32 {
        let i = y as i32;
        let j = x as i32;
        let angle =
            (cdf::MODE_TO_ANGLE[mode as usize] + i32::from(angle_delta) * 3).clamp(1, 269) as usize;
        let above_len = 4 + usize::from(angle < 90) * 4;
        let left_len = 4 + usize::from(angle > 180) * 4;
        let interpolate = |a: i32, b: i32, shift: i32| (a * (32 - shift) + b * shift + 16) >> 5;
        let above_at = |index: i32| {
            if index < 0 {
                top_left
            } else {
                above[(index as usize).min(above_len - 1)]
            }
        };
        let left_at = |index: i32| {
            if index < 0 {
                top_left
            } else {
                left[(index as usize).min(left_len - 1)]
            }
        };
        match angle {
            90 => above[x],
            180 => left[y],
            angle if angle < 90 => {
                let derivative = cdf::DR_INTRA_DERIVATIVE[angle];
                let index = (i + 1) * derivative;
                let base = (index >> 6) + j;
                let shift = (index >> 1) & 31;
                if base >= above_len as i32 - 1 {
                    above[above_len - 1]
                } else {
                    interpolate(above[base as usize], above[(base + 1) as usize], shift)
                }
            }
            angle if angle < 180 => {
                let dx = cdf::DR_INTRA_DERIVATIVE[180 - angle];
                let dy = cdf::DR_INTRA_DERIVATIVE[angle - 90];
                let index = (j << 6) - (i + 1) * dx;
                let base = index >> 6;
                if base >= -1 {
                    let shift = (index >> 1) & 31;
                    interpolate(above_at(base), above_at(base + 1), shift)
                } else {
                    let index = (i << 6) - (j + 1) * dy;
                    let base = index >> 6;
                    let shift = (index >> 1) & 31;
                    interpolate(left_at(base), left_at(base + 1), shift)
                }
            }
            _ => {
                let derivative = cdf::DR_INTRA_DERIVATIVE[270 - angle];
                let index = (j + 1) * derivative;
                let base = (index >> 6) + i;
                let shift = (index >> 1) & 31;
                if base >= left_len as i32 - 1 {
                    left[left_len - 1]
                } else {
                    interpolate(left[base as usize], left[(base + 1) as usize], shift)
                }
            }
        }
    }

    fn reference_above(&self, plane: usize, block_sx: usize, block_sy: usize) -> [i32; 9] {
        const N: usize = 4;
        let fallback = 1 << (self.bit_depth - 1);
        let by = block_sy / 4 - self.sb_r;
        let bx = block_sx / 4 - self.sb_c;
        let have_above_right = if block_sy / 4 == self.sb_r && block_sy > 0 {
            // The complete superblock row above has already been decoded,
            // including the neighbouring horizontal superblock.  The local
            // BlockDecoded guard cannot represent that cross-superblock edge.
            block_sx + N < self.plane_widths[plane]
        } else {
            self.block_decoded_at(plane, by as isize - 1, bx as isize + 1)
        };
        let mut reference = [0; 2 * N + 1];
        if block_sy > 0 {
            let available = if have_above_right { 2 * N } else { N };
            for (x, sample) in reference.iter_mut().enumerate() {
                *sample =
                    self.reconstructed_sample(plane, block_sx + x.min(available - 1), block_sy - 1);
            }
        } else if block_sx > 0 {
            reference.fill(self.reconstructed_sample(plane, block_sx - 1, block_sy));
        } else {
            reference.fill(fallback - 1);
        }
        reference
    }

    fn reference_left(&self, plane: usize, block_sx: usize, block_sy: usize) -> [i32; 9] {
        const N: usize = 4;
        let fallback = 1 << (self.bit_depth - 1);
        let by = block_sy / 4 - self.sb_r;
        let bx = block_sx / 4 - self.sb_c;
        let have_below_left = if block_sx / 4 == self.sb_c && block_sx > 0 {
            // The complete superblock column to the left has already been
            // decoded, including the neighbouring vertical superblock.
            block_sy + N < self.plane_heights[plane]
        } else {
            self.block_decoded_at(plane, by as isize + 1, bx as isize - 1)
        };
        let mut reference = [0; 2 * N + 1];
        if block_sx > 0 {
            let available = if have_below_left { 2 * N } else { N };
            for (y, sample) in reference.iter_mut().enumerate() {
                *sample =
                    self.reconstructed_sample(plane, block_sx - 1, block_sy + y.min(available - 1));
            }
        } else if block_sy > 0 {
            reference.fill(self.reconstructed_sample(plane, block_sx, block_sy - 1));
        } else {
            reference.fill(fallback + 1);
        }
        reference
    }

    fn reference_top_left(&self, plane: usize, block_sx: usize, block_sy: usize) -> i32 {
        let mid = 1 << (self.bit_depth - 1);
        if block_sx > 0 && block_sy > 0 {
            self.reconstructed_sample(plane, block_sx - 1, block_sy - 1)
        } else if block_sy > 0 {
            self.reconstructed_sample(plane, block_sx, block_sy - 1)
        } else if block_sx > 0 {
            self.reconstructed_sample(plane, block_sx - 1, block_sy)
        } else {
            mid
        }
    }

    /// DC intra prediction value for a 4×4 transform block (§7.11.2.5).
    fn dc_avg(&self, plane: usize, sx: usize, sy: usize) -> i32 {
        self.dc_pred(plane, sx, sy, 4, 4)
    }

    fn dc_pred(&self, plane: usize, sx: usize, sy: usize, width: usize, height: usize) -> i32 {
        let have_above = sy > 0;
        let have_left = sx > 0;
        match (have_above, have_left) {
            (true, true) => {
                let mut s = 0;
                for k in 0..width {
                    s += self.reconstructed_sample(plane, sx + k, sy - 1);
                }
                for k in 0..height {
                    s += self.reconstructed_sample(plane, sx - 1, sy + k);
                }
                (s + ((width + height) / 2) as i32) / (width + height) as i32
            }
            (false, true) => {
                let mut s = 0;
                for k in 0..height {
                    s += self.reconstructed_sample(plane, sx - 1, sy + k);
                }
                (s + (height / 2) as i32) / height as i32
            }
            (true, false) => {
                let mut s = 0;
                for k in 0..width {
                    s += self.reconstructed_sample(plane, sx + k, sy - 1);
                }
                (s + (width / 2) as i32) / width as i32
            }
            (false, false) => 1 << (self.bit_depth - 1),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn code_coeffs(
        &mut self,
        plane: usize,
        x4: usize,
        y4: usize,
        block_width: usize,
        block_area: usize,
        quant: &[i32; 16],
    ) {
        let ptype = usize::from(plane > 0);
        let scan = &cdf::DEFAULT_SCAN_4X4;

        let mut eob = 0usize;
        for c in 0..16 {
            if quant[scan[c]] != 0 {
                eob = c + 1;
            }
        }

        let txb_ctx = self.txb_skip_ctx(plane, x4, y4, block_width, block_area);
        self.sym
            .encode_symbol(usize::from(eob == 0), &cdf::TXB_SKIP[txb_ctx]);
        if eob == 0 {
            self.set_ctx(plane, x4, y4, 0, 0);
            return;
        }

        // eob position (TX_CLASS_2D ⇒ eob_pt context 0).
        let eobpt = eobpt_from_eob(eob);
        self.sym.encode_symbol(eobpt - 1, &cdf::EOB_PT_16[ptype][0]);
        if eobpt >= 3 {
            let nbits = eobpt - 2;
            let base = (1usize << (eobpt - 2)) + 1;
            let extra = eob - base;
            self.sym.encode_symbol(
                (extra >> (nbits - 1)) & 1,
                &cdf::EOB_EXTRA[ptype][eobpt - 3],
            );
            let mut i = nbits as isize - 2;
            while i >= 0 {
                self.sym.encode_literal(((extra >> i) & 1) as u32, 1);
                i -= 1;
            }
        }

        // Base levels + base range, scanned from the last coefficient back to DC.
        let mut levels = [0i32; 16];
        for c in (0..eob).rev() {
            let pos = scan[c];
            let level = quant[pos].abs();
            if c == eob - 1 {
                let ctx = coeff_base_eob_ctx(c);
                self.sym.encode_symbol(
                    (level.min(3) - 1) as usize,
                    &cdf::COEFF_BASE_EOB[ptype][ctx],
                );
            } else {
                let ctx = coeff_base_ctx(pos, &levels);
                self.sym
                    .encode_symbol(level.min(3) as usize, &cdf::COEFF_BASE[ptype][ctx]);
            }
            if level > NUM_BASE_LEVELS {
                let br_ctx = coeff_br_ctx(pos, &levels);
                let mut rem = level - 3;
                for _ in 0..4 {
                    let brv = rem.min(3);
                    self.sym
                        .encode_symbol(brv as usize, &cdf::COEFF_BR[ptype][br_ctx]);
                    rem -= brv;
                    if brv < 3 {
                        break;
                    }
                }
            }
            levels[pos] = level;
        }

        // Signs (DC sign is CDF-coded; the rest are raw bits) and golomb tails.
        for (c, &pos) in scan.iter().enumerate().take(eob) {
            let level = quant[pos].abs();
            if level != 0 {
                let neg = quant[pos] < 0;
                if c == 0 {
                    let ctx = self.dc_sign_ctx(plane, x4, y4);
                    self.sym
                        .encode_symbol(usize::from(neg), &cdf::DC_SIGN[ptype][ctx]);
                } else {
                    self.sym.encode_literal(u32::from(neg), 1);
                }
                if level > COEFF_BASE_PLUS_RANGE {
                    golomb(&mut self.sym, (level - COEFF_BASE_PLUS_RANGE) as u32);
                }
            }
        }

        // The decoder stores the coefficient entropy context as a three-bit
        // magnitude, not as the full coefficient sum.  Keep the encoder's
        // context state identical so txb_skip and DC-sign contexts select the
        // same CDFs on both sides of the bitstream.
        let cul = levels.iter().sum::<i32>().min(7) as u8;
        let dc_cat = if quant[0] == 0 {
            0
        } else if quant[0] < 0 {
            1
        } else {
            2
        };
        self.set_ctx(plane, x4, y4, cul, dc_cat);
    }

    fn set_ctx(&mut self, plane: usize, x4: usize, y4: usize, cul: u8, dc: u8) {
        self.above_level[plane][x4] = cul;
        self.above_dc[plane][x4] = dc;
        self.left_level[plane][y4] = cul;
        self.left_dc[plane][y4] = dc;
    }

    fn txb_skip_ctx(
        &self,
        plane: usize,
        x4: usize,
        y4: usize,
        block_width: usize,
        block_area: usize,
    ) -> usize {
        if plane == 0 {
            if block_width == 4 {
                return 0;
            }
            let top = i32::from(self.above_level[0][x4] & 7);
            let left = i32::from(self.left_level[0][y4] & 7);
            if top == 0 && left == 0 {
                1
            } else if top == 0 || left == 0 {
                2 + usize::from(top.max(left) > 3)
            } else if top.max(left) <= 3 {
                4
            } else if top.min(left) <= 3 {
                5
            } else {
                6
            }
        } else {
            let above = self.above_level[plane][x4] | self.above_dc[plane][x4];
            let left = self.left_level[plane][y4] | self.left_dc[plane][y4];
            let mut ctx = usize::from(above != 0) + usize::from(left != 0) + 7;
            if block_area > 16 {
                ctx += 3;
            }
            ctx
        }
    }

    fn dc_sign_ctx(&self, plane: usize, x4: usize, y4: usize) -> usize {
        let mut s = 0i32;
        for &cat in &[self.above_dc[plane][x4], self.left_dc[plane][y4]] {
            if cat == 1 {
                s -= 1;
            } else if cat == 2 {
                s += 1;
            }
        }
        if s < 0 {
            1
        } else if s > 0 {
            2
        } else {
            0
        }
    }
}

fn palette_block_size_context(bw: usize) -> usize {
    (2 * bw.ilog2() as usize).saturating_sub(6).min(6)
}

fn encode_ns(sym: &mut AdaptiveSymbolEncoder, value: usize, n: usize) {
    if n <= 1 {
        return;
    }
    let w = n.ilog2() + 1;
    let m = (1usize << w) - n;
    if value < m {
        sym.encode_literal(value as u32, w - 1);
    } else {
        let coded = value + m;
        sym.encode_literal((coded >> 1) as u32, w - 1);
        sym.encode_literal((coded & 1) as u32, 1);
    }
}

fn palette_ceil_log2(value: usize) -> u32 {
    if value <= 1 {
        0
    } else {
        usize::BITS - (value - 1).leading_zeros()
    }
}

fn palette_color_context(
    index_map: &[u8],
    bw: usize,
    row: usize,
    col: usize,
    palette_size: usize,
) -> ([usize; 8], usize) {
    let mut scores = [0i32; 8];
    if col > 0 {
        scores[usize::from(index_map[row * bw + col - 1])] += 2;
    }
    if row > 0 && col > 0 {
        scores[usize::from(index_map[(row - 1) * bw + col - 1])] += 1;
    }
    if row > 0 {
        scores[usize::from(index_map[(row - 1) * bw + col])] += 2;
    }
    let mut order = [0usize; 8];
    for (index, value) in order.iter_mut().enumerate() {
        *value = index;
    }
    for index in 0..3.min(palette_size) {
        let mut max_index = index;
        for candidate in index..palette_size {
            if scores[candidate] > scores[max_index] {
                max_index = candidate;
            }
        }
        if max_index != index {
            let score = scores[max_index];
            let value = order[max_index];
            for shift in (index + 1..=max_index).rev() {
                scores[shift] = scores[shift - 1];
                order[shift] = order[shift - 1];
            }
            scores[index] = score;
            order[index] = value;
        }
    }
    let hash = (scores[0] + scores[1] * 2 + scores[2] * 2) as usize;
    let context = usize::try_from(cdf::PALETTE_COLOR_CONTEXT[hash]).unwrap_or(0);
    (order, context)
}

fn intra_mode_context(mode: u8) -> usize {
    match mode {
        0 | 9 | 12 => 0,
        1 | 10 => 1,
        2 | 11 => 2,
        3 | 8 => 3,
        _ => 4,
    }
}

const fn is_directional_mode(mode: u8) -> bool {
    mode >= V_PRED && mode <= D67_PRED
}

fn smooth_weight(block_width: usize, index: usize) -> i32 {
    match block_width {
        4 => SMOOTH_WEIGHTS_4[index],
        8 => SMOOTH_WEIGHTS_8[index],
        16 => SMOOTH_WEIGHTS_16[index],
        32 => SMOOTH_WEIGHTS_32[index],
        64 => SMOOTH_WEIGHTS_64[index],
        _ => unreachable!("lossless blocks use AV1 square sizes"),
    }
}

fn angle_delta_cdf(mode: u8) -> &'static [u16; 7] {
    match mode {
        V_PRED => &cdf::ANGLE_DELTA_V,
        H_PRED => &cdf::ANGLE_DELTA_H,
        D45_PRED => &cdf::ANGLE_DELTA_D45,
        D135_PRED => &cdf::ANGLE_DELTA_D135,
        D113_PRED => &cdf::ANGLE_DELTA_D113,
        D157_PRED => &cdf::ANGLE_DELTA_D157,
        D203_PRED => &cdf::ANGLE_DELTA_D203,
        D67_PRED => &cdf::ANGLE_DELTA_D67,
        _ => unreachable!("angle delta is only signalled for directional modes"),
    }
}

fn angle_delta_symbol(delta: i8) -> usize {
    usize::try_from(delta.clamp(-3, 3) + 3).unwrap_or(3)
}

fn cfl_alpha_sign(alpha_q3: i8) -> usize {
    if alpha_q3 == 0 {
        0
    } else if alpha_q3 < 0 {
        1
    } else {
        2
    }
}

fn cfl_joint_signs(joint_sign: usize) -> (usize, usize) {
    let sign_u = ((joint_sign + 1) * 11) >> 5;
    let sign_v = joint_sign + 1 - 3 * sign_u;
    (sign_u, sign_v)
}

fn round_power_of_two_signed(value: i32, bits: u32) -> i32 {
    let rounding = 1i32 << (bits - 1);
    if value < 0 {
        -((-value + rounding) >> bits)
    } else {
        (value + rounding) >> bits
    }
}

fn intra_frame_y_mode_cdf(above: usize, left: usize) -> &'static [u16; 13] {
    if above == 0 && left == 0 {
        return &cdf::INTRA_FRAME_Y_MODE_DC_DC;
    }
    &cdf::INTRA_FRAME_Y_MODE[above][left]
}

fn uv_mode_cfl_not_allowed_cdf(y_mode: u8) -> &'static [u16; 13] {
    if y_mode == DC_PRED {
        return &cdf::UV_MODE_CFL_NOT_ALLOWED_DC;
    }
    &cdf::UV_MODE_CFL_NOT_ALLOWED[usize::from(y_mode.min(12))]
}

fn uv_mode_cfl_allowed_cdf(y_mode: u8) -> &'static [u16; 14] {
    if y_mode == DC_PRED {
        return &cdf::UV_MODE_CFL_ALLOWED_DC;
    }
    &cdf::UV_MODE_CFL_ALLOWED[usize::from(y_mode.min(12))]
}

/// Selects the partition CDF by `bsl` (`Mi_Width_Log2`); M0 never uses 128×128 superblocks.
fn partition_cdf(bsl: usize, ctx: usize) -> &'static [u16] {
    match bsl {
        1 => &cdf::PARTITION_W8[ctx],
        2 => &cdf::PARTITION_W16[ctx],
        3 => &cdf::PARTITION_W32[ctx],
        _ => &cdf::PARTITION_W64[ctx],
    }
}

fn partition_contexts_for_dimensions(width: usize, height: usize) -> (u8, u8) {
    (
        partition_context_coordinate(width),
        partition_context_coordinate(height),
    )
}

fn partition_context_coordinate(size: usize) -> u8 {
    match size {
        4 => 31,
        8 => 30,
        16 => 28,
        32 => 24,
        64 => 16,
        128 => 0,
        _ => unreachable!("lossless partition dimensions must be AV1 block sizes"),
    }
}

/// Derives the 2-symbol `split_or_horz` CDF from the partition CDF (§8.3.2): the vertical-ish
/// partition probabilities are folded into the "split" outcome.
fn split_or_horz_cdf(p: &[u16]) -> [u16; 2] {
    let psum = (p[2] - p[1])
        + (p[3] - p[2])
        + (p[4] - p[3])
        + (p[6] - p[5])
        + (p[7] - p[6])
        + (p[9] - p[8]);
    [32768 - psum, 32768]
}

/// Derives the 2-symbol `split_or_vert` CDF from the partition CDF (§8.3.2).
fn split_or_vert_cdf(p: &[u16]) -> [u16; 2] {
    let psum = (p[1] - p[0])
        + (p[3] - p[2])
        + (p[4] - p[3])
        + (p[5] - p[4])
        + (p[6] - p[5])
        + (p[8] - p[7]);
    [32768 - psum, 32768]
}

/// `eobPt` from `eob` (inverts `eob = (eobPt < 2) ? eobPt : (1 << (eobPt-2)) + 1`, §5.11.39).
fn eobpt_from_eob(eob: usize) -> usize {
    if eob <= 1 {
        eob
    } else {
        (32 - ((eob - 1) as u32).leading_zeros()) as usize + 1
    }
}

fn coeff_base_eob_ctx(c: usize) -> usize {
    if c == 0 {
        0
    } else if c <= 2 {
        1
    } else if c <= 4 {
        2
    } else {
        3
    }
}

fn coeff_base_ctx(pos: usize, levels: &[i32; 16]) -> usize {
    let (row, col) = (pos >> 2, pos & 3);
    let mut mag = 0i32;
    for &(dr, dc) in &cdf::SIG_REF_DIFF_OFFSET_2D {
        let (rr, cc) = (row + dr, col + dc);
        if rr < 4 && cc < 4 {
            mag += levels[(rr << 2) + cc].abs().min(3);
        }
    }
    let ctx = (((mag + 1) >> 1).min(4)) as usize;
    if row == 0 && col == 0 {
        return 0;
    }
    ctx + usize::from(cdf::COEFF_BASE_CTX_OFFSET_4X4[row.min(4)][col.min(4)])
}

fn coeff_br_ctx(pos: usize, levels: &[i32; 16]) -> usize {
    let (row, col) = (pos >> 2, pos & 3);
    let mut mag = 0i32;
    for &(dr, dc) in &cdf::MAG_REF_OFFSET_2D {
        let (rr, cc) = (row + dr, col + dc);
        if rr < 4 && cc < 4 {
            mag += levels[(rr << 2) + cc].abs().min(15);
        }
    }
    let mag = (((mag + 1) >> 1).min(6)) as usize;
    if pos == 0 {
        mag
    } else if row < 2 && col < 2 {
        mag + 7
    } else {
        mag + 14
    }
}

/// Exp-Golomb tail used for coefficient magnitudes above the base-range cap (§5.11.39).
fn golomb(sym: &mut AdaptiveSymbolEncoder, x: u32) {
    let len = 32 - x.leading_zeros(); // bit length, x >= 1
    for _ in 0..(len - 1) {
        sym.encode_literal(0, 1);
    }
    sym.encode_literal(1, 1);
    let mut i = len as isize - 2;
    while i >= 0 {
        sym.encode_literal((x >> i) & 1, 1);
        i -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lossless_types::LosslessPlanes;

    #[test]
    fn rate_estimator_assigns_fewer_bits_to_likely_symbols() {
        const SKEWED_CDF: [u16; 4] = [28_672, 30_720, 32_256, 32_768];
        let mut likely = RateEstimator::new();
        likely.encode_symbol(0, &SKEWED_CDF);
        let mut unlikely = RateEstimator::new();
        unlikely.encode_symbol(3, &SKEWED_CDF);
        assert!(likely.rate_q10 < unlikely.rate_q10);
    }

    #[test]
    fn smooth_horizontal_uses_left_edge_in_scan_order() {
        let mut samples = vec![100u16; 64];
        for (row, value) in [10, 20, 30, 40].into_iter().enumerate() {
            samples[(row + 4) * 8 + 3] = value;
        }
        let planes = LosslessPlanes {
            width: 8,
            height: 8,
            bit_depth: 8,
            monochrome: true,
            subsampling_x: 0,
            subsampling_y: 0,
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
            planes: vec![&samples],
        };
        let encoder = FrameEncoder::new(&planes, true);
        let predicted = encoder.predict_block_4x4(0, 4, 4, SMOOTH_H_PRED, 0)[0];
        assert_eq!(predicted, 10);
    }

    #[test]
    fn smooth_prediction_uses_the_same_left_edge_order() {
        let mut samples = vec![100u16; 64];
        for (row, value) in [10, 20, 30, 40].into_iter().enumerate() {
            samples[(row + 4) * 8 + 3] = value;
        }
        let planes = LosslessPlanes {
            width: 8,
            height: 8,
            bit_depth: 8,
            monochrome: true,
            subsampling_x: 0,
            subsampling_y: 0,
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
            planes: vec![&samples],
        };
        let encoder = FrameEncoder::new(&planes, true);
        let predicted = encoder.predict_block_4x4(0, 4, 4, SMOOTH_PRED, 0)[0];
        assert_eq!(predicted, 55);
    }

    #[test]
    fn transform_wide_predictions_match_reference_vectors() {
        let samples = (0..144)
            .map(|index| {
                let (row, col) = (index / 12, index % 12);
                (row * 17 + col * 11 + row * col * 3) as u16
            })
            .collect::<Vec<_>>();
        let planes = LosslessPlanes {
            width: 12,
            height: 12,
            bit_depth: 12,
            monochrome: true,
            subsampling_x: 0,
            subsampling_y: 0,
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 0,
            full_range: true,
            planes: vec![&samples],
        };
        let encoder = FrameEncoder::new(&planes, true);
        for (mode, angle, expected) in [
            (
                D45_PRED,
                -3,
                [
                    159, 179, 191, 191, 187, 191, 191, 191, 191, 191, 191, 191, 191, 191, 191, 191,
                ],
            ),
            (
                D135_PRED,
                2,
                [
                    116, 126, 146, 166, 142, 122, 121, 141, 168, 148, 126, 116, 194, 174, 152, 132,
                ],
            ),
            (
                SMOOTH_PRED,
                0,
                [
                    134, 155, 172, 184, 165, 176, 186, 193, 188, 192, 195, 199, 204, 202, 201, 203,
                ],
            ),
            (
                PAETH_PRED,
                0,
                [
                    137, 151, 171, 191, 163, 163, 171, 191, 189, 189, 189, 191, 215, 215, 215, 215,
                ],
            ),
        ] {
            assert_eq!(encoder.predict_block_4x4(0, 4, 4, mode, angle), expected);
        }
    }
}
