use ace_serialize_derive::AceSerialize;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(AceSerialize)]
struct Tree {
    nested: Option<Vec<Rc<Tree>>>,
    grid: Vec<Option<Rc<Tree>>>,
    named: BTreeMap<u32, Rc<Tree>>,
    boxed: Box<Rc<Tree>>,
}

fn main() {}
