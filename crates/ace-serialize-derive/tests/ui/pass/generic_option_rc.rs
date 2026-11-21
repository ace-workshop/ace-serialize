use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct HolderOpt<T> {
    item: Option<Rc<T>>,
}

fn main() {}
