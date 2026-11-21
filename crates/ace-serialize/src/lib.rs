#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod decode;
pub mod encode;
pub mod error;
pub mod hooks;
pub mod limits;
pub mod schema;
pub mod wire;

use decode::Decoder;
use encode::Encoder;
use error::Result;
use limits::Options;
use schema::{AceDecode, AceEncode};

/// 将值编码为完整消息，使用 `encode(&value, &options)` 获取拥有所有权的字节缓冲。
/// `options` 使用 `limits::Options`；默认输出 packed，设 `packed: false` 则输出 raw。
///
/// # Errors
/// 值编码或消息组装失败时返回原有错误，包括资源限制与未关闭的编码作用域。
pub fn encode<T: AceEncode>(value: &T, options: &Options) -> Result<alloc::vec::Vec<u8>> {
    let mut encoder = Encoder::new(options);
    value.ace_encode(&mut encoder)?;
    encoder.finish()
}

/// 将值编码到调用方提供的 `dst`，工作区由调用方 `scratch` 提供；返回写入的字节数。
///
/// `options.packed` 选择 raw/packed；成功时返回值等于最终字节长度，`dst` 超出部分不被写入。
///
/// # Errors
/// `dst` 容量不足、`scratch` 不足与身份表物理相撞（`C(B) < N ≤ C(0)`）都返回既有
/// `1 InvalidArgument`（用户 2026-11-18 裁决，暂定，后续可细分）；值编码或消息组装失败时
/// 返回原有错误。失败后 `dst` 内容不作承诺（可含部分写入）。
pub fn encode_into<T: AceEncode>(
    value: &T,
    dst: &mut [u8],
    scratch: &mut [u8],
    options: &Options,
) -> Result<usize> {
    encode::encode_into_impl(value, dst, scratch, options)
}

/// 容量探测形态（需求 R29）：与 [`encode_into`] **同一条真实编码路径**，成功时返回写入的字节数；
/// **仅当因 `scratch` 不足而失败**时，除返回既有 `1 InvalidArgument` 外，还把 `need` 写成本次
/// 失败点的容量下界。
///
/// `need` 的单位是**在相同起址下容纳本次调用所需的总字节数**（即 `scratch.len()` 应达到的目标值），
/// **不是「还差多少」**：写 `need` 的那次调用已因装不下而失败，故所写值必大于本次传入的 `scratch.len()`。
/// 它只是**本次失败点**的 checked 下界（按该次分配自身的字节数与其在本切片向下对齐下的落位算，
/// 零身份槽时不加也不扣对齐前导），**不是一次即得的精确最小值**，也不是闭式充分上界。
///
/// 仅当手写编码回调丢弃本次调用中已锁存且有可报告下界的真实 `scratch` 不足、未发生其他可在
/// 产生点识别的失败，并自行返回一条未经产生点标注的 `Err(InvalidArgument)` 时，本函数才可能
/// 在原样返回该错误的同时把已锁存的下界写入 `need`；其余非 `scratch` 不足的错误不写入 `need`。
///
/// # 重试
/// 用**同一起址**、长度 ≥ `need` 的**新**缓冲区重新调用（例如把原缓冲区的前 `need` 字节作为切片），
/// 可能需要多次；**换起址必须重算**（对齐前导随起址变，同长度不同起址可用容量可能不同）。
/// 同起址、稳定输入下若再次因 `scratch` 不足失败，**下一次所报 `need` 必大于上次**（建议按翻倍扩大）。
/// 探测不重跑回调：每次调用都是一次完整的编码调用，调用方的多次重试就是多次调用。
///
/// # 零分配
/// 库自身实现与 derive 生成代码在该入口不分配（含身份/引用追踪状态，由 `scratch` 承载，R14②③）；
/// 调用链含手写 `impl AceEncode` 时以该实现自身不分配为前提。不接收也不要求调用方额外工作区。
///
/// # Errors
/// `scratch` 不足（含身份条目不足、双端相撞、arena 不足）返回既有 `1 InvalidArgument` 并写 `need`；
/// `dst` 容量不足、非法输入、资源限制、图/锁错误等照常按既有码返回且**不写 `need`**（哨兵值保持
/// 调用方传入值）——`1` 本身不区分这些来源，归因只来自库内部保留到返回点的私有诊断。
/// `encode_into` 的签名与行为不变，R15「`dst` 恰好容量必须成功」不变。
/// 失败后 `dst` 内容不作承诺（可含部分写入）。
pub fn encode_into_probe<T: AceEncode>(
    value: &T,
    dst: &mut [u8],
    scratch: &mut [u8],
    options: &Options,
    need: &mut usize,
) -> Result<usize> {
    encode::encode_into_probe_impl(value, dst, scratch, options, Some(need))
}

