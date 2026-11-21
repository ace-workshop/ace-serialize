# ace-serialize 使用指南

适用版本：`include/ace_serialize.h` v5（`ACE_ABI_VERSION` = `0x00010000`），wire 版本 1。

本指南只讲怎么用：安装、能表达哪些类型、属性怎么写、枚举与共享引用怎么组织、如何零分配编码与复用解码、C ABI 怎么调、出错怎么定位。设计与 wire 布局见 [`architecture.md`](architecture.md)，头文件条款见 [`../include/ace_serialize.h`](../include/ace_serialize.h)。

每段示例后附一行「预期」：成功时看到什么，失败时返回哪个错误码。

## 1 安装与最小例子

### 1.1 加依赖

未发布到 crates.io，按路径依赖引用：

```toml
[dependencies]
ace-serialize = { path = "crates/ace-serialize" }
ace-serialize-derive = { path = "crates/ace-serialize-derive" }
```

core 不依赖 derive：`encode`/`decode` 来自 `ace-serialize`，`#[derive(AceSerialize)]` 来自 `ace-serialize-derive`，两个都要加。核心默认 `no_std + alloc`；需要 `Arc`/`RwLock` 时开 feature：`ace-serialize = { path = "crates/ace-serialize", features = ["std"] }`。

### 1.2 第一个例子

```rust
use ace_serialize::limits::Options;
use ace_serialize::{decode, encode};
use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize, Debug, PartialEq)]
struct Version {
    major: u32,
    minor: u32,
}

#[derive(AceSerialize, Debug, PartialEq)]
struct Player {
    id: u64,
    name: String,
    score: Option<i32>,
    version: Version,
}

fn main() -> Result<(), ace_serialize::error::Error> {
    let player = Player {
        id: 7,
        name: String::from("ada"),
        score: Some(-3),
        version: Version { major: 1, minor: 2 },
    };
    let options = Options::default();        // packed = true
    let bytes = encode(&player, &options)?;  // Vec<u8>
    let back: Player = decode(&bytes, &options)?;
    assert_eq!(back, player);
    Ok(())
}
```

预期：成功，`back == player`。

三个入口签名：

| 函数 | 签名 | 用途 |
|---|---|---|
| `encode` | `encode<T: AceEncode>(&T, &Options) -> Result<Vec<u8>>` | 归库所有的字节缓冲 |
| `decode` | `decode<T: AceDecode>(&[u8], &Options) -> Result<T>` | 解出归调用方所有的值，返回前校验消息完成状态 |
| `decode_from` | `decode_from<T: AceDecode>(&[u8], &mut T, &Options) -> Result<()>` | 写进已有实例（§7.4） |

### 1.3 packed 与 raw

```rust
let packed_options = Options::default();                        // packed = true
let raw_options = Options { packed: false, ..Options::default() };
let packed_bytes = encode(&player, &packed_options)?;
let raw_bytes = encode(&player, &raw_options)?;
assert!(raw_bytes.len() > packed_bytes.len());
assert_eq!(decode::<Player>(&raw_bytes, &raw_options)?, decode::<Player>(&packed_bytes, &packed_options)?);
```

预期：成功；`raw_bytes` 比 `packed_bytes` 长，两者解出同一个值。

**解码必须传入与编码一致的 `packed`**：wire 里没有 magic、没有版本头，库不猜格式。`decode::<Player>(&raw_bytes, &packed_options)` 会返回解析类错误（`2 TruncatedInput` 或 `4 InvalidTag`，取决于首个不符处）。

### 1.4 错误是 `Result`，码是稳定整数

```rust
use ace_serialize::error::Error;

if let Err(error) = decode::<Player>(&bytes, &options) {
    eprintln!("decode failed: {error:?} (code {})", error.code());
    if let Error::MissingRequiredField = error { /* 必需字段缺槽 */ }
}
```

预期：成功解码时不进 `if let` 分支；字段缺槽一类失败时打印一行形如 `decode failed: MissingRequiredField (code 15)`，`Error::code()` 的整数可直接对照 §8.3 的码表。

`Error::code()` 返回的整数与 C ABI 错误码同一套（§8.3）。库不 panic、不静默截断。
## 2 数据类型支持

### 2.1 可以直接用的类型

| 类别 | 类型 | 说明 |
|---|---|---|
| 标量 | `bool` `i8` `u8` `i16` `u16` `i32` `u32` `i64` `u64` `f32` `f64` `char` | 零值可省略（不写槽） |
| 字符串 | `String`（`&str` 可直接编码） | 必须 UTF-8；空串仍写槽 |
| 容器 | `Vec<T>`、`BTreeMap<K, V>`、`Option<T>` | `Option<Option<T>>` 编译期拒绝 |
| 指针 | `Box<T>`、`Rc<T>`、`Arc<T>`（std） | `Rc`/`Arc` 按对象身份只写一份 |
| 内部可变性 | `Rc<RefCell<T>>`、`Arc<RwLock<T>>` | 编码时非阻塞取借用/锁 |
| 反向边 | `Weak<T>`、`Option<Weak<T>>` | 表达循环引用 |
| 定宽值 | `schema::value::Guid`、`DateTime`、`DateTimeOffset`、`IntPtr`、`UIntPtr`、`ToolkitsU64` | 见 §2.3 |
| 枚举 | 整数枚举、带载荷多态枚举 | 见 §5 |
| 自定义 | 派生或手写实现的结构体 | 手写见 §9 |

### 2.2 编译期就会被拒绝的

| 写法 | 原因 |
|---|---|
| `isize` / `usize` 字段 | 没有固定 wire 宽度 |
| `i128` / `u128` / `HashMap` / `HashSet` / `BTreeSet` / `VecDeque` | 不在支持集合内 |
| 元组结构体 `struct P(i32)`、单元结构体 | 只支持具名字段结构体 |
| 带生命周期参数的类型 | 不支持 |
| `Option<Option<T>>`（含经类型别名间接嵌套） | 线上无法表达两层 presence |
| 裸露的 `RefCell<T>` / `RwLock<T>` 字段 | 内部可变性需要 `Rc`/`Arc` 提供身份语义 |
| 多态枚举变体里含 `Rc`/`Arc`/`Weak` | 组合不在支持集合内 |
| 整数枚举 `#[repr(C)]` / `#[repr(transparent)]` | `repr` 不在白名单 |

这些诊断都由宏在编译期报出，以 `ace: ` 开头并指向出错字段。

