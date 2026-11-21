use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Skipped {
    value: u32,
    #[ace(skip)]
    cache: u64,
}

fn main() {}
