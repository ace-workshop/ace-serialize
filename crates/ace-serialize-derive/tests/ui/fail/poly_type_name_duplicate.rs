use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
enum DuplicateName {
    #[ace(type_name = "Same")]
    First {
        value: u8,
    },
    #[ace(type_name = "Same")]
    Second {
        value: u8,
    },
}

fn main() {}
