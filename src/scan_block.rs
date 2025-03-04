//! Main block aligner algorithm and supporting data structures.

#[cfg(feature = "simd_avx2")]
use crate::avx2::*;

#[cfg(feature = "simd_wasm")]
use crate::simd128::*;

use crate::scores::*;
use crate::cigar::*;

use std::{cmp, ptr, i16, alloc};
use std::ops::RangeInclusive;
use std::any::TypeId;

// Notes:
//
// BLOSUM62 matrix max = 11, min = -4; gap open = -11 (includes extension), gap extend = -1
//
// Dynamic programming formula:
// R[i][j] = max(R[i - 1][j] + gap_extend, D[i - 1][j] + gap_open)
// C[i][j] = max(C[i][j - 1] + gap_extend, D[i][j - 1] + gap_open)
// D[i][j] = max(D[i - 1][j - 1] + matrix[query[i]][reference[j]], R[i][j], C[i][j])
//
// indexing (we want to calculate D11):
//      x0   x1
//    +--------
// 0x | 00   01
// 1x | 10   11
//
// note that 'x' represents any bit
//
// The term "block" gets used in two contexts:
// 1. A square region of the DP matrix, which is helpful for conceptually visualizing
// the algorithm.
// 2. A rectangular region representing only cells in the DP matrix that are calculated
// due to shifting or growing. Since the step size is smaller than the block size, the
// square blocks overlap. Only the non-overlapping new cells (a rectangular block) are
// computed in each step.

/// Data structure storing the settings for block aligner.
pub struct Block<'a, M: 'static + Matrix, const TRACE: bool, const X_DROP: bool> {
    res: AlignResult,
    trace: Trace,
    query: &'a PaddedBytes,
    i: usize,
    reference: &'a PaddedBytes,
    j: usize,
    min_size: usize,
    max_size: usize,
    matrix: &'a M,
    gaps: Gaps,
    x_drop: i32
}

