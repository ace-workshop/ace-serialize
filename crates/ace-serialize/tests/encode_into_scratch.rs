//! `encode_into` 的最小集成回归：raw/packed 恰好容量、差一字节、`scratch` 不足（含同值/同选项/同起址
//! 的两条负例：arena 不足的 `minimal - S`、槽位不足的槽区上界长度；本层只固定**总长下界**与既有
//! `1` 的返回，第二槽的私有分类由 `src/encode/typed_scratch.rs` 的
//! `second_init_short_one_slot_is_insufficient_entries_with_arena_still_free` 单元测试**独立**验证）
//! 、完整调用零分配，以及身份接线（同一 `Rc`/`Arc` 引用两次、`Weak` 回边、强环 → 14）；⑦①的
//! `scratch` 按本次实际身份条目数 `N × S` 折算，并显式小于 `max_references × S` 的机械预配量。
//!
//! 每个成功用例都与拥有值形态 `encode()` 逐字节对拍：`encode_into` 只换承载（`dst` + `scratch`），
//! 不得改变 wire 字节。

use ace_serialize::encode::Encoder;
use ace_serialize::error::{Error, Result};
use ace_serialize::schema::{value::Guid, AceElement, AceEncode, ElementKind};
use ace_serialize::limits::Options;
use ace_serialize::{
    encode, encode_into, encode_into_probe, identity_entry_align, identity_entry_size,
};
use std::alloc::{GlobalAlloc, Layout, System};
use ace_serialize_derive::AceSerialize;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 失败样例都同时断言「明确报错、不 panic」，因此这些用例本身就是 panic 回归。
const SCRATCH: usize = 64 * 1024;

/// 三类退出见证用例的 `scratch` 预填值：非零填充后，「曾类型化前缀为 0」只可能来自清理
/// (d) 步的写零，不可能来自调用方初值。
const EXIT_FILL: u8 = 0xA5;

/// V2 的 unwind 见证用 panic 载荷：用作 `panic_any` 的载荷，使 `catch_unwind` 能确认捕获到的是
/// 编码回调自身（用户代码）的 panic，而不是其它地方的 panic。
const UNWIND_PANIC: &str = "V2: 编码回调自身 unwind";

fn options(packed: bool) -> Options {
    Options {
        packed,
        ..Options::default()
    }
}

fn expected_bytes<T: AceEncode>(value: &T, packed: bool) -> Vec<u8> {
    let options = options(packed);
    encode(value, &options).expect("拥有值形态必须成功")
}

/// 恰好容量 `dst` + 充足 `scratch`：成功、返回长度正确、与 `encode()` 逐字节一致。
fn assert_same_as_encode<T: AceEncode>(value: &T, packed: bool) {
    let mut scratch = vec![0u8; SCRATCH];
    assert_same_as_encode_in(value, packed, &mut scratch);
}

/// 同 [`assert_same_as_encode`]，但 `scratch` 由调用方提供：V2 的退出后实读必须读**同一块**
/// 缓冲区，自备 `scratch` 的版本读不到。
fn assert_same_as_encode_in<T: AceEncode>(value: &T, packed: bool, scratch: &mut [u8]) {
    let options = options(packed);
    let expected = expected_bytes(value, packed);
    let mut dst = vec![0u8; expected.len()];
    let written = encode_into(value, &mut dst, scratch, &options)
        .unwrap_or_else(|error| panic!("恰好容量 dst 必须成功（packed = {packed}）：{error:?}"));
    assert_eq!(
        written,
        expected.len(),
        "返回长度必须等于最终字节长度（packed = {packed}）"
    );
    assert_eq!(
        dst, expected,
        "encode_into 必须与 encode 逐字节一致（packed = {packed}）"
    );
}

/// V2（`docs/plan.md:194`）的退出后实读见证：整段读一遍 `scratch`，并复核清理 (d) 步的区域语义。
///
/// 两类证据：①整段读一遍（Miri 下这一遍就是「退出后无未初始化/越界/悬垂读」的实读证据）；
/// ②曾类型化前缀的首槽必须为 `0`——`scratch` 由本文件的退出类用例预填 [`EXIT_FILL`]，故该 0
/// 只可能来自清理 (d) 步，同时也就证明该槽在清理前确实被写过（未写过的槽仍是 `0xA5`）。
fn witness_exit_scratch(scratch: &[u8], context: &str) {
    let size = identity_entry_size();
    let align = identity_entry_align();
    let base = scratch.as_ptr() as usize;
    let prefix = (align - base % align) % align;
    assert!(
        scratch.len() >= prefix + size,
        "{context}：scratch 必须容得下一个身份槽（P = {prefix}、S = {size}）"
    );
    let zero_bytes = scratch.iter().filter(|byte| **byte == 0).count();
    assert!(
        zero_bytes >= size,
        "{context}：退出后整段必须可读，且至少一槽已被清理写零（实读 {zero_bytes} 个零字节）"
    );
    assert!(
        scratch[prefix..prefix + size].iter().all(|byte| *byte == 0),
        "{context}：清理必须把曾类型化的首槽写零（预填 {EXIT_FILL:#04x}，故只可能来自 (d) 步）"
    );
}

/// 同起址重试到成功所需的最小 `scratch` 总长：与 [`probe_converges_on_total_need`] 同一重试口径
/// （每次按所报 `need` 扩大、起址不变），并顺带用成功那次的 `dst` 与拥有值形态对拍。
fn minimal_scratch_len<T: AceEncode>(value: &T, packed: bool, backing: &mut [u8]) -> usize {
    minimal_scratch_len_with_steps(value, packed, backing).0
}

/// 同 [`minimal_scratch_len`]，并把逐轮所报 `need` 一并返回（同起址逐步逼近的完整轨迹；末项即成功
/// 那轮实际供给的长度）。
fn minimal_scratch_len_with_steps<T: AceEncode>(
    value: &T,
    packed: bool,
    backing: &mut [u8],
) -> (usize, Vec<usize>) {
    let options = options(packed);
    let expected = expected_bytes(value, packed);
    let base = backing.as_ptr();
    let mut reported: Vec<usize> = Vec::new();
    let mut len = 0usize;
    loop {
        let mut dst = vec![0u8; expected.len()];
        let mut need = usize::MAX;
        assert_eq!(backing.as_ptr(), base, "每次重试必须与首次同起址");
        match encode_into_probe(value, &mut dst, &mut backing[..len], &options, &mut need) {
            Ok(written) => {
                assert_eq!(written, expected.len(), "成功返回值必须等于字节长度");
                assert_eq!(dst, expected, "成功那次的 wire 必须与 encode 逐字节一致");
                return (len, reported);
            }
            Err(Error::InvalidArgument) => {
                assert!(need > len, "need 是同起址所需总长：need = {need}、len = {len}");
                reported.push(need);
                len = need;
            }
            Err(other) => panic!("scratch 不足只允许报既有 1，实得 {other:?}"),
        }
    }
}

/// 本次调用的**实际身份条目数 N**：按 `max_references` 边界实测（N-1 报 `6 ResourceLimit`、
/// N 成功），不假定具体数值。
fn identity_entries_required<T: AceEncode>(value: &T, packed: bool) -> usize {
    let expected = expected_bytes(value, packed);
    for max in 1..=8usize {
        let limited = Options {
            max_references: max,
            ..options(packed)
        };
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        if encode_into(value, &mut dst, &mut scratch, &limited) != Ok(expected.len()) {
            continue;
        }
        let under = Options {
            max_references: max - 1,
            ..options(packed)
        };
        let mut under_dst = vec![0u8; expected.len()];
        let mut under_scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(value, &mut under_dst, &mut under_scratch, &under),
            Err(Error::ResourceLimit),
            "N-1 身份条目名额必须报 6 ResourceLimit（否则边界不唯一）"
        );
        return max;
    }
    panic!("packed = {packed}：8 个身份条目内未成功，样本超出本用例范围");
}

/// V2 的 `Weak` 口径：以**调用前基线**为准——只有基线为零才断言退出后归零，否则只断言回到基线。
fn assert_weak_at_baseline<T>(value: &Rc<T>, baseline: usize, context: &str) {
    let after = Rc::weak_count(value);
    if baseline == 0 {
        assert_eq!(
            after, 0,
            "{context}：基线为零时退出后必须归零（无残留身份 guard）"
        );
    } else {
        assert_eq!(after, baseline, "{context}：基线非零时退出后必须回到基线");
    }
}

struct Leaf {
    v: i32,
}

