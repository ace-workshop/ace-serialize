use super::Decoder;
use crate::{
    error::{Error, Result},
    schema::{AceDecode, AceDecodeCyclic, AceElement, ElementKind, NonOptional},
};
#[cfg(feature = "std")]
use alloc::sync::{Arc, Weak as SyncWeak};
use alloc::{
    boxed::Box,
    collections::{btree_map, BTreeMap, BTreeSet},
    rc::{Rc, Weak},
    vec::Vec,
};
use core::any::Any;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Push(u32),
    Pop(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Schedule {
    events: Vec<Event>,
    peak: usize,
}

struct NodeCounts {
    discovery: usize,
    incoming_unopened: usize,
    unfinished_strong: usize,
    unopened_weak: usize,
}

struct BuildScheduler<'a> {
    strong: &'a BTreeMap<u32, Vec<u32>>,
    counts: BTreeMap<u32, NodeCounts>,
    strong_predecessors: BTreeMap<u32, Vec<u32>>,
    weak_predecessors: BTreeMap<u32, Vec<u32>>,
    sources: BTreeSet<usize>,
    open: Vec<u32>,
    schedule: Schedule,
}

impl BuildScheduler<'_> {
    fn push_node(&mut self, id: u32) {
        self.open.push(id);
        self.schedule.events.push(Event::Push(id));
        self.schedule.peak = self.schedule.peak.max(self.open.len());
        if let Some(targets) = self.strong.get(&id) {
            for target in targets {
                let counts = self
                    .counts
                    .get_mut(target)
                    .expect("validated strong target");
                counts.incoming_unopened -= 1;
                if counts.incoming_unopened == 0 {
                    self.sources.insert(counts.discovery);
                }
            }
        }
        if let Some(owners) = self.weak_predecessors.get(&id) {
            for owner in owners {
                self.counts
                    .get_mut(owner)
                    .expect("validated weak owner")
                    .unopened_weak -= 1;
            }
        }
    }

    fn pop_node(&mut self) {
        let id = self.open.pop().expect("only a ready stack top is popped");
        self.schedule.events.push(Event::Pop(id));
        if let Some(owners) = self.strong_predecessors.get(&id) {
            for owner in owners {
                self.counts
                    .get_mut(owner)
                    .expect("validated strong owner")
                    .unfinished_strong -= 1;
            }
        }
    }
}

