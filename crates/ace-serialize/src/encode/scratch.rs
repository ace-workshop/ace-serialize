//! `encode_into` 的 scratch 承载路径（用户 2026-11-18 裁决，暂定）。
//!
//! 本模块只做字节搬运与定宽字段编解码，**不新增 `unsafe`**：`unsafe` 仍只在
//! `encode/typed_scratch.rs` 的「字节区 ↔ 类型化身份槽」一条路径上。本模块用到的两块
//! 承载是：
//!
//! - 身份表：`EntryStore` 的类型化身份槽（`typed_scratch::Entry`），线性扫描；
//! - 其余工作状态：arena 内的定宽小端字段描述符（[`ScratchState`]）。
//!
//! arena 取用户裁决的**简单版**：身份槽区在前、临时数据区（arena）在后，**单调分配、不回收**，
//! 容量＝`scratch` 扣除条目区与对齐后的剩余字节。因此增长只能「另申请一块并复制」，旧块留在
//! 已占字节内（复制冗余，架构 §3 未证充分上界）。`dst` 只承担最终 wire：raw 模式下 `main`
//! 直接写 `dst[4..]`；packed 模式下不得先把完整 raw 写进 `dst`，`main` 暂存在 arena 中，
//! 由 [`Encoder::finish_into`] 逐 word 打包写 `dst`。
//!
//! 错误映射（用户 2026-11-18 裁决，暂定，后续可细分）：`dst` 容量不足、身份表物理相撞
//! （`C(B) < N ≤ C(0)`）与 arena 不足三种容量类情形都映射既有 `1 InvalidArgument`。

use super::typed_scratch::{Entry, EntryState, Guard, InitError, StoreCapView};
use super::*;
use core::any::TypeId;
use core::cell::Cell;

/// 描述符字段的无值哨兵：这些字段都受 `i32`/`u32` 计数上限约束，`u32::MAX` 不会作为真值出现。
const NONE: u32 = u32::MAX;

/// 帧描述符：字段深度、发射顺序、保存的字段选择、捕获目标、多态头与槽字节缓冲。
const FRAME_STRIDE: usize = 64;
const FRAME_DEPTH: usize = 0;
const FRAME_LAST_TAG: usize = 4;
const FRAME_LAST_EMITTED: usize = 8;
const FRAME_SAVED_ORDER: usize = 12;
const FRAME_TARGET: usize = 16;
const FRAME_POLY_KIND: usize = 20;
const FRAME_POLY_TAG: usize = 24;
const FRAME_POLY_TYPE_NAME: usize = 28;
const FRAME_SLOTS_OFF: usize = 32;
const FRAME_SLOTS_LEN: usize = 40;
const FRAME_SLOTS_CAP: usize = 48;

/// 已离开对象块：对象体在 main 内的偏移、槽块区段与多态头（`finish_into` 回填用）。
const BLOCK_STRIDE: usize = 40;
const BLOCK_BODY_POS: usize = 0;
const BLOCK_SLOTS_OFF: usize = 8;
const BLOCK_SLOTS_LEN: usize = 16;
const BLOCK_POLY_KIND: usize = 24;
const BLOCK_POLY_TAG: usize = 28;
const BLOCK_POLY_TYPE_NAME: usize = 32;

/// 未提交记录：身份目标 id（普通记录为 [`NONE`]）与记录体缓冲。
const RECORD_STRIDE: usize = 32;
const RECORD_TARGET_ID: usize = 0;
const RECORD_BODY_OFF: usize = 8;
const RECORD_BODY_LEN: usize = 16;
const RECORD_BODY_CAP: usize = 24;

/// 身份捕获：记录 id、捕获状态与进入时的作用域坐标（帧数、记录数、元素深度）。
const CAPTURE_STRIDE: usize = 32;
const CAPTURE_ID: usize = 0;
const CAPTURE_STATE: usize = 4;
const CAPTURE_ENTRY_FRAMES: usize = 8;
const CAPTURE_ENTRY_RECORDS: usize = 16;
const CAPTURE_ENTRY_DEPTH: usize = 24;

/// 元素作用域栈：外层字段选择（[`NONE`] 表示 `None`）。
const ORDER_STRIDE: usize = 4;

/// 多态头种类（`finish_into` 回填时按此写 `u16 tag` 与可选 `u32 type_name`）。
const POLY_NONE: u32 = 0;
const POLY_NUMERIC: u32 = 1;
const POLY_ESCAPE: u32 = 2;

/// 捕获状态（与 `CaptureState` 一一对应，只在本模块的描述符中出现）。
const CAPTURE_UNSUBMITTED: u32 = 0;
const CAPTURE_CLAIMABLE: u32 = 1;
const CAPTURE_NATIVE_COMMITTED: u32 = 2;
const CAPTURE_OBJECT_COMMITTED: u32 = 3;

/// arena 内的一段字节缓冲（`off` 为 scratch 内绝对偏移，`len`/`cap` 为字节数）。
#[derive(Debug, Clone, Copy)]
struct Buf {
    off: usize,
    len: usize,
    cap: usize,
}

impl Buf {
    const EMPTY: Self = Self {
        off: 0,
        len: 0,
        cap: 0,
    };
}

/// arena 内的定长元素数组（`stride` 字节/元素，元素按 8 字节对齐分配）。
#[derive(Debug, Clone, Copy)]
struct Slab {
    off: usize,
    len: usize,
    cap: usize,
    stride: usize,
}

impl Slab {
    const fn new(stride: usize) -> Self {
        Self {
            off: 0,
            len: 0,
            cap: 0,
            stride,
        }
    }
}

/// `encode_into` 帧内的 arena 工作状态：只存偏移与长度，不存引用、不回收
/// （架构 §2.1.1 的「arena 只用 offset/length 定位」）。
pub(super) struct ScratchState {
    /// main 已写字节数（`M`）：raw 模式即 `dst[4..4+M]` 的长度。
    main_len: usize,
    /// packed 模式下 main 的 arena 缓冲；raw 模式下 main 直接写 `dst`，此缓冲不用。
    main_buf: Buf,
    /// 根槽字节（`SLOT_SIZE`，无根槽时为空）。
    assist: Buf,
    frames: Slab,
    blocks: Slab,
    records: Slab,
    captures: Slab,
    orders: Slab,
    /// R23 试准备已物理预留、尚未逻辑入栈的记录体（`len` 恒为 0）；`Committed` 时由本次接管
    /// 消耗、`Err` 时留给下一次 `scratch_push_record` 复用；钩子未接管（`NotStarted`／`Declined`）
    /// 时该未提交预留被撤销、`B` 恢复到试准备前的 `B_pre`。
    pending_body: Buf,
}

impl ScratchState {
    pub(super) fn new() -> Self {
        Self {
            main_len: 0,
            main_buf: Buf::EMPTY,
            assist: Buf::EMPTY,
            frames: Slab::new(FRAME_STRIDE),
            blocks: Slab::new(BLOCK_STRIDE),
            records: Slab::new(RECORD_STRIDE),
            captures: Slab::new(CAPTURE_STRIDE),
            orders: Slab::new(ORDER_STRIDE),
            pending_body: Buf::EMPTY,
        }
    }
}

/// 记录首错锁存的入口（与 `Encoder::fail` 同语义，但可在已分别借出 `failure` 时使用）。这里声明的
/// 来源是**`scratch` 容量不足**（与 `FailureLatch::record` 声明的「其它」相对）。
///
/// 帧内能力借用的容量失败 → 既有 `1` 的锁存入口：**只有**它就是本次调用锁存的首个失败时，
/// 才把该失败点的私有诊断（`scratch` 不足的下界）绑进本编码器的归因；`failure` 已被前一个
/// 其它失败占据时不覆盖首错、也不写归因，并且那个失败的产生点已标记「本次出现其它失败」——
/// 返回点因此不会把选了另一个分支的值写成 `need`（需求 R29：不得把其它失败写成容量不足；
/// 缺后端/替换编码器不得继承历史诊断）。
fn latch_scratch_shortfall(
    slot: &FailureLatch,
    shortfall: &Cell<Option<usize>>,
    cap: &StoreCapView,
) -> Error {
    if slot.get().is_none() {
        shortfall.set(cap.shortfall().map(|shortfall| shortfall.min_total));
    }
    slot.record_scratch_capacity(capacity())
}

/// 容量类错误的映射（用户 2026-11-18 裁决，暂定）：arena/`dst` 容量不足一律既有 `1`。
fn capacity() -> Error {
    Error::InvalidArgument
}

/// 同时借出帧内能力借用与 arena 工作状态（两者是不同的 `Encoder` 字段，互不重叠）。
fn parts<'i, 'a>(
    store: &'i mut Option<&'a mut StoreCapView>,
    ws: &'i mut Option<ScratchState>,
) -> (&'i mut StoreCapView, &'i mut ScratchState) {
    (
        store.as_deref_mut().expect("scratch 承载路径必须有帧内能力借用"),
        ws.as_mut().expect("scratch 承载路径必须有工作区"),
    )
}

fn get_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
}

fn put_u32(bytes: &mut [u8], off: usize, value: u32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
}

