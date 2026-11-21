# ace-serialize 架构与格式设计

- 状态：设计文档（现行）；ABI 版本 `0x00010000`，wire 版本 `1`；文档版本 1.0，2026-11-21。
- 范围：本文只写结构与格式（wire、schema、derive 接口、错误位置、C ABI 边界）；行为契约（FR/NFR/AC）与实现/验证计划不在本文范围内，二者冲突时以行为契约为准，本文不重复裁定。
- 术语：**slot／槽**＝16 字节定长单元 `i32 depth + i32 tag + 8B payload`；**record／记录**＝main 区 `i32 id + i32 size + body` 块；**envelope／信封**＝`[M][main][A][assist]` 双区 framing；**identity／身份**＝一次调用内同一个共享对象的编号；**arena／临时区**＝`scratch` 内身份槽之后的单调分配字节区。

## 1 目标与取舍

设计目标（按优先级）：

1. **单遍**：编码一次遍历值图即产出全部字节；解码一次读取即物化，不为格式再做一遍遍历（图解码的两阶段是构造需要，见 §6）。
2. **紧凑**：字段槽 16 字节，零值与缺席不占槽，`packed` 可整块压缩零字节。
3. **可零分配**：写入调用方缓冲的入口在库与 derive 生成代码路径上不堆分配，工作状态全部落在调用方 `scratch`。
4. **`no_std`**：核心 crate 只依赖 `core` 与 `alloc`；`std` feature 只增加 `Arc`/`sync::Weak`/`RwLock` 等便利设施。
5. **C ABI**：同一份格式与语义经 `cdylib` 暴露给 C 与其它语言，不要求运行期反射。
6. **共享引用**：`Rc`/`Arc` 的共享 DAG 按真实对象身份复用编号；环由 `Weak` 反向边表达。

明确的取舍：

- **wire 是自有格式**：固定小端、双区、对象槽范围、`u32` 计数、标准 word packing。不与其它序列化实现保证字节兼容；与外部字节流的双向兼容不在格式承诺内。
- **不引入序列化框架**：布局与演进模型是本库的一部分（order/tag、双区、预算、未知槽机械跳过、身份拓扑、图事务），通用框架的抽象与错误语义都不承载这些约束。
- **用标准库与 alloc 已足够**：`BTreeMap` 提供确定键序（哈希容器遍历序不确定，不能进 wire）、`Rc`/`Arc`/`Weak` 提供身份与反向边、`Vec` 提供缓冲。
- **类型不进 wire**：不写 magic、版本、checksum、根类型名。调用方必须提供正确的字段序（schema）与压缩模式；格式无法自识别。
- **身份只按地址在一次调用内查询**：对象地址用于同次 `Rc`/`Arc` 查找，绝不写入 wire，也不用内容哈希判定同一对象（相等但不同实例必须保留不同身份）。
- **不重访值图**：为测容量而二次编码会二次执行手写回调（读 `weak_count`、写 `Cell`、临时分配），改变可观察行为；容量探测改为在一次真实调用内报告失败点下界（§5.1）。
- **不为身份预建整栈结构**：栈占用会随限额增长，而栈耗尽表现为 abort，不是 `Result`。

零分配责任边界：库自身实现与 derive 生成代码保证；手写 `impl AceEncode`/`impl AceDecode` 的分配与副作用由该实现自负，库不代其承诺。

## 2 整体结构

三个 crate 的职责边界：

| crate | 职责 | 编译面 |
|---|---|---|
| `ace-serialize` | wire 读写、编码器与解码器、schema trait 与内建实现、限额与错误码 | `no_std` + `alloc`；`std` feature 增补 |
| `ace-serialize-derive` | `#[derive(AceSerialize)]` 生成 trait 实现、编译期校验属性 | proc-macro，宿主 `std` |
| `ace-serialize-ffi` | 冻结 C 头声明的导出面：动态值、类型引用、schema、options、编解码 | `std`，`cdylib` |

依赖方向单向：derive 只生成对核心公共路径的调用（编译期不依赖核心实现），ffi 依赖核心并启用其 `std` feature，核心不依赖任何一方。

`std` 与 `no_std` 的分界：

- 核心默认无 `std`；`Arc`、`alloc::sync::Weak`、`std::sync::RwLock` 的实现与 FFI 需要的便利项在 `feature = "std"` 下编译。
- 身份条目的 `Guard` 枚举含 `std` 分支，因此每条目字节数 `S` 与对齐 `A` 随 feature 变化——这正是 `S`/`A` 用函数发布而非常量发布的原因（§5.2）。

公开面与非公开面：

- 公共：`encode`/`encode_into`/`encode_into_probe`/`decode`/`decode_from`、`identity_entry_size`/`identity_entry_align`、`Encoder`/`Decoder`、schema trait 与内建类型实现、`Options`、`Error`。
- `#[doc(hidden)]` 接缝：给 derive 与 ffi 适配器复用同一套格式语义的入口——`Encoder::encode_collection`（按集合记录形态写一条记录并返回 id）、`Encoder::check_field_consumed`（字段编码后校验字段选择已消费）、`Decoder::enter_collection_checked`/`leave_collection`（集合层计数）、`Decoder::record_body_len`（只读体长，用于在收费前判定普通体与多态体）、`AceElement` 的 bulk 钩子与 `decode_element_into`。
- 这些接缝存在的理由是一条约束：跨 crate 的适配器不得复制 wire 逻辑。它们不是普通调用方的 API，不承诺稳定，也不替代内建类型实现。

模块划分：

