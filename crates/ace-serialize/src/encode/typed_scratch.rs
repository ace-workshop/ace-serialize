//! scratch 内的类型化身份槽存储（架构 §2.1.4）。
//!
//! 本模块是全仓唯一允许新增 `unsafe` 的路径：「`&mut [u8]` 字节区 ↔ 类型化身份槽」。
//! `Entry`/`EntryStore` 均为**模块私有**类型：二者都不经 `lib.rs` 导出、不进公共 API，也不出现在
//! 本模块之外的类型里；模块外只经 safe 入口访问，`unsafe fn` 一律不带任何 `pub`（含
//! `pub(super)`/`pub(crate)`）。
//!
//! `EntryStore` 是清理所有者：**构造（`new`）与销毁（`Drop`）都封在本模块内**，模块外只能经
//! [`with_store`] 的帧内受控作用域拿到 [`StoreCapView`]——一个 `dyn` 的、**unsized** 的能力视图，
//! 拿不到、也搬不动拥有值。因此安全调用方既拿不到拥有值，也无法用 `mem::forget` 跳过终止清理
//! （不变量 4、架构 §2.1.4/§2.1.5 的所有权边界）。
//!
//! scratch 的承载是 `MaybeUninit<u8>` 区：已初始化槽的 padding 可能未初始化，普通
//! `&mut [u8]` 无法承载（不变量 5）。构造时（「原字节仍有效」）做一次承载转换，并只记承载裸指针
//! + `len`（不带寿命参数，见 [`EntryStore`] 的字段注释），此后一切
//! 槽地址都由该承载指针按 checked 偏移派生（不经 `usize` 往返、保留 provenance），写槽用
//! `MaybeUninit` 写，只有已初始化前缀才视为 `Entry`，清理写完前不重借普通字节切片。
//!
//! 三态状态机（架构 §2.1.4）：`Bytes`（`initialized == 0`，无活条目，整片可作字节）、
//! `Typed`（`[0, initialized)` 内有活 `Entry`）、`Cleaning`（终止清理的私有中间态，
//! 不借出任何视图）。`Bytes` 内还要区分「活跃」与「已完成清理」，唯一定义见
//! [`EntryStore::is_cleaned`]。
//!
//! 终止清理只有一条顺序（架构 §2.1.4 顺序条 (a)–(f)）：结束所有视图 → 进入 `Cleaning` →
//! 逐项析构前缀 → 用裸指针把曾被类型化的整段恢复为有效字节 → 在同一 `Cleaning` 内把 arena
//! 占用 `B` 归零 → 转回 `Bytes`。`B` 的归零只发生在 (e)。两个清理半步
//! （`drop_prefix`、`reinit_typed_range`）是**模块私有**的：模块外只开放完整序列
//! [`EntryStore::terminate`]，它也是 `Drop` 的唯一入口。

use alloc::rc::Weak as RcWeak;
#[cfg(feature = "std")]
use alloc::sync::Weak as ArcWeak;
use core::any::{Any, TypeId};
use core::mem::MaybeUninit;
use core::ptr;
use core::ptr::NonNull;

/// 身份族：`Guard` 的析构必须按族分派，不得按裸地址猜（架构 §2.1.2、不变量 4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReferenceFamily {
    Rc,
    #[cfg(feature = "std")]
    Arc,
}

/// 身份三态（架构 §2.1.2）：`Reserved`/`Active`/`Done`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryState {
    Reserved,
    Active,
    Done,
}

/// 身份的弱 guard：只持有 `Weak<dyn Any>`，析构不执行用户代码（不变量 5、8）。
pub(super) enum Guard {
    Rc(RcWeak<dyn Any>),
    #[cfg(feature = "std")]
    Arc(ArcWeak<dyn Any>),
}

/// 身份条目（架构 §2.1.2；最终字段集合仍待定稿，见架构 §4「仍未闭合」①）。
/// `family + address` 仅供同次查询、绝不写 wire；`target` 单独保存，
/// 因为目标析构后无法再经 `upgrade()` 求类型。
pub(super) struct Entry {
    pub(super) family: ReferenceFamily,
    pub(super) address: usize,
    pub(super) target: TypeId,
    pub(super) id: u32,
    pub(super) state: EntryState,
    pub(super) guard: Guard,
}

/// `init` 失败的私有分支（架构 §2.1.4：两支不得合并，也不得新增公开错误码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitError {
    /// `N > C(0)`：arena 为空也装不下下一槽 ⇒ R19④ 已判定的身份条目不足，
    /// 调用方映射既有 `1 InvalidArgument`。
    InsufficientEntries,
    /// `N > C(B)` 且 `N <= C(0)`：仅与当前 arena 占用相撞，**不等于**已判定身份不足。
    /// 用户 2026-11-18 裁决（暂定）把该情形的映射也定为既有 `1 InvalidArgument`（后续可细分）；
    /// 本模块只返回该私有分支，不替调用方选定映射。
    ArenaCollision,
}

/// arena 单调分配失败（`arena_alloc_zeroed`）。
///
/// 用户 2026-11-18 裁决（暂定）：arena 不足按「容量不足」处理，调用方映射既有
/// `1 InvalidArgument`（后续可细分）。本模块不自造错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ArenaFull;

/// 真实「`scratch` 不足」的私有诊断（需求 R29／架构 §2.1.7）。
///
/// `1 InvalidArgument` 本身不能作归因依据——身份槽不足、双端相撞、arena 不足、`dst` 不足与部分
/// 语义错误都可能返 `1`——所以「这次到底是不是 `scratch` 不够、按同一起址要多少」只能由触发失败的
/// 分配点自己记下，保留到返回点；公开错误码与错误优先级都不因此改变，本诊断也不出模块。
/// 只记**首个**失败点：公开返回值就是首个失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScratchShortfall {
    /// 在**相同起址**下容纳本次调用所需的**总**字节数（`scratch.len()` 的目标值）的 checked
    /// 下界：**不是**「还差多少」，也**不是**一次即得的精确值——重试时换起址必须重算。
    pub(super) min_total: usize,
}

/// 存储三态（架构 §2.1.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreState {
    Bytes,
    Typed,
    Cleaning,
}

/// `encode_into` 帧内 scratch 的身份槽区与字节 arena 的分界（架构 §2.1.4）。
///
/// 构造时一次性取得那一块 `scratch` 的独占借用（唯一来源）；此后一切地址都由该借用
/// 派生的起点与自持的 `base_offset`/长度/计数重算，不缓存裸指针，也不接收外部切片或
/// 外部计数。条目区自低地址向上增长、arena 自高地址向下增长，`arena_bytes` 记 arena
/// 已占字节 `B`。
///
/// 本类型及其构造/销毁都是**模块内**的：模块外无从取得拥有值，故不变量 4 的清理责任
/// 没有「安全拿到再遗忘」的旁路。
///
/// 模块外的唯一使用形态是 [`with_store`]：帧内由本模块构造所有者、调用方只借能力，
/// 退出（正常或错误）时由本模块显式走一次 [`EntryStore::terminate`]。
struct EntryStore {
    /// 唯一的 scratch 借用：条目区与 arena 的同一来源（不变量 1、9）。承载为
    /// `MaybeUninit<u8>`——已初始化槽的 padding 可能未初始化（不变量 5）。
    ///
    /// 存成「裸指针 + 长度」而不是 `&'storage mut [MaybeUninit<u8>]`：那一块的独占借用由
    /// [`with_store`] 的 `&mut [u8]` 形参在整帧内持有，本类型不逃出该帧，故不需要寿命参数。
    /// 寿命参数一去，`Encoder<'a>` 就能直接持 `&'a mut EntryStore`：`&'a mut T` 对 `'a` 协变，
    /// 而把它写成带对象寿命的 `&'a mut (dyn + 'a)` 会把该寿命放进 `&mut` 的不变位置，让公开
    /// `Encoder<'a>` 由协变变不变（F-V）。
    scratch: NonNull<MaybeUninit<u8>>,
    /// scratch 的字节长度（`len`）。
    scratch_len: usize,
    /// 条目区起点相对 scratch 起点的对齐前导 `P`（`0 <= P < A`）。
    base_offset: usize,
    /// 已初始化条目前缀的唯一权威计数。
    initialized: usize,
    /// 曾被完整类型化的条目数，单调不减，供 `reinit_typed_range` 使用。
    ever_typed: usize,
    /// arena 已占字节 `B`（归属见架构 §2.1.5；本阶段尚无写入者）。
    arena_bytes: usize,
    /// arena 已分配**载荷**字节合计：各次成功分配的 `len` 之和，**不含对齐填充**。
    ///
    /// `arena_bytes - arena_payload` 就是 arena 尾端（即切片尾端侧）的对齐填充，它随切片长度变化；
    /// 同起址扩容后这段填充不会原样保留，故下界推导只能用它、不得用 `arena_bytes`。
    arena_payload: usize,
    /// 首个真实 `scratch` 不足的诊断（需求 R29）：只由失败点写入，供探测入口在返回点写 `need`。
    shortfall: Option<ScratchShortfall>,
    state: StoreState,
    /// 终止清理是否已完成（「已完成清理」判据的一部分，见 [`EntryStore::is_cleaned`]）。
    cleanup_completed: bool,
}

impl EntryStore {
    /// `S = size_of::<Entry>()`：由类型求值、不写死，也不得用字段字节和冒充（§2.1.3 断言 3）。
    const SIZE: usize = core::mem::size_of::<Entry>();
    /// `A = align_of::<Entry>()`：非零 2 的幂（§2.1.3）。
    const ALIGN: usize = core::mem::align_of::<Entry>();

    /// 取得 scratch 的唯一独占借用并计算对齐前导 `P = (A - base % A) % A`。
    ///
    /// 承载转换只在此处发生一次：此刻调用方交来的字节仍有效，转换后每个元素都允许未初始化，
    /// 于是「已初始化槽含未初始化 padding」不再与承载类型冲突（不变量 5）。
    fn new(scratch: &mut [u8]) -> Self {
        let base = scratch.as_ptr() as usize;
        let align = Self::ALIGN;
        let base_offset = (align - base % align) % align;
        debug_assert!(align.is_power_of_two() && Self::SIZE >= 1);
        debug_assert!(base_offset < align, "对齐前导最多 A - 1（§2.1.3）");
        let scratch_len = scratch.len();
        // `MaybeUninit<u8>` 与 `u8` 同尺寸同对齐（core 保证），`NonNull::cast` 只丢长度元数据、
        // 不改地址；每个元素都允许未初始化（不变量 1、5）。该区间的独占借用由 [`with_store`]
        // 的 `&mut [u8]` 形参在整帧内持有（不变量 9）。
        let scratch = NonNull::from(&mut *scratch).cast::<MaybeUninit<u8>>();
        Self {
            scratch,
            scratch_len,
            base_offset,
            initialized: 0,
            ever_typed: 0,
            arena_bytes: 0,
            arena_payload: 0,
            shortfall: None,
            state: StoreState::Bytes,
            cleanup_completed: false,
        }
    }