### 2.3 定宽值的 wire 约定

| 类型 | wire |
|---|---|
| `Guid` | 16 字节，RFC 4122 网络字节序 |
| `DateTime` | `u64`：低 62 位 ticks + 高 2 位 Kind |
| `DateTimeOffset` | `i64` clock ticks + `i16` 分钟偏移，共 10 字节 |
| `IntPtr` / `UIntPtr` | 固定 8 字节，不随平台指针宽度变化 |
| `ToolkitsU64` | 定宽 `u64` 包装 |

```rust
use ace_serialize::schema::value::{DateTime, DateTimeOffset, Guid};

#[derive(ace_serialize_derive::AceSerialize, Debug, PartialEq)]
struct Session { token: Guid, created: DateTime, expires: DateTimeOffset }
```

预期：成功往返；三个字段各占一个定宽槽。跨端交换 `Guid` 时按 RFC 4122 网络字节序映射。

## 3 字段编号与可选字段

### 3.1 `order` 是协议，不是排版

```rust
#[derive(AceSerialize, Debug, PartialEq)]
struct Account {
    #[ace(order = 0)] id: u64,
    #[ace(order = 1)] name: String,
    #[ace(order = 4)] email: Option<String>,
    #[ace(skip)]      cache: u64,
}
```

预期：编译成功；`email` 占 4 号，`cache` 仍占 3 号但不写不读。解码后线上字段保持原值，`cache` 恢复为 `0`；**原值 `cache == 0` 时**整个 `Account` 往返相等。

| 规则 | 说明 |
|---|---|
| 不写 `order` | 按**声明序号**自动编号（第 0、1、2… 个字段） |
| 显式 `order` | 非负且放得进 `i32`；与自动编号一起参与重复检测 |
| 发射顺序 | 永远按 `order` 升序，与声明顺序无关 |
| 允许空洞 | `order` 可以是 0/1/4，中间的 2、3 表示「曾经有、现已删」 |
| `#[ace(reserved(2, 3))]` | 类型级锁号，字段不得占用 |
| `#[ace(skip)]` | 不写不读，构造时取 `Default::default()`；**仍占用它的编号** |

最后一条最容易踩：上面例子里 `cache` 是第 4 个字段（序号 3），`skip` 之后依然占 3 号，所以 `email` 必须写 `order = 4`；写成 `order = 3` 会得到编译错误 `error: ace: duplicate order 3`。

### 3.2 可选字段与零值省略

可省略（不写槽）的只有三类：**零值标量（不超过 8 字节）**、`Option::None`、带 `#[ace(default = Variant)]` 且正取该变体的整数枚举；解码时零值标量补零、`None` 恢复 `None`、枚举取默认变体。宽值（超过 8 字节，如 `Guid`、`DateTimeOffset`）与 `String`、bytes、集合、嵌套结构体**仍需槽**，即使内容为空或全零也要写槽与 record。因此「字段是否出现过」只由类型决定：

| 想表达的语义 | 字段类型 |
|---|---|
| 可为空的值 | `Option<T>` |
| 有自然默认值（零值）的值 | 直接用 `T` |
| 有非零默认值的枚举 | 整数枚举 + `#[ace(default = Variant)]`（§5.1） |

### 3.3 版本演进

三个版本是**先后替换**同一个类型定义，不是同时存在。以下三段分属不同版本的同一类型，请分别放进各自的构建单元（或三个独立模块），不要整块复制到同一个文件（同名 `Config` 会重复定义）：

```rust
// v1：只有 volume（order 0）
#[derive(AceSerialize)]
struct Config {
    #[ace(order = 0)] volume: u8,
}

// v2：加字段并给新号；新字段必须可省略，老数据缺槽才能解出默认值
#[derive(AceSerialize)]
struct Config {
    #[ace(order = 0)] volume: u8,
    #[ace(order = 1)] language: Option<String>,
    #[ace(order = 2)] muted: bool,                 // 零值可省略：false 不写槽
}

// v3：删掉 order 2 的字段 → 必须锁号，禁止新字段复用
#[derive(AceSerialize)]
#[ace(reserved(2))]
struct Config {
    #[ace(order = 0)] volume: u8,
    #[ace(order = 1)] language: Option<String>,
}
```

预期：v2 代码读 v1 数据成功（缺 order 1、2 槽 → `language = None`、`muted = false`）；v3 代码读 v2 数据成功（`muted = false` 时 order 2 本来没写槽；`muted = true` 时 order 2 槽作为未知槽被跳过）。

规则：新字段必须是可省略类型（标量/`Option`/带 `default` 的枚举），否则老数据解码返回 `15`；删字段后必须用 `reserved` 永久保留该号，否则新字段会误读老数据的槽；未知字段被机械跳过，不报错。

## 4 属性速查

| 位置 | 写法 | 作用 |
|---|---|---|
| 类型 | `#[derive(AceSerialize)]` | 生成编码/解码/图路径实现 |
| 类型 | `#[ace(reserved(3, 4, 7))]` | 锁定字段号，字段不得占用 |
| 类型（整数枚举） | `#[ace(default = Idle)]` | 该变体可省略，缺席时取它 |
| 字段 | `#[ace(order = 3)]` | 字段号 = 线上 tag |
| 字段 | `#[ace(skip)]` | 不参与序列化，解码时取 `Default` |
| 变体 | `#[ace(tag = 7)]` | 多态枚举的数值判别（`0..=65534`） |
| 变体 | `#[ace(type_name = "Alarm")]` | 多态枚举的字符串判别（65535 逃生口） |

- **共享不是属性**：字段类型写成 `Rc<T>` / `Arc<T>` / `Weak<T>`，库就按对象身份编码（§6）。
- 属性写错位置、重复写、写不存在的名字，宏都在编译期诊断并指向出错字段/变体。

## 5 枚举

两种枚举语义完全不同：**整数枚举**只有判别值、没有载荷；**多态枚举**的变体带载荷，按判别值分派到不同结构。

### 5.1 整数枚举

```rust
use ace_serialize::limits::Options;
use ace_serialize::{decode, encode};
use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(u8)]
enum Job { Warrior = 1, Mage = 2, Rogue = 3 }

#[derive(AceSerialize, Debug, PartialEq)]
#[repr(i16)]
#[ace(default = Idle)]
enum State { Idle = 0, Busy = 1, Done = 9 }

fn main() -> Result<(), ace_serialize::error::Error> {
    let options = Options::default();
    let bytes = encode(&Job::Mage, &options)?;
    assert_eq!(decode::<Job>(&bytes, &options)?, Job::Mage);
    Ok(())
}
```