| 模块 | 职责 |
|---|---|
| `wire/envelope` | 双区 framing 的读写与长度/预算校验 |
| `wire/record` | main 平铺记录的边界索引（id → 区间），不解释 body |
| `wire/slot` | assist 16B 槽流；对象槽范围的机械校验 |
| `wire/packed` | 标准 word packing/unpacking，独立于 envelope |
| `encode/mod` | 单遍编码器：对象帧、字段槽、身份表、记录提交、`finish` 组装 |
| `encode/typed_scratch` | `scratch` 字节区与类型化身份槽的互转（全仓唯一新增 `unsafe` 的路径） |
| `encode/scratch` | `encode_into` 的 arena 承载与偏移/长度描述符 |
| `decode/mod` | 单遍解码器：envelope/packed 校验、记录视图、字段游标、对象与集合进入 |
| `decode/graph` | 两阶段图解码：身份规划、构造调度、`new_cyclic` 构造 |
| `schema/mod` | trait 定义，标量/字符串/容器/图/宽值的编解码实现，bulk 快路径 |
| `schema/value` | 宽值类型（`Guid`、`DateTime`、`DateTimeOffset`） |
| `limits`/`error`/`hooks` | 限额与选项、稳定错误码、测试计数钩子 |

属性解析复用已装的 `proc-macro2`/`quote`/`syn`，不手写 token 解析。

实现位置：`crates/ace-serialize/src/lib.rs`、`src/{wire,encode,decode,schema}/`、`crates/ace-serialize-derive/src/{lib,attr,field,gen}.rs`、`crates/ace-serialize-ffi/src/lib.rs`。

## 3 wire 格式

### 3.1 envelope（信封）

`[i32 LE M][main M 字节][i32 LE A][assist A 字节]`。

- `M`/`A` 为非负字节长度，不含自身 4 字节前缀；raw 总长严格为 `M + A + 8`。
- 读端要求 envelope 恰好耗尽输入：尾部多余字节 → `2 TruncatedInput`；长度字段为负或越界 → `2`；raw 字节数超 `max_raw_bytes` → `6`。
- 结构上没有 magic、版本、checksum、根类型头。
- `main` 放全部 record，`assist` 放全部槽；两块不重叠，字段归属只由对象槽范围决定。

### 3.2 raw 与 packed

- `Options.packed` 显式选择格式，两端必须一致；解码绝不猜测。
- `packed` 是标准 word packing：每个 8 字节 word 先写 1 字节位掩码（bit i 表示第 i 个字节非零），随后按序只写非零字节；掩码 `0x00`（连续全零 word）与 `0xff`（连续全非零 word）后跟 1 字节 run 长度 N，`0xff` 再跟 N 个原样 word。
- 编码前把 raw 补零到 8 字节边界，补零计入 `max_raw_bytes`。
- 解码先展开成 raw，再按 `M`/`A` 算出逻辑长度：只接受逻辑末尾之后对齐所需的 0..7 个零字节，其余尾随数据 → `2`。packing 层无法区分 padding 与其他尾随数据，所以由 envelope 层收口。
- 选择：`raw` 长度可预测、可直接映射到固定缓冲、便于逐字节核对；`packed` 对大量零或低熵结构更快更小，但需要一块与 raw 等长的展开缓冲，且展开前受 `max_input_bytes`、展开后受 `max_raw_bytes` 双重约束。

### 3.3 record（记录）

main 平铺：每条 `[i32 inst_id][i32 size][body size 字节]`。

- `inst_id` 为消息内正整数：0 与负数不是有效记录，重复 id → `7 InvalidReference`；`size` 为非负 body 字节数，负数 → `2`。
- id 不要求顺序排列，记录边界只由 `size` 切分。未知 id 的记录按 `size` 整块跳过：不解析 body，不参与对象槽范围校验，也不能触发类型解析。

### 3.4 slot（槽）与对象

assist 全部由 16 字节槽组成：`[i32 depth][i32 tag][8B payload]`，长度必须是 16 的倍数（否则 `2`）。

- `assist[0]` 是根槽，`depth = 0`、`tag = 0`，不属于任何对象范围；根为 `None` 时没有根槽，assist 长度为 0。
- 普通对象记录 body 为 `[u32 slot_offset][u32 slot_count]`（多态对象体头见 §3.5）。二者以槽为单位，指向 assist 的连续区间 `[offset, offset+count)`。
- `slot_offset ≥ 1`（0 是根槽，报 `4 InvalidTag`）；区间不得重叠，且按记录在 main 中出现的次序单调递增（越界与乱序 → `4`）。
- `depth` 只作一致性校验（对象字段槽深度为进入前的 open 加一），不是字段归属依据；归属只由槽范围决定。
- 字段 `tag` 即稳定字段编号 order，同一对象内严格递增；缺口即表示缺席（§3.6）。
- 未被请求的槽与未知块在解码时可以直接不读：进入对象时已完成范围校验，不读不构成错误。

### 3.5 多态对象体头

对象 body 之前可带 `[u16 variant_tag]`；`variant_tag == 65535` 时再跟 `[u32 type_name_record_id]`。

- 数值 tag 范围 `0..=65534`，不写类型名。
- 逃生口 65535 必须给出显式别名，别名是一条普通字符串记录。
- 解码：别名记录缺失或越界 → `7`；内容非法 UTF-8 → `8`；合法但无匹配别名 → `4`。

### 3.6 字段槽的 payload 与省略规则

payload 的 8 字节只从类型自身位值构造，不读相邻内存：

| 类型 | payload |
|---|---|
| 无符号整数、`bool`、`char` | 自然位值小端零扩展到 8B |
| 有符号整数 | 自然位值小端符号扩展到 8B |
| `f32` | IEEE-754 低 4 字节位模式 + 高 4 字节零 |
| `f64` | 原 8 字节位模式，不规范化 NaN |
| 引用类（对象、字符串、bytes、集合、`Box`、`Rc`/`Arc`、多态对象） | 目标 record id，8B 零扩展 |

可省略字段只有三类，其余一律写槽：

1. 位模式全零的零值标量（`-0.0` 不是全零，必须写）；
2. `Option::None`（只消耗字段编号，不写槽）；
3. 标了 `#[ace(default = Variant)]` 且正取该变体的整数枚举。

