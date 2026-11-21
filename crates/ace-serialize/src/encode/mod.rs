//! Assembly of flat records and depth-tagged slots.
use crate::error::{Error, Result};
use crate::limits::Options;
use crate::schema::AceEncode;
use crate::wire::{envelope, packed, slot::SLOT_SIZE};
#[cfg(feature = "std")]
use alloc::sync::Arc;
use alloc::{boxed::Box, collections::BTreeMap, rc::Rc, vec::Vec};
use core::any::TypeId;
use core::cell::Cell;

/// scratch 身份槽的私有 `unsafe` 模块（架构 §2.1.4）：全仓唯一允许新增 `unsafe` 的路径。
mod typed_scratch;
/// `encode_into` 的 scratch 承载路径（用户 2026-11-18 裁决：arena 简单版）。
mod scratch;

use scratch::ScratchState;
use typed_scratch::StoreCapView;

/// 需求 R27 的 `S`/`A` 的 crate 内转发点（公开入口在 crate 根）。
pub(crate) use typed_scratch::{identity_entry_align, identity_entry_size};

/// 信封头：raw 前缀两个 i32 长度字段，计入 raw 预算（与 wire/envelope.rs 的 encoded_len 一致）。
const RAW_HEADER_BYTES: usize = 8;
/// 记录头：i32 id 与 i32 size。
const RECORD_HEADER_BYTES: usize = 8;
/// 普通对象体：`u32 slot_offset + u32 slot_count`（FR-W06）。
const OBJECT_BODY_BYTES: usize = 8;
/// 多态对象体的逃生口 tag（FR-W07）：65535 专供显式 type_name，数值 tag 为 0..=65534。
const POLY_ESCAPE_TAG: u16 = 65535;

/// 多态对象体头（FR-W07）：`u16 variant_tag`，逃生口另有 `u32 type_name_record_id`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PolyHeader {
    tag: u16,
    type_name_id: Option<u32>,
}

impl PolyHeader {
    /// 对象范围之前的体字节数：数值 tag 2B，逃生口 2B+4B。
    fn header_bytes(self) -> usize {
        if self.type_name_id.is_some() {
            6
        } else {
            2
        }
    }
    /// 对象体总字节数：头加 `u32 slot_offset + u32 slot_count`。
    fn body_bytes(self) -> usize {
        self.header_bytes() + OBJECT_BODY_BYTES
    }
}

/// 打开的对象帧（设计 §1 第 2 项）：字段槽先累积在帧内，离开时整体成为一个对象块。
struct ObjectFrame {
    /// 字段槽的线上 depth，即 k+1，k 为进入前的 open（V4-1、V4-2）。
    depth: i32,
    /// 帧内最后写入的字段 tag，同一对象内严格递增。
    last_tag: Option<i32>,
    /// 帧内最后发射的字段 tag；发射时复查，同一对象内严格递增。
    last_emitted: Option<i32>,
    /// 进入对象前的 `current_order`，离开时恢复。
    saved_order: Option<u32>,
    /// 身份目标自身对象帧绑定的预留记录 id；普通对象帧为 `None`。
    target: Option<u32>,
    /// 帧内字段槽的线上字节，每槽 `SLOT_SIZE` 字节。
    slots: Vec<u8>,
    /// 多态对象体头；普通对象为 `None`（FR-W07）。
    poly: Option<PolyHeader>,
}