impl AceEncode for Leaf {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = self.v.ace_encode(e);
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

struct Shared {
    a: Rc<Leaf>,
    b: Rc<Leaf>,
}

impl AceEncode for Shared {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = self
            .a
            .ace_encode(e)
            .and_then(|()| e.begin_field(1))
            .and_then(|()| self.b.ace_encode(e));
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

/// 同 [`Shared`] 的 `Arc` 版本（std 侧）：共享身份 DAG 的零分配/恰好容量样本。
#[cfg(feature = "std")]
struct ArcShared {
    a: std::sync::Arc<Leaf>,
    b: std::sync::Arc<Leaf>,
}

#[cfg(feature = "std")]
impl AceEncode for ArcShared {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = self
            .a
            .ace_encode(e)
            .and_then(|()| e.begin_field(1))
            .and_then(|()| self.b.ace_encode(e));
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

/// V2 的 unwind 样本：先写出真实占用（一个身份槽 + arena），再由**用户代码** panic。
struct PanicsAfterWrites {
    leaf: Rc<Leaf>,
}

impl AceEncode for PanicsAfterWrites {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        self.leaf.ace_encode(e)?;
        e.begin_field(1)?;
        self.leaf.ace_encode(e)?;
        // 编码回调自身的 unwind（用户代码），与清理步骤无关。
        std::panic::panic_any(UNWIND_PANIC);
    }
}

struct BackEdge {
    weak: Weak<Leaf>,
    owner: Rc<Leaf>,
}

impl AceEncode for BackEdge {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = self
            .weak
            .ace_encode(e)
            .and_then(|()| e.begin_field(1))
            .and_then(|()| self.owner.ace_encode(e));
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

/// F4 样本目标：编码自身时读出 `Weak::weak_count` 并写进 wire，使「预留身份转强时多持有
/// 一个 `Weak`」成为可观察的字节差异。
struct CountedLeaf {
    me: RefCell<Weak<CountedLeaf>>,
    seen: Rc<Cell<usize>>,
}

impl AceEncode for CountedLeaf {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let weaks = self.me.borrow().weak_count();
        self.seen.set(weaks);
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = (weaks as u64).ace_encode(e);
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

/// F4 样本：字段 0 先编码 `Weak`（预留身份），字段 1 再编码同一目标的 `Rc`（Reserved → Active）。
struct WeakWatch {
    weak: Weak<CountedLeaf>,
    owner: Rc<CountedLeaf>,
}

impl AceEncode for WeakWatch {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = self
            .weak
            .ace_encode(e)
            .and_then(|()| e.begin_field(1))
            .and_then(|()| self.owner.ace_encode(e));
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

struct Link {
    next: RefCell<Option<Rc<Link>>>,
}

impl AceEncode for Link {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = match &*self.next.borrow() {
            Some(next) => next.ace_encode(e),
            None => e.encode_none(),
        };
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

#[derive(AceSerialize)]
enum Numeric {
    #[ace(tag = 0)]
    Idle,
    #[ace(tag = 7)]
    Busy { id: u32, label: String },
}

#[derive(AceSerialize)]
enum Named {
    #[ace(type_name = "Alarm")]
    Alarm { level: i32 },
    #[ace(type_name = "Quiet")]
    Quiet,
}

#[derive(AceSerialize)]
struct Derived {
    name: String,
    table: BTreeMap<u32, String>,
    maybe: Option<u32>,
    event: Numeric,
    named: Named,
    list: Vec<Numeric>,
}

/// derive 自环样本（同 `tests/derive_s1.rs` 的 `Link`/`Node`）：`Rc` 目标 + 自身 `Weak`。
#[derive(AceSerialize)]
struct CycleLink {
    node: Rc<CycleNode>,
}

#[derive(AceSerialize)]
struct CycleNode {
    value: u32,
    me: Weak<CycleNode>,
}

/// derive 样本：字段经过 `Vec`、`BTreeMap`、`Option` 与两种多态对象头。
fn derived_value() -> Derived {
    Derived {
        name: "hello".to_string(),
        table: BTreeMap::from([(1u32, "one".to_string()), (2, "two".to_string())]),
        maybe: Some(5),
        event: Numeric::Busy {
            id: 9,
            label: "busy".to_string(),
        },
        named: Named::Alarm { level: -3 },
        list: vec![
            Numeric::Idle,
            Numeric::Busy {
                id: 2,
                label: "x".to_string(),
            },
        ],
    }
}

/// V1 补证样本：同一次调用里**同时**走 derive 生成的对象代码（字段 0）与手写
/// `impl AceEncode for Leaf`（字段 1），且手写侧经 `Rc` 写了身份槽——「derive 与手写混用」
/// 路径上确实写槽的既有样本（裸 `Derived` 走的是无身份路径，读回 `scratch` 不会带槽证据）。
struct MixedDeriveAndHand {
    derived: Derived,
    leaf: Rc<Leaf>,
}

impl AceEncode for MixedDeriveAndHand {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = self
            .derived
            .ace_encode(e)
            .and_then(|()| e.begin_field(1))
            .and_then(|()| self.leaf.ace_encode(e));
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

/// derive 生成代码 + 数值 tag/type_name 逃生口多态对象 + Map/String 记录：与拥有值形态对拍。
///
/// V1 补证：本条的值同时含 derive 生成代码与手写 `Leaf` impl（`MixedDeriveAndHand`），且手写侧
/// 写了身份槽，因此 `Ok` 退出后要对同一块 `scratch` 整段实读并复核 `Weak` 基线。
#[test]
fn derived_types_and_polymorphic_objects_match_owned_form() {
    let value = MixedDeriveAndHand {
        derived: derived_value(),
        leaf: Rc::new(Leaf { v: 21 }),
    };
    for packed in [false, true] {
        let baseline = Rc::weak_count(&value.leaf);
        let mut scratch = vec![EXIT_FILL; SCRATCH];
        assert_same_as_encode_in(&value, packed, &mut scratch);
        witness_exit_scratch(
            &scratch,
            &format!("derive 与手写混用（packed = {packed}）"),
        );
        assert_weak_at_baseline(
            &value.leaf,
            baseline,
            &format!("derive 与手写混用（packed = {packed}）"),
        );
    }
}

#[test]
fn raw_and_packed_exact_capacity_match_owned_form() {
    let leaf = Leaf { v: 7 };
    for packed in [false, true] {
        assert_same_as_encode(&leaf, packed);
    }
}

#[test]
fn one_byte_short_dst_fails_with_explicit_error() {
    let leaf = Leaf { v: 7 };
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&leaf, packed);
        assert!(expected.len() > 1);
        let mut dst = vec![0u8; expected.len() - 1];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&leaf, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "差一字节必须明确报错（packed = {packed}），用户 2026-11-18 暂定映射既有 1"
        );
    }
    // V2 ②：普通 `Err` 退出（真实错误：`dst` 差一字节）——清理前已写至少一槽。身份槽在目标
    // 展开前写入（`scratch_resolve_strong_identity`），故「同一 `Rc` 引用两次」的值在收尾失败时
    // 既有已写槽、也已占 arena。
    let shared_leaf = Rc::new(Leaf { v: 9 });
    let shared = Shared {
        a: Rc::clone(&shared_leaf),
        b: Rc::clone(&shared_leaf),
    };
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&shared, packed);
        assert!(expected.len() > 1);
        let baseline = Rc::weak_count(&shared_leaf);
        let mut dst = vec![0u8; expected.len() - 1];
        let mut scratch = vec![EXIT_FILL; SCRATCH];
        assert_eq!(
            encode_into(&shared, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "差一字节必须明确报错（packed = {packed}，含身份条目）"
        );
        witness_exit_scratch(&scratch, &format!("普通 `Err` 退出（packed = {packed}）"));
        assert_weak_at_baseline(
            &shared_leaf,
            baseline,
            &format!("普通 `Err` 退出（packed = {packed}）"),
        );
    }
}

/// ⑦② 的槽位样本：根是 `Vec`，第二槽要等末尾那个**另一个** `Rc` 才申请（`N == 2`），故槽区失败
/// 点落在大量 arena 分配之后、在 `len` 高位可观察。
#[derive(AceSerialize)]
struct Empty {}

fn late_second_slot_graph() -> Vec<Rc<Empty>> {
    let first = Rc::new(Empty {});
    let second = Rc::new(Empty {});
    vec![first, second]
}

/// `late_second_slot_graph` 的 arena 载荷实测值 `(第二槽申请前已占, 整次调用合计)`：由 wire 布局与
/// 选项决定，与 `std` feature 无关（随 feature 变的只有 `S`）。两值与该样本的实测边界见
/// `insufficient_scratch_with_exact_dst_returns_invalid_argument` 的数值表。
fn empty_vec_arena_payload(packed: bool) -> (usize, usize) {
    if packed {
        (1504, 1584)
    } else {
        (1440, 1520)
    }
}

#[test]
fn insufficient_scratch_with_exact_dst_returns_invalid_argument() {
    let leaf = Leaf { v: 7 };
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&leaf, packed);
        let mut dst = vec![0u8; expected.len()];
        // 空 scratch：连对象帧/根槽的 arena 都放不下 ⇒ 容量类错误映射既有 1。
        let mut scratch: Vec<u8> = Vec::new();
        assert_eq!(
            encode_into(&leaf, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "scratch 不足必须返回 1（packed = {packed}）"
        );
    }
    // ⑦② 的两类容量不足：**槽位不足**（身份槽区被 arena 占用挤到少一槽）与 **arena 不足**。两者都
    // 只以既有 `1` 表现在同一块缓冲上，集成层能观察到的区分式只有**所报总长下界** `need`：
    //   `need == minimal` ⇒ 失败点所需总长恰为整次调用的最小可用长度，即 arena 分配不足；
    //   `need <  minimal` ⇒ 失败点早于整次调用所需，**不能**单独据此判定是槽位（任何早于后续分配
    //   的 arena 下界都 `< minimal`）。下面的两条负例各用一侧：本节专取 `need == minimal` 的 arena
    //   不足，本节之后的 `late_second_slot_graph` 样本取槽区区间内的长度、断言 `need < minimal`。
    //
    // 本节样本：一个**以身份槽开启的根 `Rc` 图**（根 `Rc` 先开第一槽、内部共享 `Rc` 再开第二槽，
    // `N == 2`），用**同一个值 / 同一组选项 / 同一块缓冲区的起址**给出一对：
    //   正例：从零长起按探测入口所报 `need` 同起址逐步扩容，落在 `minimal` 时成功且 wire 与
    //         `encode()` 逐字节一致（即“按 `need` 供给即可成功”）；
    //   负例：同一块缓冲只开到 `minimal - S`。
    // 集成层看不到私有分类（`InitError`/`shortfall` 不出模块），因此这里不宣称任何失败的分类。
    // 可验证的只是一个**排除**：`need` 报 `minimal` 而不是 `< minimal` 的某个值，即排除「首个失败落在
    // 第二槽申请点」——第二槽的 `init` 若失败，它记的下界是 `P + 2S + payload_so_far`，而该槽的目标
    // （`Rc<Leaf>`）在其 `init` 之后才写信封与记录 ⇒ `payload_so_far < payload_total`；成功又需要
    // `minimal >= P + 2S + payload_total` ⇒ 该下界 `< minimal`，而 `short + 1 < minimal`（`S >= 2`）
    // ⇒ 若失败在第二槽，`need` 必 `< minimal`。实得 `need == minimal = short + S`，故本样本的
    // `minimal - S` 上首个失败点**在 arena 分配、不在第二槽申请点**——这条负例是 arena 不足的例子，
    // 不是槽位证据。
    // 该区分式只**单向**可用：`need == minimal` 可排除槽位失败，`need < minimal` 则不能反过来确认
    // 槽位失败。反例即本节之后的 `Vec<Rc<Empty>>` 样本：raw `minimal = 1616`、`minimal - S = 1568` 时
    // `need = 1600 = minimal - 16`，而首个失败点是 64B 的 arena 分配（当时 `B == 1440`、
    // `C(B) == 2 == N`，两槽都已写入），`-16` 来自其后一次 16B arena 分配未计入该点下界（packed 同形：
    // `minimal = 1680`、`minimal - S = 1632`、`need = 1664`）。所以那个样本的 arena 负例不满足
    // `need == minimal`，不能当 arena 证据；它的真实价值是下面的槽区区间。
    // 第二槽的私有分类（`InsufficientEntries` 与 `ArenaCollision` 两支、且失败当时 arena 仍有余量）
    // 由 `src/encode/typed_scratch.rs` 的单元测试
    // `second_init_short_one_slot_is_insufficient_entries_with_arena_still_free` 在 `len = P + S` 上
    // **独立**验证；它是「私有分类＝`InsufficientEntries`」存在的独立证据，不是对本集成样本失败点的
    // 跨样本完整归因（两者不在同一长度上观测同一件事）。
    let leaf = Rc::new(Leaf { v: 9 });
    let graph = Rc::new(Shared {
        a: Rc::clone(&leaf),
        b: Rc::clone(&leaf),
    });
    let entry_size = identity_entry_size();
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&graph, packed);
        assert_eq!(
            identity_entries_required(&graph, packed),
            2,
            "根 Rc 先开第一槽、内部共享 Rc 再开第二槽（packed = {packed}）"
        );
        let mut backing = vec![0u8; SCRATCH];
        let base = backing.as_ptr();
        let (minimal, steps) = minimal_scratch_len_with_steps(&graph, packed, &mut backing);
        assert_eq!(
            steps.last().copied(),
            Some(minimal),
            "正例：成功那轮供给的正是上一轮所报的 `need`，`need` 逐轮严格增长（{steps:?}）"
        );
        assert_eq!(backing.as_ptr(), base, "正例与负例必须同一块缓冲区的起址");
        let short = minimal - entry_size;
        assert_eq!(minimal - short, entry_size, "缩短量恰为一个身份槽 S");
        // 负例（arena 不足）：同值、同选项、同起址，`dst` 恰好容量，仅 `scratch` 少一个 `S`。
        let mut dst = vec![0u8; expected.len()];
        assert_eq!(
            encode_into(&graph, &mut dst, &mut backing[..short], &options),
            Err(Error::InvalidArgument),
            "少一个身份槽必须返回既有 1、不 panic（本例失败于 arena，不是槽位证据；packed = {packed}）"
        );
        // 同起址、同长度的探测：所报 `need` 就是正例所需的全长 `minimal = short + S`。
        let mut dst = vec![0u8; expected.len()];
        let mut need = usize::MAX;
        assert_eq!(
            encode_into_probe(&graph, &mut dst, &mut backing[..short], &options, &mut need),
            Err(Error::InvalidArgument),
            "少一个身份槽时探测入口同样返回 1（packed = {packed}）"
        );
        assert_eq!(
            need, minimal,
            "区分式（arena）：`need == minimal == short + S`（本例失败于 arena，不在第二槽申请点；packed = {packed}）"
        );
        // 总长侧对照（不涉归因）：同一 `short` 长度下，零身份、arena 需求更小
        // （`control_minimal < short`）的样本成功——该长度对另一个样本是可用的；这只说明 `short`
        // 并非普遍不可用，不说明本样本差的是身份槽，也不替任何失败点定位。
        let control: Vec<String> = (0..8).map(|_| String::from("abc")).collect();
        let mut control_backing = vec![0u8; SCRATCH];
        let control_minimal = minimal_scratch_len(&control, packed, &mut control_backing);
        assert!(
            control_minimal < short,
            "对照样本的最小 `scratch`（{control_minimal}B）必须小于该长度（{short}B）"
        );
        let control_expected = expected_bytes(&control, packed);
        let mut control_dst = vec![0u8; control_expected.len()];
        assert_eq!(
            encode_into(
                &control,
                &mut control_dst,
                &mut control_backing[..short],
                &options
            ),
            Ok(control_expected.len()),
            "同长度下零身份对照必须成功（packed = {packed}）"
        );
        assert_eq!(control_dst, control_expected);
    }
    // ⑦② 的第二条负例：**槽位不足**（身份槽区被 arena 占用挤到少一槽）。样本换成
    // `Vec<Rc<Empty>>`：它的第二槽要等末尾那个**另一个** `Rc` 才申请，槽区失败点因此落在大量 arena
    // 分配之后、在 `len` 高位可观察（上一个样本的第二槽开在编码早期，`minimal - S` 早已越过它的
    // 槽区区间，这正是上面 `need == minimal` 只能作排除、不能作确认的原因）。
    //
    // 实测（同一块缓冲区、起址对齐 `P == 0`；样本的 arena 载荷与选项绑定、与 `std` feature 无关，
    // 随 feature 变的只有 `S`）：
    //   态            槽区上界 `len`  `need = 2S + 已占载荷`  `minimal = 2S + 载荷合计`
    //   默认 raw       1535            1536（2·48 + 1440）   1616（2·48 + 1520）
    //   默认 packed    1599            1600（2·48 + 1504）   1680（2·48 + 1584）
    //   `std` raw      1551            1552（2·56 + 1440）   1632（2·56 + 1520）
    //   `std` packed   1615            1616（2·56 + 1504）   1696（2·56 + 1584）
    // 断言只用运行时 `S` 与 `empty_vec_arena_payload` 的两个载荷值推出上表，不写死四个 `len`。
    // 该长度上 `C(B) == 1 < N == 2 <= C(0)`：失败的私有分支是「与当前 arena 占用相撞」那一支（集成层
    // 看不到私有值，所以这里只用下界与阶梯刻画）；`N > C(0)` 那一支（`InsufficientEntries`）由上面提到
    // 的单元测试在 `len = P + S` 上独立固定。
    let sample = late_second_slot_graph();
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&sample, packed);
        assert_eq!(
            identity_entries_required(&sample, packed),
            2,
            "两个不同 `Rc` 各占一槽（packed = {packed}）"
        );
        let (payload_before, payload_total) = empty_vec_arena_payload(packed);
        let mut backing = vec![0u8; SCRATCH];
        let base = backing.as_ptr();
        assert_eq!(
            (base as usize) % identity_entry_align(),
            0,
            "上表的长度值以对齐前导 `P == 0` 为前提（packed = {packed}）"
        );
        // 正例（与下面两条阶梯负例同值/同选项/同起址）：`minimal` 时成功、wire 与 `encode()` 逐字节
        // 一致（helper 内已断言），且所报全长与载荷合计吻合。
        let (minimal, _steps) = minimal_scratch_len_with_steps(&sample, packed, &mut backing);
        assert_eq!(backing.as_ptr(), base, "正例与两条阶梯负例必须同一块缓冲区的起址");
        assert_eq!(
            minimal,
            2 * entry_size + payload_total,
            "正例所用全长 = 2S + 载荷合计（packed = {packed}）"
        );
        // 正例的零分配证据（`docs/plan.md`「`encode_into` scratch」①：恰好 `dst` + 按实际所需供给
        // `scratch` + 入口到返回零分配是**同一次调用**上的断言）：`dst` 恰好容量、`scratch` 恰好
        // `minimal`、零分配、返回长度正确，一次核完。先用同一次的实际所需量核对它低于按
        // `max_references` 机械预配的门槛，再把同一块 `backing` 的前 `minimal` 字节喂给这次调用。
        let mechanical = Options::default().max_references * entry_size;
        assert!(
            minimal < mechanical,
            "按实际所需供给的 scratch（{minimal}B）必须严格小于 max_references × S（{mechanical}B，\
             packed = {packed}）"
        );
        assert_zero_alloc_at(&sample, packed, &mut backing[..minimal]);
        // 阶梯①（**不是**槽位证据）：`S + 已占载荷 - 1` 上**已写入 1 个槽**（`initialized == 1`），
        // 首槽早已落槽——首槽在编码早期 arena 还几乎空着时就申请了；先失败的是其后 **block 描述符
        // 表的 arena 预留**（`slab_reserve` 首次扩容 = `8 × BLOCK_STRIDE(40) = 320B`，`SLAB_ALIGN`）。
        // 该点的实际 arena 载荷是 `payload_before − 320`，故槽容量
        // `C(B) = (first_len − (payload_before − 320)) / S = (S + 319) / S`——`std` 下 `S == 56` 时为
        // 6、默认下 `S == 48` 时为 7，**仍远大于 1**：该长度上槽位并不吃紧，失败点不在槽位。所报
        // 下界 `S + 已占载荷` 是该失败点的 checked 下界：当时已分配载荷 + 本次 `320B` 恰为已占载荷，
        // 再加已写入那一个槽的 `S`。
        let first_len = entry_size + payload_before - 1;
        let mut dst = vec![0u8; expected.len()];
        let mut need = usize::MAX;
        assert_eq!(
            encode_into_probe(&sample, &mut dst, &mut backing[..first_len], &options, &mut need),
            Err(Error::InvalidArgument),
            "block 描述符预留不下同样只报既有 1、不 panic（packed = {packed}）"
        );
        assert_eq!(
            need,
            entry_size + payload_before,
            "该 arena 预留点的下界 = S + 已占载荷（packed = {packed}）"
        );
        // 阶梯②（槽位不足负例）：`2S + 已占载荷 - 1` 上首槽装得下（`initialized == 1`）、第二槽
        // `init` 装不下，故 `need < minimal`。该长度的槽位刻画只是由 `slot_need = 2S + payload_before`
        // 与 `slot_len = slot_need - 1` 的定义得出的恒等式 `(slot_len − payload_before) / S == 1`
        // （整数除法）；`payload_before` 是已分配载荷、**不是**含对齐填充的 `B`，故该恒等式**不是**
        // 对真实 `B` 的观测；「失败点落在第二槽 `init`」由下面的下界算式取证：`need == 2S + 已占载荷`
        // 正是第二槽 `init` 的下界
        // `(initialized + 1) × S + 已分配载荷`（`initialized == 1`）的取值，而第二槽一旦装得下、
        // 失败点后移到其后的 arena 分配，下界必然严格大于该值（见下面的区间上界自检）。
        let slot_need = 2 * entry_size + payload_before;
        let slot_len = slot_need - 1;
        let mut dst = vec![0u8; expected.len()];
        assert_eq!(
            encode_into(&sample, &mut dst, &mut backing[..slot_len], &options),
            Err(Error::InvalidArgument),
            "槽区少一槽必须返回既有 1、不 panic（packed = {packed}）"
        );
        let mut dst = vec![0u8; expected.len()];
        let mut need = usize::MAX;
        assert_eq!(
            encode_into_probe(&sample, &mut dst, &mut backing[..slot_len], &options, &mut need),
            Err(Error::InvalidArgument),
            "槽区少一槽时探测入口同样返回 1（packed = {packed}）"
        );
        assert_eq!(
            need, slot_need,
            "槽位失败点的下界 = 2S + 已占载荷（packed = {packed}）"
        );
        assert!(
            need < minimal,
            "区分式（槽位）：need = {need} 必须 < minimal = {minimal}（packed = {packed}）"
        );
        // 区间上界自检（用**本次调用的实际结果**证明失败点后移，而不是只比数值大小）：再长一字节到
        // `slot_need`，两槽都装得下（`C(B) == 2 == N`），失败点后移为其后的一次 arena 分配——根槽写入
        // `assist` 那次预留（`grown_capacity` 以 `max(64)` 起配 ⇒ `64B`）。故所报下界恰为
        // `slot_need + 64`：既离开了槽位失败点的 `slot_need`，也不是「槽申请多拒一字节」的
        // `slot_need + 1`，且 `after` 由本次调用写入而非调用方哨兵值。
        let mut dst = vec![0u8; expected.len()];
        let mut after = usize::MAX;
        assert_eq!(
            encode_into_probe(&sample, &mut dst, &mut backing[..slot_need], &options, &mut after),
            Err(Error::InvalidArgument),
            "两槽装得下后仍失败于其后的 arena 预留，只报既有 1（packed = {packed}）"
        );
        assert!(
            after > slot_need,
            "失败点后移后所报下界必须变大：after = {after} > slot_need = {slot_need}（packed = {packed}）"
        );
        assert_eq!(
            after,
            slot_need + 64,
            "后移后的失败点是 64B 的根槽 `assist` 预留 ⇒ 下界 = slot_need + 64（实得 {after}；只报 \
             slot_need 或 slot_need + 1 都说明第二槽仍被拒，packed = {packed}）"
        );
    }
}

