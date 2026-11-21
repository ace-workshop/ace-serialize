# ace-serialize 入门指南（面向 C# / LSerialize 开发者）

> 本文是**使用向**说明文档，回答三个问题：这个库是做什么的、怎么用、以及它和你熟悉的 C# `Pinwheel.LSerialize` 到底哪里一样、哪里不一样。
> 行为契约的权威在 [`requirements.md`](requirements.md)，系统结构在 [`architecture.md`](architecture.md)；本文只做导航与对照，不重定义契约。
> 说明：§2.2 / §4 / §7 / §9 / §10 的示例、以及 §3 的字节 dump，都在本仓库实际编译并跑过往返验证；其余片段（§8.3、§13、§16）是示意片段，省略了上下文变量、`use` 与错误处理，不能逐字复制编译。

---

## 0. 先看结论

| 问题 | 答案 |
|---|---|
| 这是什么 | C# 库 `Pinwheel.LSerialize` 的 Rust 重写：**标签驱动的二进制序列化器**，靠代码生成（C# 源生成器 / Rust `derive`）为每个类型生成成对的编码/解码函数 |
| 和 JSON/MessagePack 的区别 | 不带字段名、不带类型名、不带版本头；字段用**数字 tag**（就是 `order`）定位，字节数极小 |
| 和 protobuf 的区别 | 不需要 `.proto` 和独立编译器；schema 就是你的类型定义；但**没有**跨语言/跨版本的标准生态，字段号靠你自己维护 |
| 和 C# 版能互通字节吗 | **不能**。语义（字段顺序、缺省省略、身份复用）继承，字节布局是重新设计的（见 §8.5） |
| 最大卖点 | ① 紧凑（默认再叠一层标准 Cap'n Proto packed）② 无反射、无运行期注册 ③ 可选 `no_std + alloc` ④ 支持共享对象图与循环引用（`Rc/Arc/Weak`）⑤ 提供"写进调用方缓冲区"的入口，可做到零堆分配 |
| 当前完成度 | core + derive 已实现、测试通过；`encode_into`/`decode_from` 已实现；**C ABI（FFI）仍是空实现**，C 头是草案 |

一句话心智模型：

> **你用 `#[derive(AceSerialize)]` 标注类型 → 宏在编译期生成 `AceEncode`/`AceDecode` 实现 → 运行时 `encode` 把值压成一段字节，`decode` 按同一份 schema 把字节还原成值。**

---

## 1. 从五个角度看这个库

### 1.1 角度一：它是一个"schema 驱动"的序列化器

普通序列化（JSON）把**结构信息**也写进数据里：

```json
{ "id": 7, "name": "ace", "level": 3 }
```

`ace-serialize` 不写字段名，只写**字段号 + 值**。因为字段号就是 `order`，解码方必须持有同一份 schema（同一个 Rust 类型）才知道第 1 号槽是 `id`。

代价：schema 必须两端一致。
收益：体积小一个数量级，解析不用查字符串表。

### 1.2 角度二：它是一套"两区"的字节格式

一条消息被切成两块：

```text
[ i32 main_len ][ main 区 ][ i32 assist_len ][ assist 区 ]
```

- **main 区** = 记录堆。每条记录 `{ i32 id, i32 size, body }`，装字符串、集合、对象体。
- **assist 区** = 槽位流。每槽固定 16 字节 `{ i32 depth, i32 tag, 8 字节 payload }`，装标量值和指向记录的 id。

为什么要分两块？因为"字段可能缺席"。槽流里 tag 递增，缺了哪个 tag 就说明哪个字段没写；而大块数据（字符串、数组）不必挤在槽里，只放一个 8 字节 id 指过去。这两件事分开做，解析器就能**顺序扫一遍槽**就把字段对齐完。

### 1.3 角度三：它是一个"可省略字段"的格式

默认情况下**零值不占字节**：

- `0`、`false`、`0.0` 不写槽（但 `-0.0` 要写，因为位模式不是全零）
- `Option::None` 不写槽
- 标了 `#[ace(default = X)]` 的整数枚举，等于 `X` 时不写槽

反过来，`String`、`Vec`、`Map`、`Box`、`Rc`、嵌套结构体、没标 `default` 的枚举**永远要写**——哪怕空串、空集合、全零对象。

### 1.4 角度四：它是一个"身份感知"的格式

如果两个字段指向**同一个** `Rc<T>`，解码后仍然是同一个 `Rc`（`Rc::ptr_eq` 为真）；两个内容相等但不同实例，解码后仍是两个实例。循环引用用 `Weak<T>` 表达，配合 `Rc::new_cyclic` 构造。

这是 C# 版里 `LSerializeExtension.Cache` 那套引用缓存想做的事，Rust 版把它做成了正式契约（按对象身份、不按内容哈希）。

### 1.5 角度五：它是 C# 版的"同语义、不同实现"

| 维度 | C# LSerialize | Rust ace-serialize |
|---|---|---|
| 生成方式 | Roslyn 源生成器，产出 `*.g.cs` | `proc-macro` derive，编译期展开 |
| 扩展方式 | 运行期注册表 `LFormatterProvider.Register<T>` | 编译期 trait 实现 `impl AceEncode for T` |
| 内存 | `unsafe` 指针 + 非托管缓冲 | 安全切片 `&[u8]` / `&mut [u8]`，unsafe 只在内部 bulk 拷贝 |
| 错误 | `throw` 异常 | `Result<T, Error>`，稳定错误码 1..16 |
| 状态 | `[ThreadStatic] LSerializeContext.Default` 全局上下文 | `Options` 每次调用显式传入，无全局可变状态 |

**结论：写法像，字节不像。** 迁移时模型定义可以照搬，但**字段号要按新协议重新给一遍**（原因见 §4），二进制存档必须重新生成。

---

## 2. 十分钟上手

### 2.1 加依赖

本库尚未发布到 crates.io，按路径依赖（或 workspace 内引用）：

```toml
[dependencies]
ace-serialize = { path = "crates/ace-serialize" }
ace-serialize-derive = { path = "crates/ace-serialize-derive" }
```

注意：**core 不依赖 derive**。`#[derive(AceSerialize)]` 来自 `ace-serialize-derive`，`encode`/`decode` 来自 `ace-serialize`，两个都要加。

需要 `Arc`/`RwLock` 支持时打开 feature：

```toml
ace-serialize = { path = "crates/ace-serialize", features = ["std"] }
```

### 2.2 第一个例子

