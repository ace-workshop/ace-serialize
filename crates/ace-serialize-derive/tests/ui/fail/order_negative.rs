use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct NegativeOrder {
    #[ace(order = -1)]
    value: u32,
}

fn main() {}