#[test]
fn same_rc_referenced_twice_shares_one_identity() {
    let leaf = Rc::new(Leaf { v: 9 });
    let value = Shared {
        a: Rc::clone(&leaf),
        b: Rc::clone(&leaf),
    };
    for packed in [false, true] {
        // V2 ①：`Ok` 退出后对同一块 `scratch` 整段实读，清理 (d) 步须已把首槽写零。
        let baseline = Rc::weak_count(&leaf);
        let mut scratch = vec![EXIT_FILL; SCRATCH];
        assert_same_as_encode_in(&value, packed, &mut scratch);
        witness_exit_scratch(&scratch, &format!("`Ok` 退出（packed = {packed}）"));
        assert_weak_at_baseline(&leaf, baseline, &format!("`Ok` 退出（packed = {packed}）"));
    }
}

/// V2 ③：**捕获 unwind** 退出（`catch_unwind` 包住的手写回调）。
///
/// 三类退出中只有这一类没有可并入的既有用例：既有用例全部以 `Ok`/`Err` 返回，没有一个让编码
/// 回调 panic。panic 本身属**用户代码**（与「清理步骤不得 panic」是两件事）：这里的 panic 是
/// `PanicsAfterWrites` 自己发的，清理步骤（`drop_prefix`/`reinit_typed_range`）不执行用户代码、
/// 无需 unwind——若清理在 unwind 中 panic，二次 panic 会直接 abort，下面的实读断言根本走不到。
#[test]
fn callback_unwind_leaves_cleaned_scratch_after_written_slots() {
    let leaf = Rc::new(Leaf { v: 13 });
    let value = PanicsAfterWrites {
        leaf: Rc::clone(&leaf),
    };
    for packed in [false, true] {
        let options = options(packed);
        let baseline = Rc::weak_count(&leaf);
        let mut dst = vec![0u8; 4096];
        let mut scratch = vec![EXIT_FILL; SCRATCH];
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encode_into(&value, &mut dst, &mut scratch, &options)
        }));
        let payload = caught.expect_err("编码回调的 panic 必须被 catch_unwind 捕获");
        assert_eq!(
            payload.downcast_ref::<&'static str>().copied(),
            Some(UNWIND_PANIC),
            "捕获到的必须是编码回调自身的 panic（packed = {packed}）"
        );
        witness_exit_scratch(&scratch, &format!("捕获 unwind 退出（packed = {packed}）"));
        assert_weak_at_baseline(
            &leaf,
            baseline,
            &format!("捕获 unwind 退出（packed = {packed}）"),
        );
    }
}

#[test]
fn weak_back_edge_encodes_same_wire_as_owned_form() {
    let leaf = Rc::new(Leaf { v: 11 });
    let value = BackEdge {
        weak: Rc::downgrade(&leaf),
        owner: Rc::clone(&leaf),
    };
    for packed in [false, true] {
        // V1 补证：`Weak` 反向边同样要走过写槽/预留身份，故 `Ok` 退出后对同一块 `scratch`
        // 整段实读；`Weak` 基线按调用前取值（本样本为一条 `downgrade`）。
        let baseline = Rc::weak_count(&leaf);
        let mut scratch = vec![EXIT_FILL; SCRATCH];
        assert_same_as_encode_in(&value, packed, &mut scratch);
        witness_exit_scratch(&scratch, &format!("`Weak` 反向边（packed = {packed}）"));
        assert_weak_at_baseline(
            &leaf,
            baseline,
            &format!("`Weak` 反向边（packed = {packed}）"),
        );
    }
}

#[test]
fn strong_cycle_returns_cycle_unsupported() {
    let node = Rc::new(Link {
        next: RefCell::new(None),
    });
    *node.next.borrow_mut() = Some(Rc::clone(&node));
    let options = options(false);
    let mut dst = vec![0u8; 4096];
    let mut scratch = vec![0u8; SCRATCH];
    assert_eq!(
        encode_into(&node, &mut dst, &mut scratch, &options),
        Err(Error::CycleUnsupported)
    );
    assert_eq!(encode(&node, &options), Err(Error::CycleUnsupported));
    *node.next.borrow_mut() = None;
}

#[test]
fn packed_runs_and_long_records_match_owned_form() {
    // 600 个元素：既覆盖多 word 记录，也覆盖 `00`/`ff` run 与 255 上限的拆段。
    for packed in [false, true] {
        for value in [vec![-1i32; 600], vec![0i32; 600]] {
            assert_same_as_encode(&value, packed);
        }
    }
}

/// F2（R15）：`Rc<String>` 的 16B 字符串合法 raw 恰为 48B（信封 8 + 记录 8+16 + 根槽 16）。
/// 提交预检若把已弹出的待提交记录重复计费（算成 56B），这条合法预算与恰好 `dst` 会被误拒。
#[test]
fn root_identity_record_budget_is_not_charged_twice() {
    let value = Rc::new("0123456789abcdef".to_string());
    let raw = encode(
        &value,
        &Options {
            packed: false,
            max_raw_bytes: 48,
            ..Options::default()
        },
    )
    .expect("拥有值形态在合法 raw 预算 48B 必须成功");
    assert_eq!(raw.len(), 48, "该样本的合法 raw 长度是反例前提");
    for packed in [false, true] {
        let options = Options {
            packed,
            max_raw_bytes: 48,
            ..Options::default()
        };
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Ok(expected.len()),
            "恰好容量 dst 与合法 raw 预算下必须成功（packed = {packed}）"
        );
        assert_eq!(dst, expected);
    }
    // 复审补证：同一用例的 raw/packed 两模式各加 `len+1`——返回长度仍是 `len`，输出前缀不变，
    // 多出的尾字节保持调用方原值（不得被库写零/越界写）。
    for packed in [false, true] {
        let options = Options {
            packed,
            max_raw_bytes: 49,
            ..Options::default()
        };
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0xAAu8; expected.len() + 1];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Ok(expected.len()),
            "raw 预算放宽一字节时返回长度不得随之变化（packed = {packed}）"
        );
        assert_eq!(
            &dst[..expected.len()],
            expected.as_slice(),
            "输出前缀必须与恰好容量一致（packed = {packed}）"
        );
        assert_eq!(
            dst[expected.len()],
            0xAA,
            "多出的一字节必须保持调用方原值（packed = {packed}）"
        );
    }
}

/// F2：根身份目标记录只算一条 —— `max_records = 1` 时 `Rc<Vec<u32>>([37])` 不得被算成两条。
#[test]
fn single_root_identity_record_uses_one_record_slot() {
    let value = Rc::new(vec![37u32]);
    for packed in [false, true] {
        let options = Options {
            packed,
            max_records: 1,
            ..Options::default()
        };
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Ok(expected.len()),
            "一条根身份记录不得占用两条记录名额（packed = {packed}）"
        );
        assert_eq!(dst, expected);
    }
}

/// F3：回调用 `Encoder::new` 替换传入编码器后正常返回 —— 收尾必须明确报错，
/// 不得因拿不到 `StoreCap` 而 panic。
struct ReplacedEncoder;

impl AceEncode for ReplacedEncoder {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        *e = Encoder::new(&Options::default());
        Ok(())
    }
}

/// F3：回调换出借用编码器并对它调用公开 `finish` —— 必须返回明确错误，
/// 不得 debug 断言 panic，也不得在 release 落入空的拥有值组装。
struct FinishesBorrowed {
    outcome: Cell<Option<Error>>,
}

impl AceEncode for FinishesBorrowed {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let borrowed = core::mem::replace(e, Encoder::new(&Options::default()));
        self.outcome.set(borrowed.finish().err());
        Err(Error::InvalidArgument)
    }
}

#[test]
fn replaced_encoder_fails_explicitly_instead_of_panicking() {
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&ReplacedEncoder, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "替换编码器后收尾必须明确报错（packed = {packed}）"
        );
    }
}

/// F3：同一替换样本放进 `Rc` —— 默认 `ace_encode_target` 会进入对象捕获，回调替换编码器后
/// 捕获/身份收尾不得在缺后端时 panic，必须返回明确错误（raw/packed 两态）。
#[test]
fn rc_wrapped_replaced_encoder_errors_instead_of_panicking() {
    let value = Rc::new(ReplacedEncoder);
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "Rc 内的替换回调同样必须明确报错而不是 panic（packed = {packed}）"
        );
    }
}

/// V：公开 `Encoder<'a>` 必须保持协变 —— `Encoder<'static>` 能按值缩短为 `Encoder<'short>`。
/// 本用例的价值在编译期：若寿命参数变为不变，函数体无法通过类型检查。
#[test]
fn encoder_lifetime_stays_covariant() {
    fn shorten<'short>(encoder: Encoder<'static>) -> Encoder<'short> {
        encoder
    }
    let _ = shorten;
}

#[test]
fn public_finish_on_a_swapped_out_borrowed_encoder_errors() {
    let value = FinishesBorrowed {
        outcome: Cell::new(None),
    };
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "换出借用编码器后入口必须明确报错（packed = {packed}）"
        );
        assert_eq!(
            value.outcome.get(),
            Some(Error::InvalidArgument),
            "公开 finish 对借用编码器必须报错而不是输出空拥有值结果（packed = {packed}）"
        );
    }
}

/// F4：预留身份转强时的 `Weak` 持有量必须与拥有值路径一致 —— 读取 `Weak::weak_count`
/// 的手写回调不得观察到差异（否则 wire 字节不同）。
#[test]
fn reserved_identity_promotion_holds_no_extra_weak() {
    let seen = Rc::new(Cell::new(0));
    let owner: Rc<CountedLeaf> = Rc::new_cyclic(|me| CountedLeaf {
        me: RefCell::new(me.clone()),
        seen: Rc::clone(&seen),
    });
    let value = WeakWatch {
        weak: Rc::downgrade(&owner),
        owner: Rc::clone(&owner),
    };
    for packed in [false, true] {
        // V1 补证：`Rc::new_cyclic` 的受支持自环（节点持有自身 `Weak`）路径同样要走过写槽 /
        // 预留身份转强，故 `Ok` 退出后对同一块 `scratch` 整段实读。
        let baseline = Rc::weak_count(&owner);
        let mut scratch = vec![EXIT_FILL; SCRATCH];
        assert_same_as_encode_in(&value, packed, &mut scratch);
        witness_exit_scratch(&scratch, &format!("受支持环 new_cyclic（packed = {packed}）"));
        assert_weak_at_baseline(
            &owner,
            baseline,
            &format!("受支持环 new_cyclic（packed = {packed}）"),
        );
    }
    assert!(seen.get() > 0, "手写回调必须真的读到弱引用计数");
}

/// 最小计数 allocator：只统计**当前线程**在 `COUNTING` 打开期间的分配次数，
/// 计数区间之外与其它测试线程的分配照常放行（测试默认并行运行）。
struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    let _ = COUNTING.try_with(|flag| {
        if flag.get() {
            ALLOCATIONS.with(|counter| counter.set(counter.get() + 1));
        }
    });
}

