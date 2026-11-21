use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Item {
    id: u64,
}

#[derive(AceSerialize)]
struct Bag {
    items: Vec<Item>,
}

fn main() {}
