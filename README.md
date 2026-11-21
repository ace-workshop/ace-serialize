# ace-serialize

标签驱动的二进制序列化库：Rust 核心（`no_std + alloc`）+ C ABI。用「schema + 字段号 + 槽」描述数据布局，编码结果紧凑、可零分配写入，解码可复用已有容器容量，并支持结构体、枚举、多态对象与共享引用图。

## 能做什么

- **零分配编码**：`encode_into(value, dst, scratch, options)` 把结果直接写进调用方给的 `dst`，工作区由 `scratch` 承载；库自身与 derive 生成代码在这条路径上不分配（手写实现以自身不分配为前提）。
- **容量复用解码**：`decode_from(src, out, options)` 把输入写进已有实例，`String`/`Vec`/`BTreeMap` 等容器在容量足够时保留原分配。
- **schema 驱动**：`#[derive(AceSerialize)]` 为结构体生成编解码实现，字段用 `#[ace(order = n)]` 定号。
- **两种枚举**：整数枚举按定宽 `repr` 写判别值；带载荷的多态枚举按 `type_key`/`tag` 分派到具体变体。
- **共享与引用**：同一次消息内按对象身份（Rust 侧 `Rc`/`Arc` 同一实例、C 侧 `ace_value_share` 后 `ptr ==`）只写一份，解码还原成同一实例；反向边用 `Weak`，强环禁止构造。
- **两种输出**：默认 `packed`（无损压缩，体积小），`packed: false` 输出 `raw`（定长槽，可跳读）。
- **限额防护**：深度、字节、记录、槽、集合、引用条数、Map 键长度全部有上限，逐项返回稳定错误码 `0..16`。
- **C ABI**：`include/ace_serialize.h`（v5）声明 53 个 `ace_*` 函数，错误码为 `0..16` 闭集，只能末尾追加。

## 安装

本库未发布到 crates.io，按路径依赖引用（或作为 workspace 成员）：

```toml
[dependencies]
ace-serialize = { path = "crates/ace-serialize" }
ace-serialize-derive = { path = "crates/ace-serialize-derive" }
```

core 不依赖 derive：`encode`/`decode` 来自 `ace-serialize`，`#[derive(AceSerialize)]` 来自 `ace-serialize-derive`，两个都要加。

核心默认 `no_std + alloc`。需要 `Arc`/`RwLock` 等 `std` 类型时打开 feature：

```toml
ace-serialize = { path = "crates/ace-serialize", features = ["std"] }
```

## 快速开始

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

`Options::default()` 的 `packed` 为 `true`。改成 `raw` 只需换一个 `Options`，并且**解码必须用与编码相同的设置**（wire 里没有标识位）：

```rust
let raw = Options { packed: false, ..Options::default() };
let raw_bytes = encode(&player, &raw)?;
let back: Player = decode(&raw_bytes, &raw)?;   // 用 Options::default() 解这里会返回错误
```

预期：成功；`raw_bytes` 比 `bytes` 长（raw 每个字段占一个定长槽）。

## 零分配编码与复用解码

需要「编码时不分配」时，把输出缓冲和工作区都交给库：

```rust
use ace_serialize::{
    decode_from, encode_into, encode_into_probe, identity_entry_align, identity_entry_size,
    limits::Options,
};

let options = Options::default();

// dst 装最终字节，scratch 装身份表与内部临时数据
let mut dst = vec![0u8; 4096];
let mut scratch = vec![0u8; 64 * 1024];

let written = encode_into(&player, &mut dst, &mut scratch, &options)?;
let payload = &dst[..written];               // 成功时 written == 最终字节长度
```

`scratch` 的最小容量按库发布的两个数值折算：每个身份条目 `S = identity_entry_size()` 字节，条目区起点按 `A = identity_entry_align()` 对齐。确实需要 `N ≥ 1` 个身份槽时，容量为「对齐前导 `P` + `N × S` + 字节区」，其中 `P = (A - 切片起址 % A) % A` 且 `0 <= P < A`；**零身份槽时既不扣也不加 `P`**。长度相同、起址不同的两个 `scratch` 可用容量可能不同。

容量不够时 `encode_into` 返回 `1 InvalidArgument`；想知道需要多少，用探测入口（与 `encode_into` 同一条编码路径，每次调用都是一次完整编码）：

