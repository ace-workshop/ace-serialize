//! `decode_from` 的最小集成回归：入口只经 `ace_decode_into` 写入（`Vec` 缓冲指针不变）、结果与
//! `decode` 逐值一致（raw/packed）、`RefCell`/`RwLock` 单元格不可用（泄漏借用、中毒、被弃 guard）
//! → 既有 `1 InvalidArgument` 且该单元格未被修改、状态未被清除、复合目标辨别例、整体替换不触发
//! 检查，以及截断/非法值（`bool`/`char`/枚举判别/UTF-8/presence）仍返既有码；根 `String`／`Vec`
//! （含恰好容量）的指针与容量复用、内建 `Option<Vec<T>>` 的 `Some→Some` 内层复用、嵌套
//! `Vec<Vec<T>>` 的内层集合复用（R22①）、超限输入仍返既有 `6 ResourceLimit`。
//!
//! 只覆盖入口与内建覆写；derive 字段级 `ace_decode_into` 属 WP-E5，不在此断言（D4 用 derive
//! 整数枚举只为构造未知判别坏包）。

use ace_serialize::decode::Decoder;
use ace_serialize::encode::Encoder;
use ace_serialize::error::{Error, Result};
use ace_serialize::limits::Options;
use ace_serialize::schema::{AceDecode, AceEncode};
use ace_serialize::{decode, decode_from, encode};
use ace_serialize_derive::AceSerialize;
use std::cell::RefCell;

fn options(packed: bool) -> Options {
    Options {
        packed,
        ..Options::default()
    }
}

fn bytes_of<T: AceEncode>(value: &T, packed: bool) -> Vec<u8> {
    encode(value, &options(packed)).expect("拥有值形态必须成功")
}

/// ①容器缓冲复用（指针不变）+ ②成功结果与 `decode` 逐值一致，raw/packed 两态；根 `Vec` 与根
/// `String` 另按恰好容量（`capacity == 解码长度`）给样，断言指针与容量都不变。
#[test]
fn decode_from_reuses_vec_capacity_and_matches_decode() {
    let ints: Vec<i32> = vec![1, 0, -2];
    let strings: Vec<String> = vec![String::from("a"), String::from("bc")];
    let text = String::from("hello ace");
    for packed in [false, true] {
        let opts = options(packed);
        let src = bytes_of(&ints, packed);
        let mut out: Vec<i32> = Vec::with_capacity(8);
        out.extend_from_slice(&[9, 9, 9, 9]);
        let ptr = out.as_ptr();
        decode_from(&src, &mut out, &opts).unwrap();
        assert_eq!(out.as_ptr(), ptr, "packed = {packed}: 缓冲不得重新分配");
        assert_eq!(out, ints);
        assert_eq!(out, decode::<Vec<i32>>(&src, &opts).unwrap());

        // 同一输入、目标为恰好容量：不只外层指针，容量也不得变化。
        let mut out: Vec<i32> = Vec::with_capacity(ints.len());
        out.extend_from_slice(&[9, 9, 9]);
        assert_eq!(out.capacity(), ints.len(), "样本须为恰好容量");
        let ptr = out.as_ptr();
        let capacity = out.capacity();
        decode_from(&src, &mut out, &opts).unwrap();
        assert_eq!(
            out.as_ptr(),
            ptr,
            "packed = {packed}: 恰好容量时根 Vec 不得重新分配"
        );
        assert_eq!(
            out.capacity(),
            capacity,
            "packed = {packed}: 根 Vec 容量不得变化"
        );
        assert_eq!(out, ints);
        assert_eq!(out, decode::<Vec<i32>>(&src, &opts).unwrap());

        let src = bytes_of(&strings, packed);
        let mut out: Vec<String> = Vec::with_capacity(4);
        out.push(String::from("x"));
        let ptr = out.as_ptr();
        decode_from(&src, &mut out, &opts).unwrap();
        assert_eq!(out.as_ptr(), ptr, "packed = {packed}: 外层缓冲不得重新分配");
        assert_eq!(out, strings);
        assert_eq!(out, decode::<Vec<String>>(&src, &opts).unwrap());

        // 嵌套内层（R22①）：目标外层与**内层**都已有足够容量时，内层 `Vec` 的缓冲不得重新分配。
        let nested: Vec<Vec<i32>> = vec![vec![1, 2], vec![3]];
        let src = bytes_of(&nested, packed);
        let mut out: Vec<Vec<i32>> = Vec::with_capacity(4);
        for seeded in [vec![9, 9, 9], vec![8]] {
            // 容量大于解码长度且与首次分配量不同：未复用时新内层不会是 16，断言因此有辨别力。
            let mut inner = Vec::with_capacity(16);
            inner.extend_from_slice(&seeded);
            out.push(inner);
        }
        let outer = out.as_ptr();
        let inner_slots: Vec<(usize, usize)> = out
            .iter()
            .map(|inner| (inner.as_ptr() as usize, inner.capacity()))
            .collect();
        decode_from(&src, &mut out, &opts).unwrap();
        assert_eq!(
            out.as_ptr(),
            outer,
            "packed = {packed}: 嵌套外层不得重新分配"
        );
        assert_eq!(
            out.iter()
                .map(|inner| (inner.as_ptr() as usize, inner.capacity()))
                .collect::<Vec<_>>(),
            inner_slots,
            "packed = {packed}: 内层 Vec 缓冲不得重新分配"
        );
        assert_eq!(out, nested);
        assert_eq!(out, decode::<Vec<Vec<i32>>>(&src, &opts).unwrap());

        // 根 `String`：既有容量恰好等于解码长度，指针与容量都不变。
        let src = bytes_of(&text, packed);
        let mut out = String::with_capacity(text.len());
        assert_eq!(out.capacity(), text.len(), "样本须为恰好容量");
        out.push_str("旧值");
        let ptr = out.as_ptr();
        let capacity = out.capacity();
        decode_from(&src, &mut out, &opts).unwrap();
        assert_eq!(
            (out.as_ptr(), out.capacity()),
            (ptr, capacity),
            "packed = {packed}: 根 String 不得重新分配"
        );
        assert_eq!(out, text);
        assert_eq!(out, decode::<String>(&src, &opts).unwrap());
    }
}

