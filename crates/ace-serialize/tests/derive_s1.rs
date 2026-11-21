use ace_serialize::decode::{decode_cyclic, Decoder};
use ace_serialize::error::Error;
use ace_serialize::limits::Options;
use ace_serialize::{decode, decode_from, encode};
use ace_serialize_derive::AceSerialize;
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};

/// 每个字段槽为 16 字节（FR-W03）；raw 输出的长度差即槽数差。
const SLOT_BYTES: usize = 16;

fn raw_options() -> Options {
    Options {
        packed: false,
        ..Options::default()
    }
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Item {
    id: u64,
    label: String,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Record {
    enabled: bool,
    count: i32,
    ratio: f64,
    name: String,
    maybe: Option<u32>,
    values: Vec<u16>,
    nested: Vec<Vec<u16>>,
    table: BTreeMap<u32, String>,
    boxed: Box<u8>,
    item: Item,
    note: Option<Item>,
}

#[test]
fn derived_struct_round_trips_every_field_kind() {
    let value = Record {
        enabled: true,
        count: -7,
        ratio: 2.5,
        name: String::from("ace"),
        maybe: Some(0),
        values: vec![1, 2, 3],
        nested: vec![vec![4, 5], vec![6]],
        table: BTreeMap::from([(4, String::from("four"))]),
        boxed: Box::new(9),
        item: Item {
            id: 11,
            label: String::new(),
        },
        note: Some(Item {
            id: 12,
            label: String::from("note"),
        }),
    };
    for options in [Options::default(), raw_options()] {
        let bytes = encode(&value, &options).unwrap();
        assert_eq!(decode::<Record>(&bytes, &options).unwrap(), value);

        // 写入既有实例时逐字段走 `ace_decode_into`（WP-E5/D2）：目标以非默认旧值起步且容器容量
        // 足够，故字段缓冲指针与容量都不得变化——只断言值相等无法区分整体替换式实现。
        let mut out = Record {
            enabled: false,
            count: 5,
            ratio: 0.5,
            name: {
                let mut name = String::with_capacity(64);
                name.push_str("stale name");
                name
            },
            maybe: None,
            values: {
                let mut values = Vec::with_capacity(8);
                values.extend_from_slice(&[9, 9]);
                values
            },
            nested: {
                let mut outer = Vec::with_capacity(4);
                for seeded in [vec![9u16, 9], vec![8]] {
                    // 内层容量大于解码长度且与首次分配量不同：未复用时新内层不会是 16，断言有辨别力。
                    let mut inner = Vec::with_capacity(16);
                    inner.extend_from_slice(&seeded);
                    outer.push(inner);
                }
                outer
            },
            table: BTreeMap::from([(1, String::from("old"))]),
            boxed: Box::new(1),
            item: Item {
                id: 0,
                label: String::from("old"),
            },
            note: Some(Item {
                id: 0,
                label: {
                    let mut label = String::with_capacity(32);
                    label.push_str("stale");
                    label
                },
            }),
        };
        let name_pointer = out.name.as_ptr();
        let name_capacity = out.name.capacity();
        let values_pointer = out.values.as_ptr();
        let values_capacity = out.values.capacity();
        let nested_inner: Vec<(usize, usize)> = out
            .nested
            .iter()
            .map(|inner| (inner.as_ptr() as usize, inner.capacity()))
            .collect();
        let note = out.note.as_ref().expect("旧值须为 Some 以覆盖 Some→Some");
        let note_label_pointer = note.label.as_ptr();
        let note_label_capacity = note.label.capacity();

        decode_from(&bytes, &mut out, &options).unwrap();

        assert_eq!(out.name.as_ptr(), name_pointer, "name 缓冲不得重新分配");
        assert_eq!(out.name.capacity(), name_capacity, "name 容量不得变化");
        assert_eq!(out.values.as_ptr(), values_pointer, "values 缓冲不得重新分配");
        assert_eq!(out.values.capacity(), values_capacity, "values 容量不得变化");
        // R22①（derive 侧嵌套内层容器）：`Vec<Vec<T>>` 的**内层** `Vec` 缓冲同样不得重新分配。
        assert_eq!(
            out.nested
                .iter()
                .map(|inner| (inner.as_ptr() as usize, inner.capacity()))
                .collect::<Vec<_>>(),
            nested_inner,
            "nested 的内层 Vec 缓冲不得重新分配"
        );
        assert_eq!(out, value);
        assert_eq!(out, decode::<Record>(&bytes, &options).unwrap());
        let note = out.note.as_ref().expect("Some→Some 保留 Some");
        assert_eq!(note.id, 12);
        assert_eq!(note.label, "note");
        assert_eq!(
            note.label.as_ptr(),
            note_label_pointer,
            "Option 内层缓冲不得重新分配"
        );
        assert_eq!(note.label.capacity(), note_label_capacity);
    }
    let bytes = encode(&value, &Options::default()).unwrap();
    let mut decoder = Decoder::new(&bytes, &Options::default()).unwrap();
    assert_eq!(decode_cyclic::<Record>(&mut decoder).unwrap(), value);
    decoder.finish().unwrap();
}

#[derive(AceSerialize)]
struct Scalars {
    flag: bool,
    number: u32,
    ratio: f32,
}

#[test]
fn zero_valued_scalars_write_no_slot_but_negative_zero_is_written() {
    let options = raw_options();
    let base = encode(
        &Scalars {
            flag: false,
            number: 0,
            ratio: 0.0,
        },
        &options,
    )
    .unwrap()
    .len();
    let flagged = encode(
        &Scalars {
            flag: true,
            number: 0,
            ratio: 0.0,
        },
        &options,
    )
    .unwrap()
    .len();
    let negative_zero = encode(
        &Scalars {
            flag: false,
            number: 0,
            ratio: -0.0,
        },
        &options,
    )
    .unwrap()
    .len();
    assert_eq!(flagged, base + SLOT_BYTES);
    assert_eq!(negative_zero, base + SLOT_BYTES);
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Maybe {
    value: Option<i32>,
}

#[test]
fn option_none_writes_no_slot_and_some_zero_is_written() {
    let options = raw_options();
    let none = encode(&Maybe { value: None }, &options).unwrap();
    let some_zero = encode(&Maybe { value: Some(0) }, &options).unwrap();
    assert_eq!(some_zero.len(), none.len() + SLOT_BYTES);
    assert_eq!(
        decode::<Maybe>(&none, &options).unwrap(),
        Maybe { value: None }
    );
    assert_eq!(
        decode::<Maybe>(&some_zero, &options).unwrap(),
        Maybe { value: Some(0) }
    );
    // WP-E5/D3：`Some→None` 整体替换、`None→Some` 新建（derive 只转发完整字段类型）。
    let mut out = Maybe { value: Some(9) };
    decode_from(&none, &mut out, &options).unwrap();
    assert_eq!(out, Maybe { value: None });
    let mut out = Maybe { value: None };
    decode_from(&some_zero, &mut out, &options).unwrap();
    assert_eq!(out, Maybe { value: Some(0) });
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Defaults {
    flag: bool,
    number: i64,
    ratio: f64,
    letter: char,
    maybe: Option<u8>,
}

#[test]
fn absent_scalar_fields_decode_to_zero_values() {
    let options = raw_options();
    let zero = Defaults {
        flag: false,
        number: 0,
        ratio: 0.0,
        letter: '\0',
        maybe: None,
    };
    let bytes = encode(&zero, &options).unwrap();
    assert_eq!(decode::<Defaults>(&bytes, &options).unwrap(), zero);
    // WP-E5/D2：缺席标量在写入既有实例时恢复 `Default`，不保留旧值。
    let mut out = Defaults {
        flag: true,
        number: -7,
        ratio: 2.5,
        letter: 'x',
        maybe: Some(9),
    };
    decode_from(&bytes, &mut out, &options).unwrap();
    assert_eq!(out, zero);
    let mut decoder = Decoder::new(&bytes, &options).unwrap();
    assert_eq!(decode_cyclic::<Defaults>(&mut decoder).unwrap(), zero);
    decoder.finish().unwrap();
}

#[derive(AceSerialize)]
struct Older {
    #[ace(order = 0)]
    id: u32,
}

#[derive(AceSerialize)]
struct Newer {
    #[ace(order = 0)]
    id: u32,
    #[ace(order = 1)]
    name: String,
}

#[test]
fn absent_required_field_reports_missing_required_field() {
    let options = raw_options();
    let bytes = encode(&Older { id: 7 }, &options).unwrap();
    assert_eq!(
        decode::<Newer>(&bytes, &options).err(),
        Some(Error::MissingRequiredField)
    );
    let mut decoder = Decoder::new(&bytes, &options).unwrap();
    assert_eq!(
        decode_cyclic::<Newer>(&mut decoder).err(),
        Some(Error::MissingRequiredField)
    );
    // WP-E5：前序字段（`id`）已写入、后序字段（`name`）缺席也在写入既有实例的路径报同一错误；
    // 失败后的 `out` 不作断言（不承诺整体不变）。
    let mut out = Newer {
        id: 0,
        name: String::new(),
    };
    assert_eq!(
        decode_from(&bytes, &mut out, &options).err(),
        Some(Error::MissingRequiredField)
    );
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Declared {
    #[ace(order = 9)]
    late: u32,
    #[ace(order = 2)]
    early: u32,
}

#[derive(AceSerialize)]
struct Ascending {
    #[ace(order = 2)]
    early: u32,
    #[ace(order = 9)]
    late: u32,
}

#[test]
fn fields_are_emitted_in_ascending_order_whatever_the_declaration_order() {
    let options = raw_options();
    let declared = encode(&Declared { late: 5, early: 3 }, &options).unwrap();
    let ascending = encode(&Ascending { early: 3, late: 5 }, &options).unwrap();
    assert_eq!(declared, ascending);
    assert_eq!(
        decode::<Declared>(&declared, &options).unwrap(),
        Declared { late: 5, early: 3 }
    );
}

#[derive(AceSerialize, Debug, PartialEq)]
struct WithSkip {
    value: u32,
    #[ace(skip)]
    cache: u64,
    tail: u32,
}

#[derive(AceSerialize)]
struct Plain {
    #[ace(order = 0)]
    value: u32,
    #[ace(order = 2)]
    tail: u32,
}

#[test]
fn skipped_field_is_not_written_and_keeps_its_number() {
    let options = raw_options();
    let with_skip = encode(
        &WithSkip {
            value: 5,
            cache: 99,
            tail: 6,
        },
        &options,
    )
    .unwrap();
    let plain = encode(&Plain { value: 5, tail: 6 }, &options).unwrap();
    assert_eq!(with_skip, plain);
    assert_eq!(
        decode::<WithSkip>(&with_skip, &options).unwrap(),
        WithSkip {
            value: 5,
            cache: 0,
            tail: 6,
        }
    );
    // WP-E5/D2：`skip` 字段在写入既有实例时恢复 `Default`（与拥有值路径同一语义）。
    let mut out = WithSkip {
        value: 0,
        cache: 77,
        tail: 0,
    };
    decode_from(&with_skip, &mut out, &options).unwrap();
    assert_eq!(
        out,
        WithSkip {
            value: 5,
            cache: 0,
            tail: 6,
        }
    );
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Bag {
    items: Vec<Item>,
}

#[test]
fn vec_of_derived_structs_round_trips() {
    let value = Bag {
        items: vec![
            Item {
                id: 1,
                label: String::from("first"),
            },
            Item {
                id: 2,
                label: String::new(),
            },
        ],
    };
    let bytes = encode(&value, &Options::default()).unwrap();
    assert_eq!(decode::<Bag>(&bytes, &Options::default()).unwrap(), value);

    // R22①：`Vec<Derived>` 的元素级复用——已有元素经生成的 `decode_element_into` 逐字段写入，
    // 内层 `String` 缓冲不重新分配（容量 16 与首次分配量不同，断言有辨别力）。
    let mut out = Bag {
        items: vec![
            Item {
                id: 9,
                label: String::with_capacity(16),
            },
            Item {
                id: 8,
                label: String::with_capacity(16),
            },
        ],
    };
    let seeded: Vec<(usize, usize)> = out
        .items
        .iter()
        .map(|item| (item.label.as_ptr() as usize, item.label.capacity()))
        .collect();
    decode_from(&bytes, &mut out, &Options::default()).unwrap();
    assert_eq!(
        out.items
            .iter()
            .map(|item| (item.label.as_ptr() as usize, item.label.capacity()))
            .collect::<Vec<_>>(),
        seeded,
        "元素级复用：内层 String 缓冲不得重新分配"
    );
    assert_eq!(out, value);
    assert_eq!(out, decode::<Bag>(&bytes, &Options::default()).unwrap());
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Pair<T> {
    first: T,
    rest: Vec<T>,
}

#[test]
fn generic_struct_round_trips_with_its_type_argument() {
    let value = Pair {
        first: 3u32,
        rest: vec![4, 5],
    };
    let bytes = encode(&value, &Options::default()).unwrap();
    assert_eq!(
        decode::<Pair<u32>>(&bytes, &Options::default()).unwrap(),
        value
    );
}

#[derive(AceSerialize)]
struct Link {
    node: Rc<Node>,
}

#[derive(AceSerialize)]
struct Node {
    value: u32,
    me: Weak<Node>,
}

#[test]
fn rc_and_self_weak_fields_round_trip_through_cyclic_decode() {
    let node = Rc::new_cyclic(|me| Node {
        value: 3,
        me: Weak::clone(me),
    });
    let options = Options::default();
    let bytes = encode(&Link { node }, &options).unwrap();
    let mut decoder = Decoder::new(&bytes, &options).unwrap();
    let link = decode_cyclic::<Link>(&mut decoder).unwrap();
    decoder.finish().unwrap();
    assert_eq!(link.node.value, 3);
    let me = link.node.me.upgrade().expect("self Weak target is alive");
    assert!(Rc::ptr_eq(&me, &link.node));

    // R22②/C9：`Rc::new_cyclic` 自环形状的 `decode_from` 图路径——既有实例整体替换后，自持
    // `Weak` 回边仍指回同一节点（图路径不因写入既有实例而丢掉身份）。
    let mut reused = Link {
        node: Rc::new(Node {
            value: 0,
            me: Weak::new(),
        }),
    };
    decode_from(&bytes, &mut reused, &options).unwrap();
    assert_eq!(reused.node.value, 3);
    let me = reused
        .node
        .me
        .upgrade()
        .expect("decode_from 后 self Weak target is alive");
    assert!(
        Rc::ptr_eq(&me, &reused.node),
        "decode_from 的 Weak 回边须指回同一节点"
    );
}

#[derive(AceSerialize, Debug, PartialEq)]
struct QualifiedScalar {
    value: ::core::primitive::u32,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct PlainScalar {
    value: u32,
}

#[test]
fn qualified_primitive_scalar_is_omittable() {
    let options = raw_options();
    let qualified_zero = encode(&QualifiedScalar { value: 0 }, &options).unwrap();
    let plain_zero = encode(&PlainScalar { value: 0 }, &options).unwrap();
    assert_eq!(qualified_zero, plain_zero);
    let qualified_nonzero = encode(&QualifiedScalar { value: 1 }, &options).unwrap();
    assert_eq!(qualified_nonzero.len(), qualified_zero.len() + SLOT_BYTES);
    assert_eq!(
        decode::<QualifiedScalar>(&plain_zero, &options).unwrap(),
        QualifiedScalar { value: 0 }
    );
    assert_eq!(
        decode::<PlainScalar>(&plain_zero, &options).unwrap(),
        PlainScalar { value: 0 }
    );
}

#[test]
// rustc also reports the derive output, so the allow must cover the struct and its impls here.
#[allow(unused_parens)]
fn parenthesized_scalar_and_option_fields_keep_their_classification() {
    #[derive(AceSerialize, Debug, PartialEq)]
    struct Parenthesized {
        count: (u32),
        maybe: (Option<u32>),
    }

    let options = raw_options();
    let absent = Parenthesized {
        count: 0,
        maybe: None,
    };
    let present = Parenthesized {
        count: 1,
        maybe: Some(0),
    };
    let absent_bytes = encode(&absent, &options).unwrap();
    let present_bytes = encode(&present, &options).unwrap();
    assert_eq!(present_bytes.len(), absent_bytes.len() + 2 * SLOT_BYTES);
    let decoded_absent = decode::<Parenthesized>(&absent_bytes, &options).unwrap();
    let decoded_present = decode::<Parenthesized>(&present_bytes, &options).unwrap();
    assert_eq!(decoded_absent, absent);
    assert_eq!(decoded_present, present);
}

#[derive(AceSerialize)]
struct Target {
    value: u32,
}

#[derive(AceSerialize)]
struct SharedHop {
    id: u32,
    link: Option<Rc<Target>>,
}

#[test]
fn root_vec_of_shared_structs_round_trips_through_cyclic_decode() {
    let value = vec![
        SharedHop { id: 1, link: None },
        SharedHop {
            id: 3,
            link: Some(Rc::new(Target { value: 2 })),
        },
    ];
    let options = Options::default();
    let bytes = encode(&value, &options).unwrap();
    let mut decoder = Decoder::new(&bytes, &options).unwrap();
    let decoded = decode_cyclic::<Vec<SharedHop>>(&mut decoder).unwrap();
    decoder.finish().unwrap();
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].id, 1);
    assert!(decoded[0].link.is_none());
    assert_eq!(decoded[1].id, 3);
    assert_eq!(decoded[1].link.as_ref().map(|target| target.value), Some(2));
}

#[test]
// rustc also reports the derive output, so the allow must cover the struct and its impls here.
#[allow(unused_parens)]
fn parenthesized_shared_cell_field_round_trips_through_cyclic_decode() {
    #[derive(AceSerialize)]
    struct ParenthesizedShared {
        cell: (Rc<std::cell::RefCell<u32>>),
    }

    let value = ParenthesizedShared {
        cell: Rc::new(std::cell::RefCell::new(7)),
    };
    let options = Options::default();
    let bytes = encode(&value, &options).unwrap();
    let mut decoder = Decoder::new(&bytes, &options).unwrap();
    let decoded = decode_cyclic::<ParenthesizedShared>(&mut decoder).unwrap();
    decoder.finish().unwrap();
    assert_eq!(*decoded.cell.borrow(), 7);
}

#[test]
fn vec_children_with_option_weak_parent_round_trip_through_cyclic_decode() {
    #[derive(AceSerialize)]
    struct TreeNode {
        value: u32,
        children: Vec<Rc<TreeNode>>,
        parent: Option<Weak<TreeNode>>,
    }

    #[derive(AceSerialize)]
    struct TreeLink {
        root: Rc<TreeNode>,
        alias: Rc<TreeNode>,
    }

    // C9③的正对照载体：同一条消息内既有同 id 的别名字段（`first`/`alias`），也有异 id 的等值
    // 实例（`first`/`second`），用于区分「按 id 判身份」与「按值或全局去重」。
    #[derive(AceSerialize)]
    struct TreeIdentityMix {
        first: Rc<TreeNode>,
        second: Rc<TreeNode>,
        alias: Rc<TreeNode>,
    }

    let root = Rc::new_cyclic(|root_me| TreeNode {
        value: 1,
        children: vec![Rc::new(TreeNode {
            value: 2,
            children: Vec::new(),
            parent: Some(Weak::clone(root_me)),
        })],
        parent: None,
    });
    // 图身份见证不得依赖打包形态：raw 与 packed 都要求同一 `Rc` 与 `Weak` 回边。
    for options in [Options::default(), raw_options()] {
        let bytes = encode(
            &TreeLink {
                root: Rc::clone(&root),
                alias: Rc::clone(&root),
            },
            &options,
        )
        .unwrap();
        let mut decoder = Decoder::new(&bytes, &options).unwrap();
        let link = decode_cyclic::<TreeLink>(&mut decoder).unwrap();
        decoder.finish().unwrap();
        assert_eq!(link.root.value, 1);
        assert!(link.root.parent.is_none());
        assert_eq!(link.root.children.len(), 1);
        let child = &link.root.children[0];
        assert_eq!(child.value, 2);
        let back = child
            .parent
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("child parent Weak target is alive");
        assert!(Rc::ptr_eq(&back, &link.root), "Weak 回边须指回根");

        // R22②：`shared` 类型有字节级拥有值入口，`decode` 与 `decode_from` 都可达图路径；两者结果
        // 与上面的拥有值形态一致，共享身份（两字段同一 `Rc`）与 `Weak` 回边均保持。
        let owned = decode::<TreeLink>(&bytes, &options).unwrap();
        let mut reused = TreeLink {
            root: Rc::new(TreeNode {
                value: 0,
                children: Vec::new(),
                parent: None,
            }),
            alias: Rc::new(TreeNode {
                value: 0,
                children: Vec::new(),
                parent: None,
            }),
        };
        decode_from(&bytes, &mut reused, &options).unwrap();
        for link in [&owned, &reused] {
            assert_eq!(link.root.value, 1);
            assert!(link.root.parent.is_none());
            assert_eq!(link.root.children.len(), 1);
            let child = &link.root.children[0];
            assert_eq!(child.value, 2);
            let back = child
                .parent
                .as_ref()
                .and_then(Weak::upgrade)
                .expect("child parent Weak target is alive");
            assert!(Rc::ptr_eq(&back, &link.root), "Weak 回边须指回根");
            assert!(Rc::ptr_eq(&link.root, &link.alias), "两个字段须指向同一 Rc");
        }
        // 身份按 id 而非按内容判定：同一输入两次解码得到等值但不同实例。
        assert_eq!(owned.root.value, reused.root.value);
        assert!(!Rc::ptr_eq(&owned.root, &reused.root), "两次解码各自建实例");
        // 不同 id 的两个等值实例不得合并：分别拥有后解码仍是两个 `Rc`。
        let separate = TreeLink {
            root: Rc::new(TreeNode {
                value: 1,
                children: Vec::new(),
                parent: None,
            }),
            alias: Rc::new(TreeNode {
                value: 1,
                children: Vec::new(),
                parent: None,
            }),
        };
        let bytes = encode(&separate, &options).unwrap();
        let decoded = decode::<TreeLink>(&bytes, &options).unwrap();
        assert_eq!(decoded.root.value, decoded.alias.value);
        assert!(
            !Rc::ptr_eq(&decoded.root, &decoded.alias),
            "不同 id 的等值实例不得合并"
        );
        // C9③：同一条消息内既有不同 id 的等值实例、也有相同 id 的别名字段；`decode_from` 必须
        // 按 id 判身份——同 id 的 `first`/`alias` 仍 `ptr_eq`，异 id 的 `first`/`second` 不得合并。
        let first = Rc::new(TreeNode {
            value: 5,
            children: Vec::new(),
            parent: None,
        });
        let second = Rc::new(TreeNode {
            value: 5,
            children: Vec::new(),
            parent: None,
        });
        let bytes = encode(
            &TreeIdentityMix {
                first: Rc::clone(&first),
                second: Rc::clone(&second),
                alias: Rc::clone(&first),
            },
            &options,
        )
        .unwrap();
        let mut reused = TreeIdentityMix {
            first: Rc::new(TreeNode {
                value: 0,
                children: Vec::new(),
                parent: None,
            }),
            second: Rc::new(TreeNode {
                value: 0,
                children: Vec::new(),
                parent: None,
            }),
            alias: Rc::new(TreeNode {
                value: 0,
                children: Vec::new(),
                parent: None,
            }),
        };
        decode_from(&bytes, &mut reused, &options).unwrap();
        assert_eq!(reused.first.value, 5);
        assert_eq!(reused.second.value, 5);
        assert!(
            Rc::ptr_eq(&reused.first, &reused.alias),
            "同一消息内相同 id 的字段必须仍是同一实例（decode_from 正对照）"
        );
        assert!(
            !Rc::ptr_eq(&reused.first, &reused.second),
            "同一消息内不同 id 的等值实例不得合并（decode_from 侧）"
        );
    }

    // C9①：`Arc` 共享身份的 derive `decode_from` 图路径（std 侧）——`Arc::ptr_eq` 为真、
    // `Weak::upgrade` 回同一实例，且 `decode`（拥有值）与 `decode_from`（写入既有实例）同结果。
    #[cfg(feature = "std")]
    {
        use std::sync::{Arc, Weak as ArcWeak};

        #[derive(AceSerialize)]
        struct ArcNode {
            value: u32,
            children: Vec<Arc<ArcNode>>,
            parent: Option<ArcWeak<ArcNode>>,
        }

        #[derive(AceSerialize)]
        struct ArcLink {
            root: Arc<ArcNode>,
            alias: Arc<ArcNode>,
        }

        let root: Arc<ArcNode> = Arc::new_cyclic(|root_me| ArcNode {
            value: 1,
            children: vec![Arc::new(ArcNode {
                value: 2,
                children: Vec::new(),
                parent: Some(ArcWeak::clone(root_me)),
            })],
            parent: None,
        });
        for options in [Options::default(), raw_options()] {
            let bytes = encode(
                &ArcLink {
                    root: Arc::clone(&root),
                    alias: Arc::clone(&root),
                },
                &options,
            )
            .unwrap();
            let owned = decode::<ArcLink>(&bytes, &options).unwrap();
            let mut reused = ArcLink {
                root: Arc::new(ArcNode {
                    value: 0,
                    children: Vec::new(),
                    parent: None,
                }),
                alias: Arc::new(ArcNode {
                    value: 0,
                    children: Vec::new(),
                    parent: None,
                }),
            };
            decode_from(&bytes, &mut reused, &options).unwrap();
            for link in [&owned, &reused] {
                assert_eq!(link.root.value, 1);
                assert_eq!(link.root.children.len(), 1);
                let child = &link.root.children[0];
                assert_eq!(child.value, 2);
                let back = child
                    .parent
                    .as_ref()
                    .and_then(ArcWeak::upgrade)
                    .expect("child parent Weak target is alive");
                assert!(Arc::ptr_eq(&back, &link.root), "Arc Weak 回边须指回根");
                assert!(
                    Arc::ptr_eq(&link.root, &link.alias),
                    "两个字段须指向同一 Arc"
                );
            }
            // 身份按 id 而非按内容判定：两次解码得到等值但不同实例。
            assert!(!Arc::ptr_eq(&owned.root, &reused.root), "两次解码各自建实例");
        }
    }
}

#[test]
fn rc_refcell_strong_edge_with_weak_refcell_back_edge_round_trips_through_cyclic_decode() {
    #[derive(AceSerialize)]
    struct CellA {
        value: u32,
        b: Rc<std::cell::RefCell<CellB>>,
    }

    #[derive(AceSerialize)]
    struct CellB {
        value: u32,
        a: Weak<std::cell::RefCell<CellA>>,
    }

    #[derive(AceSerialize)]
    struct CellLink {
        root: Rc<std::cell::RefCell<CellA>>,
    }

    let b = Rc::new(std::cell::RefCell::new(CellB {
        value: 2,
        a: Weak::new(),
    }));
    let a = Rc::new(std::cell::RefCell::new(CellA {
        value: 1,
        b: Rc::clone(&b),
    }));
    b.borrow_mut().a = Rc::downgrade(&a);
    let options = Options::default();
    let bytes = encode(&CellLink { root: a }, &options).unwrap();
    let mut decoder = Decoder::new(&bytes, &options).unwrap();
    let link = decode_cyclic::<CellLink>(&mut decoder).unwrap();
    decoder.finish().unwrap();
    let root = link.root.borrow();
    assert_eq!(root.value, 1);
    let child = root.b.borrow();
    assert_eq!(child.value, 2);
    let back = child.a.upgrade().expect("back Weak target is alive");
    assert!(Rc::ptr_eq(&back, &link.root));
}

mod arc_vec_self_reference {
    #![cfg(feature = "std")]

    use ace_serialize_derive::AceSerialize;
    use std::sync::Arc;

    #[derive(AceSerialize)]
    struct ArcTree {
        value: u32,
        children: Vec<Arc<ArcTree>>,
    }

    #[test]
    fn arc_vec_self_reference_compiles() {
        let _ = ArcTree {
            value: 1,
            children: Vec::new(),
        };
    }
}
