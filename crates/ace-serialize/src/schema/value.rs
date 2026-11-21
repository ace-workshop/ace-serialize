//! Value-level wire conversions; these functions do not assemble slots or records.

use crate::error::{Error, Result};

macro_rules! integer_codec {
    ($ty:ty, $wire:ty, $encode:ident, $decode:ident, $default:ident) => {
        pub fn $encode(v: $ty) -> [u8; 8] {
            (v as $wire).to_le_bytes()
        }
        pub fn $decode(payload: &[u8; 8]) -> $ty {
            <$wire>::from_le_bytes(*payload) as $ty
        }
        pub fn $default(v: $ty) -> bool {
            v == 0
        }
    };
}
integer_codec!(i8, i64, encode_i8, decode_i8, is_default_i8);
integer_codec!(i16, i64, encode_i16, decode_i16, is_default_i16);
integer_codec!(i32, i64, encode_i32, decode_i32, is_default_i32);
integer_codec!(i64, i64, encode_i64, decode_i64, is_default_i64);
integer_codec!(u8, u64, encode_u8, decode_u8, is_default_u8);
integer_codec!(u16, u64, encode_u16, decode_u16, is_default_u16);
integer_codec!(u32, u64, encode_u32, decode_u32, is_default_u32);
integer_codec!(u64, u64, encode_u64, decode_u64, is_default_u64);

pub fn encode_isize(v: isize) -> [u8; 8] {
    encode_i64(v as i64)
}
/// The wire value is always 8 bytes; values exceeding the target pointer width
/// are invalid and return `Error::InvalidTag`.
pub fn decode_isize(payload: &[u8; 8]) -> Result<isize> {
    isize::try_from(decode_i64(payload)).map_err(|_| Error::InvalidTag)
}
pub fn is_default_isize(v: isize) -> bool {
    v == 0
}
pub fn encode_usize(v: usize) -> [u8; 8] {
    encode_u64(v as u64)
}
/// The wire value is always 8 bytes; values exceeding the target pointer width
/// are invalid and return `Error::InvalidTag`.
pub fn decode_usize(payload: &[u8; 8]) -> Result<usize> {
    usize::try_from(decode_u64(payload)).map_err(|_| Error::InvalidTag)
}
pub fn is_default_usize(v: usize) -> bool {
    v == 0
}

pub fn encode_bool(v: bool) -> [u8; 8] {
    encode_u8(u8::from(v))
}
/// Non-canonical boolean payloads are invalid tags.
pub fn decode_bool(payload: &[u8; 8]) -> Result<bool> {
    match u64::from_le_bytes(*payload) {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::InvalidTag),
    }
}
pub fn is_default_bool(v: bool) -> bool {
    !v
}

pub fn encode_char(v: char) -> [u8; 8] {
    encode_u32(v as u32)
}
/// Validate a raw Unicode scalar before encoding it.
pub fn encode_char_raw(v: u32) -> Result<[u8; 8]> {
    char::from_u32(v)
        .map(encode_char)
        .ok_or(Error::InvalidArgument)
}
pub fn decode_char(payload: &[u8; 8]) -> Result<char> {
    let v = u64::from_le_bytes(*payload);
    let scalar = u32::try_from(v).map_err(|_| Error::InvalidTag)?;
    char::from_u32(scalar).ok_or(Error::InvalidUtf8)
}
pub fn is_default_char(v: char) -> bool {
    v == '\0'
}

pub fn encode_f32(v: f32) -> [u8; 8] {
    encode_u32(v.to_bits())
}
pub fn decode_f32(payload: &[u8; 8]) -> f32 {
    f32::from_bits(decode_u32(payload))
}
pub fn is_default_f32(v: f32) -> bool {
    v.to_bits() == 0
}
pub fn encode_f64(v: f64) -> [u8; 8] {
    v.to_bits().to_le_bytes()
}
pub fn decode_f64(payload: &[u8; 8]) -> f64 {
    f64::from_bits(u64::from_le_bytes(*payload))
}
pub fn is_default_f64(v: f64) -> bool {
    v.to_bits() == 0
}