/// ids 只含身份节点，顺序即发现序号；root=0 是无事件的虚根。
/// ids 必须恰为强可达集合，Weak 不扩展它；此处只返回峰值，不检查构造预算。
/// root=0 时允许 weak[0] 表达普通根的弱请求，目标仍须在消息内强可达集合中。
/// weak[0] 不参与身份栈顶的就绪计数；普通根在全部事件完成、目标均 Done 后装配。
/// 虚根自身无事件，也不得成为强／弱目标。
fn plan_build_schedule(
    ids: &[u32],
    strong: &BTreeMap<u32, Vec<u32>>,
    weak: &BTreeMap<u32, Vec<u32>>,
    root: u32,
) -> Result<Schedule> {
    let mut counts = BTreeMap::new();
    for (discovery, &id) in ids.iter().enumerate() {
        if id == 0
            || counts
                .insert(
                    id,
                    NodeCounts {
                        discovery,
                        incoming_unopened: 0,
                        unfinished_strong: strong.get(&id).map_or(0, Vec::len),
                        unopened_weak: weak.get(&id).map_or(0, Vec::len),
                    },
                )
                .is_some()
        {
            return Err(Error::InvalidReference);
        }
    }
    if root != 0 && !counts.contains_key(&root) {
        return Err(Error::InvalidReference);
    }
    for (&owner, targets) in strong.iter().chain(weak) {
        if (!counts.contains_key(&owner) && !(root == 0 && owner == 0))
            || targets.iter().any(|target| !counts.contains_key(target))
        {
            return Err(Error::InvalidReference);
        }
    }
    let mut reachable = BTreeSet::new();
    let mut pending = Vec::new();
    if root == 0 {
        pending.extend(strong.get(&0).into_iter().flatten().copied());
    } else {
        pending.push(root);
    }
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(strong.get(&id).into_iter().flatten().copied());
        }
    }
    if reachable.len() != ids.len() {
        return Err(Error::InvalidReference);
    }

    // 普通拓扑排序不处理 Weak/LIFO；标准有序容器配合反向计数避免反复扫描边。
    let mut strong_predecessors: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut weak_predecessors: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (&owner, targets) in strong {
        if owner == 0 {
            continue;
        }
        for &target in targets {
            counts
                .get_mut(&target)
                .expect("validated strong target")
                .incoming_unopened += 1;
            strong_predecessors.entry(target).or_default().push(owner);
        }
    }
    for (&owner, targets) in weak {
        if owner != 0 {
            for &target in targets {
                weak_predecessors.entry(target).or_default().push(owner);
            }
        }
    }
    let sources = counts
        .values()
        .filter(|counts| counts.incoming_unopened == 0)
        .map(|counts| counts.discovery)
        .collect();
    let mut scheduler = BuildScheduler {
        strong,
        counts,
        strong_predecessors,
        weak_predecessors,
        sources,
        open: Vec::new(),
        schedule: Schedule {
            events: Vec::new(),
            peak: 0,
        },
    };
    let mut unopened = ids.len();
    while unopened != 0 || !scheduler.open.is_empty() {
        // 这两个计数分别等价于全部强目标在 D、全部弱目标在 O∪D。
        if scheduler.open.last().is_some_and(|id| {
            let counts = &scheduler.counts[id];
            counts.unfinished_strong == 0 && counts.unopened_weak == 0
        }) {
            scheduler.pop_node();
        } else {
            let discovery = scheduler
                .sources
                .pop_first()
                .ok_or(Error::CycleUnsupported)?;
            scheduler.push_node(ids[discovery]);
            unopened -= 1;
        }
    }
    Ok(scheduler.schedule)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Rc,
    #[cfg(any(test, feature = "std"))]
    Arc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeType {
    target: core::any::TypeId,
    family: Family,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum Purity {
    Pure,
    Graph,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum RecordLayout {
    Leaf,
    Object,
    Collection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strength {
    Strong,
    Weak,
}

#[derive(Debug, Clone, Copy)]
enum ReferenceMode {
    #[cfg(test)]
    Owned { purity: Purity },
    Shared {
        node: NodeType,
        strength: Strength,
        #[cfg(test)]
        payload_purity: Purity,
    },
}

#[derive(Debug, Clone, Copy)]
struct ReferenceDescriptor {
    wire_id: u64,
    #[cfg(test)]
    layout: RecordLayout,
    mode: ReferenceMode,
}

impl ReferenceDescriptor {
    fn shared_node(self, expected: Strength) -> NodeType {
        #[cfg(not(test))]
        let ReferenceMode::Shared { node, strength } = self.mode;
        #[cfg(test)]
        let ReferenceMode::Shared {
            node,
            strength,
            payload_purity: _payload_purity,
        } = self.mode
        else {
            panic!("shared adapter requires a shared descriptor");
        };
        assert_eq!(
            strength, expected,
            "reference strength must match its adapter"
        );
        node
    }

    #[cfg(test)]
    fn check_expansion_depth(self, decoder: &Decoder<'_>) -> Result<()> {
        match self.layout {
            RecordLayout::Leaf => Ok(()),
            RecordLayout::Object | RecordLayout::Collection => {
                if decoder.open() > decoder.options().max_depth {
                    Err(Error::DepthExceeded)
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// [`EntryState`] 在阶段一（规划）上的投影：构造期的三个状态在规划期不可达，
/// 规划代码只关心「预留／展开中／已计划」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase1State {
    Reserved,
    Active,
    Planned,
}

struct Entry<'job> {
    node: NodeType,
    state: EntryState<'job>,
}

/// 身份在整条图解码流程中的状态。阶段一（规划）与阶段二（构造）共用同一张身份表，
/// 因此两段状态机写在同一个枚举里：规划结束即把整张表连同构造任务移交构造上下文，
/// 不再复制表、也不另存 ids→job。
// Any 擦除完整句柄而非载荷；标准 Rc::new_cyclic 提供 Open 阶段的同分配 Weak。
enum EntryState<'job> {
    /// 阶段一：弱请求预留，目标体尚未展开。
    Reserved,
    /// 阶段一：正在展开目标体；再次强到达即强回边（14）。
    Active,
    /// 阶段一完成：目标体计划已取走，构造任务就绪。`None` 是规划中的瞬时窗口。
    Planned(Option<BuildJob<'job>>),
    /// 阶段二：任务已取走、尚未登记 Weak。
    Building,
    /// 阶段二：已在 `new_cyclic` 闭包内登记自己的 Weak。
    Open { weak: Box<dyn Any> },
    /// 阶段二：构造完成。
    Done {
        weak: Box<dyn Any>,
        strong: Box<dyn Any>,
    },
}

/// 阶段一图上下文；全消息共用一个 Decoder、身份表和预算账本。
/// 只由完整消息根入口创建，字段规划必须沿用当前上下文。
pub struct DecodeCtx<'a, 'raw> {
    decoder: &'a mut Decoder<'raw>,
    entries: BTreeMap<u32, Entry<'static>>,
    strong_discovery: Vec<u32>,
    /// 虚根（owner 0）发出的强请求，即消息根的顶层身份目标；单独成表，
    /// 免去为「只有根请求」的常见形状建一张边表。
    root_targets: Vec<u32>,
    /// 虚根发出的弱请求；仅普通根（`root == 0`）可达。
    root_weak_targets: Vec<u32>,
    /// owner 非零的强边（身份之间的边）。
    strong_edges: BTreeMap<u32, Vec<u32>>,
    /// owner 非零的弱边。
    weak_edges: BTreeMap<u32, Vec<u32>>,
    current_owner: u32,
    ordinary_owners: BTreeSet<u32>,
    position: PlanPosition,
    root: u32,
}

/// 原生完整值使用的记录布局；不为宽值或集合增加身份语义。
pub enum PlanValueLayout {
    /// 不打开对象／集合层的叶记录。
    Leaf,
    /// 打开集合层的记录。
    Collection,
}

#[derive(Clone, Copy)]
enum PlanPosition {
    Slot,
    Element,
    Target(u32),
}

#[derive(Debug, PartialEq, Eq)]
enum SharedPlan<P> {
    /// 首次到达的目标计划体；由构造任务的闭包按值捕获，不再单独装箱。
    Definition {
        id: u32,
        body: P,
    },
    Reference {
        id: u32,
    },
}

impl<P> SharedPlan<P> {
    fn id(&self) -> u32 {
        match self {
            Self::Definition { id, .. } | Self::Reference { id } => *id,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WeakPlan {
    id: u32,
}

#[cfg(test)]
pub(crate) struct RootPlan<P> {
    value: P,
    root: u32,
}

#[cfg(test)]
impl<P> RootPlan<P> {
    fn value(value: P) -> Self {
        Self { value, root: 0 }
    }
}

#[cfg(test)]
impl<P> RootPlan<SharedPlan<P>> {
    fn identity(value: SharedPlan<P>) -> Self {
        Self {
            root: value.id(),
            value,
        }
    }
}

#[derive(Debug)]
#[cfg(test)]
pub(crate) struct DecodePlan<P> {
    value: P,
    root: u32,
    nodes: BTreeMap<u32, NodeType>,
    schedule: Schedule,
    materialized_bytes: usize,
}

// 显式 schema 声明；不能对所有 AceDecode 做 blanket impl。
#[cfg(test)]
trait PureValue: AceDecode {}

#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
struct Ready<T: PureValue>(T);

#[cfg(test)]
impl<T: PureValue> Ready<T> {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        T::ace_decode(decoder).map(Self)
    }
}

impl<'a, 'raw> DecodeCtx<'a, 'raw> {
    fn new(decoder: &'a mut Decoder<'raw>) -> Self {
        Self {
            decoder,
            entries: BTreeMap::new(),
            strong_discovery: Vec::new(),
            root_targets: Vec::new(),
            root_weak_targets: Vec::new(),
            strong_edges: BTreeMap::new(),
            weak_edges: BTreeMap::new(),
            current_owner: 0,
            ordinary_owners: BTreeSet::new(),
            position: PlanPosition::Slot,
            root: 0,
        }
    }

    /// 记录一条强边：虚根请求单独记，其余按 owner 归组。
    fn add_strong_edge(&mut self, id: u32) {
        if self.current_owner == 0 {
            self.root_targets.push(id);
        } else {
            self.strong_edges
                .entry(self.current_owner)
                .or_default()
                .push(id);
        }
    }

    /// 记录一条弱边；分组规则同 [`DecodeCtx::add_strong_edge`]。
    fn add_weak_edge(&mut self, id: u32) {
        if self.current_owner == 0 {
            self.root_weak_targets.push(id);
        } else {
            self.weak_edges
                .entry(self.current_owner)
                .or_default()
                .push(id);
        }
    }

    /// 测试脚本：把虚根边折回 0 号键，供直接调用 [`plan_build_schedule`] 的用例观察调度。
    #[cfg(test)]
    fn schedule_edges(&self) -> (BTreeMap<u32, Vec<u32>>, BTreeMap<u32, Vec<u32>>) {
        let mut strong = self.strong_edges.clone();
        if !self.root_targets.is_empty() {
            strong.insert(0, self.root_targets.clone());
        }
        let mut weak = self.weak_edges.clone();
        if !self.root_weak_targets.is_empty() {
            weak.insert(0, self.root_weak_targets.clone());
        }
        (strong, weak)
    }

    /// 返回已登记身份的阶段一状态；类型不符按类型冲突报 7。
    /// 阶段二状态只在 [`decode_cyclic`] 的第二段出现，规划期不可达。
    fn existing_entry(&self, id: u32, node: NodeType) -> Result<Option<Phase1State>> {
        match self.entries.get(&id) {
            Some(entry) if entry.node != node => Err(Error::InvalidReference),
            Some(entry) => Ok(Some(match entry.state {
                EntryState::Reserved => Phase1State::Reserved,
                EntryState::Active => Phase1State::Active,
                EntryState::Planned(_) => Phase1State::Planned,
                EntryState::Building | EntryState::Open { .. } | EntryState::Done { .. } => {
                    panic!("phase two state during planning")
                }
            })),
            None => Ok(None),
        }
    }

    fn check_entry_budget(&self) -> Result<()> {
        if self.entries.len() >= self.decoder.options().max_references {
            return Err(Error::ResourceLimit);
        }
        Ok(())
    }

    #[cfg(test)]
    fn plan_owned<P, F>(&mut self, descriptor: ReferenceDescriptor, parse_target: F) -> Result<P>
    where
        F: FnOnce(&mut Self, u32) -> Result<P>,
    {
        let ReferenceMode::Owned { purity: _purity } = descriptor.mode else {
            panic!("owned adapter requires an owned descriptor");
        };
        let id = self.decoder.record_span(descriptor.wire_id)?.inst_id as u32;
        descriptor.check_expansion_depth(self.decoder)?;
        // 普通拥有值不登记身份，也不改变最近的身份 owner。
        parse_target(self, id)
    }

    fn resolve_strong<P, F>(
        &mut self,
        descriptor: ReferenceDescriptor,
        parse_target: F,
    ) -> Result<SharedPlan<P>>
    where
        F: FnOnce(&mut Self, u32) -> Result<P>,
    {
        let node = descriptor.shared_node(Strength::Strong);
        let id = self.decoder.record_span(descriptor.wire_id)?.inst_id as u32;
        let previous = self.existing_entry(id, node)?;
        match previous {
            Some(Phase1State::Active) => return Err(Error::CycleUnsupported),
            Some(Phase1State::Planned) => {
                self.add_strong_edge(id);
                return Ok(SharedPlan::Reference { id });
            }
            None | Some(Phase1State::Reserved) => {}
        }
        // 复用不打开假想层；生产目标由实际 object/value reader 检查展开深度。
        // C1 脚本没有目标 trait，仍用其显式布局检查。
        #[cfg(test)]
        descriptor.check_expansion_depth(self.decoder)?;
        if previous.is_none() {
            self.check_entry_budget()?;
        }
        self.add_strong_edge(id);
        self.entries.insert(
            id,
            Entry {
                node,
                state: EntryState::Active,
            },
        );
        let discovery_len = self.strong_discovery.len();
        self.strong_discovery.push(id);
        let owner = core::mem::replace(&mut self.current_owner, id);
        let parsed = parse_target(self, id);
        self.current_owner = owner;
        match parsed {
            Ok(body) => {
                self.entries.insert(
                    id,
                    Entry {
                        node,
                        state: EntryState::Planned(None),
                    },
                );
                Ok(SharedPlan::Definition { id, body })
            }
            Err(error) => {
                // 失败时恢复活动身份；整个事务必须丢弃，不支持继续规划兄弟字段或重试。
                // 能走到这里的先前状态只可能是 `Reserved`（其 job 必为 `None`）。
                if previous.is_some() {
                    self.entries.insert(
                        id,
                        Entry {
                            node,
                            state: EntryState::Reserved,
                        },
                    );
                } else {
                    self.entries.remove(&id);
                }
                self.strong_discovery.truncate(discovery_len);
                Err(error)
            }
        }
    }

    fn resolve_weak(&mut self, descriptor: ReferenceDescriptor) -> Result<WeakPlan> {
        let node = descriptor.shared_node(Strength::Weak);
        let id = self.decoder.record_span(descriptor.wire_id)?.inst_id as u32;
        if self.existing_entry(id, node)?.is_none() {
            self.check_entry_budget()?;
            self.entries.insert(
                id,
                Entry {
                    node,
                    state: EntryState::Reserved,
                },
            );
        }
        self.add_weak_edge(id);
        Ok(WeakPlan { id })
    }
}

impl DecodeCtx<'_, '_> {
    fn reference(&mut self) -> Result<u64> {
        match self.position {
            PlanPosition::Slot => self
                .decoder
                .decode_required_scalar()
                .map(u64::from_le_bytes),
            PlanPosition::Element => self.decoder.read_element_id().map(u64::from),
            PlanPosition::Target(id) => self.target_element(id, |ctx| ctx.reference()),
        }
    }

    // 共享目标体没有外部槽；元素 reader 复用既有纯值校验与物化收费点。
    fn target_element<P>(
        &mut self,
        id: u32,
        read: impl FnOnce(&mut Self) -> Result<P>,
    ) -> Result<P> {
        self.decoder.begin_record_view(id)?;
        self.decoder.begin_element();
        let previous = core::mem::replace(&mut self.position, PlanPosition::Element);
        let parsed = read(self).and_then(|value| {
            if self.decoder.peek_body()?.is_empty() {
                Ok(value)
            } else {
                Err(Error::InvalidTag)
            }
        });
        self.position = previous;
        self.decoder.end_element();
        self.decoder.end_record_view();
        parsed
    }

    /// 按当前真实位置打开对象；普通拥有型对象不建立新的身份 owner。
    ///
    /// # Errors
    /// 返回对象引用、深度、预算、槽结构或字段读取错误；失败事务须丢弃。
    pub fn object<P, F>(&mut self, read_fields: F) -> Result<P>
    where
        F: FnOnce(&mut Self) -> Result<P>,
    {
        let id = match self.position {
            PlanPosition::Target(id) => u64::from(id),
            _ => self.reference()?,
        };
        self.decoder.enter_object(id)?;
        // 外部集合的元素作用域仍在，但对象内的字段必须读取 8B 槽。
        let previous = core::mem::replace(&mut self.position, PlanPosition::Slot);
        let parsed = read_fields(self);
        self.position = previous;
        let left = self.decoder.leave_object();
        parsed.and_then(|plan| left.map(|()| plan))
    }

    /// 按严格递增的 order 规划当前对象的 Cyclic 字段，不另启图事务。
    ///
    /// # Errors
    /// 返回字段选择或该字段的阶段一错误。
    pub fn field<T: AceDecodeCyclic>(&mut self, order: u32) -> Result<T::Plan> {
        self.decoder.begin_field(order)?;
        let previous = core::mem::replace(&mut self.position, PlanPosition::Slot);
        let parsed = T::decode_plan(self);
        self.position = previous;
        parsed
    }

    /// 立即读取只有普通 AceDecode 的字段；完整值直接保存在调用者的 Plan。
    /// 非零身份 owner 按集合去重，规划结束后拒绝环上的普通字段。
    ///
    /// # Errors
    /// 返回当前字段的普通读取错误；owner 的环合法性由根驱动最终检查。
    pub fn ordinary_field<T: AceDecode>(&mut self, order: u32) -> Result<T> {
        let owner = self.current_owner;
        self.decoder.begin_field(order)?;
        if owner != 0 {
            self.ordinary_owners.insert(owner);
        }
        T::ace_decode(self.decoder)
    }

    /// 读取明确提供 Cyclic 桥接的完整纯值，复用既有值／元素 reader。
    ///
    /// # Errors
    /// 返回内容、引用、深度或物化预算错误，不创建图身份。
    pub fn value<T>(&mut self, layout: PlanValueLayout) -> Result<T>
    where
        T: AceDecode + AceElement + AceDecodeCyclic,
    {
        match self.position {
            PlanPosition::Slot => T::ace_decode(self.decoder),
            PlanPosition::Element => T::decode_element(self.decoder),
            PlanPosition::Target(id) => {
                // String 等引用叶体与集合 reader 需要 id；标量／宽值体直接走元素 reader。
                if matches!(layout, PlanValueLayout::Collection)
                    || matches!(T::element_kind(), ElementKind::Ref)
                {
                    let previous = self
                        .decoder
                        .selected
                        .replace(Some(u64::from(id).to_le_bytes()));
                    let parsed = T::ace_decode(self.decoder);
                    self.decoder.selected = previous;
                    parsed
                } else {
                    self.target_element(id, |ctx| T::decode_element(ctx.decoder))
                }
            }
        }
    }

    /// 规划 Rc 目标；首次强到达保存类型化计划，后续共享只返回同一 id。
    ///
    /// # Errors
    /// 引用／身份类型冲突返回 7，强回边返回 14，其余错误按真实展开顺序返回。
    pub fn strong<T>(&mut self) -> Result<u32>
    where
        T: AceDecodeCyclic + 'static,
        T::Plan: 'static,
    {
        self.strong_in_family::<T>(Family::Rc)
    }

    /// 规划 Arc 目标；与 `strong` 共用身份表和调度，只替换指针族。
    ///
    /// # Errors
    /// 同 `strong`。
    #[cfg(feature = "std")]
    pub(crate) fn strong_arc<T>(&mut self) -> Result<u32>
    where
        T: AceDecodeCyclic + 'static,
        T::Plan: 'static,
    {
        self.strong_in_family::<T>(Family::Arc)
    }

    fn strong_in_family<T>(&mut self, family: Family) -> Result<u32>
    where
        T: AceDecodeCyclic + 'static,
        T::Plan: 'static,
    {
        let root = matches!(self.position, PlanPosition::Slot)
            && self.decoder.frames.is_empty()
            && self.decoder.coll_depth == 0
            && self.decoder.element_depth == 0;
        let wire_id = self.reference()?;
        let descriptor = ReferenceDescriptor {
            wire_id,
            // C1 脚本的预声明布局仅供测试；生产由 T 的实际读取入口执行深度检查。
            #[cfg(test)]
            layout: RecordLayout::Leaf,
            mode: ReferenceMode::Shared {
                node: NodeType {
                    target: core::any::TypeId::of::<T>(),
                    family,
                },
                strength: Strength::Strong,
                #[cfg(test)]
                payload_purity: Purity::Graph,
            },
        };
        let plan = self.resolve_strong(descriptor, |ctx, id| {
            let previous = core::mem::replace(&mut ctx.position, PlanPosition::Target(id));
            let parsed = T::decode_plan(ctx);
            ctx.position = previous;
            parsed
        })?;
        let id = plan.id();
        if let SharedPlan::Definition { body, .. } = plan {
            let job = match family {
                Family::Rc => make_rc_job(id, body, T::build_from_plan),
                #[cfg(feature = "std")]
                Family::Arc => make_arc_job(id, body, T::build_from_plan),
                #[cfg(all(test, not(feature = "std")))]
                Family::Arc => panic!("Arc jobs require the std feature"),
            };
            let entry = self
                .entries
                .get_mut(&id)
                .expect("planned identity must exist");
            assert_eq!(entry.node, job.node, "job type must match planned identity");
            let EntryState::Planned(slot) = &mut entry.state else {
                panic!("only a planned identity takes a job");
            };
            assert!(
                slot.replace(job.run).is_none(),
                "one job per planned identity"
            );
        }
        if root {
            self.root = id;
        }
        Ok(id)
    }

    /// 登记 Weak 请求，不展开目标体，也不扩展强可达集合。
    ///
    /// # Errors
    /// 无效引用、类型冲突或身份表预算错误立即返回；缺少强目标在根规划结束时返回 7。
    pub fn weak<T: 'static>(&mut self) -> Result<u32> {
        self.weak_in_family::<T>(Family::Rc)
    }

    /// 登记 sync::Weak 请求；语义同 `weak`，只替换指针族。
    ///
    /// # Errors
    /// 同 `weak`。
    #[cfg(feature = "std")]
    pub(crate) fn weak_arc<T: 'static>(&mut self) -> Result<u32> {
        self.weak_in_family::<T>(Family::Arc)
    }

    fn weak_in_family<T: 'static>(&mut self, family: Family) -> Result<u32> {
        let wire_id = self.reference()?;
        self.resolve_weak(ReferenceDescriptor {
            wire_id,
            #[cfg(test)]
            layout: RecordLayout::Leaf,
            mode: ReferenceMode::Shared {
                node: NodeType {
                    target: core::any::TypeId::of::<T>(),
                    family,
                },
                strength: Strength::Weak,
                #[cfg(test)]
                payload_purity: Purity::Graph,
            },
        })
        .map(|plan| plan.id)
    }

    /// 槽位置沿用缺席语义；元素位置读取 0／1 presence byte，不允许嵌套 Option。
    ///
    /// # Errors
    /// 非法 presence 返回 4；Some 的错误由内层阶段一读取返回。
    pub fn option<T>(&mut self) -> Result<Option<T::Plan>>
    where
        T: AceDecodeCyclic + NonOptional,
    {
        let present = match self.position {
            PlanPosition::Slot => !self.decoder.selected_is_none(),
            PlanPosition::Element => match self.decoder.read_element_bytes(1)?[0] {
                0 => false,
                1 => true,
                _ => return Err(Error::InvalidTag),
            },
            PlanPosition::Target(id) => return self.target_element(id, |ctx| ctx.option::<T>()),
        };
        if present {
            T::decode_plan(self).map(Some)
        } else {
            Ok(None)
        }
    }

    /// 按线序规划映射；当前键对应的值校验成功后才检查重复键。
    ///
    /// # Errors
    /// 返回键、值、引用、集合预算或尾部错误；重复键返回 16（`DuplicateKey`），不继续读取后续对。
    pub fn map<K, V>(&mut self) -> Result<alloc::collections::BTreeMap<K, V::Plan>>
    where
        K: AceElement + Ord,
        V: AceDecodeCyclic,
    {
        let wire_id = match self.position {
            PlanPosition::Target(id) => u64::from(id),
            _ => self.reference()?,
        };
        let id = u32::try_from(wire_id).map_err(|_| Error::InvalidReference)?;
        self.decoder.enter_collection()?;
        if let Err(error) = self.decoder.begin_record_view(id) {
            self.decoder.leave_collection();
            return Err(error);
        }
        let parsed = (|| {
            let count = self.decoder.read_element_id()? as usize;
            self.decoder.check_collection_len(count)?;
            let mut map = BTreeMap::new();
            self.decoder.begin_element();
            let previous = core::mem::replace(&mut self.position, PlanPosition::Element);
            let pairs = (0..count).try_for_each(|_| {
                let key = K::decode_map_key(self.decoder)?;
                let value = V::decode_plan(self)?;
                // 当前值的全部内容（含普通字段）合法后，才到达本对的查重点。
                // 用 `entry` 而非 `insert`：后者先替换旧值再返回旧值，会让后者覆盖前者。
                match map.entry(key) {
                    btree_map::Entry::Occupied(_) => return Err(Error::DuplicateKey),
                    btree_map::Entry::Vacant(slot) => {
                        slot.insert(value);
                    }
                }
                Ok(())
            });
            self.position = previous;
            self.decoder.end_element();
            pairs?;
            if !self.decoder.peek_body()?.is_empty() {
                return Err(Error::InvalidTag);
            }
            Ok(map)
        })();
        self.decoder.end_record_view();
        self.decoder.leave_collection();
        parsed
    }

    /// 规划 Vec 集合的 `T::Plan` 序列：先尝试整段 bulk，否则逐项规划，并保持线序。
    ///
    /// # Errors
    /// 返回引用、集合预算、元素规划或尾部错误；尾部非空返回 InvalidTag（4）。
    pub(crate) fn vec<T: AceDecodeCyclic>(&mut self) -> Result<Vec<T::Plan>> {
        let id = self.collection_id()?;
        self.decoder.enter_collection()?;
        if let Err(error) = self.decoder.begin_record_view(id) {
            self.decoder.leave_collection();
            return Err(error);
        }
        let previous = core::mem::replace(&mut self.position, PlanPosition::Element);
        self.decoder.begin_element();
        let parsed = (|| {
            let count = self.decoder.read_element_id()? as usize;
            self.decoder.check_collection_len(count)?;
            let mut consumed = 0;
            let bulk =
                T::decode_elements_bulk_plan(count, self.decoder.peek_body()?, &mut consumed)?;
            match bulk {
                Some(plans) => {
                    self.decoder.advance_body(consumed)?;
                    Ok(plans)
                }
                None => (0..count).map(|_| T::decode_plan(self)).collect(),
            }
        })();
        // 尾检须在视图与元素作用域仍打开时执行；内容读取失败保留原错误。
        let parsed = parsed.and_then(|plans| {
            if self.decoder.peek_body()?.is_empty() {
                Ok(plans)
            } else {
                Err(Error::InvalidTag)
            }
        });
        self.position = previous;
        self.decoder.end_element();
        self.decoder.end_record_view();
        self.decoder.leave_collection();
        parsed
    }

    /// 规划 Box 的唯一元素：与 Vec 相同的视图与收束规则，但无 count、无 bulk。
    ///
    /// # Errors
    /// 返回引用、集合预算、元素规划或尾部错误；尾部非空返回 InvalidTag（4）。
    pub(crate) fn boxed<T: AceDecodeCyclic>(&mut self) -> Result<Box<T::Plan>> {
        let id = self.collection_id()?;
        self.decoder.enter_collection()?;
        if let Err(error) = self.decoder.begin_record_view(id) {
            self.decoder.leave_collection();
            return Err(error);
        }
        let previous = core::mem::replace(&mut self.position, PlanPosition::Element);
        self.decoder.begin_element();
        let parsed = T::decode_plan(self).map(Box::new).and_then(|plan| {
            if self.decoder.peek_body()?.is_empty() {
                Ok(plan)
            } else {
                Err(Error::InvalidTag)
            }
        });
        self.position = previous;
        self.decoder.end_element();
        self.decoder.end_record_view();
        self.decoder.leave_collection();
        parsed
    }

    /// 集合类入口的共同引用解析：Target 直取 id，其他位置经 `reference()`，并转换为 u32。
    fn collection_id(&mut self) -> Result<u32> {
        let wire_id = match self.position {
            PlanPosition::Target(id) => u64::from(id),
            _ => self.reference()?,
        };
        u32::try_from(wire_id).map_err(|_| Error::InvalidReference)
    }

    fn finish_plan(&mut self, root: u32) -> Result<Schedule> {
        for entry in self.entries.values() {
            match entry.state {
                EntryState::Reserved => return Err(Error::InvalidReference),
                EntryState::Active => panic!("root adapter returned with an active identity"),
                EntryState::Planned(_) => {}
                EntryState::Building | EntryState::Open { .. } | EntryState::Done { .. } => {
                    panic!("phase two state before phase two")
                }
            }
        }
        // 身份之间无边时（所有身份都由虚根直接请求）环检查恒无环：慢路径的前向／反向表
        // 都为空，每个分量都是单点、`cyclic` 恒为假。故只在该前提不成立时走完整 SCC 扫描。
        let no_identity_edges = self.strong_edges.is_empty() && self.weak_edges.is_empty();
        if !no_identity_edges {
            self.validate_ordinary_cycles()?;
        }
        self.decoder.validate_finished()?;
        if root != 0 {
            assert_eq!(
                self.root_targets.as_slice(),
                [root],
                "identity root must be the only top-level strong target"
            );
            assert!(
                self.root_weak_targets.is_empty(),
                "identity root cannot have top-level weak requests"
            );
        }
        // 同一前提（身份之间无边）下调度也有闭式结果：每个身份都是独立源，按发现序号
        // 依次 `Push` 后即可 `Pop`（其强目标与弱请求计数都为零），因此事件按发现序成对出现、
        // 峰值恒为 1（空图为 0）。慢路径在此前提下的源集合、可达集合与就绪判定都只产生这一结果。
        let schedule = if no_identity_edges {
            let ids = &self.strong_discovery;
            if root != 0 && ids.as_slice() != [root] {
                return Err(Error::InvalidReference);
            }
            let mut events = Vec::with_capacity(ids.len() * 2);
            for &id in ids {
                events.push(Event::Push(id));
                events.push(Event::Pop(id));
            }
            let schedule = Schedule {
                events,
                peak: usize::from(!ids.is_empty()),
            };
            // 闭式结果必须与完整调度器逐事件一致；调试构建下由既有全部图用例交叉验证。
            #[cfg(debug_assertions)]
            {
                let (strong, weak) = self.schedule_inputs(root);
                debug_assert_eq!(
                    plan_build_schedule(&self.strong_discovery, &strong, &weak, root),
                    Ok(schedule.clone()),
                    "closed-form schedule must match the full scheduler"
                );
            }
            schedule
        } else {
            let (strong, weak) = self.schedule_inputs(root);
            plan_build_schedule(&self.strong_discovery, &strong, &weak, root)?
        };
        if schedule.peak > self.decoder.options().max_build_nesting {
            return Err(Error::ResourceLimit);
        }
        Ok(schedule)
    }

    /// 供 [`plan_build_schedule`] 使用的边表：按 owner 归组，虚根边（仅普通根保留）
    /// 折回 0 号键；两者都没有时返回空表，调度器据此走空图／单点路径。
    fn schedule_inputs(&mut self, root: u32) -> (BTreeMap<u32, Vec<u32>>, BTreeMap<u32, Vec<u32>>) {
        let (mut strong, mut weak) = (
            core::mem::take(&mut self.strong_edges),
            core::mem::take(&mut self.weak_edges),
        );
        if root == 0 {
            if !self.root_targets.is_empty() {
                strong.insert(0, core::mem::take(&mut self.root_targets));
            }
            if !self.root_weak_targets.is_empty() {
                weak.insert(0, core::mem::take(&mut self.root_weak_targets));
            }
        }
        (strong, weak)
    }

    fn validate_ordinary_cycles(&self) -> Result<()> {
        // 仓库与标准库均无 SCC API；两次显式栈遍历避免递归扫描不可信深图。
        let mut forward: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let mut reverse: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (&owner, targets) in self.strong_edges.iter().chain(&self.weak_edges) {
            debug_assert_ne!(owner, 0, "virtual root edges are not grouped by owner");
            for &target in targets {
                forward.entry(owner).or_default().push(target);
                reverse.entry(target).or_default().push(owner);
            }
        }
        let mut seen = BTreeSet::new();
        let mut order = Vec::new();
        for &id in &self.strong_discovery {
            let mut pending = alloc::vec![(id, false)];
            while let Some((id, exiting)) = pending.pop() {
                if exiting {
                    order.push(id);
                } else if seen.insert(id) {
                    pending.push((id, true));
                    pending.extend(
                        forward
                            .get(&id)
                            .into_iter()
                            .flatten()
                            .map(|&target| (target, false)),
                    );
                }
            }
        }
        seen.clear();
        for id in order.into_iter().rev() {
            if !seen.insert(id) {
                continue;
            }
            let mut component = alloc::vec![id];
            let mut pending = alloc::vec![id];
            while let Some(id) = pending.pop() {
                for &owner in reverse.get(&id).into_iter().flatten() {
                    if seen.insert(owner) {
                        component.push(owner);
                        pending.push(owner);
                    }
                }
            }
            let cyclic = component.len() > 1
                || forward
                    .get(&id)
                    .is_some_and(|targets| targets.contains(&id));
            if cyclic && component.iter().any(|id| self.ordinary_owners.contains(id)) {
                return Err(Error::CycleUnsupported);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
std::thread_local! {
    // 测试 10、11 的阶段二见证：每进入一次 `BuildCtx::new` 计一次；线程局部，避免并行测试互相计数。
    static BUILD_CTX_ENTRIES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

pub(super) fn decode_cyclic<T: AceDecodeCyclic>(decoder: &mut Decoder<'_>) -> Result<T> {
    let mut ctx = DecodeCtx::new(decoder);
    let plan = T::decode_plan(&mut ctx)?;
    let root = ctx.root;
    let schedule = ctx.finish_plan(root)?;
    // 身份表连同其构造任务整体移交阶段二，不在两阶段之间重建表。
    let entries = core::mem::take(&mut ctx.entries);
    drop(ctx);
    let mut ctx = BuildCtx::new(entries);
    #[cfg(test)]
    BUILD_CTX_ENTRIES.set(BUILD_CTX_ENTRIES.get() + 1);
    let mut cursor = BuildCursor {
        events: &schedule.events,
        next: 0,
    };
    cursor.run_all(&mut ctx);
    let value = T::build_from_plan(&mut ctx, plan);
    // 交付前释放临时强缓存；Weak-only 句柄不能从缓存获得隐藏的强 owner。
    drop(ctx);
    Ok(value)
}

/// C1 脚本接线复用生产身份表、完成校验及调度，不创建图节点。
#[cfg(test)]
fn decode_plan<P, F>(mut decoder: Decoder<'_>, plan_root: F) -> Result<DecodePlan<P>>
where
    F: FnOnce(&mut DecodeCtx) -> Result<RootPlan<P>>,
{
    let mut ctx = DecodeCtx::new(&mut decoder);
    let RootPlan { value, root } = plan_root(&mut ctx)?;
    let materialized_bytes = ctx.decoder.decoded;
    let schedule = ctx.finish_plan(root)?;
    // 脚本观察身份集合：只读投影身份表，不参与生产路径。
    let nodes = ctx
        .entries
        .iter()
        .map(|(&id, entry)| (id, entry.node))
        .collect();
    Ok(DecodePlan {
        value,
        root,
        nodes,
        schedule,
        materialized_bytes,
    })
}

type BuildJob<'job> = Box<
    dyn for<'ctx, 'cursor, 'events> FnOnce(
            &'ctx mut BuildCtx<'job>,
            &'cursor mut BuildCursor<'events>,
        ) + 'job,
>;

struct PreparedJob<'job> {
    node: NodeType,
    run: BuildJob<'job>,
}

/// 不可失败的构造上下文；句柄返回值不借用本上下文。
pub struct BuildCtx<'a> {
    entries: BTreeMap<u32, Entry<'a>>,
}

impl<'job> BuildCtx<'job> {
    /// 接管阶段一身份表：每个已计划身份都带着自己的构造任务，整表原样移交。
    fn new(entries: BTreeMap<u32, Entry<'job>>) -> Self {
        for (&id, entry) in &entries {
            assert_ne!(id, 0, "virtual root cannot have a job");
            // std 下 Rc 与 Arc 两族都是真实 job；非 std 构建只会产生 Rc。
            #[cfg(not(feature = "std"))]
            assert_eq!(
                entry.node.family,
                Family::Rc,
                "only Rc jobs are supported without std"
            );
            assert!(
                matches!(entry.state, EntryState::Planned(Some(_))),
                "only planned identities with a job reach the build phase"
            );
        }
        Self { entries }
    }

    /// C1 脚本按「身份 + 自备任务」装配构造上下文；生产路径改由 [`BuildCtx::new`] 继承身份表。
    #[cfg(test)]
    fn from_parts(
        nodes: BTreeMap<u32, NodeType>,
        mut jobs: BTreeMap<u32, PreparedJob<'job>>,
    ) -> Self {
        assert_eq!(
            nodes.len(),
            jobs.len(),
            "job ids must match planned identities"
        );
        let entries = nodes
            .into_iter()
            .map(|(id, node)| {
                let job = jobs.remove(&id).expect("planned identity must have a job");
                assert_eq!(node, job.node, "job type must match planned identity");
                (
                    id,
                    Entry {
                        node,
                        state: EntryState::Planned(Some(job.run)),
                    },
                )
            })
            .collect();
        assert!(jobs.is_empty(), "job ids must match planned identities");
        Self { entries }
    }

    fn entry<T: 'static>(&self, id: u32, family: Family) -> &Entry<'job> {
        let entry = self.entries.get(&id).expect("build identity must exist");
        assert_eq!(
            entry.node,
            NodeType {
                target: core::any::TypeId::of::<T>(),
                family,
            },
            "build target type and family must match"
        );
        entry
    }

    fn entry_mut<T: 'static>(&mut self, id: u32, family: Family) -> &mut Entry<'job> {
        let entry = self
            .entries
            .get_mut(&id)
            .expect("build identity must exist");
        assert_eq!(
            entry.node,
            NodeType {
                target: core::any::TypeId::of::<T>(),
                family,
            },
            "build target type and family must match"
        );
        entry
    }

    /// 在 new_cyclic 闭包内先登记自己的 Weak，再构造任何子节点。
    ///
    /// # Panics
    /// 身份类型不符、job 尚未取出或重复登记时属于内部协议错误。
    pub fn bind_weak<T: 'static>(&mut self, id: u32, weak: Weak<T>) {
        let entry = self.entry_mut::<T>(id, Family::Rc);
        assert!(
            matches!(entry.state, EntryState::Building),
            "Weak must bind once, immediately after taking its job"
        );
        entry.state = EntryState::Open {
            weak: Box::new(weak),
        };
    }

    /// 克隆已 Open／Done 目标的 Weak；未知或 Unopened 目标返回 None。
    ///
    /// # Panics
    /// 已知身份的类型、family 或擦除载荷不匹配时属于内部协议错误。
    pub fn weak<T: 'static>(&self, id: u32) -> Option<Weak<T>> {
        self.entries.get(&id)?;
        let weak = match &self.entry::<T>(id, Family::Rc).state {
            EntryState::Open { weak } | EntryState::Done { weak, .. } => weak,
            EntryState::Planned(_) | EntryState::Building => return None,
            EntryState::Reserved | EntryState::Active => {
                panic!("phase one state during build")
            }
        };
        Some(Weak::clone(
            weak.downcast_ref::<Weak<T>>()
                .expect("cached Weak type must match identity"),
        ))
    }

    fn complete<T: 'static>(&mut self, id: u32, strong: Rc<T>) {
        let entry = self.entry_mut::<T>(id, Family::Rc);
        let EntryState::Open { weak } = core::mem::replace(&mut entry.state, EntryState::Building)
        else {
            panic!("only an open identity can complete");
        };
        assert!(
            Weak::ptr_eq(
                weak.downcast_ref::<Weak<T>>()
                    .expect("cached Weak type must match identity"),
                &Rc::downgrade(&strong)
            ),
            "completed Rc must match bound Weak allocation"
        );
        entry.state = EntryState::Done {
            weak,
            strong: Box::new(strong),
        };
    }

    /// 克隆已 Done 目标的 Rc；未知或尚未完成的目标返回 None。
    ///
    /// # Panics
    /// 已知身份的类型、family 或擦除载荷不匹配时属于内部协议错误。
    pub fn strong<T: 'static>(&self, id: u32) -> Option<Rc<T>> {
        self.entries.get(&id)?;
        let EntryState::Done { strong, .. } = &self.entry::<T>(id, Family::Rc).state else {
            return None;
        };
        Some(Rc::clone(
            strong
                .downcast_ref::<Rc<T>>()
                .expect("cached Rc type must match identity"),
        ))
    }

    /// 在 Arc::new_cyclic 闭包内先登记自己的 sync::Weak，再构造任何子节点。
    ///
    /// # Panics
    /// 身份类型或族不符、job 尚未取出或重复登记时属于内部协议错误。
    #[cfg(feature = "std")]
    fn bind_weak_arc<T: 'static>(&mut self, id: u32, weak: SyncWeak<T>) {
        let entry = self.entry_mut::<T>(id, Family::Arc);
        assert!(
            matches!(entry.state, EntryState::Building),
            "Weak must bind once, immediately after taking its job"
        );
        entry.state = EntryState::Open {
            weak: Box::new(weak),
        };
    }

    /// 克隆已 Open／Done 的 sync::Weak；未知或 Unopened 目标返回 None。
    ///
    /// # Panics
    /// 已知身份的类型、family 或擦除载荷不匹配时属于内部协议错误。
    #[cfg(feature = "std")]
    pub(crate) fn weak_arc<T: 'static>(&self, id: u32) -> Option<SyncWeak<T>> {
        self.entries.get(&id)?;
        let weak = match &self.entry::<T>(id, Family::Arc).state {
            EntryState::Open { weak } | EntryState::Done { weak, .. } => weak,
            EntryState::Planned(_) | EntryState::Building => return None,
            EntryState::Reserved | EntryState::Active => {
                panic!("phase one state during build")
            }
        };
        Some(SyncWeak::clone(
            weak.downcast_ref::<SyncWeak<T>>()
                .expect("cached Weak type must match identity"),
        ))
    }

    #[cfg(feature = "std")]
    fn complete_arc<T: 'static>(&mut self, id: u32, strong: Arc<T>) {
        let entry = self.entry_mut::<T>(id, Family::Arc);
        let EntryState::Open { weak } = core::mem::replace(&mut entry.state, EntryState::Building)
        else {
            panic!("only an open identity can complete");
        };
        assert!(
            SyncWeak::ptr_eq(
                weak.downcast_ref::<SyncWeak<T>>()
                    .expect("cached Weak type must match identity"),
                &Arc::downgrade(&strong)
            ),
            "completed Arc must match bound Weak allocation"
        );
        entry.state = EntryState::Done {
            weak,
            strong: Box::new(strong),
        };
    }

    /// 克隆已 Done 目标的 Arc；未知或尚未完成的目标返回 None。
    ///
    /// # Panics
    /// 已知身份的类型、family 或擦除载荷不匹配时属于内部协议错误。
    #[cfg(feature = "std")]
    pub(crate) fn strong_arc<T: 'static>(&self, id: u32) -> Option<Arc<T>> {
        self.entries.get(&id)?;
        let EntryState::Done { strong, .. } = &self.entry::<T>(id, Family::Arc).state else {
            return None;
        };
        Some(Arc::clone(
            strong
                .downcast_ref::<Arc<T>>()
                .expect("cached Arc type must match identity"),
        ))
    }

    fn run_job(&mut self, id: u32, cursor: &mut BuildCursor<'_>) {
        let job = {
            let entry = self
                .entries
                .get_mut(&id)
                .expect("scheduled identity must exist");
            match core::mem::replace(&mut entry.state, EntryState::Building) {
                EntryState::Planned(Some(job)) => job,
                EntryState::Planned(None) => panic!("identity job already taken"),
                EntryState::Building | EntryState::Open { .. } | EntryState::Done { .. } => {
                    panic!("identity job already executed")
                }
                EntryState::Reserved | EntryState::Active => {
                    panic!("phase one state during build")
                }
            }
        };
        // 先结束表项借用，嵌套 job 才能独占借用整个上下文。
        job(self, cursor);
        assert!(
            matches!(self.entries[&id].state, EntryState::Done { .. }),
            "identity job must complete before returning"
        );
    }
}