```rust
use ace_serialize::{decode, encode};
use ace_serialize::limits::Options;
use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize, Debug, PartialEq)]
struct Player {
    id: u32,
    name: String,
    level: u16,
    skills: Vec<String>,
    nick: Option<String>,
}

fn main() -> Result<(), ace_serialize::error::Error> {
    let player = Player {
        id: 7,
        name: "ace".to_string(),
        level: 3,
        skills: vec!["fire".to_string(), "ice".to_string()],
        nick: None,
    };

    let options = Options::default();          // 默认 packed = true
    let bytes = encode(&player, &options)?;    // Vec<u8>
    let back: Player = decode(&bytes, &options)?;

    assert_eq!(back, player);
    Ok(())
}
```

对照 C#：

```csharp
var bytes = LSerializer.Serialize(ref player, pack: true);
var back  = LSerializer.Deserialize<Player>(bytes, unpack: true);
```

差别只有两处：`pack` 从参数变成了 `Options.packed` 字段；异常变成了 `Result` + `?`。

### 2.3 raw 与 packed

```rust
let raw = Options { packed: false, ..Options::default() };
let raw_bytes = encode(&player, &raw)?;
let packed_bytes = encode(&player, &Options::default())?;
assert!(raw_bytes.len() > packed_bytes.len());
```

**解码时必须告诉库输入是哪种**，库不会自动识别（没有 magic、没有版本头）：

```rust
let value = decode::<Player>(&raw_bytes, &raw)?;      // 对
// decode::<Player>(&raw_bytes, &Options::default())  // 错，会返回错误
```

C# 版的 `pack` / `unpack` 参数是同一个道理，只是它藏在默认参数里更容易忘。

### 2.4 错误处理：`Result` 而不是异常

```rust
use ace_serialize::error::Error;

match decode::<Player>(&bytes, &Options::default()) {
    Ok(value) => println!("ok: {:?}", value.id),
    Err(e) => {
        // e 是枚举，e.code() 是稳定 i32（C ABI 用同一套码）
        eprintln!("decode failed: {:?} (code {})", e, e.code());
        match e {
            Error::TruncatedInput => { /* 输入被截断 */ }
            Error::MissingRequiredField => { /* 必需字段缺槽 */ }
            Error::ResourceLimit => { /* 超预算 */ }
            _ => {}
        }
    }
}
```

全部错误码（与 C 头一致，只允许末尾追加）：

| 码 | 名称 | 典型触发 |
|---:|---|---|
| 1 | `InvalidArgument` | 参数非法、`dst`/`scratch` 容量不足、活动可变借用或锁被占用 |
| 2 | `TruncatedInput` | 头/槽/记录/packed 数据被截断 |
| 3 | `UnknownTypeKey` | C ABI 未注册的 schema key |
| 4 | `InvalidTag` | 非法判别值、槽范围重叠/逆序、非法 presence |
| 5 | `DepthExceeded` | 超过 `max_depth`（默认 256） |
| 6 | `ResourceLimit` | 超过字节/记录/槽/集合/引用预算 |
| 7 | `InvalidReference` | 引用 id 缺失、重复定义或越界 |
| 8 | `InvalidUtf8` | 非法 UTF-8 / 非法 Unicode 标量 |
| 9 | `VersionMismatch` | ABI 版本不匹配 |
| 10 | `PanicCaught` | FFI 边界捕获到 panic |
| 11 | `Unsupported` | 本期不支持的能力 |
| 12 | `NotFound` | C ABI 查询无此项 |
| 13 | `TypeMismatch` | 动态值无法无损转换 |
| 14 | `CycleUnsupported` | 强引用环无法安全构造 |
| 15 | `MissingRequiredField` | 不可省略字段缺槽 |
| 16 | `DuplicateKey` | Map 解码时同一 key 出现两次（Rust 独有） |

---

## 3. 字节长什么样（手算一遍就懂了）

定义：

```rust
#[derive(AceSerialize)]
struct TwoInts { x: i32, y: i32 }
```

编码 `TwoInts { x: 7, y: 0 }`（raw），实际输出 56 字节：

```text
10 00 00 00                          main_len = 16
01 00 00 00  08 00 00 00             record: id=1, size=8
01 00 00 00  01 00 00 00             body: slot_offset=1, slot_count=1
20 00 00 00                          assist_len = 32（两个槽）
00 00 00 00  00 00 00 00  01 00 00 00 00 00 00 00   slot0: depth=0 tag=0 payload=1（根 → record 1）
00 00 00 00  00 00 00 00  07 00 00 00 00 00 00 00   slot1: depth=1 tag=0 payload=7（x = 7）
```

读懂它的四个要点：

1. **根槽固定在 `assist[0]`**：`depth=0, tag=0`，payload 是 record id（引用型根）或值本身（≤8 字节标量根）。根槽**不属于任何对象**。
2. **对象记录体只有 8 字节**：`slot_offset` + `slot_count`，告诉你这个对象的字段槽在 assist 里的连续区间 `[1, 2)`。
3. **`y = 0` 没有槽**：零值标量省略，所以 `slot_count = 1` 而不是 2。解码时 tag 0 有了、tag 1 没来 → `y` 取零值。
4. 换成 `{ x: 7, y: 9 }`，assist 变 48 字节、`slot_count = 2`，总长 72 字节。

同一个值开 packed 后只有 17 字节：

```text
11 10 01 11 08 01 11 01 20 00 00 01 01 01 01 01 07
```

packed 是 Cap'n Proto 标准打包：每 8 字节一组，前面加一个"哪些字节非零"的位图，全零的组用两个字节打发掉。它只是 raw 的一层无损压缩，**语义完全一样**。

---

## 4. 字段编号：`order` 是协议，不是排版

```rust
#[derive(AceSerialize)]
struct Account {
    #[ace(order = 0)] id: u64,
    #[ace(order = 1)] name: String,
    #[ace(order = 4)] email: Option<String>,
    #[ace(skip)]      cache: u64,
}
```

规则（每一条都对应一个真实坑）：

| 规则 | 说明 |
|---|---|
| 不写 `order` | 按**声明序号**自动编号（第 0、1、2… 个字段） |
| 显式 `order` | 必须非负、能放进 `i32`；参与同一个重复检测 |
| 编码顺序 | 永远按 `order` 升序发射，与声明顺序无关 |
| 允许空洞 | `order` 可以是 0/1/4，中间的 2、3 就是"以前有过、现在删了" |
| `#[ace(reserved(2, 3))]` | 类型级锁号，字段不得占用；删字段后必须保留号，否则老数据会被新字段误读 |
| `#[ace(skip)]` | 不写不读，构造时取 `Default::default()`；**仍然占用它的编号** |

最后一条最容易踩。上面例子里 `cache` 是第 4 个字段（序号 3），`skip` 之后它依然占 3 号，所以 `email` 必须写 `order = 4`。如果你给 `email` 写 `order = 3`，宏会直接报：

```text
error: ace: duplicate order 3
```

对照 C#：

