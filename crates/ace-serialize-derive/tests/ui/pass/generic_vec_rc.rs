use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct HolderVec<T> {
    items: Vec<Rc<T>>,
}

fn main() {}