// SAFETY: 只统计调用次数后原样转发给 `System`，不改变分配语义、不持有指针。
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: 由本 trait 的契约要求，`layout` 已由调用方保证有效。
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` 来自本 allocator 的 `alloc`，`layout` 与分配时一致。
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: 同上，原样转发给 `System`。
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// 在计数窗口内跑一次 `encode_into`：断言入口到返回零分配、返回长度正确、与 `encode()` 逐字节一致。
fn assert_zero_alloc<T: AceEncode>(value: &T) {
    for packed in [false, true] {
        let mut scratch = vec![0u8; SCRATCH];
        assert_zero_alloc_at(value, packed, &mut scratch);
    }
}

/// 同 [`assert_zero_alloc`]，但 `scratch` 由调用方按本次所需长度提供（⑦①「同一次调用」用）。
fn assert_zero_alloc_at<T: AceEncode>(value: &T, packed: bool, scratch: &mut [u8]) {
    let options = options(packed);
    let expected = expected_bytes(value, packed);
    let mut dst = vec![0u8; expected.len()];
    ALLOCATIONS.with(|counter| counter.set(0));
    COUNTING.with(|flag| flag.set(true));
    let written = encode_into(value, &mut dst, scratch, &options);
    COUNTING.with(|flag| flag.set(false));
    let observed = ALLOCATIONS.with(Cell::get);
    assert_eq!(observed, 0, "入口到返回不得分配（packed = {packed}）");
    assert_eq!(written, Ok(expected.len()));
    assert_eq!(dst, expected);
}

#[test]
fn full_call_allocates_nothing() {
    let leaf = Rc::new(Leaf { v: 13 });
    assert_zero_alloc(&Shared {
        a: Rc::clone(&leaf),
        b: Rc::clone(&leaf),
    });
    // 开头③：共享身份 DAG 的计数证据补 `Arc` 侧（std 专属；`Rc` 侧见上面的 `Shared`）。
    #[cfg(feature = "std")]
    {
        use std::sync::Arc;
        let arc = Arc::new(Leaf { v: 13 });
        assert_zero_alloc(&ArcShared {
            a: Arc::clone(&arc),
            b: Arc::clone(&arc),
        });
    }
    // ⑦①：同一次调用上同时断言「按**本次实际身份条目数 N** 折算的 `scratch`」+「恰好 `dst`」
    // +「零分配」。`scratch.len()` 按 `N × S`（`S = identity_entry_size()`）+ 非身份区折算，并
    // 显式断言它严格小于「按 `max_references` 机械预配」的 `max_references × S`。
    let shared = Shared {
        a: Rc::clone(&leaf),
        b: Rc::clone(&leaf),
    };
    let entry_size = identity_entry_size();
    for packed in [false, true] {
        let identity_entries = identity_entries_required(&shared, packed);
        assert_eq!(identity_entries, 1, "同一 Rc 两次引用只占一个身份条目");
        let mut backing = vec![0u8; SCRATCH];
        let minimal = minimal_scratch_len(&shared, packed, &mut backing);
        let non_identity = minimal - identity_entries * entry_size;
        assert!(non_identity > 0, "样本必须有非身份区（对齐前导 + arena）");
        let supplied = identity_entries * entry_size + non_identity;
        let mechanical = Options::default().max_references * entry_size;
        assert!(
            supplied < mechanical,
            "按实际条目数折算的 scratch（{supplied}B）必须严格小于 max_references × S（{mechanical}B）"
        );
        assert_zero_alloc_at(&shared, packed, &mut backing[..supplied]);
    }
    // 经过集合的既有样本：集合体的 `u32 count` 前缀与逐项元素都不得由库分配。
    assert_zero_alloc(&vec![
        Numeric::Idle,
        Numeric::Busy {
            id: 2,
            label: "y".to_string(),
        },
    ]);
    // F1（R14）：命中库内建 bulk 的集合样本——整段体不得为合成记录体构造 `Vec`。
    assert_zero_alloc(&vec![1i32, -2, 3, 0, i32::MIN]);
    // derive 生成代码 + `Vec`/`BTreeMap`/`Option`/多态对象：入口到返回同样零分配。
    assert_zero_alloc(&derived_value());
    // derive 自环样本：`Rc` 目标 + 自身 `Weak` 的预留/转强全程零分配。
    let cycle = CycleLink {
        node: Rc::new_cyclic(|me| CycleNode {
            value: 3,
            me: Weak::clone(me),
        }),
    };
    assert_zero_alloc(&cycle);
    // V1 补证：本条才是真正编码回边的自环——`CycleNode.me` 本身是被序列化的字段，编码它就会
    // 反查 `node` 的身份槽。`Ok` 退出后用**同一块**预填 `scratch` 整段实读，并复核 `Weak` 回到
    // 调用前基线（基线非零：节点自己持有该 `Weak`）。
    for packed in [false, true] {
        let baseline = Rc::weak_count(&cycle.node);
        let mut scratch = vec![EXIT_FILL; SCRATCH];
        assert_same_as_encode_in(&cycle, packed, &mut scratch);
        witness_exit_scratch(&scratch, &format!("derive 自环 CycleLink（packed = {packed}）"));
        assert_weak_at_baseline(
            &cycle.node,
            baseline,
            &format!("derive 自环 CycleLink（packed = {packed}）"),
        );
    }
    // 计数器自检：钩子确实生效（否则上面的 0 会平凡通过）。
    ALLOCATIONS.with(|counter| counter.set(0));
    COUNTING.with(|flag| flag.set(true));
    let mut probe: Vec<u8> = Vec::new();
    probe.push(1);
    COUNTING.with(|flag| flag.set(false));
    assert!(ALLOCATIONS.with(Cell::get) > 0, "计数 allocator 必须能观测到分配");
}


/// B1(a)：手写 bulk hook 对**合法空集合**返回 `true`、自身不写也不分配时，库侧不得为
/// 「4B 计数 + 元素体」新建合成 `Vec`：计数写在栈上，元素字节直接拼进记录。
///
/// 钩子按用户裁决④（`docs/architecture.md` §2.1.6）**不依赖** `out` 的初始 `len`/`capacity`：
/// 空集合什么都不写，因此初值不影响结果。
static HAND_BULK_CALLS: AtomicUsize = AtomicUsize::new(0);

struct HandBulk;

impl AceEncode for HandBulk {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        self.ace_encode_element(e)
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&[0])
    }
    fn ace_encode_elements_bulk(items: &[Self], _out: &mut Vec<u8>) -> Result<bool> {
        assert!(items.is_empty(), "本用例只构造空集合");
        HAND_BULK_CALLS.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
}

/// B1(b)：借用路径的内建定宽元素必须**整段**拷进记录体（`u32 count` 走栈上，元素字节一次性
/// 拷入）。若仍经公开 `&mut Vec<u8>` hook 合成记录体，记账 allocator 会看到一次分配；若改回
/// 逐元素写入，本用例的 wire 比对仍会通过、但 `crate::hooks` 的元素计数会变（见仓内
/// `borrowed_bulk_body_is_one_bulk_and_no_element_calls`），故两条一起才辨别「伪 bulk」。
#[test]
fn borrowed_path_copies_builtin_bulk_segments_whole() {
    // 元素宽度 1/4/8 与 16B 透明 newtype；都满足「无 padding、内存字节即小端 wire 字节」。
    assert_zero_alloc(&vec![1i32, -2, 3, 0, i32::MIN]);
    assert_zero_alloc(&vec![u64::MAX, 0, 7]);
    assert_zero_alloc(&vec![1.5f64, -0.0, f64::MIN]);
    assert_zero_alloc(&vec!['中', '\0', char::MAX]);
    assert_zero_alloc(&vec![true, false, true]);
    assert_zero_alloc(&vec![Guid([3; 16]), Guid([9; 16])]);
    // 指针宽度元素只在 64 位小端有整段体（与 `AceElement for isize/usize` 的 cfg 一致）。
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    {
        assert_zero_alloc(&vec![isize::MIN, 0, 7]);
        assert_zero_alloc(&vec![usize::MAX, 1]);
    }
    // 空集合同样零分配：计数仍走栈上，且不得为空的整段体新建缓冲。
    assert_zero_alloc(&Vec::<i32>::new());
}

/// B2（用户 2026-11-18 裁决 A）：两个 `#[doc(hidden)]` 直写方法只在「内存字节 == 小端 wire
/// 字节」的内建定宽元素上给出 `Some(write)`；内存布局与 wire 不一致的类型与手写 impl 必须保持
/// 默认 `None`，否则整段拷贝会写出错误字节或改变手写 hook 的既有调用。
#[test]
fn bulk_direct_hook_is_given_only_where_memory_bytes_are_wire_bytes() {
    use ace_serialize::schema::value::{DateTime, DateTimeOffset};
    // 定宽内建元素的直写 impl 全部按 `target_endian = "little"` 编译：大端按 FR-W09 回落默认
    // `None`（逐项写入），故同一批类型按端序分支断言，与 `vec_i32_encode_is_one_bulk_and_matches_wire_body` 同口径。
    if cfg!(target_endian = "little") {
        assert!(<i32 as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<u32 as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<u8 as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<u64 as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<f32 as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<f64 as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<bool as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<char as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<Guid as AceElement>::encode_elements_bulk_direct().is_some());
        assert!(<i32 as AceEncode>::ace_encode_elements_bulk_direct().is_some());
        assert!(<Guid as AceEncode>::ace_encode_elements_bulk_direct().is_some());
    } else {
        // 大端没有整段体：这些 `Some` impl 被 cfg 排除，只剩 trait 默认的 `None`。
        assert!(<i32 as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<u32 as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<u8 as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<u64 as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<f32 as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<f64 as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<bool as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<char as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<Guid as AceElement>::encode_elements_bulk_direct().is_none());
        assert!(<i32 as AceEncode>::ace_encode_elements_bulk_direct().is_none());
        assert!(<Guid as AceEncode>::ace_encode_elements_bulk_direct().is_none());
    }
    // 16B 内存 vs 8B/10B wire：整段拷贝会写错字节，必须继续逐项转换。
    assert!(<DateTime as AceElement>::encode_elements_bulk_direct().is_none());
    assert!(<DateTimeOffset as AceElement>::encode_elements_bulk_direct().is_none());
    assert!(<DateTime as AceEncode>::ace_encode_elements_bulk_direct().is_none());
    // 变宽元素体（记录）没有连续体可拷。
    assert!(<String as AceElement>::encode_elements_bulk_direct().is_none());
    // 手写 impl 保持默认 `None`：其公开 hook 仍按原样调用。
    assert!(<HandBulk as AceEncode>::ace_encode_elements_bulk_direct().is_none());
}

#[test]
fn hand_written_bulk_hook_on_an_empty_collection_allocates_nothing() {
    let value: Vec<HandBulk> = Vec::new();
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        HAND_BULK_CALLS.store(0, Ordering::Relaxed);
        ALLOCATIONS.with(|counter| counter.set(0));
        COUNTING.with(|flag| flag.set(true));
        let written = encode_into(&value, &mut dst, &mut scratch, &options);
        COUNTING.with(|flag| flag.set(false));
        let observed = ALLOCATIONS.with(Cell::get);
        assert_eq!(
            observed, 0,
            "命中手写 bulk hook 时库侧不得新建合成记录体 `Vec`（packed = {packed}）"
        );
        assert_eq!(
            HAND_BULK_CALLS.load(Ordering::Relaxed),
            1,
            "必须真的走手写 bulk hook，否则零分配会平凡通过（packed = {packed}）"
        );
        assert_eq!(written, Ok(expected.len()));
        assert_eq!(dst, expected);
    }
}

/// I1：根级替换场景（回调换入拥有值编码器、忽略其内部错误并返回 `Ok`）与嵌套路径一致，
/// 统一报既有 `1 InvalidArgument`；不得返回被换入编码器锁存的 `6 ResourceLimit`。
struct ReplacesWithLimitedEncoder;

impl AceEncode for ReplacesWithLimitedEncoder {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        *e = Encoder::new(&Options {
            max_records: 0,
            ..Options::default()
        });
        // 忽略错误并返回 `Ok`：`6 ResourceLimit` 只会落进被换入编码器的首错锁存。
        let _ = e.begin_record();
        Ok(())
    }
}

#[test]
fn replaced_encoder_reports_invalid_argument_not_the_latched_resource_limit() {
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(
                &ReplacesWithLimitedEncoder,
                &mut dst,
                &mut scratch,
                &options
            ),
            Err(Error::InvalidArgument),
            "替换场景必须与嵌套路径一致报 1，不得暴露被换入编码器锁存的 6（packed = {packed}）"
        );
    }
}

/// R29①②：`scratch` 不足时探测入口返回既有 `1`，并把「同起址下所需总长」（`scratch.len()` 的
/// 目标值，**不是差额**）写进 `need`；按该 `need` 用**同一起址**的切片重试即成功（同值、同选项），
/// 且同起址下若再次因 `scratch` 不足失败，下一次所报 `need` 必须增长。
fn probe_converges_on_total_need<T: AceEncode>(value: &T) {
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(value, packed);
        let mut backing = vec![0u8; SCRATCH];
        let base = backing.as_ptr();
        let mut len = 0usize;
        let mut reported: Vec<usize> = Vec::new();
        loop {
            let mut dst = vec![0u8; expected.len()];
            let mut need = usize::MAX;
            let slice = &mut backing[..len];
            assert_eq!(
                slice.as_ptr(),
                base,
                "每次重试必须与首次同起址（packed = {packed}）"
            );
            match encode_into_probe(value, &mut dst, slice, &options, &mut need) {
                Ok(written) => {
                    assert!(
                        !reported.is_empty(),
                        "零长 scratch 必须至少失败一次并报告 need（packed = {packed}）"
                    );
                    assert_eq!(written, expected.len(), "成功返回值必须与 encode_into 同一结果");
                    assert_eq!(dst, expected, "探测入口的 wire 必须与 encode 逐字节一致");
                    break;
                }
                Err(Error::InvalidArgument) => {
                    assert!(
                        need > len,
                        "need 是同起址所需总长而不是差额：need = {need}，当前 len = {len}（packed = {packed}）"
                    );
                    if let Some(previous) = reported.last() {
                        assert!(
                            need > *previous,
                            "同起址下再次因 scratch 不足，need 必须增长：{previous} → {need}（packed = {packed}）"
                        );
                    }
                    assert!(need <= SCRATCH, "need 必须落在本用例的缓冲内：{need}");
                    reported.push(need);
                    len = need;
                }
                Err(other) => panic!("scratch 不足只允许报既有 1，实得 {other:?}（packed = {packed}）"),
            }
        }
    }
}

#[test]
fn probe_reports_total_need_and_retry_at_that_total_succeeds() {
    // 零身份槽：失败点只有 arena 分配。
    probe_converges_on_total_need(&Leaf { v: 7 });
    // 两个共享身份：失败点会落到身份槽需求上（`P + N*S + B`）。
    let leaf = Rc::new(Leaf { v: 9 });
    probe_converges_on_total_need(&Shared {
        a: Rc::clone(&leaf),
        b: Rc::clone(&leaf),
    });
}

/// B1：`need` 必须是**不带旧尾部填充**的下界。旧 `B` 含随切片尾端变化的对齐填充，同起址扩容后
/// 那段填充不保留，因此不能用它当前缀占用。raw `Leaf { v: 7 }`、起址 8 字节对齐、`dst` 足够：
/// 收尾前四次分配为 `512@8 + 64@1 + 320@8 + 64@1`（载荷 960、无填充），失败的第 5 次是收尾合并的
/// `32@1`；`scratch.len() = 991` 时旧 `B = 967`（含 7 字节尾部填充）会高报 999，而 992 字节同起址
/// 即可整次成功。
#[test]
fn probe_reports_lower_bound_without_the_old_tail_padding() {
    const SHORT: usize = 991;
    const NEED: usize = 992;
    let value = Leaf { v: 7 };
    let options = options(false);
    let expected = expected_bytes(&value, false);
    let mut backing = vec![0u8; SCRATCH];
    // 起址按已发布的 `A` 对齐（`P == 0`），并因此落在 8 字节对齐上（arena 的两种对齐是 8 与 1）。
    let align = ace_serialize::identity_entry_align();
    let lead = (align - backing.as_ptr() as usize % align) % align;
    let base = &mut backing[lead..];
    assert_eq!(base.as_ptr() as usize % 8, 0, "本用例的前提是 8 字节对齐起址");

    let mut dst = vec![0u8; expected.len()];
    let mut need = usize::MAX;
    assert_eq!(
        encode_into_probe(&value, &mut dst, &mut base[..SHORT], &options, &mut need),
        Err(Error::InvalidArgument),
        "991 字节不足，必须明确报错"
    );
    assert_eq!(
        need, NEED,
        "同起址所需总长必须不带旧尾部填充：991 时旧 B 含的 7 字节尾部填充不算（不得报 999）"
    );

    let mut retry_dst = vec![0u8; expected.len()];
    let mut retry_need = usize::MAX;
    assert_eq!(
        encode_into_probe(
            &value,
            &mut retry_dst,
            &mut base[..NEED],
            &options,
            &mut retry_need
        ),
        Ok(expected.len()),
        "按所报 992 字节同起址重试必须成功（下界确实够用）"
    );
    assert_eq!(retry_dst, expected, "重试成功的 wire 必须与 encode 逐字节一致");
}

/// B2 混合失败①：「缺后端」分支不得继承历史诊断。空 `scratch` 下手写回调忽略 `enter_object()`
/// 的容量错误、换入 `Encoder::new` 并返回 `Ok(())`，`finish_into` 因**缺后端**返回 `1`。
struct IgnoresScratchCapacityThenReplaces;

impl AceEncode for IgnoresScratchCapacityThenReplaces {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let _ = e.enter_object();
        *e = Encoder::new(&Options::default());
        Ok(())
    }
}

/// 混合失败②的 `scratch` 长度：首次编码用掉 8 个帧槽 `512@8` 与首个对象帧的槽缓冲 `64@1`
/// （起址 8 字节对齐、无对齐填充，恰好占满），因此第二次编码的槽缓冲分配必然失败。
const DST_THEN_SCRATCH: usize = 576;

