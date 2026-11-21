use ace_serialize_derive::AceSerialize;
use std::cell::RefCell;

#[derive(AceSerialize)]
struct OwnedCell {
    value: RefCell<u32>,
}

fn main() {}
