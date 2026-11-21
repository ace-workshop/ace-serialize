# ace-serialize

Rust 版 LSerialize（标签驱动的序列化库）：`no_std + alloc` 核心，另提供 C ABI 供 C#/C 调用。

## 特性

- 零分配编码：`encode_into(value, dst, scratch, options)` 把结果写入调用方提供的 `dst`，工作区与身份/引用追踪状态全部由 `scratch` 承载；库自身与 derive 生成路径保证零分配，手写 `AceEncode`/`AceElement` 实现由实现者自行负责是否分配。
- 就地复用解码：`decode_from(src, out, options)` 支持复用调用方已有容器的容量，适用范围与容量条件见 `decode_from` 文档。
- derive 标签：`#[derive(AceSerialize)]` 配合 `#[ace(...)]` 描述结构与字段标签；手写实现走 `AceEncode` / `AceDecode` trait。
- 共享身份：同一次消息内按 `ReferenceEquals` 语义识别同一对象；C 侧用 `ace_value_share` 表达，判同口径为 `ptr ==`。
- 多态对象：按 `type_key` 分派到已注册的 schema，解码得到对应具体类型。
- 两阶段图解码：先建身份图再回填引用；强环禁止构造，已构造时编码返回 `14`。
- C ABI：`include/ace_serialize.h`（v5）声明 53 个 `ace_*` 函数，错误码为 `0..16` 闭集，只能末尾追加。
- `no_std + alloc`：核心不依赖 `std`，`std` 是可选 feature。

## 快速开始

Rust（依赖本仓库的 `ace-serialize` crate）：

```rust
use ace_serialize::{decode, encode, limits::Options};

#[derive(ace_serialize_derive::AceSerialize)]
#[ace(type_key = "example.player")] /* derive 属性语法的权威说明见 crates/ace-serialize-derive */
pub struct Player {
    pub id: u64,
    pub name: String, /* no_std 构建下即 alloc::string::String */
}

fn main() -> ace_serialize::error::Result<()> {
    let options = Options::default();
    let player = Player { id: 7, name: String::from("ada") };

    let bytes = encode(&player, &options)?;       /* 默认 packed 输出；options.packed = false 则 raw */
    let back: Player = decode(&bytes, &options)?; /* decode 的完整签名见 docs/architecture.md */
    let _ = back;
    Ok(())
}
```

不需要分配时改用 `encode_into`（`dst` + `scratch` 均由调用方提供），容量不足用 `encode_into_probe` 取 `need` 下界。

C：

```c
#include "ace_serialize.h"
#include <string.h>

AceOptions *options = NULL;
AceSchema *schema = NULL;
AceTypeRef *id_type = NULL;
AceValue *value = NULL;
AceValue *id = NULL;
AceBuffer out = {0};

ace_options_new(&options);
ace_schema_new("example.player", &schema);
ace_type_prim(ACE_FK_U64, &id_type);
ace_schema_add_field(schema, 0, id_type);   /* field_id 即 order；成功即收编 id_type */
ace_options_add_schema(options, schema);    /* 深拷贝整个 schema 图进入 options */

ace_value_object(NULL, &value);             /* type_key 由 ace_serialize 外给 */
ace_value_u64(7, &id);
ace_object_set(value, 0, id);               /* 成功 = 父对 child 增加一个引用 */
ace_value_free(id);                         /* 调用方自有句柄仍须自行 free */

ace_serialize("example.player", value, options, &out); /* 字节在 out.ptr / out.len */
ace_buffer_free(&out);
ace_value_free(value);

ace_schema_free(schema);                    /* 深拷贝后调用方侧结构可随时释放 */
ace_options_free(options);
```

完整错误码处理与解引用见 `ace_deserialize` 与 `tests/c/serialize_model.c`。

## 仓库布局

| 路径 | 作用 |
| --- | --- |
| `crates/ace-serialize` | 核心库（`no_std + alloc`），编码/解码、wire 格式、限额、schema/derive 契约 |
| `crates/ace-serialize-derive` | `#[derive(AceSerialize)]` 与 `#[ace(...)]` 的 proc-macro 实现 |
| `crates/ace-serialize-ffi` | C ABI 实现，产出 cdylib |
| `include/ace_serialize.h` | C ABI 头（v5 冻结），错误码与全部 `ace_*` 声明 |
| `tests/c/` | C 侧测试程序，按头文件真实调用序列编写 |
| `tools/gen_golden.py` | golden 数据生成与 `--check` 校验 |
| `docs/` | 需求、架构与计划文档 |

## 兼容性与限制

1. 强环禁止构造：对象不得被 `set`/`add` 进自身或祖先。这是调用方使用禁令；若已构造，`ace_serialize` 返回 `14`（CycleUnsupported），且 `ace_value_free` 对强环按引用计数语义不保证释放。
2. C ABI 不表达 `Weak`：弱引用在该层无对应物，请求返回 `11`（Unsupported）。
3. 值自身无 `type_key`，且其位置 schema 与白名单同 `type_key` 注册项布局不同：编码返回 `13`（TypeMismatch）。
4. `max_build_nesting` 只对 Rust 图解码生效；C ABI 不消费该值（`ace_options_set_build_nesting` 接受并保存，但不影响 C 侧解码结果与错误码），C 侧结构深度仍由 `max_depth` 约束，超限返回 `5`（DepthExceeded）。
5. C# 客户端需在 C ABI 之上自行封装，本仓库不提供托管绑定。
6. wire 与 C# 旧格式**有意不逐字节兼容**：同一数据两端字节不同，不能直接互换。
7. Miri 通过只代表已覆盖的路径；跨平台与其它目标未做过验证。

## 构建与测试

```sh
cargo test -p ace-serialize                      # 核心 crate
cargo check -p ace-serialize --no-default-features # no_std 构建
cargo test -p ace-serialize-ffi                  # C ABI 单元测试
cargo build -p ace-serialize-ffi --release       # 产出 cdylib 供 C 测试链接

cc -std=c17 -Wall -Wextra -Iinclude tests/c/serialize_model.c \
   -Ltarget/release -lace_serialize_ffi -Wl,-rpath,"$PWD/target/release" \
   -o target/release/serialize_model_c && ./target/release/serialize_model_c

python3 tools/gen_golden.py --check              # golden 数据校验
```

## 性能

在 8 次交错的基准中，20 行对比项全部满足 Rust ≤ C#（取中位数）：最差一项为 `g.shared` 的 raw 解码，耗时是 C# 的 `0.881×`；最好一项为 `a.int` 的解码，`0.087×`。上述结果**仅限已测样本，不外推**到其它数据形状、环境或平台。

## 文档

- `docs/requirements.md`——需求，v0.34，权威口径
- `docs/architecture.md`——架构、数据流与 wire/身份设计
- `docs/plan.md`——当前实施计划
- `include/ace_serialize.h`——C ABI 头，v5 冻结

## 隐私

本库离线运行、不联网、不采集任何使用数据，详见 [`PRIVACY.md`](PRIVACY.md)。

## 许可

双许可，任选其一：`MIT OR Apache-2.0`。条款见 [`LICENSE-MIT`](LICENSE-MIT) 与 [`LICENSE-APACHE`](LICENSE-APACHE)。
