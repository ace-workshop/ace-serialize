//! 46 个 C# 实机 case 的 `(case, pack)` 对照回归（37 raw + 9 packed）。
//!
//! 地真来源：`_recon/D-ground-truth-bytes.md` §5（每个 case 的 DUMP 与 BYTE_NOTES）；
//! case 编号与「保留／修正」口径见 `docs/requirements.md` §5 的 46-case 表；分类口径见
//! `docs/plan.md` WP-E6 / AC-R01。**每一行都同时断言往返**：编码 → 解码 → 与原值逐字段相等
//! （共享身份按 id 拓扑单独断言 `Rc::ptr_eq`），再按类别断言字节。测试名与断言消息都带类别：
//!
//! - `[csharp-bytes]`（5 行）：C# 语义与字节都重合，逐字节断言 §5 的 DUMP。跳过区间只允许出现在
//!   BYTE_NOTES 明写「out-of-value adjacent bytes (not guaranteed zero)」或「8-byte pointer read
//!   past 4-byte int field tag」的偏移上；长度不同即 FAIL。当前 5 行都没有这类标注，故跳过表为空。
//! - `[rust-golden]`（37 行）：C# 语义保留或按需求「修正」，但字节布局与本库不同，**不做 C# 逐字节
//!   断言**，改断言**独立参考实现**生成的期望字节：`tools/gen_golden.py`（Python 标准库，按
//!   `docs/requirements.md` 的 FR-W01–FR-W12 / FR-P01–FR-P03 / FR-T03 / FR-T04 与
//!   `_recon/code-spec/E-code-level-spec.md` 的线格式段落实现，**不调用本库、不读取本库输出**）。
//!   期望字节在 `tests/data/ace_wire_golden.hex`，每行标注脚本内 case 标识与规范章节，
//!   `python3 tools/gen_golden.py --check` 可重新生成并逐字节比对。每行注释写明 C# 长度、本库长度
//!   与差异性质（结构推导摘要保留在注释里）。
//!   **这 37 行再按「规范是否唯一规定」分两层**（见本文件 `wire_facts` / `canonical_wire` 一节）：
//!   ① 规范唯一规定 → 逐字节断言：envelope 长度算术 M+A+8（FR-W01）、record 头 `i32 id + i32 size +
//!      body` 且 main 被整数条 record 恰好铺满（FR-W02/W11）、槽 16B 形态（FR-W03）、可省略闭集与
//!      `Option::None` 省略（FR-W04）、对象槽范围（FR-W06/W07）、UTF-8 原文与集合 `u32 count + 元素`
//!      （FR-W08/W09）、Map 按 key 字节序（FR-T03）、根槽在 `assist[0]` 且 depth=0/tag=0（FR-W10）、
//!      数值归一/扩展位模式（FR-W12）、packed（FR-P01..FR-P03）。引用型 payload 另按固定夹具的
//!      **引用位置表**逐个断言「完整 8B 高位为零 + 目标 id 存在」（`ReferenceLayout`），标量 payload
//!      原样保留，不在比较时抹掉差异。
//!   ② 规范未唯一规定 → **这是实现选择，不是规范规定**，共三维：record id 的具体编号次序（FR-W02 只
//!      要求正整数、消息内唯一）、对象槽块在 assist 中的出现次序（FR-W06 只要求可达对象范围按记录出现
//!      次序单调递增、互不重叠）、packed `ff` run 的延续分段（FR-P02 把延续选择交给 PACK：把后续 word
//!      原样并入 run 还是在此截断，解包字节相同）。前两维改断言从盘上字节重算的不变量并规范化后逐字节
//!      比较其余内容；packed 分段维只做「格式检查 + FR-P03 零补齐 + 解包后等于该行 raw 金样」。
//!   因此分层是 **29 行直接逐字节硬比、8 行只在非唯一维度归一/查不变量**：8 = 7 个多 record 的 raw 行
//!   （`e.nested`/`f.dict3`/`g.shared`/`g.equal`/`i.complex`/`j.interface`/`j.abstract`）+ packed 的
//!   `b.long`（`ff` run 后面还有 word）。其余 8 个 packed 行的 `ff` run 都在流末尾、也没有相邻零 word，
//!   packed 字节由规范/PACK 唯一决定，仍硬比。清单只写在 `GOLDEN_COMPARISONS` 一处，元断言核对。
//! - `[rust-roundtrip]`（4 行）：C# 侧没有返回 byte[]（异常/abort），**没有 C# 参照**
//!   （`UNABLE-C#-ref`），故只按 Rust 新规范做往返与身份拓扑断言——不是「通过 C# 字节对照」。
//!
//! 背景（不是豁免）：`docs/requirements.md` 第 45 行记「Rust … 不与 C# 字节互通：数值规范扩展、
//! 统一 root、u32 count、对象槽范围、数值多态及标准 packed 都是明确差异」。因此 46 行中只有 5 行
//! 能按 C# 字节判定；需要跳过邻接内存的 9 行（`a.int`/`a.short`/`a.byte`/`a.bool.*`/`a.float`/
//! `a.char`/`c.struct`/`d.class`）因整段布局不同，已归入 `[rust-golden]`，不参与 C# 逐字节断言。

use ace_serialize::decode::{decode_cyclic, Decoder};
use ace_serialize::limits::Options;
use ace_serialize::schema::{AceDecode, AceEncode};
use ace_serialize::{decode, encode};
use ace_serialize_derive::AceSerialize;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::{Rc, Weak};

const GROUND_TRUTH: &str = "_recon/D-ground-truth-bytes.md §5";

/// `tools/gen_golden.py` 独立生成的 37 行 `[rust-golden]` 期望字节。
/// 每行：`case 模式 脚本 case 标识 规范章节 hex`；由 `--check` 与脚本输出逐字节比对。
const RUST_GOLDEN_TABLE: &str = include_str!("data/ace_wire_golden.hex");

/// C# `WireDump.TwoInts`（`c.struct`）：X、Y 按声明序 order 0/1。
#[derive(AceSerialize, Debug, PartialEq)]
struct TwoInts {
    x: i32,
    y: i32,
}

/// C# `WireDump.OneInt`（`d.class`、`d.default`、`i.complex` 的 Child、`k.class.array.null`）。
#[derive(AceSerialize, Debug, PartialEq)]
struct OneInt {
    value: i32,
}

/// C# `WireDump.LevelA/LevelB/LevelC`（`e.nested`）：A→B→C，C.Value=99。
#[derive(AceSerialize, Debug, PartialEq)]
struct LevelA {
    next: LevelB,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct LevelB {
    next: LevelC,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct LevelC {
    value: i32,
}

/// C# `WireDump.SharedHolder`（`g.shared`/`g.equal`）：Left/Right 两个 `OneInt` 字段。
#[derive(AceSerialize, Debug, PartialEq)]
struct SharedHolder {
    left: Rc<OneInt>,
    right: Rc<OneInt>,
}

/// C# `WireDump.Complex`（`i.null.fields`/`i.complex`）：Maybe、Items、Child、Text。
#[derive(AceSerialize, Debug, PartialEq)]
struct Complex {
    maybe: Option<i32>,
    items: Option<Vec<i32>>,
    child: Option<OneInt>,
    text: Option<String>,
}

/// C# `WireDump.Cat`/`WireDump.Dog`（`j.interface`/`j.abstract`）：Rust 侧以数值 tag 的多态
/// enum 承载派生类型，tag 0=Cat(Lives)、tag 1=Dog(Age)。
#[derive(AceSerialize, Debug, PartialEq)]
enum Animal {
    #[ace(tag = 0)]
    Cat { lives: i32 },
    #[ace(tag = 1)]
    Dog { age: i32 },
}

/// C# `WireDump.InterfaceHolder`/`WireDump.AbstractHolder` 的单字段容器。
#[derive(AceSerialize, Debug, PartialEq)]
struct AnimalHolder {
    pet: Animal,
}

/// C# `WireDump.CycleA`/`WireDump.CycleB`（`h.cycle`）的 Weak 回边模型：B.Next 是回到根的弱边。
#[derive(AceSerialize)]
struct CycleA {
    next: Rc<CycleB>,
}

#[derive(AceSerialize)]
struct CycleB {
    next: Weak<CycleA>,
}

fn raw_options() -> Options {
    Options {
        packed: false,
        ..Options::default()
    }
}

/// 用等价 Rust 值编码；失败即用例自身前提不成立（不是对照结果）。
fn encoded<T: AceEncode>(value: &T, packed: bool) -> Vec<u8> {
    encode(
        value,
        &Options {
            packed,
            ..Options::default()
        },
    )
    .expect("等价 Rust 值必须能编码")
}

/// 往返断言（每一行都要有）：编码 → 解码 → 与原值逐字段相等；返回解码值供身份拓扑断言。
fn roundtrip<T>(value: &T, packed: bool) -> T
where
    T: AceEncode + AceDecode + PartialEq + core::fmt::Debug,
{
    let options = Options {
        packed,
        ..Options::default()
    };
    let bytes = encode(value, &options).expect("往返：编码必须成功");
    let decoded = decode::<T>(&bytes, &options).expect("往返：本库输出必须能被本库解码");
    assert_eq!(
        decoded, *value,
        "往返：解码结果必须与原值逐字段相等（packed = {packed}）"
    );
    decoded
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("金样 hex 只含小写十六进制与空白"),
    }
}

fn decode_hex(hex: &str) -> Vec<u8> {
    let digits: Vec<u8> = hex
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert!(digits.len() % 2 == 0, "金样 hex 必须是偶数个十六进制字符");
    digits
        .chunks(2)
        .map(|pair| (nibble(pair[0]) << 4) | nibble(pair[1]))
        .collect()
}

fn hex_all(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn hex_head(bytes: &[u8]) -> String {
    hex_all(&bytes[..bytes.len().min(16)])
}

/// `b.long` 的 raw 期望字节（D §5 :582–769 的 DUMP：main=138 / `8a 00 00 00`，字符串体 130 字节 `x`）。
/// 两处共用：`[csharp-bytes]` 行 `case_b_long_raw_csharp_bytes` 的硬比，以及 packed 行的
/// run 不变量解包后比较（`b.long` 的 raw 是 `[csharp-bytes]` 行，不在 `[rust-golden]` 表的 raw 行里）。
fn b_long_raw_dump_hex() -> String {
    let mut expected = String::from("8a0000000100000082000000");
    expected.push_str(&"78".repeat(130));
    expected.push_str("1000000000000000000000000100000000000000");
    expected
}

/// `[csharp-bytes]`：逐字节对照 §5 的 DUMP。`skipped` 只放 BYTE_NOTES 明写「不确定」的消息内偏移
/// 区间 `[start, end)`（当前 5 行都没有这类标注，故全部传 `&[]`）；长度不同即 FAIL。
#[track_caller]
fn assert_csharp_bytes(
    case: &str,
    pack: bool,
    expected_hex: &str,
    skipped: &[(usize, usize)],
    actual: &[u8],
) {
    let expected = decode_hex(expected_hex);
    let mode = if pack { "pack=true" } else { "pack=false" };
    let source = format!("{GROUND_TRUTH} `### {case} / {mode}`");
    for &(start, end) in skipped {
        assert!(
            start < end && end <= expected.len(),
            "[csharp-bytes] case {case} ({mode}): 跳过区间 {start}..{end} 越界"
        );
    }
    if actual.len() != expected.len() {
        panic!(
            "[csharp-bytes] case {case} ({mode}) FAIL: 长度不同，C# {} 字节 / 本库 {} 字节\n  C# 前 16 字节: {}\n  本库前 16 字节: {}\n  本库全量: {}\n  出处: {source}",
            expected.len(),
            actual.len(),
            hex_head(&expected),
            hex_head(actual),
            hex_all(actual),
        );
    }
    for (offset, &want) in expected.iter().enumerate() {
        if skipped
            .iter()
            .any(|&(start, end)| (start..end).contains(&offset))
        {
            continue;
        }
        let got = actual[offset];
        assert!(
            got == want,
            "[csharp-bytes] case {case} ({mode}) FAIL: 偏移 0x{offset:04x}（{offset}）C#=0x{want:02x} 本库=0x{got:02x}\n  C# 前 16 字节: {}\n  本库前 16 字节: {}\n  本库全量: {}\n  出处: {source}",
            hex_head(&expected),
            hex_head(actual),
            hex_all(actual),
        );
    }
}

/// 按 `(case, pack)` 从金样表取 `tools/gen_golden.py` 生成的期望字节；表里缺项即 FAIL（清单与
/// 测试行必须一一对应）。
#[track_caller]
fn rust_golden(case: &str, pack: bool) -> Vec<u8> {
    let mode = if pack { "pack" } else { "raw" };
    for line in RUST_GOLDEN_TABLE.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(
            fields.len(),
            5,
            "金样表每行必须是「case 模式 脚本 case 标识 规范章节 hex」5 列，实际 {line:?}"
        );
        if fields[0] == case && fields[1] == mode {
            return decode_hex(fields[4]);
        }
    }
    panic!("[rust-golden] case {case} ({mode}) FAIL: tests/data/ace_wire_golden.hex 里没有该条目");
}

// ---------------------------------------------------------------------------
// 金样分层：逐字节硬比 vs 非唯一维度归一/查不变量（29 + 8 行）
//
// 依据 `docs/requirements.md`，无唯一规定的维度共三个：FR-W02 只要求 record id 为正整数、消息内唯一
// （**未**规定编号与落盘次序）；FR-W06 只要求可达对象槽范围按记录出现次序 offset 单调递增、互不重叠
// （**未**规定记录次序本身）；FR-P02 把 packed `ff` run 的延续选择交给 PACK（把后续 word 原样并入 run
// 还是在此截断，解包字节相同）。因此「record id 编号次序」「对象槽块出现次序」「packed run 分段」都是
// **实现选择，不是规范规定**——本库/参考脚本取后序（子 record 先落盘先拿号）与「后续全非零 word 的
// 最大连续段」，先序/其它合法分段会给出不同字节。
// `wire_facts()` 从字节重算前两维的不变量（并逐位置校验引用高位与目标存在性）；`canonical_wire()` 只
// 把前两维归一，其余字节原样保留后逐字节比较；`assert_packed_run_invariant()` 对第三维只做格式检查 +
// FR-P03 零补齐 + 解包后与原 raw 逐字节比较。envelope 算术、record 头/size、槽 16B 形态、depth/tag、
// payload 位模式、正文、根槽位置仍全部逐字节判定。
// ---------------------------------------------------------------------------

/// 单行的比较策略。`GOLDEN_COMPARISONS` 是**唯一事实来源**：`assert_rust_golden` 与元断言
/// `rust_golden_rows_follow_the_declared_comparison` 都只读这张表，因此分层不会与断言脱节。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GoldenComparison {
    /// 规范（含 PACK）唯一决定 → 逐字节硬比。
    Bytes,
    /// record id 编号次序 / 对象槽块出现次序非唯一 → 从盘上字节重算不变量 + 规范化后逐字节。
    LayoutNormalized,
    /// packed `ff` run 的延续分段非唯一 → 格式检查 + FR-P03 零补齐 + 解包后等于该行 raw 金样。
    PackedRunInvariant,
}

