#!/usr/bin/env python3
"""ace-serialize wire 金样生成器：按规范独立实现的参考编码器。

**独立性边界**：本脚本的线格式只依据文档推导，不调用本仓 Rust 实现、不读取其输出：

- `docs/requirements.md`：FR-W01（envelope）、FR-W02（record 头）、FR-W03（16B 槽）、
  FR-W04（可省略闭集）、FR-W06（对象槽范围）、FR-W07（多态 record）、FR-W08（字符串 body）、
  FR-W09（集合 body）、FR-W10（统一根槽）、FR-W12（数值宽度）、FR-P02/FR-P03（标准 packed 与补零）。
- `_recon/code-spec/E-code-level-spec.md`：§1.2/§1.3（envelope 组装）、§2.2（16B 槽与 tag 缺口）、
  §4.1（集合短格式）、§6.1（packed 基本码；仅作对照，Rust 侧按标准 packed）。

packed 步骤复用同仓 `tools/packed_ref.py`（它自身按标准 packed 规则独立实现，规则与 FR-P02 一致）。

**布局分层：规范唯一规定 vs 本脚本的实现选择**（审核逐条核这两张表）：

① 规范唯一规定 → 逐字节断言（`SPEC` 列逐 case 标注依据）：

- FR-W01 envelope：`[i32 M][main][i32 A][assist]`，总长严格 `M+A+8`，A 为 16 的非负倍数。
- FR-W02 record：`i32 id + i32 size + body`，size 不含头，main 被整数条 record 恰好铺满（FR-W11：平铺、无嵌套）。
- FR-W03 槽：assist 全为 16B `i32 depth + i32 tag + 8B payload`；同一对象范围内 tag 严格递增。
- FR-W04 省略：位模式全零标量、`Option::None` 无槽，保留 tag 空洞而不是重排。
- FR-W06/W07 对象范围：具体对象 body 为 `u32 slot_offset + u32 slot_count`（多态前置 `u16 variant_tag`），
  offset ≥1、独占、不重叠、按记录出现次序单调递增。
- FR-W08/W09 正文：字符串 body 是 UTF-8 原文；集合 body 是 `u32 count + 元素`；Map 按 key 的字节序（FR-T03）。
- FR-W10 根槽：恒为 `assist[0]`、`depth=0/tag=0`、不属于任何对象范围；引用型 root 的 payload 是 root record id。
- FR-W12 数值：自然宽度零扩展/符号扩展；f32 为低 4B 位模式 + 高 4B 零；-0.0 必写。
- FR-P01..FR-P03 packed（叠加在 raw 字节之上，`tools/packed_ref.py` 按标准 packed 独立实现）。

② **规范未唯一规定 → 本脚本/本库的实现选择，不是规范规定**（Rust 测试对这些维度改断言不变量，不锁定本布局）：

- **record id 的具体编号次序**：FR-W02 只要求 id 为正整数、消息内唯一，未规定编号与落盘次序。本脚本按
  **后序**编号——引用型字段的槽 payload 必须是子 record 的 id，而 id 在 record 落盘时才分配，于是子 record
  先落盘、先拿号。先序或其它合法编号在规范上同样成立，字节会与本文件不同。
- **对象槽块在 assist 中的出现次序**：FR-W06 只要求可达对象的范围按记录出现次序单调递增、互不重叠，
  未规定记录次序本身；本脚本的槽块次序随记录落盘次序。合法实现可以选择别的次序。
- **packed `ff` run 的延续分段**：FR-P02 把延续选择交给 PACK——把后续 word 原样并入 run 还是在此截断，
  解包字节相同。本脚本（`tools/packed_ref.py`）取「后续全非零 word 的最大连续段」，所以 `b.long` 的
  `ff` 后面只续接 14 个 word；把下一个 word（`78×6 10 00`）并进去改成 15 个，解包字节不变。

两级规范推导仍成立，且只决定「哪些槽属于哪个对象」而非「谁先编号」：FR-W06 的范围单调递增 +
FR-W04 的省略空洞 → 父对象的槽块必须从当前已分配槽数起连续；FR-W10 的根槽占 0 → 对象槽从 1 起。

**引用位置的独立校验**（`derive()`/`validate()`）：按固定夹具的 `REFERENCE_LAYOUT` 标出全部引用位置
（根槽、对象槽中的引用型 payload、`f.dict3` 的 Map 键），逐个断言①完整 8B payload 的高 4B 为零、
②目标 id 存在（1..=N）；两条不成立都直接失败，**不做「先截断再比」**。标量 payload 原样保留。

**用法**：

- `python3 tools/gen_golden.py`            打印 37 个 `[rust-golden]` case 的 hex
- `python3 tools/gen_golden.py --write`    重写 `crates/ace-serialize/tests/data/ace_wire_golden.hex`
- `python3 tools/gen_golden.py --check`    重新生成并与已提交 `.hex` 逐字节比对；交叉校验 5 个 C# 地真行；
  再跑五条负控（改坏 id 引用 / 把嵌套 depth 写成 1 / 污染引用高位 / 改坏第二条共享边 / packed 位图
  显式写零字节），坏字节必须被 `derive()`（packed 条走 `strict_unpack()`）拒绝
"""

import struct
import sys
from dataclasses import dataclass
from pathlib import Path

from packed_ref import pack, unpack

SCRIPT = "tools/gen_golden.py"
GOLDEN_PATH = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "ace-serialize"
    / "tests"
    / "data"
    / "ace_wire_golden.hex"
)

# FR-W12 数值宽度表（自然宽度，小端）。
WIDTH = {
    "bool": 1,
    "i8": 1,
    "u8": 1,
    "i16": 2,
    "u16": 2,
    "i32": 4,
    "u32": 4,
    "f32": 4,
    "char": 4,
    "i64": 8,
    "u64": 8,
    "f64": 8,
}
SIGNED = {"i8", "i16", "i32", "i64"}


# ---------------------------------------------------------------------------
# 值模型：只覆盖 37 个金样涉及到的形状。节点按**实例身份**（`id()`）参与身份表，
# 与 FR-T04「身份按真实对象身份、不按内容哈希」一致；`eq=False` 使 `==` 不按内容比较。
# ---------------------------------------------------------------------------


