use ace_serialize_derive::AceSerialize;
use std::rc::{Rc, Weak};

#[derive(AceSerialize)]
struct Node {
    value: u32,
    parent: Weak<Node>,
    shared: Rc<u32>,
}

fn main() {}
