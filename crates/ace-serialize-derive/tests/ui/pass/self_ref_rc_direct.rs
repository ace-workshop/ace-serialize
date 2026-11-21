use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct Node {
    value: u32,
    next: Rc<Node>,
}

fn main() {}