缺席不写槽但保留编号空洞，解码靠 tag 缺口识别，不用字段总数推断。字符串、bytes、集合、`Box`、`Rc`/`Arc`、嵌套结构体与未标默认变体的枚举不可省略：空串、空集合、全零嵌套对象也必须写槽与 record。不可省略字段缺槽 → `15 MissingRequiredField`；`Some(v)` 必写，含 `Some(0)`、`Some(false)`、`Some("")`。

### 3.7 值表示与集合编码

- **字符串与 bytes**：record body 就是原始字节，`size` 即字节长，无终止符、无内部长度；空串是 `size = 0` 的记录。字符串必须是 UTF-8（非法 → `8`），内嵌 NUL 是普通字符并完整保留。
- **集合**：record body 为 `[u32 count][元素…]`。元素没有 depth/tag 头，位置就是顺序。
  - 元素体用自然宽度：`bool`/`i8`/`u8` 1B，`i16`/`u16` 2B，`i32`/`u32`/`f32`/`char` 4B，`i64`/`u64`/`f64` 8B；引用类元素的元素体是 `u32` record id。
  - `Option` 元素在元素体前加 1 字节 presence：0 为 `None`（无后续体），1 为 `Some`（后接内层体），其它值 → `4`。
  - 空集合是 `count = 0`、body 长 4 的记录，与 `None` 不同。
  - `count` 必须在 `u32` 与 `max_collection_len` 内，且在写任何元素之前判定。
- **Map／`BTreeMap`**：body 为 `[u32 count][key][value]…`，`count` 是键值对数。键按类型自身的 `Ord` 升序编码；解码按键在线上出现的顺序读取并增量查重，同一键第二次出现 → `16 DuplicateKey`（不得后者覆盖，不得静默成功）。
- **定宽宽值**：`Guid` 16B（RFC 4122 网络字节序）；`DateTime` 为 `u64`（低 62 位 ticks + 高 2 位 Kind，`0/1/2` 合法，`3` 解码报 `4`）；`DateTimeOffset` 为 `i64` ticks + `i16` 分钟，共 10B；`IntPtr`/`UIntPtr` 固定 8B，不随平台指针宽度。超过 8 字节的宽值走独立 record（根或字段 payload 放 record id），不进字段槽。

### 3.8 根

根恒用 `assist[0]`：

- `≤ 8B` 标量根直接放数值槽，零值也必须有根槽。
- 引用类根 payload 放根 record id（id 从 1 起）。
- `None` 根没有根槽、assist 长度 0；`Some(true)` 是 16 字节 assist 且 payload 为 1。
- `> 8B` 定宽值根走 record，payload 放该 record 的 id。

实现位置：`src/wire/envelope.rs`、`src/wire/record.rs`、`src/wire/slot.rs`、`src/wire/packed.rs`、`src/encode/mod.rs`（对象体、体头、`finish` 回填）、`src/schema/mod.rs`（标量与集合）、`src/decode/mod.rs`（进入与校验）。

## 4 类型与 schema

### 4.1 四类 trait 的分工

| trait | 职责 | 谁实现 |
|---|---|---|
| `AceEncode` | 值编码：根位、字段位、元素位三个入口，另含 bulk 钩子 | 内建标量/容器/宽值、`#[derive]`、手写 |
| `AceDecode` | 单遍解码成新值；含 `ace_decode_into` 复用钩子（默认整体替换） | 同上 |
| `AceElement` | 集合元素体形态：`Value { size }`（定宽）、`Ref`（`u32` id）、`Variable`（自定长）；含 `decode_element_into` 与两条 bulk 钩子 | 同上 |
| `AceDecodeCyclic` | 两阶段图解码：`decode_plan`（阶段一）与 `build_from_plan`（阶段二） | 含 `Rc`/`Arc`/`Weak` 的类型 |
| `NonOptional` | 标记：允许出现在单个 `Option` 内 | 内建类型；`Option<Option<T>>` 在编译期被拒 |

准入不同源：只实现 `AceElement` 的类型可以进 `Vec<T>`，但不能进 `Rc<Vec<T>>`；图读回准入只看 `AceDecodeCyclic`。

### 4.2 derive 生成什么

`#[derive(AceSerialize)]` 按类型形状生成：

- **结构体（不含共享字段）**：`AceEncode`（进入对象 → 按 order 升序逐字段编码 → 离开对象）、`AceDecode`（含 `ace_decode_into`，逐字段写入既有实例）、`AceElement`（元素形态）、`AceDecodeCyclic`。
- **结构体（含 `Rc`/`Arc`/`Weak` 的 shared 类型）**：生成 `AceEncode`、`AceDecode`、`AceDecodeCyclic`、`NonOptional`；**不生成** `AceElement`。`ace_decode` 直接把解码器交给图路径，`ace_decode_into` 保持整体替换。
- **整数枚举**：`repr` 决定 wire 宽度，未标 `repr` 按 `i32`，判别值超出 `i32` 编译错；`#[ace(default = Variant)]` 让该变体在字段位可省略、缺席时恢复。
- **多态枚举（变体带载荷）**：按变体判别值写/读多态体头，再按该变体的字段编码；只支持 unit 与具名字段变体，不支持含 `Rc`/`Arc`/`Weak` 字段的变体。

生成代码只调用核心的公共路径与 `#[doc(hidden)]` 接缝，不复制格式逻辑；属性错误在编译期以 `ace: ` 前缀的诊断报告，并定位到字段或变体的 span。

实现位置：`crates/ace-serialize-derive/src/gen.rs`（`Shape::emit` 1001–1016 决定生成哪几份 impl，`encode_impl` 1044–1058，`non_optional_impl` 1257–1264）。

### 4.3 `#[ace(...)]` 属性

| 位置 | 属性 | 语义 |
|---|---|---|
| 字段 | `order = n` | 稳定字段编号；非负、可表示为 `i32`；同一对象内必须严格递增 |
| 字段 | `skip` | 不进 wire；构造与复用取字段类型的 `Default` |
| 类型 | `reserved(n, …)` | 声明保留编号空洞，用于演进，不写槽 |
| 类型（整数枚举） | `default = Variant` | 该变体在字段位可省略，缺席时恢复 |
| 变体（多态） | `tag = n` | 数值判别值，`0..=65534` |
| 变体（多态） | `type_name = "…"` | 走 65535 逃生口，写显式别名字符串 |