    /// 可用条目数 `C(B) = (len - P - B) / S`（架构 §2.1.3）：先做 `len - P - B` 再除，
    /// 向下取整；`len < P + B` 时为 0。
    ///
    /// 边界：`len == 0` 或 `len < P` 时直接返回 0，不做 `P + …` 运算、不形成槽指针；
    /// `initialized == 0` **不**使其返回 0（空片与否只看 `len`/`P`/`B`）。返回值 0 也**不**
    /// 等于「不足一槽」（`B` 可单独使其为 0），「arena 为空也装不下」只能以 `C(0)` 另行比较。
    pub(super) fn entry_capacity(&self) -> usize {
        self.capacity_for(self.arena_bytes)
    }

    /// 已初始化身份条目数（`Encoder` 的线性查找与计数用）。
    pub(super) fn entries_len(&self) -> usize {
        self.initialized
    }

    /// `C(arena_bytes)`：`entry_capacity` 与「空 arena 也装不下」（`C(0)`）判定的共用算式。
    fn capacity_for(&self, arena_bytes: usize) -> usize {
        let len = self.scratch_len;
        let prefix = self.base_offset;
        debug_assert!(prefix < Self::ALIGN);
        if len == 0 || len < prefix {
            return 0;
        }
        let Some(free) = len.checked_sub(prefix) else {
            return 0;
        };
        let Some(free) = free.checked_sub(arena_bytes) else {
            return 0;
        };
        free / Self::SIZE
    }

    /// 记录首个真实 `scratch` 不足诊断（需求 R29）：后续失败点不覆盖，因为公开返回值就是首个失败。
    ///
    /// 下界算式的两项（载荷落位、本次分配）都不必超过当前长度，而本次调用已经在当前长度下失败，
    /// 故所需总长必然大于 `scratch_len`：取较大者，既保证按 `need` 重试必然增长，也仍是 checked 下界。
    fn note_shortfall(&mut self, min_total: usize) {
        if self.shortfall.is_none() {
            let min_total = min_total.max(self.scratch_len.saturating_add(1));
            self.shortfall = Some(ScratchShortfall { min_total });
        }
    }

    /// 本次失败的 arena 分配在**同一起址**下所需的总字节数（`scratch.len()` 目标值）下界。
    ///
    /// 按**该次分配自己的字节数与其在下行对齐下的落位**算，不复用旧切片的 `B`、也不用统一公式覆盖
    /// 所有失败点（架构 §2.1.3）：所需总长 `T` 必须满足 `align_down(base + T - B' - len, align) - base
    /// >= P + N*S`（`initialized == 0` 时右侧为 `0`——**零槽不加也不扣对齐前导 `P`**），其中 `B'` 是
    /// 同起址扩容后该次分配之前的 arena 占用。`B'` 至少含全部已分配载荷（`arena_payload`），且失败点
    /// 的起点必须落在 `P + N*S` 之后的第一个 `align` 对齐点上，故取最小解的 checked 形式：
    /// `T = arena_payload + len + (P + N*S) + ((align - ((P + N*S + base % align) % align)) % align)`。
    ///
    /// **不得**把旧切片的 `arena_bytes` 当前缀占用：它含随切片尾端变化的对齐填充，同起址扩容后那段
    /// 填充不保留，带进来会把 `need` 高报（源码反例见 raw `Leaf { v: 7 }`、`scratch.len() = 991`：
    /// 旧 `B = 967` 含 7 字节尾部填充 ⇒ 999，而 992 字节同起址即可成功）。
    /// 溢出或算不出时返回 `None`（此时不写诊断，`need` 保持调用方值）。
    fn arena_min_total(&self, len: usize, align: usize) -> Option<usize> {
        let slot_end = if self.initialized > 0 {
            self.initialized
                .checked_mul(Self::SIZE)?
                .checked_add(self.base_offset)?
        } else {
            0
        };
        let base_rem = (self.scratch.as_ptr() as usize) % align;
        let shift = slot_end.checked_add(base_rem)?;
        let pad = (align - shift % align) % align;
        self.arena_payload
            .checked_add(len)?
            .checked_add(slot_end)?
            .checked_add(pad)
    }

    /// 从 arena 高地址侧单调分配 `len` 字节并按 `align` 对齐，返回 scratch 内绝对偏移。
    ///
    /// 不回收：`arena_bytes` 只增不减（用户 2026-11-18 裁决），因此新区域落在
    /// `[len - B_new, len)` 内、与已占 arena 直接相邻（中间只可能有对齐填充）。
    /// 分配时把 `[start, 旧 arena 起点)` **整体写零**，于是已占 arena 恒为已初始化字节
    /// （不变量 5）：对齐填充与本次分配一起清零，后续 `arena_bytes`/`arena_bytes_mut` 才可
    /// 安全地形成普通 `[u8]` 视图。
    ///
    /// 状态前提在 **release 同样生效**：终止清理一旦开始或完成就不得再增长 arena
    /// （§2.1.4 顺序条「适用范围」）。该检查在任何内存修改之前完成。
    pub(super) fn arena_alloc_zeroed(
        &mut self,
        len: usize,
        align: usize,
    ) -> Result<usize, ArenaFull> {
        self.arena_alloc_zeroed_impl(len, align, true)
    }

    /// 试探性分配：失败时**不写 `shortfall` 诊断**（R23 新钩子的试准备用，§5「准备失败不得锁存」）。
    ///
    /// 与 [`EntryStore::arena_alloc_zeroed`] 走同一落位与写零路径，只是失败时不留任何容量归因；
    /// 成功时照常推进 arena 占用（单调、不回收）。用于「先试能否容纳、失败就回到既有分派」的场景，
    /// 以免一次试探把后续真实失败的下界改成试探分配点。
    pub(super) fn try_arena_alloc_zeroed(&mut self, len: usize, align: usize) -> Option<usize> {
        self.arena_alloc_zeroed_impl(len, align, false).ok()
    }

    /// R23 未提交试探预留的撤销点 `B_pre`（架构 §5「R23 写入接缝」③）：`B`（`arena_bytes`）与
    /// 载荷合计（`arena_payload`，仅供 `need` 下界推导）的快照。
    ///
    /// 撤销只允许回退到**试探预留之前**的占用：已提交的占用（前序记录体、已完成块槽、描述符、
    /// assist 等）不经过这里，也不得回缩。
    pub(super) fn arena_mark(&self) -> ArenaMark {
        ArenaMark {
            bytes: self.arena_bytes,
            payload: self.arena_payload,
        }
    }

    /// 撤销**未提交的试探预留**：把 `B` 恢复到 `mark` 并丢弃其间的分配（架构 §5「R23 写入接缝」③）。
    ///
    /// 前提：调用方已结束指向被撤销区间的所有视图（钩子的借用在回调返回时结束，不变量 6）；
    /// 恢复后该段重新归入自由区，后续 `arena_alloc_zeroed` 会把它与对齐填充一起重新写零。
    pub(super) fn arena_revert(&mut self, mark: ArenaMark) {
        debug_assert!(
            mark.bytes <= self.arena_bytes && mark.payload <= self.arena_payload,
            "撤销点必须来自本次调用更早的 arena 占用（已提交占用不得回缩）"
        );
        self.arena_bytes = mark.bytes;
        self.arena_payload = mark.payload;
    }

    fn arena_alloc_zeroed_impl(
        &mut self,
        len: usize,
        align: usize,
        note_shortfall: bool,
    ) -> Result<usize, ArenaFull> {
        assert!(
            matches!(self.state, StoreState::Bytes | StoreState::Typed) && !self.cleanup_completed,
            "终止清理开始后不得再增长 arena（架构 §2.1.4 顺序条）"
        );
        debug_assert!(align.is_power_of_two(), "arena 对齐必须是 2 的幂");
        let Some(start) = self.arena_span_fit(len, align) else {
            if note_shortfall {
                // 真实 `scratch` 不足：在该失败点记下同起址所需总长的下界（需求 R29）。
                let min_total = self.arena_min_total(len, align);
                if let Some(min_total) = min_total {
                    self.note_shortfall(min_total);
                }
            }
            return Err(ArenaFull);
        };
        // `arena_span_fit` 已 checked 核过 `start <= old_end <= total`，下面的减法与区间均不会下溢。
        let total = self.scratch_len;
        let old_end = total - self.arena_bytes;
        let new_used = total - start;
        let base_ptr = self.scratch.as_ptr().cast::<u8>();
        // SAFETY: `[start, old_end)` 落在唯一借用之内（`start <= old_end <= total`，均已 checked）；
        // 这一段包含本次分配与对齐填充，其字节尚为未初始化（或将被覆盖），故只用承载指针
        // `write_bytes` 写零、**不先构造 `&mut [u8]`**（不变量 5）；`write_bytes` 不 panic、
        // 不执行用户代码（不变量 8），且不改动 `[old_end, total)` 的已初始化 arena。
        unsafe { ptr::write_bytes(base_ptr.add(start), 0, old_end - start) };
        self.arena_bytes = new_used;
        // 载荷合计不超过 `B`（`new_used <= total`），故这里不会溢出。
        self.arena_payload += len;
        Ok(start)
    }

