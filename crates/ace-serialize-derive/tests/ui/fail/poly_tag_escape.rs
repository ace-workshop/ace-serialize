use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
enum Escape {
    #[ace(tag = 65535)]
    Last {
        value: u8,
    },
}

fn main() {}
