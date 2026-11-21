use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
#[ace(reserved(3, 4))]
struct ReservedConflict {
    #[ace(order = 3)]
    value: u32,
}

fn main() {}
