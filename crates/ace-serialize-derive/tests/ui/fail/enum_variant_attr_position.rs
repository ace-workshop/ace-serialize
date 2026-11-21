use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
#[repr(i32)]
enum Tagged {
    #[ace(default = One)]
    One,
    Two,
}

fn main() {}