/// 37 行 `(case, pack, 策略)`：`Bytes` 29 行、`LayoutNormalized` 7 行、`PackedRunInvariant` 1 行。
const GOLDEN_COMPARISONS: &[(&str, bool, GoldenComparison)] = &[
    // 21 个单/零 record 的 raw 行：布局没有自由度 → 逐字节。
    ("a.long", false, GoldenComparison::Bytes),
    ("a.int", false, GoldenComparison::Bytes),
    ("a.short", false, GoldenComparison::Bytes),
    ("a.byte", false, GoldenComparison::Bytes),
    ("a.bool.true", false, GoldenComparison::Bytes),
    ("a.bool.false", false, GoldenComparison::Bytes),
    ("a.float", false, GoldenComparison::Bytes),
    ("a.double", false, GoldenComparison::Bytes),
    ("a.char", false, GoldenComparison::Bytes),
    ("c.struct", false, GoldenComparison::Bytes),
    ("d.class", false, GoldenComparison::Bytes),
    ("d.default", false, GoldenComparison::Bytes),
    ("f.array0", false, GoldenComparison::Bytes),
    ("f.array3", false, GoldenComparison::Bytes),
    ("f.array200", false, GoldenComparison::Bytes),
    ("f.list0", false, GoldenComparison::Bytes),
    ("f.list3", false, GoldenComparison::Bytes),
    ("f.dict0", false, GoldenComparison::Bytes),
    ("i.nullable.value", false, GoldenComparison::Bytes),
    ("i.nullable.zero", false, GoldenComparison::Bytes),
    ("i.null.fields", false, GoldenComparison::Bytes),
    // 7 个多 record 的 raw 行：record id 编号与槽块次序有自由度 → 不变量 + 规范化后逐字节。
    ("e.nested", false, GoldenComparison::LayoutNormalized),
    ("f.dict3", false, GoldenComparison::LayoutNormalized),
    ("g.shared", false, GoldenComparison::LayoutNormalized),
    ("g.equal", false, GoldenComparison::LayoutNormalized),
    ("i.complex", false, GoldenComparison::LayoutNormalized),
    ("j.interface", false, GoldenComparison::LayoutNormalized),
    ("j.abstract", false, GoldenComparison::LayoutNormalized),
    // packed：8 行的 `ff` run 都在流末尾且无相邻零 word → 逐字节；`b.long` 的 `ff` run 后面还有 word
    // （金样取 14 个后续 word，改成 15 个解包字节不变）→ 只查格式 + 零补齐 + 解包后比 raw。
    ("a.int", true, GoldenComparison::Bytes),
    ("a.long", true, GoldenComparison::Bytes),
    ("b.chinese", true, GoldenComparison::Bytes),
    ("b.long", true, GoldenComparison::PackedRunInvariant),
    ("c.struct", true, GoldenComparison::Bytes),
    ("d.class", true, GoldenComparison::Bytes),
    ("d.default", true, GoldenComparison::Bytes),
    ("f.array3", true, GoldenComparison::Bytes),
    ("i.nullable.value", true, GoldenComparison::Bytes),
];

/// 取某行的比较策略；表里缺项即 FAIL（策略表必须覆盖 `.hex` 的全部 37 行）。
#[track_caller]
fn golden_comparison(case: &str, pack: bool) -> GoldenComparison {
    GOLDEN_COMPARISONS
        .iter()
        .find(|(name, mode, _)| *name == case && *mode == pack)
        .map(|(_, _, comparison)| *comparison)
        .unwrap_or_else(|| panic!("[rust-golden] case {case}（pack={pack}）缺少比较策略"))
}

/// 固定夹具的**引用位置**表。`slots` 是 `(规范化序号, tag)`：该槽的完整 8B payload 必须是引用；
/// `map_keys` 是 `(规范化序号, 键个数)`：该 record 的 Map 正文 `u32 count + (u32 key + 4B 值)×count`
/// 的键是引用（键是 4B record id，没有高 4B）。规范化序号是 `wire_facts` 算出的次序（根 record = 1，
/// 槽按 tag、Map 键按正文次序前序 DFS）。
///
/// 这些位置**必须先断言**：①完整 8B payload 的高 4B 为零（FR-W02/W10）；②低 4B 是存在的 record id
/// （1..=N）。任一不成立直接 FAIL——不允许先 `as u32` 截断再把高位污染抹平，也不允许目标不可达时跳过。
/// 只有上述校验通过后才允许重编号；未列出的 payload 一律按标量位模式原样保留（FR-W12）。
/// 只有含 >1 条 record 的行才可能有 record 内引用，因此表只列这 7 行（元断言核对该覆盖）。
struct ReferenceLayout {
    slots: &'static [(usize, i32)],
    map_keys: &'static [(usize, usize)],
}

static NO_REFERENCES: ReferenceLayout = ReferenceLayout {
    slots: &[],
    map_keys: &[],
};

const GOLDEN_REFERENCES: &[(&str, ReferenceLayout)] = &[
    (
        "e.nested",
        ReferenceLayout {
            // LevelA.next → LevelB、LevelB.next → LevelC；LevelC.value 是标量 99（不在表内）。
            slots: &[(1, 0), (2, 0)],
            map_keys: &[],
        },
    ),
    (
        "f.dict3",
        ReferenceLayout {
            // 根 Map 正文的 3 个键（`a`/`c`/`中`）：键是 4B record id，值 i32 是标量。
            slots: &[],
            map_keys: &[(1, 3)],
        },
    ),
    (
        "g.shared",
        ReferenceLayout {
            // SharedHolder.left/right 同时指向同一个 OneInt record。
            slots: &[(1, 0), (1, 1)],
            map_keys: &[],
        },
    ),
    (
        "g.equal",
        ReferenceLayout {
            // SharedHolder.left/right 指向两个相等但不同身份的 OneInt record。
            slots: &[(1, 0), (1, 1)],
            map_keys: &[],
        },
    ),
    (
        "i.complex",
        ReferenceLayout {
            // Complex.maybe 是标量 5（不在表内）；items/child/text 是引用型 Some(..)。
            slots: &[(1, 1), (1, 2), (1, 3)],
            map_keys: &[],
        },
    ),
    (
        "j.interface",
        ReferenceLayout {
            // AnimalHolder.pet → 多态 Cat record；Cat.lives 是标量 9（不在表内）。
            slots: &[(1, 0)],
            map_keys: &[],
        },
    ),
    (
        "j.abstract",
        ReferenceLayout {
            // AnimalHolder.pet → 多态 Dog record；Dog.age 是标量 4（不在表内）。
            slots: &[(1, 0)],
            map_keys: &[],
        },
    ),
];

/// 没有 record 内引用位置的行（单/零 record 行；根槽引用由 FR-W10 单独断言）返回空表。
fn reference_layout(case: &str) -> &'static ReferenceLayout {
    GOLDEN_REFERENCES
        .iter()
        .find(|(name, _)| *name == case)
        .map(|(_, layout)| layout)
        .unwrap_or(&NO_REFERENCES)
}

fn le_i32(bytes: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(bytes[at..at + 4].try_into().expect("i32 需 4 字节"))
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("u32 需 4 字节"))
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("u64 需 8 字节"))
}

/// FR-W02/W10：引用型 payload 是 8B 零扩展的 record id（`高 4B 为零` + 低 4B 是存在的 id）。
/// 高 4B 非零时返回 `None`——不能先截断再判，否则 `0x00000001_00000003` 会被误认为 id 3。
fn wire_reference(payload: u64, ids: &BTreeSet<i32>) -> Option<i32> {
    if payload >> 32 != 0 {
        return None;
    }
    let low = payload as u32;
    if low == 0 || low > i32::MAX as u32 {
        return None;
    }
    ids.contains(&(low as i32)).then_some(low as i32)
}

/// 从 `(record id, body)` 列表里取某条 record 的正文。
fn record_body<'a>(records: &[(i32, &'a [u8])], id: i32) -> &'a [u8] {
    records
        .iter()
        .find(|(record, _)| *record == id)
        .expect("record 必须存在")
        .1
}

/// Map 正文 `u32 count + (u32 键 id + 4B 值)×count`（FR-W09/FR-T03）的键引用：键是 **4B** record id，
/// 没有高 4B（FR-W02「集合内引用 id 用 u32」），值原样保留。长度与 count 与引用位置表不符即 FAIL。
#[track_caller]
fn map_key_ids(body: &[u8], keys: usize, what: &str) -> Vec<u32> {
    assert_eq!(
        body.len(),
        4 + 8 * keys,
        "{what}：Map 正文长度必须等于 u32 count + 8B×键个数（{keys}），实际 {}",
        body.len()
    );
    assert_eq!(
        le_u32(body, 0) as usize,
        keys,
        "{what}：Map 正文 count 必须等于引用位置表声明的键数 {keys}"
    );
    (0..keys).map(|key| le_u32(body, 4 + 8 * key)).collect()
}

/// 对象 record 的槽块事实——全部由盘上字节重算，不读编码器内部状态。
#[derive(Clone, Copy, Debug)]
struct SlotBlock {
    offset: usize,
    count: usize,
    /// `None` = 具体对象（FR-W06）；`Some(_)` = 多态对象 variant_tag（FR-W07）。
    variant_tag: Option<u16>,
    /// 该 record 的嵌套层（FR-T07：root record = 1），由引用图重算。
    depth: u32,
}

/// 从字节重算的 wire 结构；`wire_facts` 的构造过程本身就是不变量断言。
struct WireFacts<'a> {
    /// main 顺序的 `(record id, body)`。
    records: Vec<(i32, &'a [u8])>,
    ids: BTreeSet<i32>,
    /// `(depth, tag, payload)`，与 assist 的 16B 槽一一对应。
    slots: Vec<(i32, i32, u64)>,
    blocks: BTreeMap<i32, SlotBlock>,
    /// 根槽 payload 解析出的 record id（标量根或无根槽为 `None`）。
    root: Option<i32>,
    /// 规范化次序（根 record = 1）：根槽前序 DFS，槽按 tag、Map 键按正文次序。引用位置表的
    /// 序号就是这里的下标 + 1，`canonical_wire` 也按它重编号。
    order: Vec<i32>,
}