struct BuildCursor<'events> {
    events: &'events [Event],
    next: usize,
}

impl BuildCursor<'_> {
    fn take_required(&mut self) -> Event {
        let event = *self
            .events
            .get(self.next)
            .expect("build events ended before matching Pop");
        self.next += 1;
        event
    }

    fn drive_until_pop(&mut self, ctx: &mut BuildCtx<'_>, owner: u32) {
        loop {
            match self.take_required() {
                Event::Push(child) => ctx.run_job(child, self),
                Event::Pop(id) => {
                    assert_eq!(id, owner, "build Pop must match the current owner");
                    return;
                }
            }
        }
    }

    fn run_all(&mut self, ctx: &mut BuildCtx<'_>) {
        while self.next < self.events.len() {
            let Event::Push(id) = self.take_required() else {
                panic!("top-level build event must be Push");
            };
            ctx.run_job(id, self);
        }
    }
}

fn make_rc_job<'job, T, P, F>(id: u32, body: P, builder: F) -> PreparedJob<'job>
where
    T: 'static,
    P: 'job,
    F: FnOnce(&mut BuildCtx<'job>, P) -> T + 'job,
{
    let run: BuildJob<'job> = Box::new(
        move |ctx: &mut BuildCtx<'job>, cursor: &mut BuildCursor<'_>| {
            let strong = Rc::new_cyclic(|own_weak: &Weak<T>| {
                ctx.bind_weak::<T>(id, own_weak.clone());
                cursor.drive_until_pop(ctx, id);
                builder(ctx, body)
            });
            ctx.complete(id, strong);
        },
    );
    PreparedJob {
        node: NodeType {
            target: core::any::TypeId::of::<T>(),
            family: Family::Rc,
        },
        run,
    }
}

#[cfg(feature = "std")]
fn make_arc_job<'job, T, P, F>(id: u32, body: P, builder: F) -> PreparedJob<'job>
where
    T: 'static,
    P: 'job,
    F: FnOnce(&mut BuildCtx<'job>, P) -> T + 'job,
{
    let run: BuildJob<'job> = Box::new(
        move |ctx: &mut BuildCtx<'job>, cursor: &mut BuildCursor<'_>| {
            let strong = Arc::new_cyclic(|own_weak: &SyncWeak<T>| {
                ctx.bind_weak_arc::<T>(id, own_weak.clone());
                cursor.drive_until_pop(ctx, id);
                builder(ctx, body)
            });
            ctx.complete_arc(id, strong);
        },
    );
    PreparedJob {
        node: NodeType {
            target: core::any::TypeId::of::<T>(),
            family: Family::Arc,
        },
        run,
    }
}

