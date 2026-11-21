use ace_serialize_derive::AceSerialize;

/// FR-T02：`Option<Option<T>>` 字段在编译期诊断并定位到字段。
#[derive(AceSerialize)]
struct Nested {
    value: Option<Option<i32>>,
}

fn main() {}
