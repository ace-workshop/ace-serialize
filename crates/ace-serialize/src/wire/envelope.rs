use crate::error::{Error, Result};
use crate::limits::Options;
use alloc::vec::Vec;

// Length checks are separate so oversized inputs can be tested without allocating them.
fn encoded_len(main: usize, assist: usize, budget: usize) -> Result<usize> {
    // 依次检查单项 i32 上限、checked_add、Vec 的 isize 上限和 raw 预算。
    if main > i32::MAX as usize || assist > i32::MAX as usize {
        return Err(Error::ResourceLimit);
    }
    let total = main
        .checked_add(assist)
        .and_then(|n| n.checked_add(8))
        .ok_or(Error::ResourceLimit)?;
    if total > isize::MAX as usize || total > budget {
        return Err(Error::ResourceLimit);
    }
    Ok(total)
}

/// 将两个区写成小端 raw envelope；长度或 raw 预算超限返回 `ResourceLimit`。
/// raw 预算计整个 envelope，与解码时累计 record 的物化预算不同。
pub fn write_envelope(main: &[u8], assist: &[u8], opts: &Options) -> Result<Vec<u8>> {
    let total = encoded_len(main.len(), assist.len(), opts.max_raw_bytes)?;
    let mut raw = Vec::with_capacity(total);
    raw.extend_from_slice(&(main.len() as i32).to_le_bytes());
    raw.extend_from_slice(main);
    raw.extend_from_slice(&(assist.len() as i32).to_le_bytes());
    raw.extend_from_slice(assist);
    Ok(raw)
}