/// RFC 4122 network-order bytes, not the mixed-endian C# byte-array layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Guid(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub ticks: u64,
    pub kind: Kind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTimeOffset {
    pub clock_ticks: i64,
    pub offset_minutes: i16,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct IntPtr(pub i64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct UIntPtr(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ToolkitsU64(pub u64);
pub fn encode_guid(v: Guid) -> [u8; 16] {
    v.0
}
pub fn decode_guid(bytes: &[u8; 16]) -> Guid {
    Guid(*bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Kind {
    Unspecified = 0,
    Utc = 1,
    Local = 2,
}

const MAX_TICKS: i64 = 3_155_378_975_999_999_999;
const TICKS_MASK: u64 = (1_u64 << 62) - 1;
/// Encode ticks and Kind. Ticks must fit 62 bits and not exceed .NET
/// DateTime.MaxValue.Ticks (3155378975999999999), else InvalidArgument.
pub fn encode_datetime(ticks: u64, kind: Kind) -> Result<u64> {
    if ticks > TICKS_MASK || ticks > MAX_TICKS as u64 {
        return Err(Error::InvalidArgument);
    }
    Ok(ticks | ((kind as u64) << 62))
}
/// Reject ticks above .NET `DateTime.MaxValue.Ticks` (3155378975999999999),
/// consistent with this project's `DateTimeOffset` handling.
pub fn decode_datetime(v: u64) -> Result<(u64, Kind)> {
    let kind = match v >> 62 {
        0 => Kind::Unspecified,
        1 => Kind::Utc,
        2 => Kind::Local,
        _ => return Err(Error::InvalidTag),
    };
    let ticks = v & TICKS_MASK;
    if ticks > MAX_TICKS as u64 {
        return Err(Error::InvalidTag);
    }
    Ok((ticks, kind))
}

fn valid_datetime_offset(ticks: i64, offset: i16) -> bool {
    (0..=MAX_TICKS).contains(&ticks)
        && (-840..=840).contains(&offset)
        && (0..=MAX_TICKS).contains(&(ticks - i64::from(offset) * 600_000_000))
}
pub fn encode_datetime_offset(clock_ticks: i64, offset_minutes: i16) -> Result<[u8; 10]> {
    if !valid_datetime_offset(clock_ticks, offset_minutes) {
        return Err(Error::InvalidArgument);
    }
    let mut bytes = [0; 10];
    bytes[..8].copy_from_slice(&clock_ticks.to_le_bytes());
    bytes[8..].copy_from_slice(&offset_minutes.to_le_bytes());
    Ok(bytes)
}
pub fn decode_datetime_offset(bytes: &[u8; 10]) -> Result<(i64, i16)> {
    let ticks = i64::from_le_bytes(bytes[..8].try_into().unwrap());
    let offset = i16::from_le_bytes([bytes[8], bytes[9]]);
    if !valid_datetime_offset(ticks, offset) {
        return Err(Error::InvalidTag);
    }
    Ok((ticks, offset))
}

pub fn encode_intptr(v: i64) -> [u8; 8] {
    v.to_le_bytes()
}
pub fn decode_intptr(bytes: &[u8; 8]) -> i64 {
    i64::from_le_bytes(*bytes)
}
pub fn encode_uintptr(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}
pub fn decode_uintptr(bytes: &[u8; 8]) -> u64 {
    u64::from_le_bytes(*bytes)
}
/// Toolkits.Guid's signed 64-bit underlying value maps by bit pattern.
pub fn encode_toolkits_guid(v: i64) -> [u8; 8] {
    (v as u64).to_le_bytes()
}
pub fn decode_toolkits_guid(bytes: &[u8; 8]) -> i64 {
    u64::from_le_bytes(*bytes) as i64
}

pub fn encode_str(v: &str) -> &[u8] {
    v.as_bytes()
}
pub fn decode_str(bytes: &[u8]) -> Result<&str> {
    core::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_extensions_and_boundaries() {
        assert_eq!(encode_i8(-1), [255; 8]);
        assert_eq!(encode_i8(1), [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode_u8(1), [1, 0, 0, 0, 0, 0, 0, 0]);
        for (v, bytes) in [
            (i32::MIN, [0, 0, 0, 128, 255, 255, 255, 255]),
            (i32::MAX, [255, 255, 255, 127, 0, 0, 0, 0]),
        ] {
            assert_eq!(encode_i32(v), bytes);
            assert_eq!(decode_i32(&bytes), v);
        }
        assert_eq!(encode_u32(u32::MAX), [255, 255, 255, 255, 0, 0, 0, 0]);
        assert_eq!(decode_u32(&encode_u32(u32::MAX)), u32::MAX);
        assert_eq!(encode_i64(i64::MIN), [0, 0, 0, 0, 0, 0, 0, 128]);
        assert_eq!(decode_i64(&encode_i64(i64::MIN)), i64::MIN);
        assert_eq!(encode_u64(u64::MAX), [255; 8]);
        assert_eq!(decode_u64(&encode_u64(u64::MAX)), u64::MAX);
        assert_eq!(decode_isize(&encode_isize(-123)), Ok(-123));
        assert_eq!(decode_usize(&encode_usize(123)), Ok(123));
    }
    #[test]
    fn pointer_wire_boundaries() {
        for value in [isize::MIN, isize::MAX] {
            assert_eq!(decode_isize(&encode_isize(value)), Ok(value));
        }
        assert_eq!(decode_usize(&encode_usize(usize::MAX)), Ok(usize::MAX));
        // On 32-bit targets these same inputs must return InvalidTag; not tested
        // locally on 32-bit, only the checked conversion code path is covered.
        if isize::BITS == 64 {
            assert_eq!(decode_isize(&encode_i64(i64::MAX)), Ok(isize::MAX));
            assert_eq!(decode_usize(&encode_u64(u64::MAX)), Ok(usize::MAX));
        } else {
            assert_eq!(decode_isize(&encode_i64(i64::MAX)), Err(Error::InvalidTag));
            assert_eq!(decode_usize(&encode_u64(u64::MAX)), Err(Error::InvalidTag));
        }
    }
    #[test]
    fn bool_payloads() {
        assert_eq!(encode_bool(false), [0; 8]);
        assert_eq!(encode_bool(true), [1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_bool(&encode_bool(false)), Ok(false));
        assert_eq!(decode_bool(&encode_bool(true)), Ok(true));
        assert_eq!(decode_bool(&encode_u64(2)), Err(Error::InvalidTag));
        assert_eq!(decode_bool(&encode_u64(256)), Err(Error::InvalidTag));
    }
    #[test]
    fn unicode_scalar_validation() {
        for c in ['\0', '中', '🙂', '\u{10ffff}'] {
            assert_eq!(decode_char(&encode_char(c)), Ok(c));
            assert_eq!(encode_char_raw(c as u32), Ok(encode_char(c)));
        }
        for v in [0xd800, 0x110000] {
            assert_eq!(encode_char_raw(v), Err(Error::InvalidArgument));
            assert_eq!(decode_char(&encode_u32(v)), Err(Error::InvalidUtf8));
        }
        assert_eq!(decode_char(&encode_u64(1 << 32)), Err(Error::InvalidTag));
    }
    #[test]
    fn float_bits_round_trip_and_defaults() {
        for bits in [0, 0x80000000, 0x7f800000, 0xff800000, 0x7fc12345] {
            let v = f32::from_bits(bits);
            assert_eq!(encode_f32(v), (bits as u64).to_le_bytes());
            assert_eq!(decode_f32(&encode_f32(v)).to_bits(), bits);
        }
        for bits in [
            0,
            0x8000000000000000,
            0x7ff0000000000000,
            0xfff0000000000000,
            0x7ff8123456789abc,
        ] {
            let v = f64::from_bits(bits);
            assert_eq!(encode_f64(v), bits.to_le_bytes());
            assert_eq!(decode_f64(&encode_f64(v)).to_bits(), bits);
        }
        assert!(is_default_f32(0.0));
        assert!(!is_default_f32(-0.0));
        assert!(is_default_f64(0.0));
        assert!(!is_default_f64(-0.0));
    }
    #[test]
    fn datetime_kind_and_tick_bounds() {
        for kind in [Kind::Unspecified, Kind::Utc, Kind::Local] {
            let encoded = encode_datetime(42, kind).unwrap();
            assert_eq!(encoded, 42 | ((kind as u64) << 62));
            assert_eq!(decode_datetime(encoded), Ok((42, kind)));
        }
        assert_eq!(decode_datetime(3_u64 << 62), Err(Error::InvalidTag));
        assert_eq!(
            encode_datetime(1_u64 << 62, Kind::Utc),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            encode_datetime(MAX_TICKS as u64 + 1, Kind::Utc),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn datetime_decode_tick_upper_bound() {
        for kind in [Kind::Unspecified, Kind::Utc, Kind::Local] {
            let kind_bits = (kind as u64) << 62;
            assert_eq!(
                decode_datetime(MAX_TICKS as u64 | kind_bits),
                Ok((MAX_TICKS as u64, kind))
            );
            assert_eq!(
                decode_datetime((MAX_TICKS as u64 + 1) | kind_bits),
                Err(Error::InvalidTag)
            );
        }
    }
    #[test]
    fn datetime_offset_bounds() {
        let ticks = 600_000_000_000_i64;
        let mut expected = [0; 10];
        expected[..8].copy_from_slice(&ticks.to_le_bytes());
        expected[8..].copy_from_slice(&60_i16.to_le_bytes());
        assert_eq!(encode_datetime_offset(ticks, 60), Ok(expected));
        assert_eq!(decode_datetime_offset(&expected), Ok((ticks, 60)));
        for (ticks, offset) in [
            (MAX_TICKS + 1, 0_i16),
            (ticks, 841),
            (ticks, -841),
            (0, 1),
            (MAX_TICKS, -1),
            (-1, 0),
        ] {
            assert_eq!(
                encode_datetime_offset(ticks, offset),
                Err(Error::InvalidArgument)
            );
            let mut bytes = [0; 10];
            bytes[..8].copy_from_slice(&ticks.to_le_bytes());
            bytes[8..].copy_from_slice(&offset.to_le_bytes());
            assert_eq!(decode_datetime_offset(&bytes), Err(Error::InvalidTag));
        }
    }
    #[test]
    fn guid_and_pointer_bit_patterns() {
        let bytes = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(encode_guid(Guid(bytes)), bytes);
        assert_eq!(decode_guid(&bytes), Guid(bytes));
        assert_ne!(
            encode_guid(Guid(bytes)),
            [
                0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ]
        );
        assert_eq!(encode_intptr(-1), [255; 8]);
        assert_eq!(decode_intptr(&[255; 8]), -1);
        assert_eq!(encode_uintptr(u64::MAX), [255; 8]);
        assert_eq!(decode_uintptr(&[255; 8]), u64::MAX);
        assert_eq!(encode_toolkits_guid(-1), [255; 8]);
        assert_eq!(decode_toolkits_guid(&[255; 8]), -1);
    }
    #[test]
    fn utf8_and_borrowed_strings() {
        let long = "a".repeat(130);
        for text in ["", "中文🙂", "a\0b", long.as_str()] {
            assert_eq!(decode_str(encode_str(text)), Ok(text));
            assert_eq!(encode_str(text).as_ptr(), text.as_ptr());
        }
        assert_eq!(decode_str(&[0xc0, 0x80]), Err(Error::InvalidUtf8));
    }
}

#[cfg(test)]
mod scalar_type_tests {
    use super::*;

    macro_rules! check_integer {
        ($name:ident, $ty:ty, $encode:ident, $decode:ident, $default:ident) => {
            #[test]
            fn $name() {
                for v in [<$ty>::MIN, 0, 1, <$ty>::MAX] {
                    assert_eq!($decode(&$encode(v)), v);
                    assert_eq!($default(v), v == 0);
                }
            }
        };
    }
    check_integer!(i8_values, i8, encode_i8, decode_i8, is_default_i8);
    check_integer!(i16_values, i16, encode_i16, decode_i16, is_default_i16);
    check_integer!(i32_values, i32, encode_i32, decode_i32, is_default_i32);
    check_integer!(i64_values, i64, encode_i64, decode_i64, is_default_i64);
    check_integer!(u8_values, u8, encode_u8, decode_u8, is_default_u8);
    check_integer!(u16_values, u16, encode_u16, decode_u16, is_default_u16);
    check_integer!(u32_values, u32, encode_u32, decode_u32, is_default_u32);
    check_integer!(u64_values, u64, encode_u64, decode_u64, is_default_u64);
    #[test]
    fn isize_values() {
        for v in [isize::MIN, 0, 1, isize::MAX] {
            assert_eq!(decode_isize(&encode_isize(v)), Ok(v));
            assert_eq!(is_default_isize(v), v == 0);
        }
    }
    #[test]
    fn usize_values() {
        for v in [usize::MIN, 0, 1, usize::MAX] {
            assert_eq!(decode_usize(&encode_usize(v)), Ok(v));
            assert_eq!(is_default_usize(v), v == 0);
        }
    }

    #[test]
    fn datetime_offset_valid_boundaries() {
        for (ticks, offset) in [
            (0, 0),
            (MAX_TICKS, 0),
            (504_000_000_000, 840),
            (MAX_TICKS - 504_000_000_000, -840),
        ] {
            let encoded = encode_datetime_offset(ticks, offset).unwrap();
            assert_eq!(decode_datetime_offset(&encoded), Ok((ticks, offset)));
        }
        assert!(is_default_bool(false));
        assert!(!is_default_bool(true));
        assert!(is_default_char('\0'));
        assert!(!is_default_char('a'));
    }
}

#[cfg(test)]
mod golden_integer_tests {
    use super::*;
    #[test]
    fn integer_min_max_independent_bytes() {
        assert_eq!(encode_i8(i8::MIN), [128, 255, 255, 255, 255, 255, 255, 255]);
        assert_eq!(encode_i8(i8::MAX), [127, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode_i16(i16::MIN), [0, 128, 255, 255, 255, 255, 255, 255]);
        assert_eq!(encode_i16(i16::MAX), [255, 127, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode_i32(i32::MIN), [0, 0, 0, 128, 255, 255, 255, 255]);
        assert_eq!(encode_i32(i32::MAX), [255, 255, 255, 127, 0, 0, 0, 0]);
        assert_eq!(encode_i64(i64::MIN), [0, 0, 0, 0, 0, 0, 0, 128]);
        assert_eq!(
            encode_i64(i64::MAX),
            [255, 255, 255, 255, 255, 255, 255, 127]
        );
        assert_eq!(encode_u8(u8::MIN), [0; 8]);
        assert_eq!(encode_u8(u8::MAX), [255, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode_u16(u16::MIN), [0; 8]);
        assert_eq!(encode_u16(u16::MAX), [255, 255, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode_u32(u32::MIN), [0; 8]);
        assert_eq!(encode_u32(u32::MAX), [255, 255, 255, 255, 0, 0, 0, 0]);
        assert_eq!(encode_u64(u64::MIN), [0; 8]);
        assert_eq!(encode_u64(u64::MAX), [255; 8]);
        if isize::BITS == 64 {
            assert_eq!(encode_isize(isize::MIN), [0, 0, 0, 0, 0, 0, 0, 128]);
            assert_eq!(
                encode_isize(isize::MAX),
                [255, 255, 255, 255, 255, 255, 255, 127]
            );
            assert_eq!(encode_usize(usize::MAX), [255; 8]);
        } else {
            assert_eq!(encode_isize(isize::MIN), [0, 0, 0, 128, 255, 255, 255, 255]);
            assert_eq!(encode_isize(isize::MAX), [255, 255, 255, 127, 0, 0, 0, 0]);
            assert_eq!(encode_usize(usize::MAX), [255, 255, 255, 255, 0, 0, 0, 0]);
        }
        assert_eq!(encode_usize(usize::MIN), [0; 8]);
        assert_eq!(encode_intptr(i64::MIN), [0, 0, 0, 0, 0, 0, 0, 128]);
        assert_eq!(
            encode_intptr(i64::MAX),
            [255, 255, 255, 255, 255, 255, 255, 127]
        );
        assert_eq!(encode_uintptr(u64::MIN), [0; 8]);
        assert_eq!(encode_uintptr(u64::MAX), [255; 8]);
    }
}