```rust
let mut need = 0usize;
match encode_into_probe(&player, &mut dst, &mut scratch, &options, &mut need) {
    Ok(_n) => { /* 成功，写入 n 字节 */ }
    Err(ace_serialize::error::Error::InvalidArgument) => {
        // 仅「scratch 不足」这一种失败会写 need；need 是「同一起址下所需总字节数」，
        // 不是「还差多少」，且它必大于本次 scratch.len()，也不保证一次就够。
        // 重试要用同一起址、长度 ≥ need 的缓冲（建议按翻倍扩大）；
        // 换起址必须重新探测：对齐前导随起址变化，同长度不同起址可用容量可能不同。
    }
    Err(other) => return Err(other),
}
```

注意 `1 InvalidArgument` 不区分「`dst` 不够」「`scratch` 不够」与「身份表相撞」；失败后 `dst` 内容不作承诺（可能已部分写入），不得当作有效输出。

反过来，解码到已有实例时用 `decode_from`：

```rust
let mut player = Player {
    id: 0,
    name: { let mut s = String::with_capacity(64); s.push_str("stale"); s },
    score: None,
    version: Version { major: 0, minor: 0 },
};

decode_from(&bytes, &mut player, &options)?;   // 成功即 player 已按输入更新
```

预期：成功；`name` 等容器在容量足够时缓冲指针不变（复用已有分配）。复用只覆盖「实际走到复用、且未被整体替换」的缓冲：`String`/`Vec`/`Option`/`BTreeMap`、非 shared 的 derive 结构体与同变体的多态枚举会复用；`Box<T>`、含 `Rc`/`Arc`/`Weak` 的 derive 类型、`Some → None`、跨变体切换与缺席字段恢复 `Default` 会整体替换。失败时 `out` 已被部分修改，不得当有效值使用。

`encode_into` 的零分配承诺覆盖库自身实现与 derive 路径；调用链里含手写 `impl AceEncode` 时，以该手写实现自身不分配为前提。

## C ABI

头文件 `include/ace_serialize.h`（v5）是唯一权威：调用顺序为 options → schema → type → value → serialize/deserialize → free。

先构建 cdylib：

```sh
cargo build -p ace-serialize-ffi --release
cc -std=c17 -Wall -Wextra -Iinclude tests/c/serialize_model.c \
   -Ltarget/release -lace_serialize_ffi -Wl,-rpath,"$PWD/target/release" \
   -o target/release/serialize_model_c && ./target/release/serialize_model_c
```

