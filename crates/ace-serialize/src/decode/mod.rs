//! Reading depth-tagged fields. Views borrow the decoder; for consecutive reads,
//! use the owned string/bytes methods or drop each view before the next read.
use crate::{
    error::{Error, Result},
    limits::Options,
    schema::value,
    wire::{
        envelope, packed,
        record::{RecordIndex, RecordSpan},
        slot::{Slot, SlotRange, SlotStream},
    },
};
use alloc::{borrow::Cow, collections::BTreeMap, string::String, vec::Vec};

/// 对象体的槽范围描述字节数：`u32 slot_offset + u32 slot_count`（FR-W06）。
const OBJECT_BODY_BYTES: usize = 8;
/// 多态对象体的逃生口 tag（FR-W07）：65535 专供显式 type_name。
const POLY_ESCAPE_TAG: u16 = 65535;

/// 打开的对象帧（设计 §2 第 2、5 项）：每个对象独立的槽游标与 order 状态。
struct ObjectFrame {
    /// 下一个未读槽的槽号（assist 下标，根槽为 0）。
    position: usize,
    /// 对象块的结束槽号（不含）。
    end: usize,
    /// 本帧最近一次请求的字段 order，同一对象内请求须严格递增。
    last_order: Option<i32>,
    /// 进入本帧前的 `selected`，`leave_object` 时恢复。帧本身按下标一一对应，
    /// 故保存在帧内，不再另开一条与 `frames` 同步的保存栈。
    saved_selected: Option<Option<[u8; 8]>>,
}

/// 已校验的对象范围（设计 §2 第 4 项）。同一 record 再次进入时复用结果，仅核对进入深度。
struct KnownObject {
    id: i32,
    slots: SlotRange,
    /// 首次进入时的 k；非空对象的槽深度为 k+1，因此再次进入须与之相同。
    entry_depth: usize,
}

/// 已校验的多态对象体（FR-W07）：variant tag、可选 type_name 别名与槽范围。
struct PolyBody {
    tag: u16,
    type_name: Option<String>,
    range: SlotRange,
}

pub struct Decoder<'a> {
    raw: Cow<'a, [u8]>,
    main_end: usize,
    assist_start: usize,
    index: RecordIndex,
    /// 根槽读取状态：0 表示未读，1 表示已读。对象字段游标保存在各自的 `ObjectFrame` 中。
    position: usize,
    opts: Options,
    decoded: usize,
    selected: Option<Option<[u8; 8]>>,
    /// 元素作用域可嵌套（列表套列表），按深度计数；内层结束不得清除外层的作用域。
    element_depth: usize,
    /// 打开的集合数（根集合与嵌套集合均计），与打开的对象帧数之和即 `open()`（V4-1）；由 `enter_collection` 按 `max_depth` 检查（FR-T07）。
    coll_depth: usize,
    record_views: Vec<(usize, usize, usize)>,
    /// 打开的对象帧，栈顶为当前对象。
    frames: Vec<ObjectFrame>,
    /// 已校验的对象范围，键为 record body_start（即 main 物理次序，设计 §0 C2）。
    known_objects: BTreeMap<usize, KnownObject>,
}