```csharp
[LSerializable]
public partial class Account {
    [LSerializeOrder(0)] public ulong Id;
    [LSerializeOrder(1)] public string Name;
    [LSerializeOrder(4)] public string Email;
    [LSerializeIgnore]    public ulong Cache;
}
```

C# 版有个容易忽略的差别：它的 tag 是 writer 每次 `IncreaseTag()` 递增出来的，也就是**写入顺序**；生成代码按声明顺序调用 `WriteXxx`，`LSerializeOrder` 在活路径里并不参与字节定位。Rust 的 `order` 才是**真正的字段 id**：发射顺序按 `order` 排序，与声明顺序无关。

所以从 C# 迁过来时，字段号要按你**想要的协议顺序**重新给一遍，不能指望"声明顺序自动对齐"。而 Rust 的自动编号取的是声明位置（含 `skip` 字段），这也是 `skip` 必须保留编号的原因——否则后面所有字段的号都会前移，已发布的数据就全错位了。

---

## 5. 属性速查表

| 位置 | 写法 | 作用 | C# 对应 |
|---|---|---|---|
| 类型 | `#[derive(AceSerialize)]` | 生成编码/解码实现 | `[LSerializable]` |
| 类型 | `#[ace(reserved(3, 4, 7))]` | 锁定字段号 | 无 |
| 类型（整数枚举） | `#[ace(default = Idle)]` | 该变体可省略，缺席恢复它 | 无（靠 `default` 值） |
| 字段 | `#[ace(order = 3)]` | 字段号 = 线上 tag | `[LSerializeOrder(3)]` |
| 字段 | `#[ace(skip)]` | 不参与序列化，取 `Default` | `[LSerializeIgnore]` |
| 变体 | `#[ace(tag = 7)]` | 多态枚举的数值判别 | 无（靠类型/接口） |
| 变体 | `#[ace(type_name = "Alarm")]` | 多态枚举的字符串判别（65535 逃生口） | 无 |

属性写错位置、重复写、写不存在的名字，宏都会在**编译期**报 `ace: ...` 开头的诊断，并且指向出错的那个字段/变体，而不是留下一堆下游类型错误。

---

## 6. 支持的类型

### 6.1 可以直接用的

| 类别 | 类型 | 说明 |
|---|---|---|
| 标量 | `bool` `i8` `u8` `i16` `u16` `i32` `u32` `i64` `u64` `f32` `f64` `char` | 可省略（零值不写槽） |
| 字符串 | `String`（`&str` 可编码） | 必须 UTF-8；空串也要写槽 |
| 容器 | `Vec<T>`、`BTreeMap<K, V>`、`Option<T>` | `Option<Option<T>>` 编译期拒绝 |
| 指针 | `Box<T>`、`Rc<T>`、`Arc<T>`（std）、`Rc<RefCell<T>>`、`Arc<RwLock<T>>` | `Rc`/`Arc` 按身份复用 id |
| 反向边 | `Weak<T>`、`Option<Weak<T>>` | 表达循环引用 |
| 定宽值 | `schema::value::Guid`、`DateTime`、`DateTimeOffset`、`IntPtr`、`UIntPtr`、`ToolkitsU64` | 见 §6.3 |
| 枚举 | 整数枚举、带载荷多态枚举 | 见 §7 |
| 自定义 | `#[derive(AceSerialize)]` 的结构体、手写 `impl` 的类型 | 手写见 §9 |

### 6.2 编译期就会被拒绝的

| 写法 | 原因 |
|---|---|
| `isize` / `usize` 字段 | 没有固定 wire 宽度 |
| `i128` / `u128` / `HashMap` / `HashSet` / `BTreeSet` / `VecDeque` | 不在支持集合内（`HashMap` 遍历顺序不确定） |
| 元组结构体 `struct P(i32)`、单元结构体 | 只支持具名字段结构体 |
| 带生命周期参数的类型 | 不支持 |
| `Option<Option<T>>` | 线上无法表达两层 presence |
| 字段类型是 `RefCell<T>` / `RwLock<T>`（没有 `Rc`/`Arc` 包着） | 拥有型内部可变性没有身份语义 |
| 多态枚举变体里含 `Rc`/`Arc`/`Weak` | 本期不支持（图 + 多态变体组合属后续批次） |
| 整数枚举 `#[repr(C)]` / `#[repr(transparent)]` / `isize`/`usize` | 表示不在白名单 |

### 6.3 定宽值的特别约定

| 类型 | wire | 和 C# 的差异 |
|---|---|---|
| `Guid([u8; 16])` | 16 字节，RFC 4122 网络字节序 | C# `Guid.ToByteArray()` 是混合小端序，**字节序不同** |
| `DateTime { ticks, kind }` | `u64`：低 62 位 ticks + 高 2 位 Kind（0/1/2） | 与 .NET 二进制布局同构 |
| `DateTimeOffset` | 10 字节：`i64` 本地时钟 ticks + `i16` 分钟偏移 | C# 只用 8 字节，**信息可能丢失** |
| `IntPtr` / `UIntPtr` | 固定 8 字节 | 不随平台指针宽度变化 |

---

## 7. 两种枚举，别搞混

### 7.1 整数枚举（对应 C# 的 `enum`）

```rust
#[derive(AceSerialize, Debug, PartialEq)]
#[repr(u8)]
enum Job { Warrior = 1, Mage = 2, Rogue = 3 }
```

- 全部变体无载荷；
- `repr` 只能是 `i8/u8/i16/u16/i32/u32/i64/u64`；
- **不写 `repr` 时按 `i32` 编码**，判别值超出 `i32` 会编译失败（提示你补 `#[repr(i64)]`）；
- 想要"默认值可省略"就加类型级 `#[ace(default = Idle)]`。

### 7.2 多态枚举（对应 C# 的"接口/抽象类多态"）

C# 里你有 `ILSerializable<T>` 接口、抽象基类、`Dog : Animal`。Rust 没有继承，用**带载荷的枚举**表达同一件事：

```rust
#[derive(AceSerialize, Debug, PartialEq)]
enum Shape {
    #[ace(tag = 1)] Circle { radius: f64 },
    #[ace(tag = 2)] Rect { w: f64, h: f64 },
    #[ace(tag = 3)] Empty,
}
```

规则：

- 每个变体必须显式写 `#[ace(tag = n)]`（0..=65534）或 `#[ace(type_name = "...")]`；
- **同一个枚举不能混用**两种模式；
- 变体只能是单元变体或具名字段变体（不能是元组变体）；
- `type_name` 走 65535 逃生口，线上额外写一条别名字符串记录。

`type_name` 模式长这样：

```rust
#[derive(AceSerialize)]
enum Message {
    #[ace(type_name = "ping")] Ping { seq: u32 },
    #[ace(type_name = "pong")] Pong,
}
```