    /// 只读落位预检（R23 试准备用）：按给定顺序的 0–2 次分配能否都落位，**不分配、不写零、
    /// 不推进占用**。每次分配用 `(len, align)`，`None` 表示该次不分配。与
    /// [`EntryStore::arena_alloc_zeroed`] 共用同一 `arena_span_fit` 判据，因此预检通过即意味着
    /// 随后的真实预留必然成功；预检失败时 arena 仍保持原样，不会把旧分派挤到容量不足。
    pub(super) fn arena_fits_in_order(
        &self,
        first: Option<(usize, usize)>,
        second: Option<(usize, usize)>,
    ) -> bool {
        let mut arena_bytes = self.arena_bytes;
        for (len, align) in [first, second].into_iter().flatten() {
            let Some(start) = self.arena_span_fit_with(arena_bytes, len, align) else {
                return false;
            };
            // `arena_span_fit_with` 已确保 `start <= total`。
            arena_bytes = self.scratch_len - start;
        }
        true
    }

    /// 本次分配的起点偏移与容量判定（`None` 即 `arena_alloc_zeroed` 的三类失败）：
    /// 向下对齐落位 + 不变量 7 的写入侧条件式（`initialized > 0` 时条目区与 arena 不得交叠；
    /// `initialized == 0` 时只要求 arena 在界内）。
    fn arena_span_fit(&self, len: usize, align: usize) -> Option<usize> {
        self.arena_span_fit_with(self.arena_bytes, len, align)
    }

    /// 同 [`Self::arena_span_fit`]，但以传入的 arena 占用（`arena_bytes`）为起点，使得未实际发生的
    /// 分配也可纯判定（R23 预检的连续落位）。
    fn arena_span_fit_with(&self, arena_bytes: usize, len: usize, align: usize) -> Option<usize> {
        let total = self.scratch_len;
        let old_end = total.checked_sub(arena_bytes)?;
        let raw_start = old_end.checked_sub(len)?;
        let base = self.scratch.as_ptr() as usize;
        let aligned = base.checked_add(raw_start)? & !(align - 1);
        let start = aligned.checked_sub(base)?;
        let new_used = total.checked_sub(start)?;
        let fits = if self.initialized > 0 {
            self.initialized
                .checked_mul(Self::SIZE)
                .and_then(|bytes| bytes.checked_add(self.base_offset))
                .and_then(|end| end.checked_add(new_used))
                .is_some_and(|end| end <= total)
        } else {
            new_used <= total
        };
        fits.then_some(start)
    }

    /// 借出/提交前的可失败区间判定（I1）：与 [`EntryStore::arena_bytes_mut`] 同一状态、范围与
    /// 不交叠判据，但越界或交叠时返回 `false` 而不是断言，供 R23 取段与提交前校验用。
    pub(super) fn arena_range_ok(&self, offset: usize, len: usize) -> bool {
        if !matches!(self.state, StoreState::Bytes | StoreState::Typed) {
            return false;
        }
        if len == 0 {
            return true;
        }
        self.overlap_free() && self.arena_contains(offset, len)
    }

    /// 已占用 arena 的区间判定：`[len - B, len)` 内。
    fn arena_contains(&self, offset: usize, len: usize) -> bool {
        let total = self.scratch_len;
        let begin = total.saturating_sub(self.arena_bytes);
        offset >= begin && offset.checked_add(len).is_some_and(|end| end <= total)
    }

    /// 已占用 arena 的只读区间视图（不变量 6）：只在 `Bytes`/`Typed` 态借出，`Cleaning` 不借出。
    pub(super) fn arena_bytes(&self, offset: usize, len: usize) -> &[u8] {
        assert!(
            matches!(self.state, StoreState::Bytes | StoreState::Typed),
            "Cleaning 期间不借出任何视图（架构 §2.1.4 顺序 (a)）"
        );
        if len == 0 {
            return &[];
        }
        assert!(
            self.overlap_free(),
            "arena 视图访问前必须按不变量 7 的条件式核类型前缀不交叠"
        );
        assert!(self.arena_contains(offset, len), "arena 区间必须落在已占用 arena 内");
        // SAFETY: 已占用 arena `[len - B, len)` 由 `arena_alloc_zeroed` 整体写零 ⇒ 全部字节已初始化；
        // 区间由 assert 确在界内；不可变视图由 `&self` 借出，不越过该块 scratch 的独占借用（由调用帧持有），与条目区不相交
        // （不变量 6、7、9）。
        unsafe {
            core::slice::from_raw_parts(self.scratch.as_ptr().cast::<u8>().add(offset), len)
        }
    }

    /// 已占用 arena 的可写区间视图（不变量 6）：与只读视图同样的边界与状态要求。
    pub(super) fn arena_bytes_mut(&mut self, offset: usize, len: usize) -> &mut [u8] {
        assert!(
            matches!(self.state, StoreState::Bytes | StoreState::Typed),
            "Cleaning 期间不借出任何视图（架构 §2.1.4 顺序 (a)）"
        );
        if len == 0 {
            return &mut [];
        }
        assert!(
            self.overlap_free(),
            "arena 可写视图访问前必须按不变量 7 的条件式核类型前缀不交叠"
        );
        assert!(self.arena_contains(offset, len), "arena 区间必须落在已占用 arena 内");
        // SAFETY: 同 `arena_bytes`；`&mut self` 独占借出、不与其它 arena/条目视图共存（不变量 6）。
        unsafe {
            core::slice::from_raw_parts_mut(self.scratch.as_ptr().cast::<u8>().add(offset), len)
        }
    }

    /// arena 内两段区间之间的复制（已占区全为已初始化字节，可当普通字节用 `copy_within`）。
    pub(super) fn arena_copy_within(&mut self, src: usize, dst: usize, len: usize) {
        if len == 0 {
            return;
        }
        assert!(
            matches!(self.state, StoreState::Bytes | StoreState::Typed),
            "Cleaning 期间不借出任何视图（架构 §2.1.4 顺序 (a)）"
        );
        assert!(
            self.overlap_free(),
            "arena 复制前必须按不变量 7 的条件式核类型前缀不交叠"
        );
        assert!(
            self.arena_contains(src, len) && self.arena_contains(dst, len),
            "arena 复制两段都必须落在已占用 arena 内"
        );
        let begin = self.scratch_len - self.arena_bytes;
        // SAFETY: 已占用 arena 全为已初始化字节；视图只在本次调用内存在、不越过该块 scratch 的独占借用，
        // 且以 `&mut self` 独占（不变量 5、6、9）。`copy_within` 是 safe 的切片内复制，不 panic。
        let arena = unsafe {
            core::slice::from_raw_parts_mut(
                self.scratch.as_ptr().cast::<u8>().add(begin),
                self.arena_bytes,
            )
        };
        arena.copy_within(src - begin..src - begin + len, dst - begin);
    }

    /// 申请并写入下一槽；成功返回写入的槽索引 `initialized`（返回时 `initialized` 已加 1、
    /// `ever_typed` 已取大）。失败不落槽、不改任何计数与状态，且两支不合并
    /// （[`InitError::InsufficientEntries`] / [`InitError::ArenaCollision`]）。
    ///
    /// 状态前提在 **release 同样生效**：终止清理一旦开始（`Cleaning`）或已完成
    /// （`cleanup_completed`）就不得再 `init`（架构 §2.1.4 顺序条「适用范围」与迁移表）。
    /// 该检查在任何内存修改之前完成，故违约时既不写槽也不改计数。
    pub(super) fn init(&mut self, entry: Entry) -> Result<usize, InitError> {
        assert!(
            matches!(self.state, StoreState::Bytes | StoreState::Typed) && !self.cleanup_completed,
            "终止清理开始后不得再 init（架构 §2.1.4 迁移表）"
        );
        let next = self
            .initialized
            .checked_add(1)
            .ok_or(InitError::InsufficientEntries)?;
        if next > self.entry_capacity() {
            // 两类不足必须分开（§2.1.3 断言 4）：只有「arena 为空也装不下」才是 R19④
            // 已判定的身份条目不足；`N > C(B)` 而 `N <= C(0)` 只是与当前 arena 占用相撞。
            // 用户 2026-11-18 裁决（暂定）把两者的映射都定为既有 `1`（后续可细分），
            // 但本模块只返回私有分支、不替调用方选定映射（架构 §3 第 5 条）。
            // 有槽的失败点才计其身份槽需求：`P + N*S + arena_payload` 为该次分配在同起址下的总长
            // 下界——只用载荷合计，不用含旧尾部填充的 `B`（理由见 `arena_min_total`）。
            let min_total = next
                .checked_mul(Self::SIZE)
                .and_then(|bytes| bytes.checked_add(self.base_offset))
                .and_then(|end| end.checked_add(self.arena_payload));
            if let Some(min_total) = min_total {
                self.note_shortfall(min_total);
            }
            return Err(if next > self.capacity_for(0) {
                InitError::InsufficientEntries
            } else {
                InitError::ArenaCollision
            });
        }
        let index = self.initialized;
        // SAFETY: 容量检查已按不变量 7 的写入侧条件式确认下一槽装得下（`N <= C(B)` 即
        // `P + N*S + B <= len`），`index == initialized`，地址由 `new` 的唯一借用派生
        // （不变量 1、2、7、9）。
        unsafe { self.init_at(index, entry) };
        // 写槽与提交 `initialized`/`ever_typed`/状态属同一不可失败序列
        // （不变量 3、架构 §2.1.4「清理幂等②」）。
        self.initialized = next;
        self.ever_typed = self.ever_typed.max(next);
        self.state = StoreState::Typed;
        Ok(index)
    }

    /// 仅 `[0, initialized)` 的独占视图；作用域内不得同时借出 arena（不变量 6）。
    ///
    /// 空片（`initialized == 0`）返回空视图，不形成类型化指针（§2.1.3 断言 1、2）。
    /// 状态前提在 **release 同样生效**：`Cleaning` 期间不得借出任何视图（顺序 (a)），
    /// 该检查在形成任何指针之前完成。
    pub(super) fn entries_mut(&mut self) -> &mut [Entry] {
        assert!(
            matches!(self.state, StoreState::Bytes | StoreState::Typed),
            "Cleaning 期间不借出任何视图（架构 §2.1.4 顺序 (a)）"
        );
        if self.initialized == 0 {
            return &mut [];
        }
        debug_assert!(!self.cleanup_completed, "已完成清理的 Bytes 不应有活条目");
        debug_assert!(self.initialized <= self.ever_typed);
        debug_assert!(
            self.prefix_fits(),
            "读/析构前必须核 P + N*S + B <= len（不变量 7）"
        );
        // SAFETY: 前缀 `[0, initialized)` 均已由 `init` 写入、按 `Entry` 对齐，且
        // `initialized > 0`；视图寿命由 `&mut self` 独占、不越过该块 scratch 的独占借用（不变量 2、6、9）。
        unsafe { self.entries_mut_raw() }
    }

