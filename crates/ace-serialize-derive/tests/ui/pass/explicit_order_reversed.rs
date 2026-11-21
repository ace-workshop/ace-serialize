use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct ReversedOrder {
    #[ace(order = 3)]
    last: u32,
    #[ace(order = 1)]
    first: u32,
}

fn main() {}