// increasing step size gives a bit extra speed but results in lower accuracy
// current settings are fast, at the expense of some accuracy, and step size does not grow
const STEP: usize = if L / 2 < 8 { L / 2 } else { 8 };
const LARGE_STEP: usize = STEP; // use larger step size when the block size gets large
const GROW_STEP: usize = L; // used when not growing by powers of 2
const GROW_EXP: bool = true; // grow by powers of 2
const X_DROP_ITER: usize = 2; // make sure that the X-drop iteration is truly met instead of just one "bad" step
impl<'a, M: 'static + Matrix, const TRACE: bool, const X_DROP: bool> Block<'a, M, { TRACE }, { X_DROP }> {
    /// Align two strings with block aligner.
    ///
    /// If `TRACE` is true, then information for computing the traceback will be stored.
    /// After alignment, the traceback CIGAR string can then be computed.
    /// This will slow down alignment and use a lot more memory.
    ///
    /// If `X_DROP` is true, then the alignment process will be terminated early when
    /// the max score in the current block drops by `x_drop` below the max score encountered
    /// so far. If `X_DROP` is false, then global alignment is done.
    ///
    /// Since larger scores are better, gap and mismatches penalties should be negative.
    ///
    /// The minimum and maximum sizes of the block must be powers of 2 that are greater than the
    /// number of 16-bit lanes in a SIMD vector.
    ///
    /// The block aligner algorithm will dynamically shift a block down or right and grow its size
    /// to efficiently calculate the alignment between two strings.
    /// This is fast, but it may be slightly less accurate than computing the entire the alignment
    /// dynamic programming matrix. Growing the size of the block allows larger gaps and
    /// other potentially difficult regions to be handled correctly.
    /// 16-bit deltas and 32-bit offsets are used to ensure that accurate scores are
    /// computed, even when the the strings are long.
    pub fn align(query: &'a PaddedBytes, reference: &'a PaddedBytes, matrix: &'a M, gaps: Gaps, size: RangeInclusive<usize>, x_drop: i32) -> Self {
        // check invariants so bad stuff doesn't happen later
        assert!(gaps.open < 0 && gaps.extend < 0, "Gap costs must be negative!");
        // there are edge cases with calculating traceback that doesn't work if
        // gap open does not cost more than gap extend
        assert!(gaps.open < gaps.extend, "Gap open must cost more than gap extend!");
        let min_size = if *size.start() < L { L } else { *size.start() };
        let max_size = if *size.end() < L { L } else { *size.end() };
        assert!(min_size < (u16::MAX as usize) && max_size < (u16::MAX as usize), "Block sizes must be smaller than 2^16 - 1!");
        if GROW_EXP {
            assert!(min_size.is_power_of_two() && max_size.is_power_of_two(), "Block sizes must be powers of two!");
        } else {
            assert!(min_size % L == 0 && max_size % L == 0, "Block sizes must be multiples of {}!", L);
        }
        if X_DROP {
            assert!(x_drop >= 0, "X-drop threshold amount must be nonnegative!");
            assert!(TypeId::of::<M>() != TypeId::of::<ByteMatrix>(), "X-drop alignment with ByteMatrix is not fully supported!");
        }

        let mut a = Self {
            res: AlignResult { score: 0, query_idx: 0, reference_idx: 0 },
            trace: if TRACE { Trace::new(query.len(), reference.len()) } else { Trace::new(0, 0) },
            query,
            i: 0,
            reference,
            j: 0,
            min_size,
            max_size,
            matrix,
            gaps,
            x_drop
        };

        unsafe { a.align_core(); }
        a
    }

    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    unsafe fn align_core(&mut self) {
        // store the best alignment ending location for x drop alignment
        let mut best_max = 0i32;
        let mut best_argmax_i = 0usize;
        let mut best_argmax_j = 0usize;

        let mut prev_dir = Direction::Grow;
        let mut dir = Direction::Grow;
        let mut prev_size = 0;
        let mut block_size = self.min_size;
        let mut step = STEP;

        // 32-bit score offsets
        let mut off = 0i32;
        let mut prev_off;
        let mut off_max = 0i32;

        // bottom and right borders of the current block
        let mut D_col = Aligned::new(self.max_size);
        let mut C_col = Aligned::new(self.max_size);
        let mut D_row = Aligned::new(self.max_size);
        let mut R_row = Aligned::new(self.max_size);

        // reused buffers for storing values that must be shifted
        // into the other border when the block moves in one direction
        let mut temp_buf1 = Aligned::new(L);
        let mut temp_buf2 = Aligned::new(L);

        // how many steps since the latest best score was encountered
        let mut y_drop_iter = 0;

        // how many steps where the X-drop threshold is met
        let mut x_drop_iter = 0;

        // the state at the previous checkpoint (where latest best score was encountered)
        let mut i_ckpt = self.i;
        let mut j_ckpt = self.j;
        let mut off_ckpt = 0i32;
        let mut D_col_ckpt = Aligned::new(self.max_size);
        let mut C_col_ckpt = Aligned::new(self.max_size);
        let mut D_row_ckpt = Aligned::new(self.max_size);
        let mut R_row_ckpt = Aligned::new(self.max_size);

        let prefix_scan_consts = get_prefix_scan_consts(self.gaps.extend as i16);
        let gap_extend_all = get_gap_extend_all(self.gaps.extend as i16);

        // corner value that affects the score when shifting down then right, or right then down
        let mut D_corner = simd_set1_i16(MIN);

        loop {
            #[cfg(feature = "debug")]
            {
                println!("i: {}", self.i);
                println!("j: {}", self.j);
                println!("{:?}", dir);
                println!("block size: {}", block_size);
            }

            prev_off = off;
            let mut grow_D_max = simd_set1_i16(MIN);
            let mut grow_D_argmax = simd_set1_i16(0);
            let (D_max, D_argmax, right_max, down_max) = match dir {
                Direction::Right => {
                    off = off_max;
                    #[cfg(feature = "debug")]
                    println!("off: {}", off);
                    let off_add = simd_set1_i16(clamp(prev_off - off));

                    if TRACE {
                        self.trace.add_block(self.i, self.j + block_size - step, step, block_size, true);
                    }

                    // offset previous columns with newly computed offset
                    self.just_offset(block_size, D_col.as_mut_ptr(), C_col.as_mut_ptr(), off_add);

                    // compute new elements in the block as a result of shifting by the step size
                    // this region should be block_size x step
                    let (D_max, D_argmax) = self.place_block(
                        self.query,
                        self.reference,
                        self.i,
                        self.j + block_size - step,
                        step,
                        block_size,
                        D_col.as_mut_ptr(),
                        C_col.as_mut_ptr(),
                        temp_buf1.as_mut_ptr(),
                        temp_buf2.as_mut_ptr(),
                        if prev_dir == Direction::Down { simd_adds_i16(D_corner, off_add) } else { simd_set1_i16(MIN) },
                        true,
                        prefix_scan_consts,
                        gap_extend_all
                    );

                    // sum of a couple elements on the right border
                    let right_max = self.prefix_max(D_col.as_ptr(), step);

                    // shift and offset bottom row
                    D_corner = self.shift_and_offset(
                        block_size,
                        D_row.as_mut_ptr(),
                        R_row.as_mut_ptr(),
                        temp_buf1.as_mut_ptr(),
                        temp_buf2.as_mut_ptr(),
                        off_add,
                        step
                    );
                    // sum of a couple elements on the bottom border
                    let down_max = self.prefix_max(D_row.as_ptr(), step);

                    (D_max, D_argmax, right_max, down_max)
                },
                Direction::Down => {
                    off = off_max;
                    #[cfg(feature = "debug")]
                    println!("off: {}", off);
                    let off_add = simd_set1_i16(clamp(prev_off - off));

                    if TRACE {
                        self.trace.add_block(self.i + block_size - step, self.j, block_size, step, false);
                    }

                    // offset previous rows with newly computed offset
                    self.just_offset(block_size, D_row.as_mut_ptr(), R_row.as_mut_ptr(), off_add);

                    // compute new elements in the block as a result of shifting by the step size
                    // this region should be step x block_size
                    let (D_max, D_argmax) = self.place_block(
                        self.reference,
                        self.query,
                        self.j,
                        self.i + block_size - step,
                        step,
                        block_size,
                        D_row.as_mut_ptr(),
                        R_row.as_mut_ptr(),
                        temp_buf1.as_mut_ptr(),
                        temp_buf2.as_mut_ptr(),
                        if prev_dir == Direction::Right { simd_adds_i16(D_corner, off_add) } else { simd_set1_i16(MIN) },
                        false,
                        prefix_scan_consts,
                        gap_extend_all
                    );

                    // sum of a couple elements on the bottom border
                    let down_max = self.prefix_max(D_row.as_ptr(), step);

                    // shift and offset last column
                    D_corner = self.shift_and_offset(
                        block_size,
                        D_col.as_mut_ptr(),
                        C_col.as_mut_ptr(),
                        temp_buf1.as_mut_ptr(),
                        temp_buf2.as_mut_ptr(),
                        off_add,
                        step
                    );
                    // sum of a couple elements on the right border
                    let right_max = self.prefix_max(D_col.as_ptr(), step);

                    (D_max, D_argmax, right_max, down_max)
                },
                Direction::Grow => {
                    D_corner = simd_set1_i16(MIN);
                    let grow_step = block_size - prev_size;

                    #[cfg(feature = "debug")]
                    println!("off: {}", off);
                    #[cfg(feature = "debug")]
                    println!("Grow down");

                    if TRACE {
                        // with a larger block, the size of the trace array might need to be
                        // increased
                        self.trace.resize_trace(self.i, self.j, self.query.len(), self.reference.len(), block_size);
                        self.trace.add_block(self.i + prev_size, self.j, prev_size, grow_step, false);
                    }

                    // down
                    // this region should be prev_size x prev_size
                    let (D_max1, D_argmax1) = self.place_block(
                        self.reference,
                        self.query,
                        self.j,
                        self.i + prev_size,
                        grow_step,
                        prev_size,
                        D_row.as_mut_ptr(),
                        R_row.as_mut_ptr(),
                        D_col.as_mut_ptr().add(prev_size),
                        C_col.as_mut_ptr().add(prev_size),
                        simd_set1_i16(MIN),
                        false,
                        prefix_scan_consts,
                        gap_extend_all
                    );

                    #[cfg(feature = "debug")]
                    println!("Grow right");

                    if TRACE {
                        self.trace.add_block(self.i, self.j + prev_size, grow_step, block_size, true);
                    }

                    // right
                    // this region should be block_size x prev_size
                    let (D_max2, D_argmax2) = self.place_block(
                        self.query,
                        self.reference,
                        self.i,
                        self.j + prev_size,
                        grow_step,
                        block_size,
                        D_col.as_mut_ptr(),
                        C_col.as_mut_ptr(),
                        D_row.as_mut_ptr().add(prev_size),
                        R_row.as_mut_ptr().add(prev_size),
                        simd_set1_i16(MIN),
                        true,
                        prefix_scan_consts,
                        gap_extend_all
                    );

                    let right_max = self.prefix_max(D_col.as_ptr(), step);
                    let down_max = self.prefix_max(D_row.as_ptr(), step);
                    grow_D_max = D_max1;
                    grow_D_argmax = D_argmax1;

                    // must update the checkpoint saved values just in case
                    // the block must grow again from this position
                    let mut i = 0;
                    while i < block_size {
                        D_col_ckpt.set_vec(&D_col, i);
                        C_col_ckpt.set_vec(&C_col, i);
                        D_row_ckpt.set_vec(&D_row, i);
                        R_row_ckpt.set_vec(&R_row, i);
                        i += L;
                    }

                    if TRACE {
                        self.trace.save_ckpt();
                    }

                    (D_max2, D_argmax2, right_max, down_max)
                }
            };

            prev_dir = dir;
            let D_max_max = simd_hmax_i16(D_max);
            // grow max is an auxiliary value used when growing because it requires two separate
            // place_block steps
            let grow_max = simd_hmax_i16(grow_D_max);
            // max score of the entire block
            let max = cmp::max(D_max_max, grow_max);
            off_max = off + (max as i32) - (ZERO as i32);
            #[cfg(feature = "debug")]
            println!("down max: {}, right max: {}", down_max, right_max);

            y_drop_iter += 1;
            // if block grows but the best score does not improve, then the block must grow again
            let mut grow_no_max = dir == Direction::Grow;

            if off_max > best_max {
                if X_DROP {
                    // calculate location with the best score
                    let lane_idx = simd_hargmax_i16(D_max, D_max_max);
                    let idx = simd_slow_extract_i16(D_argmax, lane_idx) as usize;
                    let r = (idx % (block_size / L)) * L + lane_idx;
                    let c = (block_size - step) + idx / (block_size / L);

                    match dir {
                        Direction::Right => {
                            best_argmax_i = self.i + r;
                            best_argmax_j = self.j + c;
                        },
                        Direction::Down => {
                            best_argmax_i = self.i + c;
                            best_argmax_j = self.j + r;
                        },
                        Direction::Grow => {
                            // max could be in either block
                            if max >= grow_max {
                                best_argmax_i = self.i + (idx % (block_size / L)) * L + lane_idx;
                                best_argmax_j = self.j + prev_size + idx / (block_size / L);
                            } else {
                                let lane_idx = simd_hargmax_i16(grow_D_max, grow_max);
                                let idx = simd_slow_extract_i16(grow_D_argmax, lane_idx) as usize;
                                best_argmax_i = self.i + prev_size + idx / (prev_size / L);
                                best_argmax_j = self.j + (idx % (prev_size / L)) * L + lane_idx;
                            }
                        }
                    }
                }

                if block_size < self.max_size {
                    // if able to grow in the future, then save the current location
                    // as a checkpoint
                    i_ckpt = self.i;
                    j_ckpt = self.j;
                    off_ckpt = off;

                    let mut i = 0;
                    while i < block_size {
                        D_col_ckpt.set_vec(&D_col, i);
                        C_col_ckpt.set_vec(&C_col, i);
                        D_row_ckpt.set_vec(&D_row, i);
                        R_row_ckpt.set_vec(&R_row, i);
                        i += L;
                    }

                    if TRACE {
                        self.trace.save_ckpt();
                    }

                    grow_no_max = false;
                }

                best_max = off_max;

                y_drop_iter = 0;
            }

            if X_DROP {
                if off_max < best_max - self.x_drop {
                    if x_drop_iter < X_DROP_ITER - 1 {
                        x_drop_iter += 1;
                    } else {
                        // x drop termination
                        break;
                    }
                } else {
                    x_drop_iter = 0;
                }
            }

            if self.i + block_size > self.query.len() && self.j + block_size > self.reference.len() {
                // reached the end of the strings
                break;
            }

            // first check if the shift direction is "forced" to avoid going out of bounds
            if self.j + block_size > self.reference.len() {
                self.i += step;
                dir = Direction::Down;
                continue;
            }
            if self.i + block_size > self.query.len() {
                self.j += step;
                dir = Direction::Right;
                continue;
            }

            // check if it is possible to grow
            let next_size = if GROW_EXP { block_size * 2 } else { block_size + GROW_STEP };
            if next_size <= self.max_size {
                // if approximately (block_size / step) iterations has passed since the last best
                // max, then it is time to grow
                if y_drop_iter > (block_size / step) - 1 || grow_no_max {
                    // y drop grow block
                    prev_size = block_size;
                    block_size = next_size;
                    dir = Direction::Grow;
                    if STEP != LARGE_STEP && block_size >= (LARGE_STEP / STEP) * self.min_size {
                        step = LARGE_STEP;
                    }

                    // return to checkpoint
                    self.i = i_ckpt;
                    self.j = j_ckpt;
                    off = off_ckpt;

                    let mut i = 0;
                    while i < prev_size {
                        D_col.set_vec(&D_col_ckpt, i);
                        C_col.set_vec(&C_col_ckpt, i);
                        D_row.set_vec(&D_row_ckpt, i);
                        R_row.set_vec(&R_row_ckpt, i);
                        i += L;
                    }

                    if TRACE {
                        self.trace.restore_ckpt();
                    }

                    y_drop_iter = 0;
                    continue;
                }
            }

            // move according to where the max is
            if down_max > right_max {
                self.i += step;
                dir = Direction::Down;
            } else {
                self.j += step;
                dir = Direction::Right;
            }
        }

        #[cfg(any(feature = "debug", feature = "debug_size"))]
        {
            println!("query size: {}, reference size: {}", self.query.len() - 1, self.reference.len() - 1);
            println!("end block size: {}", block_size);
        }

        self.res = if X_DROP {
            AlignResult {
                score: best_max,
                query_idx: best_argmax_i,
                reference_idx: best_argmax_j
            }
        } else {
            debug_assert!(self.i <= self.query.len());
            let score = off + match dir {
                Direction::Right | Direction::Grow => {
                    let idx = self.query.len() - self.i;
                    debug_assert!(idx < block_size);
                    (D_col.get(idx) as i32) - (ZERO as i32)
                },
                Direction::Down => {
                    let idx = self.reference.len() - self.j;
                    debug_assert!(idx < block_size);
                    (D_row.get(idx) as i32) - (ZERO as i32)
                }
            };
            AlignResult {
                score,
                query_idx: self.query.len(),
                reference_idx: self.reference.len()
            }
        };
    }

    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    #[inline]
    unsafe fn just_offset(&self, block_size: usize, buf1: *mut i16, buf2: *mut i16, off_add: Simd) {
        let mut i = 0;
        while i < block_size {
            let a = simd_adds_i16(simd_load(buf1.add(i) as _), off_add);
            let b = simd_adds_i16(simd_load(buf2.add(i) as _), off_add);
            simd_store(buf1.add(i) as _, a);
            simd_store(buf2.add(i) as _, b);
            i += L;
        }
    }

    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    #[inline]
    unsafe fn prefix_max(&self, buf: *const i16, step: usize) -> i16 {
        if STEP == LARGE_STEP {
            simd_prefix_hadd_i16!(simd_load(buf as _), STEP)
        } else {
            if step == STEP {
                simd_prefix_hadd_i16!(simd_load(buf as _), STEP)
            } else {
                simd_prefix_hadd_i16!(simd_load(buf as _), LARGE_STEP)
            }
        }
    }

    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    #[inline]
    unsafe fn shift_and_offset(&self, block_size: usize, buf1: *mut i16, buf2: *mut i16, temp_buf1: *mut i16, temp_buf2: *mut i16, off_add: Simd, step: usize) -> Simd {
        #[inline]
        unsafe fn sr(a: Simd, b: Simd, step: usize) -> Simd {
            if STEP == LARGE_STEP {
                simd_sr_i16!(a, b, STEP)
            } else {
                if step == STEP {
                    simd_sr_i16!(a, b, STEP)
                } else {
                    simd_sr_i16!(a, b, LARGE_STEP)
                }
            }
        }
        let mut curr1 = simd_adds_i16(simd_load(buf1 as _), off_add);
        let D_corner = simd_set1_i16(simd_extract_i16!(curr1, STEP - 1));
        let mut curr2 = simd_adds_i16(simd_load(buf2 as _), off_add);

        let mut i = 0;
        while i < block_size - L {
            let next1 = simd_adds_i16(simd_load(buf1.add(i + L) as _), off_add);
            let next2 = simd_adds_i16(simd_load(buf2.add(i + L) as _), off_add);
            let shifted = sr(next1, curr1, step);
            simd_store(buf1.add(i) as _, shifted);
            simd_store(buf2.add(i) as _, sr(next2, curr2, step));
            curr1 = next1;
            curr2 = next2;
            i += L;
        }

        let next1 = simd_load(temp_buf1 as _);
        let next2 = simd_load(temp_buf2 as _);
        let shifted = sr(next1, curr1, step);
        simd_store(buf1.add(block_size - L) as _, shifted);
        simd_store(buf2.add(block_size - L) as _, sr(next2, curr2, step));
        D_corner
    }

    /// Place block right or down.
    ///
    /// Assumes all inputs are already relative to the current offset.
    ///
    /// Inside this function, everything will be treated as shifting right,
    /// conceptually. The same process can be trivially used for shifting
    /// down by calling this function with different parameters.
    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    // Want this to be inlined in some places and not others, so let
    // compiler decide.
    unsafe fn place_block(&mut self,
                          query: &PaddedBytes,
                          reference: &PaddedBytes,
                          start_i: usize,
                          start_j: usize,
                          width: usize,
                          height: usize,
                          D_col: *mut i16,
                          C_col: *mut i16,
                          D_row: *mut i16,
                          R_row: *mut i16,
                          mut D_corner: Simd,
                          right: bool,
                          prefix_scan_consts: PrefixScanConsts,
                          gap_extend_all: Simd) -> (Simd, Simd) {
        let (gap_open, gap_extend) = self.get_const_simd();
        let mut D_max = simd_set1_i16(MIN);
        let mut D_argmax = simd_set1_i16(0);
        let mut curr_i = simd_set1_i16(0);

        if width == 0 || height == 0 {
            return (D_max, D_argmax);
        }

        // hottest loop in the whole program
        for j in 0..width {
            let mut R01 = simd_set1_i16(MIN);
            let mut D11 = simd_set1_i16(MIN);
            let mut R11 = simd_set1_i16(MIN);

            let c = reference.get(start_j + j);

            let mut i = 0;
            while i < height {
                #[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "mca"))]
                asm!("# LLVM-MCA-BEGIN place_block inner loop", options(nomem, nostack, preserves_flags));

                let D10 = simd_load(D_col.add(i) as _);
                let C10 = simd_load(C_col.add(i) as _);
                let D00 = simd_sl_i16!(D10, D_corner, 1);
                D_corner = D10;

                let scores = self.matrix.get_scores(c, halfsimd_loadu(query.as_ptr(start_i + i) as _), right);
                D11 = simd_adds_i16(D00, scores);
                if start_i + i == 0 && start_j + j == 0 {
                    D11 = simd_insert_i16!(D11, ZERO, 0);
                }

                let C11 = simd_max_i16(simd_adds_i16(C10, gap_extend), simd_adds_i16(D10, gap_open));
                D11 = simd_max_i16(D11, C11);
                // at this point, C11 is fully calculated and D11 is partially calculated

                let D11_open = simd_adds_i16(D11, simd_subs_i16(gap_open, gap_extend));
                R11 = simd_prefix_scan_i16(D11_open, prefix_scan_consts);
                // do prefix scan before using R01 to break up dependency chain that depends on
                // the last element of R01 from the previous loop iteration
                R11 = simd_max_i16(R11, simd_adds_i16(simd_broadcasthi_i16(R01), gap_extend_all));
                // fully calculate D11 using R11
                D11 = simd_max_i16(D11, R11);
                R01 = R11;

                #[cfg(feature = "debug")]
                {
                    print!("s:   ");
                    simd_dbg_i16(scores);
                    print!("D00: ");
                    simd_dbg_i16(simd_subs_i16(D00, simd_set1_i16(ZERO)));
                    print!("C11: ");
                    simd_dbg_i16(simd_subs_i16(C11, simd_set1_i16(ZERO)));
                    print!("R11: ");
                    simd_dbg_i16(simd_subs_i16(R11, simd_set1_i16(ZERO)));
                    print!("D11: ");
                    simd_dbg_i16(simd_subs_i16(D11, simd_set1_i16(ZERO)));
                }

                if TRACE {
                    let trace_D_C = simd_cmpeq_i16(D11, C11);
                    let trace_D_R = simd_cmpeq_i16(D11, R11);
                    #[cfg(feature = "debug")]
                    {
                        print!("D_C: ");
                        simd_dbg_i16(trace_D_C);
                        print!("D_R: ");
                        simd_dbg_i16(trace_D_R);
                    }
                    // compress trace with movemask to save space
                    let trace = simd_movemask_i8(simd_blend_i8(trace_D_C, trace_D_R, simd_set1_i16(0xFF00u16 as i16)));
                    self.trace.add_trace(trace as TraceType);
                }

                D_max = simd_max_i16(D_max, D11);

                if X_DROP {
                    // keep track of the best score and its location
                    let mask = simd_cmpeq_i16(D_max, D11);
                    D_argmax = simd_blend_i8(D_argmax, curr_i, mask);
                    curr_i = simd_adds_i16(curr_i, simd_set1_i16(1));
                }

                simd_store(D_col.add(i) as _, D11);
                simd_store(C_col.add(i) as _, C11);
                i += L;

                #[cfg(all(any(target_arch = "x86", target_arch = "x86_64"), feature = "mca"))]
                asm!("# LLVM-MCA-END", options(nomem, nostack, preserves_flags));
            }

            D_corner = simd_set1_i16(MIN);

            ptr::write(D_row.add(j), simd_extract_i16!(D11, L - 1));
            ptr::write(R_row.add(j), simd_extract_i16!(R11, L - 1));

            if !X_DROP && start_i + height > query.len()
                && start_j + j >= reference.len() {
                if TRACE {
                    // make sure that the trace index is updated since the rest of the loop
                    // iterations are skipped
                    self.trace.add_trace_idx((width - 1 - j) * (height / L));
                }
                break;
            }
        }

        (D_max, D_argmax)
    }

    /// Get the resulting score and ending location of the alignment.
    #[inline]
    pub fn res(&self) -> AlignResult {
        self.res
    }

    /// Get the trace of the alignment, assuming `TRACE` is true.
    #[inline]
    pub fn trace(&self) -> &Trace {
        assert!(TRACE);
        &self.trace
    }

    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[inline]
    unsafe fn get_const_simd(&self) -> (Simd, Simd) {
        // some useful constant simd vectors
        let gap_open = simd_set1_i16(self.gaps.open as i16);
        let gap_extend = simd_set1_i16(self.gaps.extend as i16);
        (gap_open, gap_extend)
    }
}