    /// 仅 `[0, initialized)` 的共享视图（身份线性查找用；与 `entries_mut` 同一边界与状态要求）。
    ///
    /// 该视图同样不得跨越任何用户回调（架构 §2.1.4「回调边界」）。
    pub(super) fn entries(&self) -> &[Entry] {
        assert!(
            matches!(self.state, StoreState::Bytes | StoreState::Typed),
            "Cleaning 期间不借出任何视图（架构 §2.1.4 顺序 (a)）"
        );
        if self.initialized == 0 {
            return &[];
        }
        debug_assert!(!self.cleanup_completed, "已完成清理的 Bytes 不应有活条目");
        debug_assert!(self.initialized <= self.ever_typed);
        debug_assert!(
            self.prefix_fits(),
            "读/析构前必须核 P + N*S + B <= len（不变量 7）"
        );
        // SAFETY: 前缀 `[0, initialized)` 均已由 `init` 写入、按 `Entry` 对齐，且
        // `initialized > 0`；视图寿命由 `&self` 借用约束、不越过该块 scratch 的独占借用（不变量 2、6、9）。
        unsafe {
            core::slice::from_raw_parts(self.slot_ptr_shared(0).cast::<Entry>(), self.initialized)
        }
    }

    /// 终止清理 (a)–(c)：进入 `Cleaning` 并逐项析构前缀，析构完成后把 `initialized` 置 0。
    ///
    /// 空操作只限「已完成清理」（判据见 [`EntryStore::is_cleaned`]）；仍活跃的 `Bytes`
    /// （含 `initialized == 0` 且 `B == 0` 的回调提前失败路径）必须同样走首次迁移。
    ///
    /// 模块私有：模块外只能经 [`EntryStore::terminate`] 走完整序列，不得单独调用半步。
    fn drop_prefix(&mut self) {
        if self.is_cleaned() {
            debug_assert_eq!(self.initialized, 0);
            return;
        }
        let dropping = self.initialized;
        // (b) 先进入 `Cleaning`：此后不借出任何视图；字节视图的放行只由状态决定、
        // 与计数无关。
        self.state = StoreState::Cleaning;
        if dropping > 0 {
            debug_assert!(
                self.prefix_fits(),
                "析构前必须核 P + N*S + B <= len（不变量 7）"
            );
            // SAFETY: `dropping` 为进入清理前的 `initialized` 快照，`[0, dropping)` 每个元素
            // 都已由 `init` 写入，地址由唯一借用派生（不变量 1–3、9）。
            unsafe { self.drop_prefix_raw(dropping) };
        }
        // (c) 析构完成后才把前缀计数置 0（不变量 3）。
        self.initialized = 0;
    }

    /// 终止清理 (d)–(f)：把曾被类型化的整段恢复为有效字节，在同一 `Cleaning` 内把 `B` 归零，
    /// 全部完成后转回 `Bytes`。
    ///
    /// 空操作只限「已完成清理」；仍活跃的 `Bytes` 必须先经 [`EntryStore::drop_prefix`] 的
    /// 首次迁移进入 `Cleaning`。`ever_typed == 0` 时在求址前即为空步，但 (e) 归零 `B` 与
    /// (f) 转态仍必须执行。
    ///
    /// 模块私有：模块外只能经 [`EntryStore::terminate`] 走完整序列，不得单独调用半步。
    fn reinit_typed_range(&mut self) {
        if self.is_cleaned() {
            return;
        }
        debug_assert_eq!(
            self.state,
            StoreState::Cleaning,
            "活跃 Bytes 必须先经 drop_prefix 的首次迁移（架构 §2.1.4 迁移表）"
        );
        debug_assert_eq!(self.initialized, 0);
        // (d) 恢复范围只由单调不减的 `ever_typed` 决定，不以析构后归零的 `initialized` 判定。
        if self.ever_typed > 0 {
            let typed = self.ever_typed;
            // SAFETY: 对 `[P, P + ever_typed*S)` 只用承载指针 `write_bytes` 写零，不构造
            // `&mut [u8]` 字节切片（不变量 5）；`ever_typed` 不超过曾成功写入的槽数
            // （不变量 3），故该段在 scratch 内。
            unsafe { self.reinit_typed_raw(typed) };
        }
        // (e) 仍在同一 `Cleaning` 内归零 `B`（及其载荷合计）：只操作字节与计数器，不借
        // `ever_typed` 覆盖段。
        self.arena_bytes = 0;
        self.arena_payload = 0;
        // (f) 上述全部完成后才转回 `Bytes`。
        self.state = StoreState::Bytes;
        self.cleanup_completed = true;
    }

    /// 终止清理的唯一入口：`Drop` 与 [`with_store`] 的显式退出路径共用（重复调用按状态机幂等）。
    ///
    /// 这是本模块对外唯一开放的清理形态——两个半步都是模块私有的，调用方无法只做其中一步
    /// 就重开 `init` 或提前放行字节视图（架构 §2.1.4 顺序条「适用范围」）。
    pub(super) fn terminate(&mut self) {
        self.drop_prefix();
        self.reinit_typed_range();
    }

    /// 三态状态机「已完成清理」的唯一定义（全文同此口径，不另写一版）：
    /// 状态为 `Bytes`，且 `B == 0`，且 `ever_typed` 覆盖段已写零（`ever_typed == 0`
    /// 视为覆盖段为空、已满足），且终止清理已完成（曾进入 `Cleaning` 并把 `B` 归零）。
    ///
    /// 因此**不得**只凭 `n == 0`、`initialized == 0` 或 `B == 0` 判定已完成：新建的
    /// `Bytes` 同样满足 `ever_typed == 0` 且 `B == 0`，但它仍可 `init`。
    fn is_cleaned(&self) -> bool {
        matches!(self.state, StoreState::Bytes) && self.cleanup_completed && self.arena_bytes == 0
    }

    /// 不变量 7 的访问侧条件式：`initialized > 0` 时要求 `P + initialized*S + B <= len`（与
    /// 读/析构侧同一算式），`initialized == 0` 时只要求 `B <= len`——不得用无条件式禁止首次插入，
    /// 也不得在 arena 视图访问时只核 arena 自身范围。
    fn overlap_free(&self) -> bool {
        if self.initialized > 0 {
            self.prefix_fits()
        } else {
            self.arena_bytes <= self.scratch_len
        }
    }

    /// 不变量 7 的读/析构侧条件式检查：`initialized > 0` 时要求 `P + initialized*S + B <= len`。
    fn prefix_fits(&self) -> bool {
        self.initialized
            .checked_mul(Self::SIZE)
            .and_then(|bytes| bytes.checked_add(self.base_offset))
            .and_then(|end| end.checked_add(self.arena_bytes))
            .is_some_and(|end| end <= self.scratch_len)
    }

    /// 不变量 7 的写入侧条件式检查：下一槽写入后要求 `P + (index + 1)*S + B <= len`。
    fn slot_fits(&self, index: usize) -> bool {
        index
            .checked_add(1)
            .and_then(|count| count.checked_mul(Self::SIZE))
            .and_then(|bytes| bytes.checked_add(self.base_offset))
            .and_then(|end| end.checked_add(self.arena_bytes))
            .is_some_and(|end| end <= self.scratch_len)
    }

    /// 已对齐的槽地址 `base + P + index*S`（不变量 1）：`i*S` 与 `P + i*S` 均用 checked 运算，
    /// 地址由承载指针按 checked 偏移派生——不经 `usize` 往返，provenance 随 `add`/`cast` 保留。
    ///
    /// 返回 `*mut MaybeUninit<Entry>`：写入点直接按 `MaybeUninit` 写，读取点才 `cast::<Entry>()`。
    ///
    /// # Safety
    /// 调用方必须保证 `index <= ever_typed` 且 `P + (index + 1)*S <= len`——即该槽落在由
    /// `new` 唯一取得的借用之内（不变量 1、7、9）。
    unsafe fn slot_ptr(&mut self, index: usize) -> *mut MaybeUninit<Entry> {
        debug_assert!(index <= self.ever_typed);
        let offset = index
            .checked_mul(Self::SIZE)
            .and_then(|bytes| bytes.checked_add(self.base_offset))
            .expect("条目槽偏移溢出：容量检查应已排除");
        debug_assert!(
            offset
                .checked_add(Self::SIZE)
                .is_some_and(|end| end <= self.scratch_len),
            "条目槽必须落在 scratch 内（不变量 7）"
        );
        let base = self.scratch.as_ptr();
        // SAFETY: `offset + S <= len` 由调用方保证、`offset` 已经 checked 运算（不变量 7）；
        // 指针由该独占借用的承载指针 `add` 偏移派生，provenance 不丢失、不跨切片也不跨调用
        // 保留（不变量 1、9）。`MaybeUninit<Entry>` 与 `Entry` 布局相同，`cast` 不改地址。
        let slot = unsafe { base.cast::<u8>().add(offset) }.cast::<MaybeUninit<Entry>>();
        debug_assert_eq!(
            slot as usize % Self::ALIGN,
            0,
            "条目槽地址必须按 Entry 对齐（不变量 1）"
        );
        slot
    }

    /// 已对齐槽地址的只读版本（`entries` 用）：地址派生与 [`EntryStore::slot_ptr`] 相同。
    ///
    /// # Safety
    /// 调用方必须保证 `index < initialized` 且该前缀每个元素都已由 `init` 写入（不变量 1、2、7）。
    unsafe fn slot_ptr_shared(&self, index: usize) -> *const MaybeUninit<Entry> {
        debug_assert!(index < self.initialized);
        let offset = index
            .checked_mul(Self::SIZE)
            .and_then(|bytes| bytes.checked_add(self.base_offset))
            .expect("条目槽偏移溢出：容量检查应已排除");
        debug_assert!(
            offset
                .checked_add(Self::SIZE)
                .is_some_and(|end| end <= self.scratch_len),
            "条目槽必须落在 scratch 内（不变量 7）"
        );
        let base = self.scratch.as_ptr();
        // SAFETY: 偏移 checked、范围由调用方保证；指针由唯一借用的承载指针派生，provenance
        // 不丢失；`MaybeUninit<Entry>` 与 `Entry` 布局相同，`cast` 不改地址（不变量 1、9）。
        unsafe { base.cast::<u8>().add(offset) }.cast::<MaybeUninit<Entry>>()
    }