/// B2 混合失败②：raw 路径先锁存 `dst` 不足，随后又发生一条被忽略的 `scratch` 容量错误，最终返回
/// 值仍是前一个 `dst` 错误——`need` 不得继承后续那条被忽略的诊断。
struct DstThenIgnoredScratchCapacity;

impl AceEncode for DstThenIgnoredScratchCapacity {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let _ = Leaf { v: 1 }.ace_encode(e);
        let _ = Leaf { v: 2 }.ace_encode(e);
        Ok(())
    }
}

/// B2 反例：回调**直接返回**的 `Err` 与锁存首错同码。空 `scratch` 下先 `enter_object()` 锁存真实的
/// `scratch` 不足（下界 512），随后 `begin_field(0)` 因**无对象帧**返回非容量 `InvalidArgument`；
/// 入口实际返回的是后者，归因必须按控制流（回调直返）判定，不得按「码值等于首错」覆写哨兵。
struct IgnoresScratchCapacityThenFailsWithoutFrame;

impl AceEncode for IgnoresScratchCapacityThenFailsWithoutFrame {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let _ = e.enter_object();
        e.begin_field(0)
    }
}

/// r3 反例①：`scratch` 下界先被锁存，随后在** `dst` 容量点**失败。raw、空 `dst`、8 字节对齐空
/// `scratch`：`enter_object()` 因 `scratch` 不足锁存下界 512 且不压对象帧，`write_root_record(b"x")`
/// 写根槽时因 `dst` 不足报 `InvalidArgument`——后者属「其它失败」，必须让待写的诊断失效。
struct LatchScratchThenExhaustsDst;

impl AceEncode for LatchScratchThenExhaustsDst {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let _ = e.enter_object();
        e.write_root_record(b"x")
    }
}

/// r4 出口异码排除：锁存 `scratch` 下界后回调**直接返回**一条自行构造的
/// `Err(ResourceLimit)`。入口原样返回 `6`，且因最终返回值不是 `InvalidArgument`（`scratch`
/// 不足只会返回 `1`），`need` 保持哨兵——异码自造 `Err` 不得继承锁存下界。已否决的替代推断
/// 仍是码值相等/末错（见 `IgnoresScratchCapacityThenFailsWithoutFrame`）。
struct IgnoresScratchCapacityThenReturnsResourceLimit;

impl AceEncode for IgnoresScratchCapacityThenReturnsResourceLimit {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let _ = e.enter_object();
        Err(Error::ResourceLimit)
    }
}

/// B3 夹具的字段类型：忽略 `write_field_scalar` 的结果并返回 `Ok(())`。
///
/// `write_field_scalar` 只在**成功**后 `commit_order`，从不消费 `current_order`（消费只发生在
/// `encode_scalar`/`encode_none`/`encode_record`），因此 derive 生成的字段守卫随后报既有 `1`。
/// 补足 `scratch` 后该守卫仍报同一个 `1`：它不源于 `scratch` 容量不足。
struct IgnoresFieldWriteResult;

impl AceEncode for IgnoresFieldWriteResult {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let _ = e.write_field_scalar(1, [0; 8]);
        Ok(())
    }
}

impl ace_serialize::schema::AceDecode for IgnoresFieldWriteResult {
    fn ace_decode(_d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        Err(Error::InvalidArgument)
    }
}

impl ace_serialize::schema::AceDecodeCyclic for IgnoresFieldWriteResult {
    type Plan = ();
    fn decode_plan(_ctx: &mut ace_serialize::decode::DecodeCtx) -> Result<()> {
        Err(Error::InvalidArgument)
    }
    fn build_from_plan(_ctx: &mut ace_serialize::decode::BuildCtx<'_>, _plan: ()) -> Self {
        Self
    }
}

/// B3 夹具：derive 生成的字段守卫（`current_order` 未消费）就是这条 `1` 的产生点。
#[derive(AceSerialize)]
struct IgnoresFieldWriteGuard {
    only: IgnoresFieldWriteResult,
}

/// r6 反例：外层手写根回调**丢弃** derive 字段守卫的 `1`，随后再让一条真实的资源限制失败产生并
/// 丢弃，自己返回 `Ok(())`。守卫的 `1` 若锁存首错，`encode_into` 就会把它当返回值（`1`）；它只标
/// 「其它」来源时必须不影响首错，返回后发生的 `6 ResourceLimit` 才是首错。
struct SwallowsFieldWriteGuardThenResourceLimit;

impl AceEncode for SwallowsFieldWriteGuardThenResourceLimit {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let guard = IgnoresFieldWriteGuard {
            only: IgnoresFieldWriteResult,
        };
        let _ = guard.ace_encode(e);
        // `max_records = 0` 下 `begin_record` 就是既有资源限制的错误产生点。
        let _ = e.begin_record();
        Ok(())
    }
}

/// A6 入口见证①：`RefCell` 的可变借用被 `mem::forget` 泄漏，单元格从此不可用。单元格本身用
/// `Box::leak` 活到进程结束，被泄漏的 `RefMut` 不会悬垂；只泄漏一次，后续调用直接进入不可用状态。
struct LeakedRefMutCell(&'static RefCell<Leaf>);

impl LeakedRefMutCell {
    fn new() -> Self {
        Self(Box::leak(Box::new(RefCell::new(Leaf { v: 1 }))))
    }
}

impl AceEncode for LeakedRefMutCell {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        if let Ok(guard) = self.0.try_borrow_mut() {
            std::mem::forget(guard);
        }
        self.0.ace_encode(e)
    }
}

/// A6 入口见证②：同一 `RwLock` 的写 guard 在编码期间仍被持有。库侧 `try_read` 必须立即失败，
/// 而不是阻塞等待 guard 释放。
#[cfg(feature = "std")]
struct HeldWriteGuardLock(std::sync::Arc<std::sync::RwLock<Leaf>>);

#[cfg(feature = "std")]
impl AceEncode for HeldWriteGuardLock {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        (*self.0).ace_encode(e)
    }
}

/// A6 入口见证③：写 guard 在 unwind 中释放后锁已中毒。库侧 `try_read` 必须返回错误，而不是让
/// 中毒 panic 逃出这次编码。
#[cfg(feature = "std")]
struct PoisonedLeafLock(std::sync::RwLock<Leaf>);

#[cfg(feature = "std")]
impl AceEncode for PoisonedLeafLock {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        self.0.ace_encode(e)
    }
}

/// R29 哨兵：`1` 不能作归因依据——`dst` 少一字节（容量类但不是 `scratch` 不足）与既有非容量
/// `1` 样本，调用后 `need` 必须保持调用方传入值（只在该 `scratch` 不足分支写 `need`）。
/// A6：同一入口上的锁/借用冲突（`RefCell` 泄漏借用、`RwLock` 占用/中毒）也属既有非容量 `1`。
#[test]
fn probe_leaves_need_untouched_when_the_failure_is_not_scratch_capacity() {
    const SENTINEL: usize = 0x5A5A_5A5A;
    let leaf = Leaf { v: 7 };
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&leaf, packed);
        let mut dst = vec![0u8; expected.len() - 1];
        let mut scratch = vec![0u8; SCRATCH];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(&leaf, &mut dst, &mut scratch, &options, &mut need),
            Err(Error::InvalidArgument),
            "差一字节 `dst` 必须明确报错（packed = {packed}）"
        );
        assert_eq!(
            need, SENTINEL,
            "dst 容量不足不是 scratch 不足，不得写 need（packed = {packed}）"
        );
    }
    // 既有非容量 `1` 样本：替换编码器（F3）与替换后锁存限额（I1）。
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut scratch = vec![0u8; SCRATCH];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(
                &ReplacesWithLimitedEncoder,
                &mut dst,
                &mut scratch,
                &options,
                &mut need
            ),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            need, SENTINEL,
            "非容量失败不得写 need（packed = {packed}）"
        );
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(
                &ReplacedEncoder,
                &mut dst,
                &mut scratch,
                &options,
                &mut need
            ),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            need, SENTINEL,
            "替换编码器不是 scratch 不足，不得写 need（packed = {packed}）"
        );
    }
    // B2①：「缺后端」分支不得继承历史诊断——空 `scratch` 下 `enter_object()` 的容量错误被忽略，
    // 随后换入 `Encoder::new` 并返回 `Ok(())`，`finish_into` 因缺后端报 `1`。
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut empty_backing = [0u8; 8];
        let lead = (8 - empty_backing.as_ptr() as usize % 8) % 8;
        let empty = &mut empty_backing[lead..lead];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(
                &IgnoresScratchCapacityThenReplaces,
                &mut dst,
                empty,
                &options,
                &mut need
            ),
            Err(Error::InvalidArgument),
            "换入 `Encoder::new` 后收尾必须因缺后端报 1（packed = {packed}）"
        );
        assert_eq!(
            need, SENTINEL,
            "缺后端不是 scratch 不足，不得继承被忽略的那条历史诊断（packed = {packed}）"
        );
    }
    // B2②：raw 路径先锁存 `dst` 不足、随后又发生被忽略的 `scratch` 容量错误——最终返回的仍是
    // 前一个 `dst` 错误，`need` 必须保持传入值。
    {
        let options = options(false);
        let mut backing = vec![0u8; DST_THEN_SCRATCH + 8];
        let lead = (8 - backing.as_ptr() as usize % 8) % 8;
        let tight = &mut backing[lead..lead + DST_THEN_SCRATCH];
        // 前提核对：同一回调在 `dst` 充足时确实撞到 `scratch` 上限，否则本条不构成混合失败。
        let mut roomy_dst = vec![0u8; 4096];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(&DstThenIgnoredScratchCapacity, &mut roomy_dst, tight, &options, &mut need),
            Err(Error::InvalidArgument)
        );
        assert!(
            need > DST_THEN_SCRATCH,
            "该回调必须真的因 scratch 不足失败，否则本条回归不构成混合失败：need = {need}"
        );
        assert_ne!(
            need, SENTINEL,
            "正对照必须真的写 need：哨兵值不满足 `> DST_THEN_SCRATCH`，不能冒充归因"
        );
        // 混合失败：`dst` 只留 1 字节 ⇒ 首个锁存错误是 `dst` 不足，后来的 scratch 错误被忽略。
        let mut dst = vec![0u8; 1];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(&DstThenIgnoredScratchCapacity, &mut dst, tight, &options, &mut need),
            Err(Error::InvalidArgument),
            "最终返回值是前一个 `dst` 不足错误"
        );
        assert_eq!(
            need, SENTINEL,
            "最终返回的不是后那条被忽略的 scratch 不足，不得写 need"
        );
    }
    // B2 反例：回调直返的 `Err` 与锁存首错同码——来源是「回调直返」而非「锁存首错」，不得写 `need`。
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut empty_backing = [0u8; 8];
        let lead = (8 - empty_backing.as_ptr() as usize % 8) % 8;
        let empty = &mut empty_backing[lead..lead];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(
                &IgnoresScratchCapacityThenFailsWithoutFrame,
                &mut dst,
                empty,
                &options,
                &mut need
            ),
            Err(Error::InvalidArgument),
            "无对象帧的 `begin_field` 返回非容量 `1`（packed = {packed}）"
        );
        assert_eq!(
            need, SENTINEL,
            "回调直返的同码 `Err` 不得覆写哨兵：码值相等不等于返回了锁存首错（packed = {packed}）"
        );
    }
    // r3①：`dst` 容量点必须在产生点标为「其它失败」，使已锁存的 `scratch` 下界失效。
    {
        let options = options(false);
        let mut dst: [u8; 0] = [];
        let mut empty_backing = [0u8; 8];
        let lead = (8 - empty_backing.as_ptr() as usize % 8) % 8;
        let empty = &mut empty_backing[lead..lead];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(&LatchScratchThenExhaustsDst, &mut dst, empty, &options, &mut need),
            Err(Error::InvalidArgument),
            "空 `dst` 下写根槽必须报容量 `1`"
        );
        assert_eq!(
            need, SENTINEL,
            "`dst` 容量失败必须让已锁存的 scratch 下界失效，不得写成 512"
        );
    }
    // r4：出口只对 `Err(InvalidArgument)` 写 `need`——锁存 `scratch` 不足后回调直接返回**自造**
    // 的 `Err(ResourceLimit)`，入口原样返回 `6`，且该码不可能源于 `scratch` 不足，`need` 保持
    // 哨兵。已知限制收窄到自造**同码** `Err(InvalidArgument)`：公开 `Result` 不携带 provenance，
    // 无法区分来源；不得用码值相等或末错去补，本用例只锁定异码不写。
    for packed in [false, true] {
        let options = options(packed);
        let mut dst = vec![0u8; 64];
        let mut empty_backing = [0u8; 8];
        let lead = (8 - empty_backing.as_ptr() as usize % 8) % 8;
        let empty = &mut empty_backing[lead..lead];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(
                &IgnoresScratchCapacityThenReturnsResourceLimit,
                &mut dst,
                empty,
                &options,
                &mut need
            ),
            Err(Error::ResourceLimit),
            "回调直返的错误必须原样返回（packed = {packed}）"
        );
        assert_eq!(
            need, SENTINEL,
            "回调自造的异码 `Err` 不可能源于 scratch 不足，`need` 必须保持哨兵（packed = {packed}）"
        );
    }
    // B3：derive 守卫产生的 `1` 属「其它失败」。raw、8 字节对齐的 512 字节 `scratch`、`dst`
    // 充足：`enter_object` 的 8 个帧槽恰好占满 `scratch`，随后 0 号手写字段的
    // `write_field_scalar(1, [0; 8])` 因对象帧的槽缓冲不足锁存下界并被忽略（返回 `Ok(())`），
    // 未消费的字段选择让 derive 守卫报既有 `1`。补足 `scratch` 后该守卫照旧报 `1`，故这不是
    // `scratch` 容量不足：锁存的下界不得写进 `need`。
    {
        let options = options(false);
        let value = IgnoresFieldWriteGuard {
            only: IgnoresFieldWriteResult,
        };
        let mut dst = vec![0u8; 4096];
        // 前提核对：同一值在 `scratch` 充足时仍报同一个 `1`，否则本条不构成「非 scratch 原因」。
        let mut roomy = vec![0u8; SCRATCH];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(&value, &mut dst, &mut roomy, &options, &mut need),
            Err(Error::InvalidArgument),
            "补足 scratch 后 derive 守卫照旧报 `1`"
        );
        assert_eq!(need, SENTINEL, "充足 scratch 下不得写 need");
        let mut backing = vec![0u8; 512 + 8];
        let lead = (8 - backing.as_ptr() as usize % 8) % 8;
        let tight = &mut backing[lead..lead + 512];
        assert_eq!(tight.as_ptr() as usize % 8, 0, "本用例的前提是 8 字节对齐起址");
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(&value, &mut dst, tight, &options, &mut need),
            Err(Error::InvalidArgument),
            "字段未消费的 derive 守卫必须返回既有 `1`"
        );
        assert_eq!(
            need, SENTINEL,
            "该 `1` 由 derive 守卫产生（补足 scratch 后仍报），不是 scratch 容量不足，不得写 need"
        );
    }
    // A6：编码入口的锁/借用冲突——手写回调访问不可用单元格，库侧 `try_borrow`/`try_read` 拒绝
    // 后必须由 `encode_into` 原样返回既有 `1`。`dst`（4096）与 `scratch`（SCRATCH）都充足，排除
    // 容量因素；探测入口的 `need` 必须保持哨兵（这些失败不是 `scratch` 不足）。
    for packed in [false, true] {
        let options = options(packed);
        let leaked = LeakedRefMutCell::new();
        let mut dst = vec![0u8; 4096];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&leaked, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "泄漏 RefMut 的 RefCell 必须经 `encode_into` 返回既有 1，且不得 panic（packed = {packed}）"
        );
        let mut dst = vec![0u8; 4096];
        let mut need = SENTINEL;
        assert_eq!(
            encode_into_probe(&leaked, &mut dst, &mut scratch, &options, &mut need),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            need, SENTINEL,
            "借用冲突不是 scratch 容量不足，不得写 need（packed = {packed}）"
        );
        #[cfg(feature = "std")]
        {
            let lock = std::sync::Arc::new(std::sync::RwLock::new(Leaf { v: 1 }));
            let guard = lock.write().unwrap();
            let held = HeldWriteGuardLock(std::sync::Arc::clone(&lock));
            let mut dst = vec![0u8; 4096];
            let mut scratch = vec![0u8; SCRATCH];
            assert_eq!(
                encode_into(&held, &mut dst, &mut scratch, &options),
                Err(Error::InvalidArgument),
                "写 guard 仍被持有时必须经 `encode_into` 返回既有 1（packed = {packed}）"
            );
            let mut dst = vec![0u8; 4096];
            let mut need = SENTINEL;
            assert_eq!(
                encode_into_probe(&held, &mut dst, &mut scratch, &options, &mut need),
                Err(Error::InvalidArgument)
            );
            assert_eq!(
                need, SENTINEL,
                "锁占用不是 scratch 容量不足，不得写 need（packed = {packed}）"
            );
            drop(guard);

            let poisoned = std::sync::RwLock::new(Leaf { v: 1 });
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = poisoned.write().unwrap();
                panic!("poison the lock while the write guard is held");
            }));
            assert!(poisoned.try_read().is_err(), "前提：锁必须已被中毒");
            let poisoned = PoisonedLeafLock(poisoned);
            let mut dst = vec![0u8; 4096];
            let mut scratch = vec![0u8; SCRATCH];
            assert_eq!(
                encode_into(&poisoned, &mut dst, &mut scratch, &options),
                Err(Error::InvalidArgument),
                "中毒的 RwLock 必须经 `encode_into` 返回既有 1（packed = {packed}）"
            );
            let mut dst = vec![0u8; 4096];
            let mut need = SENTINEL;
            assert_eq!(
                encode_into_probe(&poisoned, &mut dst, &mut scratch, &options, &mut need),
                Err(Error::InvalidArgument)
            );
            assert_eq!(
                need, SENTINEL,
                "锁中毒不是 scratch 容量不足，不得写 need（packed = {packed}）"
            );
        }
    }
}