impl<'a> WireFacts<'a> {
    fn body(&self, id: i32) -> &'a [u8] {
        record_body(&self.records, id)
    }

    fn reference(&self, payload: u64) -> Option<i32> {
        wire_reference(payload, &self.ids)
    }
}

/// 从盘上字节重算 wire 结构并逐条断言不变量（FR-W01/W02/W03/W06/W07/W10 + FR-T07）。
///
/// 「哪条 record 是对象」只能由槽块覆盖关系从字节推出：正文长 8（FR-W06）或 10（FR-W07）且声明的
/// offset 恰好接在已覆盖范围的末尾（FR-W06 的单调递增 + 不重叠 → 从 1 起贪心覆盖唯一）。37 个金样的
/// 字符串/集合正文长度或首字段都不满足该形态；不满足时这里失败而不是放宽。
///
/// 「引用位置必须真的是引用」：wire 不自描述（FR-W11），槽里的畸形引用无法与标量 payload 区分，因此按
/// **固定夹具的引用位置表**（`ReferenceLayout`）逐位置断言：①完整 8B payload 的高 4B 为零；②低 4B 是
/// 存在的 record id（1..=N）。未列出的 payload 当作标量、原样保留。此外：③根槽 payload 必须指向存在的
/// record id（FR-W10）；④被改坏的引用会让对应的对象 record 失去唯一来源，由「对象 record 必须可从
/// 根槽到达」拒绝（负控用例 `wire_invariants_reject_dangling_root_reference` /
/// `wire_invariants_reject_dangling_second_shared_edge` 印证）。
#[track_caller]
fn wire_facts<'a>(bytes: &'a [u8], what: &str, layout: &ReferenceLayout) -> WireFacts<'a> {
    assert!(bytes.len() >= 8, "{what}：FR-W01 至少要含两个 i32 长度前缀");
    let main_len = le_i32(bytes, 0);
    assert!(
        main_len >= 0 && 4 + main_len as usize + 4 <= bytes.len(),
        "{what}：FR-W01 main 长度越界"
    );
    let main = &bytes[4..4 + main_len as usize];
    let assist_len = le_i32(bytes, 4 + main_len as usize);
    let assist = &bytes[8 + main_len as usize..];
    assert_eq!(
        assist_len as usize,
        assist.len(),
        "{what}：FR-W01 assist 长度与声明不符"
    );
    assert_eq!(
        bytes.len(),
        main_len as usize + assist.len() + 8,
        "{what}：FR-W01 总长必须严格 M+A+8"
    );
    assert_eq!(
        assist.len() % 16,
        0,
        "{what}：FR-W03 assist 必须是 16B 的倍数"
    );

    let slots: Vec<(i32, i32, u64)> = (0..assist.len() / 16)
        .map(|index| {
            let at = index * 16;
            (
                le_i32(assist, at),
                le_i32(assist, at + 4),
                le_u64(assist, at + 8),
            )
        })
        .collect();

    // FR-W02：record 头 `i32 id + i32 size + body`，id 正整数且唯一，main 被整数条 record 恰好铺满。
    let mut records: Vec<(i32, &[u8])> = Vec::new();
    let mut ids = BTreeSet::new();
    let mut position = 0usize;
    while position < main.len() {
        assert!(position + 8 <= main.len(), "{what}：FR-W02 record 头越界");
        let id = le_i32(main, position);
        let size = le_i32(main, position + 4);
        assert!(id > 0, "{what}：FR-W02 record id 必须为正");
        assert!(
            size >= 0 && position + 8 + size as usize <= main.len(),
            "{what}：FR-W02 record size 越界"
        );
        assert!(ids.insert(id), "{what}：FR-W02 record id {id} 重复");
        records.push((id, &main[position + 8..position + 8 + size as usize]));
        position += 8 + size as usize;
    }
    assert_eq!(
        position,
        main.len(),
        "{what}：FR-W02 main 必须被整数条 record 恰好铺满"
    );
    // 「编号从 1 起连续」是本实现的编号约定（FR-W02 只要求正整数、消息内唯一），额外固定以防漂移。
    assert!(
        ids.iter().copied().eq(1..=records.len() as i32),
        "{what}：record id 必须恰为 1..=N（本实现约定）"
    );

    // FR-W10：根槽恒为 assist[0]、depth=0/tag=0、不属于任何对象范围。
    if let Some(&(depth, tag, _)) = slots.first() {
        assert_eq!(
            (depth, tag),
            (0, 0),
            "{what}：FR-W10 根槽必须是 assist[0] 且 depth=0/tag=0"
        );
    }

    // FR-W06：对象槽块独占、不重叠、按记录出现次序 offset 单调递增；FR-W03：块内 tag 严格递增。
    let mut declared_blocks: Vec<(i32, usize, usize, Option<u16>)> = Vec::new();
    let mut covered = 1usize;
    for &(id, body) in &records {
        let (variant_tag, at) = match body.len() {
            8 => (None, 0),
            10 => (Some(u16::from_le_bytes([body[0], body[1]])), 2),
            _ => continue, // 字符串/集合正文（FR-W08/W09），不是对象
        };
        let offset = le_u32(body, at) as usize;
        let count = le_u32(body, at + 4) as usize;
        if offset != covered || offset + count > slots.len() {
            continue;
        }
        assert!(
            offset >= 1,
            "{what}：FR-W06 对象 slot_offset 必须 ≥1（根槽不属于任何对象）"
        );
        let tags: Vec<i32> = slots[offset..offset + count]
            .iter()
            .map(|slot| slot.1)
            .collect();
        assert!(
            tags.windows(2).all(|pair| pair[0] < pair[1]),
            "{what}：FR-W03 record {id} 的槽 tag 必须严格递增（省略字段留空洞），实际 {tags:?}"
        );
        declared_blocks.push((id, offset, count, variant_tag));
        covered = offset + count;
    }
    if slots.is_empty() {
        assert!(
            declared_blocks.is_empty(),
            "{what}：FR-W06 assist 为空时不得有对象槽块"
        );
    } else {
        assert_eq!(
            covered,
            slots.len(),
            "{what}：FR-W06 除 assist[0] 外的槽必须恰好被对象槽块覆盖"
        );
    }

    // FR-T07：嵌套层由「槽 payload 引用 → 子 record」重算（root record = 1，进入新对象 +1），
    // 再把每个对象槽块的 depth 与它的嵌套层、tag 顺序逐槽核对。
    // FR-W10：引用型 root 的完整 8B payload 高 4B 必须为零——**先看完整 8B**，不能 `as u32` 截断后再判，
    // 否则 `0x00000001_00000003` 会被误认为 root record 3 而放过。
    if !main.is_empty() && !slots.is_empty() {
        assert_eq!(
            slots[0].2 >> 32,
            0,
            "{what}：FR-W10 引用型 root 的完整 8B payload 高 4B 必须为零，实际 0x{:016x}",
            slots[0].2
        );
    }
    let root = slots.first().and_then(|slot| wire_reference(slot.2, &ids));
    if !main.is_empty() {
        assert!(
            root.is_some(),
            "{what}：FR-W10 引用型 root 的槽 payload 必须指向存在的 record id，实际 0x{:016x}",
            slots[0].2
        );
    }
    let mut depths: BTreeMap<i32, u32> = BTreeMap::new();
    if let Some(root_id) = root {
        depths.insert(root_id, 1);
        let mut queue = vec![root_id];
        while let Some(id) = queue.pop() {
            let Some(&(_, offset, count, _)) = declared_blocks.iter().find(|block| block.0 == id)
            else {
                continue; // 字符串/集合/Map 记录没有槽块
            };
            let depth = depths[&id];
            for index in offset..offset + count {
                let (slot_depth, tag, payload) = slots[index];
                assert_eq!(
                    slot_depth as u32, depth,
                    "{what}：FR-T07/W03 record {id} 的槽 depth 必须等于其嵌套层 {depth}（槽 {index}、tag {tag}）"
                );
                let Some(target) = wire_reference(payload, &ids) else {
                    continue;
                };
                match depths.get(&target) {
                    Some(&seen) => assert_eq!(
                        seen,
                        depth + 1,
                        "{what}：FR-T07 共享 record {target} 被引用于不同嵌套层（{seen} 与 {}）",
                        depth + 1
                    ),
                    None => {
                        depths.insert(target, depth + 1);
                        queue.push(target);
                    }
                }
            }
        }
    }
    for (id, ..) in &declared_blocks {
        assert!(
            depths.contains_key(id),
            "{what}：FR-W06 对象 record {id} 必须可从根槽到达"
        );
    }
    let blocks: BTreeMap<i32, SlotBlock> = declared_blocks
        .into_iter()
        .map(|(id, offset, count, variant_tag)| {
            (
                id,
                SlotBlock {
                    offset,
                    count,
                    variant_tag,
                    depth: depths[&id],
                },
            )
        })
        .collect();

    // 规范化次序（引用位置表的坐标系，也是 `canonical_wire` 的重编号次序）：根 record 先访问，
    // 槽按 tag、Map 键按正文次序做前序 DFS。
    let mut order: Vec<i32> = Vec::new();
    let mut pending: Vec<i32> = root.into_iter().collect();
    while let Some(id) = pending.pop() {
        if order.contains(&id) {
            continue;
        }
        order.push(id);
        let canonical_index = order.len();
        let mut children: Vec<i32> = Vec::new();
        if let Some(block) = blocks.get(&id) {
            for index in block.offset..block.offset + block.count {
                if let Some(target) = wire_reference(slots[index].2, &ids) {
                    children.push(target);
                }
            }
        }
        for &(index, keys) in layout.map_keys {
            if index == canonical_index {
                let body = record_body(&records, id);
                children.extend(
                    map_key_ids(body, keys, what)
                        .into_iter()
                        .filter_map(|key| wire_reference(u64::from(key), &ids)),
                );
            }
        }
        pending.extend(children.iter().rev().copied());
    }

    // 引用位置校验：每处必须是「8B 零扩展的存在的 record id」。任一处不成立直接 FAIL，
    // 不进入重编号比较（否则高位污染/悬空 id 会被规范化抹平）。
    for &(canonical_index, tag) in layout.slots {
        let id = *order.get(canonical_index - 1).unwrap_or_else(|| {
            panic!(
                "{what}：引用位置表指向规范序号 {canonical_index}，但规范化次序只有 {} 条 record",
                order.len()
            )
        });
        let block = blocks[&id];
        let index = (block.offset..block.offset + block.count)
            .find(|&index| slots[index].1 == tag)
            .unwrap_or_else(|| {
                panic!(
                    "{what}：引用位置表指向 record {id}（规范序号 {canonical_index}）的 tag {tag}，但该槽块没有这个 tag"
                )
            });
        let payload = slots[index].2;
        assert_eq!(
            payload >> 32,
            0,
            "{what}：引用位置（规范序号 {canonical_index}、tag {tag}）的完整 8B payload 高 4B 必须为零（FR-W02/W10），实际 0x{payload:016x}"
        );
        assert!(
            wire_reference(payload, &ids).is_some(),
            "{what}：引用位置（规范序号 {canonical_index}、tag {tag}）的目标 id 必须存在（1..={}），实际 0x{payload:016x}",
            records.len()
        );
    }
    for &(canonical_index, keys) in layout.map_keys {
        let id = *order.get(canonical_index - 1).unwrap_or_else(|| {
            panic!(
                "{what}：引用位置表指向规范序号 {canonical_index}，但规范化次序只有 {} 条 record",
                order.len()
            )
        });
        for key in map_key_ids(record_body(&records, id), keys, what) {
            assert!(
                key > 0 && (key as usize) <= records.len() && ids.contains(&(key as i32)),
                "{what}：Map 键引用（规范序号 {canonical_index}）的目标 id 必须存在（1..={}），实际 {key}",
                records.len()
            );
        }
    }
    assert_eq!(
        order.len(),
        records.len(),
        "{what}：规范化次序必须覆盖全部 record（根槽 + 槽引用 + Map 键），实际 {} / {}",
        order.len(),
        records.len()
    );

    WireFacts {
        records,
        ids,
        slots,
        blocks,
        root,
        order,
    }
}

fn push_slot(assist: &mut Vec<u8>, depth: i32, tag: i32, payload: u64) {
    assist.extend(depth.to_le_bytes());
    assist.extend(tag.to_le_bytes());
    assist.extend(payload.to_le_bytes());
}