impl<'a> Decoder<'a> {
    pub fn new(raw: &'a [u8], opts: &Options) -> Result<Self> {
        if raw.len() > opts.max_input_bytes {
            return Err(Error::ResourceLimit);
        }
        let mut raw = if opts.packed {
            Cow::Owned(packed::unpack(raw, opts)?)
        } else {
            Cow::Borrowed(raw)
        };
        if opts.packed {
            // Packing pads the complete envelope to a word boundary.
            if raw.len() < 8 {
                return Err(Error::TruncatedInput);
            }
            let m = usize::try_from(i32::from_le_bytes(raw[..4].try_into().unwrap()))
                .map_err(|_| Error::TruncatedInput)?;
            let a_start = m.checked_add(8).ok_or(Error::TruncatedInput)?;
            if a_start > raw.len() {
                return Err(Error::TruncatedInput);
            }
            let a = usize::try_from(i32::from_le_bytes(
                raw[a_start - 4..a_start].try_into().unwrap(),
            ))
            .map_err(|_| Error::TruncatedInput)?;
            let end = a_start.checked_add(a).ok_or(Error::TruncatedInput)?;
            let padded = end.checked_add(7).ok_or(Error::TruncatedInput)? & !7;
            if padded != raw.len() || end > raw.len() || raw[end..].iter().any(|&b| b != 0) {
                return Err(Error::TruncatedInput);
            }
            raw.to_mut().truncate(end);
        }
        let (main, assist) = envelope::read_envelope(&raw, opts)?;
        let main_end = 4 + main.len();
        let assist_start = main_end + 4;
        let index = RecordIndex::parse(main, opts)?;
        SlotStream::parse(assist, opts)?;
        Ok(Self {
            raw,
            main_end,
            assist_start,
            index,
            position: 0,
            opts: opts.clone(),
            decoded: 0,
            selected: None,
            element_depth: 0,
            coll_depth: 0,
            record_views: Vec::new(),
            frames: Vec::new(),
            known_objects: BTreeMap::new(),
        })
    }
    pub(crate) fn options(&self) -> &Options {
        &self.opts
    }
    /// 只查已校验的 record 索引；引用查询本身不是一次物化。
    fn record_span(&self, payload: u64) -> Result<RecordSpan> {
        let id = i32::try_from(payload)
            .ok()
            .filter(|&id| id > 0)
            .ok_or(Error::InvalidReference)?;
        self.index.get(id).copied().ok_or(Error::InvalidReference)
    }
    /// 计实际物化次数，不按 record id 去重；共享复用不进入这些收费点。
    fn charge_materialization(&mut self, size: usize) -> Result<()> {
        let decoded = self.decoded.checked_add(size).ok_or(Error::ResourceLimit)?;
        if decoded > self.opts.max_decoded_bytes {
            return Err(Error::ResourceLimit);
        }
        self.decoded = decoded;
        Ok(())
    }
    /// Open a record cursor; nested views are closed in LIFO order.
    pub fn begin_record_view(&mut self, inst_id: u32) -> Result<()> {
        let span = self.record_span(u64::from(inst_id))?;
        self.charge_materialization(span.body_end - span.body_start)?;
        let start = 4 + span.body_start;
        let end = 4 + span.body_end;
        self.record_views.push((start, end, start));
        Ok(())
    }
    pub fn end_record_view(&mut self) {
        self.record_views.pop();
    }
    /// 该 record 的**体长度**（字节）：普通对象体 8、多态对象体 10/14。**不计费、不物化、不改变
    /// 游标或任何解码状态**——只读 [`Self::record_span`] 已校验的记录索引。
    ///
    /// **FFI 专用接缝**（R36）：C ABI 解码多态对象时须在核心入口收费之前判定「普通体 vs 多态体」。
    /// [`Self::begin_record_view`] 会按完整体长计入 `max_decoded_bytes`，用它探测会让同一记录被收费
    /// 两次，并抢在 `enter_object`/`enter_polymorphic_object` 的深度判定之前报 `6`。未知/越界 id
    /// 沿用索引查询的 `7 InvalidReference`。非 FFI 调用方不应依赖本入口。
    #[doc(hidden)]
    pub fn record_body_len(&mut self, id: u32) -> Result<usize> {
        let span = self.record_span(u64::from(id))?;
        Ok(span.body_end - span.body_start)
    }
    fn body_range(&mut self, n: usize) -> Result<core::ops::Range<usize>> {
        let (_, end, pos) = self.record_views.last_mut().ok_or(Error::InvalidArgument)?;
        let next = pos
            .checked_add(n)
            .filter(|&next| next <= *end)
            .ok_or(Error::TruncatedInput)?;
        let range = *pos..next;
        *pos = next;
        Ok(range)
    }
    pub fn read_body_bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        let range = self.body_range(n)?;
        Ok(self.raw[range].to_vec())
    }
    /// 只推进当前记录视图的游标 `n` 字节，不复制。边界检查与 [`Self::read_body_bytes`] 同一
    /// `body_range`（越界仍报 `TruncatedInput`），供已自行消费该段的调用方丢掉返回值时使用。
    pub(crate) fn advance_body(&mut self, n: usize) -> Result<()> {
        self.body_range(n)?;
        Ok(())
    }
    /// Read the rest of the innermost record view as owned bytes.
    pub fn read_body_to_end(&mut self) -> Result<Vec<u8>> {
        let (_, end, pos) = self.record_views.last().ok_or(Error::InvalidArgument)?;
        let remaining = *end - *pos;
        self.read_body_bytes(remaining)
    }
    /// Borrow the unread bytes of the innermost record view without advancing it.
    pub(crate) fn peek_body(&self) -> Result<&[u8]> {
        let (_, end, pos) = self.record_views.last().ok_or(Error::InvalidArgument)?;
        Ok(&self.raw[*pos..*end])
    }
    /// Reject a collection count beyond `max_collection_len` or the u32 count range.
    /// Call after reading the count and before allocating its elements.
    ///
    /// **FFI 专用接缝**（R35）：与内建 `Vec`/`BTreeMap` 解码共用同一上限与错误码 `6`，使跨 crate 的
    /// 适配器不必复制长度判定。非 FFI 调用方不应依赖本入口。
    #[doc(hidden)]
    pub fn check_collection_len(&mut self, count: usize) -> Result<()> {
        if count > self.opts.max_collection_len || count > u32::MAX as usize {
            return Err(Error::ResourceLimit);
        }
        Ok(())
    }
    pub fn read_element_id(&mut self) -> Result<u32> {
        let range = self.body_range(4)?;
        Ok(u32::from_le_bytes(self.raw[range].try_into().unwrap()))
    }
    pub fn begin_element(&mut self) {
        self.element_depth = self.element_depth.saturating_add(1);
    }
    pub fn end_element(&mut self) {
        self.element_depth = self.element_depth.saturating_sub(1);
    }
    pub fn read_element_bytes(&mut self, n: usize) -> Result<&[u8]> {
        if self.element_depth == 0 {
            return Err(Error::InvalidArgument);
        }
        let range = self.body_range(n)?;
        Ok(&self.raw[range])
    }
    fn slots(&self) -> SlotStream<'_> {
        SlotStream::parse(&self.raw[self.assist_start..], &self.opts)
            .expect("validated slot stream")
    }
    pub fn root_is_none(&self) -> bool {
        self.assist_start == self.raw.len()
    }
    pub fn read_root_scalar(&mut self) -> Result<[u8; 8]> {
        if self.position != 0 {
            return Err(Error::InvalidArgument);
        }
        let slot = self.slots().get(0).ok_or(Error::MissingRequiredField)?;
        if slot.depth != 0 || slot.tag != 0 {
            return Err(Error::InvalidTag);
        }
        self.position = 1;
        Ok(slot.payload)
    }
    fn record(&mut self, payload: [u8; 8]) -> Result<RecordView<'_>> {
        let span = self.record_span(u64::from_le_bytes(payload))?;
        self.charge_materialization(span.body_end - span.body_start)?;
        let body = self
            .raw
            .get(4 + span.body_start..4 + span.body_end)
            .filter(|_| 4 + span.body_end <= self.main_end)
            .ok_or(Error::TruncatedInput)?;
        Ok(RecordView { body })
    }
    pub fn read_root_record(&mut self) -> Result<RecordView<'_>> {
        let payload = self.read_root_scalar()?;
        self.record(payload)
    }
    /// 当前对象帧中的字段查找（设计 §2 第 5、6 项）：跳过 tag 小于 `order` 的未知槽（U3），
    /// 返回逻辑位置及该位置上 tag 恰为 `order` 的槽。只读，不推进游标。
    fn find_field(&self, order: u32) -> Result<(i32, usize, Option<Slot>)> {
        let tag = i32::try_from(order).map_err(|_| Error::InvalidArgument)?;
        let frame = self.frames.last().ok_or(Error::InvalidArgument)?;
        if frame.last_order.is_some_and(|last| tag <= last) {
            return Err(Error::InvalidArgument);
        }
        let slots = self.slots();
        let mut position = frame.position;
        while position < frame.end && slots.get(position).ok_or(Error::InvalidTag)?.tag < tag {
            position += 1;
        }
        let found = slots
            .get(position)
            .filter(|slot| position < frame.end && slot.tag == tag);
        Ok((tag, position, found))
    }
    fn field(&mut self, order: u32) -> Result<Option<[u8; 8]>> {
        let (tag, position, found) = self.find_field(order)?;
        let frame = self.frames.last_mut().ok_or(Error::InvalidArgument)?;
        frame.last_order = Some(tag);
        frame.position = if found.is_some() {
            position + 1
        } else {
            position
        };
        Ok(found.map(|slot| slot.payload))
    }
    /// Select the next field, consuming its slot only when present.
    pub fn begin_field(&mut self, order: u32) -> Result<bool> {
        let payload = self.field(order)?;
        self.selected = Some(payload);
        Ok(payload.is_some())
    }
    pub fn selected_is_none(&self) -> bool {
        match self.selected {
            Some(payload) => payload.is_none(),
            None => self.root_is_none(),
        }
    }
    pub fn decode_scalar(&mut self) -> Result<[u8; 8]> {
        match self.selected {
            Some(payload) => Ok(payload.unwrap_or([0; 8])),
            None => self.read_root_scalar(),
        }
    }
    pub fn decode_required_scalar(&mut self) -> Result<[u8; 8]> {
        match self.selected {
            Some(payload) => payload.ok_or(Error::MissingRequiredField),
            None => self.read_root_scalar(),
        }
    }
    pub fn decode_record(&mut self) -> Result<RecordView<'_>> {
        match self.selected {
            Some(payload) => self.record(payload.ok_or(Error::MissingRequiredField)?),
            None => self.read_root_record(),
        }
    }
    /// Inspect presence without consuming the field or advancing its order.
    pub fn has_field(&self, order: u32) -> Result<bool> {
        Ok(self.find_field(order)?.2.is_some())
    }
    pub fn read_field_scalar(&mut self, order: u32) -> Result<[u8; 8]> {
        Ok(self.field(order)?.unwrap_or([0; 8]))
    }
    pub fn read_field_option_scalar(&mut self, order: u32) -> Result<Option<[u8; 8]>> {
        self.field(order)
    }
    pub fn read_field_record(&mut self, order: u32) -> Result<RecordView<'_>> {
        let payload = self.field(order)?.ok_or(Error::MissingRequiredField)?;
        self.record(payload)
    }
    pub fn read_field_option_record(&mut self, order: u32) -> Result<Option<RecordView<'_>>> {
        match self.field(order)? {
            Some(payload) => self.record(payload).map(Some),
            None => Ok(None),
        }
    }
    pub fn read_field_string(&mut self, order: u32) -> Result<String> {
        Ok(String::from(self.read_field_record(order)?.as_str()?))
    }
    pub fn read_field_bytes(&mut self, order: u32) -> Result<Vec<u8>> {
        Ok(self.read_field_record(order)?.as_bytes().to_vec())
    }
    /// 打开的对象帧与嵌套集合数，即 `enter_object` 的深度 k（V4-1）。
    fn open(&self) -> usize {
        self.frames.len() + self.coll_depth
    }
    /// 进入引用的对象（设计 §2 第 2 项 ①–⑧）。`payload` 为完整 u64 引用；根对象取根槽载荷。
    /// 第 ②–⑦ 步任一失败时 open 不变、不压帧；打开 view 之后的失败先关闭 view（V4-3、V4-11）。
    pub fn enter_object(&mut self, payload: u64) -> Result<()> {
        let k = self.open();
        if k > self.opts.max_depth {
            return Err(Error::DepthExceeded);
        }
        let span = self.record_span(payload)?;
        self.charge_materialization(span.body_end - span.body_start)?;
        let start = 4 + span.body_start;
        self.record_views.push((start, 4 + span.body_end, start));
        let range = match self.read_object_block(span.inst_id, span.body_start, k) {
            Ok(range) => range,
            Err(error) => {
                self.record_views.pop();
                return Err(error);
            }
        };
        let first_slot = range.offset as usize;
        let saved_selected = self.selected.take();
        self.frames.push(ObjectFrame {
            position: first_slot,
            end: first_slot + range.count as usize,
            last_order: None,
            saved_selected,
        });
        Ok(())
    }
    /// 读取恰好 8 字节的对象体（设计 §2 第 2 项 ⑥）并校验其槽范围（第 7 项）。须在 `enter_object` 打开的 view 内调用。
    fn read_object_block(&mut self, id: i32, body_start: usize, k: usize) -> Result<SlotRange> {
        let body = self.body_range(OBJECT_BODY_BYTES)?;
        if !self.peek_body()?.is_empty() {
            return Err(Error::InvalidTag);
        }
        let range = SlotRange::parse_body(&self.raw[body])?;
        self.check_object_range(id, body_start, range, k)?;
        Ok(range)
    }
    /// 进入多态对象（FR-W07）：读取 variant tag 与可选 type_name，并校验对象槽范围；
    /// 返回 `(tag, type_name)`。数值 tag 的变体匹配在派生侧（无匹配别名→4 InvalidTag）。
    /// tag 65535 的 type_name 记录不存在/越界→7 InvalidReference，内容非法 UTF-8→8 InvalidUtf8。
    pub fn enter_polymorphic_object(&mut self, payload: u64) -> Result<(u16, Option<String>)> {
        let k = self.open();
        if k > self.opts.max_depth {
            return Err(Error::DepthExceeded);
        }
        let span = self.record_span(payload)?;
        self.charge_materialization(span.body_end - span.body_start)?;
        let start = 4 + span.body_start;
        self.record_views.push((start, 4 + span.body_end, start));
        let parsed = self.read_polymorphic_block(span.inst_id, span.body_start, k);
        match parsed {
            Ok(body) => {
                let first_slot = body.range.offset as usize;
                let saved_selected = self.selected.take();
                self.frames.push(ObjectFrame {
                    position: first_slot,
                    end: first_slot + body.range.count as usize,
                    last_order: None,
                    saved_selected,
                });
                Ok((body.tag, body.type_name))
            }
            Err(error) => {
                self.record_views.pop();
                Err(error)
            }
        }
    }
    /// 读取多态对象体（FR-W07）并校验其槽范围；须在 `enter_polymorphic_object` 打开的 view 内调用。
    fn read_polymorphic_block(&mut self, id: i32, body_start: usize, k: usize) -> Result<PolyBody> {
        let tag_range = self.body_range(2)?;
        let tag = self.read_u16(tag_range)?;
        let type_name = if tag == POLY_ESCAPE_TAG {
            let type_name_range = self.body_range(4)?;
            let type_name_id = self.read_u32(type_name_range)?;
            Some(self.read_type_name(type_name_id)?)
        } else {
            None
        };
        let body = self.body_range(OBJECT_BODY_BYTES)?;
        if !self.peek_body()?.is_empty() {
            return Err(Error::InvalidTag);
        }
        let range = SlotRange::parse_body(&self.raw[body])?;
        self.check_object_range(id, body_start, range, k)?;
        Ok(PolyBody {
            tag,
            type_name,
            range,
        })
    }
    /// 读 2 字节小端字段；view 边界已由 `body_range` 校验，越界仍报 `TruncatedInput`。
    fn read_u16(&self, range: core::ops::Range<usize>) -> Result<u16> {
        self.raw
            .get(range)
            .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
            .map(u16::from_le_bytes)
            .ok_or(Error::TruncatedInput)
    }
    /// 读 4 字节小端字段；view 边界已由 `body_range` 校验，越界仍报 `TruncatedInput`。
    fn read_u32(&self, range: core::ops::Range<usize>) -> Result<u32> {
        self.raw
            .get(range)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
            .ok_or(Error::TruncatedInput)
    }
    /// 读逃生口的 type_name 记录体（FR-W07、FR-W08）：不存在/越界→7，非法 UTF-8→8。
    fn read_type_name(&mut self, id: u32) -> Result<String> {
        let span = self.record_span(u64::from(id))?;
        self.charge_materialization(span.body_end - span.body_start)?;
        let body = self
            .raw
            .get(4 + span.body_start..4 + span.body_end)
            .filter(|_| 4 + span.body_end <= self.main_end)
            .ok_or(Error::TruncatedInput)?;
        value::decode_str(body).map(String::from)
    }
    /// 对象范围的结构校验（设计 §2 第 3、4、7 项）。首次进入时校验范围，并与物理相邻的已知对象比较顺序与重叠；
    /// 同一 record 再次进入时复用结果，仅核对进入深度。
    fn check_object_range(
        &mut self,
        id: i32,
        body_start: usize,
        range: SlotRange,
        k: usize,
    ) -> Result<()> {
        if let Some(known) = self.known_objects.get(&body_start) {
            if known.slots.count != 0 && known.entry_depth != k {
                return Err(Error::InvalidTag);
            }
            return Ok(());
        }
        let previous = self
            .known_objects
            .range(..body_start)
            .next_back()
            .map(|(_, known)| (known.id, known.slots));
        let next = self
            .known_objects
            .range(body_start..)
            .next()
            .map(|(_, known)| (known.id, known.slots));
        // 邻居至多三个（前、本、后）；栈上定长数组避免每个对象一次堆分配，顺序与原收集结果一致。
        let mut neighbours = [(0i32, range); 3];
        let mut len = 0usize;
        if let Some(previous) = previous {
            neighbours[len] = previous;
            len += 1;
        }
        neighbours[len] = (id, range);
        len += 1;
        if let Some(next) = next {
            neighbours[len] = next;
            len += 1;
        }
        SlotRange::validate_ranges(&neighbours[..len], self.slots().len(), &self.opts)?;
        self.check_slot_structure(range, k)?;
        self.known_objects.insert(
            body_start,
            KnownObject {
                id,
                slots: range,
                entry_depth: k,
            },
        );
        Ok(())
    }
    /// 逐槽检查（设计 §2 第 3 项，V4-2）：线上 depth 超过 max_depth+1 → DepthExceeded；不等于 k+1 → InvalidTag；
    /// tag 须严格递增，否则 InvalidTag。
    fn check_slot_structure(&self, range: SlotRange, k: usize) -> Result<()> {
        let slots = self.slots();
        let limit = i64::try_from(self.opts.max_depth)
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        let expected = i64::try_from(k).unwrap_or(i64::MAX).saturating_add(1);
        let first_slot = range.offset as usize;
        let end_slot = first_slot + range.count as usize;
        let mut previous: Option<i32> = None;
        for position in first_slot..end_slot {
            let slot = slots.get(position).ok_or(Error::InvalidTag)?;
            let depth = i64::from(slot.depth);
            if depth > limit {
                return Err(Error::DepthExceeded);
            }
            if depth != expected || previous.is_some_and(|tag| slot.tag <= tag) {
                return Err(Error::InvalidTag);
            }
            previous = Some(slot.tag);
        }
        Ok(())
    }
    /// 离开当前对象（设计 §2 第 7 项，V4-4）。未读的未知槽无需处理，其结构已在进入时校验；
    /// 弹出对象帧、关闭进入时打开的 view，并恢复父帧的 selected 状态。
    pub fn leave_object(&mut self) -> Result<()> {
        let Some(frame) = self.frames.pop() else {
            return Err(Error::InvalidArgument);
        };
        self.record_views.pop();
        self.selected = frame.saved_selected;
        Ok(())
    }
    /// 进入一层集合（根集合与嵌套集合均计）：k 为进入前的 `open()`，超过 `max_depth` 时返回 `DepthExceeded`。
    pub(crate) fn enter_collection(&mut self) -> Result<()> {
        let k = self.open();
        if k > self.opts.max_depth {
            return Err(Error::DepthExceeded);
        }
        self.coll_depth = self.coll_depth.checked_add(1).ok_or(Error::DepthExceeded)?;
        Ok(())
    }
    /// 离开一层嵌套集合，须与 `enter_collection`／[`Self::enter_collection_checked`] 成对调用。
    ///
    /// **FFI 专用接缝**（R35）：进入成功后的正常返回与 `Result` 错误路径都要调用（与
    /// `encode_collection` 同口径），不构成 panic 展开后的清理承诺。
    #[doc(hidden)]
    pub fn leave_collection(&mut self) {
        self.coll_depth = self.coll_depth.saturating_sub(1);
    }
    /// **FFI 专用接缝**（R35）：进入一层集合，使跨 crate 的适配器在不复制集合解码语义的前提下
    /// 与内建 `Vec`/`BTreeMap` 共用同一套集合层计数。
    ///
    /// 进入的是集合层：`frames.len() + coll_depth` 随之加一，故随后解出的对象字段槽 `depth`
    /// 与编码侧一致；深度（`max_depth` → `5`）沿用既有判定，不另立限额或错误码。
    ///
    /// 失败时层数不变，调用方**不**得再调 [`Self::leave_collection`]；成功后须成对调用。
    /// 非 FFI 调用方不应依赖本接缝：集合元素的解码步骤仍属 `Vec`/`BTreeMap` 内建实现的职责。
    #[doc(hidden)]
    pub fn enter_collection_checked(&mut self) -> Result<()> {
        self.enter_collection()
    }
    /// 完成解码（设计 §2 第 8 项）：存在未离开的对象帧时拒绝。未读取的槽（未请求的字段或未知块）不是错误；
    /// 根槽无论是否读取都须为 depth 0、tag 0。
    pub fn finish(self) -> Result<()> {
        self.validate_finished()
    }
    pub(crate) fn validate_finished(&self) -> Result<()> {
        if !self.frames.is_empty() {
            return Err(Error::InvalidArgument);
        }
        if self.root_is_none() {
            return Ok(());
        }
        match self.slots().get(0) {
            Some(slot) if slot.depth == 0 && slot.tag == 0 => Ok(()),
            _ => Err(Error::InvalidTag),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RecordView<'d> {
    body: &'d [u8],
}
impl<'d> RecordView<'d> {
    pub fn as_str(&self) -> Result<&'d str> {
        value::decode_str(self.body)
    }
    pub fn as_bytes(&self) -> &'d [u8] {
        self.body
    }
    pub fn as_fixed<const N: usize>(&self) -> Result<&'d [u8; N]> {
        self.body.try_into().map_err(|_| Error::InvalidTag)
    }
    pub fn as_guid(&self) -> Result<value::Guid> {
        Ok(value::decode_guid(self.as_fixed()?))
    }
    pub fn as_datetime_offset(&self) -> Result<(i64, i16)> {
        value::decode_datetime_offset(self.as_fixed()?)
    }
}

