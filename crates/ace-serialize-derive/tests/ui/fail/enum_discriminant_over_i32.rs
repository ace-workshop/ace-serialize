use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
enum Unmarked {
    Zero = 0,
    Big = 4_294_967_296,
}

fn main() {}