@dataclass(eq=False)
class Scalar:
    """≤8B 定宽标量（FR-W12）。`value` 为位值/数值（`bool` 用 True/False）。"""

    kind: str
    value: object


@dataclass(eq=False)
class Text:
    """`String`：值为字符串，body 为 UTF-8 原文（FR-W08）。"""

    text: str


@dataclass(eq=False)
class Seq:
    """`Vec`：body 为 `u32 count + elements`（FR-W09）。"""

    elements: tuple


@dataclass(eq=False)
class Map:
    """`BTreeMap<String, i32>`：按 key 的 Ord 顺序（FR-T03）写 `u32 count + 对`（FR-W09）。"""

    entries: tuple  # ((Text, 值节点), ...)


@dataclass(eq=False)
class Obj:
    """具体对象 record：body 为 `u32 slot_offset + u32 slot_count`（FR-W06）。"""

    fields: tuple


@dataclass(eq=False)
class PolyObj:
    """多态对象 record：body 为 `u16 variant_tag + u32 slot_offset + u32 slot_count`（FR-W07）。"""

    variant_tag: int
    fields: tuple


@dataclass(eq=False)
class Opt:
    """`Option<T>`：`None` 无槽，`Some(v)` 必写（FR-W04/FR-T02）。"""

    value: object


def obj(*fields):
    """具体对象：参数为按 tag 递增的 `(order, 值节点)`。"""
    return Obj(tuple(fields))


# ---------------------------------------------------------------------------
# 参考编码器
# ---------------------------------------------------------------------------


def i32(value):
    return struct.pack("<i", value)


def u32(value):
    return struct.pack("<I", value)


def u16(value):
    return struct.pack("<H", value)


def u64(value):
    return struct.pack("<Q", value)


def scalar_bytes(scalar):
    """FR-W12：按类型自然位值产出的**自然宽度**小端字节（集合元素用）。"""
    kind, value = scalar.kind, scalar.value
    if kind == "bool":
        return bytes([1 if value else 0])
    if kind == "char":
        return u32(value)
    if kind == "f32":
        return struct.pack("<f", value)
    if kind == "f64":
        return struct.pack("<d", value)
    if kind in SIGNED:
        return int(value).to_bytes(WIDTH[kind], "little", signed=True)
    return int(value).to_bytes(WIDTH[kind], "little", signed=False)


def scalar_slot_payload(scalar):
    """FR-W12：字段/根槽的 8B payload——无符号/正数零扩展、有符号负数符号扩展。

    `f32` 取 IEEE754 低 4B 位模式 + 高 4B 零；`f64` 取原 8B 位模式；`char` 取 Unicode 标量 u32。
    """
    kind, value = scalar.kind, scalar.value
    if kind in SIGNED:
        return struct.pack("<q", int(value))  # 8B 二补码 = 符号扩展
    if kind == "bool" or kind == "char" or kind in ("u8", "u16", "u32"):
        return u64(int(value))
    if kind == "f32":
        return struct.pack("<f", value) + bytes(4)
    if kind == "f64":
        return struct.pack("<d", value)
    if kind == "u64":
        return u64(int(value))
    raise ValueError(f"未知标量类型 {kind}")


class Encoder:
    """双区编码器：`main` 平铺 record（FR-W02），`slots` 为 assist 的 16B 槽（FR-W03）。"""

    def __init__(self):
        self.main = bytearray()
        self.slots = []  # [(depth, tag, payload8)]
        self.ids = {}  # id(node) -> record id（FR-T04 身份表）
        self.next_id = 1  # FR-W02：id 为消息内正整数，从 1 起
        self.objects = []  # [(record_id, slot_offset, slot_count, depth)]：自校验用的结构事实

    # -- 输出 -------------------------------------------------------------

    def finish(self):
        """FR-W01：`[i32 M][main][i32 A][assist]`，总长严格 M+A+8。"""
        main = bytes(self.main)
        assist = b"".join(
            i32(depth) + i32(tag) + payload for depth, tag, payload in self.slots
        )
        return i32(len(main)) + main + i32(len(assist)) + assist

    # -- 根 ---------------------------------------------------------------

    def encode_root(self, value):
        """FR-W10：根槽固定 assist[0]（depth=0/tag=0，不属任何对象范围）；`None` 无根槽。"""
        if isinstance(value, Opt):
            if value.value is None:
                return  # assist 长度 0
            self.encode_root(value.value)
            return
        index = len(self.slots)
        self.slots.append((0, 0, None))  # 先占住 assist[0]，引用型要等 record id 才能回填
        self.slots[index] = (0, 0, self.root_payload(value))

    def root_payload(self, value):
        if isinstance(value, Scalar):
            return scalar_slot_payload(value)  # ≤8B 标量 → 数值根槽（零值也必写）
        return u64(self.record(value, depth=1))  # 引用型 → payload = root record id

    # -- 字段槽 -----------------------------------------------------------

    def field_payload(self, value, depth):
        """返回该字段 16B 槽的 8B payload；`None` 表示该字段按 FR-W04 省略。"""
        if isinstance(value, Scalar):
            payload = scalar_slot_payload(value)
            return None if payload == bytes(8) else payload  # FR-W04①：位模式全零的标量
        if isinstance(value, Opt):
            if value.value is None:
                return None  # FR-W04②：Option::None
            return self.required_payload(value.value, depth)  # Some(v) 必写，含 Some(0)
        return self.required_payload(value, depth)

    def required_payload(self, value, depth):
        """必写字段的 payload：标量 → 8B 自然位值；引用型 → 8B 零扩展 record id。

        `depth` 是**持有该槽的对象**的深度；引用型子对象的深度是 `depth + 1`（FR-T07 深度按嵌套层计）。
        """
        if isinstance(value, Scalar):
            return scalar_slot_payload(value)
        return u64(self.record(value, depth + 1))

    # -- record -----------------------------------------------------------

    def record(self, node, depth):
        """把引用型值写成一条 main record，返回其 id；`depth` 是该对象自身的深度。

        记录与自身槽块都**在子对象之后**写出：槽 payload 需要子 record 的 id，而 id 在
        record 落盘时才分配。**这是本脚本的实现选择（后序编号/落盘），不是规范规定**——
        FR-W02 只要求 id 为正整数且唯一，FR-W06 只要求可达对象范围按记录出现次序单调递增、
        互不重叠；先序编号等其它合法次序会给出不同的字节（见文件头的布局分层表）。
        """
        identity = id(node)
        if identity in self.ids:
            return self.ids[identity]  # FR-T04：同一实例复用 id，不重复展开

        if isinstance(node, Text):
            body = node.text.encode("utf-8")  # FR-W08：body 就是原始字节
        elif isinstance(node, Seq):
            body = u32(len(node.elements)) + b"".join(
                self.element_bytes(element, depth + 1) for element in node.elements
            )  # FR-W09
        elif isinstance(node, Map):
            ordered = sorted(
                node.entries, key=lambda entry: entry[0].text.encode("utf-8")
            )  # FR-T03：BTreeMap 按 key 的 Ord（String 即 UTF-8 字节序）
            body = u32(len(ordered)) + b"".join(
                u32(self.record(map_key, depth + 1))
                + self.element_bytes(map_value, depth + 1)
                for map_key, map_value in ordered
            )  # FR-W09：Map 按 key 再 value，count 为键值对数
        elif isinstance(node, PolyObj):
            payloads = [
                self.field_payload(field_value, depth) for _, field_value in node.fields
            ]
            offset = len(self.slots)
            count = self.append_slots(node.fields, payloads, depth)
            body = u16(node.variant_tag) + u32(offset) + u32(count)  # FR-W07
        elif isinstance(node, Obj):
            payloads = [
                self.field_payload(field_value, depth) for _, field_value in node.fields
            ]
            offset = len(self.slots)
            count = self.append_slots(node.fields, payloads, depth)
            body = u32(offset) + u32(count)  # FR-W06
        else:
            raise ValueError(f"未知引用型节点 {node!r}")

        record_id = self.next_id
        self.next_id += 1
        self.main += i32(record_id) + i32(len(body)) + body  # FR-W02
        self.ids[identity] = record_id
        if isinstance(node, (Obj, PolyObj)):
            self.objects.append((record_id, offset, count, depth))
        return record_id

    def append_slots(self, fields, payloads, depth):
        """把本对象未省略字段的槽按 tag 递增追加为**连续块**，返回槽数（FR-W03/FR-W06）。"""
        count = 0
        for (tag, _), payload in zip(fields, payloads):
            if payload is None:
                continue  # 省略字段保留 tag 空洞（FR-W04）
            self.slots.append((depth, tag, payload))
            count += 1
        return count

    def element_bytes(self, element, depth):
        """FR-W09：集合元素无 Depth/Tag 头——标量按自然宽度紧密排列，引用型写 u32 record id。"""
        if isinstance(element, Scalar):
            return scalar_bytes(element)
        if isinstance(element, Opt):
            raise ValueError("本脚本只覆盖 37 个金样用到的元素形状")
        return u32(self.record(element, depth))


