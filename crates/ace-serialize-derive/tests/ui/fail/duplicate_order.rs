use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Duplicate {
    #[ace(order = 1)]
    first: u32,
    #[ace(order = 1)]
    second: u32,
}

fn main() {}
