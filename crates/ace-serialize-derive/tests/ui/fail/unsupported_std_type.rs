use ace_serialize_derive::AceSerialize;
use std::collections::{BTreeSet, HashMap};

#[derive(AceSerialize)]
struct UnsupportedStdType {
    wide: u128,
    map: Option<HashMap<u32, u32>>,
    set: Vec<BTreeSet<u8>>,
}

fn main() {}
