use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Generic<T> {
    value: T,
    items: Vec<T>,
}

fn main() {}
