use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Inner {
    value: u32,
}

#[derive(AceSerialize)]
struct Outer {
    inner: Inner,
    count: i32,
}

fn main() {}