fn put_u64(bytes: &mut [u8], off: usize, value: u64) {
    bytes[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn opt(value: Option<u32>) -> u32 {
    value.unwrap_or(NONE)
}

fn unopt(value: u32) -> Option<u32> {
    if value == NONE {
        None
    } else {
        Some(value)
    }
}

fn poly_kind(poly: Option<PolyHeader>) -> (u32, u32, u32) {
    match poly {
        None => (POLY_NONE, 0, NONE),
        Some(PolyHeader {
            tag,
            type_name_id: None,
        }) => (POLY_NUMERIC, u32::from(tag), NONE),
        Some(PolyHeader {
            tag,
            type_name_id: Some(id),
        }) => (POLY_ESCAPE, u32::from(tag), id),
    }
}

fn poly_of(kind: u32, tag: u32, type_name: u32) -> Option<PolyHeader> {
    match kind {
        POLY_NUMERIC => Some(PolyHeader {
            tag: tag as u16,
            type_name_id: None,
        }),
        POLY_ESCAPE => Some(PolyHeader {
            tag: tag as u16,
            type_name_id: Some(type_name),
        }),
        _ => None,
    }
}

fn read_buf(bytes: &[u8], off: usize, len_off: usize, cap_off: usize) -> Buf {
    Buf {
        off: get_u64(bytes, off) as usize,
        len: get_u64(bytes, len_off) as usize,
        cap: get_u64(bytes, cap_off) as usize,
    }
}

fn write_buf(bytes: &mut [u8], base: usize, buf: Buf) {
    put_u64(bytes, base, buf.off as u64);
    put_u64(bytes, base + 8, buf.len as u64);
    put_u64(bytes, base + 16, buf.cap as u64);
}

/// 元素数组的分配对齐（`slab_reserve` 与 R23 预检共用同一落位参数）。
const SLAB_ALIGN: usize = 8;

/// 保证 `buf` 至少容纳 `need` 字节：不够就另申请一块（容量翻倍）并把已写字节复制过去。
/// 单调分配、不回收 ⇒ 旧块留在已占 arena 内（用户 2026-11-18 裁决）。
fn buf_reserve(cap: &mut StoreCapView, buf: &mut Buf, need: usize) -> Result<()> {
    if need <= buf.cap {
        return Ok(());
    }
    let new_cap = grown_capacity(buf.cap, need).ok_or_else(capacity)?;
    let off = cap.arena_alloc_zeroed(new_cap, 1).map_err(|_| capacity())?;
    if buf.len > 0 {
        cap.arena_copy_within(buf.off, off, buf.len);
    }
    buf.off = off;
    buf.cap = new_cap;
    Ok(())
}

/// 追加字节；`bytes` 为空时不申请、不形成区间视图。
fn buf_push(cap: &mut StoreCapView, buf: &mut Buf, bytes: &[u8]) -> Result<()> {    if bytes.is_empty() {
        return Ok(());
    }
    let need = buf.len.checked_add(bytes.len()).ok_or_else(capacity)?;
    buf_reserve(cap, buf, need)?;
    cap.arena_bytes_mut(buf.off + buf.len, bytes.len())
        .copy_from_slice(bytes);
    buf.len = need;
    Ok(())
}

/// 用 `bytes` 覆写缓冲内容（根槽写入用）。
fn buf_assign(cap: &mut StoreCapView, buf: &mut Buf, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        buf.len = 0;
        return Ok(());
    }
    buf_reserve(cap, buf, bytes.len())?;
    cap.arena_bytes_mut(buf.off, bytes.len())
        .copy_from_slice(bytes);
    buf.len = bytes.len();
    Ok(())
}

/// 保证元素数组至少容纳 `need` 个元素（容量翻倍、复制已用元素）。
fn slab_reserve(cap: &mut StoreCapView, slab: &mut Slab, need: usize) -> Result<()> {
    if need <= slab.cap {
        return Ok(());
    }
    let mut new_cap = slab.cap.max(8);
    while new_cap < need {
        new_cap = new_cap.checked_mul(2).ok_or_else(capacity)?;
    }
    let bytes = new_cap.checked_mul(slab.stride).ok_or_else(capacity)?;
    let off = cap
        .arena_alloc_zeroed(bytes, SLAB_ALIGN)
        .map_err(|_| capacity())?;
    if slab.len > 0 {
        let used = slab.len * slab.stride;
        cap.arena_copy_within(slab.off, off, used);
    }
    slab.off = off;
    slab.cap = new_cap;
    Ok(())
}

/// [`buf_reserve`] 的**不锁存**版本：预留不下时返回 `None` 且不写 `shortfall` 诊断（R23 试准备用）。
/// 预留成功时照常推进 arena 占用（单调不回收）。
fn try_buf_reserve(cap: &mut StoreCapView, buf: &mut Buf, need: usize) -> Option<()> {
    if need <= buf.cap {
        return Some(());
    }
    let new_cap = grown_capacity(buf.cap, need)?;
    let off = cap.try_arena_alloc_zeroed(new_cap, 1)?;
    if buf.len > 0 {
        cap.arena_copy_within(buf.off, off, buf.len);
    }
    buf.off = off;
    buf.cap = new_cap;
    Some(())
}

/// `buf_reserve`/`try_buf_reserve` 的容量增长语义：`max(64)` 起翻倍到盖住 `need`。
/// 单独抽出供 R23 预检在同一落位参数下无副作用地判断「能不能放”。
fn grown_capacity(cap: usize, need: usize) -> Option<usize> {
    let mut new_cap = cap.max(64);
    while new_cap < need {
        new_cap = new_cap.checked_mul(2)?;
    }
    Some(new_cap)
}

/// `slab_reserve` 为 `need` 个元素会另申请的字节数（`Some(0)`＝现有容量够、不分配）。
/// 与 [`slab_reserve`] 同一 `max(8)` 起翻倍与对齐参数，供 R23 预检复用。
fn slab_growth_bytes(slab: &Slab, need: usize) -> Option<usize> {
    if need <= slab.cap {
        return Some(0);
    }
    let mut new_cap = slab.cap.max(8);
    while new_cap < need {
        new_cap = new_cap.checked_mul(2)?;
    }
    new_cap.checked_mul(slab.stride)
}

fn slab_push(cap: &mut StoreCapView, slab: &mut Slab, elem: &[u8]) -> Result<usize> {
    debug_assert_eq!(elem.len(), slab.stride);    let need = slab.len.checked_add(1).ok_or_else(capacity)?;
    slab_reserve(cap, slab, need)?;
    let at = slab.off + slab.len * slab.stride;
    cap.arena_bytes_mut(at, slab.stride).copy_from_slice(elem);
    let index = slab.len;
    slab.len = need;
    Ok(index)
}

fn slab_read<'x>(cap: &'x StoreCapView, slab: &Slab, index: usize) -> &'x [u8] {
    assert!(index < slab.len, "元素下标必须在数组长度内");
    cap.arena_bytes(slab.off + index * slab.stride, slab.stride)
}

fn slab_write<'x>(cap: &'x mut StoreCapView, slab: &Slab, index: usize) -> &'x mut [u8] {
    assert!(index < slab.len, "元素下标必须在数组长度内");
    cap.arena_bytes_mut(slab.off + index * slab.stride, slab.stride)
}

fn root_slot(payload: [u8; 8]) -> [u8; SLOT_SIZE] {
    let mut slot = [0u8; SLOT_SIZE];
    slot[8..].copy_from_slice(&payload);
    slot
}

impl<'a> Encoder<'a> {
    /// 借用形态的 crate 私有构造（用户 2026-11-18 裁决授权）：`dst` 与帧内 `EntryStore` 的
    /// 能力借用都由 `encode_into` 帧提供；公开 `new`/`finish` 的签名与行为不变。
    pub(super) fn borrowed(opts: &Options, dst: &'a mut [u8], store: &'a mut StoreCapView) -> Self {
        let mut encoder = Self::new(opts);
        encoder.dst = Some(dst);
        encoder.store = Some(store);
        encoder.ws = Some(ScratchState::new());
        encoder
    }

    fn scratch_ws(&self) -> &ScratchState {
        self.ws.as_ref().expect("scratch 承载路径必须有工作区")
    }

    pub(super) fn scratch_frames_len(&self) -> usize {
        self.scratch_ws().frames.len
    }

    fn scratch_records_len(&self) -> usize {
        self.scratch_ws().records.len
    }

    fn scratch_main_len(&self) -> usize {
        self.scratch_ws().main_len
    }