完整示例（注册 schema、造值、序列化、反序列化、逐句柄释放）：

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

    /* 1. options：NULL options = 默认限额 + 空白名单（任何 key 都查不到） */
    if (ace_options_new(&options) != ACE_OK) return 1;
    if (ace_options_set_packed(options, 1) != ACE_OK) return 1;  /* 1 = packed（默认） */

    /* 2. schema：field_id 即 order；add_field 成功即收编 field_type */
    if (ace_schema_new("example.player", &schema) != ACE_OK) return 1;
    if (ace_type_prim(ACE_FK_U64, &id_type) != ACE_OK) return 1;
    if (ace_type_str(&name_type) != ACE_OK) return 1;
    if (ace_schema_add_field(schema, 0, id_type) != ACE_OK) return 1;
    if (ace_schema_add_field(schema, 1, name_type) != ACE_OK) return 1;

    /* 3. 白名单：深拷贝整个 schema 图进 options，之后调用方侧结构可随时释放 */
    if (ace_options_add_schema(options, schema) != ACE_OK) return 1;

    /* 4. 值：成功 set = 父对 child 增加一个引用，调用方句柄仍须自己 free */
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

    /* 5. 序列化：字节在 out.ptr / out.len */
    if (ace_serialize("example.player", player, options, &out) != ACE_OK) return 1;
    printf("encoded %zu bytes\n", out.len);

    /* 6. 反序列化：同一份字节解成新的 AceValue 树 */
    {
        AceValue *back = NULL;
        const AceValue *field = NULL;
        uint64_t id = 0;
        if (ace_deserialize("example.player", out.ptr, out.len, options, &back) != ACE_OK) return 1;
        if (ace_object_get(back, 0, &field) != ACE_OK) return 1;   /* 借用视图，不得 free */
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

预期：`ace_serialize` 与 `ace_deserialize` 均返回 `ACE_OK`，打印的 `id=7`。

两份 C 端用法可对照 `tests/c/serialize_model.c`（schema/值构造与错误码）与 `tests/c/deserialize_model.c`（反序列化、共享 `ptr ==`、释放顺序）。

错误码 `0..16` 闭集，逐码一句话：

| 码 | 名称 | 含义 |
|---:|---|---|
| 0 | `ACE_OK` | 成功 |
| 1 | `ACE_INVALID_ARGUMENT` | 参数非法；`dst`/`scratch` 容量不足；借用或锁被占用 |
| 2 | `ACE_TRUNCATED_INPUT` | 输入被截断 |
| 3 | `ACE_UNKNOWN_TYPE_KEY` | 该 `type_key` 未注册（含 `NULL` options 的空白名单） |
| 4 | `ACE_INVALID_TAG` | 非法判别值、槽区间非法、非法 presence |
| 5 | `ACE_DEPTH_EXCEEDED` | 超过 `max_depth`（默认 256） |
| 6 | `ACE_RESOURCE_LIMIT` | 超过字节/记录/槽/集合/引用预算 |
| 7 | `ACE_INVALID_REFERENCE` | 引用 id 缺失、重复或越界 |
| 8 | `ACE_INVALID_UTF8` | 非法 UTF-8 或非法 Unicode 标量 |
| 9 | `ACE_VERSION_MISMATCH` | ABI 版本不匹配 |
| 10 | `ACE_PANIC_CAUGHT` | FFI 边界捕获到 panic |
| 11 | `ACE_UNSUPPORTED` | 请求了本层不支持的能力（如 `Weak`） |
| 12 | `ACE_NOT_FOUND` | 查询无此项（合法查询结果，非截断） |
| 13 | `ACE_TYPE_MISMATCH` | 动态值无法无损转换成目标 schema 类型 |
| 14 | `ACE_CYCLE_UNSUPPORTED` | 强引用环无法安全构造 |
| 15 | `ACE_MISSING_REQUIRED_FIELD` | 不可省略字段缺槽 |
| 16 | `ACE_DUPLICATE_KEY` | Map 解码遇重复 key |

## 常用属性

| 位置 | 写法 | 作用 |
|---|---|---|
| 类型 | `#[derive(AceSerialize)]` | 生成编解码与图路径实现 |
| 类型 | `#[ace(reserved(2, 3))]` | 锁定字段号，字段不得占用（删字段后保留号） |
| 类型（整数枚举） | `#[ace(default = Idle)]` | 该变体可省略，缺席时取它 |
| 字段 | `#[ace(order = 4)]` | 字段号 = 线上 tag；不写则按声明序号自动编号 |
| 字段 | `#[ace(skip)]` | 不写不读，构造时取 `Default::default()`；**仍占用它的编号** |
| 变体 | `#[ace(tag = 7)]` | 多态枚举的数值判别（`0..=65534`） |
| 变体 | `#[ace(type_name = "Alarm")]` | 多态枚举的字符串判别（65535 逃生口） |

属性写错位置或重复，宏在编译期报以 `ace: ` 开头、指向该字段/变体的诊断。共享身份不靠属性：字段类型写成 `Rc<T>` / `Arc<T>` / `Weak<T>`，库就按对象身份编码。

## 支持的类型

可以直接当字段或根值使用：

| 类别 | 类型 |
|---|---|
| 标量 | `bool` `i8` `u8` `i16` `u16` `i32` `u32` `i64` `u64` `f32` `f64` `char` |
| 字符串 | `String`（`&str` 可直接编码） |
| 容器 | `Vec<T>`、`BTreeMap<K, V>`、`Option<T>`（`Option<Option<T>>` 编译期拒绝） |
| 指针与共享 | `Box<T>`、`Rc<T>`、`Arc<T>`（std）、`Rc<RefCell<T>>`、`Arc<RwLock<T>>` |
| 反向边 | `Weak<T>`、`Option<Weak<T>>` |
| 定宽值 | `schema::value::Guid`、`DateTime`、`DateTimeOffset`、`IntPtr`、`UIntPtr` |
| 枚举 | 整数枚举、带载荷多态枚举 |
| 自定义 | 派生或手写实现的结构体 |

编译期会被拒绝：

| 写法 | 原因 |
|---|---|
| `isize` / `usize` 字段 | 没有固定 wire 宽度 |
| `i128` / `u128` / `HashMap` / `HashSet` / `BTreeSet` / `VecDeque` | 不在支持集合内 |
| 元组结构体 `struct P(i32)`、单元结构体 | 只支持具名字段结构体 |
| 带生命周期参数的类型 | 不支持 |
| `Option<Option<T>>` | 线上无法表达两层 presence |
| 裸露的 `RefCell<T>` / `RwLock<T>` 字段 | 内部可变性需要 `Rc`/`Arc` 提供身份语义 |
| 多态枚举变体里含 `Rc`/`Arc`/`Weak` | 组合不在支持集合内 |
| 整数枚举 `#[repr(C)]` / `#[repr(transparent)]` | `repr` 不在白名单 |

可省略（不写槽）的只有三类：**零值标量（不超过 8 字节）**、`Option::None`、带 `#[ace(default = Variant)]` 且正取该变体的整数枚举；解码时零值标量补零、`None` 恢复 `None`、枚举取默认变体。宽值（超过 8 字节，如 `Guid` 16B、`DateTimeOffset` 10B）与字符串、bytes、集合、嵌套结构体**仍需槽**：即使内容为空或全零，也必须写字段槽与 record。

## 限制

1. **强环禁止构造**：不得把对象 `set`/`add` 进自身或祖先。若已构造，`ace_serialize` 返回 `14`，且 `ace_value_free` 对强环按引用计数语义不保证释放。正确写法是一条强边 + 一条 `Weak` 回边。
2. **C ABI 不表达 `Weak`**：该层无对应物，请求返回 `11`。
3. **值无自身 `type_key` 且位置 schema 与白名单同 key 注册项布局不同**：编码返回 `13`。
4. **`max_build_nesting` 只对 Rust 图解码生效**：`ace_options_set_build_nesting` 接受并保存该值，但不影响 C 侧解码结果与错误码；C 侧结构深度仍由 `max_depth` 约束，超限返回 `5`。C 侧需要图解码时须自行封装。
5. **wire 与其它实现不保证互通**：没有 magic、没有版本头，版本协商靠 `ace_abi_version`/`ace_wire_version` 查询，不能从字节识别。
6. **其它语言需自行封装**：本仓库只提供 Rust 核心与 C ABI，不含任何托管绑定。

## 构建与测试

```sh
cargo test -p ace-serialize                          # 核心 crate 全部测试
cargo check -p ace-serialize --no-default-features   # no_std 构建
cargo test -p ace-serialize-ffi                      # C ABI 单元测试
cargo build -p ace-serialize-ffi --release           # 产出 cdylib 供 C 测试链接

cc -std=c17 -Wall -Wextra -Iinclude tests/c/serialize_model.c \
   -Ltarget/release -lace_serialize_ffi -Wl,-rpath,"$PWD/target/release" \
   -o target/release/serialize_model_c && ./target/release/serialize_model_c

python3 tools/gen_golden.py --check                  # golden 数据校验
```

`tests/c/` 下的 C 程序按头文件的真实调用序列编写，覆盖 schema、值构造、序列化、反序列化、共享身份与错误路径。

## 性能

同一批用例、同一台机器、同配置下做 8 次交错计时并取中位数：20/20 个用例耗时不超过基线，最差一项为 `g.shared` 的 raw 解码 `0.881×`，最好一项为 `a.int` 的解码 `0.087×`（倍数为相对基线的耗时比）。该结果**仅限已测样本，不外推**到其它数据形状、环境或平台。

## 文档

- [`docs/guide.md`](docs/guide.md)——使用指南：属性、枚举、共享、零分配、C ABI、常见问题
- [`docs/architecture.md`](docs/architecture.md)——结构、数据流与 wire/身份设计
- [`include/ace_serialize.h`](include/ace_serialize.h)——C ABI 头（v5），错误码与全部 `ace_*` 声明

## 隐私

本库离线运行、不联网、不采集任何使用数据，详见 [`PRIVACY.md`](PRIVACY.md)。

## 许可

双许可，任选其一：`MIT OR Apache-2.0`。条款见 [`LICENSE-MIT`](LICENSE-MIT) 与 [`LICENSE-APACHE`](LICENSE-APACHE)。
