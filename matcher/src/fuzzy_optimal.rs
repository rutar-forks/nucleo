use std::cmp::max;

use crate::chars::{Char, CharClass};
use crate::matrix::{MatcherDataView, MatrixCell, ScoreCell};
use crate::score::{
    BONUS_CONSECUTIVE, BONUS_FIRST_CHAR_MULTIPLIER, PENALTY_GAP_EXTENSION, PENALTY_GAP_START,
    SCORE_MATCH,
};
use crate::{Matcher, MatcherConfig};

impl Matcher {
    pub(crate) fn fuzzy_match_optimal<const INDICES: bool, H: Char + PartialEq<N>, N: Char>(
        &mut self,
        haystack: &[H],
        needle: &[N],
        start: usize,
        greedy_end: usize,
        end: usize,
        indices: &mut Vec<u32>,
    ) -> Option<u16> {
        // construct a matrix (and copy the haystack), the matrix and haystack size are bounded
        // to avoid the slow O(mn) time complexity for large inputs. Furthermore, it allows
        // us to treat needle indices as u16
        let Some(mut matrix) = self.slab.alloc(&haystack[start..end], needle.len()) else {
            return self.fuzzy_match_greedy_::<INDICES, H, N>(
                haystack,
                needle,
                start,
                greedy_end,
                indices,
            );
        };

        let prev_class = start
            .checked_sub(1)
            .map(|i| haystack[i].char_class(&self.config))
            .unwrap_or(self.config.initial_char_class);
        let matched = matrix.setup::<INDICES, _>(needle, prev_class, &self.config);
        // this only happened with unicode haystacks, for ASCII the prefilter handles all rejects
        if !matched {
            assert!(
                !N::ASCII || !H::ASCII,
                "should have been caught by prefilter"
            );
            return None;
        }

        // populate the matrix and find the best score
        let matrix_len = matrix.populate_matrix::<INDICES, _>(needle);
        let last_row_off = matrix.row_offs[needle.len() - 1];
        let relative_last_row_off = last_row_off as usize + 1 - needle.len();
        let (match_end, match_score_cell) = matrix.current_row[relative_last_row_off..]
            .iter()
            .enumerate()
            .max_by_key(|(_, cell)| cell.score)
            .expect("there must be atleast one match");
        if INDICES {
            matrix.reconstruct_optimal_path(match_end as u16, indices, matrix_len, start as u32);
        }
        Some(match_score_cell.score as u16)
    }
}

fn next_m_score(p_score: i32, m_score: i32, bonus: u16) -> ScoreCell {
    let consecutive_bonus = max(bonus, BONUS_CONSECUTIVE);
    let score_match = m_score + consecutive_bonus as i32;
    let score_skip = p_score + bonus as i32;
    if score_match > score_skip {
        ScoreCell {
            score: score_match + SCORE_MATCH as i32,
            matched: true,
        }
    } else {
        ScoreCell {
            score: score_skip + SCORE_MATCH as i32,
            matched: false,
        }
    }
}

fn p_score(prev_p_score: i32, prev_m_score: i32) -> (i32, bool) {
    let score_match = if prev_m_score >= 0 {
        (prev_m_score - PENALTY_GAP_START as i32).max(0)
    } else {
        i32::MIN / 2
    };
    let score_skip = if prev_p_score >= 0 {
        (prev_p_score - PENALTY_GAP_EXTENSION as i32).max(0)
    } else {
        i32::MIN / 2
    };
    if score_match > score_skip {
        (score_match, true)
    } else {
        (score_skip, false)
    }
}

