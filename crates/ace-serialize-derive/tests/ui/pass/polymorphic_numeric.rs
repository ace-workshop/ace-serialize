use ace_serialize_derive::AceSerialize;

/// 数值 tag 多态枚举：单元变体与带载荷变体（AC-D01 compile-pass）。
#[derive(AceSerialize)]
enum Numeric {
    #[ace(tag = 0)]
    Idle,
    #[ace(tag = 7)]
    Busy {
        id: u32,
        label: String,
    },
    #[ace(tag = 65534)]
    Last {
        value: u8,
    },
}

#[derive(AceSerialize)]
struct Holder {
    event: Numeric,
    maybe: Option<Numeric>,
    list: Vec<Numeric>,
}

fn main() {}
