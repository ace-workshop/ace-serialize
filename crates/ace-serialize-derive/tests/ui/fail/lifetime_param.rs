use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Borrowed<'a> {
    value: &'a str,
}

fn main() {}
