use ace_serialize_derive::AceSerialize;

/// 类型别名绕过语法检查，仍按解析后的真实类型在编译期诊断（FR-T02、架构 §NotOption）。
type Nested = Option<Option<i32>>;

#[derive(AceSerialize)]
struct Holder {
    value: Nested,
}

fn main() {}
