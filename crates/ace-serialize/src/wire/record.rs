use crate::error::{Error, Result};
use crate::limits::Options;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordSpan {
    pub inst_id: i32,
    pub size: i32,
    pub body_start: usize,
    pub body_end: usize,
}

#[derive(Debug)]
pub struct RecordIndex {
    records: Vec<RecordSpan>,
    by_id: BTreeMap<i32, usize>,
}

impl RecordIndex {
    /// Indexes record boundaries without interpreting bodies. A negative size is
    /// `TruncatedInput`, consistent with invalid envelope lengths.
    pub fn parse(main: &[u8], opts: &Options) -> Result<Self> {
        if main.len() > opts.max_raw_bytes {
            return Err(Error::ResourceLimit);
        }
        let mut index = Self {
            records: Vec::new(),
            by_id: BTreeMap::new(),
        };
        let mut pos = 0usize;
        while pos < main.len() {
            let body_start = pos.checked_add(8).ok_or(Error::TruncatedInput)?;
            if body_start > main.len() {
                return Err(Error::TruncatedInput);
            }
            let inst_id = i32::from_le_bytes(main[pos..pos + 4].try_into().unwrap());
            let size = i32::from_le_bytes(main[pos + 4..body_start].try_into().unwrap());
            let body_size = usize::try_from(size).map_err(|_| Error::TruncatedInput)?;
            let body_end = body_start
                .checked_add(body_size)
                .ok_or(Error::TruncatedInput)?;
            if body_end > main.len() {
                return Err(Error::TruncatedInput);
            }
            if inst_id <= 0 || index.by_id.contains_key(&inst_id) {
                return Err(Error::InvalidReference);
            }
            if index.records.len() >= opts.max_records {
                return Err(Error::ResourceLimit);
            }
            index.by_id.insert(inst_id, index.records.len());
            index.records.push(RecordSpan {
                inst_id,
                size,
                body_start,
                body_end,
            });
            pos = body_end;
        }
        Ok(index)
    }

    pub fn get(&self, id: i32) -> Option<&RecordSpan> {
        self.by_id.get(&id).map(|&index| &self.records[index])
    }
    pub fn iter(&self) -> impl Iterator<Item = &RecordSpan> {
        self.records.iter()
    }
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: i32, body: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&(body.len() as i32).to_le_bytes());
        b.extend_from_slice(body);
        b
    }
    #[test]
    fn single_record() {
        let bytes = record(1, b"x");
        let idx = RecordIndex::parse(&bytes, &Options::default()).unwrap();
        assert_eq!(idx.len(), 1);
        assert_eq!(
            idx.get(1),
            Some(&RecordSpan {
                inst_id: 1,
                size: 1,
                body_start: 8,
                body_end: 9
            })
        );
        assert_eq!(idx.get(2), None);
    }
    #[test]
    fn multiple_records() {
        let mut bytes = record(1, b"abc");
        bytes.extend(record(3, b""));
        let idx = RecordIndex::parse(&bytes, &Options::default()).unwrap();
        assert_eq!(
            idx.iter()
                .map(|r| (r.inst_id, r.body_start, r.body_end))
                .collect::<Vec<_>>(),
            [(1, 8, 11), (3, 19, 19)]
        );
    }
    #[test]
    fn zero_id() {
        assert_eq!(
            RecordIndex::parse(&record(0, b""), &Options::default()).unwrap_err(),
            Error::InvalidReference
        );
    }
    #[test]
    fn duplicate_id() {
        let mut b = record(1, b"");
        b.extend(record(1, b""));
        assert_eq!(
            RecordIndex::parse(&b, &Options::default()).unwrap_err(),
            Error::InvalidReference
        );
    }
    #[test]
    fn negative_size() {
        let mut b = record(1, b"");
        b[4..8].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            RecordIndex::parse(&b, &Options::default()).unwrap_err(),
            Error::TruncatedInput
        );
    }
    #[test]
    fn truncated_header() {
        assert_eq!(
            RecordIndex::parse(&[0; 7], &Options::default()).unwrap_err(),
            Error::TruncatedInput
        );
    }
    #[test]
    fn truncated_body() {
        let mut b = record(1, b"");
        b[4] = 1;
        assert_eq!(
            RecordIndex::parse(&b, &Options::default()).unwrap_err(),
            Error::TruncatedInput
        );
    }
    #[test]
    fn record_limit() {
        let mut b = record(1, b"");
        b.extend(record(2, b""));
        let o = Options {
            max_records: 1,
            ..Options::default()
        };
        assert_eq!(
            RecordIndex::parse(&b, &o).unwrap_err(),
            Error::ResourceLimit
        );
    }
}