/// Map 正文 `u32 count + (u32 key record id + 4B 值)×count`（FR-W09/FR-T03）的键 id 要跟着重编号，
/// 4B 值原样保留。只在引用位置表把该 record 列为 Map 时调用（本批只有 `f.dict3`），
/// 其它正文（字符串、标量集合）字节不依赖布局，原样保留。
#[track_caller]
fn rewrite_map_keys(
    body: &[u8],
    keys: usize,
    new_id: &BTreeMap<i32, i32>,
    what: &str,
) -> Vec<u8> {
    map_key_ids(body, keys, what); // 长度与 count 已在 wire_facts 校验；此处再核一次防调用方漂移
    let mut out = body[..4].to_vec();
    for key in 0..keys {
        let at = 4 + 8 * key;
        let old = le_u32(body, at) as i32;
        out.extend((new_id[&old] as u32).to_le_bytes());
        out.extend(&body[at + 4..at + 8]);
    }
    out
}

/// 只把「record id 编号次序」与「对象槽块出现次序」这两维归一，其余字节原样保留：
/// 1. 次序 = `wire_facts` 算出的规范化次序（根 record 前序 DFS：槽按 tag、Map 键按正文次序），
///    并已断言覆盖全部 record；不再按内容排序、也不把不可达 record 带进比较。
/// 2. record 按该次序重编号（1..=N），对象槽块按同一次序从 1 起连续排布。
/// 3. **只**重写 `ReferenceLayout` 列出的引用位置（槽 payload 与 Map 键 id）；标量 payload、槽的
///    depth/tag、正文、variant tag、长度全部原样——高位污染与悬空 id 已在 `wire_facts` 拒绝，
///    不会被规范化抹平。
#[track_caller]
fn canonical_wire(facts: &WireFacts, what: &str, layout: &ReferenceLayout) -> Vec<u8> {
    let order = &facts.order;
    let new_id: BTreeMap<i32, i32> = order
        .iter()
        .enumerate()
        .map(|(index, &id)| (id, index as i32 + 1))
        .collect();
    let mut new_offset: BTreeMap<i32, usize> = BTreeMap::new();
    let mut next = 1usize;
    for &id in order {
        if let Some(block) = facts.blocks.get(&id) {
            new_offset.insert(id, next);
            next += block.count;
        }
    }
    // 引用位置（真实 record id → tag / Map 键个数）：只有这些位置参与重编号。
    let reference_slots: BTreeSet<(i32, i32)> = layout
        .slots
        .iter()
        .map(|&(index, tag)| (order[index - 1], tag))
        .collect();
    let map_keys: BTreeMap<i32, usize> = layout
        .map_keys
        .iter()
        .map(|&(index, keys)| (order[index - 1], keys))
        .collect();

    let mut main: Vec<u8> = Vec::new();
    for &id in order {
        let new_body: Vec<u8> = match facts.blocks.get(&id) {
            Some(block) => {
                let mut body = Vec::new();
                if let Some(tag) = block.variant_tag {
                    body.extend(tag.to_le_bytes());
                }
                body.extend((new_offset[&id] as u32).to_le_bytes());
                body.extend((block.count as u32).to_le_bytes());
                body
            }
            None => match map_keys.get(&id) {
                Some(&keys) => rewrite_map_keys(facts.body(id), keys, &new_id, what),
                None => facts.body(id).to_vec(),
            },
        };
        main.extend(new_id[&id].to_le_bytes());
        main.extend((new_body.len() as i32).to_le_bytes());
        main.extend(new_body);
    }

    let mut assist: Vec<u8> = Vec::new();
    if let Some(&(depth, tag, payload)) = facts.slots.first() {
        // 引用型根槽换成新编号（FR-W10）；标量根槽原样保留。
        let root_payload = match facts.root {
            Some(root_id) => new_id[&root_id] as u64,
            None => payload,
        };
        push_slot(&mut assist, depth, tag, root_payload);
    }
    for &id in order {
        let Some(block) = facts.blocks.get(&id) else {
            continue;
        };
        for index in block.offset..block.offset + block.count {
            let (depth, tag, payload) = facts.slots[index];
            assert_eq!(
                depth as u32, block.depth,
                "{what}：FR-T07 record {id} 的槽 depth 必须等于其嵌套层 {}",
                block.depth
            );
            let payload = if reference_slots.contains(&(id, tag)) {
                let target = facts
                    .reference(payload)
                    .expect("引用位置已在 wire_facts 校验为存在的 record id");
                new_id[&target] as u64 // FR-W02：引用 id 用 8B 零扩展
            } else {
                payload // 标量 payload 原样保留
            };
            push_slot(&mut assist, depth, tag, payload);
        }
    }

    let mut canonical = Vec::with_capacity(main.len() + assist.len() + 8);
    canonical.extend((main.len() as i32).to_le_bytes());
    canonical.extend(main);
    canonical.extend((assist.len() as i32).to_le_bytes());
    canonical.extend(assist);
    canonical
}

/// 逐字节比较（不跳过任何偏移）：29 行直接硬比的行（21 个 raw 单/零 record + 8 个 packed）使用。
#[track_caller]
fn assert_bytes_identical(case: &str, mode: &str, golden: &[u8], actual: &[u8]) {
    if actual.len() != golden.len() {
        panic!(
            "[rust-golden] case {case} ({mode}) FAIL: 长度不同，金样 {} 字节 / 本库 {} 字节\n  金样前 16 字节: {}\n  本库前 16 字节: {}\n  本库全量: {}",
            golden.len(),
            actual.len(),
            hex_head(golden),
            hex_head(actual),
            hex_all(actual),
        );
    }
    for (offset, &want) in golden.iter().enumerate() {
        let got = actual[offset];
        assert!(
            got == want,
            "[rust-golden] case {case} ({mode}) FAIL: 偏移 0x{offset:04x}（{offset}）金样=0x{want:02x} 本库=0x{got:02x}\n  金样前 16 字节: {}\n  本库前 16 字节: {}\n  本库全量: {}",
            hex_head(golden),
            hex_head(actual),
            hex_all(actual),
        );
    }
}

/// `[rust-golden]` 断言入口：策略由 `GOLDEN_COMPARISONS` 唯一决定（29 行硬比、7 行规范化、1 行 packed 不变量）。
///
/// 所有 **raw** 行都先跑 `wire_facts`：它从盘上字节重算 envelope、record 边界、唯一连续 id、16B 槽、
/// 贪心覆盖、tag、depth，并逐个校验固定夹具的引用位置（高位为零 + 目标存在）。packed 行只比 packed 字节
/// （或 `b.long` 的格式 + 零补齐 + 解包后比 raw），其 raw 行已在同一 case 的 raw 用例里跑过同一套断言。
#[track_caller]
fn assert_rust_golden(case: &str, pack: bool, actual: &[u8]) {
    let golden = rust_golden(case, pack);
    let mode = if pack { "pack=true" } else { "pack=false" };
    let strategy = golden_comparison(case, pack);
    if strategy == GoldenComparison::PackedRunInvariant {
        assert_packed_run_invariant(case, mode, &golden, actual);
        return;
    }
    if pack {
        // packed 字节不是 envelope，不进 `wire_facts`；这些行的 raw 行已在同一 case 的 raw 用例里
        // 过过同一套不变量与引用位置校验。
        assert_bytes_identical(case, mode, &golden, actual);
        return;
    }
    let what_golden = format!("金样 {case} ({mode})");
    let what_actual = format!("本库输出 {case} ({mode})");
    let layout = reference_layout(case);
    let golden_facts = wire_facts(&golden, &what_golden, layout);
    let actual_facts = wire_facts(actual, &what_actual, layout);
    if strategy == GoldenComparison::Bytes {
        assert_bytes_identical(case, mode, &golden, actual);
        return;
    }
    let want = canonical_wire(&golden_facts, &what_golden, layout);
    let got = canonical_wire(&actual_facts, &what_actual, layout);
    if got != want {
        panic!(
            "[rust-golden] case {case} ({mode}) FAIL: 规范化后仍不同（record id 编号与槽块次序已归零，其余字节必须一致）\n  金样规范化 {} 字节: {}\n  本库规范化 {} 字节: {}",
            want.len(),
            hex_all(&want),
            got.len(),
            hex_all(&got),
        );
    }
}

/// 严格 unpack（本测试自带，**不调用被测实现**）：逐 word 还原 packed 流，缺 tag 数据、缺 run 计数、
/// 缺 run body 均直接 FAIL；普通 tag 显式携带的字节必须非零（FR-P02：第 i 位表示该 word 第 i 字节
/// 非零——只查「置位所需字节存在」会放行 `13 8a 00 01` 这种多置一位再显式写零的等价编码）；输出长度
/// 必为 8 的倍数（FR-P03 零补齐）。`ff` 的 8 字节属于原样续接正文，不做非零限制。
#[track_caller]
fn unpack_packed(packed: &[u8], what: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    while pos < packed.len() {
        let tag = packed[pos];
        pos += 1;
        let mut word = [0u8; 8];
        for (index, byte) in word.iter_mut().enumerate() {
            if tag & (1 << index) != 0 {
                let value = *packed
                    .get(pos)
                    .unwrap_or_else(|| panic!("{what}：packed 流缺 tag 对应的数据字节"));
                assert!(
                    tag == 0xff || value != 0,
                    "{what}：packed tag 0x{tag:02x} 的第 {index} 位表示该字节非零（FR-P02），但显式写入了 0x00；偏移 {pos}"
                );
                *byte = value;
                pos += 1;
            }
        }
        out.extend_from_slice(&word);
        if tag == 0 || tag == 0xff {
            let extra = *packed
                .get(pos)
                .unwrap_or_else(|| panic!("{what}：packed 流缺 run 计数")) as usize;
            pos += 1;
            if tag == 0 {
                out.resize(out.len() + 8 * extra, 0);
            } else {
                let end = pos + 8 * extra;
                let run = packed
                    .get(pos..end)
                    .unwrap_or_else(|| panic!("{what}：packed 流的 ff run body 截断"));
                out.extend_from_slice(run);
                pos = end;
            }
        }
    }
    assert_eq!(out.len() % 8, 0, "{what}：解包长度必须是 8B 的倍数");
    out
}

/// `b.long` packed 行：`ff` run 的**延续分段**是 PACK 的实现自由（FR-P02），金样取 14 个后续 word；
/// 把下一个 word（`78×6 10 00`）原样并入 run 改 15 个，packed 字节不同、解包字节完全相同。
/// 因此这一行不硬比 packed 字节，只查：①两边 packed 流都能被严格 parse；②解包并按 FR-P03 零补齐后，
/// 与该行 raw 金样逐字节相同（raw 不自由）。
#[track_caller]
fn assert_packed_run_invariant(case: &str, mode: &str, golden: &[u8], actual: &[u8]) {
    // `b.long` 的 raw 基线是 `[csharp-bytes]` 行的 C# DUMP（不在 `[rust-golden]` 表的 raw 行里）。
    assert_eq!(case, "b.long", "packed run 不变量行目前只有 b.long");
    let raw = decode_hex(&b_long_raw_dump_hex());
    let mut padded = raw.clone();
    padded.resize((raw.len() + 7) / 8 * 8, 0); // FR-P03：不足 8B 用零补齐
    let golden_unpacked = unpack_packed(golden, &format!("金样 {case} ({mode})"));
    assert_eq!(
        golden_unpacked, padded,
        "[rust-golden] case {case} ({mode})：金样 packed 行自身必须解包回该行 raw 金样（FR-P01/P03）"
    );
    let unpacked = unpack_packed(actual, &format!("本库输出 {case} ({mode})"));
    assert_eq!(
        unpacked,
        padded,
        "[rust-golden] case {case} ({mode}) FAIL: 解包并零补齐后必须与该行 raw 金样逐字节相同（packed run 分段自由，raw 不自由）\n  期望 {} 字节: {}\n  实际 {} 字节: {}",
        padded.len(),
        hex_all(&padded),
        unpacked.len(),
        hex_all(&unpacked),
    );
}