impl ObjectFrame {
    /// 对象体字节数：普通对象 8B；多态对象另加 u16 tag 与逃生口的 u32 type_name。
    fn body_bytes(&self) -> usize {
        self.poly.map_or(OBJECT_BODY_BYTES, PolyHeader::body_bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReferenceFamily {
    Rc,
    #[cfg(feature = "std")]
    Arc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct IdentityKey {
    family: ReferenceFamily,
    address: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityState {
    /// Weak 已预留记录 id，等待同消息内的强引用展开目标。
    Reserved,
    Active,
    Done,
}

struct IdentityEntry {
    id: u32,
    target: TypeId,
    state: IdentityState,
}

struct Capture {
    id: u32,
    state: CaptureState,
    entry: (usize, usize, usize),
}

enum CaptureState {
    Unsubmitted,
    Claimable,
    NativeCommitted,
    ObjectCommitted,
}

struct BufferedRecord {
    target_id: Option<u32>,
    body: Vec<u8>,
}

type BoundaryFingerprint = (
    Option<u32>,
    Option<(usize, Option<i32>, Option<i32>)>,
    Option<usize>,
);

/// 已离开对象的直接槽块。`finish` 时接入 assist，并回填对象记录体。
struct ObjectBlock {
    /// 对象记录体在 main 中的起始位置。
    body_pos: usize,
    slots: Vec<u8>,
    /// 多态对象体头；普通对象为 `None`。
    poly: Option<PolyHeader>,
}

/// 首错锁存槽（D1）：记住首个失败，并记住本次调用是否产生过 **`scratch` 容量不足以外** 的失败。
///
/// 后一项是需求 R29 的来源标注，只能在**产生点**记：公开 `Error` 不携带来源，`scratch` 不足、
/// `dst` 不足与其它失败同为 `1`，返回后无法区分（回调可以忽略容量失败、再返回另一个同码失败）。
/// 因此每个失败点自己声明类别，**不比较 `Error` 值**：`record_scratch_capacity` 只有真实
/// `scratch` 不足（`scratch::latch_scratch_shortfall`）会调，`record` 是其它一切（`dst` 容量点、
/// 非容量失败、资源限制……）。
///
/// 只要产生过「其它」失败，锁存首错绑定的 `scratch` 诊断就不再代表本次返回值，不得写进 `need`；
/// 纯 `scratch` 容量级联（先 `begin_element` 锁存、后 `leave_object` 也失败）仍属同一个归因，
/// 照常写。
///
/// 另有只标「其它」不锁首错的入口 [`FailureLatch::mark_other`]：失败已由调用方另行传播、不得替
/// 调用方决定 `finish` 返回值的产生点（derive 字段守卫，需求 R30）用它。
///
/// **本判据覆盖可在产生点标注的情形**：手写回调自行构造、不经过标注点的 `Err` 无从标注。
/// 其中**异码**失败由返回点的出口条件挡掉（`scratch` 不足只会返回 `1 InvalidArgument`，见
/// `encode_into_probe_impl`）；**同码** `Err(InvalidArgument)` 与「回调原样传播那条 `scratch`
/// 不足」在公开 `Result` 上仍不可区分，属已知限制。不得为了补这个洞改用「`Error` 值相等」或
/// 「末错」推断来源：公开码相同不代表同源，两者都已被既有反例否决（需求 R29）。
pub(crate) struct FailureLatch {
    first: Cell<Option<Error>>,
    /// 本次调用产生过 `scratch` 容量不足以外的失败：容量诊断不再代表本次返回值。
    other: Cell<bool>,
}

impl FailureLatch {
    const fn new() -> Self {
        Self {
            first: Cell::new(None),
            other: Cell::new(false),
        }
    }
    pub(crate) fn get(&self) -> Option<Error> {
        self.first.get()
    }
    /// 记录一个**其它**失败（`scratch` 容量不足以外，含 `dst` 容量点）并原样返回：未锁存时它就是
    /// 首错，已锁存时首错不变；两种情形都标记本次调用出现过其它失败。
    pub(crate) fn record(&self, error: Error) -> Error {
        self.other.set(true);
        if self.first.get().is_none() {
            self.first.set(Some(error));
        }
        error
    }
    /// 记录一个 **`scratch` 容量不足** 的失败并原样返回：不改变「本次调用是否出现其它失败」。
    pub(crate) fn record_scratch_capacity(&self, error: Error) -> Error {
        if self.first.get().is_none() {
            self.first.set(Some(error));
        }
        error
    }
    /// 只标「本次调用出现过其它失败」（`scratch` 容量不足以外），**不碰首错锁存**：用于失败已由
    /// 调用方另行传播、不得替调用方决定 `finish` 返回值的产生点（如 derive 字段守卫，需求 R30）。
    pub(crate) fn mark_other(&self) {
        self.other.set(true);
    }
    /// 测试专用：清空锁存（`finish_ignoring_latch`）。
    #[cfg(test)]
    fn clear(&self) {
        self.first.set(None);
        self.other.set(false);
    }
}

/// R23 写入接缝的尝试结果（`docs/architecture.md` §5「R23 写入接缝」的统一分派）。
///
/// 只要不是 [`BulkInto::Committed`]，调用方必须回到同一条既有分派（既有直写 → 旧
/// `&mut Vec<u8>` hook → 逐项编码）：新钩子准备失败不构成新的失败面，也不得抢占或锁存
/// 旧路径的错误。
pub(crate) enum BulkInto {
    /// 未启用、非 scratch 承载或试准备失败：未逻辑入栈任何记录、未锁存首错与容量诊断，且已撤销
    /// 本次的未提交试探预留（`B` 恢复到 `B_pre`）。按既有分派从 `begin_record` 起。
    NotStarted,
    /// 已打开记录上的新钩子返回 `Ok(None)`（身份目标路径）：未提交的试探预留已撤销、记录体描述符
    /// 恢复原样，在同一记录上继续既有分派。
    Declined,
    /// 已原位提交 `n` 字节（等于预期体长）。
    Committed(usize),
}

/// 单遍编码器。`'a` 是目的地借用与帧内身份存储能力借用的寿命形状（架构 §2.1.5）；
/// 公开的 `new`/`finish` 签名不变，拥有值路径不涉及调用方 `dst`/`scratch`。
pub struct Encoder<'a> {
    main: Vec<u8>,
    /// 根槽（assist[0]）的字节，无根槽时为空；对象块在 `finish` 时接在其后。
    assist: Vec<u8>,
    next_record_id: u32,
    committed_records: usize,
    identities: BTreeMap<IdentityKey, IdentityEntry>,
    captures: Vec<Capture>,
    /// 首次登记的身份以 Weak 保活分配，避免编码期间地址复用误认身份。
    identity_guards: Vec<Box<dyn core::any::Any>>,
    /// 打开的对象帧，栈顶为当前对象。
    frames: Vec<ObjectFrame>,
    /// 已离开的对象块，按离开次序排列（即记录提交次序，子先于父）。
    blocks: Vec<ObjectBlock>,
    /// 已打开的容器数（对象与集合），即 V4-1 的 open。
    open: usize,
    /// 槽计数（V4-6）：根预留或根槽，加上所有已压入的字段槽。
    slot_count: usize,
    /// 根预留名额：尚未写根时为 1，写根或 write_root_none 时清零；
    /// 仅用于 raw 字节预算区分「名额」与「实物」，不改变 slot_count 的计数语义（V4-12）。
    root_reserved: usize,
    root_written: bool,
    current_order: Option<u32>,
    opts: Options,
    /// 元素作用域可嵌套（列表套列表），按深度计数；内层结束不得清除外层的作用域。
    element_depth: usize,
    /// 与 element_depth 同步的外层字段选择栈：进入元素作用域时压入，离开时弹出并恢复。
    element_orders: Vec<Option<u32>>,
    record_bodies: Vec<BufferedRecord>,
    /// 捕获期间不可弹出的最低栈深，依次为对象帧数、record 数。
    scope_floor: (usize, usize),
    /// 首个失败（D1 锁存）：调用方忽略的编码子调用错误仍由 `finish` 返回。
    failure: FailureLatch,
    /// 与 `failure` **成对写入**的私有容量归因（需求 R29）：锁存的首错若来自真实 `scratch`
    /// 不足，这里记下该失败点的容量下界，否则为 `None`。
    ///
    /// 归因只属本编码器实例：回调换入 `Encoder::new` 后，新实例的归因是空的，返回点因此不会
    /// 继承旧实例的历史诊断。
    shortfall: Cell<Option<usize>>,
    /// 调用方目的地借用（架构 §2.1.5 的 `Output::Borrowed`）：`encode_into` 目的地的独占借用。
    /// 拥有值路径（`encode()`）的输出仍由 `main`/`assist` 组装，此时该字段为 `None`。
    dst: Option<&'a mut [u8]>,
    /// 帧内能力视图的借用（架构 §2.1.5 的 `StoreCap` 能力借用）：只有 crate 私有构造
    /// [`Encoder::borrowed`] 设置，拥有值路径恒为 `None`。
    ///
    /// 记成 `&'a mut StoreCapView`，而 `StoreCapView` 是 `typed_scratch` 内部的
    /// `dyn StoreCap + 'static`：能力视图是 **unsized** 的，父模块既不能把两次嵌套
    /// `with_store` 帧的所有者内容换掉（`mem::swap`/`mem::replace` 都要求 `Sized`），也不能
    /// 把内层帧的视图搬进外层帧的字段（`for<'cap>` 高阶约束）。对象寿命写死 `'static` 而不是
    /// 默认的 `'a`：`&'a mut (dyn + 'a)` 会把 `'a` 放进 `&mut` 的不变位置，而
    /// `&'a mut (dyn + 'static)` 对 `'a` 协变，公开 `Encoder<'a>` 因而仍可把
    /// `Encoder<'static>` 按值缩短（F-V）。
    /// `Encoder` 仍只借用能力：不拥有 `EntryStore`、不含条目计数、不负清理责任（§2.1.4）。
    store: Option<&'a mut StoreCapView>,
    /// arena 承载的工作状态（`encode_into` 路径）。拥有值路径恒为 `None`，继续用
    /// `Vec`/`BTreeMap`（用户 2026-11-18 裁决：公开 `new`/`finish` 与拥有值路径保持现状）。
    ws: Option<ScratchState>,
}

impl<'a> Encoder<'a> {
    pub fn new(opts: &Options) -> Self {
        Self {
            main: Vec::new(),
            assist: Vec::new(),
            next_record_id: 1,
            committed_records: 0,
            identities: BTreeMap::new(),
            captures: Vec::new(),
            identity_guards: Vec::new(),
            frames: Vec::new(),
            blocks: Vec::new(),
            open: 0,
            // 根槽名额在创建时预留并计入计数（V4-12）；`write_root_none` 释放该名额。
            slot_count: 1,
            root_reserved: 1,
            root_written: false,
            current_order: None,
            opts: opts.clone(),
            element_depth: 0,
            element_orders: Vec::new(),
            record_bodies: Vec::new(),
            scope_floor: (0, 0),
            failure: FailureLatch::new(),
            shortfall: Cell::new(None),
            dst: None,
            store: None,
            ws: None,
        }
    }
    pub(crate) fn options(&self) -> &Options {
        &self.opts
    }
    /// 记录首个失败并原样返回；已记录的首个失败不被覆盖（D1 锁存）。
    /// 本入口声明的是**其它**来源（`scratch` 容量不足以外，见 [`FailureLatch`]）。
    pub(crate) fn fail(&self, error: Error) -> Error {
        self.failure.record(error)
    }
    /// 首错锁存**实际选中**的那个失败点的 `scratch` 容量下界（需求 R29）。
    ///
    /// 只在探测入口的返回点调用：来源由控制流在**产生点**标注（[`FailureLatch`]），返回后不比较
    /// `Error` 值——公开码相同不代表同源。`Some` 当且仅当首错就是真实 `scratch` 不足、且本次调用
    /// 没出现过其它失败（含 `dst` 容量点）：`scratch::latch_scratch_shortfall` 只在它自己成为首错
    /// 时写该诊断，首错又从不被覆盖。缺后端（含替换编码器）、`dst` 不足与其它首错都不写 `need`。
    /// 返回点另有一层出口条件：只有最终返回 `Err(Error::InvalidArgument)` 时才可能写 `need`
    /// （`scratch` 不足只会返回 `1`）。判据覆盖范围与已知限制见 [`FailureLatch`]。
    pub(crate) fn latched_shortfall(&self) -> Option<usize> {
        if self.failure.other.get() {
            return None;
        }
        self.shortfall.get()
    }
    /// 记录数预算：已提交记录、已打开的父记录与未 leave 的对象帧，加本次待提交的一条。
    /// 纯 id 预留不占记录名额；begin_record 与 check_record_body 共用。
    fn check_record_count(&self) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_check_record_count();
        }
        let pending = self
            .record_bodies
            .len()
            .checked_add(self.frames.len())
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        let records = self
            .committed_records
            .checked_add(pending)
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        if records > self.opts.max_records || records > i32::MAX as usize {
            return Err(self.fail(Error::ResourceLimit));
        }
        Ok(())
    }
    /// 记录 id 预算是否满足（纯判定、不锁存）：供既有检查与 R23 试准备共用。
    fn record_id_ok(&self) -> bool {
        self.next_record_id != 0
            && self.next_record_id <= i32::MAX as u32
            && self.next_record_id.checked_add(1).is_some()
    }
    fn check_record_id(&self) -> Result<()> {
        if self.record_id_ok() {
            Ok(())
        } else {
            Err(self.fail(Error::ResourceLimit))
        }
    }
    /// 只预留统一编号，不写记录，也不占已提交记录数。
    fn reserve_record_id(&mut self) -> Result<u32> {
        self.check_record_id()?;
        let id = self.next_record_id;
        self.next_record_id = self
            .next_record_id
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        Ok(id)
    }
    /// Weak 首次到达仅预留身份；已有身份只复用 id，不展开目标。
    pub(crate) fn resolve_weak<T: 'static>(&mut self, value: &alloc::rc::Weak<T>) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_resolve_weak(value);
        }
        self.resolve_weak_identity::<T>(
            ReferenceFamily::Rc,
            alloc::rc::Weak::as_ptr(value) as usize,
            || Box::new(value.clone()),
        )
    }
    /// 首次强到达时展开 Rc 目标；Reserved 沿用预留 id，Done 复用 id，Active 为强环。
    pub(crate) fn resolve_rc<T: AceEncode + 'static>(&mut self, value: &Rc<T>) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_resolve_rc(value);
        }
        self.resolve_strong_identity(
            ReferenceFamily::Rc,
            Rc::as_ptr(value) as usize,
            || Box::new(Rc::downgrade(value)),
            &**value,
        )
    }
    /// 首次强到达时展开 Arc 目标，身份规则同 `resolve_rc`。
    #[cfg(feature = "std")]
    pub(crate) fn resolve_arc<T: AceEncode + 'static>(&mut self, value: &Arc<T>) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_resolve_arc(value);
        }
        self.resolve_strong_identity(
            ReferenceFamily::Arc,
            Arc::as_ptr(value) as usize,
            || Box::new(Arc::downgrade(value)),
            &**value,
        )
    }
    /// sync::Weak 首次到达仅预留身份，身份规则同 `resolve_weak`。
    #[cfg(feature = "std")]
    pub(crate) fn resolve_sync_weak<T: 'static>(
        &mut self,
        value: &alloc::sync::Weak<T>,
    ) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_resolve_sync_weak(value);
        }
        self.resolve_weak_identity::<T>(
            ReferenceFamily::Arc,
            alloc::sync::Weak::as_ptr(value) as usize,
            || Box::new(value.clone()),
        )
    }
    /// Weak 身份登记（Rc 与 Arc 共用），键为 (family, address)。
    fn resolve_weak_identity<T: 'static>(
        &mut self,
        family: ReferenceFamily,
        address: usize,
        guard: impl FnOnce() -> Box<dyn core::any::Any>,
    ) -> Result<u32> {
        let key = IdentityKey { family, address };
        let target = TypeId::of::<T>();
        if let Some(entry) = self.identities.get(&key) {
            if entry.target != target {
                return Err(self.fail(Error::InvalidReference));
            }
            return match entry.state {
                IdentityState::Reserved | IdentityState::Active | IdentityState::Done => {
                    Ok(entry.id)
                }
            };
        }
        let references = self
            .identities
            .len()
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        if references > self.opts.max_references {
            return Err(self.fail(Error::ResourceLimit));
        }
        let id = self.reserve_record_id()?;
        self.identity_guards.push(guard());
        self.identities.insert(
            key,
            IdentityEntry {
                id,
                target,
                state: IdentityState::Reserved,
            },
        );
        Ok(id)
    }
    /// 强引用身份登记与目标展开（Rc 与 Arc 共用），键为 (family, address)。
    fn resolve_strong_identity<T: AceEncode + 'static>(
        &mut self,
        family: ReferenceFamily,
        address: usize,
        guard: impl FnOnce() -> Box<dyn core::any::Any>,
        inner: &T,
    ) -> Result<u32> {
        let key = IdentityKey { family, address };
        let target = TypeId::of::<T>();
        let id = if let Some(entry) = self.identities.get_mut(&key) {
            if entry.target != target {
                return Err(self.fail(Error::InvalidReference));
            }
            match entry.state {
                IdentityState::Active => return Err(self.fail(Error::CycleUnsupported)),
                IdentityState::Done => return Ok(entry.id),
                IdentityState::Reserved => {
                    entry.state = IdentityState::Active;
                    entry.id
                }
            }
        } else {
            let references = self
                .identities
                .len()
                .checked_add(1)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?;
            if references > self.opts.max_references {
                return Err(self.fail(Error::ResourceLimit));
            }
            let id = self.reserve_record_id()?;
            self.identity_guards.push(guard());
            self.identities.insert(
                key,
                IdentityEntry {
                    id,
                    target,
                    state: IdentityState::Active,
                },
            );
            id
        };
        self.captures.push(Capture {
            id,
            state: CaptureState::Unsubmitted,
            entry: (0, 0, 0),
        });
        let encoded = inner.ace_encode_target(self);
        let capture = self.captures.pop().unwrap();
        encoded?;
        if !matches!(
            capture.state,
            CaptureState::NativeCommitted | CaptureState::ObjectCommitted
        ) {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.identities.get_mut(&key).unwrap().state = IdentityState::Done;
        Ok(id)
    }
    /// 记录捕获边界的字段选择、栈顶对象帧槽状态与栈顶 record 体长度。
    fn boundary_fingerprint(&self) -> BoundaryFingerprint {
        if self.ws.is_some() {
            return self.scratch_boundary_fingerprint();
        }
        (
            self.current_order,
            self.frames
                .last()
                .map(|frame| (frame.slots.len(), frame.last_tag, frame.last_emitted)),
            self.record_bodies.last().map(|record| record.body.len()),
        )
    }
    pub(crate) fn capture_native(
        &mut self,
        f: impl FnOnce(&mut Encoder<'_>) -> Result<()>,
    ) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_capture_native(f);
        }
        let capture = self
            .captures
            .last()
            .ok_or_else(|| self.fail(Error::InvalidArgument))?;
        if !matches!(capture.state, CaptureState::Unsubmitted) {
            return Err(self.fail(Error::InvalidArgument));
        }
        let id = capture.id;
        let boundary = self.boundary_fingerprint();
        let entry = (
            self.frames.len(),
            self.record_bodies.len(),
            self.element_depth,
        );
        let previous_floor = core::mem::replace(&mut self.scope_floor, (entry.0, entry.1));
        let committed = (|| {
            self.begin_target_record(id)?;
            self.begin_element();
            let encoded = f(self);
            if encoded.is_ok() {
                let target_bound = self
                    .record_bodies
                    .last()
                    .is_some_and(|record| record.target_id == Some(id));
                let current = (
                    self.frames.len(),
                    self.record_bodies.len(),
                    self.element_depth,
                );
                if !target_bound || current != (entry.0, entry.1 + 1, entry.2 + 1) {
                    return Err(self.fail(Error::InvalidArgument));
                }
            }
            self.end_element();
            let committed = encoded.and_then(|()| self.end_record())?;
            if committed != id {
                return Err(self.fail(Error::InvalidArgument));
            }
            Ok(())
        })();
        self.scope_floor = previous_floor;
        committed?;
        if self.boundary_fingerprint() != boundary {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.captures.last_mut().unwrap().state = CaptureState::NativeCommitted;
        Ok(())
    }
    /// 将当前捕获标记为可领取的对象目标，并校验对象提交及进入时的作用域边界。
    pub(crate) fn capture_object(
        &mut self,
        f: impl FnOnce(&mut Encoder<'_>) -> Result<()>,
    ) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_capture_object(f);
        }
        let capture = self
            .captures
            .last()
            .ok_or_else(|| self.fail(Error::InvalidArgument))?;
        if !matches!(capture.state, CaptureState::Unsubmitted) {
            return Err(self.fail(Error::InvalidArgument));
        }
        let id = capture.id;
        let entry = (
            self.frames.len(),
            self.record_bodies.len(),
            self.element_depth,
        );
        let capture = self.captures.last_mut().unwrap();
        capture.state = CaptureState::Claimable;
        capture.entry = entry;
        let boundary = self.boundary_fingerprint();
        let previous_floor = core::mem::replace(&mut self.scope_floor, (entry.0, entry.1));
        let encoded = f(self);
        self.scope_floor = previous_floor;
        encoded?;
        let capture = self
            .captures
            .last()
            .ok_or_else(|| self.fail(Error::InvalidArgument))?;
        if capture.id != id || !matches!(capture.state, CaptureState::ObjectCommitted) {
            return Err(self.fail(Error::InvalidArgument));
        }
        if entry
            != (
                self.frames.len(),
                self.record_bodies.len(),
                self.element_depth,
            )
        {
            return Err(self.fail(Error::InvalidArgument));
        }
        if self.boundary_fingerprint() != boundary {
            return Err(self.fail(Error::InvalidArgument));
        }
        Ok(())
    }
    fn begin_target_record(&mut self, id: u32) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_begin_target_record(id);
        }
        if id == 0 || id > i32::MAX as u32 || id >= self.next_record_id {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.check_record_count()?;
        self.check_buffered_bytes(RECORD_HEADER_BYTES)?;
        self.record_bodies.push(BufferedRecord {
            target_id: Some(id),
            body: Vec::new(),
        });
        Ok(())
    }
    /// Start a buffered record. Records must be closed in strict LIFO order.
    pub fn begin_record(&mut self) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_begin_record();
        }
        self.check_record_count()?;
        self.check_record_id()?;
        self.check_buffered_bytes(RECORD_HEADER_BYTES)?;
        self.record_bodies.push(BufferedRecord {
            target_id: None,
            body: Vec::new(),
        });
        Ok(())
    }
    /// raw 字节预算（计整个 envelope）：信封头、主区、已打开记录（头与体）、未 leave 对象帧的
    /// 占位记录、全部已压入槽（仅含已写根槽，不含根预留名额）及本次追加。这些项为成功输出的下界。
    fn check_buffered_bytes(&self, additional: usize) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_check_buffered_bytes(additional);
        }
        let pending = self
            .frames
            .iter()
            .try_fold(0usize, |n, frame| {
                n.checked_add(RECORD_HEADER_BYTES)?
                    .checked_add(frame.body_bytes())
            })
            .and_then(|n| {
                n.checked_add(
                    self.slot_count
                        .checked_sub(self.root_reserved)?
                        .checked_mul(SLOT_SIZE)?,
                )
            })
            .and_then(|n| n.checked_add(RAW_HEADER_BYTES))
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        let total = self
            .record_bodies
            .iter()
            .try_fold(self.main.len(), |n, record| {
                n.checked_add(RECORD_HEADER_BYTES)?
                    .checked_add(record.body.len())
            })
            .and_then(|n| n.checked_add(pending))
            .and_then(|n| n.checked_add(additional))
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        if total > self.opts.max_raw_bytes
            || total > i32::MAX as usize
            || total > isize::MAX as usize
        {
            return Err(self.fail(Error::ResourceLimit));
        }
        Ok(())
    }
    /// Append to the current record body; no record must be opened implicitly.
    pub fn write_body_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_write_body_bytes(bytes);
        }
        if self.record_bodies.is_empty() {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.check_buffered_bytes(bytes.len())?;
        self.record_bodies
            .last_mut()
            .unwrap()
            .body
            .extend_from_slice(bytes);
        Ok(())
    }
    pub fn write_element_id(&mut self, id: u32) -> Result<()> {
        self.write_body_bytes(&id.to_le_bytes())
    }
    pub fn in_record(&self) -> bool {
        if self.ws.is_some() {
            return self.scratch_in_record();
        }
        !self.record_bodies.is_empty()
    }
    /// Close the top record, committing it flat; begin/end must be paired LIFO.
    pub fn end_record(&mut self) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_end_record();
        }
        if self.record_bodies.len() <= self.scope_floor.1 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let record = self
            .record_bodies
            .pop()
            .ok_or_else(|| self.fail(Error::InvalidArgument))?;
        let committed = match record.target_id {
            Some(id) => self.commit_target_record(id, &record.body),
            None => self.alloc_record(&record.body),
        };
        match committed {
            Ok(id) => Ok(id),
            Err(error) => {
                self.record_bodies.push(record);
                Err(error)
            }
        }
    }
    /// 进入元素作用域：暂存外层字段选择并清空，使元素自身的对象不写字段槽（id 由容器写入）。
    pub fn begin_element(&mut self) {
        if self.ws.is_some() {
            self.scratch_begin_element();
            return;
        }
        self.element_depth = self.element_depth.saturating_add(1);
        self.element_orders.push(self.current_order.take());
    }
    /// 离开元素作用域：恢复进入时暂存的外层字段选择；嵌套元素各对应一项，内层结束不影响外层。
    pub fn end_element(&mut self) {
        if self.ws.is_some() {
            self.scratch_end_element();
            return;
        }
        self.element_depth = self.element_depth.saturating_sub(1);
        self.current_order = self.element_orders.pop().flatten();
    }
    pub fn write_element_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if self.element_depth == 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.write_body_bytes(bytes)
    }
    /// 在整段复制前复用记录预算检查，失败不改变编码器状态。
    /// 记录数与字节均计入已打开的父记录，与 begin_record 同口径。
    pub(crate) fn check_record_body(&self, body_len: usize) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_check_record_body(body_len);
        }
        i32::try_from(body_len).map_err(|_| self.fail(Error::ResourceLimit))?;
        self.check_record_count()?;
        self.check_record_id()?;
        self.check_buffered_bytes(
            body_len
                .checked_add(8)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )
    }
    /// 身份目标体的新增字节预检（设计 r4）：只计本次新增的体字节。目标记录的名额与记录头已由
    /// `begin_target_record` 计入，故此处不调用 `check_record_count`，也不再计 `RECORD_HEADER_BYTES`。
    pub(crate) fn check_body_additional(&self, additional: usize) -> Result<()> {
        self.check_buffered_bytes(additional)
    }
    /// R23 新钩子的常规记录接缝（§5①、B1）：试准备与物理预留都走**不锁存**路径，只在**新钩子
    /// 真正接管**时才逻辑入栈这条记录；否则把已预留的记录体留给既有分派的下一条记录复用。
    ///
    /// 只有 scratch 承载路径能交出这段（§5①）：拥有值路径的剩余容量不是已初始化字节，故
    /// 不启用、也不打开记录。任何准备失败（预算不足、预留不下、取段越界/交叠）返回
    /// `NotStarted`，调用方按既有分派重新走 `begin_record` 与旧 hook，**不锁存也不抢占**旧路径
    /// 的错误。
    pub(crate) fn bulk_into_new_record<T>(
        &mut self,
        items: &[T],
        body_len: usize,
        hook: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
    ) -> Result<BulkInto> {
        if self.ws.is_none() {
            return Ok(BulkInto::NotStarted);
        }
        self.scratch_bulk_into_new_record(items, body_len, hook)
    }
    /// R23 新钩子的身份目标接缝（§5①）：目标记录已由 `begin_target_record` 打开，只做预留、
    /// 借出与提交；试准备失败同样不锁存。
    pub(crate) fn bulk_into_open_record<T>(
        &mut self,
        items: &[T],
        body_len: usize,
        hook: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
    ) -> Result<BulkInto> {
        if self.ws.is_none() {
            return Ok(BulkInto::NotStarted);
        }
        self.scratch_bulk_into_body(items, body_len, hook)
    }
    pub fn alloc_record(&mut self, body: &[u8]) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_alloc_record(body);
        }
        self.check_record_body(body.len())?;
        Ok(self.alloc_record_unchecked(body))
    }
    fn commit_target_record(&mut self, id: u32, body: &[u8]) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_commit_target_record(id, body);
        }
        if id == 0 || id > i32::MAX as u32 || id >= self.next_record_id {
            return Err(self.fail(Error::InvalidArgument));
        }
        i32::try_from(body.len()).map_err(|_| self.fail(Error::ResourceLimit))?;
        self.check_record_count()?;
        self.check_buffered_bytes(
            body.len()
                .checked_add(RECORD_HEADER_BYTES)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )?;
        self.append_record_unchecked(id, body);
        Ok(id)
    }
    /// 仅由已完成记录长度、记录数、id 与 raw 字节检查的入口调用。
    fn append_record_unchecked(&mut self, id: u32, body: &[u8]) {
        let size = body.len() as i32;
        self.main.extend_from_slice(&(id as i32).to_le_bytes());
        self.main.extend_from_slice(&size.to_le_bytes());
        self.main.extend_from_slice(body);
        self.committed_records += 1;
    }
    /// 仅由已完成记录长度、记录数、id 与 raw 字节检查的入口调用，避免联合预算检查后重复检查。
    fn alloc_record_unchecked(&mut self, body: &[u8]) -> u32 {
        let id = self.next_record_id;
        self.append_record_unchecked(id, body);
        self.next_record_id += 1;
        id
    }
    /// Reject a collection length beyond `max_collection_len` or the u32 count range.
    /// Call before writing the count or any element.
    pub(crate) fn check_collection_len(&self, len: usize) -> Result<()> {
        if len > self.opts.max_collection_len || len > u32::MAX as usize {
            return Err(self.fail(Error::ResourceLimit));
        }
        Ok(())
    }
    /// 压入 `additional` 个槽前的计数检查（V4-6）：计数不得超过 `max_slots`，字节长度须在 i32 与 isize 范围内。
    fn check_slot_budget(&self, additional: usize) -> Result<()> {
        let count = self
            .slot_count
            .checked_add(additional)
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        let len = count
            .checked_mul(SLOT_SIZE)
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        if count > self.opts.max_slots || len > i32::MAX as usize || len > isize::MAX as usize {
            return Err(self.fail(Error::ResourceLimit));
        }
        Ok(())
    }
    /// 写根槽前的检查：元素作用域内不得写根槽；根槽名额已计入计数，此处不再增加。
    fn check_root_slot(&self) -> Result<()> {
        if self.element_depth != 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.check_slot_budget(0)
    }
    /// 写入当前对象帧的一个字段槽；`depth` 须等于该帧的字段深度（V4-2），否则返回 `InvalidArgument`。
    /// 无对象帧时同样返回 `InvalidArgument`。
    pub(crate) fn push_slot(&mut self, depth: i32, tag: i32, payload: [u8; 8]) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_push_slot(depth, tag, payload);
        }
        self.check_slot_budget(1)?;
        // 改动对象帧前复查发射顺序，失败不改变状态（R-PUSH-01 / I4）。
        self.check_emit_order(tag)?;
        // 槽字节（SLOT_SIZE）先计入 raw 预算再写入帧，超限不分配（AC-E02）。
        self.check_buffered_bytes(SLOT_SIZE)?;
        let Some(frame) = self.frames.last() else {
            return Err(self.fail(Error::InvalidArgument));
        };
        if depth != frame.depth {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.push_slot_unchecked(tag, payload);
        Ok(())
    }
    /// 把字段槽追加到栈顶对象帧并计入槽数；调用方须已完成深度、发射顺序、槽计数与 raw 字节检查。
    fn push_slot_unchecked(&mut self, tag: i32, payload: [u8; 8]) {
        let frame = self.frames.last_mut().unwrap();
        frame.last_emitted = Some(tag);
        frame.slots.extend_from_slice(&frame.depth.to_le_bytes());
        frame.slots.extend_from_slice(&tag.to_le_bytes());
        frame.slots.extend_from_slice(&payload);
        self.slot_count += 1;
    }
    /// 根写入前的检查：根槽未写过，且**实际后端**没有已打开的对象帧。
    ///
    /// 「已打开的对象帧」取自当前承载：拥有值路径看 `frames`，借用路径看 arena 内的帧描述符；
    /// 只看拥有值字段会使借用路径打开对象后仍能写根槽，与拥有值路径的可观察行为不等价。
    fn check_root(&self) -> Result<()> {
        let object_open = if self.ws.is_some() {
            self.scratch_frames_len() > 0
        } else {
            !self.frames.is_empty()
        };
        if self.root_written || object_open {
            Err(self.fail(Error::InvalidArgument))
        } else {
            Ok(())
        }
    }
    pub fn write_root_none(&mut self) -> Result<()> {
        self.check_root()?;
        // None 根不写根槽，释放创建时预留的名额（V4-12）。
        self.slot_count -= 1;
        self.root_reserved = 0;
        self.root_written = true;
        Ok(())
    }
    pub fn write_root_scalar(&mut self, payload: [u8; 8]) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_write_root_scalar(payload);
        }
        self.check_root()?;
        self.check_root_slot()?;
        self.check_buffered_bytes(SLOT_SIZE)?;
        self.write_root_scalar_unchecked(payload);
        Ok(())
    }
    /// 仅由已完成根、槽计数与 raw 字节检查的入口调用，避免联合预算检查后重复检查。
    fn write_root_scalar_unchecked(&mut self, payload: [u8; 8]) {
        // 根槽的 depth 与 tag 均为 0，载荷位于字节 8..16。
        let mut slot = [0u8; SLOT_SIZE];
        slot[8..].copy_from_slice(&payload);
        self.assist = slot.to_vec();
        self.root_reserved = 0;
        self.root_written = true;
    }
    pub fn write_root_record(&mut self, body: &[u8]) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_write_root_record(body);
        }
        self.check_root()?;
        self.check_root_slot()?;
        i32::try_from(body.len()).map_err(|_| self.fail(Error::ResourceLimit))?;
        self.check_record_count()?;
        self.check_record_id()?;
        self.check_buffered_bytes(
            body.len()
                .checked_add(RECORD_HEADER_BYTES + SLOT_SIZE)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )?;
        let id = self.alloc_record_unchecked(body);
        self.write_root_scalar_unchecked(u64::from(id).to_le_bytes());
        Ok(())
    }
    fn check_order(&self, order: u32) -> Result<i32> {
        if self.ws.is_some() {
            return self.scratch_check_order(order);
        }
        let frame = self
            .frames
            .last()
            .ok_or_else(|| self.fail(Error::InvalidArgument))?;
        let tag = i32::try_from(order).map_err(|_| self.fail(Error::InvalidArgument))?;
        if frame.last_tag.is_some_and(|last| tag <= last) {
            return Err(self.fail(Error::InvalidArgument));
        }
        Ok(tag)
    }
    /// 发射复查（R-PUSH-01 / I4）：tag 须严格大于当前对象帧已发射的 tag；只读，失败不改状态。
    fn check_emit_order(&self, tag: i32) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_check_emit_order(tag);
        }
        let frame = self
            .frames
            .last()
            .ok_or_else(|| self.fail(Error::InvalidArgument))?;
        if frame.last_emitted.is_some_and(|last| tag <= last) {
            return Err(self.fail(Error::InvalidArgument));
        }
        Ok(())
    }
    fn commit_order(&mut self, tag: i32) {
        if self.ws.is_some() {
            self.scratch_commit_order(tag);
            return;
        }
        self.frames.last_mut().unwrap().last_tag = Some(tag);
    }
    /// Select a field and validate strictly increasing orders within the current object.
    pub fn begin_field(&mut self, order: u32) -> Result<()> {
        let tag = self.check_order(order)?;
        self.commit_order(tag);
        self.current_order = Some(order);
        Ok(())
    }
    pub fn current_order(&self) -> Option<u32> {
        self.current_order
    }
    /// 字段发射后的协议校验：字段类型编码完毕必须已消费当前字段选择（`encode_scalar` /
    /// `encode_none` / `encode_record` 三者之一），否则本帧的字段声明与发射不一致。
    ///
    /// 本方法是 derive 字段守卫的唯一产生点，也是**已批准并登记的公开面增量**（需求 R30；
    /// `#[doc(hidden)]`，与既有 derive 专用接缝同型），供 `derive` 生成代码在字段编码后校验
    /// 「当前字段选择已被消费」：失败必须在这里产生并标为**其它**来源（[`FailureLatch::mark_other`]，
    /// 即 `scratch` 容量不足以外）。同一字段的发射刚因 `scratch` 不足被锁存、又被回调忽略时，本条
    /// 仍说明本次调用不是纯容量级联，锁存的容量诊断不得代表返回值。
    ///
    /// 本条**只标「其它」、不写首错锁存**：错误由 derive 生成代码原样传播，是否让本次调用失败由
    /// 调用方决定；若替调用方锁存首错，外层回调丢弃本错误后原本应返回的异码（如 `6 ResourceLimit`）
    /// 会被改写成 `1`。
    ///
    /// 错误码与优先级不变：与 `encode_scalar` 在旧选择上的拒绝相同，都是既有 `1 InvalidArgument`；
    /// 该公开面增量**不改变**错误码、优先级、`encode_into` 行为与回调调用次数。
    #[doc(hidden)]
    pub fn check_field_consumed(&self) -> Result<()> {
        if self.current_order.is_some() {
            self.failure.mark_other();
            return Err(Error::InvalidArgument);
        }
        Ok(())
    }
    /// 当前对象帧的字段槽深度（k+1）；无对象帧时返回 `InvalidArgument`。
    fn field_depth(&self) -> Result<i32> {
        if self.ws.is_some() {
            return self.scratch_field_depth();
        }
        self.frames
            .last()
            .map(|frame| frame.depth)
            .ok_or_else(|| self.fail(Error::InvalidArgument))
    }
    pub fn encode_scalar(&mut self, payload: [u8; 8]) -> Result<()> {
        match self.current_order {
            Some(order) => {
                self.push_slot(self.field_depth()?, order as i32, payload)?;
                // 发射一个字段槽后消费当前字段选择，同一字段不得二次发射（R-PUSH-01）。
                self.current_order = None;
                Ok(())
            }
            None => self.write_root_scalar(payload),
        }
    }
    pub fn encode_record(&mut self, body: &[u8]) -> Result<()> {
        match self.current_order {
            Some(order) => {
                let depth = self.field_depth()?;
                self.check_slot_budget(1)?;
                // 分配记录前复查发射顺序，避免拒绝时留下未引用记录（R-PUSH-01 / I4）。
                self.check_emit_order(order as i32)?;
                // 记录与其槽须一并通过 raw 预算，否则 push_slot 可能在记录已追加后失败（R-PUSH-01）。
                self.check_buffered_bytes(
                    body.len()
                        .checked_add(RECORD_HEADER_BYTES + SLOT_SIZE)
                        .ok_or_else(|| self.fail(Error::ResourceLimit))?,
                )?;
                let id = self.alloc_record(body)?;
                self.push_slot(depth, order as i32, u64::from(id).to_le_bytes())?;
                // 与 encode_scalar 相同：发射后消费当前字段选择（R-PUSH-01）。
                self.current_order = None;
                Ok(())
            }
            None => self.write_root_record(body),
        }
    }
    /// 字段位置的 None 消费字段选择（同一字段不得再发射）；无字段选择时写根 None。
    pub fn encode_none(&mut self) -> Result<()> {
        if self.current_order.take().is_some() {
            Ok(())
        } else {
            self.write_root_none()
        }
    }
    pub fn write_field_scalar(&mut self, order: u32, payload: [u8; 8]) -> Result<()> {
        let tag = self.check_order(order)?;
        self.push_slot(self.field_depth()?, tag, payload)?;
        self.commit_order(tag);
        Ok(())
    }
    pub fn write_field_record(&mut self, order: u32, body: &[u8]) -> Result<()> {
        let tag = self.check_order(order)?;
        let depth = self.field_depth()?;
        self.check_slot_budget(1)?;
        // 记录与其槽须一并通过 raw 预算，否则 push_slot 可能在记录已追加后失败（R-PUSH-01）。
        self.check_buffered_bytes(
            body.len()
                .checked_add(RECORD_HEADER_BYTES + SLOT_SIZE)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )?;
        let id = self.alloc_record(body)?;
        self.push_slot(depth, tag, u64::from(id).to_le_bytes())?;
        self.commit_order(tag);
        Ok(())
    }
    pub fn write_field_none(&mut self, order: u32) -> Result<()> {
        let tag = self.check_order(order)?;
        self.commit_order(tag);
        Ok(())
    }
    pub fn write_field_option_scalar(
        &mut self,
        order: u32,
        payload: Option<[u8; 8]>,
    ) -> Result<()> {
        match payload {
            Some(value) => self.write_field_scalar(order, value),
            None => self.write_field_none(order),
        }
    }
    /// 只读规划一次对象进入（V4-1）：深度与根检查、帧深度、进入后的 open、捕获目标。
    /// 失败不改变编码器状态，使进入多态对象前写 type_name 记录的前置检查可复用。
    fn object_frame_plan(&self) -> Result<(i32, usize, Option<u32>)> {
        if self.ws.is_some() {
            return self.scratch_object_frame_plan();
        }
        let k = self.open;
        if k > self.opts.max_depth {
            return Err(self.fail(Error::DepthExceeded));
        }
        if k == 0 {
            self.check_root()?;
        }
        let depth = k
            .checked_add(1)
            .and_then(|depth| i32::try_from(depth).ok())
            .ok_or_else(|| self.fail(Error::DepthExceeded))?;
        let next_open = self
            .open
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::DepthExceeded))?;
        let entry = (
            self.frames.len(),
            self.record_bodies.len(),
            self.element_depth,
        );
        let mut target = None;
        if let Some(capture) = self.captures.last() {
            if capture.entry == entry {
                match capture.state {
                    CaptureState::Claimable => target = Some(capture.id),
                    CaptureState::ObjectCommitted => return Err(self.fail(Error::InvalidArgument)),
                    _ => {}
                }
            }
        }
        Ok((depth, next_open, target))
    }
    /// 进入对象（V4-1）：k 为进入前的 open；先检查深度与根，二者均先于任何写入。
    pub fn enter_object(&mut self) -> Result<()> {
        self.enter_object_with(None)
    }
    /// 进入带体头的对象（FR-W06、FR-W07）：深度与根检查及帧结构与 `enter_object` 相同，
    /// 仅对象体在槽范围前多写多态头。
    fn enter_object_with(&mut self, poly: Option<PolyHeader>) -> Result<()> {
        if self.ws.is_some() {
            return self.scratch_enter_object_with(poly);
        }
        let (depth, next_open, target) = self.object_frame_plan()?;
        self.open = next_open;
        self.frames.push(ObjectFrame {
            depth,
            last_tag: None,
            last_emitted: None,
            saved_order: self.current_order,
            target,
            slots: Vec::new(),
            poly,
        });
        self.current_order = None;
        Ok(())
    }
    /// 进入多态对象（FR-W07）：`tag` 0..=65534 为数值判别；65535 为 type_name 逃生口，
    /// 必须给出显式别名，并以既有字符串记录机制写一条 UTF-8 记录。
    /// 头部与 tag 不一致（65535 无别名、数值 tag 带别名）报 `InvalidArgument`。
    pub fn enter_polymorphic_object(&mut self, tag: u16, type_name: Option<&str>) -> Result<()> {
        // 先做只读规划：拒绝时不写任何记录（R-PUSH-01 的原子性）。
        self.object_frame_plan()?;
        let type_name_id = match (tag, type_name) {
            (POLY_ESCAPE_TAG, Some(name)) => Some(self.alloc_record(name.as_bytes())?),
            (POLY_ESCAPE_TAG, None) | (_, Some(_)) => return Err(self.fail(Error::InvalidArgument)),
            (_, None) => None,
        };
        self.enter_object_with(Some(PolyHeader { tag, type_name_id }))
    }
    /// 离开对象：弹出帧并分配带体头占位的对象记录（普通 8B、多态 10B/14B，FR-W06、FR-W07），返回其 record id。
    /// 离开后 open 为 0 的根对象，
    /// 其 record id 写入根槽（V4-7）；字段位置的普通对象，其 record id 写入父帧的字段槽。
    /// 元素作用域内的对象自身不写字段槽（id 由容器写入）；元素内部嵌套的字段对象仍按字段位置写槽。
    pub fn leave_object(&mut self) -> Result<u32> {
        if self.ws.is_some() {
            return self.scratch_leave_object();
        }
        if self.frames.len() <= self.scope_floor.0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let frame = self
            .frames
            .pop()
            .ok_or_else(|| self.fail(Error::InvalidArgument))?;
        self.open = self.open.saturating_sub(1);
        self.current_order = frame.saved_order;
        // 对象体的占位长度由体头决定（FR-W06、FR-W07）：普通 8B，多态 10B 或 14B。
        let body_bytes = frame.body_bytes();
        let placeholder = [0u8; 6 + OBJECT_BODY_BYTES];
        let body = &placeholder[..body_bytes];
        if let Some(id) = frame.target {
            let capture = self
                .captures
                .last()
                .ok_or_else(|| self.fail(Error::InvalidArgument))?;
            if capture.id != id || !matches!(capture.state, CaptureState::Claimable) {
                return Err(self.fail(Error::InvalidArgument));
            }
            self.commit_target_record(id, body)?;
            self.blocks.push(ObjectBlock {
                body_pos: self.main.len() - body_bytes,
                slots: frame.slots,
                poly: frame.poly,
            });
            self.captures.last_mut().unwrap().state = CaptureState::ObjectCommitted;
            return Ok(id);
        }
        self.check_record_count()?;
        self.check_record_id()?;
        // 根层元素作用域内的普通对象不写根槽，其 id 由外层元素写入（S3-G R3）。
        let writes_root = self.open == 0 && self.element_depth == 0;
        // 字段位置的普通对象把 id 写入父帧字段槽，槽序取待写字段选择（设计 §四第 1 项）。
        // 元素自身的对象进入时字段选择已被暂存清空，故不写槽；元素内部嵌套的字段对象照常写槽。
        let field_tag = match self.current_order {
            Some(order) if !writes_root => Some(order as i32),
            _ => None,
        };
        let need = if writes_root {
            self.check_root()?;
            self.check_root_slot()?;
            RECORD_HEADER_BYTES + body_bytes + SLOT_SIZE
        } else if let Some(tag) = field_tag {
            // 字段槽的检查先于分配占位记录，拒绝时不留下未被引用的记录（R-PUSH-01 / I4）。
            self.check_slot_budget(1)?;
            self.check_emit_order(tag)?;
            RECORD_HEADER_BYTES + body_bytes + SLOT_SIZE
        } else {
            RECORD_HEADER_BYTES + body_bytes
        };
        self.check_buffered_bytes(need)?;
        let id = self.alloc_record_unchecked(body);
        // alloc_record_unchecked 把记录体追加在 main 末尾，占位体即末尾 `body_bytes` 字节。
        self.blocks.push(ObjectBlock {
            body_pos: self.main.len() - body_bytes,
            slots: frame.slots,
            poly: frame.poly,
        });
        if writes_root {
            self.write_root_scalar_unchecked(u64::from(id).to_le_bytes());
        } else if let Some(tag) = field_tag {
            self.push_slot_unchecked(tag, u64::from(id).to_le_bytes());
            // 与 encode_scalar 相同：发射后消费当前字段选择，同一字段不得二次发射（R-PUSH-01）。
            self.current_order = None;
        }
        Ok(id)
    }
    /// 进入一层嵌套容器（集合）：k 为进入前的 open，超过 `max_depth` 时返回 `DepthExceeded`。
    pub(crate) fn enter_collection(&mut self) -> Result<()> {
        if self.open > self.opts.max_depth {
            return Err(self.fail(Error::DepthExceeded));
        }
        self.open = self
            .open
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::DepthExceeded))?;
        Ok(())
    }
    /// 离开一层嵌套容器，须与 `enter_collection` 成对调用。
    pub(crate) fn leave_collection(&mut self) {
        self.open = self.open.saturating_sub(1);
    }
    /// **FFI 专用接缝**：按集合记录形态写一条新记录并返回其记录 id，使跨 crate 的适配器
    /// 在不复制集合编码语义的前提下拿到 id（`Vec`/`BTreeMap` 的内建实现用同一套步骤）。
    ///
    /// 记录体为 `u32 count` + `body` 逐项写出的元素，元素位于元素作用域内；深度
    /// （`max_depth` → `5`）、集合长度（`max_collection_len` → `6`）与记录/字节预算的判定
    /// 与内建集合实现同源，任何一步失败都先 `leave_collection` 再返回错误。
    ///
    /// 非 FFI 调用方不应依赖本接缝：它不含定宽元素的整段写入（bulk）路径，也不做映射键预算
    /// 检查（由调用方按 `AceElement::check_map_key` 自行判定），这两项仍属 `Vec`/`BTreeMap`
    /// 内建实现的职责。
    #[doc(hidden)]
    pub fn encode_collection(
        &mut self,
        len: usize,
        body: impl FnOnce(&mut Encoder<'_>) -> Result<()>,
    ) -> Result<u32> {
        self.enter_collection()?;
        let encoded = (|| {
            self.check_collection_len(len)?;
            self.begin_record()?;
            // `check_collection_len` 已排除超出 `u32` 计数的长度。
            self.write_body_bytes(&(len as u32).to_le_bytes())?;
            self.begin_element();
            let elements = body(self);
            self.end_element();
            elements?;
            self.end_record()
        })();
        self.leave_collection();
        encoded
    }
    pub fn finish(mut self) -> Result<Vec<u8>> {
        // 借用形态（调用方 `dst` + 帧内能力借用）的收尾只能走 crate 私有 `finish_into`。
        // 回调可以把传入的编码器换成 `Encoder::new` 后对换出的借用编码器调本方法，故这里必须
        // 显式报错：不得以 debug 断言充当拒绝（release 不生效），也不得输出空的拥有值结果。
        if let Some(error) = self.failure.get() {
            return Err(error);
        }
        if self.dst.is_some() || self.store.is_some() || self.ws.is_some() {
            return Err(self.fail(Error::InvalidArgument));
        }
        // Weak 预留必须由同消息内的强目标填充，禁止输出悬空引用。
        if self
            .identities
            .values()
            .any(|entry| entry.state == IdentityState::Reserved)
        {
            return Err(self.fail(Error::InvalidArgument));
        }
        // 元素作用域未结束（begin_element 未与 end_element 配对）时禁止组装。
        if self.in_record() || self.open != 0 || self.element_depth != 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        // 对象块由根槽引用；无根槽时块无处接入，拒绝生成偏移无效的布局。
        if !self.blocks.is_empty() && self.assist.is_empty() {
            return Err(self.fail(Error::InvalidArgument));
        }
        let mut assist = self.assist;
        // 块按离开次序（即记录提交次序）接在根槽之后，槽号从 1 起（设计 §1 第 4 项）。
        let mut start = 1u32;
        for block in &self.blocks {
            let count = (block.slots.len() / SLOT_SIZE) as u32;
            // 多态对象头先于槽范围写入（FR-W07）：u16 tag，逃生口再加 u32 type_name record id。
            let header_bytes = match block.poly {
                Some(poly) => {
                    self.main[block.body_pos..block.body_pos + 2]
                        .copy_from_slice(&poly.tag.to_le_bytes());
                    if let Some(type_name_id) = poly.type_name_id {
                        self.main[block.body_pos + 2..block.body_pos + 6]
                            .copy_from_slice(&type_name_id.to_le_bytes());
                    }
                    poly.header_bytes()
                }
                None => 0,
            };
            let range_pos = block.body_pos + header_bytes;
            self.main[range_pos..range_pos + 4].copy_from_slice(&start.to_le_bytes());
            self.main[range_pos + 4..range_pos + 8].copy_from_slice(&count.to_le_bytes());
            assist.extend_from_slice(&block.slots);
            start += count;
        }
        let raw = envelope::write_envelope(&self.main, &assist, &self.opts)?;
        if self.opts.packed {
            packed::pack(&raw, &self.opts)
        } else {
            Ok(raw)
        }
    }
    /// 测试专用：清除锁存标志后执行 `finish`，用于验证被拒绝的调用没有副作用。
    /// 仅跳过锁存检查；结构校验与组装与 `finish` 完全相同。
    #[cfg(test)]
    pub(crate) fn finish_ignoring_latch(self) -> Result<Vec<u8>> {
        self.failure.clear();
        self.finish()
    }
}