约束：属性位置固定（字段属性出现在类型上是错误，反之亦然）；同一属性不得重复；同一多态枚举内不得混用数值 tag 与 `type_name`；tag 与别名都不得重复。`shared` 不是属性，而由字段类型中是否出现 `Rc`/`Arc`/`Weak` 推断。

### 4.4 判别值与 type_key 分派

- **整数枚举**：`repr` 只接受 `i8`/`u8`/`i16`/`u16`/`i32`/`u32`/`i64`/`u64`，其它表示（`repr(C)`、`isize`/`usize`、transparent、align 等）编译错。判别值按位模式承载（不做数值换算），必须落在 repr 位宽内且不得重复；wire 写该宽度的位模式；解码遇到不在变体集合内的判别值 → `4 InvalidTag`。
- **多态枚举**：数值 tag 在同一 schema 内选择变体；`type_name` 逃生口把稳定字符串别名写进 record，供没有编译期类型信息的调用方分派。
- **type_key**：wire 不含根类型名。Rust 侧按泛型静态分派（`decode::<T>`）。C ABI 侧由调用方显式提供 `type_key` 并从 options 白名单查 schema：未注册（含 `NULL` options 的空白名单）→ `3`（fail-closed，全拒绝）；对象自带 key 与实参冲突 → `1`；多态逃生口把该节点自身的 `type_key` 写成别名。

实现位置：`src/schema/mod.rs`（trait 与内建实现）、`src/schema/value.rs`（宽值）、`crates/ace-serialize-derive/src/{attr,field,gen}.rs`。

## 5 编码流程

### 5.1 三个入口

| 入口 | 输出 | 工作区 |
|---|---|---|
| `encode(&T, &Options) -> Result<Vec<u8>>` | 拥有型字节缓冲 | 库内 `Vec`/`BTreeMap` |
| `encode_into(&T, dst, scratch, &Options) -> Result<usize>` | 调用方 `dst`，返回写入字节数 | 调用方 `scratch` |
| `encode_into_probe(…, need: &mut usize)` | 同上，另在 `scratch` 不足时写 `need` | 同上 |

`encode` 与 `encode_into` 走同一套编码逻辑，差别只在承载：拥有型路径用堆结构累积 main/assist，写完再组装 envelope；`encode_into` 中 `dst` 是唯一的 wire 目的地——`raw` 模式下 main 直接写 `dst[4..]`，`packed` 模式下 main 先落在 arena，`finish` 时逐 word 打包写 `dst`（不得先把完整 raw 写进 `dst`）。成功返回值等于最终字节长度。

`encode_into_probe` 与 `encode_into` 是同一条真实编码路径（不重跑回调，不新增失败面），额外行为只有一条：**仅当**本次首错是真实的 `scratch` 不足、且本次调用没有出现其它来源的失败时，把**同一 `scratch` 起址下容纳本次调用所需的总字节数**写进 `need`。

- `need` 是目标总长，不是「还差多少」，所以必然大于本次 `scratch.len()`。
- 它只是**本次失败点**的 checked 下界：按该次分配自身的字节数与其在对齐下的落位计算，既不是一次即得的精确最小值，也不是闭式充分上界。
- 重试必须用**同一起址**、长度不小于 `need` 的新缓冲，可能需要多次；换起址必须重算（对齐前导随起址变化）。
- `dst` 不足、非法输入、限额与图错误照常返回既有码，且不写 `need`。

### 5.2 `scratch` 布局

`scratch` 分两段：**身份槽区在前，字节 arena 在后**。

```
[对齐前导 P][N × S 条目区][arena 剩余字节]
```

- 起址向上对齐到 `A` 得到前导 `P = (A - 起址 % A) % A`（`0 <= P < A`），第 i 个槽落在 `起址 + P + i × S`。长度相同、起址不同的切片可用容量可能不同。
- 确实需要身份槽时按 `P + N × S + 字节区` 计容量；**零身份槽时既不扣也不加前导**。对齐余量不得转嫁到 `dst`。
- `S = size_of::<Entry>()`、`A = align_of::<Entry>()` 由 `identity_entry_size()`/`identity_entry_align()` 发布：`Entry` 含受 `std` 控制的 `Guard` 分支，`S`/`A` 随目标与 feature 变化，不能写死成常量。
- arena 是简单版：单调分配、不回收，容量等于 `scratch` 扣除条目区与对齐后的剩余字节；增长只能另申请一块再复制，旧块留在已占字节内。
- 条目区与 arena 相撞、arena 不足都映射既有 `1 InvalidArgument`，不新增错误码。

身份条目 `Entry` 的字段为 `family`、`address`、`target`、`id`、`state`、`guard`，布局已冻结（改变属破坏性变更）：

- `family + address` 只用于同次调用的身份查询（区分 `Rc`/`Arc` 指针族），绝不写 wire。
- `target`（`TypeId`）单独保存，因为目标析构后无法再从指针求类型。
- `state` 为 `Reserved`/`Active`/`Done`：`Weak` 先到只预留编号，强目标首次到达展开并沿用该编号，`Active` 期间再次强到达即强环（`14`）。
- `guard` 持 `Weak<dyn Any>` 保活分配，防止编码期间地址复用被误认成同一身份；其析构不执行用户代码。

### 5.3 组装顺序

1. 进入对象时压帧（深度为进入前 open 加一），字段槽先累积在帧内。
2. 离开对象时整块成为一条记录：对象体 8B（多态 10B 或 14B）加该帧的槽块。字段位的对象把自身 record id 写进父帧对应的字段槽，根对象把 id 写进根槽。
3. 记录头与对象体的槽范围只在能确定位置时回填，且回填只用局部 offset/索引，绝不跨缓冲扩容持有裸指针。
4. `finish` 按提交次序（子先于父）把各对象块接到根槽之后，槽号从 1 起连续分配，最后组装 envelope 并按需 `packed`。
5. `finish` 的收尾校验：存在未离开的对象帧、未结束的元素作用域、未填充的 `Weak` 预留（会输出悬空引用）、有对象块却没有根槽，均返回 `1`。