别名字符串是**协议的一部分**：改名等于改协议，老数据会解不出来（返回 `InvalidTag`）。

---

## 8. 与 C# LSerialize 的逐项对照

### 8.1 概念对照

| C# | Rust | 备注 |
|---|---|---|
| `[LSerializable] partial class Foo` | `#[derive(AceSerialize)] struct Foo` | Rust 没有 partial，宏补 `impl` |
| `[LSerializeOrder(n)]` | `#[ace(order = n)]` | 语义相同：字段号 = tag |
| `[LSerializeIgnore]` | `#[ace(skip)]` | Rust 的 skip 仍占号 |
| `[LSerializeInclude]` | 无 | Rust 没有"选择性包含"，字段默认全参与 |
| `ILSerializable<T>` 接口 | `AceEncode` / `AceDecode` trait | 编译期绑定，不是运行期接口表 |
| `LFormatter<T>` 抽象类 | 同上（直接 impl trait） | 无基类，无虚表 |
| `LFormatterProvider.Register<T>()` | `impl AceEncode for T` | 运行期注册 → 编译期解析 |
| `LSerializeContext` | `Options` | 每次调用显式传 |
| `LSerializeException` | `Error` 枚举 | 稳定错误码 |
| `class` 引用语义 + GC | `struct` 值语义 + `Rc`/`Arc` | 共享必须显式声明 |
| `null` | `Option<T>` | 类型系统强制处理 |
| `GetHashCode()` / `Equals()` | 不生成 | 需要就 `#[derive(PartialEq)]` 自己声明 |

### 8.2 API 对照

| C# | Rust | 说明 |
|---|---|---|
| `LSerializer.Serialize<T>(ref v, pack)` → `byte[]` | `encode(&v, &options)` → `Result<Vec<u8>>` | 拥有所有权的便捷路径 |
| `LSerializer.SafeSerialize<T>(ref v, pack)` | 同上 | Rust 没有全局上下文，两条路径合一 |
| `LSerializer.FastSerialize<T>(ref v, byte* buf, pack)` → `int` | `encode_into(&v, &mut dst, &mut scratch, &options)` → `Result<usize>` | 写进调用方缓冲；多了 `scratch` 工作区 |
| 无 | `encode_into_probe(..., &mut need)` | Rust 额外提供容量探测（`scratch` 不够时回填所需字节数下界） |
| `LSerializer.Deserialize<T>(byte[], unpack)` | `decode::<T>(&bytes, &options)` | |
| `LSerializer.FastDeserialize<T>(byte*, len, ref v, unpack)` | `decode_from(&bytes, &mut v, &options)` | 写进既有实例，复用容器容量 |
| `LStreamWriter` / `LStreamReader` | `Encoder` / `Decoder` | 手写 impl 时才直接接触 |
| `LSerializeContext.DepthLimit = 1000` | `Options.max_depth = 256` | **有意不同**（Rust 栈安全优先） |

### 8.3 生成代码对照

C# 源生成器为 `Complex` 产出（真实产物，节选）：

```csharp
public unsafe partial class Complex : ILSerializable<Complex>
{
    public static void Serialize(LSerializeContext* context, LStreamWriter* writer, ref Complex value)
    {
        context->SetValidFieldCount(GetValidFieldCount(ref value));
        writer->WriteNumber(context, ref value.@Maybe);
        writer->WriteList<Int32>(context, ref value.@Items);
        writer->WriteManagedStruct(context, ref value.@Child);
        writer->WriteString(context, ref value.@Text);
    }

    public static void Deserialize(LSerializeContext* context, LStreamReader* reader, ref Complex value)
    {
        value = new Complex();
        reader->ReadNumber(context, ref value.@Maybe);
        ...
    }

    private static int GetValidFieldCount(ref Complex value) { ... }   // 数"哪些字段要写"
}
```

同一形状的 Rust derive 产出（语义等价，语法不同）：

```rust
impl AceEncode for Complex {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;                        // 打开一个对象帧
        let mut body = || -> Result<()> {
            e.begin_field(0)?;                    // 选中 order 0
            self.maybe.ace_encode(e)?;            // 该字段自己决定写值还是省略
            e.check_field_consumed()?;            // 校验这次选择已被消费
            e.begin_field(1)?;
            self.items.ace_encode(e)?;
            e.check_field_consumed()?;
            // ...
            Ok(())
        };
        let encoded = body();                     // 先记下错误，保证帧一定被关闭
        let left = e.leave_object();
        encoded?;
        left?;
        Ok(())
    }
}
```

**职责是一一对应的**：

| C# 生成代码 | Rust 生成代码 | 职责 |
|---|---|---|
| `GetValidFieldCount` + `SetValidFieldCount` | 无 | C# 靠"数有效字段"定位缺席；Rust 靠 tag 递增 + 缺槽 |
| `writer->WriteXxx(context, ref value.F)` | `e.begin_field(n)? + F.ace_encode(e)?` | 逐字段发射 |
| `value = new Complex()` | 逐字段绑定后 `Self { .. }` | 构造新值 |
| `RegisterFormatter()` + `ComplexFormatter` | 无 | Rust 不需要注册表 |
| `IEquatable` / `GetHashCode` | 无 | Rust 不生成相等/哈希 |

### 8.4 类型映射对照

| C# 类型 | Rust 类型 | 注意 |
|---|---|---|
| `int` / `long` / `byte` / `short` | `i32` / `i64` / `u8` / `i16` | 宽度必须显式选 |
| `float` / `double` | `f32` / `f64` | 不规范化 NaN，`-0.0` 保留 |
| `string` | `String` | 不能为 null，空串 ≠ null |
| `T[]` / `List<T>` | `Vec<T>` | 计数从 `u16` 变 `u32` |
| `Dictionary<K,V>` | `BTreeMap<K,V>` | 按 key 排序编码，**没有 `HashMap`** |
| `T?`（可空值类型） | `Option<T>` | |
| `T`（可空引用） | `Option<T>` 或 `Box<T>` | 要"有或没有"就用 `Option` |
| `class` 共享引用 | `Rc<T>` / `Arc<T>` | 需要显式 |
| 循环引用 | `Weak<T>` | 配合 `Rc::new_cyclic` |
| `System.Guid` | `schema::value::Guid([u8;16])` | 字节序不同 |
| `System.DateTime` | `schema::value::DateTime { ticks, kind }` | |
| `System.DateTimeOffset` | `schema::value::DateTimeOffset` | 10 字节，比 C# 完整 |
| `decimal` | 无直接对应 | 自己定义结构体（如 `{ lo, mid, hi, flags }`）表达 |
| `System.Type` | 无 | 用 `type_name` 字符串或枚举 tag 表达 |
| 接口/抽象类多态 | 带载荷 `enum` + `#[ace(tag)]` | |