/// 把 packed 流里的 `ff` run 多并入紧跟其后的一个 word（FR-P02 把延续选择交给 PACK）：run 计数 +1、
/// run body 追加该 word 的 8 字节，并删掉它原来的普通 tag 与显式字节。要求 `ff` run 后面紧跟一个普通
/// tag 的 word；解包结果与输入逐字节相同（packed 字节不同）。
#[track_caller]
fn extend_ff_run_by_one_word(packed: &[u8], what: &str) -> Vec<u8> {
    let mut pos = 0usize;
    while pos < packed.len() {
        let tag = packed[pos];
        match tag {
            0x00 => {
                let extra = *packed
                    .get(pos + 1)
                    .unwrap_or_else(|| panic!("{what}：packed 流缺零 run 计数"));
                pos += 2 + 8 * extra as usize;
            }
            0xff => {
                let count_at = pos + 9; // tag + 当前 word 的 8 字节
                let extra = *packed
                    .get(count_at)
                    .unwrap_or_else(|| panic!("{what}：packed 流缺 ff run 计数"));
                let run_end = count_at + 1 + 8 * extra as usize;
                let next_tag = *packed
                    .get(run_end)
                    .unwrap_or_else(|| panic!("{what}：`ff` run 已在流末尾，没有可并入的 word"));
                assert!(
                    next_tag != 0x00 && next_tag != 0xff,
                    "{what}：`ff` run 后面必须是普通 tag 的 word，实际 tag 0x{next_tag:02x}"
                );
                let mut word = [0u8; 8];
                let mut at = run_end + 1;
                for (index, byte) in word.iter_mut().enumerate() {
                    if next_tag & (1 << index) != 0 {
                        *byte = *packed
                            .get(at)
                            .unwrap_or_else(|| panic!("{what}：packed 流缺 tag 对应的数据字节"));
                        at += 1;
                    }
                }
                let mut extended =
                    Vec::with_capacity(packed.len() + 8 - next_tag.count_ones() as usize);
                extended.extend_from_slice(&packed[..count_at]);
                extended.push(extra + 1);
                extended.extend_from_slice(&packed[count_at + 1..run_end]);
                extended.extend_from_slice(&word);
                extended.extend_from_slice(&packed[at..]);
                return extended;
            }
            _ => pos += 1 + tag.count_ones() as usize,
        }
    }
    panic!("{what}：packed 流里没有可并入一个 word 的 `ff` run")
}

/// `a.long` pack=false：**[rust-golden]**。requirements §5 #2 标「保留」（i64 值语义保留），但字节布局不同：C# 根标量 8 字节写在 main（16 字节），本库写 16B 根槽到 assist（24 字节）。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：FR-W01/W03：main=0；assist=16B 根槽（depth=0, tag=0, payload=i64 0x0102030405060708）。
#[test]
fn case_a_long_raw_rust_golden() {
    let value = 0x0102_0304_0506_0708_i64;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.long", false, &bytes);
    roundtrip(&value, false);
}

/// `a.int` pack=false：**[rust-golden]**。requirements §5 #1「修正」：统一根槽（C# 根标量 8 字节写在 main）。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：FR-W01 envelope + FR-W03 16B 槽：main=0；assist=16B 根槽（depth=0, tag=0, payload=i64 零扩展的 0x12345678）。
#[test]
fn case_a_int_raw_rust_golden() {
    let value = 0x1234_5678_i32;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.int", false, &bytes);
    roundtrip(&value, false);
}

/// `a.short` pack=false：**[rust-golden]**。requirements §5 #3「修正」：统一根槽 + 数值规范扩展。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：main=0；assist=16B 根槽，payload=i64 零扩展的 i16 0x1234。
#[test]
fn case_a_short_raw_rust_golden() {
    let value = 0x1234_i16;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.short", false, &bytes);
    roundtrip(&value, false);
}

/// `a.byte` pack=false：**[rust-golden]**。requirements §5 #4「修正」：统一根槽 + 数值规范扩展。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：main=0；assist=16B 根槽，payload=u64 零扩展的 0xab。
#[test]
fn case_a_byte_raw_rust_golden() {
    let value = 0xab_u8;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.byte", false, &bytes);
    roundtrip(&value, false);
}

/// `a.bool.true` pack=false：**[rust-golden]**。requirements §5 #5「修正」：统一根槽 + 布尔按 1 扩展。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：main=0；assist=16B 根槽，payload=u64 的 1。
#[test]
fn case_a_bool_true_raw_rust_golden() {
    let value = true;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.bool.true", false, &bytes);
    roundtrip(&value, false);
}

/// `a.bool.false` pack=false：**[rust-golden]**。requirements §5 #6「修正」：false 不丢根值（统一根槽）。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：main=0；assist=16B 根槽，payload 全零。
#[test]
fn case_a_bool_false_raw_rust_golden() {
    let value = false;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.bool.false", false, &bytes);
    roundtrip(&value, false);
}

/// `a.float` pack=false：**[rust-golden]**。requirements §5 #7「修正」：统一根槽 + 位模式零扩展。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：main=0；assist=16B 根槽，payload=i64 零扩展的 f32 位模式 0x3fc00000。
#[test]
fn case_a_float_raw_rust_golden() {
    let value = 1.5_f32;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.float", false, &bytes);
    roundtrip(&value, false);
}

/// `a.double` pack=false：**[rust-golden]**。requirements §5 #8 标「保留」（f64 位模式语义保留），但字节布局不同：C# 根标量 8 字节写在 main（16 字节），本库写 16B 根槽到 assist（24 字节）。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：FR-W01/W03：main=0；assist=16B 根槽，payload=f64 -2.25 的位模式 0xc002000000000000（小端）。
#[test]
fn case_a_double_raw_rust_golden() {
    let value = -2.25_f64;
    let bytes = encoded(&value, false);
    assert_rust_golden("a.double", false, &bytes);
    roundtrip(&value, false);
}

/// `a.char` pack=false：**[rust-golden]**。requirements §5 #9「修正」：按 Unicode 标量 u32 零扩展。
/// C# 地真 16 字节，本库 24 字节。
/// 金样结构：main=0；assist=16B 根槽，payload=i64 零扩展的 U+4E2D。
#[test]
fn case_a_char_raw_rust_golden() {
    let value = '\u{4E2D}';
    let bytes = encoded(&value, false);
    assert_rust_golden("a.char", false, &bytes);
    roundtrip(&value, false);
}

/// `b.empty` raw：**[csharp-bytes]**（D §5 :449–497；requirements §5 #10「保留」）：
/// C# 的 id=1,size=0 引用记录与 16B assist 根槽与本库布局重合。
#[test]
fn case_b_empty_raw_csharp_bytes() {
    let value = String::new();
    let bytes = encoded(&value, false);
    assert_csharp_bytes(
        "b.empty",
        false,
        "0800000001000000000000001000000000000000000000000100000000000000",
        &[],
        &bytes,
    );
    roundtrip(&value, false);
}

/// `b.null` raw：**[csharp-bytes]**（D §5 :498–521；requirements §5 #11「保留」）：
/// null 串即无根槽的 8 零字节。
#[test]
fn case_b_null_raw_csharp_bytes() {
    let value = None::<String>;
    let bytes = encoded(&value, false);
    assert_csharp_bytes("b.null", false, "0000000000000000", &[], &bytes);
    roundtrip(&value, false);
}

/// `b.chinese` raw：**[csharp-bytes]**（D §5 :522–581；requirements §5 #12「保留」）：
/// UTF-8 10 字节原样写入，size=10。
#[test]
fn case_b_chinese_raw_csharp_bytes() {
    let value = String::from("中文🙂");
    let bytes = encoded(&value, false);
    assert_csharp_bytes(
        "b.chinese",
        false,
        "12000000010000000a000000e4b8ade69687f09f99821000000000000000000000000100000000000000",
        &[],
        &bytes,
    );
    roundtrip(&value, false);
}

/// `b.long` raw：**[csharp-bytes]**（D §5 :582–769；requirements §5 #13「保留」）：
/// main=138（`8a 00 00 00`），字符串体长度 130（`82 00 00 00`），130 个 ASCII x。
#[test]
fn case_b_long_raw_csharp_bytes() {
    let value = "x".repeat(130);
    let bytes = encoded(&value, false);
    assert_csharp_bytes("b.long", false, &b_long_raw_dump_hex(), &[], &bytes);
    roundtrip(&value, false);
}

