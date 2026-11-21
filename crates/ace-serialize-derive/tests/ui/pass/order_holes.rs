use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Holes {
    #[ace(order = 0)]
    head: u32,
    #[ace(order = 7)]
    tail: u32,
}

fn main() {}
