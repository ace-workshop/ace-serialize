use ace_serialize_derive::AceSerialize;
use std::cell::RefCell;
use std::rc::{Rc, Weak};

#[derive(AceSerialize)]
struct A {
    b: Rc<RefCell<B>>,
}

#[derive(AceSerialize)]
struct B {
    a: Weak<RefCell<A>>,
}

fn main() {}
