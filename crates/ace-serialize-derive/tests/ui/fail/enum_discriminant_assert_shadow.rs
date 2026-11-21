use ace_serialize_derive::AceSerialize;

// 调用端同名宏：生成的 const 兜底断言若走裸 `assert!`，会被这里劫持。
macro_rules! assert {
    ($($tt:tt)*) => {};
}

const BIG: isize = 4_294_967_296;

#[derive(AceSerialize)]
enum Unmarked {
    Zero = 0,
    Big = BIG,
}

fn main() {
    assert!(true);
}
