use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
#[ace(order = 1)]
struct Misplaced {
    value: u32,
}

fn main() {}