def encode(case, value, packed):
    """生成 `case` 的完整消息；`packed=True` 叠加标准 packed（FR-P01）。

    `case` 只用于取固定夹具的引用位置表（`REFERENCE_LAYOUT`），不参与线格式推导。
    """
    encoder = Encoder()
    encoder.encode_root(value)
    raw = encoder.finish()
    validate(case, raw, encoder.objects)
    return pack(raw) if packed else raw


# ---------------------------------------------------------------------------
# 结构自校验：把生成的字节**按规范再读一遍**，逐条核对可判定的不变量。
#
# 关键点（独立性）：下面 `derive()` 只读 `raw` 字节自己的长度前缀、record 头、槽与 payload，
# **不读编码器的 `objects`**；`validate()` 才把重算结果与编码器自述结构比对。
# 编码器算错时这里以「盘上 vs 编码器」不一致失败，而不是同义反复地自证。
# wire 不自描述（FR-W11），故「哪条 record 是对象」只能由槽块覆盖关系从字节推出：正文长 8（FR-W06）
# 或 10（FR-W07）且声明的 offset 恰好接在已覆盖范围末尾；37 个固定形状下该判定唯一。
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SlotBlock:
    """对象 record 的槽块事实：全部由盘上字节重算，与编码器无关。"""

    offset: int
    count: int
    variant: object  # None=具体对象（FR-W06）；否则为 u16 variant_tag（FR-W07）
    depth: int  # 该 record 的嵌套层（FR-T07：root record=1），由引用图重算


@dataclass(frozen=True)
class Structure:
    """从盘上字节重算出的 wire 结构。"""

    records: tuple  # ((record_id, body), ...)，main 顺序
    slots: tuple  # ((depth, tag, payload), ...)
    blocks: dict  # record_id -> SlotBlock
    root: object  # 根槽 payload 解析出的 record id（标量根/无根槽为 None）


def reference(payload, ids):
    """FR-W02/W10：引用型 payload 是 8B 零扩展的 record id（**完整 8B 的高 4B 为零**、低 4B 是存在的 id）。

    高 4B 非零时返回 None——不能先截断再判，否则 `0x00000001_00000003` 会被误认为 id 3。
    """
    if payload >> 32 != 0:
        return None
    low = payload & 0xFFFFFFFF
    return low if low in ids else None


# 固定夹具的**引用位置**表（与 Rust 测试的 `ReferenceLayout` 同口径）：
# `slots` 是 ((规范化序号, tag), ...)——该槽的完整 8B payload 必须是引用；
# `map_keys` 是 ((规范化序号, 键个数), ...)——该 record 的 Map 正文 `u32 count + (u32 key + 4B 值)×count`
# 的键是引用（键是 4B record id，没有高 4B）。规范化序号是 `derive()` 算出的次序（根 record = 1，
# 槽按 tag、Map 键按正文次序前序 DFS）。只有含 >1 条 record 的行才有 record 内引用。
REFERENCE_LAYOUT = {
    "e.nested": {"slots": ((1, 0), (2, 0)), "map_keys": ()},  # LevelA.next→LevelB→LevelC
    "f.dict3": {"slots": (), "map_keys": ((1, 3),)},  # 根 Map 的 3 个 4B 键引用
    "g.shared": {"slots": ((1, 0), (1, 1)), "map_keys": ()},  # Left/Right 同指一个 OneInt
    "g.equal": {"slots": ((1, 0), (1, 1)), "map_keys": ()},  # Left/Right 两个不同 OneInt
    "i.complex": {"slots": ((1, 1), (1, 2), (1, 3)), "map_keys": ()},  # items/child/text（maybe 是标量）
    "j.interface": {"slots": ((1, 0),), "map_keys": ()},  # AnimalHolder.pet → Cat
    "j.abstract": {"slots": ((1, 0),), "map_keys": ()},  # AnimalHolder.pet → Dog
}
NO_REFERENCES = {"slots": (), "map_keys": ()}