    /// 把 `entry` 按所有权移入下一槽 `i == initialized`（不变量 2、3）。
    ///
    /// # Safety
    /// 调用方必须已完成不变量 7 的写入侧检查（`P + N*S + B <= len`，`N = initialized + 1`），
    /// 并保证 `index == initialized`、该槽尚未初始化。
    unsafe fn init_at(&mut self, index: usize, entry: Entry) {
        debug_assert_eq!(
            index, self.initialized,
            "只写下一槽 i == initialized（不变量 2）"
        );
        debug_assert!(
            self.slot_fits(index),
            "写入前必须核 P + (i+1)*S + B <= len（不变量 7）"
        );
        // SAFETY: 容量检查已通过 ⇒ 该槽落在唯一借用内并已对齐（不变量 1、7、9）；承载是
        // `MaybeUninit<Entry>`，`write` 以 `MaybeUninit::new` 把 `entry` 按所有权移入、不读取
        // 该槽原有字节（不变量 2、3）。
        unsafe { self.slot_ptr(index).write(MaybeUninit::new(entry)) };
    }

    /// 逐项析构 `[0, count)`（不变量 3、4）。
    ///
    /// # Safety
    /// `count` 必须是进入清理前的 `initialized` 快照，且该前缀每个元素都已由 `init` 写入。
    unsafe fn drop_prefix_raw(&mut self, count: usize) {
        debug_assert!(count <= self.ever_typed);
        for index in 0..count {
            // SAFETY: `index < count <= ever_typed`，该槽已由 `init` 写入、按 `Entry` 对齐且在
            // 唯一借用内（不变量 1、2、7、9），故把承载 `cast::<Entry>()` 得到已初始化值；
            // `drop_in_place` 按所有权移出、之后该槽退回未初始化、不再当作有效值（不变量 3）；
            // `Guard` 只含 `Weak`，析构不执行用户代码（不变量 8）。
            unsafe { ptr::drop_in_place(self.slot_ptr(index).cast::<Entry>()) };
        }
    }

    /// 把曾被类型化的整段 `[P, P + count*S)` 用裸指针写零（不变量 5）。
    ///
    /// # Safety
    /// `count` 必须是 `ever_typed` 的当前值，且 `P + count*S <= len`；调用时状态必须是
    /// `Cleaning` 且条目已全部析构（`initialized == 0`）。
    unsafe fn reinit_typed_raw(&mut self, count: usize) {
        debug_assert_eq!(count, self.ever_typed);
        let bytes = count
            .checked_mul(Self::SIZE)
            .expect("恢复区间字节数溢出：容量检查应已排除");
        let end = self
            .base_offset
            .checked_add(bytes)
            .expect("恢复区间偏移溢出：容量检查应已排除");
        debug_assert!(
            end <= self.scratch_len,
            "恢复区间必须落在 scratch 内（不变量 7）"
        );
        let base = self.scratch.as_ptr().cast::<u8>();
        debug_assert_eq!(
            (base as usize).wrapping_add(self.base_offset) % Self::ALIGN,
            0,
            "曾类型化区间必须按 Entry 对齐（不变量 1）"
        );
        // SAFETY: `[P, P + count*S)` 落在唯一借用之内并已按 `Entry` 对齐（不变量 1、7、9）；
        // 只用承载指针 `write_bytes` 写零、**不构造 `&mut [u8]` 字节切片**（该区可能含未初始化
        // padding，不变量 5），写零后该段重新成为有效字节；`write_bytes` 不 panic、
        // 不执行用户代码（不变量 8）。
        unsafe { ptr::write_bytes(base.add(self.base_offset), 0, bytes) };
    }

    /// `[0, initialized)` 的类型化视图（不变量 2、6）。
    ///
    /// # Safety
    /// 调用方必须保证 `initialized > 0`、该前缀每个元素都已由 `init` 写入、当前不是
    /// `Cleaning`；返回视图不越过该块 scratch 的独占借用（由调用帧持有），且不得与 arena 视图同时存在（不变量 6）。
    unsafe fn entries_mut_raw(&mut self) -> &mut [Entry] {
        debug_assert!(self.initialized > 0);
        let count = self.initialized;
        // SAFETY: 前缀 `[0, count)` 每个槽都已由 `init` 写入、按 `Entry` 对齐且在唯一借用内
        // （不变量 1、2、7、9）；只有该已初始化前缀被 `cast::<Entry>()`，未初始化槽不视为
        // `Entry`（不变量 2）；视图由 `&mut self` 独占借出、不越过该块 scratch 的独占借用、且不与 arena
        // 视图共存（不变量 6、9）。
        unsafe { core::slice::from_raw_parts_mut(self.slot_ptr(0).cast::<Entry>(), count) }
    }
}

impl Drop for EntryStore {
    /// 不变量 8：`Drop` 不得 panic、不执行用户代码；`Ok`/`Err`/unwind 共用同一顺序，
    /// 且 `finish` 之后重复调用按 [`EntryStore::is_cleaned`] 幂等。
    fn drop(&mut self) {
        self.terminate();
    }
}

/// R23 未提交试探预留的撤销点（架构 §5「R23 写入接缝」③）：由 [`EntryStore::arena_mark`] 取得、
/// [`EntryStore::arena_revert`] 消费。两个字段都不对外发布，只在 crate 内表达 `B_pre`。
#[derive(Debug, Clone, Copy)]
pub(super) struct ArenaMark {
    bytes: usize,
    payload: usize,
}

/// 帧内能力视图：`with_store` 交给调用方的**唯一**形态（架构 §2.1.4「模块边界」、§2.1.5）。
///
/// 交出的类型是 `dyn`（**unsized**）而不是 `&mut EntryStore`：`mem::swap`/`mem::replace` 与按值移动
/// 都要求 `Sized`，所以父模块既不能把两次嵌套 `with_store` 帧的**所有者内容**换掉（内层帧结束后
/// 外层仍用已结束借用/已释放后备区），也不能把内层帧的能力视图搬进外层帧的 `Encoder` 字段
/// （后者另由 [`with_store`] 的 `for<'cap>` 高阶约束拦住）。所有者的构造、销毁与终止清理仍只在
/// [`with_store`] 帧内完成，能力方法只读写该帧的 arena/条目区。
///
/// 能力集就是 `Encoder` 在 scratch 承载路径上需要的全部：条目计数、arena 分配与视图、条目读写；
/// **不含** `terminate`/`drop_prefix`/`reinit_typed_range` 等清理入口。
pub(super) trait StoreCap {
    fn entries_len(&self) -> usize;
    fn arena_alloc_zeroed(&mut self, len: usize, align: usize) -> Result<usize, ArenaFull>;
    /// 试探性分配：失败时不写 `shortfall` 诊断（R23 试准备用）。
    fn try_arena_alloc_zeroed(&mut self, len: usize, align: usize) -> Option<usize>;
    /// R23 未提交试探预留的撤销点 `B_pre`（只读快照）。
    fn arena_mark(&self) -> ArenaMark;
    /// 撤销未提交的试探预留：把 `B` 恢复到 `mark`（已提交占用不经此处回缩）。
    fn arena_revert(&mut self, mark: ArenaMark);
    /// 可失败的范围 + 不交叠判定，供 R23 取段与提交前校验用（不新增 `unsafe`）。
    fn arena_range_ok(&self, offset: usize, len: usize) -> bool;
    /// 只读落位预检（R23 试准备用）：按顺序的 0–2 次分配能否都落位，不改变 arena 占用。
    fn arena_fits_in_order(
        &self,
        first: Option<(usize, usize)>,
        second: Option<(usize, usize)>,
    ) -> bool;
    fn arena_bytes(&self, offset: usize, len: usize) -> &[u8];
    fn arena_bytes_mut(&mut self, offset: usize, len: usize) -> &mut [u8];
    fn arena_copy_within(&mut self, src: usize, dst: usize, len: usize);
    fn init(&mut self, entry: Entry) -> Result<usize, InitError>;
    fn entries(&self) -> &[Entry];
    fn entries_mut(&mut self) -> &mut [Entry];
    /// 首个真实 `scratch` 不足的诊断（需求 R29）：只读诊断，供容量失败→既有 `1` 的锁存点把
    /// 该失败点的下界绑到本编码器的归因上（能力集里唯一不为 arena/条目区服务的一项）。
    fn shortfall(&self) -> Option<ScratchShortfall>;
}

impl StoreCap for EntryStore {
    fn entries_len(&self) -> usize {
        EntryStore::entries_len(self)
    }
    fn arena_alloc_zeroed(&mut self, len: usize, align: usize) -> Result<usize, ArenaFull> {
        EntryStore::arena_alloc_zeroed(self, len, align)
    }
    fn try_arena_alloc_zeroed(&mut self, len: usize, align: usize) -> Option<usize> {
        EntryStore::try_arena_alloc_zeroed(self, len, align)
    }
    fn arena_mark(&self) -> ArenaMark {
        EntryStore::arena_mark(self)
    }
    fn arena_revert(&mut self, mark: ArenaMark) {
        EntryStore::arena_revert(self, mark)
    }
    fn arena_range_ok(&self, offset: usize, len: usize) -> bool {
        EntryStore::arena_range_ok(self, offset, len)
    }
    fn arena_fits_in_order(
        &self,
        first: Option<(usize, usize)>,
        second: Option<(usize, usize)>,
    ) -> bool {
        EntryStore::arena_fits_in_order(self, first, second)
    }
    fn arena_bytes(&self, offset: usize, len: usize) -> &[u8] {
        EntryStore::arena_bytes(self, offset, len)
    }
    fn arena_bytes_mut(&mut self, offset: usize, len: usize) -> &mut [u8] {
        EntryStore::arena_bytes_mut(self, offset, len)
    }
    fn arena_copy_within(&mut self, src: usize, dst: usize, len: usize) {
        EntryStore::arena_copy_within(self, src, dst, len)
    }
    fn init(&mut self, entry: Entry) -> Result<usize, InitError> {
        EntryStore::init(self, entry)
    }
    fn entries(&self) -> &[Entry] {
        EntryStore::entries(self)
    }
    fn entries_mut(&mut self) -> &mut [Entry] {
        EntryStore::entries_mut(self)
    }
    fn shortfall(&self) -> Option<ScratchShortfall> {
        self.shortfall
    }
}