### 5.4 零分配与首错锁存

- 库自身与 derive 生成代码在 `encode_into` 路径上不堆分配：身份表与全部工作状态都在 `scratch` 内，`dst` 只承载最终 wire。
- 手写实现的分配自负；`AceElement` 的 bulk 钩子给手写实现一条可选零分配快路径（拿到恰好等于本次集合体长的切片）：**恰好写满必须成功**，只有放不下才返回 `1`，不得 panic、不得静默截断、不得谎报长度。
- 定宽整段拷贝走 bulk 快路径，只在「无 padding 且小端内存字节即 wire 字节」的类型上启用；内存布局与 wire 不一致的类型（如 `DateTime`、`DateTimeOffset`）必须逐元素转换。
- 首错锁存：调用方忽略的子调用错误仍由返回点返回首个错误，不被后续错误覆盖。容量归因只由**产生点**自己标注——公开错误码相同不代表同源，不靠比较 `Error` 值推断。

失败后 `dst` 内容不作承诺（可能已部分写入）；结构上的拒绝一律发生在分配记录或压槽之前，避免留下未被引用的记录。

实现位置：`src/lib.rs`（入口）、`src/encode/mod.rs`（帧、槽、对象、`finish`）、`src/encode/scratch.rs`、`src/encode/typed_scratch.rs`。

## 6 解码流程

### 6.1 单遍解码

`decode::<T>(src, &Options)` 分三步：

1. `Decoder::new` 做输入校验：输入字节数 → `packed` 展开 → envelope 读取（含尾随字节）→ 记录索引 → 槽流。任一步失败都在读值之前返回。
2. `T::ace_decode` 按 schema 读取：根（无值、数值、记录）、对象（进入 → 逐字段选择 → 读取 → 离开）、集合（进入 → `count` → 逐元素 → 离开）。
3. `Decoder::finish` 收尾：不得有未离开的对象帧；无根槽视为根为 `None`；有根槽时其 `depth`/`tag` 必须为 0。

读取语义要点：

- 字段选择严格递增：`begin_field(order)` 保存该字段的 payload（缺口即缺席），同一对象内不得回退。
- 记录视图按 LIFO 关闭；进入对象时按整个 record 体长计入物化预算。
- 未被请求的字段与未知块可以不读，进入对象时已完成范围与不重叠校验。

### 6.2 两阶段图解码

含共享或环的类型走 `AceDecodeCyclic`：

- **阶段一（规划）**：`decode_plan` 建立身份表与构造计划，不构造任何用户值。身份表条目状态为「预留」（`Weak` 请求）、「展开中」（`Rc`/`Arc` 首次强到达）或「已计划」；强边与弱边按 owner 归组，虚根 0 表示消息根。规划结束产出构造调度：事件序列与构造峰值。
- **阶段二（构造）**：`BuildCtx` 按事件运行 `Rc::new_cyclic`/`Arc::new_cyclic`，闭包内先登记自身 `Weak`，再构造子节点。
- 两阶段共享同一张身份表：规划结束时整表连同构造任务一起移交，不复制表，也不另存 ids 到 job 的旁表。
- 阶段二不可失败：阶段一已返回全部输入错误；阶段二遇到类型/族不符、任务重复取用等属内部协议违反，直接 panic（§9.4）。

### 6.3 身份、Weak 与强环

- 同一身份重复到达：已构造完成则复用同一节点（引用加一），不复制子树。
- **强环拒绝**：字段位再次强到达「展开中」的身份 → `14 CycleUnsupported`；环上含普通（非 Cyclic）字段的强连通分量同样 → `14`。拒绝发生在构造之前，因此不会先造出环再报错，也不需要事后打断。
- **`Weak` 回边**：不扩展强可达集合，也不延长生命周期。目标在本消息内强可达时，构造完成后 `downgrade`；或在目标自身的 `new_cyclic` 闭包内克隆已登记的 `Weak`（此时 `upgrade()` 返回 `None` 是合法中间态），全部构造完成后身份必须正确。仅当本消息内根本不存在该 id 的强目标时返回 `7 InvalidReference`。
- 构造结束后释放临时强缓存，避免「只有 `Weak` 的目标」被上下文隐式持有。

### 6.4 `decode_from` 的复用边界

`decode_from(src, &mut out, &Options)` 与 `decode` 走同一套校验，成功后只经 `AceDecode::ace_decode_into` 写入既有实例。复用只覆盖**实际到达复用路径且未被整体替换**的缓冲（责任边界在实现侧）：

- 复用：内建 `String`/`Vec`/`Option`/`RefCell`/`RwLock`、`BTreeMap`（节点与键重建，值在原位置复用）、非 shared 的 derive 结构体（逐字段写入）与同一变体的多态枚举载荷；`Vec<Vec<T>>` 的内层集合也因此保留分配。
- 整体替换（不复用）：未覆写复用钩子的手写实现、`Box` 等未覆写类型、shared 类型（走图路径，在消息根整体替换）、`Some` 变 `None`、跨变体切换、缺席字段恢复 `Default`。
- 失败态：返回 `Err` 时 `out` 的内容不作承诺（可能已被部分修改），调用方不得把它当作有效值。
- 单元格状态：内建 `RefCell`/`RwLock` 在**向该单元格写入之前**检查是否可用（存在未释放借用、锁中毒、被弃 guard → `1`），不修改该单元格，也不清除或规整其状态。粒度是单元格而非整个 `out`，前序字段可能已被更新；整体替换不算向该单元格写入，因此不触发该检查。

实现位置：`src/decode/mod.rs`（输入校验、对象/字段/集合、`finish`）、`src/decode/graph.rs`（规划、调度、构造、身份表）、`src/lib.rs`（`decode_from`）。