/// Holds the trace generated by block aligner.
#[derive(Clone)]
pub struct Trace {
    trace: Vec<TraceType>,
    right: Vec<u64>,
    block_start: Vec<u32>,
    block_size: Vec<u16>,
    trace_idx: usize,
    block_idx: usize,
    ckpt_trace_idx: usize,
    ckpt_block_idx: usize,
    query_len: usize,
    reference_len: usize
}

impl Trace {
    #[inline]
    fn new(query_len: usize, reference_len: usize) -> Self {
        let len = query_len + reference_len;
        let trace = Vec::new();
        let right = vec![0u64; div_ceil(len, 64)];
        let block_start = vec![0u32; len * 2];
        let block_size = vec![0u16; len * 2];

        Self {
            trace,
            right,
            block_start,
            block_size,
            trace_idx: 0,
            block_idx: 0,
            ckpt_trace_idx: 0,
            ckpt_block_idx: 0,
            query_len,
            reference_len
        }
    }

    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[inline]
    unsafe fn add_trace(&mut self, t: TraceType) {
        debug_assert!(self.trace_idx < self.trace.len());
        store_trace(self.trace.as_mut_ptr().add(self.trace_idx), t);
        self.trace_idx += 1;
    }

    #[inline]
    fn add_block(&mut self, i: usize, j: usize, width: usize, height: usize, right: bool) {
        debug_assert!(self.block_idx * 2 < self.block_start.len());
        unsafe {
            *self.block_start.as_mut_ptr().add(self.block_idx * 2) = i as u32;
            *self.block_start.as_mut_ptr().add(self.block_idx * 2 + 1) = j as u32;
            *self.block_size.as_mut_ptr().add(self.block_idx * 2) = height as u16;
            *self.block_size.as_mut_ptr().add(self.block_idx * 2 + 1) = width as u16;

            let a = self.block_idx / 64;
            let b = self.block_idx % 64;
            let v = *self.right.as_ptr().add(a) & !(1 << b); // clear bit
            *self.right.as_mut_ptr().add(a) = v | ((right as u64) << b);

            self.block_idx += 1;
        }
    }

