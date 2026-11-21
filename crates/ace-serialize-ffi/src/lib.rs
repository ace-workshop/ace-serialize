//! C ABI 动态值模型与 schema 模型：`AceValue` 的构造、读取、集合操作、共享与释放，以及
//! `AceTypeRef` 类型引用节点、`AceSchema`、`AceEnumSchema` 的构造与释放。
//!
//! 对外契约以冻结的 `include/ace_serialize.h`（v4）为准；本文件只实现该头声明的 `AceValue`
//! 与 `AceTypeRef`/`AceSchema`/`AceEnumSchema` 函数族，不改动也不补充声明。所有入口都由
//! [`guard`] 包住：跨 FFI 边界不得 panic，panic 一律映射为 `10 ACE_PANIC_CAUGHT`。
//!
//! `AceValue` 是引用计数节点（R32）：`AceValue*` 是 `Arc` 的原始指针化句柄，`ace_value_free`
//! 每次都只减一个引用、归零才释放整棵子树；`ace_object_set`/`ace_list_add` 成功即父容器持有
//! 一份引用而调用方句柄仍然有效。借用视图（`ace_object_get`/`ace_list_at`/`ace_value_get_str`
//! 等）不持引用：寿命 ≤ 父值、不得 free、不得被 set/add。

// FFI 入口按 C 头契约接收原始指针（空指针即非法参数、悬垂由调用方寿命契约约束），
// 调用方的 `unsafe` 责任不因 Rust 侧签名而转移，故不把入口标成 `unsafe fn`。
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::{Cell, RefCell, UnsafeCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_char, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::Arc;

use ace_serialize::decode::Decoder;
use ace_serialize::encode::Encoder;
use ace_serialize::error::{Error, Result as AceResult};
use ace_serialize::limits::Options;
use ace_serialize::schema::{value, AceElement, AceEncode, ElementKind, NonOptional};

/// 成功码（头 `ACE_OK`）。
const ACE_OK: i32 = 0;

/// 普通对象体的固定长度（FR-W06）：`u32 slot_offset + u32 slot_count`。
const OBJECT_BODY_BYTES: usize = 8;

/// FR-W07 的 type_name 逃生口判别值。C ABI 没有注册数值变体的接口，故对象的多态表达一律走
/// 逃生口：`type_name` 记录里放的正是该节点自身的 `type_key`（R33②③）。
const POLY_ESCAPE_TAG: u16 = 65535;

/// 动态值种类；判别值与头 `AceKind` 的枚举次序一致（0..8）。
enum ValueKind {
    Null = 0,
    Bool = 1,
    I64 = 2,
    U64 = 3,
    F64 = 4,
    Str = 5,
    Bytes = 6,
    List = 7,
    Object = 8,
}

enum Body {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    /// f64 位模式原样保存：不做宿主浮点运算、不规范化 NaN、保留 `-0`（NFR-D02）。
    /// F64→f32 的按位映射规则属编码阶段（FR-A03），不在构造期判定。
    F64(u64),
    Str(Box<[u8]>),
    Bytes(Box<[u8]>),
    /// 元素以 `Arc` 持有：`get`/`at` 借出的句柄就是节点地址，容器扩容不使它失效；
    /// 同一节点可同时被多个父容器与调用方句柄引用（R32 共享身份）。
    List(Vec<Arc<AceValue>>),
    Object(Object),
}

/// 动态对象：`field_id` 即 wire order，`BTreeMap` 的升序与 wire 的 order 递增一致。
struct Object {
    /// `ace_value_object` 的 `type_key` 副本（可缺席）；`ace_serialize` 的 key 冲突判定读它。
    type_key: Option<Box<str>>,
    /// 字段值按引用计数共享：父容器持有一份引用，调用方句柄各自另持一份。
    fields: BTreeMap<i32, Arc<AceValue>>,
}

/// opaque 值节点：`Arc` 分配、地址稳定，故节点地址既是 `get`/`at` 的借用句柄，也是
/// `ace_serialize` 的身份键（`AceValue*` 即 `Arc` 的原始指针化句柄，同一节点的所有句柄地址相同）。
pub struct AceValue {
    /// `UnsafeCell` 提供 body 的内部可变性：`set`/`add` 允许构造强环（`child` 就是接收者
    /// 自己或祖先），此时「接收者的可变借用」与「引用计数操作建立的 `&ArcInner<AceValue>`
    /// 共享借用」覆盖同一节点。变更只经体内指针发生，且总在引用计数操作之后，故两者不重叠
    /// （`RefCell` 的借用守卫借不出编码适配器需要的 `&'a` 字段视图，故选裸指针形态）。
    body: UnsafeCell<Body>,
}

impl AceValue {
    /// body 的只读视图：寿命跟随 `&self`。
    ///
    /// 契约（头 v4 借用视图条款）：借用视图寿命 ≤ 父值、不得被 `set`/`add`；因此读视图存续
    /// 期间不会有写路径进入同一个 body。
    fn body(&self) -> &Body {
        // SAFETY: 只读访问；写路径（`ace_object_set`/`ace_list_add`）只在体内指针上短暂建立
        // `&mut Body`，且不跨引用计数操作、不跨调用持有。
        unsafe { &*self.body.get() }
    }

    /// body 的可变视图，只供 `set`/`add` 使用。
    ///
    /// SAFETY: 调用时不得有其它 `&Body` 借用在使用（写路径在函数内临时建立、随函数结束失效），
    /// 且必须在任何引用计数操作完成之后才调用——否则共享借用会覆盖刚建立的 `&mut`。
    unsafe fn body_mut(&self) -> &mut Body {
        &mut *self.body.get()
    }

    /// 新建一枚引用计数为 1 的节点（解码路径用）；`ace_value_*` 构造入口另走 [`construct`]。
    fn node(body: Body) -> Arc<Self> {
        Arc::new(Self {
            body: UnsafeCell::new(body),
        })
    }

    fn kind(&self) -> ValueKind {
        match self.body() {
            Body::Null => ValueKind::Null,
            Body::Bool(_) => ValueKind::Bool,
            Body::I64(_) => ValueKind::I64,
            Body::U64(_) => ValueKind::U64,
            Body::F64(_) => ValueKind::F64,
            Body::Str(_) => ValueKind::Str,
            Body::Bytes(_) => ValueKind::Bytes,
            Body::List(_) => ValueKind::List,
            Body::Object(_) => ValueKind::Object,
        }
    }
}

// 测试钩子：节点析构计数（引用计数归零才触发），用于证明替换/减引用确实各归还一次。
// 用 thread-local 而非全局计数：`cargo test` 并行跑各用例，全局计数会把别的线程的析构算进来。
#[cfg(test)]
thread_local! {
    static DROPPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
impl Drop for AceValue {
    fn drop(&mut self) {
        DROPPED.with(|count| count.set(count.get() + 1));
    }
}

/// 统一入口骨架：把函数体收进 `catch_unwind`，panic 映射为 `10`。
fn guard<F>(body: F) -> i32
where
    F: FnOnce() -> Result<(), Error>,
{
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => ACE_OK,
        Ok(Err(error)) => error.code(),
        Err(_) => Error::PanicCaught.code(),
    }
}

/// 句柄类构造骨架：`out` 为 NULL → `1`；失败时 `*out` 保持为空，成功即移交所有权。
fn construct_handle<T, F>(out: *mut *mut T, build: F) -> i32
where
    F: FnOnce() -> Result<T, Error>,
{
    if out.is_null() {
        return Error::InvalidArgument.code();
    }
    guard(|| {
        // SAFETY: 头约定 `out` 是可写的句柄槽；先置空以满足「失败时 *out 为空」。
        unsafe { *out = ptr::null_mut() };
        let handle = Box::new(build()?);
        // SAFETY: 同一个非空 `out` 槽，所有权随指针交给调用方。
        unsafe { *out = Box::into_raw(handle) };
        Ok(())
    })
}

/// `AceValue` 构造骶架（`out` 约定同 [`construct_handle`]）：句柄是 `Arc` 的原始指针化，
/// 新节点的引用计数为 1，`ace_value_free` 每调用一次减一、归零才释放。
fn construct<F>(out: *mut *mut AceValue, build: F) -> i32
where
    F: FnOnce() -> Result<Body, Error>,
{
    if out.is_null() {
        return Error::InvalidArgument.code();
    }
    guard(|| {
        // SAFETY: 头约定 `out` 是可写的句柄槽；先置空以满足「失败时 *out 为空」。
        unsafe { *out = ptr::null_mut() };
        let node = Arc::new(AceValue {
            body: UnsafeCell::new(build()?),
        });
        // SAFETY: 同一个非空 `out` 槽，所有权随指针交给调用方。
        unsafe { *out = Arc::into_raw(node) as *mut AceValue };
        Ok(())
    })
}

/// 共享枚举（`ace_value_share` 的内核）：引用计数 +1，返回同一节点的另一枚句柄。
///
/// SAFETY: `value` 必须是由本库 `Arc::into_raw` 产出且仍存活句柄的节点地址。
unsafe fn share_handle(value: *const AceValue) -> *mut AceValue {
    Arc::increment_strong_count(value);
    value as *mut AceValue
}

/// 减一个引用（`ace_value_free` 的内核）：归零才释放该节点及其子树。
///
/// SAFETY: `value` 必须是由本库 `Arc::into_raw` 产出且仍存活句柄的节点地址，且每枚句柄只减一次。
unsafe fn release_handle(value: *mut AceValue) {
    drop(Arc::from_raw(value as *const AceValue));
}

/// 复制调用方输入字节；`ptr` 可 NULL 仅当 `len == 0`（同 codec 条款）。
fn copy_input(ptr: *const u8, len: usize) -> Result<Box<[u8]>, Error> {
    if len == 0 {
        return Ok(Box::default());
    }
    if ptr.is_null() {
        return Err(Error::InvalidArgument);
    }
    // SAFETY: 头约定 `len` 是 `ptr` 指向的有效字节数。
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) }
        .to_vec()
        .into_boxed_slice())
}

/// 借用调用方输入字节（零拷贝，`ace_deserialize` 用）：`ptr` 可 NULL 仅当 `len == 0`，且调用方
/// 在**本次调用期间**保活、不改写这段内存（头 codec 契约：`buf` 是本次调用的输入）。
///
/// 不复制是限额要求：超大 `len` 的整包拷贝会在 `max_input_bytes` 判定之前就把内存拿走，超预算时
/// 可能 OOM abort 而不是受控返回 `6`。需要拥有字节的解码路径（packed 解包）自己复制。
fn input_slice<'a>(ptr: *const u8, len: usize) -> Result<&'a [u8], Error> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(Error::InvalidArgument);
    }
    // SAFETY: 调用期间 `ptr..ptr+len` 是有效且未改写的字节（头 codec 契约）；返回的切片不跨本次
    // 调用（`ace_deserialize` 只在函数体内用它，不存回 `*out`）。
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// schema/enum 的 `type_key` 必填：NULL → `1`，非 UTF-8 → `8`。
fn required_type_key(ptr: *const c_char) -> Result<Box<str>, Error> {
    read_type_key(ptr)?.ok_or(Error::InvalidArgument)
}

/// 读取 NUL 结尾的 `type_key`（可 NULL）；复制进库，调用方之后可释放原串。
fn read_type_key(ptr: *const c_char) -> Result<Option<Box<str>>, Error> {
    if ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: 头约定 `type_key` 是调用方维护的、NUL 结尾的 UTF-8 串。
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| Error::InvalidUtf8)?;
    Ok(Some(Box::from(text)))
}

// ——— 版本与布局发布（头 v4：`ace_abi_version` / `ace_wire_version` / `S`·`A`） ———

/// `ACE_ABI_VERSION`（头 `include/ace_serialize.h`）：1.0.0。
const ABI_VERSION: u32 = 0x0001_0000;
/// `ACE_WIRE_VERSION`（头）：当前 wire 版本 1。
const WIRE_VERSION: u32 = 1;

/// 查询 ABI 版本（`ace_abi_version`，AC-A01 要求 C 进程可查询）。
#[no_mangle]
pub extern "C" fn ace_abi_version() -> u32 {
    ABI_VERSION
}

/// 查询 wire 版本（`ace_wire_version`）。
#[no_mangle]
pub extern "C" fn ace_wire_version() -> u32 {
    WIRE_VERSION
}

/// 发布身份条目字节数 `S`（`ace_identity_entry_size`，R27）。
///
/// 转发主 crate 的 `identity_entry_size()`；值随目标/feature 变化（当前远小于 `u32::MAX`），
/// 极端情况下饱和到 `u32::MAX` 而不是 panic——函数不得跨 FFI 边界 panic。
#[no_mangle]
pub extern "C" fn ace_identity_entry_size() -> u32 {
    u32::try_from(ace_serialize::identity_entry_size()).unwrap_or(u32::MAX)
}

/// 发布身份条目对齐 `A`（`ace_identity_entry_align`，R27）；语义同上。
#[no_mangle]
pub extern "C" fn ace_identity_entry_align() -> u32 {
    u32::try_from(ace_serialize::identity_entry_align()).unwrap_or(u32::MAX)
}

// ——— 构造 ———

/// 构造 Null 值（`ace_value_null`）。
#[no_mangle]
pub extern "C" fn ace_value_null(out: *mut *mut AceValue) -> i32 {
    construct(out, || Ok(Body::Null))
}

/// 构造 Bool 值（`ace_value_bool`）：仅接受 0/1，其余 `1 InvalidArgument`。
#[no_mangle]
pub extern "C" fn ace_value_bool(value: u8, out: *mut *mut AceValue) -> i32 {
    construct(out, || match value {
        0 => Ok(Body::Bool(false)),
        1 => Ok(Body::Bool(true)),
        _ => Err(Error::InvalidArgument),
    })
}

/// 构造 I64 值（`ace_value_i64`）。
#[no_mangle]
pub extern "C" fn ace_value_i64(value: i64, out: *mut *mut AceValue) -> i32 {
    construct(out, || Ok(Body::I64(value)))
}

/// 构造 U64 值（`ace_value_u64`）。
#[no_mangle]
pub extern "C" fn ace_value_u64(value: u64, out: *mut *mut AceValue) -> i32 {
    construct(out, || Ok(Body::U64(value)))
}

/// 构造 F64 值（`ace_value_f64_bits`）：入参即位模式，原样保存。
#[no_mangle]
pub extern "C" fn ace_value_f64_bits(bits: u64, out: *mut *mut AceValue) -> i32 {
    construct(out, || Ok(Body::F64(bits)))
}

/// 构造 Str 值（`ace_value_str`）：按长度复制，可含内嵌 NUL；须为 UTF-8，否则 `8`。
#[no_mangle]
pub extern "C" fn ace_value_str(utf8: *const u8, len: usize, out: *mut *mut AceValue) -> i32 {
    construct(out, || {
        let bytes = copy_input(utf8, len)?;
        std::str::from_utf8(&bytes).map_err(|_| Error::InvalidUtf8)?;
        Ok(Body::Str(bytes))
    })
}

/// 构造 Bytes 值（`ace_value_bytes`）：按长度复制原始字节，不做编码校验。
#[no_mangle]
pub extern "C" fn ace_value_bytes(bytes: *const u8, len: usize, out: *mut *mut AceValue) -> i32 {
    construct(out, || Ok(Body::Bytes(copy_input(bytes, len)?)))
}

/// 构造空 List 值（`ace_value_list`）。
#[no_mangle]
pub extern "C" fn ace_value_list(out: *mut *mut AceValue) -> i32 {
    construct(out, || Ok(Body::List(Vec::new())))
}

/// 构造空 Object 值（`ace_value_object`）：`type_key` 可 NULL，非 NULL 时复制进库。
#[no_mangle]
pub extern "C" fn ace_value_object(type_key: *const c_char, out: *mut *mut AceValue) -> i32 {
    construct(out, || {
        Ok(Body::Object(Object {
            type_key: read_type_key(type_key)?,
            fields: BTreeMap::new(),
        }))
    })
}

// ——— 读取（借用视图，寿命 ≤ 父值） ———

/// 读取种类（`ace_value_kind`）。
#[no_mangle]
pub extern "C" fn ace_value_kind(value: *const AceValue, out: *mut i32) -> i32 {
    guard(|| {
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        *out = value.kind() as i32;
        Ok(())
    })
}

/// 读取 Bool（`ace_value_get_bool`）。
#[no_mangle]
pub extern "C" fn ace_value_get_bool(value: *const AceValue, out: *mut u8) -> i32 {
    guard(|| {
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        match value.body() {
            Body::Bool(flag) => {
                *out = u8::from(*flag);
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    })
}

/// 读取 I64（`ace_value_get_i64`）。
#[no_mangle]
pub extern "C" fn ace_value_get_i64(value: *const AceValue, out: *mut i64) -> i32 {
    guard(|| {
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        match value.body() {
            Body::I64(number) => {
                *out = *number;
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    })
}

/// 读取 U64（`ace_value_get_u64`）。
#[no_mangle]
pub extern "C" fn ace_value_get_u64(value: *const AceValue, out: *mut u64) -> i32 {
    guard(|| {
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        match value.body() {
            Body::U64(number) => {
                *out = *number;
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    })
}

/// 读取 F64 位模式（`ace_value_get_f64_bits`）：按位返回，不做浮点运算。
#[no_mangle]
pub extern "C" fn ace_value_get_f64_bits(value: *const AceValue, out: *mut u64) -> i32 {
    guard(|| {
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        match value.body() {
            Body::F64(bits) => {
                *out = *bits;
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    })
}

/// 读取 Str 的借用视图（`ace_value_get_str`）：`*ptr`/`*len` 指向库内缓冲，不得 free。
#[no_mangle]
pub extern "C" fn ace_value_get_str(
    value: *const AceValue,
    ptr_out: *mut *const u8,
    len_out: *mut usize,
) -> i32 {
    guard(|| {
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let ptr_out = unsafe { ptr_out.as_mut() }.ok_or(Error::InvalidArgument)?;
        let len_out = unsafe { len_out.as_mut() }.ok_or(Error::InvalidArgument)?;
        match value.body() {
            Body::Str(bytes) => {
                *ptr_out = bytes.as_ptr();
                *len_out = bytes.len();
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    })
}

/// 读取 Bytes 的借用视图（`ace_value_get_bytes`）：不得 free。
#[no_mangle]
pub extern "C" fn ace_value_get_bytes(
    value: *const AceValue,
    ptr_out: *mut *const u8,
    len_out: *mut usize,
) -> i32 {
    guard(|| {
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let ptr_out = unsafe { ptr_out.as_mut() }.ok_or(Error::InvalidArgument)?;
        let len_out = unsafe { len_out.as_mut() }.ok_or(Error::InvalidArgument)?;
        match value.body() {
            Body::Bytes(bytes) => {
                *ptr_out = bytes.as_ptr();
                *len_out = bytes.len();
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    })
}

// ——— 集合操作 ———

/// 设置对象字段（`ace_object_set`）：成功即父对象持有 `child` 的一份引用（引用计数 +1），
/// **调用方句柄仍有效**、须自行 `ace_value_free` 减引用；替换同 `field_id` 时旧值的父引用
/// 减一次，归零才释放（失效只及该父返回的借用视图）。
///
/// 失败顺序：NULL 参数 → `1`；接收者不是 Object → `13`；`field_id < 0`（保留号，order 非负）
/// → `12`；失败不增加引用，`child` 仍归调用方。把对象自身或其祖先 set/add 进对象（强环）
/// 是调用方使用禁令而不是未定义行为：这里允许构造，`ace_serialize` 以 `14` 担下来。
#[no_mangle]
pub extern "C" fn ace_object_set(object: *mut AceValue, field_id: i32, child: *mut AceValue) -> i32 {
    guard(|| {
        if object.is_null() || child.is_null() {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: 两个非空句柄按头契约由本库分配；接收者只经 `&` 观察，变更走体内指针。
        let receiver = unsafe { &*object };
        let Body::Object(_) = receiver.body() else {
            return Err(Error::TypeMismatch);
        };
        if field_id < 0 {
            return Err(Error::NotFound);
        }
        // SAFETY: 先为父对象加一份引用（R32⑤）：调用方句柄仍持自己那份、须自行 free；
        // 同 `field_id` 的旧值随 `Arc` 析构减一次父引用，归零才释放。
        //
        // 顺序要紧（P1a）：`child` 可能就是接收者自己（强环），`increment_strong_count`
        // 会建立覆盖整节点的共享借用；必须先做完计数、再进入 body 变更，两者才不重叠。
        unsafe { Arc::increment_strong_count(child as *const AceValue) };
        // SAFETY: 计数操作已结束，此刻才建立 body 的可变借用，且不建立整节点的 `&mut AceValue`。
        let Body::Object(body) = (unsafe { receiver.body_mut() }) else {
            unreachable!("上面已确认接收者是 Object");
        };
        body.fields
            .insert(field_id, unsafe { Arc::from_raw(child as *const AceValue) });
        Ok(())
    })
}

/// 读取对象字段的借用视图（`ace_object_get`）：寿命 ≤ 父值、父修改/释放后失效，
/// 不得 free、不得被 set/add。
#[no_mangle]
pub extern "C" fn ace_object_get(
    object: *const AceValue,
    field_id: i32,
    out: *mut *const AceValue,
) -> i32 {
    guard(|| {
        let object = unsafe { object.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        let Body::Object(body) = object.body() else {
            return Err(Error::TypeMismatch);
        };
        if field_id < 0 {
            return Err(Error::NotFound);
        }
        let found = body.fields.get(&field_id).ok_or(Error::NotFound)?;
        // 借用视图就是节点地址：只要该节点还有任何一份引用，地址就稳定。
        *out = Arc::as_ptr(found);
        Ok(())
    })
}

/// 追加列表元素（`ace_list_add`）：成功即列表持有 `child` 的一份引用（引用计数 +1），
/// 调用方句柄仍有效、须自行 `ace_value_free` 减引用；失败不增加引用。
#[no_mangle]
pub extern "C" fn ace_list_add(list: *mut AceValue, child: *mut AceValue) -> i32 {
    guard(|| {
        if list.is_null() || child.is_null() {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: 两个非空句柄按头契约由本库分配；接收者只经 `&` 观察，变更走体内指针。
        let receiver = unsafe { &*list };
        let Body::List(_) = receiver.body() else {
            return Err(Error::TypeMismatch);
        };
        // SAFETY: 先为列表加一份引用（R32⑤）：调用方句柄仍持自己那份，须由调用方自己 free。
        // 顺序同 `ace_object_set`（P1a）：`child` 可能就是接收者自己，计数必须早于 body 变更。
        unsafe { Arc::increment_strong_count(child as *const AceValue) };
        // SAFETY: 计数操作已结束，此刻才建立 body 的可变借用。
        let Body::List(elements) = (unsafe { receiver.body_mut() }) else {
            unreachable!("上面已确认接收者是 List");
        };
        elements.push(unsafe { Arc::from_raw(child as *const AceValue) });
        Ok(())
    })
}

/// 读取列表长度（`ace_list_len`）。
#[no_mangle]
pub extern "C" fn ace_list_len(list: *const AceValue, out: *mut usize) -> i32 {
    guard(|| {
        let list = unsafe { list.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        match list.body() {
            Body::List(elements) => {
                *out = elements.len();
                Ok(())
            }
            _ => Err(Error::TypeMismatch),
        }
    })
}

/// 读取列表元素的借用视图（`ace_list_at`）：越界 → `12`，寿命 ≤ 父值、不得 free、不得 set/add。
#[no_mangle]
pub extern "C" fn ace_list_at(
    list: *const AceValue,
    index: usize,
    out: *mut *const AceValue,
) -> i32 {
    guard(|| {
        let list = unsafe { list.as_ref() }.ok_or(Error::InvalidArgument)?;
        let out = unsafe { out.as_mut() }.ok_or(Error::InvalidArgument)?;
        let Body::List(elements) = list.body() else {
            return Err(Error::TypeMismatch);
        };
        let found = elements.get(index).ok_or(Error::NotFound)?;
        *out = Arc::as_ptr(found);
        Ok(())
    })
}

// ——— 共享与释放（R32 引用计数） ———

/// 共享值（`ace_value_share`）：`*out` 与 `value` 指向同一节点、该节点引用计数 +1（`0 Ok`）。
/// 两枚句柄各自 `ace_value_free` 一次后节点恰释放一次，仍有引用时不得提前释放。
///
/// 失败三分类（R32①）：(a) `out == NULL` → `1`，**不访问输出**；(b) `out` 合法且
/// `value == NULL`（检查范围内可判定无效）→ `1` 且 `*out = NULL`，不留半成品、不沿用调用方
/// 原值；(c) 伪造/悬垂/非本库分配的 `value` 不新增任何安全保证（沿用 FR-A02）。
#[no_mangle]
pub extern "C" fn ace_value_share(value: *const AceValue, out: *mut *mut AceValue) -> i32 {
    let Some(out) = (unsafe { out.as_mut() }) else {
        // (a) 输出槽无效：不访问输出。
        return Error::InvalidArgument.code();
    };
    // 先置空：任何失败都不得留下半成品或沿用调用方原值（FR-A02）。
    *out = ptr::null_mut();
    guard(|| {
        if value.is_null() {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: 头约定可判定的非空 `value` 是本库分配的活句柄（伪造/悬垂不在保证内）。
        // 计数直接用调用方的裸指针：先转 `&AceValue` 再转回来的话，裸指针只拿到 `AceValue`
        // 载荷的 provenance，`Arc::from_raw` 随后访问前导计数字段就落到 provenance 之外
        // （Stacked Borrows 实测会报 UB），故此处只做空指针判定、不经过引用。
        *out = unsafe { share_handle(value) };
        Ok(())
    })
}

/// 减一个引用（`ace_value_free`）：引用计数 -1，归零才释放该节点及其整棵子树。
/// NULL 视为已释放（`0`）；借用视图（`get`/`at` 借出）不持引用，不得 free；
/// 强环（`set`/`add` 使对象引用自身或祖先）按引用计数语义不保证释放（调用方不得构造）。
#[no_mangle]
pub extern "C" fn ace_value_free(value: *mut AceValue) -> i32 {
    guard(|| {
        if value.is_null() {
            return Ok(());
        }
        // SAFETY: 未释放的句柄按头契约只释放一次；子树随引用归零由 `Arc` 析构释放。
        unsafe { release_handle(value) };
        Ok(())
    })
}

// ——— 类型引用节点与 schema（FR-A03：`AceTypeRef` / `AceSchema` / `AceEnumSchema`） ———

/// `AceFieldKind` 的取值（头 `AceFieldKind` 次序）。标量/宽值子集为
/// `ACE_FK_BOOL..ACE_FK_UINTPTR`，其中 `ACE_FK_STR`/`ACE_FK_BYTES` 不是子集成员
/// （走 `ace_type_str`/`ace_type_bytes`）。`ACE_FK_ENUM..ACE_FK_OBJ` 只是分类值，
/// 结构由对应的 `AceTypeRef` 节点种类表达，故不在此列常量。
const ACE_FK_BOOL: i32 = 0;
const ACE_FK_I8: i32 = 1;
const ACE_FK_U8: i32 = 2;
const ACE_FK_I16: i32 = 3;
const ACE_FK_U16: i32 = 4;
const ACE_FK_I32: i32 = 5;
const ACE_FK_U32: i32 = 6;
const ACE_FK_I64: i32 = 7;
const ACE_FK_U64: i32 = 8;
/// f32/f64 没有全序，故 `BTreeMap` 键类型不可能包含它们（FR-T03）。
const ACE_FK_F32: i32 = 9;
const ACE_FK_F64: i32 = 10;
const ACE_FK_CHAR: i32 = 11;
const ACE_FK_STR: i32 = 12;
const ACE_FK_BYTES: i32 = 13;
const ACE_FK_GUID: i32 = 14;
const ACE_FK_DATETIME: i32 = 15;
const ACE_FK_DATETIME_OFFSET: i32 = 16;
const ACE_FK_INTPTR: i32 = 17;
const ACE_FK_UINTPTR: i32 = 18;

/// 类型节点种类（FR-A03）：可任意嵌套组合，子节点一律由父节点收编。
enum TypeBody {
    /// 标量/宽值子集，值即 `AceFieldKind`。
    Prim(i32),
    Str,
    Bytes,
    List(Box<AceTypeRef>),
    /// 键须有线序（对齐 `BTreeMap K: Ord`）：`ace_type_map` 拒绝含 f32/f64 的键子树。
    Map(Box<AceTypeRef>, Box<AceTypeRef>),
    /// 内层不得再为 option（FR-T02）。
    Option(Box<AceTypeRef>),
    /// 借用 `AceSchema`（不收编）：被引用实体须晚于本节点释放。
    Object(*mut AceSchema),
    /// 借用 `AceEnumSchema`（不收编）；句柄留给后续阶段的 schema 注册读取。
    Enum(#[allow(dead_code)] *mut AceEnumSchema),
}

/// opaque 类型引用节点。
///
/// 每次收编都要求子句柄未被收编，且构造器只产出新节点，因此节点图是严格树：
/// 节点祖先环不可构造。模型里唯一能构造出的环是 schema 自引用（字段类型借用自身
/// schema），由 `ace_schema_add_field` 拒绝。
pub struct AceTypeRef {
    body: TypeBody,
    /// `true` 表示已被父节点或 schema 收编：随所有者释放，调用方不得再 `ace_type_free`。
    adopted: Cell<bool>,
}

impl AceTypeRef {
    fn new(body: TypeBody) -> Self {
        Self {
            body,
            adopted: Cell::new(false),
        }
    }
}

// 测试钩子：节点析构计数，用于证明收编后随父释放、失败不收编确实各只归还一次。
#[cfg(test)]
thread_local! {
    static TYPE_DROPPED: Cell<usize> = const { Cell::new(0) };
}

impl Drop for AceTypeRef {
    fn drop(&mut self) {
        #[cfg(test)]
        TYPE_DROPPED.with(|count| count.set(count.get() + 1));
        // 迭代摘子节点：深嵌套（如 List<List<…>>）不得把嵌套深度展开成栈深度。
        let mut pending: Vec<Box<AceTypeRef>> = Vec::new();
        detach_children(&mut self.body, &mut pending);
        while let Some(mut node) = pending.pop() {
            detach_children(&mut node.body, &mut pending);
        }
    }
}

/// 摘下 `body` 的子节点交给 `pending` 迭代释放；`body` 本身换成无子叶节点。
fn detach_children(body: &mut TypeBody, pending: &mut Vec<Box<AceTypeRef>>) {
    match std::mem::replace(body, TypeBody::Bytes) {
        TypeBody::List(child) | TypeBody::Option(child) => pending.push(child),
        TypeBody::Map(key, val) => {
            pending.push(key);
            pending.push(val);
        }
        _ => {}
    }
}

/// `AceEnumSchema` 的 repr：需求的 8 种定宽整数；未显式设置按 i32（FR-T01）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EnumRepr {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
}

impl EnumRepr {
    /// 由 `AceFieldKind` 取值映射；非 8 种定宽整数 → `None`。
    fn from_kind(kind: i32) -> Option<Self> {
        Some(match kind {
            1 => Self::I8,
            2 => Self::U8,
            3 => Self::I16,
            4 => Self::U16,
            5 => Self::I32,
            6 => Self::U32,
            7 => Self::I64,
            8 => Self::U64,
            _ => return None,
        })
    }

    fn width(self) -> u32 {
        match self {
            Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 => 32,
            Self::I64 | Self::U64 => 64,
        }
    }

    fn signed(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    /// 按 repr 宽度把 `uint64_t` 位模式截回：高位全 0（零扩展），或（有符号 repr）
    /// 为该位宽值的规范符号扩展；其余（超位宽、非规范扩展）→ `None`。
    fn truncate(self, tag: u64) -> Option<u64> {
        let width = self.width();
        if width == 64 {
            return Some(tag);
        }
        let low = tag & ((1u64 << width) - 1);
        let high = tag >> width;
        if high == 0 {
            return Some(low);
        }
        let sign_extension = (1u64 << (64 - width)) - 1;
        if self.signed() && high == sign_extension && (low >> (width - 1)) & 1 == 1 {
            return Some(low);
        }
        None
    }
}

/// opaque 整数枚举 schema：repr + 已注册变体集 + 可选默认变体。
pub struct AceEnumSchema {
    /// `ace_enum_new` 的 type_key 副本；判重与注册在 `ace_options_add_schema`（后续阶段），
    /// 故当前无本 crate 内读者。
    #[allow(dead_code)]
    type_key: Box<str>,
    repr: EnumRepr,
    /// 截回 repr 宽度后的变体判别值位模式。
    variants: BTreeSet<u64>,
    /// 可省略的默认变体（截回 repr 宽度后的位模式）。
    default: Option<u64>,
}

/// opaque schema：type_key + `field_id`（即 wire order）→ 字段类型节点。
pub struct AceSchema {
    /// 调用方维护的稳定 type_key 副本；注册进 options 的是后续阶段，故当前无本 crate 内读者。
    #[allow(dead_code)]
    type_key: Box<str>,
    /// 收编的字段类型节点随本 schema 释放；`BTreeMap` 升序与 wire order 递增一致。
    fields: BTreeMap<i32, Box<AceTypeRef>>,
}

/// 收编前校验：`child` 必须非空、且未被任何父节点或 schema 收编。失败时不收编。
fn check_type_child(child: *mut AceTypeRef) -> Result<(), Error> {
    if child.is_null() {
        return Err(Error::InvalidArgument);
    }
    // SAFETY: `child` 非空且按头契约由本库分配。
    if unsafe { (*child).adopted.get() } {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}

/// 收编子树：把 `child` 的所有权转给即将建成的父节点，子句柄就此失效。
///
/// SAFETY: 调用方须先用 [`check_type_child`] 确认 `child` 非空、未被收编，且同一节点
/// 在一次构造里只出现一次（否则所有者会重复释放）。
unsafe fn adopt_type(child: *mut AceTypeRef) -> Box<AceTypeRef> {
    let adopted = Box::from_raw(child);
    (*child).adopted.set(true);
    adopted
}

/// 迭代遍历 `root` 子树（含自身），`predicate` 为真即返回 `true`。
fn type_subtree_any(root: *const AceTypeRef, mut predicate: impl FnMut(&TypeBody) -> bool) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        // SAFETY: 子树节点归 `root` 所有，在本次调用期间存活。
        let body = unsafe { &(*node).body };
        if predicate(body) {
            return true;
        }
        match body {
            TypeBody::List(child) | TypeBody::Option(child) => pending.push(&**child),
            TypeBody::Map(key, val) => {
                pending.push(&**key);
                pending.push(&**val);
            }
            _ => {}
        }
    }
    false
}

/// 构造 Prim 类型节点（`ace_type_prim`）：仅接受标量/宽值子集，即
/// `ACE_FK_BOOL..ACE_FK_UINTPTR` 去掉 `ACE_FK_STR`/`ACE_FK_BYTES` 后的取值；
/// STR/BYTES 与复合/ENUM 分类值一律 `1`。
#[no_mangle]
pub extern "C" fn ace_type_prim(kind: i32, out: *mut *mut AceTypeRef) -> i32 {
    construct_handle(out, || {
        if !(0..=ACE_FK_UINTPTR).contains(&kind) || kind == ACE_FK_STR || kind == ACE_FK_BYTES {
            return Err(Error::InvalidArgument);
        }
        Ok(AceTypeRef::new(TypeBody::Prim(kind)))
    })
}

/// 构造 Bytes 类型节点（`ace_type_bytes`）。
#[no_mangle]
pub extern "C" fn ace_type_bytes(out: *mut *mut AceTypeRef) -> i32 {
    construct_handle(out, || Ok(AceTypeRef::new(TypeBody::Bytes)))
}

/// 构造 Str 类型节点（`ace_type_str`）。
#[no_mangle]
pub extern "C" fn ace_type_str(out: *mut *mut AceTypeRef) -> i32 {
    construct_handle(out, || Ok(AceTypeRef::new(TypeBody::Str)))
}

/// 构造 List 类型节点（`ace_type_list`）：成功收编 `elem`，失败不收编。
#[no_mangle]
pub extern "C" fn ace_type_list(elem: *mut AceTypeRef, out: *mut *mut AceTypeRef) -> i32 {
    construct_handle(out, || {
        check_type_child(elem)?;
        // SAFETY: 上面已确认 `elem` 非空且未收编，此处只收编一次。
        Ok(AceTypeRef::new(TypeBody::List(unsafe {
            adopt_type(elem)
        })))
    })
}

/// 构造 Map 类型节点（`ace_type_map`）：成功收编 `key`/`val`，失败不收编。
///
/// 键须有线序（对齐 `BTreeMap K: Ord`）：键子树含 f32/f64 无全序 → `1`。
/// 同一节点不能同时作 key 与 val（两处收编会让所有者重复释放）→ `1`。
#[no_mangle]
pub extern "C" fn ace_type_map(
    key: *mut AceTypeRef,
    val: *mut AceTypeRef,
    out: *mut *mut AceTypeRef,
) -> i32 {
    construct_handle(out, || {
        check_type_child(key)?;
        check_type_child(val)?;
        if ptr::eq(key, val) {
            return Err(Error::InvalidArgument);
        }
        if type_subtree_any(key, |body| {
            matches!(body, TypeBody::Prim(kind) if *kind == ACE_FK_F32 || *kind == ACE_FK_F64)
        }) {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: 上面已确认两个子节点非空、未收编且互不相同；收编后归本节点所有。
        let (key, val) = unsafe { (adopt_type(key), adopt_type(val)) };
        Ok(AceTypeRef::new(TypeBody::Map(key, val)))
    })
}

/// 构造 Option 类型节点（`ace_type_option`）：成功收编 `inner`，失败不收编。
/// 内层不得再为 option（FR-T02：`Option<Option<T>>` 禁止）→ `1`。
#[no_mangle]
pub extern "C" fn ace_type_option(inner: *mut AceTypeRef, out: *mut *mut AceTypeRef) -> i32 {
    construct_handle(out, || {
        check_type_child(inner)?;
        // SAFETY: `check_type_child` 已确认 `inner` 非空且由本库分配。
        if matches!(unsafe { &(*inner).body }, TypeBody::Option(_)) {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: 同上，`inner` 未收编且此处只收编一次。
        Ok(AceTypeRef::new(TypeBody::Option(unsafe {
            adopt_type(inner)
        })))
    })
}

/// 构造 Object 类型节点（`ace_type_object`）：借用已 `ace_schema_new` 的 `AceSchema`，不收编。
#[no_mangle]
pub extern "C" fn ace_type_object(schema: *mut AceSchema, out: *mut *mut AceTypeRef) -> i32 {
    construct_handle(out, || {
        if schema.is_null() {
            return Err(Error::InvalidArgument);
        }
        Ok(AceTypeRef::new(TypeBody::Object(schema)))
    })
}

/// 构造 Enum 类型节点（`ace_type_enum`）：借用 `AceEnumSchema`，不收编。
#[no_mangle]
pub extern "C" fn ace_type_enum(
    enum_schema: *mut AceEnumSchema,
    out: *mut *mut AceTypeRef,
) -> i32 {
    construct_handle(out, || {
        if enum_schema.is_null() {
            return Err(Error::InvalidArgument);
        }
        Ok(AceTypeRef::new(TypeBody::Enum(enum_schema)))
    })
}

/// 释放未被收编的类型节点及其整棵子树（`ace_type_free`）。NULL 视为已释放（`0`）
/// （同 `ace_value_free`）；已被父节点或 schema 收编的句柄随所有者释放，单独释放 → `1` 且不释放。
#[no_mangle]
pub extern "C" fn ace_type_free(type_ref: *mut AceTypeRef) -> i32 {
    guard(|| {
        if type_ref.is_null() {
            return Ok(());
        }
        // SAFETY: `type_ref` 非空且按头契约由本库分配。
        if unsafe { (*type_ref).adopted.get() } {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: 未收编的顶层节点只释放一次；子树由 `Drop` 迭代释放。
        drop(unsafe { Box::from_raw(type_ref) });
        Ok(())
    })
}

/// 新建 schema（`ace_schema_new`）：`type_key` 必填，复制进库。
#[no_mangle]
pub extern "C" fn ace_schema_new(type_key: *const c_char, out: *mut *mut AceSchema) -> i32 {
    construct_handle(out, || {
        Ok(AceSchema {
            type_key: required_type_key(type_key)?,
            fields: BTreeMap::new(),
        })
    })
}

/// 释放 schema 连同其收编的全部字段类型节点（`ace_schema_free`）。
///
/// 头只要求调用方自行保证「被借用的 schema/enum 晚于引用它的类型节点释放」，未声明
/// 强制校验顺序，故这里不做任何判序，只释放自身拥有的部分。
#[no_mangle]
pub extern "C" fn ace_schema_free(schema: *mut AceSchema) -> i32 {
    guard(|| {
        if schema.is_null() {
            return Ok(());
        }
        // SAFETY: 非空 schema 按头契约只释放一次；字段类型节点随本结构析构。
        drop(unsafe { Box::from_raw(schema) });
        Ok(())
    })
}

/// 挂接字段类型（`ace_schema_add_field`）：成功即收编 `field_type`，失败不收编。
///
/// 失败顺序：NULL schema/类型节点 → `1`；`field_id < 0`（order 非负）→ `1`；重复 `field_id` → `1`；
/// 已收编的节点 → `1`；字段类型借用了本 schema（自引用环）→ `1`。
#[no_mangle]
pub extern "C" fn ace_schema_add_field(
    schema: *mut AceSchema,
    field_id: i32,
    field_type: *mut AceTypeRef,
) -> i32 {
    guard(|| {
        let schema = unsafe { schema.as_mut() }.ok_or(Error::InvalidArgument)?;
        let schema_ptr: *mut AceSchema = schema;
        if field_id < 0 {
            return Err(Error::InvalidArgument);
        }
        check_type_child(field_type)?;
        if schema.fields.contains_key(&field_id) {
            return Err(Error::InvalidArgument);
        }
        if type_subtree_any(field_type, |body| {
            matches!(body, TypeBody::Object(target) if ptr::eq(*target, schema_ptr))
        }) {
            // schema 自引用：字段类型借用了它即将归属的 schema，深拷贝 schema 图会无限展开。
            return Err(Error::InvalidArgument);
        }
        // SAFETY: 上面已确认 `field_type` 非空且未收编，此处只收编一次。
        let adopted = unsafe { adopt_type(field_type) };
        schema.fields.insert(field_id, adopted);
        Ok(())
    })
}

/// 新建整数枚举 schema（`ace_enum_new`）：`type_key` 必填，复制进库；repr 未设置时按 i32。
#[no_mangle]
pub extern "C" fn ace_enum_new(type_key: *const c_char, out: *mut *mut AceEnumSchema) -> i32 {
    construct_handle(out, || {
        Ok(AceEnumSchema {
            type_key: required_type_key(type_key)?,
            repr: EnumRepr::I32,
            variants: BTreeSet::new(),
            default: None,
        })
    })
}

/// 释放枚举 schema（`ace_enum_free`）。
#[no_mangle]
pub extern "C" fn ace_enum_free(enum_schema: *mut AceEnumSchema) -> i32 {
    guard(|| {
        if enum_schema.is_null() {
            return Ok(());
        }
        // SAFETY: 非空枚举 schema 按头契约只释放一次。
        drop(unsafe { Box::from_raw(enum_schema) });
        Ok(())
    })
}

/// 设置 repr（`ace_enum_set_repr`）：仅 8 种定宽整数，其余 → `1`。
/// 若已有变体/默认变体放不进新 repr（超位宽且非规范符号扩展）→ `1`，repr 保持不变。
#[no_mangle]
pub extern "C" fn ace_enum_set_repr(enum_schema: *mut AceEnumSchema, kind: i32) -> i32 {
    guard(|| {
        let enum_schema = unsafe { enum_schema.as_mut() }.ok_or(Error::InvalidArgument)?;
        let repr = EnumRepr::from_kind(kind).ok_or(Error::InvalidArgument)?;
        let fits = enum_schema
            .variants
            .iter()
            .chain(enum_schema.default.iter())
            .all(|tag| repr.truncate(*tag).is_some());
        if !fits {
            return Err(Error::InvalidArgument);
        }
        enum_schema.repr = repr;
        Ok(())
    })
}

/// 注册变体判别值（`ace_enum_add_variant`）：`tag` 按位模式承载，不在 repr 位宽内
/// （且非该位宽的规范符号扩展）或截回后与已有变体重复 → `1`。
#[no_mangle]
pub extern "C" fn ace_enum_add_variant(enum_schema: *mut AceEnumSchema, tag: u64) -> i32 {
    guard(|| {
        let enum_schema = unsafe { enum_schema.as_mut() }.ok_or(Error::InvalidArgument)?;
        let tag = enum_schema.repr.truncate(tag).ok_or(Error::InvalidArgument)?;
        if !enum_schema.variants.insert(tag) {
            return Err(Error::InvalidArgument);
        }
        Ok(())
    })
}

/// 标记可选默认变体（`ace_enum_set_default_variant`）：仅当存在同 tag（截回后同值）变体，
/// 否则（不在变体集或超位宽）→ `1`。
#[no_mangle]
pub extern "C" fn ace_enum_set_default_variant(enum_schema: *mut AceEnumSchema, tag: u64) -> i32 {
    guard(|| {
        let enum_schema = unsafe { enum_schema.as_mut() }.ok_or(Error::InvalidArgument)?;
        let tag = enum_schema.repr.truncate(tag).ok_or(Error::InvalidArgument)?;
        if !enum_schema.variants.contains(&tag) {
            return Err(Error::InvalidArgument);
        }
        enum_schema.default = Some(tag);
        Ok(())
    })
}

// ——— options（限制副本 + schema 白名单；R25 的 build_nesting 单列） ———

/// arena 下标：深拷贝的图中子节点与被借用实体都用它引用，故拷贝与释放都不展开成栈深度。
type NodeId = usize;

/// 深拷贝后的节点。互引 schema（`Object` ↔ `Schema`）只构成 arena 内的环，不是递归结构。
/// `ace_serialize` 的编码适配器按这里的节点种类分派（见 [`Dyn`]）。
enum OwnedNode {
    Prim(i32),
    Str,
    Bytes,
    List(NodeId),
    Map(NodeId, NodeId),
    Option(NodeId),
    /// 被 `ace_type_object` 借用的 schema：指向一个 [`OwnedNode::Schema`]。
    Object(NodeId),
    /// 被 `ace_type_enum` 借用的枚举 schema：指向一个 [`OwnedNode::EnumDef`]。
    Enum(NodeId),
    /// 一个 schema 的值副本：type_key + 字段表。
    Schema {
        /// 该 schema 已注册的 `type_key`：既是白名单的键，也是多态分派（R33②）时判定
        /// 「值自带的 key 与所在位置的静态 schema 是否同一个类型」的依据。
        type_key: Box<str>,
        fields: BTreeMap<i32, NodeId>,
    },
    /// 一个枚举 schema 的值副本。
    EnumDef {
        /// 同上：白名单的键就是 `type_key`。
        #[allow(dead_code)]
        type_key: Box<str>,
        repr: EnumRepr,
        variants: BTreeSet<u64>,
        default: Option<u64>,
    },
}

/// 拷贝来源：只在复制期间作去重键，不进入库内副本（副本里不保存裸指针）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CopySource {
    Type(*const AceTypeRef),
    Schema(*const AceSchema),
    Enum(*const AceEnumSchema),
}

impl CopySource {
    fn type_node(node: &AceTypeRef) -> Self {
        Self::Type(node as *const AceTypeRef)
    }

    fn schema(schema: &AceSchema) -> Self {
        Self::Schema(schema as *const AceSchema)
    }

    fn enum_schema(enum_schema: &AceEnumSchema) -> Self {
        Self::Enum(enum_schema as *const AceEnumSchema)
    }
}

/// 两趟深拷贝的登记表：第一趟按来源登记 arena 下标（互引/共享实体只登记一次），
/// 第二趟按登记顺序建节点。两趟都是迭代遍历，不随图深度占栈。
///
/// 节点直接追加进 options 的共享 arena，故下标以 `base`（本次复制的起始下标）偏移——全部已注册
/// schema 的节点下标因此全局唯一，多态分派只需换根下标（R33②）。
struct CopyArena<'a> {
    /// 目标 arena：options 的节点表。
    nodes: &'a mut Vec<OwnedNode>,
    /// 本次复制的起始下标，即复制前 arena 的长度。
    base: NodeId,
    /// 本次复制内的登记顺序；绝对 arena 下标为 `base + 登记序`。
    sources: Vec<CopySource>,
    /// 来源 → arena 下标；第一趟只增不改，故第二趟查到的子下标一定已登记。
    ids: BTreeMap<CopySource, NodeId>,
}

impl CopyArena<'_> {
    fn register(&mut self, source: CopySource) -> NodeId {
        if let Some(id) = self.ids.get(&source) {
            return *id;
        }
        let id = self.base + self.sources.len();
        self.ids.insert(source, id);
        self.sources.push(source);
        id
    }

    /// 第一趟：登记根 schema 及其整张图的全部实体，返回根在图中的下标。
    fn register_graph(&mut self, root: &AceSchema) -> NodeId {
        let root_id = self.register(CopySource::schema(root));
        let mut index = 0;
        while index < self.sources.len() {
            let source = self.sources[index];
            index += 1;
            match source {
                CopySource::Type(node) => {
                    // SAFETY: 来源类型节点按头契约在注册期间存活。
                    match unsafe { &(*node).body } {
                        TypeBody::List(child) | TypeBody::Option(child) => {
                            self.register(CopySource::type_node(child));
                        }
                        TypeBody::Map(key, val) => {
                            self.register(CopySource::type_node(key));
                            self.register(CopySource::type_node(val));
                        }
                        TypeBody::Object(target) => {
                            // SAFETY: 借用的 schema 须晚于引用它的类型节点释放，注册期间存活。
                            self.register(CopySource::schema(unsafe { &**target }));
                        }
                        TypeBody::Enum(target) => {
                            // SAFETY: 借用的枚举 schema 同上，注册期间存活。
                            self.register(CopySource::enum_schema(unsafe { &**target }));
                        }
                        TypeBody::Prim(_) | TypeBody::Str | TypeBody::Bytes => {}
                    }
                }
                CopySource::Schema(schema) => {
                    // SAFETY: 来源 schema 按头契约在注册期间存活。
                    for field_type in unsafe { &(*schema).fields }.values() {
                        self.register(CopySource::type_node(field_type));
                    }
                }
                CopySource::Enum(_) => {}
            }
        }
        root_id
    }

    /// 第二趟：按登记顺序建节点并追加进 arena，子节点与借用实体一律改写成 arena 下标。
    fn build(&mut self) {
        let sources = self.sources.clone();
        let built: Vec<OwnedNode> = sources.into_iter().map(|source| self.node(source)).collect();
        self.nodes.extend(built);
    }

    /// 已登记实体的 arena 下标。第一趟登记了图中全部实体，故查找必命中；
    /// 缺失只可能是本模块内的逻辑错误，跨 FFI 由 [`guard`] 映射为 `10`。
    fn id(&self, source: CopySource) -> NodeId {
        self.ids[&source]
    }

    fn node(&self, source: CopySource) -> OwnedNode {
        match source {
            CopySource::Type(node) => {
                // SAFETY: 同第一趟，来源类型节点在注册期间存活。
                match unsafe { &(*node).body } {
                    TypeBody::Prim(kind) => OwnedNode::Prim(*kind),
                    TypeBody::Str => OwnedNode::Str,
                    TypeBody::Bytes => OwnedNode::Bytes,
                    TypeBody::List(child) => OwnedNode::List(self.id(CopySource::type_node(child))),
                    TypeBody::Map(key, val) => OwnedNode::Map(
                        self.id(CopySource::type_node(key)),
                        self.id(CopySource::type_node(val)),
                    ),
                    TypeBody::Option(inner) => {
                        OwnedNode::Option(self.id(CopySource::type_node(inner)))
                    }
                    // SAFETY: 被借用实体在注册期间存活，第一趟已为其登记下标。
                    TypeBody::Object(target) => {
                        OwnedNode::Object(self.id(CopySource::schema(unsafe { &**target })))
                    }
                    TypeBody::Enum(target) => {
                        OwnedNode::Enum(self.id(CopySource::enum_schema(unsafe { &**target })))
                    }
                }
            }
            CopySource::Schema(schema) => {
                // SAFETY: 同第一趟。
                let schema = unsafe { &*schema };
                OwnedNode::Schema {
                    type_key: schema.type_key.clone(),
                    fields: schema
                        .fields
                        .iter()
                        .map(|(field_id, field_type)| {
                            (*field_id, self.id(CopySource::type_node(field_type)))
                        })
                        .collect(),
                }
            }
            CopySource::Enum(enum_schema) => {
                // SAFETY: 同第一趟。
                let enum_schema = unsafe { &*enum_schema };
                OwnedNode::EnumDef {
                    type_key: enum_schema.type_key.clone(),
                    repr: enum_schema.repr,
                    variants: enum_schema.variants.clone(),
                    default: enum_schema.default,
                }
            }
        }
    }
}

/// 深拷贝 `schema` 的整张图并追加进 `nodes`，返回根 schema 的 arena 下标。
///
/// 收编的类型节点、被借用 schema 与被借用枚举 schema 一律按值重建；复制成功后调用方可 free
/// 全部调用方侧结构。每次注册都跑一趟新登记表：来源指针只在本趟内作去重键，不跨调用缓存
/// （调用方释放后地址可能被复用）。
fn copy_schema(nodes: &mut Vec<OwnedNode>, schema: &AceSchema) -> NodeId {
    let base = nodes.len();
    let mut arena = CopyArena {
        nodes,
        base,
        sources: Vec::new(),
        ids: BTreeMap::new(),
    };
    let root = arena.register_graph(schema);
    arena.build();
    root
}

/// opaque options 句柄：Rust [`Options`] 限制副本 + 已注册 schema 白名单（深拷贝图）。
pub struct AceOptions {
    limits: Options,
    /// 白名单：`type_key` → 根 schema 在 `nodes` 中的下标。键即 `ace_options_add_schema`
    /// 判重 `type_key` 的唯一位置，也是编解码多态分派（R33②）的查找表。
    schemas: BTreeMap<Box<str>, NodeId>,
    /// 全部已注册 schema 共用的深拷贝 arena；`Object`/`Enum` 节点用下标指向同一 arena 的
    /// `Schema`/`EnumDef`。共用一张表使下标全局唯一，形状比较与多态分派都不必跳 arena。
    nodes: Vec<OwnedNode>,
}

/// 新建 options（`ace_options_new`）：默认值与 Rust `Options::default()` 一致，白名单为空。
/// 具体数值（packed=true、深度 256、身份构造闭包 256、64 MiB 各预算、2^20 记录/槽/引用、
/// 2^24 集合、1 MiB Map 键）以 `ace_serialize::limits::Options::default()` 为单一来源，此处不复制。
#[no_mangle]
pub extern "C" fn ace_options_new(out: *mut *mut AceOptions) -> i32 {
    construct_handle(out, || {
        Ok(AceOptions {
            limits: Options::default(),
            schemas: BTreeMap::new(),
            nodes: Vec::new(),
        })
    })
}

/// 释放 options 及其白名单副本（`ace_options_free`）。NULL 视为已释放（`0`，同 `ace_value_free`）；
/// 非空句柄只释放一次，二次释放不承诺安全（同头）。
#[no_mangle]
pub extern "C" fn ace_options_free(options: *mut AceOptions) -> i32 {
    guard(|| {
        if options.is_null() {
            return Ok(());
        }
        // SAFETY: 非空 options 按头契约只释放一次；白名单副本随本结构析构。
        drop(unsafe { Box::from_raw(options) });
        Ok(())
    })
}

/// 设置 packed（`ace_options_set_packed`）：仅接受 0/1（同 `ace_value_bool`），其余 → `1` 且不改原值。
#[no_mangle]
pub extern "C" fn ace_options_set_packed(options: *mut AceOptions, enabled: u8) -> i32 {
    guard(|| {
        let options = unsafe { options.as_mut() }.ok_or(Error::InvalidArgument)?;
        options.limits.packed = match enabled {
            0 => false,
            1 => true,
            _ => return Err(Error::InvalidArgument),
        };
        Ok(())
    })
}

/// 设置各维度限额（`ace_options_set_limits`）：按头里的参数序逐个映射到 [`Options`]。
/// 零值合法，语义是该维度最严格的上限而非禁用限制（见 `limits.rs`）。
/// 身份构造闭包峰值不在此列，头将其单列为 `ace_options_set_build_nesting`。
///
/// 失败顺序：NULL options → `1`（其余参数为 `size_t`，无非法取值）。
#[no_mangle]
pub extern "C" fn ace_options_set_limits(
    options: *mut AceOptions,
    depth: usize,
    input_bytes: usize,
    raw_bytes: usize,
    decoded_bytes: usize,
    records: usize,
    slots: usize,
    collection_len: usize,
    references: usize,
    map_string_key_bytes: usize,
) -> i32 {
    guard(|| {
        let options = unsafe { options.as_mut() }.ok_or(Error::InvalidArgument)?;
        let limits = &mut options.limits;
        limits.max_depth = depth;
        limits.max_input_bytes = input_bytes;
        limits.max_raw_bytes = raw_bytes;
        limits.max_decoded_bytes = decoded_bytes;
        limits.max_records = records;
        limits.max_slots = slots;
        limits.max_collection_len = collection_len;
        limits.max_references = references;
        limits.max_map_string_key_bytes = map_string_key_bytes;
        Ok(())
    })
}

/// 设置解码侧身份构造闭包峰值上限（`ace_options_set_build_nesting`，需求 R25）：默认 256，
/// 超限返回 `6 ResourceLimit`。只作用于解码规划期；编码侧不使用该参数——编码的嵌套深度
/// 一律由 `max_depth` 约束并返回 `5 DepthExceeded`。零值合法（不允许身份节点）。
///
/// 失败顺序：NULL options → `1`。
#[no_mangle]
pub extern "C" fn ace_options_set_build_nesting(options: *mut AceOptions, limit: usize) -> i32 {
    guard(|| {
        let options = unsafe { options.as_mut() }.ok_or(Error::InvalidArgument)?;
        options.limits.max_build_nesting = limit;
        Ok(())
    })
}

/// 注册 schema（`ace_options_add_schema`）：深拷贝 schema 图进库，重复 `type_key` → `1`。
///
/// 副本用 Rust 自有类型重建（类型节点、被借用 schema 与被借用枚举 schema 全部按值复制），
/// 不保存裸指针，故复制成功后调用方仍可 free 全部调用方侧结构而不影响库内副本。
/// 拷贝在插入前完成，失败（NULL 参数/重复 `type_key`）不影响已注册集合。
///
/// 失败顺序：NULL options 或 NULL schema → `1`；`type_key` 已注册 → `1`。
#[no_mangle]
pub extern "C" fn ace_options_add_schema(
    options: *mut AceOptions,
    schema: *const AceSchema,
) -> i32 {
    guard(|| {
        let options = unsafe { options.as_mut() }.ok_or(Error::InvalidArgument)?;
        // SAFETY: `schema` 按头契约是调用方存活期内的 `ace_schema_new` 句柄。
        let schema = unsafe { schema.as_ref() }.ok_or(Error::InvalidArgument)?;
        if options.schemas.contains_key(&*schema.type_key) {
            return Err(Error::InvalidArgument);
        }
        let root = copy_schema(&mut options.nodes, schema);
        options.schemas.insert(schema.type_key.clone(), root);
        Ok(())
    })
}

// ——— codec：`ace_serialize` / `ace_buffer_free`（P3-3b） ———

// 测试钩子：置位后让 `ace_serialize` 的入口体 panic 一次，用于证明 panic 经 `guard` 映射为 `10`。
#[cfg(test)]
thread_local! {
    static PANIC_IN_SERIALIZE: Cell<bool> = const { Cell::new(false) };
}

/// `AceBuffer` 的 C 布局（头 `AceBuffer`）：`{ptr,len,cap}` 由 Rust 维护。
///
/// `ptr` 来自本库 `Box<[u8]>` 的分配，`len` 是实际字节数，`cap` 恒等于 `len`
/// （拥有值编码路径按精确长度交付，不暴露 `Vec` 的额外容量）。
#[repr(C)]
pub struct AceBuffer {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl AceBuffer {
    /// 空缓冲：失败与释放后都停在这个状态（`ptr=NULL,len=0,cap=0`）。
    fn empty() -> Self {
        Self {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }
}

/// 标量/宽值节点（`OwnedNode::Prim`）的 wire 形态。
enum Wire<'a> {
    /// 8B 槽载荷；元素位置只取前 `width` 字节（FR-W12 的自然宽度）。
    Slot { payload: [u8; 8], width: usize },
    /// 定长记录体（Guid 16B、DateTimeOffset 10B）：字段位置的记录体与元素体字节相同。
    Record(&'a [u8]),
}

/// `I64`/`U64` 值的公共数值视图：`u64` 全范围可用 `i128` 无损承载。
fn integer_value(value: &AceValue) -> AceResult<i128> {
    match value.body() {
        Body::I64(number) => Ok(*number as i128),
        Body::U64(number) => Ok(*number as i128),
        _ => Err(Error::TypeMismatch),
    }
}

/// `I64`/`U64` 值到目标有符号宽度的无损范围转换（FR-A03）；越界即 `13 TypeMismatch`。
fn fit_signed(value: &AceValue, bits: u32) -> AceResult<i64> {
    let bound = 1i128 << (bits - 1);
    let number = integer_value(value)?;
    if number < -bound || number >= bound {
        return Err(Error::TypeMismatch);
    }
    Ok(number as i64)
}

/// `I64`/`U64` 值到目标无符号宽度的无损范围转换（FR-A03）；负数或越界即 `13 TypeMismatch`。
fn fit_unsigned(value: &AceValue, bits: u32) -> AceResult<u64> {
    let bound = 1i128 << bits;
    let number = integer_value(value)?;
    if number < 0 || number >= bound {
        return Err(Error::TypeMismatch);
    }
    Ok(number as u64)
}

/// F64→f32 的按位映射（头 `ace_value_f64_bits` 注释写死的规则）：±0（复制 sign）与 ±inf 恒成功；
/// NaN 仅当 f64 fraction 低 29 位全 0（`f32 = sign<<31 | exp全1 | (F>>29)&0x7FFFFF`）；
/// 有限值仅当可精确表示为 f32（转换后转回 f64 与原位模式逐位一致）。
fn narrow_f64_bits(bits: u64) -> Option<u32> {
    let sign = ((bits >> 63) as u32) << 31;
    let exponent = (bits >> 52) & 0x7FF;
    let fraction = bits & ((1u64 << 52) - 1);
    if exponent == 0x7FF {
        return if fraction == 0 {
            Some(sign | 0x7F80_0000)
        } else if fraction & 0x1FFF_FFFF == 0 {
            Some(sign | 0x7F80_0000 | ((fraction >> 29) as u32 & 0x7F_FFFF))
        } else {
            None
        };
    }
    if exponent == 0 && fraction == 0 {
        return Some(sign);
    }
    let narrowed = f64::from_bits(bits) as f32;
    (f64::from(narrowed).to_bits() == bits).then(|| narrowed.to_bits())
}

/// 标量/宽值 kind 的 wire 形态；kind 与值的动态种类不匹配一律 `13 TypeMismatch`。
fn scalar_wire<'a>(kind: i32, value: &'a AceValue) -> AceResult<Wire<'a>> {
    let slot = |payload: [u8; 8], width: usize| Wire::Slot { payload, width };
    match kind {
        ACE_FK_BOOL => match value.body() {
            Body::Bool(flag) => Ok(slot(value::encode_bool(*flag), 1)),
            _ => Err(Error::TypeMismatch),
        },
        ACE_FK_I8 => Ok(slot(value::encode_i8(fit_signed(value, 8)? as i8), 1)),
        ACE_FK_U8 => Ok(slot(value::encode_u8(fit_unsigned(value, 8)? as u8), 1)),
        ACE_FK_I16 => Ok(slot(value::encode_i16(fit_signed(value, 16)? as i16), 2)),
        ACE_FK_U16 => Ok(slot(value::encode_u16(fit_unsigned(value, 16)? as u16), 2)),
        ACE_FK_I32 => Ok(slot(value::encode_i32(fit_signed(value, 32)? as i32), 4)),
        ACE_FK_U32 => Ok(slot(value::encode_u32(fit_unsigned(value, 32)? as u32), 4)),
        ACE_FK_I64 => Ok(slot(value::encode_i64(fit_signed(value, 64)?), 8)),
        ACE_FK_U64 => Ok(slot(value::encode_u64(fit_unsigned(value, 64)?), 8)),
        ACE_FK_F32 => match value.body() {
            Body::F64(bits) => {
                let narrowed = narrow_f64_bits(*bits).ok_or(Error::TypeMismatch)?;
                Ok(slot(value::encode_f32(f32::from_bits(narrowed)), 4))
            }
            _ => Err(Error::TypeMismatch),
        },
        ACE_FK_F64 => match value.body() {
            Body::F64(bits) => Ok(slot(bits.to_le_bytes(), 8)),
            _ => Err(Error::TypeMismatch),
        },
        ACE_FK_CHAR => {
            let character =
                char::from_u32(fit_unsigned(value, 32)? as u32).ok_or(Error::TypeMismatch)?;
            Ok(slot(value::encode_char(character), 4))
        }
        ACE_FK_GUID => match value.body() {
            Body::Bytes(bytes) if bytes.len() == 16 => Ok(Wire::Record(bytes.as_ref())),
            _ => Err(Error::TypeMismatch),
        },
        ACE_FK_DATETIME => {
            let bits = match value.body() {
                Body::U64(bits) => *bits,
                Body::I64(number) => *number as u64,
                _ => return Err(Error::TypeMismatch),
            };
            // 与 `value::encode_datetime` 同一规范：Kind 与 ticks 越界 → 4 InvalidTag。
            value::decode_datetime(bits)?;
            Ok(slot(bits.to_le_bytes(), 8))
        }
        ACE_FK_DATETIME_OFFSET => match value.body() {
            Body::Bytes(bytes) if bytes.len() == 10 => {
                let fixed: &[u8; 10] =
                    bytes.as_ref().try_into().map_err(|_| Error::TypeMismatch)?;
                // 与 `value::encode_datetime_offset` 同一规范：非法位模式 → 4 InvalidTag。
                value::decode_datetime_offset(fixed)?;
                Ok(Wire::Record(bytes.as_ref()))
            }
            _ => Err(Error::TypeMismatch),
        },
        ACE_FK_INTPTR => Ok(slot(value::encode_intptr(fit_signed(value, 64)?), 8)),
        ACE_FK_UINTPTR => Ok(slot(value::encode_uintptr(fit_unsigned(value, 64)?), 8)),
        // `TypeBody::Prim` 只收标量/宽值子集，其余 kind 不可达。
        _ => Err(Error::Unsupported),
    }
}

/// 标量 kind 恰好是 derive 的 `Kind::Scalar` 集合（12 种原生标量）：结构体字段取默认值时不写槽。
fn is_scalar_kind(kind: i32) -> bool {
    (ACE_FK_BOOL..=ACE_FK_CHAR).contains(&kind)
}

/// derive 的 `Kind::Scalar` 默认值判据：8B 载荷全零即 `value::is_default_*` 为真
/// （整数/char 的 0、`false`、f32/f64 的 `bits() == 0`）。
fn is_default_scalar(wire: &Wire<'_>) -> bool {
    match wire {
        Wire::Slot { payload, .. } => *payload == [0; 8],
        Wire::Record(_) => false,
    }
}

/// `AceValue` 的结构全序：`ace_deserialize` 的映射键判重用。
///
/// 整数按数值比较（`I64`/`U64` 同序）、字符串/字节按辞序、`List` 逐元素、`Object` 按
/// `field_id` 升序、`Null` 最小。解码出的键已按 schema 补全默认值，故这里比的就是有效值；编码侧
/// 的值还没补默认，不得用本函数判重，见 [`Effective`]。复合键里若含 Map，其条目按 ABI 列表次序
/// 比较（解码按 wire 次序装列表，编码按有效键序写，故两者同序）。
fn structural_order(left: &AceValue, right: &AceValue) -> Ordering {
    fn rank(value: &AceValue) -> u8 {
        match value.body() {
            Body::Null => 0,
            Body::Bool(_) => 1,
            Body::I64(_) | Body::U64(_) => 2,
            Body::F64(_) => 3,
            Body::Str(_) => 4,
            Body::Bytes(_) => 5,
            Body::List(_) => 6,
            Body::Object(_) => 7,
        }
    }
    let numbers = match (left.body(), right.body()) {
        (Body::I64(x), Body::I64(y)) => Some(x.cmp(y)),
        (Body::U64(x), Body::U64(y)) => Some(x.cmp(y)),
        (Body::I64(x), Body::U64(y)) => Some((*x as i128).cmp(&(*y as i128))),
        (Body::U64(x), Body::I64(y)) => Some((*x as i128).cmp(&(*y as i128))),
        _ => None,
    };
    if let Some(order) = numbers {
        return order;
    }
    match (left.body(), right.body()) {
        (Body::Null, Body::Null) => Ordering::Equal,
        (Body::Bool(x), Body::Bool(y)) => x.cmp(y),
        (Body::F64(x), Body::F64(y)) => x.cmp(y),
        (Body::Str(x), Body::Str(y)) => x.cmp(y),
        (Body::Bytes(x), Body::Bytes(y)) => x.cmp(y),
        (Body::List(x), Body::List(y)) => x
            .iter()
            .zip(y.iter())
            .map(|(x, y)| structural_order(x, y))
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| x.len().cmp(&y.len())),
        (Body::Object(x), Body::Object(y)) => x
            .fields
            .iter()
            .zip(y.fields.iter())
            .map(|((x_field, x_value), (y_field, y_value))| {
                x_field
                    .cmp(y_field)
                    .then_with(|| structural_order(x_value, y_value))
            })
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| x.fields.len().cmp(&y.fields.len())),
        _ => rank(left).cmp(&rank(right)),
    }
}

/// [`structural_order`] 的 `Ord` 包装：Map 解码用它在解完**每一对**后增量查已见键，
/// 与核心 `BTreeMap` 的键序同一判据（`Equal` 即重复键）。
struct OrderedKey(Arc<AceValue>);

impl PartialEq for OrderedKey {
    fn eq(&self, other: &Self) -> bool {
        structural_order(&self.0, &other.0) == Ordering::Equal
    }
}

impl Eq for OrderedKey {}

impl PartialOrd for OrderedKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedKey {
    fn cmp(&self, other: &Self) -> Ordering {
        structural_order(&self.0, &other.0)
    }
}

/// 映射键的**有效值**（B1）：按 key schema 归一后的键，也就是**解码后会得到的那份值**。
///
/// 编码省略默认标量、`Option::None` 与缺席字段都不写槽、枚举默认变体不写槽，解码又按 schema
/// 恢复这些默认 —— 于是「原始动态值不同」不等于「解码后的键不同」。只比原始值会让「空对象 vs
/// 显式 `{0:false}`」这类键同时进映射、写出同一份键体，产物自己解不回（第二对报 `16`）。
///
/// 归一口径：缺席字段按 schema 补默认、`Option` 的缺席与 `Null` 都是 `Null`、枚举展开成解码
/// 侧的 `I64`/`U64`、嵌套 Map 的条目按有效键序排好（编码本来也按键序写、解码按 wire 次序读回）。
/// 值的 `type_key` 不参与比较（与旧 [`structural_order`] 口径一致）：两条只差 `type_key` 的
/// 对象键仍判同值。
enum Effective {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    /// f64 位模式（与 [`structural_order`] 同一口径：按位模式数值比较，不做浮点比较）。
    F64(u64),
    Str(Box<[u8]>),
    Bytes(Box<[u8]>),
    List(Vec<Effective>),
    /// 已按（有效键，有效值）排好序的条目。
    Map(Vec<(Effective, Effective)>),
    /// `field_id` 升序，含 schema 未声明但值里带的字段（编码会报 `12`，判重仍保留其区分度）。
    Object(Vec<(i32, Effective)>),
}

impl Effective {
    /// 种类序：与 [`structural_order`] 的 `rank` 同序；`Map` 是本适配器归一出来的集合形态，排最后。
    fn rank(&self) -> u8 {
        match self {
            Effective::Null => 0,
            Effective::Bool(_) => 1,
            Effective::I64(_) | Effective::U64(_) => 2,
            Effective::F64(_) => 3,
            Effective::Str(_) => 4,
            Effective::Bytes(_) => 5,
            Effective::List(_) => 6,
            Effective::Object(_) => 7,
            Effective::Map(_) => 8,
        }
    }
}

/// [`Effective`] 的全序：整数按数值（跨 `I64`/`U64`）、字符串/字节按辞序、list 逐元素、
/// 映射按键值对序列、对象按 `field_id` 升序 —— 与 [`structural_order`] 同一判据，故非默认值键的
/// wire 次序与旧实现逐字节一致。
fn effective_order(left: &Effective, right: &Effective) -> Ordering {
    let numbers = match (left, right) {
        (Effective::I64(x), Effective::I64(y)) => Some(x.cmp(y)),
        (Effective::U64(x), Effective::U64(y)) => Some(x.cmp(y)),
        (Effective::I64(x), Effective::U64(y)) => Some((*x as i128).cmp(&(*y as i128))),
        (Effective::U64(x), Effective::I64(y)) => Some((*x as i128).cmp(&(*y as i128))),
        _ => None,
    };
    if let Some(order) = numbers {
        return order;
    }
    match (left, right) {
        (Effective::Null, Effective::Null) => Ordering::Equal,
        (Effective::Bool(x), Effective::Bool(y)) => x.cmp(y),
        (Effective::F64(x), Effective::F64(y)) => x.cmp(y),
        (Effective::Str(x), Effective::Str(y)) => x.cmp(y),
        (Effective::Bytes(x), Effective::Bytes(y)) => x.cmp(y),
        (Effective::List(x), Effective::List(y)) => x
            .iter()
            .zip(y.iter())
            .map(|(x, y)| effective_order(x, y))
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| x.len().cmp(&y.len())),
        (Effective::Map(x), Effective::Map(y)) => x
            .iter()
            .zip(y.iter())
            .map(|((x_key, x_value), (y_key, y_value))| {
                effective_order(x_key, y_key).then_with(|| effective_order(x_value, y_value))
            })
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| x.len().cmp(&y.len())),
        (Effective::Object(x), Effective::Object(y)) => x
            .iter()
            .zip(y.iter())
            .map(|((x_field, x_value), (y_field, y_value))| {
                x_field
                    .cmp(y_field)
                    .then_with(|| effective_order(x_value, y_value))
            })
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| x.len().cmp(&y.len())),
        _ => left.rank().cmp(&right.rank()),
    }
}

impl PartialEq for Effective {
    fn eq(&self, other: &Self) -> bool {
        effective_order(self, other) == Ordering::Equal
    }
}

impl Eq for Effective {}

impl PartialOrd for Effective {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Effective {
    fn cmp(&self, other: &Self) -> Ordering {
        effective_order(self, other)
    }
}

/// 值体的原始形状（不按 schema 补默认）：用于没有省略规则可归一的种类（`str`/`bytes`/宽值）。
fn effective_from_body(body: &Body) -> Effective {
    match body {
        Body::Null => Effective::Null,
        Body::Bool(flag) => Effective::Bool(*flag),
        Body::I64(number) => Effective::I64(*number),
        Body::U64(number) => Effective::U64(*number),
        Body::F64(bits) => Effective::F64(*bits),
        Body::Str(text) => Effective::Str(text.clone()),
        Body::Bytes(bytes) => Effective::Bytes(bytes.clone()),
        Body::List(items) => Effective::List(
            items
                .iter()
                .map(|item| effective_from_body(item.body()))
                .collect(),
        ),
        Body::Object(object) => Effective::Object(
            object
                .fields
                .iter()
                .map(|(&field, value)| (field, effective_from_body(value.body())))
                .collect(),
        ),
    }
}

/// [`Dyn::map_entries`] 的一项：有效键（排序与判重只用它）、键适配器、值适配器。
type MapEntry<'a> = (Effective, Dyn<'a>, Dyn<'a>);

/// 编码适配器：把「已注册 schema 图的节点 + `AceValue`」当作值递归编码。
///
/// 每个 `AceEncode`/`AceElement` 入口都只调主 crate 的公开编码器 API；`Option` 的记录与省略规则
/// 由主 crate 的 `Option` 实现驱动。集合（list/map）的记录体走主 crate 的
/// [`Encoder::encode_collection`] 接缝：`u32 count`、元素作用域、深度（`5`）、集合长度与字节
/// 预算（`6`）都由该接缝判定，与 `Vec`/`BTreeMap` 的内建实现同源，本适配器不手写 wire 字节。
///
/// 共享身份（R32）由 [`Identities`] 按节点地址维护：同一 `AceValue` 节点只写一条记录，其余位置
/// 写引用 id；强环由 [`check_value_graph`] 在编码前拒为 `14`。
#[derive(Clone, Copy)]
struct Dyn<'a> {
    /// 本次调用的 options 副本：节点 arena、白名单（多态分派）与限额都从这里取。
    options: &'a AceOptions,
    /// 当前值对应的 schema 节点下标。
    node: NodeId,
    /// 本次 `ace_serialize` 的节点身份表。
    identities: &'a Identities,
    /// 当前值。
    value: &'a AceValue,
}

/// 发射位置：字段位置的引用 id 占 8B 槽载荷，元素体的引用 id 占 4B。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Field,
    Element,
}

/// 引用类节点的 wire 形状（R32④）：同一节点重复出现时形状必须一致，否则 `13 TypeMismatch`，
/// 不得写到与首次记录不同的位置类型上。
///
/// 对象必须带自己的 schema 节点下标：记录体完全由 schema 决定（字段数、字段类型），
/// 只看「都是对象」就复用引用 id 会跳过字段类型校验。list/map 同理带**元素（键与值）形状**
/// 的 schema 节点：同一节点被不兼容的元素形状复用时，首次记录体写不出第二种布局。
///
/// 多态体（FR-W07）与普通对象体是两种不同的记录体，故单列 [`Shape::PolyObject`]：
/// 同一 wire id 不能在两种体头下复用（→ `13`）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Str,
    Bytes,
    Object(NodeId),
    PolyObject(NodeId),
    List(NodeId),
    Map(NodeId, NodeId),
}

/// 两份 schema 节点是否描述同一 wire 形状（R32④）：字段数、字段 order、字段类型与嵌套的
/// 集合/对象形状全等即兼容。`type_key` 不参与比较：记录体只由形状决定。
///
/// 迭代比对并按节点对去重：schema 图可以互引成环（`Object` ↔ `Schema`），重复出现的节点对
/// 沿用首次结论，因此不会展开成无限递归。
fn same_schema_shape(nodes: &[OwnedNode], left: NodeId, right: NodeId) -> bool {
    let mut pending = vec![(left, right)];
    let mut seen: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
    while let Some((left, right)) = pending.pop() {
        if left == right || !seen.insert((left, right)) {
            continue;
        }
        match (&nodes[left], &nodes[right]) {
            (OwnedNode::Prim(left), OwnedNode::Prim(right)) if left == right => {}
            (OwnedNode::Str, OwnedNode::Str) | (OwnedNode::Bytes, OwnedNode::Bytes) => {}
            (OwnedNode::List(left), OwnedNode::List(right))
            | (OwnedNode::Option(left), OwnedNode::Option(right))
            | (OwnedNode::Object(left), OwnedNode::Object(right))
            | (OwnedNode::Enum(left), OwnedNode::Enum(right)) => pending.push((*left, *right)),
            (OwnedNode::Map(left_key, left_value), OwnedNode::Map(right_key, right_value)) => {
                pending.push((*left_key, *right_key));
                pending.push((*left_value, *right_value));
            }
            (
                OwnedNode::Schema { fields: left, .. },
                OwnedNode::Schema { fields: right, .. },
            ) => {
                if left.len() != right.len() {
                    return false;
                }
                for ((left_id, left_type), (right_id, right_type)) in left.iter().zip(right) {
                    if left_id != right_id {
                        return false;
                    }
                    pending.push((*left_type, *right_type));
                }
            }
            (
                OwnedNode::EnumDef {
                    repr: left_repr,
                    variants: left_variants,
                    default: left_default,
                    ..
                },
                OwnedNode::EnumDef {
                    repr: right_repr,
                    variants: right_variants,
                    default: right_default,
                    ..
                },
            ) => {
                if left_repr != right_repr
                    || left_variants != right_variants
                    || left_default != right_default
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// 两份多态体是否描述同一 wire 形状：体头里的 `type_name` 记录使 `type_key` 成为记录体的
/// 一部分，故除字段形状外还要求两者的 `type_key` 相同（普通对象体没有这一项，见
/// [`same_schema_shape`]）。
fn same_poly_schema_shape(nodes: &[OwnedNode], left: NodeId, right: NodeId) -> bool {
    let same_key = matches!(
        (&nodes[left], &nodes[right]),
        (
            OwnedNode::Schema { type_key: left, .. },
            OwnedNode::Schema { type_key: right, .. }
        ) if left == right
    );
    same_key && same_schema_shape(nodes, left, right)
}

/// 两份 wire 形状是否兼容（R32④）：对象、list 的元素形状与 map 的键/值形状均按 schema 形状
/// 比较（含嵌套），其余按种类比较。
///
/// 编码侧在身份表命中前用它判「同一节点被不兼容 schema 复用」，解码侧在身份表命中时用它判
/// 「同一记录 id 在不同形状下被复用」——两侧对同一情形给出同一 `13 TypeMismatch`。
fn same_shape(nodes: &[OwnedNode], first: Shape, current: Shape) -> bool {
    match (first, current) {
        (Shape::Object(left), Shape::Object(right)) | (Shape::List(left), Shape::List(right)) => {
            same_schema_shape(nodes, left, right)
        }
        (Shape::PolyObject(left), Shape::PolyObject(right)) => {
            same_poly_schema_shape(nodes, left, right)
        }
        (Shape::Map(left_key, left_value), Shape::Map(right_key, right_value)) => {
            same_schema_shape(nodes, left_key, right_key)
                && same_schema_shape(nodes, left_value, right_value)
        }
        (first, current) => first == current,
    }
}

/// 一次 `ace_serialize` 内的节点身份表：`AceValue` **节点地址** → 已写出记录 id 与其 wire 形状。
///
/// 首次到达写记录并登记，之后的位置只写引用 id（同一消息内同一身份只有一条记录）。
/// 身份键是节点指针，不按 `type_key`、也不按字段值：值相等的两个不同节点不得合并。
///
/// list/map 记录体与记录 id 经主 crate 的 `Encoder::encode_collection` 接缝取得（见 [`Dyn`]），
/// 因而与 `Vec`/`BTreeMap` 的内建实现共享深度、集合长度与字节预算判定。
struct Identities {
    written: RefCell<BTreeMap<usize, (u32, Shape)>>,
    /// 已预留的身份条目数。记录 id 要等记录体写完后才由 `Encoder` 给出，故父节点登记晚于子节点
    /// （后序）；预算只能按「首次发射即预约」计数，不能看 `written.len()`——否则一条嵌套链上的
    /// 条目都还没落表，全部能通过同一次检查。
    reserved: Cell<usize>,
}

impl Identities {
    fn new() -> Self {
        Self {
            written: RefCell::new(BTreeMap::new()),
            reserved: Cell::new(0),
        }
    }

    fn get(&self, key: usize) -> Option<(u32, Shape)> {
        self.written.borrow().get(&key).copied()
    }

    /// 首次发射前的身份条目预算（`max_references`）：与核心编码的身份路径同一判据
    /// （`encode/mod.rs`：第 N 条身份要求 `N ≤ max_references`）。命中既有身份不重复计。
    fn reserve_reference(&self, limits: &Options) -> AceResult<()> {
        let reserved = self.reserved.get();
        if reserved >= limits.max_references {
            return Err(Error::ResourceLimit);
        }
        self.reserved.set(reserved + 1);
        Ok(())
    }

    fn remember(&self, key: usize, id: u32, shape: Shape) {
        self.written.borrow_mut().insert(key, (id, shape));
    }
}

/// 写一处引用：字段位置占 8B 槽载荷，元素位置占 4B 元素体（与核心 `Rc`/`Arc` 实现同形）。
fn write_reference_id(e: &mut Encoder<'_>, id: u32, place: Placement) -> AceResult<()> {
    match place {
        Placement::Field => e.encode_scalar(u64::from(id).to_le_bytes()),
        Placement::Element => e.write_element_bytes(&id.to_le_bytes()),
    }
}

/// 编码前强环检测（R32⑥）：按 `Body` 的子边迭代遍历值图，出现回边即返回 `14 CycleUnsupported`。
///
/// 先做这一趟的理由：编码按 schema 递归展开值，强环（`set`/`add` 使对象引用自身或祖先）若等
/// 递归撞上再报，映射键的结构排序会先无限递归；且拒环必须发生在任何写入之前，不得把部分写入
/// 当成功。适配器不在编码期做环检测，故强环不会走到递归展开。
fn check_value_graph(root: &AceValue) -> AceResult<()> {
    // 缺失 = 未访问；1 = 展开中（在当前 DFS 路径上）；2 = 已完成。共享 DAG 重复到达直接跳过。
    let mut marks: BTreeMap<usize, u8> = BTreeMap::new();
    let mut pending: Vec<(*const AceValue, bool)> = vec![(root, false)];
    while let Some((node, leaving)) = pending.pop() {
        let key = node as usize;
        if leaving {
            marks.insert(key, 2);
            continue;
        }
        match marks.get(&key) {
            Some(1) => return Err(Error::CycleUnsupported),
            Some(_) => continue,
            None => {}
        }
        marks.insert(key, 1);
        pending.push((node, true));
        // SAFETY: 值图在本次调用期间由调用方保活（头契约），子节点随父节点的引用存活。
        match unsafe { &*node }.body() {
            Body::List(items) => {
                pending.extend(items.iter().map(|item| (Arc::as_ptr(item), false)));
            }
            Body::Object(object) => {
                pending.extend(object.fields.values().map(|value| (Arc::as_ptr(value), false)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// 枚举节点编码所需的一次性结果。
struct EnumWire {
    /// 8B 槽载荷：有符号 repr 符号扩展、无符号 repr 零扩展（同 derive）。
    payload: [u8; 8],
    /// 元素体宽度（字节），即 repr 自然宽度。
    width: usize,
    /// 截回 repr 宽度后的变体判别值位模式，用于与默认变体比较。
    tag: u64,
}

impl<'a> Dyn<'a> {
    fn child(&self, node: NodeId, value: &'a AceValue) -> Self {
        Self {
            options: self.options,
            node,
            identities: self.identities,
            value,
        }
    }

    fn node(&self) -> &'a OwnedNode {
        &self.options.nodes[self.node]
    }

    /// 节点地址即身份键：`Arc` 原始指针化后，同一节点的所有句柄地址相同。
    fn key(&self) -> usize {
        self.value as *const AceValue as usize
    }

    /// 文本字段与字符串键共用：值必须是 Str（构造期已保证 UTF-8）。
    fn text(&self) -> AceResult<&'a str> {
        match self.value.body() {
            Body::Str(bytes) => std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8),
            _ => Err(Error::TypeMismatch),
        }
    }

    fn raw(&self) -> AceResult<&'a [u8]> {
        match self.value.body() {
            Body::Bytes(bytes) => Ok(bytes),
            _ => Err(Error::TypeMismatch),
        }
    }

    /// 枚举判别值：值以 `I64`/`U64` 位模式承载（头 `ace_enum_*` 注释），按 repr 截回并与已注册
    /// 变体集比较；超位宽 → `13`，不在变体集 → `4`（与解码侧同一规范）。
    fn enum_wire(&self, definition: NodeId) -> AceResult<EnumWire> {
        let OwnedNode::EnumDef {
            repr,
            variants,
            default: _,
            type_key: _,
        } = &self.options.nodes[definition]
        else {
            return Err(Error::InvalidArgument);
        };
        let bits = match self.value.body() {
            Body::I64(number) => *number as u64,
            Body::U64(number) => *number,
            _ => return Err(Error::TypeMismatch),
        };
        let tag = repr.truncate(bits).ok_or(Error::TypeMismatch)?;
        if !variants.contains(&tag) {
            return Err(Error::InvalidTag);
        }
        let shift = 64 - repr.width();
        let payload = if repr.signed() {
            (((tag << shift) as i64) >> shift).to_le_bytes()
        } else {
            tag.to_le_bytes()
        };
        Ok(EnumWire {
            payload,
            width: (repr.width() / 8) as usize,
            tag,
        })
    }

    fn is_default_variant(&self, definition: NodeId, tag: u64) -> bool {
        matches!(
            &self.options.nodes[definition],
            OwnedNode::EnumDef {
                default: Some(default),
                ..
            } if *default == tag
        )
    }

    fn list(&self, element: NodeId) -> AceResult<Vec<Dyn<'a>>> {
        let Body::List(items) = self.value.body() else {
            return Err(Error::TypeMismatch);
        };
        Ok(items
            .iter()
            .map(|item| self.child(element, item))
            .collect())
    }

    /// Map 条目（头 ABI）：`List<Object{k: order 0, v: order 1}>` → 验形后的
    /// `(有效键, 键, 值)`，按**有效键值**（[`Effective`]）排好（未判重）。
    ///
    /// 排序用有效值而非原始动态值：编码省略默认标量/默认变体，解码又恢复它们，只比原始值时
    /// 「空对象 vs 显式 `{0:false}`」这类等价键会得到两个不同位置，产物自己解不回。
    fn map_entries(&self) -> AceResult<Vec<MapEntry<'a>>> {
        let OwnedNode::Map(key, value) = self.node() else {
            return Err(Error::InvalidArgument);
        };
        let Body::List(items) = self.value.body() else {
            return Err(Error::TypeMismatch);
        };
        let mut entries = Vec::with_capacity(items.len());
        for item in items {
            let Body::Object(entry) = item.body() else {
                return Err(Error::TypeMismatch);
            };
            if entry.fields.len() != 2 {
                return Err(Error::TypeMismatch);
            }
            let (Some(key_value), Some(value_value)) =
                (entry.fields.get(&0), entry.fields.get(&1))
            else {
                return Err(Error::TypeMismatch);
            };
            let entry_key = self.child(*key, key_value);
            entries.push((
                entry_key.effective()?,
                entry_key,
                self.child(*value, value_value),
            ));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(entries)
    }

    /// Map 的编码视图：按有效键序排好的 `(键, 值)`。等价键（原始值不同、解码后同值）→
    /// `16 DuplicateKey`，在写任何记录之前判定。
    fn map(&self) -> AceResult<Vec<(Dyn<'a>, Dyn<'a>)>> {
        let entries = self.map_entries()?;
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Error::DuplicateKey);
        }
        Ok(entries
            .into_iter()
            .map(|(_, entry_key, entry_value)| (entry_key, entry_value))
            .collect())
    }

    /// 当前值在 key schema 下的有效值（B1）：见 [`Effective`]。
    ///
    /// 只归一写入规则会省略的部分：标量默认值、`Option` 的 `None`、枚举默认变体、对象缺席字段
    /// （对象先按[`Self::object_target`]解析实际编码目标，见 [`Self::effective_object`]）、嵌套
    /// Map / List 的次序与内部默认值；其余种类按值体原样比较。
    fn effective(&self) -> AceResult<Effective> {
        match self.node() {
            OwnedNode::Prim(kind) if is_scalar_kind(*kind) => self.effective_scalar(*kind),
            OwnedNode::Enum(definition) => self.effective_enum(*definition),
            OwnedNode::Option(inner) => Ok(match self.option(*inner)? {
                None => Effective::Null,
                Some(inner) => inner.effective()?,
            }),
            OwnedNode::List(element) => match self.value.body() {
                Body::List(items) => Ok(Effective::List(
                    items
                        .iter()
                        .map(|item| self.child(*element, item).effective())
                        .collect::<AceResult<Vec<_>>>()?,
                )),
                _ => Ok(effective_from_body(self.value.body())),
            },
            OwnedNode::Map(..) => self.effective_map(),
            OwnedNode::Object(schema) => self.effective_object(*schema),
            OwnedNode::Schema { .. } => self.effective_object(self.node),
            OwnedNode::Prim(_) | OwnedNode::Str | OwnedNode::Bytes | OwnedNode::EnumDef { .. } => {
                Ok(effective_from_body(self.value.body()))
            }
        }
    }

    /// 标量字段的有效值：位模式全零（[`is_default_scalar`]）归一成该 kind 的解码默认体，故
    /// 「缺席」「显式默认」「解码恢复的默认」三者同值。
    fn effective_scalar(&self, kind: i32) -> AceResult<Effective> {
        Ok(match is_default_scalar(&scalar_wire(kind, self.value)?) {
            true => effective_from_body(&default_scalar(kind)),
            false => effective_from_body(self.value.body()),
        })
    }

    /// 枚举的有效值：取默认变体与缺席同值，故统一按解码侧 `I64`/`U64` 口径展开（见
    /// [`DynReader::enum_body`]）。
    fn effective_enum(&self, definition: NodeId) -> AceResult<Effective> {
        let wire = self.enum_wire(definition)?;
        let OwnedNode::EnumDef { repr, .. } = &self.options.nodes[definition] else {
            return Err(Error::InvalidArgument);
        };
        Ok(if repr.signed() {
            Effective::I64(i64::from_le_bytes(wire.payload))
        } else {
            Effective::U64(u64::from_le_bytes(wire.payload))
        })
    }

    /// 嵌套 Map 的有效值：复用 [`Self::map_entries`] 的验形与键序，故插入次序不同的同一张映射
    /// 归一成同一个值（编码也按键序写，解码按 wire 次序读回）。
    fn effective_map(&self) -> AceResult<Effective> {
        Ok(Effective::Map(
            self.map_entries()?
                .into_iter()
                .map(|(entry_key, _, entry_value)| Ok((entry_key, entry_value.effective()?)))
                .collect::<AceResult<Vec<_>>>()?,
        ))
    }

    /// 对象键的有效值：先按 [`Self::object_target`] 取**实际编码的目标 schema**，再按该 schema
    /// 的字段表逐个取值（缺席补默认），最后带上该 schema 未声明的字段。
    ///
    /// 目标解析必须与实际编码同源（R33②）：值自带 `type_key` 时编码按该 key 的注册目标写字段，
    /// 若这里改用位置声明的 schema 补默认，同一份值会得到两份「有效值」，等价键就漏判。
    /// `type_key` 本身不进入 [`Effective`]（不参与结构值比较）。
    fn effective_object(&self, declared: NodeId) -> AceResult<Effective> {
        let (schema, _) = self.object_target(declared)?;
        let OwnedNode::Schema { fields, .. } = &self.options.nodes[schema] else {
            return Err(Error::InvalidArgument);
        };
        let Body::Object(object) = self.value.body() else {
            return Ok(effective_from_body(self.value.body()));
        };
        let mut effective: BTreeMap<i32, Effective> = BTreeMap::new();
        for (&order, &node) in fields {
            let present = object.fields.get(&order).map(|value| &**value);
            effective.insert(order, self.effective_field(node, present)?);
        }
        for (&order, value) in &object.fields {
            if !fields.contains_key(&order) {
                effective.insert(order, effective_from_body(value.body()));
            }
        }
        Ok(Effective::Object(effective.into_iter().collect()))
    }

    /// 字段的有效值（与 [`Self::encode_field`] 的 presence 规则对称）：标量缺席取默认、`Option`
    /// 缺席取 `Null`、注册了默认变体的枚举缺席取该变体；其余缺席给 `Null`——编码会在写记录时报
    /// `15`，判重只需要一个确定值。
    fn effective_field(&self, node: NodeId, present: Option<&'a AceValue>) -> AceResult<Effective> {
        match (&self.options.nodes[node], present) {
            (OwnedNode::Prim(kind), None) if is_scalar_kind(*kind) => {
                Ok(effective_from_body(&default_scalar(*kind)))
            }
            (OwnedNode::Option(_), None) => Ok(Effective::Null),
            (OwnedNode::Enum(definition), None) => Ok(effective_from_body(
                &default_variant_body(&self.options.nodes, *definition)?,
            )),
            (_, None) => Ok(Effective::Null),
            (_, Some(value)) => self.child(node, value).effective(),
        }
    }

    /// `Option` 的 presence 规则：`Null` 与缺席都是 `None`（缺席由 [`Self::encode_field`] 处理）。
    fn option(&self, inner: NodeId) -> AceResult<Option<Dyn<'a>>> {
        match self.value.body() {
            Body::Null => Ok(None),
            _ => Ok(Some(self.child(inner, self.value))),
        }
    }

    /// 对象的值与字段表：值必须是 Object，且不得带 schema 未声明的 `field_id`。
    ///
    /// 多余字段头未约定；按值模型「未知 `field_id` → `12 NotFound`」（`ace_object_get` 同码）拒绝，
    /// 不静默丢弃调用方数据。
    fn object_parts(
        &self,
        schema: NodeId,
    ) -> AceResult<(&'a BTreeMap<i32, NodeId>, &'a BTreeMap<i32, Arc<AceValue>>)> {
        let OwnedNode::Schema { fields, .. } = &self.options.nodes[schema] else {
            return Err(Error::InvalidArgument);
        };
        let Body::Object(object) = self.value.body() else {
            return Err(Error::TypeMismatch);
        };
        if object
            .fields
            .keys()
            .any(|field_id| !fields.contains_key(field_id))
        {
            return Err(Error::NotFound);
        }
        Ok((fields, &object.fields))
    }

    /// 引用类节点的身份发射：首次到达写记录并登记 id，之后只写引用 id。
    ///
    /// `write` 负责写出记录**并写出本处位置的引用**（字段位置由 `leave_object` 顺带写槽，
    /// 其余显式写 id），返回该记录 id。形状不一致（同一节点被不兼容 schema 复用）→ `13`。
    ///
    /// 命中身份表时必须重新校形状：对象节点第二次到达时不会重跑 `object_parts`/字段类型校验，
    /// 所以对象得按 schema 形状（而非「都是对象」）判定兼容。
    fn emit_shared(
        &self,
        e: &mut Encoder<'_>,
        place: Placement,
        shape: Shape,
        write: impl FnOnce(&mut Encoder<'_>) -> AceResult<u32>,
    ) -> AceResult<()> {
        let key = self.key();
        if let Some((id, first)) = self.identities.get(key) {
            if !self.same_shape(first, shape) {
                return Err(Error::TypeMismatch);
            }
            return write_reference_id(e, id, place);
        }
        // 首次发射就是新身份条目：在写记录之前预约（超限 → `6`，且尚未写入任何字节）。
        self.identities
            .reserve_reference(&self.options.limits)?;
        let id = write(e)?;
        self.identities.remember(key, id, shape);
        Ok(())
    }

    /// 身份表命中前的形状相容判定（R32④）：见 [`same_shape`]。
    fn same_shape(&self, first: Shape, current: Shape) -> bool {
        same_shape(&self.options.nodes, first, current)
    }

    /// 写一条原始字节记录并返回其 id：字节与 `Encoder::encode_record` 相同，但 id 可见。
    fn write_record(e: &mut Encoder<'_>, body: &[u8]) -> AceResult<u32> {
        e.begin_record()?;
        e.write_body_bytes(body)?;
        e.end_record()
    }

    /// 对象的身份发射：先按该节点自身的 `type_key` 选定 schema（R33②），字段位置的槽由
    /// `leave_object` 顺带写，元素位置另写记录 id。
    fn object_shared(
        &self,
        e: &mut Encoder<'_>,
        declared: NodeId,
        place: Placement,
    ) -> AceResult<()> {
        let (schema, polymorphic) = self.object_target(declared)?;
        let shape = match polymorphic {
            Some(_) => Shape::PolyObject(schema),
            None => Shape::Object(schema),
        };
        self.emit_shared(e, place, shape, |e| {
            let (fields, values) = self.object_parts(schema)?;
            match polymorphic {
                Some(type_name) => e.enter_polymorphic_object(POLY_ESCAPE_TAG, Some(type_name))?,
                None => e.enter_object()?,
            }
            let encoded = self.encode_fields(fields, values, e);
            let left = e.leave_object();
            encoded?;
            let id = left?;
            if place == Placement::Element {
                e.write_element_bytes(&id.to_le_bytes())?;
            }
            Ok(id)
        })
    }

    /// 对象发射的目标 schema 与多态判定（R33②）：值**自带 `type_key`** 时一律按该 key 查白名单
    /// 分派（未注册 → `3 UnknownTypeKey`）——与所在位置的静态 schema 同 key 也不跳过白名单，
    /// 目标 schema 取注册表副本。与静态 schema 同 key 时仍写普通对象体（与 Rust 派生侧同一
    /// schema 的字节互读，不得多一层体头），不同 key 时以 FR-W07 逃生口写出（`type_name` 就是
    /// 该 `type_key`）。只有**未带** `type_key` 的值才沿用位置 schema（内嵌 schema 副本）——但该
    /// 内嵌副本的 `type_key` 又被白名单注册成**另一套布局**时返回 `13`（R33⑥，见下方分支）。
    fn object_target(&self, declared: NodeId) -> AceResult<(NodeId, Option<&'a str>)> {
        let OwnedNode::Schema {
            type_key: declared_key,
            ..
        } = &self.options.nodes[declared]
        else {
            return Err(Error::InvalidArgument);
        };
        let own = match self.value.body() {
            Body::Object(object) => object.type_key.as_deref(),
            // 非对象值不参与分派：`object_parts` 会按既有口径报 `13 TypeMismatch`。
            _ => None,
        };
        let Some(key) = own else {
            // R33⑥（v0.33）：普通对象体（8B）在 wire 上不携带「值是否自带 `type_key`」，故值不带
            // key 时「位置内嵌布局」与「同 key 的注册目标布局」若不同，写出的体无法自解释（解码
            // 侧按注册目标解析，见 `normal_object_target`）——fail-closed 返回既有 `13`，不产生
            // 歧义 wire。同 key 同布局仍用位置布局（字节与完全未注册时相同），未注册不受影响。
            if let Some(&root) = self.options.schemas.get(&**declared_key) {
                if !same_schema_shape(&self.options.nodes, declared, root) {
                    return Err(Error::TypeMismatch);
                }
            }
            return Ok((declared, None));
        };
        let root = *self.options.schemas.get(key).ok_or(Error::UnknownTypeKey)?;
        Ok(match key == declared_key.as_ref() {
            true => (root, None),
            false => (root, Some(key)),
        })
    }

    /// 文本的身份发射：记录体就是 UTF-8 字节，与核心 `String` 的体同形。
    fn text_shared(&self, e: &mut Encoder<'_>, place: Placement) -> AceResult<()> {
        let text = self.text()?;
        self.emit_shared(e, place, Shape::Str, |e| {
            let id = Self::write_record(e, value::encode_str(text))?;
            write_reference_id(e, id, place)?;
            Ok(id)
        })
    }

    /// 字节的身份发射：记录体就是原始字节。
    fn raw_shared(&self, e: &mut Encoder<'_>, place: Placement) -> AceResult<()> {
        let bytes = self.raw()?;
        self.emit_shared(e, place, Shape::Bytes, |e| {
            let id = Self::write_record(e, bytes)?;
            write_reference_id(e, id, place)?;
            Ok(id)
        })
    }

    /// 列表的身份发射：记录体是 `u32 count` + 元素体（与 `Vec<T>` 的逐项分支同形），记录 id 由
    /// `Encoder::encode_collection` 接缝给出，故深度、集合长度与字节预算与内建实现同源。
    fn list_shared(&self, e: &mut Encoder<'_>, element: NodeId, place: Placement) -> AceResult<()> {
        self.emit_shared(e, place, Shape::List(element), |e| {
            let items = self.list(element)?;
            let id = e.encode_collection(items.len(), |e| {
                items.iter().try_for_each(|item| item.ace_encode_element(e))
            })?;
            write_reference_id(e, id, place)?;
            Ok(id)
        })
    }

    /// Map 的身份发射：记录体是 `u32 count` + 按有效键序的键值对（与 `BTreeMap` 的布局同形），
    /// 记录 id 同样来自 `Encoder::encode_collection`。
    ///
    /// 映射键预算在开记录前逐键判定（与内建 `check_map_body` 同一判据）；重复键（含「原始值不同、
    /// 有效值相同」的等价键）已在 [`Self::map`] 排序时报 `16`。
    fn map_shared(
        &self,
        e: &mut Encoder<'_>,
        key: NodeId,
        value: NodeId,
        place: Placement,
    ) -> AceResult<()> {
        self.emit_shared(e, place, Shape::Map(key, value), |e| {
            let entries = self.map()?;
            for (entry_key, _) in &entries {
                entry_key.check_map_key(&self.options.limits)?;
            }
            let id = e.encode_collection(entries.len(), |e| {
                for (entry_key, entry_value) in &entries {
                    entry_key.ace_encode_element(e)?;
                    entry_value.ace_encode_element(e)?;
                }
                Ok(())
            })?;
            write_reference_id(e, id, place)?;
            Ok(id)
        })
    }

    /// 按 `field_id`（即 wire order）升序遍历 schema 字段表，逐个发射。
    fn encode_fields(
        &self,
        fields: &'a BTreeMap<i32, NodeId>,
        values: &'a BTreeMap<i32, Arc<AceValue>>,
        e: &mut Encoder<'_>,
    ) -> AceResult<()> {
        for (&order, &node) in fields {
            self.encode_field(order, node, values.get(&order).map(|value| &**value), e)?;
        }
        Ok(())
    }

    /// 字段的一次发射，presence 与省略规则沿用 derive 的字段分类：
    ///
    /// - 12 种原生标量字段（`Kind::Scalar`）：缺席按默认值，默认值不写槽；
    /// - `Option`：缺席或 `Null` 都是 `None`，不写槽；`Some` 恒写（含 `Some(默认值)`）；
    /// - 整数枚举：缺席且注册了默认变体时可省略，取默认变体时也不写槽；
    /// - 其余（`STR`/`BYTES`/宽值/`LIST`/`MAP`/`OBJ`）：必写，缺席 → `15 MissingRequiredField`。
    fn encode_field(
        &self,
        order: i32,
        node: NodeId,
        present: Option<&'a AceValue>,
        e: &mut Encoder<'_>,
    ) -> AceResult<()> {
        match &self.options.nodes[node] {
            OwnedNode::Option(inner) => match present {
                None => Ok(()),
                Some(value) if matches!(value.body(), Body::Null) => Ok(()),
                Some(value) => self.emit_field(order, *inner, value, e),
            },
            OwnedNode::Prim(kind) if is_scalar_kind(*kind) => {
                let Some(value) = present else {
                    return Ok(());
                };
                if is_default_scalar(&scalar_wire(*kind, value)?) {
                    return Ok(());
                }
                self.emit_field(order, node, value, e)
            }
            OwnedNode::Enum(definition) => match present {
                None if self.registered_default(*definition) => Ok(()),
                None => Err(Error::MissingRequiredField),
                Some(value) => {
                    let wire = self.child(node, value).enum_wire(*definition)?;
                    if self.is_default_variant(*definition, wire.tag) {
                        return Ok(());
                    }
                    self.emit_field(order, node, value, e)
                }
            },
            _ => match present {
                None => Err(Error::MissingRequiredField),
                Some(value) => self.emit_field(order, node, value, e),
            },
        }
    }

    fn registered_default(&self, definition: NodeId) -> bool {
        matches!(
            &self.options.nodes[definition],
            OwnedNode::EnumDef {
                default: Some(_),
                ..
            }
        )
    }

    /// 选中字段并编码其值（derive 的 `begin_field` + `ace_encode` + `check_field_consumed` 三段）。
    fn emit_field(
        &self,
        order: i32,
        node: NodeId,
        value: &'a AceValue,
        e: &mut Encoder<'_>,
    ) -> AceResult<()> {
        e.begin_field(order as u32)?;
        self.child(node, value).ace_encode(e)?;
        e.check_field_consumed()
    }

    fn emit_wire(&self, wire: &Wire<'_>, e: &mut Encoder<'_>, element: bool) -> AceResult<()> {
        match wire {
            Wire::Slot { payload, width } => {
                if element {
                    e.write_element_bytes(&payload[..*width])
                } else {
                    e.encode_scalar(*payload)
                }
            }
            Wire::Record(bytes) => {
                if element {
                    e.write_element_bytes(bytes)
                } else {
                    e.encode_record(bytes)
                }
            }
        }
    }
}

impl<'a> AceEncode for Dyn<'a> {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
        match self.node() {
            OwnedNode::Prim(kind) => self.emit_wire(&scalar_wire(*kind, self.value)?, e, false),
            OwnedNode::Str => self.text_shared(e, Placement::Field),
            OwnedNode::Bytes => self.raw_shared(e, Placement::Field),
            OwnedNode::Enum(definition) => e.encode_scalar(self.enum_wire(*definition)?.payload),
            OwnedNode::List(element) => self.list_shared(e, *element, Placement::Field),
            OwnedNode::Map(key, value) => self.map_shared(e, *key, *value, Placement::Field),
            OwnedNode::Option(inner) => self.option(*inner)?.ace_encode(e),
            OwnedNode::Object(schema) => self.object_shared(e, *schema, Placement::Field),
            OwnedNode::Schema { .. } => self.object_shared(e, self.node, Placement::Field),
            OwnedNode::EnumDef { .. } => Err(Error::InvalidArgument),
        }
    }

    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> AceResult<()> {
        match self.node() {
            OwnedNode::Prim(kind) => self.emit_wire(&scalar_wire(*kind, self.value)?, e, true),
            OwnedNode::Str => self.text_shared(e, Placement::Element),
            OwnedNode::Bytes => self.raw_shared(e, Placement::Element),
            OwnedNode::Enum(definition) => {
                let wire = self.enum_wire(*definition)?;
                e.write_element_bytes(&wire.payload[..wire.width])
            }
            OwnedNode::List(element) => self.list_shared(e, *element, Placement::Element),
            OwnedNode::Map(key, value) => self.map_shared(e, *key, *value, Placement::Element),
            OwnedNode::Option(inner) => self.option(*inner)?.encode_element(e),
            OwnedNode::Object(schema) => self.object_shared(e, *schema, Placement::Element),
            OwnedNode::Schema { .. } => self.object_shared(e, self.node, Placement::Element),
            OwnedNode::EnumDef { .. } => Err(Error::InvalidArgument),
        }
    }
}

impl<'a> AceElement for Dyn<'a> {
    /// 元素体长度随种类变化（定宽/引用/变长），故一律逐项编码，不参与整段 bulk。
    fn element_kind() -> ElementKind {
        ElementKind::Variable
    }

    fn encode_element(&self, e: &mut Encoder<'_>) -> AceResult<()> {
        <Self as AceEncode>::ace_encode_element(self, e)
    }

    /// 适配器只用于编码；解码入口不存在，按 `11 Unsupported` 拒绝而不是留一个假实现。
    fn decode_element(_: &mut Decoder<'_>) -> AceResult<Self> {
        Err(Error::Unsupported)
    }

    /// 字符串键的预算与核心 `String` 实现同源（`max_map_string_key_bytes`），其余键不限。
    fn check_map_key(&self, opts: &Options) -> AceResult<()> {
        match self.node() {
            OwnedNode::Str if self.text()?.len() > opts.max_map_string_key_bytes => {
                Err(Error::ResourceLimit)
            }
            _ => Ok(()),
        }
    }
}

impl NonOptional for Dyn<'_> {}

/// 按已注册的 schema 图编码整棵树；`out` 的字节由 `Encoder::finish` 组装（含 envelope 与 packed）。
///
/// 先做值图强环检测（`14`）：拒环发生在任何写入之前，`*out` 保持空缓冲。
fn encode_registered(options: &AceOptions, root: NodeId, value: &AceValue) -> AceResult<Vec<u8>> {
    check_value_graph(value)?;
    let identities = Identities::new();
    let adapter = Dyn {
        options,
        node: root,
        identities: &identities,
        value,
    };
    let mut encoder = Encoder::new(&options.limits);
    adapter.ace_encode(&mut encoder)?;
    encoder.finish()
}

/// `type_key` 与值自带 key 的解析（头条款）：两者都给时必须一致，`NULL` 仅当值自带可解析 key。
fn resolve_type_key(requested: Option<Box<str>>, value: &AceValue) -> AceResult<Box<str>> {
    let own = match value.body() {
        Body::Object(object) => object.type_key.as_deref(),
        _ => None,
    };
    match (requested, own) {
        (Some(requested), Some(own)) if *requested != *own => Err(Error::InvalidArgument),
        (Some(requested), _) => Ok(requested),
        (None, Some(own)) => Ok(Box::from(own)),
        (None, None) => Err(Error::InvalidArgument),
    }
}

/// 序列化（`ace_serialize`）：`type_key` 查已注册 schema，按 schema 把 `AceValue` 值图编码成 wire。
/// 同一 `AceValue` 节点被多处引用时只写一条 record、其余位置写引用 id（R32 共享身份）；
/// 同一节点被不兼容 schema 复用时写不出第二条记录，改返 `13`（R32④）。
///
/// 嵌套对象按**该节点自身的 `type_key`** 分派（R33②）：值自带的 key 与所在位置的静态字段
/// schema 不同时，用该 key 查白名单并写 FR-W07 逃生口（`type_name` 就是该 key）；同类型或缺
/// `type_key` 时仍写普通对象体，与 Rust 派生侧同一 schema 的字节互读。
///
/// 失败（含 panic）后 `*out` 一律是空缓冲（`ptr=NULL,len=0,cap=0`），成功则移交一段
/// `Box<[u8]>` 分配，`len` 为实际字节数、`cap == len`，由 [`ace_buffer_free`] 归还。
///
/// 失败码：NULL `out`/`value`/key 冲突 → `1`；未注册 `type_key`（含 NULL options 的空白名单，
/// 以及嵌套节点自称但未注册的 key）→ `3`；强环（调用方使用禁令，见头 v4 所有权段落）→ `14`，
/// 先于任何写入判定；kind 不匹配/
/// 整数越界 → `13`；值不带 `type_key` 而位置 schema 的 `type_key` 已注册成**另一套布局**
/// （R33⑥ fail-closed）→ 同样是 `13`；缺必写字段 → `15`；值里带 schema 未声明的 `field_id` → `12`；Map ABI
/// 列表里重复 key → `16`；枚举判别值不在变体集 → `4`。核心编码器的错误（`5` 深度、`6` 资源、
/// `10` panic 等）原样透传。
#[no_mangle]
pub extern "C" fn ace_serialize(
    type_key: *const c_char,
    value: *const AceValue,
    options: *const AceOptions,
    out: *mut AceBuffer,
) -> i32 {
    // SAFETY: 头 codec 契约要求 `out` 非 NULL 且指向可写的 `AceBuffer` 槽；这里只在本函数作用域内
    // 建立一次 `&mut AceBuffer`，不与其它借用重叠，槽的写入也只经该借用。
    let Some(out) = (unsafe { out.as_mut() }) else {
        return Error::InvalidArgument.code();
    };
    // 先清空：失败路径（含 panic）不得留下半成品。
    *out = AceBuffer::empty();
    guard(|| {
        #[cfg(test)]
        if PANIC_IN_SERIALIZE.with(|flag| flag.replace(false)) {
            panic!("ace_serialize test hook");
        }
        // SAFETY: 头契约：非 NULL 的 `value` 是本库 `ace_value_*` 产出且本次调用期间保活的句柄，
        // 故 `&AceValue` 在调用期内有效；同一图在本调用内被只读遍历（头禁止同一句柄并发使用，
        // 调用方不得同时 set/add），故读视图期间没有写路径进入 body。
        let value = unsafe { value.as_ref() }.ok_or(Error::InvalidArgument)?;
        let key = resolve_type_key(read_type_key(type_key)?, value)?;
        // NULL options = 默认限额 + 空白名单（头条款），故查不到 key → 3。
        let default_options;
        // SAFETY: 头契约：非 NULL 的 `options` 是本库 `ace_options_*` 产出且调用期间保活的句柄；
        // 只读借用 `&AceOptions`（限额副本、白名单、节点 arena），不跨调用保存，故调用方随后
        // free 不影响本函数。
        let options = match unsafe { options.as_ref() } {
            Some(options) => options,
            None => {
                default_options = AceOptions {
                    limits: Options::default(),
                    schemas: BTreeMap::new(),
                    nodes: Vec::new(),
                };
                &default_options
            }
        };
        let root = *options.schemas.get(&key).ok_or(Error::UnknownTypeKey)?;
        let bytes = encode_registered(options, root, value)?;
        let mut boxed = bytes.into_boxed_slice();
        out.ptr = boxed.as_mut_ptr();
        out.len = boxed.len();
        out.cap = boxed.len();
        // 所有权交给调用方：`ptr`/`len` 已记录，`Box<[u8]>` 由 `ace_buffer_free` 重建后归还。
        std::mem::forget(boxed);
        Ok(())
    })
}

/// 释放 `ace_serialize` 交付的缓冲并清零结构（`ace_buffer_free`）。
///
/// 同一已清零结构二次 free 安全（返回 `0`）；NULL 视为已释放（`0`，同 `ace_value_free`/
/// `ace_options_free`）。头只承诺 `AceBuffer` 具备这一幂等性，其余句柄仍按「只释放一次」。
#[no_mangle]
pub extern "C" fn ace_buffer_free(buffer: *mut AceBuffer) -> i32 {
    guard(|| {
        let Some(buffer) = (unsafe { buffer.as_mut() }) else {
            return Ok(());
        };
        let AceBuffer { ptr, len, cap: _ } = *buffer;
        if !ptr.is_null() {
            // SAFETY: `ptr`/`len` 来自本库 `ace_serialize` 的 `Box<[u8]>`（`cap == len`），
            // 这里重建同一分配；清零后同一结构不会再进入本分支。
            drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(ptr, len)) });
        }
        *buffer = AceBuffer::empty();
        Ok(())
    })
}

// ——— 解码（P3-6：R32 解码侧） ———

/// 一次 `ace_deserialize` 内的身份表条目：wire 记录 id → 唯一 `AceValue` 节点。
///
/// 身份范围仅限本次调用：表随调用栈上的 [`DynReader`] 一起丢弃，本 crate 不存在跨调用的
/// 节点缓存（R32③；AC-A05⑤ 的「跨结果不共享」靠这一点成立）。
enum Identity {
    /// 已在解码中、节点尚未构造：再次到达同一 id 只能是 wire 强环。
    Open,
    /// 已构造成唯一节点：再次到达只加一个引用，不复制子树。
    Done(Arc<AceValue>, Shape),
}

/// 解码适配器：把「已注册 schema 图 + wire 字节」物化成 `AceValue` 动态值图。
///
/// 所有 wire 读取都走主 crate 的 [`Decoder`] API（`decode_*`/`begin_field`/
/// `enter_object`/`begin_record_view`/`read_element_id`/`read_element_bytes`），集合的深度与
/// 长度判定走解码接缝 [`Decoder::enter_collection_checked`]／[`Decoder::check_collection_len`]/
/// [`Decoder::leave_collection`]（R35），对象体头按只读接缝 [`Decoder::record_body_len`] 路由
/// （R36），本适配器不手写 wire 解析；分派按 [`OwnedNode`] 的种类进行，与编码适配器 [`Dyn`] 对称。
///
/// 身份（R32②③）：对象与引用类容器（`str`/`bytes`/`list`/`map`）按 wire 记录 id 登记，同一 id
/// 第二次到达时返回同一节点（引用 +1）；标量与 >8B 宽值 record 不进身份表（FR-T12②）。强环不靠
/// 事后检查：节点在子树解码完成后才构造，命中「解码中」标记即 `14`，故不会先构造环再报错。
struct DynReader<'a> {
    /// 本次调用的 options 副本：节点 arena、白名单（多态分派）与限额都从这里取。
    /// 集合长度与结构深度走核心的公开/接缝入口（`enter_collection_checked` /
    /// `check_collection_len` / `enter_object`），不在此复制。
    options: &'a AceOptions,
    /// 本次解码的身份表：记录 id → 节点（或「解码中」标记）。
    identities: BTreeMap<u32, Identity>,
}

/// 元素体的定长读取：边界已由 `Decoder::read_element_bytes` 校验，长度必为 `N`。
fn element_bytes<const N: usize>(d: &mut Decoder<'_>) -> AceResult<[u8; N]> {
    d.read_element_bytes(N)?
        .try_into()
        .map_err(|_| Error::TruncatedInput)
}

/// 拒绝记录视图内未读的尾部字节（与核心的 `check_tail` 同一判据：`4 InvalidTag`）。
///
/// 只试读 1 字节：读得到即说明有尾字节，读不到（`2`）说明恰好读完。核心的 `peek_body` 是 crate
/// 私有，故用等价的试读代替，不为此新建接缝。
fn expect_empty_body(d: &mut Decoder<'_>) -> AceResult<()> {
    match d.read_body_bytes(1) {
        Ok(_) => Err(Error::InvalidTag),
        Err(Error::TruncatedInput) => Ok(()),
        Err(error) => Err(error),
    }
}

/// F32 → F64 的按位加宽（头 `ace_value_f64_bits` 注释写死的规则）：sign 复制、quiet/payload
/// 左移 29 位、低 29 位补 0。NaN/±inf 走显式位移（`f64::from` 不承诺保留 NaN payload），其余
/// （含 subnormal 与 ±0）用宿主转换，IEEE-754 下恒精确。
fn widen_f32_bits(bits: u32) -> u64 {
    let sign = u64::from(bits >> 31) << 63;
    let exponent = (bits >> 23) & 0xFF;
    let fraction = u64::from(bits & 0x7F_FFFF);
    if exponent == 0xFF {
        return sign | (0x7FFu64 << 52) | (fraction << 29);
    }
    f64::from(f32::from_bits(bits)).to_bits()
}

/// 标量字段缺席时的默认值（derive 的 `Kind::Scalar` 取 `Default::default()`）：整数 0、`false`、
/// `f32`/`f64` 的 `bits() == 0`、`char` 的 `'\0'`。
fn default_scalar(kind: i32) -> Body {
    match kind {
        ACE_FK_BOOL => Body::Bool(false),
        ACE_FK_I8 | ACE_FK_I16 | ACE_FK_I32 | ACE_FK_I64 => Body::I64(0),
        ACE_FK_U8 | ACE_FK_U16 | ACE_FK_U32 | ACE_FK_U64 | ACE_FK_CHAR => Body::U64(0),
        ACE_FK_F32 | ACE_FK_F64 => Body::F64(0),
        // 调用点已限定 12 种原生标量（`is_scalar_kind`），此分支不可达。
        _ => Body::Null,
    }
}

/// 整数枚举缺席时的默认变体体（FR-D07；未注册默认变体 → `15`）：有符号 repr 符号扩展、无符号
/// repr 零扩展，与解码侧 [`DynReader::enum_body`] 的取值口径一致。
///
/// 编码（缺席字段的有效值，见 [`Dyn::effective_field`]）与解码（缺席字段恢复默认值）共用本
/// 函数，两边的默认值因此是同一份。
fn default_variant_body(nodes: &[OwnedNode], definition: NodeId) -> AceResult<Body> {
    let OwnedNode::EnumDef {
        repr,
        variants: _,
        default,
        type_key: _,
    } = &nodes[definition]
    else {
        return Err(Error::InvalidArgument);
    };
    let tag = default.ok_or(Error::MissingRequiredField)?;
    let mask = u64::MAX >> (64 - repr.width());
    Ok(if repr.signed() {
        let shift = 64 - repr.width();
        Body::I64((((tag & mask) << shift) as i64) >> shift)
    } else {
        Body::U64(tag & mask)
    })
}

impl<'a> DynReader<'a> {
    fn new(options: &'a AceOptions) -> Self {
        Self {
            options,
            identities: BTreeMap::new(),
        }
    }

    /// 引用载荷：字段位置取 8B 槽载荷，元素位置取 4B 元素体（与核心 `Rc`/`Arc`/集合元素两条
    /// 实现同形）。超出 32 位的载荷 → `7`（同核心的 `u32::try_from`）。
    fn reference_id(d: &mut Decoder<'_>, place: Placement) -> AceResult<u32> {
        match place {
            Placement::Field => u32::try_from(u64::from_le_bytes(d.decode_required_scalar()?))
                .map_err(|_| Error::InvalidReference),
            Placement::Element => d.read_element_id(),
        }
    }

    /// 身份表查询与登记：未命中登记「解码中」；命中「解码中」即强环 `14`；命中已完成节点则按
    /// 同一节点复用（R32③），但先按 [`same_shape`] 校形状，不兼容复用 → `13`（R32④）。
    fn share(&mut self, id: u32, shape: Shape) -> AceResult<Option<Arc<AceValue>>> {
        match self.identities.get(&id) {
            None => {
                // 首次到达即新身份条目：与核心解码规划期同一判据（`decode/graph.rs`：
                // 已满 `max_references` → `6`），在物化节点之前判；命中既有条目不重复计。
                if self.identities.len() >= self.options.limits.max_references {
                    return Err(Error::ResourceLimit);
                }
                self.identities.insert(id, Identity::Open);
                Ok(None)
            }
            Some(Identity::Open) => Err(Error::CycleUnsupported),
            Some(Identity::Done(node, first)) => {
                if !same_shape(&self.options.nodes, *first, shape) {
                    return Err(Error::TypeMismatch);
                }
                Ok(Some(Arc::clone(node)))
            }
        }
    }

    /// 首次到达的收尾：登记已完成节点（查询方各持一个引用）。
    fn remember(&mut self, id: u32, shape: Shape, node: Arc<AceValue>) -> Arc<AceValue> {
        self.identities
            .insert(id, Identity::Done(Arc::clone(&node), shape));
        node
    }

    /// 集合体的 `u32 count`。长度上限由核心 [`Decoder::check_collection_len`] 按同一上限与同一
    /// 错误码 `6` 判定（R35），本适配器不再自己判。
    fn collection_count(d: &mut Decoder<'_>) -> AceResult<usize> {
        Ok(d.read_element_id()? as usize)
    }

    /// 字段/根位置的值解码：presence 已由 [`Self::field`] 或根槽决定，这里的 `decode_*` 读 selected
    /// 或根槽（与核心各 `AceDecode` impl 的三段式同序）。
    fn value(&mut self, d: &mut Decoder<'_>, node: NodeId) -> AceResult<Arc<AceValue>> {
        let options = self.options;
        match &options.nodes[node] {
            OwnedNode::Prim(kind) => Ok(AceValue::node(self.prim(d, *kind)?)),
            OwnedNode::Enum(definition) => {
                let payload = d.decode_required_scalar()?;
                Ok(AceValue::node(self.enum_body(*definition, payload)?))
            }
            OwnedNode::Str => self.text(d, Placement::Field, false),
            OwnedNode::Bytes => self.raw(d, Placement::Field),
            OwnedNode::List(element) => self.list(d, *element, Placement::Field),
            OwnedNode::Map(key, value) => self.map(d, *key, *value, Placement::Field),
            OwnedNode::Option(inner) => {
                if d.selected_is_none() {
                    Ok(AceValue::node(Body::Null))
                } else {
                    self.value(d, *inner)
                }
            }
            OwnedNode::Object(schema) => self.object(d, *schema, Placement::Field),
            OwnedNode::Schema { .. } => self.object(d, node, Placement::Field),
            OwnedNode::EnumDef { .. } => Err(Error::InvalidArgument),
        }
    }

    /// 元素位置的值解码（与核心各 `AceElement::decode_element` impl 同形）：元素体是自然宽度
    /// （FR-W12），引用类元素体是 4B 记录 id。
    fn element(&mut self, d: &mut Decoder<'_>, node: NodeId) -> AceResult<Arc<AceValue>> {
        let options = self.options;
        match &options.nodes[node] {
            OwnedNode::Prim(kind) => Ok(AceValue::node(self.element_prim(d, *kind)?)),
            OwnedNode::Enum(definition) => {
                let body = self.enum_element(d, *definition)?;
                Ok(AceValue::node(body))
            }
            OwnedNode::Str => self.text(d, Placement::Element, false),
            OwnedNode::Bytes => self.raw(d, Placement::Element),
            OwnedNode::List(element) => self.list(d, *element, Placement::Element),
            OwnedNode::Map(key, value) => self.map(d, *key, *value, Placement::Element),
            OwnedNode::Object(schema) => self.object(d, *schema, Placement::Element),
            OwnedNode::Schema { .. } => self.object(d, node, Placement::Element),
            OwnedNode::Option(inner) => {
                // presence 字节先于元素体：0 = None（只占一位、无体），1 = Some + 内层元素体（FR-W09）。
                let presence = d.read_element_bytes(1)?[0];
                match presence {
                    0 => Ok(AceValue::node(Body::Null)),
                    1 => self.element(d, *inner),
                    _ => Err(Error::InvalidTag),
                }
            }
            OwnedNode::EnumDef { .. } => Err(Error::InvalidArgument),
        }
    }

    /// 字段的一次解码，presence 与省略规则沿用 derive 的字段分类（与 [`Dyn::encode_field`] 对称）：
    ///
    /// - 12 种原生标量：缺席按默认值；
    /// - `Option`：缺席解出 `Null`（与显式 `Null` 等价）；
    /// - 整数枚举：缺席且注册了默认变体 → 该变体（FR-D07）；字段位置必然有根槽，故缺席只可能是
    ///   字段缺席，不必再区分根缺席；
    /// - 其余（STR/BYTES/宽值/LIST/MAP/OBJ）：缺席交给核心报 `15 MissingRequiredField`。
    fn field(&mut self, d: &mut Decoder<'_>, order: i32, node: NodeId) -> AceResult<Arc<AceValue>> {
        let options = self.options;
        d.begin_field(order as u32)?;
        let absent = d.selected_is_none();
        match &options.nodes[node] {
            OwnedNode::Prim(kind) if is_scalar_kind(*kind) => {
                if absent {
                    Ok(AceValue::node(default_scalar(*kind)))
                } else {
                    self.value(d, node)
                }
            }
            OwnedNode::Option(inner) => {
                if absent {
                    Ok(AceValue::node(Body::Null))
                } else {
                    self.value(d, *inner)
                }
            }
            OwnedNode::Enum(definition) => {
                if absent {
                    self.default_variant(*definition)
                } else {
                    self.value(d, node)
                }
            }
            _ => self.value(d, node),
        }
    }

    /// 文本（`str`）：记录体即 UTF-8 字节，非法 UTF-8 → `8`。`map_key` 为真时按
    /// `max_map_string_key_bytes` 判键长预算（NFR-L01，与核心 `String::decode_map_key` 同一上限与
    /// 错误码）。预算按**位置**判，不看是否已物化：同一记录 id 先作普通字段、再作 Map 键复用，
    /// 命中 [`Identity::Done`] 也照样拒。首次物化时记录体的长度没有公开读法（核心的 `peek_body`
    /// 是 crate 私有），故在复制后、解析与建节点前拒。
    fn text(
        &mut self,
        d: &mut Decoder<'_>,
        place: Placement,
        map_key: bool,
    ) -> AceResult<Arc<AceValue>> {
        let id = Self::reference_id(d, place)?;
        if let Some(node) = self.share(id, Shape::Str)? {
            // `Shape::Str` 已保证节点是 str；已物化不影响本次位置的键长预算。
            if map_key {
                let Body::Str(text) = node.body() else {
                    return Err(Error::InvalidArgument);
                };
                if text.len() > self.options.limits.max_map_string_key_bytes {
                    return Err(Error::ResourceLimit);
                }
            }
            return Ok(node);
        }
        d.begin_record_view(id)?;
        let body = d.read_body_to_end();
        d.end_record_view();
        let body = body?;
        if map_key && body.len() > self.options.limits.max_map_string_key_bytes {
            return Err(Error::ResourceLimit);
        }
        value::decode_str(&body)?;
        Ok(self.remember(
            id,
            Shape::Str,
            AceValue::node(Body::Str(body.into_boxed_slice())),
        ))
    }

    /// 原始字节（`bytes`）：记录体即字节，不做编码校验。
    fn raw(&mut self, d: &mut Decoder<'_>, place: Placement) -> AceResult<Arc<AceValue>> {
        let id = Self::reference_id(d, place)?;
        if let Some(node) = self.share(id, Shape::Bytes)? {
            return Ok(node);
        }
        d.begin_record_view(id)?;
        let body = d.read_body_to_end();
        d.end_record_view();
        let body = body?;
        Ok(self.remember(
            id,
            Shape::Bytes,
            AceValue::node(Body::Bytes(body.into_boxed_slice())),
        ))
    }

    /// 列表：记录体是 `u32 count` + 元素体，元素位置的引用是 4B 记录 id。
    fn list(
        &mut self,
        d: &mut Decoder<'_>,
        element: NodeId,
        place: Placement,
    ) -> AceResult<Arc<AceValue>> {
        let id = Self::reference_id(d, place)?;
        let shape = Shape::List(element);
        if let Some(node) = self.share(id, shape)? {
            return Ok(node);
        }
        // 深度先判（R35）：与内建 `Vec` 同序——集合超深优先于记录视图与 `count` 的错误。
        d.enter_collection_checked()?;
        let decoded = match d.begin_record_view(id) {
            Ok(()) => {
                let decoded = self.read_items(d, element);
                d.end_record_view();
                decoded
            }
            Err(error) => Err(error),
        };
        d.leave_collection();
        Ok(self.remember(
            id,
            shape,
            AceValue::node(Body::List(decoded?)),
        ))
    }

    /// 列表记录体：count → 长度上限 → 元素作用域 → 逐项解码 → 尾部无残留（与核心 `Vec<T>` 解码
    /// 同序）。集合层由调用方的 `enter_collection_checked` 进入（R35），故元素对象的字段深度与
    /// 编码侧一致。
    fn read_items(
        &mut self,
        d: &mut Decoder<'_>,
        element: NodeId,
    ) -> AceResult<Vec<Arc<AceValue>>> {
        let count = Self::collection_count(d)?;
        d.check_collection_len(count)?;
        d.begin_element();
        let mut items = Vec::new();
        let decoded = (0..count).try_for_each(|_| -> AceResult<()> {
            let item = self.element(d, element)?;
            items.push(item);
            Ok(())
        });
        d.end_element();
        decoded?;
        expect_empty_body(d)?;
        Ok(items)
    }

    /// Map：ABI 表示是 `List<Object{k: order 0, v: order 1}>`，记录体为 `u32 count` 后接按键序
    /// 排列的键值对（键元素、值元素）。
    fn map(
        &mut self,
        d: &mut Decoder<'_>,
        key: NodeId,
        value: NodeId,
        place: Placement,
    ) -> AceResult<Arc<AceValue>> {
        let id = Self::reference_id(d, place)?;
        let shape = Shape::Map(key, value);
        if let Some(node) = self.share(id, shape)? {
            return Ok(node);
        }
        // 深度先判（R35）：与内建 `BTreeMap` 同序——集合超深优先于记录视图与 `count` 的错误。
        d.enter_collection_checked()?;
        let decoded = match d.begin_record_view(id) {
            Ok(()) => {
                let decoded = self.read_map_entries(d, key, value);
                d.end_record_view();
                decoded
            }
            Err(error) => Err(error),
        };
        d.leave_collection();
        Ok(self.remember(
            id,
            shape,
            AceValue::node(Body::List(decoded?)),
        ))
    }

    /// 映射记录体：条目按 wire 次序装成 `List<Object{0: key, 1: value}>`；重复键 → `16`（R24，与核心
    /// `BTreeMap` 解码同码）。每解完一对即按核心键序查已见键：第二次出现当场拒，不被后续条目的
    /// 其它错误（截断/非法引用）覆盖；重复时不替换首次那对（保持「不覆盖旧值」）。
    ///
    /// 这里的键比较比的就是**解码后的值**（默认值已由本适配器补全），故与编码侧的 [`Effective`]
    /// 同一判据：编码能写出来的 wire，解码不会在这里报重复键。`OrderedKey` 含 `Arc<AceValue>`
    /// （内部可变性），但它作集合键时只有 `Ord`/`PartialEq` 被用到，两者只读 `value`。
    #[allow(clippy::mutable_key_type)]
    fn read_map_entries(
        &mut self,
        d: &mut Decoder<'_>,
        key: NodeId,
        value: NodeId,
    ) -> AceResult<Vec<Arc<AceValue>>> {
        let count = Self::collection_count(d)?;
        d.check_collection_len(count)?;
        d.begin_element();
        let mut entries: Vec<(Arc<AceValue>, Arc<AceValue>)> = Vec::new();
        let mut seen: BTreeSet<OrderedKey> = BTreeSet::new();
        let decoded = (0..count).try_for_each(|_| -> AceResult<()> {
            let entry_key = self.key_element(d, key)?;
            let entry_value = self.element(d, value)?;
            if !seen.insert(OrderedKey(Arc::clone(&entry_key))) {
                return Err(Error::DuplicateKey);
            }
            entries.push((entry_key, entry_value));
            Ok(())
        });
        d.end_element();
        decoded?;
        expect_empty_body(d)?;
        Ok(entries
            .into_iter()
            .map(|(entry_key, entry_value)| {
                AceValue::node(Body::Object(Object {
                    type_key: None,
                    fields: BTreeMap::from([(0, entry_key), (1, entry_value)]),
                }))
            })
            .collect())
    }

    /// 映射键的元素解码：字符串键先判键长预算（NFR-L01），其余键等同 [`Self::element`]。
    fn key_element(&mut self, d: &mut Decoder<'_>, node: NodeId) -> AceResult<Arc<AceValue>> {
        if matches!(&self.options.nodes[node], OwnedNode::Str) {
            return self.text(d, Placement::Element, true);
        }
        self.element(d, node)
    }

    /// 对象：先按位置读引用 id（字段位置是 8B 槽载荷，元素位置是 4B 元素体，与 `str`/`bytes`/
    /// `list`/`map` 同一条规则，见 [`Self::reference_id`]），再按键序解码字段。对象值带自己的
    /// `type_key`（`ace_value_object` 的语义），故解码结果可再交给 `ace_serialize(NULL, …)` 而不必
    /// 由调用方补 key。
    ///
    /// 体头分两种（R33②）：普通对象体（8B 槽范围）或多态对象体（FR-W07 的 10B/14B）——先按
    /// `Decoder::record_body_len` 返回的记录体长度路由，各自交核心入口解析，本适配器不手写体头。
    /// 该查询不计入 `max_decoded_bytes`、不物化、不动游标（R36），故路由本身不多花预算，也不抢在
    /// 核心入口的深度判定之前报 `6`。恰 8B 走普通体，更长（合法体长只有 10/14）走多态体；
    /// 其余体长（如 9）由相应核心入口自行判错，错误码与旧探测法一致。
    ///
    /// 普通体没有体头区分「内嵌副本 vs 同 key 的注册目标」，故解码目标按编码侧的同一条规则解析
    /// （见 [`Self::normal_object_target`]）。
    fn object(
        &mut self,
        d: &mut Decoder<'_>,
        schema: NodeId,
        place: Placement,
    ) -> AceResult<Arc<AceValue>> {
        let id = Self::reference_id(d, place)?;
        if d.record_body_len(id)? > OBJECT_BODY_BYTES {
            return self.polymorphic_object(d, id);
        }
        let schema = self.normal_object_target(schema)?;
        let shape = Shape::Object(schema);
        if let Some(node) = self.share(id, shape)? {
            return Ok(node);
        }
        let options = self.options;
        let OwnedNode::Schema { type_key, fields } = &options.nodes[schema] else {
            return Err(Error::InvalidArgument);
        };
        let type_key = type_key.clone();
        let decoded = match d.enter_object(u64::from(id)) {
            Ok(()) => {
                let decoded = self.read_fields(d, fields);
                let left = d.leave_object();
                match decoded {
                    Ok(values) => left.map(|()| values),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        Ok(self.remember(
            id,
            shape,
            AceValue::node(Body::Object(Object {
                type_key: Some(type_key),
                fields: decoded?,
            })),
        ))
    }

    /// 普通对象体（8B）的解码目标 schema（R33②）：与编码侧 [`Dyn::object_target`] 同一条规则
    /// ——位置 schema 的 `type_key` 已注册白名单时取该**注册目标**的字段布局。
    ///
    /// 编码对自带同 key 的值也用注册目标，且同 key 仍写普通 8B 体（不写多态头），故这种记录体
    /// 里没有可分辨「内嵌副本 vs 注册目标」的信息：解码若坚持用位置 schema 的内嵌副本，同一份
    /// 8B 体在两侧会按两套字段布局解析（种类与值都错）。未注册时保留位置 schema 的静态路径。
    ///
    /// 「值不带 key + 两套布局不同」的输入已在编码侧被 R33⑥ 拒为 `13`，故这里按注册目标解析
    /// 不会遇到两套布局不一致的合法 wire。
    fn normal_object_target(&self, declared: NodeId) -> AceResult<NodeId> {
        let OwnedNode::Schema { type_key, .. } = &self.options.nodes[declared] else {
            return Err(Error::InvalidArgument);
        };
        Ok(self
            .options
            .schemas
            .get(&**type_key)
            .copied()
            .unwrap_or(declared))
    }

    /// 多态体（FR-W07、R33②④）：体头由核心解析（type_name 记录的三类错误仍按 `7`/`8`/`4`），
    /// 再按 `type_name`——即该节点自身的 `type_key`——查白名单取字段 schema（未注册 → `3`）。
    /// C ABI 不注册数值变体，故数值 tag 无变体可匹配 → `4 InvalidTag`。
    fn polymorphic_object(&mut self, d: &mut Decoder<'_>, id: u32) -> AceResult<Arc<AceValue>> {
        // 身份命中先于进入（R32③、R36）：同一 record 的体头固定，第二次到达必然读回同一个
        // `type_name`、解出同一个注册 schema，故已登记为 [`Shape::PolyObject`] 的节点可直接复用，
        // 不再进入对象、不读名称、不按体长二次收费。「解码中」仍是强环 `14`。
        //
        // 但身份表也登记 `str`/`bytes`/`list`/`map`：同一 record id 先从那些形状到达、再从多态体
        // 到达时，记录体与「本次是不是多态体」互不相干，直接复用会跳过 [`Self::share`] 的
        // [`same_shape`] 比较并交出一个种类错误的节点。故快返只接受 `Shape::PolyObject`，其余已
        // 完成形状 → `13 TypeMismatch`（不进入、不读名称、不收费，与 `share` 同一错误码）。
        match self.identities.get(&id) {
            Some(Identity::Done(node, Shape::PolyObject(_))) => return Ok(Arc::clone(node)),
            Some(Identity::Done(_, _)) => return Err(Error::TypeMismatch),
            Some(Identity::Open) => return Err(Error::CycleUnsupported),
            None => {}
        }
        let options = self.options;
        let (tag, type_name) = d.enter_polymorphic_object(u64::from(id))?;
        let decoded = match (tag, type_name.as_deref()) {
            (POLY_ESCAPE_TAG, Some(type_key)) => match options.schemas.get(type_key).copied() {
                Some(schema) => self.read_polymorphic_fields(d, id, schema, type_key),
                None => Err(Error::UnknownTypeKey),
            },
            _ => Err(Error::InvalidTag),
        };
        // 进入成功即必然可离开；解码错误优先于离开结果（与核心派生实现的 `decoded?; left?` 同序）。
        let left = d.leave_object();
        let node = decoded?;
        left?;
        Ok(node)
    }

    /// 多态体的字段解码：字段规则与普通对象体相同，差异只在 schema 来自该 `type_key` 的注册项、
    /// 形状带多态标记（多态体与普通体是两种记录体，不得共用同一条 wire 记录）。
    fn read_polymorphic_fields(
        &mut self,
        d: &mut Decoder<'_>,
        id: u32,
        schema: NodeId,
        type_key: &str,
    ) -> AceResult<Arc<AceValue>> {
        let shape = Shape::PolyObject(schema);
        if let Some(node) = self.share(id, shape)? {
            return Ok(node);
        }
        let options = self.options;
        let OwnedNode::Schema { fields, .. } = &options.nodes[schema] else {
            return Err(Error::InvalidArgument);
        };
        let values = self.read_fields(d, fields)?;
        Ok(self.remember(
            id,
            shape,
            AceValue::node(Body::Object(Object {
                type_key: Some(Box::from(type_key)),
                fields: values,
            })),
        ))
    }

    fn read_fields(
        &mut self,
        d: &mut Decoder<'_>,
        fields: &'a BTreeMap<i32, NodeId>,
    ) -> AceResult<BTreeMap<i32, Arc<AceValue>>> {
        let mut values = BTreeMap::new();
        for (&order, &field) in fields {
            let value = self.field(d, order, field)?;
            values.insert(order, value);
        }
        Ok(values)
    }

    /// 标量/宽值的字段/根位置解码：8B 槽载荷（标量与 `DateTime`/`IntPtr`）或定长记录体（`Guid`/
    /// `DateTimeOffset`），与编码适配器的 [`scalar_wire`] 一一对称、与核心 `AceDecode` impl 同源。
    fn prim(&mut self, d: &mut Decoder<'_>, kind: i32) -> AceResult<Body> {
        match kind {
            ACE_FK_GUID => Ok(Body::Bytes(
                d.decode_record()?.as_fixed::<16>()?.to_vec().into_boxed_slice(),
            )),
            ACE_FK_DATETIME_OFFSET => {
                let fixed = *d.decode_record()?.as_fixed::<10>()?;
                // 非法位模式 → `4`（与 `ace_value_*` 及编码侧 `scalar_wire` 同一规范）。
                value::decode_datetime_offset(&fixed)?;
                Ok(Body::Bytes(fixed.to_vec().into_boxed_slice()))
            }
            ACE_FK_DATETIME => {
                let bits = u64::from_le_bytes(d.decode_required_scalar()?);
                value::decode_datetime(bits)?;
                Ok(Body::U64(bits))
            }
            ACE_FK_INTPTR => Ok(Body::I64(value::decode_intptr(&d.decode_required_scalar()?))),
            ACE_FK_UINTPTR => Ok(Body::U64(value::decode_uintptr(&d.decode_required_scalar()?))),
            _ => Self::scalar(kind, d.decode_required_scalar()?),
        }
    }

    /// 12 种原生标量的槽载荷解码：与核心 `scalar!` 的 `AceDecode` 逐条同源（有符号按 `I64`、无符号
    /// 与 `char` 按 `U64` 承载，`char` 的非法标量 → `8`、非规范 `bool` → `4`）。
    fn scalar(kind: i32, payload: [u8; 8]) -> AceResult<Body> {
        Ok(match kind {
            ACE_FK_BOOL => Body::Bool(value::decode_bool(&payload)?),
            ACE_FK_I8 => Body::I64(i64::from(value::decode_i8(&payload))),
            ACE_FK_I16 => Body::I64(i64::from(value::decode_i16(&payload))),
            ACE_FK_I32 => Body::I64(i64::from(value::decode_i32(&payload))),
            ACE_FK_I64 => Body::I64(value::decode_i64(&payload)),
            ACE_FK_U8 => Body::U64(u64::from(value::decode_u8(&payload))),
            ACE_FK_U16 => Body::U64(u64::from(value::decode_u16(&payload))),
            ACE_FK_U32 => Body::U64(u64::from(value::decode_u32(&payload))),
            ACE_FK_U64 => Body::U64(value::decode_u64(&payload)),
            ACE_FK_F32 => Body::F64(widen_f32_bits(value::decode_f32(&payload).to_bits())),
            ACE_FK_F64 => Body::F64(value::decode_f64(&payload).to_bits()),
            ACE_FK_CHAR => Body::U64(u64::from(u32::from(value::decode_char(&payload)?))),
            _ => return Err(Error::InvalidArgument),
        })
    }

    /// 标量/宽值的元素位置解码：元素体是自然宽度（FR-W12），与核心 `element!` 的 `decode_element`
    /// 逐条同源（`char` 的非法标量在元素位置是 `8`，字段位置是 `4`——与核心两条实现一致）。
    fn element_prim(&mut self, d: &mut Decoder<'_>, kind: i32) -> AceResult<Body> {
        Ok(match kind {
            ACE_FK_BOOL => {
                let byte = d.read_element_bytes(1)?[0];
                Body::Bool(match byte {
                    0 => false,
                    1 => true,
                    _ => return Err(Error::InvalidTag),
                })
            }
            ACE_FK_I8 => Body::I64(i64::from(i8::from_le_bytes(element_bytes(d)?))),
            ACE_FK_I16 => Body::I64(i64::from(i16::from_le_bytes(element_bytes(d)?))),
            ACE_FK_I32 => Body::I64(i64::from(i32::from_le_bytes(element_bytes(d)?))),
            ACE_FK_I64 => Body::I64(i64::from_le_bytes(element_bytes(d)?)),
            ACE_FK_U8 => Body::U64(u64::from(u8::from_le_bytes(element_bytes(d)?))),
            ACE_FK_U16 => Body::U64(u64::from(u16::from_le_bytes(element_bytes(d)?))),
            ACE_FK_U32 => Body::U64(u64::from(u32::from_le_bytes(element_bytes(d)?))),
            ACE_FK_U64 => Body::U64(u64::from_le_bytes(element_bytes(d)?)),
            ACE_FK_F32 => Body::F64(widen_f32_bits(u32::from_le_bytes(element_bytes(d)?))),
            ACE_FK_F64 => Body::F64(u64::from_le_bytes(element_bytes(d)?)),
            ACE_FK_CHAR => {
                let scalar = u32::from_le_bytes(element_bytes(d)?);
                let character = char::from_u32(scalar).ok_or(Error::InvalidUtf8)?;
                Body::U64(u64::from(u32::from(character)))
            }
            ACE_FK_GUID => Body::Bytes(element_bytes::<16>(d)?.to_vec().into_boxed_slice()),
            ACE_FK_DATETIME => {
                let bits = u64::from_le_bytes(element_bytes(d)?);
                value::decode_datetime(bits)?;
                Body::U64(bits)
            }
            ACE_FK_DATETIME_OFFSET => {
                let fixed = element_bytes::<10>(d)?;
                value::decode_datetime_offset(&fixed)?;
                Body::Bytes(fixed.to_vec().into_boxed_slice())
            }
            ACE_FK_INTPTR => Body::I64(value::decode_intptr(&element_bytes(d)?)),
            ACE_FK_UINTPTR => Body::U64(value::decode_uintptr(&element_bytes(d)?)),
            _ => return Err(Error::InvalidArgument),
        })
    }

    /// 整数枚举的判别值解码：按 repr 位宽解码（超出位宽 → `4`，不截断）并与已注册变体集比较
    /// （未知 → `4`）；值以 `I64`（有符号 repr，符号扩展）或 `U64`（无符号 repr）承载，与头
    /// `ace_enum_*` 的位模式口径、核心 `__ace_enum_from_payload` 同源。
    fn enum_body(&self, definition: NodeId, payload: [u8; 8]) -> AceResult<Body> {
        let OwnedNode::EnumDef {
            repr,
            variants,
            default: _,
            type_key: _,
        } = &self.options.nodes[definition]
        else {
            return Err(Error::InvalidArgument);
        };
        let mask = u64::MAX >> (64 - repr.width());
        if repr.signed() {
            let value = i64::from_le_bytes(payload);
            if repr.width() < 64 {
                let bound = 1i64 << (repr.width() - 1);
                if value < -bound || value >= bound {
                    return Err(Error::InvalidTag);
                }
            }
            if !variants.contains(&((value as u64) & mask)) {
                return Err(Error::InvalidTag);
            }
            Ok(Body::I64(value))
        } else {
            let value = u64::from_le_bytes(payload);
            if repr.width() < 64 && value >> repr.width() != 0 {
                return Err(Error::InvalidTag);
            }
            if !variants.contains(&value) {
                return Err(Error::InvalidTag);
            }
            Ok(Body::U64(value))
        }
    }

    /// 元素的枚举体：元素体是 repr 自然宽度，有符号 repr 按最高位符号扩展到 8B（同核心
    /// `element_load`），再走同一判别值校验。
    fn enum_element(&self, d: &mut Decoder<'_>, definition: NodeId) -> AceResult<Body> {
        let OwnedNode::EnumDef { repr, .. } = &self.options.nodes[definition] else {
            return Err(Error::InvalidArgument);
        };
        let width = (repr.width() / 8) as usize;
        let bytes = d.read_element_bytes(width)?;
        let mut payload = [0u8; 8];
        payload[..width].copy_from_slice(bytes);
        if repr.signed() && bytes[width - 1] & 0x80 != 0 {
            payload[width..].fill(0xFF);
        }
        self.enum_body(definition, payload)
    }

    /// 缺席的枚举字段：注册了默认变体时恢复该变体（FR-D07），否则 `15 MissingRequiredField`。
    fn default_variant(&self, definition: NodeId) -> AceResult<Arc<AceValue>> {
        Ok(AceValue::node(default_variant_body(
            &self.options.nodes,
            definition,
        )?))
    }
}

/// 按已注册 schema 图解码整条消息：`Decoder` 与身份表都只存活在本次调用内（R32③）。
fn decode_registered(
    options: &AceOptions,
    root: NodeId,
    bytes: &[u8],
) -> AceResult<Arc<AceValue>> {
    let mut decoder = Decoder::new(bytes, &options.limits)?;
    let mut reader = DynReader::new(options);
    let value = reader.value(&mut decoder, root)?;
    decoder.finish()?;
    Ok(value)
}

/// 反序列化（`ace_deserialize`）：`type_key` 查已注册 schema，按 schema 把 wire 字节物化成
/// `AceValue` 动态值图。同一 wire 记录 id 在**一次调用内**解析为同一个原生节点（R32②③：对象与
/// `str`/`bytes`/`list`/`map` 承诺指针身份，标量与 >8B 宽值 record 不承诺）；**身份不跨调用缓存**
/// ——两次调用的结果同时存活时各自独立（AC-A05⑤）。
///
/// 多态对象按体头里的 `type_name`（即该节点自身的 `type_key`）查白名单解出派生 schema 的对象，
/// 并让解出的节点带上该 `type_key`（R33①③）；身份仍按 wire 记录 id 登记，不按 `type_key`（R33⑤）。
///
/// 失败（含 panic）后 `*out` 一律为 `NULL`（FR-A02），已构造的节点随身份表一并丢弃，不留半成品。
///
/// 输入**零拷贝借用**（见 [`input_slice`]）：不分配与 `len` 等大的缓冲，`max_input_bytes` 由
/// `Decoder::new` 在触碰字节之前判（超限 → `6`）。错误优先级与旧实现一致：参数 → 未注册
/// `type_key`（`3`）→ 输入预算（`6`）→ 其余解码错误。
///
/// 失败码：NULL `out`/`buf`/`type_key` → `1`；未注册 `type_key`（含 NULL options 的空白名单，
/// 以及多态体头里自称但未注册的 key）→ `3`；多态体的判别值不在 C ABI 的已注册集合（数值 tag）→
/// `4`；wire 强环 → `14`（命中「解码中」标记即拒，不先构造环）；同一记录 id 在不兼容 schema
/// 形状下复用 → `13`；其余核心解码错误原样透传：截断 `2`、非法标签 `4`、深度 `5`、资源 `6`、
/// 非法引用 `7`、UTF-8 `8`、缺必填字段 `15`、Map 重复键 `16`。
#[no_mangle]
pub extern "C" fn ace_deserialize(
    type_key: *const c_char,
    buf: *const u8,
    len: usize,
    options: *const AceOptions,
    out: *mut *mut AceValue,
) -> i32 {
    // SAFETY: 头 codec 契约要求 `out` 非 NULL 且指向可写的 `*mut AceValue` 槽；这里只在本函数
    // 作用域内建立一次 `&mut *mut AceValue`，成功路径的最后一步才写入，失败路径先置 NULL。
    let Some(out) = (unsafe { out.as_mut() }) else {
        return Error::InvalidArgument.code();
    };
    // 先置空：失败路径（含 panic）不得留下半成品。
    *out = ptr::null_mut();
    guard(|| {
        let key = required_type_key(type_key)?;
        // 输入零拷贝借用（`ptr` 可 NULL 仅当 `len == 0`，同 codec 条款）：不整包复制。
        let bytes = input_slice(buf, len)?;
        // NULL options = 默认限额 + 空白名单（头条款），故查不到 key → 3。
        let default_options;
        // SAFETY: 头契约：非 NULL 的 `options` 是本库 `ace_options_*` 产出且调用期间保活的句柄；
        // 只读借用 `&AceOptions`（限额、白名单、节点 arena），不跨调用保存，故调用方随后 free
        // 不影响本次解码。
        let options = match unsafe { options.as_ref() } {
            Some(options) => options,
            None => {
                default_options = AceOptions {
                    limits: Options::default(),
                    schemas: BTreeMap::new(),
                    nodes: Vec::new(),
                };
                &default_options
            }
        };
        let root = *options.schemas.get(&key).ok_or(Error::UnknownTypeKey)?;
        let value = decode_registered(options, root, bytes)?;
        *out = Arc::into_raw(value) as *mut AceValue;
        Ok(())
    })
}

/// 门 B 的分配/释放配对探针：只统计**已武装线程**的 alloc/realloc/free，故 `cargo test` 并行
/// 跑其它用例时不会把它们的分配算进来。计数用 `const` 初始化的 thread-local（无析构、无需
/// 惰性初始化），因此在分配器内部再取计数不会再进入分配路径。
#[cfg(test)]
mod alloc_probe {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static ALLOCS: Cell<usize> = const { Cell::new(0) };
        static DEALLOCS: Cell<usize> = const { Cell::new(0) };
        static NET_BYTES: Cell<i64> = const { Cell::new(0) };
        static REQUESTED_BYTES: Cell<usize> = const { Cell::new(0) };
    }

    /// 一次读数：累计分配次数、累计释放次数、净字节（分配 − 释放）、**累计请求字节**
    /// （alloc/realloc 申请量之和，含已释放的；用来断言「某段没有与输入等大的分配」）。
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Reading {
        pub allocs: usize,
        pub deallocs: usize,
        pub net_bytes: i64,
        pub requested_bytes: usize,
    }

    impl Reading {
        fn raw() -> Self {
            Self {
                allocs: ALLOCS.with(Cell::get),
                deallocs: DEALLOCS.with(Cell::get),
                net_bytes: NET_BYTES.with(Cell::get),
                requested_bytes: REQUESTED_BYTES.with(Cell::get),
            }
        }

        /// `self` 相对 `base` 的增量。
        fn since(self, base: Self) -> Self {
            Self {
                allocs: self.allocs - base.allocs,
                deallocs: self.deallocs - base.deallocs,
                net_bytes: self.net_bytes - base.net_bytes,
                requested_bytes: self.requested_bytes - base.requested_bytes,
            }
        }
    }

    /// 计数分配器：转调 `System`，只按线程标记记账。
    pub struct CountingAllocator;

    fn record(allocs: usize, deallocs: usize, bytes: i64, requested: usize) {
        // `try_with`：TLS 不可用时静默跳过——分配器里绝不能 panic。计数键没有析构，正常不会走到。
        let _ = ARMED.try_with(|armed| {
            if !armed.get() {
                return;
            }
            let _ = ALLOCS.try_with(|count| count.set(count.get() + allocs));
            let _ = DEALLOCS.try_with(|count| count.set(count.get() + deallocs));
            let _ = NET_BYTES.try_with(|net| net.set(net.get() + bytes));
            let _ = REQUESTED_BYTES.try_with(|total| total.set(total.get() + requested));
        });
    }

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record(1, 0, layout.size() as i64, layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record(1, 0, layout.size() as i64, layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // `realloc` 在分配器层面是「释放旧块 + 分配新块」，故计数上成对记一次；
            // 否则每次扩容都会让 alloc/free 次数相差一个“未配对的 alloc”而不是真的泄漏。
            record(1, 1, new_size as i64 - layout.size() as i64, new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            record(0, 1, -(layout.size() as i64), 0);
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    /// 在武装状态下跑 `body`，返回该段的增量读数。
    pub fn measure(body: impl FnOnce()) -> Reading {
        let base = Reading::raw();
        ARMED.with(|armed| armed.set(true));
        body();
        ARMED.with(|armed| armed.set(false));
        Reading::raw().since(base)
    }
}

#[cfg(test)]
#[global_allocator]
static COUNTING_ALLOCATOR: alloc_probe::CountingAllocator = alloc_probe::CountingAllocator;

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn dropped() -> usize {
        DROPPED.with(std::cell::Cell::get)
    }

    fn kind_of(value: *const AceValue) -> i32 {
        let mut kind = -1;
        assert_eq!(ace_value_kind(value, &mut kind), ACE_OK);
        kind
    }

    fn free(value: *mut AceValue) {
        assert!(!value.is_null());
        assert_eq!(ace_value_free(value), ACE_OK);
    }

    fn object() -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_object(ptr::null(), &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn list() -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_list(&mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn i64_value(number: i64) -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_i64(number, &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn str_value(text: &[u8]) -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_str(text.as_ptr(), text.len(), &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn bool_value(flag: bool) -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_bool(u8::from(flag), &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn null_value() -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_null(&mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn bytes_of(value: *const AceValue) -> Vec<u8> {
        let (mut bytes, mut len) = (ptr::null(), 0usize);
        assert_eq!(ace_value_get_bytes(value, &mut bytes, &mut len), ACE_OK);
        assert!(!bytes.is_null());
        unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec()
    }

    fn text_of(value: *const AceValue) -> Vec<u8> {
        let (mut bytes, mut len) = (ptr::null(), 0usize);
        assert_eq!(ace_value_get_str(value, &mut bytes, &mut len), ACE_OK);
        assert!(!bytes.is_null());
        unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec()
    }

    #[test]
    fn scalars_round_trip_with_kinds() {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_null(&mut value), ACE_OK);
        assert_eq!(kind_of(value), 0);
        free(value);

        let mut value = ptr::null_mut();
        assert_eq!(ace_value_bool(1, &mut value), ACE_OK);
        assert_eq!(kind_of(value), 1);
        let mut flag = 7u8;
        assert_eq!(ace_value_get_bool(value, &mut flag), ACE_OK);
        assert_eq!(flag, 1);
        free(value);

        let mut value = ptr::null_mut();
        assert_eq!(ace_value_bool(0, &mut value), ACE_OK);
        let mut flag = 1u8;
        assert_eq!(ace_value_get_bool(value, &mut flag), ACE_OK);
        assert_eq!(flag, 0);
        free(value);

        let value = i64_value(-7);
        assert_eq!(kind_of(value), 2);
        let mut number = 0i64;
        assert_eq!(ace_value_get_i64(value, &mut number), ACE_OK);
        assert_eq!(number, -7);
        free(value);

        let mut value = ptr::null_mut();
        assert_eq!(ace_value_u64(u64::MAX, &mut value), ACE_OK);
        assert_eq!(kind_of(value), 3);
        let mut number = 0u64;
        assert_eq!(ace_value_get_u64(value, &mut number), ACE_OK);
        assert_eq!(number, u64::MAX);
        free(value);
    }

    #[test]
    fn f64_bits_are_stored_verbatim() {
        // 普通值、-0 与带 payload 的 quiet NaN 都必须按位往返（不做宿主浮点运算）。
        for bits in [
            (-2.25f64).to_bits(),
            0x8000_0000_0000_0000, // -0.0
            0x7FF8_0000_0000_0001, // quiet NaN，低 29 位非 0
            0x7FF0_0000_0000_0000, // +inf
        ] {
            let mut value = ptr::null_mut();
            assert_eq!(ace_value_f64_bits(bits, &mut value), ACE_OK);
            assert_eq!(kind_of(value), 4);
            let mut read = 0u64;
            assert_eq!(ace_value_get_f64_bits(value, &mut read), ACE_OK);
            assert_eq!(read, bits);
            free(value);
        }
    }

    #[test]
    fn invalid_bool_is_rejected_and_out_is_null() {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_bool(2, &mut value), 1);
        assert!(value.is_null());
        assert_eq!(ace_value_bool(0xFF, &mut value), 1);
        assert!(value.is_null());
    }

    #[test]
    fn str_keeps_embedded_nul_and_empty_is_allowed() {
        let value = str_value(b"a\0b");
        assert_eq!(kind_of(value), 5);
        assert_eq!(text_of(value), b"a\0b");
        free(value);

        let mut value = ptr::null_mut();
        assert_eq!(ace_value_str(ptr::null(), 0, &mut value), ACE_OK);
        assert_eq!(text_of(value), b"");
        // NULL 指针 + 非零长度是非法参数。
        let mut rejected = ptr::null_mut();
        assert_eq!(ace_value_str(ptr::null(), 3, &mut rejected), 1);
        assert!(rejected.is_null());
        free(value);
    }

    #[test]
    fn str_requires_valid_utf8() {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_str(b"\xFF\xFE".as_ptr(), 2, &mut value), 8);
        assert!(value.is_null());
        // 内嵌 NUL 合法，不是 UTF-8 违例。
        let value = str_value("中文🙂\0tail".as_bytes());
        assert_eq!(text_of(value), "中文🙂\0tail".as_bytes());
        free(value);
    }

    #[test]
    fn bytes_round_trip_including_empty() {
        let mut value = ptr::null_mut();
        let raw = [0x00u8, 0xFF, 0x10];
        assert_eq!(ace_value_bytes(raw.as_ptr(), raw.len(), &mut value), ACE_OK);
        assert_eq!(kind_of(value), 6);
        assert_eq!(bytes_of(value), raw.to_vec());
        free(value);

        let mut value = ptr::null_mut();
        assert_eq!(ace_value_bytes(ptr::null(), 0, &mut value), ACE_OK);
        assert_eq!(bytes_of(value), Vec::<u8>::new());
        // Str 与 Bytes 是不同 kind，getter 不串用。
        let mut text = ptr::null();
        let mut len = 0usize;
        assert_eq!(ace_value_get_str(value, &mut text, &mut len), 13);
        free(value);
    }

    #[test]
    fn list_add_len_and_at() {
        let list = list();
        assert_eq!(kind_of(list), 7);
        let number = i64_value(7);
        assert_eq!(ace_list_add(list, number), ACE_OK);
        let text = str_value(b"x");
        assert_eq!(ace_list_add(list, text), ACE_OK);
        let mut len = 0usize;
        assert_eq!(ace_list_len(list, &mut len), ACE_OK);
        assert_eq!(len, 2);

        let mut element = ptr::null();
        assert_eq!(ace_list_at(list, 0, &mut element), ACE_OK);
        let mut read = 0i64;
        assert_eq!(ace_value_get_i64(element, &mut read), ACE_OK);
        assert_eq!(read, 7);

        assert_eq!(ace_list_at(list, 1, &mut element), ACE_OK);
        assert_eq!(text_of(element), b"x");

        // 越界（含 usize::MAX 这种极端下标）一律 12，不崩溃。
        assert_eq!(ace_list_at(list, 2, &mut element), 12);
        assert_eq!(ace_list_at(list, usize::MAX, &mut element), 12);

        // R32⑤：add 只给列表加一份引用，调用方句柄仍有效、须各自 free 减引用。
        free(number);
        free(text);
        free(list);
    }

    #[test]
    fn object_set_keeps_caller_handle_and_replace_drops_parent_reference_only() {
        let object = object();
        let old = str_value(b"old");
        assert_eq!(ace_object_set(object, 0, old), ACE_OK);
        // 父各持一份，调用方那份随即归还：只剩父引用（P2）。
        let one = i64_value(5);
        assert_eq!(ace_object_set(object, 1, one), ACE_OK);
        free(one);

        let mut field = ptr::null();
        assert_eq!(ace_object_get(object, 0, &mut field), ACE_OK);
        assert_eq!(text_of(field), b"old");
        let mut number = 0i64;
        assert_eq!(ace_object_get(object, 1, &mut field), ACE_OK);
        assert_eq!(ace_value_get_i64(field, &mut number), ACE_OK);
        assert_eq!(number, 5);

        // set 后调用方句柄仍可读、仍可被父引用之外的自己释放（R32⑤）。
        assert_eq!(text_of(old), b"old");
        let before = dropped();
        let payload = [0xDEu8, 0xAD];
        let fresh = bytes_value(&payload);
        assert_eq!(ace_object_set(object, 0, fresh), ACE_OK);
        // 替换只减父对旧值的引用；调用方仍持一份，故旧值此时不得被释放。
        assert_eq!(dropped(), before);
        assert_eq!(text_of(old), b"old");
        assert_eq!(ace_object_get(object, 0, &mut field), ACE_OK);
        assert_eq!(bytes_of(field), payload.to_vec());

        // 调用方自己减掉旧值的那份引用后才归零释放。
        free(old);
        assert_eq!(dropped(), before + 1);

        // 父修改后重新借用仍一致，且不 panic。
        let two = i64_value(9);
        assert_eq!(ace_object_set(object, 2, two), ACE_OK);
        free(two);
        assert_eq!(ace_object_get(object, 0, &mut field), ACE_OK);
        assert_eq!(bytes_of(field), payload.to_vec());
        free(fresh);
        free(object);
    }

    #[test]
    fn object_missing_and_reserved_fields_are_not_found() {
        let object = object();
        let mut field = ptr::null();
        assert_eq!(ace_object_get(object, 42, &mut field), 12);
        assert_eq!(ace_object_get(object, -1, &mut field), 12);

        // 保留号（负数 order）在 set 侧同样拒绝，且失败不加引用：child 仍可单独释放。
        let before = dropped();
        let child = i64_value(1);
        assert_eq!(ace_object_set(object, -1, child), 12);
        assert_eq!(dropped(), before);
        free(child);
        assert_eq!(dropped(), before + 1);
        free(object);
    }

    #[test]
    fn kind_mismatch_is_type_mismatch() {
        let value = str_value(b"text");
        let (mut number, mut unsigned, mut bits, mut flag) = (0i64, 0u64, 0u64, 0u8);
        assert_eq!(ace_value_get_i64(value, &mut number), 13);
        assert_eq!(ace_value_get_u64(value, &mut unsigned), 13);
        assert_eq!(ace_value_get_f64_bits(value, &mut bits), 13);
        assert_eq!(ace_value_get_bool(value, &mut flag), 13);
        let (mut ptr_out, mut len) = (ptr::null(), 0usize);
        assert_eq!(ace_value_get_bytes(value, &mut ptr_out, &mut len), 13);
        let mut len = 0usize;
        assert_eq!(ace_list_len(value, &mut len), 13);
        let mut element = ptr::null();
        assert_eq!(ace_list_at(value, 0, &mut element), 13);
        assert_eq!(ace_object_get(value, 0, &mut element), 13);
        // 失败路径不张引用，但内联创建的值仍归调用方，须自己归还（P2）。
        let stray = i64_value(1);
        assert_eq!(ace_object_set(value, 0, stray), 13);
        free(stray);
        free(value);

        let list = list();
        let mut field = ptr::null();
        assert_eq!(ace_object_get(list, 0, &mut field), 13);
        // 失败不增加引用：这里为验证不泄漏，直接释放即可。
        let child = i64_value(2);
        assert_eq!(ace_object_set(list, 0, child), 13);
        free(child);
        free(list);
    }

    #[test]
    fn null_arguments_are_invalid() {
        assert_eq!(ace_value_null(ptr::null_mut()), 1);
        assert_eq!(ace_value_i64(1, ptr::null_mut()), 1);
        assert_eq!(ace_value_bytes(b"x".as_ptr(), 1, ptr::null_mut()), 1);

        let value = i64_value(3);
        assert_eq!(ace_value_kind(ptr::null(), &mut 0i32), 1);
        assert_eq!(ace_value_get_i64(value, ptr::null_mut()), 1);
        assert_eq!(ace_value_get_i64(ptr::null(), &mut 0i64), 1);
        let (mut ptr_out, mut len) = (ptr::null(), 0usize);
        assert_eq!(ace_value_get_str(value, &mut ptr_out, &mut len), 13);
        assert_eq!(ace_value_get_str(ptr::null(), &mut ptr_out, &mut len), 1);
        assert_eq!(ace_value_get_str(value, ptr::null_mut(), &mut len), 1);
        assert_eq!(ace_value_get_str(value, &mut ptr_out, ptr::null_mut()), 1);
        // 上面全是失败/借用路径，不持引用：句柄仍归调用方（P2）。
        free(value);

        let object = object();
        let mut field = ptr::null();
        assert_eq!(ace_object_get(object, 0, ptr::null_mut()), 1);
        assert_eq!(ace_object_get(ptr::null(), 0, &mut field), 1);
        assert_eq!(ace_object_set(object, 0, ptr::null_mut()), 1);
        assert_eq!(ace_object_set(ptr::null_mut(), 0, ptr::null_mut()), 1);
        let child = i64_value(1);
        assert_eq!(ace_object_set(object, 0, child), ACE_OK);
        free(child);
        free(object);

        let list = list();
        let mut len = 0usize;
        assert_eq!(ace_list_len(list, ptr::null_mut()), 1);
        assert_eq!(ace_list_len(ptr::null(), &mut len), 1);
        assert_eq!(ace_list_at(list, 0, ptr::null_mut()), 1);
        assert_eq!(ace_list_add(list, ptr::null_mut()), 1);
        // NULL 接收者 → 1；失败不增引用，内联 child 须自己归还（P2）。
        let stray = i64_value(1);
        assert_eq!(ace_list_add(ptr::null_mut(), stray), 1);
        free(stray);
        free(list);
    }

    #[test]
    fn set_and_share_hold_independent_references() {
        let root = object();
        let child = str_value(b"keep");
        assert_eq!(ace_object_set(root, 0, child), ACE_OK);
        // 同一节点可被另一父与调用方各自引用：set 只加一份父引用，调用方句柄仍有效。
        let sink = list();
        assert_eq!(ace_list_add(sink, child), ACE_OK);
        assert_eq!(text_of(child), b"keep");

        let before = dropped();
        // 调用方减掉自己那份后，两个父容器仍各持一份，节点不得释放。
        free(child);
        assert_eq!(dropped(), before);        let mut field = ptr::null();
        assert_eq!(ace_object_get(root, 0, &mut field), ACE_OK);
        assert_eq!(text_of(field), b"keep");
        let mut element = ptr::null();
        assert_eq!(ace_list_at(sink, 0, &mut element), ACE_OK);
        assert_eq!(text_of(element), b"keep");
        // 两处借用视图是同一个节点地址（共享身份）。
        assert_eq!(field, element);

        // 引用归零才释放：列表节点自己释放，共享的 str 仍被 root 持有。
        let before_list = dropped();
        free(sink);
        assert_eq!(dropped(), before_list + 1);
        assert_eq!(ace_object_get(root, 0, &mut field), ACE_OK);
        assert_eq!(text_of(field), b"keep");
        free(root);
        // root 与它引用的 str 一起释放（合上前面列表那一个，共 3 个节点）。
        assert_eq!(dropped(), before_list + 3);
    }

    #[test]
    fn share_points_at_the_same_node_and_both_handles_release_once() {
        let value = str_value(b"shared");
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(value, &mut shared), ACE_OK);
        // 同一原生节点：两枚句柄地址相同（C 侧 `ptr ==`），引用计数 +1。
        assert_eq!(shared, value);
        assert_eq!(text_of(shared), b"shared");

        let before = dropped();
        // 两枚句柄各 free 一次：第一次只减引用，节点仍可读。
        free(shared);
        assert_eq!(dropped(), before);
        assert_eq!(text_of(value), b"shared");
        free(value);
        assert_eq!(dropped(), before + 1);
    }

    #[test]
    fn share_failure_classes_keep_out_empty() {
        let value = str_value(b"x");
        // (a) `out == NULL` → 1，不访问输出。
        assert_eq!(ace_value_share(value, ptr::null_mut()), 1);
        // (b) `out` 合法而 `value == NULL` → 1 且 `*out` 置空，不沿用调用方原值。
        let mut out = value;
        assert_eq!(ace_value_share(ptr::null(), &mut out), 1);
        assert!(out.is_null());
        // 对照组：合法参数成功，且 `*out` 与入参同一节点。
        let mut out = ptr::null_mut();
        assert_eq!(ace_value_share(value, &mut out), ACE_OK);
        assert_eq!(out, value);
        free(out);
        free(value);
    }

    #[test]
    fn self_reference_is_constructible_and_free_does_not_release_it() {
        // 强环是调用方使用禁令而不是未定义行为：构造成功，由 ace_serialize 拒为 14。
        let root = object();
        assert_eq!(ace_object_set(root, 0, root), ACE_OK);
        let before = dropped();
        // 引用计数语义：调用方这份减掉后环内自引用仍持一份，不保证释放。
        assert_eq!(ace_value_free(root), ACE_OK);
        assert_eq!(dropped(), before);
    }

    #[test]
    fn list_self_reference_is_constructible_and_free_does_not_release_it() {
        // 列表自环与对象自环同路：add 把列表加进自己也得构造得成（P1a 回归）。
        let root = list();
        assert_eq!(ace_list_add(root, root), ACE_OK);
        let before = dropped();
        // 同上：环内自引用仍持一份，不保证释放（本用例的例外）。
        assert_eq!(ace_value_free(root), ACE_OK);
        assert_eq!(dropped(), before);
    }

    #[test]
    fn null_free_is_idempotent_and_borrowed_views_match_nodes() {
        let object = object();
        let keep = str_value(b"keep");
        assert_eq!(ace_object_set(object, 0, keep), ACE_OK);
        let mut borrowed = ptr::null();
        assert_eq!(ace_object_get(object, 0, &mut borrowed), ACE_OK);
        // 借用视图就是节点地址，与调用方句柄指向同一节点（但借用不持引用）。
        assert_eq!(borrowed, keep.cast_const());
        assert_eq!(text_of(borrowed), b"keep");

        let before = dropped();
        free(keep);
        assert_eq!(text_of(borrowed), b"keep");
        free(object);
        // object + str 两个节点释放。
        assert_eq!(dropped(), before + 2);
        // NULL 视为已释放（幂等兜底，不触碰任何内存）。
        assert_eq!(ace_value_free(ptr::null_mut()), ACE_OK);
    }

    #[test]
    fn type_key_is_optional_and_must_be_utf8() {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_object(ptr::null(), &mut value), ACE_OK);
        assert_eq!(kind_of(value), 8);
        free(value);

        let mut value = ptr::null_mut();
        assert_eq!(ace_value_object(c"demo.root".as_ptr(), &mut value), ACE_OK);
        free(value);

        let mut value = ptr::null_mut();
        assert_eq!(ace_value_object(c"\xFF".as_ptr(), &mut value), 8);
        assert!(value.is_null());
    }

    #[test]
    fn panic_is_caught_and_mapped() {
        let code = guard(|| -> Result<(), Error> { panic!("boom") });
        assert_eq!(code, Error::PanicCaught.code());
        assert_eq!(code, 10);
    }

    // ——— 类型引用节点与 schema ———

    fn dropped_types() -> usize {
        TYPE_DROPPED.with(std::cell::Cell::get)
    }

    fn free_type(node: *mut AceTypeRef) {
        assert!(!node.is_null());
        assert_eq!(ace_type_free(node), ACE_OK);
    }

    fn prim(kind: i32) -> *mut AceTypeRef {
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_prim(kind, &mut node), ACE_OK);
        assert!(!node.is_null());
        node
    }

    fn schema_with(key: *const c_char) -> *mut AceSchema {
        let mut schema = ptr::null_mut();
        assert_eq!(ace_schema_new(key, &mut schema), ACE_OK);
        assert!(!schema.is_null());
        schema
    }

    fn enum_with(key: *const c_char) -> *mut AceEnumSchema {
        let mut enum_schema = ptr::null_mut();
        assert_eq!(ace_enum_new(key, &mut enum_schema), ACE_OK);
        assert!(!enum_schema.is_null());
        enum_schema
    }

    #[test]
    fn prim_accepts_only_scalar_and_wide_kinds() {
        // 子集 = ACE_FK_BOOL..ACE_FK_UINTPTR 去掉 STR(12)/BYTES(13)。
        for kind in (0..=18).filter(|kind| *kind != 12 && *kind != 13) {
            free_type(prim(kind));
        }
        // STR/BYTES/复合/ENUM 及越界值一律 1，且 out 置空。
        for kind in [12, 13, 19, 20, 21, 22, 23, 24, -1] {
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_prim(kind, &mut node), 1);
            assert!(node.is_null());
        }
        assert_eq!(ace_type_prim(0, ptr::null_mut()), 1);
    }

    #[test]
    fn option_rejects_nested_option_and_keeps_child_on_failure() {
        let mut option = ptr::null_mut();
        assert_eq!(ace_type_option(prim(5), &mut option), ACE_OK);
        // 内层不得再为 option：失败时不收编外层 option。
        let mut rejected = ptr::null_mut();
        assert_eq!(ace_type_option(option, &mut rejected), 1);
        assert!(rejected.is_null());
        // List<Option<i32>> 合法。
        let mut list = ptr::null_mut();
        assert_eq!(ace_type_list(option, &mut list), ACE_OK);
        free_type(list);

        assert_eq!(ace_type_option(ptr::null_mut(), &mut rejected), 1);
        let stray = prim(1);
        assert_eq!(ace_type_option(stray, ptr::null_mut()), 1);
        free_type(stray);
    }

    #[test]
    fn map_key_must_be_wire_orderable() {
        let mut map = ptr::null_mut();
        // f32 无全序，不能作键；失败不收编两个子节点。
        let (float_key, val) = (prim(9), prim(5));
        assert_eq!(ace_type_map(float_key, val, &mut map), 1);
        assert!(map.is_null());
        free_type(float_key);
        free_type(val);

        // 键子树内的 f32 同样无全序。
        let mut option_float = ptr::null_mut();
        assert_eq!(ace_type_option(prim(9), &mut option_float), ACE_OK);
        let val = prim(5);
        assert_eq!(ace_type_map(option_float, val, &mut map), 1);
        free_type(option_float);
        free_type(val);

        // 同一节点不能同时作 key 与 val。
        let shared = prim(7);
        let before = dropped_types();
        assert_eq!(ace_type_map(shared, shared, &mut map), 1);
        assert_eq!(dropped_types(), before);
        free_type(shared);

        // 字符串键 + List 值合法。
        let mut key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut key), ACE_OK);
        let mut list = ptr::null_mut();
        assert_eq!(ace_type_list(prim(5), &mut list), ACE_OK);
        assert_eq!(ace_type_map(key, list, &mut map), ACE_OK);
        free_type(map);
    }

    #[test]
    fn schema_rejects_duplicate_and_negative_field_id() {
        let schema = schema_with(c"demo.record".as_ptr());
        assert_eq!(ace_schema_add_field(schema, 3, prim(5)), ACE_OK);
        // 重复 field_id → 1，且失败未收编。
        let duplicate = prim(7);
        assert_eq!(ace_schema_add_field(schema, 3, duplicate), 1);
        free_type(duplicate);
        // 负 field_id（order 非负）→ 1。
        let negative = prim(7);
        assert_eq!(ace_schema_add_field(schema, -1, negative), 1);
        free_type(negative);

        // type_key 必填：NULL → 1，非 UTF-8 → 8。
        let mut missing = ptr::null_mut();
        assert_eq!(ace_schema_new(ptr::null(), &mut missing), 1);
        assert!(missing.is_null());
        assert_eq!(ace_schema_new(c"\xFF".as_ptr(), &mut missing), 8);
        assert!(missing.is_null());
        assert_eq!(ace_schema_free(schema), ACE_OK);
    }

    #[test]
    fn enum_repr_accepts_only_fixed_width_integers() {
        let enum_schema = enum_with(c"demo.color".as_ptr());
        for kind in [0, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 23, -1] {
            assert_eq!(ace_enum_set_repr(enum_schema, kind), 1);
        }
        for kind in 1..=8 {
            assert_eq!(ace_enum_set_repr(enum_schema, kind), ACE_OK);
        }
        assert_eq!(ace_enum_free(enum_schema), ACE_OK);
    }

    #[test]
    fn enum_tags_are_bit_patterns_truncated_to_repr_width() {
        let enum_schema = enum_with(c"demo.color".as_ptr());
        assert_eq!(ace_enum_set_repr(enum_schema, 1), ACE_OK); // i8
        // repr(i8)=-1 传 0xFF 合法（高位全 0）。
        assert_eq!(ace_enum_add_variant(enum_schema, 0xFF), ACE_OK);
        // 0xFFFF_FFFF 超 i8 位宽且不是该位宽的规范符号扩展。
        assert_eq!(ace_enum_add_variant(enum_schema, 0xFFFF_FFFF), 1);
        // u64::MAX 是 0xFF 的规范符号扩展 → 截回后与已有变体重复。
        assert_eq!(ace_enum_add_variant(enum_schema, u64::MAX), 1);
        assert_eq!(ace_enum_add_variant(enum_schema, 0x01FF), 1);
        assert_eq!(ace_enum_add_variant(enum_schema, 0x00FF), 1);
        // 高位全 0 的零扩展合法；截回后同值仍算重复。
        assert_eq!(ace_enum_add_variant(enum_schema, 0x7F), ACE_OK);
        assert_eq!(ace_enum_add_variant(enum_schema, 0x00_007F), 1);
        assert_eq!(ace_enum_free(enum_schema), ACE_OK);

        // 已有变体放不进更窄的 repr → 1，repr 保持不变。
        let enum_schema = enum_with(c"demo.width".as_ptr());
        assert_eq!(ace_enum_set_repr(enum_schema, 3), ACE_OK); // i16
        assert_eq!(ace_enum_add_variant(enum_schema, 0x0100), ACE_OK);
        assert_eq!(ace_enum_set_repr(enum_schema, 1), 1); // i8 放不下 0x0100
        assert_eq!(ace_enum_set_repr(enum_schema, 5), ACE_OK); // i32
        assert_eq!(ace_enum_add_variant(enum_schema, 0x0100), 1);
        assert_eq!(ace_enum_free(enum_schema), ACE_OK);
    }

    #[test]
    fn enum_default_variant_must_be_registered() {
        let enum_schema = enum_with(c"demo.color".as_ptr());
        assert_eq!(ace_enum_set_repr(enum_schema, 3), ACE_OK); // i16
        assert_eq!(ace_enum_add_variant(enum_schema, 0), ACE_OK);
        assert_eq!(ace_enum_add_variant(enum_schema, 0xFFFF), ACE_OK);
        assert_eq!(ace_enum_set_default_variant(enum_schema, 0), ACE_OK);
        // 不在变体集 → 1；超 i16 位宽且非规范扩展 → 1。
        assert_eq!(ace_enum_set_default_variant(enum_schema, 7), 1);
        assert_eq!(ace_enum_set_default_variant(enum_schema, 0x1_0000), 1);
        // u64::MAX 是 0xFFFF 的规范符号扩展 → 截回后同值，仍是已注册变体。
        assert_eq!(ace_enum_set_default_variant(enum_schema, u64::MAX), ACE_OK);
        assert_eq!(ace_enum_free(enum_schema), ACE_OK);
    }

    #[test]
    fn adopted_nodes_cannot_be_freed_and_parent_stays_usable() {
        let schema = schema_with(c"demo.record".as_ptr());
        let field = prim(5);
        assert_eq!(ace_schema_add_field(schema, 0, field), ACE_OK);
        // 已收编：句柄随 schema 释放，单独 free → 1，且 schema 仍可用。
        assert_eq!(ace_type_free(field), 1);
        assert_eq!(ace_schema_add_field(schema, 1, prim(7)), ACE_OK);

        let inner = prim(5);
        let mut option = ptr::null_mut();
        assert_eq!(ace_type_option(inner, &mut option), ACE_OK);
        assert_eq!(ace_type_free(inner), 1);
        assert_eq!(ace_schema_add_field(schema, 2, option), ACE_OK);
        assert_eq!(ace_type_free(option), 1);

        // 收编的三个字段节点 + option 内层共 4 个节点随 schema 释放。
        let before = dropped_types();
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(dropped_types(), before + 4);
    }

    #[test]
    fn schema_self_reference_is_rejected_as_cycle() {
        let schema = schema_with(c"demo.cyclic".as_ptr());
        let mut object_type = ptr::null_mut();
        assert_eq!(ace_type_object(schema, &mut object_type), ACE_OK);
        // 字段类型借用了它即将归属的 schema → 环。
        assert_eq!(ace_schema_add_field(schema, 0, object_type), 1);
        // 失败未收编：节点仍归调用方。
        assert_eq!(ace_type_free(object_type), ACE_OK);
        assert_eq!(ace_type_object(ptr::null_mut(), &mut object_type), 1);
        assert!(object_type.is_null());

        // 借用其它 schema 的节点可以收编：先释放引用方，再释放被借用实体。
        let other = schema_with(c"demo.other".as_ptr());
        assert_eq!(ace_type_object(other, &mut object_type), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, object_type), ACE_OK);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(other), ACE_OK);
    }

    #[test]
    fn adoption_and_free_are_counted_once_per_node() {
        let before = dropped_types();
        let mut list = ptr::null_mut();
        assert_eq!(ace_type_list(prim(5), &mut list), ACE_OK);
        let mut outer = ptr::null_mut();
        assert_eq!(ace_type_list(list, &mut outer), ACE_OK);
        // 已收编的节点不得再次收编，也不得单独释放。
        let mut rejected = ptr::null_mut();
        assert_eq!(ace_type_list(list, &mut rejected), 1);
        assert!(rejected.is_null());
        assert_eq!(ace_type_free(list), 1);
        assert_eq!(dropped_types(), before);
        free_type(outer);
        assert_eq!(dropped_types(), before + 3);
        assert_eq!(ace_type_free(ptr::null_mut()), ACE_OK);
    }

    #[test]
    fn deep_nesting_frees_without_recursion() {
        const DEPTH: usize = 20_000;
        let before = dropped_types();
        let mut node = prim(5);
        for _ in 0..DEPTH {
            let mut outer = ptr::null_mut();
            assert_eq!(ace_type_list(node, &mut outer), ACE_OK);
            node = outer;
        }
        // 迭代释放：不因嵌套深度展开成栈深度。
        free_type(node);
        assert_eq!(dropped_types(), before + DEPTH + 1);
    }

    #[test]
    fn null_enum_and_type_handles_are_invalid() {
        let mut enum_schema = ptr::null_mut();
        assert_eq!(ace_enum_new(ptr::null(), &mut enum_schema), 1);
        assert!(enum_schema.is_null());
        assert_eq!(ace_enum_new(c"\xFF".as_ptr(), &mut enum_schema), 8);
        assert!(enum_schema.is_null());
        assert_eq!(ace_enum_set_repr(ptr::null_mut(), 5), 1);
        assert_eq!(ace_enum_add_variant(ptr::null_mut(), 0), 1);
        assert_eq!(ace_enum_set_default_variant(ptr::null_mut(), 0), 1);
        assert_eq!(ace_enum_free(ptr::null_mut()), ACE_OK);
        assert_eq!(ace_schema_free(ptr::null_mut()), ACE_OK);

        let stray = prim(5);
        assert_eq!(ace_schema_add_field(ptr::null_mut(), 0, stray), 1);
        assert_eq!(ace_schema_add_field(schema_with(c"demo.record".as_ptr()), 0, ptr::null_mut()), 1);
        free_type(stray);

        // 枚举 schema 默认 repr 为 i32：0xFFFF_FFFF 合法；u64::MAX 是它的规范符号扩展 →
        // 截回后同值，算重复。
        let enum_schema = enum_with(c"demo.color".as_ptr());
        assert_eq!(ace_enum_add_variant(enum_schema, 0xFFFF_FFFF), ACE_OK);
        assert_eq!(ace_enum_add_variant(enum_schema, u64::MAX), 1);
        assert_eq!(ace_enum_add_variant(enum_schema, 1), ACE_OK);
        assert_eq!(ace_enum_free(enum_schema), ACE_OK);
    }

    // ——— options 与 schema 白名单 ———

    fn options() -> *mut AceOptions {
        let mut options = ptr::null_mut();
        assert_eq!(ace_options_new(&mut options), ACE_OK);
        assert!(!options.is_null());
        options
    }

    /// 已注册图的根字段表（克隆，避免借用提前结算）。句柄均来自本库且未释放。
    fn registered_fields(options: *mut AceOptions, type_key: &str) -> BTreeMap<i32, NodeId> {
        // SAFETY: 测试内 options 句柄存活。
        let options = unsafe { &*options };
        let root = *options.schemas.get(type_key).expect("type_key 已注册");
        match &options.nodes[root] {
            OwnedNode::Schema { fields, .. } => fields.clone(),
            _ => panic!("根节点必须是 Schema 节点"),
        }
    }

    /// 含两个字段的 `demo.record`：0 → i32，1 → list<string>。
    fn record_schema() -> *mut AceSchema {
        let schema = schema_with(c"demo.record".as_ptr());
        assert_eq!(ace_schema_add_field(schema, 0, prim(5)), ACE_OK);
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        let mut list = ptr::null_mut();
        assert_eq!(ace_type_list(text, &mut list), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 1, list), ACE_OK);
        schema
    }

    #[test]
    fn options_default_matches_rust_defaults() {
        let options = options();
        // 默认值只以 `Options::default()` 为单一来源，逐项核对防止 FFI 侧另写一套常量。
        // SAFETY: 测试内 options 句柄存活。
        let limits = unsafe { &(*options).limits };
        assert_eq!(*limits, Options::default());
        assert!(limits.packed);
        assert_eq!(limits.max_depth, 256);
        assert_eq!(limits.max_build_nesting, 256);
        assert_eq!(limits.max_input_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_raw_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_decoded_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_records, 1 << 20);
        assert_eq!(limits.max_slots, 1 << 20);
        assert_eq!(limits.max_collection_len, 1 << 24);
        assert_eq!(limits.max_references, 1 << 20);
        assert_eq!(limits.max_map_string_key_bytes, 1024 * 1024);
        // 白名单默认为空。
        assert!(unsafe { &(*options).schemas }.is_empty());

        assert_eq!(ace_options_free(options), ACE_OK);
        // NULL 视为已释放（同 `ace_value_free`）；NULL out → 1。
        assert_eq!(ace_options_free(ptr::null_mut()), ACE_OK);
        assert_eq!(ace_options_new(ptr::null_mut()), 1);
    }

    #[test]
    fn options_setters_apply_and_reject_invalid_arguments() {
        let options = options();
        assert!(unsafe { &(*options).limits }.packed);
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        assert!(!unsafe { &(*options).limits }.packed);
        assert_eq!(ace_options_set_packed(options, 1), ACE_OK);
        assert!(unsafe { &(*options).limits }.packed);
        // 仅 0/1（同 `ace_value_bool`）：其余 → 1，且原值不变。
        assert_eq!(ace_options_set_packed(options, 2), 1);
        assert_eq!(ace_options_set_packed(options, 0xFF), 1);
        assert!(unsafe { &(*options).limits }.packed);

        // 头里的参数序：depth, input, raw, decoded, records, slots, collection_len,
        // references, map_string_key_bytes。
        assert_eq!(
            ace_options_set_limits(options, 3, 4, 5, 6, 7, 8, 9, 10, 11),
            ACE_OK
        );
        {
            let limits = unsafe { &(*options).limits };
            assert_eq!(limits.max_depth, 3);
            assert_eq!(limits.max_input_bytes, 4);
            assert_eq!(limits.max_raw_bytes, 5);
            assert_eq!(limits.max_decoded_bytes, 6);
            assert_eq!(limits.max_records, 7);
            assert_eq!(limits.max_slots, 8);
            assert_eq!(limits.max_collection_len, 9);
            assert_eq!(limits.max_references, 10);
            assert_eq!(limits.max_map_string_key_bytes, 11);
            // 身份构造闭包峰值不属 `set_limits`：该字段保持默认值。
            assert_eq!(limits.max_build_nesting, 256);
        }

        assert_eq!(ace_options_set_build_nesting(options, 64), ACE_OK);
        assert_eq!(unsafe { &(*options).limits }.max_build_nesting, 64);
        // 两个限额分列（R25）：`set_limits` 不覆盖身份构造闭包峰值。
        assert_eq!(
            ace_options_set_limits(options, 1, 1, 1, 1, 1, 1, 1, 1, 1),
            ACE_OK
        );
        assert_eq!(unsafe { &(*options).limits }.max_build_nesting, 64);

        // 零值合法：表示该维度最严格上限，不是禁用限制。
        assert_eq!(
            ace_options_set_limits(options, 0, 0, 0, 0, 0, 0, 0, 0, 0),
            ACE_OK
        );
        assert_eq!(unsafe { &(*options).limits }.max_depth, 0);
        assert_eq!(unsafe { &(*options).limits }.max_slots, 0);
        assert_eq!(ace_options_set_build_nesting(options, 0), ACE_OK);
        assert_eq!(unsafe { &(*options).limits }.max_build_nesting, 0);

        // NULL options → 1。
        assert_eq!(ace_options_set_packed(ptr::null_mut(), 1), 1);
        assert_eq!(
            ace_options_set_limits(ptr::null_mut(), 0, 0, 0, 0, 0, 0, 0, 0, 0),
            1
        );
        assert_eq!(ace_options_set_build_nesting(ptr::null_mut(), 1), 1);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn add_schema_deep_copies_graph_after_caller_frees() {
        let options = options();
        let schema = record_schema();
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        // 调用方释放自己那份（连同收编的字段类型节点）：库内副本必须仍然完整。
        assert_eq!(ace_schema_free(schema), ACE_OK);

        // SAFETY: options 句柄存活，白名单已注册该 type_key。
        let nodes = unsafe { &(*options).nodes };
        let fields = registered_fields(options, "demo.record");
        assert_eq!(fields.len(), 2);
        assert_eq!(unsafe { (*options).schemas.len() }, 1);
        // 可观察字段内容：字段 0 仍是 Prim(i32)，字段 1 仍是带字符串叶的 List。
        match &nodes[fields[&0]] {
            OwnedNode::Prim(kind) => assert_eq!(*kind, 5),
            _ => panic!("字段 0 应为 Prim 节点"),
        }
        let OwnedNode::List(element) = &nodes[fields[&1]] else {
            panic!("字段 1 应为 List 节点");
        };
        assert!(matches!(nodes[*element], OwnedNode::Str));
        // Schema + Prim + List + Str：arena 里只有这四个节点。
        assert_eq!(nodes.len(), 4);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn add_schema_copies_borrowed_enum_schema() {
        let options = options();
        let schema = schema_with(c"demo.record".as_ptr());
        let color = enum_with(c"demo.color".as_ptr());
        assert_eq!(ace_enum_set_repr(color, 1), ACE_OK); // i8
        assert_eq!(ace_enum_add_variant(color, 0x00), ACE_OK);
        assert_eq!(ace_enum_add_variant(color, 0xFF), ACE_OK);
        assert_eq!(ace_enum_set_default_variant(color, 0xFF), ACE_OK);
        let mut enum_type = ptr::null_mut();
        assert_eq!(ace_type_enum(color, &mut enum_type), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, enum_type), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 复制完成后调用方可 free 全部调用方侧结构（先 free 引用方 schema，再 free 被借用 enum）。
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_enum_free(color), ACE_OK);

        // SAFETY: options 句柄存活。
        let nodes = unsafe { &(*options).nodes };
        let fields = registered_fields(options, "demo.record");
        let OwnedNode::Enum(definition) = &nodes[fields[&0]] else {
            panic!("字段 0 应为 Enum 节点");
        };
        match &nodes[*definition] {
            OwnedNode::EnumDef {
                type_key,
                repr,
                variants,
                default,
            } => {
                assert_eq!(&**type_key, "demo.color");
                assert!(*repr == EnumRepr::I8);
                assert_eq!(*variants, BTreeSet::from([0x00, 0xFF]));
                assert_eq!(*default, Some(0xFF));
            }
            _ => panic!("Enum 节点必须指向 EnumDef"),
        }
        assert_eq!(nodes.len(), 3); // Schema + Enum + EnumDef
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn add_schema_rejects_duplicate_type_key_and_keeps_registration() {
        let options = options();
        let first = record_schema();
        assert_eq!(ace_options_add_schema(options, first), ACE_OK);
        let duplicate = schema_with(c"demo.record".as_ptr());
        assert_eq!(ace_schema_add_field(duplicate, 0, prim(1)), ACE_OK);
        assert_eq!(ace_options_add_schema(options, duplicate), 1);
        assert_eq!(ace_schema_free(duplicate), ACE_OK);
        // 失败不影响已注册集合：仍是首次注册的两字段副本。
        assert_eq!(registered_fields(options, "demo.record").len(), 2);
        assert_eq!(unsafe { (*options).schemas.len() }, 1);
        assert_eq!(ace_schema_free(first), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn add_schema_rejects_null_handles() {
        let options = options();
        let schema = schema_with(c"demo.record".as_ptr());
        assert_eq!(ace_options_add_schema(ptr::null_mut(), schema), 1);
        assert_eq!(ace_options_add_schema(options, ptr::null()), 1);
        assert!(unsafe { &(*options).schemas }.is_empty());
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn add_schema_leaves_adopted_field_nodes_owned_by_caller_schema() {
        let options = options();
        let schema = schema_with(c"demo.record".as_ptr());
        let field = prim(5);
        assert_eq!(ace_schema_add_field(schema, 0, field), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        // 收编者归 schema：注册不改变所有权，也不让句柄变成可单独释放。
        assert_ne!(ace_type_free(field), ACE_OK);
        // 注册是「拷贝当时的图」：调用方后续再挂字段、再释放都不影响库内副本。
        assert_eq!(ace_schema_add_field(schema, 1, prim(7)), ACE_OK);
        assert_eq!(registered_fields(options, "demo.record").len(), 1);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(registered_fields(options, "demo.record").len(), 1);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn add_schema_copies_mutually_referencing_schemas_once() {
        let options = options();
        let a = schema_with(c"demo.a".as_ptr());
        let b = schema_with(c"demo.b".as_ptr());
        let mut object_b = ptr::null_mut();
        assert_eq!(ace_type_object(b, &mut object_b), ACE_OK);
        assert_eq!(ace_schema_add_field(a, 0, object_b), ACE_OK);
        let mut object_a = ptr::null_mut();
        assert_eq!(ace_type_object(a, &mut object_a), ACE_OK);
        // a → b → a 是合法调用方图（`add_field` 只拒直接自引用），深拷贝须靠记忆化收敛。
        assert_eq!(ace_schema_add_field(b, 0, object_a), ACE_OK);
        assert_eq!(ace_options_add_schema(options, a), ACE_OK);

        // SAFETY: options 句柄存活。
        let nodes = unsafe { &(*options).nodes };
        let root = unsafe { &(*options).schemas }["demo.a"];
        let a_fields = registered_fields(options, "demo.a");
        let OwnedNode::Object(b_id) = &nodes[a_fields[&0]] else {
            panic!("a 的字段应为 Object 节点");
        };
        let OwnedNode::Schema {
            type_key: b_key,
            fields: b_fields,
        } = &nodes[*b_id]
        else {
            panic!("Object 应指向 Schema 节点");
        };
        assert_eq!(&**b_key, "demo.b");
        let OwnedNode::Object(a_id) = &nodes[b_fields[&0]] else {
            panic!("b 的字段应为 Object 节点");
        };
        // 回到根：环在 arena 里闭合，互引不重复展开。
        assert_eq!(*a_id, root);
        assert_eq!(nodes.len(), 4); // Schema a + Object b + Schema b + Object a

        // 调用方侧互引没有严格释放先后（析构不解引用借用目标）。
        assert_eq!(ace_schema_free(a), ACE_OK);
        assert_eq!(ace_schema_free(b), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn add_schema_copies_deep_nesting_without_recursion() {
        const DEPTH: usize = 10_000;
        let options = options();
        let schema = schema_with(c"demo.deep".as_ptr());
        let mut node = prim(5);
        for _ in 0..DEPTH {
            let mut outer = ptr::null_mut();
            assert_eq!(ace_type_list(node, &mut outer), ACE_OK);
            node = outer;
        }
        assert_eq!(ace_schema_add_field(schema, 0, node), ACE_OK);
        // 深拷贝两趟都是迭代遍历：不随嵌套深度展开成栈深度。
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // SAFETY: options 句柄存活。
        let nodes = unsafe { &(*options).nodes };
        // 1 个 Schema + DEPTH 个 List + 1 个 Prim。
        assert_eq!(nodes.len(), DEPTH + 2);
        let mut cursor = registered_fields(options, "demo.deep")[&0];
        let mut depth = 0;
        while let OwnedNode::List(child) = &nodes[cursor] {
            cursor = *child;
            depth += 1;
        }
        assert_eq!(depth, DEPTH);
        match &nodes[cursor] {
            OwnedNode::Prim(kind) => assert_eq!(*kind, 5),
            _ => panic!("最深层应为 Prim 节点"),
        }

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    // ——— codec：`ace_serialize` / `ace_buffer_free`（P3-3b） ———

    fn u64_value(number: u64) -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_u64(number, &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn bytes_value(bytes: &[u8]) -> *mut AceValue {
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_bytes(bytes.as_ptr(), bytes.len(), &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn set(object: *mut AceValue, field_id: i32, child: *mut AceValue) {
        assert_eq!(ace_object_set(object, field_id, child), ACE_OK);
    }

    fn add(list: *mut AceValue, child: *mut AceValue) {
        assert_eq!(ace_list_add(list, child), ACE_OK);
    }

    /// 装配用：`set` 后立即归还调用方那份引用（父容器已持一份）。
    ///
    /// 与 [`set`] 分开：共享身份/替换用例仍要在 `set` 之后继续用 child 句柄；
    /// 普通树/DAG 装配不该把调用方句柄永久留在那里。
    fn set_owned(object: *mut AceValue, field_id: i32, child: *mut AceValue) {
        set(object, field_id, child);
        free(child);
    }

    /// 装配用：`add` 后立即归还调用方那份引用（同上）。
    fn add_owned(list: *mut AceValue, child: *mut AceValue) {
        add(list, child);
        free(child);
    }

    /// Map 的 ABI 值（头）：`List<Object{k: order 0, v: order 1}>`，按实参次序插入。
    fn map_value(entries: &[(&str, i64)]) -> *mut AceValue {
        let map = list();
        for (key, value) in entries {
            add_owned(map, map_pair(str_value(key.as_bytes()), i64_value(*value)));
        }
        map
    }

    /// 一对 Map 条目（头 ABI：`Object{k: order 0, v: order 1}`），键值任意种类。
    fn map_pair(entry_key: *mut AceValue, entry_value: *mut AceValue) -> *mut AceValue {
        let entry = object();
        set_owned(entry, 0, entry_key);
        set_owned(entry, 1, entry_value);
        entry
    }

    /// `demo.model` 的动态值：与参照 `Model` 同值，Map 故意乱序插入。
    fn model_value(color_tag: u64) -> *mut AceValue {
        let value = object();
        set_owned(value, 0, i64_value(7));
        set_owned(value, 1, str_value("模型".as_bytes()));
        set_owned(value, 2, bytes_value(&[0x00, 0xFF, 0x10]));
        let values = list();
        for number in [1i64, -2, 3] {
            add_owned(values, i64_value(number));
        }
        set_owned(value, 3, values);
        // 4 号 option<string> 缺席（= None）；5 号 option<i32> 写 Some(0)（默认值也必须写槽）。
        set_owned(value, 5, i64_value(0));
        set_owned(value, 6, map_value(&[("b", 2), ("a", 1)]));
        set_owned(value, 7, u64_value(color_tag));
        let inner = object();
        set_owned(inner, 0, i64_value(-5));
        set_owned(inner, 1, str_value("内".as_bytes()));
        set_owned(value, 8, inner);
        value
    }

    /// 字节字段的参照编码：核心没有「原始字节」Rust 类型（`Vec<u8>` 是 List），故按核心既有
    /// 规则（字段/元素体就是原始字节记录）手写两个入口，仅用于本模块的字节对照。
    struct Raw(Vec<u8>);

    impl AceEncode for Raw {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.encode_record(&self.0)
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.begin_record()?;
            e.write_body_bytes(&self.0)?;
            let id = e.end_record()?;
            e.write_element_bytes(&id.to_le_bytes())
        }
    }

    /// 参照枚举：`repr(i8)` + 显式默认变体 Red。四个入口逐句对应 derive 生成的代码。
    #[derive(Clone, Copy)]
    enum Color {
        Red = 1,
        Green = 2,
    }

    impl Color {
        fn payload(self) -> [u8; 8] {
            ((self as i8 as i64) as u64).to_le_bytes()
        }
    }

    impl AceEncode for Color {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            if e.current_order().is_some() && self.payload() == Color::Red.payload() {
                return e.encode_none();
            }
            e.encode_scalar(self.payload())
        }
        fn ace_encode_some(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.encode_scalar(self.payload())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }

    impl AceElement for Color {
        fn element_kind() -> ElementKind {
            ElementKind::Value { size: 1 }
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.write_element_bytes(&self.payload()[..1])
        }
        fn decode_element(_: &mut Decoder<'_>) -> AceResult<Self> {
            Err(Error::Unsupported)
        }
    }

    impl NonOptional for Color {}

    struct Inner {
        x: i64,
        label: String,
    }

    impl Inner {
        fn encode_fields(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            if !value::is_default_i64(self.x) {
                e.begin_field(0)?;
                self.x.ace_encode(e)?;
                e.check_field_consumed()?;
            }
            e.begin_field(1)?;
            self.label.ace_encode(e)?;
            e.check_field_consumed()
        }
    }

    impl AceEncode for Inner {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.enter_object()?;
            let encoded = self.encode_fields(e);
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.enter_object()?;
            let encoded = self.encode_fields(e);
            let left = e.leave_object();
            encoded?;
            let id = left?;
            e.write_element_bytes(&id.to_le_bytes())
        }
    }

    /// `demo.model` 的参照值：字段 0..8 与 `model_schema` 逐一对应。
    struct Model {
        id: i32,
        name: String,
        blob: Raw,
        values: Vec<i32>,
        nick: Option<String>,
        count: Option<i32>,
        tags: BTreeMap<String, i32>,
        color: Color,
        inner: Inner,
    }

    impl Model {
        fn encode_fields(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            if !value::is_default_i32(self.id) {
                e.begin_field(0)?;
                self.id.ace_encode(e)?;
                e.check_field_consumed()?;
            }
            e.begin_field(1)?;
            self.name.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(2)?;
            self.blob.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(3)?;
            self.values.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(4)?;
            self.nick.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(5)?;
            self.count.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(6)?;
            self.tags.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(7)?;
            self.color.ace_encode(e)?;
            e.check_field_consumed()?;
            e.begin_field(8)?;
            self.inner.ace_encode(e)?;
            e.check_field_consumed()
        }
    }

    impl AceEncode for Model {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.enter_object()?;
            let encoded = self.encode_fields(e);
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.enter_object()?;
            let encoded = self.encode_fields(e);
            let left = e.leave_object();
            encoded?;
            let id = left?;
            e.write_element_bytes(&id.to_le_bytes())
        }
    }

    fn model(color: Color) -> Model {
        let mut tags = BTreeMap::new();
        // 与动态侧同一插入次序（b 先、a 后）：`BTreeMap` 自身按键序，线上必须是 a、b。
        tags.insert("b".to_string(), 2);
        tags.insert("a".to_string(), 1);
        Model {
            id: 7,
            name: "模型".to_string(),
            blob: Raw(vec![0x00, 0xFF, 0x10]),
            values: vec![1, -2, 3],
            nick: None,
            count: Some(0),
            tags,
            color,
            inner: Inner {
                x: -5,
                label: "内".to_string(),
            },
        }
    }

    fn inner_schema() -> *mut AceSchema {
        let schema = schema_with(c"demo.inner".as_ptr());
        assert_eq!(ace_schema_add_field(schema, 0, prim(ACE_FK_I64)), ACE_OK);
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 1, text), ACE_OK);
        schema
    }

    fn color_schema() -> *mut AceEnumSchema {
        let color = enum_with(c"demo.color".as_ptr());
        assert_eq!(ace_enum_set_repr(color, ACE_FK_I8), ACE_OK);
        assert_eq!(ace_enum_add_variant(color, 1), ACE_OK); // Red
        assert_eq!(ace_enum_add_variant(color, 2), ACE_OK); // Green
        assert_eq!(ace_enum_set_default_variant(color, 1), ACE_OK);
        color
    }

    /// `demo.model` 的 schema：0 i32、1 str、2 bytes、3 list<i32>、4 option<str>、
    /// 5 option<i32>、6 map<str,i32>、7 enum(i8)、8 obj<demo.inner>。
    fn model_schema(color: *mut AceEnumSchema, inner: *mut AceSchema) -> *mut AceSchema {
        fn add(schema: *mut AceSchema, field_id: i32, field: *mut AceTypeRef) {
            assert_eq!(ace_schema_add_field(schema, field_id, field), ACE_OK);
        }
        let schema = schema_with(c"demo.model".as_ptr());
        add(schema, 0, prim(ACE_FK_I32));
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        add(schema, 1, text);
        let mut bytes = ptr::null_mut();
        assert_eq!(ace_type_bytes(&mut bytes), ACE_OK);
        add(schema, 2, bytes);
        let mut numbers = ptr::null_mut();
        assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut numbers), ACE_OK);
        add(schema, 3, numbers);
        let mut nick = ptr::null_mut();
        assert_eq!(ace_type_str(&mut nick), ACE_OK);
        let mut option_nick = ptr::null_mut();
        assert_eq!(ace_type_option(nick, &mut option_nick), ACE_OK);
        add(schema, 4, option_nick);
        let mut count = ptr::null_mut();
        assert_eq!(ace_type_option(prim(ACE_FK_I32), &mut count), ACE_OK);
        add(schema, 5, count);
        let mut key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut key), ACE_OK);
        let mut tags = ptr::null_mut();
        assert_eq!(ace_type_map(key, prim(ACE_FK_I32), &mut tags), ACE_OK);
        add(schema, 6, tags);
        let mut color_type = ptr::null_mut();
        assert_eq!(ace_type_enum(color, &mut color_type), ACE_OK);
        add(schema, 7, color_type);
        let mut inner_type = ptr::null_mut();
        assert_eq!(ace_type_object(inner, &mut inner_type), ACE_OK);
        add(schema, 8, inner_type);
        schema
    }

    /// 编码一次；成功时归还缓冲并返回字节，失败时断言 `out` 已清空并返回空字节。
    fn serialize_with(
        options: *const AceOptions,
        type_key: Option<&str>,
        value: *const AceValue,
    ) -> (i32, Vec<u8>) {
        let key = type_key.map(|key| std::ffi::CString::new(key).expect("type_key 无内嵌 NUL"));
        let mut buffer = AceBuffer::empty();
        let code = ace_serialize(
            key.as_ref().map_or(ptr::null(), |key| key.as_ptr()),
            value,
            options,
            &mut buffer,
        );
        if code != ACE_OK {
            assert!(buffer.ptr.is_null(), "失败必须清空 ptr");
            assert_eq!((buffer.len, buffer.cap), (0, 0), "失败必须清空 len/cap");
            return (code, Vec::new());
        }
        assert!(!buffer.ptr.is_null());
        assert!(buffer.len > 0);
        assert_eq!(buffer.cap, buffer.len, "cap 口径为实际长度");
        let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) }.to_vec();
        assert_eq!(ace_buffer_free(&mut buffer), ACE_OK);
        (code, bytes)
    }

    #[test]
    fn serialize_matches_core_encode_byte_for_byte() {
        let options = options();
        let color = color_schema();
        let inner = inner_schema();
        let schema = model_schema(color, inner);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        // 参照编码与 `ace_serialize` 用同一份限额副本，差异只可能来自编码路径本身。
        let limits = unsafe { (*options).limits.clone() };

        for (tag, reference) in [(2u64, Color::Green), (1, Color::Red)] {
            let value = model_value(tag);
            let (code, bytes) = serialize_with(options, Some("demo.model"), value);
            assert_eq!(code, ACE_OK);
            let expected = ace_serialize::encode(&model(reference), &limits).expect("参照编码");
            assert_eq!(bytes, expected, "color tag = {tag}");
            free(value);
        }

        // 标量字段取默认值（id=0）与枚举默认变体（Red）都省略槽：仍与参照逐字节一致。
        let value = model_value(1);
        set_owned(value, 0, i64_value(0));
        let mut reference = model(Color::Red);
        reference.id = 0;
        let (code, bytes) = serialize_with(options, Some("demo.model"), value);
        assert_eq!(code, ACE_OK);
        assert_eq!(bytes, ace_serialize::encode(&reference, &limits).expect("参照编码"));
        free(value);

        // Option 的 presence 规则：显式 Null 与字段缺席等价，都不写槽。
        let absent = model_value(2);
        let explicit = model_value(2);
        let mut null = ptr::null_mut();
        assert_eq!(ace_value_null(&mut null), ACE_OK);
        set_owned(explicit, 4, null);
        let (code, from_absent) = serialize_with(options, Some("demo.model"), absent);
        assert_eq!(code, ACE_OK);
        let (code, from_null) = serialize_with(options, Some("demo.model"), explicit);
        assert_eq!(code, ACE_OK);
        assert_eq!(from_absent, from_null);
        free(absent);
        free(explicit);

        // 释放顺序：先 free 引用方 schema，再 free 被借用的 inner schema 与 enum。
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(inner), ACE_OK);
        assert_eq!(ace_enum_free(color), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    // ——— 共享身份与强环（R32） ———

    /// 未压缩信封（`packed=false`）的两个区：`(main, assist)`。
    fn unpacked_regions(bytes: &[u8]) -> (&[u8], &[u8]) {
        let main_len =
            i32::from_le_bytes(bytes[..4].try_into().expect("main 长度")) as usize;
        let main = &bytes[4..4 + main_len];
        let assist_start = 4 + main_len + 4;
        let assist_len = i32::from_le_bytes(
            bytes[4 + main_len..assist_start]
                .try_into()
                .expect("assist 长度"),
        ) as usize;
        (main, &bytes[assist_start..assist_start + assist_len])
    }

    /// 根槽载荷里的记录 id。记录 id 从 1 起顺序分配、根记录最后写出，故它也是本次的记录条数。
    fn record_count(assist: &[u8]) -> u64 {
        u64::from_le_bytes(assist[8..16].try_into().expect("根槽载荷"))
    }

    /// `demo.child`：0 → i32。
    fn child_schema() -> *mut AceSchema {
        let schema = schema_with(c"demo.child".as_ptr());
        assert_eq!(ace_schema_add_field(schema, 0, prim(ACE_FK_I32)), ACE_OK);
        schema
    }

    /// `demo.parent`：0、1 两个字段都是 `obj<child>`（共享身份的两个引用位置）。
    fn parent_schema(child: *mut AceSchema) -> *mut AceSchema {
        let schema = schema_with(c"demo.parent".as_ptr());
        for field_id in [0, 1] {
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_object(child, &mut node), ACE_OK);
            assert_eq!(ace_schema_add_field(schema, field_id, node), ACE_OK);
        }
        schema
    }

    #[test]
    fn shared_dag_writes_one_record_and_reuses_the_reference_id() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        let child_type = child_schema();
        let schema = parent_schema(child_type);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 共享 DAG：parent{0: child, 1: share(child)}——两个字段指向同一节点。
        let child = object();
        set_owned(child, 0, i64_value(7));
        let parent = object();
        set(parent, 0, child);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(child, &mut shared), ACE_OK);
        assert_eq!(shared, child);
        set(parent, 1, shared);

        let (code, bytes) = serialize_with(options, Some("demo.parent"), parent);
        assert_eq!(code, ACE_OK);
        let (_, assist) = unpacked_regions(&bytes);
        // assist = 根槽（16B）+ child 块（1 个字段槽 16B）+ parent 块（2 个字段槽 32B）。
        assert_eq!(assist.len(), 16 + 16 + 32);
        // 同一节点只写一条 record：共享图共 2 条（parent + child）。
        assert_eq!(record_count(assist), 2);
        // parent 两个字段槽的载荷是同一个引用 id（第二次出现写引用，不新建记录）。
        assert_eq!(&assist[40..48], &assist[56..64]);
        assert_eq!(&assist[40..48], &1u64.to_le_bytes());

        // 对照组：两个内容相同但是不同节点 → 各写一条 record，共 3 条，字节也更多。
        let first = object();
        set_owned(first, 0, i64_value(7));
        let second = object();
        set_owned(second, 0, i64_value(7));
        let duplicated = object();
        set(duplicated, 0, first);
        set(duplicated, 1, second);
        let (code, other) = serialize_with(options, Some("demo.parent"), duplicated);
        assert_eq!(code, ACE_OK);
        let (_, other_assist) = unpacked_regions(&other);
        assert_eq!(record_count(other_assist), 3);
        assert_ne!(&other_assist[40..48], &other_assist[56..64]);
        assert!(bytes.len() < other.len());

        // 引用计数：调用方自己那份与各父引用都归零后节点才释放。
        free(shared);
        free(child);
        free(parent);
        free(first);
        free(second);
        free(duplicated);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(child_type), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn strong_cycle_returns_14_without_partial_output() {
        let options = options();
        let child_type = child_schema();
        let schema = parent_schema(child_type);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 对象引用自身：构造成功（强环是调用方使用禁令），编码返 14、不挂起、不撞栈。
        let parent = object();
        let child = object();
        set_owned(child, 0, i64_value(1));
        set(parent, 0, child);
        set(parent, 1, parent);
        assert_eq!(serialize_with(options, Some("demo.parent"), parent).0, 14);

        // 间接环 parent → child → parent 同样 14。
        let parent = object();
        let child = object();
        set(parent, 0, child);
        set(child, 0, parent);
        assert_eq!(serialize_with(options, Some("demo.parent"), parent).0, 14);

        // 经列表的环（列表含自身）也必须 14，而不是递归展开后撞栈。
        let parent = object();
        let child = object();
        set(parent, 0, child);
        let cyclic_list = list();
        assert_eq!(ace_list_add(cyclic_list, cyclic_list), ACE_OK);
        set(child, 0, cyclic_list);
        assert_eq!(serialize_with(options, Some("demo.parent"), parent).0, 14);

        // 强环按引用计数语义不保证释放，故这里不 free 环内节点（测试进程结束时丢弃）。
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(child_type), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn shared_node_under_incompatible_schema_is_type_mismatch() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        let schema = schema_with(c"demo.mixed".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, text), ACE_OK);
        let mut raw = ptr::null_mut();
        assert_eq!(ace_type_bytes(&mut raw), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 1, raw), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 同一节点先按 str 写出记录，再按 bytes 复用 → 13 TypeMismatch，不另写一条 record 规避。
        let node = str_value(b"shared");
        let value = object();
        set(value, 0, node);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(node, &mut shared), ACE_OK);
        set(value, 1, shared);
        assert_eq!(serialize_with(options, Some("demo.mixed"), value).0, 13);
        free(shared);
        free(node);
        free(value);

        // 对照：两个形状相同的 str 类型节点不算不兼容，共享仍只写一条 record。
        let same_shape = schema_with(c"demo.text".as_ptr());
        for field_id in [0, 1] {
            let mut field = ptr::null_mut();
            assert_eq!(ace_type_str(&mut field), ACE_OK);
            assert_eq!(ace_schema_add_field(same_shape, field_id, field), ACE_OK);
        }
        assert_eq!(ace_options_add_schema(options, same_shape), ACE_OK);
        let node = str_value(b"same");
        let value = object();
        set(value, 0, node);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(node, &mut shared), ACE_OK);
        set(value, 1, shared);
        let (code, bytes) = serialize_with(options, Some("demo.text"), value);
        assert_eq!(code, ACE_OK);
        let (_, assist) = unpacked_regions(&bytes);
        // assist = 根槽（16B）+ 根对象块的两个字段槽（32B）。
        assert_eq!(assist.len(), 16 + 32);
        // 共 2 条 record：共享的 str 一条 + 根对象一条，两个字段槽指向同一个引用 id。
        assert_eq!(record_count(assist), 2);
        assert_eq!(&assist[24..32], &1u64.to_le_bytes());
        assert_eq!(&assist[40..48], &1u64.to_le_bytes());
        free(shared);
        free(node);
        free(value);
        assert_eq!(ace_schema_free(same_shape), ACE_OK);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn shared_object_under_compatible_schema_shapes_reuses_the_reference_id() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        // 两份字段形状相同、但节点与 type_key 都不同的对象 schema：形状兼容即共享（P1c 对照）。
        let left = schema_with(c"demo.left".as_ptr());
        assert_eq!(ace_schema_add_field(left, 0, prim(ACE_FK_I32)), ACE_OK);
        let right = schema_with(c"demo.right".as_ptr());
        assert_eq!(ace_schema_add_field(right, 0, prim(ACE_FK_I32)), ACE_OK);
        let schema = schema_with(c"demo.pair".as_ptr());
        for (field_id, target) in [(0, left), (1, right)] {
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_object(target, &mut node), ACE_OK);
            assert_eq!(ace_schema_add_field(schema, field_id, node), ACE_OK);
        }
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 同一对象共享到两个 obj 字段：两次到达的 schema 节点不同但形状全等，不得误拒。
        let child = object();
        set_owned(child, 0, i64_value(7));
        let value = object();
        set(value, 0, child);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(child, &mut shared), ACE_OK);
        assert_eq!(shared, child);
        set(value, 1, shared);

        let (code, bytes) = serialize_with(options, Some("demo.pair"), value);
        assert_eq!(code, ACE_OK);
        let (_, assist) = unpacked_regions(&bytes);
        // assist = 根槽（16B）+ child 块（1 个字段槽 16B）+ 根对象块（2 个字段槽 32B）。
        assert_eq!(assist.len(), 16 + 16 + 32);
        // 兼容形状共享只写一条 record：child 一条 + 根对象一条，两字段槽指向同一引用 id。
        assert_eq!(record_count(assist), 2);
        assert_eq!(&assist[40..48], &assist[56..64]);
        assert_eq!(&assist[40..48], &1u64.to_le_bytes());

        free(shared);
        free(child);
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(right), ACE_OK);
        assert_eq!(ace_schema_free(left), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn shared_object_under_incompatible_schema_shape_is_type_mismatch() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        // 两份字段类型不同的对象 schema：同一节点第二次到达时不得跳过字段类型校验（P1c）。
        let wants_i32 = schema_with(c"demo.want_i32".as_ptr());
        assert_eq!(ace_schema_add_field(wants_i32, 0, prim(ACE_FK_I32)), ACE_OK);
        let wants_str = schema_with(c"demo.want_str".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(wants_str, 0, text), ACE_OK);
        let schema = schema_with(c"demo.clash".as_ptr());
        for (field_id, target) in [(0, wants_i32), (1, wants_str)] {
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_object(target, &mut node), ACE_OK);
            assert_eq!(ace_schema_add_field(schema, field_id, node), ACE_OK);
        }
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // {0: I64(7)} 放进 {0:i32} 字段合法；再共享到 {0:str} 字段必须 13，不得复用引用 id 蒙过去。
        let child = object();
        set_owned(child, 0, i64_value(7));
        let value = object();
        set(value, 0, child);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(child, &mut shared), ACE_OK);
        set(value, 1, shared);
        assert_eq!(serialize_with(options, Some("demo.clash"), value).0, 13);

        free(shared);
        free(child);
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(wants_str), ACE_OK);
        assert_eq!(ace_schema_free(wants_i32), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    // ——— list/map 的共享身份（R32②） ———

    /// 未压缩信封的 main 记录表：`(record id, body)`，按落盘次序；body 借用 `main`。
    fn records(main: &[u8]) -> Vec<(u32, &[u8])> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < main.len() {
            let id = u32::from_le_bytes(main[offset..offset + 4].try_into().expect("record id"));
            let size = i32::from_le_bytes(
                main[offset + 4..offset + 8]
                    .try_into()
                    .expect("record size"),
            ) as usize;
            out.push((id, &main[offset + 8..offset + 8 + size]));
            offset += 8 + size;
        }
        out
    }

    /// 引用 id 必须指向已写记录。
    fn record_body<'a>(main: &'a [u8], id: u32) -> &'a [u8] {
        let body = records(main)
            .into_iter()
            .find(|(record, _)| *record == id)
            .map(|(_, body)| body);
        body.expect("引用 id 必须指向已写记录")
    }

    /// Map 记录体的键值序列：键 id 解析为文本记录体，值按 i32 读。
    fn map_pairs(main: &[u8], body: &[u8]) -> Vec<(Vec<u8>, i32)> {
        let count = u32::from_le_bytes(body[..4].try_into().expect("map count")) as usize;
        let mut out = Vec::new();
        for index in 0..count {
            let at = 4 + index * 8;
            let key_id = u32::from_le_bytes(body[at..at + 4].try_into().expect("map key id"));
            let value = i32::from_le_bytes(body[at + 4..at + 8].try_into().expect("map value"));
            out.push((record_body(main, key_id).to_vec(), value));
        }
        out
    }

    fn list_value(numbers: &[i64]) -> *mut AceValue {
        let value = list();
        for number in numbers {
            add_owned(value, i64_value(*number));
        }
        value
    }

    fn text_list_value(items: &[&str]) -> *mut AceValue {
        let value = list();
        for item in items {
            add_owned(value, str_value(item.as_bytes()));
        }
        value
    }

    /// `demo.lists`：0、1 两个字段都是独立的 `list<i32>` 类型节点。
    fn shared_list_schema(key: *const c_char) -> *mut AceSchema {
        let schema = schema_with(key);
        for field_id in [0, 1] {
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut node), ACE_OK);
            assert_eq!(ace_schema_add_field(schema, field_id, node), ACE_OK);
        }
        schema
    }

    /// `demo.maps`：0、1 两个字段都是独立的 `map<str,i32>` 类型节点。
    fn shared_map_schema(key: *const c_char) -> *mut AceSchema {
        let schema = schema_with(key);
        for field_id in [0, 1] {
            let mut map_key = ptr::null_mut();
            assert_eq!(ace_type_str(&mut map_key), ACE_OK);
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_map(map_key, prim(ACE_FK_I32), &mut node), ACE_OK);
            assert_eq!(ace_schema_add_field(schema, field_id, node), ACE_OK);
        }
        schema
    }

    /// 两个字段共享同一 `Rc<Vec<i32>>` 的参照值：与 FFI 的共享 list 同值。
    struct SharedNumbers {
        left: Rc<Vec<i32>>,
        right: Rc<Vec<i32>>,
    }

    impl AceEncode for SharedNumbers {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.enter_object()?;
            let encoded = (|| {
                e.begin_field(0)?;
                self.left.ace_encode(e)?;
                e.check_field_consumed()?;
                e.begin_field(1)?;
                self.right.ace_encode(e)?;
                e.check_field_consumed()
            })();
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }

    /// 两个字段共享同一 `Rc<BTreeMap<String, i32>>` 的参照值。
    struct SharedTags {
        left: Rc<BTreeMap<String, i32>>,
        right: Rc<BTreeMap<String, i32>>,
    }

    impl AceEncode for SharedTags {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.enter_object()?;
            let encoded = (|| {
                e.begin_field(0)?;
                self.left.ace_encode(e)?;
                e.check_field_consumed()?;
                e.begin_field(1)?;
                self.right.ace_encode(e)?;
                e.check_field_consumed()
            })();
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }

    /// `demo.names` 的参照值：`list<str>` 的非共享逐字节对照。
    struct Names {
        names: Vec<String>,
    }

    impl AceEncode for Names {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> AceResult<()> {
            e.enter_object()?;
            let encoded = (|| {
                e.begin_field(0)?;
                self.names.ace_encode(e)?;
                e.check_field_consumed()
            })();
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }

    #[test]
    fn shared_list_between_fields_writes_one_record() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        let schema = shared_list_schema(c"demo.lists".as_ptr());
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        let limits = unsafe { (*options).limits.clone() };

        // 共享 DAG：两个字段指向同一 list 节点 [1, 2]。
        let items = list_value(&[1, 2]);
        let value = object();
        set(value, 0, items);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(items, &mut shared), ACE_OK);
        assert_eq!(shared, items);
        set(value, 1, shared);

        let (code, bytes) = serialize_with(options, Some("demo.lists"), value);
        assert_eq!(code, ACE_OK);
        let (main, assist) = unpacked_regions(&bytes);
        // 共享同一 list 节点：list 记录一条 + 根对象一条。
        assert_eq!(records(main).len(), 2);
        assert_eq!(record_count(assist), 2);
        // 根对象块的两个字段槽是同一个引用 id，且它就是 list 记录。
        assert_eq!(&assist[24..32], &assist[40..48]);
        let list_id = u32::from_le_bytes(assist[24..28].try_into().expect("引用 id"));
        // 该 list 记录体与核心 `Vec<i32>` 的集合体逐字节相同（id 编号与本对照无关）。
        let list_body = [2u8, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!(record_body(main, list_id), list_body);
        let numbers = Rc::new(vec![1, 2]);
        let reference = SharedNumbers {
            left: Rc::clone(&numbers),
            right: Rc::clone(&numbers),
        };
        let expected = ace_serialize::encode(&reference, &limits).expect("参照编码");
        let (expected_main, _) = unpacked_regions(&expected);
        assert_eq!(records(expected_main).len(), 2);
        let expected_list = records(expected_main)
            .into_iter()
            .find(|(_, body)| *body == list_body)
            .expect("核心的 list 记录体");
        assert_eq!(record_body(main, list_id), expected_list.1);

        // 对照组：内容相同但实例不同 → 两个 list 各写一条记录。
        let first = list_value(&[1, 2]);
        let second = list_value(&[1, 2]);
        let duplicated = object();
        set(duplicated, 0, first);
        set(duplicated, 1, second);
        let (code, other) = serialize_with(options, Some("demo.lists"), duplicated);
        assert_eq!(code, ACE_OK);
        let (other_main, other_assist) = unpacked_regions(&other);
        assert_eq!(records(other_main).len(), 3);
        assert_eq!(record_count(other_assist), 3);
        assert_ne!(&other_assist[24..32], &other_assist[40..48]);
        assert!(bytes.len() < other.len());

        free(shared);
        free(items);
        free(value);
        free(first);
        free(second);
        free(duplicated);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn shared_map_between_fields_writes_one_record() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        let schema = shared_map_schema(c"demo.maps".as_ptr());
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        let limits = unsafe { (*options).limits.clone() };

        // 共享 DAG：两个字段指向同一 Map ABI 节点。
        let entries = map_value(&[("b", 2), ("a", 1)]);
        let value = object();
        set(value, 0, entries);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(entries, &mut shared), ACE_OK);
        assert_eq!(shared, entries);
        set(value, 1, shared);

        let (code, bytes) = serialize_with(options, Some("demo.maps"), value);
        assert_eq!(code, ACE_OK);
        let (main, assist) = unpacked_regions(&bytes);
        // 共享同一 Map 节点：两个键记录 + map 记录一条 + 根对象一条。
        assert_eq!(records(main).len(), 4);
        assert_eq!(record_count(assist), 4);
        assert_eq!(&assist[24..32], &assist[40..48]);
        let map_id = u32::from_le_bytes(assist[24..28].try_into().expect("引用 id"));
        let map_body = record_body(main, map_id);
        assert_eq!(map_pairs(main, map_body), vec![(b"a".to_vec(), 1), (b"b".to_vec(), 2)]);

        // 核心 `Rc<BTreeMap>` 参照：同样的记录条数与键值序列（id 编号差异允许）。
        let mut tags = BTreeMap::new();
        tags.insert("b".to_string(), 2);
        tags.insert("a".to_string(), 1);
        let tags = Rc::new(tags);
        let reference = SharedTags {
            left: Rc::clone(&tags),
            right: Rc::clone(&tags),
        };
        let expected = ace_serialize::encode(&reference, &limits).expect("参照编码");
        let (expected_main, _) = unpacked_regions(&expected);
        assert_eq!(records(expected_main).len(), 4);
        let core_map = records(expected_main)
            .into_iter()
            .find(|(_, body)| body.len() == map_body.len())
            .expect("核心的 map 记录");
        assert_eq!(map_pairs(main, map_body), map_pairs(expected_main, core_map.1));

        // 对照组：内容相同但实例不同 → 两个 Map 各写一条记录（7 条）。
        let first = map_value(&[("b", 2), ("a", 1)]);
        let second = map_value(&[("b", 2), ("a", 1)]);
        let duplicated = object();
        set(duplicated, 0, first);
        set(duplicated, 1, second);
        let (code, other) = serialize_with(options, Some("demo.maps"), duplicated);
        assert_eq!(code, ACE_OK);
        let (other_main, other_assist) = unpacked_regions(&other);
        assert_eq!(records(other_main).len(), 7);
        assert_eq!(record_count(other_assist), 7);
        assert_ne!(&other_assist[24..32], &other_assist[40..48]);
        assert!(bytes.len() < other.len());

        free(shared);
        free(entries);
        free(value);
        free(first);
        free(second);
        free(duplicated);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn map_string_key_budget_is_enforced_on_the_collection_seam() {
        let options = options();
        let schema = shared_map_schema(c"demo.map_budget".as_ptr());
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        // 键预算 1 字节（参数末位）：键 "a" 合法，键 "bb" 超限 → 6 ResourceLimit。
        assert_eq!(
            ace_options_set_limits(options, 256, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1),
            ACE_OK
        );
        let value = object();
        set_owned(value, 0, map_value(&[("a", 1)]));
        set_owned(value, 1, map_value(&[("bb", 2)]));
        assert_eq!(serialize_with(options, Some("demo.map_budget"), value).0, 6);
        free(value);

        // 对照：同值但键都在预算内 → 成功，说明 6 来自键长而非其它限额。
        let value = object();
        set_owned(value, 0, map_value(&[("a", 1)]));
        set_owned(value, 1, map_value(&[("b", 2)]));
        assert_eq!(serialize_with(options, Some("demo.map_budget"), value).0, ACE_OK);
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn shared_list_under_incompatible_element_shape_is_type_mismatch() {
        let options = options();
        let schema = schema_with(c"demo.list_clash".as_ptr());
        let mut numbers = ptr::null_mut();
        assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut numbers), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, numbers), ACE_OK);
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        let mut texts = ptr::null_mut();
        assert_eq!(ace_type_list(text, &mut texts), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 1, texts), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 同一 list 节点先按 list<i32> 写出，再按 list<str> 复用 → 13 TypeMismatch。
        let items = list_value(&[1]);
        let value = object();
        set(value, 0, items);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(items, &mut shared), ACE_OK);
        set(value, 1, shared);
        assert_eq!(serialize_with(options, Some("demo.list_clash"), value).0, 13);

        free(shared);
        free(items);
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn shared_nested_list_shapes_compare_recursively() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        // 0、1 两个字段各有一棵独立的 `list<list<i32>>` 类型节点树：形状全等，共享合法。
        let schema = schema_with(c"demo.nested".as_ptr());
        for field_id in [0, 1] {
            let mut inner = ptr::null_mut();
            assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut inner), ACE_OK);
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_list(inner, &mut node), ACE_OK);
            assert_eq!(ace_schema_add_field(schema, field_id, node), ACE_OK);
        }
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        let outer = list();
        add_owned(outer, list_value(&[1]));
        add_owned(outer, list_value(&[2]));
        let value = object();
        set(value, 0, outer);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(outer, &mut shared), ACE_OK);
        set(value, 1, shared);

        // 外层 list 一条 + 两个内层 list + 根对象 = 4 条记录。
        let (code, bytes) = serialize_with(options, Some("demo.nested"), value);
        assert_eq!(code, ACE_OK);
        let (main, assist) = unpacked_regions(&bytes);
        assert_eq!(records(main).len(), 4);
        assert_eq!(&assist[24..32], &assist[40..48]);

        free(shared);
        free(outer);
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);

        // 嵌套形状不同：list<list<i32>> 的记录体不能复用给 list<list<str>> 字段 → 13。
        let clash = schema_with(c"demo.nested_clash".as_ptr());
        let mut inner_numbers = ptr::null_mut();
        assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut inner_numbers), ACE_OK);
        let mut nested_numbers = ptr::null_mut();
        assert_eq!(ace_type_list(inner_numbers, &mut nested_numbers), ACE_OK);
        assert_eq!(ace_schema_add_field(clash, 0, nested_numbers), ACE_OK);
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        let mut inner_texts = ptr::null_mut();
        assert_eq!(ace_type_list(text, &mut inner_texts), ACE_OK);
        let mut nested_texts = ptr::null_mut();
        assert_eq!(ace_type_list(inner_texts, &mut nested_texts), ACE_OK);
        assert_eq!(ace_schema_add_field(clash, 1, nested_texts), ACE_OK);
        assert_eq!(ace_options_add_schema(options, clash), ACE_OK);

        let outer = list();
        add_owned(outer, list_value(&[1]));
        let value = object();
        set(value, 0, outer);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(outer, &mut shared), ACE_OK);
        set(value, 1, shared);
        assert_eq!(serialize_with(options, Some("demo.nested_clash"), value).0, 13);

        free(shared);
        free(outer);
        free(value);
        assert_eq!(ace_schema_free(clash), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn collection_depth_uses_core_max_depth() {
        let options = options();
        assert_eq!(
            ace_options_set_limits(options, 1, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20),
            ACE_OK
        );
        // 单层 list：根对象 open=1，list 进入前 1 > 1 不成立 → 允许。
        let flat = schema_with(c"demo.flat".as_ptr());
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut node), ACE_OK);
        assert_eq!(ace_schema_add_field(flat, 0, node), ACE_OK);
        assert_eq!(ace_options_add_schema(options, flat), ACE_OK);
        let value = object();
        set_owned(value, 0, list_value(&[1]));
        assert_eq!(serialize_with(options, Some("demo.flat"), value).0, ACE_OK);
        free(value);
        assert_eq!(ace_schema_free(flat), ACE_OK);

        // 嵌套 list：根对象 open=1 → 外层 list open=2 → 内层 list 进入前 open=2 > 1 → 5。
        let nested = schema_with(c"demo.nested_depth".as_ptr());
        let mut inner = ptr::null_mut();
        assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut inner), ACE_OK);
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(inner, &mut node), ACE_OK);
        assert_eq!(ace_schema_add_field(nested, 0, node), ACE_OK);
        assert_eq!(ace_options_add_schema(options, nested), ACE_OK);
        let value = object();
        let outer = list();
        add_owned(outer, list_value(&[1]));
        set_owned(value, 0, outer);
        assert_eq!(serialize_with(options, Some("demo.nested_depth"), value).0, 5);
        free(value);
        assert_eq!(ace_schema_free(nested), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// I2 回归：FFI 自有身份表也受 `max_references` 约束。编码（[`Identities`]）与解码
    /// （[`DynReader`]）两侧都在**首次登记前**按核心判据收费：超限 → `6`，恰达上限 → 成功；
    /// 命中既有身份不重复计（两个字段指向同一节点只占一条）。
    ///
    /// 身份条目数：根对象本身也走身份发射（`Shape::Object`），故「根 + 两个叶子」= 3 条，
    /// 「根 + 共享叶子」= 2 条。
    #[test]
    fn reference_budget_covers_ffi_identity_tables() {
        // 小图：`demo.refs{0: obj<demo.bare>, 1: obj<demo.bare>}`，`demo.bare{0: i32}`；
        // 每个对象节点恰占一条身份（i32 不是身份类节点）。
        let bare = schema_with(c"demo.bare".as_ptr());
        add_field(bare, 0, prim(ACE_FK_I32));
        let refs = schema_with(c"demo.refs".as_ptr());
        add_field(refs, 0, object_type(bare));
        add_field(refs, 1, object_type(bare));
        let options = options();
        assert_eq!(ace_options_add_schema(options, refs), ACE_OK);
        assert_eq!(ace_options_add_schema(options, bare), ACE_OK);

        let bare_value = |number: i64| {
            let value = object();
            set_owned(value, 0, i64_value(number));
            value
        };
        // 根 + 两个不同叶子 = 3 条身份。
        let distinct = || {
            let value = object();
            set_owned(value, 0, bare_value(1));
            set_owned(value, 1, bare_value(2));
            value
        };
        // 根 + 同一节点的两处 = 2 条身份（第二个字段命中既有身份）。
        let shared = || {
            let value = object();
            let node = bare_value(1);
            let mut second = ptr::null_mut();
            assert_eq!(ace_value_share(node, &mut second), ACE_OK);
            set(value, 0, node);
            set(value, 1, second);
            free(node);
            free(second);
            value
        };
        let set_references = |references: usize| {
            let limits = 1 << 20;
            assert_eq!(
                ace_options_set_limits(
                    options, limits, limits, limits, limits, limits, limits, limits, references,
                    limits
                ),
                ACE_OK
            );
        };

        // 编码侧 references = 0：任何新身份都超限 → 6，且失败输出为空。
        set_references(0);
        let value = shared();
        assert_eq!(serialize_with(options, Some("demo.refs"), value).0, 6);
        free(value);

        // 超上限：2 条身份 + references = 1 → 编码侧 6。
        set_references(1);
        let value = shared();
        assert_eq!(serialize_with(options, Some("demo.refs"), value).0, 6);
        free(value);

        // 恰达上限（2 条身份 + references = 2）→ 编码成功；同一份 wire 用同一限额解回。
        set_references(2);
        let value = shared();
        let (code, wire) = serialize_with(options, Some("demo.refs"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (code, decoded) = deserialize_with(options, Some("demo.refs"), &wire);
        assert_eq!(code, ACE_OK);
        free(decoded);
        // 解码侧 references = 0：超限 → 6，且 *out 为空。
        set_references(0);
        assert_eq!(deserialize_with(options, Some("demo.refs"), &wire).0, 6);

        // 超上限：3 条身份 + references = 2 → 编码侧 6。
        set_references(2);
        let value = distinct();
        assert_eq!(serialize_with(options, Some("demo.refs"), value).0, 6);
        free(value);
        // 恰达上限（3 条身份 + references = 3）→ 成功；同一份 wire 在 references = 2 下解码侧 6。
        set_references(3);
        let value = distinct();
        let (code, wire) = serialize_with(options, Some("demo.refs"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        set_references(2);
        assert_eq!(deserialize_with(options, Some("demo.refs"), &wire).0, 6);
        set_references(3);
        let (code, decoded) = deserialize_with(options, Some("demo.refs"), &wire);
        assert_eq!(code, ACE_OK);
        free(decoded);

        assert_eq!(ace_schema_free(refs), ACE_OK);
        assert_eq!(ace_schema_free(bare), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn list_of_strings_matches_core_encode_byte_for_byte() {
        let options = options();
        let schema = schema_with(c"demo.names".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(text, &mut node), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, node), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        let limits = unsafe { (*options).limits.clone() };

        // 非共享 list<str>：与核心 `Vec<String>` 整包逐字节一致（含空串与多字节文本）。
        let value = object();
        set_owned(value, 0, text_list_value(&["a", "", "模型"]));
        let (code, bytes) = serialize_with(options, Some("demo.names"), value);
        assert_eq!(code, ACE_OK);
        let reference = Names {
            names: vec!["a".to_string(), String::new(), "模型".to_string()],
        };
        assert_eq!(
            bytes,
            ace_serialize::encode(&reference, &limits).expect("参照编码")
        );
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn map_entries_are_encoded_in_key_order_regardless_of_insertion_order() {
        let options = options();
        let keyed = schema_with(c"demo.keyed".as_ptr());
        let mut key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut key), ACE_OK);
        let mut tags = ptr::null_mut();
        assert_eq!(ace_type_map(key, prim(ACE_FK_I32), &mut tags), ACE_OK);
        assert_eq!(ace_schema_add_field(keyed, 0, tags), ACE_OK);
        assert_eq!(ace_options_add_schema(options, keyed), ACE_OK);

        let mut encoded = Vec::new();
        for entries in [
            [("b", 2), ("a", 1)],
            [("a", 1), ("b", 2)],
        ] {
            let value = object();
            set_owned(value, 0, map_value(&entries));
            let (code, bytes) = serialize_with(options, Some("demo.keyed"), value);
            assert_eq!(code, ACE_OK);
            free(value);
            encoded.push(bytes);
        }
        assert!(!encoded[0].is_empty());
        assert_eq!(encoded[0], encoded[1], "Map 条目必须按 key 序写出");
        assert_eq!(ace_schema_free(keyed), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn serialize_uses_the_value_owned_type_key_when_argument_is_null() {
        let options = options();
        let schema = schema_with(c"demo.required".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, text), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 值自带 key + 实参 NULL：按值自带 key 解析。
        let mut carried = ptr::null_mut();
        assert_eq!(
            ace_value_object(c"demo.required".as_ptr(), &mut carried),
            ACE_OK
        );
        set_owned(carried, 0, str_value(b"x"));
        let (code, from_value) = serialize_with(options, None, carried);
        assert_eq!(code, ACE_OK);
        free(carried);

        // 实参给同一个 key：两条路径必须产出同一份字节。
        let value = object();
        set_owned(value, 0, str_value(b"x"));
        let (code, from_argument) = serialize_with(options, Some("demo.required"), value);
        assert_eq!(code, ACE_OK);
        assert_eq!(from_value, from_argument);
        free(value);

        // 值与实参 key 冲突 → 1。
        let mut conflicted = ptr::null_mut();
        assert_eq!(ace_value_object(c"demo.other".as_ptr(), &mut conflicted), ACE_OK);
        assert_eq!(
            serialize_with(options, Some("demo.required"), conflicted).0,
            1
        );
        free(conflicted);
        // NULL key 且值不自带 key → 1。
        let untyped = object();
        assert_eq!(serialize_with(options, None, untyped).0, 1);
        free(untyped);
        assert_eq!(serialize_with(options, None, ptr::null()).0, 1);

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn serialize_maps_error_codes() {
        let options = options();
        // 未注册 type_key（含 NULL options 的空白名单）→ 3。
        let untyped = object();
        assert_eq!(serialize_with(options, Some("demo.missing"), untyped).0, 3);
        assert_eq!(serialize_with(ptr::null(), Some("demo.missing"), untyped).0, 3);
        assert_eq!(serialize_with(options, Some("demo.missing"), untyped).0, 3);
        assert_eq!(ace_serialize(ptr::null(), untyped, options, ptr::null_mut()), 1);
        free(untyped);

        // 13：schema 要 i32，值是 str。
        let mismatch = schema_with(c"demo.mismatch".as_ptr());
        assert_eq!(ace_schema_add_field(mismatch, 0, prim(ACE_FK_I32)), ACE_OK);
        assert_eq!(ace_options_add_schema(options, mismatch), ACE_OK);
        let wrong = object();
        set_owned(wrong, 0, str_value(b"text"));
        assert_eq!(serialize_with(options, Some("demo.mismatch"), wrong).0, 13);
        free(wrong);

        // 15：必写字段（str）缺席。
        let required = schema_with(c"demo.required".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(required, 0, text), ACE_OK);
        assert_eq!(ace_options_add_schema(options, required), ACE_OK);
        let empty = object();
        assert_eq!(serialize_with(options, Some("demo.required"), empty).0, 15);
        free(empty);

        // 12：值里带 schema 未声明的 field_id。
        let extra = object();
        set_owned(extra, 1, str_value(b"x"));
        assert_eq!(serialize_with(options, Some("demo.required"), extra).0, 12);
        free(extra);

        // 16：Map ABI 列表里同一 key 出现两次。
        let keyed = schema_with(c"demo.keyed".as_ptr());
        let mut key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut key), ACE_OK);
        let mut tags = ptr::null_mut();
        assert_eq!(ace_type_map(key, prim(ACE_FK_I32), &mut tags), ACE_OK);
        assert_eq!(ace_schema_add_field(keyed, 0, tags), ACE_OK);
        assert_eq!(ace_options_add_schema(options, keyed), ACE_OK);
        let duplicate = object();
        set_owned(duplicate, 0, map_value(&[("a", 1), ("a", 2)]));
        assert_eq!(serialize_with(options, Some("demo.keyed"), duplicate).0, 16);
        free(duplicate);

        // 4：枚举判别值不在已注册变体集。
        let color = color_schema();
        let inner = inner_schema();
        let schema = model_schema(color, inner);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        let unknown = model_value(9);
        assert_eq!(serialize_with(options, Some("demo.model"), unknown).0, 4);
        free(unknown);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(inner), ACE_OK);
        assert_eq!(ace_enum_free(color), ACE_OK);
        // 本用例注册过、但没走到归还的三个 schema 句柄（P2）。
        assert_eq!(ace_schema_free(mismatch), ACE_OK);
        assert_eq!(ace_schema_free(required), ACE_OK);
        assert_eq!(ace_schema_free(keyed), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// B1 回归：**等价键**（原始动态值不同、解码后同值）必须在**编码侧**报 `16`，不得先写出自己解
    /// 不回的 wire。四类省略面各一例：缺席 vs 显式默认标量、`Option` 的缺席 vs `Null`、枚举缺席
    /// vs 默认变体（同在一个键里）、嵌套 Map 的插入次序；另加一对**多态对象键**（B1 × R33②：归
    /// 一必须按实际编码目标 schema 补默认）。正向对照证明不同有效值的键仍可编码并解回。
    #[test]
    fn serialize_rejects_equivalent_map_keys_by_effective_value() {
        let tag = enum_with(c"demo.eff_tag".as_ptr());
        assert_eq!(ace_enum_set_repr(tag, ACE_FK_I32), ACE_OK);
        assert_eq!(ace_enum_add_variant(tag, 0), ACE_OK);
        assert_eq!(ace_enum_add_variant(tag, 1), ACE_OK);
        assert_eq!(ace_enum_set_default_variant(tag, 0), ACE_OK);

        // key schema：0 bool（标量，默认值不写槽）、1 option<i32>、2 enum（默认变体 0）、
        // 3 map<str,i32>（必写）。
        let key = schema_with(c"demo.eff_key".as_ptr());
        add_field(key, 0, prim(ACE_FK_BOOL));
        let mut option = ptr::null_mut();
        assert_eq!(ace_type_option(prim(ACE_FK_I32), &mut option), ACE_OK);
        add_field(key, 1, option);
        let mut tag_type = ptr::null_mut();
        assert_eq!(ace_type_enum(tag, &mut tag_type), ACE_OK);
        add_field(key, 2, tag_type);
        let mut entries = ptr::null_mut();
        assert_eq!(
            ace_type_map(str_type(), prim(ACE_FK_I32), &mut entries),
            ACE_OK
        );
        add_field(key, 3, entries);

        let mut key_type = ptr::null_mut();
        assert_eq!(ace_type_object(key, &mut key_type), ACE_OK);
        let mut map_type = ptr::null_mut();
        assert_eq!(ace_type_map(key_type, str_type(), &mut map_type), ACE_OK);
        let root = schema_with(c"demo.eff_map".as_ptr());
        add_field(root, 0, map_type);

        let options = options();
        assert_eq!(ace_options_add_schema(options, root), ACE_OK);
        assert_eq!(ace_options_add_schema(options, key), ACE_OK);

        // 键 A：只写必写的嵌套 Map（空）；标量/Option/枚举全缺席。
        let absent = || {
            let value = object();
            set_owned(value, 3, map_value(&[]));
            value
        };
        // 键 B：把三类缺席全写成显式默认值 —— 两者字段体都为空，解码后同值。
        let explicit = || {
            let value = object();
            set_owned(value, 0, bool_value(false));
            set_owned(value, 1, null_value());
            set_owned(value, 2, u64_value(0));
            set_owned(value, 3, map_value(&[]));
            value
        };
        // 嵌套 Map 作键内字段：插入次序不同、条目集相同。
        let nested = |entries: &[(&str, i64)]| {
            let value = object();
            set_owned(value, 3, map_value(entries));
            value
        };

        for (label, left, right) in [
            ("缺席 vs 显式默认", absent(), explicit()),
            (
                "嵌套 Map 插入次序",
                nested(&[("a", 1), ("b", 2)]),
                nested(&[("b", 2), ("a", 1)]),
            ),
        ] {
            let map = list();
            add_owned(map, map_pair(left, str_value(b"left")));
            add_owned(map, map_pair(right, str_value(b"right")));
            let value = object();
            set_owned(value, 0, map);
            assert_eq!(
                serialize_with(options, Some("demo.eff_map"), value).0,
                Error::DuplicateKey.code(),
                "{label}：等价键必须在编码侧报 16"
            );
            free(value);
        }

        // 正向对照：字段 0 取 true 的键与键 A 不同值 → 编码成功，解回两条。
        let distinct = object();
        set_owned(distinct, 0, bool_value(true));
        set_owned(distinct, 3, map_value(&[]));
        let map = list();
        add_owned(map, map_pair(absent(), str_value(b"left")));
        add_owned(map, map_pair(distinct, str_value(b"right")));
        let value = object();
        set_owned(value, 0, map);
        let (code, bytes) = serialize_with(options, Some("demo.eff_map"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (code, decoded) = deserialize_with(options, Some("demo.eff_map"), &bytes);
        assert_eq!(code, ACE_OK);
        let mut len = 0;
        assert_eq!(ace_list_len(field(decoded, 0), &mut len), ACE_OK);
        assert_eq!(len, 2);
        free(decoded);

        assert_eq!(ace_schema_free(root), ACE_OK);
        assert_eq!(ace_schema_free(key), ACE_OK);
        assert_eq!(ace_enum_free(tag), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);

        // B1 × R33②（复审阻断）：位置声明 `obj<Base>`、值自带 `type_key = Derived` 时，实际编码按
        // **注册目标 Derived** 写字段并补默认（`Base = {0: bool}`、`Derived = {0: bool, 1: bool}`）。
        // 空对象与显式 `{1:false}` 在 Derived 下同值（两字段都取默认、都省略）→ 解码后两个键相同，
        // 必须在编码侧报 `16`，不得编码成功再让自己的解码报重复键。
        let base = schema_with(c"demo.eff_poly_base".as_ptr());
        add_field(base, 0, prim(ACE_FK_BOOL));
        let derived = schema_with(c"demo.eff_poly_derived".as_ptr());
        add_field(derived, 0, prim(ACE_FK_BOOL));
        add_field(derived, 1, prim(ACE_FK_BOOL));
        let poly_root = schema_with(c"demo.eff_poly_map".as_ptr());
        let mut poly_entries = ptr::null_mut();
        assert_eq!(
            ace_type_map(object_type(base), str_type(), &mut poly_entries),
            ACE_OK
        );
        add_field(poly_root, 0, poly_entries);

        // 同名的 `options` 局部绑定遮住了 helper，用模块路径显式取。
        let poly_options = self::options();
        assert_eq!(ace_options_add_schema(poly_options, poly_root), ACE_OK);
        assert_eq!(ace_options_add_schema(poly_options, base), ACE_OK);
        assert_eq!(ace_options_add_schema(poly_options, derived), ACE_OK);

        // 键值都自带 Derived 的 `type_key`；`field_one` 为 `None` 时字段 1 缺席（派生默认）。
        let derived_key = |field_one: Option<bool>| {
            let mut value = ptr::null_mut();
            assert_eq!(
                ace_value_object(c"demo.eff_poly_derived".as_ptr(), &mut value),
                ACE_OK
            );
            if let Some(flag) = field_one {
                set_owned(value, 1, bool_value(flag));
            }
            value
        };
        let map = list();
        add_owned(map, map_pair(derived_key(None), str_value(b"left")));
        add_owned(map, map_pair(derived_key(Some(false)), str_value(b"right")));
        let value = object();
        set_owned(value, 0, map);
        assert_eq!(
            serialize_with(poly_options, Some("demo.eff_poly_map"), value).0,
            Error::DuplicateKey.code(),
            "按 Derived 补默认时两个键同值：必须在编码侧报 16"
        );
        free(value);

        assert_eq!(ace_options_free(poly_options), ACE_OK);
        // 先释放容器（它的 key 类型借用了 `base`），再释放被借用的 schema。
        assert_eq!(ace_schema_free(poly_root), ACE_OK);
        assert_eq!(ace_schema_free(base), ACE_OK);
        assert_eq!(ace_schema_free(derived), ACE_OK);
    }

    #[test]
    fn serialize_passes_core_limit_errors_through() {
        let inner = inner_schema();
        let color = color_schema();
        let schema = model_schema(color, inner);
        let reference_limits = Options::default();

        // 5 DepthExceeded：根对象进入时 k=0 合法，内层对象进入时 k=1 > 0。
        let depth = options();
        assert_eq!(ace_options_add_schema(depth, schema), ACE_OK);
        assert_eq!(
            ace_options_set_limits(depth, 0, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20),
            ACE_OK
        );
        let value = model_value(2);
        assert_eq!(serialize_with(depth, Some("demo.model"), value).0, 5);
        free(value);
        assert_eq!(ace_options_free(depth), ACE_OK);

        // 6 ResourceLimit：集合长度上限 0，非空 list 立即被拒（核心 `check_collection_len`）。
        let length = options();
        assert_eq!(ace_options_add_schema(length, schema), ACE_OK);
        assert_eq!(
            ace_options_set_limits(length, 256, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 0, 1 << 20, 1 << 20),
            ACE_OK
        );
        let value = model_value(2);
        assert_eq!(serialize_with(length, Some("demo.model"), value).0, 6);
        free(value);
        assert_eq!(ace_options_free(length), ACE_OK);

        assert_eq!(reference_limits.max_depth, 256);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(inner), ACE_OK);
        assert_eq!(ace_enum_free(color), ACE_OK);
    }

    #[test]
    fn serialize_maps_panic_to_10_and_keeps_out_empty() {
        let options = options();
        let required = schema_with(c"demo.required".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(required, 0, text), ACE_OK);
        assert_eq!(ace_options_add_schema(options, required), ACE_OK);
        let value = object();
        set_owned(value, 0, str_value(b"x"));

        PANIC_IN_SERIALIZE.with(|flag| flag.set(true));
        let mut buffer = AceBuffer::empty();
        let key = std::ffi::CString::new("demo.required").expect("type_key 无内嵌 NUL");
        assert_eq!(
            ace_serialize(key.as_ptr(), value, options, &mut buffer),
            Error::PanicCaught.code()
        );
        assert!(buffer.ptr.is_null());
        assert_eq!((buffer.len, buffer.cap), (0, 0));
        assert_eq!(buffer.ptr, AceBuffer::empty().ptr);

        // 钩子是一次性的：同一输入随后正常成功。
        assert_eq!(serialize_with(options, Some("demo.required"), value).0, ACE_OK);
        free(value);
        assert_eq!(ace_schema_free(required), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn buffer_free_clears_structure_and_is_idempotent() {
        let options = options();
        let required = schema_with(c"demo.required".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(required, 0, text), ACE_OK);
        assert_eq!(ace_options_add_schema(options, required), ACE_OK);
        let value = object();
        set_owned(value, 0, str_value(b"x"));

        let key = std::ffi::CString::new("demo.required").expect("type_key 无内嵌 NUL");
        let mut buffer = AceBuffer::empty();
        assert_eq!(ace_serialize(key.as_ptr(), value, options, &mut buffer), ACE_OK);
        assert!(!buffer.ptr.is_null());
        assert!(buffer.len > 0);
        assert_eq!(buffer.cap, buffer.len);

        assert_eq!(ace_buffer_free(&mut buffer), ACE_OK);
        assert!(buffer.ptr.is_null());
        assert_eq!((buffer.len, buffer.cap), (0, 0));
        // 同一已清零结构二次 free 安全（AC-A02）。
        assert_eq!(ace_buffer_free(&mut buffer), ACE_OK);
        assert!(buffer.ptr.is_null());
        // NULL 视为已释放（同 `ace_value_free`）。
        assert_eq!(ace_buffer_free(ptr::null_mut()), ACE_OK);

        free(value);
        assert_eq!(ace_schema_free(required), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    // ——— 解码与解码侧共享身份（P3-6：R32 解码侧） ———

    /// 反序列化一次；失败时断言 `*out` 已置空（FR-A02）并返回空指针。
    fn deserialize_with(
        options: *const AceOptions,
        type_key: Option<&str>,
        bytes: &[u8],
    ) -> (i32, *mut AceValue) {
        let key = type_key.map(|key| std::ffi::CString::new(key).expect("type_key 无内嵌 NUL"));
        // 哨兵（悬垂但非空、不参与解引用）：入口必须先清空，失败时不得沿用调用方原值。
        let mut value = ptr::NonNull::<AceValue>::dangling().as_ptr();
        let code = ace_deserialize(
            key.as_ref().map_or(ptr::null(), |key| key.as_ptr()),
            bytes.as_ptr(),
            bytes.len(),
            options,
            &mut value,
        );
        if code != ACE_OK {
            assert!(value.is_null(), "失败必须置空 *out");
            return (code, ptr::null_mut());
        }
        assert!(!value.is_null());
        (code, value)
    }

    /// 字段的借用视图（`ace_object_get`）。
    fn field(object: *const AceValue, field_id: i32) -> *const AceValue {
        let mut value: *const AceValue = ptr::null();
        assert_eq!(ace_object_get(object, field_id, &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    fn i64_of(value: *const AceValue) -> i64 {
        let mut number = 0i64;
        assert_eq!(ace_value_get_i64(value, &mut number), ACE_OK);
        number
    }

    /// 列表元素的借用视图（`ace_list_at`）。
    fn list_item(list: *const AceValue, index: usize) -> *const AceValue {
        let mut value: *const AceValue = ptr::null();
        assert_eq!(ace_list_at(list, index, &mut value), ACE_OK);
        assert!(!value.is_null());
        value
    }

    /// 手写 wire 的一只槽：`i32 depth + i32 tag + 8B payload`。
    fn slot(depth: i32, tag: i32, payload: u64) -> Vec<u8> {
        [
            depth.to_le_bytes().as_slice(),
            tag.to_le_bytes().as_slice(),
            payload.to_le_bytes().as_slice(),
        ]
        .concat()
    }

    /// 手写 wire 的一条记录：`i32 id + i32 size + body`。
    fn record(id: i32, body: &[u8]) -> Vec<u8> {
        [
            id.to_le_bytes().as_slice(),
            (body.len() as i32).to_le_bytes().as_slice(),
            body,
        ]
        .concat()
    }

    /// 未压缩信封：`i32 main_len + main + i32 assist_len + assist`。
    fn envelope(main: &[u8], assist: &[u8]) -> Vec<u8> {
        [
            (main.len() as i32).to_le_bytes().as_slice(),
            main,
            (assist.len() as i32).to_le_bytes().as_slice(),
            assist,
        ]
        .concat()
    }

    #[test]
    fn deserialize_round_trips_the_model_value_graph() {
        let options = options();
        let color = color_schema();
        let inner = inner_schema();
        let schema = model_schema(color, inner);
        // 嵌套 schema 也独立注册：解码出的嵌套对象自带 `demo.inner`（R33②），再编码时它同样要
        // 过白名单，故下面的「解码结果再编码」往返要求该 key 在白名单里。
        assert_eq!(ace_options_add_schema(options, inner), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        let value = model_value(2);
        let (code, bytes) = serialize_with(options, Some("demo.model"), value);
        assert_eq!(code, ACE_OK);
        let (code, decoded) = deserialize_with(options, Some("demo.model"), &bytes);
        assert_eq!(code, ACE_OK);

        // 树等价：解码结果再编码出同一份字节（Map 条目按核心键序，故字节就是结构口径）；
        // `type_key` 为 NULL 也能编码，说明解码出的对象自带其 schema 的 key。
        assert_eq!(serialize_with(options, None, decoded), (ACE_OK, bytes.clone()));

        // 逐字段核对种类与值：对象/str/bytes/list/option/enum/map/嵌套对象。
        assert_eq!(kind_of(field(decoded, 0)), 2);
        assert_eq!(i64_of(field(decoded, 0)), 7);
        assert_eq!(kind_of(field(decoded, 1)), 5);
        assert_eq!(text_of(field(decoded, 1)), "模型".as_bytes());
        assert_eq!(kind_of(field(decoded, 2)), 6);
        assert_eq!(bytes_of(field(decoded, 2)), vec![0x00, 0xFF, 0x10]);

        let numbers = field(decoded, 3);
        assert_eq!(kind_of(numbers), 7);
        let mut len = 0usize;
        assert_eq!(ace_list_len(numbers, &mut len), ACE_OK);
        assert_eq!(len, 3);
        for (index, expected) in [1i64, -2, 3].into_iter().enumerate() {
            let mut item: *const AceValue = ptr::null();
            assert_eq!(ace_list_at(numbers, index, &mut item), ACE_OK);
            assert_eq!(i64_of(item), expected);
        }

        // 4 号 option<str> 缺席与显式 Null 等价；5 号 option<i32> 是 Some(0)。
        assert_eq!(kind_of(field(decoded, 4)), 0);
        assert_eq!(kind_of(field(decoded, 5)), 2);
        assert_eq!(i64_of(field(decoded, 5)), 0);

        // 6 号 Map 按 ABI 装成 List<Object{0: key, 1: value}>，条目按键序（a、b）。
        let tags = field(decoded, 6);
        assert_eq!(kind_of(tags), 7);
        assert_eq!(ace_list_len(tags, &mut len), ACE_OK);
        assert_eq!(len, 2);
        for (index, (key, number)) in [("a", 1i64), ("b", 2)].into_iter().enumerate() {
            let mut entry: *const AceValue = ptr::null();
            assert_eq!(ace_list_at(tags, index, &mut entry), ACE_OK);
            assert_eq!(text_of(field(entry, 0)), key.as_bytes());
            assert_eq!(i64_of(field(entry, 1)), number);
        }

        // 7 号枚举（repr i8）以 I64 承载判别值位模式；8 号嵌套对象连自己的 key 一起还原。
        assert_eq!(kind_of(field(decoded, 7)), 2);
        assert_eq!(i64_of(field(decoded, 7)), 2);
        let nested = field(decoded, 8);
        assert_eq!(kind_of(nested), 8);
        assert_eq!(i64_of(field(nested, 0)), -5);
        assert_eq!(text_of(field(nested, 1)), "内".as_bytes());

        free(decoded);
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(inner), ACE_OK);
        assert_eq!(ace_enum_free(color), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn deserialize_shares_identity_within_one_call_only() {
        let options = options();
        // 0、1 两个字段各有一棵独立的 `list<i32>` 类型节点树（形状全等）。
        let lists = shared_list_schema(c"demo.decode_share".as_ptr());
        assert_eq!(ace_options_add_schema(options, lists), ACE_OK);
        // 0、1 两个字段各有一棵独立的 `map<str,i32>` 类型节点树。
        let maps = shared_map_schema(c"demo.decode_map_share".as_ptr());
        assert_eq!(ace_options_add_schema(options, maps), ACE_OK);

        // 共享 list：同一节点被两个字段引用，编码后只有一条 list 记录。
        let items = list_value(&[1, 2]);
        let value = object();
        set(value, 0, items);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(items, &mut shared), ACE_OK);
        set(value, 1, shared);
        free(shared);
        free(items);
        let (code, bytes) = serialize_with(options, Some("demo.decode_share"), value);
        assert_eq!(code, ACE_OK);
        free(value);

        let (code, first) = deserialize_with(options, Some("demo.decode_share"), &bytes);
        assert_eq!(code, ACE_OK);
        // 同一次调用内：两处字段是同一个原生节点（R32③、AC-A05③）。
        assert_eq!(field(first, 0) as usize, field(first, 1) as usize);
        assert_eq!(kind_of(field(first, 0)), 7);
        // 再解一次，两份结果同时存活（AC-A05⑤）。
        let (code, second) = deserialize_with(options, Some("demo.decode_share"), &bytes);
        assert_eq!(code, ACE_OK);
        assert_eq!(field(second, 0) as usize, field(second, 1) as usize);
        assert_ne!(field(first, 0) as usize, field(second, 0) as usize);

        // 共享节点只析构一次：每份结果的根对象 + list + 2 个 i64 = 4 个节点。
        let before = dropped();
        free(first);
        assert_eq!(dropped(), before + 4);
        free(second);
        assert_eq!(dropped(), before + 8);

        // 共享 map：Map 也进身份表（R32②）。
        let tags = map_value(&[("a", 1)]);
        let value = object();
        set(value, 0, tags);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(tags, &mut shared), ACE_OK);
        set(value, 1, shared);
        free(shared);
        free(tags);
        let (code, bytes) = serialize_with(options, Some("demo.decode_map_share"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (code, decoded) = deserialize_with(options, Some("demo.decode_map_share"), &bytes);
        assert_eq!(code, ACE_OK);
        assert_eq!(field(decoded, 0) as usize, field(decoded, 1) as usize);
        assert_eq!(kind_of(field(decoded, 0)), 7);
        free(decoded);

        assert_eq!(ace_schema_free(lists), ACE_OK);
        assert_eq!(ace_schema_free(maps), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn deserialize_shared_objects_keep_pointer_identity() {
        let options = options();
        let child = child_schema();
        // 0、1 两个字段都是 `obj<demo.child>`（各自独立的 Object 节点，指向同一 schema）。
        let schema = schema_with(c"demo.decode_child_share".as_ptr());
        for field_id in [0, 1] {
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_object(child, &mut node), ACE_OK);
            assert_eq!(ace_schema_add_field(schema, field_id, node), ACE_OK);
        }
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        let inner = object();
        set_owned(inner, 0, i64_value(42));
        let value = object();
        set(value, 0, inner);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(inner, &mut shared), ACE_OK);
        set(value, 1, shared);
        free(shared);
        free(inner);
        let (code, bytes) = serialize_with(options, Some("demo.decode_child_share"), value);
        assert_eq!(code, ACE_OK);
        free(value);

        let (code, decoded) = deserialize_with(options, Some("demo.decode_child_share"), &bytes);
        assert_eq!(code, ACE_OK);
        let (left, right) = (field(decoded, 0), field(decoded, 1));
        assert_eq!(left as usize, right as usize);
        assert_eq!(kind_of(left), 8);
        // 共享节点的字段值两处一致（同一节点，自然不会分裂成两份）。
        assert_eq!(i64_of(field(left, 0)), 42);
        assert_eq!(i64_of(field(right, 0)), 42);
        free(decoded);

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(child), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// 元素位置的对象体是 4B 记录 id（不是 8B 槽载荷）：非环 `list<object>` 必须解出原值，
    /// 不得因为读到外层 list 的槽载荷而误报 `14`。
    ///
    /// 元素对象用**零字段** schema，且本条保留为接缝前的最小证据：它只钉「元素位置读的是 4B
    /// 元素体」，不涉及字段槽深度。集合内**带字段**对象的深度由
    /// [`deserialize_reads_object_fields_inside_lists`] 与
    /// [`deserialize_reads_object_fields_inside_maps`] 覆盖（R35 接缝之前那两条会因字段槽
    /// `depth` 差 1 返回 `4`）。
    #[test]
    fn deserialize_reads_object_id_at_element_position() {
        let options = options();
        let empty = schema_with(c"demo.decode_empty".as_ptr());
        let schema = schema_with(c"demo.decode_element_object".as_ptr());
        let mut object_type = ptr::null_mut();
        assert_eq!(ace_type_object(empty, &mut object_type), ACE_OK);
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(object_type, &mut node), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, node), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        let items = list();
        for _ in 0..2 {
            add_owned(items, object());
        }
        let value = object();
        set_owned(value, 0, items);
        let (code, bytes) = serialize_with(options, Some("demo.decode_element_object"), value);
        assert_eq!(code, ACE_OK);
        free(value);

        let (code, decoded) = deserialize_with(options, Some("demo.decode_element_object"), &bytes);
        // 元素位置读 4B 元素体（元素记录 id），而不是外层字段槽的载荷——否则这里会是 14。
        assert_eq!(code, ACE_OK);
        let items = field(decoded, 0);
        assert_eq!(kind_of(items), 7);
        let mut len = 0usize;
        assert_eq!(ace_list_len(items, &mut len), ACE_OK);
        assert_eq!(len, 2);
        for index in 0..len {
            let mut item: *const AceValue = ptr::null();
            assert_eq!(ace_list_at(items, index, &mut item), ACE_OK);
            assert_eq!(kind_of(item), 8);
            // 零字段对象：借出的字段视图不存在（`12`），确认解出来的是对象体而不是错位数据。
            let mut payload: *const AceValue = ptr::null();
            assert_eq!(ace_object_get(item, 0, &mut payload), Error::NotFound.code());
            assert_eq!(Error::NotFound.code(), 12);
        }
        free(decoded);

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(empty), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// 集合内的带字段对象（R35）：核心以 `frames.len() + coll_depth` 校验对象字段槽的 `depth`，
    /// 解码器进入集合层后元素对象的字段深度才与编码侧一致；否则字段槽 `depth` 差 1 → `4`。
    /// 字段位置的 `list` 与元素位置的内层 `list` 共用同一实现，两条路径都须解出原值。
    #[test]
    fn deserialize_reads_object_fields_inside_lists() {
        let options = options();
        let child = child_schema();
        let schema = schema_with(c"demo.decode_list_fields".as_ptr());
        // 字段 0 → `list<obj<child>>`；字段 1 → `list<list<obj<child>>>`（内层 list 在元素位置）。
        let mut object_type = ptr::null_mut();
        assert_eq!(ace_type_object(child, &mut object_type), ACE_OK);
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(object_type, &mut node), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, node), ACE_OK);
        let mut inner = ptr::null_mut();
        assert_eq!(ace_type_object(child, &mut inner), ACE_OK);
        let mut inner_list = ptr::null_mut();
        assert_eq!(ace_type_list(inner, &mut inner_list), ACE_OK);
        let mut outer_list = ptr::null_mut();
        assert_eq!(ace_type_list(inner_list, &mut outer_list), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 1, outer_list), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        let items = list();
        for number in [7i64, -2] {
            let item = object();
            set_owned(item, 0, i64_value(number));
            add_owned(items, item);
        }
        let inner_list = list();
        let inner_item = object();
        set_owned(inner_item, 0, i64_value(11));
        add_owned(inner_list, inner_item);
        let nested = list();
        add_owned(nested, inner_list);
        let value = object();
        set_owned(value, 0, items);
        set_owned(value, 1, nested);
        let (code, bytes) = serialize_with(options, Some("demo.decode_list_fields"), value);
        assert_eq!(code, ACE_OK);
        free(value);

        let (code, decoded) = deserialize_with(options, Some("demo.decode_list_fields"), &bytes);
        // 接缝前这里是 `4`（元素对象字段槽 depth = 3，核心按 k=1 期望 2）；不得是 `14`。
        assert_eq!(code, ACE_OK);
        let items = field(decoded, 0);
        assert_eq!(kind_of(items), 7);
        for (index, expected) in [7i64, -2].into_iter().enumerate() {
            let item = list_item(items, index);
            assert_eq!(kind_of(item), 8);
            assert_eq!(i64_of(field(item, 0)), expected);
        }
        // 元素位置的 list：内层记录 id 读的是 4B 元素体，字段深度仍与字段位置一致。
        let nested = field(decoded, 1);
        let inner_list = list_item(nested, 0);
        assert_eq!(kind_of(inner_list), 7);
        let inner_item = list_item(inner_list, 0);
        assert_eq!(kind_of(inner_item), 8);
        assert_eq!(i64_of(field(inner_item, 0)), 11);
        free(decoded);

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(child), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// Map 值对象同理（R35）：ABI 的 Map 是 `List<Object{0: key, 1: value}>`，值对象带字段时
    /// 同样需要集合层深度；字段位置的 `map` 与元素位置的 `map` 各走一条路径。
    #[test]
    fn deserialize_reads_object_fields_inside_maps() {
        let options = options();
        let child = child_schema();
        let schema = schema_with(c"demo.decode_map_fields".as_ptr());
        // 字段 0 → `map<str, obj<child>>`；字段 1 → `list<map<str, obj<child>>>`（Map 在元素位置）。
        let mut key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut key), ACE_OK);
        let mut value_type = ptr::null_mut();
        assert_eq!(ace_type_object(child, &mut value_type), ACE_OK);
        let mut map_type = ptr::null_mut();
        assert_eq!(ace_type_map(key, value_type, &mut map_type), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, map_type), ACE_OK);
        let mut inner_key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut inner_key), ACE_OK);
        let mut inner_value = ptr::null_mut();
        assert_eq!(ace_type_object(child, &mut inner_value), ACE_OK);
        let mut inner_map = ptr::null_mut();
        assert_eq!(ace_type_map(inner_key, inner_value, &mut inner_map), ACE_OK);
        let mut outer_list = ptr::null_mut();
        assert_eq!(ace_type_list(inner_map, &mut outer_list), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 1, outer_list), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        /// 一对 `Object{0: str key, 1: obj<child>{0: number}}`。
        fn entry(key: &str, number: i64) -> *mut AceValue {
            let entry = object();
            set_owned(entry, 0, str_value(key.as_bytes()));
            let value = object();
            set_owned(value, 0, i64_value(number));
            set_owned(entry, 1, value);
            entry
        }

        let tags = list();
        add_owned(tags, entry("a", 1));
        add_owned(tags, entry("b", 2));
        let nested_tags = list();
        add_owned(nested_tags, entry("k", 21));
        let nested = list();
        add_owned(nested, nested_tags);
        let value = object();
        set_owned(value, 0, tags);
        set_owned(value, 1, nested);
        let (code, bytes) = serialize_with(options, Some("demo.decode_map_fields"), value);
        assert_eq!(code, ACE_OK);
        free(value);

        let (code, decoded) = deserialize_with(options, Some("demo.decode_map_fields"), &bytes);
        // 接缝前这里是 `4`；不得是 `14`。
        assert_eq!(code, ACE_OK);
        let tags = field(decoded, 0);
        assert_eq!(kind_of(tags), 7);
        for (index, (key, number)) in [("a", 1i64), ("b", 2)].into_iter().enumerate() {
            let entry = list_item(tags, index);
            assert_eq!(kind_of(entry), 8);
            assert_eq!(text_of(field(entry, 0)), key.as_bytes());
            let expected = field(entry, 1);
            assert_eq!(kind_of(expected), 8);
            assert_eq!(i64_of(field(expected, 0)), number);
        }
        // 元素位置的 Map：键值对的深度与字段位置一致。
        let nested = field(decoded, 1);
        let nested_tags = list_item(nested, 0);
        assert_eq!(kind_of(nested_tags), 7);
        let entry = list_item(nested_tags, 0);
        assert_eq!(text_of(field(entry, 0)), b"k");
        assert_eq!(i64_of(field(field(entry, 1), 0)), 21);
        free(decoded);

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_schema_free(child), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// 同一 `str` 记录先作普通字段、再作 Map 键复用：命中 [`Identity::Done`] 也要按
    /// `max_map_string_key_bytes` 报 `6`（首次物化与复用同一上限）。
    #[test]
    fn deserialize_reused_map_key_still_checks_key_budget() {
        /// 字段 0 是 `str`、字段 1 是只含一对的 `map<str,i32>`；键与字段值可指向同一节点。
        fn budget_value(field_text: *mut AceValue, entry_key: *mut AceValue) -> *mut AceValue {
            let entries = list();
            let entry = object();
            set_owned(entry, 0, entry_key);
            set_owned(entry, 1, i64_value(1));
            add_owned(entries, entry);
            let value = object();
            set_owned(value, 0, field_text);
            set_owned(value, 1, entries);
            value
        }

        let options = options();
        let schema = schema_with(c"demo.decode_key_budget".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, text), ACE_OK);
        let mut key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut key), ACE_OK);
        let mut tags = ptr::null_mut();
        assert_eq!(ace_type_map(key, prim(ACE_FK_I32), &mut tags), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 1, tags), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // 共享键：字段 0 与 Map 键是同一节点，wire 上共用一条记录（6 字节 "abcdef"）。
        let shared = str_value(b"abcdef");
        let mut shared_key = ptr::null_mut();
        assert_eq!(ace_value_share(shared, &mut shared_key), ACE_OK);
        let value = budget_value(shared, shared_key);
        let (code, shared_bytes) = serialize_with(options, Some("demo.decode_key_budget"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        // 非共享键（两个独立节点、同文本）：走首次物化那条路。
        let value = budget_value(str_value(b"abcdef"), str_value(b"abcdef"));
        let (code, fresh_bytes) = serialize_with(options, Some("demo.decode_key_budget"), value);
        assert_eq!(code, ACE_OK);
        free(value);

        // 编码用默认预算（1 MiB）；解码前把 Map 键预算收到 6：恰好放下，说明 6 来自预算而非共享。
        for budget in [6usize, 3] {
            assert_eq!(
                ace_options_set_limits(
                    options,
                    256,
                    1 << 20,
                    1 << 20,
                    1 << 20,
                    1 << 20,
                    1 << 20,
                    16,
                    1 << 20,
                    budget
                ),
                ACE_OK
            );
            let expected = if budget == 6 {
                ACE_OK
            } else {
                Error::ResourceLimit.code()
            };
            assert_eq!(
                deserialize_with(options, Some("demo.decode_key_budget"), &shared_bytes).0,
                expected
            );
            assert_eq!(
                deserialize_with(options, Some("demo.decode_key_budget"), &fresh_bytes).0,
                expected
            );
        }
        assert_eq!(Error::ResourceLimit.code(), 6);

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// I3 回归：`ace_deserialize` 先判参数与输入预算、再碰输入字节 —— 极小 `max_input_bytes`
    /// 加大输入时既不整包拷贝（累计请求字节远小于 `len`）也不读输入，只返回 `6`；未注册
    /// `type_key` 仍优先于预算（与旧实现同一优先级）。
    #[test]
    fn deserialize_checks_input_budget_before_reading_the_input() {
        let options = options();
        let schema = schema_with(c"demo.budget".as_ptr());
        add_field(schema, 0, prim(ACE_FK_I32));
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        // 输入预算 1B：任何非空输入都超限（其余维度留大）。
        assert_eq!(
            ace_options_set_limits(options, 256, 1, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20),
            ACE_OK
        );

        // 8 MiB 输入在测量区**外**分配；库若整包复制，请求字节必然 ≥ `len`。
        let input = vec![0u8; 8 << 20];
        let key = c"demo.budget";
        let mut out = ptr::NonNull::<AceValue>::dangling().as_ptr();
        let reading = alloc_probe::measure(|| {
            let code = ace_deserialize(
                key.as_ptr(),
                input.as_ptr(),
                input.len(),
                options,
                &mut out,
            );
            assert_eq!(code, Error::ResourceLimit.code());
        });
        assert!(out.is_null(), "失败必须置空 *out");
        assert!(
            reading.requested_bytes < input.len(),
            "超预算的输入不得整包拷贝：{reading:?} / len = {}",
            input.len()
        );

        // 未注册 key + 超预算输入：仍先报 3（旧实现的优先级）。
        let mut out = ptr::NonNull::<AceValue>::dangling().as_ptr();
        let missing = c"demo.budget_missing";
        assert_eq!(
            ace_deserialize(
                missing.as_ptr(),
                input.as_ptr(),
                input.len(),
                options,
                &mut out
            ),
            Error::UnknownTypeKey.code()
        );
        assert!(out.is_null());

        // NULL buf + 非零长度仍为 1（参数校验不变）。
        let mut out = ptr::NonNull::<AceValue>::dangling().as_ptr();
        assert_eq!(
            ace_deserialize(key.as_ptr(), ptr::null(), 1, options, &mut out),
            Error::InvalidArgument.code()
        );

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    /// 重复键当场报：第二对重复、第三对截断时必须 `16`（不是截断的 `2`）。
    #[test]
    fn deserialize_reports_duplicate_map_key_at_second_pair() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        let schema = schema_with(c"demo.decode_duplicate".as_ptr());
        let mut key = ptr::null_mut();
        assert_eq!(ace_type_str(&mut key), ACE_OK);
        let mut tags = ptr::null_mut();
        assert_eq!(ace_type_map(key, prim(ACE_FK_I32), &mut tags), ACE_OK);
        assert_eq!(ace_schema_add_field(schema, 0, tags), ACE_OK);
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);

        // Map 体声明 3 对、实际只写 2 对：第三对截断（旧实现先解完所有条目，会报 `2`）。
        let pairs = [
            3u32.to_le_bytes().as_slice(),
            1i32.to_le_bytes().as_slice(),
            3u32.to_le_bytes().as_slice(),
            2i32.to_le_bytes().as_slice(),
        ]
        .concat();
        let map_body = [3u32.to_le_bytes().as_slice(), pairs.as_slice()].concat();
        let wire = envelope(
            &[
                // 根对象体：槽范围（offset 1、count 1）。
                record(1, &[1, 0, 0, 0, 1, 0, 0, 0]).as_slice(),
                record(2, &map_body).as_slice(),
                record(3, b"a").as_slice(),
            ]
            .concat(),
            &[slot(0, 0, 1), slot(1, 0, 2)].concat(),
        );
        assert_eq!(
            deserialize_with(options, Some("demo.decode_duplicate"), &wire).0,
            Error::DuplicateKey.code()
        );
        assert_eq!(Error::DuplicateKey.code(), 16);

        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn deserialize_rejects_strong_cycles_without_building_them() {
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        // 互引的两个 schema：add_field 只拒直接自引用，a → b → a 是合法调用方图。
        let a = schema_with(c"demo.cycle_a".as_ptr());
        let b = schema_with(c"demo.cycle_b".as_ptr());
        let mut object_b = ptr::null_mut();
        assert_eq!(ace_type_object(b, &mut object_b), ACE_OK);
        assert_eq!(ace_schema_add_field(a, 0, object_b), ACE_OK);
        let mut object_a = ptr::null_mut();
        assert_eq!(ace_type_object(a, &mut object_a), ACE_OK);
        assert_eq!(ace_schema_add_field(b, 0, object_a), ACE_OK);
        assert_eq!(ace_options_add_schema(options, a), ACE_OK);

        // wire：根记录 1 的对象体只含一个字段，其引用又指回记录 1 —— 强环。
        let main = record(1, &[1, 0, 0, 0, 1, 0, 0, 0]);
        let assist = [slot(0, 0, 1), slot(1, 0, 1)].concat();
        let bytes = envelope(&main, &assist);
        assert_eq!(
            deserialize_with(options, Some("demo.cycle_a"), &bytes).0,
            Error::CycleUnsupported.code()
        );
        assert_eq!(Error::CycleUnsupported.code(), 14);

        // 同一条判据也覆盖容器：list 记录的元素体指回该 list 记录自身。
        //
        // 注：自环用例的 4B 元素体与外层字段槽载荷同为 list 记录 id，两条读法都得 `14`，故它不能
        // 单独证明元素位置读的是元素体——`deserialize_reads_object_id_at_element_position`
        // 用非环 `list<object>`（两个 id 不同）区分两条读法。
        let child = child_schema();
        let list_cycle = schema_with(c"demo.list_cycle".as_ptr());
        let mut element = ptr::null_mut();
        assert_eq!(ace_type_object(child, &mut element), ACE_OK);
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(element, &mut node), ACE_OK);
        assert_eq!(ace_schema_add_field(list_cycle, 0, node), ACE_OK);
        assert_eq!(ace_options_add_schema(options, list_cycle), ACE_OK);
        let list_wire = envelope(
            &[
                record(1, &[1, 0, 0, 0, 1, 0, 0, 0]).as_slice(),
                // list 记录 2：count = 1，元素体（4B 记录 id）= 2，即它自己。
                record(2, &[1, 0, 0, 0, 2, 0, 0, 0]).as_slice(),
            ]
            .concat(),
            &[slot(0, 0, 1), slot(1, 0, 2)].concat(),
        );
        assert_eq!(
            deserialize_with(options, Some("demo.list_cycle"), &list_wire).0,
            Error::CycleUnsupported.code()
        );

        assert_eq!(ace_schema_free(a), ACE_OK);
        assert_eq!(ace_schema_free(b), ACE_OK);
        assert_eq!(ace_schema_free(list_cycle), ACE_OK);
        assert_eq!(ace_schema_free(child), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
    }

    #[test]
    fn deserialize_passes_core_errors_through() {
        let registry = options();
        assert_eq!(ace_options_set_packed(registry, 0), ACE_OK);
        let required = schema_with(c"demo.decode_required".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(required, 0, text), ACE_OK);
        assert_eq!(ace_options_add_schema(registry, required), ACE_OK);

        // 3：未注册 type_key（含 NULL options 的空白名单）。
        let main = record(1, &[1, 0, 0, 0, 1, 0, 0, 0]);
        let assist = [slot(0, 0, 1), slot(1, 0, 2)].concat();
        let bytes = [main.as_slice(), record(2, b"x").as_slice()].concat();
        let wire = envelope(&bytes, &assist);
        assert_eq!(
            deserialize_with(registry, Some("demo.decode_unknown"), &wire).0,
            3
        );
        assert_eq!(deserialize_with(ptr::null(), Some("demo.decode_required"), &wire).0, 3);

        // 1：NULL out / NULL type_key / NULL buf + 非零长度。
        assert_eq!(ace_deserialize(ptr::null(), wire.as_ptr(), wire.len(), registry, ptr::null_mut()), 1);
        let mut out = ptr::NonNull::<AceValue>::dangling().as_ptr();
        assert_eq!(
            ace_deserialize(ptr::null(), wire.as_ptr(), wire.len(), registry, &mut out),
            1
        );
        assert!(out.is_null());
        let key = std::ffi::CString::new("demo.decode_required").expect("type_key 无内嵌 NUL");
        assert_eq!(
            ace_deserialize(key.as_ptr(), ptr::null(), 3, registry, &mut out),
            1
        );
        assert!(out.is_null());

        // 2：截断（信封声明的长度大于实际字节）。
        assert_eq!(
            deserialize_with(registry, Some("demo.decode_required"), &wire[..wire.len() - 1]).0,
            2
        );

        // 8：记录体不是合法 UTF-8。
        let invalid_utf8 = envelope(
            &[main.as_slice(), record(2, &[0xFF]).as_slice()].concat(),
            &assist,
        );
        assert_eq!(
            deserialize_with(registry, Some("demo.decode_required"), &invalid_utf8).0,
            8
        );

        // 15：必填的 str 字段缺席（对象体 count=0）。
        let empty = envelope(
            &record(1, &[1, 0, 0, 0, 0, 0, 0, 0]),
            &slot(0, 0, 1),
        );
        assert_eq!(
            deserialize_with(registry, Some("demo.decode_required"), &empty).0,
            15
        );

        // 4：bool 槽载荷非规范（2 → InvalidTag）。
        let flags = schema_with(c"demo.decode_flag".as_ptr());
        assert_eq!(ace_schema_add_field(flags, 0, prim(ACE_FK_BOOL)), ACE_OK);
        assert_eq!(ace_options_add_schema(registry, flags), ACE_OK);
        let invalid_tag = envelope(
            &record(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            &[slot(0, 0, 1), slot(1, 0, 2)].concat(),
        );
        assert_eq!(
            deserialize_with(registry, Some("demo.decode_flag"), &invalid_tag).0,
            4
        );

        // 5：深度上限 0——根对象（k=0）允许进入，字段位置的 list（k=1）超限。
        let shallow = options();
        assert_eq!(ace_options_set_packed(shallow, 0), ACE_OK);
        assert_eq!(
            ace_options_set_limits(shallow, 0, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20),
            ACE_OK
        );
        let nested = schema_with(c"demo.decode_nested".as_ptr());
        let mut inner = ptr::null_mut();
        assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut inner), ACE_OK);
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(inner, &mut node), ACE_OK);
        assert_eq!(ace_schema_add_field(nested, 0, node), ACE_OK);
        assert_eq!(ace_options_add_schema(shallow, nested), ACE_OK);
        let item_record = record(2, &[0, 0, 0, 0]);
        let deep = envelope(
            &[record(1, &[1, 0, 0, 0, 1, 0, 0, 0]).as_slice(), item_record.as_slice()]
                .concat(),
            &[slot(0, 0, 1), slot(1, 0, 2)].concat(),
        );
        assert_eq!(
            deserialize_with(shallow, Some("demo.decode_nested"), &deep).0,
            5
        );
        assert_eq!(ace_options_free(shallow), ACE_OK);

        // 6：集合长度上限 0——同一条非空 list 记录被拒。
        let tight = options();
        assert_eq!(ace_options_set_packed(tight, 0), ACE_OK);
        assert_eq!(ace_options_add_schema(tight, nested), ACE_OK);
        assert_eq!(
            ace_options_set_limits(tight, 256, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 0, 1 << 20, 1 << 20),
            ACE_OK
        );
        let items = envelope(
            &[record(1, &[1, 0, 0, 0, 1, 0, 0, 0]).as_slice(), record(2, &[1, 0, 0, 0, 7, 0, 0, 0]).as_slice()]
                .concat(),
            &[slot(0, 0, 1), slot(1, 0, 2)].concat(),
        );
        assert_eq!(
            deserialize_with(tight, Some("demo.decode_nested"), &items).0,
            6
        );
        assert_eq!(ace_options_free(tight), ACE_OK);

        // 13：同一记录 id 先按 str、后按 obj 复用 —— 形状不兼容，不得拆成两个节点。
        let child = child_schema();
        let clash = schema_with(c"demo.decode_clash".as_ptr());
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        assert_eq!(ace_schema_add_field(clash, 0, text), ACE_OK);
        let mut object_type = ptr::null_mut();
        assert_eq!(ace_type_object(child, &mut object_type), ACE_OK);
        assert_eq!(ace_schema_add_field(clash, 1, object_type), ACE_OK);
        assert_eq!(ace_options_add_schema(registry, clash), ACE_OK);
        let clash_wire = envelope(
            &[record(1, &[1, 0, 0, 0, 2, 0, 0, 0]).as_slice(), record(2, b"abcdefgh").as_slice()]
                .concat(),
            &[slot(0, 0, 1), slot(1, 0, 2), slot(1, 1, 2)].concat(),
        );
        assert_eq!(
            deserialize_with(registry, Some("demo.decode_clash"), &clash_wire).0,
            13
        );

        assert_eq!(ace_schema_free(required), ACE_OK);
        assert_eq!(ace_schema_free(flags), ACE_OK);
        assert_eq!(ace_schema_free(nested), ACE_OK);
        assert_eq!(ace_schema_free(clash), ACE_OK);
        assert_eq!(ace_schema_free(child), ACE_OK);
        assert_eq!(ace_options_free(registry), ACE_OK);
    }

    /// 错误优先级（R35）：集合深度先判，再读 `count`、再判长度。深度错误不得被记录视图／`count`
    /// 的错误覆盖，深度足够时同两条 wire 仍按既有单错语义报 `7`／`2`（只改“谁先报”）。
    #[test]
    fn deserialize_checks_collection_depth_before_count() {
        let nested = schema_with(c"demo.decode_depth_first".as_ptr());
        let mut list = ptr::null_mut();
        assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut list), ACE_OK);
        assert_eq!(ace_schema_add_field(nested, 0, list), ACE_OK);
        // 根对象 1 个字段，字段 0 的槽指向 list 记录 2。
        let main = record(1, &[1, 0, 0, 0, 1, 0, 0, 0]);
        let assist = [slot(0, 0, 1), slot(1, 0, 2)].concat();
        // list 记录 2 缺失：先读 `count` 会报 7（InvalidReference）。
        let missing = envelope(&main, &assist);
        // list 记录 2 体不足 4B（`count` 被截断）：先读 `count` 会报 2。
        let truncated = envelope(
            &[main.as_slice(), record(2, &[0, 0]).as_slice()].concat(),
            &assist,
        );

        // max_depth = 0：根对象（k=0）可进入，字段位置的 list（k=1）超限 → 5，与记录缺失/截断无关。
        let shallow = options();
        assert_eq!(ace_options_set_packed(shallow, 0), ACE_OK);
        assert_eq!(ace_options_add_schema(shallow, nested), ACE_OK);
        assert_eq!(
            ace_options_set_limits(shallow, 0, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20, 1 << 20),
            ACE_OK
        );
        for wire in [&missing, &truncated] {
            assert_eq!(
                deserialize_with(shallow, Some("demo.decode_depth_first"), wire).0,
                5
            );
        }
        assert_eq!(ace_options_free(shallow), ACE_OK);

        // 对照：深度足够时同两条 wire 按既有单错语义报 7／2（顺序改动只影响优先级）。
        let deep = options();
        assert_eq!(ace_options_set_packed(deep, 0), ACE_OK);
        assert_eq!(ace_options_add_schema(deep, nested), ACE_OK);
        assert_eq!(
            deserialize_with(deep, Some("demo.decode_depth_first"), &missing).0,
            7
        );
        assert_eq!(
            deserialize_with(deep, Some("demo.decode_depth_first"), &truncated).0,
            2
        );
        assert_eq!(ace_options_free(deep), ACE_OK);
        assert_eq!(ace_schema_free(nested), ACE_OK);
    }

    // ——— R33 多态对象（按节点自身 `type_key` 分派） ———

    /// `str` 字段类型节点。
    fn str_type() -> *mut AceTypeRef {
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_str(&mut node), ACE_OK);
        node
    }

    /// `obj<schema>` 字段类型节点（借用 `schema`，须晚于引用方释放）。
    fn object_type(schema: *mut AceSchema) -> *mut AceTypeRef {
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_object(schema, &mut node), ACE_OK);
        node
    }

    /// `list<elem>` 字段类型节点（收编 `elem`）。
    fn list_type(elem: *mut AceTypeRef) -> *mut AceTypeRef {
        let mut node = ptr::null_mut();
        assert_eq!(ace_type_list(elem, &mut node), ACE_OK);
        node
    }

    fn add_field(schema: *mut AceSchema, field_id: i32, field_type: *mut AceTypeRef) {
        assert_eq!(ace_schema_add_field(schema, field_id, field_type), ACE_OK);
    }

    /// 基类 `demo.node_base{0: i32, 1: str}` 与两个派生 schema：`demo.node_b` 的字段 2 是 `i32`、
    /// `demo.node_c` 的字段 2 是 `str`。派生字段类型不同，故「按哪个 schema 写出」在解出来的
    /// 字段种类上可观察。
    fn poly_node_schemas() -> (*mut AceSchema, *mut AceSchema, *mut AceSchema) {
        let base = schema_with(c"demo.node_base".as_ptr());
        add_field(base, 0, prim(ACE_FK_I32));
        add_field(base, 1, str_type());
        let derived_b = schema_with(c"demo.node_b".as_ptr());
        add_field(derived_b, 0, prim(ACE_FK_I32));
        add_field(derived_b, 1, str_type());
        add_field(derived_b, 2, prim(ACE_FK_I32));
        let derived_c = schema_with(c"demo.node_c".as_ptr());
        add_field(derived_c, 0, prim(ACE_FK_I32));
        add_field(derived_c, 1, str_type());
        add_field(derived_c, 2, str_type());
        (base, derived_b, derived_c)
    }

    /// 容器 `demo.holder{0: list<obj<node_base>>, 1: list<obj<node_base>>}`：两个并列的基类列表，
    /// 用于同节点共享与「同 type_key 的不同节点不合并」。
    fn poly_holder_schema(base: *mut AceSchema) -> *mut AceSchema {
        let holder = schema_with(c"demo.holder".as_ptr());
        add_field(holder, 0, list_type(object_type(base)));
        add_field(holder, 1, list_type(object_type(base)));
        holder
    }

    /// 携带自身 `type_key` 的节点值：字段 0/1 是基类字段，`extra`（可为 NULL）是派生字段。
    fn node_value(type_key: &str, id: i64, name: &str, extra: *mut AceValue) -> *mut AceValue {
        let key = std::ffi::CString::new(type_key).expect("type_key 无内嵌 NUL");
        let mut value = ptr::null_mut();
        assert_eq!(ace_value_object(key.as_ptr(), &mut value), ACE_OK);
        set_owned(value, 0, i64_value(id));
        set_owned(value, 1, str_value(name.as_bytes()));
        if !extra.is_null() {
            set_owned(value, 2, extra);
        }
        value
    }

    /// 容器值：两个列表各放给定元素（`second` 为 NULL 时第二个列表为空）。
    fn holder_value(first: *mut AceValue, second: *mut AceValue) -> *mut AceValue {
        let value = object();
        set_owned(value, 0, first);
        set_owned(
            value,
            1,
            if second.is_null() { list() } else { second },
        );
        value
    }

    /// 未压缩 wire 里的多态对象体：逃生口体恰为 `2B tag + 4B type_name id + 8B 槽范围` = 14B。
    /// 只用于 raw 两态里的结构性核对（packed 由 `unpacked_regions` 的未压缩前提排除）。
    fn polymorphic_bodies(main: &[u8]) -> Vec<&[u8]> {
        records(main)
            .into_iter()
            .map(|(_, body)| body)
            .filter(|body| {
                body.len() == 14 && body.starts_with(&POLY_ESCAPE_TAG.to_le_bytes())
            })
            .collect()
    }

    /// R33②③（AC-A06①②⑥）：`List<基类>` 的元素各按**自身 `type_key`** 分派到派生 schema，
    /// 解码按同一 key 解回派生 schema 的对象；raw 与 packed 两态逐字段（含派生字段）往返相等。
    #[test]
    fn polymorphic_list_round_trips_derived_objects() {
        let (base, derived_b, derived_c) = poly_node_schemas();
        let holder = poly_holder_schema(base);

        for packed in [0u8, 1] {
            let options = options();
            assert_eq!(ace_options_set_packed(options, packed), ACE_OK);
            for schema in [base, derived_b, derived_c, holder] {
                assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
            }

            let items = list();
            add_owned(items, node_value("demo.node_b", 1, "b", i64_value(7)));
            add_owned(items, node_value("demo.node_c", 2, "c", str_value(b"deep")));
            let value = holder_value(items, ptr::null_mut());
            let (code, bytes) = serialize_with(options, Some("demo.holder"), value);
            assert_eq!(code, ACE_OK);
            free(value);

            if packed == 0 {
                // 两个元素各写一条多态记录：没有按父容器的静态元素 schema 统一写出。
                let (main, _) = unpacked_regions(&bytes);
                assert_eq!(polymorphic_bodies(main).len(), 2, "packed = {packed}");
            }

            let (code, decoded) = deserialize_with(options, Some("demo.holder"), &bytes);
            assert_eq!(code, ACE_OK, "packed = {packed}");
            let items = field(decoded, 0);
            assert_eq!(kind_of(items), 7);

            let b = list_item(items, 0);
            assert_eq!(kind_of(b), 8);
            assert_eq!(i64_of(field(b, 0)), 1);
            assert_eq!(text_of(field(b, 1)), b"b");
            // 派生字段按 `demo.node_b` 解出 i32——基类没有字段 2，按静态 schema 解不出这个种类。
            assert_eq!(kind_of(field(b, 2)), 2);
            assert_eq!(i64_of(field(b, 2)), 7);

            let c = list_item(items, 1);
            assert_eq!(kind_of(c), 8);
            assert_eq!(i64_of(field(c, 0)), 2);
            assert_eq!(text_of(field(c, 1)), b"c");
            // 同一元素位置、另一个派生 schema：字段 2 是 str。
            assert_eq!(kind_of(field(c, 2)), 5);
            assert_eq!(text_of(field(c, 2)), b"deep");

            // 解出的节点各自带 `type_key`：外给 NULL 也能再编码成同一份字节（分派可重复）。
            assert_eq!(serialize_with(options, None, decoded), (ACE_OK, bytes.clone()));
            free(decoded);
            assert_eq!(ace_options_free(options), ACE_OK);
        }

        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(base), ACE_OK);
        assert_eq!(ace_schema_free(derived_b), ACE_OK);
        assert_eq!(ace_schema_free(derived_c), ACE_OK);
    }

    /// R33②（AC-A06③）：`type_key` 未注册 → 编码与解码两侧都是既有 `3 UnknownTypeKey`。
    #[test]
    fn polymorphic_dispatch_requires_registered_type_key() {
        let (base, derived_b, derived_c) = poly_node_schemas();
        let holder = poly_holder_schema(base);

        // 编码侧：白名单只有基类与容器，元素自称 `demo.node_b` → 3。
        let encode_only = options();
        for schema in [base, holder] {
            assert_eq!(ace_options_add_schema(encode_only, schema), ACE_OK);
        }
        let items = list();
        add_owned(items, node_value("demo.node_b", 1, "b", i64_value(7)));
        let value = holder_value(items, ptr::null_mut());
        assert_eq!(serialize_with(encode_only, Some("demo.holder"), value).0, 3);
        free(value);
        assert_eq!(ace_options_free(encode_only), ACE_OK);

        // 解编码侧：先按完整白名单写出一份合法 wire，再在缺 `demo.node_b` 的白名单上解 → 3。
        let full = options();
        assert_eq!(ace_options_set_packed(full, 0), ACE_OK);
        for schema in [base, derived_b, derived_c, holder] {
            assert_eq!(ace_options_add_schema(full, schema), ACE_OK);
        }
        let items = list();
        add_owned(items, node_value("demo.node_b", 1, "b", i64_value(7)));
        let value = holder_value(items, ptr::null_mut());
        let (code, bytes) = serialize_with(full, Some("demo.holder"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        assert_eq!(ace_options_free(full), ACE_OK);

        let decode_only = options();
        assert_eq!(ace_options_set_packed(decode_only, 0), ACE_OK);
        for schema in [base, holder] {
            assert_eq!(ace_options_add_schema(decode_only, schema), ACE_OK);
        }
        assert_eq!(
            deserialize_with(decode_only, Some("demo.holder"), &bytes).0,
            3
        );
        assert_eq!(ace_options_free(decode_only), ACE_OK);

        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(base), ACE_OK);
        assert_eq!(ace_schema_free(derived_b), ACE_OK);
        assert_eq!(ace_schema_free(derived_c), ACE_OK);
    }

    /// R33② + I1：值自带 `type_key` 时，即使与所在位置的静态 schema 同 key，也要过白名单。
    /// ① 只注册容器（字段图内嵌 base 副本）：元素显式自带 `demo.node_base` → `3`（不得因为同 key
    /// 直接复用内嵌副本）；② 对照：把 base 独立注册后同形状成功，且同 key 仍写普通对象体。
    #[test]
    fn polymorphic_same_key_values_still_require_registration() {
        let (base, derived_b, derived_c) = poly_node_schemas();
        let holder = poly_holder_schema(base);

        // ① 白名单只有容器：内嵌的 base 副本不是注册项，元素的 `demo.node_base` → 3。
        let embedded_only = options();
        assert_eq!(ace_options_add_schema(embedded_only, holder), ACE_OK);
        let items = list();
        add_owned(items, node_value("demo.node_base", 1, "base", ptr::null_mut()));
        let value = holder_value(items, ptr::null_mut());
        assert_eq!(serialize_with(embedded_only, Some("demo.holder"), value).0, 3);
        free(value);
        assert_eq!(ace_options_free(embedded_only), ACE_OK);

        // ② 对照：base 独立注册 → 成功；同 key 写普通对象体，没有多态头，解码也按普通体还原。
        let registered = options();
        assert_eq!(ace_options_set_packed(registered, 0), ACE_OK);
        for schema in [base, holder] {
            assert_eq!(ace_options_add_schema(registered, schema), ACE_OK);
        }
        let items = list();
        add_owned(items, node_value("demo.node_base", 1, "base", ptr::null_mut()));
        let value = holder_value(items, ptr::null_mut());
        let (code, bytes) = serialize_with(registered, Some("demo.holder"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (main, _) = unpacked_regions(&bytes);
        assert_eq!(polymorphic_bodies(main).len(), 0, "同 key 不得多写多态头");
        let (code, decoded) = deserialize_with(registered, Some("demo.holder"), &bytes);
        assert_eq!(code, ACE_OK);
        let element = list_item(field(decoded, 0), 0);
        assert_eq!(kind_of(element), 8);
        assert_eq!(i64_of(field(element, 0)), 1);
        assert_eq!(text_of(field(element, 1)), b"base");
        free(decoded);
        assert_eq!(ace_options_free(registered), ACE_OK);

        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(base), ACE_OK);
        assert_eq!(ace_schema_free(derived_b), ACE_OK);
        assert_eq!(ace_schema_free(derived_c), ACE_OK);
    }

    /// R33⑥ 夹具：位置 schema 内嵌一份 `{0: i32, 1: str}` 的对象副本，白名单另注册同 `type_key`
    /// 的**独立句柄**（`same_layout` 决定它的布局：`{0: i32, 1: str}`，或与内嵌不同的 `{0: str}`），
    /// 以及容器 `*hold{0: obj<内嵌副本>}`。返回 `(内嵌副本, 注册副本, 容器)`。
    ///
    /// 两个句柄指针不同，故库内是两份独立副本——同一句柄会被复制去重，布局必然一致。
    fn keyless_layout_schemas(
        key: &std::ffi::CStr,
        holder_key: &std::ffi::CStr,
        same_layout: bool,
    ) -> (*mut AceSchema, *mut AceSchema, *mut AceSchema) {
        let embedded = schema_with(key.as_ptr());
        add_field(embedded, 0, prim(ACE_FK_I32));
        add_field(embedded, 1, str_type());
        let registered = schema_with(key.as_ptr());
        if same_layout {
            add_field(registered, 0, prim(ACE_FK_I32));
            add_field(registered, 1, str_type());
        } else {
            // 异布局：字段 0 换成 str，种类不同故解出来的字段能直接辨别用的是哪套布局。
            add_field(registered, 0, str_type());
        }
        let holder = schema_with(holder_key.as_ptr());
        add_field(holder, 0, object_type(embedded));
        (embedded, registered, holder)
    }

    /// 不带 `type_key` 的对象值：字段 0 是 i32、字段 1 是 str（即内嵌副本的布局）。
    fn keyless_node(id: i64, name: &str) -> *mut AceValue {
        let value = object();
        set_owned(value, 0, i64_value(id));
        set_owned(value, 1, str_value(name.as_bytes()));
        value
    }

    /// R33⑥①：值**不带** `type_key`、而白名单里同 `type_key` 的注册项布局不同 → 编码 fail-closed
    /// 返回既有 `13`，不写歧义 wire（`serialize_with` 在失败时断言 `*out` 为空缓冲）。
    #[test]
    fn keyless_object_with_differently_shaped_registration_is_type_mismatch() {
        let (embedded, registered, holder) =
            keyless_layout_schemas(c"demo.kbase", c"demo.khold", false);
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        for schema in [holder, registered] {
            assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        }
        // 值恰好按位置内嵌布局写：没有 R33⑥ 的校验时这份输入本会成功。
        let value = object();
        set_owned(value, 0, keyless_node(7, "x"));
        assert_eq!(serialize_with(options, Some("demo.khold"), value).0, 13);
        free(value);

        assert_eq!(ace_options_free(options), ACE_OK);
        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(registered), ACE_OK);
        assert_eq!(ace_schema_free(embedded), ACE_OK);
    }

    /// R33⑥②：同 `type_key` 的注册项与内嵌副本**同布局**时间的不受影响——编码成功，且字节与
    /// 「完全未注册」时逐字节相同（R33⑥ 不改其它成功输入的字节）。raw/packed 两态各一遍。
    #[test]
    fn keyless_object_with_identically_shaped_registration_keeps_unregistered_bytes() {
        let (embedded, registered, holder) =
            keyless_layout_schemas(c"demo.sbase", c"demo.shold", true);
        for packed in [0u8, 1] {
            let unregistered = options();
            assert_eq!(ace_options_set_packed(unregistered, packed), ACE_OK);
            assert_eq!(ace_options_add_schema(unregistered, holder), ACE_OK);
            let with_registration = options();
            assert_eq!(ace_options_set_packed(with_registration, packed), ACE_OK);
            for schema in [holder, registered] {
                assert_eq!(ace_options_add_schema(with_registration, schema), ACE_OK);
            }

            let value = object();
            set_owned(value, 0, keyless_node(7, "x"));
            let (code, plain) = serialize_with(unregistered, Some("demo.shold"), value);
            assert_eq!(code, ACE_OK, "packed = {packed}");
            let (code, registered_bytes) =
                serialize_with(with_registration, Some("demo.shold"), value);
            assert_eq!(code, ACE_OK, "packed = {packed}");
            assert_eq!(
                registered_bytes, plain,
                "同 key 同布局不得改变字节（packed = {packed}）"
            );
            free(value);

            assert_eq!(ace_options_free(unregistered), ACE_OK);
            assert_eq!(ace_options_free(with_registration), ACE_OK);
        }
        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(registered), ACE_OK);
        assert_eq!(ace_schema_free(embedded), ACE_OK);
    }

    /// R33⑥③：值**自带** `type_key`、且注册布局与内嵌布局不同 → 仍按 R33② 走注册目标（同 key 写
    /// 普通对象体，无多态头），往返成功且字段种类与值按注册布局还原。
    #[test]
    fn keyed_object_with_differently_shaped_registration_uses_registered_target() {
        let (embedded, registered, holder) =
            keyless_layout_schemas(c"demo.kbase", c"demo.khold", false);
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        for schema in [holder, registered] {
            assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        }

        // 子值自带与位置 schema 同 key 的 `type_key`：字段 0 是 str——只有注册布局能承载它。
        let mut child = ptr::null_mut();
        assert_eq!(ace_value_object(c"demo.kbase".as_ptr(), &mut child), ACE_OK);
        set_owned(child, 0, str_value(b"hi"));
        let value = object();
        set_owned(value, 0, child);
        let (code, bytes) = serialize_with(options, Some("demo.khold"), value);
        assert_eq!(code, ACE_OK);
        free(value);

        let (main, _) = unpacked_regions(&bytes);
        assert_eq!(polymorphic_bodies(main).len(), 0, "同 key 仍写普通对象体");
        let (code, decoded) = deserialize_with(options, Some("demo.khold"), &bytes);
        assert_eq!(code, ACE_OK);
        let element = field(decoded, 0);
        assert_eq!(kind_of(element), 8);
        assert_eq!(kind_of(field(element, 0)), 5);
        assert_eq!(text_of(field(element, 0)), b"hi");
        free(decoded);

        assert_eq!(ace_options_free(options), ACE_OK);
        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(registered), ACE_OK);
        assert_eq!(ace_schema_free(embedded), ACE_OK);
    }

    /// R33④（AC-A06④）：多态体的判别值不属于 C ABI 的已注册集合 → 既有 `4 InvalidTag`。
    /// C ABI 只用 type_name 逃生口，数值 tag 没有可匹配的变体。
    ///
    /// 三条手写 wire 的槽（根槽 + 字段 0/1）完全相同，只改体头：逃生口（14B，别名即已注册的
    /// `demo.node_base`）→ 正常解出；普通对象体（8B）→ 正常解出；数值 tag（10B）→ `4`。
    /// 前两条立起「该位置的槽与字段本身合法」，故第三条的 `4` 只能来自判别值。
    #[test]
    fn polymorphic_numeric_tag_is_invalid_tag() {
        let (base, derived_b, derived_c) = poly_node_schemas();
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        assert_eq!(ace_options_add_schema(options, base), ACE_OK);

        // 记录 2 = 字段 1 的 str 体；记录 3 = 逃生口的 type_name（UTF-8 原文）。
        let text_record = record(2, b"x");
        let name_record = record(3, b"demo.node_base");
        let assist = [slot(0, 0, 1), slot(1, 0, 7), slot(1, 1, 2)].concat();
        let escape = [
            65535u16.to_le_bytes().as_slice(),
            3u32.to_le_bytes().as_slice(),
            1u32.to_le_bytes().as_slice(),
            2u32.to_le_bytes().as_slice(),
        ]
        .concat();
        let numeric = [
            1u16.to_le_bytes().as_slice(),
            1u32.to_le_bytes().as_slice(),
            2u32.to_le_bytes().as_slice(),
        ]
        .concat();
        let plain = [1u32.to_le_bytes().as_slice(), 2u32.to_le_bytes().as_slice()].concat();

        for body in [&escape, &plain] {
            let wire = envelope(
                &[
                    record(1, body).as_slice(),
                    text_record.as_slice(),
                    name_record.as_slice(),
                ]
                .concat(),
                &assist,
            );
            let (code, decoded) = deserialize_with(options, Some("demo.node_base"), &wire);
            assert_eq!(code, ACE_OK);
            assert_eq!(i64_of(field(decoded, 0)), 7);
            assert_eq!(text_of(field(decoded, 1)), b"x");
            free(decoded);
        }

        let wire = envelope(
            &[
                record(1, &numeric).as_slice(),
                text_record.as_slice(),
                name_record.as_slice(),
            ]
            .concat(),
            &assist,
        );
        assert_eq!(deserialize_with(options, Some("demo.node_base"), &wire).0, 4);

        assert_eq!(ace_options_free(options), ACE_OK);
        assert_eq!(ace_schema_free(base), ACE_OK);
        assert_eq!(ace_schema_free(derived_b), ACE_OK);
        assert_eq!(ace_schema_free(derived_c), ACE_OK);
    }

    /// R33⑤（AC-A06⑤）：身份按节点指针。同一派生对象的两处引用只写一条多态记录、解码后 `ptr ==`；
    /// `type_key` 与字段值都相同的两个不同节点写两条记录、解码后不合并。
    #[test]
    fn polymorphic_objects_keep_pointer_identity() {
        let (base, derived_b, derived_c) = poly_node_schemas();
        let holder = poly_holder_schema(base);
        let options = options();
        assert_eq!(ace_options_set_packed(options, 0), ACE_OK);
        for schema in [base, derived_b, derived_c, holder] {
            assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        }

        // —— 共享：同一节点经 `ace_value_share` 放进两个基类列表 ——
        let shared = node_value("demo.node_b", 5, "same", i64_value(9));
        let mut second = ptr::null_mut();
        assert_eq!(ace_value_share(shared, &mut second), ACE_OK);
        let first_list = list();
        add_owned(first_list, shared);
        let second_list = list();
        add_owned(second_list, second);
        let value = holder_value(first_list, second_list);
        let (code, bytes) = serialize_with(options, Some("demo.holder"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (main, _) = unpacked_regions(&bytes);
        assert_eq!(polymorphic_bodies(main).len(), 1, "同一节点只写一条多态记录");
        let (code, decoded) = deserialize_with(options, Some("demo.holder"), &bytes);
        assert_eq!(code, ACE_OK);
        let left = list_item(field(decoded, 0), 0);
        let right = list_item(field(decoded, 1), 0);
        assert!(std::ptr::eq(left, right), "同一节点的两处引用解码后仍是同一节点");
        free(decoded);

        // —— 不合并：两个不同节点、同 `type_key`、同字段值 ——
        let first_list = list();
        add_owned(first_list, node_value("demo.node_b", 5, "same", i64_value(9)));
        let second_list = list();
        add_owned(second_list, node_value("demo.node_b", 5, "same", i64_value(9)));
        let value = holder_value(first_list, second_list);
        let (code, bytes) = serialize_with(options, Some("demo.holder"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (main, _) = unpacked_regions(&bytes);
        assert_eq!(
            polymorphic_bodies(main).len(),
            2,
            "身份不按 type_key：两个节点各写一条记录"
        );
        let (code, decoded) = deserialize_with(options, Some("demo.holder"), &bytes);
        assert_eq!(code, ACE_OK);
        let left = list_item(field(decoded, 0), 0);
        let right = list_item(field(decoded, 1), 0);
        assert!(!std::ptr::eq(left, right), "type_key 与字段值相同也不得合并");
        assert_eq!(i64_of(field(left, 2)), 9);
        assert_eq!(i64_of(field(right, 2)), 9);
        free(decoded);

        assert_eq!(ace_options_free(options), ACE_OK);
        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(base), ACE_OK);
        assert_eq!(ace_schema_free(derived_b), ACE_OK);
        assert_eq!(ace_schema_free(derived_c), ACE_OK);
    }

    // ——— 门 B：分配/释放配对（计数全局分配器，见 [`super::alloc_probe`]） ———

    /// `gateb.leaf{0: i32, 1: str}` 的值。
    fn leaf_value(id: i64, name: &[u8]) -> *mut AceValue {
        let value = object();
        set_owned(value, 0, i64_value(id));
        set_owned(value, 1, str_value(name));
        value
    }

    /// 门 B 树状值一轮：`gateb.tree{0: obj<gateb.leaf>, 1: list<obj<gateb.leaf>>}`。
    /// 本轮的 schema/options/缓冲区/值全部创建并释放，不向下一轮遗留句柄。
    fn tree_round() {
        let leaf = schema_with(c"gateb.leaf".as_ptr());
        add_field(leaf, 0, prim(ACE_FK_I32));
        add_field(leaf, 1, str_type());
        let tree = schema_with(c"gateb.tree".as_ptr());
        add_field(tree, 0, object_type(leaf));
        add_field(tree, 1, list_type(object_type(leaf)));

        let options = options();
        assert_eq!(ace_options_add_schema(options, tree), ACE_OK);
        assert_eq!(ace_options_add_schema(options, leaf), ACE_OK);

        let value = object();
        set_owned(value, 0, leaf_value(7, b"left"));
        let items = list();
        add_owned(items, leaf_value(8, b"a"));
        add_owned(items, leaf_value(9, b"b"));
        set_owned(value, 1, items);

        let (code, bytes) = serialize_with(options, Some("gateb.tree"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (code, decoded) = deserialize_with(options, Some("gateb.tree"), &bytes);
        assert_eq!(code, ACE_OK);
        assert_eq!(i64_of(field(field(decoded, 0), 0)), 7);
        assert_eq!(text_of(field(field(decoded, 0), 1)), b"left");
        let listed = field(decoded, 1);
        assert_eq!(i64_of(field(list_item(listed, 1), 0)), 9);
        assert_eq!(text_of(field(list_item(listed, 1), 1)), b"b");
        free(decoded);

        assert_eq!(ace_options_free(options), ACE_OK);
        assert_eq!(ace_schema_free(tree), ACE_OK);
        assert_eq!(ace_schema_free(leaf), ACE_OK);
    }

    /// 门 B 共享 DAG 一轮：`gateb.dag{0: obj<gateb.leaf>, 1: obj<gateb.leaf>,
    /// 2: list<obj<gateb.leaf>>}`，字段 0/1 是 `ace_value_share` 出来的同一节点。
    fn shared_round() {
        let leaf = schema_with(c"gateb.leaf".as_ptr());
        add_field(leaf, 0, prim(ACE_FK_I32));
        add_field(leaf, 1, str_type());
        let dag = schema_with(c"gateb.dag".as_ptr());
        add_field(dag, 0, object_type(leaf));
        add_field(dag, 1, object_type(leaf));
        add_field(dag, 2, list_type(object_type(leaf)));

        let options = options();
        assert_eq!(ace_options_add_schema(options, dag), ACE_OK);
        assert_eq!(ace_options_add_schema(options, leaf), ACE_OK);

        let node = leaf_value(5, b"same");
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(node, &mut shared), ACE_OK);
        let value = object();
        set_owned(value, 0, node);
        set_owned(value, 1, shared);
        let items = list();
        add_owned(items, leaf_value(6, b"other"));
        set_owned(value, 2, items);

        let (code, bytes) = serialize_with(options, Some("gateb.dag"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (code, decoded) = deserialize_with(options, Some("gateb.dag"), &bytes);
        assert_eq!(code, ACE_OK);
        let first = field(decoded, 0);
        let second = field(decoded, 1);
        assert!(std::ptr::eq(first, second), "两处引用解码后仍是同一节点");
        assert_eq!(i64_of(field(first, 0)), 5);
        assert_eq!(i64_of(field(list_item(field(decoded, 2), 0), 0)), 6);
        free(decoded);

        assert_eq!(ace_options_free(options), ACE_OK);
        assert_eq!(ace_schema_free(dag), ACE_OK);
        assert_eq!(ace_schema_free(leaf), ACE_OK);
    }

    /// 门 B 多态对象一轮：`gateb.pholder{0: obj<gateb.base>, 1: list<obj<gateb.base>>}`，
    /// 值自带 `gateb.derived`，按自身 `type_key` 走 FR-W07 逃生口体。
    fn polymorphic_round() {
        let base = schema_with(c"gateb.base".as_ptr());
        add_field(base, 0, prim(ACE_FK_I32));
        add_field(base, 1, str_type());
        let derived = schema_with(c"gateb.derived".as_ptr());
        add_field(derived, 0, prim(ACE_FK_I32));
        add_field(derived, 1, str_type());
        add_field(derived, 2, prim(ACE_FK_I64));
        let holder = schema_with(c"gateb.pholder".as_ptr());
        add_field(holder, 0, object_type(base));
        add_field(holder, 1, list_type(object_type(base)));

        let options = options();
        for schema in [base, derived, holder] {
            assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        }

        let value = object();
        set_owned(
            value,
            0,
            node_value("gateb.derived", 3, "poly", i64_value(99)),
        );
        let items = list();
        add_owned(items, node_value("gateb.derived", 4, "item", i64_value(42)));
        set_owned(value, 1, items);

        let (code, bytes) = serialize_with(options, Some("gateb.pholder"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        let (code, decoded) = deserialize_with(options, Some("gateb.pholder"), &bytes);
        assert_eq!(code, ACE_OK);
        // 基类 schema 没有字段 2：读得到派生值即证明按派生 `type_key` 分派。
        assert_eq!(i64_of(field(field(decoded, 0), 2)), 99);
        assert_eq!(i64_of(field(list_item(field(decoded, 1), 0), 2)), 42);
        free(decoded);

        assert_eq!(ace_options_free(options), ACE_OK);
        assert_eq!(ace_schema_free(holder), ACE_OK);
        assert_eq!(ace_schema_free(base), ACE_OK);
        assert_eq!(ace_schema_free(derived), ACE_OK);
    }

    /// 门 B 晚失败一轮（I4）：先让共享子节点成功写出/物化，再让后续字段失败 —— 编码侧字段 2
    /// 缺席 → `15`，解码侧字段 2 的枚举判别值不在窄变体集 → `4`。失败路径的缓冲与输出必须干净，
    /// 已写过记录 / 已建出的节点与身份表随调用结束全部释放（配对与净字节由探针断言）。
    ///
    /// 两个 options 只差枚举变体集：宽集（{1,2}）产出合法 wire，窄集（{1}）在字段 0/1 已物化
    /// 共享节点之后、解字段 2 时报 `4`。
    fn late_failure_round() {
        let node = schema_with(c"gatebl.late_node".as_ptr());
        add_field(node, 0, prim(ACE_FK_I32));
        let wide_tag = enum_with(c"gatebl.late_tag".as_ptr());
        assert_eq!(ace_enum_set_repr(wide_tag, ACE_FK_I32), ACE_OK);
        assert_eq!(ace_enum_add_variant(wide_tag, 1), ACE_OK);
        assert_eq!(ace_enum_add_variant(wide_tag, 2), ACE_OK);
        let narrow_tag = enum_with(c"gatebl.late_tag".as_ptr());
        assert_eq!(ace_enum_set_repr(narrow_tag, ACE_FK_I32), ACE_OK);
        assert_eq!(ace_enum_add_variant(narrow_tag, 1), ACE_OK);

        // `gatebl.late_pair{0: obj<late_node>, 1: obj<late_node>, 2: enum<late_tag>}`；两个
        // options 各持一份副本（只差枚举变体集）。
        let wide = options();
        let narrow = options();
        let mut pairs = Vec::new();
        for (options, tag) in [(wide, wide_tag), (narrow, narrow_tag)] {
            let pair = schema_with(c"gatebl.late_pair".as_ptr());
            add_field(pair, 0, object_type(node));
            add_field(pair, 1, object_type(node));
            let mut tag_type = ptr::null_mut();
            assert_eq!(ace_type_enum(tag, &mut tag_type), ACE_OK);
            add_field(pair, 2, tag_type);
            assert_eq!(ace_options_add_schema(options, pair), ACE_OK);
            assert_eq!(ace_options_add_schema(options, node), ACE_OK);
            pairs.push(pair);
        }

        // 字段 0/1 是同一节点（共享身份），字段 2 可选。
        let pair_value = |tag: Option<u64>| {
            let shared = object();
            set_owned(shared, 0, i64_value(7));
            let mut second = ptr::null_mut();
            assert_eq!(ace_value_share(shared, &mut second), ACE_OK);
            let value = object();
            set(value, 0, shared);
            set(value, 1, second);
            free(shared);
            free(second);
            if let Some(tag) = tag {
                set_owned(value, 2, u64_value(tag));
            }
            value
        };

        // 编码侧晚失败：共享节点已写出记录、字段 1 已写引用 id，字段 2 缺席 → 15。
        let value = pair_value(None);
        assert_eq!(serialize_with(wide, Some("gatebl.late_pair"), value).0, 15);
        free(value);

        // 宽集合法 wire → 窄集解码：字段 0/1 先物化共享节点，字段 2 的 tag 2 不在 {1} → 4。
        let value = pair_value(Some(2));
        let (code, wire) = serialize_with(wide, Some("gatebl.late_pair"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        assert_eq!(
            deserialize_with(narrow, Some("gatebl.late_pair"), &wire).0,
            4
        );

        assert_eq!(ace_options_free(wide), ACE_OK);
        assert_eq!(ace_options_free(narrow), ACE_OK);
        for pair in pairs {
            assert_eq!(ace_schema_free(pair), ACE_OK);
        }
        assert_eq!(ace_schema_free(node), ACE_OK);
        assert_eq!(ace_enum_free(wide_tag), ACE_OK);
        assert_eq!(ace_enum_free(narrow_tag), ACE_OK);
    }

    /// 门 B 分配/释放配对：四类值图（树状、共享 DAG、多态、晚失败）各跑多轮
    /// 「构造 → 序列化 → 反序列化 → 全部释放」，断言每轮 alloc 次数 == free 次数、净字节 0，
    /// 且各轮读数完全相同（跨轮不增长）。
    ///
    /// 预热轮只报告不断言：`std` 的首次惰性初始化（如本测试线程的一次性缓冲）会在第一轮留下
    /// 不属于本库持有的分配；预热后每轮必须是零净增。
    #[test]
    fn allocation_and_free_pair_up_across_rounds() {
        let shapes: [(&str, fn()); 4] = [
            ("tree", tree_round),
            ("shared", shared_round),
            ("polymorphic", polymorphic_round),
            ("late-failure", late_failure_round),
        ];
        const ROUNDS: usize = 3;
        for (label, round) in shapes {
            let warmup = alloc_probe::measure(round);
            println!("gate B alloc probe {label} warmup: {warmup:?}");
            let mut readings = Vec::new();
            for index in 0..ROUNDS {
                let reading = alloc_probe::measure(round);
                println!("gate B alloc probe {label} round {index}: {reading:?}");
                assert!(
                    reading.allocs > 0,
                    "{label} round {index}: 探针未计数（分配器未生效？）"
                );
                assert_eq!(
                    reading.allocs, reading.deallocs,
                    "{label} round {index}: alloc 次数必须等于 free 次数（{reading:?}）"
                );
                assert_eq!(
                    reading.net_bytes, 0,
                    "{label} round {index}: 净字节必须为 0（{reading:?}）"
                );
                readings.push(reading);
            }
            for (index, reading) in readings.iter().enumerate() {
                assert_eq!(
                    *reading,
                    readings[ROUNDS - 1],
                    "{label} round {index}: 跨轮读数漂移 {readings:?}"
                );
            }
        }
    }

    // ——— AC-A04：不同独立句柄并发（同一输入与串行一致、一次失败不污染他次） ———

    /// `demo.conc`：0 → i32、1 → str、2/3 → 各自独立的 `list<i32>` 类型节点（共享身份的两个
    /// 引用位置，同 [`shared_list_schema`] 的形状）。
    fn concurrent_schema() -> *mut AceSchema {
        let schema = schema_with(c"demo.conc".as_ptr());
        add_field(schema, 0, prim(ACE_FK_I32));
        let mut text = ptr::null_mut();
        assert_eq!(ace_type_str(&mut text), ACE_OK);
        add_field(schema, 1, text);
        for field_id in [2, 3] {
            let mut node = ptr::null_mut();
            assert_eq!(ace_type_list(prim(ACE_FK_I32), &mut node), ACE_OK);
            add_field(schema, field_id, node);
        }
        schema
    }

    /// 与 [`concurrent_schema`] 同形的值：0 = 7、1 = "并发"、2/3 两个字段共用同一个
    /// `list<i32>` 节点 `[1, -2, 3]`（编码后一处记录、其余位置写引用 id）。
    fn concurrent_value() -> *mut AceValue {
        let value = object();
        set_owned(value, 0, i64_value(7));
        set_owned(value, 1, str_value("并发".as_bytes()));
        let numbers = list_value(&[1, -2, 3]);
        let mut shared = ptr::null_mut();
        assert_eq!(ace_value_share(numbers, &mut shared), ACE_OK);
        set(value, 2, numbers);
        set(value, 3, shared);
        free(numbers);
        free(shared);
        value
    }

    /// 本组并发用例的 wire：主线程一次产出，各线程**只读复用同一片字节**。
    fn concurrent_wire() -> Vec<u8> {
        let options = options();
        let schema = concurrent_schema();
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        let value = concurrent_value();
        let (code, bytes) = serialize_with(options, Some("demo.conc"), value);
        assert_eq!(code, ACE_OK);
        free(value);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
        bytes
    }

    /// 一次解码的可比对观察值：根/字段种类、I64 值、STR 文本、list 长度与元素，以及「同一调用
    /// 内 2/3 两字段是否同一个原生节点」这一**指针关系**。
    ///
    /// 只记关系不记绝对地址：需求只承诺一次 `ace_deserialize` 内的身份、并明文声明没有跨图/跨
    /// 线程共享的线程安全承诺（R32），故这里不对跨线程/跨图的身份做任何断言。
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ConcurrentView {
        root_kind: i32,
        id_kind: i32,
        id_value: i64,
        name_kind: i32,
        name_text: Vec<u8>,
        list_len: usize,
        list_items: Vec<i64>,
        shared_node: bool,
    }

    /// 串行参照的期望值（写死字面量：让「与串行一致」的比对不是自证）。
    fn expected_concurrent_view() -> ConcurrentView {
        ConcurrentView {
            root_kind: 8,
            id_kind: 2,
            id_value: 7,
            name_kind: 5,
            name_text: "并发".as_bytes().to_vec(),
            list_len: 3,
            list_items: vec![1, -2, 3],
            shared_node: true,
        }
    }

    /// 读回 [`ConcurrentView`]（借用视图，不持有引用）。
    fn observe_concurrent(root: *const AceValue) -> ConcurrentView {
        let items = field(root, 2);
        let mut list_len = 0usize;
        assert_eq!(ace_list_len(items, &mut list_len), ACE_OK);
        ConcurrentView {
            root_kind: kind_of(root),
            id_kind: kind_of(field(root, 0)),
            id_value: i64_of(field(root, 0)),
            name_kind: kind_of(field(root, 1)),
            name_text: text_of(field(root, 1)),
            list_len,
            list_items: (0..list_len)
                .map(|index| i64_of(list_item(items, index)))
                .collect(),
            shared_node: field(root, 2) as usize == field(root, 3) as usize,
        }
    }

    /// 一次完整的合法调用：**本线程**自建 `AceOptions` + schema 图，解码同一份 wire，读回观察
    /// 值，再释放本线程全部句柄（句柄不跨线程传）。失败时 `*out` 为空由 [`deserialize_with`]
    /// 断言；本函数返回该错误码与 `None`。
    fn concurrent_decode(bytes: &[u8]) -> (i32, Option<ConcurrentView>) {
        let options = options();
        let schema = concurrent_schema();
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        let (code, decoded) = deserialize_with(options, Some("demo.conc"), bytes);
        let view = (code == ACE_OK).then(|| observe_concurrent(decoded));
        if !decoded.is_null() {
            free(decoded);
        }
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
        (code, view)
    }

    /// 一次完整的**非法**调用：只注册 `demo.conc`，却按未注册的 `demo.conc_missing` 解码同一份
    /// 合法 wire → `3`；`*out` 为空由 [`deserialize_with`] 断言。
    fn concurrent_unknown_key(bytes: &[u8]) -> i32 {
        let options = options();
        let schema = concurrent_schema();
        assert_eq!(ace_options_add_schema(options, schema), ACE_OK);
        let (code, _) = deserialize_with(options, Some("demo.conc_missing"), bytes);
        assert_eq!(ace_schema_free(schema), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
        code
    }

    /// 一次完整的**非法**调用：schema 要 i32、值给 str → `13`；失败缓冲为空由
    /// [`serialize_with`] 断言。
    fn concurrent_type_mismatch() -> i32 {
        let options = options();
        let mismatch = schema_with(c"demo.conc_mismatch".as_ptr());
        assert_eq!(ace_schema_add_field(mismatch, 0, prim(ACE_FK_I32)), ACE_OK);
        assert_eq!(ace_options_add_schema(options, mismatch), ACE_OK);
        let wrong = object();
        set_owned(wrong, 0, str_value(b"text"));
        let (code, _) = serialize_with(options, Some("demo.conc_mismatch"), wrong);
        free(wrong);
        assert_eq!(ace_schema_free(mismatch), ACE_OK);
        assert_eq!(ace_options_free(options), ACE_OK);
        code
    }

    /// AC-A04①：不同独立句柄并发调用同一输入，结果与串行一致 —— 8 个线程各自
    /// `ace_options_new` + 自建 schema 图解码**同一份** wire（零拷贝入口，故直接共享同一片只读
    /// 字节），join 后逐个与串行参照比对；结束时再跑一次串行，证明并发没有留下残留状态。
    #[test]
    fn concurrent_independent_handles_match_serial_decode() {
        const THREADS: usize = 8;
        let wire = concurrent_wire();
        let bytes: &[u8] = &wire;

        let (code, reference) = concurrent_decode(bytes);
        assert_eq!(code, ACE_OK);
        let reference = reference.expect("串行参照必须解出值");
        assert_eq!(reference, expected_concurrent_view());

        let runs: Vec<(i32, Option<ConcurrentView>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| scope.spawn(move || concurrent_decode(bytes)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("工作线程不得 panic"))
                .collect()
        });
        assert_eq!(runs.len(), THREADS);
        for (index, run) in runs.iter().enumerate() {
            assert_eq!(run.0, ACE_OK, "线程 {index}: 解码必须成功");
            assert_eq!(
                run.1.as_ref(),
                Some(&reference),
                "线程 {index}: 结果必须与串行参照一致"
            );
        }

        // 并发之后同一实现再解一次：结果与第一轮串行完全相同。
        assert_eq!(concurrent_decode(bytes), (ACE_OK, Some(reference)));
    }

    /// 同一轮并发里每个线程的角色与结果。
    enum ConcurrentSlot {
        Legal(usize, (i32, Option<ConcurrentView>)),
        UnknownKey(usize, i32),
        TypeMismatch(usize, i32),
    }

    /// AC-A04②：一次失败不污染他次 —— 同一轮 8 个线程里 4 个合法解码、2 个未注册 key（`3`）、
    /// 2 个类型不匹配（`13`）交错执行；失败线程的输出为空（由 `deserialize_with`/
    /// `serialize_with` 断言），合法线程的结果仍与串行参照一致，结束后串行复跑不变。
    ///
    /// 合法线程与失败线程共用的是输入字节与代码，**不共用任何句柄**：options 与 schema 都在各
    /// 线程内自建、在线程内释放（共享同一 options 跨线程不是合法用法，本用例不引入）。
    #[test]
    fn concurrent_failures_do_not_contaminate_legal_calls() {
        const THREADS: usize = 8;
        let wire = concurrent_wire();
        let bytes: &[u8] = &wire;

        let (code, reference) = concurrent_decode(bytes);
        assert_eq!(code, ACE_OK);
        let reference = reference.expect("串行参照必须解出值");
        assert_eq!(reference, expected_concurrent_view());

        let slots: Vec<ConcurrentSlot> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|index| {
                    scope.spawn(move || match index % 4 {
                        0 | 1 => ConcurrentSlot::Legal(index, concurrent_decode(bytes)),
                        2 => ConcurrentSlot::UnknownKey(index, concurrent_unknown_key(bytes)),
                        _ => ConcurrentSlot::TypeMismatch(index, concurrent_type_mismatch()),
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("工作线程不得 panic"))
                .collect()
        });

        let (mut legal, mut unknown_key, mut type_mismatch) = (0usize, 0usize, 0usize);
        for slot in &slots {
            match slot {
                ConcurrentSlot::Legal(index, (code, view)) => {
                    legal += 1;
                    assert_eq!(*code, ACE_OK, "合法线程 {index}: 必须成功");
                    assert_eq!(
                        view.as_ref(),
                        Some(&reference),
                        "合法线程 {index}: 结果必须与串行参照一致"
                    );
                }
                ConcurrentSlot::UnknownKey(index, code) => {
                    unknown_key += 1;
                    assert_eq!(*code, 3, "未注册 key 线程 {index}: 期望 3");
                    assert_eq!(*code, Error::UnknownTypeKey.code());
                }
                ConcurrentSlot::TypeMismatch(index, code) => {
                    type_mismatch += 1;
                    assert_eq!(*code, 13, "类型不匹配线程 {index}: 期望 13");
                    assert_eq!(*code, Error::TypeMismatch.code());
                }
            }
        }
        assert_eq!(slots.len(), THREADS);
        let counts = (legal, unknown_key, type_mismatch);
        assert_eq!(counts, (4, 2, 2), "一轮里成功与失败必须交错");

        // 失败轮之后串行复跑：与第一轮串行结果完全相同（并发失败没有任何残留）。
        assert_eq!(concurrent_decode(bytes), (ACE_OK, Some(reference)));
    }
}