/// 从 raw envelope 借用两个区，返回切片的生命周期与 `raw` 相同。
/// raw 预算超限返回 `ResourceLimit`；长度无效或尾部多余返回 `TruncatedInput`。
/// 此处只计 raw 字节，不计后续 record 物化预算。
pub fn read_envelope<'a>(raw: &'a [u8], opts: &Options) -> Result<(&'a [u8], &'a [u8])> {
    if raw.len() > opts.max_raw_bytes {
        return Err(Error::ResourceLimit);
    }
    if raw.len() < 8 {
        return Err(Error::TruncatedInput);
    }
    let main_len = i32::from_le_bytes(raw[..4].try_into().unwrap());
    let main_len = usize::try_from(main_len).map_err(|_| Error::TruncatedInput)?;
    let main_end = 4usize.checked_add(main_len).ok_or(Error::TruncatedInput)?;
    let assist_start = main_end.checked_add(4).ok_or(Error::TruncatedInput)?;
    if assist_start > raw.len() {
        return Err(Error::TruncatedInput);
    }
    let assist_len = i32::from_le_bytes(raw[main_end..assist_start].try_into().unwrap());
    let assist_len = usize::try_from(assist_len).map_err(|_| Error::TruncatedInput)?;
    let end = assist_start
        .checked_add(assist_len)
        .ok_or(Error::TruncatedInput)?;
    if end != raw.len() {
        return Err(Error::TruncatedInput);
    }
    Ok((&raw[4..main_end], &raw[assist_start..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_round_trip() {
        let opts = Options::default();
        let raw = write_envelope(&[], &[], &opts).unwrap();
        assert_eq!(&raw, &[0; 8]);
        assert_eq!(read_envelope(&raw, &opts), Ok((&[][..], &[][..])));
    }

    #[test]
    fn both_nonempty_round_trip() {
        let opts = Options::default();
        let raw = write_envelope(b"abc", b"de", &opts).unwrap();
        assert_eq!(raw, [3, 0, 0, 0, b'a', b'b', b'c', 2, 0, 0, 0, b'd', b'e']);
        let (main, assist) = read_envelope(&raw, &opts).unwrap();
        assert_eq!((main, assist), (&b"abc"[..], &b"de"[..]));
        assert_eq!(main.as_ptr(), raw[4..].as_ptr());
        assert_eq!(assist.as_ptr(), raw[11..].as_ptr());
    }

    #[test]
    fn one_empty_round_trip() {
        let opts = Options::default();
        for (main, assist) in [(&b"abc"[..], &b""[..]), (&b""[..], &b"abc"[..])] {
            let raw = write_envelope(main, assist, &opts).unwrap();
            assert_eq!(read_envelope(&raw, &opts), Ok((main, assist)));
        }
    }

    #[test]
    fn malformed_lengths_are_truncated() {
        let opts = Options::default();
        assert_eq!(read_envelope(&[0; 7], &opts), Err(Error::TruncatedInput));
        assert_eq!(
            read_envelope(&[1, 0, 0, 0, 0, 0, 0, 0], &opts),
            Err(Error::TruncatedInput)
        );
        assert_eq!(
            read_envelope(&[0, 0, 0, 0, 1, 0, 0, 0], &opts),
            Err(Error::TruncatedInput)
        );
        assert_eq!(
            read_envelope(&[255, 255, 255, 255, 0, 0, 0, 0], &opts),
            Err(Error::TruncatedInput)
        );
        assert_eq!(
            read_envelope(&[0, 0, 0, 0, 255, 255, 255, 255], &opts),
            Err(Error::TruncatedInput)
        );
    }

    #[test]
    fn trailing_bytes_are_truncated() {
        assert_eq!(
            read_envelope(&[0; 9], &Options::default()),
            Err(Error::TruncatedInput)
        );
    }

    #[test]
    fn huge_lengths_exceed_budget() {
        // 源级 checked_add 已覆盖；真实 32 位执行未实测，这些用例不声称触发加法溢出。
        assert_eq!(
            encoded_len(i32::MAX as usize, 0, Options::default().max_raw_bytes),
            Err(Error::ResourceLimit)
        );
        assert_eq!(
            encoded_len(i32::MAX as usize + 1, 0, usize::MAX),
            Err(Error::ResourceLimit)
        );
        assert_eq!(
            encoded_len(0, i32::MAX as usize + 1, usize::MAX),
            Err(Error::ResourceLimit)
        );
    }

    #[test]
    fn vec_capacity_boundary_without_allocation() {
        // 本机 64 位按 isize::MAX 判断：此长度在界内；32 位执行未实测，
        // 相同输入按 isize::MAX 判断将超限。单项 i32 限制使 64 位无法到达超限分支。
        let result = encoded_len(1 << 30, 1 << 30, usize::MAX);
        if usize::BITS == 32 {
            assert_eq!(result, Err(Error::ResourceLimit));
        } else {
            assert_eq!(result, Ok((1usize << 31) + 8));
        }
    }

    #[test]
    fn raw_budget_is_independent_of_other_budgets() {
        let mut opts = Options {
            max_input_bytes: 1,
            max_raw_bytes: 8,
            max_decoded_bytes: 1,
            ..Options::default()
        };
        let raw = [1, 0, 0, 0, b'x', 0, 0, 0, 0];
        assert_eq!(write_envelope(b"x", &[], &opts), Err(Error::ResourceLimit));
        assert_eq!(read_envelope(&raw, &opts), Err(Error::ResourceLimit));
        opts.max_decoded_bytes = usize::MAX;
        assert_eq!(write_envelope(b"x", &[], &opts), Err(Error::ResourceLimit));
        assert_eq!(read_envelope(&raw, &opts), Err(Error::ResourceLimit));
        opts.max_raw_bytes = raw.len();
        opts.max_decoded_bytes = 1;
        assert_eq!(write_envelope(b"x", &[], &opts), Ok(raw.to_vec()));
        assert_eq!(read_envelope(&raw, &opts), Ok((&b"x"[..], &[][..])));
    }

    #[test]
    fn budget_limits_read_and_write() {
        let opts = Options {
            max_raw_bytes: 8,
            ..Options::default()
        };
        assert_eq!(write_envelope(b"x", &[], &opts), Err(Error::ResourceLimit));
        assert_eq!(read_envelope(&[0; 9], &opts), Err(Error::ResourceLimit));
    }
}
