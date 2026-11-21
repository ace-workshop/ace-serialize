use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct MalformedAttr {
    #[ace(order)]
    value: u32,
}

fn main() {}