/// `RefCell` 单元格不可用：泄漏 `RefMut`（写侧）与泄漏 `Ref`（读侧，编码侧试借用会成功）。
/// 逐例断言 `1 InvalidArgument`、该单元格未被修改、借用状态未被清除/规整。
#[test]
fn decode_from_rejects_refcell_leaked_borrows() {
    let opts = options(false);
    let src = bytes_of(&RefCell::new(37u32), false);

    let mut out = RefCell::new(5u32);
    std::mem::forget(out.borrow_mut());
    assert_eq!(
        decode_from(&src, &mut out, &opts),
        Err(Error::InvalidArgument)
    );
    assert_eq!(*out.get_mut(), 5, "泄漏 RefMut 时该单元格不得被修改");
    assert!(out.try_borrow_mut().is_err(), "借用状态不得被清除");

    let mut out = RefCell::new(5u32);
    std::mem::forget(out.borrow());
    assert_eq!(
        decode_from(&src, &mut out, &opts),
        Err(Error::InvalidArgument)
    );
    assert_eq!(*out.borrow(), 5, "泄漏 Ref 时该单元格不得被修改");
    assert!(out.try_borrow_mut().is_err(), "借用状态不得被清除");
}

/// `RwLock` 单元格不可用：中毒、被弃写 guard、被弃读 guard。
#[cfg(feature = "std")]
#[test]
fn decode_from_rejects_rwlock_unavailable_cells() {
    use std::sync::RwLock;
    let opts = options(false);
    let src = bytes_of(&RwLock::new(37u32), false);

    let mut out = RwLock::new(5u32);
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = out.write().unwrap();
        panic!("poison the lock while the write guard is held");
    }));
    assert!(poisoned.is_err());
    assert_eq!(
        decode_from(&src, &mut out, &opts),
        Err(Error::InvalidArgument)
    );
    assert_eq!(*out.get_mut().unwrap_or_else(|error| error.into_inner()), 5);
    assert!(out.try_read().is_err(), "poison 标记不得被清除");

    let mut out = RwLock::new(5u32);
    std::mem::forget(out.write().unwrap());
    assert_eq!(
        decode_from(&src, &mut out, &opts),
        Err(Error::InvalidArgument)
    );
    assert_eq!(*out.get_mut().unwrap(), 5, "被弃写 guard 时内容不得被修改");
    assert!(out.try_write().is_err(), "被弃写 guard 不得被规整");

    let mut out = RwLock::new(5u32);
    std::mem::forget(out.read().unwrap());
    assert_eq!(*out.read().unwrap(), 5);
    assert_eq!(
        decode_from(&src, &mut out, &opts),
        Err(Error::InvalidArgument)
    );
    assert_eq!(*out.read().unwrap(), 5, "被弃读 guard 时内容不得被修改");
    assert!(out.try_write().is_err(), "被弃读 guard 不得被规整");
}

/// 复合目标：order 0 普通字段、order 1 为不可用 `RefCell`。断言错误码与该单元格未变；
/// 前序字段按 FR-T09⑤ 不作承诺，不断言。
struct HeadThenCell {
    head: u32,
    cell: RefCell<u32>,
}