预期：成功往返。`State` 缺槽时解出 `Idle`（`default` 变体可省略）。

规则：

- 全部变体无载荷；
- `repr` 只能是 `i8`/`u8`/`i16`/`u16`/`i32`/`u32`/`i64`/`u64`；
- **不写 `repr` 时按 `i32` 编码**，判别值超出 `i32` 会编译失败（提示补 `#[repr(i64)]`）；
- 解码遇到不在变体集合内的判别值 → `4 InvalidTag`。

C 侧对应 `AceEnumSchema`：`ace_enum_new` → `ace_enum_set_repr` → `ace_enum_add_variant` →（可选）`ace_enum_set_default_variant`，判别值以 `uint64_t` 位模式承载（§10.5）。

### 5.2 多态枚举

```rust
#[derive(AceSerialize, Debug, PartialEq)]
enum Shape {
    #[ace(tag = 1)] Circle { radius: f64 },
    #[ace(tag = 2)] Rect { w: f64, h: f64 },
    #[ace(tag = 3)] Empty,
}

fn main() -> Result<(), ace_serialize::error::Error> {
    let options = Options::default();
    let value = Shape::Rect { w: 2.0, h: 3.0 };
    let bytes = encode(&value, &options)?;
    assert_eq!(decode::<Shape>(&bytes, &options)?, value);
    Ok(())
}
```

预期：成功往返。

规则：每个变体必须显式写 `#[ace(tag = n)]`（`0..=65534`）或 `#[ace(type_name = "...")]`；**同一枚举不能混用**两种模式；变体只能是单元变体或具名字段变体（不能是元组变体）；变体里不能直接放 `Rc`/`Arc`/`Weak`；未知判别值 → `4 InvalidTag`。

`type_name` 模式把别名字符串写进 wire（65535 逃生口）：

```rust
#[derive(AceSerialize, Debug, PartialEq)]
enum Message {
    #[ace(type_name = "ping")] Ping { seq: u32 },
    #[ace(type_name = "pong")] Pong,
}
```

预期：成功往返。别名字符串是**协议的一部分**：改名等于改协议，老数据解码返回 `4 InvalidTag`。同一枚举可以同时出现在字段、集合元素与 `Option` 内层三个位置（`struct Holder { shape: Shape, history: Vec<Shape>, pending: Option<Shape> }`），三处都能正常往返。

## 6 共享对象与引用

### 6.1 同一实例只写一份

```rust
use std::rc::Rc;
use ace_serialize::limits::Options;
use ace_serialize::{decode, encode};
use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize, Debug, PartialEq)]
struct Target { value: u32 }

#[derive(AceSerialize, Debug, PartialEq)]
struct Pair { left: Option<Rc<Target>>, right: Option<Rc<Target>> }

fn main() -> Result<(), ace_serialize::error::Error> {
    let options = Options::default();
    let shared = Rc::new(Target { value: 7 });
    let value = Pair { left: Some(Rc::clone(&shared)), right: Some(Rc::clone(&shared)) };
    let bytes = encode(&value, &options)?;
    let back: Pair = decode(&bytes, &options)?;

    let (left, right) = (back.left.unwrap(), back.right.unwrap());
    assert!(Rc::ptr_eq(&left, &right));   // 身份被还原成同一实例
    assert_eq!(left.value, 7);
    Ok(())
}
```

预期：成功；两个字段解码后是**同一个**实例（`Rc::ptr_eq` 为真）。

判同按对象身份、不按内容：两个内容相等但不同的实例，解码后仍是两个实例。`Arc<T>` 语义相同（需 `std` feature）。C 侧用 `ace_value_share` 表达同一对象，判同口径是 `ptr ==`（§10.4）。

### 6.2 循环引用与强环

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
    parent: Option<Weak<Node>>,        // 反向边用 Weak，不延长生命周期
}

fn main() -> Result<(), ace_serialize::error::Error> {
    let options = Options::default();
    let root = Rc::new_cyclic(|me| Node {
        value: 1,
        children: vec![Rc::new(Node {
            value: 2,
            children: Vec::new(),
            parent: Some(Weak::clone(me)),
        })],
        parent: None,
    });

    let bytes = encode(&root, &options)?;
    let mut decoder = Decoder::new(&bytes, &options)?;
    let back = decode_cyclic::<Rc<Node>>(&mut decoder)?;
    decoder.finish()?;

    assert_eq!(back.value, 1);
    let up = back.children[0].parent.as_ref().and_then(Weak::upgrade).unwrap();
    assert!(Rc::ptr_eq(&up, &back));   // 回边指回同一实例
    Ok(())
}
```

预期：成功；子节点的 `parent` 升级后与根是同一实例。

要点：

- 正确写法是一条强边（`Rc<T>`）+ 一条 `Weak` 回边；`Weak` 只能出现在结构体内，不能单独作根值。**强引用环禁止构造**：`Rc<RefCell<T>>` 自环、`A→B→A` 全强边，编码返回 `14 CycleUnsupported`，不挂起、不栈溢出。
- **`decode` 还是 `decode_cyclic`**：derive 为含 `Rc`/`Arc`/`Weak` 的类型生成了图路径实现，`decode::<T>` 通常可用；根值本身就是图节点（如 `Rc<Node>`）时用显式两阶段入口 `decode_cyclic`，并在之后调 `decoder.finish()`。
- **借用/锁状态**：`Rc<RefCell<T>>` 编码时 `try_borrow`、`Arc<RwLock<T>>` 编码时 `try_read`；存在活动可变借用或锁被占用时返回 `1 InvalidArgument`，**不阻塞、不自旋**。下面例子的 `result` 即为 `Err(Error::InvalidArgument)`：

```rust
let cell = std::rc::Rc::new(std::cell::RefCell::new(7u32));
let guard = cell.borrow_mut();
let result = encode(&cell, &Options::default());
```

## 7 零分配与复用

### 7.1 `encode_into`：输出与工作区都由调用方提供

```rust
use ace_serialize::encode_into;
use ace_serialize::limits::Options;
use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize, Debug, PartialEq)]
struct Packet { seq: u32, payload: Vec<u8> }