/// 仅阶段一返回输入错误；prepare 及构造阶段的协议违反一律 panic。
#[cfg(test)]
fn decode_and_build<'raw, 'job, P, Q, R, F, Prepare, BuildRoot>(
    decoder: Decoder<'raw>,
    plan_root: F,
    prepare: Prepare,
    build_root: BuildRoot,
) -> Result<R>
where
    F: FnOnce(&mut DecodeCtx) -> Result<RootPlan<P>>,
    Prepare: FnOnce(P) -> (Q, BTreeMap<u32, PreparedJob<'job>>),
    BuildRoot: FnOnce(&mut BuildCtx<'job>, Q) -> R,
{
    let DecodePlan {
        value,
        root,
        nodes,
        schedule,
        ..
    } = decode_plan(decoder, plan_root)?;
    let (root_plan, jobs) = prepare(value);
    let mut ctx = BuildCtx::from_parts(nodes, jobs);
    let mut cursor = BuildCursor {
        events: &schedule.events,
        next: 0,
    };
    cursor.run_all(&mut ctx);
    assert!(
        ctx.entries
            .values()
            .all(|entry| matches!(entry.state, EntryState::Done { .. })),
        "all identities must finish before root assembly"
    );
    assert_eq!(
        cursor.next,
        schedule.events.len(),
        "all build events must be consumed"
    );
    if root != 0 {
        assert!(
            matches!(
                ctx.entries
                    .get(&root)
                    .expect("identity root must exist")
                    .state,
                EntryState::Done { .. }
            ),
            "identity root must be done"
        );
    }
    let value = build_root(&mut ctx, root_plan);
    // 根交付前释放临时强缓存，不让上下文成为 Weak-only 目标的隐藏 owner。
    drop(ctx);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{
        collections::BTreeSet,
        rc::{Rc, Weak},
        vec,
    };
    use core::cell::{Cell, RefCell};

    use crate::limits::Options;

    #[derive(Debug)]
    struct ModelNode {
        own: Weak<ModelNode>,
        dropped: Rc<Cell<usize>>,
    }

    impl Drop for ModelNode {
        fn drop(&mut self) {
            self.dropped.set(self.dropped.get() + 1);
        }
    }

    struct OtherModelNode;

    #[derive(Debug, PartialEq, Eq)]
    struct ModelValue(u32);

    impl AceDecode for ModelValue {
        fn ace_decode(decoder: &mut Decoder<'_>) -> Result<Self> {
            u32::ace_decode(decoder).map(Self)
        }
    }

    impl PureValue for ModelValue {}

    #[derive(Debug, Default, PartialEq, Eq)]
    struct ModelPlan {
        values: Vec<Ready<ModelValue>>,
        owned: Vec<ModelPlan>,
        shared: Vec<SharedPlan<ModelPlan>>,
        weak: Vec<WeakPlan>,
    }

    #[derive(Clone, Copy)]
    enum Field {
        Value,
        Owned,
        Strong,
        Weak,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Visit {
        Body(u32, usize),
        Field(u32, u32),
    }

    fn options() -> Options {
        Options {
            packed: false,
            ..Options::default()
        }
    }

    // R2 P3a 坏包辅助：与 schema 的 target_raw 同形，只拼装线上字节，不调用编码器。
    fn r2_rec(id: i32, body: &[u8]) -> Vec<u8> {
        [
            id.to_le_bytes().as_slice(),
            (body.len() as i32).to_le_bytes().as_slice(),
            body,
        ]
        .concat()
    }
    fn r2_slot(depth: i32, tag: i32, payload: [u8; 8]) -> Vec<u8> {
        [
            depth.to_le_bytes().as_slice(),
            tag.to_le_bytes().as_slice(),
            payload.as_slice(),
        ]
        .concat()
    }
    fn r2_raw(main: &[u8], assist: &[u8]) -> Vec<u8> {
        [
            (main.len() as i32).to_le_bytes().as_slice(),
            main,
            (assist.len() as i32).to_le_bytes().as_slice(),
            assist,
        ]
        .concat()
    }
    /// 调用前后的解码器状态快照：位置、记录视图数、元素深度与集合深度必须恢复。
    fn r2_snapshot(ctx: &DecodeCtx<'_, '_>) -> (u8, usize, usize, usize) {
        let position = match ctx.position {
            PlanPosition::Slot => 0,
            PlanPosition::Element => 1,
            PlanPosition::Target(_) => 2,
        };
        (
            position,
            ctx.decoder.record_views.len(),
            ctx.decoder.element_depth,
            ctx.decoder.coll_depth,
        )
    }

    // 对象体仍是线上 SlotRange；只在测试内声明字段脚本，不从 body 猜引用。
    fn message(root: u64, objects: &[(u32, i32, &[u64])]) -> Vec<u8> {
        let mut main = Vec::new();
        let mut slots = Vec::new();
        let mut slot = |depth: i32, tag: i32, payload: u64| {
            slots.extend(depth.to_le_bytes());
            slots.extend(tag.to_le_bytes());
            slots.extend(payload.to_le_bytes());
        };
        slot(0, 0, root);
        let mut offset = 1u32;
        for &(id, depth, fields) in objects {
            main.extend(id.to_le_bytes());
            main.extend(8i32.to_le_bytes());
            main.extend(offset.to_le_bytes());
            main.extend((fields.len() as u32).to_le_bytes());
            for (order, &payload) in fields.iter().enumerate() {
                slot(depth, order as i32 + 1, payload);
            }
            offset += fields.len() as u32;
        }
        [
            (main.len() as i32).to_le_bytes().as_slice(),
            &main,
            (slots.len() as i32).to_le_bytes().as_slice(),
            &slots,
        ]
        .concat()
    }

    fn shared<T: 'static>(id: u64, strength: Strength, purity: Purity) -> ReferenceDescriptor {
        ReferenceDescriptor {
            wire_id: id,
            layout: RecordLayout::Object,
            mode: ReferenceMode::Shared {
                node: NodeType {
                    target: core::any::TypeId::of::<T>(),
                    family: Family::Rc,
                },
                strength,
                payload_purity: purity,
            },
        }
    }

    #[cfg(feature = "std")]
    fn shared_arc<T: 'static>(id: u64, strength: Strength) -> ReferenceDescriptor {
        ReferenceDescriptor {
            wire_id: id,
            layout: RecordLayout::Object,
            mode: ReferenceMode::Shared {
                node: NodeType {
                    target: core::any::TypeId::of::<T>(),
                    family: Family::Arc,
                },
                strength,
                payload_purity: Purity::Pure,
            },
        }
    }

    fn owned(id: u64) -> ReferenceDescriptor {
        ReferenceDescriptor {
            wire_id: id,
            layout: RecordLayout::Object,
            mode: ReferenceMode::Owned {
                purity: Purity::Graph,
            },
        }
    }

    fn model(
        ctx: &mut DecodeCtx,
        id: u32,
        schema: &[(u32, &[Field])],
        purity: Purity,
        visits: &mut Vec<Visit>,
    ) -> Result<ModelPlan> {
        ctx.decoder.enter_object(u64::from(id))?;
        visits.push(Visit::Body(id, ctx.decoder.open() - 1));
        let parsed = (|| {
            let mut plan = ModelPlan::default();
            let fields = schema.iter().find(|(target, _)| *target == id).unwrap().1;
            for (index, field) in fields.iter().enumerate() {
                let order = index as u32 + 1;
                visits.push(Visit::Field(id, order));
                ctx.decoder.begin_field(order)?;
                if matches!(field, Field::Value) {
                    plan.values.push(Ready::decode(ctx.decoder)?);
                    continue;
                }
                let target = u64::from_le_bytes(ctx.decoder.decode_required_scalar()?);
                match field {
                    Field::Owned => {
                        plan.owned.push(ctx.plan_owned(owned(target), |c, id| {
                            model(c, id, schema, purity, visits)
                        })?)
                    }
                    Field::Strong => plan.shared.push(ctx.resolve_strong(
                        shared::<ModelNode>(target, Strength::Strong, purity),
                        |c, id| model(c, id, schema, purity, visits),
                    )?),
                    Field::Weak => plan.weak.push(ctx.resolve_weak(shared::<ModelNode>(
                        target,
                        Strength::Weak,
                        purity,
                    ))?),
                    Field::Value => unreachable!("pure field was decoded above"),
                }
            }
            Ok(plan)
        })();
        let left = ctx.decoder.leave_object();
        parsed.and_then(|plan| left.map(|()| plan))
    }

    fn plan_model(
        raw: &[u8],
        opts: &Options,
        identity: bool,
        schema: &[(u32, &[Field])],
        purity: Purity,
        visits: &mut Vec<Visit>,
    ) -> Result<DecodePlan<ModelPlan>> {
        // 将根的类型化 SharedPlan 保留在 ModelPlan 内；RootPlan 不接受外填 id。
        decode_plan(Decoder::new(raw, opts)?, |c| {
            let id = u64::from_le_bytes(c.decoder.decode_required_scalar()?);
            if identity {
                let root = RootPlan::identity(c.resolve_strong(
                    shared::<ModelNode>(id, Strength::Strong, purity),
                    |c, id| model(c, id, schema, purity, visits),
                )?);
                Ok(RootPlan {
                    root: root.root,
                    value: ModelPlan {
                        shared: vec![root.value],
                        ..ModelPlan::default()
                    },
                })
            } else {
                c.plan_owned(owned(id), |c, id| model(c, id, schema, purity, visits))
                    .map(RootPlan::value)
            }
        })
    }

    fn take_definition<P>(plan: &mut SharedPlan<P>) -> Option<(u32, P)> {
        let id = plan.id();
        match core::mem::replace(plan, SharedPlan::Reference { id }) {
            SharedPlan::Definition { id, body } => Some((id, body)),
            SharedPlan::Reference { .. } => None,
        }
    }

    fn prepare_self_weak<'job>(
        mut plan: SharedPlan<ModelPlan>,
        constructed: &'job Cell<usize>,
        dropped: &Rc<Cell<usize>>,
    ) -> (SharedPlan<ModelPlan>, BTreeMap<u32, PreparedJob<'job>>) {
        let (id, body) = take_definition(&mut plan).expect("self-weak root definition");
        let dropped = Rc::clone(dropped);
        let job = make_rc_job(id, body, move |ctx, body| {
            assert_eq!(body.weak, vec![WeakPlan { id }]);
            let own = ctx
                .weak::<ModelNode>(body.weak[0].id)
                .expect("planned handle");
            assert!(own.upgrade().is_none(), "Open Weak cannot yet upgrade");
            constructed.set(constructed.get() + 1);
            ModelNode { own, dropped }
        });
        let mut jobs = BTreeMap::new();
        assert!(jobs.insert(id, job).is_none());
        (plan, jobs)
    }

    #[test]
    fn ac_g01_rc_self_weak_identity() {
        let raw = message(1, &[(1, 1, &[1])]);
        let constructed = Cell::new(0);
        let dropped = Rc::new(Cell::new(0));
        let result = decode_and_build(
            Decoder::new(&raw, &options()).unwrap(),
            |c| {
                let id = u64::from_le_bytes(c.decoder.decode_required_scalar()?);
                c.resolve_strong(
                    shared::<ModelNode>(id, Strength::Strong, Purity::Graph),
                    |c, id| {
                        model(
                            c,
                            id,
                            &[(1, &[Field::Weak])],
                            Purity::Graph,
                            &mut Vec::new(),
                        )
                    },
                )
                .map(RootPlan::identity)
            },
            |plan| prepare_self_weak(plan, &constructed, &dropped),
            |ctx, plan| ctx.strong::<ModelNode>(plan.id()).expect("planned handle"),
        );
        assert!(
            result.is_ok(),
            "self-weak construction failed: {:?}",
            result.as_ref().err()
        );
        let root = result.unwrap();
        assert_eq!(constructed.get(), 1);
        let upgraded = root.own.upgrade().expect("completed self Weak");
        assert!(Rc::ptr_eq(&root, &upgraded));
    }

    struct BackRoot {
        child: Rc<BackChild>,
    }

    struct BackChild {
        root: Weak<BackRoot>,
    }

    #[test]
    fn ac_g01_heterogeneous_weak_back_to_root() {
        let raw = message(1, &[(1, 1, &[2]), (2, 2, &[1])]);
        let roots = Cell::new(0);
        let children = Cell::new(0);
        let result = decode_and_build(
            Decoder::new(&raw, &options()).unwrap(),
            |c| {
                let id = u64::from_le_bytes(c.decoder.decode_required_scalar()?);
                c.resolve_strong(
                    shared::<BackRoot>(id, Strength::Strong, Purity::Graph),
                    |c, id| {
                        c.decoder.enter_object(u64::from(id))?;
                        let target = u64::from_le_bytes(c.decoder.read_field_scalar(1)?);
                        let child = c.resolve_strong(
                            shared::<BackChild>(target, Strength::Strong, Purity::Graph),
                            |c, id| {
                                c.decoder.enter_object(u64::from(id))?;
                                let target = u64::from_le_bytes(c.decoder.read_field_scalar(1)?);
                                let weak = c.resolve_weak(shared::<BackRoot>(
                                    target,
                                    Strength::Weak,
                                    Purity::Graph,
                                ))?;
                                c.decoder.leave_object()?;
                                Ok(weak)
                            },
                        )?;
                        c.decoder.leave_object()?;
                        Ok(child)
                    },
                )
                .map(RootPlan::identity)
            },
            |mut plan| {
                let (id, mut body) = take_definition(&mut plan).expect("root definition");
                let (child_id, child_body) = take_definition(&mut body).expect("child definition");
                let mut jobs = BTreeMap::new();
                assert!(jobs
                    .insert(
                        child_id,
                        make_rc_job(child_id, child_body, |ctx, body| {
                            children.set(children.get() + 1);
                            BackChild {
                                root: ctx.weak::<BackRoot>(body.id).expect("planned handle"),
                            }
                        })
                    )
                    .is_none());
                assert!(jobs
                    .insert(
                        id,
                        make_rc_job(id, body, |ctx, body| {
                            roots.set(roots.get() + 1);
                            BackRoot {
                                child: ctx.strong::<BackChild>(body.id()).expect("planned handle"),
                            }
                        })
                    )
                    .is_none());
                (plan, jobs)
            },
            |ctx, plan| ctx.strong::<BackRoot>(plan.id()).expect("planned handle"),
        );
        assert!(
            result.is_ok(),
            "heterogeneous construction failed: {:?}",
            result.as_ref().err()
        );
        let root = result.unwrap();
        let upgraded = root.child.root.upgrade().expect("completed root Weak");
        assert!(Rc::ptr_eq(&root, &upgraded));
        assert_eq!((roots.get(), children.get()), (1, 1));
    }

    struct BranchNode {
        leaves: Vec<Rc<ModelValue>>,
    }

    fn plan_branch(c: &mut DecodeCtx, id: u32) -> Result<Vec<SharedPlan<Ready<ModelValue>>>> {
        c.decoder.enter_object(u64::from(id))?;
        let parsed = (|| {
            let mut leaves = Vec::new();
            for order in 1..=if id == 1 { 1 } else { 2 } {
                let target = u64::from_le_bytes(c.decoder.read_field_scalar(order)?);
                leaves.push(c.resolve_strong(
                    ReferenceDescriptor {
                        layout: RecordLayout::Leaf,
                        ..shared::<ModelValue>(target, Strength::Strong, Purity::Pure)
                    },
                    |c, id| {
                        let record = c.decoder.record(u64::from(id).to_le_bytes())?;
                        let bytes = record
                            .as_bytes()
                            .try_into()
                            .map_err(|_| Error::TruncatedInput)?;
                        Ok(Ready(ModelValue(u32::from_le_bytes(bytes))))
                    },
                )?);
            }
            Ok(leaves)
        })();
        let left = c.decoder.leave_object();
        parsed.and_then(|plan| left.map(|()| plan))
    }

    fn prepare_branch<'job>(
        plan: &mut SharedPlan<Vec<SharedPlan<Ready<ModelValue>>>>,
        jobs: &mut BTreeMap<u32, PreparedJob<'job>>,
        next_builder: &'job Cell<usize>,
    ) {
        let Some((id, mut body)) = take_definition(plan) else {
            return;
        };
        for leaf in body.iter_mut() {
            if let Some((leaf_id, leaf_body)) = take_definition(leaf) {
                assert!(jobs
                    .insert(
                        leaf_id,
                        make_rc_job(leaf_id, leaf_body, move |_, Ready(value)| {
                            assert_eq!(next_builder.get(), if leaf_id == 2 { 0 } else { 1 });
                            next_builder.set(next_builder.get() + 1);
                            value
                        })
                    )
                    .is_none());
            }
        }
        assert!(jobs
            .insert(
                id,
                make_rc_job(id, body, move |ctx, body| {
                    assert_eq!(next_builder.get(), if id == 3 { 2 } else { 3 });
                    next_builder.set(next_builder.get() + 1);
                    BranchNode {
                        leaves: body
                            .into_iter()
                            .map(|leaf| {
                                ctx.strong::<ModelValue>(leaf.id()).expect("planned handle")
                            })
                            .collect(),
                    }
                })
            )
            .is_none());
    }

    #[test]
    fn ac_g01_shared_diamond_and_distinct_equal() {
        // 沿用对象 SlotRange 模型；另加真实叶记录与 count + id 的集合记录。
        let objects = message(5, &[(1, 2, &[2]), (3, 2, &[2, 4])]);
        let main_len = i32::from_le_bytes(objects[..4].try_into().unwrap()) as usize;
        let mut main = objects[4..4 + main_len].to_vec();
        let leaf = 42u32.to_le_bytes();
        let collection = [2u32, 1, 3]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        for (id, body) in [
            (2u32, leaf.as_slice()),
            (4, leaf.as_slice()),
            (5, collection.as_slice()),
        ] {
            main.extend(id.to_le_bytes());
            main.extend((body.len() as i32).to_le_bytes());
            main.extend(body);
        }
        let raw = [
            (main.len() as i32).to_le_bytes().as_slice(),
            &main,
            &objects[4 + main_len..],
        ]
        .concat();
        let next_builder = Cell::new(0);
        let result = decode_and_build(
            Decoder::new(&raw, &options()).unwrap(),
            |c| {
                let id = u64::from_le_bytes(c.decoder.decode_required_scalar()?);
                let branches = c.plan_owned(
                    ReferenceDescriptor {
                        layout: RecordLayout::Collection,
                        ..owned(id)
                    },
                    |c, id| {
                        c.decoder.enter_collection()?;
                        c.decoder.begin_record_view(id)?;
                        let parsed = (|| {
                            let count = c.decoder.read_element_id()? as usize;
                            c.decoder.check_collection_len(count)?;
                            let mut branches = Vec::new();
                            for _ in 0..count {
                                let target = u64::from(c.decoder.read_element_id()?);
                                branches.push(c.resolve_strong(
                                    shared::<BranchNode>(target, Strength::Strong, Purity::Graph),
                                    plan_branch,
                                )?);
                            }
                            assert!(c.decoder.peek_body()?.is_empty());
                            Ok(branches)
                        })();
                        c.decoder.end_record_view();
                        c.decoder.leave_collection();
                        parsed
                    },
                )?;
                assert_eq!(c.strong_discovery, vec![1, 2, 3, 4]);
                let (strong, weak) = c.schedule_edges();
                assert_eq!(
                    plan_build_schedule(&c.strong_discovery, &strong, &weak, 0)?.events,
                    vec![
                        Event::Push(1),
                        Event::Push(3),
                        Event::Push(2),
                        Event::Pop(2),
                        Event::Push(4),
                        Event::Pop(4),
                        Event::Pop(3),
                        Event::Pop(1)
                    ]
                );
                Ok(RootPlan::value(branches))
            },
            |mut branches| {
                let mut jobs = BTreeMap::new();
                for branch in &mut branches {
                    prepare_branch(branch, &mut jobs, &next_builder);
                }
                (branches, jobs)
            },
            |ctx, branches| {
                branches
                    .into_iter()
                    .map(|branch| {
                        ctx.strong::<BranchNode>(branch.id())
                            .expect("planned handle")
                    })
                    .collect::<Vec<_>>()
            },
        );
        assert!(
            result.is_ok(),
            "diamond construction failed: {:?}",
            result.as_ref().err()
        );
        let branches = result.unwrap();
        assert_eq!(
            next_builder.get(),
            4,
            "builder order must be Z,W,B,A exactly once"
        );
        assert!(Rc::ptr_eq(&branches[0].leaves[0], &branches[1].leaves[0]));
        assert_eq!(branches[1].leaves[0], branches[1].leaves[1]);
        assert!(!Rc::ptr_eq(&branches[1].leaves[0], &branches[1].leaves[1]));
    }

    #[test]
    fn fr_t06_open_weak_then_upgrade_and_drop() {
        let raw = message(1, &[(1, 1, &[2, 2]), (2, 2, &[2])]);
        let constructed = Cell::new(0);
        let dropped = Rc::new(Cell::new(0));
        let result = decode_and_build(
            Decoder::new(&raw, &options()).unwrap(),
            |c| {
                let id = u64::from_le_bytes(c.decoder.decode_required_scalar()?);
                c.plan_owned(owned(id), |c, id| {
                    model(
                        c,
                        id,
                        &[(1, &[Field::Strong, Field::Weak]), (2, &[Field::Weak])],
                        Purity::Graph,
                        &mut Vec::new(),
                    )
                })
                .map(RootPlan::value)
            },
            |mut plan| {
                let (shared, jobs) = prepare_self_weak(
                    plan.shared.pop().expect("strong root field"),
                    &constructed,
                    &dropped,
                );
                plan.shared.push(shared);
                (plan, jobs)
            },
            |ctx, plan| {
                (
                    ctx.strong::<ModelNode>(plan.shared[0].id())
                        .expect("planned handle"),
                    ctx.weak::<ModelNode>(plan.weak[0].id)
                        .expect("planned handle"),
                )
            },
        );
        assert!(
            result.is_ok(),
            "lifecycle construction failed: {:?}",
            result.as_ref().err()
        );
        let (root, observer) = result.unwrap();
        assert_eq!((constructed.get(), dropped.get()), (1, 0));
        {
            let upgraded = observer.upgrade().expect("completed ordinary-root Weak");
            let own = root.own.upgrade().expect("completed self Weak");
            assert!(Rc::ptr_eq(&root, &upgraded));
            assert!(Rc::ptr_eq(&root, &own));
        }
        drop(root);
        assert!(observer.upgrade().is_none());
        assert_eq!((constructed.get(), dropped.get()), (1, 1));
    }

    #[test]
    fn ac_g01_phase1_error_constructs_no_graph_nodes() {
        let raw = message(1, &[(1, 1, &[1])]);
        let prepared = Cell::new(false);
        let constructed = Cell::new(0);
        let dropped = Rc::new(Cell::new(0));
        let result = decode_and_build(
            Decoder::new(&raw, &options()).unwrap(),
            |c| {
                let id = u64::from_le_bytes(c.decoder.decode_required_scalar()?);
                c.resolve_strong(
                    shared::<ModelNode>(id, Strength::Strong, Purity::Graph),
                    |c, id| {
                        model(
                            c,
                            id,
                            &[(1, &[Field::Strong])],
                            Purity::Graph,
                            &mut Vec::new(),
                        )
                    },
                )
                .map(RootPlan::identity)
            },
            |plan| {
                prepared.set(true);
                prepare_self_weak(plan, &constructed, &dropped)
            },
            |ctx, plan| ctx.strong::<ModelNode>(plan.id()).expect("planned handle"),
        );
        let error = result.expect_err("phase one must reject a strong self back edge");
        assert_eq!(error, Error::CycleUnsupported);
        assert_eq!(error.code(), 14);
        assert!(!prepared.get());
        assert_eq!((constructed.get(), dropped.get()), (0, 0));
    }

    fn assert_error<P: core::fmt::Debug>(result: Result<P>, expected: Error, code: i32) {
        let error = result.unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(error.code(), code);
    }

    #[test]
    fn bad09_prior_noncycle_depth_error_is_5() {
        let raw = message(1, &[(1, 1, &[2, 1]), (2, 2, &[])]);
        let mut visits = Vec::new();
        assert_error(
            plan_model(
                &raw,
                &Options {
                    max_depth: 0,
                    ..options()
                },
                true,
                &[(1, &[Field::Owned, Field::Strong]), (2, &[])],
                Purity::Graph,
                &mut visits,
            ),
            Error::DepthExceeded,
            5,
        );
        assert_eq!(visits, vec![Visit::Body(1, 0), Visit::Field(1, 1)]);
    }

    #[test]
    fn bad09_reached_strong_back_edge_is_14() {
        let raw = message(1, &[(1, 1, &[1])]);
        let mut visits = Vec::new();
        assert_error(
            plan_model(
                &raw,
                &Options {
                    max_depth: 0,
                    max_build_nesting: 0,
                    ..options()
                },
                true,
                &[(1, &[Field::Strong])],
                Purity::Graph,
                &mut visits,
            ),
            Error::CycleUnsupported,
            14,
        );
        assert_eq!(visits, vec![Visit::Body(1, 0), Visit::Field(1, 1)]);
    }

    #[test]
    fn bad04_shared_id_type_or_family_conflict_is_7() {
        let raw = message(1, &[(1, 1, &[2, 2]), (2, 2, &[42])]);
        let mut parsed_conflicting_body = false;
        let result = decode_plan(Decoder::new(&raw, &options()).unwrap(), |c| {
            let id = u64::from_le_bytes(c.decoder.decode_required_scalar()?);
            c.decoder.enter_object(id)?;
            let target = u64::from_le_bytes(c.decoder.read_field_scalar(1)?);
            let weak = c.resolve_weak(shared::<ModelNode>(target, Strength::Weak, Purity::Pure))?;
            assert_eq!(weak.id, 2);
            assert!(
                matches!(c.entries[&2].state, EntryState::Reserved),
                "weak target stays reserved without a strong request"
            );
            let target = u64::from_le_bytes(c.decoder.read_field_scalar(2)?);
            c.resolve_strong(
                shared::<OtherModelNode>(target, Strength::Strong, Purity::Pure),
                |_, _| {
                    parsed_conflicting_body = true;
                    Ok(())
                },
            )?;
            c.decoder.leave_object()?;
            Ok(RootPlan::value(()))
        });
        assert_error(result, Error::InvalidReference, 7);
        assert!(!parsed_conflicting_body);
        #[cfg(feature = "std")]
        {
            // 同完整目标的 Rc/Arc 族冲突，两个方向都必须在解析冲突体前返回 7。
            let mut conflicting_body = false;
            let result = decode_plan(Decoder::new(&raw, &options()).unwrap(), |c| {
                c.resolve_strong(
                    shared::<ModelNode>(2, Strength::Strong, Purity::Pure),
                    |_, _| Ok(()),
                )?;
                c.resolve_strong(shared_arc::<ModelNode>(2, Strength::Strong), |_, _| {
                    conflicting_body = true;
                    Ok(())
                })?;
                Ok(RootPlan::value(()))
            });
            assert_error(result, Error::InvalidReference, 7);
            assert!(!conflicting_body);
            conflicting_body = false;
            let result = decode_plan(Decoder::new(&raw, &options()).unwrap(), |c| {
                c.resolve_strong(shared_arc::<ModelNode>(2, Strength::Strong), |_, _| Ok(()))?;
                c.resolve_strong(
                    shared::<ModelNode>(2, Strength::Strong, Purity::Pure),
                    |_, _| {
                        conflicting_body = true;
                        Ok(())
                    },
                )?;
                Ok(RootPlan::value(()))
            });
            assert_error(result, Error::InvalidReference, 7);
            assert!(!conflicting_body);
            // Reserved Weak 的族冲突：Rc 预留后 Arc 强定义，或 Arc 预留后 Rc 强定义。
            conflicting_body = false;
            let result = decode_plan(Decoder::new(&raw, &options()).unwrap(), |c| {
                c.resolve_weak(shared::<ModelNode>(2, Strength::Weak, Purity::Pure))?;
                c.resolve_strong(shared_arc::<ModelNode>(2, Strength::Strong), |_, _| {
                    conflicting_body = true;
                    Ok(())
                })?;
                Ok(RootPlan::value(()))
            });
            assert_error(result, Error::InvalidReference, 7);
            assert!(!conflicting_body);
            conflicting_body = false;
            let result = decode_plan(Decoder::new(&raw, &options()).unwrap(), |c| {
                c.resolve_weak(shared_arc::<ModelNode>(2, Strength::Weak))?;
                c.resolve_strong(
                    shared::<ModelNode>(2, Strength::Strong, Purity::Pure),
                    |_, _| {
                        conflicting_body = true;
                        Ok(())
                    },
                )?;
                Ok(RootPlan::value(()))
            });
            assert_error(result, Error::InvalidReference, 7);
            assert!(!conflicting_body);
            // 同为 Arc 时，T 与 RwLock<T> 是不同的完整目标类型。
            conflicting_body = false;
            let result = decode_plan(Decoder::new(&raw, &options()).unwrap(), |c| {
                c.resolve_strong(shared_arc::<ModelNode>(2, Strength::Strong), |_, _| Ok(()))?;
                c.resolve_strong(
                    shared_arc::<std::sync::RwLock<ModelNode>>(2, Strength::Strong),
                    |_, _| {
                        conflicting_body = true;
                        Ok(())
                    },
                )?;
                Ok(RootPlan::value(()))
            });
            assert_error(result, Error::InvalidReference, 7);
            assert!(!conflicting_body);
            // Active 的 Arc 身份遇到 Rc 强回边：先报族冲突 7，不得报同族强回边 14。
            conflicting_body = false;
            let result = decode_plan(Decoder::new(&raw, &options()).unwrap(), |c| {
                c.resolve_strong(shared_arc::<ModelNode>(2, Strength::Strong), |c, _| {
                    c.resolve_strong(
                        shared::<ModelNode>(2, Strength::Strong, Purity::Pure),
                        |_, _| {
                            conflicting_body = true;
                            Ok(())
                        },
                    )?;
                    Ok(())
                })?;
                Ok(RootPlan::value(()))
            });
            assert_error(result, Error::InvalidReference, 7);
            assert!(!conflicting_body);
        }
    }

    #[test]
    fn fr_t06_missing_strong_target_is_7() {
        let raw = message(1, &[(1, 1, &[2]), (2, 2, &[42])]);
        let mut visits = Vec::new();
        assert_error(
            plan_model(
                &raw,
                &options(),
                false,
                &[(1, &[Field::Weak]), (2, &[Field::Value])],
                Purity::Pure,
                &mut visits,
            ),
            Error::InvalidReference,
            7,
        );
        assert_eq!(visits, vec![Visit::Body(1, 0), Visit::Field(1, 1)]);
        for target in [0, 3, i32::MAX as u64 + 1, (1u64 << 32) + 2] {
            for field in [Field::Strong, Field::Weak] {
                let raw = message(1, &[(1, 1, &[target]), (2, 2, &[42])]);
                let mut visits = Vec::new();
                assert_error(
                    plan_model(
                        &raw,
                        &options(),
                        false,
                        &[(1, &[field]), (2, &[Field::Value])],
                        Purity::Pure,
                        &mut visits,
                    ),
                    Error::InvalidReference,
                    7,
                );
                assert_eq!(visits, vec![Visit::Body(1, 0), Visit::Field(1, 1)]);
            }
        }
    }

    #[test]
    fn fr_e05_shared_once_owned_each_materialization() {
        let raw = message(1, &[(1, 1, &[2, 2, 2, 2, 2]), (2, 2, &[42])]);
        let decoder = Decoder::new(&raw, &options()).unwrap();
        let r = decoder.index.get(1).unwrap().size as usize;
        let s = decoder.index.get(2).unwrap().size as usize;
        let budget = r + 3 * s;
        let schema: &[(u32, &[Field])] = &[
            (
                1,
                &[
                    Field::Strong,
                    Field::Strong,
                    Field::Weak,
                    Field::Owned,
                    Field::Owned,
                ],
            ),
            (2, &[Field::Value]),
        ];
        let mut visits = Vec::new();
        let plan = plan_model(
            &raw,
            &Options {
                max_decoded_bytes: budget,
                ..options()
            },
            false,
            schema,
            Purity::Pure,
            &mut visits,
        )
        .unwrap();
        assert_eq!(plan.materialized_bytes, budget);
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.root, 0);
        assert_eq!(plan.value.shared.len(), 2);
        let SharedPlan::Definition { id, body } = &plan.value.shared[0] else {
            panic!("first strong must define")
        };
        assert_eq!(*id, 2);
        assert_eq!(body.values, vec![Ready(ModelValue(42))]);
        assert_eq!(plan.value.shared[1], SharedPlan::Reference { id: 2 });
        assert_eq!(plan.value.weak, vec![WeakPlan { id: 2 }]);
        assert_eq!(plan.value.owned.len(), 2);
        for owned in &plan.value.owned {
            assert_eq!(owned.values, vec![Ready(ModelValue(42))]);
        }
        assert_eq!(
            visits
                .iter()
                .filter(|visit| matches!(visit, Visit::Body(2, _)))
                .collect::<Vec<_>>(),
            vec![&Visit::Body(2, 1); 3]
        );
        assert_error(
            plan_model(
                &raw,
                &Options {
                    max_decoded_bytes: budget - 1,
                    ..options()
                },
                false,
                schema,
                Purity::Pure,
                &mut Vec::new(),
            ),
            Error::ResourceLimit,
            6,
        );
    }

    #[test]
    fn ac_e02_graph_reference_entries_boundary() {
        let raw = message(
            1,
            &[
                (1, 1, &[2, 2, 2, 3, 4]),
                (2, 2, &[42]),
                (3, 2, &[43]),
                (4, 2, &[44]),
            ],
        );
        let schema: &[(u32, &[Field])] = &[
            (
                1,
                &[
                    Field::Weak,
                    Field::Strong,
                    Field::Strong,
                    Field::Owned,
                    Field::Strong,
                ],
            ),
            (2, &[Field::Value]),
            (3, &[Field::Value]),
            (4, &[Field::Value]),
        ];
        let plan = plan_model(
            &raw,
            &Options {
                max_references: 2,
                ..options()
            },
            false,
            schema,
            Purity::Pure,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(plan.nodes.keys().copied().collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(plan.value.shared[1], SharedPlan::Reference { id: 2 });
        assert_eq!(plan.value.owned[0].values, vec![Ready(ModelValue(43))]);
        for cap in [1, 0] {
            let mut visits = Vec::new();
            assert_error(
                plan_model(
                    &raw,
                    &Options {
                        max_references: cap,
                        ..options()
                    },
                    false,
                    schema,
                    Purity::Pure,
                    &mut visits,
                ),
                Error::ResourceLimit,
                6,
            );
            assert!(!visits
                .iter()
                .any(|visit| matches!(visit, Visit::Body(4, _))));
            assert_eq!(
                visits
                    .iter()
                    .any(|visit| matches!(visit, Visit::Body(3, _))),
                cap == 1
            );
        }
        let raw = message(1, &[(1, 1, &[2]), (2, 2, &[42])]);
        let plan = plan_model(
            &raw,
            &Options {
                max_references: 0,
                ..options()
            },
            false,
            &[(1, &[Field::Owned]), (2, &[Field::Value])],
            Purity::Pure,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(plan.nodes.is_empty());
        assert_eq!(plan.value.owned[0].values, vec![Ready(ModelValue(42))]);
    }

    #[test]
    fn ac_e02_build_nesting_budget_boundary() {
        let raw = message(
            1,
            &[(1, 1, &[2]), (2, 2, &[3, 4]), (3, 3, &[2]), (4, 3, &[2])],
        );
        let schema: &[(u32, &[Field])] = &[
            (1, &[Field::Strong]),
            (2, &[Field::Strong, Field::Strong]),
            (3, &[Field::Weak]),
            (4, &[Field::Weak]),
        ];
        let plan = plan_model(
            &raw,
            &Options {
                max_build_nesting: 2,
                ..options()
            },
            false,
            schema,
            Purity::Graph,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(plan.root, 0);
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.schedule.peak, 2);
        assert_eq!(
            plan.schedule.events,
            vec![
                Event::Push(2),
                Event::Push(3),
                Event::Pop(3),
                Event::Push(4),
                Event::Pop(4),
                Event::Pop(2)
            ]
        );
        assert_error(
            plan_model(
                &raw,
                &Options {
                    max_build_nesting: 1,
                    ..options()
                },
                false,
                schema,
                Purity::Graph,
                &mut Vec::new(),
            ),
            Error::ResourceLimit,
            6,
        );
        let raw = message(1, &[(1, 1, &[1])]);
        let plan = plan_model(
            &raw,
            &Options {
                max_build_nesting: 1,
                ..options()
            },
            true,
            &[(1, &[Field::Weak])],
            Purity::Graph,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(plan.root, 1);
        assert_eq!(plan.schedule.peak, 1);
        assert_error(
            plan_model(
                &raw,
                &Options {
                    max_build_nesting: 0,
                    ..options()
                },
                true,
                &[(1, &[Field::Weak])],
                Purity::Graph,
                &mut Vec::new(),
            ),
            Error::ResourceLimit,
            6,
        );
        let raw = message(42, &[]);
        let plan = decode_plan(
            Decoder::new(
                &raw,
                &Options {
                    max_build_nesting: 0,
                    ..options()
                },
            )
            .unwrap(),
            |c| Ready::<ModelValue>::decode(c.decoder).map(RootPlan::value),
        )
        .unwrap();
        assert_eq!(plan.value, Ready(ModelValue(42)));
        assert_eq!(plan.root, 0);
        assert!(plan.nodes.is_empty());
        assert!(plan.schedule.events.is_empty());
        assert_eq!(plan.schedule.peak, 0);
    }

    fn edges(entries: &[(u32, &[u32])]) -> BTreeMap<u32, Vec<u32>> {
        entries
            .iter()
            .map(|&(id, targets)| (id, targets.to_vec()))
            .collect()
    }

    fn check_schedule(
        ids: &[u32],
        strong: &BTreeMap<u32, Vec<u32>>,
        weak: &BTreeMap<u32, Vec<u32>>,
        events: &[Event],
    ) -> usize {
        let nodes: BTreeSet<_> = ids.iter().copied().collect();
        assert_eq!(nodes.len(), ids.len());
        assert_eq!(events.len(), 2 * ids.len());
        let mut opened = Vec::new();
        let mut pushes = BTreeMap::new();
        let mut pops = BTreeMap::new();
        let mut peak = 0;
        for (position, &event) in events.iter().enumerate() {
            match event {
                Event::Push(id) => {
                    assert!(nodes.contains(&id), "unknown push: {id}");
                    assert_eq!(pushes.insert(id, position), None, "duplicate push: {id}");
                    opened.push(id);
                    peak = peak.max(opened.len());
                }
                Event::Pop(id) => {
                    assert_eq!(opened.pop(), Some(id), "non-LIFO pop: {id}");
                    assert_eq!(pops.insert(id, position), None, "duplicate pop: {id}");
                }
            }
        }
        assert!(opened.is_empty());
        assert_eq!(pushes.len(), nodes.len());
        assert_eq!(pops.len(), nodes.len());
        for id in nodes {
            assert!(pushes[&id] < pops[&id]);
        }
        // 0 是仅有强拥有关系的分析虚根，不产生 push/pop。
        for (&owner, targets) in strong {
            if owner != 0 {
                for target in targets {
                    assert!(pops[target] < pops[&owner], "strong: {owner} -> {target}");
                }
            }
        }
        for (&owner, targets) in weak {
            if owner != 0 {
                for target in targets {
                    assert!(pushes[target] < pops[&owner], "weak: {owner} -> {target}");
                }
            }
        }
        peak
    }

    #[test]
    fn ac_g01_schedule_self_weak() {
        let strong = edges(&[]);
        let weak = edges(&[(1, &[1])]);
        let schedule = plan_build_schedule(&[1], &strong, &weak, 1).unwrap();
        assert_eq!(schedule.events, vec![Event::Push(1), Event::Pop(1)]);
        assert_eq!(schedule.peak, 1);
        assert_eq!(check_schedule(&[1], &strong, &weak, &schedule.events), 1);

        assert_eq!(
            plan_build_schedule(&[], &strong, &edges(&[]), 1),
            Err(Error::InvalidReference)
        );
        assert_eq!(
            plan_build_schedule(&[1], &edges(&[(1, &[2])]), &edges(&[]), 1),
            Err(Error::InvalidReference)
        );
        assert_eq!(
            plan_build_schedule(&[1], &strong, &edges(&[(1, &[2])]), 1),
            Err(Error::InvalidReference)
        );
        // Weak 不能使一个已定义但无强拥有者的节点变成消息内拥有目标。
        assert_eq!(
            plan_build_schedule(&[1, 2], &strong, &edges(&[(1, &[2])]), 1),
            Err(Error::InvalidReference)
        );
        for ids in [&[0][..], &[1, 1][..]] {
            assert_eq!(
                plan_build_schedule(ids, &strong, &edges(&[]), 1),
                Err(Error::InvalidReference)
            );
        }
        for (strong, weak) in [
            (edges(&[(1, &[0])]), edges(&[])),
            (edges(&[]), edges(&[(1, &[0])])),
        ] {
            assert_eq!(
                plan_build_schedule(&[1], &strong, &weak, 1),
                Err(Error::InvalidReference)
            );
        }
        assert_eq!(Error::InvalidReference.code(), 7);
    }

    #[test]
    fn ac_g01_schedule_back_edge_to_root() {
        let ids = [9, 2];
        let strong = edges(&[(9, &[2])]);
        let weak = edges(&[(2, &[9])]);
        let schedule = plan_build_schedule(&ids, &strong, &weak, 9).unwrap();
        assert_eq!(
            schedule.events,
            vec![Event::Push(9), Event::Push(2), Event::Pop(2), Event::Pop(9)]
        );
        assert_eq!(schedule.peak, 2);
        assert_eq!(check_schedule(&ids, &strong, &weak, &schedule.events), 2);
    }

    #[test]
    fn graph_schedule_parent_tree() {
        let ids = [90, 7, 3];
        let strong = edges(&[(90, &[3, 7])]);
        let weak = edges(&[(7, &[90]), (3, &[90])]);
        let schedule = plan_build_schedule(&ids, &strong, &weak, 90).unwrap();
        assert_eq!(
            schedule.events,
            vec![
                Event::Push(90),
                Event::Push(7),
                Event::Pop(7),
                Event::Push(3),
                Event::Pop(3),
                Event::Pop(90),
            ]
        );
        assert_eq!(schedule.peak, 2);
        assert_eq!(check_schedule(&ids, &strong, &weak, &schedule.events), 2);
    }

    #[test]
    fn graph_schedule_star_is_not_leaf_count_deep() {
        let ids = [8, 5, 4, 3, 2, 1];
        let strong = edges(&[(0, &[8]), (8, &[1, 2, 3, 4, 5])]);
        let weak = edges(&[(5, &[8]), (4, &[8]), (3, &[8]), (2, &[8]), (1, &[8])]);
        let schedule = plan_build_schedule(&ids, &strong, &weak, 0).unwrap();
        assert_eq!(schedule.peak, 2);
        assert_eq!(check_schedule(&ids, &strong, &weak, &schedule.events), 2);
    }

    #[test]
    fn graph_schedule_shared_p_two_weak_owners() {
        let ids = [40, 10, 80, 20];
        let strong = edges(&[(0, &[10, 40]), (40, &[80]), (10, &[80]), (80, &[20])]);
        let weak = edges(&[(20, &[40, 10])]);
        let schedule = plan_build_schedule(&ids, &strong, &weak, 0).unwrap();
        assert!(schedule.peak <= 4);
        assert_eq!(
            check_schedule(&ids, &strong, &weak, &schedule.events),
            schedule.peak
        );
    }

    #[test]
    fn graph_schedule_k_owners_requires_k_open_owners() {
        let ids = [31, 13, 7, 21, 9];
        let strong = edges(&[
            (0, &[7, 13, 31]),
            (31, &[21]),
            (13, &[21]),
            (7, &[21]),
            (21, &[9]),
        ]);
        let weak = edges(&[(9, &[31, 13, 7])]);
        let schedule = plan_build_schedule(&ids, &strong, &weak, 0).unwrap();
        let k = 3;
        assert_eq!(schedule.peak, k + 2);
        assert_eq!(
            check_schedule(&ids, &strong, &weak, &schedule.events),
            k + 2
        );
        // I1：算法峰值不是最小峰值；先完成 q 再打开 p 可达到 K+1。
        let shallower = [
            Event::Push(31),
            Event::Push(13),
            Event::Push(7),
            Event::Push(9),
            Event::Pop(9),
            Event::Push(21),
            Event::Pop(21),
            Event::Pop(7),
            Event::Pop(13),
            Event::Pop(31),
        ];
        assert_eq!(check_schedule(&ids, &strong, &weak, &shallower), k + 1);
    }

    #[test]
    fn ac_g01_schedule_strong_cycle_is_14() {
        let strong = edges(&[(1, &[2]), (2, &[1])]);
        assert_eq!(
            plan_build_schedule(&[1, 2], &strong, &edges(&[]), 1),
            Err(Error::CycleUnsupported)
        );
        assert_eq!(Error::CycleUnsupported.code(), 14);
    }
    extern crate std;
    use crate::{
        decode::{decode_cyclic, BuildCtx as PublicBuildCtx, DecodeCtx as PublicDecodeCtx},
        schema::{AceDecodeCyclic, NonOptional},
    };
    std::thread_local! {
        static ORDINARY_READS: Cell<usize> = const { Cell::new(0) };
        static CYCLIC_PLANS: Cell<usize> = const { Cell::new(0) };
        static BUILDS: Cell<usize> = const { Cell::new(0) };
    }

    fn reset_counts() {
        ORDINARY_READS.set(0);
        CYCLIC_PLANS.set(0);
        BUILDS.set(0);
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Ordinary(bool);
    impl NonOptional for Ordinary {}
    impl AceDecode for Ordinary {
        fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
            ORDINARY_READS.set(ORDINARY_READS.get() + 1);
            bool::ace_decode(d).map(Self)
        }
    }

    struct OrdinaryNode {
        value: Ordinary,
        child: Option<Rc<OrdinaryNode>>,
        back: Option<Weak<OrdinaryNode>>,
    }
    impl AceDecodeCyclic for OrdinaryNode {
        type Plan = (Ordinary, Option<Ordinary>, Option<u32>, Option<u32>);
        fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.ordinary_field::<Ordinary>(1)?,
                    ctx.ordinary_field::<Option<Ordinary>>(2)?,
                    ctx.field::<Option<Rc<Self>>>(3)?,
                    ctx.field::<Option<Weak<Self>>>(4)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
            BUILDS.set(BUILDS.get() + 1);
            assert!(plan.1.is_none());
            Self {
                value: plan.0,
                child: Option::<Rc<Self>>::build_from_plan(ctx, plan.2),
                back: Option::<Weak<Self>>::build_from_plan(ctx, plan.3),
            }
        }
    }

    // 稀疏字段仍复用既有消息夹具；删掉对应槽并调整后续 SlotRange。
    type OrdinaryObject = (u32, i32, bool, Option<u32>, Option<u32>);

    fn ordinary_message(objects: &[OrdinaryObject]) -> Vec<u8> {
        let mut raw = message(1, &[]);
        let mut main = Vec::new();
        let mut slots = raw.split_off(8);
        let mut offset = 1u32;
        for &(id, depth, value, child, back) in objects {
            let fields = [
                (1i32, Some(u64::from(value))),
                (3, child.map(u64::from)),
                (4, back.map(u64::from)),
            ];
            let count = fields.iter().filter(|(_, value)| value.is_some()).count() as u32;
            main.extend(id.to_le_bytes());
            main.extend(8i32.to_le_bytes());
            main.extend(offset.to_le_bytes());
            main.extend(count.to_le_bytes());
            for (tag, payload) in fields {
                if let Some(payload) = payload {
                    slots.extend(depth.to_le_bytes());
                    slots.extend(tag.to_le_bytes());
                    slots.extend(payload.to_le_bytes());
                }
            }
            offset += count;
        }
        [
            (main.len() as i32).to_le_bytes().as_slice(),
            &main,
            (slots.len() as i32).to_le_bytes().as_slice(),
            &slots,
        ]
        .concat()
    }

    #[test]
    fn r_c2_01_build_ctx_option_and_panic_contract() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        let node = NodeType {
            target: core::any::TypeId::of::<u32>(),
            family: Family::Rc,
        };
        let mut ctx = BuildCtx {
            entries: BTreeMap::from([(
                1,
                Entry {
                    node,
                    state: EntryState::Building,
                },
            )]),
        };
        assert!(ctx.weak::<u32>(99).is_none());
        assert!(ctx.strong::<u32>(99).is_none());
        assert!(ctx.weak::<u32>(1).is_none());
        assert!(ctx.strong::<u32>(1).is_none());
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak::<u64>(1))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.strong::<u64>(1))).is_err());
        ctx.entries.get_mut(&1).unwrap().node.family = Family::Arc;
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak::<u32>(1))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.strong::<u32>(1))).is_err());
        ctx.entries.get_mut(&1).unwrap().node.family = Family::Rc;
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.bind_weak::<u32>(99, Weak::new()))).is_err());
        ctx.entries.get_mut(&1).unwrap().state = EntryState::Planned(Some(Box::new(|_, _| {})));
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.bind_weak::<u32>(1, Weak::new()))).is_err());
        ctx.entries.get_mut(&1).unwrap().state = EntryState::Building;
        let value = Rc::new_cyclic(|own: &Weak<u32>| {
            ctx.bind_weak::<u32>(1, own.clone());
            assert!(ctx.weak::<u32>(1).is_some());
            assert!(Weak::ptr_eq(&ctx.weak::<u32>(1).unwrap(), own));
            assert!(ctx.weak::<u32>(1).unwrap().upgrade().is_none());
            assert!(ctx.strong::<u32>(1).is_none());
            assert!(
                catch_unwind(AssertUnwindSafe(|| ctx.bind_weak::<u32>(1, own.clone()))).is_err()
            );
            37
        });
        ctx.complete(1, value.clone());
        assert!(
            ctx.strong::<u32>(1).is_some(),
            "Done returns a cached strong handle"
        );
        assert!(Rc::ptr_eq(&ctx.strong::<u32>(1).unwrap(), &value));
        assert!(Rc::ptr_eq(
            &ctx.weak::<u32>(1).unwrap().upgrade().unwrap(),
            &value
        ));
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak::<u64>(1))).is_err());
        {
            ctx.entries.get_mut(&1).unwrap().node.family = Family::Arc;
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak::<u32>(1))).is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.strong::<u32>(1))).is_err());
            ctx.entries.get_mut(&1).unwrap().node.family = Family::Rc;
        }
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.bind_weak::<u32>(1, Weak::new()))).is_err());
        ctx.entries.get_mut(&1).unwrap().state = EntryState::Done {
            weak: Box::new(Rc::downgrade(&value)),
            strong: Box::new(37u32),
        };
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.strong::<u32>(1))).is_err());
        ctx.entries.get_mut(&1).unwrap().state = EntryState::Open {
            weak: Box::new(37u32),
        };
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak::<u32>(1))).is_err());
    }

    struct FinishProbe;
    impl AceDecodeCyclic for FinishProbe {
        type Plan = ();
        fn decode_plan(_ctx: &mut PublicDecodeCtx) -> Result<()> {
            Ok(())
        }
        fn build_from_plan(_ctx: &mut PublicBuildCtx<'_>, (): ()) -> Self {
            BUILDS.set(BUILDS.get() + 1);
            Self
        }
    }
    #[test]
    fn r_c2_02_phase1_finish_precedes_build() {
        reset_counts();
        let mut raw = message(7, &[]);
        raw[12..16].copy_from_slice(&1i32.to_le_bytes());
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        assert_eq!(
            decode_cyclic::<FinishProbe>(&mut decoder).err(),
            Some(Error::InvalidTag)
        );
        assert_eq!(BUILDS.get(), 0);
        let raw = message(37, &[]);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        assert_eq!(decode_cyclic::<u32>(&mut decoder), Ok(37));
        assert_eq!(decoder.finish(), Ok(()));
    }

    struct PublicNode {
        value: u32,
        back: Option<Weak<PublicNode>>,
    }
    impl AceDecodeCyclic for PublicNode {
        type Plan = (u32, Option<u32>);
        fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| Ok((ctx.field::<u32>(1)?, ctx.field::<Option<Weak<Self>>>(2)?)))
        }
        fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
            BUILDS.set(BUILDS.get() + 1);
            Self {
                value: plan.0,
                back: Option::<Weak<Self>>::build_from_plan(ctx, plan.1),
            }
        }
    }
    struct ManualRoot {
        node: Rc<PublicNode>,
        value: u32,
    }
    impl AceDecodeCyclic for ManualRoot {
        type Plan = (u32, u32);
        fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
            CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
            ctx.object(|ctx| Ok((ctx.field::<Rc<PublicNode>>(1)?, ctx.field::<u32>(2)?)))
        }
        fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
            BUILDS.set(BUILDS.get() + 1);
            Self {
                node: Rc::<PublicNode>::build_from_plan(ctx, plan.0),
                value: plan.1,
            }
        }
    }
    impl AceDecode for ManualRoot {
        fn ace_decode(decoder: &mut Decoder<'_>) -> Result<Self> {
            decode_cyclic::<Self>(decoder)
        }
    }
    #[test]
    fn fr_t10_public_cyclic_manual_root() {
        reset_counts();
        let raw = message(1, &[(1, 1, &[2, 37]), (2, 2, &[99, 2])]);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        let result = ManualRoot::ace_decode(&mut decoder);
        assert!(
            result.is_ok(),
            "public manual root must decode: {:?}",
            result.err()
        );
        let root = result.unwrap();
        assert_eq!((root.value, root.node.value), (37, 99));
        assert!(Rc::ptr_eq(
            &root.node,
            &root.node.back.as_ref().unwrap().upgrade().unwrap()
        ));
        assert_eq!((CYCLIC_PLANS.get(), BUILDS.get()), (1, 2));
        decoder.finish().unwrap();
        reset_counts();
        let raw = message(3, &[(3, 1, &[1]), (1, 2, &[2, 37]), (2, 3, &[99, 2])]);
        struct OrdinaryParent;
        impl AceDecode for OrdinaryParent {
            fn ace_decode(decoder: &mut Decoder<'_>) -> Result<Self> {
                let id = u64::from_le_bytes(decoder.decode_required_scalar()?);
                decoder.enter_object(id)?;
                let parsed = decoder
                    .begin_field(1)
                    .and_then(|_| ManualRoot::ace_decode(decoder));
                let left = decoder.leave_object();
                parsed.and_then(|_| left.map(|()| Self))
            }
        }
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        assert_eq!(
            OrdinaryParent::ace_decode(&mut decoder).err(),
            Some(Error::InvalidArgument)
        );
        assert_eq!((CYCLIC_PLANS.get(), BUILDS.get()), (0, 0));

        let raw = message(1, &[(1, 1, &[])]);
        for state in 0..4 {
            let mut decoder = Decoder::new(&raw, &options()).unwrap();
            match state {
                0 => decoder.begin_record_view(1).unwrap(),
                1 => decoder.enter_collection().unwrap(),
                2 => decoder.begin_element(),
                3 => {
                    decoder.read_root_scalar().unwrap();
                }
                _ => unreachable!(),
            }
            let position = decoder.position;
            assert_eq!(
                ManualRoot::ace_decode(&mut decoder).err(),
                Some(Error::InvalidArgument)
            );
            assert_eq!(
                decoder.position, position,
                "rejected root never advances the cursor"
            );
            assert_eq!((CYCLIC_PLANS.get(), BUILDS.get()), (0, 0));
        }
    }

    #[test]
    fn cycle_unsupported_ordinary_field_on_identity_cycle() {
        for objects in [
            vec![(1, 1, true, None, Some(1))],
            vec![(1, 1, true, Some(2), None), (2, 2, true, None, Some(1))],
        ] {
            reset_counts();
            let raw = ordinary_message(&objects);
            let mut decoder = Decoder::new(&raw, &options()).unwrap();
            assert_eq!(
                decode_cyclic::<Rc<OrdinaryNode>>(&mut decoder).err(),
                Some(Error::CycleUnsupported)
            );
            assert_eq!(BUILDS.get(), 0);
            assert_eq!(ORDINARY_READS.get(), objects.len());
        }
        reset_counts();
        let mut raw = ordinary_message(&[(1, 1, true, None, Some(1))]);
        // 全局 framing 合法，只把普通 bool 的 payload 改为非法的 2。
        let m = i32::from_le_bytes(raw[..4].try_into().unwrap()) as usize;
        raw[4 + m + 4 + 16 + 8] = 2;
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        assert_eq!(
            decode_cyclic::<Rc<OrdinaryNode>>(&mut decoder).err(),
            Some(Error::InvalidTag)
        );
        assert_eq!((ORDINARY_READS.get(), BUILDS.get()), (1, 0));
    }
    #[test]
    fn ordinary_field_on_noncyclic_identity_succeeds() {
        reset_counts();
        let raw = ordinary_message(&[(1, 1, true, None, None)]);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        {
            let mut ctx = DecodeCtx::new(&mut decoder);
            Rc::<OrdinaryNode>::decode_plan(&mut ctx).unwrap();
            assert_eq!(ctx.ordinary_owners, BTreeSet::from([1]));
            assert!(ctx.ordinary_owners.len() <= ctx.entries.len());
            assert_eq!(ctx.entries.len(), 1);
        }
        reset_counts();
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        let result = decode_cyclic::<Rc<OrdinaryNode>>(&mut decoder);
        assert!(
            result.is_ok(),
            "noncyclic ordinary field must succeed: {:?}",
            result.err()
        );
        let root = result.unwrap();
        assert_eq!(root.value, Ordinary(true));
        assert!(root.child.is_none() && root.back.is_none());
        assert_eq!((ORDINARY_READS.get(), BUILDS.get()), (1, 1));
        let raw = message(1, &[(1, 1, &[1, 2]), (2, 2, &[99, 2])]);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        let mut ctx = DecodeCtx::new(&mut decoder);
        ctx.object(|ctx| {
            ctx.ordinary_field::<Ordinary>(1)?;
            ctx.field::<Rc<PublicNode>>(2)?;
            ctx.ordinary_field::<Option<Ordinary>>(3)?;
            Ok(())
        })
        .unwrap();
        assert!(
            ctx.ordinary_owners.is_empty(),
            "virtual owner 0 is never registered"
        );
        assert_eq!(ctx.entries.len(), 1);
        assert!(
            ctx.finish_plan(0).is_ok(),
            "unrelated cyclic graph does not reject virtual ordinary values"
        );
    }

    struct SharedRoot {
        first: Rc<OrdinaryNode>,
        second: Rc<OrdinaryNode>,
        back: Option<Weak<SharedRoot>>,
    }
    impl AceDecodeCyclic for SharedRoot {
        type Plan = (u32, u32, Option<u32>);
        fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<Rc<OrdinaryNode>>(1)?,
                    ctx.field::<Rc<OrdinaryNode>>(2)?,
                    ctx.field::<Option<Weak<Self>>>(3)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
            BUILDS.set(BUILDS.get() + 1);
            Self {
                first: Rc::<OrdinaryNode>::build_from_plan(ctx, plan.0),
                second: Rc::<OrdinaryNode>::build_from_plan(ctx, plan.1),
                back: Option::<Weak<Self>>::build_from_plan(ctx, plan.2),
            }
        }
    }
    #[test]
    fn r_c2_04_map_plan_graph_values_and_phase_order() {
        use crate::{encode::Encoder, schema::AceEncode};
        use alloc::string::String;

        std::thread_local! {
            static WIRE_ORDER: Cell<u64> = const { Cell::new(0) };
        }
        // 不提供 Clone、Default 或 AceDecode，且单独跟踪映射键入口。
        #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
        struct WireKey(u32);
        impl AceElement for WireKey {
            fn element_kind() -> ElementKind {
                u32::element_kind()
            }
            fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                self.0.encode_element(e)
            }
            fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
                u32::decode_element(d).map(Self)
            }
            fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
                let key = u32::decode_map_key(d)?;
                WIRE_ORDER.set(WIRE_ORDER.get() * 10 + u64::from(key));
                Ok(Self(key))
            }
        }
        struct MapValue(Option<Rc<PublicNode>>);
        impl AceDecodeCyclic for MapValue {
            type Plan = Option<u32>;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                WIRE_ORDER.set(WIRE_ORDER.get() * 10 + 9);
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                Option::<Rc<PublicNode>>::decode_plan(ctx)
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                Self(Option::<Rc<PublicNode>>::build_from_plan(ctx, plan))
            }
        }
        type GraphMap = BTreeMap<WireKey, MapValue>;
        struct MapParent {
            values: Option<GraphMap>,
            ordinary: Ordinary,
        }
        impl AceDecodeCyclic for MapParent {
            type Plan = (<Option<GraphMap> as AceDecodeCyclic>::Plan, Ordinary);
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| {
                    Ok((
                        ctx.field::<Option<GraphMap>>(1)?,
                        ctx.ordinary_field::<Ordinary>(2)?,
                    ))
                })
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                Self {
                    values: Option::<GraphMap>::build_from_plan(ctx, plan.0),
                    ordinary: plan.1,
                }
            }
        }
        // 只补本测试的映射 record，沿用原有对象／槽消息夹具。
        fn map_wire(root: u64, objects: &[(u32, i32, &[u64])], body: &[u8]) -> Vec<u8> {
            let raw = message(root, objects);
            let len = i32::from_le_bytes(raw[..4].try_into().unwrap()) as usize;
            let mut main = raw[4..4 + len].to_vec();
            main.extend(10u32.to_le_bytes());
            main.extend((body.len() as i32).to_le_bytes());
            main.extend(body);
            [
                (main.len() as i32).to_le_bytes().as_slice(),
                &main,
                &raw[4 + len..],
            ]
            .concat()
        }
        let mut body = 3u32.to_le_bytes().to_vec();
        for (key, present) in [(3u32, true), (1, false), (2, true)] {
            body.extend(key.to_le_bytes());
            body.push(u8::from(present));
            if present {
                body.extend(1u32.to_le_bytes());
            }
        }
        let check_map = |map: &GraphMap| {
            assert_eq!(
                map.keys().map(|key| key.0).collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
            assert!(map[&WireKey(1)].0.is_none());
            let first = map[&WireKey(2)].0.as_ref().unwrap();
            let second = map[&WireKey(3)].0.as_ref().unwrap();
            assert!(Rc::ptr_eq(first, second));
            assert_eq!(first.value, 99);
            assert!(Rc::ptr_eq(
                first,
                &first.back.as_ref().unwrap().upgrade().unwrap()
            ));
            assert_eq!(WIRE_ORDER.get(), 391929, "键和值按 wire 顺序交替规划");
            assert_eq!(CYCLIC_PLANS.get(), 3);
        };
        let raw = map_wire(10, &[(1, 2, &[99, 1])], &body);
        reset_counts();
        WIRE_ORDER.set(0);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        let result = decode_cyclic::<GraphMap>(&mut decoder);
        assert!(result.is_ok(), "图值 Map 根必须成功: {:?}", result.err());
        let map = result.unwrap();
        check_map(&map);
        assert_eq!(BUILDS.get(), 4);
        assert_eq!(decoder.finish(), Ok(()));

        // 同一映射作为共享目标体时不能再次读取外部槽或元素 id。
        reset_counts();
        WIRE_ORDER.set(0);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        let map = decode_cyclic::<Rc<GraphMap>>(&mut decoder).unwrap();
        check_map(&map);
        assert_eq!(BUILDS.get(), 4);

        reset_counts();
        WIRE_ORDER.set(0);
        let raw = map_wire(20, &[(20, 1, &[10, 1]), (1, 3, &[99, 1])], &body);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        let parent = decode_cyclic::<MapParent>(&mut decoder).unwrap();
        check_map(parent.values.as_ref().unwrap());
        assert_eq!(parent.ordinary, Ordinary(true));
        assert_eq!((ORDINARY_READS.get(), BUILDS.get()), (1, 5));
        assert_eq!(decoder.finish(), Ok(()));

        fn check_key_budget<K: AceElement + Ord>(key: K) {
            let mut encoder = Encoder::new(&options());
            BTreeMap::from([(key, 999u32)])
                .ace_encode(&mut encoder)
                .unwrap();
            let raw = encoder.finish().unwrap();
            for (limit, error) in [(3, Error::ResourceLimit), (4, Error::InvalidReference)] {
                reset_counts();
                let opts = Options {
                    max_map_string_key_bytes: limit,
                    ..options()
                };
                let mut decoder = Decoder::new(&raw, &opts).unwrap();
                assert_eq!(
                    decode_cyclic::<BTreeMap<K, Rc<PublicNode>>>(&mut decoder).err(),
                    Some(error),
                    "键预算必须先于值引用校验"
                );
                assert_eq!(BUILDS.get(), 0);
            }
        }
        check_key_budget(String::from("abcd"));
        check_key_budget(vec![String::from("abcd")]);

        reset_counts();
        WIRE_ORDER.set(0);
        let raw = map_wire(10, &[], &u32::MAX.to_le_bytes());
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        assert_eq!(
            decode_cyclic::<GraphMap>(&mut decoder).err(),
            Some(Error::ResourceLimit)
        );
        assert_eq!(
            (WIRE_ORDER.get(), CYCLIC_PLANS.get(), BUILDS.get()),
            (0, 0, 0)
        );
        let mut trailing = body;
        trailing.push(0);
        let raw = map_wire(10, &[(1, 2, &[99, 1])], &trailing);
        let mut decoder = Decoder::new(&raw, &options()).unwrap();
        assert_eq!(
            decode_cyclic::<GraphMap>(&mut decoder).err(),
            Some(Error::InvalidTag)
        );
        assert_eq!(BUILDS.get(), 0);
    }

    #[test]
    fn r_c2_04_map_duplicate_key_validates_value_first() {
        use alloc::string::String;

        std::thread_local! {
            static MAP_NODES: Cell<usize> = const { Cell::new(0) };
        }
        struct MapNode;
        impl AceDecodeCyclic for MapNode {
            type Plan = (String, Option<u32>);
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| {
                    ORDINARY_READS.set(ORDINARY_READS.get() + 1);
                    Ok((
                        ctx.ordinary_field::<String>(1)?,
                        ctx.field::<Option<Weak<Self>>>(2)?,
                    ))
                })
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                MAP_NODES.set(MAP_NODES.get() + 1);
                let _ordinary = plan.0;
                let _back = Option::<Weak<Self>>::build_from_plan(ctx, plan.1);
                Self
            }
        }
        struct MapValue;
        impl AceDecodeCyclic for MapValue {
            type Plan = u32;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<u32> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                Rc::<MapNode>::decode_plan(ctx)
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: u32) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let _target = Rc::<MapNode>::build_from_plan(ctx, plan);
                Self
            }
        }
        for (name, ids, bad_ordinary, reserved, error, reads) in [
            (
                "合法第二值后查重",
                &[1u32, 2][..],
                false,
                false,
                Error::DuplicateKey,
                2,
            ),
            (
                "第二值直接非法",
                &[1, 99][..],
                false,
                false,
                Error::InvalidReference,
                1,
            ),
            (
                "第二值普通字段非法",
                &[1, 2][..],
                true,
                false,
                Error::InvalidUtf8,
                2,
            ),
            (
                "重复后不读第三值",
                &[1, 2, 99][..],
                false,
                true,
                Error::DuplicateKey,
                2,
            ),
        ] {
            reset_counts();
            MAP_NODES.set(0);
            let first: &[u64] = if reserved { &[4, 3] } else { &[4] };
            // 节点 2 有合法普通字段和 Weak 自环：查重必须先于 SCC 的 14。
            // 最后一例另有 Reserved Weak 3：查重也必须先于最终引用检查的 7。
            let raw = message(10, &[(1, 2, first), (2, 2, &[5, 2]), (3, 2, &[])]);
            let len = i32::from_le_bytes(raw[..4].try_into().unwrap()) as usize;
            let mut main = raw[4..4 + len].to_vec();
            let mut body = (ids.len() as u32).to_le_bytes().to_vec();
            for (index, id) in ids.iter().enumerate() {
                body.extend((if index < 2 { 1u32 } else { 2 }).to_le_bytes());
                body.extend(id.to_le_bytes());
            }
            let second: &[u8] = if bad_ordinary { &[0xff] } else { b"ok" };
            for (id, body) in [(10u32, body.as_slice()), (4, b"ok".as_slice()), (5, second)] {
                main.extend(id.to_le_bytes());
                main.extend((body.len() as i32).to_le_bytes());
                main.extend(body);
            }
            let raw = [
                (main.len() as i32).to_le_bytes().as_slice(),
                &main,
                &raw[4 + len..],
            ]
            .concat();
            let opts = Options {
                max_build_nesting: 0,
                ..options()
            };
            // framing 必须合法；第三值的不存在引用只会在 schema 读取时被发现。
            let mut decoder = Decoder::new(&raw, &opts).unwrap();
            assert_eq!(
                decode_cyclic::<BTreeMap<u32, MapValue>>(&mut decoder).err(),
                Some(error),
                "{name}"
            );
            assert_eq!(CYCLIC_PLANS.get(), 2, "{name}: 不得读取第三值");
            assert_eq!(ORDINARY_READS.get(), reads, "{name}: 普通字段立即读取");
            assert_eq!((MAP_NODES.get(), BUILDS.get()), (0, 0), "{name}");
        }
    }

    #[test]
    fn ordinary_field_on_shared_dag_succeeds() {
        for cycle in [false, true] {
            reset_counts();
            let fields: &[u64] = if cycle { &[2, 2, 1] } else { &[2, 2] };
            let raw = message(1, &[(1, 1, fields), (2, 2, &[1])]);
            let mut decoder = Decoder::new(&raw, &options()).unwrap();
            let result = decode_cyclic::<Rc<SharedRoot>>(&mut decoder);
            assert!(
                result.is_ok(),
                "shared DAG or noncyclic descendant must succeed: {:?}",
                result.err()
            );
            let root = result.unwrap();
            assert!(Rc::ptr_eq(&root.first, &root.second));
            assert_eq!(root.first.value, Ordinary(true));
            assert_eq!(root.back.is_some(), cycle);
            if cycle {
                assert!(Rc::ptr_eq(
                    &root,
                    &root.back.as_ref().unwrap().upgrade().unwrap()
                ));
            }
            assert_eq!((ORDINARY_READS.get(), BUILDS.get()), (1, 2));
        }
    }

    std::thread_local! {
        static REFCELL_NODES: Cell<usize> = const { Cell::new(0) };
    }

    struct RefCellNode {
        value: u32,
        first: Option<Rc<RefCell<Self>>>,
        second: Option<Rc<RefCell<Self>>>,
        back: Option<Weak<RefCell<Self>>>,
    }
    impl AceDecodeCyclic for RefCellNode {
        type Plan = (u32, Option<u32>, Option<u32>, Option<u32>);
        fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
            CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<u32>(1)?,
                    ctx.field::<Option<Rc<RefCell<Self>>>>(2)?,
                    ctx.field::<Option<Rc<RefCell<Self>>>>(3)?,
                    ctx.field::<Option<Weak<RefCell<Self>>>>(4)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
            BUILDS.set(BUILDS.get() + 1);
            let node = Self {
                value: plan.0,
                first: Option::<Rc<RefCell<Self>>>::build_from_plan(ctx, plan.1),
                second: Option::<Rc<RefCell<Self>>>::build_from_plan(ctx, plan.2),
                back: Option::<Weak<RefCell<Self>>>::build_from_plan(ctx, plan.3),
            };
            REFCELL_NODES.set(REFCELL_NODES.get() + 1);
            node
        }
    }

    #[test]
    fn r_c2_05_rc_refcell_dag_and_weak_identity() {
        // Both a completed child and the still-open root must retain RefCell identity.
        for weak_id in [2, 1] {
            reset_counts();
            REFCELL_NODES.set(0);
            let raw = message(1, &[(1, 1, &[37, 2, 2, weak_id]), (2, 2, &[99])]);
            let mut decoder = Decoder::new(&raw, &options()).unwrap();
            let result = decode_cyclic::<Rc<RefCell<RefCellNode>>>(&mut decoder);
            assert!(
                result.is_ok(),
                "Rc<RefCell<T>> DAG must decode: {:?}",
                result.err()
            );
            let root = result.unwrap();
            let (first, second, back) = {
                let node = root.borrow();
                assert_eq!(node.value, 37);
                (
                    node.first.as_ref().unwrap().clone(),
                    node.second.as_ref().unwrap().clone(),
                    node.back.as_ref().unwrap().clone(),
                )
            };
            assert!(Rc::ptr_eq(&first, &second));
            assert_eq!(first.borrow().value, 99);
            assert!(first.borrow().first.is_none());
            assert!(first.borrow().second.is_none());
            assert!(first.borrow().back.is_none());
            let target = if weak_id == 2 { &first } else { &root };
            assert!(Weak::ptr_eq(&back, &Rc::downgrade(target)));
            assert!(Rc::ptr_eq(&back.upgrade().unwrap(), target));
            assert_eq!(
                (CYCLIC_PLANS.get(), REFCELL_NODES.get(), BUILDS.get()),
                (2, 2, 2)
            );
            assert_eq!(decoder.finish(), Ok(()));
            let root_weak = Rc::downgrade(&root);
            let child_weak = Rc::downgrade(&first);
            drop((root, first, second));
            assert!(root_weak.upgrade().is_none());
            assert!(child_weak.upgrade().is_none());
            assert!(back.upgrade().is_none());
        }
    }

    #[test]
    fn r_c2_05_rc_refcell_strong_cycle_constructs_no_nodes() {
        for objects in [
            vec![(1, 1, &[37, 1][..])],
            vec![(1, 1, &[37, 2][..]), (2, 2, &[99, 1][..])],
        ] {
            reset_counts();
            REFCELL_NODES.set(0);
            let raw = message(1, &objects);
            let mut decoder = Decoder::new(&raw, &options()).unwrap();
            let error = decode_cyclic::<Rc<RefCell<RefCellNode>>>(&mut decoder).err();
            assert_eq!(error, Some(Error::CycleUnsupported));
            assert_eq!(error.unwrap().code(), 14);
            assert_eq!(CYCLIC_PLANS.get(), objects.len());
            assert_eq!((REFCELL_NODES.get(), BUILDS.get()), (0, 0));
        }
    }

    #[cfg(feature = "std")]
    mod arc_tests {
        use super::*;
        use std::sync::RwLock;

        std::thread_local! {
            static ARC_NODES: Cell<usize> = const { Cell::new(0) };
        }

        fn reset_arc_counts() {
            reset_counts();
            ARC_NODES.set(0);
        }

        struct ArcValue {
            value: u32,
        }
        impl AceDecodeCyclic for ArcValue {
            type Plan = u32;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<u32> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| ctx.field::<u32>(1))
            }
            fn build_from_plan(_ctx: &mut PublicBuildCtx<'_>, value: u32) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                ARC_NODES.set(ARC_NODES.get() + 1);
                Self { value }
            }
        }

        struct RcChild {
            back: SyncWeak<ArcRoot>,
        }
        impl AceDecodeCyclic for RcChild {
            type Plan = u32;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<u32> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| ctx.field::<SyncWeak<ArcRoot>>(1))
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, back: u32) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let node = Self {
                    back: SyncWeak::<ArcRoot>::build_from_plan(ctx, back),
                };
                ARC_NODES.set(ARC_NODES.get() + 1);
                node
            }
        }

        struct ArcRoot {
            first: Arc<ArcValue>,
            second: Arc<ArcValue>,
            other: Arc<ArcValue>,
            child: Rc<RcChild>,
            back: SyncWeak<ArcRoot>,
        }
        impl AceDecodeCyclic for ArcRoot {
            type Plan = (u32, u32, u32, u32, u32);
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| {
                    Ok((
                        ctx.field::<Arc<ArcValue>>(1)?,
                        ctx.field::<Arc<ArcValue>>(2)?,
                        ctx.field::<Arc<ArcValue>>(3)?,
                        ctx.field::<Rc<RcChild>>(4)?,
                        ctx.field::<SyncWeak<ArcRoot>>(5)?,
                    ))
                })
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let node = Self {
                    first: Arc::<ArcValue>::build_from_plan(ctx, plan.0),
                    second: Arc::<ArcValue>::build_from_plan(ctx, plan.1),
                    other: Arc::<ArcValue>::build_from_plan(ctx, plan.2),
                    child: Rc::<RcChild>::build_from_plan(ctx, plan.3),
                    back: SyncWeak::<ArcRoot>::build_from_plan(ctx, plan.4),
                };
                ARC_NODES.set(ARC_NODES.get() + 1);
                node
            }
        }

        #[test]
        fn r_s3_e_01_arc_build_ctx_option_and_panic_contract() {
            use std::panic::{catch_unwind, AssertUnwindSafe};
            let node = NodeType {
                target: core::any::TypeId::of::<u32>(),
                family: Family::Arc,
            };
            let mut ctx = BuildCtx {
                entries: BTreeMap::from([(
                    1,
                    Entry {
                        node,
                        state: EntryState::Building,
                    },
                )]),
            };
            assert!(ctx.weak_arc::<u32>(99).is_none());
            assert!(ctx.strong_arc::<u32>(99).is_none());
            assert!(ctx.weak_arc::<u32>(1).is_none());
            assert!(ctx.strong_arc::<u32>(1).is_none());
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak_arc::<u64>(1))).is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.strong_arc::<u64>(1))).is_err());
            // Arc 访问器与绑定都拒绝 Rc 族身份。
            ctx.entries.get_mut(&1).unwrap().node.family = Family::Rc;
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak_arc::<u32>(1))).is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.strong_arc::<u32>(1))).is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| {
                ctx.bind_weak_arc::<u32>(1, SyncWeak::new())
            }))
            .is_err());
            ctx.entries.get_mut(&1).unwrap().node.family = Family::Arc;
            // 绑定只允许已取出 job 的 Unopened(None)；未知身份、类型不符或 job 未取出都是协议 panic。
            assert!(catch_unwind(AssertUnwindSafe(|| {
                ctx.bind_weak_arc::<u32>(99, SyncWeak::new())
            }))
            .is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| {
                ctx.bind_weak_arc::<u64>(1, SyncWeak::new())
            }))
            .is_err());
            ctx.entries.get_mut(&1).unwrap().state = EntryState::Planned(Some(Box::new(|_, _| {})));
            assert!(catch_unwind(AssertUnwindSafe(|| {
                ctx.bind_weak_arc::<u32>(1, SyncWeak::new())
            }))
            .is_err());
            ctx.entries.get_mut(&1).unwrap().state = EntryState::Building;
            let value = Arc::new_cyclic(|own: &SyncWeak<u32>| {
                ctx.bind_weak_arc::<u32>(1, own.clone());
                assert!(ctx
                    .weak_arc::<u32>(1)
                    .is_some_and(|weak| SyncWeak::ptr_eq(&weak, own)));
                assert!(ctx.weak_arc::<u32>(1).unwrap().upgrade().is_none());
                assert!(ctx.strong_arc::<u32>(1).is_none());
                assert!(catch_unwind(AssertUnwindSafe(|| {
                    ctx.bind_weak_arc::<u32>(1, own.clone())
                }))
                .is_err());
                37
            });
            // 完成必须来自刚绑定的分配；分配不一致是协议 panic，之后按原分配重新打开。
            assert!(catch_unwind(AssertUnwindSafe(|| {
                ctx.complete_arc::<u32>(1, Arc::new(38))
            }))
            .is_err());
            ctx.entries.get_mut(&1).unwrap().state = EntryState::Open {
                weak: Box::new(Arc::downgrade(&value)),
            };
            ctx.complete_arc(1, Arc::clone(&value));
            assert!(Arc::ptr_eq(&ctx.strong_arc::<u32>(1).unwrap(), &value));
            assert!(SyncWeak::ptr_eq(
                &ctx.weak_arc::<u32>(1).unwrap(),
                &Arc::downgrade(&value)
            ));
            assert!(Arc::ptr_eq(
                &ctx.weak_arc::<u32>(1).unwrap().upgrade().unwrap(),
                &value
            ));
            // 擦除载荷类型错误时，Done 的强句柄与 Open 的弱句柄访问都是协议 panic。
            ctx.entries.get_mut(&1).unwrap().state = EntryState::Done {
                weak: Box::new(Arc::downgrade(&value)),
                strong: Box::new(37u32),
            };
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.strong_arc::<u32>(1))).is_err());
            ctx.entries.get_mut(&1).unwrap().state = EntryState::Open {
                weak: Box::new(37u32),
            };
            assert!(catch_unwind(AssertUnwindSafe(|| ctx.weak_arc::<u32>(1))).is_err());
        }

        #[test]
        fn r_s3_e_02_arc_shared_and_mixed_weak_identity() {
            use std::panic::{catch_unwind, AssertUnwindSafe};
            // 根 1 含两个同 id 的 Arc 叶、一个同值异 id 的 Arc 叶、Rc 子节点 4 与指向自身的 SyncWeak；4 回指根。
            let raw = message(
                1,
                &[
                    (1, 1, &[2, 2, 3, 4, 1]),
                    (2, 2, &[42]),
                    (3, 2, &[42]),
                    (4, 2, &[1]),
                ],
            );
            reset_arc_counts();
            let legal = Options {
                max_build_nesting: 2,
                ..options()
            };
            let mut decoder = Decoder::new(&raw, &legal).unwrap();
            let decoded = catch_unwind(AssertUnwindSafe(|| {
                decode_cyclic::<Arc<ArcRoot>>(&mut decoder)
            }));
            assert!(decoded.is_ok(), "合法 Arc 混合图不得因构造入口 panic");
            let root = decoded.unwrap().expect("合法 Arc 混合图必须解码成功");
            assert_eq!(decoder.finish(), Ok(()));
            assert!(Arc::ptr_eq(&root.first, &root.second));
            assert!(!Arc::ptr_eq(&root.first, &root.other));
            assert_eq!((root.first.value, root.other.value), (42, 42));
            assert!(root
                .back
                .upgrade()
                .is_some_and(|back| Arc::ptr_eq(&back, &root)));
            assert!(root
                .child
                .back
                .upgrade()
                .is_some_and(|back| Arc::ptr_eq(&back, &root)));
            assert_eq!((ARC_NODES.get(), BUILDS.get()), (4, 4));
            let root_weak = Arc::downgrade(&root);
            let child_weak = Rc::downgrade(&root.child);
            drop(root);
            assert!(root_weak.upgrade().is_none());
            assert!(child_weak.upgrade().is_none());
            // 同一图的构造峰值为 2；预算 1 必须在阶段一后返回 6，且不构造任何节点。
            reset_arc_counts();
            let tight = Options {
                max_build_nesting: 1,
                ..options()
            };
            let mut decoder = Decoder::new(&raw, &tight).unwrap();
            let error = decode_cyclic::<Arc<ArcRoot>>(&mut decoder).err();
            assert_eq!(error, Some(Error::ResourceLimit));
            assert_eq!(error.unwrap().code(), 6);
            assert_eq!((ARC_NODES.get(), BUILDS.get()), (0, 0));
        }

        struct LockNode {
            value: u32,
            first: Option<Arc<RwLock<Self>>>,
            second: Option<Arc<RwLock<Self>>>,
            back: Option<SyncWeak<RwLock<Self>>>,
        }
        impl AceDecodeCyclic for LockNode {
            type Plan = (u32, Option<u32>, Option<u32>, Option<u32>);
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| {
                    Ok((
                        ctx.field::<u32>(1)?,
                        ctx.field::<Option<Arc<RwLock<Self>>>>(2)?,
                        ctx.field::<Option<Arc<RwLock<Self>>>>(3)?,
                        ctx.field::<Option<SyncWeak<RwLock<Self>>>>(4)?,
                    ))
                })
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let node = Self {
                    value: plan.0,
                    first: Option::<Arc<RwLock<Self>>>::build_from_plan(ctx, plan.1),
                    second: Option::<Arc<RwLock<Self>>>::build_from_plan(ctx, plan.2),
                    back: Option::<SyncWeak<RwLock<Self>>>::build_from_plan(ctx, plan.3),
                };
                ARC_NODES.set(ARC_NODES.get() + 1);
                node
            }
        }

        struct CycleA {
            child: Arc<RwLock<CycleB>>,
        }
        impl AceDecodeCyclic for CycleA {
            type Plan = u32;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<u32> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| ctx.field::<Arc<RwLock<CycleB>>>(1))
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: u32) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let node = Self {
                    child: Arc::<RwLock<CycleB>>::build_from_plan(ctx, plan),
                };
                ARC_NODES.set(ARC_NODES.get() + 1);
                node
            }
        }

        struct CycleB {
            back: SyncWeak<RwLock<CycleA>>,
        }
        impl AceDecodeCyclic for CycleB {
            type Plan = u32;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<u32> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| ctx.field::<SyncWeak<RwLock<CycleA>>>(1))
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: u32) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let node = Self {
                    back: SyncWeak::<RwLock<CycleA>>::build_from_plan(ctx, plan),
                };
                ARC_NODES.set(ARC_NODES.get() + 1);
                node
            }
        }

        #[test]
        fn r_s3_e_03_arc_rwlock_dag_and_weak_identity() {
            // 与 RefCell 形状一致：根两字段共享同一子节点，根上的 Weak 分别指向 Done 子节点与 Open 根。
            for weak_id in [2, 1] {
                reset_arc_counts();
                let raw = message(1, &[(1, 1, &[37, 2, 2, weak_id]), (2, 2, &[99])]);
                let mut decoder = Decoder::new(&raw, &options()).unwrap();
                let result = decode_cyclic::<Arc<RwLock<LockNode>>>(&mut decoder);
                assert!(
                    result.is_ok(),
                    "Arc<RwLock<T>> DAG must decode: {:?}",
                    result.err()
                );
                let root = result.unwrap();
                let (first, second, back) = {
                    let node = root.read().unwrap();
                    assert_eq!(node.value, 37);
                    (
                        node.first.as_ref().unwrap().clone(),
                        node.second.as_ref().unwrap().clone(),
                        node.back.as_ref().unwrap().clone(),
                    )
                };
                assert!(Arc::ptr_eq(&first, &second));
                {
                    let child = first.read().unwrap();
                    assert_eq!(child.value, 99);
                    assert!(child.first.is_none());
                    assert!(child.second.is_none());
                    assert!(child.back.is_none());
                }
                let target = if weak_id == 2 { &first } else { &root };
                assert!(SyncWeak::ptr_eq(&back, &Arc::downgrade(target)));
                assert!(Arc::ptr_eq(&back.upgrade().unwrap(), target));
                assert_eq!(
                    (CYCLIC_PLANS.get(), ARC_NODES.get(), BUILDS.get()),
                    (2, 2, 2)
                );
                assert_eq!(decoder.finish(), Ok(()));
                let root_weak = Arc::downgrade(&root);
                let child_weak = Arc::downgrade(&first);
                drop((root, first, second));
                assert!(root_weak.upgrade().is_none());
                assert!(child_weak.upgrade().is_none());
                assert!(back.upgrade().is_none());
            }

            // E03 独立能力：RwLock<u32> 可直接作为 Cyclic 根读出原值。
            reset_arc_counts();
            let raw = message(37, &[]);
            let mut decoder = Decoder::new(&raw, &options()).unwrap();
            let lock = decode_cyclic::<RwLock<u32>>(&mut decoder).unwrap();
            assert_eq!(*lock.read().unwrap(), 37);
            assert_eq!(decoder.finish(), Ok(()));

            // 异构锁包装二节点回环：A 强持有 B，B 的 Weak 回指 A。
            reset_arc_counts();
            let raw = message(1, &[(1, 1, &[2]), (2, 2, &[1])]);
            let mut decoder = Decoder::new(&raw, &options()).unwrap();
            let root = decode_cyclic::<Arc<RwLock<CycleA>>>(&mut decoder).unwrap();
            let child = { root.read().unwrap().child.clone() };
            let back = { child.read().unwrap().back.clone() };
            assert!(back
                .upgrade()
                .is_some_and(|upgraded| Arc::ptr_eq(&upgraded, &root)));
            assert_eq!(
                (CYCLIC_PLANS.get(), ARC_NODES.get(), BUILDS.get()),
                (2, 2, 2)
            );
            assert_eq!(decoder.finish(), Ok(()));
            let root_weak = Arc::downgrade(&root);
            let child_weak = Arc::downgrade(&child);
            drop((root, child));
            assert!(root_weak.upgrade().is_none());
            assert!(child_weak.upgrade().is_none());
            assert!(back.upgrade().is_none());
        }

        struct ArcCycle {
            next: Option<Arc<Self>>,
        }
        impl AceDecodeCyclic for ArcCycle {
            type Plan = Option<u32>;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| ctx.field::<Option<Arc<Self>>>(1))
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let node = Self {
                    next: Option::<Arc<Self>>::build_from_plan(ctx, plan),
                };
                ARC_NODES.set(ARC_NODES.get() + 1);
                node
            }
        }

        struct LockCycle {
            next: Option<Arc<RwLock<Self>>>,
        }
        impl AceDecodeCyclic for LockCycle {
            type Plan = Option<u32>;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                CYCLIC_PLANS.set(CYCLIC_PLANS.get() + 1);
                ctx.object(|ctx| ctx.field::<Option<Arc<RwLock<Self>>>>(1))
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                BUILDS.set(BUILDS.get() + 1);
                let node = Self {
                    next: Option::<Arc<RwLock<Self>>>::build_from_plan(ctx, plan),
                };
                ARC_NODES.set(ARC_NODES.get() + 1);
                node
            }
        }

        #[test]
        fn r_s3_e_04_arc_strong_cycle_constructs_no_nodes() {
            let self_loop = message(1, &[(1, 1, &[1])]);
            let two_node = message(1, &[(1, 1, &[2]), (2, 2, &[1])]);
            for raw in [&self_loop, &two_node] {
                // 已到达的强回边在构造预算为 0 时仍须报 14。
                for opts in [
                    options(),
                    Options {
                        max_build_nesting: 0,
                        ..options()
                    },
                ] {
                    reset_arc_counts();
                    let mut decoder = Decoder::new(raw, &opts).unwrap();
                    let error = decode_cyclic::<Arc<ArcCycle>>(&mut decoder).err();
                    assert_eq!(error, Some(Error::CycleUnsupported));
                    assert_eq!(error.unwrap().code(), 14);
                    assert_eq!((BUILDS.get(), ARC_NODES.get()), (0, 0));
                    let mut decoder = Decoder::new(raw, &opts).unwrap();
                    let error = decode_cyclic::<Arc<RwLock<LockCycle>>>(&mut decoder).err();
                    assert_eq!(error, Some(Error::CycleUnsupported));
                    assert_eq!(error.unwrap().code(), 14);
                    assert_eq!((BUILDS.get(), ARC_NODES.get()), (0, 0));
                }
            }
            // 失败事务不重试：新 Decoder 对合法单节点仍按原协议完成。
            let single = message(1, &[(1, 1, &[])]);
            reset_arc_counts();
            let mut decoder = Decoder::new(&single, &options()).unwrap();
            let node = decode_cyclic::<Arc<ArcCycle>>(&mut decoder).unwrap();
            assert!(node.next.is_none());
            assert_eq!(decoder.finish(), Ok(()));
            assert_eq!((BUILDS.get(), ARC_NODES.get()), (1, 1));
            reset_arc_counts();
            let mut decoder = Decoder::new(&single, &options()).unwrap();
            let node = decode_cyclic::<Arc<RwLock<LockCycle>>>(&mut decoder).unwrap();
            assert!(node.read().unwrap().next.is_none());
            assert_eq!(decoder.finish(), Ok(()));
            assert_eq!((BUILDS.get(), ARC_NODES.get()), (1, 1));
        }

        struct RequestFirst {
            weak: SyncWeak<ArcValue>,
            first: Arc<ArcValue>,
            second: Arc<ArcValue>,
        }
        impl AceDecodeCyclic for RequestFirst {
            type Plan = (u32, u32, u32);
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| {
                    Ok((
                        ctx.field::<SyncWeak<ArcValue>>(1)?,
                        ctx.field::<Arc<ArcValue>>(2)?,
                        ctx.field::<Arc<ArcValue>>(3)?,
                    ))
                })
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: Self::Plan) -> Self {
                Self {
                    weak: SyncWeak::<ArcValue>::build_from_plan(ctx, plan.0),
                    first: Arc::<ArcValue>::build_from_plan(ctx, plan.1),
                    second: Arc::<ArcValue>::build_from_plan(ctx, plan.2),
                }
            }
        }

        struct WeakOnlyRoot;
        impl AceDecodeCyclic for WeakOnlyRoot {
            type Plan = u32;
            fn decode_plan(ctx: &mut PublicDecodeCtx) -> Result<u32> {
                ctx.object(|ctx| ctx.field::<SyncWeak<ArcValue>>(1))
            }
            fn build_from_plan(ctx: &mut PublicBuildCtx<'_>, plan: u32) -> Self {
                drop(SyncWeak::<ArcValue>::build_from_plan(ctx, plan));
                Self
            }
        }

        #[test]
        fn r_s3_e_05_sync_weak_targets_and_identity_budgets() {
            // 普通根先请求 Weak，再由同 id 的强定义与重复强引用共同占用一个身份条目。
            let raw = message(1, &[(1, 1, &[2, 2, 2]), (2, 2, &[42])]);
            reset_arc_counts();
            let one_entry = Options {
                max_references: 1,
                ..options()
            };
            let mut decoder = Decoder::new(&raw, &one_entry).unwrap();
            let root = decode_cyclic::<RequestFirst>(&mut decoder).unwrap();
            assert!(root
                .weak
                .upgrade()
                .is_some_and(|weak| Arc::ptr_eq(&weak, &root.first)));
            assert!(Arc::ptr_eq(&root.first, &root.second));
            assert_eq!(root.first.value, 42);
            assert_eq!(decoder.finish(), Ok(()));
            assert_eq!((ARC_NODES.get(), BUILDS.get()), (1, 1));

            reset_arc_counts();
            let no_entry = Options {
                max_references: 0,
                ..options()
            };
            let mut decoder = Decoder::new(&raw, &no_entry).unwrap();
            let error = decode_cyclic::<RequestFirst>(&mut decoder).err();
            assert_eq!(error, Some(Error::ResourceLimit));
            assert_eq!(error.unwrap().code(), 6);
            assert_eq!((ARC_NODES.get(), BUILDS.get()), (0, 0));

            // 只有 Weak 而没有消息内强 owner 时，即使记录存在也返回 7；缺失或非法 id 同样为 7。
            let owner_less = message(1, &[(1, 1, &[2]), (2, 2, &[42])]);
            for raw in [
                owner_less,
                message(1, &[(1, 1, &[3])]),
                message(1, &[(1, 1, &[0])]),
                message(1, &[(1, 1, &[i32::MAX as u64 + 1])]),
                message(1, &[(1, 1, &[(1u64 << 32) + 2])]),
            ] {
                reset_arc_counts();
                let mut decoder = Decoder::new(&raw, &options()).unwrap();
                let error = decode_cyclic::<WeakOnlyRoot>(&mut decoder).err();
                assert_eq!(error, Some(Error::InvalidReference));
                assert_eq!(error.unwrap().code(), 7);
                assert_eq!((ARC_NODES.get(), BUILDS.get()), (0, 0));
            }

            // 空根的 Option<SyncWeak<_>> 与 Option<Arc<_>> 为 None，不产生身份节点。
            let empty = [0i32.to_le_bytes(), 0i32.to_le_bytes()].concat();
            reset_arc_counts();
            let mut decoder = Decoder::new(&empty, &options()).unwrap();
            assert!(decode_cyclic::<Option<SyncWeak<ArcValue>>>(&mut decoder)
                .unwrap()
                .is_none());
            assert_eq!(decoder.finish(), Ok(()));
            let mut decoder = Decoder::new(&empty, &options()).unwrap();
            assert!(decode_cyclic::<Option<Arc<ArcValue>>>(&mut decoder)
                .unwrap()
                .is_none());
            assert_eq!(decoder.finish(), Ok(()));
            assert_eq!(
                (CYCLIC_PLANS.get(), ARC_NODES.get(), BUILDS.get()),
                (0, 0, 0)
            );
        }
    }

    // R2 P3a 尾检失败例：尾随字节必须在当前视图与元素作用域仍打开时被检出（InvalidTag=4），且收束后位置与深度完全恢复。
    mod r2_tail_tests {
        use super::*;

        /// 完整解码入口的两态：raw 与 packed 各跑一次，返回成功与错误的一致结果。
        fn decode_both_modes<T: AceDecodeCyclic>(raw: &[u8]) -> [Result<T>; 2] {
            [false, true].map(|packed| {
                let opts = Options {
                    packed,
                    ..options()
                };
                let bytes = if packed {
                    crate::wire::packed::pack(raw, &opts).unwrap()
                } else {
                    raw.to_vec()
                };
                let mut decoder = Decoder::new(&bytes, &opts).unwrap();
                decode_cyclic::<T>(&mut decoder)
            })
        }

        #[test]
        fn r_s3_g_r2_10_vec_rc_trailing_byte_invalid_tag() {
            // 根 Vec<Rc<u32>>：Vec 记录声明一个元素，引用后追加 00。
            let raw = r2_raw(
                &[
                    r2_rec(1, &37u32.to_le_bytes()),
                    r2_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0, 0]),
                ]
                .concat(),
                &r2_slot(0, 0, 2u64.to_le_bytes()),
            );
            BUILD_CTX_ENTRIES.set(0);
            for result in decode_both_modes::<Vec<alloc::rc::Rc<u32>>>(&raw) {
                let error = result.err();
                assert_eq!(error, Some(Error::InvalidTag));
                assert_eq!(error.unwrap().code(), 4);
            }
            // 尾检在阶段一失败，两态均未进入阶段二 BuildCtx::new。
            assert_eq!(BUILD_CTX_ENTRIES.get(), 0);

            // 直接调用阶段一入口，验证视图、元素作用域与集合深度在尾检失败后全部恢复。
            let opts = options();
            let mut decoder = Decoder::new(&raw, &opts).unwrap();
            let mut ctx = DecodeCtx::new(&mut decoder);
            let before = r2_snapshot(&ctx);
            assert_eq!(ctx.vec::<Rc<u32>>().err(), Some(Error::InvalidTag));
            assert_eq!(r2_snapshot(&ctx), before);

            // 同字节去掉尾随 00 的正例：解码成功，且阶段二恰好构造一次，证明上面的零计数有效。
            let valid = r2_raw(
                &[
                    r2_rec(1, &37u32.to_le_bytes()),
                    r2_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
                ]
                .concat(),
                &r2_slot(0, 0, 2u64.to_le_bytes()),
            );
            let mut decoder = Decoder::new(&valid, &opts).unwrap();
            BUILD_CTX_ENTRIES.set(0);
            let decoded = decode_cyclic::<Vec<alloc::rc::Rc<u32>>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded.len(), 1);
            assert_eq!(*decoded[0], 37);
            assert_eq!(BUILD_CTX_ENTRIES.get(), 1);
        }

        #[test]
        fn r_s3_g_r2_11_box_rc_trailing_byte_invalid_tag() {
            // 根 Box<Rc<u32>>：Box 记录无 count，引用后追加 00。
            let raw = r2_raw(
                &[r2_rec(1, &37u32.to_le_bytes()), r2_rec(2, &[1, 0, 0, 0, 0])].concat(),
                &r2_slot(0, 0, 2u64.to_le_bytes()),
            );
            BUILD_CTX_ENTRIES.set(0);
            for result in decode_both_modes::<alloc::boxed::Box<alloc::rc::Rc<u32>>>(&raw) {
                let error = result.err();
                assert_eq!(error, Some(Error::InvalidTag));
                assert_eq!(error.unwrap().code(), 4);
            }
            assert_eq!(BUILD_CTX_ENTRIES.get(), 0);

            let opts = options();
            let mut decoder = Decoder::new(&raw, &opts).unwrap();
            let mut ctx = DecodeCtx::new(&mut decoder);
            let before = r2_snapshot(&ctx);
            assert_eq!(ctx.boxed::<Rc<u32>>().err(), Some(Error::InvalidTag));
            assert_eq!(r2_snapshot(&ctx), before);

            // 同字节去掉尾随 00 的正例：解码成功，且阶段二恰好构造一次，证明上面的零计数有效。
            let valid = r2_raw(
                &[r2_rec(1, &37u32.to_le_bytes()), r2_rec(2, &[1, 0, 0, 0])].concat(),
                &r2_slot(0, 0, 2u64.to_le_bytes()),
            );
            let mut decoder = Decoder::new(&valid, &opts).unwrap();
            BUILD_CTX_ENTRIES.set(0);
            let decoded =
                decode_cyclic::<alloc::boxed::Box<alloc::rc::Rc<u32>>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(**decoded, 37);
            assert_eq!(BUILD_CTX_ENTRIES.get(), 1);
        }

        #[test]
        fn r_s3_g_r2_12_vec_u32_bulk_trailing_byte_invalid_tag() {
            // 根 Vec<u32>：bulk 分支上 count=1，元素 7 之后追加单字节 00（设计金样：body 9B，raw 41B）。
            let body: [u8; 9] = [1, 0, 0, 0, 7, 0, 0, 0, 0];
            let raw = r2_raw(&r2_rec(1, &body), &r2_slot(0, 0, 1u64.to_le_bytes()));
            assert_eq!(body.len(), 9);
            assert_eq!(raw.len(), 41);
            for result in decode_both_modes::<Vec<u32>>(&raw) {
                let error = result.err();
                assert_eq!(error, Some(Error::InvalidTag));
                assert_eq!(error.unwrap().code(), 4);
            }

            let opts = options();
            let mut decoder = Decoder::new(&raw, &opts).unwrap();
            let mut ctx = DecodeCtx::new(&mut decoder);
            let before = r2_snapshot(&ctx);
            crate::hooks::reset();
            assert_eq!(ctx.vec::<u32>().err(), Some(Error::InvalidTag));
            assert_eq!(r2_snapshot(&ctx), before);
            // 小端走 bulk（元素级为 0）；非小端逐项走元素级。两端错误码均为 4。
            #[cfg(target_endian = "little")]
            assert_eq!(crate::hooks::snapshot(), (1, 0));
            #[cfg(not(target_endian = "little"))]
            assert_eq!(crate::hooks::snapshot(), (0, 1));
        }
    }
}
