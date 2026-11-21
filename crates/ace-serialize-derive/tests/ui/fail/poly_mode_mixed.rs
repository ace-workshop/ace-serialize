use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
enum Mixed {
    #[ace(tag = 0)]
    Numeric {
        value: u8,
    },
    #[ace(type_name = "Text")]
    Text {
        value: u8,
    },
}

fn main() {}
