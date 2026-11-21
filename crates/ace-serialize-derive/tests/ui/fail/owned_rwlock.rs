use ace_serialize_derive::AceSerialize;
use std::sync::RwLock;

#[derive(AceSerialize)]
struct OwnedLock {
    value: RwLock<u32>,
}

fn main() {}