impl<H: Char> MatcherDataView<'_, H> {
    fn setup<const INDICES: bool, N: Char>(
        &mut self,
        needle: &[N],
        mut prev_class: CharClass,
        config: &MatcherConfig,
    ) -> bool
    where
        H: PartialEq<N>,
    {
        let mut row_iter = needle.iter().copied().zip(self.row_offs.iter_mut());
        let (mut needle_char, mut row_start) = row_iter.next().unwrap();

        let col_iter = self
            .haystack
            .iter_mut()
            .zip(self.bonus.iter_mut())
            .enumerate();

        let mut matched = false;
        for (i, (c_, bonus_)) in col_iter {
            let (c, class) = c_.char_class_and_normalize(config);
            *c_ = c;

            let bonus = config.bonus_for(prev_class, class);
            // save bonus for later so we don't have to recompute it each time
            *bonus_ = bonus;
            prev_class = class;

            let i = i as u16;
            if c == needle_char {
                // save the first idx of each char
                if let Some(next) = row_iter.next() {
                    *row_start = i;
                    (needle_char, row_start) = next;
                } else if !matched {
                    *row_start = i;
                    // we have atleast one match
                    matched = true;
                }
            }
        }
        if !matched {
            return false;
        }
        debug_assert_eq!(self.row_offs[0], 0);
        Self::score_row::<true, INDICES, _>(
            self.current_row,
            self.matrix_cells,
            self.haystack,
            self.bonus,
            0,
            self.row_offs[1],
            0,
            needle[0],
            needle[1],
        );
        true
    }

    fn score_row<const FIRST_ROW: bool, const INDICES: bool, N: Char>(
        current_row: &mut [ScoreCell],
        matrix_cells: &mut [MatrixCell],
        haystack: &[H],
        bonus: &[u16],
        row_off: u16,
        mut next_row_off: u16,
        needle_idx: u16,
        needle_char: N,
        next_needle_char: N,
    ) where
        H: PartialEq<N>,
    {
        next_row_off -= 1;
        let relative_row_off = row_off - needle_idx;
        let next_relative_row_off = next_row_off - needle_idx;
        let skipped_col_iter = haystack[row_off as usize..next_row_off as usize]
            .iter()
            .zip(bonus[row_off as usize..next_row_off as usize].iter())
            .zip(current_row[relative_row_off as usize..next_relative_row_off as usize].iter_mut())
            .zip(matrix_cells.iter_mut());
        let mut prev_p_score = i32::MIN / 2;
        let mut prev_m_score = i32::MIN / 2;
        for (((&c, bonus), score_cell), matrix_cell) in skipped_col_iter {
            let (p_score, p_matched) = p_score(prev_p_score, prev_m_score);
            let m_cell = if FIRST_ROW {
                if c == needle_char {
                    next_m_score(0, i32::MIN / 2, bonus * BONUS_FIRST_CHAR_MULTIPLIER)
                } else {
                    ScoreCell {
                        score: i32::MIN / 2,
                        matched: false,
                    }
                }
            } else {
                *score_cell
            };
            if INDICES {
                matrix_cell.set(p_matched, m_cell.matched);
            }
            prev_p_score = p_score;
            prev_m_score = m_cell.score;
        }
        let col_iter = haystack[next_row_off as usize..]
            .windows(2)
            .zip(bonus[next_row_off as usize..].windows(2))
            .zip(current_row[next_relative_row_off as usize..].iter_mut())
            .zip(matrix_cells[(next_relative_row_off - relative_row_off) as usize..].iter_mut());
        for (((c, bonus), score_cell), matrix_cell) in col_iter {
            let (p_score, p_matched) = p_score(prev_p_score, prev_m_score);
            let m_cell = if FIRST_ROW {
                if c[0] == needle_char {
                    next_m_score(0, i32::MIN / 2, bonus[0] * BONUS_FIRST_CHAR_MULTIPLIER)
                } else {
                    ScoreCell {
                        score: i32::MIN / 2,
                        matched: false,
                    }
                }
            } else {
                *score_cell
            };
            *score_cell = if c[1] == next_needle_char {
                next_m_score(p_score, m_cell.score, bonus[1])
            } else {
                ScoreCell {
                    score: i32::MIN / 2,
                    matched: false,
                }
            };
            if INDICES {
                matrix_cell.set(p_matched, m_cell.matched);
            }
            prev_p_score = p_score;
            prev_m_score = m_cell.score;
        }
    }

    fn populate_matrix<const INDICES: bool, N: Char>(&mut self, needle: &[N]) -> usize
    where
        H: PartialEq<N>,
    {
        let mut matrix_cells = &mut self.matrix_cells[self.current_row.len()..];
        let mut row_iter = needle[1..]
            .iter()
            .copied()
            .zip(self.row_offs[1..].iter().copied())
            .enumerate();
        let (mut needle_idx, (mut needle_char, mut row_off)) = row_iter.next().unwrap();
        for (next_needle_idx, (next_needle_char, next_row_off)) in row_iter {
            Self::score_row::<false, INDICES, _>(
                self.current_row,
                matrix_cells,
                self.haystack,
                self.bonus,
                row_off,
                next_row_off,
                needle_idx as u16 + 1,
                needle_char,
                next_needle_char,
            );
            let len = self.current_row.len() + needle_idx + 1 - row_off as usize;
            matrix_cells = &mut matrix_cells[len..];
            (needle_idx, needle_char, row_off) = (next_needle_idx, next_needle_char, next_row_off);
        }
        matrix_cells.as_ptr() as usize - self.matrix_cells.as_ptr() as usize
    }

    fn reconstruct_optimal_path(
        &self,
        max_score_end: u16,
        indices: &mut Vec<u32>,
        matrix_len: usize,
        start: u32,
    ) {
        let indices_start = indices.len();
        indices.resize(indices_start + self.row_offs.len(), 0);
        let indices = &mut indices[indices_start..];
        let last_row_off = *self.row_offs.last().unwrap();
        indices[self.row_offs.len() - 1] = start + max_score_end as u32 + last_row_off as u32;

        let mut matrix_cells = &self.matrix_cells[..matrix_len];
        let width = self.current_row.len();
        let mut row_iter = self.row_offs[..self.row_offs.len() - 1]
            .iter()
            .copied()
            .enumerate()
            .rev()
            .map(|(i, off)| {
                let relative_off = off as usize - i;
                let row;
                (matrix_cells, row) =
                    matrix_cells.split_at(matrix_cells.len() - (width - relative_off));
                (i, off, row)
            });
        let (mut row_idx, mut row_off, mut row) = row_iter.next().unwrap();
        let mut col = max_score_end;
        let relative_last_row_off = last_row_off as usize + 1 - self.row_offs.len();
        let mut matched = self.current_row[col as usize + relative_last_row_off].matched;
        col += last_row_off - row_off - 1;
        loop {
            if matched {
                indices[row_idx] = start + col as u32 + row_off as u32;
            }
            let next_matched = row[col as usize].get(matched);
            if matched {
                let Some((next_row_idx, next_row_off, next_row)) = row_iter.next() else{
                    break;
                };
                col += row_off - next_row_off;
                (row_idx, row_off, row) = (next_row_idx, next_row_off, next_row)
            }
            col -= 1;
            matched = next_matched;
        }
    }
}