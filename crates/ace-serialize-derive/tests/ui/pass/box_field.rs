use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Boxed {
    value: Box<u32>,
}

fn main() {}