def layout_of(case):
    """取该 case 的引用位置表；未列出的行（单/零 record）没有 record 内引用位置。"""
    return REFERENCE_LAYOUT.get(case, NO_REFERENCES)


def map_key_ids(body, keys):
    """Map 正文 `u32 count + (u32 键 id + 4B 值)×count`（FR-W09/FR-T03）的 4B 键引用。"""
    assert len(body) == 4 + 8 * keys, (
        f"Map 正文长度必须等于 u32 count + 8B×键个数（{keys}），实际 {len(body)}"
    )
    (count,) = struct.unpack("<I", body[:4])
    assert count == keys, f"Map 正文 count 必须等于引用位置表声明的键数 {keys}"
    return [struct.unpack("<I", body[4 + 8 * key : 8 + 8 * key])[0] for key in range(keys)]


def derive(raw, layout=NO_REFERENCES):
    """只从 `raw` 字节重算结构并逐条断言不变量（FR-W01/W02/W03/W06/W07/W10 + FR-T07）。

    `layout` 是固定夹具的引用位置表（`REFERENCE_LAYOUT`）：这些位置必须先过两道断言
    （完整 8B payload 的高 4B 为零；目标 id 存在 1..=N），才允许后续比对；未列出的 payload 当标量原样保留。
    """
    assert len(raw) >= 8, "FR-W01：至少含两个 i32 长度前缀"
    (main_len,) = struct.unpack("<i", raw[:4])
    assert main_len >= 0 and 4 + main_len + 4 <= len(raw), "FR-W01：main 长度越界"
    main = raw[4 : 4 + main_len]
    (assist_len,) = struct.unpack("<i", raw[4 + main_len : 8 + main_len])
    assist = raw[8 + main_len :]
    assert assist_len == len(assist), "FR-W01：assist 长度与声明不符"
    assert len(raw) == main_len + assist_len + 8, "FR-W01：总长必须严格 M+A+8"
    assert assist_len % 16 == 0, "FR-W03：assist 必须是 16B 的倍数"

    slots = tuple(
        struct.unpack("<iiQ", assist[position : position + 16])
        for position in range(0, assist_len, 16)
    )

    # FR-W02：record 头 `i32 id + i32 size + body`，id 正整数且唯一，main 被整数条 record 恰好铺满。
    records = []
    ids = set()
    position = 0
    while position < main_len:
        assert position + 8 <= main_len, "FR-W02：record 头越界"
        record_id, size = struct.unpack("<ii", main[position : position + 8])
        assert record_id > 0, "FR-W02：record id 必须为正"
        assert size >= 0 and position + 8 + size <= main_len, "FR-W02：record size 越界"
        assert record_id not in ids, f"FR-W02：record id {record_id} 重复"
        ids.add(record_id)
        records.append((record_id, main[position + 8 : position + 8 + size]))
        position += 8 + size
    assert position == main_len, "FR-W02：main 必须被整数条 record 恰好铺满"
    assert sorted(ids) == list(range(1, len(records) + 1)), (
        "record id 必须恰为 1..N（本实现的编号约定；FR-W02 只要求正整数、消息内唯一）"
    )

    # FR-W10：根槽恒为 assist[0]、depth=0/tag=0、不属于任何对象范围。
    if slots:
        assert slots[0][:2] == (0, 0), "FR-W10：根槽必须是 assist[0] 且 depth=0/tag=0"

    # FR-W06：对象槽块独占、不重叠、按记录出现次序 offset 单调递增 → 从 1 起贪心覆盖即唯一。
    blocks = {}
    covered = 1
    for record_id, body in records:
        if len(body) not in (8, 10):
            continue  # 字符串/集合正文（FR-W08/W09），不是对象
        if len(body) == 8:
            variant, at = None, 0
        else:
            variant, at = struct.unpack("<H", body[:2])[0], 2
        offset, count = struct.unpack("<II", body[at : at + 8])
        if offset != covered or offset + count > len(slots):
            continue
        assert offset >= 1, "FR-W06：对象 slot_offset 必须 ≥1（根槽不属于任何对象）"
        tags = [slot[1] for slot in slots[offset : offset + count]]
        assert all(a < b for a, b in zip(tags, tags[1:])), (
            f"FR-W03：record {record_id} 的槽 tag 必须严格递增（省略字段留空洞），实际 {tags}"
        )
        blocks[record_id] = (offset, count, variant)
        covered = offset + count
    if slots:
        assert covered == len(slots), (
            f"FR-W06：除 assist[0] 外的 {len(slots) - 1} 个槽必须恰好被对象槽块覆盖，"
            f"实际覆盖 {covered - 1}"
        )
    else:
        assert not blocks, "FR-W06：assist 为空时不得有对象槽块"

    # FR-T07：嵌套层由「槽 payload 引用 → 子 record」重算（root record=1，进入新对象 +1），
    # 再把每个对象槽块的 depth 与它的嵌套层、tag 顺序逐槽核对。
    # FR-W10：引用型 root 的完整 8B payload 高 4B 必须为零——**先看完整 8B**，不能截断后再判。
    root = None
    if slots:
        if main_len > 0:
            assert slots[0][2] >> 32 == 0, (
                "FR-W10：引用型 root 的完整 8B payload 高 4B 必须为零，"
                f"实际 0x{slots[0][2]:016x}"
            )
        root = reference(slots[0][2], ids)
        if main_len > 0:
            assert root is not None, (
                "FR-W10：引用型 root 的槽 payload 必须指向存在的 record id，"
                f"实际 0x{slots[0][2]:016x}"
            )
    depths = {}
    if root is not None:
        depths[root] = 1
        queue = [root]
        while queue:
            record_id = queue.pop()
            block = blocks.get(record_id)
            if block is None:
                continue  # 字符串/集合/Map 记录没有槽块
            offset, count, _variant = block
            depth = depths[record_id]
            for index in range(offset, offset + count):
                slot_depth, tag, payload = slots[index]
                assert slot_depth == depth, (
                    f"FR-T07/FR-W03：record {record_id} 的槽 depth 必须等于其嵌套层 {depth}"
                    f"（槽 {index}、tag {tag}）"
                )
                target = reference(payload, ids)
                if target is None:
                    continue
                if target in depths:
                    assert depths[target] == depth + 1, (
                        f"FR-T07：共享 record {target} 被引用于不同嵌套层"
                        f"（{depths[target]} 与 {depth + 1}）"
                    )
                else:
                    depths[target] = depth + 1
                    queue.append(target)
    for record_id in blocks:
        assert record_id in depths, f"FR-W06：对象 record {record_id} 必须可从根槽到达"

    bodies = dict(records)
    # 规范化次序（引用位置表的坐标系）：根 record 先访问，槽按 tag、Map 键按正文次序做前序 DFS。
    order = []
    pending = [root] if root is not None else []
    while pending:
        record_id = pending.pop()
        if record_id in order:
            continue
        order.append(record_id)
        canonical_index = len(order)
        children = []
        block = blocks.get(record_id)
        if block is not None:
            offset, count, _variant = block
            for index in range(offset, offset + count):
                target = reference(slots[index][2], ids)
                if target is not None:
                    children.append(target)
        for index, keys in layout["map_keys"]:
            if index == canonical_index:
                children.extend(
                    target
                    for target in (
                        reference(key, ids) for key in map_key_ids(bodies[record_id], keys)
                    )
                    if target is not None
                )
        pending.extend(reversed(children))

    # 引用位置校验：每处必须是「8B 零扩展的存在的 record id」。任一处不成立直接失败，
    # 不进入后续比对（否则高位污染/悬空 id 会被抹平）。
    for canonical_index, tag in layout["slots"]:
        assert 1 <= canonical_index <= len(order), (
            f"引用位置表指向规范序号 {canonical_index}，但规范化次序只有 {len(order)} 条 record"
        )
        record_id = order[canonical_index - 1]
        offset, count, _variant = blocks[record_id]
        payloads = [
            slots[index][2]
            for index in range(offset, offset + count)
            if slots[index][1] == tag
        ]
        assert payloads, (
            f"引用位置表指向 record {record_id}（规范序号 {canonical_index}）的 tag {tag}，"
            "但该槽块没有这个 tag"
        )
        payload = payloads[0]
        assert payload >> 32 == 0, (
            f"FR-W02/W10：引用位置（规范序号 {canonical_index}、tag {tag}）的完整 8B payload "
            f"高 4B 必须为零，实际 0x{payload:016x}"
        )
        assert reference(payload, ids) is not None, (
            f"FR-W02/W10：引用位置（规范序号 {canonical_index}、tag {tag}）的目标 id "
            f"必须存在（1..={len(records)}），实际 0x{payload:016x}"
        )
    for canonical_index, keys in layout["map_keys"]:
        assert 1 <= canonical_index <= len(order), (
            f"引用位置表指向规范序号 {canonical_index}，但规范化次序只有 {len(order)} 条 record"
        )
        record_id = order[canonical_index - 1]
        for key in map_key_ids(bodies[record_id], keys):
            assert key in ids, (
                f"FR-W09/W10：Map 键引用（规范序号 {canonical_index}）的目标 id "
                f"必须存在（1..={len(records)}），实际 {key}"
            )
    assert len(order) == len(records), (
        f"规范化次序必须覆盖全部 record（根槽 + 槽引用 + Map 键），"
        f"实际 {len(order)} / {len(records)}"
    )

    return Structure(
        records=tuple(records),
        slots=slots,
        blocks={
            record_id: SlotBlock(offset, count, variant, depths[record_id])
            for record_id, (offset, count, variant) in blocks.items()
        },
        root=root,
    )