/// 发布每个身份条目所需的最小字节数 `S = size_of::<Entry>()`（需求 R27）。
///
/// 与 [`identity_entry_align`] 的 `A` 一起，`S`/`A` 是 `scratch` 布局对外承诺的两项数值：调用方
/// 据此折算 [`encode_into`] 的 `scratch` 最小容量——确实需要 `N ≥ 1` 个身份槽时，按「切片起址
/// 向上对齐到 `A` 得到的对齐前导 `P` + `N × S` 条目区 + 字节区」计算；**零身份槽时既不扣也
/// 不加对齐前导 `P`**（对齐前导只在确实要放身份槽时发生）。因为 `P` 取决于切片起址，长度相同、
/// 起址不同的两个合法 `scratch` 可用容量可能不同；因容量不足失败时返回既有 `1 InvalidArgument`。
/// 任何对齐余量都不得转嫁 `dst`。
///
/// 取值按当前目标与启用的 feature 从 `Entry` 布局求值（`Entry` 内的 `Guard` 有受 `std` 控制的
/// 分支），因此不是可写死的单一数值；不承诺换目标或换 feature 时 `S`/`A` 两个数值各自都会变。
/// 取函数形态（`pub const fn`）而非公开 `pub const`：既能在常量上下文求值（如
/// `const N: usize = identity_entry_size();`），也不新增一个看起来像固定数值的公开常量名。
///
/// `Entry` 的字段布局（`family`/`address`/`target`/`id`/`state`/`guard`）已由需求 R27 冻结：
/// 布局改变即属**破坏性变更**，须走需求变更流程。C 头的对应数值随门 A 发布，本入口不改变 C 头。
pub const fn identity_entry_size() -> usize {
    encode::identity_entry_size()
}

/// 发布每个身份条目的对齐 `A = align_of::<Entry>()`（需求 R27）。
///
/// `A` 恒为非零 2 的幂、且 `S = size_of::<Entry>()` 是它的整数倍；`scratch` 的条目区起点由
/// 切片起址向上对齐到 `A` 得到：对齐前导 `P = (A - 起址 % A) % A`（`0 <= P < A`），第 `i` 槽
/// 落在 `起址 + P + i × S`（`A` 为 2 的幂时该地址对 `A` 对齐）。最小容量的算法与「零身份槽
/// 时不加对齐前导 `P`」的例外见 [`identity_entry_size`]。
///
/// 取值按目标平台与 `std` feature 求值（理由与形态选择同 [`identity_entry_size`]）。改变条目
/// 对齐属**破坏性变更**，须走需求变更流程；C 头的对应数值随门 A 发布。
pub const fn identity_entry_align() -> usize {
    encode::identity_entry_align()
}

/// 从完整消息解码拥有所有权的值，使用 `decode::<T>(&bytes, &options)` 指定目标类型。
/// `options` 使用 `limits::Options`，其中 `packed` 必须与输入格式一致；返回前校验消息完成状态。
///
/// # Errors
/// 输入格式、值解码或消息完成校验失败时返回原有错误。
pub fn decode<T: AceDecode>(src: &[u8], options: &Options) -> Result<T> {
    let mut decoder = Decoder::new(src, options)?;
    let value = T::ace_decode(&mut decoder)?;
    decoder.finish()?;
    Ok(value)
}