### 8.5 字节格式差异（**不能互通**）

| 维度 | C# LSerialize | Rust ace-serialize |
|---|---|---|
| 集合计数 | 短集合 `u16` | `u32` |
| 根值 | `raw` 非托管数组特例 | 统一根槽 `assist[0]`，零值也有槽 |
| 字段定位 | 全流 `(depth, tag)` | 对象槽范围 `[offset, count)`，`offset ≥ 1` |
| 数值写入 | 读邻接内存（有泄漏缺陷） | 按类型自然宽度零扩展/符号扩展，不读相邻字节 |
| 压缩 | 私有 Packer（`ff [N-1] [8*N]` 逃逸段） | Cap'n Proto 标准 packed（`ff` + 8 字节 + `N`） |
| `Guid` | `ToByteArray()` 混合端序 | RFC 4122 网络字节序 |
| `DateTimeOffset` | 8 字节，可能丢信息 | 10 字节完整 |
| Map 重复 key | 无专用错误码（按通用失败处理） | 返回 `16 DuplicateKey` |
| 最大深度 | 1000 | 256 |
| 记录嵌套 | 可嵌套 | main 一律平铺 |

需求文档里明确写了"**不与 C# 字节互通**"，且不复刻 C# 的十类缺陷（邻接内存泄漏、错误 Nullable、哈希身份合并、静默越界等）。所以：

> 迁移的正确姿势是**照着 C# 的模型和字段号重写类型定义，然后重新生成所有存档**，而不是想办法读老字节。

---

## 9. 手写实现（对照 C# 的 formatter 注册）

### 9.1 最简单的场景：给一个标量包个 newtype

C# 里你会写：

```csharp
LFormatterProvider.Register(new CelsiusFormatter());   // 运行期注册

sealed class CelsiusFormatter : LFormatter<Celsius> {
    public override void Serialize(LSerializeContext* c, LStreamWriter* w, ref Celsius v)
        => w.WriteDouble(c, ref v.Value);
}
```

Rust 里就是"给类型实现 trait"，没有注册这一步：

```rust
use ace_serialize::encode::Encoder;
use ace_serialize::decode::Decoder;
use ace_serialize::error::Result;
use ace_serialize::schema::{value, AceDecode, AceEncode};

#[derive(Debug, PartialEq)]
struct Celsius(f64);

impl AceEncode for Celsius {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(value::encode_f64(self.0))   // 有字段选择就写槽，没有就写根槽
    }
    // 放进 Vec 时用这个（默认实现会返回 InvalidArgument，必须覆写）
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&value::encode_f64(self.0))
    }
}

impl AceDecode for Celsius {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        let payload = d.decode_scalar()?;
        Ok(Celsius(value::decode_f64(&payload)))
    }
}
```

这样它就能当**根值**和**普通字段**用了。但如果你要把它放进 `Option<Celsius>`、`Vec<Celsius>`，或者让它成为 `#[derive]` 结构体的字段（derive 需要字段类型支持图路径），还得补三个：

```rust
use ace_serialize::schema::{AceDecodeCyclic, AceElement, ElementKind, NonOptional};

impl NonOptional for Celsius {}                  // 允许放进 Option

impl AceElement for Celsius {                    // 允许当集合元素
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 8 }           // 定宽 8 字节，可走批量拷贝
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&value::encode_f64(self.0))
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        let bytes = d.read_element_bytes(8)?;
        Ok(Celsius(value::decode_f64(&bytes.try_into().unwrap())))
    }
}

impl AceDecodeCyclic for Celsius {               // 允许参与两阶段图解码
    type Plan = Self;
    fn decode_plan(ctx: &mut ace_serialize::decode::DecodeCtx) -> Result<Self> {
        ctx.value::<Self>(ace_serialize::decode::PlanValueLayout::Leaf)
    }
    fn build_from_plan(_ctx: &mut ace_serialize::decode::BuildCtx<'_>, plan: Self) -> Self {
        plan
    }
}
```

> 为什么这么啰嗦？因为 Rust 没有运行期类型信息和反射，编译器必须在**编译期**知道每个类型在"字段位置 / 集合元素位置 / Option 内层 / 图节点"四种位置各怎么编解码。C# 靠运行期注册表 + 虚方法绕过了这件事，代价是每次都要查表。

### 9.2 手写一个对象类型（了解 derive 生成了什么）

```rust
#[derive(Debug, PartialEq)]
struct Point { x: i32, y: i32 }

impl AceEncode for Point {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;                       // 开帧
        let mut body = || -> Result<()> {
            e.begin_field(0)?;                   // tag 0
            self.x.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(1)?;                   // tag 1
            self.y.ace_encode(e)?;
            e.check_field_consumed()?;
            Ok(())
        };
        let encoded = body();                    // 先保存错误
        let left = e.leave_object();             // 无论如何都关帧
        encoded?;                                // 再传播错误
        left?;                                   // 最后要 record id
        Ok(())
    }
}

impl AceDecode for Point {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        let payload = d.decode_required_scalar()?;      // 读槽里的 record id
        d.enter_object(u64::from_le_bytes(payload))?;   // 进入对象
        let mut body = || -> Result<Self> {
            let x = if d.begin_field(0)? { i32::ace_decode(d)? } else { 0 };
            let y = if d.begin_field(1)? { i32::ace_decode(d)? } else { 0 };
            Ok(Point { x, y })
        };
        let decoded = body();
        let left = d.leave_object();
        let value = decoded?;
        left?;
        Ok(value)
    }
}
```

三个"为什么"：

1. **为什么用闭包**：`leave_object()` 必须在任何失败路径上都被调用，否则编码器状态不闭合。闭包把"可能失败的字段体"和"必须执行的收尾"分开，这是 Rust 里表达 `try/finally` 的常用手法。
2. **为什么 `check_field_consumed()`**：`begin_field(3)` 之后必须有且只有一次"发射"（写标量/写记录/写 None）。漏了就是协议 bug，库用这个调用帮你抓住。
3. **为什么 `left?` 在最后**：`leave_object()` 返回 record id，`encoded?` 先传播字段错误，再检查收尾错误——顺序反了会掩盖真正的失败原因。

### 9.3 必须实现哪些 trait（速查）

| 使用位置 | `AceEncode` | `AceDecode` | `AceElement` | `AceDecodeCyclic` | `NonOptional` |
|---|:---:|:---:|:---:|:---:|:---:|
| 直接当根值 `encode` / `decode` | ✅ | ✅ | — | — | — |
| 作为 `#[derive]` 结构体的字段 | ✅ | ✅ | — | ✅ | — |
| `Option<T>` 内层 | ✅ | ✅ | — | ✅ | ✅ |
| `Vec<T>` / Map 值 | ✅ | ✅ | ✅ | ✅ | — |
| `Rc<T>` / `Arc<T>` 的目标 | ✅ | ✅ | — | ✅ | — |

