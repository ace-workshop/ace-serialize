use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
#[repr(C)]
enum CRepr {
    First,
    Second,
}

#[derive(AceSerialize)]
#[repr(isize)]
enum PointerRepr {
    First,
}

fn main() {}
