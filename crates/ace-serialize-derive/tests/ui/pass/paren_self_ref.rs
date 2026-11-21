use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct Node {
    value: u32,
    next: (Rc<Node>),
}

#[derive(AceSerialize)]
struct OptNode {
    value: u32,
    next: Option<(Rc<OptNode>)>,
}

fn main() {}