    /// This must be used before adding new traces to make sure the trace array is large enough.
    #[inline]
    fn resize_trace(&mut self, i: usize, j: usize, q_len: usize, r_len: usize, block_size: usize) {
        self.trace.resize(self.trace_idx + (block_size / L) * (q_len + block_size - i + r_len + block_size - j), 0 as TraceType);
    }

    #[inline]
    fn add_trace_idx(&mut self, add: usize) {
        self.trace_idx += add;
    }

    #[inline]
    fn save_ckpt(&mut self) {
        self.ckpt_trace_idx = self.trace_idx;
        self.ckpt_block_idx = self.block_idx;
    }

    /// The trace data structure is like a stack, so all trace values and blocks after the
    /// checkpoint is essentially popped off the stack.
    #[inline]
    fn restore_ckpt(&mut self) {
        unsafe { self.trace.set_len(self.ckpt_trace_idx); }
        self.trace_idx = self.ckpt_trace_idx;
        self.block_idx = self.ckpt_block_idx;
    }

    /// Create a CIGAR string that represents a single traceback path ending on the specified
    /// location.
    pub fn cigar(&self, mut i: usize, mut j: usize) -> Cigar {
        assert!(i <= self.query_len && j <= self.reference_len, "Traceback cigar end position must be in bounds!");

        unsafe {
            let mut res = Cigar::new(i + j + 5);
            let mut block_idx = self.block_idx;
            let mut trace_idx = self.trace_idx;
            let mut block_i;
            let mut block_j;
            let mut block_width;
            let mut block_height;
            let mut right;

            // use lookup table instead of hard to predict branches
            static OP_LUT: [(Operation, usize, usize); 8] = [
                (Operation::M, 1, 1), // 0b000
                (Operation::I, 1, 0), // 0b001
                (Operation::D, 0, 1), // 0b010
                (Operation::I, 1, 0), // 0b011, bias towards i -= 1 to avoid going out of bounds
                (Operation::M, 1, 1), // 0b100
                (Operation::D, 0, 1), // 0b101
                (Operation::I, 1, 0), // 0b110
                (Operation::D, 0, 1) // 0b111, bias towards j -= 1 to avoid going out of bounds
            ];

            while i > 0 || j > 0 {
                loop {
                    block_idx -= 1;
                    block_i = *self.block_start.as_ptr().add(block_idx * 2) as usize;
                    block_j = *self.block_start.as_ptr().add(block_idx * 2 + 1) as usize;
                    block_height = *self.block_size.as_ptr().add(block_idx * 2) as usize;
                    block_width = *self.block_size.as_ptr().add(block_idx * 2 + 1) as usize;
                    trace_idx -= block_width * block_height / L;

                    if i >= block_i && j >= block_j {
                        right = (((*self.right.as_ptr().add(block_idx / 64) >> (block_idx % 64)) & 0b1) << 2) as usize;
                        break;
                    }
                }

                if right > 0 {
                    while i >= block_i && j >= block_j && (i > 0 || j > 0) {
                        let curr_i = i - block_i;
                        let curr_j = j - block_j;
                        let idx = trace_idx + curr_i / L + curr_j * (block_height / L);
                        let t = ((*self.trace.as_ptr().add(idx) >> ((curr_i % L) * 2)) & 0b11) as usize;
                        let lut_idx = right | t;
                        let op = OP_LUT[lut_idx].0;
                        i -= OP_LUT[lut_idx].1;
                        j -= OP_LUT[lut_idx].2;
                        res.add(op);
                    }
                } else {
                    while i >= block_i && j >= block_j && (i > 0 || j > 0) {
                        let curr_i = i - block_i;
                        let curr_j = j - block_j;
                        let idx = trace_idx + curr_j / L + curr_i * (block_width / L);
                        let t = ((*self.trace.as_ptr().add(idx) >> ((curr_j % L) * 2)) & 0b11) as usize;
                        let lut_idx = right | t;
                        let op = OP_LUT[lut_idx].0;
                        i -= OP_LUT[lut_idx].1;
                        j -= OP_LUT[lut_idx].2;
                        res.add(op);
                    }
                }
            }

            res
        }
    }