mod graph;
pub use graph::BuildCtx;
pub use graph::DecodeCtx;
pub use graph::PlanValueLayout;

/// 完整消息根的两阶段图解码入口；字段必须沿用已有 DecodeCtx。
///
/// # Errors
/// 非完整消息根状态返回 InvalidArgument（1），且不启动图规划或构造。
/// 所有输入、身份、完成校验及构造预算错误均在构造任何图节点之前返回。
pub fn decode_cyclic<T: crate::schema::AceDecodeCyclic>(decoder: &mut Decoder<'_>) -> Result<T> {
    if !decoder.frames.is_empty()
        || decoder.selected.is_some()
        || !decoder.record_views.is_empty()
        || decoder.coll_depth != 0
        || decoder.element_depth != 0
        || decoder.position != 0
    {
        return Err(Error::InvalidArgument);
    }
    graph::decode_cyclic::<T>(decoder)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::Encoder;
    use alloc::{vec, vec::Vec};
    fn opts(packed: bool) -> Options {
        Options {
            packed,
            ..Options::default()
        }
    }
    fn slot(depth: i32, tag: i32, payload: [u8; 8]) -> Vec<u8> {
        [
            depth.to_le_bytes().as_slice(),
            tag.to_le_bytes().as_slice(),
            &payload,
        ]
        .concat()
    }
    fn raw(main: &[u8], slots: &[u8]) -> Vec<u8> {
        [
            (main.len() as i32).to_le_bytes().as_slice(),
            main,
            (slots.len() as i32).to_le_bytes().as_slice(),
            slots,
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
    #[test]
    fn record_view_bounds_and_decoded_budget() {
        let mut e = Encoder::new(&opts(false));
        e.write_root_record(b"abc").unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(
            &bytes,
            &Options {
                max_decoded_bytes: 3,
                ..opts(false)
            },
        )
        .unwrap();
        assert_eq!(d.read_body_bytes(1), Err(Error::InvalidArgument));
        d.begin_record_view(1).unwrap();
        assert_eq!(d.begin_record_view(1), Err(Error::ResourceLimit));
        assert_eq!(d.read_body_bytes(4), Err(Error::TruncatedInput));
        assert_eq!(d.read_body_bytes(3).unwrap(), b"abc");
        d.end_record_view();
        assert_eq!(d.begin_record_view(u32::MAX), Err(Error::InvalidReference));
    }
    #[test]
    fn round_trip_fields_and_storage_modes() {
        for packed in [false, true] {
            let o = opts(packed);
            let mut e = Encoder::new(&o);
            e.enter_object().unwrap();
            e.write_field_scalar(1, value::encode_i32(42)).unwrap();
            e.write_field_none(2).unwrap();
            e.write_field_record(3, "🙂".as_bytes()).unwrap();
            e.write_field_record(4, &[7; 16]).unwrap();
            e.write_field_record(
                5,
                &value::encode_datetime_offset(600_000_000_000, 60).unwrap(),
            )
            .unwrap();
            e.leave_object().unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(matches!(d.raw, Cow::Owned(_)), packed);
            let root = d.read_root_scalar().unwrap();
            d.enter_object(u64::from_le_bytes(root)).unwrap();
            assert_eq!(d.read_field_scalar(1), Ok(value::encode_i32(42)));
            assert_eq!(d.read_field_scalar(2), Ok([0; 8]));
            assert_eq!(d.read_field_string(3).unwrap(), "🙂");
            assert_eq!(
                d.read_field_record(4).unwrap().as_guid(),
                Ok(value::Guid([7; 16]))
            );
            assert_eq!(
                d.read_field_record(5).unwrap().as_datetime_offset(),
                Ok((600_000_000_000, 60))
            );
            d.leave_object().unwrap();
            assert_eq!(d.finish(), Ok(()));
        }
    }
    #[test]
    fn handwritten_tag_gap_and_required() {
        let mut e = Encoder::new(&opts(false));
        e.enter_object().unwrap();
        e.write_field_scalar(3, [9; 8]).unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.read_field_scalar(1), Ok([0; 8]));
        assert_eq!(d.read_field_scalar(2), Ok([0; 8]));
        assert_eq!(d.read_field_scalar(3), Ok([9; 8]));
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
        let mut e = Encoder::new(&opts(false));
        e.enter_object().unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.read_field_record(1), Err(Error::MissingRequiredField));
    }
    #[test]
    fn handwritten_none_and_zero_root() {
        let bytes = [0; 8];
        let d = Decoder::new(&bytes, &opts(false)).unwrap();
        assert!(d.root_is_none());
        assert_eq!(d.finish(), Ok(()));
        let bytes = raw(&[], &slot(0, 0, [0; 8]));
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        assert!(!d.root_is_none());
        assert_eq!(d.read_root_scalar(), Ok([0; 8]));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn strings_and_handwritten_invalid_utf8() {
        let long = "x".repeat(130);
        for s in ["", "🙂", long.as_str(), "a\0b"] {
            let o = opts(false);
            let mut e = Encoder::new(&o);
            e.write_root_record(s.as_bytes()).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(d.read_root_record().unwrap().as_str(), Ok(s));
            assert_eq!(d.finish(), Ok(()));
        }
        let bytes = raw(
            &[1, 0, 0, 0, 1, 0, 0, 0, 255],
            &slot(0, 0, 1u64.to_le_bytes()),
        );
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        assert_eq!(
            d.read_root_record().unwrap().as_str(),
            Err(Error::InvalidUtf8)
        );
    }
    #[test]
    fn wide_errors_and_datetime_validation() {
        for size in [9, 15] {
            let o = opts(false);
            let mut e = Encoder::new(&o);
            e.write_root_record(&vec![0; size]).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            let v = d.read_root_record().unwrap();
            assert_eq!(v.as_guid(), Err(Error::InvalidTag));
            assert_eq!(v.as_datetime_offset(), Err(Error::InvalidTag));
        }
        assert_eq!(value::decode_datetime(3u64 << 62), Err(Error::InvalidTag));
        let mut invalid = [0; 10];
        invalid[8..].copy_from_slice(&841i16.to_le_bytes());
        let o = opts(false);
        let mut e = Encoder::new(&o);
        e.write_root_record(&invalid).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &o).unwrap();
        assert_eq!(
            d.read_root_record().unwrap().as_datetime_offset(),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn depth_and_extra_slots() {
        // V4-1：根对象 k=0、子对象 k=1 满足 max_depth=1；孙对象 k=2 超限，进入即 DepthExceeded。
        let mut e = Encoder::new(&opts(false));
        e.enter_object().unwrap();
        e.enter_object().unwrap();
        e.enter_object().unwrap();
        e.write_field_scalar(1, [3; 8]).unwrap();
        let leaf_id = e.leave_object().unwrap();
        let leaf_ref = u64::from(leaf_id).to_le_bytes();
        e.write_field_scalar(1, leaf_ref).unwrap();
        let inner_id = e.leave_object().unwrap();
        let inner_ref = u64::from(inner_id).to_le_bytes();
        e.write_field_scalar(1, inner_ref).unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let o = Options {
            max_depth: 1,
            ..opts(false)
        };
        let mut d = Decoder::new(&bytes, &o).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        let inner = d.read_field_scalar(1).unwrap();
        d.enter_object(u64::from_le_bytes(inner)).unwrap();
        let leaf = d.read_field_scalar(1).unwrap();
        assert_eq!(
            d.enter_object(u64::from_le_bytes(leaf)),
            Err(Error::DepthExceeded)
        );
        d.leave_object().unwrap();
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn known_slot_depth_errors() {
        // V4-2：根对象 k=0 的字段槽线上 depth 须为 1；depth 3 超过 max_depth+1 为 DepthExceeded，depth 2 不等于 1 为 InvalidTag。
        let o = Options {
            max_depth: 1,
            ..opts(false)
        };
        for (depth, error) in [(3, Error::DepthExceeded), (2, Error::InvalidTag)] {
            let root_slot = slot(0, 0, 1u64.to_le_bytes());
            let bytes = raw(
                &record(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
                &[root_slot, slot(depth, 1, [5; 8])].concat(),
            );
            let mut d = Decoder::new(&bytes, &o).unwrap();
            let root = d.read_root_scalar().unwrap();
            assert_eq!(d.enter_object(u64::from_le_bytes(root)), Err(error));
        }
    }
    #[test]
    fn unreferenced_extra_slots_pass_finish() {
        let bytes = raw(&[], &[slot(0, 0, [0; 8]), slot(1, 1, [1; 8])].concat());
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        d.read_root_scalar().unwrap();
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn reference_order_and_options() {
        let o = opts(false);
        let mut e = Encoder::new(&o);
        e.enter_object().unwrap();
        e.write_field_scalar(2, 42u64.to_le_bytes()).unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &o).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.read_field_option_record(1), Ok(None));
        assert_eq!(d.read_field_option_record(2), Err(Error::InvalidReference));
        assert_eq!(d.read_field_scalar(2), Err(Error::InvalidArgument));
    }
    #[test]
    fn nested_round_trip() {
        let o = opts(false);
        let mut e = Encoder::new(&o);
        e.enter_object().unwrap();
        e.write_field_scalar(1, [1; 8]).unwrap();
        e.enter_object().unwrap();
        e.write_field_scalar(1, [3; 8]).unwrap();
        let inner_id = e.leave_object().unwrap();
        let inner_ref = u64::from(inner_id).to_le_bytes();
        e.write_field_scalar(2, inner_ref).unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &o).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.read_field_scalar(1), Ok([1; 8]));
        let inner_payload = d.read_field_scalar(2).unwrap();
        d.enter_object(u64::from_le_bytes(inner_payload)).unwrap();
        assert_eq!(d.read_field_scalar(1), Ok([3; 8]));
        d.leave_object().unwrap();
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn malformed_record_body_is_truncated() {
        let bytes = raw(
            &[1, 0, 0, 0, 2, 0, 0, 0, 1],
            &slot(0, 0, 1u64.to_le_bytes()),
        );
        assert_eq!(
            Decoder::new(&bytes, &opts(false)).err(),
            Some(Error::TruncatedInput)
        );
    }
    #[test]
    fn packed_rejects_nonzero_padding() {
        let o = opts(true);
        let malformed = [1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0];
        let bytes = packed::pack(&malformed, &o).unwrap();
        assert_eq!(Decoder::new(&bytes, &o).err(), Some(Error::TruncatedInput));
    }
    #[test]
    fn option_scalar_presence_and_owned_bytes() {
        let o = opts(false);
        let mut e = Encoder::new(&o);
        e.enter_object().unwrap();
        e.write_field_option_scalar(1, Some([0; 8])).unwrap();
        e.write_field_none(2).unwrap();
        e.write_field_record(3, b"abc").unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &o).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.read_field_option_scalar(1), Ok(Some([0; 8])));
        assert_eq!(d.read_field_option_scalar(2), Ok(None));
        assert_eq!(d.read_field_bytes(3), Ok(b"abc".to_vec()));
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn decoded_budget_is_cumulative() {
        let o = opts(false);
        let mut e = Encoder::new(&o);
        e.enter_object().unwrap();
        e.write_field_record(1, b"ab").unwrap();
        e.write_field_record(2, b"cd").unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        // 根对象体 8 字节于进入时计费（V4-5）；"ab" 使累计恰为 10，"cd" 使累计 12 超限。
        let limited = Options {
            max_decoded_bytes: 10,
            ..o
        };
        let mut d = Decoder::new(&bytes, &limited).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.read_field_bytes(1), Ok(b"ab".to_vec()));
        assert_eq!(d.read_field_bytes(2), Err(Error::ResourceLimit));
    }
    #[test]
    fn collection_count_limit_is_checked() {
        let mut d = Decoder::new(
            &[0; 8],
            &Options {
                max_collection_len: 2,
                ..opts(false)
            },
        )
        .unwrap();
        assert_eq!(d.check_collection_len(2), Ok(()));
        assert_eq!(d.check_collection_len(3), Err(Error::ResourceLimit));
    }
    #[test]
    fn peek_body_is_non_consuming_and_budget_neutral() {
        let mut e = Encoder::new(&opts(false));
        e.write_root_record(&[1, 2, 3, 4]).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        d.begin_record_view(1).unwrap();
        assert_eq!(d.decoded, 4);
        assert_eq!(d.peek_body(), Ok(&[1, 2, 3, 4][..]));
        assert_eq!(d.decoded, 4);
        assert_eq!(d.peek_body(), Ok(&[1, 2, 3, 4][..]));
        assert_eq!(d.decoded, 4);
        assert_eq!(d.read_body_bytes(2), Ok(vec![1, 2]));
        assert_eq!(d.decoded, 4);
        assert_eq!(d.peek_body(), Ok(&[3, 4][..]));
        assert_eq!(d.decoded, 4);
        assert_eq!(d.read_body_to_end(), Ok(vec![3, 4]));
        assert_eq!(d.decoded, 4);
        assert_eq!(d.peek_body(), Ok(&[][..]));
        assert_eq!(d.decoded, 4);
    }
    #[test]
    fn peek_body_requires_open_view() {
        let d = Decoder::new(&[0; 8], &opts(false)).unwrap();
        assert_eq!(d.peek_body(), Err(Error::InvalidArgument));
    }
    #[test]
    fn siblings_same_depth_do_not_mix() {
        // AC-W02（需求:190）：同深度兄弟对象各自持有字段游标，读取一个兄弟不影响另一个；FR-W04（需求:69）：零值字段可省略，省略时 begin_field 返回 false。
        let mut e = Encoder::new(&opts(false));
        e.enter_object().unwrap();
        e.enter_object().unwrap();
        e.write_field_scalar(0, 100u64.to_le_bytes()).unwrap();
        e.write_field_scalar(7, 700u64.to_le_bytes()).unwrap();
        let id_a = e.leave_object().unwrap();
        e.write_field_scalar(1, u64::from(id_a).to_le_bytes())
            .unwrap();
        e.enter_object().unwrap();
        e.write_field_scalar(0, 200u64.to_le_bytes()).unwrap();
        e.write_field_scalar(3, [0; 8]).unwrap();
        e.write_field_scalar(7, 900u64.to_le_bytes()).unwrap();
        let id_b = e.leave_object().unwrap();
        e.write_field_scalar(2, u64::from(id_b).to_le_bytes())
            .unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        let a = d.read_field_scalar(1).unwrap();
        d.enter_object(u64::from_le_bytes(a)).unwrap();
        assert_eq!(d.read_field_scalar(0), Ok(100u64.to_le_bytes()));
        assert_eq!(d.begin_field(3), Ok(false));
        assert_eq!(d.read_field_scalar(7), Ok(700u64.to_le_bytes()));
        d.leave_object().unwrap();
        let b = d.read_field_scalar(2).unwrap();
        d.enter_object(u64::from_le_bytes(b)).unwrap();
        assert_eq!(d.read_field_scalar(0), Ok(200u64.to_le_bytes()));
        assert_eq!(d.read_field_scalar(3), Ok([0; 8]));
        assert_eq!(d.read_field_scalar(7), Ok(900u64.to_le_bytes()));
        d.leave_object().unwrap();
        // 根对象也须离开：finish 拒绝未离开的对象帧（设计 §2 第 8 项），与编码端的根 leave 对应。
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn unknown_record_with_bogus_ranges_is_skipped() {
        // AC-W03（需求:191）：未被任何槽引用的记录即使携带越界的对象范围也不被校验，读取与结束均不受影响。
        let main = [
            record(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            record(2, &[5, 0, 0, 0, 9, 0, 0, 0]),
            // 依据 AC-W03（需求:191）及返工决策 I1：未知记录的重叠（3、4）和逆序（5、6）范围均须机械跳过。
            record(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
            record(4, &[2, 0, 0, 0, 1, 0, 0, 0]),
            record(5, &[2, 0, 0, 0, 1, 0, 0, 0]),
            record(6, &[1, 0, 0, 0, 1, 0, 0, 0]),
        ]
        .concat();
        let slots = [
            slot(0, 0, 1u64.to_le_bytes()),
            slot(1, 1, [7; 8]),
            // 依据返工决策 I1：补入未引用槽，使伪造的重叠、逆序范围均落在槽数组内。
            slot(1, 2, [8; 8]),
        ]
        .concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(root)), Ok(()));
        assert_eq!(d.read_field_scalar(1), Ok([7; 8]));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn failed_child_enter_keeps_sibling_fields() {
        // 设计 §2 第 2 项（V4-11）：子对象进入在引用查找阶段失败时 open 不变、不压帧，同级字段游标保持不动。
        let mut e = Encoder::new(&opts(false));
        e.enter_object().unwrap();
        e.write_field_scalar(1, 42u64.to_le_bytes()).unwrap();
        e.write_field_scalar(2, 7u64.to_le_bytes()).unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.read_field_scalar(1), Ok(42u64.to_le_bytes()));
        // 依据设计:80 及返工决策 I3：父帧内完整 u64 非法引用校验失败后，仍须保持兄弟字段可读。
        assert_eq!(
            d.enter_object((1u64 << 32) | 1),
            Err(Error::InvalidReference)
        );
        assert_eq!(d.enter_object(42), Err(Error::InvalidReference));
        assert_eq!(d.read_field_scalar(2), Ok(7u64.to_le_bytes()));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn finish_rejects_open_object_frame() {
        // 设计 §2 第 8 项：存在未离开的对象帧时 finish 返回 InvalidArgument；离开该帧后同一编码可正常完成。
        let mut e = Encoder::new(&opts(false));
        e.enter_object().unwrap();
        e.write_field_scalar(1, 1u64.to_le_bytes()).unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        assert_eq!(d.finish(), Err(Error::InvalidArgument));
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn object_body_must_be_exactly_eight_bytes() {
        // V4-9 第 9 项、§2 第 2 项 ⑥、V4-11：对象体须恰为 8 字节，短于 8 字节为 TruncatedInput，长于 8 字节为 InvalidTag；
        // 进入失败时不压帧，同级字段的游标保持不动。
        // 主区按子先父后排列（子记录 id 1 在前，根记录 id 2 在后）：编码端约定为子先父后，
        // validate_ranges 要求物理次序上的 offset 不小于前一范围的末端。
        let root_body = [1, 0, 0, 0, 2, 0, 0, 0];
        let slots = [
            slot(0, 0, 2u64.to_le_bytes()),
            slot(1, 1, 1u64.to_le_bytes()),
            slot(1, 2, [2; 8]),
        ]
        .concat();
        for (child_body, error) in [
            (&[1, 0, 0, 0, 0, 0, 0][..], Error::TruncatedInput),
            (&[1, 0, 0, 0, 0, 0, 0, 0, 0][..], Error::InvalidTag),
        ] {
            let main = [record(1, child_body), record(2, &root_body)].concat();
            let bytes = raw(&main, &slots);
            let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
            let root = d.read_root_scalar().unwrap();
            d.enter_object(u64::from_le_bytes(root)).unwrap();
            let child = d.read_field_scalar(1).unwrap();
            assert_eq!(d.enter_object(u64::from_le_bytes(child)), Err(error));
            assert_eq!(d.read_field_scalar(2), Ok([2; 8]));
            assert_eq!(d.leave_object(), Ok(()));
            assert_eq!(d.finish(), Ok(()));
        }
        // 对照：恰为 8 字节的子体进入成功。
        let child_body = [1, 0, 0, 0, 0, 0, 0, 0];
        let main = [record(1, &child_body), record(2, &root_body)].concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        let child = d.read_field_scalar(1).unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(child)), Ok(()));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn reference_payload_is_full_u64_and_unknown_tag_peek_keeps_slot() {
        // V4-9 第 10 项、§2 第 2 项 ②、B2：引用载荷按完整 u64 校验，高位非零为 InvalidReference 而非截断为低 32 位；
        // 未知小 tag 的 begin_field 只跳过、不消费该槽，之后的 tag 仍可读取。
        let main = record(1, &[1, 0, 0, 0, 2, 0, 0, 0]);
        let slots = [
            slot(0, 0, 1u64.to_le_bytes()),
            slot(1, 1, [1; 8]),
            slot(1, 5, [5; 8]),
        ]
        .concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        assert_eq!(
            d.enter_object((1u64 << 32) | 1),
            Err(Error::InvalidReference)
        );
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(root)), Ok(()));
        // 依据设计:40、:77 及返工决策 I2：重复 has_field 只读、不消费未知小 tag 后的已知槽。
        assert_eq!(d.has_field(3), Ok(false));
        assert_eq!(d.has_field(3), Ok(false));
        assert_eq!(d.has_field(5), Ok(true));
        assert_eq!(d.has_field(5), Ok(true));
        assert_eq!(d.begin_field(3), Ok(false));
        assert_eq!(d.read_field_scalar(5), Ok([5; 8]));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn object_materialization_budget_is_exact_and_per_entry() {
        // V4-9 第 7 项、V4-5、V4-11：每次进入都按对象体 8 字节计入累计量，同一对象重复引用逐次计费；累计恰为上限时通过，
        // 超限的进入失败后同级字段仍可读取。Encoder 的首个顶层对象离开即写入根槽，之后不能再开顶层对象，故子对象嵌套在根内编码。
        let mut e = Encoder::new(&opts(false));
        e.enter_object().unwrap();
        e.enter_object().unwrap();
        e.write_field_scalar(1, [1; 8]).unwrap();
        let id_c = u64::from(e.leave_object().unwrap()).to_le_bytes();
        e.write_field_scalar(1, id_c).unwrap();
        e.write_field_scalar(2, id_c).unwrap();
        e.write_field_scalar(3, [3; 8]).unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let limited = Options {
            max_decoded_bytes: 24,
            ..opts(false)
        };
        let mut d = Decoder::new(&bytes, &limited).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        let child = d.read_field_scalar(1).unwrap();
        d.enter_object(u64::from_le_bytes(child)).unwrap();
        d.leave_object().unwrap();
        let child = d.read_field_scalar(2).unwrap();
        d.enter_object(u64::from_le_bytes(child)).unwrap();
        d.leave_object().unwrap();
        assert_eq!(d.read_field_scalar(3), Ok([3; 8]));
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
        let limited = Options {
            max_decoded_bytes: 23,
            ..opts(false)
        };
        let mut d = Decoder::new(&bytes, &limited).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        let child = d.read_field_scalar(1).unwrap();
        d.enter_object(u64::from_le_bytes(child)).unwrap();
        d.leave_object().unwrap();
        let child = d.read_field_scalar(2).unwrap();
        assert_eq!(
            d.enter_object(u64::from_le_bytes(child)),
            Err(Error::ResourceLimit)
        );
        assert_eq!(d.read_field_scalar(3), Ok([3; 8]));
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
        let limited = Options {
            max_decoded_bytes: 15,
            ..opts(false)
        };
        let mut d = Decoder::new(&bytes, &limited).unwrap();
        let root = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root)).unwrap();
        let child = d.read_field_scalar(1).unwrap();
        assert_eq!(
            d.enter_object(u64::from_le_bytes(child)),
            Err(Error::ResourceLimit)
        );
        assert_eq!(d.read_field_scalar(3), Ok([3; 8]));
        d.leave_object().unwrap();
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn object_range_offset_zero_is_invalid_tag_at_real_entry() {
        // 依据：BAD-03（docs/requirements.md:279）、设计 §2 第 4 项与 §5：偏移 0 指向根槽，不能属于对象范围，须经真实解码入口 enter_object 返回 InvalidTag。
        let main = [record(1, &[0; 8]), record(2, &[1, 0, 0, 0, 2, 0, 0, 0])].concat();
        let slots = [
            slot(0, 0, 2u64.to_le_bytes()),
            slot(1, 1, 1u64.to_le_bytes()),
            slot(1, 2, [2; 8]),
        ]
        .concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(root)), Ok(()));
        let child = d.read_field_scalar(1).unwrap();
        assert_eq!(
            d.enter_object(u64::from_le_bytes(child)),
            Err(Error::InvalidTag)
        );
        assert_eq!(d.read_field_scalar(2), Ok([2; 8]));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn object_range_past_assist_end_is_invalid_tag_at_real_entry() {
        // 依据：BAD-03（docs/requirements.md:279）、设计 §2 第 4 项与 §5：子对象范围 [3,5) 末端超过 4 个槽，须经真实解码入口 enter_object 返回 InvalidTag。
        let main = [
            record(1, &[1, 0, 0, 0, 2, 0, 0, 0]),
            record(2, &[3, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let slots = [
            slot(0, 0, 1u64.to_le_bytes()),
            slot(1, 1, 2u64.to_le_bytes()),
            slot(1, 2, [2; 8]),
            slot(2, 1, [3; 8]),
        ]
        .concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(root)), Ok(()));
        let child = d.read_field_scalar(1).unwrap();
        assert_eq!(
            d.enter_object(u64::from_le_bytes(child)),
            Err(Error::InvalidTag)
        );
        assert_eq!(d.read_field_scalar(2), Ok([2; 8]));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn overlapping_object_ranges_are_invalid_tag_at_second_entry() {
        // 依据：BAD-03（docs/requirements.md:279）、设计 §2 第 4 项与 §5：对象 B 的范围 [2,3) 与已校验的对象 A 的 [1,3) 重叠，须经真实解码入口 enter_object 返回 InvalidTag。
        let main = [
            record(1, &[1, 0, 0, 0, 2, 0, 0, 0]),
            record(2, &[2, 0, 0, 0, 1, 0, 0, 0]),
            record(3, &[3, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let slots = [
            slot(0, 0, 3u64.to_le_bytes()),
            slot(2, 1, [1; 8]),
            slot(2, 2, [2; 8]),
            slot(1, 1, 1u64.to_le_bytes()),
            slot(1, 2, 2u64.to_le_bytes()),
        ]
        .concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(root)), Ok(()));
        let a = d.read_field_scalar(1).unwrap();
        assert_eq!(a, 1u64.to_le_bytes());
        assert_eq!(d.enter_object(u64::from_le_bytes(a)), Ok(()));
        assert_eq!(d.leave_object(), Ok(()));
        let b = d.read_field_scalar(2).unwrap();
        assert_eq!(b, 2u64.to_le_bytes());
        assert_eq!(
            d.enter_object(u64::from_le_bytes(b)),
            Err(Error::InvalidTag)
        );
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn reversed_object_ranges_are_invalid_tag_at_second_entry() {
        // 依据：BAD-03（docs/requirements.md:279）、设计 §2 第 4 项与 §5：物理次序上后出现的对象 B 的范围 [1,2) 逆序于已校验的对象 A 的 [2,3)，须经真实解码入口 enter_object 返回 InvalidTag。
        let main = [
            record(1, &[2, 0, 0, 0, 1, 0, 0, 0]),
            record(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            record(3, &[3, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let slots = [
            slot(0, 0, 3u64.to_le_bytes()),
            slot(2, 1, [1; 8]),
            slot(2, 1, [2; 8]),
            slot(1, 1, 1u64.to_le_bytes()),
            slot(1, 2, 2u64.to_le_bytes()),
        ]
        .concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(root)), Ok(()));
        let a = d.read_field_scalar(1).unwrap();
        assert_eq!(a, 1u64.to_le_bytes());
        assert_eq!(d.enter_object(u64::from_le_bytes(a)), Ok(()));
        assert_eq!(d.leave_object(), Ok(()));
        let b = d.read_field_scalar(2).unwrap();
        assert_eq!(b, 2u64.to_le_bytes());
        assert_eq!(
            d.enter_object(u64::from_le_bytes(b)),
            Err(Error::InvalidTag)
        );
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
    #[test]
    fn duplicate_tag_in_object_is_invalid_tag_at_real_entry() {
        // 依据：BAD-03（docs/requirements.md:279）、设计 §2 第 4 项与 §5：同一对象内 tag 1 重复（tag 须严格递增），须经真实解码入口 enter_object 返回 InvalidTag。
        let main = [
            record(1, &[1, 0, 0, 0, 2, 0, 0, 0]),
            record(2, &[3, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let slots = [
            slot(0, 0, 2u64.to_le_bytes()),
            slot(2, 1, [1; 8]),
            slot(2, 1, [1; 8]),
            slot(1, 1, 1u64.to_le_bytes()),
            slot(1, 2, [2; 8]),
        ]
        .concat();
        let bytes = raw(&main, &slots);
        let mut d = Decoder::new(&bytes, &opts(false)).unwrap();
        let root = d.read_root_scalar().unwrap();
        assert_eq!(d.enter_object(u64::from_le_bytes(root)), Ok(()));
        let a = d.read_field_scalar(1).unwrap();
        assert_eq!(a, 1u64.to_le_bytes());
        assert_eq!(
            d.enter_object(u64::from_le_bytes(a)),
            Err(Error::InvalidTag)
        );
        assert_eq!(d.read_field_scalar(2), Ok([2; 8]));
        assert_eq!(d.leave_object(), Ok(()));
        assert_eq!(d.finish(), Ok(()));
    }
}