---

## 10. 共享对象图与循环引用

```rust
use std::rc::{Rc, Weak};
use ace_serialize::decode::{decode_cyclic, Decoder};
use ace_serialize::limits::Options;
use ace_serialize::encode;
use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize)]
struct Node {
    value: u32,
    children: Vec<Rc<Node>>,
    parent: Option<Weak<Node>>,       // 反向边用 Weak，不延长生命周期
}

let root = Rc::new_cyclic(|me| Node {
    value: 1,
    children: vec![Rc::new(Node {
        value: 2,
        children: Vec::new(),
        parent: Some(Weak::clone(me)),  // 闭包里先拿到自己的 Weak
    })],
    parent: None,
});

let bytes = encode(&root, &Options::default())?;

let mut decoder = Decoder::new(&bytes, &Options::default())?;
let back = decode_cyclic::<Rc<Node>>(&mut decoder)?;   // 图路径用这个入口
decoder.finish()?;

assert_eq!(back.value, 1);
let child = &back.children[0];
let up = child.parent.as_ref().and_then(Weak::upgrade).unwrap();
assert!(Rc::ptr_eq(&up, &back));      // 回边指回同一实例
```

要点：

- **共享**：两个字段指向同一个 `Rc`，解码后 `Rc::ptr_eq` 为真；两个内容相等但不同实例，解码后仍是两个（按身份，不按内容哈希）。
- **循环**：强引用环（`Rc<RefCell<T>>` 自环、`A→B→A` 强边）解码返回 `14 CycleUnsupported`，不挂起、不栈溢出；正确写法是**一条强边 + 一条 `Weak` 回边**。
- **`decode` 还是 `decode_cyclic`**：derive 为含 `Rc`/`Arc`/`Weak` 的类型生成了 `AceDecode`，它内部会走图路径，所以 `decode::<T>` 通常也能用；`decode_cyclic` 是显式的两阶段入口，适合根值本身就是图节点（如 `Rc<Node>`）的情况。
- **编码锁状态**：`Rc<RefCell<T>>` 用 `try_borrow`、`Arc<RwLock<T>>` 用 `try_read`。有活动可变借用或锁被占用时返回 `1 InvalidArgument`，**不阻塞、不自旋**。

---

## 11. Rust 语法速成（给 C# 开发者）

这一节只讲**读本库代码时真正会遇到的**语法，每条都给"你的 C# 直觉"和"Rust 的实际规则"。

### 11.1 值语义 vs 引用语义

```csharp
var a = new Point { X = 1 };
var b = a;          // 同一个对象，改 b 就是改 a
```

```rust
let a = Point { x: 1, y: 2 };
let b = a;          // 复制/移动，a 之后不能再用（除非 Point: Copy）
```

- `struct` 默认按值移动；需要共享就显式用 `Rc<T>` / `Arc<T>`。
- 函数参数 `&T` = 只读借用（类似 `in`），`&mut T` = 独占可变借用（类似 `ref`，但编译器保证没有别名）。
- 一个值同一时刻**要么**有多个 `&T`，**要么**有一个 `&mut T`。这就是借用检查器，也是 Rust 没有数据竞争的原因。

### 11.2 `Option<T>` 取代 `null`

```csharp
string name = null;
if (name != null) Console.WriteLine(name.Length);
```

```rust
let name: Option<String> = None;
if let Some(n) = &name {
    println!("{}", n.len());
}
let len = name.as_ref().map(|n| n.len()).unwrap_or(0);
```

- 没有 `null`，所以"可能没有"必须写进类型。
- 本库的 `Option<T>` 还有线上含义：`None` 不写槽，`Some(0)` 要写槽——两者不等价。

### 11.3 `Result<T, E>` 取代异常

```csharp
try { var v = Deserialize<Player>(bytes); }
catch (LSerializeException e) { ... }
```

```rust
let v = decode::<Player>(&bytes, &options)?;      // ? = 失败就 return Err
```

`?` 是"如果 `Err` 就直接返回"，等价于 C# 里每行都包一层 try/catch 但不用写。需要转换错误类型时用 `map_err`：

```rust
let n = u32::try_from(id).map_err(|_| Error::InvalidReference)?;
```

### 11.4 `trait` ≈ 接口，但没有继承

```csharp
interface IFormatter<T> { void Serialize(ref T value); }
class MyFormatter : IFormatter<MyType> { ... }
```

```rust
trait AceEncode {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()>;
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        Err(Error::InvalidArgument)      // 默认实现，和 C# 8 的默认接口方法类似
    }
}
impl AceEncode for MyType { ... }
```

两条和 C# 不一样的地方：

1. **孤儿规则**：你只能为"自己的类型"或"自己的 trait"实现 trait。想给外部类型（比如 `u128`）加本库的能力，得用 newtype 包一层：
   ```rust
   struct MyU128(u128);
   impl AceEncode for MyU128 { ... }
   ```
2. **没有虚表继承**：多态靠 `enum` + `match`（见 §7.2），或者 `dyn Trait`（本库不用）。

### 11.5 `#[derive]` ≈ 源生成器

```csharp
[LSerializable]                       // Roslyn 生成 *.g.cs
public partial class Player { ... }
```

```rust
#[derive(AceSerialize)]               // proc-macro 在编译期展开
struct Player { ... }
```

区别：

- 生成的是 `impl` 块，不是新文件；想看展开结果用 `cargo expand`（需安装）或 `cargo rustc -- -Zunpretty=expanded`（nightly）。
- 属性参数语法不同：C# 是 `[LSerializeOrder(3)]`，Rust 是 `#[ace(order = 3)]`（`ace` 是 derive 声明的辅助属性命名空间）。
- 宏**看不到**运行期信息，所以类型必须能在语法层面被识别；不认识的类型直接编译失败，而不是运行时才发现。

### 11.6 `enum` 是"判别联合"

```rust
enum Shape {
    Circle { radius: f64 },
    Rect { w: f64, h: f64 },
    Empty,
}
```

C# 里等价物是"抽象基类 + 若干子类 + 模式匹配"，或者 C# 9 的 `record` 继承。Rust 的 `enum` 一个类型就装下所有变体，`match` 时编译器还会检查你有没有漏掉分支。

### 11.7 `match` ≈ `switch` + 类型模式

```rust
match shape {
    Shape::Circle { radius } => println!("r={radius}"),
    Shape::Rect { w, h } => println!("{}", w * h),
    Shape::Empty => {}
}
```