    /// Return all of the rectangular regions that were calculated separately as
    /// block aligner shifts and grows.
    pub fn blocks(&self) -> Vec<Rectangle> {
        let mut res = Vec::with_capacity(self.block_idx);

        for i in 0..self.block_idx {
            unsafe {
                res.push(Rectangle {
                    row: *self.block_start.as_ptr().add(i * 2) as usize,
                    col: *self.block_start.as_ptr().add(i * 2 + 1) as usize,
                    height: *self.block_size.as_ptr().add(i * 2) as usize,
                    width: *self.block_size.as_ptr().add(i * 2 + 1) as usize
                });
            }
        }

        res
    }
}

/// A rectangular region.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Rectangle {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize
}

#[inline]
fn clamp(x: i32) -> i16 {
    cmp::min(cmp::max(x, i16::MIN as i32), i16::MAX as i32) as i16
}

#[inline]
fn div_ceil(n: usize, d: usize) -> usize {
    (n + d - 1) / d
}

/// Same alignment as SIMD vectors.
struct Aligned {
    layout: alloc::Layout,
    ptr: *const i16
}

impl Aligned {
    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    pub unsafe fn new(block_size: usize) -> Self {
        // custom alignment
        let layout = alloc::Layout::from_size_align_unchecked(block_size * 2, L_BYTES);
        let ptr = alloc::alloc_zeroed(layout) as *const i16;
        let mut i = 0;
        while i < block_size {
            simd_store(ptr.add(i) as _, simd_set1_i16(MIN));
            i += L;
        }
        Self { layout, ptr }
    }