fn main() -> Result<(), ace_serialize::error::Error> {
    let options = Options::default();
    let packet = Packet { seq: 1, payload: vec![1, 2, 3] };

    let mut dst = vec![0u8; 4096];            // 装最终字节
    let mut scratch = vec![0u8; 64 * 1024];   // 装身份表与内部临时数据

    let written = encode_into(&packet, &mut dst, &mut scratch, &options)?;
    assert_eq!(&dst[..written], &ace_serialize::encode(&packet, &options)?[..]);
    Ok(())
}
```

预期：成功；`written` 等于最终字节长度，`dst[..written]` 与 `encode` 输出逐字节相同。

契约：成功时返回值 = 最终字节长度，`dst` 超出部分不被写入；`dst` 容量不足、`scratch` 不足、身份表物理相撞都返回 `1 InvalidArgument`（该码不区分来源）；失败后 `dst` 内容不作承诺（可能已部分写入）。零分配承诺覆盖库自身实现与 derive 路径；调用链含手写 `impl AceEncode` 时以其自身不分配为前提。

### 7.2 `scratch` 的最小容量

```rust
let s = ace_serialize::identity_entry_size();   // 每个身份条目 S 字节
let a = ace_serialize::identity_entry_align();  // 条目区对齐 A
```

预期：两个调用各返回一个 `usize`；`a` 是非零 2 的幂、`s` 是 `a` 的整数倍，具体数值随目标平台与启用的 feature 变化，文档不写死。

需要 `N ≥ 1` 个身份槽时：

```text
对齐前导 P = (A - 切片起址 % A) % A      // 0 <= P < A
最小容量 = P + N × S + 字节区
```

- **零身份槽时不扣也不加前导 `P`**；`A` 是非零 2 的幂，`S` 是 `A` 的整数倍，第 `i` 槽落在 `起址 + P + i × S`；
- `S`/`A` 按目标平台与启用的 feature 求值，不是可写死的固定数值，也不承诺换目标或换 feature 后仍是同一对数值；
- 两者都是 `pub const fn`，可在常量上下文求值（`const ENTRY: usize = ace_serialize::identity_entry_size();`）。`scratch` 不是「越大越好」的推荐值，只是上界哨兵：库按本次实际用到的条目数取用。

### 7.3 `encode_into_probe`：取所需容量

```rust
use ace_serialize::error::Error;
use ace_serialize::encode_into_probe;

let mut dst = vec![0u8; 4096];
let mut scratch = vec![0u8; 1024];
let mut need = 0usize;              // 哨兵：非 scratch 不足的失败不会改写它
loop {
    match encode_into_probe(&packet, &mut dst, &mut scratch, &options, &mut need) {
        Ok(n) => { dst.truncate(n); break; }
        Err(Error::InvalidArgument) if need > scratch.len() => {
            scratch = vec![0u8; need * 2];   // 下一轮重新探测：起址变了，need 必须重算
        }
        Err(error) => return Err(error),
    }
}
```

预期：成功时 `dst` 与 `encode` 逐字节相同；`scratch` 起初不足以身份表时，第一轮失败并写入所需总字节数下界。

`need` 语义：

- 单位是**同一起址下容纳本次调用所需的总字节数**，不是「还差多少」；
- 写 `need` 的那次调用已因装不下而失败，故所写值必大于本次 `scratch.len()`；
- 它只是**本次失败点**的已检查下界，不保证一次就够；
- 只有「`scratch` 不足」会写 `need`；`dst` 不足、非法输入、资源限制等照常返回且**不写** `need`（哨兵保持调用方传入值）；
- 重试要用**同一起址**、长度 ≥ `need` 的新缓冲区，同起址稳定输入下再次失败时报出的 `need` 必大于上次（建议按翻倍扩大）；**换起址必须重新探测**。

### 7.4 `decode_from`：复用已有容器

```rust
use ace_serialize::decode_from;
use ace_serialize::limits::Options;
use ace_serialize_derive::AceSerialize;

#[derive(AceSerialize, Debug, PartialEq)]
struct Batch { name: String, values: Vec<u32> }

fn main() -> Result<(), ace_serialize::error::Error> {
    let options = Options::default();
    let source = Batch { name: String::from("frame"), values: vec![1, 2, 3] };
    let bytes = ace_serialize::encode(&source, &options)?;

    // 上一帧留下的实例：容量已分配过
    let mut out = Batch {
        name: { let mut s = String::with_capacity(64); s.push_str("stale"); s },
        values: { let mut v = Vec::with_capacity(8); v.extend_from_slice(&[9, 9]); v },
    };
    let (name_ptr, values_ptr) = (out.name.as_ptr(), out.values.as_ptr());

    decode_from(&bytes, &mut out, &options)?;
    assert_eq!(out, source);
    assert_eq!(out.name.as_ptr(), name_ptr);      // 容量足够，未重新分配
    assert_eq!(out.values.as_ptr(), values_ptr);
    Ok(())
}
```

预期：成功；`String`/`Vec` 缓冲指针不变（复用已有分配），结果与 `decode` 逐值一致。

复用范围（按责任边界）：

| 位置 | 是否复用 |
|---|---|
| `String` / `Vec<T>` / `Option` / `BTreeMap` | 复用缓冲（`BTreeMap` 保留节点，同键值在原位置复用） |
| `Vec<Vec<T>>` 的内层 | 复用（元素走元素复用路径） |
| 非 shared 的 derive 结构体 | 逐字段写入既有结构体字段 |
| 多态枚举 | 同一判别变体时复用该变体载荷；跨变体整体替换 |
| `Box<T>` 等未覆写类型 | 整体替换 |
| 含 `Rc`/`Arc`/`Weak` 的 derive 类型 | 走图路径，在消息根整体替换 |
| `Some → None`、跨变体切换、缺席字段恢复 `Default` | 整体替换 |

另外：`options.packed` 必须与输入一致；返回 `Err` 时 `out` 内容不作任何承诺（可能已被部分修改），不得当有效值；目标单元格（`RefCell`/`RwLock`）在写入前检查可用性，存在未释放借用、锁中毒或被弃 guard 时返回 `1 InvalidArgument`，不修改该单元格、也不规整其状态（该粒度是单元格而非整个 `out`）。

## 8 错误处理与限额

### 8.1 `Options` 全部字段与默认值

| 字段 | 默认值 | 含义 |
|---|---|---|
| `packed` | `true` | 是否使用 packed 格式 |
| `max_depth` | `256` | 最大嵌套深度，根节点深度为零 |
| `max_build_nesting` | `256` | 同时打开的身份构造闭包数上限；`0` 表示不允许身份节点（仅 Rust 图解码使用） |
| `max_input_bytes` | `64 MiB` | 输入层字节预算（压缩输入与 raw 分别计量） |
| `max_raw_bytes` | `64 MiB` | 解包后 raw 字节预算（含 envelope） |
| `max_decoded_bytes` | `64 MiB` | 物化层预算：每次实际物化 record 时累计其 size |
| `max_records` | `1 << 20` | 输入的 record 数量上限 |
| `max_slots` | `1 << 20` | 输入的 slot 数量上限（含根槽） |
| `max_collection_len` | `1 << 24` | 单个集合元素数或 Map 键值对数上限 |
| `max_references` | `1 << 20` | 引用表条数上限 |
| `max_map_string_key_bytes` | `1 MiB` | Map 字符串键的 UTF-8 字节数上限 |

### 8.2 调限额

`Options` 是普通结构体，按需改写字段：

```rust
use ace_serialize::limits::Options;

