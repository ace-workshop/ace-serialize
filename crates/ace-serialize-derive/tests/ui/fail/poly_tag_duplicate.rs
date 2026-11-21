use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
enum Duplicate {
    #[ace(tag = 1)]
    First {
        value: u8,
    },
    #[ace(tag = 1)]
    Second {
        value: u8,
    },
}

fn main() {}