    #[cfg_attr(feature = "simd_avx2", target_feature(enable = "avx2"))]
    #[cfg_attr(feature = "simd_wasm", target_feature(enable = "simd128"))]
    #[inline]
    pub unsafe fn set_vec(&mut self, o: &Aligned, idx: usize) {
        simd_store(self.ptr.add(idx) as _, simd_load(o.as_ptr().add(idx) as _));
    }

    #[inline]
    pub fn get(&self, i: usize) -> i16 {
        unsafe { *self.ptr.add(i) }
    }

    #[allow(dead_code)]
    #[inline]
    pub fn set(&mut self, i: usize, v: i16) {
        unsafe { ptr::write(self.ptr.add(i) as _, v); }
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut i16 {
        self.ptr as _
    }

    #[inline]
    pub fn as_ptr(&self) -> *const i16 {
        self.ptr
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { alloc::dealloc(self.ptr as _, self.layout); }
    }
}

/// A padded string that helps avoid out of bounds access when using SIMD.
///
/// A single padding byte in inserted before the start of the string,
/// and `block_size` bytes are inserted after the end of the string.
#[derive(Clone, PartialEq, Debug)]
pub struct PaddedBytes {
    s: Vec<u8>,
    len: usize
}

impl PaddedBytes {
    /// Create from a byte slice.
    ///
    /// Make sure that `block_size` is greater than or equal to the upper bound
    /// block size used in the `Block::align` function.
    #[inline]
    pub fn from_bytes<M: Matrix>(b: &[u8], block_size: usize) -> Self {
        let mut v = b.to_owned();
        let len = v.len();
        v.insert(0, M::NULL);
        v.resize(v.len() + block_size, M::NULL);
        v.iter_mut().for_each(|c| *c = M::convert_char(*c));
        Self { s: v, len }
    }

