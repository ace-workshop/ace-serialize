use ace_serialize_derive::AceSerialize;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(AceSerialize)]
struct Leaf {
    value: u32,
}

#[derive(AceSerialize)]
struct Holder {
    leaf: Option<Rc<RefCell<Leaf>>>,
}

fn main() {}