## 7 预算与限额

### 7.1 `Options` 字段

| 字段 | 含义 | 默认 |
|---|---|---|
| `max_depth` | 最大结构嵌套深度，根为 0 | 256 |
| `max_build_nesting` | 解码规划期同时打开的身份构造闭包峰值（不计虚根与普通对象/集合深度） | 256 |
| `max_input_bytes` | 输入层字节预算（`packed` 输入在展开前的字节数） | 64 MiB |
| `max_raw_bytes` | 展开后 raw 字节预算，含 envelope | 64 MiB |
| `max_decoded_bytes` | 物化预算：每次实际物化 record 累计其 `size` | 64 MiB |
| `max_records` | 记录条数上限 | 2^20 |
| `max_slots` | 槽数上限（含根槽） | 2^20 |
| `max_collection_len` | 单集合元素数或 Map 键值对数上限 | 2^24 |
| `max_references` | 身份/引用表条数上限 | 2^20 |
| `max_map_string_key_bytes` | Map 字符串键的 UTF-8 字节数上限 | 1 MiB |
| `packed` | 是否使用 `packed` 格式 | `true` |

零值合法，表示该维度最严格的上限（不是「禁用限制」）：深度 0 只允许根，槽数 0 不允许任何槽，身份表 0 不允许身份节点。

### 7.2 检查位置概览（非全局优先级）

下面列出各类检查**发生的位置概览，不是全局优先级**：实际先返回哪条错误取决于入口、输入长度与解析进度，同一份字节在不同入口可能先触到不同的检查。

1. **输入层**：输入字节数超 `max_input_bytes` → `6`。
2. **解包**：展开总长超 `max_raw_bytes` 或 `isize::MAX` → `6`；每个单元自身的截断 → `2`。先无分配扫描算总长，再精确预留。
3. **envelope**：raw 超 `max_raw_bytes` → `6`；长度非法或尾随字节 → `2`。
4. **记录索引**（`wire/record.rs:39–52`）：逐条扫描时先查 `size` 与 body 边界（负数或越界 → `2`），再查 id（`≤ 0` 或重复 → `7`），最后查记录预算（超 `max_records` → `6`）。
5. **槽流**：长度非 16 的倍数 → `2`；槽数超 `max_slots` → `6`。
6. **深度**：进入对象或集合前比较当前 open 与 `max_depth`，超限 → `5`（编码与解码两侧一致）。
7. **集合长度**：编码在**写 `count` 之前**检查 `max_collection_len` 与 `u32` 表示 → `6`（`schema/mod.rs:1817`、`encode/mod.rs:1336`）；解码必须**先读 `count`、再检查、后才读元素**（`schema/mod.rs:1726`、`2187`、`2220`）。
8. **对象进入**：先物化收费（见第 9 条），再校验对象槽范围（offset 为 0、越界、重叠、乱序 → `4`；乘加溢出 → `6`）（`decode/mod.rs:393–398`）。
9. **物化**：每次实际物化 record 时累计其体长，超 `max_decoded_bytes` → `6`；共享身份命中已构造节点不进入收费点。
10. **身份表**：插入前比较 `max_references` → `6`。
11. **Map 字符串键**：复制键字节之前检查 `max_map_string_key_bytes` → `6`。
12. **身份构造峰值**：规划结束时比较 `max_build_nesting` → `6`。

### 7.3 错误码

稳定闭集 `0..16`，只能末尾追加，已发布码值的编号与语义不变（`0` 表示成功，不占 `Error` 枚举）：

| 码 | 名称 | 一句语义 |
|---|---|---|
| 1 | `InvalidArgument` | 无效参数、冲突 key、非 Option 位置的失效 `Weak`、活动可变借用/锁占用/poison、容量不足 |
| 2 | `TruncatedInput` | 头、槽、记录、packed body 或 run 计数截断 |
| 3 | `UnknownTypeKey` | 未注册 schema key |
| 4 | `InvalidTag` | 非法已知判别、槽范围重叠或逆序、非法 presence 等结构标记 |
| 5 | `DepthExceeded` | 结构嵌套超配置深度 |
| 6 | `ResourceLimit` | 输入、展开 raw、物化、record/槽数量、集合、引用、key 等上限或不可表示长度 |
| 7 | `InvalidReference` | 缺失、重复、非法 id 或引用越界（不保证检出 schema 类型不符） |
| 8 | `InvalidUtf8` | 非法 UTF-8 或非法 Unicode 标量 |
| 9 | `VersionMismatch` | ABI 版本不匹配；不用于 wire 版本探测 |
| 10 | `PanicCaught` | unwind 边界兜底 |
| 11 | `Unsupported` | 非本期支持能力 |
| 12 | `NotFound` | field/list 查询无此项 |
| 13 | `TypeMismatch` | 动态值无法无损转换到 schema 类型 |
| 14 | `CycleUnsupported` | 强引用环等不可安全填充的循环 |
| 15 | `MissingRequiredField` | 不可省略字段的槽缺失 |
| 16 | `DuplicateKey` | Map 解码遇同一 key 第二次出现 |

`max_build_nesting` 与 `max_depth` 是两个不同限额，不得合并表述：前者只对 **Rust 图解码的规划期身份构造峰值**生效、超限 `6`；后者是结构深度、超限 `5`。C ABI 的 `ace_deserialize` 不消费 `max_build_nesting`（C 侧只承诺树与共享 DAG，不构造强环，不存在身份构造闭包峰值），其结构深度仍由 `max_depth` 约束。

实现位置：`src/limits.rs`、`src/error.rs`、`src/wire/`（各层预算）、`src/decode/graph.rs:506–510`（身份表）、`src/decode/graph.rs:1060–1062`（构造峰值）。

## 8 C ABI 设计

### 8.1 句柄与所有权

