use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Unknown {
    #[ace(rename = "other")]
    value: u32,
}

fn main() {}