/// r6：derive 字段守卫只标「其它」、不写首错锁存——外层回调丢弃它、再丢弃后发生的
/// `6 ResourceLimit` 并返回 `Ok(())` 时，普通 `encode_into`（非探测入口）仍返回 `ResourceLimit`。
#[test]
fn encode_into_keeps_resource_limit_when_the_field_guard_error_is_swallowed() {
    let options = Options {
        packed: false,
        max_records: 0,
        ..Options::default()
    };
    let value = SwallowsFieldWriteGuardThenResourceLimit;
    let mut dst = vec![0u8; 4096];
    let mut scratch = vec![0u8; SCRATCH];
    assert_eq!(
        encode_into(&value, &mut dst, &mut scratch, &options),
        Err(Error::ResourceLimit),
        "守卫的 `1` 不得锁存成首错，`encode_into` 必须返回后发生的 `6 ResourceLimit`"
    );
}

/// R29④：探测入口整链零分配（与 `encode_into` 同一真实路径，含身份/引用追踪）——成功出口与
/// 「写 `need` 的失败出口」都不得分配，也不要求调用方额外工作区。
#[test]
fn probe_call_allocates_nothing() {
    let leaf = Rc::new(Leaf { v: 13 });
    let value = Shared {
        a: Rc::clone(&leaf),
        b: Rc::clone(&leaf),
    };
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        // 成功出口。
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        let mut need = usize::MAX;
        ALLOCATIONS.with(|counter| counter.set(0));
        COUNTING.with(|flag| flag.set(true));
        let written = encode_into_probe(&value, &mut dst, &mut scratch, &options, &mut need);
        COUNTING.with(|flag| flag.set(false));
        assert_eq!(
            ALLOCATIONS.with(Cell::get),
            0,
            "探测入口成功路径不得分配（packed = {packed}）"
        );
        assert_eq!(written, Ok(expected.len()));
        assert_eq!(dst, expected);

        // 失败出口（写 `need` 的那一条）：长度 0、起址 8 字节对齐的 `scratch` ⇒ 首个失败分配是
        // 8 个 64 字节帧槽（`512@8`）；零身份槽时不加也不扣对齐前导 `P`、无尾部填充，故 `need`
        // 恰为 512（断言具体值，不用 `need > 0` 给「根本不写」放行）。
        let mut empty_backing = [0u8; 8];
        let lead = (8 - empty_backing.as_ptr() as usize % 8) % 8;
        let empty = &mut empty_backing[lead..lead];
        assert_eq!(empty.as_ptr() as usize % 8, 0, "本用例的前提是 8 字节对齐起址");
        let mut dst = vec![0u8; expected.len()];
        let mut need = usize::MAX;
        ALLOCATIONS.with(|counter| counter.set(0));
        COUNTING.with(|flag| flag.set(true));
        let failed = encode_into_probe(&value, &mut dst, empty, &options, &mut need);
        COUNTING.with(|flag| flag.set(false));
        assert_eq!(
            ALLOCATIONS.with(Cell::get),
            0,
            "scratch 不足路径同样不得分配（packed = {packed}）"
        );
        assert_eq!(failed, Err(Error::InvalidArgument));
        assert_eq!(
            need, 512,
            "该分支必须把哨兵换成具体下界：8 个 64 字节帧槽（packed = {packed}）"
        );
    }
}

// R23 探针的计数（线程局部：与其它用例并行时不会互相干扰）。
thread_local! {
    /// 新钩子被调用的次数。
    static INTO_CALLS: Cell<usize> = const { Cell::new(0) };
    /// 逐项编码入口被调用的次数（命中新钩子时必须为 0）。
    static INTO_ELEMENT_CALLS: Cell<usize> = const { Cell::new(0) };
    /// 新钩子收到的切片长度（必须恰为集合体长，而不是物理预留量）。
    static INTO_DST_LEN: Cell<usize> = const { Cell::new(0) };
    /// 未声明探针的新钩子被调用的次数（不得为非 0）。
    static UNDECLARED_CALLS: Cell<usize> = const { Cell::new(0) };
    /// 未声明探针的逐项编码入口被调用的次数。
    static UNDECLARED_ELEMENT_CALLS: Cell<usize> = const { Cell::new(0) };
}

fn reset_into_probe() {
    INTO_CALLS.with(|c| c.set(0));
    INTO_ELEMENT_CALLS.with(|c| c.set(0));
    INTO_DST_LEN.with(|c| c.set(0));
}

/// R23 探针：**只**覆写新钩子 `ace_encode_elements_bulk_into` 并让能力声明访问器返回 `Some`，
/// 旧 `&mut Vec<u8>` hook 与旧直写都保持 trait 默认（`Ok(false)`／`None`）。用于证明启用判定
/// 只看能力声明访问器、命中时不走逐项编码。
#[derive(Debug, PartialEq, Eq)]
struct IntoProbe(u32);

impl AceElement for IntoProbe {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 4 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        INTO_ELEMENT_CALLS.with(|c| c.set(c.get() + 1));
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn decode_element(d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(4)?;
        Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
    }
    fn ace_encode_elements_bulk_into(items: &[Self], dst: &mut [u8]) -> Result<Option<usize>> {
        let needed = items
            .len()
            .checked_mul(4)
            .and_then(|n| n.checked_add(4))
            .ok_or(Error::ResourceLimit)?;
        // 只有放不下才返回既有 `1`；恰好写满必须成功。
        if dst.len() < needed {
            return Err(Error::InvalidArgument);
        }
        INTO_CALLS.with(|c| c.set(c.get() + 1));
        INTO_DST_LEN.with(|c| c.set(dst.len()));
        dst[..4].copy_from_slice(&(items.len() as u32).to_le_bytes());
        for (index, item) in items.iter().enumerate() {
            let start = 4 + index * 4;
            dst[start..start + 4].copy_from_slice(&item.0.to_le_bytes());
        }
        Ok(Some(needed))
    }
    fn encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>> {
        Some(<Self as AceElement>::ace_encode_elements_bulk_into)
    }
}

impl AceEncode for IntoProbe {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        Some(4)
    }
    fn ace_encode_elements_bulk_into(items: &[Self], dst: &mut [u8]) -> Result<Option<usize>>
    where
        Self: Sized,
    {
        <Self as AceElement>::ace_encode_elements_bulk_into(items, dst)
    }
    fn ace_encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>>
    where
        Self: Sized,
    {
        Some(<Self as AceEncode>::ace_encode_elements_bulk_into)
    }
}

/// R23 未声明探针：在**实际分派侧**（`AceEncode`，即 `Vec<T: AceEncode>` 所用的接缝）覆写新钩子，
/// 但**不**声明能力（访问器保持默认 `None`）。库必须按「未声明」处理——不做任何额外预留、
/// 也不调用该钩子。计数落在分派侧，而不是只在 `AceElement` 上（后者在根 `Vec` 路径不会被调用）。
#[derive(Debug, PartialEq, Eq)]
struct UndeclaredProbe(u32);

impl AceElement for UndeclaredProbe {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 4 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        UNDECLARED_ELEMENT_CALLS.with(|c| c.set(c.get() + 1));
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn decode_element(d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(4)?;
        Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
    }
}

impl AceEncode for UndeclaredProbe {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        Some(4)
    }
    fn ace_encode_elements_bulk_into(_items: &[Self], _dst: &mut [u8]) -> Result<Option<usize>>
    where
        Self: Sized,
    {
        UNDECLARED_CALLS.with(|c| c.set(c.get() + 1));
        Ok(None)
    }
    // 能力声明访问器保持默认 `None`：本探针按「未声明」处理。
}

/// R23①：`dst` 恰好容量必须成功，返回长度等于 wire 长度，且借出的切片长度恰为体长
/// （`4 + len × value_size`），不是 `buf_reserve` 的物理预留量。
#[test]
fn bulk_into_hook_fills_exactly_and_reports_body_length() {
    let value = vec![IntoProbe(1), IntoProbe(2), IntoProbe(3)];
    for packed in [false, true] {
        let options = options(packed);
        // 拥有值形态（`encode`）不启用新钩子，但逐项写出的 wire 必须一致。
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        reset_into_probe();
        let written =
            encode_into(&value, &mut dst, &mut scratch, &options).unwrap_or_else(|error| {
                panic!("恰好容量 dst 必须成功（packed = {packed}）：{error:?}")
            });
        assert_eq!(
            written,
            expected.len(),
            "返回长度即体长（packed = {packed}）"
        );
        assert_eq!(
            dst, expected,
            "命中新钩子必须写出同一 wire（packed = {packed}）"
        );
        assert_eq!(INTO_CALLS.with(Cell::get), 1, "必须命中新钩子");
        assert_eq!(
            INTO_DST_LEN.with(Cell::get),
            4 + 3 * 4,
            "借出的切片长度恰为体长，而不是物理预留量（packed = {packed}）"
        );
    }
}

/// R23②（I2①）：最终 wire `dst` 差一字节则明确报既有 `1`、不 panic、不报成功；而新钩子收到的
/// **体切片仍恰为体长**（是 wire `dst` 不够，不是钩子面对短切片）。直接断言钩子切片，而不是只
/// 看最终 `dst`。
#[test]
fn bulk_into_hook_sees_exact_body_slice_even_when_final_dst_is_one_byte_short() {
    let value = vec![IntoProbe(4), IntoProbe(5)];
    let body_len = 4 + 2 * 4;
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        assert!(expected.len() > 1);
        let mut dst = vec![0u8; expected.len() - 1];
        let mut scratch = vec![0u8; SCRATCH];
        reset_into_probe();
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "差一字节必须报既有 1（packed = {packed}）"
        );
        assert_eq!(INTO_CALLS.with(Cell::get), 1, "新钩子必须被调用一次");
        assert_eq!(
            INTO_DST_LEN.with(Cell::get),
            body_len,
            "钩子收到的切片恰为体长，而不是被 wire dst 拖成更短（packed = {packed}）"
        );
    }
    // I2 余项：钩子**自身**收到 `body_len - 1` 的切片时必须报既有 `1`、不得截断写。直接以
    // 探针的钩子入口对短切片调用，不经库，因此断言的是钩子契约本身。
    let mut short = vec![0u8; body_len - 1];
    assert_eq!(
        <IntoProbe as AceEncode>::ace_encode_elements_bulk_into(&value, &mut short),
        Err(Error::InvalidArgument),
        "钩子收到差 1 的切片必须报既有 1"
    );
    assert!(
        short.iter().all(|byte| *byte == 0),
        "差 1 的切片必须原样返回：不得写入截断体"
    );
}

/// R23③：两个旧 `&mut Vec<u8>` 钩子的形参与默认语义不变（签名级回归）。
///
/// 签名检查只用**无共享计数**的默认实现：不调用 `HandBulk` 的覆写 hook，也不读写
/// `HAND_BULK_CALLS`，以免与 `hand_written_bulk_hook_on_an_empty_collection_allocates_nothing`
/// 的 `== 1` 断言在并行时报互干扰。
#[test]
fn old_vec_bulk_hooks_keep_their_signatures() {
    // 签名级：仍收 `&mut Vec<u8>`、仍返回 `Result<bool>`（函数指针强制即证明签名未变）。
    let _encode_side: fn(&[HandBulk], &mut Vec<u8>) -> Result<bool> =
        <HandBulk as AceEncode>::ace_encode_elements_bulk;
    let element_side: fn(&[String], &mut Vec<u8>) -> Result<bool> =
        <String as AceElement>::encode_elements_bulk;
    // 未覆写的默认仍为 `Ok(false)`（内建变宽元素无 bulk），且不触碰任何共享计数。
    assert_eq!(element_side(&[], &mut Vec::new()), Ok(false));
    assert_eq!(
        <String as AceElement>::encode_elements_bulk(&[], &mut vec![9u8]),
        Ok(false),
        "默认实现不得修改 out、也不得依赖其初值"
    );
}

/// R23④：**同一次调用**上同时成立「恰好 `dst` + 入口到返回零分配 + 实际命中新钩子」。
#[test]
fn bulk_into_hook_exact_dst_zero_alloc_and_hit_in_one_call() {
    let value = vec![IntoProbe(7), IntoProbe(9)];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        reset_into_probe();
        ALLOCATIONS.with(|counter| counter.set(0));
        COUNTING.with(|flag| flag.set(true));
        let written = encode_into(&value, &mut dst, &mut scratch, &options);
        COUNTING.with(|flag| flag.set(false));
        let observed = ALLOCATIONS.with(Cell::get);
        assert_eq!(observed, 0, "入口到返回不得分配（packed = {packed}）");
        assert_eq!(written, Ok(expected.len()), "恰好 dst 必须成功");
        assert_eq!(dst, expected);
        assert_eq!(INTO_CALLS.with(Cell::get), 1, "必须真的命中新钩子");
        assert_eq!(
            INTO_ELEMENT_CALLS.with(Cell::get),
            0,
            "命中新钩子时不得逐项编码（packed = {packed}）"
        );
    }
}