def validate(case, raw, objects):
    """FR-W01/W02/W03/W06/W07/W10：从 `raw` 字节重算结构，再与编码器自述结构比对。

    `objects` 是编码器编码时记录的结构事实 `(record_id, slot_offset, slot_count, depth)`，只用于
    **比对**：如果编码器算错，`derive()` 的盘上结论会与它不一致而失败，不再被同义反复掩盖。
    `case` 决定引用位置表（`layout_of`）：`derive()` 会先逐个校验引用高位为零与目标存在性。
    """
    derived = derive(raw, layout_of(case))
    on_wire = {
        record_id: (block.offset, block.count, block.depth)
        for record_id, block in derived.blocks.items()
    }
    declared = {
        record_id: (offset, count, depth)
        for record_id, offset, count, depth in objects
    }
    assert on_wire == declared, (
        f"FR-W06：盘上重算的对象槽块与编码器结构不符\n  盘上 {on_wire}\n  编码器 {declared}"
    )


# ---------------------------------------------------------------------------
# 37 个 [rust-golden] case 的取值（与 crates/ace-serialize/tests/csharp_conformance.rs 一一对应）
# ---------------------------------------------------------------------------

ONE_INT_7 = obj((0, Scalar("i32", 7)))
TWO_INTS = obj((0, Scalar("i32", 0x1234_5678)), (1, Scalar("i32", -2)))
# LevelA → LevelB → LevelC（C.Value = 99），逐层嵌套对象。
THREE_DEEP = obj((0, obj((0, obj((0, Scalar("i32", 99)))))))
# Complex{maybe: Some(5), items: Some([2,3]), child: Some(OneInt{8}), text: Some("中")}。
COMPLEX = obj(
    (0, Opt(Scalar("i32", 5))),
    (1, Opt(Seq((Scalar("i32", 2), Scalar("i32", 3))))),
    (2, Opt(obj((0, Scalar("i32", 8))))),
    (3, Opt(Text("中"))),
)
CHINESE = Text("中文🙂")
LONG_TEXT = Text("x" * 130)

