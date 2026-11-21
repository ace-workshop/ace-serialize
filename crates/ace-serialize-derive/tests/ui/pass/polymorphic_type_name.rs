use ace_serialize_derive::AceSerialize;

/// type_name 逃生口多态枚举：显式别名（AC-D01 compile-pass）。
#[derive(AceSerialize)]
enum Named {
    #[ace(type_name = "Alarm")]
    Alarm {
        level: i32,
    },
    #[ace(type_name = "Idle")]
    Idle,
}

#[derive(AceSerialize)]
struct Holder {
    event: Named,
    maybe: Option<Named>,
    list: Vec<Named>,
}

fn main() {}