/// R23⑤：只覆写新钩子并声明能力的手写实现必须命中——启用判定不得只看旧直写
/// `direct != None`，也不能以调用结果推断覆写。
#[test]
fn bulk_into_hook_is_enabled_by_the_capability_accessor_alone() {
    assert!(
        <IntoProbe as AceElement>::encode_elements_bulk_into_fn().is_some()
            && <IntoProbe as AceEncode>::ace_encode_elements_bulk_into_fn().is_some(),
        "两侧能力声明访问器都必须给出 `Some`"
    );
    assert!(
        <IntoProbe as AceEncode>::ace_encode_elements_bulk_direct().is_none(),
        "本探针不提供旧直写：命中只能来自新钩子"
    );
    assert_eq!(
        <IntoProbe as AceEncode>::ace_encode_elements_bulk(&[], &mut Vec::new()),
        Ok(false)
    );
    let value = vec![IntoProbe(11)];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        reset_into_probe();
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Ok(expected.len())
        );
        assert_eq!(dst, expected);
        assert_eq!(INTO_CALLS.with(Cell::get), 1, "能力声明即须命中");
        assert_eq!(INTO_ELEMENT_CALLS.with(Cell::get), 0);
    }
    // 元素形式（`AceElement` 集合路径）走同一接缝的访问器：内层 `Vec` 记录同样命中。
    let nested = vec![vec![IntoProbe(1)], vec![]];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&nested, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        reset_into_probe();
        assert_eq!(
            encode_into(&nested, &mut dst, &mut scratch, &options),
            Ok(expected.len())
        );
        assert_eq!(dst, expected);
        assert_eq!(
            INTO_CALLS.with(Cell::get),
            2,
            "两个内层集合体各命中一次元素侧新钩子（packed = {packed}）"
        );
    }
    // 身份目标（`ace_encode_target`）：目标记录由 capture 打开，新钩子在**已打开**的
    // 记录上预留与提交，wire 必须与拥有值形态一致。
    let target = Rc::new(vec![IntoProbe(3), IntoProbe(4)]);
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&target, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        reset_into_probe();
        assert_eq!(
            encode_into(&target, &mut dst, &mut scratch, &options),
            Ok(expected.len())
        );
        assert_eq!(dst, expected);
        assert_eq!(
            INTO_CALLS.with(Cell::get),
            1,
            "身份目标的集合体必须命中新钩子（packed = {packed}）"
        );
        assert_eq!(INTO_ELEMENT_CALLS.with(Cell::get), 0);
    }
}

/// 用同一块 `backing`（固定起址）找出让 `encode_into` 成功的最小 `scratch` 长度；`dst` 取 wire
/// 恰好长度。每次尝试都从同一 `backing` 起址切片，使对齐前导 `P` 一致、长度可比。
fn min_scratch_with<T: AceEncode>(value: &T, packed: bool, backing: &mut [u8]) -> usize {
    let options = options(packed);
    let expected = expected_bytes(value, packed);
    let base = backing.as_ptr();
    for len in 0..=backing.len() {
        let mut dst = vec![0u8; expected.len()];
        let slice = &mut backing[..len];
        assert_eq!(slice.as_ptr(), base, "每次尝试必须同一起址");
        if encode_into(value, &mut dst, slice, &options).is_ok() {
            assert_eq!(dst, expected, "成功后的 wire 必须与拥有值形态一致");
            return len;
        }
    }
    panic!("本用例的缓冲内找不到可成功的 scratch 长度（packed = {packed}）");
}

/// 无任何新/旧 bulk 声明的基线条：逐项编码，用来给出「一次预留」的峰值基准。
#[derive(Debug, PartialEq, Eq)]
struct PlainProbe(u32);

impl AceElement for PlainProbe {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 4 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn decode_element(d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(4)?;
        Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
    }
}

impl AceEncode for PlainProbe {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        Some(4)
    }
}

/// 已声明新钩子能力、但本次返回 `Ok(None)` 的探针：试准备预留后必须由既有分派复用，
/// 不得为同一记录体二次扩容。
#[derive(Debug, PartialEq, Eq)]
struct DeclinedProbe(u32);

thread_local! {
    /// 已声明但本次 `Ok(None)` 的钩子被调用次数。
    static DECLINED_CALLS: Cell<usize> = const { Cell::new(0) };
}

impl AceElement for DeclinedProbe {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 4 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn decode_element(d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(4)?;
        Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
    }
    fn ace_encode_elements_bulk_into(_items: &[Self], _dst: &mut [u8]) -> Result<Option<usize>> {
        DECLINED_CALLS.with(|c| c.set(c.get() + 1));
        Ok(None)
    }
    fn encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>> {
        Some(<Self as AceElement>::ace_encode_elements_bulk_into)
    }
}

impl AceEncode for DeclinedProbe {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        Some(4)
    }
    fn ace_encode_elements_bulk_into(_items: &[Self], _dst: &mut [u8]) -> Result<Option<usize>>
    where
        Self: Sized,
    {
        DECLINED_CALLS.with(|c| c.set(c.get() + 1));
        Ok(None)
    }
    fn ace_encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>>
    where
        Self: Sized,
    {
        Some(<Self as AceEncode>::ace_encode_elements_bulk_into)
    }
}

/// R23⑥：未声明能力者（访问器默认 `None`）不做额外预留、也不调新钩子——即使覆写了它，
/// 也按既有分派逐项编码，结果与拥有值形态逐字节一致。
#[test]
fn undeclared_bulk_into_hook_is_never_called() {
    assert!(
        <UndeclaredProbe as AceElement>::encode_elements_bulk_into_fn().is_none()
            && <UndeclaredProbe as AceEncode>::ace_encode_elements_bulk_into_fn().is_none(),
        "未声明者两侧访问器都必须保持默认 `None`"
    );
    let value = vec![UndeclaredProbe(1), UndeclaredProbe(2)];
    let plain = vec![PlainProbe(1), PlainProbe(2)];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        UNDECLARED_CALLS.with(|c| c.set(0));
        UNDECLARED_ELEMENT_CALLS.with(|c| c.set(0));
        ALLOCATIONS.with(|counter| counter.set(0));
        COUNTING.with(|flag| flag.set(true));
        let written = encode_into(&value, &mut dst, &mut scratch, &options);
        COUNTING.with(|flag| flag.set(false));
        assert_eq!(ALLOCATIONS.with(Cell::get), 0, "既有逐项路径同样零分配");
        assert_eq!(written, Ok(expected.len()));
        assert_eq!(dst, expected);
        assert_eq!(
            UNDECLARED_CALLS.with(Cell::get),
            0,
            "未声明者不得被调用（packed = {packed}）"
        );
        assert_eq!(
            UNDECLARED_ELEMENT_CALLS.with(Cell::get),
            2,
            "未声明者必须照常走逐项编码（packed = {packed}）"
        );
        // 「零预留」用 scratch 边界见证：未声明的峰值与无任何声明的基线相同（堆分配计数看不到
        // scratch 预留）。若未声明者错误地做了试准备，最小可成功长度会比基线大。
        assert_eq!(expected_bytes(&plain, packed), expected, "两种探针 wire 必须一致");
        let mut backing = vec![0u8; SCRATCH];
        let plain_need = min_scratch_with(&plain, packed, &mut backing);
        let undeclared_need = min_scratch_with(&value, packed, &mut backing);
        assert_eq!(
            undeclared_need, plain_need,
            "未声明者不得做任何额外预留（packed = {packed}）"
        );
    }
}

/// I2③：已声明、本次 `Ok(None)` 的试准备必须由既有分派**复用同一块**记录体，不得二次扩容：
/// 其最小可成功 `scratch` 与完全未声明的基线相同，且新钩子确实被尝试一次。
#[test]
fn declared_but_none_does_not_double_reserve_the_record_body() {
    let plain = vec![PlainProbe(1), PlainProbe(2)];
    let declined = vec![DeclinedProbe(1), DeclinedProbe(2)];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&declined, packed);
        assert_eq!(expected_bytes(&plain, packed), expected, "两种探针 wire 必须一致");
        let mut backing = vec![0u8; SCRATCH];
        let plain_need = min_scratch_with(&plain, packed, &mut backing);
        let declined_need = min_scratch_with(&declined, packed, &mut backing);
        assert_eq!(
            declined_need, plain_need,
            "已声明但 Ok(None) 不得二次扩容（packed = {packed}）"
        );
        DECLINED_CALLS.with(|c| c.set(0));
        let mut dst = vec![0u8; expected.len()];
        assert_eq!(
            encode_into(&declined, &mut dst, &mut backing[..plain_need], &options),
            Ok(expected.len()),
            "按未声明的最小 scratch 必须成功（packed = {packed}）"
        );
        assert_eq!(dst, expected);
        let calls = DECLINED_CALLS.with(Cell::get);
        assert_eq!(
            calls, 1,
            "新钩子必须被尝试一次（{calls}，packed = {packed}）"
        );
    }
}

/// w44 反例的元素：128 个 4B 元素 ⇒ 体长 516B，`buf_reserve` 从 64 起翻倍预分配 **1024B**。
/// 旧 hook 返回既有 `4`（内层编码必然失败），新钩子**已声明**但本次返回 `Ok(None)`。
struct DeclinedLarge(u32);

thread_local! {
    /// 128 元素反例探针的新钩子命中次数：钩子只在物理预留**成功之后**才被调用，
    /// 故 `> 0` 即证明本次走了「预留 + 未接管」的 `Declined` 分支。
    static LARGE_DECLINED_CALLS: Cell<usize> = const { Cell::new(0) };
}

impl AceEncode for DeclinedLarge {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn ace_element_value_size() -> Option<usize> {
        Some(4)
    }
    fn ace_encode_elements_bulk(_items: &[Self], _out: &mut Vec<u8>) -> Result<bool> {
        Err(Error::InvalidTag)
    }
    fn ace_encode_elements_bulk_into(_items: &[Self], _dst: &mut [u8]) -> Result<Option<usize>> {
        LARGE_DECLINED_CALLS.with(|c| c.set(c.get() + 1));
        Ok(None)
    }
    fn ace_encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>>
    {
        Some(<Self as AceEncode>::ace_encode_elements_bulk_into)
    }
}

/// 同一反例的**未声明**孪生：旧 hook 同样返回既有 `4`，但完全不声明快路径（不进试准备），
/// 因此它就是「不开快路径」时的旧路径基准。
struct PlainLarge(u32);

impl AceEncode for PlainLarge {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn ace_element_value_size() -> Option<usize> {
        Some(4)
    }
    fn ace_encode_elements_bulk(_items: &[Self], _out: &mut Vec<u8>) -> Result<bool> {
        Err(Error::InvalidTag)
    }
}

/// 审核给出的原反例：先编码 128 元素集合（旧 hook 报既有 `4`，被忽略；新钩子已声明但本次返回
/// `Ok(None)`），随后编码 `Leaf { v: 7 }`。`Leaf` 的 raw 工作区需求为 992B，而 `dst` 恰为其
/// raw wire 长度、`scratch` 给 1280B 且起址 8 字节对齐：不撤销那 1024B 试探预留时旧路径装不下
/// `Leaf` 的 992B（`Err(1)`），撤销后才 `Ok`。
struct CatchesDeclineThenReencodes<E>(Vec<E>);

impl<E: AceEncode> AceEncode for CatchesDeclineThenReencodes<E> {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let _ = self.0.ace_encode(e);
        Leaf { v: 7 }.ace_encode(e)
    }
}

fn catch_probe<E>(element: impl Fn(u32) -> E) -> CatchesDeclineThenReencodes<E> {
    CatchesDeclineThenReencodes((0..128u32).map(element).collect())
}

/// 原反例的起址条件：从 `backing` 内切出恰好 `len` 字节、起址 8 字节对齐的 `scratch`。
fn aligned_scratch(backing: &mut [u8], len: usize) -> &mut [u8] {
    let lead = (8 - backing.as_ptr() as usize % 8) % 8;
    let scratch = &mut backing[lead..lead + len];
    assert_eq!(
        scratch.as_ptr() as usize % 8,
        0,
        "本用例的前提是 8 字节对齐起址"
    );
    scratch
}

/// 审核原反例（`Ok(None)` 撤销项的签认证据）：128 元素集合（体长 516B、`buf_reserve` 从 64 起
/// 翻倍预留 1024B）的新钩子已声明但本次返回 `Ok(None)`，随后旧路径编码 `Leaf { v: 7 }`（需
/// 992B 工作区）。`dst` 恰为该值的 raw wire 长度、`scratch` 1280B、起址 8 字节对齐。
#[test]
fn declined_bulk_into_trial_gives_back_the_uncommitted_reservation() {
    const COUNTEREXAMPLE_SCRATCH: usize = 1280;
    let options = options(false);
    let expected = expected_bytes(&Leaf { v: 7 }, false);
    let declined = catch_probe(DeclinedLarge);
    let plain = catch_probe(PlainLarge);
    assert_eq!(
        expected_bytes(&declined, false),
        expected,
        "128 元素集合的旧 hook 失败不得写出字节，wire 只来自随后的 Leaf"
    );

    // 对照：同一调用，但 128 元素类型不声明新钩子（不进试准备）。
    let mut control_backing = vec![0u8; COUNTEREXAMPLE_SCRATCH + 8];
    let mut control_dst = vec![0u8; expected.len()];
    assert_eq!(
        encode_into(
            &plain,
            &mut control_dst,
            aligned_scratch(&mut control_backing, COUNTEREXAMPLE_SCRATCH),
            &options,
        ),
        Ok(expected.len()),
        "不开快路径的对照必须成功"
    );

    LARGE_DECLINED_CALLS.with(|c| c.set(0));
    let mut backing = vec![0u8; COUNTEREXAMPLE_SCRATCH + 8];
    let mut dst = vec![0u8; expected.len()];
    assert_eq!(
        encode_into(
            &declined,
            &mut dst,
            aligned_scratch(&mut backing, COUNTEREXAMPLE_SCRATCH),
            &options,
        ),
        Ok(expected.len()),
        "撤销 1024B 未提交试探预留后旧路径必须成功（不撤销时为 Err(1)）"
    );
    assert_eq!(
        dst, control_dst,
        "撤销后 wire 必须与不开快路径的对照逐字节一致"
    );
    assert_eq!(
        LARGE_DECLINED_CALLS.with(Cell::get),
        1,
        "新钩子必须确实被调用一次（证明进了快路径而非被跳过）"
    );
}