impl AceEncode for HeadThenCell {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = self.head.ace_encode(e).and_then(|()| {
            e.begin_field(1)?;
            self.cell.ace_encode(e)
        });
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

impl AceDecode for HeadThenCell {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        let mut out = HeadThenCell {
            head: 0,
            cell: RefCell::new(0),
        };
        Self::ace_decode_into(d, &mut out)?;
        Ok(out)
    }
    fn ace_decode_into(d: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        let payload = d.decode_required_scalar()?;
        d.enter_object(u64::from_le_bytes(payload))?;
        let decoded = (|| -> Result<()> {
            if d.begin_field(0)? {
                <u32 as AceDecode>::ace_decode_into(d, &mut out.head)?;
            }
            if d.begin_field(1)? {
                <RefCell<u32> as AceDecode>::ace_decode_into(d, &mut out.cell)?;
            }
            Ok(())
        })();
        let left = d.leave_object();
        decoded?;
        left
    }
}

#[test]
fn decode_from_rejects_unavailable_cell_after_earlier_field() {
    let opts = options(false);
    let src = bytes_of(
        &HeadThenCell {
            head: 7,
            cell: RefCell::new(99),
        },
        false,
    );
    let mut out = HeadThenCell {
        head: 1,
        cell: RefCell::new(5),
    };
    std::mem::forget(out.cell.borrow_mut());
    assert_eq!(
        decode_from(&src, &mut out, &opts),
        Err(Error::InvalidArgument)
    );
    assert_eq!(*out.cell.get_mut(), 5, "不可用单元格不得被修改");
    assert!(out.cell.try_borrow_mut().is_err(), "借用状态不得被清除");
}

/// 多态 enum 跨变体：旧变体含不可用 `RefCell`，新变体不含该包装 ⇒ 整体替换，不触发检查；
/// 同一 `Option` 目标上的 `Some→Some` 走内层复用（内层 `Vec` 指针与容量都不变），`Some→None`
/// 才是整体替换。
enum Variant {
    Cell(RefCell<u32>),
    Plain(u32),
}

impl AceEncode for Variant {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        match self {
            Variant::Cell(cell) => cell.ace_encode(e),
            Variant::Plain(value) => value.ace_encode(e),
        }
    }
}

impl AceDecode for Variant {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        <u32 as AceDecode>::ace_decode(d).map(Variant::Plain)
    }
}

#[test]
fn decode_from_whole_replacement_skips_unavailable_cells() {
    let opts = options(false);

    let mut out = Variant::Cell(RefCell::new(5));
    match &out {
        Variant::Cell(cell) => std::mem::forget(cell.borrow_mut()),
        Variant::Plain(_) => unreachable!(),
    }
    let src = bytes_of(&Variant::Plain(9), false);
    assert_eq!(decode_from(&src, &mut out, &opts), Ok(()));
    assert!(matches!(out, Variant::Plain(9)));

    let mut out = Some(RefCell::new(5u32));
    std::mem::forget(out.as_ref().unwrap().borrow_mut());
    let src = bytes_of(&None::<RefCell<u32>>, false);
    assert_eq!(decode_from(&src, &mut out, &opts), Ok(()));
    assert_eq!(out, None);

    // 内建 `Option<Vec<T>>` 的 `Some→Some` 复用：内层 `Vec` 指针与容量都不变，且值正确。
    let ints: Vec<i32> = vec![4, 5, 6];
    let src = bytes_of(&Some(ints.clone()), false);
    let mut out: Option<Vec<i32>> = Some(Vec::with_capacity(8));
    out.as_mut().unwrap().extend_from_slice(&[9, 9]);
    let inner = out.as_mut().unwrap();
    let ptr = inner.as_ptr();
    let capacity = inner.capacity();
    decode_from(&src, &mut out, &opts).unwrap();
    let inner = out.as_ref().unwrap();
    assert_eq!(inner.as_ptr(), ptr, "Some→Some 时内层 Vec 不得重新分配");
    assert_eq!(
        inner.capacity(),
        capacity,
        "Some→Some 时内层 Vec 容量不得变化"
    );
    assert_eq!(inner, &ints);
    assert_eq!(out, decode::<Option<Vec<i32>>>(&src, &opts).unwrap());
}