/// `with_store` 帧内交出的能力视图类型。`+ 'static` 是必须写的：`&'a mut dyn Trait` 的默认对象寿命
/// 界是 `'a`，那会把 `'a` 放进 `&mut` 的不变位置；写成 `dyn StoreCap + 'static` 后
/// `Encoder<'a>` 对 `'a` 保持协变（F-V，集成用例 `encoder_lifetime_stays_covariant`）。
pub(super) type StoreCapView = dyn StoreCap + 'static;

/// 帧内受控作用域：`EntryStore` 的构造与终止清理都留在本模块，调用方只借能力。
///
/// 退出时不区分 `Ok`/`Err`：能力借用先结束，再显式走一次 [`EntryStore::terminate`] 完成
/// 终止清理（与 `Drop` 共用同一入口，重复调用按三态机幂等）。调用方因此既拿不到拥有值，
/// 也无法靠 `mem::forget` 跳过清理（不变量 4、架构 §2.1.4/§2.1.5）。
pub(super) fn with_store<R>(
    scratch: &mut [u8],
    f: impl for<'cap> FnOnce(&'cap mut StoreCapView) -> R,
) -> R {
    let mut store = EntryStore::new(scratch);
    let result = f(&mut store);
    store.terminate();
    result
}

/// 需求 R27 的 `S`（crate 内只读访问点）：由类型求值、不写死，也不得用字段字节和冒充。
/// 公开入口是 crate 根的 [`crate::identity_entry_size`]；本模块不导出 `Entry`/`EntryStore`。
pub(crate) const fn identity_entry_size() -> usize {
    EntryStore::SIZE
}