/// 原反例的紧容量条件：`scratch` 1024B 时 516B 体长的物理预留要 1024B、记录描述符再放不下，
/// 故预检失败（`NotStarted`）——钩子根本不被调用，旧路径随后用同一 `scratch` 编码
/// `Leaf { v: 7 }`（需 992B）仍成功，wire 与不开快路径逐字节一致。
#[test]
fn not_started_bulk_into_trial_keeps_the_old_path_result() {
    const COUNTEREXAMPLE_TIGHT_SCRATCH: usize = 1024;
    let options = options(false);
    let expected = expected_bytes(&Leaf { v: 7 }, false);
    let declined = catch_probe(DeclinedLarge);
    let plain = catch_probe(PlainLarge);

    let mut control_backing = vec![0u8; COUNTEREXAMPLE_TIGHT_SCRATCH + 8];
    let mut control_dst = vec![0u8; expected.len()];
    assert_eq!(
        encode_into(
            &plain,
            &mut control_dst,
            aligned_scratch(&mut control_backing, COUNTEREXAMPLE_TIGHT_SCRATCH),
            &options,
        ),
        Ok(expected.len()),
        "不开快路径的对照在 1024B 下必须成功"
    );

    LARGE_DECLINED_CALLS.with(|c| c.set(0));
    let mut backing = vec![0u8; COUNTEREXAMPLE_TIGHT_SCRATCH + 8];
    let mut dst = vec![0u8; expected.len()];
    assert_eq!(
        encode_into(
            &declined,
            &mut dst,
            aligned_scratch(&mut backing, COUNTEREXAMPLE_TIGHT_SCRATCH),
            &options,
        ),
        Ok(expected.len()),
        "预检失败只表示快路径不可用，旧路径仍须成功"
    );
    assert_eq!(dst, control_dst, "预检失败不得改变 wire");
    assert_eq!(
        LARGE_DECLINED_CALLS.with(Cell::get),
        0,
        "预检失败时钩子不得被调用"
    );
}

/// w44 的 `B_pre > 0` 见证：同一 `encode_into` 调用里，字段 0 的 `Leaf` 先写出**已提交**的记录体
/// （其记录体、描述符与对象帧都在 arena 内，故字段 1 试探前的占用 `B_pre > 0`），字段 1 这个普通
/// 记录（2 元素集合）的新钩子已声明但本次返回 `Ok(None)`，字段 2 的大集合（体长 1028B）再把撤销
/// 之后的分配推过前序占用。`declining` 换 `PlainProbe` 即「不开快路径」的对照。
struct CommittedThenDeclined<E> {
    declining: Vec<E>,
    tail: Vec<PlainProbe>,
}

impl<E: AceEncode> AceEncode for CommittedThenDeclined<E> {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;
        e.begin_field(0)?;
        let encoded = Leaf { v: 7 }
            .ace_encode(e)
            .and_then(|()| e.begin_field(1))
            .and_then(|()| self.declining.ace_encode(e))
            .and_then(|()| e.begin_field(2))
            .and_then(|()| self.tail.ace_encode(e));
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}

/// w44（`B_pre > 0`）：撤销必须把 `B` **恢复到试探前的实际占用** `B_pre`，不得「一律清零」——
/// 现有两条撤销回归都是 `B_pre = 0`，恢复与清零在其上不可区分。
///
/// 判据（复用既有的最小 `scratch` 对照手法，不新建夹具族）：整次成功所需的最小同起址 `scratch`
/// 必须与「不开快路径」的对照**相等**——泄漏试探预留会把下界抬高该预留（64B）；「一律清零」则
/// 把前序已提交的记录体排除在 arena 占用之外，随后提交它时触发库自身的「arena 区间必须落在已
/// 占用 arena 内」断言。最后按对照的下界实跑一次，wire 必须与对照逐字节一致——前序记录体的值
/// `7` 与撤销之后的 `9` 都还在。
#[test]
fn declined_bulk_into_trial_restores_the_committed_occupancy() {
    let control = CommittedThenDeclined {
        declining: vec![PlainProbe(1), PlainProbe(2)],
        tail: (0..256u32).map(PlainProbe).collect(),
    };
    let declined = CommittedThenDeclined {
        declining: vec![DeclinedProbe(1), DeclinedProbe(2)],
        tail: (0..256u32).map(PlainProbe).collect(),
    };
    let options = options(false);
    let expected = expected_bytes(&declined, false);
    assert_eq!(
        expected_bytes(&control, false),
        expected,
        "两种探针的 wire 必须一致，否则本条对照不成立"
    );

    let mut backing = vec![0u8; SCRATCH];
    let control_need = min_scratch_with(&control, false, &mut backing);
    let declined_need = min_scratch_with(&declined, false, &mut backing);
    assert_eq!(
        declined_need, control_need,
        "撤销必须恢复到 B_pre：泄漏试探预留会抬高下界，清零则丢失已提交占用"
    );

    DECLINED_CALLS.with(|c| c.set(0));
    let mut dst = vec![0u8; expected.len()];
    assert_eq!(
        encode_into(&declined, &mut dst, &mut backing[..control_need], &options),
        Ok(expected.len()),
        "撤销后旧路径必须按不开快路径的下界成功"
    );
    assert_eq!(dst, expected, "撤销后 wire 必须与不开快路径逐字节一致");
    assert_eq!(
        DECLINED_CALLS.with(Cell::get),
        1,
        "新钩子必须确实被尝试一次（证明这次调用进了撤销分支）"
    );
}

/// B2（OR 判据，仅 `direct` 声明分支）：`i32` 不声明新钩子能力（访问器 `None`）但声明了旧直写
/// （`direct != None`），启用判据的独立 OR 成立；选到的新方法就是同一 trait 的默认实现（返回
/// `Ok(None)`），因此回落到对应既有直写，写出与拥有值形态逐字节一致的 wire。
#[test]
fn direct_only_declaration_routes_to_the_direct_writer() {
    if cfg!(target_endian = "little") {
        assert!(
            <i32 as AceEncode>::ace_encode_elements_bulk_into_fn().is_none(),
            "i32 不声明新钩子能力（访问器为 None）"
        );
        assert!(
            <i32 as AceEncode>::ace_encode_elements_bulk_direct().is_some(),
            "i32 声明了旧直写（direct != None），独立 OR 判据必须成立"
        );
    }
    let value = vec![1i32, -2, 3];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Ok(expected.len()),
            "仅 direct 声明也必须写出正确 wire（packed = {packed}）"
        );
        assert_eq!(dst, expected);
    }
}

/// I1：新钩子返回的长度不等于预期体长时，库必须在**提交前**拒绝——按既有 `1` 返回、不提交、
/// 不谎报成功。
#[derive(Debug, PartialEq, Eq)]
struct MismatchProbe(u32);

impl AceElement for MismatchProbe {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 4 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn decode_element(d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(4)?;
        Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
    }
    fn ace_encode_elements_bulk_into(items: &[Self], dst: &mut [u8]) -> Result<Option<usize>> {
        // 写满切片却谎报「少一字节」：库必须按既有 `1` 拒绝、且不提交。
        for (index, item) in items.iter().enumerate() {
            dst[4 + index * 4..8 + index * 4].copy_from_slice(&item.0.to_le_bytes());
        }
        dst[..4].copy_from_slice(&(items.len() as u32).to_le_bytes());
        Ok(Some(dst.len().saturating_sub(1)))
    }
    fn encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>> {
        Some(<Self as AceElement>::ace_encode_elements_bulk_into)
    }
}

impl AceEncode for MismatchProbe {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        Some(4)
    }
    fn ace_encode_elements_bulk_into(items: &[Self], dst: &mut [u8]) -> Result<Option<usize>>
    where
        Self: Sized,
    {
        <Self as AceElement>::ace_encode_elements_bulk_into(items, dst)
    }
    fn ace_encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>>
    where
        Self: Sized,
    {
        Some(<Self as AceEncode>::ace_encode_elements_bulk_into)
    }
}

#[test]
fn bulk_into_length_mismatch_is_rejected_before_commit() {
    let value = vec![MismatchProbe(1), MismatchProbe(2)];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Err(Error::InvalidArgument),
            "长度不符必须按既有 1 拒绝、不得谎报成功（packed = {packed}）"
        );
    }
}

/// B1 回归：定宽、声明新钩子能力，但旧 hook 返回既有 `4`。根回调捕获该 `4` 后改写成根标量。
/// 紧 `scratch`（64B）下旧路径本不开记录、足以成功；新路径的试准备（需 256B 记录描述符）不得
/// 锁存容量 `1`，否则 `finish_into` 会把它当首错返回。
#[derive(Debug, PartialEq, Eq)]
struct BulkTrialError(u32);

impl AceElement for BulkTrialError {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 4 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn decode_element(d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(4)?;
        Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
    }
    fn encode_elements_bulk(_slice: &[Self], _out: &mut Vec<u8>) -> Result<bool> {
        Err(Error::InvalidTag)
    }
    fn ace_encode_elements_bulk_into(_slice: &[Self], _dst: &mut [u8]) -> Result<Option<usize>> {
        Ok(None)
    }
    fn encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>> {
        Some(<Self as AceElement>::ace_encode_elements_bulk_into)
    }
}

impl AceEncode for BulkTrialError {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        Some(4)
    }
    fn ace_encode_elements_bulk(_items: &[Self], _out: &mut Vec<u8>) -> Result<bool>
    where
        Self: Sized,
    {
        Err(Error::InvalidTag)
    }
    fn ace_encode_elements_bulk_into(_items: &[Self], _dst: &mut [u8]) -> Result<Option<usize>>
    where
        Self: Sized,
    {
        Ok(None)
    }
    fn ace_encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>>
    where
        Self: Sized,
    {
        Some(<Self as AceEncode>::ace_encode_elements_bulk_into)
    }
}

/// 捕获内层旧 hook 的 `4` 后写根标量：整次编码本应成功。
struct CatchesBulkErrorThenRootScalar;

impl AceEncode for CatchesBulkErrorThenRootScalar {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        let inner = vec![BulkTrialError(1)];
        // 忽略内层旧 hook 的既有 `4`，改写根标量。
        let _ = inner.ace_encode(e);
        e.encode_scalar(7u64.to_le_bytes())
    }
}

#[test]
fn bulk_into_trial_keeps_old_path_error_priority_on_a_tight_scratch() {
    assert!(
        <BulkTrialError as AceEncode>::ace_encode_elements_bulk_into_fn().is_some(),
        "本探针必须声明新钩子能力，才会进入试准备"
    );
    let value = CatchesBulkErrorThenRootScalar;
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        // 64B 恰好够根标量的 assist（64B），但不够普通记录描述符（8 × 32 = 256B）。
        let mut scratch = vec![0u8; 64];
        let written = encode_into(&value, &mut dst, &mut scratch, &options);
        assert_eq!(
            written,
            Ok(expected.len()),
            "紧 scratch 下旧路径本可成功：试准备不得锁存容量 1（packed = {packed}）"
        );
        assert_eq!(dst, expected, "wire 必须与拥有值形态一致（packed = {packed}）");
    }
    // B1（288B raw）：描述符（8 × 32 = 256B）单独放得下，但随后的体缓冲（`buf_reserve` 从
    // 64B 起）放不下。旧路径本可成功（捕获旧 hook 的 `4` 后只写根标量，只需 64B assist，
    // 不打开任何记录），故试准备不得先占下描述符、也不得把容量失败锁存。
    let raw = options(false);
    let expected = expected_bytes(&value, false);
    assert_eq!(expected.len(), 24, "根标量 wire 恰为 24B");
    let mut dst = vec![0u8; expected.len()];
    let mut scratch = vec![0u8; 288];
    assert_eq!(
        encode_into(&value, &mut dst, &mut scratch, &raw),
        Ok(24),
        "288B raw：描述符放得下但体缓冲放不下，旧路径必须仍为 Ok(24)"
    );
    assert_eq!(dst, expected, "288B raw 的 wire 必须与拥有值形态一致");
}

/// B2 探针：**只**声明旧直写（新钩子的能力访问器保持默认 `None`），却实际覆写了新方法。
/// OR 判据由 `direct != None` 成立；OR 成立后必须优先用访问器指针、否则用该 trait 方法，
/// 因此新方法必须被命中一次，旧直写不得执行。
#[derive(Debug, PartialEq, Eq)]
struct DirectOnlyProbe(u32);

thread_local! {
    /// 仅旧直写声明时新 trait 方法的命中次数。
    static DIRECT_ONLY_METHOD_CALLS: Cell<usize> = const { Cell::new(0) };
    /// 同一探针的旧直写命中次数（命中新方法后必须为 0）。
    static DIRECT_ONLY_DIRECT_CALLS: Cell<usize> = const { Cell::new(0) };
}

impl DirectOnlyProbe {
    fn write_direct(items: &[Self], e: &mut Encoder<'_>) -> Result<()> {
        DIRECT_ONLY_DIRECT_CALLS.with(|c| c.set(c.get() + 1));
        e.write_body_bytes(&(items.len() as u32).to_le_bytes())?;
        for item in items {
            e.write_body_bytes(&item.0.to_le_bytes())?;
        }
        Ok(())
    }
}

impl AceElement for DirectOnlyProbe {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 4 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&self.0.to_le_bytes())
    }
    fn decode_element(d: &mut ace_serialize::decode::Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(4)?;
        Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
    }
}

impl AceEncode for DirectOnlyProbe {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(u64::from(self.0).to_le_bytes())
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        Some(4)
    }
    fn ace_encode_elements_bulk_direct(
    ) -> Option<fn(&[Self], &mut Encoder<'_>) -> Result<()>>
    where
        Self: Sized,
    {
        Some(Self::write_direct)
    }
    fn ace_encode_elements_bulk_into(items: &[Self], dst: &mut [u8]) -> Result<Option<usize>> {
        let needed = items
            .len()
            .checked_mul(4)
            .and_then(|n| n.checked_add(4))
            .ok_or(Error::ResourceLimit)?;
        if dst.len() < needed {
            return Err(Error::InvalidArgument);
        }
        DIRECT_ONLY_METHOD_CALLS.with(|c| c.set(c.get() + 1));
        dst[..4].copy_from_slice(&(items.len() as u32).to_le_bytes());
        for (index, item) in items.iter().enumerate() {
            let start = 4 + index * 4;
            dst[start..start + 4].copy_from_slice(&item.0.to_le_bytes());
        }
        Ok(Some(needed))
    }
    // 能力声明访问器保持默认 `None`：OR 只能由旧直写成立。
}

/// B2：仅 `direct` 声明时 OR 成立后必须选到**新 trait 方法**（访问器 `None` 不足以改走旧直写）。
#[test]
fn direct_only_declaration_with_an_overridden_new_method_selects_the_method() {
    assert!(
        <DirectOnlyProbe as AceEncode>::ace_encode_elements_bulk_into_fn().is_none(),
        "本探针的能力访问器保持默认 `None`"
    );
    assert!(
        <DirectOnlyProbe as AceEncode>::ace_encode_elements_bulk_direct().is_some(),
        "本探针只声明旧直写：OR 判据必须由 direct 成立"
    );
    let value = vec![DirectOnlyProbe(1), DirectOnlyProbe(2)];
    for packed in [false, true] {
        let options = options(packed);
        let expected = expected_bytes(&value, packed);
        let mut dst = vec![0u8; expected.len()];
        let mut scratch = vec![0u8; SCRATCH];
        DIRECT_ONLY_METHOD_CALLS.with(|c| c.set(0));
        DIRECT_ONLY_DIRECT_CALLS.with(|c| c.set(0));
        assert_eq!(
            encode_into(&value, &mut dst, &mut scratch, &options),
            Ok(expected.len()),
            "仅 direct 声明也必须写出正确 wire（packed = {packed}）"
        );
        assert_eq!(dst, expected);
        assert_eq!(
            DIRECT_ONLY_METHOD_CALLS.with(Cell::get),
            1,
            "仅 direct 声明时必须选到新 trait 方法（packed = {packed}）"
        );
        assert_eq!(
            DIRECT_ONLY_DIRECT_CALLS.with(Cell::get),
            0,
            "命中新方法后旧直写不得执行（packed = {packed}）"
        );
    }
}