/// 解码进调用方提供的已有实例：与 [`decode()`] 走同一套校验流程（`Decoder::new` 的 envelope／
/// packed／预算校验、`T::ace_decode_into`、`finish` 的收尾校验），成功后只经
/// [`AceDecode::ace_decode_into`] 写入 `out`。复用承诺按责任边界（R21）：只覆盖**实际到达复用
/// 覆写、且未被整体替换**的缓冲——这类缓冲在 `out` 已有足够容量时保留原有分配。手写
/// `impl AceDecode` 未覆写 `ace_decode_into` 的类型按默认实现整体替换，不承诺复用。
/// 已覆写的是库内建 `String`/`Vec`/`Option`/`RefCell`/`RwLock`、`BTreeMap`（节点与键重建：新 Map
/// 插入本次解码出的新键，同键值经 [`schema::AceElement::decode_element_into`] 在原位置复用，无
/// 同键旧值时照旧解码新值）与非 `shared`（不含 `Rc`/`Arc`/`Weak`）的 `#[derive]` 结构体、多态
/// enum：derive 逐字段写入既有结构体字段（多态 enum 在同一判别变体时复用该变体载荷，跨变体整体
/// 替换）。`Vec<T>` 的元素按 [`schema::AceElement::decode_element_into`] 写入既有元素，因此
/// `Vec<Vec<T>>` 的内层集合也保留原有分配。其余路径仍整体替换：`Box<T>` 等未覆写的类型、
/// `shared`（含 `Rc`/`Arc`/`Weak` 与环）的 `#[derive]` 类型（该类型由 derive 生成 [`AceDecode`]，
/// 走既有图路径并在消息根整体替换；图内已构造缓冲不承诺复用），以及 `Some → None`、跨变体切换与
/// 缺席字段恢复 `Default`。
///
/// `options.packed` 必须与输入格式一致；成功返回即表示 `out` 已按本次输入更新。
///
/// # 失败态
/// 返回 `Err` 时 `out` 的内容不作任何承诺（可能已被部分修改），调用方不得把它当作有效值；
/// 错误码按既有条款返回，不新增错误码，不 panic、不静默截断。
///
/// # 目标单元格的借用/锁状态
/// 内建 `RefCell`／`RwLock` 解码实现在**向该单元格写入之前**检查该单元格是否可用：存在未释放
/// 的借用（含经 `mem::forget` 泄漏的借用或 guard）、锁中毒、被弃 guard 时返回既有
/// `1 InvalidArgument`，不修改该单元格，也不清除、规整或改写其状态。该粒度是单元格而非整个
/// `out`：失败时前序字段可能已被更新，不承诺整个 `out` 不变。整体替换不视为向该单元格写入，
/// 因此 `Option` 由 `Some(旧值)` 变 `None`、多态 enum 换到不含该包装的变体都不触发该检查。
///
/// # Errors
/// 输入格式、值解码、目标单元格不可用或消息完成校验失败时返回原有错误。
pub fn decode_from<T: AceDecode>(src: &[u8], out: &mut T, options: &Options) -> Result<()> {
    let mut decoder = Decoder::new(src, options)?;
    T::ace_decode_into(&mut decoder, out)?;
    decoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_encode_decode_roundtrip() {
        let value = 42i32;
        for options in [
            Options {
                packed: false,
                ..Options::default()
            },
            Options::default(),
        ] {
            let encoded = encode(&value, &options);
            assert!(encoded.is_ok(), "packed = {}: {encoded:?}", options.packed);
            assert_eq!(decode::<i32>(&encoded.unwrap(), &options), Ok(value));
        }
    }

    #[test]
    fn top_level_decode_rejects_invalid_message() {
        #[derive(Debug, PartialEq)]
        struct UnreadRoot;
        impl AceDecode for UnreadRoot {
            fn ace_decode(_: &mut Decoder<'_>) -> Result<Self> {
                Ok(Self)
            }
        }

        let options = Options {
            packed: false,
            ..Options::default()
        };
        for (depth, tag) in [(1i32, 0i32), (0, 1)] {
            // 此类型不读根槽，使非法 depth/tag 只能由 finish 拒绝。
            let mut bytes = [0u8; 24];
            bytes[4..8].copy_from_slice(&16i32.to_le_bytes());
            bytes[8..12].copy_from_slice(&depth.to_le_bytes());
            bytes[12..16].copy_from_slice(&tag.to_le_bytes());
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            assert_eq!(UnreadRoot::ace_decode(&mut decoder), Ok(UnreadRoot));
            let expected = decoder.finish();
            assert_eq!(expected, Err(error::Error::InvalidTag));
            let actual = decode::<UnreadRoot>(&bytes, &options);
            assert_eq!(actual, Err(error::Error::InvalidTag));
            assert_eq!(actual, expected.map(|()| UnreadRoot));
        }

        let mut bytes = [0u8; 8].to_vec();
        bytes.push(0);
        let expected = Decoder::new(&bytes, &options).err();
        assert!(expected.is_some());
        assert_eq!(decode::<UnreadRoot>(&bytes, &options).err(), expected);
    }
}