// 处理不可信输入：收紧深度与集合，其余取默认
let strict = Options { max_depth: 32, max_collection_len: 4096, ..Options::default() };

// 大存档：放宽字节预算，仍保留深度与集合上限
let generous = Options { max_input_bytes: 256 << 20, max_raw_bytes: 256 << 20, ..Options::default() };
```

预期：超 `max_depth` → `5 DepthExceeded`；超字节/记录/槽/集合/引用预算 → `6 ResourceLimit`。只放宽确实需要的维度：`max_depth`、`max_collection_len`、`max_references` 保持小值，它们是抵御畸形输入的主要屏障。

### 8.3 错误码 `0..16`

| 码 | 名称 | 典型触发 |
|---:|---|---|
| 0 | `OK` | 成功（C ABI 的 `ACE_OK`） |
| 1 | `InvalidArgument` | 参数非法；`dst`/`scratch` 容量不足；活动借用或锁被占用 |
| 2 | `TruncatedInput` | 头/槽/记录/packed 数据被截断 |
| 3 | `UnknownTypeKey` | C ABI 未注册的 schema key（含 `NULL` options 的空白名单） |
| 4 | `InvalidTag` | 非法判别值、槽区间重叠/逆序、非法 presence |
| 5 | `DepthExceeded` | 超过 `max_depth` |
| 6 | `ResourceLimit` | 超过字节/记录/槽/集合/引用预算 |
| 7 | `InvalidReference` | 引用 id 缺失、重复定义或越界 |
| 8 | `InvalidUtf8` | 非法 UTF-8 或非法 Unicode 标量 |
| 9 | `VersionMismatch` | ABI 版本不匹配 |
| 10 | `PanicCaught` | FFI 边界捕获到 panic |
| 11 | `Unsupported` | 请求本期不支持的能力（如 C 侧 `Weak`） |
| 12 | `NotFound` | C ABI 查询无此项（合法查询结果，非截断） |
| 13 | `TypeMismatch` | 动态值无法无损转换成目标 schema 类型 |
| 14 | `CycleUnsupported` | 强引用环无法安全构造 |
| 15 | `MissingRequiredField` | 不可省略字段缺槽 |
| 16 | `DuplicateKey` | Map 解码时同一 key 出现两次 |

错误码是闭集，只能末尾追加。

## 9 手写实现自定义类型

derive 覆盖绝大多数场景；只有需要自定义 wire 表示时才手写。

### 9.1 newtype：给一个标量包一层

```rust
use ace_serialize::decode::Decoder;
use ace_serialize::encode::Encoder;
use ace_serialize::error::Result;
use ace_serialize::schema::{value, AceDecode, AceElement, AceEncode, ElementKind, NonOptional};

#[derive(Debug, PartialEq)]
struct Celsius(f64);

impl AceEncode for Celsius {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_scalar(value::encode_f64(self.0))   // 有字段选择时写槽，否则写根槽
    }
    // 放进 Vec 时用这个；默认实现返回 InvalidArgument，必须覆写
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&value::encode_f64(self.0))
    }
}

impl AceDecode for Celsius {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        Ok(Celsius(value::decode_f64(&d.decode_scalar()?)))
    }
}

impl NonOptional for Celsius {}          // 允许放进 Option<Celsius>

impl AceElement for Celsius {            // 允许当 Vec 元素
    fn element_kind() -> ElementKind { ElementKind::Value { size: 8 } }   // 定宽 8 字节
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.write_element_bytes(&value::encode_f64(self.0))
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        let mut payload = [0u8; 8];
        payload.copy_from_slice(d.read_element_bytes(8)?);
        Ok(Celsius(value::decode_f64(&payload)))
    }
}
```

预期：`Celsius(36.5)` 能当根值，往返相等。`Option<Celsius>` 作为**独立入口的值**可用（只需 `AceEncode`/`AceDecode` + `NonOptional`，上例中已实现）；作为 **derive 结构体字段**则需 `AceDecodeCyclic`；作为 **`Vec` 元素**需 `AceElement`（上例已实现）。

还需注意：

- **零分配**：库对「库自身 + derive 路径」保证零分配；手写实现里的 `Vec`/`String` 分配由实现者自负。**图路径**：要参与两阶段图解码（`Rc`/`Arc` 目标、环），还需实现 `AceDecodeCyclic`（`type Plan` + `decode_plan` + `build_from_plan` 三个成员，签名见 `crates/ace-serialize/src/schema/mod.rs`）。只实现 `AceElement` 的类型不会被接纳为图节点。
- **批量钩子**：`AceElement` 的 `encode_elements_bulk*` / `decode_elements_bulk*` 都有默认实现，不覆写也能正确工作（走逐元素路径）。

### 9.2 对象：手写一个具名字段结构体

```rust
#[derive(Debug, PartialEq)]
struct Point { x: i32, y: i32 }

impl AceEncode for Point {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_object()?;                        // 开帧
        let mut body = || -> Result<()> {
            e.begin_field(0)?;                    // 字段号 0
            self.x.ace_encode(e)?;
            e.check_field_consumed()?;            // 每个字段必须有且只有一次发射
            e.begin_field(1)?;                    // 字段号 1
            self.y.ace_encode(e)?;
            e.check_field_consumed()?;
            Ok(())
        };
        let encoded = body();                     // 先保存错误
        let left = e.leave_object();              // 无论如何都关帧
        encoded?;                                 // 再传播字段错误
        left?;                                    // 最后取 record id
        Ok(())
    }
}

