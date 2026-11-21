use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct Tree {
    value: u32,
    children: Vec<Rc<Tree>>,
}

fn main() {}