    /// main 追加字节：raw 直接写 `dst[4 + M..]`，packed 追加到 arena 的 `main_buf`。
    fn scratch_main_push(&mut self, bytes: &[u8]) -> Result<()> {
        let raw = !self.opts.packed;
        let Encoder {
            dst,
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        if raw {
            let Some(dst) = dst.as_deref_mut() else {
                return Err(failure.record(capacity()));
            };
            let main_len = ws.as_ref().expect("工作区").main_len;
            let start = 4usize
                .checked_add(main_len)
                .ok_or_else(capacity)?;
            let end = start.checked_add(bytes.len()).ok_or_else(capacity)?;
            if end > dst.len() {
                return Err(failure.record(capacity()));
            }
            dst[start..end].copy_from_slice(bytes);
            ws.as_mut().expect("工作区").main_len = main_len + bytes.len();
            Ok(())
        } else {
            let (cap, w) = parts(store, ws);
            let pushed = buf_push(cap, &mut w.main_buf, bytes);
            match pushed {
                Ok(()) => {
                    w.main_len = w.main_buf.len;
                    Ok(())
                }
                Err(_) => Err(latch_scratch_shortfall(failure, shortfall, cap)),
            }
        }
    }

    /// 在 main 的 `pos` 处覆写字节（对象块的多态头与槽范围回填）。
    fn scratch_main_write_at(&mut self, pos: usize, bytes: &[u8]) -> Result<()> {
        let raw = !self.opts.packed;
        let Encoder {
            dst,
            store,
            ws,
            failure,
            ..
        } = self;
        let end = pos.checked_add(bytes.len()).ok_or_else(capacity)?;
        if raw {
            let Some(dst) = dst.as_deref_mut() else {
                return Err(failure.record(capacity()));
            };
            let start = 4usize.checked_add(pos).ok_or_else(capacity)?;
            let stop = 4usize.checked_add(end).ok_or_else(capacity)?;
            if stop > dst.len() {
                return Err(failure.record(capacity()));
            }
            dst[start..stop].copy_from_slice(bytes);
            Ok(())
        } else {
            let (cap, w) = parts(store, ws);
            if end > w.main_buf.len {
                return Err(failure.record(capacity()));
            }
            cap.arena_bytes_mut(w.main_buf.off + pos, bytes.len())
                .copy_from_slice(bytes);
            Ok(())
        }
    }

    pub(super) fn scratch_in_record(&self) -> bool {
        self.scratch_ws().records.len > 0
    }

    pub(super) fn scratch_boundary_fingerprint(&self) -> BoundaryFingerprint {
        let w = self.scratch_ws();
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        let frame = (w.frames.len > 0).then(|| {
            let elem = slab_read(cap, &w.frames, w.frames.len - 1);
            (
                get_u64(elem, FRAME_SLOTS_LEN) as usize,
                unopt(get_u32(elem, FRAME_LAST_TAG)).map(|tag| tag as i32),
                unopt(get_u32(elem, FRAME_LAST_EMITTED)).map(|tag| tag as i32),
            )
        });
        let record = (w.records.len > 0).then(|| {
            let elem = slab_read(cap, &w.records, w.records.len - 1);
            get_u64(elem, RECORD_BODY_LEN) as usize
        });
        (self.current_order, frame, record)
    }

    /// 记录数预算是否满足（纯判定、不锁存）：供既有检查与 R23 试准备共用。
    pub(super) fn scratch_record_count_ok(&self) -> bool {
        let w = self.scratch_ws();
        let Some(pending) = w
            .records
            .len
            .checked_add(w.frames.len)
            .and_then(|count| count.checked_add(1))
        else {
            return false;
        };
        let Some(records) = self.committed_records.checked_add(pending) else {
            return false;
        };
        records <= self.opts.max_records && records <= i32::MAX as usize
    }

    pub(super) fn scratch_check_record_count(&self) -> Result<()> {
        if self.scratch_record_count_ok() {
            Ok(())
        } else {
            Err(self.fail(Error::ResourceLimit))
        }
    }

    /// raw 字节预算是否满足（纯判定、不锁存）：供既有检查与 R23 试准备共用。
    fn scratch_buffered_bytes_ok(&self, additional: usize) -> bool {
        let w = self.scratch_ws();
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        let mut pending = 0usize;
        for index in 0..w.frames.len {
            let elem = slab_read(cap, &w.frames, index);
            let poly = poly_of(
                get_u32(elem, FRAME_POLY_KIND),
                get_u32(elem, FRAME_POLY_TAG),
                get_u32(elem, FRAME_POLY_TYPE_NAME),
            );
            let body_bytes = poly.map_or(OBJECT_BODY_BYTES, PolyHeader::body_bytes);
            let Some(next) = pending
                .checked_add(RECORD_HEADER_BYTES)
                .and_then(|total| total.checked_add(body_bytes))
            else {
                return false;
            };
            pending = next;
        }
        let Some(slots) = self
            .slot_count
            .checked_sub(self.root_reserved)
            .and_then(|count| count.checked_mul(SLOT_SIZE))
        else {
            return false;
        };
        let Some(pending) = pending
            .checked_add(slots)
            .and_then(|total| total.checked_add(RAW_HEADER_BYTES))
        else {
            return false;
        };
        let mut total = w.main_len;
        for index in 0..w.records.len {
            let elem = slab_read(cap, &w.records, index);
            let Some(next) = total
                .checked_add(RECORD_HEADER_BYTES)
                .and_then(|n| n.checked_add(get_u64(elem, RECORD_BODY_LEN) as usize))
            else {
                return false;
            };
            total = next;
        }
        let Some(total) = total
            .checked_add(pending)
            .and_then(|n| n.checked_add(additional))
        else {
            return false;
        };
        total <= self.opts.max_raw_bytes
            && total <= i32::MAX as usize
            && total <= isize::MAX as usize
    }

    pub(super) fn scratch_check_buffered_bytes(&self, additional: usize) -> Result<()> {
        if self.scratch_buffered_bytes_ok(additional) {
            Ok(())
        } else {
            Err(self.fail(Error::ResourceLimit))
        }
    }

    /// 压入一条未提交记录描述符（`target_id` 为身份目标或 `None`）。若试准备已物理预留记录体，
    /// 则复用它（`len` 仍为 0），以使回退到既有分派时不为同一记录体二次扩容。
    fn scratch_push_record(&mut self, target_id: Option<u32>) -> Result<()> {
        let Encoder {
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        let (cap, w) = parts(store, ws);
        let body = core::mem::replace(&mut w.pending_body, Buf::EMPTY);
        let mut elem = [0u8; RECORD_STRIDE];
        put_u32(&mut elem, RECORD_TARGET_ID, opt(target_id));
        write_buf(&mut elem, RECORD_BODY_OFF, body);
        match slab_push(cap, &mut w.records, &elem) {
            Ok(_) => Ok(()),
            Err(_) => Err(latch_scratch_shortfall(failure, shortfall, cap)),
        }
    }

    pub(super) fn scratch_begin_record(&mut self) -> Result<()> {
        self.scratch_check_record_count()?;
        self.check_record_id()?;
        self.scratch_check_buffered_bytes(RECORD_HEADER_BYTES)?;
        self.scratch_push_record(None)
    }

    pub(super) fn scratch_begin_target_record(&mut self, id: u32) -> Result<()> {
        if id == 0 || id > i32::MAX as u32 || id >= self.next_record_id {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.scratch_check_record_count()?;
        self.scratch_check_buffered_bytes(RECORD_HEADER_BYTES)?;
        self.scratch_push_record(Some(id))
    }

    pub(super) fn scratch_write_body_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.scratch_in_record() {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.scratch_check_buffered_bytes(bytes.len())?;
        let Encoder {
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        let (cap, w) = parts(store, ws);
        let index = w.records.len - 1;
        let mut body = read_buf(
            slab_read(cap, &w.records, index),
            RECORD_BODY_OFF,
            RECORD_BODY_LEN,
            RECORD_BODY_CAP,
        );
        if buf_push(cap, &mut body, bytes).is_err() {
            return Err(latch_scratch_shortfall(failure, shortfall, cap));
        }
        write_buf(slab_write(cap, &w.records, index), RECORD_BODY_OFF, body);
        Ok(())
    }

    /// R23 新钩子在**已打开记录**上的写入接缝（§5①、②、⑤；身份目标用）：在当前记录体上按
    /// `body_len` 做**物理预留**（沿用 [`buf_reserve`] 的 `max(64)` 起翻倍语义，可大于体长），
    /// 但只借出**恰为体长**的已清零段（`[off + len, off + len + body_len)`）。
    ///
    /// 试准备不锁存：预留不下或取段越界/交叠时返回 [`BulkInto::NotStarted`]，**不写首错、不写
    /// 容量诊断**，由调用方回到同一既有分派。`Some(n)` 在回调返回后校验 `n == 预期体长` 与
    /// 「段仍在范围、不交叠」（I1）后原位提交；`Err` 与长度不符**不提交**、长度不变，按既有 `1`
    /// 返回；`Ok(None)` **撤销本次未提交的试探预留**（`B` 恢复到 `B_pre`、记录体描述符恢复原样，
    /// 旧路径随后自行按需扩容一次，不构成二次扩容）。切片只在回调期间借出。
    pub(super) fn scratch_bulk_into_body<T>(
        &mut self,
        items: &[T],
        body_len: usize,
        hook: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
    ) -> Result<BulkInto> {
        let Encoder {
            store, ws, failure, ..
        } = self;
        let (cap, w) = parts(store, ws);
        if w.records.len == 0 {
            return Ok(BulkInto::NotStarted);
        }
        let index = w.records.len - 1;
        let body_pre = read_buf(
            slab_read(cap, &w.records, index),
            RECORD_BODY_OFF,
            RECORD_BODY_LEN,
            RECORD_BODY_CAP,
        );
        let mut body = body_pre;
        // 准备失败（长度溢出、预留不下）不作失败面：不接管，由调用方回到同一既有分派。
        let Some(need) = body.len.checked_add(body_len) else {
            return Ok(BulkInto::NotStarted);
        };
        // 撤销点 `B_pre`（§5③）：本次物理预留尚未提交，钩子未接管时把 `B` 恢复到该值。
        let mark = cap.arena_mark();
        if try_buf_reserve(cap, &mut body, need).is_none() {
            // 预留不下时 arena 未被触碰（`try_buf_reserve` 失败不推进 `B`）。
            return Ok(BulkInto::NotStarted);
        }
        let start = body.off + body.len;
        // 取段（可失败，I1）：范围与不交叠复用既有判据；失败按准备失败回同一分派，并撤销刚做的
        // 预留（记录体描述符尚未改写，无需恢复）。
        if !cap.arena_range_ok(start, body_len) {
            cap.arena_revert(mark);
            return Ok(BulkInto::NotStarted);
        }
        // 预留后的记录描述符：`Committed` 时提交、`Err` 时留在已占字节内；`Ok(None)` 时恢复。
        write_buf(slab_write(cap, &w.records, index), RECORD_BODY_OFF, body);
        let written = hook(items, cap.arena_bytes_mut(start, body_len))?;
        // 回调后校验（I1）：长度相符，且段仍在范围、不交叠；否则不提交、按既有 `1` 返回。
        match written {
            Some(n) if n == body_len && cap.arena_range_ok(start, body_len) => {
                body.len = need;
                write_buf(slab_write(cap, &w.records, index), RECORD_BODY_OFF, body);
                Ok(BulkInto::Committed(n))
            }
            // 长度不符或段越界/交叠：不提交、长度不变，按既有 `1` 返回（不得谎报成功）。
            Some(_) => Err(failure.record(capacity())),
            // 已声明但本次不适用：撤销未提交的试探预留并恢复记录体描述符，旧路径自行按需扩容
            // 一次（不构成二次扩容）。
            None => {
                cap.arena_revert(mark);
                write_buf(
                    slab_write(cap, &w.records, index),
                    RECORD_BODY_OFF,
                    body_pre,
                );
                Ok(BulkInto::Declined)
            }
        }
    }

    /// R23 新钩子的**普通记录**接缝（§5①；B1）：先以既有落位算法**无副作用地预检**「记录描述符
    /// + 记录体缓冲**两者都能落位**」，两者都通过才实际预留，且只预留体缓冲——描述符延后到
    /// 新钩子真正接管（或旧分派的下一次 `begin_record`）时申请。试准备**不写首错、不写容量诊断**，
    /// 也**不逻辑入栈**普通记录——只在**新钩子真正接管**时才 `slab_push` 一条记录。预留成功后的
    /// 记录体暂存于 `pending_body`；钩子未接管时撤销该未提交的试探预留（`B` 恢复到 `B_pre`、
    /// 丢弃 `pending_body` 里的预留体），旧分派看到与未试准备时完全相同的容量。预检失败时
    /// arena 原样不动。
    pub(super) fn scratch_bulk_into_new_record<T>(
        &mut self,
        items: &[T],
        body_len: usize,
        hook: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
    ) -> Result<BulkInto> {
        // 预算预检与物理预留都用不锁存路径：任何准备失败都只表示本次快路径不可用。
        if !self.scratch_record_count_ok() || !self.record_id_ok() {
            return Ok(BulkInto::NotStarted);
        }
        let Some(additional) = RECORD_HEADER_BYTES.checked_add(body_len) else {
            return Ok(BulkInto::NotStarted);
        };
        if !self.scratch_buffered_bytes_ok(additional) {
            return Ok(BulkInto::NotStarted);
        }
        let Encoder {
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        let (cap, w) = parts(store, ws);
        let pending_pre = w.pending_body;
        let mut body = pending_pre;
        let Some(need) = body.len.checked_add(body_len) else {
            return Ok(BulkInto::NotStarted);
        };
        // 体缓冲先、记录描述符后（描述符延后到接管/`begin_record`）：预检两次分配与随后的真实
        // 分配同序同参数，所以「预检通过」⇒两次预留都必然成功；反之两不预留，旧分派不受影响。
        let body_alloc = match grown_capacity(body.cap, need) {
            Some(body_cap) if need > body.cap => Some((body_cap, 1)),
            Some(_) => None,
            None => return Ok(BulkInto::NotStarted),
        };
        let next_records = w.records.len + 1;
        let descriptor_alloc = match slab_growth_bytes(&w.records, next_records) {
            Some(0) => None,
            Some(bytes) => Some((bytes, SLAB_ALIGN)),
            None => return Ok(BulkInto::NotStarted),
        };
        if !cap.arena_fits_in_order(body_alloc, descriptor_alloc) {
            return Ok(BulkInto::NotStarted);
        }
        // 撤销点 `B_pre`（§5③）：预检通过后的实际预留仍是未提交的试探预留（尚未逻辑入栈、
        // 未被任何成功写入提交），钩子未接管时把 `B` 恢复到该值。
        let mark = cap.arena_mark();
        if try_buf_reserve(cap, &mut body, need).is_none() {
            // 预留不下时 arena 未被触碰（`try_buf_reserve` 失败不推进 `B`）。
            return Ok(BulkInto::NotStarted);
        }
        let start = body.off + body.len;
        if !cap.arena_range_ok(start, body_len) {
            cap.arena_revert(mark);
            return Ok(BulkInto::NotStarted);
        }
        // 预留留存到钩子返回：`Err`／长度不符时按已占字节留在 arena 内（§5③ 的例外），
        // `Committed` 时由下面的记录消耗，`Ok(None)` 时撤销并丢弃。
        w.pending_body = body;
        let written = hook(items, cap.arena_bytes_mut(start, body_len))?;
        match written {
            Some(n) if n == body_len && cap.arena_range_ok(start, body_len) => {
                // 新钩子真正接管：此刻才逻辑入栈普通记录（描述符已在预检中确认可落位），并消费已
                // 预留的记录体。
                let mut committed = core::mem::replace(&mut w.pending_body, Buf::EMPTY);
                committed.len = need;
                let mut elem = [0u8; RECORD_STRIDE];
                put_u32(&mut elem, RECORD_TARGET_ID, NONE);
                write_buf(&mut elem, RECORD_BODY_OFF, committed);
                match slab_push(cap, &mut w.records, &elem) {
                    Ok(_) => Ok(BulkInto::Committed(n)),
                    Err(_) => Err(latch_scratch_shortfall(failure, shortfall, cap)),
                }
            }
            Some(_) => Err(failure.record(capacity())),
            // 已声明但本次不适用：撤销未提交的试探预留、丢弃 `pending_body` 里的预留体，旧路径
            // （`begin_record`）随后自行按需扩容一次（不构成二次扩容）。
            None => {
                cap.arena_revert(mark);
                w.pending_body = pending_pre;
                Ok(BulkInto::Declined)
            }
        }
    }

    pub(super) fn scratch_check_record_body(&self, body_len: usize) -> Result<()> {
        i32::try_from(body_len).map_err(|_| self.fail(Error::ResourceLimit))?;
        self.scratch_check_record_count()?;
        self.check_record_id()?;
        self.scratch_check_buffered_bytes(
            body_len
                .checked_add(8)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )
    }

    /// 把 arena 缓冲里的记录体接到 main（packed 时复制进 `main_buf`）。
    fn scratch_main_push_arena(&mut self, off: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let raw = !self.opts.packed;
        let Encoder {
            dst,
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        if raw {
            let Some(dst) = dst.as_deref_mut() else {
                return Err(failure.record(capacity()));
            };
            let main_len = ws.as_ref().expect("工作区").main_len;
            let start = 4usize.checked_add(main_len).ok_or_else(capacity)?;
            let end = start.checked_add(len).ok_or_else(capacity)?;
            if end > dst.len() {
                return Err(failure.record(capacity()));
            }
            let (cap, w) = parts(store, ws);
            dst[start..end].copy_from_slice(cap.arena_bytes(off, len));
            w.main_len = main_len + len;
            Ok(())
        } else {
            let (cap, w) = parts(store, ws);
            let main_len = w.main_buf.len;
            let need = main_len.checked_add(len).ok_or_else(capacity)?;
            if buf_reserve(cap, &mut w.main_buf, need).is_err() {
                return Err(latch_scratch_shortfall(failure, shortfall, cap));
            }
            let target = w.main_buf.off + main_len;
            cap.arena_copy_within(off, target, len);
            w.main_buf.len = need;
            w.main_len = need;
            Ok(())
        }
    }

    fn scratch_append_record_arena(&mut self, id: u32, off: usize, len: usize) -> Result<()> {
        let mut header = [0u8; RECORD_HEADER_BYTES];
        header[..4].copy_from_slice(&(id as i32).to_le_bytes());
        header[4..].copy_from_slice(&(len as i32).to_le_bytes());
        self.scratch_main_push(&header)?;
        self.scratch_main_push_arena(off, len)?;
        self.committed_records += 1;
        Ok(())
    }

    fn scratch_append_record(&mut self, id: u32, body: &[u8]) -> Result<()> {
        let mut header = [0u8; RECORD_HEADER_BYTES];
        header[..4].copy_from_slice(&(id as i32).to_le_bytes());
        header[4..].copy_from_slice(&(body.len() as i32).to_le_bytes());
        self.scratch_main_push(&header)?;
        self.scratch_main_push(body)?;
        self.committed_records += 1;
        Ok(())
    }

    pub(super) fn scratch_alloc_record(&mut self, body: &[u8]) -> Result<u32> {
        self.scratch_check_record_body(body.len())?;
        let id = self.next_record_id;
        self.scratch_append_record(id, body)?;
        self.next_record_id += 1;
        Ok(id)
    }

    pub(super) fn scratch_alloc_record_arena(&mut self, off: usize, len: usize) -> Result<u32> {
        self.scratch_check_record_body(len)?;
        let id = self.next_record_id;
        self.scratch_append_record_arena(id, off, len)?;
        self.next_record_id += 1;
        Ok(id)
    }

    pub(super) fn scratch_commit_target_record(&mut self, id: u32, body: &[u8]) -> Result<u32> {
        if id == 0 || id > i32::MAX as u32 || id >= self.next_record_id {
            return Err(self.fail(Error::InvalidArgument));
        }
        i32::try_from(body.len()).map_err(|_| self.fail(Error::ResourceLimit))?;
        self.scratch_check_record_count()?;
        self.scratch_check_buffered_bytes(
            body.len()
                .checked_add(RECORD_HEADER_BYTES)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )?;
        self.scratch_append_record(id, body)?;
        Ok(id)
    }

    fn scratch_commit_target_arena(&mut self, id: u32, off: usize, len: usize) -> Result<u32> {
        if id == 0 || id > i32::MAX as u32 || id >= self.next_record_id {
            return Err(self.fail(Error::InvalidArgument));
        }
        i32::try_from(len).map_err(|_| self.fail(Error::ResourceLimit))?;
        self.scratch_check_record_count()?;
        self.scratch_check_buffered_bytes(
            len.checked_add(RECORD_HEADER_BYTES)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )?;
        self.scratch_append_record_arena(id, off, len)?;
        Ok(id)
    }

    pub(super) fn scratch_end_record(&mut self) -> Result<u32> {
        if self.scratch_records_len() <= self.scope_floor.1 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let index = self.scratch_records_len() - 1;
        let (target_id, off, len) = {
            let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
            let w = self.scratch_ws();
            let elem = slab_read(cap, &w.records, index);
            (
                unopt(get_u32(elem, RECORD_TARGET_ID)),
                get_u64(elem, RECORD_BODY_OFF) as usize,
                get_u64(elem, RECORD_BODY_LEN) as usize,
            )
        };
        // 与拥有值路径同语义（`end_record` 先弹出、失败再恢复）：提交检查前先逻辑弹出当前
        // 描述符，否则待提交记录已被计入 `records` 却又按新增记录计一次（记录数与
        // `header + body` 双计），合法的恰好容量与合法 raw 预算会被误拒。
        // 描述符字节留在 arena 内（单调分配、不回收），失败时只需恢复栈长。
        self.ws.as_mut().expect("工作区").records.len = index;
        let committed = match target_id {
            Some(id) => self.scratch_commit_target_arena(id, off, len),
            None => self.scratch_alloc_record_arena(off, len),
        };
        match committed {
            Ok(id) => Ok(id),
            Err(error) => {
                self.ws.as_mut().expect("工作区").records.len = index + 1;
                Err(error)
            }
        }
    }

    pub(super) fn scratch_begin_element(&mut self) {
        self.element_depth = self.element_depth.saturating_add(1);
        let order = self.current_order.take();
        let Encoder {
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        let (cap, w) = parts(store, ws);
        let mut elem = [0u8; ORDER_STRIDE];
        put_u32(&mut elem, 0, opt(order));
        if slab_push(cap, &mut w.orders, &elem).is_err() {
            latch_scratch_shortfall(failure, shortfall, cap);
        }
    }

    pub(super) fn scratch_end_element(&mut self) {
        self.element_depth = self.element_depth.saturating_sub(1);
        let Encoder { store, ws, .. } = self;
        let (cap, w) = parts(store, ws);
        if w.orders.len == 0 {
            self.current_order = None;
            return;
        }
        let index = w.orders.len - 1;
        let order = unopt(get_u32(slab_read(cap, &w.orders, index), 0));
        w.orders.len = index;
        self.current_order = order;
    }

    /// 回调返回点先核借用后端：回调可以用 `Encoder::new` 把编码器整体换掉，此后任何
    /// arena/身份表访问只会 panic。缺后端时保留回调自身的错误；回调返回 `Ok` 时明确报
    /// `1 InvalidArgument`（用户 2026-11-18 裁决⑤：`encode_into` 不 panic、不输出空结果）。
    fn scratch_check_backend_after_callback(&self, encoded: Result<()>) -> Result<()> {
        if self.ws.is_some() && self.store.is_some() {
            return Ok(());
        }
        Err(match encoded {
            Err(error) => error,
            Ok(()) => self.fail(Error::InvalidArgument),
        })
    }

    /// 栈顶身份捕获；无捕获时返回 `None`。
    ///
    /// 工作区缺失（回调把编码器整体换掉）时同样返回 `None`：调用方据此报明确错误，而不是在
    /// 访问 arena/身份表时 panic（F3）。
    pub(super) fn scratch_capture_last(&self) -> Option<(u32, u32, (usize, usize, usize))> {
        let w = self.ws.as_ref()?;
        if w.captures.len == 0 {
            return None;
        }
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        let elem = slab_read(cap, &w.captures, w.captures.len - 1);
        Some((
            get_u32(elem, CAPTURE_ID),
            get_u32(elem, CAPTURE_STATE),
            (
                get_u64(elem, CAPTURE_ENTRY_FRAMES) as usize,
                get_u64(elem, CAPTURE_ENTRY_RECORDS) as usize,
                get_u64(elem, CAPTURE_ENTRY_DEPTH) as usize,
            ),
        ))
    }

    fn scratch_set_capture_state(&mut self, state: u32) {
        let Encoder {
            store,
            ws,
            failure: _,
            ..
        } = self;
        let (cap, w) = parts(store, ws);
        let index = w.captures.len - 1;
        put_u32(slab_write(cap, &w.captures, index), CAPTURE_STATE, state);
    }

    pub(super) fn scratch_capture_native(
        &mut self,
        f: impl FnOnce(&mut Encoder<'_>) -> Result<()>,
    ) -> Result<()> {
        let Some((id, state, _)) = self.scratch_capture_last() else {
            return Err(self.fail(Error::InvalidArgument));
        };
        if state != CAPTURE_UNSUBMITTED {
            return Err(self.fail(Error::InvalidArgument));
        }
        let boundary = self.scratch_boundary_fingerprint();
        let entry = (
            self.scratch_frames_len(),
            self.scratch_records_len(),
            self.element_depth,
        );
        let previous_floor = core::mem::replace(&mut self.scope_floor, (entry.0, entry.1));
        let committed = (|| -> Result<()> {
            self.scratch_begin_target_record(id)?;
            self.scratch_begin_element();
            let encoded = f(self);
            // 回调返回点先核后端：换掉编码器后读 arena/身份表只会 panic（F3）。
            self.scratch_check_backend_after_callback(encoded)?;
            if encoded.is_ok() {
                let target_bound = {
                    let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
                    let w = self.scratch_ws();
                    w.records.len > 0
                        && unopt(get_u32(
                            slab_read(cap, &w.records, w.records.len - 1),
                            RECORD_TARGET_ID,
                        )) == Some(id)
                };
                let current = (
                    self.scratch_frames_len(),
                    self.scratch_records_len(),
                    self.element_depth,
                );
                if !target_bound || current != (entry.0, entry.1 + 1, entry.2 + 1) {
                    return Err(self.fail(Error::InvalidArgument));
                }
            }
            self.scratch_end_element();
            let committed = encoded.and_then(|()| self.scratch_end_record())?;
            if committed != id {
                return Err(self.fail(Error::InvalidArgument));
            }
            Ok(())
        })();
        self.scope_floor = previous_floor;
        committed?;
        if self.scratch_boundary_fingerprint() != boundary {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.scratch_set_capture_state(CAPTURE_NATIVE_COMMITTED);
        Ok(())
    }

    pub(super) fn scratch_capture_object(
        &mut self,
        f: impl FnOnce(&mut Encoder<'_>) -> Result<()>,
    ) -> Result<()> {
        let Some((id, state, _)) = self.scratch_capture_last() else {
            return Err(self.fail(Error::InvalidArgument));
        };
        if state != CAPTURE_UNSUBMITTED {
            return Err(self.fail(Error::InvalidArgument));
        }
        let entry = (
            self.scratch_frames_len(),
            self.scratch_records_len(),
            self.element_depth,
        );
        {
            let Encoder {
                store,
                ws,
                failure: _,
                ..
            } = self;
            let (cap, w) = parts(store, ws);
            let index = w.captures.len - 1;
            let elem = slab_write(cap, &w.captures, index);
            put_u32(elem, CAPTURE_STATE, CAPTURE_CLAIMABLE);
            put_u64(elem, CAPTURE_ENTRY_FRAMES, entry.0 as u64);
            put_u64(elem, CAPTURE_ENTRY_RECORDS, entry.1 as u64);
            put_u64(elem, CAPTURE_ENTRY_DEPTH, entry.2 as u64);
        }
        let boundary = self.scratch_boundary_fingerprint();
        let previous_floor = core::mem::replace(&mut self.scope_floor, (entry.0, entry.1));
        let encoded = f(self);
        // 回调返回点先核后端（F3）：换掉后 `scratch_capture_last` 只会看到缺失的工作区。
        self.scratch_check_backend_after_callback(encoded)?;
        self.scope_floor = previous_floor;
        encoded?;
        let Some((current_id, current_state, _)) = self.scratch_capture_last() else {
            return Err(self.fail(Error::InvalidArgument));
        };
        if current_id != id || current_state != CAPTURE_OBJECT_COMMITTED {
            return Err(self.fail(Error::InvalidArgument));
        }
        if entry
            != (
                self.scratch_frames_len(),
                self.scratch_records_len(),
                self.element_depth,
            )
        {
            return Err(self.fail(Error::InvalidArgument));
        }
        if self.scratch_boundary_fingerprint() != boundary {
            return Err(self.fail(Error::InvalidArgument));
        }
        Ok(())
    }

    pub(super) fn scratch_check_order(&self, order: u32) -> Result<i32> {
        let w = self.scratch_ws();
        if w.frames.len == 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        let elem = slab_read(cap, &w.frames, w.frames.len - 1);
        let tag = i32::try_from(order).map_err(|_| self.fail(Error::InvalidArgument))?;
        if unopt(get_u32(elem, FRAME_LAST_TAG)).is_some_and(|last| tag <= last as i32) {
            return Err(self.fail(Error::InvalidArgument));
        }
        Ok(tag)
    }

    pub(super) fn scratch_check_emit_order(&self, tag: i32) -> Result<()> {
        let w = self.scratch_ws();
        if w.frames.len == 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        let elem = slab_read(cap, &w.frames, w.frames.len - 1);
        if unopt(get_u32(elem, FRAME_LAST_EMITTED)).is_some_and(|last| tag <= last as i32) {
            return Err(self.fail(Error::InvalidArgument));
        }
        Ok(())
    }

    pub(super) fn scratch_commit_order(&mut self, tag: i32) {
        let Encoder { store, ws, .. } = self;
        let (cap, w) = parts(store, ws);
        let index = w.frames.len - 1;
        put_u32(slab_write(cap, &w.frames, index), FRAME_LAST_TAG, tag as u32);
    }

    pub(super) fn scratch_field_depth(&self) -> Result<i32> {
        let w = self.scratch_ws();
        if w.frames.len == 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        Ok(get_u32(
            slab_read(cap, &w.frames, w.frames.len - 1),
            FRAME_DEPTH,
        ) as i32)
    }

    fn scratch_push_slot_unchecked(&mut self, tag: i32, payload: [u8; 8]) -> Result<()> {
        {
            let Encoder {
                store,
                ws,
                failure,
                shortfall,
                ..
            } = self;
            let (cap, w) = parts(store, ws);
            let index = w.frames.len - 1;
            let mut elem = [0u8; FRAME_STRIDE];
            elem.copy_from_slice(slab_read(cap, &w.frames, index));
            let depth = get_u32(&elem, FRAME_DEPTH) as i32;
            put_u32(&mut elem, FRAME_LAST_EMITTED, tag as u32);
            let mut slots = read_buf(
                &elem,
                FRAME_SLOTS_OFF,
                FRAME_SLOTS_LEN,
                FRAME_SLOTS_CAP,
            );
            let mut bytes = [0u8; SLOT_SIZE];
            bytes[..4].copy_from_slice(&depth.to_le_bytes());
            bytes[4..8].copy_from_slice(&tag.to_le_bytes());
            bytes[8..].copy_from_slice(&payload);
            if buf_push(cap, &mut slots, &bytes).is_err() {
                return Err(latch_scratch_shortfall(failure, shortfall, cap));
            }
            write_buf(&mut elem, FRAME_SLOTS_OFF, slots);
            slab_write(cap, &w.frames, index).copy_from_slice(&elem);
        }
        self.slot_count += 1;
        Ok(())
    }

    pub(super) fn scratch_push_slot(
        &mut self,
        depth: i32,
        tag: i32,
        payload: [u8; 8],
    ) -> Result<()> {
        self.check_slot_budget(1)?;
        self.scratch_check_emit_order(tag)?;
        self.scratch_check_buffered_bytes(SLOT_SIZE)?;
        let (frame_depth, has_frame) = {
            let w = self.scratch_ws();
            if w.frames.len == 0 {
                (0, false)
            } else {
                let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
                (
                    get_u32(slab_read(cap, &w.frames, w.frames.len - 1), FRAME_DEPTH) as i32,
                    true,
                )
            }
        };
        if !has_frame || depth != frame_depth {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.scratch_push_slot_unchecked(tag, payload)
    }

    pub(super) fn scratch_write_root_scalar(&mut self, payload: [u8; 8]) -> Result<()> {
        self.check_root()?;
        self.check_root_slot()?;
        self.scratch_check_buffered_bytes(SLOT_SIZE)?;
        let slot = root_slot(payload);
        {
            let Encoder {
                store,
                ws,
                failure,
                shortfall,
                ..
            } = self;
            let (cap, w) = parts(store, ws);
            if buf_assign(cap, &mut w.assist, &slot).is_err() {
                return Err(latch_scratch_shortfall(failure, shortfall, cap));
            }
        }
        self.root_reserved = 0;
        self.root_written = true;
        Ok(())
    }

    pub(super) fn scratch_write_root_record(&mut self, body: &[u8]) -> Result<()> {
        self.check_root()?;
        self.check_root_slot()?;
        i32::try_from(body.len()).map_err(|_| self.fail(Error::ResourceLimit))?;
        self.scratch_check_record_count()?;
        self.check_record_id()?;
        self.scratch_check_buffered_bytes(
            body.len()
                .checked_add(RECORD_HEADER_BYTES + SLOT_SIZE)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?,
        )?;
        let id = self.next_record_id;
        self.scratch_append_record(id, body)?;
        self.next_record_id += 1;
        self.scratch_write_root_scalar(u64::from(id).to_le_bytes())
    }

    pub(super) fn scratch_object_frame_plan(&self) -> Result<(i32, usize, Option<u32>)> {
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
            self.scratch_frames_len(),
            self.scratch_records_len(),
            self.element_depth,
        );
        let mut target = None;
        if let Some((id, state, capture_entry)) = self.scratch_capture_last() {
            if capture_entry == entry {
                match state {
                    CAPTURE_CLAIMABLE => target = Some(id),
                    CAPTURE_OBJECT_COMMITTED => {
                        return Err(self.fail(Error::InvalidArgument))
                    }
                    _ => {}
                }
            }
        }
        Ok((depth, next_open, target))
    }

    pub(super) fn scratch_enter_object_with(&mut self, poly: Option<PolyHeader>) -> Result<()> {
        let (depth, next_open, target) = self.scratch_object_frame_plan()?;
        let mut elem = [0u8; FRAME_STRIDE];
        put_u32(&mut elem, FRAME_DEPTH, depth as u32);
        put_u32(&mut elem, FRAME_LAST_TAG, NONE);
        put_u32(&mut elem, FRAME_LAST_EMITTED, NONE);
        put_u32(&mut elem, FRAME_SAVED_ORDER, opt(self.current_order));
        put_u32(&mut elem, FRAME_TARGET, opt(target));
        let (kind, tag, type_name) = poly_kind(poly);
        put_u32(&mut elem, FRAME_POLY_KIND, kind);
        put_u32(&mut elem, FRAME_POLY_TAG, tag);
        put_u32(&mut elem, FRAME_POLY_TYPE_NAME, type_name);
        write_buf(&mut elem, FRAME_SLOTS_OFF, Buf::EMPTY);
        {
            let Encoder {
                store,
                ws,
                failure,
                shortfall,
                ..
            } = self;
            let (cap, w) = parts(store, ws);
            if slab_push(cap, &mut w.frames, &elem).is_err() {
                return Err(latch_scratch_shortfall(failure, shortfall, cap));
            }
        }
        self.open = next_open;
        self.current_order = None;
        Ok(())
    }

    fn scratch_push_block(
        &mut self,
        body_pos: usize,
        slots_off: usize,
        slots_len: usize,
        poly: Option<PolyHeader>,
    ) -> Result<()> {
        let mut elem = [0u8; BLOCK_STRIDE];
        put_u64(&mut elem, BLOCK_BODY_POS, body_pos as u64);
        put_u64(&mut elem, BLOCK_SLOTS_OFF, slots_off as u64);
        put_u64(&mut elem, BLOCK_SLOTS_LEN, slots_len as u64);
        let (kind, tag, type_name) = poly_kind(poly);
        put_u32(&mut elem, BLOCK_POLY_KIND, kind);
        put_u32(&mut elem, BLOCK_POLY_TAG, tag);
        put_u32(&mut elem, BLOCK_POLY_TYPE_NAME, type_name);
        let Encoder {
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        let (cap, w) = parts(store, ws);
        match slab_push(cap, &mut w.blocks, &elem) {
            Ok(_) => Ok(()),
            Err(_) => Err(latch_scratch_shortfall(failure, shortfall, cap)),
        }
    }

    pub(super) fn scratch_leave_object(&mut self) -> Result<u32> {
        if self.scratch_frames_len() <= self.scope_floor.0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let index = self.scratch_frames_len() - 1;
        let (saved_order, target, poly, slots_off, slots_len) = {
            let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
            let w = self.scratch_ws();
            let elem = slab_read(cap, &w.frames, index);
            (
                unopt(get_u32(elem, FRAME_SAVED_ORDER)),
                unopt(get_u32(elem, FRAME_TARGET)),
                poly_of(
                    get_u32(elem, FRAME_POLY_KIND),
                    get_u32(elem, FRAME_POLY_TAG),
                    get_u32(elem, FRAME_POLY_TYPE_NAME),
                ),
                get_u64(elem, FRAME_SLOTS_OFF) as usize,
                get_u64(elem, FRAME_SLOTS_LEN) as usize,
            )
        };
        self.ws.as_mut().expect("工作区").frames.len = index;
        self.open = self.open.saturating_sub(1);
        self.current_order = saved_order;
        let body_bytes = poly.map_or(OBJECT_BODY_BYTES, PolyHeader::body_bytes);
        let placeholder = [0u8; 6 + OBJECT_BODY_BYTES];
        let body = &placeholder[..body_bytes];
        if let Some(id) = target {
            let Some((capture_id, state, _)) = self.scratch_capture_last() else {
                return Err(self.fail(Error::InvalidArgument));
            };
            if capture_id != id || state != CAPTURE_CLAIMABLE {
                return Err(self.fail(Error::InvalidArgument));
            }
            self.scratch_commit_target_record(id, body)?;
            let body_pos = self.scratch_main_len() - body_bytes;
            self.scratch_push_block(body_pos, slots_off, slots_len, poly)?;
            self.scratch_set_capture_state(CAPTURE_OBJECT_COMMITTED);
            return Ok(id);
        }
        self.scratch_check_record_count()?;
        self.check_record_id()?;
        let writes_root = self.open == 0 && self.element_depth == 0;
        let field_tag = match self.current_order {
            Some(order) if !writes_root => Some(order as i32),
            _ => None,
        };
        let need = if writes_root {
            self.check_root()?;
            self.check_root_slot()?;
            RECORD_HEADER_BYTES + body_bytes + SLOT_SIZE
        } else if let Some(tag) = field_tag {
            self.check_slot_budget(1)?;
            self.scratch_check_emit_order(tag)?;
            RECORD_HEADER_BYTES + body_bytes + SLOT_SIZE
        } else {
            RECORD_HEADER_BYTES + body_bytes
        };
        self.scratch_check_buffered_bytes(need)?;
        let id = self.next_record_id;
        self.scratch_append_record(id, body)?;
        self.next_record_id += 1;
        let body_pos = self.scratch_main_len() - body_bytes;
        self.scratch_push_block(body_pos, slots_off, slots_len, poly)?;
        if writes_root {
            self.scratch_write_root_scalar(u64::from(id).to_le_bytes())?;
        } else if let Some(tag) = field_tag {
            self.scratch_push_slot_unchecked(tag, u64::from(id).to_le_bytes())?;
            self.current_order = None;
        }
        Ok(id)
    }

    fn scratch_entry_family(family: ReferenceFamily) -> super::typed_scratch::ReferenceFamily {
        match family {
            ReferenceFamily::Rc => super::typed_scratch::ReferenceFamily::Rc,
            #[cfg(feature = "std")]
            ReferenceFamily::Arc => super::typed_scratch::ReferenceFamily::Arc,
        }
    }

    /// 一次查找同时取槽下标、记录 id、类型与状态：下标只在本帧内用，不跨用户回调持有借用。
    fn scratch_identity(
        &self,
        family: ReferenceFamily,
        address: usize,
    ) -> Option<(usize, u32, EntryState, TypeId)> {
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        let key = Self::scratch_entry_family(family);
        cap.entries()
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.family == key && entry.address == address)
            .map(|(index, entry)| (index, entry.id, entry.state, entry.target))
    }

    fn scratch_identity_len(&self) -> usize {
        let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
        cap.entries_len()
    }

    /// 按槽下标改状态：条目只追加、不移动，跨用户回调后用下标访问仍指向同一条。
    fn scratch_set_identity_state(&mut self, index: usize, state: EntryState) {
        let Encoder { store, ws, .. } = self;
        let (cap, _) = parts(store, ws);
        if let Some(entry) = cap.entries_mut().get_mut(index) {
            entry.state = state;
        }
    }

    pub(super) fn scratch_resolve_weak<T: 'static>(
        &mut self,
        value: &alloc::rc::Weak<T>,
    ) -> Result<u32> {
        self.scratch_resolve_weak_identity::<T>(
            ReferenceFamily::Rc,
            alloc::rc::Weak::as_ptr(value) as usize,
            || {
                let guard: alloc::rc::Weak<dyn core::any::Any> = value.clone();
                Guard::Rc(guard)
            },
        )
    }

    pub(super) fn scratch_resolve_rc<T: AceEncode + 'static>(
        &mut self,
        value: &Rc<T>,
    ) -> Result<u32> {
        self.scratch_resolve_strong_identity::<T>(
            ReferenceFamily::Rc,
            Rc::as_ptr(value) as usize,
            || {
                let weak: alloc::rc::Weak<T> = Rc::downgrade(value);
                let guard: alloc::rc::Weak<dyn core::any::Any> = weak;
                Guard::Rc(guard)
            },
            &**value,
        )
    }

    #[cfg(feature = "std")]
    pub(super) fn scratch_resolve_arc<T: AceEncode + 'static>(
        &mut self,
        value: &Arc<T>,
    ) -> Result<u32> {
        self.scratch_resolve_strong_identity::<T>(
            ReferenceFamily::Arc,
            Arc::as_ptr(value) as usize,
            || {
                let weak: alloc::sync::Weak<T> = Arc::downgrade(value);
                let guard: alloc::sync::Weak<dyn core::any::Any> = weak;
                Guard::Arc(guard)
            },
            &**value,
        )
    }

    #[cfg(feature = "std")]
    pub(super) fn scratch_resolve_sync_weak<T: 'static>(
        &mut self,
        value: &alloc::sync::Weak<T>,
    ) -> Result<u32> {
        self.scratch_resolve_weak_identity::<T>(
            ReferenceFamily::Arc,
            alloc::sync::Weak::as_ptr(value) as usize,
            || {
                let guard: alloc::sync::Weak<dyn core::any::Any> = value.clone();
                Guard::Arc(guard)
            },
        )
    }

    /// 首次查找（含类型校验）后插入；返回槽下标供后续状态转换用下标访问。
    fn scratch_resolve_weak_identity<T: 'static>(
        &mut self,
        family: ReferenceFamily,
        address: usize,
        guard: impl FnOnce() -> Guard,
    ) -> Result<u32> {
        if let Some((_, id, _, target)) = self.scratch_identity(family, address) {
            if target != TypeId::of::<T>() {
                return Err(self.fail(Error::InvalidReference));
            }
            return Ok(id);
        }
        let references = self
            .scratch_identity_len()
            .checked_add(1)
            .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        if references > self.opts.max_references {
            return Err(self.fail(Error::ResourceLimit));
        }
        let id = self.reserve_record_id()?;
        let entry = Entry {
            family: Self::scratch_entry_family(family),
            address,
            target: TypeId::of::<T>(),
            id,
            state: EntryState::Reserved,
            guard: guard(),
        };
        self.scratch_push_entry(entry)?;
        Ok(id)
    }

    fn scratch_push_entry(&mut self, entry: Entry) -> Result<usize> {
        let Encoder {
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        let (cap, _) = parts(store, ws);
        match cap.init(entry) {
            Ok(index) => Ok(index),
            // 用户 2026-11-18 裁决（暂定）：两类容量类情形都映射既有 `1`（后续可细分）。
            Err(InitError::InsufficientEntries | InitError::ArenaCollision) => {
                Err(latch_scratch_shortfall(failure, shortfall, cap))
            }
        }
    }

    fn scratch_resolve_strong_identity<T: AceEncode + 'static>(
        &mut self,
        family: ReferenceFamily,
        address: usize,
        guard: impl FnOnce() -> Guard,
        inner: &T,
    ) -> Result<u32> {
        let identity = self.scratch_identity(family, address);
        let (index, id) = if let Some((index, id, state, target)) = identity {
            if target != TypeId::of::<T>() {
                return Err(self.fail(Error::InvalidReference));
            }
            match state {
                EntryState::Active => return Err(self.fail(Error::CycleUnsupported)),
                EntryState::Done => return Ok(id),
                EntryState::Reserved => {
                    self.scratch_set_identity_state(index, EntryState::Active);
                    (index, id)
                }
            }
        } else {
            let references = self
                .scratch_identity_len()
                .checked_add(1)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?;
            if references > self.opts.max_references {
                return Err(self.fail(Error::ResourceLimit));
            }
            let id = self.reserve_record_id()?;
            let entry = Entry {
                family: Self::scratch_entry_family(family),
                address,
                target: TypeId::of::<T>(),
                id,
                state: EntryState::Active,
                // 惰性 guard（与拥有值路径一致）：只有新槽插入时才多持有一个 `Weak`，
                // 预留身份转强期间不得让读取 `weak_count` 的手写回调观察到差异。
                guard: guard(),
            };
            let index = self.scratch_push_entry(entry)?;
            (index, id)
        };
        self.scratch_push_capture(id)?;
        let encoded = inner.ace_encode_target(self);
        // 回调返回点先核后端（F3）：换掉编码器后 `scratch_pop_capture` 不得再访问 arena。
        // 后端仍在时仍先弹出捕获再传播错误，保持原有的捕获栈平衡语义。
        self.scratch_check_backend_after_callback(encoded)?;
        let capture = self.scratch_pop_capture();
        encoded?;
        let Some((capture_id, state, _)) = capture else {
            return Err(self.fail(Error::InvalidArgument));
        };
        if capture_id != id
            || !matches!(state, CAPTURE_NATIVE_COMMITTED | CAPTURE_OBJECT_COMMITTED)
        {
            return Err(self.fail(Error::InvalidArgument));
        }
        self.scratch_set_identity_state(index, EntryState::Done);
        Ok(id)
    }

    fn scratch_push_capture(&mut self, id: u32) -> Result<()> {
        let mut elem = [0u8; CAPTURE_STRIDE];
        put_u32(&mut elem, CAPTURE_ID, id);
        put_u32(&mut elem, CAPTURE_STATE, CAPTURE_UNSUBMITTED);
        put_u64(&mut elem, CAPTURE_ENTRY_FRAMES, 0);
        put_u64(&mut elem, CAPTURE_ENTRY_RECORDS, 0);
        put_u64(&mut elem, CAPTURE_ENTRY_DEPTH, 0);
        let Encoder {
            store,
            ws,
            failure,
            shortfall,
            ..
        } = self;
        let (cap, w) = parts(store, ws);
        match slab_push(cap, &mut w.captures, &elem) {
            Ok(_) => Ok(()),
            Err(_) => Err(latch_scratch_shortfall(failure, shortfall, cap)),
        }
    }

    fn scratch_pop_capture(&mut self) -> Option<(u32, u32, (usize, usize, usize))> {
        let capture = self.scratch_capture_last();
        if capture.is_some() {
            let w = self.ws.as_mut().expect("工作区");
            w.captures.len -= 1;
        }
        capture
    }

    /// raw envelope 的总长（`8 + M + A`）：与 `wire::envelope::encoded_len` 同一组边界检查。
    fn scratch_raw_len(main: usize, assist: usize, opts: &Options) -> Result<usize> {
        if main > i32::MAX as usize || assist > i32::MAX as usize {
            return Err(Error::ResourceLimit);
        }
        let total = main
            .checked_add(assist)
            .and_then(|n| n.checked_add(RAW_HEADER_BYTES))
            .ok_or(Error::ResourceLimit)?;
        if total > isize::MAX as usize || total > opts.max_raw_bytes {
            return Err(Error::ResourceLimit);
        }
        Ok(total)
    }

    /// 借用形态的收尾：沿用拥有值 `finish` 的相对次序，把 wire 写进 `dst` 并返回写入长度。
    ///
    /// 与 `finish` 的差别只在承载：`main` 在 raw 模式下已写在 `dst[4..]`，packed 模式下在
    /// arena；assist 恒在 arena，最后按根槽 + 完成块序组装。`dst` 容量不足、arena 不足与身份
    /// 物理相撞都映射既有 `1 InvalidArgument`（用户 2026-11-18 裁决，暂定，后续可细分）。
    ///
    /// 按 `&mut self` 收尾（不消费编码器）：探测入口在返回后还要读本编码器的私有容量归因，
    /// 以确认本次返回值确实来自「真实 `scratch` 不足」那个失败分支（需求 R29）。
    pub(super) fn finish_into(&mut self) -> Result<usize> {
        // 回调可用 `Encoder::new` 替换传入的编码器并正常返回：此时收尾拿不到 `dst` 与
        // 帧内能力借用（更无工作区），必须显式报错——不得 panic，也不得退化成空的拥有值组装。
        //
        // 顺序与嵌套路径的 `scratch_check_backend_after_callback` 一致：**先核后端、再读首错**。
        // 否则被换入的编码器锁存的首错（如 `max_records = 0` 引起的 `6 ResourceLimit`）会抢先
        // 成为返回值，与文档为替换场景登记的 `1 InvalidArgument` 不一致；回调自身直接返回的
        // `Err` 仍由 `encode_into_impl` 先行传播，不在此处被掩盖。
        if self.dst.is_none() || self.store.is_none() || self.ws.is_none() {
            return Err(self.fail(Error::InvalidArgument));
        }
        if let Some(error) = self.failure.get() {
            return Err(error);
        }
        // Weak 预留必须由同消息内的强目标填充，禁止输出悬空引用。
        {
            let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
            if cap
                .entries()
                .iter()
                .any(|entry| entry.state == EntryState::Reserved)
            {
                return Err(self.fail(Error::InvalidArgument));
            }
        }
        if self.scratch_in_record() || self.open != 0 || self.element_depth != 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        let blocks = self.scratch_ws().blocks.len;
        // 对象块由根槽引用；无根槽时块无处接入，拒绝生成偏移无效的布局。
        if blocks > 0 && self.scratch_ws().assist.len == 0 {
            return Err(self.fail(Error::InvalidArgument));
        }
        // 块按离开次序（即记录提交次序）接在根槽之后，槽号从 1 起（设计 §1 第 4 项）。
        let mut start = 1u32;
        for index in 0..blocks {
            let (body_pos, slots_len, poly) = {
                let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
                let w = self.scratch_ws();
                let elem = slab_read(cap, &w.blocks, index);
                (
                    get_u64(elem, BLOCK_BODY_POS) as usize,
                    get_u64(elem, BLOCK_SLOTS_LEN) as usize,
                    poly_of(
                        get_u32(elem, BLOCK_POLY_KIND),
                        get_u32(elem, BLOCK_POLY_TAG),
                        get_u32(elem, BLOCK_POLY_TYPE_NAME),
                    ),
                )
            };
            let count = (slots_len / SLOT_SIZE) as u32;
            // 多态对象头先于槽范围写入（FR-W07）。
            let header_bytes = match poly {
                Some(poly) => {
                    let mut head = [0u8; 6];
                    head[..2].copy_from_slice(&poly.tag.to_le_bytes());
                    if let Some(type_name_id) = poly.type_name_id {
                        head[2..6].copy_from_slice(&type_name_id.to_le_bytes());
                    }
                    let bytes = poly.header_bytes();
                    self.scratch_main_write_at(body_pos, &head[..bytes])?;
                    bytes
                }
                None => 0,
            };
            let mut range = [0u8; 8];
            range[..4].copy_from_slice(&start.to_le_bytes());
            range[4..].copy_from_slice(&count.to_le_bytes());
            self.scratch_main_write_at(body_pos + header_bytes, &range)?;
            start = start
                .checked_add(count)
                .ok_or_else(|| self.fail(Error::ResourceLimit))?;
        }
        let main_len = self.scratch_main_len();
        // 组装 assist：根槽后接完成块序（单调分配：合并缓冲不回收旧区段）。
        let (assist_off, assist_total) = if blocks == 0 {
            let w = self.scratch_ws();
            (w.assist.off, w.assist.len)
        } else {
            let mut total = self.scratch_ws().assist.len;
            for index in 0..blocks {
                let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
                let w = self.scratch_ws();
                let elem = slab_read(cap, &w.blocks, index);
                total = total
                    .checked_add(get_u64(elem, BLOCK_SLOTS_LEN) as usize)
                    .ok_or_else(|| self.fail(Error::ResourceLimit))?;
            }
            let merged = (|| -> Result<(usize, usize)> {
                let Encoder {
                    store,
                    ws,
                    failure,
                    shortfall,
                    ..
                } = self;
                let (cap, w) = parts(store, ws);
                let off = match cap.arena_alloc_zeroed(total, 1) {
                    Ok(off) => off,
                    Err(_) => return Err(latch_scratch_shortfall(failure, shortfall, cap)),
                };
                let mut cursor = 0usize;
                if w.assist.len > 0 {
                    cap.arena_copy_within(w.assist.off, off, w.assist.len);
                    cursor = w.assist.len;
                }
                for index in 0..w.blocks.len {
                    let elem = slab_read(cap, &w.blocks, index);
                    let slots_off = get_u64(elem, BLOCK_SLOTS_OFF) as usize;
                    let slots_len = get_u64(elem, BLOCK_SLOTS_LEN) as usize;
                    if slots_len > 0 {
                        cap.arena_copy_within(slots_off, off + cursor, slots_len);
                    }
                    cursor += slots_len;
                }
                let _ = failure;
                Ok((off, cursor))
            })();
            match merged {
                Ok(assembled) => assembled,
                // `latch_scratch_shortfall` 已经锁存这个失败（要么它就是首错，要么首错已存在并标记
                // 被取代），这里不得再走一次 `fail`：那会把刚锁存的首错标记成「被后续失败取代」。
                Err(error) => return Err(error),
            }
        };
        let raw_len = match Self::scratch_raw_len(main_len, assist_total, &self.opts) {
            Ok(len) => len,
            Err(error) => return Err(self.fail(error)),
        };
        if !self.opts.packed {
            // raw：main 已在 `dst[4..4+M]`，assist 从 arena 复制到末尾。
            let Some(dst) = self.dst.take() else {
                return Err(self.fail(Error::InvalidArgument));
            };
            if dst.len() < raw_len {
                return Err(self.fail(Error::InvalidArgument));
            }
            dst[..4].copy_from_slice(&(main_len as i32).to_le_bytes());
            dst[4 + main_len..8 + main_len].copy_from_slice(&(assist_total as i32).to_le_bytes());
            if assist_total > 0 {
                let cap = self.store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
                dst[8 + main_len..raw_len]
                    .copy_from_slice(cap.arena_bytes(assist_off, assist_total));
            }
            return Ok(raw_len);
        }
        // packed：不把完整 raw 写进 dst，逐 word 从逻辑 raw 流打包。
        let padded = match raw_len.checked_add(7) {
            Some(value) => value & !7,
            None => return Err(self.fail(Error::ResourceLimit)),
        };
        if padded > self.opts.max_raw_bytes || padded > isize::MAX as usize {
            return Err(self.fail(Error::ResourceLimit));
        }
        let packed = {
            let Encoder {
                store, ws, dst, ..
            } = self;
            let Some(dst) = dst.as_deref_mut() else {
                return Err(Error::InvalidArgument);
            };
            let cap = store.as_deref().expect("scratch 承载路径必须有帧内能力借用");
            let w = ws.as_ref().expect("scratch 承载路径必须有工作区");
            let main = if main_len == 0 {
                &[][..]
            } else {
                cap.arena_bytes(w.main_buf.off, main_len)
            };
            let assist = if assist_total == 0 {
                &[][..]
            } else {
                cap.arena_bytes(assist_off, assist_total)
            };
            pack_into_stream(
                &LogicalRaw {
                    prefix: [(main_len as i32).to_le_bytes(), (assist_total as i32).to_le_bytes()],
                    main,
                    assist,
                },
                padded,
                dst,
            )
        };
        match packed {
            Ok(len) => Ok(len),
            Err(error) => Err(self.fail(error)),
        }
    }
}

/// 逻辑 raw 流的只读视图：`[M 前缀][main][A 前缀][assist]`。
///
/// packed 逐 word 打包要「按逻辑位置读任意字节」（`ff` run 要原样重发该段），因此这里先
/// 算好两段前缀，再按逻辑下标取**同段切片**：只有跨段或越过逻辑末尾的 word 才在栈上拼接
/// （超出逻辑末尾的字节读作 0，即尾部补零）。
struct LogicalRaw<'x> {
    prefix: [[u8; 4]; 2],
    main: &'x [u8],
    assist: &'x [u8],
}

impl LogicalRaw<'_> {
    /// 覆盖逻辑下标 `pos` 的段切片与它在逻辑流中的起点。
    fn segment(&self, pos: usize) -> (&[u8], usize) {
        let main_at = 4;
        let assist_at = main_at + self.main.len();
        let assist_body_at = assist_at + 4;
        if pos < main_at {
            (&self.prefix[0], 0)
        } else if pos < assist_at {
            (self.main, main_at)
        } else if pos < assist_body_at {
            (&self.prefix[1], assist_at)
        } else {
            (self.assist, assist_body_at)
        }
    }

    /// 逻辑 word：整体落在同一段时直接借用该段切片，跨段或越过末尾时才拼进栈上 `word`。
    fn word<'a>(&'a self, pos: usize, word: &'a mut [u8; 8]) -> &'a [u8] {
        let (bytes, base) = self.segment(pos);
        if let Some(slice) = bytes.get(pos - base..pos - base + 8) {
            return slice;
        }
        *word = [0; 8];
        let mut filled = 0;
        let mut at = pos;
        while filled < 8 {
            let (bytes, base) = self.segment(at);
            let offset = at - base;
            let Some(available) = bytes.len().checked_sub(offset).filter(|&len| len > 0) else {
                break;
            };
            let take = available.min(8 - filled);
            word[filled..filled + take].copy_from_slice(&bytes[offset..offset + take]);
            filled += take;
            at += take;
        }
        word
    }

    /// 把逻辑 `[start, end)` 原样拷进 `dst`：每段只拷一次，仅跨段处拼接。容量不足时先写
    /// 满剩余容量再报同一个 `1`，保留逐字节写原有的**部分写入**行为。
    fn copy_literal(
        &self,
        start: usize,
        end: usize,
        dst: &mut [u8],
        out: &mut usize,
    ) -> Result<()> {
        let mut at = start;
        while at < end {
            let (bytes, base) = self.segment(at);
            let offset = at - base;
            // `ff` 续接段只含整 word 且字节全非零，必在逻辑流内；补零区没有字节可选，不会到此。
            let Some(available) = bytes.len().checked_sub(offset).filter(|&len| len > 0) else {
                break;
            };
            let take = available.min(end - at);
            let room = (dst.len() - *out).min(take);
            dst[*out..*out + room].copy_from_slice(&bytes[offset..offset + room]);
            *out += room;
            if room < take {
                return Err(capacity());
            }
            at += take;
        }
        Ok(())
    }
}

/// 标准 packed 的逐 word 打包（与 `wire::packed::pack` 同一单元规则）：
/// 每个 word 写 tag 与非零字节；`00`/`ff` 单元再写至多 255 个续接 word，`ff` 续接原样重发。
fn pack_into_stream(stream: &LogicalRaw<'_>, padded: usize, dst: &mut [u8]) -> Result<usize> {
    let mut out = 0usize;
    let mut pos = 0usize;
    let mut word = [0u8; 8];
    while pos < padded {
        let bytes = stream.word(pos, &mut word);
        let mut tag = 0u8;
        for (index, &byte) in bytes.iter().enumerate() {
            if byte != 0 {
                tag |= 1 << index;
            }
        }
        write_word_byte(dst, &mut out, tag)?;
        for &byte in bytes.iter() {
            if byte != 0 {
                write_word_byte(dst, &mut out, byte)?;
            }
        }
        pos += 8;
        if tag == 0 || tag == 0xff {
            let start = pos;
            while pos < padded && (pos - start) / 8 < 255 {
                let next = stream.word(pos, &mut word);
                if (tag == 0 && next.iter().any(|&byte| byte != 0))
                    || (tag == 0xff && next.contains(&0))
                {
                    break;
                }
                pos += 8;
            }
            write_word_byte(dst, &mut out, ((pos - start) / 8) as u8)?;
            if tag == 0xff {
                stream.copy_literal(start, pos, dst, &mut out)?;
            }
        }
    }
    Ok(out)
}

fn write_word_byte(dst: &mut [u8], out: &mut usize, byte: u8) -> Result<()> {
    if *out >= dst.len() {
        return Err(capacity());
    }
    dst[*out] = byte;
    *out += 1;
    Ok(())
}
