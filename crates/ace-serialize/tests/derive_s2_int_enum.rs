use ace_serialize::decode::{decode_cyclic, Decoder};
use ace_serialize::error::Error;
use ace_serialize::limits::Options;
use ace_serialize::{decode, encode};
use ace_serialize_derive::AceSerialize;

/// 每个字段槽为 16 字节（FR-W03）；raw 输出的长度差即槽数差。
const SLOT_BYTES: usize = 16;

fn raw_options() -> Options {
    Options {
        packed: false,
        ..Options::default()
    }
}

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(i8)]
enum Signed {
    Low = -2,
    Zero = 0,
    High = 7,
}

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(u8)]
enum Unsigned {
    One = 1,
    Max = 255,
}

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(i32)]
enum Wide {
    Idle = 0,
    Busy = 1,
    Done = 9,
}

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(u64)]
enum Huge {
    Zero = 0,
    Far = 4_294_967_296,
}

/// 未标 repr：FR-T01 规定 wire 按 i32。
#[derive(AceSerialize, Debug, PartialEq)]
enum Unmarked {
    First,
    Second = 4,
}

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(i16)]
#[ace(default = Two)]
enum Defaulted {
    One = 1,
    Two = 2,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct AllReprs {
    signed: Signed,
    unsigned: Unsigned,
    wide: Wide,
    huge: Huge,
    unmarked: Unmarked,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Required {
    value: Wide,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct WithDefault {
    flag: u8,
    state: Defaulted,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct WithVec {
    values: Vec<Wide>,
}

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(i16)]
enum SignedMid {
    Low = -300,
    High = 5,
}

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(i32)]
enum SignedWide {
    Low = -70_000,
    High = 9,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct SignedElements {
    bytes: Vec<Signed>,
    shorts: Vec<SignedMid>,
    words: Vec<SignedWide>,
}

#[test]
fn integer_enum_fields_round_trip_every_repr() {
    let values = [
        AllReprs {
            signed: Signed::Low,
            unsigned: Unsigned::One,
            wide: Wide::Idle,
            huge: Huge::Zero,
            unmarked: Unmarked::First,
        },
        AllReprs {
            signed: Signed::High,
            unsigned: Unsigned::Max,
            wide: Wide::Done,
            huge: Huge::Far,
            unmarked: Unmarked::Second,
        },
    ];
    for value in &values {
        for options in [Options::default(), raw_options()] {
            let bytes = encode(value, &options).unwrap();
            assert_eq!(decode::<AllReprs>(&bytes, &options).unwrap(), *value);
        }
    }
    let bytes = encode(&values[1], &Options::default()).unwrap();
    let mut decoder = Decoder::new(&bytes, &Options::default()).unwrap();
    assert_eq!(decode_cyclic::<AllReprs>(&mut decoder).unwrap(), values[1]);
    decoder.finish().unwrap();
}

#[test]
fn default_variant_is_omitted_and_restored_when_absent() {
    let options = raw_options();
    let omitted = encode(
        &WithDefault {
            flag: 7,
            state: Defaulted::Two,
        },
        &options,
    )
    .unwrap();
    let written = encode(
        &WithDefault {
            flag: 7,
            state: Defaulted::One,
        },
        &options,
    )
    .unwrap();
    assert_eq!(written.len() - omitted.len(), SLOT_BYTES);
    assert_eq!(
        decode::<WithDefault>(&omitted, &options).unwrap().state,
        Defaulted::Two
    );
    assert_eq!(
        decode::<WithDefault>(&written, &options).unwrap().state,
        Defaulted::One
    );
    let mut decoder = Decoder::new(&omitted, &options).unwrap();
    assert_eq!(
        decode_cyclic::<WithDefault>(&mut decoder).unwrap().state,
        Defaulted::Two
    );
    decoder.finish().unwrap();
}

#[test]
fn absent_enum_without_default_reports_missing_required_field() {
    let options = Options::default();
    let mut encoder = ace_serialize::encode::Encoder::new(&options);
    encoder.enter_object().unwrap();
    encoder.leave_object().unwrap();
    let bytes = encoder.finish().unwrap();
    let result: Result<Required, Error> = decode(&bytes, &options);
    assert_eq!(result.unwrap_err(), Error::MissingRequiredField);
    let mut decoder = Decoder::new(&bytes, &options).unwrap();
    assert_eq!(
        decode_cyclic::<Required>(&mut decoder).unwrap_err(),
        Error::MissingRequiredField
    );
}

#[test]
fn unknown_discriminants_are_rejected_without_building_a_value() {
    let options = Options::default();
    for (payload, expected) in [
        (99_i64 as u64, Error::InvalidTag),
        (-1_i64 as u64, Error::InvalidTag),
    ] {
        let mut encoder = ace_serialize::encode::Encoder::new(&options);
        encoder.enter_object().unwrap();
        encoder
            .write_field_scalar(0, payload.to_le_bytes())
            .unwrap();
        encoder.leave_object().unwrap();
        let bytes = encoder.finish().unwrap();
        let result: Result<Required, Error> = decode(&bytes, &options);
        assert_eq!(result.unwrap_err(), expected);
    }
    let mut encoder = ace_serialize::encode::Encoder::new(&options);
    encoder.enter_object().unwrap();
    // u64::MAX 不落在 u8 的判别范围内。
    encoder
        .write_field_scalar(0, u64::MAX.to_le_bytes())
        .unwrap();
    encoder.leave_object().unwrap();
    let bytes = encoder.finish().unwrap();
    let value: Result<UnsignedHolder, Error> = decode(&bytes, &options);
    assert_eq!(value.unwrap_err(), Error::InvalidTag);
}

#[derive(AceSerialize, Debug, PartialEq)]
struct UnsignedHolder {
    value: Unsigned,
}

#[test]
fn integer_enum_elements_round_trip() {
    let value = WithVec {
        values: vec![Wide::Idle, Wide::Done, Wide::Busy],
    };
    for options in [Options::default(), raw_options()] {
        let bytes = encode(&value, &options).unwrap();
        assert_eq!(decode::<WithVec>(&bytes, &options).unwrap(), value);
    }
}

/// 元素路径只写 repr 宽度个字节；有符号 repr 的负值读回时必须按符号位回填高位（复审 I1）。
#[test]
fn signed_enum_elements_round_trip_negative_discriminants() {
    let value = SignedElements {
        bytes: vec![Signed::Low, Signed::Zero, Signed::High],
        shorts: vec![SignedMid::Low, SignedMid::High],
        words: vec![SignedWide::Low, SignedWide::High],
    };
    for options in [Options::default(), raw_options()] {
        let bytes = encode(&value, &options).unwrap();
        assert_eq!(decode::<SignedElements>(&bytes, &options).unwrap(), value);
    }
}
