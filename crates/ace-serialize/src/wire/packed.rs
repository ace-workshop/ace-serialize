//! Standard Cap'n Proto word packing, independent of envelope parsing.
//!
//! Callers select the format using `Options.packed`: when false, bypass these
//! functions and use raw bytes. Decoding never guesses the format.

use crate::error::{Error, Result};
use crate::limits::Options;
use alloc::vec::Vec;

/// Pack raw bytes after zero-padding the final word. The padded raw length is
/// subject to `max_raw_bytes`, as is the original length.
pub fn pack(raw: &[u8], opts: &Options) -> Result<Vec<u8>> {
    let padded_len = raw.len().checked_add(7).ok_or(Error::ResourceLimit)? & !7;
    if padded_len > opts.max_raw_bytes || padded_len > isize::MAX as usize {
        return Err(Error::ResourceLimit);
    }
    // 不克隆整块 raw：整 word 直接借用 raw，只有末尾不足 8 字节的 word 用栈上补零。补零 word
    // 必含至少一个 0，所以 `ff` 续接段只可能落在 raw 内。
    let mut tail = [0u8; 8];
    let tail_len = raw.len() & 7;
    if tail_len != 0 {
        tail[..tail_len].copy_from_slice(&raw[raw.len() - tail_len..]);
    }
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < padded_len {
        let word = packed_word(raw, &tail, pos);
        let mut tag = 0u8;
        for (i, &byte) in word.iter().enumerate() {
            if byte != 0 {
                tag |= 1 << i;
            }
        }
        out.push(tag);
        for &byte in word {
            if byte != 0 {
                out.push(byte);
            }
        }
        pos += 8;
        if tag == 0 || tag == 0xff {
            let start = pos;
            while pos < padded_len && (pos - start) / 8 < 255 {
                let next = packed_word(raw, &tail, pos);
                if (tag == 0 && next.iter().any(|&b| b != 0)) || (tag == 0xff && next.contains(&0))
                {
                    break;
                }
                pos += 8;
            }
            out.push(((pos - start) / 8) as u8);
            if tag == 0xff {
                out.extend_from_slice(&raw[start..pos]);
            }
        }
    }
    Ok(out)
}

/// `pos` 处的逻辑 word：整 word 借用 `raw`，末尾补零 word 借用栈上 `tail`。
fn packed_word<'a>(raw: &'a [u8], tail: &'a [u8; 8], pos: usize) -> &'a [u8] {
    match raw.get(pos..pos + 8) {
        Some(word) => word,
        None => tail,
    }
}

