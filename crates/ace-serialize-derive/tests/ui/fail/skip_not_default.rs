use ace_serialize_derive::AceSerialize;

struct NoDefault(u32);

#[derive(AceSerialize)]
struct Skipped {
    value: u32,
    #[ace(skip)]
    other: NoDefault,
}

fn main() {}