RAW_CASES = {
    "a.long": Scalar("i64", 0x0102_0304_0506_0708),
    "a.int": Scalar("i32", 0x1234_5678),
    "a.short": Scalar("i16", 0x1234),
    "a.byte": Scalar("u8", 0xAB),
    "a.bool.true": Scalar("bool", True),
    "a.bool.false": Scalar("bool", False),
    "a.float": Scalar("f32", 1.5),
    "a.double": Scalar("f64", -2.25),
    "a.char": Scalar("char", 0x4E2D),
    "c.struct": TWO_INTS,
    "d.class": obj((0, Scalar("i32", 42))),
    "d.default": obj((0, Scalar("i32", 0))),
    "e.nested": THREE_DEEP,
    "f.array0": Seq(()),
    "f.array3": Seq((Scalar("i32", 1), Scalar("i32", 0), Scalar("i32", -2))),
    "f.array200": Seq(tuple(Scalar("i32", index) for index in range(200))),
    "f.list0": Seq(()),
    "f.list3": Seq((Scalar("i32", 1), Scalar("i32", 0), Scalar("i32", -2))),
    "f.dict0": Map(()),
    "f.dict3": Map(((Text("a"), Scalar("i32", 1)), (Text("中"), Scalar("i32", 0)), (Text("c"), Scalar("i32", -2)))),
    "g.shared": obj((0, ONE_INT_7), (1, ONE_INT_7)),  # 同一 Rc 实例 → 同一 id
    "g.equal": obj(
        (0, obj((0, Scalar("i32", 7)))), (1, obj((0, Scalar("i32", 7))))
    ),  # 两个相等但不同的实例 → 两个 id
    "i.nullable.value": Opt(Scalar("i32", 123)),
    "i.nullable.zero": Opt(Scalar("i32", 0)),
    "i.null.fields": obj((0, Opt(None)), (1, Opt(None)), (2, Opt(None)), (3, Opt(None))),
    "i.complex": COMPLEX,
    "j.interface": obj((0, PolyObj(0, ((0, Scalar("i32", 9)),)))),
    "j.abstract": obj((0, PolyObj(1, ((0, Scalar("i32", 4)),)))),
}

PACKED_CASES = {
    "a.int": RAW_CASES["a.int"],
    "a.long": RAW_CASES["a.long"],
    "b.chinese": CHINESE,
    "b.long": LONG_TEXT,
    "c.struct": TWO_INTS,
    "d.class": RAW_CASES["d.class"],
    "d.default": RAW_CASES["d.default"],
    "f.array3": RAW_CASES["f.array3"],
    "i.nullable.value": RAW_CASES["i.nullable.value"],
}

# 规范章节引用（每个 case 的期望字节由哪几条条款推导；写进 .hex 供审核逐条对照）。
SPEC = {
    "a.long": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.int": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.short": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.byte": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.bool.true": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.bool.false": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.float": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.double": "FR-W01,FR-W03,FR-W10,FR-W12",
    "a.char": "FR-W01,FR-W03,FR-W10,FR-W12",
    "c.struct": "FR-W01,FR-W02,FR-W03,FR-W06,FR-W10,FR-W12",
    "d.class": "FR-W01,FR-W02,FR-W03,FR-W06,FR-W10",
    "d.default": "FR-W01,FR-W02,FR-W04,FR-W06,FR-W10",
    "e.nested": "FR-W01,FR-W02,FR-W03,FR-W06,FR-W10",
    "f.array0": "FR-W01,FR-W02,FR-W09,FR-W10",
    "f.array3": "FR-W01,FR-W02,FR-W09,FR-W10,FR-W12",
    "f.array200": "FR-W01,FR-W02,FR-W09,FR-W10,FR-W12",
    "f.list0": "FR-W01,FR-W02,FR-W09,FR-W10",
    "f.list3": "FR-W01,FR-W02,FR-W09,FR-W10,FR-W12",
    "f.dict0": "FR-W01,FR-W02,FR-W09,FR-W10",
    "f.dict3": "FR-W01,FR-W02,FR-W09,FR-W10,FR-W12,FR-T03",
    "g.shared": "FR-W02,FR-W03,FR-W06,FR-W10,FR-T04",
    "g.equal": "FR-W02,FR-W03,FR-W06,FR-W10,FR-T04",
    "i.nullable.value": "FR-W01,FR-W03,FR-W10",
    "i.nullable.zero": "FR-W01,FR-W03,FR-W10",
    "i.null.fields": "FR-W01,FR-W02,FR-W04,FR-W06,FR-W10",
    "i.complex": "FR-W02,FR-W03,FR-W06,FR-W08,FR-W09,FR-W10",
    "j.interface": "FR-W02,FR-W03,FR-W06,FR-W07,FR-W10",
    "j.abstract": "FR-W02,FR-W03,FR-W06,FR-W07,FR-W10",
}

PACKED_SPEC = {
    "a.int": "FR-P01,FR-P02,FR-P03",
    "a.long": "FR-P01,FR-P02,FR-P03",
    "b.chinese": "FR-P01,FR-P02,FR-P03",
    "b.long": "FR-P01,FR-P02,FR-P03",
    "c.struct": "FR-P01,FR-P02,FR-P03",
    "d.class": "FR-P01,FR-P02,FR-P03",
    "d.default": "FR-P01,FR-P02,FR-P03",
    "f.array3": "FR-P01,FR-P02,FR-P03",
    "i.nullable.value": "FR-P01,FR-P02,FR-P03",
}

# 5 个 [csharp-bytes] 行的 C# 地真 DUMP（_recon/D-ground-truth-bytes.md §5，冻结实测）。
# 只作**交叉校验**：这 5 行是「C# 与本库骨架/字节重合」的行，本脚本必须逐字节复现。
CSHARP_TRUTH = {
    "b.empty": "0800000001000000000000001000000000000000000000000100000000000000",
    "b.null": "0000000000000000",
    "b.chinese": "12000000010000000a000000e4b8ade69687f09f99821000000000000000000000000100000000000000",
    "b.long": "8a0000000100000082000000" + "78" * 130 + "1000000000000000000000000100000000000000",
    "i.nullable.none": "0000000000000000",
}
CSHARP_VALUES = {
    "b.empty": Text(""),
    "b.null": Opt(None),
    "b.chinese": CHINESE,
    "b.long": LONG_TEXT,
    "i.nullable.none": Opt(None),
}


# ---------------------------------------------------------------------------
# 渲染与校验
# ---------------------------------------------------------------------------


