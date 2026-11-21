use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct Left {
    others: Vec<Rc<Right>>,
}

#[derive(AceSerialize)]
struct Right {
    others: Vec<Rc<Left>>,
}

fn main() {}
