use ace_serialize_derive::AceSerialize;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(AceSerialize)]
struct SkipExemptTypes {
    value: u32,
    #[ace(skip)]
    wide: u128,
    #[ace(skip)]
    cell: RefCell<u32>,
    #[ace(skip)]
    shared: Rc<u32>,
}

fn main() {}
