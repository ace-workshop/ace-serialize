use crate::error::{Error, Result};
use crate::limits::Options;

pub const SLOT_SIZE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub depth: i32,
    pub tag: i32,
    pub payload: [u8; 8],
}

#[derive(Debug)]
pub struct SlotStream<'a> {
    assist: &'a [u8],
}

impl<'a> SlotStream<'a> {
    /// A partial final slot is `TruncatedInput` because its bytes are missing.
    pub fn parse(assist: &'a [u8], opts: &Options) -> Result<Self> {
        if assist.len() > opts.max_raw_bytes {
            return Err(Error::ResourceLimit);
        }
        if !assist.len().is_multiple_of(SLOT_SIZE) {
            return Err(Error::TruncatedInput);
        }
        if assist.len() / SLOT_SIZE > opts.max_slots {
            return Err(Error::ResourceLimit);
        }
        Ok(Self { assist })
    }
    pub fn len(&self) -> usize {
        self.assist.len() / SLOT_SIZE
    }
    pub fn is_empty(&self) -> bool {
        self.assist.is_empty()
    }
    pub fn get(&self, index: usize) -> Option<Slot> {
        let start = index.checked_mul(SLOT_SIZE)?;
        let bytes = self.assist.get(start..start.checked_add(SLOT_SIZE)?)?;
        Some(Slot {
            depth: i32::from_le_bytes(bytes[..4].try_into().unwrap()),
            tag: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            payload: bytes[8..16].try_into().unwrap(),
        })
    }
    pub fn payload_u64(&self, index: usize) -> Option<u64> {
        self.get(index).map(|slot| u64::from_le_bytes(slot.payload))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRange {
    pub offset: u32,
    pub count: u32,
}

impl SlotRange {
    /// Reads the range descriptor without validating its semantics.
    pub fn parse_body(body: &[u8]) -> Result<Self> {
        if body.len() < 8 {
            return Err(Error::TruncatedInput);
        }
        Ok(Self {
            offset: u32::from_le_bytes(body[..4].try_into().unwrap()),
            count: u32::from_le_bytes(body[4..8].try_into().unwrap()),
        })
    }

    /// Mechanically validates the supplied set of known reachable objects in record
    /// order, bounded by the actual assist slot count provided by the caller.
    /// Unknown records do not participate; this layer performs no reachability
    /// analysis. A count is accepted rather than a stream so callers can validate
    /// ranges without retaining the stream; use `SlotStream::len` for that count.
    ///
    /// For each range, checks run in this order: zero offset (`InvalidTag`, since
    /// the root slot cannot belong to an object); checked byte arithmetic
    /// (`ResourceLimit` on overflow); 实际槽边界越界报 `InvalidTag`（需求第 279 行 BAD-03：槽范围越界属非法槽结构）;
    /// `max_slots` (`ResourceLimit`); and non-overlap and ascending input order
    /// (`InvalidTag`). The actual boundary is checked before `max_slots`.
    pub fn validate_ranges(
        ranges: &[(i32, SlotRange)],
        assist_slot_len: usize,
        opts: &Options,
    ) -> Result<()> {
        let mut previous_end = 1usize;
        for &(_, range) in ranges {
            let offset = range.offset as usize;
            let count = range.count as usize;
            if offset == 0 {
                return Err(Error::InvalidTag);
            }
            let start = offset.checked_mul(SLOT_SIZE).ok_or(Error::ResourceLimit)?;
            let size = count.checked_mul(SLOT_SIZE).ok_or(Error::ResourceLimit)?;
            let end = start.checked_add(size).ok_or(Error::ResourceLimit)?;
            let end_slot = end / SLOT_SIZE;
            if end_slot > assist_slot_len {
                return Err(Error::InvalidTag);
            }
            if end_slot > opts.max_slots {
                return Err(Error::ResourceLimit);
            }
            if offset < previous_end {
                return Err(Error::InvalidTag);
            }
            previous_end = end_slot;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_slot() {
        assert_eq!(
            SlotStream::parse(&[0; 15], &Options::default()).unwrap_err(),
            Error::TruncatedInput
        );
    }
    #[test]
    fn read_slot() {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&(-2i32).to_le_bytes());
        b[4..8].copy_from_slice(&123i32.to_le_bytes());
        b[8..].copy_from_slice(&0x0102030405060708u64.to_le_bytes());
        let s = SlotStream::parse(&b, &Options::default()).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(
            s.get(0),
            Some(Slot {
                depth: -2,
                tag: 123,
                payload: [8, 7, 6, 5, 4, 3, 2, 1]
            })
        );
        assert_eq!(s.payload_u64(0), Some(0x0102030405060708));
        assert_eq!(s.get(usize::MAX), None);
    }
    #[test]
    fn slot_limit() {
        let o = Options {
            max_slots: 1,
            ..Options::default()
        };
        assert_eq!(
            SlotStream::parse(&[0; 32], &o).unwrap_err(),
            Error::ResourceLimit
        );
    }
    #[test]
    fn short_range_body() {
        assert_eq!(SlotRange::parse_body(&[0; 7]), Err(Error::TruncatedInput));
    }
    #[test]
    fn root_offset() {
        assert_eq!(
            SlotRange::validate_ranges(
                &[(
                    1,
                    SlotRange {
                        offset: 0,
                        count: 0
                    }
                )],
                usize::MAX,
                &Options::default()
            ),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn overlapping_ranges() {
        assert_eq!(
            SlotRange::validate_ranges(
                &[
                    (
                        1,
                        SlotRange {
                            offset: 1,
                            count: 2
                        }
                    ),
                    (
                        2,
                        SlotRange {
                            offset: 2,
                            count: 1
                        }
                    )
                ],
                usize::MAX,
                &Options::default()
            ),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn descending_ranges() {
        assert_eq!(
            SlotRange::validate_ranges(
                &[
                    (
                        1,
                        SlotRange {
                            offset: 3,
                            count: 0
                        }
                    ),
                    (
                        2,
                        SlotRange {
                            offset: 2,
                            count: 0
                        }
                    )
                ],
                usize::MAX,
                &Options::default()
            ),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn huge_range() {
        assert_eq!(
            SlotRange::validate_ranges(
                &[(
                    1,
                    SlotRange {
                        offset: u32::MAX,
                        count: u32::MAX
                    }
                )],
                usize::MAX,
                &Options::default()
            ),
            Err(Error::ResourceLimit)
        );
    }
    #[test]
    fn range_at_assist_end() {
        let ranges = [(
            1,
            SlotRange {
                offset: 1,
                count: 2,
            },
        )];
        assert_eq!(
            SlotRange::validate_ranges(&ranges, 3, &Options::default()),
            Ok(())
        );
    }
    #[test]
    fn range_one_slot_past_assist_end() {
        let ranges = [(
            1,
            SlotRange {
                offset: 1,
                count: 3,
            },
        )];
        let opts = Options {
            max_slots: 2,
            ..Options::default()
        };
        assert_eq!(
            SlotRange::validate_ranges(&ranges, 3, &opts),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn validate_ranges_with_parsed_slot_stream_len() {
        let assist = [0u8; 3 * SLOT_SIZE];
        let opts = Options::default();
        let stream = SlotStream::parse(&assist, &opts).unwrap();
        let range = SlotRange::parse_body(&[1, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        assert_eq!(
            SlotRange::validate_ranges(&[(1, range)], stream.len(), &opts),
            Ok(())
        );
        assert_eq!(
            SlotRange::validate_ranges(
                &[(
                    1,
                    SlotRange {
                        offset: 2,
                        count: 2
                    }
                )],
                stream.len(),
                &opts
            ),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn valid_and_empty_ranges() {
        assert_eq!(
            SlotRange::parse_body(&[1, 0, 0, 0, 2, 0, 0, 0, 9]),
            Ok(SlotRange {
                offset: 1,
                count: 2
            })
        );
        assert_eq!(
            SlotRange::validate_ranges(
                &[
                    (
                        1,
                        SlotRange {
                            offset: 1,
                            count: 2
                        }
                    ),
                    (
                        2,
                        SlotRange {
                            offset: 3,
                            count: 0
                        }
                    ),
                    (
                        3,
                        SlotRange {
                            offset: 3,
                            count: 1
                        }
                    )
                ],
                usize::MAX,
                &Options::default()
            ),
            Ok(())
        );
    }
}
