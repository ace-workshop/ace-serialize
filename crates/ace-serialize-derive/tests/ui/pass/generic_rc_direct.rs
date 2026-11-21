use ace_serialize_derive::AceSerialize;
use std::rc::Rc;

#[derive(AceSerialize)]
struct Holder<T> {
    item: Rc<T>,
}

// `shared` 类型只生成 `AceDecode` + `AceDecodeCyclic`，两者必须带同一组谓词：泛型 `skip` 字段的
// `Default` 也不能只出现在环图 impl 上，否则 `AceDecode` impl 无法满足 `decode_cyclic::<Self>`。
#[derive(AceSerialize)]
struct CacheHolder<C> {
    item: Rc<u32>,
    #[ace(skip)]
    cache: C,
}

fn main() {
    let _ = CacheHolder {
        item: Rc::new(1),
        cache: (),
    };
    let _: CacheHolder<()> = CacheHolder {
        item: Rc::new(2),
        cache: (),
    };
}
