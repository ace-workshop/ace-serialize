use ace_serialize::decode::{decode_cyclic, Decoder};
use ace_serialize::error::Error;
use ace_serialize::limits::Options;
use ace_serialize::{decode, decode_from, encode};
use ace_serialize_derive::AceSerialize;
use std::cell::RefCell;
use std::rc::Rc;

/// 数值 tag 的多态对象体：u16 tag + u32 slot_offset + u32 slot_count（FR-W07）。
const NUMERIC_BODY_BYTES: usize = 2 + 8;
/// type_name 逃生口的多态对象体：u16 tag + u32 type_name id + u32 slot_offset + u32 slot_count（FR-W07）。
const ESCAPE_BODY_BYTES: usize = 2 + 4 + 8;
/// 逃生口 tag（FR-W07）：65535 专供显式 type_name。
const ESCAPE_TAG: u16 = 65535;

fn raw_options() -> Options {
    Options {
        packed: false,
        ..Options::default()
    }
}

/// 数值 tag 多态枚举：单元变体、带载荷变体、tag 0 与 65534（FR-D05、FR-W07）。
#[derive(AceSerialize, Debug, PartialEq)]
enum Numeric {
    #[ace(tag = 0)]
    Idle,
    #[ace(tag = 7)]
    Busy { id: u16, name: String },
    #[ace(tag = 65534)]
    Last { value: u32 },
}