def entries():
    """按「28 raw + 9 packed」的固定次序产出 (case, mode, spec, bytes)。"""
    rows = []
    for case, value in RAW_CASES.items():
        rows.append((case, "raw", SPEC[case], encode(case, value, packed=False)))
    for case, value in PACKED_CASES.items():
        rows.append((case, "pack", PACKED_SPEC[case], encode(case, value, packed=True)))
    return rows


HEADER = """\
# ace-serialize wire 金样（[rust-golden] 37 行）—— 由 {script} 独立生成，勿手改
# 重新生成：python3 {script} --write    校验：python3 {script} --check
# 独立性：本文件由 {script} 按规范独立实现的参考编码器产出，不调用 Rust 实现、不读取其输出。
# 生成依据：docs/requirements.md 的 FR-W01..FR-W12 / FR-P01..FR-P03 / FR-T03 / FR-T04，
#           与 _recon/code-spec/E-code-level-spec.md §1.2/§1.3/§2.2/§4.1。
# 列：case 模式 脚本 case 标识 规范章节 hex
# 第 3 列是该 case 在 tools/gen_golden.py 里 RAW_CASES / PACKED_CASES 的字典键（可 grep 到数值定义），
# 第 4 列是推导该期望字节的规范条款。
#
# **布局分层（hex 列里有两类维度，勿混用）**
# ① 规范唯一规定 → 逐字节断言：envelope 长度算术 M+A+8（FR-W01）、record 头 i32 id+i32 size+body 且
#    main 被整数条 record 恰好铺满（FR-W02/W11）、槽 16B 形态（FR-W03）、可省略闭集与 Option::None 省略
#    （FR-W04）、对象槽范围（FR-W06/W07）、UTF-8 原文与集合 u32 count+元素（FR-W08/W09）、Map 按 key
#    字节序（FR-T03）、根槽在 assist[0] 且 depth=0/tag=0（FR-W10）、数值归一/扩展位模式（FR-W12）、
#    packed（FR-P01..FR-P03）。引用型 payload 另按固定夹具的引用位置表逐个校验「完整 8B 高位为零 +
#    目标 id 存在」；标量 payload 原样保留。
# ② **这是实现选择，不是规范规定** → 不锁定（三维）：record id 的具体编号次序（FR-W02 只要求正整数、
#    消息内唯一）、对象槽块在 assist 中的出现次序（FR-W06 只要求可达对象范围按记录出现次序单调递增、
#    互不重叠）、packed `ff` run 的延续分段（FR-P02 把延续选择交给 PACK：把后续 word 原样并入 run 还是
#    在此截断，解包字节相同）。本文件取后序编号、槽块随落盘次序、`ff` run 取「后续全非零 word 的最大
#    连续段」；先序/其它合法分段字节会不同。
#    Rust 侧 crates/ace-serialize/tests/csharp_conformance.rs 对前两维从盘上字节重算不变量并规范化后
#    比较其余字节，对第三维只做「格式检查 + FR-P03 零补齐 + 解包后等于该行 raw」；因此 37 行里
#    **29 行直接逐字节硬比、8 行只在非唯一维度归一/查不变量**：7 个多 record 的 raw 行（e.nested /
#    f.dict3 / g.shared / g.equal / i.complex / j.interface / j.abstract）+ packed 的 b.long
#    （`ff` run 后面还有 word）。其余 8 个 packed 行的 run 分段无自由度：按**解包后的 word 序列**判
#    （不按已压好的流判，否则会漏掉等价分段），既没有相邻全零 word（`00` run 可合并/拆段），也没有
#    「后面还有 word 的全非零 word」（`ff` run 可续接/截断），packed 字节由规范/PACK 唯一决定，仍硬比。
""".format(script=SCRIPT)


def render(rows):
    lines = [HEADER]
    for case, mode, spec, blob in rows:
        lines.append(f"{case} {mode} {case} {spec} {blob.hex()}\n")
    return "".join(lines)


def hexdump(case, mode, blob):
    print(f"{case} ({mode}, {len(blob)} 字节, 脚本标识 {case}): {blob.hex()}")


def main(argv):
    rows = entries()
    if "--write" in argv:
        GOLDEN_PATH.parent.mkdir(parents=True, exist_ok=True)
        GOLDEN_PATH.write_text(render(rows), encoding="utf-8")
        print(f"已写入 {GOLDEN_PATH.relative_to(Path(__file__).resolve().parent.parent)}（{len(rows)} 行）")
        return 0
    if "--check" in argv:
        return check(rows)

    for case, mode, _spec, blob in rows:
        hexdump(case, mode, blob)
    return 0


def check(rows):
    failures = []
    expected = render(rows)
    if not GOLDEN_PATH.exists():
        failures.append(f"缺少金样文件 {GOLDEN_PATH}")
    else:
        actual = GOLDEN_PATH.read_text(encoding="utf-8")
        if actual != expected:
            actual_lines = actual.splitlines()
            expected_lines = expected.splitlines()
            for index in range(max(len(actual_lines), len(expected_lines))):
                want = expected_lines[index] if index < len(expected_lines) else "<缺行>"
                got = actual_lines[index] if index < len(actual_lines) else "<缺行>"
                if want != got:
                    failures.append(f"第 {index + 1} 行不符：\n  已提交 {got}\n  重新生成 {want}")
                    break

    # 交叉校验：与 C# 地真的 5 行逐字节相同（证明参考实现复现了重合骨架，而非照抄被测实现）。
    for case, truth_hex in CSHARP_TRUTH.items():
        produced = encode(case, CSHARP_VALUES[case], packed=False).hex()
        if produced != truth_hex:
            failures.append(
                f"C# 地真交叉校验失败 {case}：\n  C# {truth_hex}\n  参考实现 {produced}"
            )

    # 结构自校验已在 encode() 内逐条执行（validate）；这里再核 sheet 完整性。
    if len(rows) != 37:
        failures.append(f"金样行数必须为 37，实际 {len(rows)}")

    # 引用位置表必须正好覆盖多 record 的 raw 行（否则引用高位/目标存在性会漏查）。
    multi_record = sorted(
        case
        for case, value in RAW_CASES.items()
        if len(derive(encode(case, value, packed=False), layout_of(case)).records) > 1
    )
    if sorted(REFERENCE_LAYOUT) != multi_record:
        failures.append(
            f"引用位置表必须正好覆盖多 record 的 raw 行：表 {sorted(REFERENCE_LAYOUT)} / 多 record {multi_record}"
        )

    controls = run_negative_controls() + run_packed_negative_controls()
    for control in controls:
        if not control.startswith("OK"):
            failures.append(control)

    if failures:
        print("CHECK FAILED")
        for failure in failures:
            print(f"- {failure}")
        return 1
    print(
        f"CHECK OK：37 行金样与 {SCRIPT} 重新生成的结果逐字节一致；"
        "5 个 C# 地真行交叉校验通过（b.empty/b.null/b.chinese/b.long/i.nullable.none）"
    )
    for control in controls:
        print(f"  {control}")
    return 0