    /// Create from the bytes in a string slice.
    ///
    /// Make sure that `block_size` is greater than or equal to the upper bound
    /// block size used in the `Block::align` function.
    #[inline]
    pub fn from_str<M: Matrix>(s: &str, block_size: usize) -> Self {
        Self::from_bytes::<M>(s.as_bytes(), block_size)
    }

    /// Create from the bytes in a string.
    ///
    /// Make sure that `block_size` is greater than or equal to the upper bound
    /// block size used in the `Block::align` function.
    #[inline]
    pub fn from_string<M: Matrix>(s: String, block_size: usize) -> Self {
        let mut v = s.into_bytes();
        let len = v.len();
        v.insert(0, M::NULL);
        v.resize(v.len() + block_size, M::NULL);
        v.iter_mut().for_each(|c| *c = M::convert_char(*c));
        Self { s: v, len }
    }

    /// Get the byte at a certain index (unchecked).
    #[inline]
    pub unsafe fn get(&self, i: usize) -> u8 {
        *self.s.as_ptr().add(i)
    }

    /// Set the byte at a certain index (unchecked).
    #[inline]
    pub unsafe fn set(&mut self, i: usize, c: u8) {
        *self.s.as_mut_ptr().add(i) = c;
    }

    /// Create a pointer to a specific index.
    #[inline]
    pub unsafe fn as_ptr(&self, i: usize) -> *const u8 {
        self.s.as_ptr().add(i)
    }

    /// Length of the original string (no padding).
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
}

/// Resulting score and alignment end position.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct AlignResult {
    pub score: i32,
    pub query_idx: usize,
    pub reference_idx: usize
}

#[derive(Copy, Clone, PartialEq, Debug)]
enum Direction {
    Right,
    Down,
    Grow
}

#[cfg(test)]
mod tests {
    use crate::scores::*;

    use super::*;

