use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct GNode<T> {
    value: T,
    next: Option<Rc<GNode<T>>>,
}

fn main() {}