/// 需求 R27 的 `A`（crate 内只读访问点）：非零 2 的幂。
/// 公开入口是 crate 根的 [`crate::identity_entry_align`]。
pub(crate) const fn identity_entry_align() -> usize {
    EntryStore::ALIGN
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{rc::Rc, vec, vec::Vec};
    use core::mem::{align_of, size_of};
    extern crate std;

    const SIZE: usize = size_of::<Entry>();
    const ALIGN: usize = align_of::<Entry>();

    fn entry_with_target(id: u32) -> (Entry, Rc<u32>) {
        let target: Rc<u32> = Rc::new(id);
        let weak: RcWeak<u32> = Rc::downgrade(&target);
        let guard: RcWeak<dyn Any> = weak;
        let entry = Entry {
            family: ReferenceFamily::Rc,
            address: Rc::as_ptr(&target) as usize,
            target: TypeId::of::<u32>(),
            id,
            state: EntryState::Reserved,
            guard: Guard::Rc(guard),
        };
        (entry, target)
    }

    fn entry(id: u32) -> Entry {
        entry_with_target(id).0
    }

    /// 从 `backing` 里切出长度恰为 `len` 的窗口：`extra == 0` 取自然对齐起点（`P == 0`），
    /// `extra == 1` 让起点后错 1 字节（`P == ALIGN - 1`，使 `P > 0`）。
    fn window<'b>(backing: &'b mut [u8], extra: usize, len: usize) -> &'b mut [u8] {
        let lead = (ALIGN - backing.as_ptr() as usize % ALIGN) % ALIGN;
        let start = lead + extra;
        &mut backing[start..start + len]
    }

    fn prefix_of(bytes: &[u8]) -> usize {
        (ALIGN - bytes.as_ptr() as usize % ALIGN) % ALIGN
    }

    /// 需求 R27 的 Rust 侧发布：crate 根的两个公开接口必须与 `Entry` 的类型属性一致，
    /// 且满足 `S ≥ 1`、`A` 为非零 2 的幂、`S` 是 `A` 的整数倍（架构 §2.1.3）。
    #[test]
    fn published_identity_entry_size_and_align_match_entry_layout() {
        let size = crate::identity_entry_size();
        let align = crate::identity_entry_align();
        assert_eq!(size, size_of::<Entry>());
        assert_eq!(align, align_of::<Entry>());
        assert!(size > 0, "S = size_of::<Entry>() ≥ 1");
        assert!(align.is_power_of_two(), "A 必须是非零 2 的幂");
        assert_eq!(size % align, 0, "S 必须是 A 的整数倍");
    }

    #[test]
    fn entry_capacity_follows_len_prefix_and_arena_occupancy() {
        assert!(ALIGN > 1);

        // (a) len == 0：无字节 ⇒ 容量 0；init 只能报「已判定的身份条目不足」。
        let mut empty: Vec<u8> = Vec::new();
        let mut store = EntryStore::new(&mut empty);
        assert_eq!(store.entry_capacity(), 0);
        assert_eq!(store.init(entry(1)), Err(InitError::InsufficientEntries));
        drop(store);

        // (b) len < P（刻意错位起点）：仍是空片，容量 0、不形成槽指针。
        let mut backing = vec![0u8; 64];
        let mut store = EntryStore::new(window(&mut backing, 1, 1));
        assert!(store.scratch_len < store.base_offset);
        assert_eq!(store.entry_capacity(), 0);
        assert_eq!(store.init(entry(1)), Err(InitError::InsufficientEntries));
        drop(store);

        // (c) 恰好一槽：len - P == SIZE；`initialized == 0` / `ever_typed == 0` 不是空片。
        let mut backing = vec![0u8; 64 + SIZE];
        let expected = prefix_of(window(&mut backing, 0, SIZE));
        assert_eq!(expected, 0);
        let mut store = EntryStore::new(window(&mut backing, 0, SIZE));
        assert_eq!(store.base_offset, expected);
        assert_eq!(store.ever_typed, 0);
        assert_eq!(store.entry_capacity(), 1);
        assert_eq!(store.init(entry(1)), Ok(0));
        assert_eq!(store.ever_typed, 1);
        // N = 2 > C(0) = 1 ⇒ 已判定的身份条目不足。
        assert_eq!(store.init(entry(2)), Err(InitError::InsufficientEntries));
        drop(store);

        // (c') 同长度、错位起址：仍是 len == SIZE，但起点后错 1 字节使 P == ALIGN - 1
        // ⇒ len - P < SIZE，可用容量变 0——容量按切片起址计，不只看长度（plan WP-E15）。
        let mut backing = vec![0u8; 64 + SIZE];
        let expected = prefix_of(window(&mut backing, 1, SIZE));
        assert_eq!(expected, ALIGN - 1);
        let mut store = EntryStore::new(window(&mut backing, 1, SIZE));
        assert_eq!(store.base_offset, expected);
        assert_eq!(store.entry_capacity(), 0);
        assert_eq!(store.init(entry(1)), Err(InitError::InsufficientEntries));
        drop(store);

        // (d) 多槽：错位起点使 P == ALIGN - 1，窗口长度取 3 * SIZE + 4 + P ⇒ len - P == 3 * SIZE + 4
        // ⇒ C(0) == 3；只有 arena 占用才减少条目容量。
        let window_len = 3 * SIZE + 4 + ALIGN - 1;
        let mut backing = vec![0u8; 64 + window_len];
        let expected = prefix_of(window(&mut backing, 1, window_len));
        assert_eq!(expected, ALIGN - 1);
        let mut store = EntryStore::new(window(&mut backing, 1, window_len));
        assert_eq!(store.base_offset, expected);
        assert_eq!(store.entry_capacity(), 3);
        store.arena_bytes = SIZE;
        assert_eq!(store.entry_capacity(), 2);
        assert_eq!(store.init(entry(1)), Ok(0));
        assert_eq!(store.init(entry(2)), Ok(1));
        // N = 3 > C(B) = 2，但 N <= C(0) = 3 ⇒ 只是与当前 arena 占用相撞（映射待裁决）。
        assert_eq!(store.init(entry(3)), Err(InitError::ArenaCollision));
        store.arena_bytes = 0;
        assert_eq!(store.init(entry(3)), Ok(2));
    }

    /// F2 修订（第二槽的私有分类）：`len = P + S` 恰一槽时，第二个 `init` 的失败是**身份槽不足**，
    /// 不是 arena 先耗尽。
    ///
    /// 本用例与集成层各自独立、不在同一长度上观测同一件事：集成负例
    /// `insufficient_scratch_with_exact_dst_returns_invalid_argument`
    /// （`tests/encode_into_scratch.rs`）只固定总长下界与既有 `1`，而它的样本在 `L - S` 上实际失败于
    /// arena 分配；第二槽的分类只有本模块观察得到，故由本用例单独固定（`P` 按真实起址算：窗口起点
    /// 后错 1 字节 ⇒ `P == A - 1`）：
    /// (a) `C(0) == 1` 时第二个槽被拒为 [`InitError::InsufficientEntries`]，不是
    ///     [`InitError::ArenaCollision`]（后者要求 `N <= C(0)`）；
    /// (b) 先在**同一** store 上让第二个槽被拒（状态不变），再在**同一** store 上真实分配不足一槽的
    ///     arena 载荷并成功——失败**当时** arena 仍有余量，限制来自身份槽数，不来自 arena 用量。
    #[test]
    fn second_init_short_one_slot_is_insufficient_entries_with_arena_still_free() {
        let size = crate::identity_entry_size();
        let align = crate::identity_entry_align();

        // (a) len - P == S ⇒ C(0) == 1：首槽可写，第二槽的 `N = 2 > C(0)` 只能报身份条目不足。
        let window_len = size + align - 1;
        let mut backing = vec![0u8; 64 + window_len];
        let mut store = EntryStore::new(window(&mut backing, 1, window_len));
        let p = store.base_offset;
        assert_eq!(p, align - 1, "窗口起点后错 1 字节 ⇒ P == A - 1（非零）");
        assert_eq!(window_len - p, size, "len = P + S（按真实起址算 P）");
        assert_eq!(store.entry_capacity(), 1, "len - P == S ⇒ C(0) == 1");
        assert_eq!(store.init(entry(1)), Ok(0), "首槽必须成功");
        assert_eq!(store.arena_bytes, 0, "本用例尚未分配任何 arena 字节：B == 0");
        assert_eq!(
            store.init(entry(2)),
            Err(InitError::InsufficientEntries),
            "N = 2 > C(0) = 1 ⇒ 身份条目不足（不是 ArenaCollision：N > C(0)）"
        );
        assert_eq!(
            (store.initialized, store.ever_typed),
            (1, 1),
            "失败不落槽、不改计数"
        );
        drop(store);

        // (b) 同一起址规则下多出不足一槽的 arena 余量（slack < S ⇒ C(0) 仍为 1）：先让第二个槽
        // 在同一 store 上被拒，再在同一 store 上真实分配该载荷——证明失败**当时** arena 仍有余量。
        let slack = size - 1;
        let window_len = size + align - 1 + slack;
        let mut backing = vec![0u8; 64 + window_len];
        let mut store = EntryStore::new(window(&mut backing, 1, window_len));
        let p = store.base_offset;
        assert_eq!(p, align - 1);
        assert_eq!(store.entry_capacity(), 1, "余量不足一槽 ⇒ C(0) 仍为 1");
        assert_eq!(store.init(entry(3)), Ok(0), "首槽必须成功");
        assert_eq!(
            store.arena_bytes, 0,
            "失败前尚未分配任何 arena 字节：B == 0"
        );
        assert_eq!(
            store.init(entry(4)),
            Err(InitError::InsufficientEntries),
            "N = 2 > C(0) = 1 ⇒ 身份条目不足（不是 ArenaCollision：N > C(0)）"
        );
        assert_eq!(
            (store.initialized, store.ever_typed),
            (1, 1),
            "失败不落槽、不改计数"
        );
        // 失败**之后**的同一 store：arena 仍放得下不足一槽的载荷并真实分配成功。
        assert!(
            store.arena_fits_in_order(Some((slack, 1)), None),
            "与 arena_alloc_zeroed 共用 arena_span_fit 的落位判据仍放得下该载荷"
        );
        let offset = store
            .arena_alloc_zeroed(slack, 1)
            .expect("失败当时 arena 仍能真实分配该载荷（arena 没先耗尽）");
        assert_eq!(offset, p + size, "arena 自高地址向下，紧接条目区之后");
        assert_eq!(store.entry_capacity(), 1, "B == slack < S ⇒ C(B) 仍为 1");
        assert_eq!((store.initialized, store.ever_typed), (1, 1));
        store.terminate();
        assert!(store.is_cleaned());
    }

    #[test]
    fn first_terminate_from_active_bytes_enters_cleaning_and_zeroes_arena_only_there() {
        let mut backing = vec![0u8; 64 + 4 * SIZE];
        let mut store = EntryStore::new(window(&mut backing, 0, 3 * SIZE + 4));
        // 活跃 Bytes：从未类型化（ever_typed == 0），但 arena 已占 5B。
        store.arena_bytes = 5;
        assert_eq!(store.state, StoreState::Bytes);
        assert_eq!(store.initialized, 0);
        assert!(!store.is_cleaned());

        store.drop_prefix();
        assert_eq!(store.state, StoreState::Cleaning);
        // `B` 只在 `Cleaning` 内归零，`drop_prefix` 不动它。
        assert_eq!(store.arena_bytes, 5);

        // (d) ever_typed == 0 ⇒ 求址前即为空步；(e) 在同一 `Cleaning` 内归零 `B`；(f) 转 `Bytes`。
        store.reinit_typed_range();
        assert_eq!(store.state, StoreState::Bytes);
        assert_eq!(store.arena_bytes, 0);
        assert!(store.is_cleaned());
    }

    /// arena 用户裁决版（2026-11-18，暂定）：单调分配、不回收、容量＝剩余字节。
    /// 断言对齐、整体写零、旧区不被覆盖、条目容量随之下降、不足时返回 `ArenaFull`。
    #[test]
    fn arena_allocations_are_monotonic_aligned_and_exhaustible() {
        let mut backing = vec![0xFFu8; 128 + 4 * SIZE + 64];
        let len = 4 * SIZE + 64;
        let mut store = EntryStore::new(window(&mut backing, 0, len));
        let base = store.scratch.as_ptr() as usize;
        assert_eq!(store.init(entry(1)), Ok(0));
        let before = store.entry_capacity();
        assert!(before >= 4, "空 arena 的条目容量由 len 与 P 决定");

        let first = store.arena_alloc_zeroed(8, 8).expect("首次分配");
        assert_eq!(
            (base + first) % ALIGN.max(8),
            0,
            "分配起点必须按请求对齐"
        );
        assert!(store.arena_bytes(first, 8).iter().all(|&b| b == 0), "新区域必须整体写零");
        let second = store.arena_alloc_zeroed(3, 1).expect("第二次分配");
        assert!(second < first, "单调向下分配：后一块在更低地址");
        // 对齐填充与本次分配一起写零，旧区保持内容。
        assert!(store.arena_bytes(second, first + 8 - second).iter().all(|&b| b == 0));
        store.arena_bytes_mut(first, 8).copy_from_slice(b"marker42");
        // 条件式不交叠：占用后仍可写身份槽，且旧区不被覆盖。
        assert_eq!(store.init(entry(2)), Ok(1));
        let third = store.arena_alloc_zeroed(16, 4).expect("第三次分配");
        assert!(third < second);
        assert_eq!(store.arena_bytes(first, 8), b"marker42", "不回收 ⇒ 旧区不被改写");
        store
            .arena_alloc_zeroed(SIZE, ALIGN)
            .expect("第四次分配（一个条目槽大小）");
        assert!(
            store.entry_capacity() < before,
            "arena 占用必须减少可用条目数（C(B) = (len - P - B) / S）"
        );
        // 剩余字节不足时返回 `ArenaFull`（调用方映射既有 `1`）。
        assert_eq!(store.arena_alloc_zeroed(len, 1), Err(ArenaFull));
        store.terminate();
        assert!(store.is_cleaned(), "终止清理仍按三态机完成");
    }

    /// I3①：活跃 `Bytes` 且 `B == 0` 与「已完成清理」字面相同，但首次终止退出必须真的
    /// 进入 `Cleaning`，不得因 `B == 0` / `n == 0` 直接空操作返回（架构 §2.1.4 顺序条 (b)(e)）。
    #[test]
    fn active_bytes_with_empty_arena_enters_cleaning_on_first_terminate() {
        let mut backing = vec![0xAAu8; 64 + 2 * SIZE];
        let mut store = EntryStore::new(window(&mut backing, 0, 2 * SIZE));
        assert_eq!(store.state, StoreState::Bytes);
        assert_eq!(store.initialized, 0);
        assert_eq!(store.ever_typed, 0);
        assert_eq!(store.arena_bytes, 0);
        assert!(!store.is_cleaned());

        store.drop_prefix();
        assert_eq!(
            store.state,
            StoreState::Cleaning,
            "零槽零 arena 也不得跳过 (b)"
        );
        assert!(!store.is_cleaned());

        store.reinit_typed_range();
        assert_eq!(store.state, StoreState::Bytes);
        assert!(store.is_cleaned());
    }

    #[test]
    fn drop_prefix_enters_cleaning_then_restores_the_typed_span() {
        let mut backing = vec![0xFFu8; 64 + 4 * SIZE];
        let lead = (ALIGN - backing.as_ptr() as usize % ALIGN) % ALIGN;
        let start = lead + 1;
        let len = 3 * SIZE + 8;
        let p = {
            let mut store = EntryStore::new(&mut backing[start..start + len]);
            let p = store.base_offset;
            assert!(p > 0);
            assert_eq!(store.init(entry(1)), Ok(0));
            assert_eq!(store.init(entry(2)), Ok(1));
            assert_eq!(store.state, StoreState::Typed);
            assert_eq!(store.ever_typed, 2);

            // `entries_mut` 只借出已初始化前缀（不变量 2、6）：写入的条目按槽索引可读。
            {
                let entries = store.entries_mut();
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].id, 1);
                assert_eq!(entries[1].id, 2);
                assert_eq!(entries[0].family, ReferenceFamily::Rc);
                assert_eq!(entries[0].state, EntryState::Reserved);
                assert_eq!(entries[0].target, TypeId::of::<u32>());
                assert_ne!(entries[0].address, 0);
                assert!(matches!(&entries[0].guard, Guard::Rc(_)));
            }

            store.drop_prefix();
            // 借出视图的放行只看状态：进入 `Cleaning` 后条目区已不可当字节读。
            assert_eq!(store.state, StoreState::Cleaning);
            assert_eq!(store.initialized, 0);
            // 高水位只增不减：`(c)` 之后 `initialized` 已为 0，恢复范围仍只能由 `ever_typed` 决定。
            assert_eq!(store.ever_typed, 2);

            store.reinit_typed_range();
            assert_eq!(store.state, StoreState::Bytes);
            assert!(store.is_cleaned());
            p
        };
        // 借用结束、拥有值销毁后实读调用方原缓冲：曾类型化整段已写零，段外保持原值。
        let bytes = &backing[start..start + len];
        let typed_span = p..p + 2 * SIZE;
        assert!(bytes[typed_span.clone()].iter().all(|&b| b == 0));
        assert!(bytes[..p].iter().all(|&b| b == 0xFF));
        assert!(bytes[typed_span.end..].iter().all(|&b| b == 0xFF));
    }

    /// I3②：入槽后 `Drop` 必须真的释放 `Weak` 并把曾类型化整段恢复为有效字节——用
    /// `Rc::weak_count` 与借用结束后的原缓冲实读双向验证（清理断言不能只看 `strong_count`）。
    #[test]
    fn terminate_after_init_releases_weak_guards_and_restores_the_typed_span() {
        let mut backing = vec![0xFFu8; 64 + 3 * SIZE];
        let lead = (ALIGN - backing.as_ptr() as usize % ALIGN) % ALIGN;
        let start = lead + 1;
        let len = 2 * SIZE + 4;
        let (target, p) = {
            let mut store = EntryStore::new(&mut backing[start..start + len]);
            let p = store.base_offset;
            assert!(p > 0);
            let (first, target) = entry_with_target(7);
            assert_eq!(store.init(first), Ok(0));
            assert_eq!(Rc::weak_count(&target), 1, "入槽后 guard 仍持有一个 Weak");
            store.terminate();
            assert!(store.is_cleaned());
            (target, p)
        };
        assert_eq!(Rc::weak_count(&target), 0, "终止清理必须释放 Weak guard");
        let bytes = &backing[start..start + len];
        assert!(
            bytes[p..p + SIZE].iter().all(|&b| b == 0),
            "曾类型化整段必须恢复为有效字节（全零）"
        );
        assert!(bytes[..p].iter().all(|&b| b == 0xFF));
        assert!(bytes[p + SIZE..].iter().all(|&b| b == 0xFF));
    }

    /// I3：首次由 `Drop` 触发清理——入槽后**不**显式调用清理，唯一的清理入口 `Drop` 必须自己
    /// 走完整个终止序列。`Rc::weak_count` 只增不减地暴露漏析构（少做 (a)–(c)），借用结束后的原
    /// 缓冲实读暴露漏恢复（少做 (d)–(f)），两者合起来排除「只做半步」。
    ///
    /// `Drop` 消费拥有值，故「已完成清理」只能以 (d)–(f) 的效果断言：`reinit_typed_range` 是唯一
    /// 恢复曾类型化整段的地方，也是唯一置 `cleanup_completed` 与把 `B` 归零的地方；同时以
    /// 触发前的 `Typed`/`!is_cleaned()` 现场固定「这是首次触发」。
    #[test]
    fn drop_cleans_a_typed_store_on_the_first_trigger() {
        let mut backing = vec![0xFFu8; 64 + 3 * SIZE];
        let lead = (ALIGN - backing.as_ptr() as usize % ALIGN) % ALIGN;
        let start = lead + 1;
        let len = 2 * SIZE + 4;
        let mut store = EntryStore::new(&mut backing[start..start + len]);
        let p = store.base_offset;
        assert!(p > 0);
        let (first, target) = entry_with_target(11);
        assert_eq!(store.init(first), Ok(0));
        // 触发前的现场：活跃 `Typed`、有活槽，且尚未完成清理。
        assert_eq!(store.state, StoreState::Typed);
        assert_eq!(store.initialized, 1);
        assert!(!store.is_cleaned());
        assert_eq!(Rc::weak_count(&target), 1, "入槽后 guard 仍持有一个 Weak");

        // 不调用 `terminate`：清理只能由 `Drop` 首次触发。
        drop(store);

        assert_eq!(
            Rc::weak_count(&target),
            0,
            "Drop 必须先析构前缀、释放 Weak guard"
        );
        let bytes = &backing[start..start + len];
        assert!(
            bytes[p..p + SIZE].iter().all(|&b| b == 0),
            "Drop 必须把曾类型化整段恢复为有效字节（清理完成）"
        );
        assert!(bytes[..p].iter().all(|&b| b == 0xFF));
        assert!(bytes[p + SIZE..].iter().all(|&b| b == 0xFF));
    }

    /// I3③：B1 回归——完成清理后 `init` 必须在 **release 同样被拒绝**，且拒绝发生在写槽
    /// 之前（不执行 UB：既不改计数，也不把原缓冲写脏）。
    #[test]
    fn init_after_terminate_is_rejected_before_writing_the_slot() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let mut backing = vec![0xFFu8; 64 + 2 * SIZE];
        let lead = (ALIGN - backing.as_ptr() as usize % ALIGN) % ALIGN;
        let start = lead;
        let len = 2 * SIZE;
        let mut store = EntryStore::new(&mut backing[start..start + len]);
        store.terminate();
        assert!(store.is_cleaned());

        let (candidate, target) = entry_with_target(5);
        let rejected = catch_unwind(AssertUnwindSafe(|| store.init(candidate)));
        assert!(
            rejected.is_err(),
            "完成清理后的 init 必须在 release 也被拒绝"
        );
        assert_eq!(store.initialized, 0);
        assert_eq!(store.ever_typed, 0);
        assert!(store.is_cleaned());
        // 写槽顺序回退（先写槽、后拒绝）会被这条抓到：写入槽的 guard 已随 `Entry` 移入缓冲区，
        // 没有析构路径释放它，计数就不会归零。该断言先于字节实读，失败时不读未恢复的字节。
        assert_eq!(Rc::weak_count(&target), 0, "拒绝之前不得把 guard 写进槽");
        drop(store);
        // 拒绝前未写入任何槽：整片仍是原始字节。
        assert!(backing[start..start + len].iter().all(|&b| b == 0xFF));
    }

    /// B1 的另一半：`Cleaning` 期间不得借出条目视图，release 同样拒绝（不形成任何指针）。
    #[test]
    fn views_are_rejected_while_cleaning_in_release_too() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let mut backing = vec![0u8; 64 + 2 * SIZE];
        let mut store = EntryStore::new(window(&mut backing, 0, 2 * SIZE));
        assert_eq!(store.init(entry(1)), Ok(0));
        store.drop_prefix();
        assert_eq!(store.state, StoreState::Cleaning);

        let rejected = catch_unwind(AssertUnwindSafe(|| {
            let _ = store.entries_mut();
        }));
        assert!(
            rejected.is_err(),
            "Cleaning 期间不得借出视图（release 同此）"
        );
    }

    /// I2：帧内能力借用与底层 scratch 解耦——能短借一个仍要跑 `Drop` 的帧内局部值，用完即还，
    /// 拥有值不被移动。模块外交出的形态是 `&'cap mut StoreCapView`（unsized 能力视图，见
    /// [`with_store`]）：底层 scratch 的独占借用由调用帧的 `&mut [u8]` 形参持有，
    /// `EntryStore` 只存承载指针 + 长度，这正是公开 `Encoder<'a>` 保持协变的条件。
    #[test]
    fn store_borrow_is_short_and_leaves_the_owner_in_place() {
        let mut backing = vec![0u8; 64 + 2 * SIZE];
        let mut store = EntryStore::new(window(&mut backing, 0, 2 * SIZE));
        assert_eq!(store.init(entry(1)), Ok(0));
        {
            // `'cap` = 本次帧内借用；底层 scratch 的寿命由调用帧持有。
            let cap: &mut EntryStore = &mut store;
            assert_eq!(cap.entry_capacity(), 2);
            assert_eq!(cap.init(entry(2)), Ok(1));
        }
        // 短借用结束后拥有值仍可用、仍未被移动。
        assert_eq!(store.ever_typed, 2);
        assert_eq!(store.state, StoreState::Typed);
    }

    /// B3：两个嵌套 `with_store` 帧各自持有自己的所有者与 arena。帧内交出的能力视图是 unsized 的
    /// `dyn StoreCapView`（`mem::swap`/`mem::replace` 要求 `Sized`），内层帧的视图也受 `for<'cap>`
    /// 约束不能搬进外层帧，故「交换两帧所有者」在类型层即被拒绝；本用例固定安全一侧的行为：
    /// 内层帧退出后外层能力视图仍指向自己的 scratch。
    #[test]
    fn nested_frames_keep_their_own_capability_views() {
        let mut outer_backing = vec![0xFFu8; 128 + 2 * SIZE];
        let mut inner_backing = vec![0xFFu8; 128 + 2 * SIZE];
        let outer_window = window(&mut outer_backing, 0, 2 * SIZE);
        let inner_window = window(&mut inner_backing, 0, 2 * SIZE);
        with_store(outer_window, |outer_cap| {
            let outer_off = outer_cap.arena_alloc_zeroed(8, 1).expect("外层 arena");
            outer_cap
                .arena_bytes_mut(outer_off, 8)
                .copy_from_slice(&[0x11u8; 8]);
            with_store(inner_window, |inner_cap| {
                assert_eq!(inner_cap.entries_len(), 0);
                let inner_off = inner_cap.arena_alloc_zeroed(8, 1).expect("内层 arena");
                inner_cap
                    .arena_bytes_mut(inner_off, 8)
                    .copy_from_slice(&[0x22u8; 8]);
            });
            // 内层帧已退出并清理了自己的所有者：外层视图仍看到自己的 0x11，未被换成内层的 0x22。
            assert_eq!(outer_cap.arena_bytes(outer_off, 8), &[0x11u8; 8]);
        });
        // 帧内借用结束后两块后备区都可再次按字节读写。
        inner_backing[0] = 0;
        outer_backing[0] = 0;
    }

    /// F5：arena 视图除了 arena 自身范围，还必须在访问时按不变量 7 的条件式显式核
    /// 「条目前缀与 arena 不交叠」（`initialized > 0` 时 `P + initialized*S + B <= len`）。
    #[test]
    fn arena_views_reject_prefix_overlap() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let mut backing = vec![0u8; 64 + 2 * SIZE];
        let mut store = EntryStore::new(window(&mut backing, 0, 2 * SIZE));
        assert_eq!(store.init(entry(1)), Ok(0));
        // 刻意制造不变量 7 的违反：`P + 1*S + B > len`，但区间仍在 arena 自身范围内。
        store.arena_bytes = 2 * SIZE;
        let read = catch_unwind(AssertUnwindSafe(|| {
            let _ = store.arena_bytes(SIZE, SIZE);
        }));
        let write = catch_unwind(AssertUnwindSafe(|| {
            let _ = store.arena_bytes_mut(SIZE, SIZE);
        }));
        // 断言前先恢复现场：清理路径不得被测试刻意破坏的占用计数继住。
        store.arena_bytes = 0;
        assert!(
            read.is_err(),
            "arena 视图必须在条件式不交叠检查失败时拒绝（不形成视图）"
        );
        assert!(
            write.is_err(),
            "arena 可写视图同样必须先核不变量 7（条件式）"
        );
    }

    #[test]
    fn repeated_cleanup_on_completed_bytes_is_a_noop() {
        let mut backing = vec![0u8; 64 + 2 * SIZE];
        let mut store = EntryStore::new(window(&mut backing, 0, 2 * SIZE));
        store.arena_bytes = 3;
        store.terminate();
        assert!(store.is_cleaned());
        let snapshot = (
            store.state,
            store.initialized,
            store.ever_typed,
            store.arena_bytes,
        );
        store.terminate();
        assert_eq!(
            (
                store.state,
                store.initialized,
                store.ever_typed,
                store.arena_bytes
            ),
            snapshot
        );

        // 新建且从未类型化的 `Bytes` 与「已完成清理」字面相同（`ever_typed == 0`、`B == 0`），
        // 但仍可 `init`，不得据此判为已完成。
        let mut fresh_backing = vec![0u8; 64 + 2 * SIZE];
        let fresh = EntryStore::new(window(&mut fresh_backing, 0, 2 * SIZE));
        assert_eq!(fresh.ever_typed, 0);
        assert_eq!(fresh.arena_bytes, 0);
        assert!(!fresh.is_cleaned());
    }
}