/// `c.struct` pack=false：**[rust-golden]**。requirements §5 #14「修正」：各槽独立、数值规范扩展；C# 两处 8 字节指针越界读（0x14–0x17、0x24–0x27）在本库不存在。
/// C# 地真 40 字节，本库 72 字节。
/// 金样结构：FR-W02/W03/W06：main=record(id=1, body=[slot_offset=1, slot_count=2])；assist=根槽(0,0,id=1)+(1,0,X 的 i64 零扩展)+(1,1,Y 的 i64 符号扩展)。
#[test]
fn case_c_struct_raw_rust_golden() {
    let value = TwoInts {
        x: 0x1234_5678,
        y: -2,
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("c.struct", false, &bytes);
    roundtrip(&value, false);
}

/// `d.class` pack=false：**[rust-golden]**。requirements §5 #15 标「保留」（Value=42 字段语义保留），但字节布局不同：C# 无根对象 record、字段值 4 字节。
/// C# 地真 24 字节，本库 56 字节。
/// 金样结构：FR-W02/W03/W06：main=record(id=1, body=[1,1])；assist=根槽(0,0,id=1)+(1,0,42 的 i64)。
#[test]
fn case_d_class_raw_rust_golden() {
    let value = OneInt { value: 42 };
    let bytes = encoded(&value, false);
    assert_rust_golden("d.class", false, &bytes);
    roundtrip(&value, false);
}

/// `d.default` pack=false：**[rust-golden]**。requirements §5 #16 标「保留」（Value=0 字段缺席语义保留），但字节布局不同：C# 为 8 零字节，本库空根对象仍写 record + 根槽。
/// C# 地真 8 字节，本库 40 字节。
/// 金样结构：FR-W02/W04/W06：main=record(id=1, body=[1,0])（槽数 0）；assist=根槽(0,0,id=1)。
#[test]
fn case_d_default_raw_rust_golden() {
    let value = OneInt { value: 0 };
    let bytes = encoded(&value, false);
    assert_rust_golden("d.default", false, &bytes);
    roundtrip(&value, false);
}

/// `e.nested` pack=false：**[rust-golden]**。requirements §5 #17 标「保留」（A→B→C 与 C.Value=99 语义保留），但字节布局不同：C# 主体写 `LevelB`/`LevelC` 运行时类型名记录，本库写对象槽范围。
/// C# 地真 170 字节，本库 120 字节。
/// 金样结构：FR-W06 后序分配：record1=LevelC body=[1,1]、record2=LevelB body=[2,1]、record3=LevelA body=[3,1]；assist=根槽(0,0,3)+(3,0,99)+(2,0,1)+(1,0,2)，槽范围 [1,2)/[2,3)/[3,4) 递增不重叠、根槽 0 不归属任何对象。
/// 分层：record id 的编号次序（本批为后序）与槽块出现次序是**实现选择，不是规范规定**
/// （FR-W02 只要求正整数唯一、FR-W06 只要求范围单调递增不重叠）→ 本行按不变量断言 + 规范化后逐字节。
#[test]
fn case_e_nested_raw_rust_golden() {
    let value = LevelA {
        next: LevelB {
            next: LevelC { value: 99 },
        },
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("e.nested", false, &bytes);
    roundtrip(&value, false);
}

/// `f.array0` pack=false：**[rust-golden]**。requirements §5 #18「修正」：空集合统一为记录（C# 顶层 int[] 只在 main 写 4 字节 count、无 record）。
/// C# 地真 12 字节，本库 36 字节。
/// 金样结构：FR-W09：main=record(id=1, body=u32 count=0)；assist=根槽(0,0,id=1)。
#[test]
fn case_f_array0_raw_rust_golden() {
    let value: Vec<i32> = Vec::new();
    let bytes = encoded(&value, false);
    assert_rust_golden("f.array0", false, &bytes);
    roundtrip(&value, false);
}

/// `f.array3` pack=false：**[rust-golden]**。requirements §5 #19「修正」：集合统一记录 + u32 count（C# 顶层 int[] 是 count 直写 main）。
/// C# 地真 24 字节，本库 48 字节。
/// 金样结构：FR-W09：main=record(id=1, body=u32 count=3 + i32 自然宽度 1,0,-2)；assist=根槽(0,0,id=1)。
#[test]
fn case_f_array3_raw_rust_golden() {
    let value: Vec<i32> = vec![1, 0, -2];
    let bytes = encoded(&value, false);
    assert_rust_golden("f.array3", false, &bytes);
    roundtrip(&value, false);
}

/// `f.array200` raw：**[rust-golden]**。requirements §5 #20「修正」：集合统一记录 + u32 count。
/// C# 地真 812 字节，本库 836 字节。
/// 金样结构（`tools/gen_golden.py` 的 `f.array200`）：main=record(id=1, body=u32 count=200 +
/// 200×i32 自然宽度，FR-W02/W09）；assist=16B 根槽(0,0,id=1)（FR-W01/W10）。
#[test]
fn case_f_array200_raw_rust_golden() {
    let value: Vec<i32> = (0..200).collect();
    let bytes = encoded(&value, false);
    assert_rust_golden("f.array200", false, &bytes);
    roundtrip(&value, false);
}

/// `f.list0` pack=false：**[rust-golden]**。requirements §5 #21「修正」：u32 count（C# 为 u16 count + 外层引用记录）。Rust 统一 `Vec`，与 `f.array0` 同形。
/// C# 地真 34 字节，本库 36 字节。
/// 金样结构：FR-W09：main=record(id=1, body=u32 count=0)；assist=根槽(0,0,id=1)。
#[test]
fn case_f_list0_raw_rust_golden() {
    let value: Vec<i32> = Vec::new();
    let bytes = encoded(&value, false);
    assert_rust_golden("f.list0", false, &bytes);
    roundtrip(&value, false);
}

/// `f.list3` pack=false：**[rust-golden]**。requirements §5 #22「修正」：u32 count 且零元素也占位（C# 只给非零元素建 referent）。Rust 统一 `Vec`，与 `f.array3` 同形。
/// C# 地真 90 字节，本库 48 字节。
/// 金样结构：FR-W09：main=record(id=1, body=u32 count=3 + i32 1,0,-2)；assist=根槽(0,0,id=1)。
#[test]
fn case_f_list3_raw_rust_golden() {
    let value: Vec<i32> = vec![1, 0, -2];
    let bytes = encoded(&value, false);
    assert_rust_golden("f.list3", false, &bytes);
    roundtrip(&value, false);
}

/// `f.dict0` pack=false：**[rust-golden]**。requirements §5 #23「修正」：u32 count（C# 为 u16 count + 外层引用记录）。与空集合同形。
/// C# 地真 34 字节，本库 36 字节。
/// 金样结构：FR-W09：main=record(id=1, body=u32 count=0)；assist=根槽(0,0,id=1)。
#[test]
fn case_f_dict0_raw_rust_golden() {
    let value: std::collections::BTreeMap<String, i32> = std::collections::BTreeMap::new();
    let bytes = encoded(&value, false);
    assert_rust_golden("f.dict0", false, &bytes);
    roundtrip(&value, false);
}

/// `f.dict3` pack=false：**[rust-golden]**。requirements §5 #24「修正」：按 key 的 Ord（`a` < `c` < `中`）而非 C# 插入序，无类型名记录、u32 count。
/// C# 地真 260 字节，本库 89 字节。
/// 金样结构：FR-W09：main=record(1,"a")+record(2,"c")+record(3,"中")+record(4, body=u32 count=3 + (u32 key record id, i32 value)×3)；assist=根槽(0,0,4)。
/// 分层：record id 的编号次序（本批为后序）与槽块出现次序是**实现选择，不是规范规定**
/// （FR-W02 只要求正整数唯一、FR-W06 只要求范围单调递增不重叠）→ 本行按不变量断言 + 规范化后逐字节。
#[test]
fn case_f_dict3_raw_rust_golden() {
    let value: std::collections::BTreeMap<String, i32> = std::collections::BTreeMap::from([
        (String::from("a"), 1),
        (String::from("中"), 0),
        (String::from("c"), -2),
    ]);
    let bytes = encoded(&value, false);
    assert_rust_golden("f.dict3", false, &bytes);
    roundtrip(&value, false);
}

/// `g.shared` raw：**[rust-golden]**。requirements §5 #25 标「保留」（同一实例共享身份），但字节布局不同：
/// C# 主体写 `WireDump.OneInt, WireDump` 类型名记录。
/// C# 地真 113 字节，本库 104 字节。
/// 金样结构（FR-W02/W06 + 身份表）：main=record(1,[1,1])(OneInt)+record(2,[2,2])(Holder)；
/// assist=根槽(0,0,2)+(2,0,7)+(1,0,1)+(1,1,1)（Left/Right 同 id=1）。
/// 分层：record id 的编号次序（本批为后序）与槽块出现次序是**实现选择，不是规范规定**
/// （FR-W02 只要求正整数唯一、FR-W06 只要求范围单调递增不重叠）→ 本行按不变量断言 + 规范化后逐字节。
#[test]
fn case_g_shared_raw_rust_golden() {
    let shared = Rc::new(OneInt { value: 7 });
    let value = SharedHolder {
        left: Rc::clone(&shared),
        right: Rc::clone(&shared),
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("g.shared", false, &bytes);
    let decoded = roundtrip(&value, false);
    assert!(
        Rc::ptr_eq(&decoded.left, &decoded.right),
        "往返后 Left/Right 必须仍是同一实例（同 id → ptr_eq）"
    );
}

/// `g.equal` raw：**[rust-golden]**。requirements §5 #26「修正」：两个不同但相等的实例不合并身份
/// （C# 按 `GetHashCode` 合并，两 case 完整字节相同）。
/// C# 地真 113 字节，本库 136 字节。
/// 金样结构（FR-W02/W06 + 身份表按指针）：main=record(1,[1,1])+record(2,[2,1])+record(3,[3,2])；
/// assist=根槽(0,0,3)+(2,0,7)+(2,0,7)+(1,0,1)+(1,1,2)（Left/Right 不同 id）。
/// 分层：record id 的编号次序（本批为后序）与槽块出现次序是**实现选择，不是规范规定**
/// （FR-W02 只要求正整数唯一、FR-W06 只要求范围单调递增不重叠）→ 本行按不变量断言 + 规范化后逐字节。
#[test]
fn case_g_equal_raw_rust_golden() {
    let value = SharedHolder {
        left: Rc::new(OneInt { value: 7 }),
        right: Rc::new(OneInt { value: 7 }),
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("g.equal", false, &bytes);
    let decoded = roundtrip(&value, false);
    assert!(
        !Rc::ptr_eq(&decoded.left, &decoded.right),
        "两个不同且相等的实例不得被合并身份（异 id → 不 ptr_eq）"
    );
}

/// `h.cycle` raw：**[rust-roundtrip]**（`UNABLE-C#-ref → rust-roundtrip`，不是「通过 C# 字节对照」）。
/// D §5 :2819–2848 只有 exit=-6（SIGABRT）与 `GetHashCode` 栈溢出的说明，C# 没有返回 byte[]，
/// 没有逐字节对照对象。requirements §5 #27 把该行的 Rust 口径记为「修正」：成功对象是 Weak 模型
/// （回边指根）。本用例按 Rust 新规范做编码 → 解码 → 身份拓扑断言；该行附带的「强环 →
/// `14 CycleUnsupported`」错误回归已在既有 `tests/encode_into_scratch.rs::strong_cycle_returns_cycle_unsupported`
/// 覆盖，不在此重复。
#[test]
fn case_h_cycle_raw_rust_roundtrip() {
    let root: Rc<CycleA> = Rc::new_cyclic(|me| CycleA {
        next: Rc::new(CycleB {
            next: Weak::clone(me),
        }),
    });
    let bytes = encoded(&root, false);
    assert!(
        !bytes.is_empty(),
        "[rust-roundtrip] h.cycle: Weak 回边模型必须产出非空消息（C# 侧该输入 SIGABRT，属有意差异）"
    );
    let back = root
        .next
        .next
        .upgrade()
        .expect("编码前 Weak 回边目标必须存活");
    assert!(Rc::ptr_eq(&back, &root), "编码前 Weak 回边必须指回根");

    let mut decoder = Decoder::new(&bytes, &raw_options()).unwrap();
    let decoded: Rc<CycleA> = decode_cyclic(&mut decoder).unwrap();
    decoder.finish().unwrap();
    let back = decoded
        .next
        .next
        .upgrade()
        .expect("解码后 Weak 回边目标必须存活");
    assert!(
        Rc::ptr_eq(&back, &decoded),
        "往返后 Weak::upgrade() 必须回到同一根（回边指根、身份正确）"
    );
}

/// `i.nullable.value` pack=false：**[rust-golden]**。requirements §5 #28「修正」：Some(123) 完整恢复（C# 只写 4 字节 payload、assist 长度 12，反序列化变 null）。
/// C# 地真 20 字节，本库 24 字节。
/// 金样结构：FR-W04：Some 必写槽；main=0；assist=16B 根槽，payload=i64 的 123。
#[test]
fn case_i_nullable_value_raw_rust_golden() {
    let value = Some(123_i32);
    let bytes = encoded(&value, false);
    assert_rust_golden("i.nullable.value", false, &bytes);
    roundtrip(&value, false);
}

/// `i.nullable.zero` pack=false：**[rust-golden]**。requirements §5 #29「修正」：Some(0) 有槽、不等于 None（C# 20 字节且反序列化变 null）。
/// C# 地真 20 字节，本库 24 字节。
/// 金样结构：FR-W04：Some(0) 必写槽；main=0；assist=16B 根槽，payload 全零。
#[test]
fn case_i_nullable_zero_raw_rust_golden() {
    let value = Some(0_i32);
    let bytes = encoded(&value, false);
    assert_rust_golden("i.nullable.zero", false, &bytes);
    roundtrip(&value, false);
}

/// `i.nullable.none` raw：**[csharp-bytes]**（D §5 :2937–2960；requirements §5 #30「保留」）：
/// None 无槽，8 零字节。
#[test]
fn case_i_nullable_none_raw_csharp_bytes() {
    let value = None::<i32>;
    let bytes = encoded(&value, false);
    assert_csharp_bytes("i.nullable.none", false, "0000000000000000", &[], &bytes);
    roundtrip(&value, false);
}

/// `i.null.fields` pack=false：**[rust-golden]**。requirements §5 #31 标「保留」（四个字段全 null 语义保留），但字节布局不同：C# 为 8 零字节，本库空根对象仍写 record + 根槽。
/// C# 地真 8 字节，本库 40 字节。
/// 金样结构：FR-W02/W04/W06：main=record(id=1, body=[1,0])；assist=根槽(0,0,id=1)。
#[test]
fn case_i_null_fields_raw_rust_golden() {
    let value = Complex {
        maybe: None,
        items: None,
        child: None,
        text: None,
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("i.null.fields", false, &bytes);
    roundtrip(&value, false);
}

/// `i.complex` pack=false：**[rust-golden]**。requirements §5 #32「修正」：C# 218 字节实际解码丢 3 个引用字段；本库按槽范围完整编码。
/// C# 地真 218 字节，本库 167 字节。
/// 金样结构：FR-W06/W09：main=record(1, body=u32 count=2 + [2,3])+record(2,[1,1])(OneInt)+record(3,"中")+record(4,[2,4])；assist=根槽(0,0,4)+(2,0,8)+(1,0,5)+(1,1,1)+(1,2,2)+(1,3,3)。
/// 分层：record id 的编号次序（本批为后序）与槽块出现次序是**实现选择，不是规范规定**
/// （FR-W02 只要求正整数唯一、FR-W06 只要求范围单调递增不重叠）→ 本行按不变量断言 + 规范化后逐字节。
#[test]
fn case_i_complex_raw_rust_golden() {
    let value = Complex {
        maybe: Some(5),
        items: Some(vec![2, 3]),
        child: Some(OneInt { value: 8 }),
        text: Some(String::from("中")),
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("i.complex", false, &bytes);
    roundtrip(&value, false);
}

/// `j.interface` pack=false：**[rust-golden]**。requirements §5 #33「修正」：数值 tag enum（C# 写 `WireDump.Cat, WireDump` 运行时类型名记录，u32 头）。
/// C# 地真 94 字节，本库 90 字节。
/// 金样结构：FR-W07：main=record(1, body=u16 tag=0 + u32 slot_offset=1 + u32 slot_count=1)+record(2,[2,1])；assist=根槽(0,0,2)+(2,0,9)+(1,0,1)。
/// 分层：record id 的编号次序（本批为后序）与槽块出现次序是**实现选择，不是规范规定**
/// （FR-W02 只要求正整数唯一、FR-W06 只要求范围单调递增不重叠）→ 本行按不变量断言 + 规范化后逐字节。
#[test]
fn case_j_interface_raw_rust_golden() {
    let value = AnimalHolder {
        pet: Animal::Cat { lives: 9 },
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("j.interface", false, &bytes);
    roundtrip(&value, false);
}

/// `j.abstract` pack=false：**[rust-golden]**。requirements §5 #34「修正」：数值 tag enum（C# 写 `WireDump.Dog, WireDump` 类型名记录，且需 IEquatable fixture）。
/// C# 地真 94 字节，本库 90 字节。
/// 金样结构：FR-W07：main=record(1, body=u16 tag=1 + u32 slot_offset=1 + u32 slot_count=1)+record(2,[2,1])；assist=根槽(0,0,2)+(2,0,4)+(1,0,1)。
/// 分层：record id 的编号次序（本批为后序）与槽块出现次序是**实现选择，不是规范规定**
/// （FR-W02 只要求正整数唯一、FR-W06 只要求范围单调递增不重叠）→ 本行按不变量断言 + 规范化后逐字节。
#[test]
fn case_j_abstract_raw_rust_golden() {
    let value = AnimalHolder {
        pet: Animal::Dog { age: 4 },
    };
    let bytes = encoded(&value, false);
    assert_rust_golden("j.abstract", false, &bytes);
    roundtrip(&value, false);
}

/// `k.array.null` raw：**[rust-roundtrip]**（`UNABLE-C#-ref → rust-roundtrip`）。D §5 :3469–3483 的
/// C# 运行在 `ArrayFormatter.Serialize` 抛 `NullReferenceException`，没有 byte[]；requirements §5 #35
/// 记为「修正」：`Vec<Option<String>>` 允许 None 元素。本用例只做 Rust 往返断言。
#[test]
fn case_k_array_null_raw_rust_roundtrip() {
    let value = vec![Some(String::from("a")), None, Some(String::from("中"))];
    roundtrip(&value, false);
}

/// `k.list.null` raw：**[rust-roundtrip]**（`UNABLE-C#-ref → rust-roundtrip`）。D §5 :3484–3498 的
/// C# 运行在 `ListFormatter.Serialize` 抛 `NullReferenceException`，没有 byte[]；requirements §5 #36
/// 记为「修正」，等价 Rust 语义同 `k.array.null`。
#[test]
fn case_k_list_null_raw_rust_roundtrip() {
    let value = vec![Some(String::from("a")), None, Some(String::from("中"))];
    roundtrip(&value, false);
}

/// `k.class.array.null` raw：**[rust-roundtrip]**（`UNABLE-C#-ref → rust-roundtrip`）。D §5 :3499–3513 的
/// C# 运行在 `ArrayFormatter.Serialize` 抛 `NullReferenceException`，没有 byte[]；requirements §5 #37
/// 记为「修正」：`Vec<Option<OneInt>>`。
#[test]
fn case_k_class_array_null_raw_rust_roundtrip() {
    let value = vec![Some(OneInt { value: 1 }), None, Some(OneInt { value: 2 })];
    roundtrip(&value, false);
}

/// `a.int` pack=true：**[rust-golden]**。requirements §5 #38「修正」：标准 packed（C# 自定义 bitmap/raw-run，且其流内混入越界读字节 `80 d9 40`）。
/// C# 地真 10 字节，本库 9 字节。
/// 金样结构：FR-P01：raw 24B = 3 word：[00 00 00 00 10 00 00 00][00×8][78 56 34 12 00 00 00 00]；w0 只 bit4 → `10|10`，w1 是唯一零 word → `00|00`（count=0 表示**零个额外零 word**），w2 位图 `0f`。
#[test]
fn case_a_int_packed_rust_golden() {
    let value = 0x1234_5678_i32;
    let bytes = encoded(&value, true);
    assert_rust_golden("a.int", true, &bytes);
    roundtrip(&value, true);
}

/// `a.long` pack=true：**[rust-golden]**。requirements §5 #39 标「保留」，但 packed 编码方式不同：C# 也是逐 word 位图（`f1`/`0f`），区别在零 word 的合并规则——C# 每个 word 各带一个位图字节、没有 `00|count` 续接 run。
/// C# 地真 11 字节，本库 14 字节。
/// 金样结构：FR-P01：raw 24B；w0 `10|10`、零 word `00|00`、w2 全非零 → `ff|<8B>|count=0`。
#[test]
fn case_a_long_packed_rust_golden() {
    let value = 0x0102_0304_0506_0708_i64;
    let bytes = encoded(&value, true);
    assert_rust_golden("a.long", true, &bytes);
    roundtrip(&value, true);
}

/// `b.chinese` pack=true：**[rust-golden]**。requirements §5 #40 标「保留」（解包后有尾块零补齐），但 C# 也是逐 word 位图（`11`/`f1`/`7f`）、区别在零 word 的合并规则：C# 无 `00|count` 续接 run，本库按标准 packed 写。
/// C# 地真 21 字节，本库 23 字节。
/// 金样结构：FR-P01：raw 42B 补零到 48B 共 6 word → `11|12 01`、`f1|0a e4 b8 ad e6`、`7f|96 87 f0 9f 99 82 10`、零 word `00|00`、`04|01`、零 word `00|00`。
#[test]
fn case_b_chinese_packed_rust_golden() {
    let value = String::from("中文🙂");
    let bytes = encoded(&value, true);
    assert_rust_golden("b.chinese", true, &bytes);
    roundtrip(&value, true);
}

/// `b.long` packed：**[rust-golden]**。requirements §5 #41 标「保留」，但 C# 用 `ff 0f` 自定义 raw-run
/// （count 在数据前），本库用 Cap'n Proto 标准 packed（count 在数据后）。
/// C# 地真 143 字节，本库 145 字节。
/// 分层：`ff` run 的延续分段是 PACK 的实现自由（FR-P02）：金样在后续 14 个全非零 word 处截断，
/// 把下一个 word（`78×6 10 00`）原样并入改成 15 个，packed 字节不同、解包字节不变。因此本行
/// **不硬比 packed 字节**，只查 packed 格式 + FR-P03 零补齐 + 解包后与该行 raw 金样逐字节相同
/// （该 raw 行本身由 `case_b_long_raw_csharp_bytes` 与 C# DUMP 硬比）。
/// 金样结构（`tools/gen_golden.py` 的 `b.long` packed）：raw 162B 补零到 168B 共 21 word，
/// 按 FR-P01 标准 packed（P02/P03）打包。
#[test]
fn case_b_long_packed_rust_golden() {
    let value = "x".repeat(130);
    let bytes = encoded(&value, true);
    assert_rust_golden("b.long", true, &bytes);
    roundtrip(&value, true);

    // 合法的 15-word 变体（把 `ff` run 后的下一个 word 原样并入 run）：packed 字节不同、解包字节相同，
    // 必须**经过实际断言入口**被接受，而不只是在 helper 上演示。
    let golden = rust_golden("b.long", true);
    let merged = extend_ff_run_by_one_word(&golden, "b.long packed 金样");
    assert_ne!(
        merged, golden,
        "15-word 变体必须与金样 packed 字节不同，否则本变体不成立"
    );
    assert!(
        packed_run_freedom(&golden) && packed_run_freedom(&merged),
        "b.long 的 `ff` run 有延续自由度：按解包后的 word 判定的结果必须对金样与 15-word 变体都成立"
    );
    assert_eq!(
        unpack_packed(&merged, "15-word 变体"),
        unpack_packed(&golden, "b.long packed 金样"),
        "15-word 变体解包后必须与金样逐字节相同"
    );
    assert_rust_golden("b.long", true, &merged);
}

/// `c.struct` pack=true：**[rust-golden]**。requirements §5 #42「修正」：确定 packed（C# 流内混入越界读字节 `fe ff ff ff` 与 `80 d9`）。
/// C# 地真 22 字节，本库 33 字节。
/// 金样结构：FR-P01：raw 72B = 9 word；w0 `11|10 01`、w1 `11|08 01`、w2 `11|02 30`、w3 零 word `00|00`、w4 `01|01`、w5 `01|01`、w6 `0f|78 56 34 12`、w7 `11|01 01`、w8 全非零 `ff|<8B>|count=0`。
#[test]
fn case_c_struct_packed_rust_golden() {
    let value = TwoInts {
        x: 0x1234_5678,
        y: -2,
    };
    let bytes = encoded(&value, true);
    assert_rust_golden("c.struct", true, &bytes);
    roundtrip(&value, true);
}

/// `d.class` pack=true：**[rust-golden]**。requirements §5 #43 标「保留」，但 packed 编码方式不同（C# `10 10 00 01 2a` 亦为逐 word 位图，但无 `00|count` 续接 run）。
/// C# 地真 5 字节，本库 17 字节。
/// 金样结构：FR-P01：raw 56B = 7 word；w0 `11|10 01`、w1 `11|08 01`、w2 `11|01 20`、w3 零 word `00|00`、w4 `01|01`、w5 `01|01`、w6 `01|2a`。
#[test]
fn case_d_class_packed_rust_golden() {
    let value = OneInt { value: 42 };
    let bytes = encoded(&value, true);
    assert_rust_golden("d.class", true, &bytes);
    roundtrip(&value, true);
}

/// `d.default` pack=true：**[rust-golden]**。requirements §5 #44 标「保留」（该表另记「不得硬匹配 C# 单 00」）；C# 用单个 `00`，本库标准 packed 写 `00|00`。
/// C# 地真 1 字节，本库 12 字节。
/// 金样结构：FR-P01：raw 40B = 5 word；w0 `11|10 01`、w1 `11|08 01`、w2 `10|10`、w3 零 word `00|00`、w4 `01|01`。
#[test]
fn case_d_default_packed_rust_golden() {
    let value = OneInt { value: 0 };
    let bytes = encoded(&value, true);
    assert_rust_golden("d.default", true, &bytes);
    roundtrip(&value, true);
}

/// `f.array3` pack=true：**[rust-golden]**。requirements §5 #45「修正」：集合记录 + u32 count 后的标准 packed（C# 顶层 int[] 无 record）。
/// C# 地真 10 字节，本库 18 字节。
/// 金样结构：FR-P01：raw 48B = 6 word；w0 `11|18 01`、w1 `11|10 03`、w2 `01|01`、w3 `1f|fe ff ff ff 10`、w4 零 word `00|00`、w5 `01|01`。
#[test]
fn case_f_array3_packed_rust_golden() {
    let value: Vec<i32> = vec![1, 0, -2];
    let bytes = encoded(&value, true);
    assert_rust_golden("f.array3", true, &bytes);
    roundtrip(&value, true);
}

/// `i.nullable.value` pack=true：**[rust-golden]**。requirements §5 #46「修正」：Some(123) 走 8B 槽（C# packed 仍按 4B payload 且反序列化变 null）。
/// C# 地真 5 字节，本库 6 字节。
/// 金样结构：FR-P01：raw 24B = 3 word；w0 `10|10`、w1 零 word `00|00`、w2 `01|7b`。
#[test]
fn case_i_nullable_value_packed_rust_golden() {
    let value = Some(123_i32);
    let bytes = encoded(&value, true);
    assert_rust_golden("i.nullable.value", true, &bytes);
    roundtrip(&value, true);
}

// ---------------------------------------------------------------------------
// 金样独立性自检：分层策略元断言、替代布局反例（走实际断言入口）、五条负控（两条引用 + 一条高位污染
// + 一条深度 + 一条 packed 位图显式零字节）
// ---------------------------------------------------------------------------

/// 改写第 `index` 个 16B 槽的 depth/payload（负控用：构造坏字节）。
fn tamper_slot(bytes: &mut [u8], index: usize, depth: Option<i32>, payload: Option<u64>) {
    let main_len = le_i32(bytes, 0) as usize;
    let at = 8 + main_len + index * 16;
    if let Some(depth) = depth {
        bytes[at..at + 4].copy_from_slice(&depth.to_le_bytes());
    }
    if let Some(payload) = payload {
        bytes[at + 8..at + 16].copy_from_slice(&payload.to_le_bytes());
    }
}

/// 金样分层元断言（由旧的 `rust_golden_layout_variant_rows_are_the_multi_record_rows` 更新）：
/// 核对**同一张策略表**（`GOLDEN_COMPARISONS`）与 `.hex` 行清单、与盘上字节推出的自由度三方一致。
///
/// 1. 策略表与 `.hex` 的 `(case, 模式)` 行一一对应：共 37 行（28 raw + 9 packed），无缺项、无多余。
/// 2. 口径为 **29 行直接硬比、8 行非硬比**（8 = 7 个多 record raw 行 + packed `b.long`）。
/// 3. 分层与字节推出的自由度一致：raw 行里 record 数 >1 的行必须 `LayoutNormalized`、≤1 的必须
///    `Bytes`；packed 行的自由度按**解包后的 word 序列**判定（`packed_run_freedom`），有自由度的（当前
///    只有 `b.long`）必须 `PackedRunInvariant`，其余必须 `Bytes`。
#[test]
fn rust_golden_rows_follow_the_declared_comparison() {
    let mut raw_rows = 0usize;
    let mut packed_rows = 0usize;
    let mut multi_record: Vec<&str> = Vec::new();
    let mut run_freedom: Vec<&str> = Vec::new();
    for line in RUST_GOLDEN_TABLE.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        let (case, mode) = (fields[0], fields[1]);
        let golden = decode_hex(fields[4]);
        if mode == "raw" {
            raw_rows += 1;
            let facts = wire_facts(&golden, case, reference_layout(case));
            let expected = if facts.records.len() > 1 {
                multi_record.push(case);
                GoldenComparison::LayoutNormalized
            } else {
                GoldenComparison::Bytes
            };
            assert_eq!(
                golden_comparison(case, false),
                expected,
                "raw 行 {case} 的比较策略必须与 record 数推出的自由度一致"
            );
        } else {
            packed_rows += 1;
            let expected = if packed_run_freedom(&golden) {
                run_freedom.push(case);
                GoldenComparison::PackedRunInvariant
            } else {
                GoldenComparison::Bytes
            };
            assert_eq!(
                golden_comparison(case, true),
                expected,
                "packed 行 {case} 的比较策略必须与 run 分段自由度一致"
            );
        }
    }
    assert_eq!(raw_rows, 28, "金样表应含 28 个 raw 行");
    assert_eq!(packed_rows, 9, "金样表应含 9 个 packed 行");
    assert_eq!(
        multi_record,
        vec![
            "e.nested",
            "f.dict3",
            "g.shared",
            "g.equal",
            "i.complex",
            "j.interface",
            "j.abstract",
        ],
        "含多条 record 的 raw 行必须与分层清单一致（这些行不锁定 record id 编号与槽块次序）"
    );
    assert_eq!(
        run_freedom,
        vec!["b.long"],
        "只有解包后仍有 run 分段自由度的 packed 行才不能硬比（当前只有 b.long）"
    );
    let hard = GOLDEN_COMPARISONS
        .iter()
        .filter(|(_, _, comparison)| *comparison == GoldenComparison::Bytes)
        .count();
    let normalized = GOLDEN_COMPARISONS
        .iter()
        .filter(|(_, _, comparison)| *comparison == GoldenComparison::LayoutNormalized)
        .count();
    let run_invariant = GOLDEN_COMPARISONS
        .iter()
        .filter(|(_, _, comparison)| *comparison == GoldenComparison::PackedRunInvariant)
        .count();
    assert_eq!(
        (hard, normalized, run_invariant),
        (29, 7, 1),
        "分层口径必须是 29 行直接硬比 + 7 行规范化 + 1 行 packed run 不变量"
    );
    assert_eq!(
        GOLDEN_COMPARISONS.len(),
        raw_rows + packed_rows,
        "策略表必须与 .hex 行数一致"
    );
    // 引用位置表只列有 record 内引用的行，且必须正好是那 7 个多 record 行。
    let mut referenced: Vec<&str> = GOLDEN_REFERENCES.iter().map(|(case, _)| *case).collect();
    referenced.sort_unstable();
    let mut expected = multi_record.clone();
    expected.sort_unstable();
    assert_eq!(
        referenced, expected,
        "引用位置表必须覆盖全部多 record 行（否则引用高位/目标存在性会漏查）"
    );
}

/// packed 行是否有 run 分段自由度。判据必须落在**解包后的 word 序列**上：`00`/`ff` run 的合并与拆段
/// 在解包后完全不可区分，按已经压好的流判会漏掉等价分段（例如末尾两个全非零 word 已合并成一个
/// `ff(count=1)`，扫流会判「run 在末尾、无自由度」，拆成两段仍然合法）。自由度只有两个来源（FR-P02）：
///
/// 1. 相邻两个全零 word：零 run 既可写成一个 `00|N`，也可拆成两段；
/// 2. 全非零 word 后面还有 word：`ff` run 的延续选择交给 PACK（把后续 word 原样并入 run 还是在此
///    截断），两种 packed 字节不同、解包相同——`b.long` 的 15-word 变体就是这一条。
fn packed_run_freedom(packed: &[u8]) -> bool {
    let raw = unpack_packed(packed, "packed 行分段自由度扫描");
    let words: Vec<&[u8]> = raw.chunks_exact(8).collect();
    words.windows(2).any(|pair| {
        (pair[0].iter().all(|byte| *byte == 0) && pair[1].iter().all(|byte| *byte == 0))
            || pair[0].iter().all(|byte| *byte != 0)
    })
}

/// `e.nested` 的**先序**变体（LevelA=1 → LevelB=2 → LevelC=3、槽块按前序排列）：仍满足 FR-W02（id
/// 正整数唯一）、FR-W06（范围单调递增、不重叠）、FR-W10（根槽 assist[0]），所以同样合法。金样不得锁死
/// 这个选择；它也用作「布局无关」负控的变异基线。
fn preorder_nested_wire() -> Vec<u8> {
    let mut preorder: Vec<u8> = Vec::new();
    preorder.extend(48_i32.to_le_bytes()); // main 长度：3 条 record × 16B
    for (id, offset) in [(1_i32, 1_u32), (2, 2), (3, 3)] {
        preorder.extend(id.to_le_bytes());
        preorder.extend(8_i32.to_le_bytes());
        preorder.extend(offset.to_le_bytes());
        preorder.extend(1_u32.to_le_bytes());
    }
    preorder.extend(64_i32.to_le_bytes()); // assist 长度：4 × 16B
    for (depth, payload) in [(0_i32, 1_u64), (1, 2), (2, 3), (3, 99)] {
        preorder.extend(depth.to_le_bytes());
        preorder.extend(0_i32.to_le_bytes()); // tag
        preorder.extend(payload.to_le_bytes());
    }
    preorder
}

/// 按**规范序号 + tag** 定位槽下标（负控用）：由 `wire_facts` 从盘上字节算出规范化次序，再把规范序号
/// 换成真实 record id、按 tag 找到槽。因此负控不依赖库当前使用的布局（先序/后序/其它合法编号与槽块
/// 次序都映射到同一逻辑槽）。
#[track_caller]
fn canonical_slot_index(
    bytes: &[u8],
    what: &str,
    layout: &ReferenceLayout,
    canonical_index: usize,
    tag: i32,
) -> usize {
    let facts = wire_facts(bytes, what, layout);
    let id = *facts.order.get(canonical_index - 1).unwrap_or_else(|| {
        panic!(
            "{what}：规范化次序只有 {} 条 record，没有规范序号 {canonical_index}",
            facts.order.len()
        )
    });
    let block = facts.blocks[&id];
    (block.offset..block.offset + block.count)
        .find(|&index| facts.slots[index].1 == tag)
        .unwrap_or_else(|| {
            panic!("{what}：record {id}（规范序号 {canonical_index}）的槽块没有 tag {tag}")
        })
}

/// 反例检验：`e.nested` 换成**先序**编号（LevelA=1 → LevelB=2 → LevelC=3）且槽块按
/// 前序排列，仍满足 FR-W02（id 正整数唯一）、FR-W06（范围单调递增、不重叠）、FR-W10（根槽 assist[0]），
/// 所以同样合法。金样不得锁死这个选择：字节必须不同，但**经过实际断言入口**（`assert_rust_golden`）
/// 必须被接受。
#[test]
fn layout_choice_is_not_pinned_by_golden() {
    let preorder = preorder_nested_wire();

    let facts = wire_facts(&preorder, "先序 e.nested", reference_layout("e.nested"));
    assert_eq!(facts.records.len(), 3, "先序布局仍是合法的 wire");
    let golden = rust_golden("e.nested", false);
    assert_ne!(
        preorder, golden,
        "先序布局应与后序金样字节不同，否则本反例不成立"
    );

    // 经过实际断言入口（而不是只在 `canonical_wire` 上演示）：先序布局与本库输出都必须被接受。
    assert_rust_golden("e.nested", false, &preorder);
    let value = LevelA {
        next: LevelB {
            next: LevelC { value: 99 },
        },
    };
    assert_rust_golden("e.nested", false, &encoded(&value, false));
}

/// 负控：把 `d.class` 的根槽 payload 改成不存在的 record id 9 → 不变量必须拒绝
/// （FR-W10：引用型 root 的槽 payload 必须指向存在的 record id）。变异基线是**独立 raw 金样**（不是本库
/// 输出），根槽恒为 assist[0]（FR-W10），因此任意合法布局下该负控都应被拒。
#[test]
#[should_panic(expected = "FR-W10 引用型 root 的槽 payload 必须指向存在的 record id")]
fn wire_invariants_reject_dangling_root_reference() {
    let mut bytes = rust_golden("d.class", false);
    tamper_slot(&mut bytes, 0, None, Some(9));
    wire_facts(&bytes, "负控 d.class", reference_layout("d.class"));
}

/// 负控：把最内层对象（规范序号 3 = LevelC，嵌套层 3）的 depth 写成 1 → 不变量必须拒绝
/// （FR-T07：槽 depth 必须等于该 record 的嵌套层）。变异基线是**合法先序布局**（不是本库的后序输出），
/// 槽号由规范序号定位：旧版写死槽 1，先序下槽 1 本来就是 depth 1，变异成了空操作。
#[test]
#[should_panic(expected = "槽 depth 必须等于其嵌套层")]
fn wire_invariants_reject_flat_nested_depth() {
    let mut bytes = preorder_nested_wire();
    let index = canonical_slot_index(
        &bytes,
        "负控 e.nested（先序）",
        reference_layout("e.nested"),
        3,
        0,
    );
    tamper_slot(&mut bytes, index, Some(1), None);
    wire_facts(
        &bytes,
        "负控 e.nested（先序）",
        reference_layout("e.nested"),
    );
}

/// 负控：`e.nested` 根槽 payload 的高 4B 污染成 1（`0x00000001_00000003`）→ 必须拒绝。
/// 先 `as u32` 截断再零扩展的写法会把它当成 root record 3 而放过（违反 FR-W02/W10）。
#[test]
#[should_panic(expected = "完整 8B payload 高 4B 必须为零")]
fn wire_invariants_reject_polluted_reference_high_half() {
    let value = LevelA {
        next: LevelB {
            next: LevelC { value: 99 },
        },
    };
    let mut bytes = encoded(&value, false);
    tamper_slot(&mut bytes, 0, None, Some(0x0000_0001_0000_0003));
    wire_facts(&bytes, "负控 e.nested", reference_layout("e.nested"));
}

/// 负控：`g.shared` 的**第二条**共享边（Right，规范序号 1、tag 1）改指不存在的 id 9 → 必须拒绝。
/// 目标仍能从 Left（tag 0）到达，所以只靠「对象 record 可达」的检查会放过这条坏引用。槽号由规范序号
/// 定位、基线是独立 raw 金样：任意合法布局下该负控都应被拒。
#[test]
#[should_panic(expected = "引用位置（规范序号 1、tag 1）的目标 id 必须存在")]
fn wire_invariants_reject_dangling_second_shared_edge() {
    let mut bytes = rust_golden("g.shared", false);
    let index = canonical_slot_index(&bytes, "负控 g.shared", reference_layout("g.shared"), 1, 1);
    tamper_slot(&mut bytes, index, None, Some(9));
    wire_facts(&bytes, "负控 g.shared", reference_layout("g.shared"));
}

/// 负控：`b.long` packed 金样的首个 tag `11`（置位 byte0/byte4）改成 `13`（多置 bit1）并**显式写 0 字节**，
/// 即 `11 8a 01` → `13 8a 00 01`。解包结果与金样完全相同（bit1 那个字节本来就是 0），所以只查「置位
/// 所需字节存在」会放行，但违反 FR-P02「第 i 位表示该 word 第 i 字节非零」。
#[test]
#[should_panic(expected = "FR-P02")]
fn packed_bitmap_rejects_explicit_zero_byte() {
    let golden = rust_golden("b.long", true);
    assert_eq!(
        golden[..3],
        [0x11u8, 0x8a, 0x01],
        "反例前提：b.long packed 金样必须以 tag 0x11 + 两个显式字节开头"
    );
    assert_eq!(
        unpack_packed(&golden, "b.long packed 金样")[1],
        0,
        "反例前提：金样第一个 word 的第 1 字节为 0，多置该位只需显式写 0x00"
    );
    let mut mutated = vec![0x13u8, 0x8a, 0x00, 0x01];
    mutated.extend_from_slice(&golden[3..]);
    assert_ne!(
        mutated, golden,
        "反例前提：变异后的 packed 字节必须与金样不同"
    );
    assert_rust_golden("b.long", true, &mutated);
}