/// type_name 逃生口枚举：每个变体都写 65535 + 显式别名记录（FR-D05、FR-W07）。
#[derive(AceSerialize, Debug, PartialEq)]
enum Named {
    #[ace(type_name = "Alarm")]
    Alarm { level: i32 },
    #[ace(type_name = "Idle")]
    Idle,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Holder {
    numeric: Numeric,
    named: Named,
}

/// E9（R18）：旧变体里含一个不可用的共享单元格（`Rc<RefCell<u32>>`，且多态 enum 变体本身
/// 不允许 `Rc` 字段，故用一层 derive 结构体装着），新变体完全不含该包装。
#[derive(AceSerialize, Debug, PartialEq)]
struct CellHolder {
    cell: Rc<RefCell<u32>>,
}

#[derive(AceSerialize, Debug, PartialEq)]
enum Switchable {
    #[ace(tag = 20)]
    Locked { holder: CellHolder },
    #[ace(tag = 21)]
    Plain { value: u32 },
}

/// 可选与集合元素位置：`Option<T>` 走 `NonOptional`，`Vec<T>` 走引用型元素（ElementKind::Ref）。
#[derive(AceSerialize, Debug, PartialEq)]
struct Collections {
    numeric: Vec<Numeric>,
    named: Option<Named>,
}

fn slot(depth: i32, tag: i32, payload: [u8; 8]) -> Vec<u8> {
    [
        depth.to_le_bytes().as_slice(),
        tag.to_le_bytes().as_slice(),
        &payload,
    ]
    .concat()
}

fn record(id: i32, body: &[u8]) -> Vec<u8> {
    [
        id.to_le_bytes().as_slice(),
        (body.len() as i32).to_le_bytes().as_slice(),
        body,
    ]
    .concat()
}

fn raw_message(main: &[u8], assist: &[u8]) -> Vec<u8> {
    [
        (main.len() as i32).to_le_bytes().as_slice(),
        main,
        (assist.len() as i32).to_le_bytes().as_slice(),
        assist,
    ]
    .concat()
}

/// 手工拼多态对象体（FR-W07）；坏包用例据此构造。
fn poly_body(tag: u16, type_name: Option<u32>, offset: u32, count: u32) -> Vec<u8> {
    let mut body = tag.to_le_bytes().to_vec();
    if let Some(id) = type_name {
        body.extend_from_slice(&id.to_le_bytes());
    }
    body.extend_from_slice(&offset.to_le_bytes());
    body.extend_from_slice(&count.to_le_bytes());
    body
}

/// 取 raw 消息的 main 区（FR-W01）。
fn main_region(bytes: &[u8]) -> &[u8] {
    let main_len = i32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    &bytes[4..4 + main_len]
}

/// 按 main 的平铺记录头切出 (id, body)（FR-W02）。
fn records(main: &[u8]) -> Vec<(i32, Vec<u8>)> {
    let mut records = Vec::new();
    let mut position = 0;
    while position < main.len() {
        let id = i32::from_le_bytes(main[position..position + 4].try_into().unwrap());
        let size =
            i32::from_le_bytes(main[position + 4..position + 8].try_into().unwrap()) as usize;
        records.push((id, main[position + 8..position + 8 + size].to_vec()));
        position += 8 + size;
    }
    records
}

#[test]
fn numeric_and_type_name_variants_round_trip() {
    let numeric = [
        Numeric::Idle,
        Numeric::Busy {
            id: 7,
            name: "busy".into(),
        },
        Numeric::Last { value: 9 },
    ];
    let named = [Named::Alarm { level: -3 }, Named::Idle];
    let holders = [
        Holder {
            numeric: Numeric::Idle,
            named: Named::Alarm { level: 4 },
        },
        Holder {
            numeric: Numeric::Busy {
                id: 1,
                name: String::new(),
            },
            named: Named::Idle,
        },
    ];
    let collections = [
        Collections {
            numeric: vec![Numeric::Idle, Numeric::Last { value: 2 }],
            named: Some(Named::Alarm { level: -1 }),
        },
        Collections {
            numeric: Vec::new(),
            named: None,
        },
    ];
    for options in [Options::default(), raw_options()] {
        for value in &numeric {
            let bytes = encode(value, &options).unwrap();
            assert_eq!(decode::<Numeric>(&bytes, &options).unwrap(), *value);
        }
        for value in &named {
            let bytes = encode(value, &options).unwrap();
            assert_eq!(decode::<Named>(&bytes, &options).unwrap(), *value);
        }
        for value in &holders {
            let bytes = encode(value, &options).unwrap();
            assert_eq!(decode::<Holder>(&bytes, &options).unwrap(), *value);
        }
        for value in &collections {
            let bytes = encode(value, &options).unwrap();
            assert_eq!(decode::<Collections>(&bytes, &options).unwrap(), *value);
        }
    }
    // 两阶段图解码入口同样接受多态根字段（AceDecodeCyclic 计划为 Self）。
    let value = &holders[0];
    let bytes = encode(value, &raw_options()).unwrap();
    let mut decoder = Decoder::new(&bytes, &raw_options()).unwrap();
    assert_eq!(decode_cyclic::<Holder>(&mut decoder).unwrap(), *value);
    decoder.finish().unwrap();

    // WP-E5：同一判别变体复用该变体已有载荷（`name` 缓冲指针/容量不变），跨变体整体替换。
    for options in [Options::default(), raw_options()] {
        let busy = Numeric::Busy {
            id: 7,
            name: String::from("busy"),
        };
        let bytes = encode(&busy, &options).unwrap();

        let mut stale_name = String::with_capacity(32);
        stale_name.push_str("stale");
        let mut out = Numeric::Busy {
            id: 0,
            name: stale_name,
        };
        let (name_pointer, name_capacity) = match &out {
            Numeric::Busy { name, .. } => (name.as_ptr(), name.capacity()),
            other => panic!("旧值须为 Busy 变体：{other:?}"),
        };
        decode_from(&bytes, &mut out, &options).unwrap();
        match &out {
            Numeric::Busy { name, .. } => {
                assert_eq!(name.as_ptr(), name_pointer, "同变体复用 name 缓冲");
                assert_eq!(name.capacity(), name_capacity, "同变体 name 容量不得变化");
            }
            other => panic!("同变体解码不得更换变体：{other:?}"),
        }
        assert_eq!(out, busy);

        let mut out = Numeric::Last { value: 9 };
        decode_from(&bytes, &mut out, &options).unwrap();
        assert_eq!(out, busy);

        // R18/E9：旧变体含不可用单元格（泄漏 `RefMut`），切到不含该包装的新变体 ⇒ 整体替换，
        // 不得因旧单元格不可用而返回 `1 InvalidArgument`。
        let plain = Switchable::Plain { value: 4 };
        let bytes = encode(&plain, &options).unwrap();
        let cell: Rc<RefCell<u32>> = Rc::new(RefCell::new(5));
        std::mem::forget(cell.borrow_mut());
        let mut out = Switchable::Locked {
            holder: CellHolder { cell },
        };
        let decoded = decode_from(&bytes, &mut out, &options);
        assert_eq!(decoded, Ok(()), "跨变体整体替换不得因旧单元格不可用而失败");
        assert_ne!(
            decoded,
            Err(Error::InvalidArgument),
            "R18：整体替换不得把旧变体的不可用单元格写成⑥检查点"
        );
        assert_eq!(out, plain);
    }
}

#[test]
fn numeric_tag_body_has_no_type_name() {
    let bytes = encode(&Numeric::Last { value: 5 }, &raw_options()).unwrap();
    let parsed = records(main_region(&bytes));
    assert_eq!(parsed.len(), 1, "数值 tag 不写 type_name 记录");
    let (id, body) = &parsed[0];
    assert_eq!(*id, 1);
    assert_eq!(body.len(), NUMERIC_BODY_BYTES);
    assert_eq!(u16::from_le_bytes(body[..2].try_into().unwrap()), 65534);
    let offset = u32::from_le_bytes(body[2..6].try_into().unwrap());
    let count = u32::from_le_bytes(body[6..10].try_into().unwrap());
    assert_eq!(offset, 1);
    assert_eq!(count, 1);
}

#[test]
fn escape_tag_writes_type_name_record() {
    let bytes = encode(&Named::Alarm { level: 3 }, &raw_options()).unwrap();
    let parsed = records(main_region(&bytes));
    assert_eq!(parsed.len(), 2, "逃生口额外写一条 type_name 记录");
    let alias = parsed
        .iter()
        .find(|(_, body)| body.as_slice() == b"Alarm")
        .expect("type_name 记录内容为显式别名字节");
    let object = parsed
        .iter()
        .find(|(_, body)| body.len() == ESCAPE_BODY_BYTES)
        .expect("多态对象体为 14 字节");
    let (alias_id, _) = alias;
    let (_, body) = object;
    assert_eq!(
        u16::from_le_bytes(body[..2].try_into().unwrap()),
        ESCAPE_TAG
    );
    assert_eq!(
        u32::from_le_bytes(body[2..6].try_into().unwrap()),
        *alias_id as u32,
        "以 u32 type_name record id 引用别名"
    );
    assert_eq!(u32::from_le_bytes(body[6..10].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(body[10..14].try_into().unwrap()), 1);
}

#[test]
fn unknown_numeric_tag_is_invalid_tag() {
    // Numeric 的合法 tag 为 0、7、65534；tag 3 无对应变体。
    let main = record(1, &poly_body(3, None, 1, 0));
    let assist = slot(0, 0, 1u64.to_le_bytes());
    let bytes = raw_message(&main, &assist);
    let result: Result<Numeric, Error> = decode(&bytes, &raw_options());
    assert_eq!(result.unwrap_err(), Error::InvalidTag);
}

#[test]
fn type_name_reference_errors() {
    let assist = slot(0, 0, 1u64.to_le_bytes());
    // ① record 不存在/越界 → 7 InvalidReference。
    let main = record(1, &poly_body(ESCAPE_TAG, Some(9), 1, 0));
    let bytes = raw_message(&main, &assist);
    let result: Result<Named, Error> = decode(&bytes, &raw_options());
    assert_eq!(result.unwrap_err(), Error::InvalidReference);
    // ② 内容非法 UTF-8 → 8 InvalidUtf8。
    let main = [
        record(2, &[0xff]),
        record(1, &poly_body(ESCAPE_TAG, Some(2), 1, 0)),
    ]
    .concat();
    let bytes = raw_message(&main, &assist);
    let result: Result<Named, Error> = decode(&bytes, &raw_options());
    assert_eq!(result.unwrap_err(), Error::InvalidUtf8);
    // ③ 合法 UTF-8 但无匹配别名 → 4 InvalidTag。
    let main = [
        record(2, b"Missing"),
        record(1, &poly_body(ESCAPE_TAG, Some(2), 1, 0)),
    ]
    .concat();
    let bytes = raw_message(&main, &assist);
    let result: Result<Named, Error> = decode(&bytes, &raw_options());
    assert_eq!(result.unwrap_err(), Error::InvalidTag);
}

#[test]
fn polymorphic_object_body_length_matches_tag_mode() {
    let numeric = encode(&Numeric::Idle, &raw_options()).unwrap();
    let escape = encode(&Named::Idle, &raw_options()).unwrap();
    let numeric_bodies = records(main_region(&numeric))
        .into_iter()
        .filter(|(_, body)| body.len() == NUMERIC_BODY_BYTES)
        .count();
    assert_eq!(numeric_bodies, 1);
    let escape_bodies = records(main_region(&escape))
        .into_iter()
        .filter(|(_, body)| body.len() == ESCAPE_BODY_BYTES)
        .count();
    assert_eq!(escape_bodies, 1);
}

#[test]
fn encoder_rejects_inconsistent_polymorphic_headers() {
    let mut encoder = ace_serialize::encode::Encoder::new(&raw_options());
    assert_eq!(
        encoder.enter_polymorphic_object(ESCAPE_TAG, None),
        Err(Error::InvalidArgument)
    );
    let mut encoder = ace_serialize::encode::Encoder::new(&raw_options());
    assert_eq!(
        encoder.enter_polymorphic_object(5, Some("Numeric")),
        Err(Error::InvalidArgument)
    );
}

/// I1 回归：变体字段名 `e` 与生成代码的 Encoder 参数同名（tag 模式）。
#[derive(AceSerialize, Debug, PartialEq)]
enum Shadowed {
    #[ace(tag = 0)]
    Idle,
    #[ace(tag = 1)]
    Value { e: u32 },
}

/// I2 回归：具名空变体 `Empty {}` 与载荷变体混编（tag 模式），花括号形态不得被当成单元变体。
/// `Skipped` 线上载荷为空、但只有 `#[ace(skip)]` 字段：写入既有实例的闭包仍经捕获的 `&mut`
/// 执行 `*cache = Default::default()`，故闭包绑定必须带 `mut`（B1 回归）。
#[derive(AceSerialize, Debug, PartialEq)]
enum NamedEmpty {
    #[ace(tag = 3)]
    Empty {},
    #[ace(tag = 4)]
    Data { value: u8 },
    #[ace(tag = 5)]
    Skipped {
        #[ace(skip)]
        cache: u8,
    },
}

#[test]
fn variant_field_named_e_does_not_shadow_encoder() {
    for options in [Options::default(), raw_options()] {
        for value in [
            Shadowed::Idle,
            Shadowed::Value { e: 0 },
            Shadowed::Value { e: 42 },
        ] {
            let bytes = encode(&value, &options).unwrap();
            assert_eq!(decode::<Shadowed>(&bytes, &options).unwrap(), value);
        }
    }
}

#[test]
fn named_empty_variant_keeps_braces() {
    for options in [Options::default(), raw_options()] {
        for value in [
            NamedEmpty::Empty {},
            NamedEmpty::Data { value: 9 },
            NamedEmpty::Skipped { cache: 0 },
        ] {
            let bytes = encode(&value, &options).unwrap();
            assert_eq!(decode::<NamedEmpty>(&bytes, &options).unwrap(), value);
        }
        // 全 `skip` 变体：线上字段为空，写入既有实例时 skip 字段仍恢复 `Default`。
        let bytes = encode(&NamedEmpty::Skipped { cache: 0 }, &options).unwrap();
        let mut out = NamedEmpty::Skipped { cache: 7 };
        decode_from(&bytes, &mut out, &options).unwrap();
        assert_eq!(out, NamedEmpty::Skipped { cache: 0 });
    }
}

#[test]
fn polymorphic_object_shares_object_slot_validation() {
    // 对象范围 offset=0 指向根槽，不属任何对象范围（FR-W06）→ 4 InvalidTag。
    let main = record(1, &poly_body(0, None, 0, 1));
    let assist = [slot(0, 0, 1u64.to_le_bytes()), slot(1, 0, [0; 8])].concat();
    let bytes = raw_message(&main, &assist);
    let result: Result<Numeric, Error> = decode(&bytes, &raw_options());
    assert_eq!(result.unwrap_err(), Error::InvalidTag);
    // 数值 tag 只允许 10B 对象体；多出的 4B → 4 InvalidTag。
    let mut body = poly_body(0, None, 1, 1);
    body.extend_from_slice(&[0; 4]);
    let main = record(1, &body);
    let bytes = raw_message(&main, &assist);
    let result: Result<Numeric, Error> = decode(&bytes, &raw_options());
    assert_eq!(result.unwrap_err(), Error::InvalidTag);
}
