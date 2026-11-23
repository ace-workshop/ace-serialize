# ace-serialize 开发约束

先读 [README](README.md) 选择 Rust 或 C 入口，再读[架构与格式](docs/architecture.md)确认不变量、[使用指南](docs/guide.md)确认调用方义务。C 声明以 [include/ace_serialize.h](include/ace_serialize.h) 为准。

## 写域和兼容性

- 本仓是 edition 2021 的独立 Cargo workspace。core 默认 `no_std + alloc`；derive 是编译期 proc-macro；仅 FFI 是面向 C 的 `cdylib`。不要让 Rust 调用方绕经 FFI，不引入服务端调度、网络协议或 Bevy。
- 字段号、保留号、判别值、raw/packed 规则及 C ABI 是对外契约。wire 不自描述；两端必须显式匹配 schema 与编码选项。保留错误码 `0..16` 的含义，不重排、不复用。契约改动须先获授权，核对头文件、实现、golden 和文档。
- `encode_into` 的输出和 scratch 由调用方提供；失败时不发送部分结果。不要把容量复用的 `decode_from` 描述为无条件零分配；失败后的目标不是有效解码结果。
- 遵守[架构 §9.1](docs/architecture.md#91-允许的-unsafe-面) 的 unsafe 边界：类型化 scratch、已有小端定宽拷贝、FFI 原始指针各有独立前提，不把现有例外扩大为任意模块都能新增 unsafe。FFI 保持借用、收编、释放和 panic 边界。
- 保留已跟踪的 `Cargo.lock`，不以此宣称锁定下游的传递依赖。manifest 未声明 MSRV，不凭单台机器的结果补写最低版本。本仓改动不自动授权更新任何消费仓或组合仓的 gitlink。

## 在仓根验证

```sh
cargo check -p ace-serialize --no-default-features --locked
cargo test --workspace --all-features --locked
python3 -B tools/gen_golden.py --check
cargo build -p ace-serialize-ffi --release --locked
```

修改 C 接口或句柄所有权时，按 README 的 C17 命令**实际链接并运行**相应的 `tests/c/` 程序；仅构建动态库不证明互调。新测试优先扩展已有 wire、derive UI 或 FFI 用例，不用重录 golden 掩盖不兼容。

按当次授权与适用审核规则工作。仅提交相关文件，使用英文 Conventional Commits；不要提交凭据、个人路径、构建产物或跨项目资料，不代其它项目发布。