必须覆盖所有分支（穷尽性检查），没有 `default` 也能编译——这是本库"解码时不会漏掉变体"的基础。

### 11.8 泛型 + trait bound

```csharp
T Deserialize<T>(byte[] data) where T : new() { ... }
```

```rust
pub fn decode<T: AceDecode>(src: &[u8], options: &Options) -> Result<T> { ... }
```

`T: AceDecode` 读作"T 必须实现 AceDecode"。本库的入口全是这种形状：`encode<T: AceEncode>`、`decode<T: AceDecode>`。调用时通常不用写类型，靠返回值推断：`let p: Player = decode(&bytes, &opts)?;`

### 11.9 生命周期：只需要认得 `'_`

```rust
pub fn encode<T: AceEncode>(value: &T, options: &Options) -> Result<Vec<u8>>
fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()>
```

- `&T` 是"借用，不拥有"。
- `Encoder<'_>` 里的 `'_` 意思是"这个编码器借用了别的东西，具体多久由编译器推断"。
- 你写业务代码时**基本不用手写生命周期**；只有手写 `impl` 时照着签名抄 `Encoder<'_>` / `Decoder<'_>` 即可。

### 11.10 闭包与所有权（为什么生成代码里有个 `body`）

```rust
let mut body = || -> Result<()> { ... };
let encoded = body();
let left = e.leave_object();
encoded?;
left?;
```

- `||` 是闭包（lambda），`body` 捕获了 `e` 的可变借用。
- 调用完 `body()` 后借用结束，所以下一行才能再用 `e`。
- 这段模式是"先做可能失败的事，再做必须做的事"——Rust 没有 `finally`，用这个顺序表达。

### 11.11 模块与路径

```rust
use ace_serialize::limits::Options;          // 类似 using AceSerialize.Limits;
use ace_serialize::schema::AceEncode;        // trait 必须 in scope 才能调方法
```

- `::core::...` / `::alloc::...` 是 `no_std` 环境下的标准库路径；本库内部大量使用，你自己的代码用 `std::` 就行。
- trait 方法要能调用，trait 必须在作用域内（`use` 进来）。这是新手最常见的"方法不存在"报错来源。

### 11.12 没有 GC：`Rc` / `Arc` / `RefCell` / `Weak`

| 需求 | Rust 工具 | C# 里的对应 |
|---|---|---|
| 多个所有者、单线程 | `Rc<T>` | 普通对象引用 |
| 多个所有者、多线程 | `Arc<T>` | 普通对象引用（线程安全） |
| 共享 + 可变 | `Rc<RefCell<T>>` / `Arc<RwLock<T>>` | 普通字段随便改 |
| 打破循环 | `Weak<T>` | 弱引用 / 干脆不管（GC 会回收） |

Rust 没有 GC，所以"循环引用"必须由你用 `Weak` 手动打破；本库把这个约束变成了协议的一部分（强环解码直接报 `14`）。

### 11.13 `no_std + alloc`

```toml
# 默认：no_std + alloc（core 只用 core/alloc，不依赖 std）
ace-serialize = { path = "crates/ace-serialize" }
# 需要 Arc/RwLock 时
ace-serialize = { path = "crates/ace-serialize", features = ["std"] }
```

嵌入式/WASM 场景可以不开 `std`；`Arc`、`RwLock`、FFI 需要 `std`。

---

## 12. 常见编译错误对照表

| 报错 | 你写 C# 时的直觉 | 原因与解法 |
|---|---|---|
| `error: ace: duplicate order 3` | "字段名不同就行" | 两个字段拿到同一个编号（常见于 `skip` 字段仍占号）→ 显式改 `order` |
| `the trait bound 'Celsius: AceDecodeCyclic' is not satisfied` | "类型能序列化就行" | derive 要求每个字段类型支持图路径 → 补 `AceDecodeCyclic`（见 §9.1） |
| `the trait bound 'Point: AceElement' is not satisfied` | "结构体当然能放进 Vec" | `Vec<T>` 的元素路径要求 `AceElement` → 补实现，或直接对 `Point` 用 `#[derive]` |
| `error: ace: Option<Option<T>> is not supported` | "两层可空很正常" | 线上无法表达两层 presence → 改成 `Option<T>` 或加一层结构体 |
| `error: ace: unsupported field type; isize and usize have no fixed wire width` | "int 就是 int" | 换成固定宽度的 `i32`/`u64` 等 |
| `error: ace: S1 仅支持具名字段结构体` | "元组结构体也行吧" | 改成 `struct P { x: i32 }`，或用 newtype + 手写 impl |
| `cannot borrow 'body' as mutable` | "闭包随便调" | 闭包捕获了 `&mut`，绑定要写 `let mut body = ...` |
| `use of moved value` | "赋值就是复制引用" | `struct` 默认移动；需要共享用 `Rc::clone(&v)` 或 `#[derive(Clone)]` |
| `cannot borrow as mutable more than once` | "两个方法都能改" | 同一作用域不能有两个 `&mut`；缩小借用范围或改用 `RefCell` |
| `no method named 'ace_encode' found for ...` | "方法明明存在" | trait 没进作用域 → 加 `use ace_serialize::schema::AceEncode;`（`AceDecode` 同理） |
| `E0117: only traits defined in the current crate can be implemented for arbitrary types` | "给外部类型加个扩展方法" | 孤儿规则 → 用 newtype 包一层 |

---

## 13. 典型配方

### 13.1 存档 / 网络包（最常用）

```rust
// 存
let bytes = encode(&save, &Options::default())?;   // packed
std::fs::write("save.bin", &bytes)?;

// 读
let bytes = std::fs::read("save.bin")?;
let save: Save = decode(&bytes, &Options::default())?;
```

### 13.2 写进复用缓冲（避免每次分配）

```rust
let mut dst = vec![0u8; 64 * 1024];
let mut scratch = vec![0u8; 8 * 1024];       // 单次调用的工作区，内容无意义

let n = encode_into(&value, &mut dst, &mut scratch, &options)?;
send(&dst[..n]);
```

`scratch` 是用来放身份表（`Rc`/`Arc` 追踪）和内部临时数据的，**不会占用 `dst`**。容量不够返回 `1 InvalidArgument`；想知道还差多少，用探测入口：

```rust
let mut need = 0usize;
if encode_into_probe(&value, &mut dst, &mut scratch, &options, &mut need).is_err() {
    // need 是"同一起址下总共需要多少字节"（不是"还差多少"）
    scratch = vec![0u8; need];
}
```

精确计算身份槽所需空间时用两个常量函数：

```rust
let per_entry = ace_serialize::identity_entry_size();   // 每个身份条目的字节数
let align     = ace_serialize::identity_entry_align();  // 对齐要求
```