/// derive 整数枚举的未知判别用例（AC-T09⑤ 非法枚举值）：线上按 `i32` 判别，合法值只有 0/1。
#[derive(AceSerialize, Debug, PartialEq)]
#[repr(i32)]
enum Phase {
    Idle = 0,
    Busy = 1,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Stage {
    phase: Phase,
}

/// 截断、非法值（非法 `bool`/`char`/枚举判别/UTF-8/presence）与超限仍返既有码（超限为
/// `6 ResourceLimit`），不 panic、不把错误改成功；`decode_from` 与 `decode` 的错误一致（准入与
/// 校验同一套）。失败后的 `out` 一律不作断言（FR-T09⑤ 不承诺整体不变）。
#[test]
fn decode_from_keeps_existing_failure_codes() {
    let opts = options(false);

    let src = bytes_of(&37u32, false);
    let truncated = &src[..src.len() - 1];
    let mut out = 0u32;
    assert_eq!(
        decode_from(truncated, &mut out, &opts),
        Err(Error::TruncatedInput)
    );
    assert_eq!(
        decode_from(truncated, &mut out, &opts).err(),
        decode::<u32>(truncated, &opts).err()
    );

    let mut src = bytes_of(&true, false);
    *src.last_mut().unwrap() = 2;
    let mut out = false;
    assert_eq!(decode_from(&src, &mut out, &opts), Err(Error::InvalidTag));
    assert_eq!(
        decode_from(&src, &mut out, &opts).err(),
        decode::<bool>(&src, &opts).err()
    );

    // 非法 `char`：码点落在代理区（0xD800）→ 8 InvalidUtf8。根槽 16B 的末 8B 即载荷。
    let mut src = bytes_of(&'中', false);
    let payload = src.len() - 8;
    src[payload..].copy_from_slice(&0xD800u64.to_le_bytes());
    let mut out = 'x';
    assert_eq!(decode_from(&src, &mut out, &opts), Err(Error::InvalidUtf8));
    assert_eq!(
        decode_from(&src, &mut out, &opts).err(),
        decode::<char>(&src, &opts).err()
    );

    // 非法枚举值：derive 整数枚举的未知判别（合法判别为 0/1，此处写 99）→ 4 InvalidTag。
    let mut encoder = Encoder::new(&opts);
    encoder.enter_object().unwrap();
    encoder
        .write_field_scalar(0, 99u64.to_le_bytes())
        .unwrap();
    encoder.leave_object().unwrap();
    let unknown = encoder.finish().unwrap();
    let mut out = Stage { phase: Phase::Busy };
    assert_eq!(decode_from(&unknown, &mut out, &opts), Err(Error::InvalidTag));
    assert_eq!(
        decode_from(&unknown, &mut out, &opts).err(),
        decode::<Stage>(&unknown, &opts).err()
    );

    // 非法 UTF-8：raw 正文里把字符串中的一个字节改成孤立续字节 0xFF → 8 InvalidUtf8。
    let text = String::from("hello ace");
    let mut src = bytes_of(&text, false);
    let body = src
        .windows(3)
        .position(|window| window == b"ace")
        .expect("raw 正文须原样包含字符串字节");
    src[body + 1] = 0xFF;
    let mut out = String::from("旧值");
    assert_eq!(decode_from(&src, &mut out, &opts), Err(Error::InvalidUtf8));
    assert_eq!(
        decode_from(&src, &mut out, &opts).err(),
        decode::<String>(&src, &opts).err()
    );

    // 非法 presence：元素位置只允许 0/1，把它改成 2 → 4 InvalidTag。
    let mut src = bytes_of(&vec![Some(5u32)], false);
    // raw 布局：4B 消息头 + 8B 记录头（id + 尺寸），体内前 4B 是元素个数 ⇒ presence 在其后 1B。
    let presence = 4 + 8 + 4;
    assert_eq!(src[presence], 1, "样本的元素 presence 须为 1");
    src[presence] = 2;
    let mut out: Vec<Option<u32>> = vec![Some(7)];
    assert_eq!(decode_from(&src, &mut out, &opts), Err(Error::InvalidTag));
    assert_eq!(
        decode_from(&src, &mut out, &opts).err(),
        decode::<Vec<Option<u32>>>(&src, &opts).err()
    );

    let ints: Vec<i32> = vec![1, 2, 3];
    let src = bytes_of(&ints, false);
    let mut out: Vec<i32> = vec![9];

    let over_decoded = Options {
        max_decoded_bytes: 0,
        ..options(false)
    };
    assert_eq!(
        decode_from(&src, &mut out, &over_decoded),
        Err(Error::ResourceLimit)
    );
    assert_eq!(
        decode_from(&src, &mut out, &over_decoded).err(),
        decode::<Vec<i32>>(&src, &over_decoded).err()
    );

    let over_collection = Options {
        max_collection_len: 0,
        ..options(false)
    };
    assert_eq!(
        decode_from(&src, &mut out, &over_collection),
        Err(Error::ResourceLimit)
    );
    assert_eq!(
        decode_from(&src, &mut out, &over_collection).err(),
        decode::<Vec<i32>>(&src, &over_collection).err()
    );
}