/// `encode_into` 的实现（公开入口在 crate 根）：`encode_into` 帧内由 [`typed_scratch::with_store`]
/// 构造 `EntryStore` 作为清理所有者（模块内构造、调用方只借能力），`Encoder` 只借用其能力
/// （架构 §2.1.4/§2.1.5）。
///
/// `scratch` 在帧内由 `EntryStore` 独占持有，`Encoder` 只拿 `&'a mut StoreCapView`（unsized 能力
/// 视图），因此能力借用与底层 scratch 的寿命在该帧内重合；三类退出（`Ok`/`Err`/unwind）都由同一
/// 终止清理入口完成（`with_store` 在 `Ok`/`Err` 退出时显式调用一次，unwind 退出由 `Drop` 调同一
/// 入口；重复调用按状态机幂等）。
pub(crate) fn encode_into_impl<T: AceEncode>(
    value: &T,
    dst: &mut [u8],
    scratch: &mut [u8],
    options: &Options,
) -> Result<usize> {
    encode_into_probe_impl(value, dst, scratch, options, None)
}

/// `encode_into_probe` 的实现（公开入口在 crate 根）。
///
/// 与 [`encode_into_impl`] 走**同一条真实编码路径**（同一调用序列、同一 `Encoder`/`EntryStore`/arena
/// 状态机、同一个调用方 `scratch`）：`need` 为 `Some` 时只多一条出口——若本次因**真实 `scratch`
/// 不足**而失败，就在返回点把该失败点的 checked 下界写进 `need`（需求 R29）。
///
/// `1 InvalidArgument` 不能当归因依据：下界只能来自 [`typed_scratch`] 在失败点留下的私有诊断；
/// 而且来源由**产生点**标注（[`FailureLatch`]），不比较 `Error` 值：本次调用只要产生过其它
/// 失败（含 `dst` 容量点），`scratch` 诊断就不代表返回值，一律不写 `need`。缺后端/替换编码器、
/// `dst` 不足、其它首错也不写 `need`，哨兵值与公开错误码、错误优先级不变。
///
/// 返回点另有一条出口条件（异码排除）：只有最终返回 `Err(Error::InvalidArgument)` 时才可能写
/// `need`——`scratch` 不足只会返回 `1 InvalidArgument`，返回其它码（如 `6 ResourceLimit`、`2`、
/// `4`）不可能源于 `scratch` 不足。这不是「码值相等即同源」的证明：同码的来源仍靠产生点标注
/// （[`FailureLatch`]）判定，本条件只挡异码，不读取、不比较锁存的首错值。
///
/// 已知限制：回调锁存一条 `scratch` 不足后直接返回一条**自造的同码** `Err(InvalidArgument)`
/// （不经过任何标注点）——它与「回调原样传播那条 `scratch` 不足」在公开 `Result` 上不可区分
/// （`Error` 不携带 provenance），此时仍会写锁存下界。不得用「码值相等」或「末错」去补：公开码
/// 相同不代表同源（既有反例 `IgnoresScratchCapacityThenFailsWithoutFrame` 已否决码值比较）。
pub(crate) fn encode_into_probe_impl<T: AceEncode>(
    value: &T,
    dst: &mut [u8],
    scratch: &mut [u8],
    options: &Options,
    need: Option<&mut usize>,
) -> Result<usize> {
    typed_scratch::with_store(scratch, |store| {
        let mut encoder = Encoder::borrowed(options, dst, store);
        let outcome = match value.ace_encode(&mut encoder) {
            Ok(()) => encoder.finish_into(),
            Err(error) => Err(error),
        };
        // 归因只看失败分支的私有诊断：返回值仍是既有 `1`，本处只写 `need`，不碰错误码与优先级。
        // 出口排除条件：`scratch` 不足只会返回 `1 InvalidArgument`，返回其它码（如 `6
        // ResourceLimit`、`2`、`4`）不可能源于 `scratch` 不足，故只有最终返回
        // `Err(Error::InvalidArgument)` 时才可能写 `need`。这不是「码值相等即同源」的证明：同码
        // 来源仍由 [`FailureLatch`] 的产生点标注判定（`other` 已排除 `dst` 容量点与非容量失败），
        // 本条件只挡异码，不读取、不比较锁存的首错值。
        let attribution = if matches!(&outcome, Err(Error::InvalidArgument)) {
            encoder.latched_shortfall()
        } else {
            None
        };
        drop(encoder);
        if let (Some(need), Some(min_total)) = (need, attribution) {
            *need = min_total;
        }
        outcome
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::value;
    use alloc::vec;

    fn raw<'b>() -> Encoder<'b> {
        Encoder::new(&Options {
            packed: false,
            ..Options::default()
        })
    }
    fn slot(depth: i32, tag: i32, payload: [u8; 8]) -> Vec<u8> {
        [
            depth.to_le_bytes().as_slice(),
            tag.to_le_bytes().as_slice(),
            &payload,
        ]
        .concat()
    }
    fn record(id: i32, body: &[u8]) -> Vec<u8> {
        [
            id.to_le_bytes().as_slice(),
            (body.len() as i32).to_le_bytes().as_slice(),
            body,
        ]
        .concat()
    }
    fn envelope_bytes(main: &[u8], assist: &[u8]) -> Vec<u8> {
        [
            (main.len() as i32).to_le_bytes().as_slice(),
            main,
            (assist.len() as i32).to_le_bytes().as_slice(),
            assist,
        ]
        .concat()
    }
    #[test]
    fn nested_record_bodies_are_flat_and_indexed() {
        use crate::{decode::Decoder, wire::record::RecordIndex};
        let opts = Options {
            packed: false,
            ..Options::default()
        };
        let mut e = Encoder::new(&opts);
        assert_eq!(e.write_body_bytes(b"x"), Err(Error::InvalidArgument));
        e.begin_record().unwrap();
        e.write_body_bytes(b"a").unwrap();
        e.begin_record().unwrap();
        e.write_body_bytes(b"inner").unwrap();
        let inner = e.end_record().unwrap();
        e.write_element_id(inner).unwrap();
        let outer = e.end_record().unwrap();
        e.write_root_scalar(u64::from(outer).to_le_bytes()).unwrap();
        let bytes = e.finish_ignoring_latch().unwrap();
        let len = i32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let index = RecordIndex::parse(&bytes[4..4 + len], &opts).unwrap();
        assert_eq!(index.len(), 2);
        assert!(index.get(inner as i32).is_some());
        assert!(index.get(outer as i32).is_some());
        let mut d = Decoder::new(&bytes, &opts).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(outer).unwrap();
        assert_eq!(d.read_body_bytes(1).unwrap(), b"a");
        assert_eq!(d.read_element_id().unwrap(), inner);
        d.begin_record_view(inner).unwrap();
        assert_eq!(d.read_body_bytes(5).unwrap(), b"inner");
        assert_eq!(d.read_body_bytes(1), Err(Error::TruncatedInput));
        d.end_record_view();
        assert_eq!(d.read_body_bytes(1), Err(Error::TruncatedInput));
        d.end_record_view();
        assert_eq!(d.begin_record_view(99), Err(Error::InvalidReference));
        d.finish().unwrap();
        let mut e = Encoder::new(&opts);
        e.begin_record().unwrap();
        assert_eq!(e.finish(), Err(Error::InvalidArgument));
        let mut e2 = Encoder::new(&opts);
        e2.begin_element();
        assert_eq!(e2.finish(), Err(Error::InvalidArgument));
    }
    #[test]
    fn buffered_record_budget_is_atomic() {
        let mut e = Encoder::new(&Options {
            packed: false,
            // 18 = 信封头 8 + 主区预算 10；未写根时不计根预留槽字节。
            max_raw_bytes: 18,
            ..Options::default()
        });
        e.begin_record().unwrap();
        assert_eq!(e.write_body_bytes(b"abc"), Err(Error::ResourceLimit));
        e.write_body_bytes(b"ab").unwrap();
        assert_eq!(e.begin_record(), Err(Error::ResourceLimit));
        assert_eq!(e.end_record(), Ok(1));
        assert_eq!(e.main, [1, 0, 0, 0, 2, 0, 0, 0, b'a', b'b']);
    }
    #[test]
    fn buffered_record_count_limit_and_empty_stack() {
        let mut e = Encoder::new(&Options {
            max_records: 1,
            ..Options::default()
        });
        assert_eq!(e.end_record(), Err(Error::InvalidArgument));
        assert_eq!(e.write_element_id(1), Err(Error::InvalidArgument));
        e.begin_record().unwrap();
        assert_eq!(e.begin_record(), Err(Error::ResourceLimit));
        e.end_record().unwrap();
        assert_eq!(e.begin_record(), Err(Error::ResourceLimit));
    }
    #[test]
    fn root_scalar_golden() {
        for v in [0, 1] {
            let mut e = raw();
            e.write_root_scalar(value::encode_i32(v)).unwrap();
            let mut expected = vec![0, 0, 0, 0, 16, 0, 0, 0];
            expected.extend_from_slice(&[0; 8]);
            expected.extend_from_slice(&[v as u8, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(e.finish().unwrap(), expected);
        }
    }
    /// 借用路径的帧内编码器：已打开一个对象帧后执行 `f`，用于验证根检查的实际后端来源。
    fn borrowed_with_open_object(
        opts: &Options,
        f: impl FnOnce(&mut Encoder<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut dst = [0u8; 256];
        let mut scratch = vec![0u8; 4096];
        typed_scratch::with_store(&mut scratch, |store| {
            let mut encoder = Encoder::borrowed(opts, &mut dst, store);
            encoder.enter_object().unwrap();
            f(&mut encoder)
        })
    }

    /// F3：借用路径的根检查必须看**实际后端的对象帧**，不能只看拥有值的 `frames`
    /// （否则回调打开对象后仍能写根槽，与拥有值路径的可观察行为不同）。
    #[test]
    fn borrowed_root_checks_see_backend_object_frames() {
        let opts = Options {
            packed: false,
            ..Options::default()
        };
        assert_eq!(
            borrowed_with_open_object(&opts, |e| e.write_root_none()),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            borrowed_with_open_object(&opts, |e| e.write_root_scalar([7; 8])),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            borrowed_with_open_object(&opts, |e| e.write_root_record(b"x")),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn root_none() {
        let mut e = raw();
        e.write_root_none().unwrap();
        assert_eq!(e.finish().unwrap(), [0; 8]);
    }
    #[test]
    fn root_string_flat() {
        let mut e = raw();
        e.write_root_record(value::encode_str("中文🙂")).unwrap();
        let main = [
            1, 0, 0, 0, 10, 0, 0, 0, 0xe4, 0xb8, 0xad, 0xe6, 0x96, 0x87, 0xf0, 0x9f, 0x99, 0x82,
        ];
        assert_eq!(
            e.finish().unwrap(),
            envelope_bytes(&main, &slot(0, 0, 1u64.to_le_bytes()))
        );
    }
    #[test]
    fn root_wide_values() {
        let guid = value::encode_guid(value::Guid([0x42; 16]));
        let dto = value::encode_datetime_offset(42, 0).unwrap();
        for body in [&guid[..], &dto[..]] {
            let mut e = raw();
            e.write_root_record(body).unwrap();
            assert_eq!(
                e.finish().unwrap(),
                envelope_bytes(&record(1, body), &slot(0, 0, 1u64.to_le_bytes()))
            );
        }
    }
    #[test]
    fn omitted_scalar_and_option_presence() {
        let mut e = raw();
        e.enter_object().unwrap();
        e.begin_field(0).unwrap();
        if !value::is_default_i32(0) {
            e.encode_scalar(value::encode_i32(0)).unwrap();
        }
        e.write_field_scalar(2, value::encode_i32(0)).unwrap();
        e.write_field_option_scalar(3, None).unwrap();
        e.write_field_option_scalar(4, Some([0; 8])).unwrap();
        assert_eq!(e.leave_object(), Ok(1));
        assert_eq!(
            e.finish().unwrap(),
            envelope_bytes(
                &record(1, &[1, 0, 0, 0, 2, 0, 0, 0]),
                &[
                    slot(0, 0, 1u64.to_le_bytes()),
                    slot(1, 2, [0; 8]),
                    slot(1, 4, [0; 8]),
                ]
                .concat()
            )
        );
    }
    #[test]
    fn empty_string_field_is_written() {
        let mut e = raw();
        e.enter_object().unwrap();
        e.write_field_record(0, b"").unwrap();
        assert_eq!(e.leave_object(), Ok(2));
        assert_eq!(
            e.finish().unwrap(),
            envelope_bytes(
                &[record(1, b""), record(2, &[1, 0, 0, 0, 1, 0, 0, 0])].concat(),
                &[
                    slot(0, 0, 2u64.to_le_bytes()),
                    slot(1, 0, 1u64.to_le_bytes())
                ]
                .concat()
            )
        );
    }
    #[test]
    fn identical_wide_values_are_distinct_records() {
        let guid = [0x55; 16];
        let mut e = raw();
        e.enter_object().unwrap();
        e.write_field_record(0, &guid).unwrap();
        e.write_field_record(1, &guid).unwrap();
        assert_eq!(e.leave_object(), Ok(3));
        assert_eq!(
            e.finish().unwrap(),
            envelope_bytes(
                &[
                    record(1, &guid),
                    record(2, &guid),
                    record(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
                ]
                .concat(),
                &[
                    slot(0, 0, 3u64.to_le_bytes()),
                    slot(1, 0, 1u64.to_le_bytes()),
                    slot(1, 1, 2u64.to_le_bytes()),
                ]
                .concat()
            )
        );
    }
    #[test]
    fn field_order_is_strict_per_object() {
        let mut e = raw();
        e.enter_object().unwrap();
        e.write_field_none(2).unwrap();
        assert_eq!(e.begin_field(1), Err(Error::InvalidArgument));
        assert_eq!(e.write_field_none(2), Err(Error::InvalidArgument));
        e.enter_object().unwrap();
        e.write_field_none(0).unwrap();
        let child = e.leave_object().unwrap();
        e.write_field_scalar(3, u64::from(child).to_le_bytes())
            .unwrap();
        assert_eq!(e.leave_object(), Ok(2));
        assert_eq!(
            e.finish_ignoring_latch().unwrap(),
            envelope_bytes(
                &[
                    record(1, &[1, 0, 0, 0, 0, 0, 0, 0]),
                    record(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
                ]
                .concat(),
                &[
                    slot(0, 0, 2u64.to_le_bytes()),
                    slot(1, 3, 1u64.to_le_bytes())
                ]
                .concat()
            )
        );
    }
    #[test]
    fn slot_and_record_budgets() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_slots: 1,
            ..Options::default()
        });
        e.write_root_scalar([0; 8]).unwrap();
        assert_eq!(e.push_slot(1, 0, [0; 8]), Err(Error::ResourceLimit));
        assert_eq!(
            e.finish_ignoring_latch().unwrap(),
            envelope_bytes(&[], &slot(0, 0, [0; 8]))
        );
        let mut e = Encoder::new(&Options {
            packed: false,
            max_records: 1,
            ..Options::default()
        });
        assert_eq!(e.alloc_record(b"a"), Ok(1));
        assert_eq!(e.alloc_record(b"b"), Err(Error::ResourceLimit));
        assert_eq!(
            e.finish_ignoring_latch().unwrap(),
            envelope_bytes(&record(1, b"a"), &[])
        );
    }
    #[test]
    fn packed_switch() {
        let mut e = Encoder::new(&Options::default());
        e.write_root_scalar([0; 8]).unwrap();
        assert_eq!(e.finish().unwrap(), [0x10, 16, 0, 1]);
        let mut e = raw();
        e.write_root_scalar([0; 8]).unwrap();
        assert_eq!(
            e.finish().unwrap(),
            [0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }
    #[test]
    fn main_budget_rejects_record_without_mutation() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_raw_bytes: 8,
            ..Options::default()
        });
        assert_eq!(e.alloc_record(b"x"), Err(Error::ResourceLimit));
        assert_eq!(e.finish_ignoring_latch().unwrap(), [0; 8]);
    }
    #[test]
    fn open_frame_slot_bytes_count_before_push() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_raw_bytes: 55,
            ..Options::default()
        });
        e.enter_object().unwrap();
        assert_eq!(e.write_field_scalar(1, [0; 8]), Ok(()));
        assert_eq!(e.write_field_scalar(2, [0; 8]), Err(Error::ResourceLimit));
    }
    #[test]
    fn open_frame_record_with_slot_checks_before_alloc() {
        let mut e = Encoder::new(&Options {
            max_raw_bytes: 55,
            ..Options::default()
        });
        e.enter_object().unwrap();
        assert_eq!(e.write_field_record(1, &[0; 8]), Err(Error::ResourceLimit));
        assert!(e.main.is_empty());
    }
    #[test]
    fn depth_and_failed_record_are_bounded() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_slots: 0,
            ..Options::default()
        });
        assert_eq!(e.write_root_record(b"x"), Err(Error::ResourceLimit));
        assert_eq!(e.finish_ignoring_latch().unwrap(), [0; 8]);

        let mut e = Encoder::new(&Options {
            packed: false,
            max_depth: 1,
            ..Options::default()
        });
        e.enter_object().unwrap();
        e.enter_object().unwrap();
        assert_eq!(e.enter_object(), Err(Error::DepthExceeded));
        let child = e.leave_object().unwrap();
        e.write_field_scalar(0, u64::from(child).to_le_bytes())
            .unwrap();
        assert_eq!(e.leave_object(), Ok(2));
        assert_eq!(e.leave_object(), Err(Error::InvalidArgument));
        assert_eq!(
            e.finish_ignoring_latch().unwrap(),
            envelope_bytes(
                &[
                    record(1, &[1, 0, 0, 0, 0, 0, 0, 0]),
                    record(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
                ]
                .concat(),
                &[
                    slot(0, 0, 2u64.to_le_bytes()),
                    slot(1, 0, 1u64.to_le_bytes())
                ]
                .concat()
            )
        );
    }

    #[test]
    fn bare_alloc_finish_budget_matches_output() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_raw_bytes: 17,
            ..Options::default()
        });
        assert_eq!(e.alloc_record(b"a"), Ok(1));
        assert_eq!(e.finish().unwrap(), envelope_bytes(&record(1, b"a"), &[]));
    }

    #[test]
    fn root_scalar_counts_its_own_slot_bytes() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_raw_bytes: 23,
            ..Options::default()
        });
        assert_eq!(e.write_root_scalar([0; 8]), Err(Error::ResourceLimit));
        assert!(e.assist.is_empty());
    }

    #[test]
    fn root_record_with_slot_checks_before_alloc() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_raw_bytes: 31,
            ..Options::default()
        });
        assert_eq!(e.write_root_record(&[0; 8]), Err(Error::ResourceLimit));
        assert!(e.main.is_empty());
    }

    #[test]
    fn single_object_layout_writes_slots_in_one_block() {
        let mut e = raw();
        e.enter_object().unwrap();
        e.write_field_scalar(0, [1; 8]).unwrap();
        e.write_field_record(1, b"abc").unwrap();
        e.write_field_none(2).unwrap();
        e.write_field_scalar(3, [2; 8]).unwrap();
        assert_eq!(e.leave_object(), Ok(2));
        assert_eq!(
            e.finish().unwrap(),
            envelope_bytes(
                &[record(1, b"abc"), record(2, &[1, 0, 0, 0, 3, 0, 0, 0])].concat(),
                &[
                    slot(0, 0, 2u64.to_le_bytes()),
                    slot(1, 0, [1; 8]),
                    slot(1, 1, 1u64.to_le_bytes()),
                    slot(1, 3, [2; 8]),
                ]
                .concat()
            )
        );
    }

    #[test]
    fn two_level_nesting_writes_child_block_first() {
        let mut e = raw();
        e.enter_object().unwrap();
        e.enter_object().unwrap();
        e.write_field_scalar(0, [5; 8]).unwrap();
        let child = e.leave_object().unwrap();
        e.write_field_scalar(0, u64::from(child).to_le_bytes())
            .unwrap();
        assert_eq!(e.leave_object(), Ok(2));
        assert_eq!(child, 1);
        assert_eq!(
            e.finish().unwrap(),
            envelope_bytes(
                &[
                    record(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
                    record(2, &[2, 0, 0, 0, 1, 0, 0, 0]),
                ]
                .concat(),
                &[
                    slot(0, 0, 2u64.to_le_bytes()),
                    slot(2, 0, [5; 8]),
                    slot(1, 0, 1u64.to_le_bytes()),
                ]
                .concat()
            )
        );
    }

    #[test]
    fn empty_object_has_zero_count_and_eight_byte_body() {
        let mut e = raw();
        e.enter_object().unwrap();
        assert_eq!(e.leave_object(), Ok(1));
        assert_eq!(
            e.finish().unwrap(),
            envelope_bytes(
                &record(1, &[1, 0, 0, 0, 0, 0, 0, 0]),
                &slot(0, 0, 1u64.to_le_bytes())
            )
        );
    }

    #[test]
    fn max_slots_counts_root_reservation_and_frame_slots() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_slots: 3,
            ..Options::default()
        });
        e.enter_object().unwrap();
        e.write_field_scalar(0, [1; 8]).unwrap();
        e.enter_object().unwrap();
        e.write_field_scalar(0, [2; 8]).unwrap();
        assert_eq!(e.write_field_scalar(1, [3; 8]), Err(Error::ResourceLimit));
        assert_eq!(e.leave_object(), Ok(1));
        assert_eq!(e.leave_object(), Ok(2));
        assert_eq!(
            e.finish_ignoring_latch().unwrap(),
            envelope_bytes(
                &[
                    record(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
                    record(2, &[2, 0, 0, 0, 1, 0, 0, 0]),
                ]
                .concat(),
                &[
                    slot(0, 0, 2u64.to_le_bytes()),
                    slot(2, 0, [2; 8]),
                    slot(1, 0, [1; 8]),
                ]
                .concat()
            )
        );
    }

    #[test]
    fn max_depth_zero_allows_root_object_only() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_depth: 0,
            ..Options::default()
        });
        e.enter_object().unwrap();
        e.write_field_scalar(0, [4; 8]).unwrap();
        assert_eq!(e.enter_object(), Err(Error::DepthExceeded));
        assert_eq!(e.leave_object(), Ok(1));
        assert_eq!(
            e.finish_ignoring_latch().unwrap(),
            envelope_bytes(
                &record(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
                &[slot(0, 0, 1u64.to_le_bytes()), slot(1, 0, [4; 8])].concat()
            )
        );
    }
    #[test]
    fn collection_len_limit_is_checked_without_mutation() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_collection_len: 2,
            ..Options::default()
        });
        e.alloc_record(b"a").unwrap();
        assert_eq!(e.check_collection_len(2), Ok(()));
        assert_eq!(e.check_collection_len(3), Err(Error::ResourceLimit));
        assert_eq!(e.next_record_id, 2);
        assert_eq!(e.main.len(), 9);
    }
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn collection_len_is_bounded_by_u32_count_range() {
        let e = Encoder::new(&Options {
            max_collection_len: usize::MAX,
            ..Options::default()
        });
        assert_eq!(e.check_collection_len(u32::MAX as usize), Ok(()));
        assert_eq!(
            e.check_collection_len(u32::MAX as usize + 1),
            Err(Error::ResourceLimit)
        );
    }

    #[test]
    fn second_emit_for_same_field_is_rejected() {
        // 同一字段二次发射返回 InvalidArgument，且失败不改变输出字节（R-PUSH-01）。
        let o = Options::default();
        let mut once = Encoder::new(&o);
        once.enter_object().unwrap();
        once.begin_field(1).unwrap();
        once.encode_scalar([7; 8]).unwrap();
        once.leave_object().unwrap();
        let expected = once.finish().unwrap();

        let mut twice = Encoder::new(&o);
        twice.enter_object().unwrap();
        twice.begin_field(1).unwrap();
        twice.encode_scalar([7; 8]).unwrap();
        assert_eq!(twice.encode_scalar([9; 8]), Err(Error::InvalidArgument));
        assert_eq!(twice.encode_record(&[1, 2, 3]), Err(Error::InvalidArgument));
        twice.leave_object().unwrap();
        assert_eq!(twice.finish_ignoring_latch().unwrap(), expected);
    }

    // 设计 :47、V4-10：先选 2、同帧写入 3 后，发射被选中的 2 须拒绝。
    #[test]
    fn stale_selection_after_later_write_is_rejected() {
        let mut e = raw();
        e.enter_object().unwrap();
        e.begin_field(2).unwrap();
        e.write_field_scalar(3, [1; 8]).unwrap();
        assert_eq!(e.encode_scalar([2; 8]), Err(Error::InvalidArgument));
    }

    #[test]
    fn rejected_record_emit_leaves_output_unchanged() {
        let o = Options::default();
        let mut without = Encoder::new(&o);
        without.enter_object().unwrap();
        without.begin_field(2).unwrap();
        without.write_field_scalar(3, [1; 8]).unwrap();
        without.leave_object().unwrap();
        let expected = without.finish().unwrap();

        let mut with = Encoder::new(&o);
        with.enter_object().unwrap();
        with.begin_field(2).unwrap();
        with.write_field_scalar(3, [1; 8]).unwrap();
        assert_eq!(with.encode_record(&[9, 9]), Err(Error::InvalidArgument));
        with.leave_object().unwrap();
        assert_eq!(with.finish_ignoring_latch().unwrap(), expected);
    }
    #[test]
    fn r_s2_d1_ignored_leave_error_latches_finish() {
        struct IgnoresLeaveError;
        impl AceEncode for IgnoresLeaveError {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                let _ = e.leave_object();
                Ok(())
            }
        }
        let mut e = raw();
        assert_eq!(IgnoresLeaveError.ace_encode(&mut e), Ok(()));
        assert_eq!(e.finish(), Err(Error::InvalidArgument));
    }
    #[test]
    fn r_s2_d1_first_error_wins() {
        let mut e = Encoder::new(&Options {
            packed: false,
            max_depth: 0,
            ..Options::default()
        });
        e.enter_object().unwrap();
        let _ = e.enter_object();
        let _ = e.leave_object();
        let _ = e.leave_object();
        assert_eq!(e.finish(), Err(Error::DepthExceeded));
    }
}
