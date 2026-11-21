use ace_serialize_derive::AceSerialize;

#[derive(Default)]
struct AceSkipDefault {
    x: u32,
}

#[derive(AceSerialize)]
struct Holder {
    value: u32,
    #[ace(skip)]
    cache: AceSkipDefault,
}

fn main() {}