def tamper_slot(raw, index, depth=None, payload=None):
    """把第 `index` 个 16B 槽的 depth/payload 换成给定值（负控用；其它字节不动）。"""
    (main_len,) = struct.unpack("<i", raw[:4])
    position = 8 + main_len + index * 16
    slot_depth, slot_tag, slot_payload = struct.unpack("<iiQ", raw[position : position + 16])
    return (
        raw[:position]
        + i32(slot_depth if depth is None else depth)
        + i32(slot_tag)
        + (slot_payload if payload is None else payload).to_bytes(8, "little")
        + raw[position + 16 :]
    )


def run_negative_controls():
    """四条负控：坏字节必须被 `derive()` 拒绝，证明校验读的是盘上字节而不是编码器状态。"""
    controls = [
        (
            "d.class",
            "改坏一个 record id 引用：d.class 根槽 payload 1 → 不存在的 9",
            tamper_slot(encode("d.class", RAW_CASES["d.class"], packed=False), 0, payload=9),
        ),
        (
            "e.nested",
            "把嵌套 depth 写成 1：e.nested 最内层槽块（LevelC）的 depth 3 → 1",
            tamper_slot(encode("e.nested", RAW_CASES["e.nested"], packed=False), 1, depth=1),
        ),
        (
            "e.nested",
            "污染引用高位：e.nested 根槽 0x3 → 0x00000001_00000003",
            tamper_slot(
                encode("e.nested", RAW_CASES["e.nested"], packed=False),
                0,
                payload=0x00000001_00000003,
            ),
        ),
        (
            "g.shared",
            "改坏第二条共享边：g.shared Right 槽 1 → 不存在的 9",
            tamper_slot(encode("g.shared", RAW_CASES["g.shared"], packed=False), 3, payload=9),
        ),
    ]
    results = []
    for case, label, tampered in controls:
        try:
            derive(tampered, layout_of(case))
        except AssertionError as error:
            results.append(f"OK 负控被拒绝 | {label} | {error}")
        else:
            results.append(f"负控未生效（坏字节通过了 derive） | {label}")
    return results


def strict_unpack(packed):
    """按 FR-P02 严格解包：缺 tag 数据 / 缺 run 计数 / 缺 run body 直接失败；**普通 tag 显式携带的
    字节必须非零**（第 i 位表示该 word 第 i 字节非零；只查「置位所需字节存在」会放行 `13 8a 00 01`
    这种多置一位再显式写零的等价编码）。`ff` 的 8 字节属于原样续接正文，不做非零限制。
    """
    result = bytearray()
    pos = 0
    while pos < len(packed):
        tag = packed[pos]
        pos += 1
        word = bytearray(8)
        for bit in range(8):
            if not tag & (1 << bit):
                continue
            assert pos < len(packed), f"packed 流缺 tag 对应的数据字节（tag 0x{tag:02x}）"
            value = packed[pos]
            assert tag == 0xFF or value != 0, (
                f"FR-P02：tag 0x{tag:02x} 的第 {bit} 位表示该字节非零，"
                f"但显式写入了 0x00（偏移 {pos}）"
            )
            word[bit] = value
            pos += 1
        result.extend(word)
        if tag in (0, 0xFF):
            assert pos < len(packed), f"packed 流缺 run 计数（tag 0x{tag:02x}）"
            extra = packed[pos]
            pos += 1
            if tag == 0:
                result.extend(bytes(8 * extra))
            else:
                end = pos + 8 * extra
                assert end <= len(packed), "packed 流的 ff run body 截断"
                result.extend(packed[pos:end])
                pos = end
    assert len(result) % 8 == 0, "解包长度必须是 8B 的倍数（FR-P03）"
    return bytes(result)


def run_packed_negative_controls():
    """packed 侧负控：`b.long` packed 的位图显式零字节反例必须被 `strict_unpack()` 拒绝。

    反例：首个 tag `11`（置位 byte0/byte4）改成 `13`（多置 bit1）并显式写 0，即
    `11 8a 01` → `13 8a 00 01`；解包结果与金样完全相同，所以只查「置位所需字节存在」会放行。
    同时用 `strict_unpack()` 校验参考实现的全部 packed 行（FR-P02 的位图非零规则）。
    """
    golden = encode("b.long", LONG_TEXT, packed=True)
    assert golden[:3] == b"\x11\x8a\x01", (
        f"反例前提：b.long packed 必须以 tag 0x11 + 两个显式字节开头，实际 {golden[:3].hex()}"
    )
    assert unpack(golden)[1] == 0, "反例前提：金样第一个 word 的第 1 字节为 0"
    tampered = b"\x13\x8a\x00\x01" + golden[3:]
    assert tampered != golden, "反例前提：变异后的 packed 字节必须与金样不同"
    assert unpack(tampered) == unpack(golden), (
        "反例前提：变异后的解包结果必须与金样相同（否则不是「位图语义漏检」反例）"
    )
    for case, value in PACKED_CASES.items():
        strict_unpack(encode(case, value, packed=True))
    results = []
    try:
        strict_unpack(tampered)
    except AssertionError as error:
        results.append(
            f"OK 负控被拒绝 | packed 位图显式零字节：b.long 首 tag 0x11 → 0x13 并显式写 0x00 | {error}"
        )
    else:
        results.append("负控未生效（显式零字节的位图通过了 strict_unpack） | packed 位图显式零字节")
    return results


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