### 13.3 读进既有实例（复用容器容量）

```rust
// player 是上一帧就存在的实例，容量已经分配过
decode_from(&bytes, &mut player, &options)?;
```

- `String` / `Vec` / `Option` / `BTreeMap` 等内建类型会**复用已有容量**（指针不变）；
- derive 结构体逐字段写入，所以字段里的容器也被复用；
- `Vec<Vec<T>>` 的内层也会复用；
- 失败时 `out` 的内容**不作承诺**（可能已被部分修改），不能当有效值用。

### 13.4 版本演进

三个版本是**先后替换**同一个类型定义（不是同时存在）：

```rust
// v1
#[derive(AceSerialize)]
struct Config {
    #[ace(order = 0)] volume: u8,
}
```

```rust
// v2：加字段，给新号；新字段用 Option，老数据缺槽时解码成 None
#[derive(AceSerialize)]
struct Config {
    #[ace(order = 0)] volume: u8,
    #[ace(order = 1)] language: Option<String>,
}
```

```rust
// v3：删掉 order 2 的字段 → 必须锁号，禁止新字段复用
#[derive(AceSerialize)]
#[ace(reserved(2))]
struct Config {
    #[ace(order = 0)] volume: u8,
    #[ace(order = 1)] language: Option<String>,
}
```

规则：**新字段必须是可省略类型**（标量/`Option`/带 default 的枚举），否则老数据解码会返回 `15 MissingRequiredField`。未知字段会被机械跳过（只按 16 字节槽跳），不会报错。

### 13.5 自定义 Guid / 时间类型

```rust
use ace_serialize::schema::value::{Guid, DateTime, DateTimeOffset, Kind};

#[derive(AceSerialize)]
struct Session {
    token: Guid,                 // 16 字节
    created: DateTime,           // ticks + kind
    expires: DateTimeOffset,     // 本地 ticks + 分钟偏移
}
```

注意 `Guid` 的字节序是 RFC 4122，和 .NET `Guid.ToByteArray()` **不同**；从 C# 数据迁移时要重新映射。

---

## 14. 当前完成度

| 部分 | 状态 | 证据 |
|---|---|---|
| wire（envelope / record / slot / packed） | ✅ 已实现 | `src/wire/*.rs` |
| encode（`encode` / `encode_into` / `encode_into_probe`） | ✅ 已实现 | `src/encode/mod.rs` |
| decode（`decode` / `decode_from` / `decode_cyclic`） | ✅ 已实现 | `src/decode/{mod,graph}.rs` |
| schema（标量/容器/宽值/图） | ✅ 已实现 | `src/schema/*.rs` |
| derive（`AceEncode`/`AceDecode`/`AceElement`/`AceDecodeCyclic`/`NonOptional`） | ✅ 已实现 | `crates/ace-serialize-derive/src/gen.rs` |
| 测试 | ✅ 通过（`cargo test -p ace-serialize`：521 个测试，382 个模块内单测 + 139 个集成/文档测试） | 含 46 个 C# 对照 case |
| C ABI（`ace-serialize-ffi`） | ❌ **空实现** | `src/lib.rs` 为空文件 |
| C 头 `include/ace_serialize.h` | ⚠️ 草案 v2，未冻结 | 头内自标 TBD-ABI |
| 与 C# 字节互通 | ❌ 明确非目标 | 需求 §1.3 |

---

## 15. 代码地图（按阅读顺序）

| 顺序 | 文件 | 看什么 |
|---:|---|---|
| 1 | `crates/ace-serialize/src/lib.rs` | 五个公开入口，每个都有详细文档注释 |
| 2 | `crates/ace-serialize/src/limits.rs` | `Options` 全部字段与默认值 |
| 3 | `crates/ace-serialize/src/error.rs` | 错误码闭集 |
| 4 | `crates/ace-serialize/src/schema/mod.rs` | trait 定义 + 标量/容器/图实现（1 万行，先看 trait 与 `scalar!`/`element!` 宏） |
| 5 | `crates/ace-serialize/src/schema/value.rs` | `Guid`/`DateTime`/`DateTimeOffset` 的位级约定 |
| 6 | `crates/ace-serialize/src/encode/mod.rs` | `Encoder`：`begin_field` / `encode_scalar` / `enter_object` |
| 7 | `crates/ace-serialize/src/decode/mod.rs` | `Decoder`：`begin_field` / `decode_scalar` / `enter_object` |
| 8 | `crates/ace-serialize/src/decode/graph.rs` | 两阶段图解码（`DecodeCtx` / `BuildCtx` / `bind_weak`） |
| 9 | `crates/ace-serialize-derive/src/gen.rs` | derive 到底生成了什么 |
| 10 | `crates/ace-serialize/tests/*.rs` | **最好的用法示例**：`derive_s1.rs`、`derive_s2_int_enum.rs`、`derive_s2_polymorphic.rs`、`encode_into_scratch.rs`、`csharp_conformance.rs` |

验证自己的理解：

```bash
cargo test -p ace-serialize                 # 全部测试
cargo test -p ace-serialize --release       # Release 同语义
cargo test -p ace-serialize --features std  # 打开 Arc/RwLock
cargo test -p ace-serialize-derive          # derive 的 compile-pass / compile-fail 用例
```

---

## 16. 一页速查

```rust
// 依赖
// ace-serialize + ace-serialize-derive

// 标注类型
#[derive(AceSerialize)]
struct Foo {
    #[ace(order = 0)] a: i32,
    #[ace(order = 1)] b: String,
    #[ace(skip)]      c: u64,        // 仍占 2 号
}

// 编码 / 解码
let opts = Options::default();                    // packed = true
let bytes = encode(&foo, &opts)?;                 // -> Vec<u8>
let foo2: Foo = decode(&bytes, &opts)?;

// raw
let raw = Options { packed: false, ..Options::default() };
let bytes = encode(&foo, &raw)?;

// 写进调用方缓冲
let mut dst = [0u8; 4096];
let mut scratch = [0u8; 4096];
let n = encode_into(&foo, &mut dst, &mut scratch, &opts)?;

// 读进既有实例
decode_from(&bytes, &mut foo, &opts)?;

// 图（Rc/Arc/Weak）
let mut d = Decoder::new(&bytes, &opts)?;
let v = decode_cyclic::<Rc<Node>>(&mut d)?;
d.finish()?;
```

记住三句话：

1. **`order` 是协议**，改号等于改格式；删字段先 `reserved`。
2. **可省略的只有三类**：零值标量、`Option::None`、带 `default` 的整数枚举；其余一律要写。
3. **和 C# 版语义像、字节不像**；迁移 = 重写类型定义 + 重新生成数据，不是读老档。
