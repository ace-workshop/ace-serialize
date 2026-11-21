use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
#[repr(i32)]
#[ace(default = Missing)]
enum Defaulted {
    First,
    Second,
}

fn main() {}
