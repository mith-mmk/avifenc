//! The single-tile, all-intra, lossless encoder: superblock/partition iteration (§5.11.2/.4),
//! intra prediction (§7.11.2), the forward 4×4 WHT (via `gamut-dsp`), and coefficient coding
//! with full context derivation (§5.11.39, §8.3.2).
//!
//! All coded blocks are square intra blocks using lossless AV1 intra prediction;
//! full superblocks use `PARTITION_NONE` and the right/bottom frame edges use
//! the spec's forced splitting. The frame is coded on
//! the MI-unit grid (`mi_cols*4 × mi_rows*4`, i.e. dimensions rounded up to a multiple of 8); the
//! out-of-frame padding is edge-replicated and cropped away on decode.

use crate::lossless_cdf as cdf;
use crate::lossless_types::LosslessPlanes;
use gamut_bitstream::SymbolEncoder as RangeEncoder;
use std::collections::HashMap;

struct AdaptiveSymbolEncoder {
    inner: RangeEncoder,
    cdfs: HashMap<usize, (Vec<u16>, u16)>,
}

impl AdaptiveSymbolEncoder {
    fn new() -> Self {
        Self {
            inner: RangeEncoder::new(),
            cdfs: HashMap::new(),
        }
    }

    fn encode_symbol(&mut self, symbol: usize, cdf: &'static [u16]) {
        self.encode_symbol_with_key(symbol, cdf, cdf.as_ptr() as usize);
    }

    fn encode_symbol_with_key(&mut self, symbol: usize, cdf: &[u16], key: usize) {
        let state = self.cdfs.entry(key).or_insert_with(|| (cdf.to_vec(), 0));
        self.inner
            .encode_symbol_adapt(symbol, &mut state.0, &mut state.1);
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
        self.inner.encode_literal(value, bits);
    }

    fn finish(self) -> Vec<u8> {
        self.inner.finish()
    }
}

struct PaletteBlock {
    colors: [u16; 2],
    index_map: Vec<u8>,
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
    /// Palette size of the block covering each MI cell (zero for no palette).
    mi_palette_size: Vec<u8>,
    /// Sorted luma palette colours of the block covering each MI cell.
    mi_palette_colors: Vec<[u16; 2]>,
}

impl<'a> FrameEncoder<'a> {
    /// Creates an encoder over full-resolution planes (Y=G, U=B, V=R for identity CICP).
    pub(crate) fn new(planes: &'a LosslessPlanes<'a>) -> Self {
        let width = planes.width as usize;
        let height = planes.height as usize;
        let mi_cols = 2 * ((width + 7) >> 3);
        let mi_rows = 2 * ((height + 7) >> 3);
        let plane_count = planes.planes.len();
        Self {
            planes: planes.planes.clone(),
            bit_depth: planes.bit_depth,
            monochrome: planes.monochrome,
            subsampling_x: usize::from(planes.subsampling_x),
            subsampling_y: usize::from(planes.subsampling_y),
            plane_widths: planes
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
                .collect(),
            plane_heights: planes
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
                .collect(),
            plane_coded_widths: planes
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
                .collect(),
            plane_coded_heights: planes
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
                .collect(),
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
            mi_palette_size: vec![0; mi_cols * mi_rows],
            mi_palette_colors: vec![[0; 2]; mi_cols * mi_rows],
        }
    }