/// Unpack into padded raw bytes, without interpreting the envelope or trimming.
/// The caller must calculate the logical end from M/A, allow only the 0..7
/// zero bytes required to align that end, and reject any other trailing bytes
/// before passing the trimmed raw to the envelope reader. The raw budget includes
/// padding. This function cannot distinguish padding from other trailing data.
pub fn unpack(packed: &[u8], opts: &Options) -> Result<Vec<u8>> {
    if packed.len() > opts.max_input_bytes {
        return Err(Error::ResourceLimit);
    }
    // Two passes, in this order, keep both error precedence and capacity exact:
    //
    // 1. Scan without allocating. Every truncation check stays per unit and precedes that
    //    unit's `max_raw_bytes`/`isize::MAX` check, exactly as a single filling pass would
    //    order them (a budget error in an earlier unit wins over a later truncation).
    //    `total` is the accumulated expanded length.
    // 2. Reserve exactly `total` with the fallible `try_reserve`, then fill. `total` never
    //    exceeds `max_raw_bytes`/`isize::MAX` here, and no push reallocates past it, so the
    //    reservation cannot raise a growth baseline from which `Vec` doubles: on 32-bit a run
    //    of zero words cannot reach capacity overflow inside `resize` before the length checks.
    //    A failed reservation returns `ResourceLimit` instead of panicking: that code already
    //    covers every length this function cannot materialize, so the failure stays inside the
    //    existing error-code set and successful inputs keep their exact bytes.
    let mut total = 0usize;
    let mut pos = 0;
    while pos < packed.len() {
        let tag = packed[pos];
        pos += 1;
        let count = tag.count_ones() as usize;
        let end = pos.checked_add(count).ok_or(Error::TruncatedInput)?;
        if end > packed.len() {
            return Err(Error::TruncatedInput);
        }
        pos = end;
        let mut extra = 0;
        if tag == 0 || tag == 0xff {
            extra = *packed.get(pos).ok_or(Error::TruncatedInput)? as usize;
            pos += 1;
            if tag == 0xff {
                let end = pos.checked_add(extra * 8).ok_or(Error::TruncatedInput)?;
                if end > packed.len() {
                    return Err(Error::TruncatedInput);
                }
                pos = end;
            }
        }
        total = total
            .checked_add((extra + 1) * 8)
            .ok_or(Error::ResourceLimit)?;
        if total > opts.max_raw_bytes || total > isize::MAX as usize {
            return Err(Error::ResourceLimit);
        }
    }

    let mut out = Vec::new();
    out.try_reserve(total).map_err(|_| Error::ResourceLimit)?;
    let mut pos = 0;
    while pos < packed.len() {
        let tag = packed[pos];
        pos += 1;
        let count = tag.count_ones() as usize;
        let end = pos.checked_add(count).ok_or(Error::TruncatedInput)?;
        let body = packed.get(pos..end).ok_or(Error::TruncatedInput)?;
        pos = end;
        let mut word = [0u8; 8];
        let mut index = 0;
        for (i, byte) in word.iter_mut().enumerate() {
            if tag & (1 << i) != 0 {
                *byte = body[index];
                index += 1;
            }
        }
        let mut extra = 0;
        let mut run = &packed[0..0];
        if tag == 0 || tag == 0xff {
            extra = *packed.get(pos).ok_or(Error::TruncatedInput)? as usize;
            pos += 1;
            if tag == 0xff {
                let end = pos.checked_add(extra * 8).ok_or(Error::TruncatedInput)?;
                run = packed.get(pos..end).ok_or(Error::TruncatedInput)?;
                pos = end;
            }
        }
        let new_len = out
            .len()
            .checked_add((extra + 1) * 8)
            .ok_or(Error::ResourceLimit)?;
        if new_len > opts.max_raw_bytes || new_len > isize::MAX as usize {
            return Err(Error::ResourceLimit);
        }
        out.extend_from_slice(&word);
        if tag == 0 {
            out.resize(new_len, 0);
        }
        if tag == 0xff {
            out.extend_from_slice(run);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn single_zero_word() {
        assert_eq!(pack(&[0; 8], &Options::default()).unwrap(), [0, 0]);
        assert_eq!(unpack(&[0, 0], &Options::default()).unwrap(), [0; 8]);
    }
    #[test]
    fn two_zero_words() {
        assert_eq!(pack(&[0; 16], &Options::default()).unwrap(), [0, 1]);
        assert_eq!(unpack(&[0, 1], &Options::default()).unwrap(), [0; 16]);
    }
    #[test]
    fn bitmap_word() {
        assert_eq!(
            pack(&[1, 0, 0, 0, 0, 0, 0, 0], &Options::default()).unwrap(),
            [1, 1]
        );
        assert_eq!(
            unpack(&[1, 1], &Options::default()).unwrap(),
            [1, 0, 0, 0, 0, 0, 0, 0]
        );
    }
    #[test]
    fn asymmetric_bitmap_word() {
        let raw = [0, 1, 0, 2, 0, 3, 0, 4];
        let packed = [0xaa, 1, 2, 3, 4];
        assert_eq!(pack(&raw, &Options::default()).unwrap(), packed);
        assert_eq!(unpack(&packed, &Options::default()).unwrap(), raw);
    }
    #[test]
    fn ff_run_accepts_zero_bytes() {
        let word = [0, 1, 0, 2, 3, 0, 4, 5];
        let mut packed = vec![0xff];
        packed.extend_from_slice(&word);
        packed.push(1);
        packed.extend_from_slice(&word);
        assert_eq!(
            unpack(&packed, &Options::default()).unwrap(),
            word.repeat(2)
        );
    }
    #[test]
    fn single_nonzero_word() {
        let mut expected = vec![0xff];
        expected.extend_from_slice(&[0xaa; 8]);
        expected.push(0);
        assert_eq!(pack(&[0xaa; 8], &Options::default()).unwrap(), expected);
        assert_eq!(unpack(&expected, &Options::default()).unwrap(), [0xaa; 8]);
    }
    #[test]
    fn nonzero_extra_run() {
        let mut expected = vec![0xff];
        expected.extend_from_slice(&[0xaa; 8]);
        expected.push(2);
        expected.extend_from_slice(&[0xaa; 16]);
        assert_eq!(pack(&[0xaa; 24], &Options::default()).unwrap(), expected);
        assert_eq!(unpack(&expected, &Options::default()).unwrap(), [0xaa; 24]);
    }
    #[test]
    fn nonzero_run_boundary_and_split() {
        for words in [256, 257] {
            let raw = vec![0xaa; words * 8];
            let mut expected = vec![0xff];
            expected.extend_from_slice(&[0xaa; 8]);
            expected.push(255);
            expected.extend_from_slice(&vec![0xaa; 255 * 8]);
            if words == 257 {
                expected.push(0xff);
                expected.extend_from_slice(&[0xaa; 8]);
                expected.push(0);
            }
            assert_eq!(pack(&raw, &Options::default()).unwrap(), expected);
            assert_eq!(unpack(&expected, &Options::default()).unwrap(), raw);
        }
    }
    #[test]
    fn zero_run_boundary_and_split() {
        let raw = vec![0; 257 * 8];
        assert_eq!(pack(&raw, &Options::default()).unwrap(), [0, 255, 0, 0]);
        assert_eq!(unpack(&[0, 255, 0, 0], &Options::default()).unwrap(), raw);
    }
    #[test]
    fn partial_word_is_padded() {
        let raw = [1; 13];
        let decoded = unpack(
            &pack(&raw, &Options::default()).unwrap(),
            &Options::default(),
        )
        .unwrap();
        assert_eq!(decoded.len(), 16);
        assert_eq!(&decoded[..13], &raw);
        assert_eq!(&decoded[13..], &[0; 3]);
    }
    #[test]
    fn csharp_single_zero_is_truncated() {
        assert_eq!(
            unpack(&[0], &Options::default()),
            Err(Error::TruncatedInput)
        );
    }
    #[test]
    fn missing_nonzero_byte_is_truncated() {
        assert_eq!(
            unpack(&[3, 1], &Options::default()),
            Err(Error::TruncatedInput)
        );
    }
    #[test]
    fn missing_ff_word_is_truncated() {
        assert_eq!(
            unpack(&[0xff, 1, 2], &Options::default()),
            Err(Error::TruncatedInput)
        );
    }
    #[test]
    fn missing_ff_count_is_truncated() {
        let mut input = vec![0xff];
        input.extend_from_slice(&[1; 8]);
        assert_eq!(
            unpack(&input, &Options::default()),
            Err(Error::TruncatedInput)
        );
    }
    #[test]
    fn missing_ff_run_body_is_truncated() {
        let mut input = vec![0xff];
        input.extend_from_slice(&[1; 8]);
        input.push(1);
        input.extend_from_slice(&[1; 7]);
        assert_eq!(
            unpack(&input, &Options::default()),
            Err(Error::TruncatedInput)
        );
    }
    #[test]
    fn every_strict_prefix_ends_at_a_unit_boundary_or_is_truncated() {
        let mut state = 1234567u32;
        let random: Vec<u8> = (0..1000)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        let mut vectors: Vec<Vec<u8>> = vec![
            vec![0],
            vec![3, 1],
            vec![0xff, 1, 2],
            [vec![0xff], vec![1; 8]].concat(),
            [vec![0xff], vec![1; 8], vec![1], vec![1; 7]].concat(),
        ];
        for raw in [
            vec![0; 8],
            vec![0; 16],
            vec![1, 0, 0, 0, 0, 0, 0, 0],
            vec![0xaa; 8],
            vec![0xaa; 24],
            vec![0xaa; 256 * 8],
            vec![0xaa; 257 * 8],
            vec![0; 257 * 8],
            vec![1; 13],
            random,
            vec![0, 1, 0, 2, 0, 3, 0, 4],
        ] {
            vectors.push(pack(&raw, &Options::default()).unwrap());
        }
        let word = [0, 1, 0, 2, 3, 0, 4, 5];
        vectors.push([vec![0xff], word.to_vec(), vec![1], word.to_vec()].concat());

        for packed in vectors {
            // Parse unit lengths independently; count bytes belong to special tags,
            // and an ff run body is part of the same unit, not a new boundary.
            let mut boundaries = vec![(0, 0)];
            let (mut pos, mut words) = (0, 0);
            while pos < packed.len() {
                let tag = packed[pos];
                pos += 1;
                pos += tag.count_ones() as usize;
                if pos > packed.len() {
                    break;
                }
                let mut extra = 0;
                if tag == 0 || tag == 0xff {
                    if pos == packed.len() {
                        break;
                    }
                    extra = packed[pos] as usize;
                    pos += 1;
                    if tag == 0xff {
                        pos += extra * 8;
                    }
                    if pos > packed.len() {
                        break;
                    }
                }
                words += 1 + extra;
                boundaries.push((pos, words));
            }
            for k in 0..packed.len() {
                let result = unpack(&packed[..k], &Options::default());
                if k == 0 {
                    // Empty input is a valid empty stream, not a truncated unit.
                    assert_eq!(result.unwrap(), []);
                } else if let Some((_, words)) = boundaries.iter().find(|&&(end, _)| end == k) {
                    // A complete unit boundary is a valid stream of whole words.
                    assert_eq!(result.unwrap().len(), words * 8, "prefix {k}");
                } else {
                    assert_eq!(result, Err(Error::TruncatedInput), "prefix {k}");
                }
            }
        }
    }
    #[test]
    fn raw_budget_includes_padding_and_runs() {
        let opts = Options {
            max_raw_bytes: 13,
            ..Options::default()
        };
        assert_eq!(pack(&[1; 13], &opts), Err(Error::ResourceLimit));
        assert_eq!(unpack(&[0, 1], &opts), Err(Error::ResourceLimit));
        let packed = pack(&[1; 13], &Options::default()).unwrap();
        for limit in [15, 16, 17] {
            let opts = Options {
                max_raw_bytes: limit,
                ..Options::default()
            };
            let expected = if limit == 15 {
                Err(Error::ResourceLimit)
            } else {
                Ok([vec![1; 13], vec![0; 3]].concat())
            };
            assert_eq!(unpack(&packed, &opts), expected);
            assert_eq!(
                pack(&[1; 13], &opts),
                if limit == 15 {
                    Err(Error::ResourceLimit)
                } else {
                    Ok(packed.clone())
                }
            );
        }
        for limit in [7, 8, 9] {
            let opts = Options {
                max_raw_bytes: limit,
                ..Options::default()
            };
            let expected = if limit == 7 {
                Err(Error::ResourceLimit)
            } else {
                Ok(vec![0; 8])
            };
            assert_eq!(unpack(&[0, 0], &opts), expected);
        }
        assert_eq!(
            unpack(
                &[0, 0],
                &Options {
                    max_raw_bytes: 7,
                    ..opts
                }
            ),
            Err(Error::ResourceLimit)
        );
    }
    #[test]
    fn error_precedence_matches_single_pass() {
        // A budget error in an earlier unit wins over a truncation in a later unit: the scan
        // checks each unit's expansion before moving on.
        let tight = Options {
            max_raw_bytes: 7,
            ..Options::default()
        };
        assert_eq!(unpack(&[0, 0, 3], &tight), Err(Error::ResourceLimit));
        // The later unit's budget error is likewise reported after the earlier units are fine.
        let fifteen = Options {
            max_raw_bytes: 15,
            ..Options::default()
        };
        assert_eq!(unpack(&[0, 0, 0, 0], &fifteen), Err(Error::ResourceLimit));
        assert_eq!(unpack(&[0, 0, 0, 0], &tight), Err(Error::ResourceLimit));
        // Within one unit the truncation check precedes its budget check: the ff run body is
        // short even though 2 words would exceed `max_raw_bytes`.
        let mut short_run = vec![0xff];
        short_run.extend_from_slice(&[1; 8]);
        short_run.push(1);
        short_run.extend_from_slice(&[1; 7]);
        assert_eq!(unpack(&short_run, &tight), Err(Error::TruncatedInput));
        // Missing word body arguments are truncated before any budget is consulted.
        assert_eq!(unpack(&[3, 1], &tight), Err(Error::TruncatedInput));
        assert_eq!(unpack(&[0], &tight), Err(Error::TruncatedInput));
    }

    #[test]
    fn input_budget_is_independent() {
        let opts = Options {
            max_input_bytes: 1,
            ..Options::default()
        };
        assert_eq!(unpack(&[0, 0], &opts), Err(Error::ResourceLimit));
        for limit in [2, 3] {
            let opts = Options {
                max_input_bytes: limit,
                ..Options::default()
            };
            assert_eq!(unpack(&[0, 0], &opts), Ok(vec![0; 8]));
        }
    }
    #[test]
    fn large_pseudorandom_round_trip() {
        let mut state = 1234567u32;
        let raw: Vec<u8> = (0..1000)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        assert_eq!(
            unpack(
                &pack(&raw, &Options::default()).unwrap(),
                &Options::default()
            )
            .unwrap(),
            raw
        );
    }
}
