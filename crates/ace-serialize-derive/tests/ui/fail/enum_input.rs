use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
enum Choice {
    First(u32),
    Second,
}

fn main() {}