| 类型 | 形态 | 释放 |
|---|---|---|
| `AceValue` | 不透明指针，即 `Arc` 原始指针化的句柄 | `ace_value_free`：每次减一个引用，归零才释放子树 |
| `AceOptions`/`AceSchema`/`AceEnumSchema`/`AceTypeRef` | 不透明拥有型句柄 | 各自的 `*_free`，只释放一次 |
| `AceBuffer` | `{ptr, len, cap}`，由库维护 | `ace_buffer_free` |

- 构造成功即移交所有权；失败时 `*out` 置空。`out` 为 `NULL` → `1`，且不访问输出。
- **借用视图**（`ace_object_get`、`ace_list_at`、`ace_value_get_str`/`get_bytes`）不持引用：寿命不超过父值，父值修改或释放后失效，不得 `free`，不得被 `set`/`add` 收编。
- `field_id` 即 wire 的 order；保留、缺席、未知字段与越界下标都返回 `12 NotFound`。

### 8.2 引用计数与共享

- `ace_object_set`/`ace_list_add` 成功即父容器对该 child 增加一个引用：**调用方句柄仍然有效**，须由调用方自己 `ace_value_free`（不 free 即泄漏）；失败不增加引用，child 仍归调用方。
- 替换同一 `field_id` 时库对旧值减一次父引用；失效只及该父返回的借用视图，调用方自有的拥有或共享引用仍可读、可 share、可 free。
- `ace_value_share(value, out)` 让 `out` 指向同一节点并把该节点引用计数加一，这是表达「两个字段是同一对象」的唯一方式。失败三分类：`out` 为 `NULL` → `1` 且不访问输出；`out` 合法但参数可判定无效 → `1` 且 `*out` 置空；伪造、悬垂或非本库分配的句柄不新增安全保证。
- 身份只承诺**对象与引用类容器**（list/map/str/bytes）；标量与大于 8B 的宽值 record 不承诺节点身份。身份范围仅限一次消息或一次反序列化，调用方不得跨调用缓存指针。

### 8.3 schema 与白名单

- `AceTypeRef` 是可组合类型节点：`list`/`map`/`option` 成功即**收编** child（child 句柄失效、归父所有、不得再 free），失败不收编；未被任何 schema 收编的顶层节点由 `ace_type_free` 释放。
- `ace_type_object`/`ace_type_enum` 对 `AceSchema`/`AceEnumSchema` 取**借用**（不收编），被引用实体必须在构造与 `add_field` 期间存活。
- `ace_schema_add_field` 成功即收编字段类型节点；`field_id` 即 order，重复 → `1`。
- `ace_options_add_schema` **深拷贝**整张 schema 图（含收编的类型节点与被借用的枚举 schema）进库：复制完成后调用方仍可 free 全部调用方侧结构，不影响库内副本；重复 `type_key` → `1`。
- 释放顺序：先 free 引用方 schema，再 free 被引用实体。
- `NULL options` 表示默认限额加空白名单，因此任何 `type_key` 都查不到 → `3`（fail-closed）。

### 8.4 编解码入口

- `ace_serialize(type_key, value, options, out)`：成功交付一段 `Box<[u8]>`（`cap == len`，由 `ace_buffer_free` 归还）；失败（含 panic）后 `*out` 一律是空缓冲。错误分派：参数或 key 冲突 → `1`；未注册 key → `3`；强环 → `14`（先于任何写入判定）；kind 不匹配或整数越界 → `13`；无自带 `type_key` 而位置 schema 的 key 已注册成另一套布局 → `13`（fail-closed）；缺必写字段 → `15`；值里带 schema 未声明的 `field_id` → `12`；Map 列表重复键 → `16`；枚举判别值不在变体集 → `4`；核心编码器的错误（`5`/`6`/`10` 等）原样透传。
- `ace_deserialize(type_key, buf, len, options, out)`：按已注册 schema 图把 wire 物化成动态值图。身份表在本次调用内建立、调用结束即丢弃，所以两次结果之间不共享节点。
- 浮点：`ace_value_f64_bits` 以位模式入参，不做宿主浮点运算；`F64` 转 `f32` 只在可精确定义时成功（±0、±inf、可精确表示的有限值、fraction 低 29 位为零的 NaN），其余 `13`；`f32` 转 `F64` 恒无损。
- 所有入口都经同一骨架包住 `catch_unwind`：panic 映射为 `10 PanicCaught`，不跨 FFI 边界展开。入口在写任何输出之前先清空，失败路径不留半成品。

### 8.5 ABI 稳定性

- 头文件声明的导出面与实现一一对应（53 个符号），头是该面的唯一权威；实现不新增、不改名、不改签名。
- 错误码 `0..16` 是闭集，只能末尾追加；已发布码值的编号与语义不变。
- `ACE_ABI_VERSION`/`ACE_WIRE_VERSION` 只用于查询与协商，不写入 wire，也无法从字节识别。
- `ace_identity_entry_size`/`ace_identity_entry_align` 随目标与 feature 发布 `S`/`A`；改变 `Entry` 布局属破坏性变更。
- `AceBuffer` 是唯一承诺「free 后清零、同一已清零结构二次 free 安全」的句柄；其它句柄按「只释放一次」使用。

实现位置：`include/ace_serialize.h`（契约）、`crates/ace-serialize-ffi/src/lib.rs`（`guard`、`construct`、`share_handle`、编解码适配器）。

## 9 安全边界

### 9.1 允许的 `unsafe` 面

只有三处，每处都带不变量：