    /// Encodes the tile and returns the symbol-coded bytes (`decode_tile`, §5.11.2).
    pub(crate) fn encode(mut self) -> Vec<u8> {
        const SB4: usize = 16; // 64×64 superblock in MI units
        let mut r = 0;
        while r < self.mi_rows {
            for plane in 0..self.planes.len() {
                self.left_level[plane].iter_mut().for_each(|v| *v = 0);
                self.left_dc[plane].iter_mut().for_each(|v| *v = 0);
            }
            let mut c = 0;
            while c < self.mi_cols {
                self.encode_partition(r, c, 64);
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

    fn encode_partition(&mut self, r: usize, c: usize, bw: usize) {
        if r >= self.mi_rows || c >= self.mi_cols {
            return;
        }
        let num4x4 = bw / 4;
        let half = num4x4 >> 1;
        let has_rows = r + half < self.mi_rows;
        let has_cols = c + half < self.mi_cols;
        let bsl = num4x4.trailing_zeros() as usize; // Mi_Width_Log2

        let split = if bw < 8 {
            false // PARTITION_NONE forced, no symbol
        } else if has_rows && has_cols {
            let ctx = self.partition_ctx(r, c, bsl);
            self.sym.encode_symbol(0, partition_cdf(bsl, ctx)); // PARTITION_NONE
            false
        } else if has_cols {
            let ctx = self.partition_ctx(r, c, bsl);
            let partition = partition_cdf(bsl, ctx);
            let current = self.sym.current_cdf(partition);
            let cdf2 = split_or_horz_cdf(&current);
            self.sym
                .encode_symbol_with_key(1, &cdf2, split_cdf_key(bsl, ctx, false)); // split
            true
        } else if has_rows {
            let ctx = self.partition_ctx(r, c, bsl);
            let partition = partition_cdf(bsl, ctx);
            let current = self.sym.current_cdf(partition);
            let cdf2 = split_or_vert_cdf(&current);
            self.sym
                .encode_symbol_with_key(1, &cdf2, split_cdf_key(bsl, ctx, true)); // split
            true
        } else {
            true // forced PARTITION_SPLIT, no symbol
        };

        if !split {
            self.encode_block(r, c, bw);
        } else {
            let h = bw / 2;
            self.encode_partition(r, c, h);
            self.encode_partition(r, c + half, h);
            self.encode_partition(r + half, c, h);
            self.encode_partition(r + half, c + half, h);
        }
    }

    fn partition_ctx(&self, r: usize, c: usize, bsl: usize) -> usize {
        let above = r > 0 && usize::from(self.mi_bsl[(r - 1) * self.mi_cols + c]) < bsl;
        let left = c > 0 && usize::from(self.mi_bsl[r * self.mi_cols + (c - 1)]) < bsl;
        usize::from(left) * 2 + usize::from(above)
    }

    fn encode_block(&mut self, r: usize, c: usize, bw: usize) {
        let n4 = bw / 4;
        let bsl = n4.trailing_zeros() as u8;
        let palette = self.palette_block(r, c, bw);
        let y_mode = palette
            .as_ref()
            .map_or_else(|| self.select_y_mode(r, c, bw), |_| DC_PRED);
        let skip = palette.is_some() || self.block_is_skippable(r, c, bw, y_mode);

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
            self.sym.encode_symbol(3, angle_delta_cdf(y_mode));
        }
        let chroma_reference = !self.monochrome && self.is_chroma_reference(r, c, bw);
        if chroma_reference {
            // In coded-lossless mode CFL is allowed when the subsampled chroma
            // block is exactly 4x4.  This includes an 8x8 luma block in 4:2:0.
            let chroma_width = (bw >> self.subsampling_x).max(4);
            let chroma_height = (bw >> self.subsampling_y).max(4);
            let cfl_allowed = chroma_width == 4 && chroma_height == 4;
            if cfl_allowed {
                let uv = uv_mode_cfl_allowed_cdf(y_mode);
                self.sym.encode_symbol(0, uv);
            } else {
                let uv = uv_mode_cfl_not_allowed_cdf(y_mode);
                self.sym.encode_symbol(0, uv);
            }
        }

        if bw >= 8 && y_mode == DC_PRED {
            let block_size_context = palette_block_size_context(bw);
            let palette_context = self.palette_context(r, c);
            let has_palette = palette.is_some();
            self.sym.encode_symbol(
                usize::from(has_palette),
                &cdf::PALETTE_Y_MODE[block_size_context][palette_context],
            );
            if let Some(palette) = palette.as_ref() {
                self.sym
                    .encode_symbol(0, &cdf::PALETTE_Y_SIZE[block_size_context]);
                let cache = self.palette_cache(r, c);
                self.signal_palette_colors(palette.colors, &cache);
                self.signal_palette_tokens(&palette.index_map, bw);
            }
            if chroma_reference {
                // This encoder only selects luma palette mode. The chroma
                // palette flag is still present when screen tools are enabled.
                self.sym
                    .encode_symbol(0, &cdf::PALETTE_UV_MODE[usize::from(has_palette)]);
            }
        }

        for y in 0..n4 {
            for x in 0..n4 {
                let (rr, cc) = (r + y, c + x);
                if rr < self.mi_rows && cc < self.mi_cols {
                    self.mi_bsl[rr * self.mi_cols + cc] = bsl;
                    self.mi_skip[rr * self.mi_cols + cc] = skip;
                    self.mi_ymode[rr * self.mi_cols + cc] = y_mode;
                    self.mi_palette_size[rr * self.mi_cols + cc] = u8::from(palette.is_some()) * 2;
                    self.mi_palette_colors[rr * self.mi_cols + cc] =
                        palette.as_ref().map_or([0; 2], |value| value.colors);
                }
            }
        }

        if skip {
            self.clear_plane_contexts(r, c, bw);
            return;
        }

        // residual(): decoder order is luma, then U/V only for chroma-reference blocks.
        self.encode_plane_unit(0, r, c, bw, y_mode);
        if chroma_reference {
            self.encode_plane_unit(1, r, c, bw, DC_PRED);
            self.encode_plane_unit(2, r, c, bw, DC_PRED);
        }
    }

    fn is_chroma_reference(&self, r: usize, c: usize, bw: usize) -> bool {
        let mi_col = c;
        let mi_row = r;
        let block_mi_width = bw / 4;
        let block_mi_height = bw / 4;
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

    fn block_is_skippable(&self, r: usize, c: usize, bw: usize, y_mode: u8) -> bool {
        for plane in 0..self.planes.len() {
            let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
            let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
            let sx = (c * 4) >> shift_x;
            let sy = (r * 4) >> shift_y;
            let unit_w = (bw >> shift_x).max(4);
            let unit_h = (bw >> shift_y).max(4);
            for y in 0..unit_h {
                for x in 0..unit_w {
                    let prediction = self.predict_block_sample(
                        plane,
                        sx + x,
                        sy + y,
                        if plane == 0 { y_mode } else { DC_PRED },
                        sx,
                        sy,
                        bw,
                    );
                    if self.sample(plane, sx + x, sy + y) != prediction {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn select_y_mode(&self, r: usize, c: usize, bw: usize) -> u8 {
        // Keep subsampled lossless planes on the proven DC path for now.  In
        // those modes the luma/chroma reference grid has additional
        // alignment rules; the directional search is enabled first for
        // full-resolution identity planes where the reference is direct.
        if self.subsampling_x != 0 || self.subsampling_y != 0 {
            return DC_PRED;
        }
        // Directional references over the coded-grid padding come from the
        // decoder's reconstructed padding, not directly from the cropped
        // source. Keep boundary blocks on the already exact DC path until a
        // reconstruction buffer is available for those references.
        let block_sx = c * 4;
        let block_sy = r * 4;
        if block_sx + bw > self.plane_widths[0] || block_sy + bw > self.plane_heights[0] {
            return DC_PRED;
        }
        let mut best_mode = DC_PRED;
        let mut best_error = self.mode_error(r, c, bw, DC_PRED);
        let mut candidates = vec![V_PRED, H_PRED];
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
        if intra_mode_context(above_mode) == 0 && intra_mode_context(left_mode) == 0 {
            candidates.push(PAETH_PRED);
        }
        for mode in candidates {
            let error = self.mode_error(r, c, bw, mode);
            if error < best_error {
                best_mode = mode;
                best_error = error;
            }
        }
        best_mode
    }

    fn mode_error(&self, r: usize, c: usize, bw: usize, mode: u8) -> u64 {
        let sx = c * 4;
        let sy = r * 4;
        let mut error = 0u64;
        for y in 0..bw {
            for x in 0..bw {
                error += self
                    .sample(0, sx + x, sy + y)
                    .abs_diff(self.predict_block_sample(0, sx + x, sy + y, mode, sx, sy, bw))
                    as u64;
            }
        }
        error
    }

    /// Selects the deliberately narrow palette path: monochrome blocks with
    /// exactly two colours. The palette cache is handled separately when the
    /// block is signalled, which keeps bilevel documents compact.
    fn palette_block(&self, r: usize, c: usize, bw: usize) -> Option<PaletteBlock> {
        if !self.monochrome || bw < 8 {
            return None;
        }
        let mut colors = [None, None];
        for y in 0..bw {
            for x in 0..bw {
                let value = self.sample(0, c * 4 + x, r * 4 + y) as u16;
                if colors[0] == Some(value) || colors[1] == Some(value) {
                    continue;
                }
                if colors[0].is_none() {
                    colors[0] = Some(value);
                } else if colors[1].is_none() {
                    colors[1] = Some(value);
                } else {
                    return None;
                }
            }
        }
        let [Some(first), Some(second)] = colors else {
            return None;
        };
        let sorted = if first < second {
            [first, second]
        } else {
            [second, first]
        };
        let mut index_map = Vec::with_capacity(bw * bw);
        for y in 0..bw {
            for x in 0..bw {
                let value = self.sample(0, c * 4 + x, r * 4 + y) as u16;
                index_map.push(u8::from(value == sorted[1]));
            }
        }
        Some(PaletteBlock {
            colors: sorted,
            index_map,
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
        for color in above[..above_n.min(2)]
            .iter()
            .chain(left[..left_n.min(2)].iter())
        {
            if !cache.contains(color) {
                cache.push(*color);
            }
        }
        cache.sort_unstable();
        cache
    }

    fn signal_palette_colors(&mut self, colors: [u16; 2], cache: &[u16]) {
        let mut cached = [false; 4];
        let mut cached_count = 0;
        for (index, &color) in cache.iter().enumerate() {
            let use_color = colors.contains(&color);
            self.sym.encode_literal(u32::from(use_color), 1);
            if use_color {
                cached[index] = true;
                cached_count += 1;
            }
            if cached_count == 2 {
                break;
            }
        }
        let transmitted = colors
            .into_iter()
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
            // palette_num_extra_bits_y = 3, hence palette_bits == bit_depth.
            self.sym.encode_literal(3, 2);
            self.sym
                .encode_literal(u32::from(transmitted[1] - transmitted[0] - 1), bit_depth);
        }
    }

    fn signal_palette_tokens(&mut self, index_map: &[u8], bw: usize) {
        encode_ns(&mut self.sym, usize::from(index_map[0]), 2);
        for diagonal in 1..(2 * bw - 1) {
            let start = diagonal.min(bw - 1);
            let end = diagonal.saturating_sub(bw - 1);
            let mut col = start;
            loop {
                let row = diagonal - col;
                let (order, context) = palette_color_context(index_map, bw, row, col);
                let actual = usize::from(index_map[row * bw + col]);
                let symbol = order.iter().position(|&value| value == actual).unwrap_or(0);
                self.sym
                    .encode_symbol(symbol, &cdf::PALETTE_SIZE_2_Y_COLOR[context]);
                if col == end {
                    break;
                }
                col -= 1;
            }
        }
    }

    fn clear_plane_contexts(&mut self, r: usize, c: usize, bw: usize) {
        for plane in 0..self.planes.len() {
            let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
            let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
            let sx = (c * 4) >> shift_x;
            let sy = (r * 4) >> shift_y;
            let unit_w = (bw >> shift_x).max(4);
            let unit_h = (bw >> shift_y).max(4);
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

    fn encode_plane_unit(&mut self, plane: usize, r: usize, c: usize, bw: usize, mode: u8) {
        let shift_x = if plane == 0 { 0 } else { self.subsampling_x };
        let shift_y = if plane == 0 { 0 } else { self.subsampling_y };
        let sx = (c * 4) >> shift_x;
        let sy = (r * 4) >> shift_y;
        let unit_w = (bw >> shift_x).max(4);
        let unit_h = (bw >> shift_y).max(4);
        let n_x = unit_w.div_ceil(4);
        let n_y = unit_h.div_ceil(4);
        for ty in 0..n_y {
            for tx in 0..n_x {
                let px = sx + tx * 4;
                let py = sy + ty * 4;
                if px >= self.plane_coded_widths[plane] || py >= self.plane_coded_heights[plane] {
                    continue;
                }
                self.transform_block(plane, px, py, bw, unit_w * unit_h, mode, sx, sy);
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
        block_sx: usize,
        block_sy: usize,
    ) {
        let mut res = [0i32; 16];
        for i in 0..4 {
            for j in 0..4 {
                res[i * 4 + j] = self.sample(plane, sx + j, sy + i)
                    - self.predict_block_sample(
                        plane,
                        sx + j,
                        sy + i,
                        mode,
                        block_sx,
                        block_sy,
                        block_width,
                    );
            }
        }
        let quant = gamut_dsp::fwht4x4(&res);
        self.code_coeffs(plane, sx >> 2, sy >> 2, block_width, block_area, &quant);
    }

    /// Lossless 4×4 prediction for the two cardinal directional modes.  The
    /// fallback values match AV1's unavailable-edge reference construction:
    /// above defaults to `2^(bd-1)-1`, left to `2^(bd-1)+1`, and the first
    /// available edge is replicated across the missing side.
    #[allow(clippy::too_many_arguments)]
    fn predict_block_sample(
        &self,
        plane: usize,
        sx: usize,
        sy: usize,
        mode: u8,
        block_sx: usize,
        block_sy: usize,
        block_width: usize,
    ) -> i32 {
        if mode == DC_PRED {
            return self.dc_avg(plane, sx & !3, sy & !3);
        }
        let mid = 1 << (self.bit_depth - 1);
        let x_offset = sx - block_sx;
        let y_offset = sy - block_sy;
        let above = if block_sy > 0 {
            self.sample(plane, block_sx + x_offset, block_sy - 1)
        } else if block_sx > 0 {
            self.sample(plane, block_sx - 1, block_sy)
        } else {
            mid - 1
        };
        let left = if block_sx > 0 {
            self.sample(plane, block_sx - 1, block_sy + y_offset)
        } else if block_sy > 0 {
            self.sample(plane, block_sx, block_sy - 1)
        } else {
            mid + 1
        };
        match mode {
            V_PRED => above,
            H_PRED => left,
            D45_PRED | D67_PRED => {
                let derivative = if mode == D45_PRED { 64 } else { 27 };
                directional_zone_one(
                    &self.reference_above(plane, block_sx, block_sy, block_sx + block_width),
                    x_offset,
                    y_offset,
                    derivative,
                )
            }
            D203_PRED => directional_zone_three(
                &self.reference_left(plane, block_sx, block_sy, block_sy + block_width),
                x_offset,
                y_offset,
                27,
            ),
            D135_PRED | D113_PRED | D157_PRED => {
                let (dx, dy) = match mode {
                    D135_PRED => (64, 64),
                    D113_PRED => (27, 151),
                    _ => (151, 27),
                };
                directional_zone_two(
                    &self.reference_above(plane, block_sx, block_sy, block_sx + block_width),
                    &self.reference_left(plane, block_sx, block_sy, block_sy + block_width),
                    self.reference_top_left(plane, block_sx, block_sy),
                    x_offset,
                    y_offset,
                    dx,
                    dy,
                )
            }
            SMOOTH_PRED | SMOOTH_V_PRED | SMOOTH_H_PRED => {
                let above_ref =
                    self.reference_above(plane, block_sx, block_sy, block_sx + block_width);
                let left_ref =
                    self.reference_left(plane, block_sx, block_sy, block_sy + block_width);
                let top_right = above_ref[block_width - 1];
                let bottom_left = left_ref[block_width - 1];
                let wx = smooth_weight(block_width, x_offset);
                let wy = smooth_weight(block_width, y_offset);
                let smooth_v = (wy * above_ref[x_offset] + (256 - wy) * bottom_left + 128) >> 8;
                let smooth_h = (wx * left_ref[y_offset] + (256 - wx) * top_right + 128) >> 8;
                match mode {
                    SMOOTH_V_PRED => smooth_v,
                    SMOOTH_H_PRED => smooth_h,
                    _ => (smooth_v + smooth_h + 1) >> 1,
                }
            }
            PAETH_PRED => {
                let top_left = if block_sx > 0 && block_sy > 0 {
                    self.sample(plane, block_sx - 1, block_sy - 1)
                } else if block_sy > 0 {
                    self.sample(plane, block_sx, block_sy - 1)
                } else if block_sx > 0 {
                    self.sample(plane, block_sx - 1, block_sy)
                } else {
                    mid
                };
                let base = above + left - top_left;
                let p_left = (base - left).abs();
                let p_top = (base - above).abs();
                let p_top_left = (base - top_left).abs();
                if p_left <= p_top && p_left <= p_top_left {
                    left
                } else if p_top <= p_top_left {
                    above
                } else {
                    top_left
                }
            }
            _ => unreachable!("lossless mode search only emits AV1 intra modes"),
        }
    }

    fn reference_above(
        &self,
        plane: usize,
        block_sx: usize,
        block_sy: usize,
        block_end_x: usize,
    ) -> Vec<i32> {
        let n = block_end_x - block_sx;
        let fallback = 1 << (self.bit_depth - 1);
        if block_sy > 0 {
            (0..2 * n + 1)
                .map(|x| self.sample(plane, block_sx + x, block_sy - 1))
                .collect()
        } else if block_sx > 0 {
            vec![self.sample(plane, block_sx - 1, block_sy); 2 * n + 1]
        } else {
            vec![fallback - 1; 2 * n + 1]
        }
    }

    fn reference_left(
        &self,
        plane: usize,
        block_sx: usize,
        block_sy: usize,
        block_end_y: usize,
    ) -> Vec<i32> {
        let n = block_end_y - block_sy;
        let fallback = 1 << (self.bit_depth - 1);
        if block_sx > 0 {
            (0..2 * n + 1)
                .map(|y| self.sample(plane, block_sx - 1, block_sy + y))
                .collect()
        } else if block_sy > 0 {
            vec![self.sample(plane, block_sx, block_sy - 1); 2 * n + 1]
        } else {
            vec![fallback + 1; 2 * n + 1]
        }
    }

    fn reference_top_left(&self, plane: usize, block_sx: usize, block_sy: usize) -> i32 {
        let mid = 1 << (self.bit_depth - 1);
        if block_sx > 0 && block_sy > 0 {
            self.sample(plane, block_sx - 1, block_sy - 1)
        } else if block_sy > 0 {
            self.sample(plane, block_sx, block_sy - 1)
        } else if block_sx > 0 {
            self.sample(plane, block_sx - 1, block_sy)
        } else {
            mid
        }
    }

    /// DC intra prediction value for a 4×4 transform block (§7.11.2.5). Neighbours are the
    /// reconstructed samples, which equal the (padded) source under lossless coding.
    fn dc_avg(&self, plane: usize, sx: usize, sy: usize) -> i32 {
        let have_above = sy > 0;
        let have_left = sx > 0;
        match (have_above, have_left) {
            (true, true) => {
                let mut s = 0;
                for k in 0..4 {
                    s += self.sample(plane, sx + k, sy - 1);
                    s += self.sample(plane, sx - 1, sy + k);
                }
                (s + 4) >> 3
            }
            (false, true) => {
                let mut s = 0;
                for k in 0..4 {
                    s += self.sample(plane, sx - 1, sy + k);
                }
                (s + 2) >> 2
            }
            (true, false) => {
                let mut s = 0;
                for k in 0..4 {
                    s += self.sample(plane, sx + k, sy - 1);
                }
                (s + 2) >> 2
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

        let cul = levels.iter().sum::<i32>().min(63) as u8;
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
            let top = i32::from(self.above_level[0][x4]);
            let left = i32::from(self.left_level[0][y4]);
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

fn palette_color_context(
    index_map: &[u8],
    bw: usize,
    row: usize,
    col: usize,
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
    for index in 0..2 {
        let mut max_index = index;
        for candidate in index..2 {
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

fn interpolate(a: i32, b: i32, shift: i32) -> i32 {
    (a * (32 - shift) + b * shift + 16) >> 5
}

fn directional_zone_one(reference: &[i32], x: usize, y: usize, derivative: i32) -> i32 {
    let index = (y as i32 + 1) * derivative;
    let base = (index >> 6) as usize + x;
    interpolate(reference[base], reference[base + 1], (index >> 1) & 31)
}

fn directional_zone_three(reference: &[i32], x: usize, y: usize, derivative: i32) -> i32 {
    let index = (x as i32 + 1) * derivative;
    let base = (index >> 6) as usize + y;
    interpolate(reference[base], reference[base + 1], (index >> 1) & 31)
}

fn directional_zone_two(
    above: &[i32],
    left: &[i32],
    top_left: i32,
    x: usize,
    y: usize,
    dx: i32,
    dy: i32,
) -> i32 {
    let index = (x as i32) * 64 - (y as i32 + 1) * dx;
    let base = index >> 6;
    if base >= -1 {
        let shift = (index >> 1) & 31;
        let a0 = if base < 0 {
            top_left
        } else {
            above[base as usize]
        };
        interpolate(a0, above[(base + 1) as usize], shift)
    } else {
        let index = (y as i32) * 64 - (x as i32 + 1) * dy;
        let base = index >> 6;
        let shift = (index >> 1) & 31;
        let a0 = if base < 0 {
            top_left
        } else {
            left[base as usize]
        };
        interpolate(a0, left[(base + 1) as usize], shift)
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

fn split_cdf_key(bsl: usize, ctx: usize, vertical: bool) -> usize {
    0x1000_0000 | (bsl << 8) | (ctx << 1) | usize::from(vertical)
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