impl AceDecode for Point {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        let payload = d.decode_required_scalar()?;         // 读槽里的 record id
        d.enter_object(u64::from_le_bytes(payload))?;      // 进入对象
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

预期：`Point { x: 7, y: 0 }` 往返相等。缺 `y` 槽时解码补零；但上面手写路径用 `begin_field(1)` 显式开槽，**不做零值省略**（零值省略是 derive 路径的行为，见 §3.2），所以 `(7, 0)` 与 `(7, 3)` 编码等长。

三条必须遵守的约束：

1. `leave_object()` 必须在任何失败路径都被调用，否则编码器状态不闭合；用闭包把「可能失败的字段体」和「必须执行的收尾」分开。
2. `begin_field(n)` 之后必须有且只有一次发射（写标量/写记录/写 `None`）；漏了就是协议 bug，`check_field_consumed()` 会抓住它。
3. `decode` 侧 `begin_field(n)?` 返回 `false` 表示该槽不存在，此时补默认值（本例补零），不要当错误。

### 9.3 各位置需要哪些 trait

同一类型在不同位置的要求不同，按上下文分三组看，不能互相替代。

**A. 独立普通入口（`encode` / `decode`）**——只看 `AceEncode` / `AceDecode`：

| 位置 | 要求 |
|---|---|
| 根值 | `T: AceEncode` / `T: AceDecode` |
| `Option<T>` 的值（内层 `T`） | `T: AceEncode` / `T: AceDecode`，另需 `T: NonOptional`；**不要求** `AceDecodeCyclic` |
| `Vec<T>`（普通入口） | 编码：`T: AceEncode`（`ace_encode_element` 需覆写）；解码：`T: AceElement`；**不要求** `AceDecodeCyclic` |
| `Map<K, V>`（普通入口） | 键**始终** `K: AceElement + Ord`；值：编码 `V: AceEncode`（`ace_encode_element` 需覆写）、解码 `V: AceElement`；**不要求** `AceDecodeCyclic` |

**B. 作为 derive 结构体的字段**——需要图 trait：

| 位置 | 要求 |
|---|---|
| 普通字段 `T` | `AceEncode` + `AceDecode` + `AceDecodeCyclic` |
| 字段 `Option<T>` | 内层 `T` 另需 `NonOptional` |
| 字段 `Vec<T>` | 图解码要求元素 `T: AceDecodeCyclic`；普通编码/解码另要求元素 `T: AceEncode` / `T: AceElement` |
| 字段 `Map<K, V>` | 键**始终** `K: AceElement + Ord`；图解码要求值 `V: AceDecodeCyclic`；普通编码/解码另要求值 `V: AceEncode` / `V: AceElement` |
| 字段 `Rc<T>` / `Arc<T>` | 目标 `T: AceDecodeCyclic + 'static` 且 `T::Plan: 'static`；核心**没有**普通 `AceDecode for Rc<T>` / `Arc<T>` |
| 字段 `Weak<T>` | 图实现只要求 `T: 'static`；**不要求** `T: AceDecodeCyclic` |

**C. 显式图入口（`decode::decode_cyclic`；derive `shared` 类型的 `ace_decode` 也走这条图路径）**——准入只看 `AceDecodeCyclic`：

| 位置 | 要求 |
|---|---|
| `Vec<T>` | `T: AceDecodeCyclic` |
| `Map<K, V>` | 键**始终** `K: AceElement + Ord`；值 `V: AceDecodeCyclic` |
| `Rc<T>` / `Arc<T>` | 目标 `T: AceDecodeCyclic + 'static` 且 `T::Plan: 'static`；只实现 `AceElement` 的类型不是图节点 |
| `Weak<T>` | 目标只要求 `T: 'static`；**不要求** `T: AceDecodeCyclic` |

一句话：普通入口的编码只看 `AceEncode`、解码只看 `AceDecode`（`Option` 内层加 `NonOptional`；`Vec` 普通解码元素需 `AceElement`；`Map` 键始终需 `AceElement + Ord`，值普通解码需 `AceElement`）；一旦当 derive 结构体字段或进图入口，就要 `AceDecodeCyclic`（`Weak<T>` 例外：只要求目标 `'static`）。

依据：`Option` 的普通编解码（`crates/ace-serialize/src/schema/mod.rs:1400–1438`）、`Vec` 的普通解码要求 `AceElement`（`schema/mod.rs:1679–1701`）与图解码要求 `AceDecodeCyclic`（`schema/mod.rs:1212–1222`）、`Map` 的键 `K: AceElement + Ord` 与值在三种入口下的约束（`schema/mod.rs:2080–2085,2100,2132`）、`Rc`/`Arc` 的图解码要求目标 `T: AceDecodeCyclic + 'static` 且 `T::Plan: 'static`、`Weak` 的图实现只要求 `T: 'static`（`schema/mod.rs:1123–1146,1165–1189`）；derive 生成实现给字段的约束见 `crates/ace-serialize-derive/src/gen.rs:1217–1220,1280–1286`。

## 10 C ABI 使用

### 10.1 构建与链接

```sh
cargo build -p ace-serialize-ffi --release
cc -std=c17 -Wall -Wextra -Iinclude tests/c/serialize_model.c \
   -Ltarget/release -lace_serialize_ffi -Wl,-rpath,"$PWD/target/release" \
   -o target/release/serialize_model_c && ./target/release/serialize_model_c
```

预期：链接成功并输出 `serialize_model: N checks, 0 failures`。头文件 `include/ace_serialize.h`（v5）声明 53 个 `ace_*` 函数，错误码 `0..16` 闭集、只能末尾追加。

### 10.2 调用顺序

```text
ace_options_new → ace_schema_new / ace_type_* / ace_schema_add_field   （建 schema 图）
  → ace_options_add_schema                                             （深拷贝进 options）
  → ace_value_* / ace_object_set / ace_list_add / ace_value_share      （造值）
  → ace_serialize / ace_deserialize
  → ace_buffer_free / ace_value_free / ace_options_free / ace_schema_free
```

### 10.3 完整示例：注册 schema、序列化、反序列化

```c
#include "ace_serialize.h"
#include <stdio.h>

int main(void) {
    AceOptions *options = NULL;
    AceSchema *schema = NULL;
    AceTypeRef *id_type = NULL;
    AceTypeRef *name_type = NULL;
    AceValue *player = NULL;
    AceBuffer out = {0};

    /* 1. options：NULL options = 默认限额 + 空白名单（任何 key 都查不到 → 3） */
    if (ace_options_new(&options) != ACE_OK) return 1;
    if (ace_options_set_packed(options, 1) != ACE_OK) return 1;   /* 1 = packed（默认） */

    /* 2. schema：field_id 即 order；add_field 成功即收编 field_type */
    if (ace_schema_new("example.player", &schema) != ACE_OK) return 1;
    if (ace_type_prim(ACE_FK_U64, &id_type) != ACE_OK) return 1;
    if (ace_type_str(&name_type) != ACE_OK) return 1;
    if (ace_schema_add_field(schema, 0, id_type) != ACE_OK) return 1;
    if (ace_schema_add_field(schema, 1, name_type) != ACE_OK) return 1;

    /* 3. 白名单：深拷贝整个 schema 图进 options */
    if (ace_options_add_schema(options, schema) != ACE_OK) return 1;

    /* 4. 值：成功 set = 父对 child 增加一个引用；调用方句柄仍须自己 free */
    if (ace_value_object(NULL, &player) != ACE_OK) return 1;
    {
        AceValue *id = NULL;
        AceValue *name = NULL;
        if (ace_value_u64(7, &id) != ACE_OK) return 1;
        if (ace_value_str((const uint8_t *)"ada", 3, &name) != ACE_OK) return 1;
        if (ace_object_set(player, 0, id) != ACE_OK) return 1;
        if (ace_object_set(player, 1, name) != ACE_OK) return 1;
        ace_value_free(id);
        ace_value_free(name);
    }

    /* 5. 序列化：字节在 out.ptr / out.len；失败时 out 为空 */
    if (ace_serialize("example.player", player, options, &out) != ACE_OK) return 1;
    printf("encoded %zu bytes\n", out.len);

    /* 6. 反序列化：同一份字节解成新的 AceValue 树 */
    {
        AceValue *back = NULL;
        const AceValue *field = NULL;
        uint64_t id = 0;
        if (ace_deserialize("example.player", out.ptr, out.len, options, &back) != ACE_OK) return 1;
        if (ace_object_get(back, 0, &field) != ACE_OK) return 1;   /* 借用视图：不得 free */
        if (ace_value_get_u64(field, &id) != ACE_OK) return 1;
        printf("decoded id=%llu\n", (unsigned long long)id);
        ace_value_free(back);
    }

    /* 7. 释放：AceBuffer 清零结构、二次 free 安全；其余句柄只释放一次 */
    ace_buffer_free(&out);
    ace_value_free(player);
    ace_options_free(options);
    ace_schema_free(schema);
    return 0;
}
```

预期：两条 codec 调用都返回 `ACE_OK`，打印 `decoded id=7`。`ace_object_set` 与 `ace_schema_add_field` 的 `field_id` 就是 Rust 侧的 `order`。复合字段按同一套路加：`ace_type_option(inner, &out)`、`ace_type_list(elem, &out)`、`ace_type_map(key, value, &out)`、`ace_type_object(schema, &out)`、`ace_type_enum(enum_schema, &out)`，再把得到的 `AceTypeRef` 交给 `ace_schema_add_field`；Map 的 ABI 值表示是 `List<Object{k → order 0, v → order 1}>`。

### 10.4 值构造与所有权

| 操作 | 语义 |
|---|---|
| `ace_value_object(NULL, &out)` | 造无自带 `type_key` 的对象；`type_key` 由 `ace_serialize` 外给 |
| `ace_object_set(parent, field_id, child)` | 成功 = 父对 child 增加一个引用；**调用方句柄仍有效，须自行 `ace_value_free`** |
| `ace_list_add(list, child)` | 同上 |
| `ace_object_get` / `ace_list_at` | 借用视图：寿命 ≤ 父，父修改或释放后失效，不得 free，不得再 set/add 收编 |
| `ace_value_share(value, &out)` | `out` 指向同一节点、引用计数 +1，用于表达「两个字段是同一对象」 |
| `ace_buffer_free` / 其余 `*_free` | 仅 `AceBuffer` 承诺 free 后清零结构、同一已清零结构二次 free 安全；其余是拥有型句柄，只释放一次 |

`ace_value_share` 后判同的标准是 `ptr ==`：`ace_value_share(inner, &shared)` → `ace_object_set(value, 1, shared)` → `ace_value_free(shared)`。

预期：成功；反序列化后字段 0 与字段 1 的 `AceValue*` 满足 `ptr ==`。身份只承诺对象与引用类容器（list/map/str/bytes），标量与宽值 record 不承诺；身份范围仅限一次消息/一次反序列化，不得跨调用缓存指针。

### 10.5 整数枚举与限额

```c
AceEnumSchema *color = NULL;
if (ace_enum_new("example.color", &color) != ACE_OK) return 1;
if (ace_enum_set_repr(color, ACE_FK_I8) != ACE_OK) return 1;    /* 不设则按 i32 */
if (ace_enum_add_variant(color, 1) != ACE_OK) return 1;         /* Red */
if (ace_enum_add_variant(color, 2) != ACE_OK) return 1;         /* Green */
if (ace_enum_set_default_variant(color, 1) != ACE_OK) return 1; /* Red 可省略 */

AceTypeRef *color_type = NULL;
if (ace_type_enum(color, &color_type) != ACE_OK) return 1;      /* 借用 enum，不收编 */
if (ace_schema_add_field(schema, 4, color_type) != ACE_OK) return 1;

/* depth, input_bytes, raw_bytes, decoded_bytes, records, slots, collection_len,
   references, map_string_key_bytes —— 9 个上限一次设置 */
if (ace_options_set_limits(options, 32, 1u << 20, 1u << 20, 1u << 20,
                           (size_t)1 << 16, (size_t)1 << 16, (size_t)1 << 12,
                           (size_t)1 << 12, 4096) != ACE_OK) return 1;
```

预期：成功。判别值一律以 `uint64_t` **位模式**承载：`repr(i8) = -1` 传 `0xFF`，`repr(i32) = -1` 传 `0xFFFFFFFF`；超位宽或重复 tag → `1`；解码遇到不在已注册集合内的判别值 → `4`。超深度 → `5`，超其余预算 → `6`。

`ace_identity_entry_size()` / `ace_identity_entry_align()` 发布 §7.2 折算用的 `S`/`A`，值随目标与 feature 变化，故用函数而非常量发布。

`ace_options_set_build_nesting` 接受并保存该值，但 **C 侧解码不消费它**：C 只承诺树/共享 DAG，无强环、无 `Weak`，不存在身份构造闭包峰值；C 侧结构深度只由 `max_depth` 约束。

### 10.6 释放顺序

```text
值（ace_value_free）→ options（ace_options_free）→ 引用方 schema（ace_schema_free）
  → 被 ace_type_object / ace_type_enum 借用的实体（ace_schema_free / ace_enum_free）
```

`ace_options_add_schema` 完成后库内已有独立深拷贝，调用方侧 `AceSchema`/`AceTypeRef` 可随时释放，不影响已注册内容。被 `ace_type_object`/`ace_type_enum` 借用的 schema/枚举必须**晚于**引用它的 `AceTypeRef`（含已被其它 schema 收编者）释放：先 free 引用方 schema，再 free 被引用实体。节点收编规则：`ace_schema_add_field` 成功即收编 `field_type`（子句柄失效、归 schema 所有、不得再 free）；`ace_type_list`/`ace_type_map`/`ace_type_option` 成功即收编参数里的子节点；`ace_type_object`/`ace_type_enum` 只取借用、不收编；`ace_type_free` 仅对未被任何 schema 收编的节点合法。

## 11 常见问题

### 11.1 返回 `13 TypeMismatch`

1. 值的 kind 与 schema 字段类型不匹配（schema 要 `str`，值给了 `i64`）。
2. `f64` → `f32` 的无损映射：有限值必须能精确表示为 `f32`；`NaN` 仅在可映射的 payload 上接受，否则 `13`。
3. `type_key` 不一致：对象值自身没有 `type_key` 而它所在位置的 schema 与白名单同 key 注册项布局不同，或值里的 `type_key` 与位置 schema 不符 → `13`。

### 11.2 返回 `15 MissingRequiredField`

该字段不可省略（标量/`Option`/带 `default` 的枚举之外的必写字段），但输入里没有对应槽。常见原因：写数据用的是旧版本结构体（少了这个字段）；字段号改过，老数据的槽落在别的号上；用了 `#[ace(skip)]` 后又把它当必写字段读。修法：把新字段改成可省略类型，或锁号后按 §3.3 演进。

### 11.3 返回 `16 DuplicateKey`

Map 解码时同一个 key 出现两次。通常是编码端把「键值对列表」当 Map 写，或值里手工塞了重复键。修法：编码前保证键唯一（Rust 侧用 `BTreeMap` 天然唯一）。

### 11.4 返回 `5 DepthExceeded`

嵌套超过 `max_depth`（默认 256，根深度为 0）。查：递归结构是否真的这么深（图编码里每个子对象都加深度）；是否把「元素位置的对象」误当普通字段（集合元素处有一层额外深度）；C 侧深度只由 `max_depth` 约束，`ace_options_set_build_nesting` 对此无效。修法：确认数据形状后调大 `max_depth`，不要设成极大值。

### 11.5 返回 `6 ResourceLimit`

超了某一项预算，按现象分类：

| 现象 | 可能命中的上限 |
|---|---|
| 大数组/大 Map 解码失败 | `max_collection_len` |
| 字符串键很长 | `max_map_string_key_bytes` |
| 输入文件本身很大 | `max_input_bytes` / `max_raw_bytes` |
| 解出来对象很多 | `max_records` / `max_slots` |
| 共享对象很多 | `max_references` |
| 物化字节总量大 | `max_decoded_bytes` |

修法：逐步单独放宽命中项以确认归因，再决定是否放宽；不要一次把全部上限调大。

### 11.6 `packed` 和 `raw` 怎么选

- 存档、网络包、体积敏感 → `packed`（默认）。
- 需要跳读/随机访问、定长槽或人眼对齐调试 → `raw`（每个字段固定 16 字节槽）；同一份数据两端格式必须一致，解码时必须声明正确格式。

两种格式语义完全一致，只换承载；`encode_into` 的 `options.packed` 同样决定输出格式。

### 11.7 C 侧返回 `3 UnknownTypeKey`

没有调用 `ace_options_add_schema` 注册该 `type_key`；`options` 传了 `NULL`（等价于空白名单，全部拒绝）；注册时的 `type_key` 与实际传给 `ace_serialize`/`ace_deserialize` 的字符串不一致（含大小写与末尾空格）。`type_key` 由调用方维护，是稳定 UTF-8 字面量、NUL 结尾、不含内嵌 NUL，不是任何语言的类型名。

### 11.8 返回 `14 CycleUnsupported`

构造了强引用环（对象被 `set`/`add` 进自身或祖先）。这是编码期检测，不是未定义行为。修法：Rust 侧环里至少一条边改成 `Weak`；C 侧不要 `ace_object_set(obj, n, obj)` 或让祖先成为后代的成员（C 只承诺树/共享 DAG）。已构造的强环，`ace_value_free` 按引用计数语义不保证释放。

### 11.9 `decode_from` 之后容器指针变了

说明该位置走的是「整体替换」而非复用路径，对照 §7.4 的表：`Box<T>` 等未覆写类型；含 `Rc`/`Arc`/`Weak` 的 derive 类型（在消息根整体替换）；`Some → None`、跨变体切换、缺席字段恢复 `Default`。这些情况下容量复用不在承诺范围内。

## 12 速查表

### 12.1 限额默认值与错误码

限额默认值见 §8.1，错误码：

```text
0 OK              1 InvalidArgument     2 TruncatedInput    3 UnknownTypeKey
4 InvalidTag      5 DepthExceeded       6 ResourceLimit     7 InvalidReference
8 InvalidUtf8     9 VersionMismatch    10 PanicCaught      11 Unsupported
12 NotFound      13 TypeMismatch       14 CycleUnsupported 15 MissingRequiredField
16 DuplicateKey
```

### 12.2 一句话记住

- 字段号就是协议：`order` 升序发射，`skip` 也占号，删字段要 `reserved` 锁号。
- 要共享就写 `Rc`/`Arc`，要环就补 `Weak` 回边，全强边返回 `14`。
- 要零分配用 `encode_into`；`scratch` 不够用 `encode_into_probe` 取 `need`，同起址按翻倍重试。
- 要复用容器用 `decode_from`；失败后 `out` 不得当有效值。
- 解码必须声明 `packed` 还是 `raw`，wire 里没有标识位。