1. **`src/encode/typed_scratch.rs`：`&mut [u8]` 字节区与类型化身份槽的互转。** 这是全仓唯一允许新增 `unsafe` 的路径。`Entry`/`EntryStore` 是模块私有类型，不出现在任何公共类型里；模块外只经 safe 入口 `with_store` 访问，`unsafe fn` 一律不带任何可见性修饰。承载是 `MaybeUninit<u8>` 区（已初始化槽的 padding 可能未初始化，普通 `&mut [u8]` 无法承载），槽地址一律由承载指针按 checked 偏移派生。
2. **`src/schema/mod.rs`：定宽整段拷贝快路径。** 仅在小端目标编译，前置条件逐条写在 `SAFETY` 注释里：类型无 padding 且小端内存字节即 wire 字节；`validate` 拒绝该类型的全部非法位模式；拷贝前先按 `count` 做边界检查；目标容量由 `reserve_exact` 保证；目标先清空，`set_len` 在拷贝之后。
3. **`crates/ace-serialize-ffi/src/lib.rs`：FFI 边界的原始指针。** 引用计数部分：`Arc::into_raw` 与 `Arc::from_raw` 成对出现，`increment_strong_count` 只对仍存活的句柄使用；`UnsafeCell` 只提供 body 的内部可变性，顺序固定为**先完成引用计数操作，再进入 body 变更**——否则计数操作建立的共享借用会覆盖刚建立的 `&mut`。同一文件内其它指针责任同样由头契约约束、逐处写 `SAFETY`：
   - **原始输入切片**：`copy_input` 复制调用方字节、`input_slice` 零拷贝借用（`ace_deserialize` 用）；`ptr` 可 `NULL` 仅当 `len == 0`，借用期间调用方保活且不改写该段，切片不跨本次调用（`lib.rs:210–244`）。
   - **C 字符串**：`read_type_key` 要求调用方维护的 NUL 结尾 UTF-8 串；复制进库后调用方即可释放原串，非 UTF-8 → `8`（`lib.rs:246–256`）。
   - **`Box` 句柄**：`ace_type_free`/`ace_schema_free` 的 `Box::from_raw` 只对未收编、且只释放一次的句柄使用；`construct_handle` 失败时把 `*out` 置空、成功即移交所有权（`lib.rs:156–171`、`979–1019`）。
   - **输出指针**：`out` 非空才写；失败路径保持 `*out` 为空，不以半成品返回。

### 9.2 不变量

- `EntryStore` 是三态机：`Bytes`（`initialized == 0`，整片可作字节）、`Typed`（前缀内有活条目）、`Cleaning`（清理中间态，不借出任何视图）。
- 终止清理只有一条顺序：结束所有视图 → 进入 `Cleaning` → 逐项析构前缀 → 用裸指针把曾被类型化的整段恢复为有效字节 → 在同一 `Cleaning` 内把 arena 占用归零 → 转回 `Bytes`。构造与析构都封在模块内，外部拿不到拥有值，也无法用 `mem::forget` 跳过清理。
- arena 只用 offset 与长度定位，不持引用、不回收；增长只能另申请一块再复制。
- 编码器的对象帧、字段槽与记录提交不跨缓冲扩容持有裸指针；回填只用局部偏移或索引。
- 解码状态全部保存在 `Decoder`/`BuildCtx` 实例内：无 thread-local、无全局缓存，失败即丢弃实例。

### 9.3 禁止清单

- 从普通 `&mut [u8]` 直接构造含 padding 的 `Entry`，或把未初始化前缀当 `Entry` 读。
- 在共享借用存续期间建立整节点的 `&mut AceValue`，或把 body 变更放在引用计数操作之前。
- 跨缓冲扩容持有裸指针回填记录头或槽范围。
- 把地址、指针、内容哈希写入 wire，或跨调用缓存句柄与指针。
- 对已收编的类型节点、借用视图、已释放句柄再次 free。
- 用 `debug_assert` 代替拒绝：release 下不生效的断言不能作为校验手段。
- 让 panic 跨 FFI 边界展开，或在失败路径留下半成品输出。

### 9.4 内部协议违反

阶段二的类型或指针族不符、构造任务重复取用、身份重复登记等属库内不变量违反（阶段一已返回全部输入错误），一律 panic，不折算成错误码。

实现位置：`src/encode/typed_scratch.rs`（模块头的不变量说明）、`src/schema/mod.rs`（拷贝契约的 `SAFETY`）、`crates/ace-serialize-ffi/src/lib.rs`（句柄与指针）、`src/decode/graph.rs`（构造期断言）。

## 10 已知限制与未验证项

行为边界（设计如此，不是缺陷）：

- **强环只能表达，不能构造**：环必须由 `Weak` 反向边构成。字段位再次强到达「展开中」的身份 → `14`；C 侧把对象自身或祖先 `set`/`add` 进容器是调用方使用禁令（不是未定义行为），`ace_serialize` 仍返回 `14`，且 `ace_value_free` 对强环按引用计数语义**不保证释放**。
- **C ABI 无 `Weak`**（`11 Unsupported`），无数值多态变体（只走 `type_name` 逃生口），也不提供跨调用或跨线程的身份缓存：共享身份只在一次消息或一次反序列化内成立。
- **无自带 `type_key` 且位置 schema 的 key 已注册成另一套布局 → `13`**：宁可拒绝，也不按错误布局解释字节。
- **C 侧需自行封装**：ABI 无泛型、无反射；Map 以 `List<Object{k,v}>` 表达；schema 必须由调用方逐字段声明并注册。
- **`decode_from` 对图路径不复用**：shared 类型走两阶段解码并在消息根整体替换，图内已构造缓冲不承诺复用。
- **`packed` 需要整块展开缓冲**：解包不是流式的，峰值同时受 `max_input_bytes` 与 `max_raw_bytes` 约束。
- **`scratch` 容量不能预先精确求得**：`S`/`A` 与起址对齐都影响可用容量，只有失败点下界（`need`），没有精确最小值，也没有闭式充分上界。

未验证与未覆盖：

- `unsafe` 面只对代表路径（身份槽的初始化与清理、整段拷贝、FFI 引用计数）做过动态检查，不构成全路径证明。
- 只在开发机目标实测；其它目标（尤其大端平台）未验证——大端下整段拷贝快路径不编译，全部走逐元素字节转换。
- 性能结论只覆盖已测样本，不外推到未测的类型形状或其它平台。
- 输入端存在一个已知不可区分点：手写编码回调自行构造、且不经过库标注点的同码 `Err`，与真实的 `scratch` 不足在公开 `Result` 上无法区分；归因只来自库内部保留到返回点的私有诊断。
