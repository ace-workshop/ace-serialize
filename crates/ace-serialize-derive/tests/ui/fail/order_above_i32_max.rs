use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct LargeOrder {
    #[ace(order = 2147483648)]
    value: u32,
}

fn main() {}