    #[test]
    fn test_no_x_drop() {
        let test_gaps = Gaps { open: -11, extend: -1 };

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAAA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AARA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 0);
        assert_eq!(a.res().score, 11);

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAAA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AAAA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 0);
        assert_eq!(a.res().score, 16);

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAAA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AARA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 0);
        assert_eq!(a.res().score, 11);

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAAA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"RRRR", 16);
        let a = Block::<_, false, false>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 0);
        assert_eq!(a.res().score, -4);

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAAA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AAA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 0);
        assert_eq!(a.res().score, 1);

        let test_gaps2 = Gaps { open: -2, extend: -1 };

        let r = PaddedBytes::from_bytes::<NucMatrix>(b"AAAN", 16);
        let q = PaddedBytes::from_bytes::<NucMatrix>(b"ATAA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &NW1, test_gaps2, 16..=16, 0);
        assert_eq!(a.res().score, 0);

        let r = PaddedBytes::from_bytes::<NucMatrix>(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 16);
        let q = PaddedBytes::from_bytes::<NucMatrix>(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &NW1, test_gaps2, 16..=16, 0);
        assert_eq!(a.res().score, 32);

        let r = PaddedBytes::from_bytes::<NucMatrix>(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 16);
        let q = PaddedBytes::from_bytes::<NucMatrix>(b"TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT", 16);
        let a = Block::<_, false, false>::align(&q, &r, &NW1, test_gaps2, 16..=16, 0);
        assert_eq!(a.res().score, -32);

        let r = PaddedBytes::from_bytes::<NucMatrix>(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 16);
        let q = PaddedBytes::from_bytes::<NucMatrix>(b"TATATATATATATATATATATATATATATATA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &NW1, test_gaps2, 16..=16, 0);
        assert_eq!(a.res().score, 0);

        let r = PaddedBytes::from_bytes::<NucMatrix>(b"TTAAAAAAATTTTTTTTTTTT", 16);
        let q = PaddedBytes::from_bytes::<NucMatrix>(b"TTTTTTTTAAAAAAATTTTTTTTT", 16);
        let a = Block::<_, false, false>::align(&q, &r, &NW1, test_gaps2, 16..=16, 0);
        assert_eq!(a.res().score, 7);

        let r = PaddedBytes::from_bytes::<NucMatrix>(b"AAAA", 16);
        let q = PaddedBytes::from_bytes::<NucMatrix>(b"C", 16);
        let a = Block::<_, false, false>::align(&q, &r, &NW1, test_gaps2, 16..=16, 0);
        assert_eq!(a.res().score, -5);
        let a = Block::<_, false, false>::align(&r, &q, &NW1, test_gaps2, 16..=16, 0);
        assert_eq!(a.res().score, -5);
    }

    #[test]
    fn test_x_drop() {
        let test_gaps = Gaps { open: -11, extend: -1 };

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAARRA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AAAAAA", 16);
        let a = Block::<_, false, true>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 1);
        assert_eq!(a.res(), AlignResult { score: 14, query_idx: 6, reference_idx: 6 });

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAAAAAAAAAAAAAARRRRRRRRRRRRRRRRAAAAAAAAAAAAA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", 16);
        let a = Block::<_, false, true>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 1);
        assert_eq!(a.res(), AlignResult { score: 60, query_idx: 15, reference_idx: 15 });
    }

    #[test]
    fn test_trace() {
        let test_gaps = Gaps { open: -11, extend: -1 };

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAARRA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AAAAAA", 16);
        let a = Block::<_, true, false>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 0);
        let res = a.res();
        assert_eq!(res, AlignResult { score: 14, query_idx: 6, reference_idx: 6 });
        assert_eq!(a.trace().cigar(res.query_idx, res.reference_idx).to_string(), "6M");

        let r = PaddedBytes::from_bytes::<AAMatrix>(b"AAAA", 16);
        let q = PaddedBytes::from_bytes::<AAMatrix>(b"AAA", 16);
        let a = Block::<_, true, false>::align(&q, &r, &BLOSUM62, test_gaps, 16..=16, 0);
        let res = a.res();
        assert_eq!(res, AlignResult { score: 1, query_idx: 3, reference_idx: 4 });
        assert_eq!(a.trace().cigar(res.query_idx, res.reference_idx).to_string(), "3M1D");

        let test_gaps2 = Gaps { open: -2, extend: -1 };

        let r = PaddedBytes::from_bytes::<NucMatrix>(b"TTAAAAAAATTTTTTTTTTTT", 16);
        let q = PaddedBytes::from_bytes::<NucMatrix>(b"TTTTTTTTAAAAAAATTTTTTTTT", 16);
        let a = Block::<_, true, false>::align(&q, &r, &NW1, test_gaps2, 16..=16, 0);
        let res = a.res();
        assert_eq!(res, AlignResult { score: 7, query_idx: 24, reference_idx: 21 });
        assert_eq!(a.trace().cigar(res.query_idx, res.reference_idx).to_string(), "2M6I16M3D");
    }

    #[test]
    fn test_bytes() {
        let test_gaps = Gaps { open: -2, extend: -1 };

        let r = PaddedBytes::from_bytes::<ByteMatrix>(b"AAAaaA", 16);
        let q = PaddedBytes::from_bytes::<ByteMatrix>(b"AAAAAA", 16);
        let a = Block::<_, false, false>::align(&q, &r, &BYTES1, test_gaps, 16..=16, 0);
        assert_eq!(a.res().score, 2);

        let r = PaddedBytes::from_bytes::<ByteMatrix>(b"abcdefg", 16);
        let q = PaddedBytes::from_bytes::<ByteMatrix>(b"abdefg", 16);
        let a = Block::<_, false, false>::align(&q, &r, &BYTES1, test_gaps, 16..=16, 0);
        assert_eq!(a.res().score, 4);
    }
}
