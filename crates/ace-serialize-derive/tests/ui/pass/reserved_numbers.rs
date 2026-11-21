use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
#[ace(reserved(3, 4))]
struct Reserved {
    #[ace(order = 0)]
    kept: u32,
    #[ace(order = 5)]
    later: u32,
}

fn main() {}
