use ace_serialize_derive::AceSerialize;

mod shadow {
    pub struct Ok;
    pub struct Err;
    pub struct Some;
    pub struct None;
    pub struct Default;
}
use shadow::{Default, Err, None, Ok, Some};

trait FromOther {
    fn from(value: u32) -> u64;
}

impl FromOther for u64 {
    fn from(value: u32) -> u64 {
        value as u64 + 1000
    }
}

#[derive(AceSerialize)]
struct ShadowedNames {
    value: Option<u32>,
    count: u32,
    #[ace(skip)]
    cache: u32,
}

/// I1 回归：`ace_decode_into` 的形参不再叫 `out`。作用域内的 `const out` 会遮蔽同名形参（形参被
/// 解析成该常量 → `expected &mut ShadowedNames, found u8`），故生成的形参带私有前缀。
const out: u8 = 3;

/// 多态 enum 的 `ace_decode_into` 同样生成形参，故另在模块内声明同名常量复验枚举路径
/// （上方 `FromOther` 与 `u64::from` 在本模块会歧义，故分模块）。
mod poly {
    use ace_serialize_derive::AceSerialize;

    const out: u8 = 3;

    #[derive(AceSerialize)]
    enum Shadowed {
        #[ace(tag = 0)]
        Idle,
        #[ace(tag = 1)]
        Value { value: u32 },
    }

    fn _uses() {
        let _ = out;
    }
}

fn main() {
    let _ = <u64 as FromOther>::from(1);
    let _ = out;
}
