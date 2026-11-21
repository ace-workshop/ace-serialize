use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
#[repr(i8)]
enum Small {
    Low = -1,
    High = 7,
}

#[derive(AceSerialize)]
#[repr(u8)]
enum Tiny {
    Only,
}

#[derive(AceSerialize)]
#[repr(i32)]
enum Medium {
    First,
    Second = 3,
}

#[derive(AceSerialize)]
#[repr(u64)]
enum Large {
    Zero,
    Far = 4_294_967_296,
}

/// 未标 repr：FR-T01 规定 wire 按 i32。
#[derive(AceSerialize)]
enum Unmarked {
    First,
    Second,
}

/// 未标 repr 的超 i32 判别值被拒绝；显式 `#[repr(i64)]` 下同一判别值合法（用户裁决 A，2026-11-14）。
#[derive(AceSerialize)]
#[repr(i64)]
enum WideDiscriminant {
    Zero = 0,
    Big = 4_294_967_296,
}

/// 零变体枚举：Rust 不允许它带整数 repr（E0084），未标 repr 时按 i32 处理。
#[derive(AceSerialize)]
enum Empty {}

#[derive(AceSerialize)]
#[repr(i16)]
#[ace(default = Fallback)]
enum WithDefault {
    Fallback,
    Other = 5,
}

#[derive(AceSerialize)]
struct Holder {
    small: Small,
    tiny: Tiny,
    medium: Medium,
    large: Large,
    wide: WideDiscriminant,
    unmarked: Unmarked,
    with_default: WithDefault,
    optional: Option<WithDefault>,
    empty: Empty,
}

#[derive(AceSerialize)]
struct Recursive {
    small: Small,
    next: Option<Rc<Recursive>>,
}

fn main() {}
