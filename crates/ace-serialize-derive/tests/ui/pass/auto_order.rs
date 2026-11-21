use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct AutoOrder {
    flag: bool,
    count: Option<i32>,
    name: String,
}

fn main() {}
