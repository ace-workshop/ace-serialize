//! Schema value categories and value-level codecs.
//!
//! Scalar payloads implement FR-W12 and the bitwise default rule FR-W04;
//! strings implement FR-W08 and wide values implement FR-T08/FR-T12.
//! Record and slot assembly are outside this module.

pub mod value;

/// Categories of schema fields; only scalar, string and wide value codecs
/// are provided here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Scalar,
    Str,
    WideValue,
    Collection,
    Object,
    PolyEnum,
    Option,
}

use crate::{
    decode::Decoder,
    encode::{BulkInto, Encoder},
    error::{Error, Result},
    limits::Options,
};
use alloc::{
    boxed::Box,
    collections::{btree_map, BTreeMap},
    string::String,
    vec::Vec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementKind {
    Value {
        size: usize,
    },
    Ref,
    /// The element body length is not fixed. Such elements are never batched, and each one
    /// is self-delimiting.
    Variable,
}

pub trait AceElement: Sized {
    fn element_kind() -> ElementKind;
    /// Write only the element body, not a slot or count.
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()>;
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self>;
    /// 向已有元素解码（需求 R22①）：默认解码出新元素再写入目标位置。
    ///
    /// 覆写它可让集合元素复用自身缓冲——内建容器（`String`、`Vec<T>`）与 `#[derive]` 生成代码按
    /// 此覆写，`Vec<Vec<T>>` 的内层集合因此在目标已有足够容量时保留原有分配。既有手写 impl
    /// 无需改动（默认实现即可），不要求 `T: 'static`。
    fn decode_element_into(d: &mut Decoder<'_>, target: &mut Self) -> Result<()> {
        *target = Self::decode_element(d)?;
        Ok(())
    }
    /// 映射键的预算检查，在编码该键前调用。默认不限制；`String` 按 `max_map_string_key_bytes` 覆盖。
    #[doc(hidden)]
    fn check_map_key(&self, _opts: &Options) -> Result<()> {
        Ok(())
    }
    /// 映射键的解码入口，默认等同 `decode_element`；`String` 在复制记录体前执行键长预算。
    #[doc(hidden)]
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        Self::decode_element(d)
    }
    /// Return false to request element-by-element encoding.
    ///
    /// 手写 hook 不得依赖 `out` 的初始 `len`/`capacity`：库只保证它可被追加，不保证与拥有值
    /// 路径观察到同一个初值（用户 2026-11-18 裁决④）。
    fn encode_elements_bulk(_slice: &[Self], _out: &mut Vec<u8>) -> Result<bool> {
        Ok(false)
    }
    /// 零分配整段体直写（FR-W09、R14）：返回 `Some(write)` 时，`write(items, e)` 把 `u32 count`
    /// 与元素体**一次性**写进已打开的记录，不构造合成 `Vec`；默认 `None` 表示没有快路径，
    /// 调用方回落到 `encode_elements_bulk` 的 `&mut Vec<u8>` hook（手写 impl 的既有行为）。
    ///
    /// 只有「无 padding、小端内存字节即 wire 字节」的库内建定宽元素返回 `Some`：内存与 wire
    /// 不一致的类型（如 `value::DateTime`）必须保持 `None`，否则整段拷贝会写出错误字节。
    #[doc(hidden)]
    fn encode_elements_bulk_direct() -> Option<fn(&[Self], &mut Encoder<'_>) -> Result<()>> {
        None
    }
    /// 零分配批量写入（需求 R23）：把 `u32 count` 与元素体写进库交出的 `dst`，返回实际写入
    /// 长度。默认 `Ok(None)`＝未提供；已声明的实现本次返回 `Ok(None)`＝本次不适用。
    /// **恰好写满（`dst.len()` 恰为体长）必须成功**，只有放不下才返回既有 `1 InvalidArgument`；
    /// 不得 panic、不得静默截断、不得谎报。`Some(n)` 的 `n` 必须等于本次集合体的预期体长：
    /// 不符按既有 `1` 返回且**不提交**（不重跑回调、不改走旧路径）。
    ///
    /// 库只在体长可精确预知（定宽整段体／身份目标）且本类型声明了能力
    /// （[`Self::encode_elements_bulk_into_fn`] 返回 `Some`，或
    /// [`Self::encode_elements_bulk_direct`] 非 `None`）时调用它；`dst` 的长度**恰为本次集合
    /// 体长**（不是物理预留量）、只在本次回调期间借出，不得保存、不得跨其它回调或 arena
    /// 增长使用。未声明时库不做任何额外预留、也不调用本方法。
    #[doc(hidden)]
    fn ace_encode_elements_bulk_into(_slice: &[Self], _dst: &mut [u8]) -> Result<Option<usize>> {
        Ok(None)
    }
    /// R23 的能力声明访问器：`Some(write)` 表示本类型声明了零分配批量写入能力，`write` 即
    /// [`Self::ace_encode_elements_bulk_into`]。读取只返回函数指针、**不执行任何用户回调**，
    /// 故「是否声明」可在任何物理预留之前判定；已声明者本次仍可能合法返回 `Ok(None)`，
    /// 不得以调用结果推断覆写。
    #[doc(hidden)]
    fn encode_elements_bulk_into_fn() -> Option<fn(&[Self], &mut [u8]) -> Result<Option<usize>>> {
        None
    }
    /// 把 `count` 个元素解码进 `out`，替换其原有内容。返回 false 时 out 未被修改。
    fn decode_elements_bulk_into(
        _out: &mut Vec<Self>,
        _count: usize,
        _bytes: &[u8],
        _consumed: &mut usize,
    ) -> Result<bool> {
        Ok(false)
    }
    fn decode_elements_bulk(
        count: usize,
        bytes: &[u8],
        consumed: &mut usize,
    ) -> Result<Option<Vec<Self>>> {
        let mut out = Vec::new();
        Ok(Self::decode_elements_bulk_into(&mut out, count, bytes, consumed)?.then_some(out))
    }
}

/// Whole-slice copies for padding-free primitives whose little-endian bytes are their wire bytes
/// (FR-W09). Compiled only on little-endian targets; big-endian targets take the element-by-element
/// path. The type system cannot check the copy contract, so each helper is an `unsafe fn` that
/// requires: `T` has no padding and its little-endian bytes are its memory representation; and
/// `validate` rejects every invalid bit pattern of `T`. `decode_slice_into` bounds-checks `bytes`
/// for `count` elements itself, before any copy. See each helper's `# Safety` section.
#[cfg(target_endian = "little")]
mod unsafe_bulk {
    use crate::error::{Error, Result};
    use alloc::vec::Vec;

    /// The bytes of `slice`'s memory, for one whole-segment copy. Encoders that write into the
    /// caller's sink (a record body) copy this view once instead of producing a `Vec`.
    ///
    /// # Safety
    ///
    /// `T` has no padding, so every byte of the view is initialized, and `T`'s in-memory bytes are
    /// its wire bytes on little-endian hosts. No caller-supplied bound applies: the view covers
    /// only `slice`'s own memory.
    pub(super) unsafe fn slice_bytes<T>(slice: &[T]) -> &[u8] {
        // SAFETY: `slice` is a live shared borrow, so `as_ptr()` is valid for reads of
        // `size_of_val(slice)` bytes for the whole call. Every byte is initialized because
        // `T` has no padding. A `u8` view needs alignment 1, which every pointer has.
        unsafe {
            core::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), core::mem::size_of_val(slice))
        }
    }

    /// Appends the wire bytes of `slice` as one raw copy. `_to_le_bytes` is not called: this
    /// module exists only on little-endian targets, where the in-memory bytes are the wire bytes.
    ///
    /// # Safety
    ///
    /// 1. `T` has no padding, and `_to_le_bytes` yields the bytes of `T`'s in-memory
    ///    representation on little-endian hosts.
    /// 2. Not applicable: encoding has no `validate`, and every `T` in `slice` is already a
    ///    valid value because `slice` is a `&[T]`.
    /// 3. Not applicable: the copy reads only `slice`'s own memory, so no caller-supplied
    ///    bound applies.
    pub(super) unsafe fn encode_slice<T: Copy, const N: usize>(
        slice: &[T],
        out: &mut Vec<u8>,
        _to_le_bytes: fn(T) -> [u8; N],
    ) {
        // SAFETY: precondition 1 is this function's precondition 1, and `out` is a unique borrow
        // that cannot alias `slice`, so the bytes are not written while read.
        let bytes = unsafe { slice_bytes(slice) };
        out.extend_from_slice(bytes);
    }
    /// Bounds-checks `count` elements against `bytes` before any allocation sized by `count`.
    fn checked_elements<T>(count: usize, bytes: &[u8]) -> Result<&[u8]> {
        let len = count
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(Error::TruncatedInput)?;
        bytes.get(..len).ok_or(Error::TruncatedInput)
    }
    /// Replaces the contents of `out` with `count` elements copied in one step. `validate`
    /// inspects the raw wire bytes first, so an invalid `bool` or `char` fails before `out` is
    /// changed.
    ///
    /// # Safety
    ///
    /// 1. `T` has no padding, and its little-endian bytes are its in-memory representation on
    ///    little-endian hosts, so copying the bytes yields the values `decode_le` would produce
    ///    for any input that `validate` accepts.
    /// 2. `validate` rejects every invalid bit pattern of `T`. A type with none passes a
    ///    constant `Ok(())`.
    /// 3. This function bounds-checks `bytes` for `count` elements itself, with
    ///    `checked_elements`, before any copy; the caller has no additional bounds obligation.
    /// 4. The copy writes `count` elements into the capacity that `reserve_exact(count)`
    ///    guarantees. `out` is a unique `&mut` borrow and cannot overlap `data`, which borrows
    ///    from `bytes`.
    /// 5. `out` is cleared before the copy, so its length is 0 and `set_len(count)` exposes only
    ///    the copied elements.
    pub(super) unsafe fn decode_slice_into<T: Copy>(
        out: &mut Vec<T>,
        count: usize,
        bytes: &[u8],
        consumed: &mut usize,
        validate: fn(&[u8]) -> Result<()>,
        _decode_le: fn(&[u8]) -> Result<T>,
    ) -> Result<()> {
        let data = checked_elements::<T>(count, bytes)?;
        validate(data)?;
        out.clear();
        out.reserve_exact(count);
        // SAFETY: `data` is a live `&[u8]` of `data.len()` bytes, valid for that many reads.
        // `reserve_exact(count)` gives `out` room for `count` elements of `size_of::<T>()` bytes,
        // which is exactly `data.len()` bytes, so the destination is valid for that many writes.
        // `checked_elements` bounded `count` by the input, so the allocation never exceeds the
        // input length. `out` is a unique borrow and `data` borrows from `bytes`, so they cannot
        // overlap. `u8` needs alignment 1 only. After the copy every element is initialized and
        // valid: `validate` rejected invalid `bool`/`char` bytes, and the other supported types
        // accept any bit pattern. `out` was cleared, so `set_len` exposes only the copied elements,
        // and it runs only after the copy.
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                out.as_mut_ptr().cast::<u8>(),
                data.len(),
            );
            out.set_len(count);
        }
        *consumed = data.len();
        Ok(())
    }
    /// Rejects any `bool` byte other than 0 or 1.
    pub(super) fn validate_bool(bytes: &[u8]) -> Result<()> {
        if bytes.iter().all(|&byte| byte <= 1) {
            Ok(())
        } else {
            Err(Error::InvalidTag)
        }
    }
    /// Rejects any little-endian `u32` group that is not a Unicode scalar value.
    // `as_chunks` is the lint's suggestion, but it needs Rust 1.88 and the crate declares no MSRV.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    pub(super) fn validate_char(bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks_exact(core::mem::size_of::<u32>()) {
            let scalar = u32::from_le_bytes(chunk.try_into().unwrap());
            char::from_u32(scalar).ok_or(Error::InvalidUtf8)?;
        }
        Ok(())
    }
}

macro_rules! element {
    ($ty:ty, $size:expr, $encode:expr, $decode:expr) => {
        element!($ty, $size, $encode, $decode, |_| Ok(()));
    };
    ($ty:ty, $size:expr, $encode:expr, $decode:expr, $validate:expr) => {
        const _: () = assert!(core::mem::size_of::<$ty>() == $size);
        impl AceElement for $ty {
            fn element_kind() -> ElementKind {
                ElementKind::Value { size: $size }
            }
            fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                crate::hooks::note_element();
                e.write_element_bytes(&($encode)(*self))
            }
            fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
                crate::hooks::note_element();
                ($decode)(d.read_element_bytes($size)?)
            }
            #[cfg(target_endian = "little")]
            fn encode_elements_bulk(slice: &[Self], out: &mut Vec<u8>) -> Result<bool> {
                // SAFETY: precondition 1 holds for each `$ty` in the `element!` list below: each is
                // a primitive or a `#[repr(transparent)]` newtype (`value::Guid`, `value::IntPtr`,
                // `value::UIntPtr`, `value::ToolkitsU64`), with no padding (`size_of::<$ty>() ==
                // $size` is asserted above), and `$encode` yields its in-memory bytes on
                // little-endian hosts. Those are `to_le_bytes` for the integers,
                // `to_bits().to_le_bytes()` for floats, `[v]` for `u8`, `[u8::from(v)]` for
                // `bool`, `u32::from(v).to_le_bytes()` for `char`, the array itself for `Guid`,
                // and `to_le_bytes` of the inner integer for the three 8-byte newtypes.
                // Preconditions 2 and 3 concern decoding only.
                unsafe { unsafe_bulk::encode_slice(slice, out, $encode) };
                crate::hooks::note_bulk();
                Ok(true)
            }
            #[cfg(target_endian = "little")]
            fn encode_elements_bulk_direct() -> Option<fn(&[Self], &mut Encoder<'_>) -> Result<()>>
            {
                // 与上面的 `encode_elements_bulk` 同一组 SAFETY 前提：`write_elements_direct`
                // 只把同一段内存视图一次性拷进记录。
                Some(write_elements_direct::<$ty>)
            }
            #[cfg(target_endian = "little")]
            fn decode_elements_bulk_into(
                out: &mut Vec<Self>,
                count: usize,
                bytes: &[u8],
                consumed: &mut usize,
            ) -> Result<bool> {
                // SAFETY: precondition 1 is as for encoding above. Precondition 2: `$validate` is
                // `validate_bool` for `bool`, which rejects any byte other than 0 or 1, and
                // `validate_char` for `char`, which rejects any 4-byte group that is not a Unicode
                // scalar value. Every other `$ty` accepts all bit patterns, so its `$validate` is
                // the constant `|_| Ok(())`. Precondition 3 has no caller obligation: the helper's
                // `checked_elements` bounds-checks `bytes` itself, before any copy.
                unsafe {
                    unsafe_bulk::decode_slice_into(out, count, bytes, consumed, $validate, $decode)
                }?;
                crate::hooks::note_bulk();
                Ok(true)
            }
        }
    };
}
element!(i8, 1, |v: i8| v.to_le_bytes(), |b: &[u8]| Ok(
    i8::from_le_bytes(b.try_into().unwrap())
));
element!(i16, 2, |v: i16| v.to_le_bytes(), |b: &[u8]| Ok(
    i16::from_le_bytes(b.try_into().unwrap())
));
element!(u16, 2, |v: u16| v.to_le_bytes(), |b: &[u8]| Ok(
    u16::from_le_bytes(b.try_into().unwrap())
));
element!(i32, 4, |v: i32| v.to_le_bytes(), |b: &[u8]| Ok(
    i32::from_le_bytes(b.try_into().unwrap())
));
element!(u32, 4, |v: u32| v.to_le_bytes(), |b: &[u8]| Ok(
    u32::from_le_bytes(b.try_into().unwrap())
));
element!(f32, 4, |v: f32| v.to_bits().to_le_bytes(), |b: &[u8]| Ok(
    f32::from_bits(u32::from_le_bytes(b.try_into().unwrap()))
));
element!(
    char,
    4,
    |v: char| u32::from(v).to_le_bytes(),
    |b: &[u8]| {
        let scalar = u32::from_le_bytes(b.try_into().unwrap());
        char::from_u32(scalar).ok_or(crate::error::Error::InvalidUtf8)
    },
    unsafe_bulk::validate_char
);
element!(i64, 8, |v: i64| v.to_le_bytes(), |b: &[u8]| Ok(
    i64::from_le_bytes(b.try_into().unwrap())
));
element!(u64, 8, |v: u64| v.to_le_bytes(), |b: &[u8]| Ok(
    u64::from_le_bytes(b.try_into().unwrap())
));
element!(u8, 1, |v: u8| [v], |b: &[u8]| Ok(b[0]));
element!(f64, 8, |v: f64| v.to_bits().to_le_bytes(), |b: &[u8]| Ok(
    f64::from_bits(u64::from_le_bytes(b.try_into().unwrap()))
));
element!(
    bool,
    1,
    |v: bool| [u8::from(v)],
    |b: &[u8]| match b[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(crate::error::Error::InvalidTag),
    },
    unsafe_bulk::validate_bool
);

element!(value::Guid, 16, value::encode_guid, |b: &[u8]| Ok(
    value::decode_guid(b.try_into().unwrap())
));
element!(
    value::IntPtr,
    8,
    |v: value::IntPtr| value::encode_intptr(v.0),
    |b: &[u8]| Ok(value::IntPtr(value::decode_intptr(b.try_into().unwrap())))
);
element!(
    value::UIntPtr,
    8,
    |v: value::UIntPtr| value::encode_uintptr(v.0),
    |b: &[u8]| Ok(value::UIntPtr(value::decode_uintptr(b.try_into().unwrap())))
);
element!(
    value::ToolkitsU64,
    8,
    |v: value::ToolkitsU64| value::encode_toolkits_guid(v.0 as i64),
    |b: &[u8]| Ok(value::ToolkitsU64(
        value::decode_toolkits_guid(b.try_into().unwrap()) as u64
    ))
);

#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::size_of::<isize>() == 8);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(core::mem::size_of::<usize>() == 8);

/// Hand-written rather than `element!`: bulk exists only on 64-bit little-endian targets, where
/// every 8-byte wire value fits the pointer width. Elsewhere the trait defaults decline bulk, and
/// on 32-bit hosts the element-by-element path reports `InvalidTag` for out-of-range values.
impl AceElement for isize {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 8 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.write_element_bytes(&value::encode_isize(*self))
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        value::decode_isize(d.read_element_bytes(8)?.try_into().unwrap())
    }
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn encode_elements_bulk(slice: &[Self], out: &mut Vec<u8>) -> Result<bool> {
        // SAFETY: precondition 1 holds: `isize` is an 8-byte integer with no padding on this cfg
        // (`size_of::<isize>() == 8` is asserted above), and `value::encode_isize` yields
        // `(v as i64).to_le_bytes()`, which is its in-memory representation on little-endian
        // 64-bit hosts. Preconditions 2 and 3 concern decoding only.
        unsafe { unsafe_bulk::encode_slice(slice, out, value::encode_isize) };
        crate::hooks::note_bulk();
        Ok(true)
    }
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn encode_elements_bulk_direct() -> Option<fn(&[Self], &mut Encoder<'_>) -> Result<()>> {
        // 与 `encode_elements_bulk` 同一前提：指针宽度 64 位、小端，内存字节即 wire 字节。
        Some(write_elements_direct::<isize>)
    }
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn decode_elements_bulk_into(
        out: &mut Vec<Self>,
        count: usize,
        bytes: &[u8],
        consumed: &mut usize,
    ) -> Result<bool> {
        // SAFETY: precondition 1 is as for `isize` encoding above. Precondition 2: every 8-byte
        // pattern is a valid `isize` on 64-bit targets, so the constant `|_| Ok(())` validator is
        // correct. Precondition 3 has no caller obligation: the helper's `checked_elements`
        // bounds-checks `bytes` itself, before any copy.
        unsafe {
            unsafe_bulk::decode_slice_into(
                out,
                count,
                bytes,
                consumed,
                |_| Ok(()),
                |b| value::decode_isize(b.try_into().unwrap()),
            )
        }?;
        crate::hooks::note_bulk();
        Ok(true)
    }
}

impl AceElement for usize {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 8 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.write_element_bytes(&value::encode_usize(*self))
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        value::decode_usize(d.read_element_bytes(8)?.try_into().unwrap())
    }
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn encode_elements_bulk(slice: &[Self], out: &mut Vec<u8>) -> Result<bool> {
        // SAFETY: precondition 1 holds: `usize` is an 8-byte integer with no padding on this cfg
        // (`size_of::<usize>() == 8` is asserted above), and `value::encode_usize` yields
        // `(v as u64).to_le_bytes()`, which is its in-memory representation on little-endian
        // 64-bit hosts. Preconditions 2 and 3 concern decoding only.
        unsafe { unsafe_bulk::encode_slice(slice, out, value::encode_usize) };
        crate::hooks::note_bulk();
        Ok(true)
    }
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn encode_elements_bulk_direct() -> Option<fn(&[Self], &mut Encoder<'_>) -> Result<()>> {
        // 与 `encode_elements_bulk` 同一前提：指针宽度 64 位、小端，内存字节即 wire 字节。
        Some(write_elements_direct::<usize>)
    }
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn decode_elements_bulk_into(
        out: &mut Vec<Self>,
        count: usize,
        bytes: &[u8],
        consumed: &mut usize,
    ) -> Result<bool> {
        // SAFETY: precondition 1 is as for `usize` encoding above. Precondition 2: every 8-byte
        // pattern is a valid `usize` on 64-bit targets, so the constant `|_| Ok(())` validator is
        // correct. Precondition 3 has no caller obligation: the helper's `checked_elements`
        // bounds-checks `bytes` itself, before any copy.
        unsafe {
            unsafe_bulk::decode_slice_into(
                out,
                count,
                bytes,
                consumed,
                |_| Ok(()),
                |b| value::decode_usize(b.try_into().unwrap()),
            )
        }?;
        crate::hooks::note_bulk();
        Ok(true)
    }
}

/// Hand-written rather than `element!`: each type is 16 bytes in memory, not its 8- or 10-byte
/// wire width, so the size assertion in `element!` cannot hold. Neither type overrides the bulk
/// methods, so both take the element-by-element path (FR-W09).
impl AceElement for value::DateTime {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 8 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        let bits = value::encode_datetime(self.ticks, self.kind).map_err(|err| e.fail(err))?;
        e.write_element_bytes(&bits.to_le_bytes())
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let bits = u64::from_le_bytes(d.read_element_bytes(8)?.try_into().unwrap());
        let (ticks, kind) = value::decode_datetime(bits)?;
        Ok(value::DateTime { ticks, kind })
    }
}

impl AceElement for value::DateTimeOffset {
    fn element_kind() -> ElementKind {
        ElementKind::Value { size: 10 }
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        let bytes = value::encode_datetime_offset(self.clock_ticks, self.offset_minutes)
            .map_err(|err| e.fail(err))?;
        e.write_element_bytes(&bytes)
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let bytes: &[u8; 10] = d.read_element_bytes(10)?.try_into().unwrap();
        let (clock_ticks, offset_minutes) = value::decode_datetime_offset(bytes)?;
        Ok(value::DateTimeOffset {
            clock_ticks,
            offset_minutes,
        })
    }
}

/// Element form of `String` (FR-W08, FR-W09). The element body is the little-endian `u32` id of
/// a separate record in `main`, whose body is the UTF-8 bytes. The empty string is a record of
/// size 0. A record body that is not valid UTF-8 fails with `InvalidUtf8`; it is never replaced
/// or truncated. String elements are never batched, so the bulk methods keep their defaults.
///
/// If encoding fails, the `Encoder` may hold an unclosed or orphaned record; the caller must
/// discard it.
impl AceElement for String {
    fn element_kind() -> ElementKind {
        ElementKind::Ref
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.begin_record()?;
        e.write_body_bytes(value::encode_str(self))?;
        let id = e.end_record()?;
        e.write_element_bytes(&id.to_le_bytes())
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
        d.begin_record_view(id)?;
        // Close the view before propagating an error from the read or the UTF-8 check.
        let body = d.read_body_to_end();
        d.end_record_view();
        Ok(String::from(value::decode_str(&body?)?))
    }
    /// 元素级复用（R22①）：清空原值后写入记录体的 UTF-8 字节，保留既有分配的容量。
    fn decode_element_into(d: &mut Decoder<'_>, target: &mut Self) -> Result<()> {
        crate::hooks::note_element();
        let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
        d.begin_record_view(id)?;
        let body = d.read_body_to_end();
        d.end_record_view();
        let body = body?;
        let text = value::decode_str(&body)?;
        target.clear();
        target.push_str(text);
        Ok(())
    }
    fn check_map_key(&self, opts: &Options) -> Result<()> {
        if self.len() > opts.max_map_string_key_bytes {
            Err(Error::ResourceLimit)
        } else {
            Ok(())
        }
    }
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
        d.begin_record_view(id)?;
        let too_long = d.peek_body()?.len() > d.options().max_map_string_key_bytes;
        // Reject before copying the body; the view is closed before the error propagates.
        let body = if too_long {
            Err(Error::ResourceLimit)
        } else {
            d.read_body_to_end()
        };
        d.end_record_view();
        Ok(String::from(value::decode_str(&body?)?))
    }
}

/// Element form of `Option<T>` (FR-W09). The presence byte comes first: 0 is `None` with no
/// body, and 1 is `Some` followed by the element body of `T`. Any other presence byte is
/// `InvalidTag`. A `None` element keeps its position in the body. Option elements are never
/// batched, so the bulk methods keep their defaults.
///
/// Each `encode_element` and `decode_element` call counts as one element, including the call
/// for the inner `T`, so a `Some` counts two.
///
/// If the inner encoding fails after the presence byte is written, the whole encoding fails, as
/// it does for a field-level `Option`; the caller must not use the `Encoder`'s output.
///
/// `Option<Option<T>>` is rejected at compile time, because `Option<T>` does not implement
/// [`NonOptional`]:
///
/// ```compile_fail
/// use ace_serialize::schema::AceElement;
/// fn accepts_element<T: AceElement>() {}
/// accepts_element::<Option<Option<i32>>>();
/// ```
impl<T: AceElement + NonOptional> AceElement for Option<T> {
    fn element_kind() -> ElementKind {
        ElementKind::Variable
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        match self {
            None => e.write_element_bytes(&[0]),
            Some(v) => {
                e.write_element_bytes(&[1])?;
                v.encode_element(e)
            }
        }
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        // Copy the byte so the borrow of `d` ends before the inner element is decoded.
        let presence = d.read_element_bytes(1)?[0];
        match presence {
            0 => Ok(None),
            1 => Ok(Some(T::decode_element(d)?)),
            _ => Err(crate::error::Error::InvalidTag),
        }
    }
    fn check_map_key(&self, opts: &Options) -> Result<()> {
        match self {
            Some(v) => v.check_map_key(opts),
            None => Ok(()),
        }
    }
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let presence = d.read_element_bytes(1)?[0];
        match presence {
            0 => Ok(None),
            1 => Ok(Some(T::decode_map_key(d)?)),
            _ => Err(crate::error::Error::InvalidTag),
        }
    }
}

/// Transparent element form: the element body is the inner body, with no record or presence byte
/// of its own. Encoding and key checks each take one `try_borrow`; a live mutable borrow is
/// `InvalidArgument`. Bulk keeps the trait default.
impl<T: AceElement> AceElement for core::cell::RefCell<T> {
    fn element_kind() -> ElementKind {
        T::element_kind()
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_borrow()
            .map_err(|_| e.fail(Error::InvalidArgument))?;
        inner.encode_element(e)
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        T::decode_element(d).map(core::cell::RefCell::new)
    }
    fn check_map_key(&self, opts: &Options) -> Result<()> {
        let inner = self.try_borrow().map_err(|_| Error::InvalidArgument)?;
        inner.check_map_key(opts)
    }
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        T::decode_map_key(d).map(core::cell::RefCell::new)
    }
}

/// Transparent element form for `RwLock`, std only: the element body is the inner body, with no
/// record or presence byte of its own. Encoding and key checks each take one `try_read`; a held
/// write lock or a poisoned lock is `InvalidArgument`. Bulk keeps the trait default.
#[cfg(feature = "std")]
impl<T: AceElement> AceElement for std::sync::RwLock<T> {
    fn element_kind() -> ElementKind {
        T::element_kind()
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_read()
            .map_err(|_| e.fail(Error::InvalidArgument))?;
        inner.encode_element(e)
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        T::decode_element(d).map(std::sync::RwLock::new)
    }
    fn check_map_key(&self, opts: &Options) -> Result<()> {
        let inner = self.try_read().map_err(|_| Error::InvalidArgument)?;
        inner.check_map_key(opts)
    }
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        T::decode_map_key(d).map(std::sync::RwLock::new)
    }
}

/// Encodes a schema value. Implementations used inside `Option` must also
/// implement [`NonOptional`]. Nested `Option` does not implement this marker,
/// so it cannot implement `AceEncode` or `AceDecode` through the option codecs.
///
/// ```compile_fail
/// use ace_serialize::schema::AceEncode;
/// fn accepts_encode<T: AceEncode>() {}
/// accepts_encode::<Option<Option<i32>>>();
/// ```
///
/// The trait itself is not `Sized`, so unsized values keep their encoders and can be named behind
/// a `?Sized` bound; the element-batch methods put `Self: Sized` on the method instead, so `str`
/// needs no bulk implementation of its own:
///
/// ```
/// use ace_serialize::schema::{value::DateTime, AceDecodeCyclic, AceEncode};
/// fn accepts_unsized<T: AceEncode + ?Sized>() {}
/// accepts_unsized::<str>();
/// assert_eq!(<i32 as AceEncode>::ace_element_value_size(), Some(4));
/// let mut out = Vec::new();
/// assert_eq!(<DateTime as AceEncode>::ace_encode_elements_bulk(&[], &mut out), Ok(false));
/// assert!(out.is_empty());
/// let mut consumed = 7;
/// assert_eq!(
///     <DateTime as AceDecodeCyclic>::decode_elements_bulk_plan(0, &[], &mut consumed),
///     Ok(None)
/// );
/// assert_eq!(consumed, 7);
/// ```
pub trait AceEncode {
    fn ace_encode(&self, encoder: &mut Encoder<'_>) -> Result<()>;
    #[doc(hidden)]
    fn ace_encode_target(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        encoder.capture_object(|e| self.ace_encode(e))
    }
    #[doc(hidden)]
    fn ace_encode_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        Err(encoder.fail(Error::InvalidArgument))
    }
    #[doc(hidden)]
    fn ace_encode_some_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        encoder.write_element_bytes(&[1])?;
        self.ace_encode_element(encoder)
    }
    /// Option 槽的 Some 分派，默认走普通编码。
    #[doc(hidden)]
    fn ace_encode_some(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        self.ace_encode(encoder)
    }
    /// 定宽元素体的 wire 字节数（非 Rust `size_of`）；非定宽元素返回 `None`。
    #[doc(hidden)]
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        None
    }
    /// 整段编码元素体并返回 `true`；返回 `false` 时不得修改 `out`，由调用方逐项编码。
    ///
    /// 手写实现不得依赖 `out` 的初始 `len`/`capacity`：库只保证它可被追加，不保证与拥有值
    /// 路径观察到同一个初值。
    #[doc(hidden)]
    fn ace_encode_elements_bulk(
        _items: &[Self],
        _out: &mut alloc::vec::Vec<u8>,
    ) -> crate::error::Result<bool>
    where
        Self: Sized,
    {
        Ok(false)
    }
    /// 整段体直写快路径（FR-W09、R14）：`Some(write)` 时 `write(items, e)` 把 `u32 count` 与
    /// 元素体一次性写进已打开的记录，不构造合成 `Vec`。默认 `None`，调用方回落到
    /// `ace_encode_elements_bulk` 的 `&mut Vec<u8>` hook。
    #[doc(hidden)]
    fn ace_encode_elements_bulk_direct(
    ) -> Option<fn(&[Self], &mut Encoder<'_>) -> crate::error::Result<()>>
    where
        Self: Sized,
    {
        None
    }
    /// 零分配批量写入的 `AceEncode` 侧镜像（需求 R23）：语义、`dst` 契约与失败面同
    /// [`AceElement::ace_encode_elements_bulk_into`]。默认 `Ok(None)`＝未提供。
    #[doc(hidden)]
    fn ace_encode_elements_bulk_into(
        _items: &[Self],
        _dst: &mut [u8],
    ) -> crate::error::Result<Option<usize>>
    where
        Self: Sized,
    {
        Ok(None)
    }
    /// R23 能力声明访问器的 `AceEncode` 侧镜像：读取只返回函数指针、不执行用户回调。
    #[doc(hidden)]
    fn ace_encode_elements_bulk_into_fn(
    ) -> Option<fn(&[Self], &mut [u8]) -> crate::error::Result<Option<usize>>>
    where
        Self: Sized,
    {
        None
    }
}
impl<T: AceEncode + 'static> AceEncode for alloc::rc::Rc<T> {
    fn ace_encode(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let id = encoder.resolve_rc(self)?;
        encoder.encode_scalar(u64::from(id).to_le_bytes())
    }
    fn ace_encode_target(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        encoder.capture_native(|e| self.ace_encode_element(e))
    }
    fn ace_encode_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let id = encoder.resolve_rc(self)?;
        encoder.write_element_bytes(&id.to_le_bytes())
    }
}
impl<T: 'static> AceEncode for alloc::rc::Weak<T> {
    fn ace_encode(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return Err(encoder.fail(Error::InvalidArgument));
        }
        let id = encoder.resolve_weak(self)?;
        encoder.encode_scalar(u64::from(id).to_le_bytes())
    }
    fn ace_encode_target(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        encoder.capture_native(|e| self.ace_encode_element(e))
    }
    fn ace_encode_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return Err(encoder.fail(Error::InvalidArgument));
        }
        let id = encoder.resolve_weak(self)?;
        encoder.write_element_bytes(&id.to_le_bytes())
    }
    fn ace_encode_some(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return encoder.encode_none();
        }
        let id = encoder.resolve_weak(self)?;
        encoder.encode_scalar(u64::from(id).to_le_bytes())
    }
    fn ace_encode_some_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return encoder.write_element_bytes(&[0]);
        }
        encoder.write_element_bytes(&[1])?;
        let id = encoder.resolve_weak(self)?;
        encoder.write_element_bytes(&id.to_le_bytes())
    }
}
#[cfg(feature = "std")]
impl<T: AceEncode + 'static> AceEncode for alloc::sync::Arc<T> {
    fn ace_encode(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let id = encoder.resolve_arc(self)?;
        encoder.encode_scalar(u64::from(id).to_le_bytes())
    }
    fn ace_encode_target(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        encoder.capture_native(|e| self.ace_encode_element(e))
    }
    fn ace_encode_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let id = encoder.resolve_arc(self)?;
        encoder.write_element_bytes(&id.to_le_bytes())
    }
}
#[cfg(feature = "std")]
impl<T: 'static> AceEncode for alloc::sync::Weak<T> {
    fn ace_encode(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return Err(encoder.fail(Error::InvalidArgument));
        }
        let id = encoder.resolve_sync_weak(self)?;
        encoder.encode_scalar(u64::from(id).to_le_bytes())
    }
    fn ace_encode_target(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        encoder.capture_native(|e| self.ace_encode_element(e))
    }
    fn ace_encode_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return Err(encoder.fail(Error::InvalidArgument));
        }
        let id = encoder.resolve_sync_weak(self)?;
        encoder.write_element_bytes(&id.to_le_bytes())
    }
    fn ace_encode_some(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return encoder.encode_none();
        }
        let id = encoder.resolve_sync_weak(self)?;
        encoder.encode_scalar(u64::from(id).to_le_bytes())
    }
    fn ace_encode_some_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        if self.upgrade().is_none() {
            return encoder.write_element_bytes(&[0]);
        }
        encoder.write_element_bytes(&[1])?;
        let id = encoder.resolve_sync_weak(self)?;
        encoder.write_element_bytes(&id.to_le_bytes())
    }
}

/// Transparent encode: each entry takes one `try_borrow` and forwards to the inner value. A live
/// mutable borrow is `InvalidArgument`; the cell neither blocks nor retries. The target entry
/// forwards unchanged and adds no record or id.
impl<T: AceEncode> AceEncode for core::cell::RefCell<T> {
    fn ace_encode(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_borrow()
            .map_err(|_| encoder.fail(Error::InvalidArgument))?;
        inner.ace_encode(encoder)
    }
    fn ace_encode_target(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_borrow()
            .map_err(|_| encoder.fail(Error::InvalidArgument))?;
        inner.ace_encode_target(encoder)
    }
    fn ace_encode_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_borrow()
            .map_err(|_| encoder.fail(Error::InvalidArgument))?;
        inner.ace_encode_element(encoder)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        T::ace_element_value_size()
    }
}

/// Transparent encode for `RwLock`, std only: each entry takes one `try_read` and forwards to the
/// inner value. A held write lock or a poisoned lock is `InvalidArgument`; the lock neither blocks
/// nor retries. The target entry forwards unchanged and adds no record or id.
#[cfg(feature = "std")]
impl<T: AceEncode> AceEncode for std::sync::RwLock<T> {
    fn ace_encode(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_read()
            .map_err(|_| encoder.fail(Error::InvalidArgument))?;
        inner.ace_encode(encoder)
    }
    fn ace_encode_target(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_read()
            .map_err(|_| encoder.fail(Error::InvalidArgument))?;
        inner.ace_encode_target(encoder)
    }
    fn ace_encode_element(&self, encoder: &mut Encoder<'_>) -> Result<()> {
        let inner = self
            .try_read()
            .map_err(|_| encoder.fail(Error::InvalidArgument))?;
        inner.ace_encode_element(encoder)
    }
    fn ace_element_value_size() -> Option<usize>
    where
        Self: Sized,
    {
        T::ace_element_value_size()
    }
}

/// Marker for values that may appear inside a single `Option`.
///
/// `Option<T>` 的 codec 要求内层 `T: NonOptional`，因此 `Option<Option<T>>` 在编译期被拒绝
/// （FR-T02）；类型别名（如 `type A = Option<Option<i32>>`）同样无法绕过。
/// 该诊断属性只改变错误文字：派生代码的调用点已按字段类型 span 发射，
/// 因此直接写法与别名写法都得到定位到字段、以 `ace: ` 开头的诊断。
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "ace: Option<Option<T>> is not supported",
    label = "ace: this Option field's inner type is itself an Option",
    note = "ace: an Option field may nest at most one Option; nested Option has no wire mapping (FR-T02)"
)]
pub trait NonOptional {}

/// Plain decode admits `Vec<T>` only when `T: AceElement`, and `Rc<u32>` is not `AceElement`.
/// Graph pointers are admitted only through `AceDecodeCyclic`, so `Option<Vec<Rc<u32>>>` has no
/// `AceDecode` implementation:
///
/// ```compile_fail,E0277
/// use ace_serialize::schema::AceDecode;
/// fn accepts_decode<T: AceDecode>() {}
/// accepts_decode::<Option<Vec<std::rc::Rc<u32>>>>();
/// ```
pub trait AceDecode: Sized {
    fn ace_decode(decoder: &mut Decoder<'_>) -> Result<Self>;
    /// Decode into an existing value. The default replaces it; `String` 与 `Vec` 复用容量.
    fn ace_decode_into(decoder: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        *out = Self::ace_decode(decoder)?;
        Ok(())
    }
}

/// 两阶段图解码：阶段一返回完整计划，阶段二不得再返回输入错误。
///
/// 图读回准入来自 `AceDecodeCyclic`，不来自 [`AceElement`]：只实现 `AceElement` 的结构体不进入
/// `Rc<Vec<S>>` 或 `Rc<Box<S>>`，补 `encode_element` 或三个 bulk 方法也不能替代本 trait。
///
/// ```compile_fail,E0277
/// use ace_serialize::schema::{AceDecodeCyclic, AceElement, ElementKind};
/// struct S(u32);
/// impl AceElement for S {
///     fn element_kind() -> ElementKind {
///         ElementKind::Value { size: 4 }
///     }
///     fn encode_element(
///         &self,
///         e: &mut ace_serialize::encode::Encoder<'_>,
///     ) -> ace_serialize::error::Result<()> {
///         e.write_element_bytes(&self.0.to_le_bytes())
///     }
///     fn decode_element(
///         d: &mut ace_serialize::decode::Decoder<'_>,
///     ) -> ace_serialize::error::Result<Self> {
///         let bytes = d.read_element_bytes(4)?;
///         Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
///     }
/// }
/// fn accepts_cyclic<T: AceDecodeCyclic>() {}
/// accepts_cyclic::<std::rc::Rc<Vec<S>>>();
/// ```
///
/// ```compile_fail,E0277
/// use ace_serialize::schema::{AceDecodeCyclic, AceElement, ElementKind};
/// struct S(u32);
/// impl AceElement for S {
///     fn element_kind() -> ElementKind {
///         ElementKind::Value { size: 4 }
///     }
///     fn encode_element(
///         &self,
///         e: &mut ace_serialize::encode::Encoder<'_>,
///     ) -> ace_serialize::error::Result<()> {
///         e.write_element_bytes(&self.0.to_le_bytes())
///     }
///     fn decode_element(
///         d: &mut ace_serialize::decode::Decoder<'_>,
///     ) -> ace_serialize::error::Result<Self> {
///         let bytes = d.read_element_bytes(4)?;
///         Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
///     }
/// }
/// fn accepts_cyclic<T: AceDecodeCyclic>() {}
/// accepts_cyclic::<std::rc::Rc<Box<S>>>();
/// ```
pub trait AceDecodeCyclic: Sized {
    /// 阶段一产生的类型化构造计划，不要求全局静态寿命。
    type Plan;
    /// 解析并校验当前值，图身份节点留待阶段二构造。
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan>;
    /// 仅消费已验证计划，构造最终值。
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self;
    /// 整段解码元素体为计划；返回 `Ok(None)` 时不得消费输入或修改计划，由调用方逐项规划。
    #[doc(hidden)]
    fn decode_elements_bulk_plan(
        _count: usize,
        _bytes: &[u8],
        _consumed: &mut usize,
    ) -> crate::error::Result<Option<alloc::vec::Vec<Self::Plan>>>
    where
        Self: Sized,
    {
        Ok(None)
    }
}

impl<T> NonOptional for alloc::rc::Rc<T> {}
impl<T> NonOptional for alloc::rc::Weak<T> {}

/// `RefCell<T>` is `NonOptional` only when `T` is, so the transparent cell cannot hide a nested
/// `Option`.
///
/// ```
/// use ace_serialize::schema::AceEncode;
/// fn accepts_encode<T: AceEncode>() {}
/// accepts_encode::<Option<core::cell::RefCell<i32>>>();
/// ```
///
/// ```compile_fail
/// use ace_serialize::schema::AceEncode;
/// fn accepts_encode<T: AceEncode>() {}
/// accepts_encode::<Option<core::cell::RefCell<Option<i32>>>>();
/// ```
impl<T: NonOptional> NonOptional for core::cell::RefCell<T> {}

/// `RwLock<T>` is `NonOptional` only when `T` is, so the transparent lock cannot hide a nested
/// `Option`.
///
/// ```
/// use ace_serialize::schema::AceEncode;
/// fn accepts_encode<T: AceEncode>() {}
/// accepts_encode::<Option<std::sync::RwLock<i32>>>();
/// ```
///
/// ```compile_fail
/// use ace_serialize::schema::AceEncode;
/// fn accepts_encode<T: AceEncode>() {}
/// accepts_encode::<Option<std::sync::RwLock<Option<i32>>>>();
/// ```
#[cfg(feature = "std")]
impl<T: NonOptional> NonOptional for std::sync::RwLock<T> {}

impl<T> AceDecodeCyclic for alloc::rc::Rc<T>
where
    T: AceDecodeCyclic + 'static,
    T::Plan: 'static,
{
    type Plan = u32;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
        ctx.strong::<T>()
    }
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
        ctx.strong::<T>(plan)
            .expect("planned strong target must be done")
    }
}
impl<T: 'static> AceDecodeCyclic for alloc::rc::Weak<T> {
    type Plan = u32;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
        ctx.weak::<T>()
    }
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
        ctx.weak::<T>(plan)
            .expect("planned weak target must be open or done")
    }
}
impl<T: AceDecodeCyclic> AceDecodeCyclic for core::cell::RefCell<T> {
    type Plan = T::Plan;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<T::Plan> {
        T::decode_plan(ctx)
    }
    fn build_from_plan(
        ctx: &mut crate::decode::BuildCtx<'_>,
        plan: T::Plan,
    ) -> core::cell::RefCell<T> {
        core::cell::RefCell::new(T::build_from_plan(ctx, plan))
    }
}
// Arc、sync::Weak 与 RwLock 的循环解码只在 std 下提供（Q6）；Rc 路径仍只依赖 core/alloc。
#[cfg(feature = "std")]
impl<T> NonOptional for alloc::sync::Arc<T> {}
#[cfg(feature = "std")]
impl<T> NonOptional for alloc::sync::Weak<T> {}
#[cfg(feature = "std")]
impl<T> AceDecodeCyclic for alloc::sync::Arc<T>
where
    T: AceDecodeCyclic + 'static,
    T::Plan: 'static,
{
    type Plan = u32;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
        ctx.strong_arc::<T>()
    }
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
        ctx.strong_arc::<T>(plan)
            .expect("planned strong target must be done")
    }
}
#[cfg(feature = "std")]
impl<T: 'static> AceDecodeCyclic for alloc::sync::Weak<T> {
    type Plan = u32;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
        ctx.weak_arc::<T>()
    }
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
        ctx.weak_arc::<T>(plan)
            .expect("planned weak target must be open or done")
    }
}
#[cfg(feature = "std")]
impl<T: AceDecodeCyclic> AceDecodeCyclic for std::sync::RwLock<T> {
    type Plan = T::Plan;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<T::Plan> {
        T::decode_plan(ctx)
    }
    fn build_from_plan(
        ctx: &mut crate::decode::BuildCtx<'_>,
        plan: T::Plan,
    ) -> std::sync::RwLock<T> {
        std::sync::RwLock::new(T::build_from_plan(ctx, plan))
    }
}
impl<T: AceDecodeCyclic + NonOptional> AceDecodeCyclic for Option<T> {
    type Plan = Option<T::Plan>;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
        ctx.option::<T>()
    }
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
        plan.map(|p| T::build_from_plan(ctx, p))
    }
}
impl<T: AceDecodeCyclic> AceDecodeCyclic for Vec<T> {
    type Plan = Vec<T::Plan>;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
        ctx.vec::<T>()
    }
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
        plan.into_iter()
            .map(|item| T::build_from_plan(ctx, item))
            .collect()
    }
}
impl<T: AceDecodeCyclic> AceDecodeCyclic for Box<T> {
    type Plan = Box<T::Plan>;
    fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
        ctx.boxed::<T>()
    }
    fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
        Box::new(T::build_from_plan(ctx, *plan))
    }
}
/// The `bulk` form also forwards `decode_elements_bulk_plan` to the element's bulk path; the plain
/// form keeps the trait default.
macro_rules! cyclic_value {
    (bulk $($ty:ty),+ $(,)?) => {$(
        impl AceDecodeCyclic for $ty {
            type Plan = Self;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self> {
                ctx.value::<Self>(crate::decode::PlanValueLayout::Leaf)
            }
            fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: Self) -> Self { plan }
            fn decode_elements_bulk_plan(
                count: usize,
                bytes: &[u8],
                consumed: &mut usize,
            ) -> Result<Option<Vec<Self::Plan>>>
            where
                Self: Sized,
            {
                <Self as AceElement>::decode_elements_bulk(count, bytes, consumed)
            }
        }
    )+};
    ($($ty:ty),+ $(,)?) => {$(
        impl AceDecodeCyclic for $ty {
            type Plan = Self;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self> {
                ctx.value::<Self>(crate::decode::PlanValueLayout::Leaf)
            }
            fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: Self) -> Self { plan }
        }
    )+};
}
cyclic_value!(
    bulk i8,
    i16,
    i32,
    i64,
    isize,
    u8,
    u16,
    u32,
    u64,
    usize,
    bool,
    char,
    f32,
    f64,
    value::Guid,
    value::IntPtr,
    value::UIntPtr,
    value::ToolkitsU64
);
// `String`, `DateTime` and `DateTimeOffset` keep the trait default for decode bulk.
cyclic_value!(String, value::DateTime, value::DateTimeOffset);

/// `ace_element_value_size` from the element's wire kind, the width `Vec` pre-checks already use.
macro_rules! value_size_hook {
    () => {
        fn ace_element_value_size() -> Option<usize>
        where
            Self: Sized,
        {
            match <Self as AceElement>::element_kind() {
                ElementKind::Value { size } => Some(size),
                ElementKind::Ref | ElementKind::Variable => None,
            }
        }
    };
}
/// `ace_encode_elements_bulk` forwarding to the element's bulk path, and the matching zero-allocation
/// direct writer. `DateTime` and `DateTimeOffset` have no bulk path and keep the trait defaults.
macro_rules! encode_bulk_hook {
    () => {
        fn ace_encode_elements_bulk(items: &[Self], out: &mut Vec<u8>) -> crate::error::Result<bool>
        where
            Self: Sized,
        {
            <Self as AceElement>::encode_elements_bulk(items, out)
        }
        fn ace_encode_elements_bulk_direct(
        ) -> Option<fn(&[Self], &mut Encoder<'_>) -> crate::error::Result<()>>
        where
            Self: Sized,
        {
            <Self as AceElement>::encode_elements_bulk_direct()
        }
    };
}

macro_rules! scalar {
    ($ty:ty, $enc:ident, $dec:expr) => {
        impl NonOptional for $ty {}
        impl AceEncode for $ty {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.encode_scalar(value::$enc(*self))
            }
            fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.capture_native(|e| <Self as AceElement>::encode_element(self, e))
            }
            fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                <Self as AceElement>::encode_element(self, e)
            }
            value_size_hook!();
            encode_bulk_hook!();
        }
        impl AceDecode for $ty {
            fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
                ($dec)(&d.decode_scalar()?)
            }
        }
    };
}
scalar!(i8, encode_i8, |p| Ok(value::decode_i8(p)));
scalar!(i16, encode_i16, |p| Ok(value::decode_i16(p)));
scalar!(i32, encode_i32, |p| Ok(value::decode_i32(p)));
scalar!(i64, encode_i64, |p| Ok(value::decode_i64(p)));
scalar!(isize, encode_isize, value::decode_isize);
scalar!(u8, encode_u8, |p| Ok(value::decode_u8(p)));
scalar!(u16, encode_u16, |p| Ok(value::decode_u16(p)));
scalar!(u32, encode_u32, |p| Ok(value::decode_u32(p)));
scalar!(u64, encode_u64, |p| Ok(value::decode_u64(p)));
scalar!(usize, encode_usize, value::decode_usize);
scalar!(bool, encode_bool, value::decode_bool);
scalar!(char, encode_char, value::decode_char);
scalar!(f32, encode_f32, |p| Ok(value::decode_f32(p)));
scalar!(f64, encode_f64, |p| Ok(value::decode_f64(p)));

impl NonOptional for str {}
impl AceEncode for str {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.encode_record(value::encode_str(self))
    }
    fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.capture_native(|e| e.write_element_bytes(value::encode_str(self)))
    }
}
impl NonOptional for &str {}
impl AceEncode for &str {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        (**self).ace_encode(e)
    }
    fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
        (**self).ace_encode_target(e)
    }
}
impl NonOptional for String {}
impl AceEncode for String {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        self.as_str().ace_encode(e)
    }
    fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.capture_native(|e| e.write_element_bytes(value::encode_str(self)))
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        <Self as AceElement>::encode_element(self, e)
    }
}
impl AceDecode for String {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        Ok(String::from(d.decode_record()?.as_str()?))
    }
    fn ace_decode_into(d: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        let s = d.decode_record()?.as_str()?;
        out.clear();
        out.push_str(s);
        Ok(())
    }
}
impl<T: AceEncode + NonOptional> AceEncode for Option<T> {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        match self {
            None => e.encode_none(),
            Some(v) => v.ace_encode_some(e),
        }
    }
    fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.capture_native(|e| self.ace_encode_element(e))
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        match self {
            None => e.write_element_bytes(&[0]),
            Some(v) => v.ace_encode_some_element(e),
        }
    }
}
impl<T: AceDecode + NonOptional> AceDecode for Option<T> {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        if d.selected_is_none() {
            Ok(None)
        } else {
            T::ace_decode(d).map(Some)
        }
    }
    /// 缺席时整体替换为 `None`（整体替换不触发目标单元格检查）；present 时 `Some` 复用内层。
    fn ace_decode_into(d: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        if d.selected_is_none() {
            *out = None;
            return Ok(());
        }
        match out {
            Some(value) => T::ace_decode_into(d, value),
            None => {
                *out = Some(T::ace_decode(d)?);
                Ok(())
            }
        }
    }
}

/// Transparent decode: decodes the inner value and wraps it in a `RefCell`.
impl<T: AceDecode> AceDecode for core::cell::RefCell<T> {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        T::ace_decode(d).map(core::cell::RefCell::new)
    }
    /// 写入前先取可变借用作为该单元格的可用性检查：任何未释放的借用（含经 `mem::forget`
    /// 泄漏的 `Ref`/`RefMut`）都会使 `try_borrow_mut` 失败，此时返回 `1 InvalidArgument` 且
    /// 不写入该单元格。写入走同一个守卫，检查与写入之间无窗口；失败路径不 `get_mut`、不重借用，
    /// 故不清理也不规整既有的借用状态。
    fn ace_decode_into(d: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        let mut inner = out.try_borrow_mut().map_err(|_| Error::InvalidArgument)?;
        T::ace_decode_into(d, &mut *inner)
    }
}

/// Transparent decode for `RwLock`, std only: decodes the inner value and wraps it in a lock.
#[cfg(feature = "std")]
impl<T: AceDecode> AceDecode for std::sync::RwLock<T> {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        T::ace_decode(d).map(std::sync::RwLock::new)
    }
    /// 写入前先取写锁作为该单元格的可用性检查：持锁或泄漏的读/写 guard（含经 `mem::forget`
    /// 泄漏的 `RwLockReadGuard`/`RwLockWriteGuard`）与中毒都使 `try_write` 失败，此时返回
    /// `1 InvalidArgument` 且不写入该单元格。写入走同一个守卫，检查与写入之间无窗口；失败路径不
    /// `get_mut`/`into_inner`，故不清除 poison 标记，也不规整被弃 guard。
    fn ace_decode_into(d: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        let mut inner = out.try_write().map_err(|_| Error::InvalidArgument)?;
        T::ace_decode_into(d, &mut *inner)
    }
}

/// 编码 Box 记录体并返回记录 id。元素体由 `encode_one` 写入：AceEncode 入口传 `ace_encode_element`，
/// 旧 AceElement 入口传 `encode_element`。
fn encode_box_record_with<T, F>(value: &T, e: &mut Encoder<'_>, encode_one: F) -> Result<u32>
where
    F: FnOnce(&T, &mut Encoder<'_>) -> Result<()>,
{
    e.begin_record()?;
    e.begin_element();
    let encoded = encode_one(value, e);
    // 先结束元素作用域，再传播元素错误。
    e.end_element();
    encoded?;
    e.end_record()
}

// 此入口专用于 Box 记录，保留 Box 引用以明确拥有型引用的边界。
#[allow(clippy::borrowed_box)]
fn encode_box_record<T: AceElement>(value: &Box<T>, e: &mut Encoder<'_>) -> Result<u32> {
    encode_box_record_with(&**value, e, T::encode_element)
}

/// 解码当前 Box 记录视图；由调用方负责关闭视图与集合深度。
fn decode_box_view_with<T, F>(d: &mut Decoder<'_>, decode: F) -> Result<Box<T>>
where
    F: Fn(&mut Decoder<'_>) -> Result<T>,
{
    d.begin_element();
    let decoded = decode(d);
    d.end_element();
    let value = decoded?;
    check_tail(d)?;
    Ok(Box::new(value))
}

/// 拥有型引用形式（FR-T01）：独立记录只含一个元素体、无计数；字段或根的 payload 为记录 id。
impl<T> NonOptional for Box<T> {}
impl<T: AceEncode> AceEncode for Box<T> {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_collection()?;
        let id = encode_box_record_with(&**self, e, T::ace_encode_element);
        let encoded = id.and_then(|id| e.encode_scalar(u64::from(id).to_le_bytes()));
        e.leave_collection();
        encoded
    }
    /// 身份目标体（设计 r4）：恰一个 `T` 元素体、无 count，直接写入 capture 已打开的目标记录。
    fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.capture_native(|e| {
            e.enter_collection()?;
            let encoded = T::ace_encode_element(&**self, e);
            // 先离开嵌套深度，再传播元素编码的错误。
            e.leave_collection();
            encoded
        })
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.enter_collection()?;
        let id = encode_box_record_with(&**self, e, T::ace_encode_element);
        // 先离开嵌套深度，再传播记录编码的错误。
        e.leave_collection();
        e.write_element_bytes(&id?.to_le_bytes())
    }
}
impl<T: AceElement> AceDecode for Box<T> {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        d.enter_collection()?;
        let decoded = d.decode_required_scalar().and_then(|payload| {
            let id =
                u32::try_from(u64::from_le_bytes(payload)).map_err(|_| Error::InvalidReference)?;
            d.begin_record_view(id)?;
            let decoded = decode_box_view_with(d, T::decode_element);
            // 先关闭记录视图，再传播元素或尾部错误。
            d.end_record_view();
            decoded
        });
        d.leave_collection();
        decoded
    }
}

/// 解码 Box 元素的记录引用；映射键通过 `decode` 在复制内层字符串前检查预算。
fn decode_box_element_with<T, F>(d: &mut Decoder<'_>, decode: F) -> Result<Box<T>>
where
    F: Fn(&mut Decoder<'_>) -> Result<T>,
{
    let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
    d.enter_collection()?;
    if let Err(error) = d.begin_record_view(id) {
        d.leave_collection();
        return Err(error);
    }
    let decoded = decode_box_view_with(d, decode);
    // 先关闭记录视图并离开集合，再传播元素或尾部错误。
    d.end_record_view();
    d.leave_collection();
    decoded
}

/// 元素形式（FR-W09、FR-T01）：元素体为独立 Box 记录的 `u32` id，不走 bulk。
impl<T: AceElement> AceElement for Box<T> {
    fn element_kind() -> ElementKind {
        ElementKind::Ref
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.enter_collection()?;
        let id = encode_box_record(self, e);
        e.leave_collection();
        e.write_element_bytes(&id?.to_le_bytes())
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        decode_box_element_with(d, T::decode_element)
    }
    fn check_map_key(&self, opts: &Options) -> Result<()> {
        (**self).check_map_key(opts)
    }
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        decode_box_element_with(d, T::decode_map_key)
    }
}

/// Collection form (FR-W09, FR-T03). The record body is `u32 count` followed by the elements, so
/// an empty `Vec` is a record whose body is `[0, 0, 0, 0]`, distinct from `None`.
impl<T> NonOptional for Vec<T> {}
/// The element bound is `T: AceEncode`, not `T: AceElement`, so an element that only implements
/// [`AceElement`] no longer makes `Vec<T>` encodable:
///
/// ```compile_fail,E0277
/// use ace_serialize::schema::{AceElement, AceEncode, ElementKind};
/// struct OnlyElement(u32);
/// impl AceElement for OnlyElement {
///     fn element_kind() -> ElementKind {
///         ElementKind::Value { size: 4 }
///     }
///     fn encode_element(
///         &self,
///         e: &mut ace_serialize::encode::Encoder<'_>,
///     ) -> ace_serialize::error::Result<()> {
///         e.write_element_bytes(&self.0.to_le_bytes())
///     }
///     fn decode_element(
///         d: &mut ace_serialize::decode::Decoder<'_>,
///     ) -> ace_serialize::error::Result<Self> {
///         let bytes = d.read_element_bytes(4)?;
///         Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
///     }
/// }
/// fn accepts_encode<T: AceEncode>() {}
/// accepts_encode::<Vec<OnlyElement>>();
/// ```
impl<T: AceEncode> AceEncode for Vec<T> {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_collection()?;
        let encoded = encode_vec_body_with(
            self,
            e,
            T::ace_element_value_size(),
            T::ace_encode_elements_bulk_into_fn(),
            T::ace_encode_elements_bulk_into,
            T::ace_encode_elements_bulk_direct(),
            T::ace_encode_elements_bulk,
            T::ace_encode_element,
        )
        .and_then(|id| e.encode_scalar(u64::from(id).to_le_bytes()));
        // 先离开集合，再传播编码错误。
        e.leave_collection();
        encoded
    }
    /// 身份目标体（设计 r4 目标布局表）：`u32 count` 与元素直接写入 capture 已打开的目标记录，目标 id 由 capture 绑定。
    fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.capture_native(|e| {
            e.enter_collection()?;
            let encoded = write_vec_target_body(
                self,
                e,
                T::ace_element_value_size(),
                T::ace_encode_elements_bulk_into_fn(),
                T::ace_encode_elements_bulk_into,
                T::ace_encode_elements_bulk_direct(),
                T::ace_encode_elements_bulk,
                T::ace_encode_element,
            );
            // 先离开集合，再传播集合编码的错误。
            e.leave_collection();
            encoded
        })
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.enter_collection()?;
        let id = encode_vec_body_with(
            self,
            e,
            T::ace_element_value_size(),
            T::ace_encode_elements_bulk_into_fn(),
            T::ace_encode_elements_bulk_into,
            T::ace_encode_elements_bulk_direct(),
            T::ace_encode_elements_bulk,
            T::ace_encode_element,
        );
        // 先离开嵌套深度，再传播集合编码的错误。
        e.leave_collection();
        e.write_element_bytes(&id?.to_le_bytes())
    }
}
impl<T: AceElement> AceDecode for Vec<T> {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        let mut out = Vec::new();
        Self::ace_decode_into(d, &mut out)?;
        Ok(out)
    }
    /// 解码并复用 `out` 的容量。失败时 out 内容不作承诺，内存安全不受影响。
    fn ace_decode_into(d: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        d.enter_collection()?;
        let decoded = d.decode_required_scalar().and_then(|payload| {
            let id =
                u32::try_from(u64::from_le_bytes(payload)).map_err(|_| Error::InvalidReference)?;
            d.begin_record_view(id)?;
            // Close the view before propagating an error from the element decode.
            let decoded = decode_vec_into_view(d, out);
            d.end_record_view();
            decoded
        });
        // 先离开集合，再传播解码错误。
        d.leave_collection();
        decoded
    }
}

/// Decodes the body of the innermost record view into `out`, reusing both the outer capacity and
/// each existing element's own buffer (R22①).
fn decode_vec_into_view<T: AceElement>(d: &mut Decoder<'_>, out: &mut Vec<T>) -> Result<()> {
    decode_vec_into_view_with(d, out, T::decode_element, T::decode_element_into)
}

/// Decodes the body of the innermost record view into `out`. The count is checked against the
/// limit before any element is read, and the vector grows only as elements decode, so an untrusted
/// count never sizes an allocation. Elements already present in `out` are decoded in place by
/// `decode_into`, so they can reuse their own buffers (R22①); elements beyond the old length (and
/// leftovers past `count`) are dropped/created as before.
fn decode_vec_into_view_with<T, F, G>(
    d: &mut Decoder<'_>,
    out: &mut Vec<T>,
    decode: F,
    decode_into: G,
) -> Result<()>
where
    T: AceElement,
    F: Fn(&mut Decoder<'_>) -> Result<T>,
    G: Fn(&mut Decoder<'_>, &mut T) -> Result<()>,
{
    let count = d.read_element_id()? as usize;
    d.check_collection_len(count)?;
    let mut consumed = 0;
    if T::decode_elements_bulk_into(out, count, d.peek_body()?, &mut consumed)? {
        d.advance_body(consumed)?;
        return check_tail(d);
    }
    d.begin_element();
    let decoded = (0..count).try_for_each(|index| {
        if index < out.len() {
            return decode_into(d, &mut out[index]);
        }
        decode(d).map(|value| out.push(value))
    });
    d.end_element();
    decoded?;
    out.truncate(count);
    check_tail(d)
}

/// Rejects bytes left unread in the innermost record view.
fn check_tail(d: &Decoder<'_>) -> Result<()> {
    if d.peek_body()?.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidTag)
    }
}

/// 内建定宽元素的零分配整段体直写（FR-W09、R14）：`u32 count` 走栈上 4B，元素体从连续内存
/// **一次性**拷进当前打开的记录（`write_body_bytes`：拥有值路径一次 `extend_from_slice`，借用
/// 路径向 arena 记录体一次整段拷入），不为合成记录体新建 `Vec`。非空且写入成功后落一次 bulk
/// 计数：空元素区没有整段拷贝（与 `scratch` 的空体直返同口径），只写 `u32 count`。
#[cfg(target_endian = "little")]
fn write_elements_direct<T>(items: &[T], e: &mut Encoder<'_>) -> Result<()> {
    let count = u32::try_from(items.len()).map_err(|_| e.fail(Error::ResourceLimit))?;
    e.write_body_bytes(&count.to_le_bytes())?;
    // SAFETY: only the `element!` fixed-width list (no padding, `size_of` asserted at each impl)
    // and the 8/16-byte `value::` newtypes return this writer, and all of them satisfy the same
    // precondition as `unsafe_bulk::encode_slice`: their in-memory bytes are their little-endian
    // wire bytes, so the view has no uninitialized byte.
    let bytes = unsafe { unsafe_bulk::slice_bytes(items) };
    e.write_body_bytes(bytes)?;
    if !bytes.is_empty() {
        crate::hooks::note_bulk();
    }
    Ok(())
}

/// 把集合体写进当前打开的记录：`u32 count` 只占栈上 4 字节，元素字节由 bulk 侧一次性写入，
/// 不为「计数 + 元素」新建合成 `Vec`（R14）。
fn write_bulk_body(e: &mut Encoder<'_>, count: u32, elements: &[u8]) -> Result<()> {
    e.write_body_bytes(&count.to_le_bytes())?;
    e.write_body_bytes(elements)
}

/// 写入集合体的 `u32 count` 与逐项元素到当前打开的记录；元素位于元素作用域内。
fn write_vec_items<T, FO>(items: &[T], e: &mut Encoder<'_>, mut encode_one: FO) -> Result<()>
where
    FO: FnMut(&T, &mut Encoder<'_>) -> Result<()>,
{
    e.write_body_bytes(&(items.len() as u32).to_le_bytes())?;
    e.begin_element();
    let encoded = items.iter().try_for_each(|v| encode_one(v, e));
    // 先结束元素作用域，再传播元素错误。
    e.end_element();
    encoded
}

/// 身份目标的集合体（设计 r4）：`u32 count` 与元素，或 bulk 整段体，直接写入 capture 已打开的目标记录。
/// 目标记录的名额与头部已由 `begin_target_record` 计入，故不调用 `check_record_body`。
/// 定宽元素的整段体按自然宽度（4 + len×size）在构造与 bulk 复制前经 `check_body_additional` 预检；
/// 变宽元素体长不可预知，逐项写入，由 `write_body_bytes` 逐次检查。
///
/// `direct` 为库内建定宽元素的零分配整段体直写（FR-W09、R14）：命中时不经公开
/// `&mut Vec<u8>` hook，也不构造合成 `Vec`。`bulk_into` 为需求 R23 的 `&mut [u8]` 新钩子：
/// 只在体长可精确预知且该类型声明了写入能力时尝试；`bulk_into_method` 是同一 trait 方法的
/// 函数指针，供「仅 `direct` 声明」时选到新方法（见 [`enable_bulk_into`]）。
fn write_vec_target_body<T, FB, FO>(
    items: &[T],
    e: &mut Encoder<'_>,
    value_size: Option<usize>,
    bulk_into: Option<fn(&[T], &mut [u8]) -> Result<Option<usize>>>,
    bulk_into_method: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
    direct: Option<fn(&[T], &mut Encoder<'_>) -> Result<()>>,
    encode_bulk: FB,
    encode_one: FO,
) -> Result<()>
where
    FB: FnOnce(&[T], &mut Vec<u8>) -> Result<bool>,
    FO: FnMut(&T, &mut Encoder<'_>) -> Result<()>,
{
    e.check_collection_len(items.len())?;
    let mut body_len = None;
    if let Some(size) = value_size {
        let len = items
            .len()
            .checked_mul(size)
            .and_then(|n| n.checked_add(4))
            .ok_or_else(|| e.fail(Error::ResourceLimit))?;
        e.check_body_additional(len)?;
        body_len = Some(len);
    }
    // R23：启用判定在物理预留之前；未启用、准备失败与 `Ok(None)` 都回到同一既有分派。
    if let Some(enable) = enable_bulk_into(body_len, bulk_into, bulk_into_method, direct) {
        if let BulkInto::Committed(n) =
            e.bulk_into_open_record(items, enable.len, enable.hook)?
        {
            debug_assert_eq!(n, enable.len, "R23 提交长度必须等于预期体长");
            return Ok(());
        }
    }
    if let Some(write) = direct {
        return write(items, e);
    }
    let count = items.len() as u32;
    let mut elements = Vec::new();
    if encode_bulk(items, &mut elements)? {
        write_bulk_body(e, count, &elements)
    } else {
        write_vec_items(items, e, encode_one)
    }
}

/// 需求 R23 的启用判定（§5③；架构 §5「R23 写入接缝」）：**体长可精确预知** 且 **该类型声明了
/// 可批量写入能力**。后者是**独立 OR**：同一接缝的能力声明访问器返回 `Some`，**或**旧直写
/// `direct != None`，任一成立即算已声明。
///
/// 三个入参都只是函数指针读取、**不执行任何用户回调**，故判定可在任何物理预留之前完成；
/// **不得**以调用结果推断覆写（已声明者本次也可能合法返回 `Ok(None)`）。OR 成立后**优先用访问器
/// 指针**，否则（仅 `direct` 声明）用传入的该 trait 方法指针 `method`：两者指向同一契约的
/// 函数指针，`method` 的默认实现返回 `Ok(None)`，此时按既有直写/逐项分派，不靠返回値猜覆写。
fn enable_bulk_into<T>(
    body_len: Option<usize>,
    bulk_into: Option<fn(&[T], &mut [u8]) -> Result<Option<usize>>>,
    method: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
    direct: Option<fn(&[T], &mut Encoder<'_>) -> Result<()>>,
) -> Option<BulkIntoEnable<T>> {
    let len = body_len?;
    if bulk_into.is_none() && direct.is_none() {
        return None;
    }
    Some(BulkIntoEnable {
        len,
        hook: bulk_into.unwrap_or(method),
    })
}

/// R23 启用判定的结果：`hook` 是本次要尝试的新钩子（访问器指针优先，否则该 trait 方法）。
struct BulkIntoEnable<T> {
    len: usize,
    hook: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
}

/// 编码集合的记录体并返回记录 id：定宽预检、整段写入（bulk）或逐项写入（`encode_one`）。
/// AceEncode 与旧 AceElement 入口共用此算法，二者只在宽度、bulk 与单项写入器的来源上不同。
///
/// 三条写入支路都把体直接写进**同一条**记录：`u32 count` 只占栈上 4 字节；命中 `direct` 时元素
/// 字节由一次整段拷贝写入，命中手写 hook 时由 hook 一次性给出，都不为合成记录体新建 `Vec`（R14）。
/// R23 的新钩子位于三层之上，命中时也只写这一条记录，不另开提交路径。
fn encode_vec_body_with<T, FB, FO>(
    items: &[T],
    e: &mut Encoder<'_>,
    value_size: Option<usize>,
    bulk_into: Option<fn(&[T], &mut [u8]) -> Result<Option<usize>>>,
    bulk_into_method: fn(&[T], &mut [u8]) -> Result<Option<usize>>,
    direct: Option<fn(&[T], &mut Encoder<'_>) -> Result<()>>,
    encode_bulk: FB,
    encode_one: FO,
) -> Result<u32>
where
    FB: FnOnce(&[T], &mut Vec<u8>) -> Result<bool>,
    FO: FnMut(&T, &mut Encoder<'_>) -> Result<()>,
{
    e.check_collection_len(items.len())?;
    let mut body_len = None;
    if let Some(size) = value_size {
        let len = items
            .len()
            .checked_mul(size)
            .and_then(|n| n.checked_add(4))
            .ok_or_else(|| e.fail(Error::ResourceLimit))?;
        e.check_record_body(len)?;
        body_len = Some(len);
    }
    // R23：新钩子（尽力尝试）→ 既有直写 → 旧 `&mut Vec<u8>` hook → 逐项。启用判定在任何
    // 物理预留之前；新钩子未接管（`Declined`/`NotStarted`）时记录未逻辑入栈，回到同一既有分派
    // 并从 `begin_record` 起（已预留的记录体由 `pending_body` 复用）。
    if let Some(enable) = enable_bulk_into(body_len, bulk_into, bulk_into_method, direct) {
        if let BulkInto::Committed(n) =
            e.bulk_into_new_record(items, enable.len, enable.hook)?
        {
            debug_assert_eq!(n, enable.len, "R23 提交长度必须等于预期体长");
            return e.end_record();
        }
    }
    if let Some(write) = direct {
        e.begin_record()?;
        write(items, e)?;
        return e.end_record();
    }
    let count = items.len() as u32;
    let mut elements = Vec::new();
    // 旧 hook 仍在 `begin_record` 之前执行（新钩子未接管时保持既有顺序与错误优先级）。
    let bulk = encode_bulk(items, &mut elements)?;
    e.begin_record()?;
    if bulk {
        write_bulk_body(e, count, &elements)?;
    } else {
        write_vec_items(items, e, encode_one)?;
    }
    e.end_record()
}

/// 编码集合的记录体并返回其记录 id。由 `Vec::encode_element` 在 `enter_collection` 之后调用。
fn encode_vec_body<T: AceElement>(items: &[T], e: &mut Encoder<'_>) -> Result<u32> {
    let value_size = match T::element_kind() {
        ElementKind::Value { size } => Some(size),
        ElementKind::Ref | ElementKind::Variable => None,
    };
    encode_vec_body_with(
        items,
        e,
        value_size,
        T::encode_elements_bulk_into_fn(),
        T::ace_encode_elements_bulk_into,
        T::encode_elements_bulk_direct(),
        T::encode_elements_bulk,
        T::encode_element,
    )
}

/// 读取集合元素的记录 id 与记录视图，把体解码进 `target`；`decode_element` 与
/// `decode_element_into` 共用同一套视图/深度收尾（先关闭视图，再传播错误）。
fn decode_vec_element<T: AceElement>(d: &mut Decoder<'_>, target: &mut Vec<T>) -> Result<()> {
    let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
    d.enter_collection()?;
    if let Err(error) = d.begin_record_view(id) {
        d.leave_collection();
        return Err(error);
    }
    let decoded = decode_vec_into_view(d, target);
    d.end_record_view();
    d.leave_collection();
    decoded
}

/// 元素形式（FR-W09）：内层集合是独立记录，记录体为 `u32 count` 后接元素，元素体为该记录的
/// `u32` id。内层元素作用域位于外层之内，因此作用域标记按深度计数。
impl<T: AceElement> AceElement for Vec<T> {
    fn element_kind() -> ElementKind {
        ElementKind::Ref
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.enter_collection()?;
        let id = encode_vec_body(self, e);
        // 先离开嵌套深度，再传播集合编码的错误。
        e.leave_collection();
        e.write_element_bytes(&id?.to_le_bytes())
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let mut out = Vec::new();
        decode_vec_element(d, &mut out)?;
        Ok(out)
    }
    /// 元素级复用（R22①）：内层集合按 `target` 已有容量解码，已有元素进一步复用自身缓冲。
    fn decode_element_into(d: &mut Decoder<'_>, target: &mut Self) -> Result<()> {
        crate::hooks::note_element();
        decode_vec_element(d, target)
    }
    /// 列表键逐个元素检查，嵌套字符串同样受 `max_map_string_key_bytes` 约束（NFR-L01）。
    fn check_map_key(&self, opts: &Options) -> Result<()> {
        self.iter().try_for_each(|v| v.check_map_key(opts))
    }
    /// 与 `decode_element` 相同，但元素经 `T::decode_map_key` 解码，使嵌套字符串在复制前执行键长预算。
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
        d.enter_collection()?;
        if let Err(error) = d.begin_record_view(id) {
            d.leave_collection();
            return Err(error);
        }
        let mut out = Vec::new();
        // 先关闭记录视图，再传播元素解码的错误。
        let decoded = decode_vec_into_view_with(
            d,
            &mut out,
            T::decode_map_key,
            // 映射键总是解码到新 `Vec`，没有既有元素可复用：保持「解码新值再写入」的默认语义。
            |d: &mut Decoder<'_>, target: &mut T| {
                *target = T::decode_map_key(d)?;
                Ok(())
            },
        );
        d.end_record_view();
        d.leave_collection();
        decoded?;
        Ok(out)
    }
}

/// 映射体写入前的检查：长度与全部键，须先于 `write_map_items`。普通映射在打开记录前检查，超限时不留下未关闭的记录。
fn check_map_body<K: AceElement + Ord, V>(map: &BTreeMap<K, V>, e: &Encoder<'_>) -> Result<()> {
    e.check_collection_len(map.len())?;
    for k in map.keys() {
        k.check_map_key(e.options()).map_err(|err| e.fail(err))?;
    }
    Ok(())
}

/// 写入映射体的 `u32 count` 与按 Ord 排列的键值对到当前打开的记录；元素位于元素作用域内。
fn write_map_items<K, V, F>(
    map: &BTreeMap<K, V>,
    e: &mut Encoder<'_>,
    mut encode_value: F,
) -> Result<()>
where
    K: AceElement + Ord,
    F: FnMut(&V, &mut Encoder<'_>) -> Result<()>,
{
    e.write_body_bytes(&(map.len() as u32).to_le_bytes())?;
    e.begin_element();
    let elements = map.iter().try_for_each(|(k, v)| {
        k.encode_element(e)?;
        encode_value(v, e)
    });
    // 先结束元素作用域，再传播元素错误。
    e.end_element();
    elements
}

/// 编码映射独立形式与元素形式共用的记录体，并返回记录 id。
fn encode_map_body<K, V, F>(
    map: &BTreeMap<K, V>,
    e: &mut Encoder<'_>,
    encode_value: F,
) -> Result<u32>
where
    K: AceElement + Ord,
    F: FnMut(&V, &mut Encoder<'_>) -> Result<()>,
{
    check_map_body(map, e)?;
    e.begin_record()?;
    write_map_items(map, e, encode_value)?;
    e.end_record()
}

/// 映射形式（FR-W09）：记录体为 `u32 count`（LE），随后是按键的 `Ord` 顺序排列的 count 对（键元素、值元素）。
/// 不走 bulk，bulk 计数恒为 0。`BTreeMap` 可作为元素，也可作为映射的键或值；
/// 映射的键需满足 `AceElement + Ord`；编码时值需满足 `AceEncode`（普通值由 N01 写出），
/// 普通解码仍要求值满足 `AceElement`。
impl<K: AceElement + Ord, V> NonOptional for alloc::collections::BTreeMap<K, V> {}
impl<K, V> AceDecodeCyclic for alloc::collections::BTreeMap<K, V>
where
    K: AceElement + Ord,
    V: AceDecodeCyclic,
{
    type Plan = alloc::collections::BTreeMap<K, V::Plan>;
    fn decode_plan(
        ctx: &mut crate::decode::DecodeCtx,
    ) -> Result<alloc::collections::BTreeMap<K, V::Plan>> {
        ctx.map::<K, V>()
    }
    fn build_from_plan(
        ctx: &mut crate::decode::BuildCtx<'_>,
        plan: alloc::collections::BTreeMap<K, V::Plan>,
    ) -> alloc::collections::BTreeMap<K, V> {
        plan.into_iter()
            .map(|(key, value)| (key, V::build_from_plan(ctx, value)))
            .collect()
    }
}
impl<K: AceElement + Ord, V: AceEncode> AceEncode for BTreeMap<K, V> {
    fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.enter_collection()?;
        let mut encode = || -> Result<()> {
            let id = encode_map_body(self, e, |v, e| v.ace_encode_element(e))?;
            e.encode_scalar(u64::from(id).to_le_bytes())
        };
        let encoded = encode();
        // 先离开集合，再传播编码错误。
        e.leave_collection();
        encoded
    }
    /// 身份目标体（设计 r4 目标布局表）：`u32 count` 与按 Ord 的键值对直接写入 capture 已打开的目标记录。
    fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
        e.capture_native(|e| {
            e.enter_collection()?;
            let encoded = check_map_body(self, e)
                .and_then(|()| write_map_items(self, e, |v, e| v.ace_encode_element(e)));
            // 先离开集合，再传播映射编码的错误。
            e.leave_collection();
            encoded
        })
    }
    fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.enter_collection()?;
        let id = encode_map_body(self, e, |v, e| v.ace_encode_element(e));
        // 先离开集合，再传播映射编码的错误。
        e.leave_collection();
        e.write_element_bytes(&id?.to_le_bytes())
    }
}
impl<K: AceElement + Ord, V: AceElement> AceDecode for BTreeMap<K, V> {
    fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
        d.enter_collection()?;
        let decoded = d.decode_required_scalar().and_then(|payload| {
            let id =
                u32::try_from(u64::from_le_bytes(payload)).map_err(|_| Error::InvalidReference)?;
            d.begin_record_view(id)?;
            // 先关闭记录视图，再传播元素解码的错误。
            let decoded = decode_btreemap_view(d);
            d.end_record_view();
            decoded
        });
        // 先离开集合，再传播解码错误。
        d.leave_collection();
        decoded
    }
    /// 解码并复用 `out` 中同键旧值的容器缓冲（需求 R22①）：Map 节点与键仍重建，值的内层集合经
    /// `V::decode_element_into` 保留原有分配。失败时 `out` 内容不作承诺。
    fn ace_decode_into(d: &mut Decoder<'_>, out: &mut Self) -> Result<()> {
        d.enter_collection()?;
        let decoded = d.decode_required_scalar().and_then(|payload| {
            let id =
                u32::try_from(u64::from_le_bytes(payload)).map_err(|_| Error::InvalidReference)?;
            d.begin_record_view(id)?;
            // 先关闭记录视图，再传播元素解码的错误。
            let decoded = decode_btreemap_view_into(d, out);
            d.end_record_view();
            decoded
        });
        // 先离开集合，再传播解码错误。
        d.leave_collection();
        decoded
    }
}

/// 解码当前记录视图的体。count 在读取任何元素前检查上限，且不用于预分配；键重复返回 `DuplicateKey`。
fn decode_btreemap_view<K: AceElement + Ord, V: AceElement>(
    d: &mut Decoder<'_>,
) -> Result<BTreeMap<K, V>> {
    decode_btreemap_view_with(d, K::decode_map_key, V::decode_element)
}

/// 同 `decode_btreemap_view`，但键和值分别由传入的解码函数处理，以保留映射键上下文。
fn decode_btreemap_view_with<K, V, FK, FV>(
    d: &mut Decoder<'_>,
    decode_key: FK,
    decode_value: FV,
) -> Result<BTreeMap<K, V>>
where
    K: AceElement + Ord,
    V: AceElement,
    FK: Fn(&mut Decoder<'_>) -> Result<K>,
    FV: Fn(&mut Decoder<'_>) -> Result<V>,
{
    let count = d.read_element_id()? as usize;
    d.check_collection_len(count)?;
    let mut map = BTreeMap::new();
    d.begin_element();
    let decoded = (0..count).try_for_each(|_| {
        let k = decode_key(d)?;
        let v = decode_value(d)?;
        // `insert` 会先替换旧值再返回它，等于让后者覆盖前者；`entry` 命中即停，不动旧值。
        match map.entry(k) {
            btree_map::Entry::Occupied(_) => return Err(Error::DuplicateKey),
            btree_map::Entry::Vacant(slot) => {
                slot.insert(v);
            }
        }
        Ok(())
    });
    // 先结束元素作用域，再传播元素错误。
    d.end_element();
    decoded?;
    check_tail(d)?;
    Ok(map)
}

/// 解码当前记录视图的体并把同键旧值交给 `V::decode_element_into`（需求 R22①）：同键旧值在
/// 原位置被借用着写入，成功后才移入新 Map，故其容器分配保留；无同键旧值时照旧解码新值。Map
/// 节点与键仍重建：新 Map 一律插入本次解码出的新键，命中的旧键随旧条目一并丢弃（键的
/// `Eq`/`Ord` 可以比线上载荷窄，不能用取回的旧键）。写入失败时被拒写的旧值仍留在 `out` 的原键
/// 位置（R17：不可用单元格保持原状）；已移入新 Map 的其它键随新 Map 一并丢弃，不作承诺。与
/// [`decode_btreemap_view`] 一样，先解码本对的值、再判重复键（`DuplicateKey`）。
fn decode_btreemap_view_into<K: AceElement + Ord, V: AceElement>(
    d: &mut Decoder<'_>,
    out: &mut BTreeMap<K, V>,
) -> Result<()> {
    let count = d.read_element_id()? as usize;
    d.check_collection_len(count)?;
    let mut map = BTreeMap::new();
    d.begin_element();
    let decoded = (0..count).try_for_each(|_| {
        let k = K::decode_map_key(d)?;
        // 新键 `k` 是要插入新 Map 的那一枚：按键查旧值、在原位置借用旧值执行 into，成功后才按同一
        // 键移出旧值，旧键随旧条目一起丢弃（键按值语义重建；键的 `Eq`/`Ord` 可能比线上载荷窄，
        // 取回旧键会让新 Map 留下旧键载荷）。失败时旧值不离开 `out`。
        let v = match out.get_mut(&k) {
            Some(old) => {
                V::decode_element_into(d, old)?;
                out.remove(&k).expect("刚借用过该键")
            }
            None => V::decode_element(d)?,
        };
        // `insert` 会先替换旧值再返回它，等于让后者覆盖前者；`entry` 命中即停，不动旧值。
        match map.entry(k) {
            btree_map::Entry::Occupied(_) => return Err(Error::DuplicateKey),
            btree_map::Entry::Vacant(slot) => {
                slot.insert(v);
            }
        }
        Ok(())
    });
    // 先结束元素作用域，再传播元素错误。
    d.end_element();
    decoded?;
    check_tail(d)?;
    *out = map;
    Ok(())
}

/// 元素形式（FR-W09）：元素体为独立映射记录的 `u32` id，不走 bulk。
impl<K: AceElement + Ord, V: AceElement> AceElement for BTreeMap<K, V> {
    fn element_kind() -> ElementKind {
        ElementKind::Ref
    }
    fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
        crate::hooks::note_element();
        e.enter_collection()?;
        let id = encode_map_body(self, e, |v, e| v.encode_element(e));
        // 先离开集合，再传播映射编码的错误。
        e.leave_collection();
        e.write_element_bytes(&id?.to_le_bytes())
    }
    fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
        d.enter_collection()?;
        if let Err(error) = d.begin_record_view(id) {
            d.leave_collection();
            return Err(error);
        }
        let decoded = decode_btreemap_view(d);
        // 先关闭记录视图并离开集合，再传播元素或尾部错误。
        d.end_record_view();
        d.leave_collection();
        decoded
    }
    /// 元素级复用（R22①）：`Vec<BTreeMap<..>>` 的既有元素按同键旧值复用其值缓冲解码，Map 节点
    /// 与键仍重建。
    fn decode_element_into(d: &mut Decoder<'_>, target: &mut Self) -> Result<()> {
        crate::hooks::note_element();
        let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
        d.enter_collection()?;
        if let Err(error) = d.begin_record_view(id) {
            d.leave_collection();
            return Err(error);
        }
        let decoded = decode_btreemap_view_into(d, target);
        // 先关闭记录视图并离开集合，再传播元素或尾部错误。
        d.end_record_view();
        d.leave_collection();
        decoded
    }
    fn check_map_key(&self, opts: &Options) -> Result<()> {
        self.iter().try_for_each(|(k, v)| {
            k.check_map_key(opts)?;
            v.check_map_key(opts)
        })
    }
    fn decode_map_key(d: &mut Decoder<'_>) -> Result<Self> {
        crate::hooks::note_element();
        let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
        d.enter_collection()?;
        if let Err(error) = d.begin_record_view(id) {
            d.leave_collection();
            return Err(error);
        }
        let decoded = decode_btreemap_view_with(d, K::decode_map_key, V::decode_map_key);
        // 先关闭记录视图并离开集合，再传播元素或尾部错误。
        d.end_record_view();
        d.leave_collection();
        decoded
    }
}

/// `$bulk` optionally names the bulk hook, e.g. `encode_bulk_hook`, for types with a bulk path.
macro_rules! inline_value {
    ($ty:ty, $encode:expr, $decode:expr $(, $bulk:ident)?) => {
        impl NonOptional for $ty {}
        impl AceEncode for $ty {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                let bytes = ($encode)(*self).map_err(|err| e.fail(err))?;
                e.encode_scalar(bytes)
            }
            fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.capture_native(|e| <Self as AceElement>::encode_element(self, e))
            }
            fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                <Self as AceElement>::encode_element(self, e)
            }
            value_size_hook!();
            $( $bulk!(); )?
        }
        impl AceDecode for $ty {
            fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
                ($decode)(&d.decode_required_scalar()?)
            }
        }
    };
}
inline_value!(
    value::DateTime,
    |v: value::DateTime| Ok(value::encode_datetime(v.ticks, v.kind)?.to_le_bytes()),
    |p: &[u8; 8]| {
        let (ticks, kind) = value::decode_datetime(u64::from_le_bytes(*p))?;
        Ok(value::DateTime { ticks, kind })
    }
);
inline_value!(
    value::IntPtr,
    |v: value::IntPtr| Ok(value::encode_intptr(v.0)),
    |p| Ok(value::IntPtr(value::decode_intptr(p))),
    encode_bulk_hook
);
inline_value!(
    value::UIntPtr,
    |v: value::UIntPtr| Ok(value::encode_uintptr(v.0)),
    |p| Ok(value::UIntPtr(value::decode_uintptr(p))),
    encode_bulk_hook
);
inline_value!(
    value::ToolkitsU64,
    |v: value::ToolkitsU64| Ok(value::encode_toolkits_guid(v.0 as i64)),
    |p| Ok(value::ToolkitsU64(value::decode_toolkits_guid(p) as u64)),
    encode_bulk_hook
);

/// `$bulk` optionally names the bulk hook, e.g. `encode_bulk_hook`, for types with a bulk path.
macro_rules! record_value {
    ($ty:ty, $encode:expr, $decode:expr $(, $bulk:ident)?) => {
        impl NonOptional for $ty {}
        impl AceEncode for $ty {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                let bytes = ($encode)(*self).map_err(|err| e.fail(err))?;
                e.encode_record(&bytes)
            }
            fn ace_encode_target(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.capture_native(|e| <Self as AceElement>::encode_element(self, e))
            }
            fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                <Self as AceElement>::encode_element(self, e)
            }
            value_size_hook!();
            $( $bulk!(); )?
        }
        impl AceDecode for $ty {
            fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
                ($decode)(d.decode_record()?.as_fixed()?)
            }
        }
    };
}
record_value!(
    value::Guid,
    |v| Ok(value::encode_guid(v)),
    |p| Ok(value::decode_guid(p)),
    encode_bulk_hook
);
record_value!(
    value::DateTimeOffset,
    |v: value::DateTimeOffset| value::encode_datetime_offset(v.clock_ticks, v.offset_minutes),
    |p| {
        let (clock_ticks, offset_minutes) = value::decode_datetime_offset(p)?;
        Ok(value::DateTimeOffset {
            clock_ticks,
            offset_minutes,
        })
    }
);

#[cfg(test)]
mod type_tests {
    use super::*;
    use crate::{error::Error, limits::Options};
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    #[cfg(feature = "std")]
    use std::sync::RwLock;
    fn opts() -> Options {
        Options {
            packed: false,
            ..Options::default()
        }
    }

    /// B2：启用判定是**独立 OR**——体长可预知且（能力访问器 `Some` **或**旧直写 `direct != None`）。
    /// OR 成立后优先用访问器指针，否则（仅 `direct` 声明）用传入的该 trait 方法指针。
    #[test]
    fn r23_enable_judgement_is_independent_or() {
        fn hook(_items: &[u8], _dst: &mut [u8]) -> Result<Option<usize>> {
            Ok(None)
        }
        fn method(_items: &[u8], _dst: &mut [u8]) -> Result<Option<usize>> {
            Ok(None)
        }
        fn direct(_items: &[u8], _e: &mut Encoder<'_>) -> Result<()> {
            Ok(())
        }
        let accessor_ptr: fn(&[u8], &mut [u8]) -> Result<Option<usize>> = hook;
        let method_ptr: fn(&[u8], &mut [u8]) -> Result<Option<usize>> = method;

        // 体长不可预知→永不启用；两者都未声明→不启用。
        assert!(enable_bulk_into::<u8>(None, Some(hook), method, Some(direct)).is_none());
        assert!(enable_bulk_into::<u8>(Some(8), None, method, None).is_none());

        // 仅访问器声明→启用且给出访问器指针（优先于 trait 方法指针）。
        let enable = enable_bulk_into::<u8>(Some(8), Some(hook), method, None)
            .expect("访问器 Some 必须启用");
        assert_eq!(enable.len, 8);
        assert_eq!(enable.hook, accessor_ptr);

        // 仅旧直写声明（访问器 None）→独立 OR 仍启用，且选到该 trait 方法指针。
        let enable = enable_bulk_into::<u8>(Some(8), None, method, Some(direct))
            .expect("direct != None 必须使独立 OR 判据成立");
        assert_eq!(enable.len, 8);
        assert_eq!(
            enable.hook, method_ptr,
            "仅 direct 声明时必须选到该 trait 方法，而不是直接跳到旧直写"
        );
    }
    // 字段位于根对象帧内（设计 §1 第 2 项）：离开根对象时 leave_object 把其 record id 写入根槽（FR-W10），无需先写根槽。
    fn field<T: AceEncode>(v: &T) -> Vec<u8> {
        let mut e = Encoder::new(&opts());
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        v.ace_encode(&mut e).unwrap();
        e.leave_object().unwrap();
        e.finish().unwrap()
    }
    fn round_field<T: AceDecode>(bytes: &[u8]) -> T {
        let mut d = Decoder::new(bytes, &opts()).unwrap();
        let root_scalar = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root_scalar)).unwrap();
        d.begin_field(1).unwrap();
        let v = T::ace_decode(&mut d).unwrap();
        d.leave_object().unwrap();
        d.finish().unwrap();
        v
    }
    fn root<T: AceEncode>(v: &T) -> Vec<u8> {
        let mut e = Encoder::new(&opts());
        v.ace_encode(&mut e).unwrap();
        e.finish().unwrap()
    }
    struct AlwaysDefault;
    impl NonOptional for AlwaysDefault {}
    impl AceEncode for AlwaysDefault {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.encode_scalar([0; 8])
        }
    }
    fn element_message_with<T>(
        value: &T,
        encode: impl FnOnce(&T, &mut Encoder<'_>) -> Result<()>,
    ) -> Result<Vec<u8>> {
        let mut e = Encoder::new(&opts());
        e.begin_record()?;
        e.begin_element();
        let encoded = encode(value, &mut e);
        e.end_element();
        encoded?;
        let id = e.end_record()?;
        e.write_root_scalar(u64::from(id).to_le_bytes())?;
        e.finish()
    }
    fn ace_element_message<T: AceElement>(value: &T) -> Result<Vec<u8>> {
        element_message_with(value, <T as AceElement>::encode_element)
    }
    fn n01_element_message<T: AceEncode>(value: &T) -> Result<Vec<u8>> {
        element_message_with(value, <T as AceEncode>::ace_encode_element)
    }
    fn single_element_body(bytes: &[u8]) -> Vec<u8> {
        let mut d = Decoder::new(bytes, &opts()).unwrap();
        let id = u32::try_from(u64::from_le_bytes(d.read_root_scalar().unwrap())).unwrap();
        d.begin_record_view(id).unwrap();
        let body = d.read_body_to_end().unwrap();
        d.end_record_view();
        d.finish().unwrap();
        body
    }
    #[test]
    fn r_s3_g_n01_default_is_invalid_argument() {
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        assert_eq!(
            AlwaysDefault.ace_encode_element(&mut e),
            Err(Error::InvalidArgument)
        );
        e.end_element();
        e.end_record().unwrap();
    }
    #[test]
    fn r_s3_g_n01_matches_element_writer_for_22_families() {
        fn assert_matches<T: AceEncode + AceElement>(value: &T) {
            let n01 = n01_element_message(value);
            let element = ace_element_message(value);
            assert!(n01.is_ok(), "N01 encoding failed: {n01:?}");
            assert!(element.is_ok(), "element encoding failed: {element:?}");
            assert_eq!(n01.unwrap(), element.unwrap());
        }
        assert_matches(&-8i8);
        assert_matches(&-1600i16);
        assert_matches(&-320_000i32);
        assert_matches(&-6_400_000_000i64);
        assert_matches(&-123isize);
        assert_matches(&8u8);
        assert_matches(&1600u16);
        assert_matches(&320_000u32);
        assert_matches(&6_400_000_000u64);
        assert_matches(&123usize);
        assert_matches(&true);
        assert_matches(&'中');
        assert_matches(&1.25f32);
        assert_matches(&-2.5f64);
        assert_matches(&value::Guid(core::array::from_fn(|index| index as u8)));
        assert_matches(&value::DateTime {
            ticks: 42,
            kind: value::Kind::Utc,
        });
        assert_matches(&value::DateTimeOffset {
            clock_ticks: 42,
            offset_minutes: 0,
        });
        assert_matches(&value::IntPtr(-42));
        assert_matches(&value::UIntPtr(42));
        assert_matches(&value::ToolkitsU64(0x8000_0000_0000_0001));
        assert_matches(&String::from("element"));
        assert_matches(&Some(37u32));
    }
    #[test]
    fn r_s3_g_n01_option_presence_matches_element_form() {
        let none = None::<u32>;
        assert_eq!(
            n01_element_message(&none).map(|bytes| single_element_body(&bytes)),
            Ok(alloc::vec![0])
        );
        let some = Some(5u32);
        assert_eq!(n01_element_message(&some), ace_element_message(&some));
        let some_string = Some(String::from("presence"));
        assert_eq!(
            n01_element_message(&some_string),
            ace_element_message(&some_string)
        );
    }
    #[test]
    fn r_s3_g_01_option_none_field_consumes_selection() {
        let mut e = Encoder::new(&opts());
        e.enter_object().unwrap();
        e.begin_field(0).unwrap();
        None::<u32>.ace_encode(&mut e).unwrap();
        assert_eq!(e.encode_scalar([7; 8]), Err(Error::InvalidArgument));
    }
    #[test]
    fn r_s3_g_01_leaf_rc_root() {
        let value = alloc::rc::Rc::new(37u32);
        let mut encoder = Encoder::new(&opts());
        let encoded = value.ace_encode(&mut encoder);
        let actual = encoded.and_then(|()| encoder.finish());
        assert_eq!(
            actual,
            Ok(alloc::vec![
                12, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 37, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 1, 0, 0, 0, 0, 0, 0, 0,
            ])
        );
    }
    #[test]
    fn r_s3_g_01_leaf_rc_nested_order() {
        let value = alloc::rc::Rc::new(alloc::rc::Rc::new(37u32));
        let mut encoder = Encoder::new(&opts());
        let encoded = value.ace_encode(&mut encoder);
        let actual = encoded.and_then(|()| encoder.finish());
        assert_eq!(
            actual,
            Ok(alloc::vec![
                24, 0, 0, 0, 2, 0, 0, 0, 4, 0, 0, 0, 37, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 2, 0, 0,
                0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
            ])
        );
    }
    #[test]
    fn r_s3_g_01_leaf_rc_string() {
        let value = alloc::rc::Rc::new(String::from("ok"));
        let mut encoder = Encoder::new(&opts());
        let encoded = value.ace_encode(&mut encoder);
        let actual = encoded.and_then(|()| encoder.finish());
        assert_eq!(
            actual,
            Ok(alloc::vec![
                10, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, b'o', b'k', 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 1, 0, 0, 0, 0, 0, 0, 0,
            ])
        );
    }
    #[test]
    fn r_s3_g_01_identity_keeps_allocation_alive() {
        let mut encoder = Encoder::new(&opts());
        encoder.begin_record().unwrap();
        encoder.begin_element();
        let a = alloc::rc::Rc::new(1u32);
        a.ace_encode_element(&mut encoder).unwrap();
        drop(a);
        let b = alloc::rc::Rc::new(2u32);
        b.ace_encode_element(&mut encoder).unwrap();
        encoder.end_element();
        let outer = encoder.end_record().unwrap();
        encoder
            .write_root_scalar(u64::from(outer).to_le_bytes())
            .unwrap();
        assert_eq!(
            encoder.finish(),
            Ok(alloc::vec![
                40, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 4, 0, 0, 0, 2, 0, 0,
                0, 3, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 3, 0, 0, 0, 0, 0, 0, 0,
            ])
        );
    }
    #[test]
    fn r_s3_g_01_leaf_rc_str_ref() {
        let value = alloc::rc::Rc::new("ok");
        let mut encoder = Encoder::new(&opts());
        let encoded = value.ace_encode(&mut encoder);
        let actual = encoded.and_then(|()| encoder.finish());
        assert_eq!(
            actual,
            Ok(alloc::vec![
                10, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, b'o', b'k', 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 1, 0, 0, 0, 0, 0, 0, 0,
            ])
        );
    }
    #[test]
    fn r_s3_g_01_target_without_commit_is_error() {
        struct NoCommit;
        impl NonOptional for NoCommit {}
        impl AceEncode for NoCommit {
            fn ace_encode(&self, _encoder: &mut Encoder<'_>) -> Result<()> {
                Ok(())
            }
            fn ace_encode_target(&self, _encoder: &mut Encoder<'_>) -> Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            alloc::rc::Rc::new(NoCommit).ace_encode(&mut Encoder::new(&opts())),
            Err(Error::InvalidArgument)
        );
    }
    fn target_message<T: AceEncode>(value: &T) -> Result<Vec<u8>> {
        let mut encoder = Encoder::new(&opts());
        value.ace_encode(&mut encoder)?;
        encoder.finish()
    }
    fn target_rec(id: i32, body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as i32).to_le_bytes());
        bytes.extend_from_slice(body);
        bytes
    }
    fn target_slot(depth: i32, tag: i32, payload: [u8; 8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&depth.to_le_bytes());
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }
    fn target_raw(main: &[u8], assist: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(main.len() as i32).to_le_bytes());
        bytes.extend_from_slice(main);
        bytes.extend_from_slice(&(assist.len() as i32).to_le_bytes());
        bytes.extend_from_slice(assist);
        bytes
    }
    struct ObjB1 {
        v: i32,
    }
    impl AceEncode for ObjB1 {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            e.begin_field(0)?;
            let encoded = self.v.ace_encode(e);
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    #[test]
    fn r_s3_g_01_option_target_root() {
        let some = alloc::rc::Rc::new(Some(37u32));
        let main = target_rec(1, &[1, 37, 0, 0, 0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        assert_eq!(main.len(), 13);
        assert_eq!(assist.len(), 16);
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 37);
        assert_eq!(target_message(&some), Ok(expected));

        let none = alloc::rc::Rc::new(None::<u32>);
        let main = target_rec(1, &[0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        assert_eq!(main.len(), 9);
        assert_eq!(assist.len(), 16);
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 33);
        assert_eq!(target_message(&none), Ok(expected));
    }
    #[test]
    fn r_s3_g_01_object_target_root() {
        let value = alloc::rc::Rc::new(ObjB1 { v: 37 });
        let main = target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]);
        let assist = [
            target_slot(0, 0, 1u64.to_le_bytes()),
            target_slot(1, 0, 37u64.to_le_bytes()),
        ]
        .concat();
        assert_eq!(main.len(), 16);
        assert_eq!(assist.len(), 32);
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 56);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_01_object_target_shared_child() {
        struct Parent {
            a: alloc::rc::Rc<ObjB1>,
            b: alloc::rc::Rc<ObjB1>,
        }
        impl AceEncode for Parent {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                self.a.ace_encode(e)?;
                e.begin_field(1)?;
                self.b.ace_encode(e)?;
                e.leave_object()?;
                Ok(())
            }
        }
        let child = alloc::rc::Rc::new(ObjB1 { v: 37 });
        let value = Parent {
            a: alloc::rc::Rc::clone(&child),
            b: alloc::rc::Rc::clone(&child),
        };
        let mut main = target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]);
        main.extend_from_slice(&target_rec(2, &[2, 0, 0, 0, 2, 0, 0, 0]));
        let assist = [
            target_slot(0, 0, 2u64.to_le_bytes()),
            target_slot(2, 0, 37u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 1u64.to_le_bytes()),
        ]
        .concat();
        assert_eq!(main.len(), 32);
        assert_eq!(assist.len(), 64);
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 104);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_02_weak_first_then_owner_shares_id() {
        struct P {
            weak: alloc::rc::Weak<u32>,
            owner: alloc::rc::Rc<u32>,
        }
        impl AceEncode for P {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                self.weak.ace_encode(e)?;
                e.begin_field(1)?;
                self.owner.ace_encode(e)?;
                e.leave_object()?;
                Ok(())
            }
        }
        let owner = alloc::rc::Rc::new(37u32);
        let value = P {
            weak: alloc::rc::Rc::downgrade(&owner),
            owner: alloc::rc::Rc::clone(&owner),
        };
        let main = [
            target_rec(1, &[37, 0, 0, 0]),
            target_rec(2, &[1, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 2u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 1u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 84);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_02_weak_target_in_rc_is_record_body() {
        struct P {
            a: alloc::rc::Rc<alloc::rc::Weak<u32>>,
            b: alloc::rc::Rc<u32>,
        }
        impl AceEncode for P {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                self.a.ace_encode(e)?;
                e.begin_field(1)?;
                self.b.ace_encode(e)?;
                e.leave_object()?;
                Ok(())
            }
        }
        let owner = alloc::rc::Rc::new(37u32);
        let value = P {
            a: alloc::rc::Rc::new(alloc::rc::Rc::downgrade(&owner)),
            b: alloc::rc::Rc::clone(&owner),
        };
        let main = [
            target_rec(1, &[2, 0, 0, 0]),
            target_rec(2, &[37, 0, 0, 0]),
            target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 2u64.to_le_bytes()),
        ]
        .concat();
        assert_eq!(main.len(), 40);
        assert_eq!(assist.len(), 48);
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 96);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_02_option_weak_root_expired_is_none() {
        assert_eq!(
            target_message(&Some(alloc::rc::Weak::<u32>::new())),
            Ok(alloc::vec![0u8; 8])
        );
    }
    #[test]
    fn r_s3_g_02_option_weak_field_expired_omitted() {
        struct Q {
            w: Option<alloc::rc::Weak<u32>>,
        }
        impl AceEncode for Q {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                self.w.ace_encode(e)?;
                e.leave_object()?;
                Ok(())
            }
        }
        let value = Q {
            w: Some(alloc::rc::Weak::<u32>::new()),
        };
        let main = target_rec(1, &[1, 0, 0, 0, 0, 0, 0, 0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 40);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_02_option_weak_target_expired_is_zero() {
        let value = alloc::rc::Rc::new(Some(alloc::rc::Weak::<u32>::new()));
        let main = target_rec(1, &[0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 33);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_02_option_weak_target_live_shares_id() {
        struct P {
            a: alloc::rc::Rc<Option<alloc::rc::Weak<u32>>>,
            b: alloc::rc::Rc<u32>,
        }
        impl AceEncode for P {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                self.a.ace_encode(e)?;
                e.begin_field(1)?;
                self.b.ace_encode(e)?;
                e.leave_object()?;
                Ok(())
            }
        }
        let owner = alloc::rc::Rc::new(37u32);
        let value = P {
            a: alloc::rc::Rc::new(Some(alloc::rc::Rc::downgrade(&owner))),
            b: alloc::rc::Rc::clone(&owner),
        };
        let main = [
            target_rec(1, &[1, 2, 0, 0, 0]),
            target_rec(2, &[37, 0, 0, 0]),
            target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 2u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 97);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_02_expired_weak_is_error() {
        assert_eq!(
            target_message(&alloc::rc::Weak::<u32>::new()),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_02_external_owner_is_error() {
        let owner = alloc::rc::Rc::new(37u32);
        let weak = alloc::rc::Rc::downgrade(&owner);
        let mut encoder = Encoder::new(&opts());
        assert_eq!(weak.ace_encode(&mut encoder), Ok(()));
        assert_eq!(encoder.finish(), Err(Error::InvalidArgument));
    }
    #[test]
    fn r_s3_g_02_weak_self_cycle_is_active_reference() {
        struct Node {
            me: alloc::rc::Weak<Node>,
        }
        impl AceEncode for Node {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                let encoded = self.me.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        let value = alloc::rc::Rc::new_cyclic(|me| Node { me: me.clone() });
        let main = target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]);
        let assist = [
            target_slot(0, 0, 1u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 56);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_r4_refcell_root_option_round_trip() {
        let none = None::<RefCell<u32>>;
        let zero = Some(RefCell::new(0u32));
        let some = Some(RefCell::new(37u32));
        assert_eq!(root(&none), [0; 8]);
        assert_eq!(root(&zero).len(), 24);
        assert_eq!(
            root(&some),
            [0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 37, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_ne!(root(&none), root(&zero));
        for value in [none, zero, some] {
            let bytes = root(&value);
            let mut d = Decoder::new(&bytes, &opts()).unwrap();
            assert_eq!(Option::<RefCell<u32>>::ace_decode(&mut d).unwrap(), value);
            d.finish().unwrap();
        }
    }
    #[test]
    fn r_s3_g_r4_refcell_option_elements_presence() {
        let v = alloc::vec![None, Some(RefCell::new(0u32)), Some(RefCell::new(37u32))];
        let bytes = root(&v);
        assert_eq!(bytes.len(), 47);
        assert_eq!(
            single_element_body(&bytes),
            [3, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 37, 0, 0, 0]
        );
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(<Vec<Option<RefCell<u32>>>>::ace_decode(&mut d).unwrap(), v);
        d.finish().unwrap();
        // Byte 17 is the presence of `Some(0)`: 4 envelope + 8 record header + 4 count + 1 for `None`.
        let mut corrupt = bytes.clone();
        corrupt[17] = 2;
        let mut d = Decoder::new(&corrupt, &opts()).unwrap();
        assert_eq!(
            <Vec<Option<RefCell<u32>>>>::ace_decode(&mut d),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn r_s3_g_r4_refcell_option_reserved_target() {
        let some = alloc::rc::Rc::new(Some(RefCell::new(37u32)));
        let main = target_rec(1, &[1, 37, 0, 0, 0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 37);
        assert_eq!(target_message(&some), Ok(expected));

        let none = alloc::rc::Rc::new(None::<RefCell<u32>>);
        let main = target_rec(1, &[0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 33);
        assert_eq!(target_message(&none), Ok(expected));
    }
    #[test]
    fn r_s3_g_r4_refcell_borrow_failure() {
        let opt = Some(RefCell::new(37u32));
        let g = opt.as_ref().unwrap().borrow_mut();
        let mut e = Encoder::new(&opts());
        assert_eq!(opt.ace_encode(&mut e), Err(Error::InvalidArgument));
        drop(g);
        assert_eq!(
            root(&opt),
            [0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 37, 0, 0, 0, 0, 0, 0, 0]
        );
    }
    #[test]
    fn r_s2_d1_ignored_refcell_borrow_error_latches_finish() {
        let cell = RefCell::new(37u32);
        let _guard = cell.borrow_mut();
        let mut e = Encoder::new(&opts());
        let _ = cell.ace_encode(&mut e);
        assert_eq!(e.finish(), Err(Error::InvalidArgument));
    }
    #[test]
    fn r_s2_d1_ignored_date_helper_error_latches_finish() {
        let mut e = Encoder::new(&opts());
        let _ = value::DateTime {
            ticks: 1u64 << 62,
            kind: value::Kind::Unspecified,
        }
        .ace_encode(&mut e);
        assert_eq!(e.finish(), Err(Error::InvalidArgument));
        let mut e = Encoder::new(&opts());
        let _ = value::DateTimeOffset {
            clock_ticks: -1,
            offset_minutes: 0,
        }
        .ace_encode(&mut e);
        assert_eq!(e.finish(), Err(Error::InvalidArgument));
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_r4_rwlock_option_round_trip_and_target() {
        let none = None::<RwLock<u32>>;
        let some = Some(RwLock::new(37u32));
        assert_eq!(root(&none), [0; 8]);
        assert_eq!(
            root(&some),
            [0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 37, 0, 0, 0, 0, 0, 0, 0]
        );
        let bytes = root(&none);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        let decoded = Option::<RwLock<u32>>::ace_decode(&mut d).unwrap();
        d.finish().unwrap();
        assert!(decoded.is_none());
        let bytes = root(&some);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        let decoded = Option::<RwLock<u32>>::ace_decode(&mut d).unwrap();
        d.finish().unwrap();
        assert_eq!(decoded.map(|lock| *lock.read().unwrap()), Some(37));
        let rc_some = alloc::rc::Rc::new(Some(RwLock::new(37u32)));
        let main = target_rec(1, &[1, 37, 0, 0, 0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 37);
        assert_eq!(target_message(&rc_some), Ok(expected));
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_r4_rwlock_option_elements_and_failure() {
        let v = alloc::vec![None, Some(RwLock::new(0u32)), Some(RwLock::new(37u32))];
        let bytes = root(&v);
        assert_eq!(bytes.len(), 47);
        assert_eq!(
            single_element_body(&bytes),
            [3, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 37, 0, 0, 0]
        );
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        let decoded = <Vec<Option<RwLock<u32>>>>::ace_decode(&mut d).unwrap();
        d.finish().unwrap();
        let values = decoded
            .iter()
            .map(|slot| slot.as_ref().map(|lock| *lock.read().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(values, [None, Some(0), Some(37)]);
        // Byte 17 is the presence of `Some(0)`: 4 envelope + 8 record header + 4 count + 1 for `None`.
        let mut corrupt = bytes.clone();
        corrupt[17] = 2;
        let mut d = Decoder::new(&corrupt, &opts()).unwrap();
        assert_eq!(
            <Vec<Option<RwLock<u32>>>>::ace_decode(&mut d).err(),
            Some(Error::InvalidTag)
        );
        let zero = Some(RwLock::new(0u32));
        let writer = zero.as_ref().unwrap().write().unwrap();
        let mut e = Encoder::new(&opts());
        assert_eq!(zero.ace_encode(&mut e), Err(Error::InvalidArgument));
        drop(writer);
        let reader = zero.as_ref().unwrap().read().unwrap();
        let shared_bytes = root(&zero);
        drop(reader);
        assert_eq!(shared_bytes.len(), 24);
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_09_rwlock_would_block_poison() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Node {
            value: u32,
            visits: AtomicUsize,
        }
        impl AceEncode for Node {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                self.visits.fetch_add(1, Ordering::Relaxed);
                e.begin_field(0)?;
                let encoded = self.value.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        let node = || Node {
            value: 37,
            visits: AtomicUsize::new(0),
        };
        let shared = alloc::sync::Arc::new(RwLock::new(node()));
        let writer = shared.write().unwrap();
        assert_eq!(target_message(&shared), Err(Error::InvalidArgument));
        drop(writer);
        assert_eq!(shared.read().unwrap().visits.load(Ordering::Relaxed), 0);
        let poisoned = alloc::sync::Arc::new(RwLock::new(node()));
        let victim = alloc::sync::Arc::clone(&poisoned);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = victim.write().unwrap();
            panic!("poison the lock while the write guard is held");
        }));
        assert_eq!(target_message(&poisoned), Err(Error::InvalidArgument));
        let reader = shared.read().unwrap();
        let bytes = target_message(&shared);
        drop(reader);
        let expected = target_raw(
            &target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            &[
                target_slot(0, 0, 1u64.to_le_bytes()),
                target_slot(1, 0, 37u64.to_le_bytes()),
            ]
            .concat(),
        );
        assert_eq!(expected.len(), 56);
        assert_eq!(bytes, Ok(expected));
        assert_eq!(shared.read().unwrap().visits.load(Ordering::Relaxed), 1);
        assert_eq!(root(&RwLock::new(37u32)).len(), 24);
    }
    #[test]
    fn r_s3_g_03_rc_strong_cycles_obey_error_priority() {
        struct Link {
            first: Option<alloc::rc::Rc<RefCell<Link>>>,
            second: Option<alloc::rc::Rc<RefCell<Link>>>,
            visits: Cell<usize>,
        }
        impl AceEncode for Link {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                self.visits.set(self.visits.get() + 1);
                e.begin_field(0)?;
                let encoded = self.first.ace_encode(e).and_then(|()| {
                    e.begin_field(1)?;
                    self.second.ace_encode(e)
                });
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        let link = || Link {
            first: None,
            second: None,
            visits: Cell::new(0),
        };
        // Self-loop: CycleUnsupported under the default and under max_depth 0.
        let shallow = Options {
            max_depth: 0,
            ..opts()
        };
        for options in [opts(), shallow.clone()] {
            let node = alloc::rc::Rc::new(RefCell::new(link()));
            node.borrow_mut().first = Some(alloc::rc::Rc::clone(&node));
            assert_eq!(
                node.ace_encode(&mut Encoder::new(&options)),
                Err(Error::CycleUnsupported)
            );
            assert_eq!(node.borrow().visits.get(), 1);
            node.borrow_mut().first = None;
        }
        // Two-cycle under the default options: both objects enter their body once.
        let a = alloc::rc::Rc::new(RefCell::new(link()));
        let b = alloc::rc::Rc::new(RefCell::new(link()));
        a.borrow_mut().first = Some(alloc::rc::Rc::clone(&b));
        b.borrow_mut().first = Some(alloc::rc::Rc::clone(&a));
        assert_eq!(
            a.ace_encode(&mut Encoder::new(&opts())),
            Err(Error::CycleUnsupported)
        );
        assert_eq!(a.borrow().visits.get(), 1);
        assert_eq!(b.borrow().visits.get(), 1);
        a.borrow_mut().first = None;
        b.borrow_mut().first = None;
        // The first field reaches a non-cyclic object that exceeds max_depth 0, so the back-edge in
        // the second field is never visited.
        let leaf = alloc::rc::Rc::new(RefCell::new(link()));
        let outer = alloc::rc::Rc::new(RefCell::new(Link {
            first: Some(alloc::rc::Rc::clone(&leaf)),
            second: None,
            visits: Cell::new(0),
        }));
        outer.borrow_mut().second = Some(alloc::rc::Rc::clone(&outer));
        assert_eq!(
            outer.ace_encode(&mut Encoder::new(&shallow)),
            Err(Error::DepthExceeded)
        );
        assert_eq!(outer.borrow().visits.get(), 1);
        assert_eq!(leaf.borrow().visits.get(), 0);
        outer.borrow_mut().second = None;
    }
    #[test]
    fn r_s3_g_04_refcell_and_failed_call_drop_state() {
        struct Node {
            value: u32,
            visits: Cell<usize>,
        }
        impl AceEncode for Node {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                self.visits.set(self.visits.get() + 1);
                e.begin_field(0)?;
                let encoded = self.value.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        struct Parent {
            child: alloc::rc::Rc<RefCell<Node>>,
        }
        impl AceEncode for Parent {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                let encoded = self.child.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                Err(Error::InvalidTag)
            }
        }
        let node = || Node {
            value: 37,
            visits: Cell::new(0),
        };
        let expected = target_raw(
            &target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            &[
                target_slot(0, 0, 1u64.to_le_bytes()),
                target_slot(1, 0, 37u64.to_le_bytes()),
            ]
            .concat(),
        );
        // A live RefMut fails with InvalidArgument before the object body is entered.
        let direct = RefCell::new(node());
        let guard = direct.borrow_mut();
        assert_eq!(
            direct.ace_encode(&mut Encoder::new(&opts())),
            Err(Error::InvalidArgument)
        );
        drop(guard);
        assert_eq!(direct.borrow().visits.get(), 0);
        let shared = alloc::rc::Rc::new(RefCell::new(node()));
        let guard = shared.borrow_mut();
        assert_eq!(
            shared.ace_encode(&mut Encoder::new(&opts())),
            Err(Error::InvalidArgument)
        );
        drop(guard);
        assert_eq!(shared.borrow().visits.get(), 0);
        // A shared Ref does not block encoding, for either the direct cell or the Rc root.
        let reader = direct.borrow();
        let direct_bytes = root(&direct);
        drop(reader);
        assert_eq!(direct_bytes, expected);
        assert_eq!(direct.borrow().visits.get(), 1);
        let reader = shared.borrow();
        let shared_bytes = root(&shared);
        drop(reader);
        assert_eq!(shared_bytes, expected);
        assert_eq!(shared.borrow().visits.get(), 1);
        // A shared child completes, then the parent's fixed error is kept; dropping the encoder
        // restores the strong count.
        let child = alloc::rc::Rc::new(RefCell::new(node()));
        let parent = Parent {
            child: alloc::rc::Rc::clone(&child),
        };
        let baseline = alloc::rc::Rc::strong_count(&child);
        assert_eq!(
            parent.ace_encode(&mut Encoder::new(&opts())),
            Err(Error::InvalidTag)
        );
        assert_eq!(alloc::rc::Rc::strong_count(&child), baseline);
        assert_eq!(child.borrow().visits.get(), 1);
        // A new call on the same topology starts from record id 1 and gives the same bytes.
        assert_eq!(root(&child), expected);
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_08_arc_nested_arc_root() {
        let value = alloc::sync::Arc::new(alloc::sync::Arc::new(37u32));
        let main = [target_rec(2, &[37, 0, 0, 0]), target_rec(1, &[2, 0, 0, 0])].concat();
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 48);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_08_arc_option_arc_some() {
        let value = alloc::sync::Arc::new(Some(alloc::sync::Arc::new(37u32)));
        let main = [
            target_rec(2, &[37, 0, 0, 0]),
            target_rec(1, &[1, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 49);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_08_arc_shared_sync_weak_owner() {
        struct P {
            a: alloc::sync::Arc<alloc::sync::Weak<u32>>,
            b: alloc::sync::Arc<u32>,
        }
        impl AceEncode for P {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                self.a.ace_encode(e)?;
                e.begin_field(1)?;
                self.b.ace_encode(e)?;
                e.leave_object()?;
                Ok(())
            }
        }
        let owner = alloc::sync::Arc::new(37u32);
        let value = P {
            a: alloc::sync::Arc::new(alloc::sync::Arc::downgrade(&owner)),
            b: alloc::sync::Arc::clone(&owner),
        };
        let main = [
            target_rec(1, &[2, 0, 0, 0]),
            target_rec(2, &[37, 0, 0, 0]),
            target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 2u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 96);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_08_sync_option_weak_expired_zero() {
        let value = alloc::sync::Arc::new(Some(alloc::sync::Weak::<u32>::new()));
        let main = target_rec(1, &[0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 33);
        assert_eq!(target_message(&value), Ok(expected));
    }
    #[test]
    fn r_s3_g_01_object_target_zero_commit_is_error() {
        struct Silent;
        impl AceEncode for Silent {
            fn ace_encode(&self, _e: &mut Encoder<'_>) -> Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            target_message(&alloc::rc::Rc::new(Silent)),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_object_target_second_object_is_error() {
        struct TwoObj;
        impl AceEncode for TwoObj {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.leave_object()?;
                e.enter_object()?;
                e.leave_object()?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&alloc::rc::Rc::new(TwoObj)),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_object_target_buffered_record_is_error() {
        struct BufferedObj;
        impl AceEncode for BufferedObj {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.begin_record()?;
                e.enter_object()?;
                e.leave_object()?;
                e.end_record()?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&alloc::rc::Rc::new(BufferedObj)),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_object_target_field_selection_is_error() {
        use alloc::rc::Rc;

        struct Misbehaving {
            v: i32,
        }
        impl AceEncode for Misbehaving {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                let encoded = self.v.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                e.begin_field(5)?;
                Ok(())
            }
        }
        struct ParentMis {
            a: alloc::rc::Rc<Misbehaving>,
        }
        impl AceEncode for ParentMis {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                let encoded = self.a.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&ParentMis {
                a: Rc::new(Misbehaving { v: 37 }),
            }),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_object_target_parent_slot_is_error() {
        use alloc::rc::Rc;

        struct M {
            v: i32,
        }
        impl AceEncode for M {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                let encoded = self.v.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                e.write_field_scalar(5, [99, 0, 0, 0, 0, 0, 0, 0])?;
                Ok(())
            }
        }
        struct P {
            a: Rc<M>,
        }
        impl AceEncode for P {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_record()?;
                e.begin_element();
                let encoded = self.a.ace_encode_element(e);
                e.end_element();
                let ended = e.end_record();
                let left = e.leave_object();
                encoded?;
                ended?;
                left?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&P {
                a: Rc::new(M { v: 37 }),
            }),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_object_target_parent_body_is_error() {
        use alloc::rc::Rc;

        struct M {
            v: i32,
        }
        impl AceEncode for M {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                let encoded = self.v.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                e.write_element_bytes(&[7])?;
                Ok(())
            }
        }
        struct P {
            a: Rc<M>,
        }
        impl AceEncode for P {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_record()?;
                e.begin_element();
                let encoded = self.a.ace_encode_element(e);
                e.end_element();
                let ended = e.end_record();
                let left = e.leave_object();
                encoded?;
                ended?;
                left?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&P {
                a: Rc::new(M { v: 37 }),
            }),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_leaf_target_parent_slot_is_error() {
        use alloc::rc::Rc;

        struct Misel;
        impl NonOptional for Misel {}
        impl AceEncode for Misel {
            fn ace_encode(&self, _e: &mut Encoder<'_>) -> Result<()> {
                Ok(())
            }
            fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.write_field_scalar(5, [99, 0, 0, 0, 0, 0, 0, 0])
            }
        }
        struct P2 {
            a: Rc<Option<Misel>>,
        }
        impl AceEncode for P2 {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_record()?;
                e.begin_element();
                let encoded = self.a.ace_encode_element(e);
                e.end_element();
                let ended = e.end_record();
                let left = e.leave_object();
                encoded?;
                ended?;
                left?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&P2 {
                a: Rc::new(Some(Misel)),
            }),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_leaf_target_record_swap_is_error() {
        struct Misel3;
        impl NonOptional for Misel3 {}
        impl AceEncode for Misel3 {
            fn ace_encode(&self, _e: &mut Encoder<'_>) -> Result<()> {
                Ok(())
            }
            fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.end_record()?;
                e.begin_record()?;
                e.write_element_bytes(&[1, 0, 0, 0])?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&alloc::rc::Rc::new(Some(Misel3))),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    fn r_s3_g_01_object_target_parent_record_pop_is_error() {
        use alloc::rc::Rc;

        struct M {
            v: i32,
        }
        impl AceEncode for M {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_field(0)?;
                let encoded = self.v.ace_encode(e);
                let left = e.leave_object();
                encoded?;
                left?;
                e.end_record()?;
                e.begin_record()?;
                Ok(())
            }
        }
        struct P {
            a: Rc<M>,
        }
        impl AceEncode for P {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                e.begin_record()?;
                e.begin_element();
                let encoded = self.a.ace_encode_element(e);
                e.end_element();
                let ended = e.end_record();
                let left = e.leave_object();
                encoded?;
                ended?;
                left?;
                Ok(())
            }
        }
        assert_eq!(
            target_message(&P {
                a: Rc::new(M { v: 37 }),
            }),
            Err(Error::InvalidArgument)
        );
    }
    #[test]
    #[cfg(target_endian = "little")]
    fn ace_element_roundtrips_and_bulk_counters() {
        crate::hooks::reset();
        // Wire bytes are explicitly LE on all hosts; this machine is little-endian.
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        0x01020304i32.encode_element(&mut e).unwrap();
        0xabu8.encode_element(&mut e).unwrap();
        1.5f64.encode_element(&mut e).unwrap();
        true.encode_element(&mut e).unwrap();
        false.encode_element(&mut e).unwrap();
        assert_eq!(e.write_root_scalar([0; 8]), Err(Error::InvalidArgument));
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish_ignoring_latch().unwrap();
        assert_eq!(
            &bytes[12..26],
            &[4, 3, 2, 1, 0xab, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f, 1]
        );
        assert_eq!(bytes[26], 0);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(d.read_root_scalar().unwrap(), u64::from(id).to_le_bytes());
        d.begin_record_view(id).unwrap();
        d.begin_element();
        assert_eq!(i32::decode_element(&mut d), Ok(0x01020304));
        assert_eq!(u8::decode_element(&mut d), Ok(0xab));
        assert_eq!(f64::decode_element(&mut d), Ok(1.5));
        assert_eq!(bool::decode_element(&mut d), Ok(true));
        assert_eq!(bool::decode_element(&mut d), Ok(false));
        assert_eq!(u8::decode_element(&mut d), Err(Error::TruncatedInput));
        d.end_element();
        d.end_record_view();
        d.finish().unwrap();
        assert_eq!(i32::element_kind(), ElementKind::Value { size: 4 });
        assert_eq!(f64::element_kind(), ElementKind::Value { size: 8 });
        assert_eq!(bool::element_kind(), ElementKind::Value { size: 1 });
        crate::hooks::reset();
        let mut out = Vec::new();
        assert!(i32::encode_elements_bulk(&[1, -2], &mut out).unwrap());
        assert_eq!(out, [1, 0, 0, 0, 254, 255, 255, 255]);
        assert_eq!(crate::hooks::snapshot(), (1, 0));
        let mut consumed = 99;
        assert_eq!(
            i32::decode_elements_bulk(2, &out, &mut consumed),
            Ok(Some(alloc::vec![1, -2]))
        );
        assert_eq!(consumed, 8);
        crate::hooks::reset();
        assert_eq!(
            bool::decode_elements_bulk(2, &[1, 2], &mut consumed),
            Err(Error::InvalidTag)
        );
        assert_eq!(consumed, 8);
        assert_eq!(crate::hooks::snapshot(), (0, 0));
        let mut e = Encoder::new(&opts());
        e.write_root_record(&[2]).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(1).unwrap();
        d.begin_element();
        assert_eq!(bool::decode_element(&mut d), Err(Error::InvalidTag));
        assert_eq!(crate::hooks::snapshot(), (0, 1));
        let mut out = Vec::new();
        assert!(u8::encode_elements_bulk(&[3, 4], &mut out).unwrap());
        assert_eq!(
            u8::decode_elements_bulk(2, &out, &mut consumed),
            Ok(Some(alloc::vec![3, 4]))
        );
        out.clear();
        assert!(f64::encode_elements_bulk(&[1.5], &mut out).unwrap());
        assert_eq!(
            f64::decode_elements_bulk(1, &out, &mut consumed),
            Ok(Some(alloc::vec![1.5]))
        );
        out.clear();
        assert!(bool::encode_elements_bulk(&[true, false], &mut out).unwrap());
        assert_eq!(out, [1, 0]);
        assert_eq!(
            bool::decode_elements_bulk(2, &out, &mut consumed),
            Ok(Some(alloc::vec![true, false]))
        );
        assert_eq!(
            bool::decode_elements_bulk(3, &out, &mut consumed),
            Err(Error::TruncatedInput)
        );
    }
    #[test]
    fn numeric_char_element_kinds() {
        assert_eq!(i8::element_kind(), ElementKind::Value { size: 1 });
        assert_eq!(i16::element_kind(), ElementKind::Value { size: 2 });
        assert_eq!(u16::element_kind(), ElementKind::Value { size: 2 });
        assert_eq!(u32::element_kind(), ElementKind::Value { size: 4 });
        assert_eq!(f32::element_kind(), ElementKind::Value { size: 4 });
        assert_eq!(char::element_kind(), ElementKind::Value { size: 4 });
        assert_eq!(i64::element_kind(), ElementKind::Value { size: 8 });
        assert_eq!(u64::element_kind(), ElementKind::Value { size: 8 });
        assert_eq!(isize::element_kind(), ElementKind::Value { size: 8 });
        assert_eq!(usize::element_kind(), ElementKind::Value { size: 8 });
    }
    #[test]
    fn numeric_char_element_boundaries_roundtrip() {
        crate::hooks::reset();
        fn roundtrip<T: AceElement + PartialEq + core::fmt::Debug + Copy>(values: &[T]) {
            let mut e = Encoder::new(&opts());
            e.begin_record().unwrap();
            e.begin_element();
            for v in values {
                v.encode_element(&mut e).unwrap();
            }
            e.end_element();
            let id = e.end_record().unwrap();
            e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &opts()).unwrap();
            d.read_root_scalar().unwrap();
            d.begin_record_view(id).unwrap();
            d.begin_element();
            for v in values {
                assert_eq!(T::decode_element(&mut d), Ok(*v));
            }
            d.end_element();
            d.end_record_view();
            d.finish().unwrap();
        }
        roundtrip(&[i8::MIN, 0, i8::MAX]);
        roundtrip(&[i16::MIN, 0, i16::MAX]);
        roundtrip(&[u16::MIN, 0, u16::MAX]);
        roundtrip(&[u32::MIN, 0, u32::MAX]);
        roundtrip(&[i64::MIN, 0, i64::MAX]);
        roundtrip(&[u64::MIN, 0, u64::MAX]);
        roundtrip(&[isize::MIN, 0, isize::MAX]);
        roundtrip(&[usize::MIN, 0, usize::MAX]);
        roundtrip(&[f32::MIN, 0.0, -0.0_f32, f32::MAX]);
        roundtrip(&['中', char::MAX, '\0']);
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        (-0.0_f32).encode_element(&mut e).unwrap();
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        assert_eq!(
            f32::decode_element(&mut d).unwrap().to_bits(),
            (-0.0_f32).to_bits()
        );
    }
    #[test]
    #[cfg(target_endian = "little")]
    fn numeric_bulk_wire_bytes_and_counters() {
        crate::hooks::reset();
        let mut out = Vec::new();
        assert!(i16::encode_elements_bulk(&[1, -2, 3], &mut out).unwrap());
        assert_eq!(out, [1, 0, 254, 255, 3, 0]);
        assert_eq!(crate::hooks::snapshot(), (1, 0));
        crate::hooks::reset();
        let mut consumed = 99;
        assert_eq!(
            char::decode_elements_bulk(1, &0xd800_u32.to_le_bytes(), &mut consumed),
            Err(Error::InvalidUtf8)
        );
        assert_eq!(consumed, 99);
        assert_eq!(crate::hooks::snapshot().0, 0);
    }
    #[test]
    #[cfg(target_endian = "little")]
    fn bulk_encode_matches_per_element_le_bytes() {
        let i32s = [1_i32, -2, 0x0102_0304];
        let mut out = Vec::new();
        assert!(i32::encode_elements_bulk(&i32s, &mut out).unwrap());
        assert_eq!(
            out,
            i32s.iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>()
        );
        let u16s = [0_u16, 0xabcd, u16::MAX];
        out.clear();
        assert!(u16::encode_elements_bulk(&u16s, &mut out).unwrap());
        assert_eq!(
            out,
            u16s.iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>()
        );
    }
    #[test]
    #[cfg(target_endian = "little")]
    fn bulk_bool_invalid_byte_fails_before_any_value() {
        crate::hooks::reset();
        let mut consumed = 7;
        assert_eq!(
            bool::decode_elements_bulk(2, &[1, 2], &mut consumed),
            Err(Error::InvalidTag)
        );
        assert_eq!(consumed, 7);
        assert_eq!(crate::hooks::snapshot().0, 0);
    }
    #[test]
    #[cfg(target_endian = "little")]
    fn bulk_char_roundtrip_counts_one_bulk_decode() {
        crate::hooks::reset();
        let chars = ['中', char::MAX, '\0'];
        let mut out = Vec::new();
        assert!(char::encode_elements_bulk(&chars, &mut out).unwrap());
        crate::hooks::reset();
        let mut consumed = 0;
        assert_eq!(
            char::decode_elements_bulk(chars.len(), &out, &mut consumed),
            Ok(Some(chars.to_vec()))
        );
        assert_eq!(consumed, out.len());
        assert_eq!(crate::hooks::snapshot(), (1, 0));
    }
    #[test]
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn isize_element_bulk_and_element_counters() {
        crate::hooks::reset();
        let mut out = Vec::new();
        assert!(isize::encode_elements_bulk(&[-1, 2], &mut out).unwrap());
        assert_eq!(
            out,
            [255, 255, 255, 255, 255, 255, 255, 255, 2, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(crate::hooks::snapshot(), (1, 0));
        let mut consumed = 0;
        assert_eq!(
            isize::decode_elements_bulk(2, &out, &mut consumed),
            Ok(Some(alloc::vec![-1, 2]))
        );
        assert_eq!(consumed, 16);
        crate::hooks::reset();
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        7isize.encode_element(&mut e).unwrap();
        e.end_element();
        e.end_record().unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 1));
    }
    #[test]
    #[cfg(not(all(target_endian = "little", target_pointer_width = "64")))]
    fn isize_declines_bulk_off_64bit_le() {
        // Bulk exists only on little-endian 64-bit targets, so isize declines everywhere else.
        let mut out = Vec::new();
        assert_eq!(isize::encode_elements_bulk(&[-1, 2], &mut out), Ok(false));
        assert!(out.is_empty());
        let mut consumed = 0;
        assert_eq!(
            isize::decode_elements_bulk(2, &[0; 16], &mut consumed),
            Ok(None)
        );
        crate::hooks::reset();
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        1isize.encode_element(&mut e).unwrap();
        e.end_element();
        e.end_record().unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 1));
    }
    #[test]
    #[cfg(not(target_endian = "little"))]
    fn bulk_declines_on_big_endian() {
        // Bulk is little-endian only, so these decline and every element takes the per-element path.
        let mut out = Vec::new();
        assert_eq!(i32::encode_elements_bulk(&[1, -2], &mut out), Ok(false));
        assert!(out.is_empty());
        let mut consumed = 0;
        assert_eq!(
            i32::decode_elements_bulk(2, &[1, 0, 0, 0, 254, 255, 255, 255], &mut consumed),
            Ok(None)
        );
        assert_eq!(consumed, 0);
        crate::hooks::reset();
        let bytes = field(&alloc::vec![1, 0, -2]);
        assert_eq!(crate::hooks::snapshot(), (0, 3));
        assert_eq!(
            field_record_body(&bytes),
            [3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 254, 255, 255, 255]
        );
    }
    /// Element-by-element encoding of `values` must produce `wire` as the record body. The bulk
    /// path must produce `wire` in one bulk call, and both decodings must return `values`.
    #[cfg(target_endian = "little")]
    fn assert_element_and_bulk_wire<T: AceElement + Copy + PartialEq + core::fmt::Debug>(
        values: &[T],
        wire: &[u8],
    ) {
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        for v in values {
            v.encode_element(&mut e).unwrap();
        }
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        assert_eq!(d.read_body_to_end().unwrap(), wire);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        for v in values {
            assert_eq!(T::decode_element(&mut d), Ok(*v));
        }
        crate::hooks::reset();
        let mut bulk = Vec::new();
        assert!(T::encode_elements_bulk(values, &mut bulk).unwrap());
        assert_eq!(bulk, wire);
        assert_eq!(crate::hooks::snapshot(), (1, 0));
        let mut consumed = 0;
        assert_eq!(
            T::decode_elements_bulk(values.len(), &bulk, &mut consumed),
            Ok(Some(values.to_vec()))
        );
        assert_eq!(consumed, wire.len());
    }
    #[test]
    fn value_element_kinds() {
        assert_eq!(value::Guid::element_kind(), ElementKind::Value { size: 16 });
        assert_eq!(
            value::IntPtr::element_kind(),
            ElementKind::Value { size: 8 }
        );
        assert_eq!(
            value::UIntPtr::element_kind(),
            ElementKind::Value { size: 8 }
        );
        assert_eq!(
            value::ToolkitsU64::element_kind(),
            ElementKind::Value { size: 8 }
        );
    }
    #[test]
    #[cfg(target_endian = "little")]
    fn guid_element_wire_bytes_and_bulk() {
        // Asymmetric samples: a reversed or mixed-endian layout would change these bytes.
        let first: [u8; 16] = core::array::from_fn(|i| i as u8);
        let second: [u8; 16] = core::array::from_fn(|i| 0x10 + i as u8);
        assert_element_and_bulk_wire(
            &[value::Guid(first), value::Guid(second)],
            &[first, second].concat(),
        );
    }
    #[test]
    #[cfg(target_endian = "little")]
    fn pointer_and_toolkits_element_bulk_matches_le_bytes() {
        let intptrs = [value::IntPtr(-1), value::IntPtr(0x0102_0304_0506_0708)];
        assert_element_and_bulk_wire(
            &intptrs,
            &intptrs
                .iter()
                .flat_map(|v| v.0.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        let uintptrs = [
            value::UIntPtr(u64::MAX),
            value::UIntPtr(0x0102_0304_0506_0708),
        ];
        assert_element_and_bulk_wire(
            &uintptrs,
            &uintptrs
                .iter()
                .flat_map(|v| v.0.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        let toolkits = [
            value::ToolkitsU64(0x8000_0000_0000_0001),
            value::ToolkitsU64(2),
        ];
        assert_element_and_bulk_wire(
            &toolkits,
            &toolkits
                .iter()
                .flat_map(|v| v.0.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
    }
    #[test]
    fn datetime_element_kinds() {
        assert_eq!(
            value::DateTime::element_kind(),
            ElementKind::Value { size: 8 }
        );
        assert_eq!(
            value::DateTimeOffset::element_kind(),
            ElementKind::Value { size: 10 }
        );
    }
    #[test]
    fn datetime_element_bits_and_kind() {
        let samples = [
            value::DateTime {
                ticks: 0,
                kind: value::Kind::Unspecified,
            },
            value::DateTime {
                ticks: 3_155_378_975_999_999_999,
                kind: value::Kind::Utc,
            },
            value::DateTime {
                ticks: 1,
                kind: value::Kind::Local,
            },
        ];
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        for v in &samples {
            v.encode_element(&mut e).unwrap();
        }
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let wire: Vec<u8> = samples
            .iter()
            .map(|v| value::encode_datetime(v.ticks, v.kind).unwrap())
            .flat_map(u64::to_le_bytes)
            .collect();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        assert_eq!(d.read_body_to_end().unwrap(), wire);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        for v in &samples {
            assert_eq!(value::DateTime::decode_element(&mut d), Ok(*v));
        }
        d.end_element();
        d.end_record_view();
        d.finish().unwrap();
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        e.write_element_bytes(&(3_u64 << 62).to_le_bytes()).unwrap();
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        assert_eq!(
            value::DateTime::decode_element(&mut d),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn datetime_offset_element_bytes_and_bounds() {
        let samples = [
            value::DateTimeOffset {
                clock_ticks: 0,
                offset_minutes: 0,
            },
            value::DateTimeOffset {
                clock_ticks: 1_000_000_000_000,
                offset_minutes: -840,
            },
            value::DateTimeOffset {
                clock_ticks: 600_000_000_000,
                offset_minutes: 840,
            },
        ];
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        for v in &samples {
            v.encode_element(&mut e).unwrap();
        }
        // A rejected element writes no bytes, so the body holds exactly the samples.
        let out_of_range = value::DateTimeOffset {
            clock_ticks: 0,
            offset_minutes: 840,
        };
        assert_eq!(
            out_of_range.encode_element(&mut e),
            Err(Error::InvalidArgument)
        );
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish_ignoring_latch().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        assert_eq!(&d.read_body_to_end().unwrap()[..10], &[0; 10]);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        for v in &samples {
            assert_eq!(value::DateTimeOffset::decode_element(&mut d), Ok(*v));
        }
        assert_eq!(d.read_body_to_end(), Ok(Vec::new()));
        d.end_element();
        d.end_record_view();
        d.finish().unwrap();
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        // clock_ticks -1 followed by offset 0: the clock is outside [0, MAX].
        e.write_element_bytes(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0])
            .unwrap();
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        assert_eq!(
            value::DateTimeOffset::decode_element(&mut d),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn datetime_elements_take_no_bulk_path() {
        crate::hooks::reset();
        let dates = [value::DateTime {
            ticks: 1,
            kind: value::Kind::Utc,
        }];
        let offsets = [value::DateTimeOffset {
            clock_ticks: 0,
            offset_minutes: 0,
        }];
        let mut out = Vec::new();
        let mut consumed = 0;
        assert_eq!(
            value::DateTime::encode_elements_bulk(&dates, &mut out),
            Ok(false)
        );
        assert_eq!(
            value::DateTimeOffset::encode_elements_bulk(&offsets, &mut out),
            Ok(false)
        );
        assert_eq!(
            value::DateTime::decode_elements_bulk(1, &[0; 8], &mut consumed),
            Ok(None)
        );
        assert_eq!(
            value::DateTimeOffset::decode_elements_bulk(1, &[0; 10], &mut consumed),
            Ok(None)
        );
        assert!(out.is_empty());
        assert_eq!(crate::hooks::snapshot(), (0, 0));
    }
    #[test]
    fn option_element_kind_is_variable() {
        assert_eq!(Option::<i32>::element_kind(), ElementKind::Variable);
    }
    #[test]
    fn option_element_presence_bytes() {
        fn record_body(v: Option<i32>) -> Vec<u8> {
            let mut e = Encoder::new(&opts());
            e.begin_record().unwrap();
            e.begin_element();
            v.encode_element(&mut e).unwrap();
            e.end_element();
            let id = e.end_record().unwrap();
            e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &opts()).unwrap();
            d.read_root_scalar().unwrap();
            d.begin_record_view(id).unwrap();
            d.read_body_to_end().unwrap()
        }
        assert_eq!(record_body(None), [0]);
        assert_eq!(record_body(Some(0)), [1, 0, 0, 0, 0]);
        assert_eq!(record_body(Some(7)), [1, 7, 0, 0, 0]);
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        e.write_element_bytes(&[2]).unwrap();
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        assert_eq!(
            Option::<i32>::decode_element(&mut d),
            Err(Error::InvalidTag)
        );
    }
    #[test]
    fn option_element_keeps_none_positions() {
        let values = [Some(1_i32), None, Some(-1)];
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        for v in &values {
            v.encode_element(&mut e).unwrap();
        }
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        assert_eq!(
            d.read_body_to_end().unwrap(),
            [1, 1, 0, 0, 0, 0, 1, 255, 255, 255, 255]
        );
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        for v in values {
            assert_eq!(Option::<i32>::decode_element(&mut d), Ok(v));
        }
        d.end_element();
        d.end_record_view();
        d.finish().unwrap();
    }
    #[test]
    fn option_element_takes_no_bulk_path() {
        crate::hooks::reset();
        let mut out = Vec::new();
        let mut consumed = 0;
        assert_eq!(
            Option::<i32>::encode_elements_bulk(&[Some(1), None], &mut out),
            Ok(false)
        );
        assert_eq!(
            Option::<i32>::decode_elements_bulk(2, &[1, 1, 0, 0, 0, 0], &mut consumed),
            Ok(None)
        );
        assert!(out.is_empty());
        assert_eq!(crate::hooks::snapshot().0, 0);
    }
    #[test]
    fn string_element_kind_is_ref() {
        assert_eq!(String::element_kind(), ElementKind::Ref);
    }
    #[test]
    fn string_element_roundtrip_and_record_bodies() {
        let samples = ["", "abc", "中文"];
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        for s in samples {
            String::from(s).encode_element(&mut e).unwrap();
        }
        e.end_element();
        let outer = e.end_record().unwrap();
        e.write_root_scalar(u64::from(outer).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(outer).unwrap();
        let ids = d.read_body_to_end().unwrap();
        // Three elements, each exactly one 4-byte record id.
        assert_eq!(ids.len(), 4 * samples.len());
        for (id, s) in ids.chunks(4).zip(samples) {
            d.begin_record_view(u32::from_le_bytes(id.try_into().unwrap()))
                .unwrap();
            assert_eq!(d.read_body_to_end().unwrap(), s.as_bytes());
            d.end_record_view();
        }
        d.end_record_view();
        d.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(outer).unwrap();
        d.begin_element();
        for s in samples {
            assert_eq!(String::decode_element(&mut d), Ok(String::from(s)));
        }
        d.end_element();
        d.end_record_view();
        d.finish().unwrap();
    }
    #[test]
    fn string_element_invalid_utf8_is_rejected() {
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.write_body_bytes(&[0xff]).unwrap();
        let invalid = e.end_record().unwrap();
        e.begin_record().unwrap();
        e.begin_element();
        e.write_element_bytes(&invalid.to_le_bytes()).unwrap();
        e.end_element();
        let outer = e.end_record().unwrap();
        e.write_root_scalar(u64::from(outer).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(outer).unwrap();
        d.begin_element();
        assert_eq!(String::decode_element(&mut d), Err(Error::InvalidUtf8));
        // A failed decode closes its nested view, so no view remains once the outer one closes.
        d.end_record_view();
        assert_eq!(d.read_body_to_end(), Err(Error::InvalidArgument));
    }
    #[test]
    fn option_string_element_presence_bytes() {
        let values = [None, Some(String::from("a"))];
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        for v in &values {
            v.encode_element(&mut e).unwrap();
        }
        e.end_element();
        let outer = e.end_record().unwrap();
        e.write_root_scalar(u64::from(outer).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(outer).unwrap();
        // None is the single byte 0; Some is presence 1 and the id 1 of the "a" record.
        assert_eq!(d.read_body_to_end().unwrap(), [0, 1, 1, 0, 0, 0]);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        d.read_root_scalar().unwrap();
        d.begin_record_view(outer).unwrap();
        d.begin_element();
        for v in values {
            assert_eq!(Option::<String>::decode_element(&mut d), Ok(v));
        }
        d.end_element();
        d.end_record_view();
        d.finish().unwrap();
    }
    #[test]
    fn string_element_takes_no_bulk_path() {
        crate::hooks::reset();
        let mut out = Vec::new();
        let mut consumed = 0;
        assert_eq!(
            String::encode_elements_bulk(&[String::from("a")], &mut out),
            Ok(false)
        );
        assert_eq!(
            String::decode_elements_bulk(1, &[1, 0, 0, 0], &mut consumed),
            Ok(None)
        );
        assert!(out.is_empty());
        assert_eq!(crate::hooks::snapshot().0, 0);
    }
    #[test]
    fn read_body_to_end_returns_remaining_record_bytes() {
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.begin_element();
        0x0102_0304_0506_0708_u64.encode_element(&mut e).unwrap();
        e.write_element_bytes(&[0xee]).unwrap();
        e.end_element();
        let id = e.end_record().unwrap();
        e.write_root_scalar(u64::from(id).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(d.read_body_to_end(), Err(Error::InvalidArgument));
        d.read_root_scalar().unwrap();
        d.begin_record_view(id).unwrap();
        d.begin_element();
        d.read_element_bytes(8).unwrap();
        assert_eq!(d.read_body_to_end(), Ok(alloc::vec![0xee]));
        assert_eq!(d.read_body_to_end(), Ok(Vec::new()));
        d.end_element();
        d.end_record_view();
        d.finish().unwrap();
    }
    #[test]
    fn option_handwritten_default_forces_slot() {
        let absent = field(&None::<AlwaysDefault>);
        let present = field(&Some(AlwaysDefault));
        let mut e = Encoder::new(&opts());
        // 字段须位于对象帧内（设计 §1 第 2 项）。
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        if !value::is_default_i32(0) {
            AlwaysDefault.ace_encode(&mut e).unwrap();
        }
        // 离开对象帧写入根槽（设计 §1 第 5 项，FR-W10）。
        e.leave_object().unwrap();
        assert_eq!(absent, e.finish().unwrap());
        assert_eq!(present.len(), absent.len() + 16);
        let mut d = Decoder::new(&present, &opts()).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        assert!(d.has_field(1).unwrap());
        let mut d = Decoder::new(&absent, &opts()).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        assert!(!d.has_field(1).unwrap());
    }
    #[test]
    fn forced_scalar_zero_writes_slot() {
        let mut e = Encoder::new(&opts());
        // 字段须位于对象帧内（设计 §1 第 2 项）。
        e.enter_object().unwrap();
        e.begin_field(0).unwrap();
        0i32.ace_encode(&mut e).unwrap();
        // 离开对象帧写入根槽（设计 §1 第 5 项，FR-W10）。
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        assert!(d.has_field(0).unwrap());
        assert_eq!(d.read_field_scalar(0).unwrap(), [0; 8]);
        // 与 enter_object 成对离开，finish 要求无未关闭对象帧（设计 §2 第 7、8 项）。
        d.leave_object().unwrap();
        d.finish().unwrap();
    }
    #[test]
    fn single_option_implements_codecs() {
        fn assert_codecs<T: AceEncode + AceDecode>() {}
        assert_codecs::<Option<i32>>();
    }
    macro_rules! option_case {
        ($name:ident, $ty:ty, $zero:expr, $nonzero:expr) => {
            #[test]
            fn $name() {
                let none: Option<$ty> = None;
                let zero = Some($zero);
                let nonzero = Some($nonzero);
                let absent = field(&none);
                let present = field(&zero);
                assert_ne!(absent, present);
                assert_eq!(absent.len() + 16, present.len());
                assert_eq!(&present[present.len() - 8..], &[0; 8]);
                for v in [none, zero, nonzero] {
                    assert_eq!(round_field::<Option<$ty>>(&field(&v)), v);
                    let bytes = root(&v);
                    let mut d = Decoder::new(&bytes, &opts()).unwrap();
                    assert_eq!(Option::<$ty>::ace_decode(&mut d).unwrap(), v);
                    d.finish().unwrap();
                }
                assert_eq!(root(&none), [0; 8]);
                let b = root(&zero);
                assert_eq!(b.len(), 24);
                assert_eq!(&b[..8], &[0, 0, 0, 0, 16, 0, 0, 0]);
                assert_eq!(&b[16..], &[0; 8]);
            }
        };
    }
    option_case!(ac_t02_i16, i16, 0, -17);
    option_case!(ac_t02_i32, i32, 0, -17);
    option_case!(ac_t02_i64, i64, 0, -17);
    option_case!(ac_t02_f32, f32, 0.0, 1.5);
    option_case!(ac_t02_f64, f64, 0.0, 1.5);
    option_case!(ac_t02_bool, bool, false, true);
    #[test]
    fn ac_t08_option_wide_fields() {
        fn check<T: AceEncode + AceDecode + NonOptional + PartialEq + core::fmt::Debug>(
            v: T,
            expected: &[u8],
            packed: bool,
        ) {
            let none: Option<T> = None;
            assert_eq!(
                round_packed_field::<Option<T>>(&packed_field(&none, packed), packed),
                None
            );
            let some = Some(v);
            let bytes = packed_field(&some, packed);
            let mut b = if packed {
                crate::wire::packed::unpack(&bytes, &opts()).unwrap()
            } else {
                bytes.clone()
            };
            // 信封 = [i32 主段长] 主段 [i32 辅段长] 辅段（wire/envelope.rs）。主段 = 值记录（8+n，id 1）+ 根对象记录
            // （8+8，id 2），共 n+24；辅段 = 根槽 assist[0]（depth 0、tag 0，载荷为根对象记录 id 2）+ 字段槽（depth 1、tag 1，
            // 载荷为值记录 id 1），共 32 字节。信封共 4+(n+24)+4+32 = n+64；packed 解包可补零到 8 字节对齐，故按 n+64 截断。
            b.truncate(expected.len() + 64);
            assert_eq!(&b[0..4], &((expected.len() + 24) as i32).to_le_bytes());
            assert_eq!(&b[4..8], &1i32.to_le_bytes());
            assert_eq!(&b[8..12], &(expected.len() as i32).to_le_bytes());
            assert_eq!(&b[12..12 + expected.len()], expected);
            assert_eq!(&b[b.len() - 8..], &1u64.to_le_bytes());
            assert_eq!(round_packed_field::<Option<T>>(&bytes, packed), some);
            for v in [none, some] {
                let root = packed_root(&v, packed);
                let mut decoder = Decoder::new(&root, &packed_opts(packed)).unwrap();
                assert_eq!(Option::<T>::ace_decode(&mut decoder).unwrap(), v);
                decoder.finish().unwrap();
            }
        }
        for packed in [false, true] {
            check(value::Guid([7; 16]), &[7; 16], packed);
            check(
                value::DateTimeOffset {
                    clock_ticks: 42,
                    offset_minutes: 0,
                },
                &[42, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                packed,
            );
        }
    }
    fn packed_opts(packed: bool) -> Options {
        Options {
            packed,
            ..Options::default()
        }
    }
    fn packed_field<T: AceEncode>(v: &T, packed: bool) -> Vec<u8> {
        let mut e = Encoder::new(&packed_opts(packed));
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        v.ace_encode(&mut e).unwrap();
        e.leave_object().unwrap();
        e.finish().unwrap()
    }
    fn round_packed_field<T: AceDecode>(bytes: &[u8], packed: bool) -> T {
        let mut d = Decoder::new(bytes, &packed_opts(packed)).unwrap();
        let root_scalar = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root_scalar)).unwrap();
        d.begin_field(1).unwrap();
        let v = T::ace_decode(&mut d).unwrap();
        d.leave_object().unwrap();
        d.finish().unwrap();
        v
    }
    fn packed_root<T: AceEncode>(v: &T, packed: bool) -> Vec<u8> {
        let mut e = Encoder::new(&packed_opts(packed));
        v.ace_encode(&mut e).unwrap();
        e.finish().unwrap()
    }
    fn option_inline<
        T: AceEncode + AceDecode + NonOptional + PartialEq + core::fmt::Debug + Copy,
    >(
        zero: T,
        nonzero: T,
    ) {
        for packed in [false, true] {
            let none = None::<T>;
            let some = Some(zero);
            let absent = packed_field(&none, packed);
            let present = packed_field(&some, packed);
            assert_ne!(absent, present);
            for v in [none, some, Some(nonzero)] {
                let b = packed_field(&v, packed);
                let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
                let payload = d.read_root_scalar().unwrap();
                d.enter_object(u64::from_le_bytes(payload)).unwrap();
                assert_eq!(d.begin_field(1).unwrap(), v.is_some());
                assert_eq!(Option::<T>::ace_decode(&mut d).unwrap(), v);
                d.leave_object().unwrap();
                d.finish().unwrap();
                let b = packed_root(&v, packed);
                let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
                assert_eq!(Option::<T>::ace_decode(&mut d).unwrap(), v);
                d.finish().unwrap();
            }
        }
    }
    #[test]
    fn ac_t08_option_datetime_modes() {
        option_inline(
            value::DateTime {
                ticks: 0,
                kind: value::Kind::Unspecified,
            },
            value::DateTime {
                ticks: 42,
                kind: value::Kind::Utc,
            },
        );
    }
    #[test]
    fn ac_t08_option_intptr_modes() {
        option_inline(value::IntPtr(0), value::IntPtr(-42));
    }
    #[test]
    fn ac_t08_option_uintptr_modes() {
        option_inline(value::UIntPtr(0), value::UIntPtr(42));
    }
    fn wide_root<T: AceEncode + AceDecode + PartialEq + core::fmt::Debug>(v: T, size: usize) {
        for packed in [false, true] {
            let b = packed_root(&v, packed);
            let raw = if packed {
                crate::wire::packed::unpack(&b, &packed_opts(packed)).unwrap()
            } else {
                b.clone()
            };
            assert_eq!(
                i32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize,
                size
            );
            assert_eq!(i32::from_le_bytes(raw[4..8].try_into().unwrap()), 1);
            assert_eq!(&raw[24 + size..32 + size], &1u64.to_le_bytes());
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(T::ace_decode(&mut d).unwrap(), v);
            d.finish().unwrap();
            assert_eq!(
                round_packed_field::<T>(&packed_field(&v, packed), packed),
                v
            );
        }
    }
    #[test]
    fn ac_t08_guid_root_modes() {
        wide_root(value::Guid([7; 16]), 16);
    }
    #[test]
    fn ac_t08_dto_root_modes() {
        wide_root(
            value::DateTimeOffset {
                clock_ticks: 42,
                offset_minutes: 0,
            },
            10,
        );
    }
    #[test]
    fn field_order_selection() {
        let mut e = Encoder::new(&opts());
        // 字段须位于对象帧内（设计 §1 第 2 项）。
        e.enter_object().unwrap();
        e.begin_field(2).unwrap();
        assert_eq!(e.current_order(), Some(2));
        assert_eq!(e.begin_field(2), Err(Error::InvalidArgument));
    }
    #[test]
    fn decode_string_into_reuses_capacity() {
        let b = root(&"hello");
        let mut d = Decoder::new(&b, &opts()).unwrap();
        let mut out = String::with_capacity(128);
        let ptr = out.as_ptr();
        String::ace_decode_into(&mut d, &mut out).unwrap();
        assert_eq!(out, "hello");
        assert_eq!(out.as_ptr(), ptr);
        d.finish().unwrap();
    }
    #[test]
    fn required_values_and_inline_types() {
        // 字段须位于对象帧内：编码空对象根（设计 §1 第 2、5 项，FR-W10）。
        let mut e = Encoder::new(&opts());
        e.enter_object().unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        assert_eq!(
            {
                d.begin_field(1).unwrap();
                String::ace_decode(&mut d)
            },
            Err(Error::MissingRequiredField)
        );
        let values = [
            root(&value::DateTime {
                ticks: 0,
                kind: value::Kind::Utc,
            }),
            root(&value::IntPtr(0)),
            root(&value::UIntPtr(0)),
            root(&value::ToolkitsU64(0)),
        ];
        for b in values {
            assert_eq!(&b[..8], &[0, 0, 0, 0, 16, 0, 0, 0]);
        }
        let b = root(&"");
        assert_eq!(&b[..4], &8i32.to_le_bytes());
        let mut e = Encoder::new(&opts());
        e.write_root_record(&[0; 9]).unwrap();
        let b = e.finish().unwrap();
        let mut d = Decoder::new(&b, &opts()).unwrap();
        assert_eq!(value::Guid::ace_decode(&mut d), Err(Error::InvalidTag));
    }

    /// Reads the body of the record that field 1 of `bytes` points to.
    fn field_record_body(bytes: &[u8]) -> Vec<u8> {
        let mut d = Decoder::new(bytes, &opts()).unwrap();
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        let payload = d.read_field_scalar(1).unwrap();
        let id = u32::try_from(u64::from_le_bytes(payload)).unwrap();
        d.begin_record_view(id).unwrap();
        let body = d.read_body_to_end().unwrap();
        d.end_record_view();
        body
    }

    #[test]
    fn vec_i32_encode_is_one_bulk_and_matches_wire_body() {
        // AC-T01 encode witness: little-endian takes one bulk call and no element calls. Big-endian
        // declines bulk and takes one element call per item. The wire body is identical either way.
        crate::hooks::reset();
        let bytes = field(&alloc::vec![1, 0, -2]);
        assert_eq!(
            crate::hooks::snapshot(),
            if cfg!(target_endian = "little") {
                (1, 0)
            } else {
                (0, 3)
            }
        );
        assert_eq!(
            field_record_body(&bytes),
            [3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 254, 255, 255, 255]
        );
        crate::hooks::reset();
        let long: Vec<i32> = (0..200).collect();
        field(&long);
        assert_eq!(
            crate::hooks::snapshot(),
            if cfg!(target_endian = "little") {
                (1, 0)
            } else {
                (0, 200)
            }
        );
    }

    /// B1(b)／B2：借用路径（`encode_into` 的 scratch 承载）与拥有值路径共用同一条整段拷贝——
    /// 内建定宽元素只记一次 bulk、不逐项调元素入口；无整段体的元素逐项写入时只记元素、
    /// **不记** bulk。两条一起才排除「逐元素伪 bulk」（只补一次 `note_bulk` 的假实现）。
    #[test]
    fn borrowed_bulk_body_is_one_bulk_and_no_element_calls() {
        struct PerElement(i32);
        impl NonOptional for PerElement {}
        impl AceEncode for PerElement {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                self.ace_encode_element(e)
            }
            fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
                crate::hooks::note_element();
                e.write_element_bytes(&self.0.to_le_bytes())
            }
        }

        let value: Vec<i32> = (0..8).collect();
        for packed in [false, true] {
            let options = Options { packed, ..opts() };
            let expected = crate::encode(&value, &options).unwrap();
            let mut dst = alloc::vec![0u8; expected.len()];
            let mut scratch = alloc::vec![0u8; 64 * 1024];
            crate::hooks::reset();
            assert_eq!(
                crate::encode_into(&value, &mut dst, &mut scratch, &options),
                Ok(expected.len())
            );
            assert_eq!(dst, expected);
            // 小端：整段直写一次 bulk、不逐项调元素；大端按 FR-W09 没有整段体，逐项写入
            // 8 个元素、不记 bulk（与 `vec_i32_encode_is_one_bulk_and_matches_wire_body` 同口径）。
            assert_eq!(
                crate::hooks::snapshot(),
                if cfg!(target_endian = "little") {
                    (1, 0)
                } else {
                    (0, value.len())
                },
                "packed = {packed}"
            );

            let per: Vec<PerElement> = (0..3).map(PerElement).collect();
            let expected = crate::encode(&per, &options).unwrap();
            let mut dst = alloc::vec![0u8; expected.len()];
            let mut scratch = alloc::vec![0u8; 64 * 1024];
            crate::hooks::reset();
            assert_eq!(
                crate::encode_into(&per, &mut dst, &mut scratch, &options),
                Ok(expected.len())
            );
            assert_eq!(dst, expected);
            assert_eq!(crate::hooks::snapshot(), (0, 3), "packed = {packed}");
        }
    }

    /// 空元素区没有整段拷贝：直写只写 `u32 count`，不得记 bulk（与 scratch 承载的空体直返
    /// 一致）。两条路径同一个判据：拥有值 `encode` 与借用路径 `encode_into`。
    #[test]
    fn empty_direct_bulk_body_records_no_bulk() {
        let value: Vec<i32> = Vec::new();
        // 空集合的 wire 不变：记录体只有 `u32 count = 0`。
        assert_eq!(field_record_body(&field(&value)), [0u8; 4]);
        for packed in [false, true] {
            let options = Options { packed, ..opts() };
            let expected = crate::encode(&value, &options).unwrap();
            crate::hooks::reset();
            assert_eq!(crate::encode(&value, &options), Ok(expected.clone()));
            assert_eq!(crate::hooks::snapshot(), (0, 0), "packed = {packed}");

            let mut dst = alloc::vec![0u8; expected.len()];
            let mut scratch = alloc::vec![0u8; 64 * 1024];
            crate::hooks::reset();
            assert_eq!(
                crate::encode_into(&value, &mut dst, &mut scratch, &options),
                Ok(expected.len())
            );
            assert_eq!(dst, expected);
            assert_eq!(crate::hooks::snapshot(), (0, 0), "packed = {packed}");
        }
    }

    #[test]
    fn vec_bulk_precheck_rejects_before_bulk_copy() {
        let o = Options {
            max_raw_bytes: 64,
            ..Options::default()
        };
        let v: Vec<i32> = (0..100).collect();
        crate::hooks::reset();
        let mut e = Encoder::new(&o);
        assert_eq!(v.ace_encode(&mut e), Err(Error::ResourceLimit));
        assert_eq!(crate::hooks::snapshot(), (0, 0));
    }

    #[test]
    fn vec_nested_bulk_precheck_counts_open_parent_bytes() {
        // 外层已缓冲 8B 头与 4B count；内层 56B 体须与之合计后再预检，否则 8+56 误过。
        let o = Options {
            max_raw_bytes: 64,
            ..Options::default()
        };
        let v: Vec<Vec<i32>> = alloc::vec![alloc::vec![0; 13]];
        crate::hooks::reset();
        let mut e = Encoder::new(&o);
        assert_eq!(v.ace_encode(&mut e), Err(Error::ResourceLimit));
        assert_eq!(crate::hooks::snapshot().0, 0);
    }

    #[test]
    fn vec_nested_bulk_precheck_counts_open_parent_records() {
        // 外层记录已打开：内层记录须与之合计，max_records=1 下共 2 条，应在复制前拒绝。
        let o = Options {
            max_records: 1,
            ..Options::default()
        };
        let v: Vec<Vec<i32>> = alloc::vec![alloc::vec![1]];
        crate::hooks::reset();
        let mut e = Encoder::new(&o);
        assert_eq!(v.ace_encode(&mut e), Err(Error::ResourceLimit));
        assert_eq!(crate::hooks::snapshot().0, 0);
    }

    #[test]
    fn open_frame_pending_record_counts_before_bulk() {
        let o = Options {
            max_records: 1,
            ..Options::default()
        };
        crate::hooks::reset();
        let mut e = Encoder::new(&o);
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        assert_eq!(
            alloc::vec![1i32].ace_encode(&mut e),
            Err(Error::ResourceLimit)
        );
        assert_eq!(crate::hooks::snapshot().0, 0);
    }

    #[test]
    fn vec_i32_decode_is_one_bulk_and_roundtrips() {
        // AC-T01 decode witness: little-endian takes one bulk call and no element calls. Big-endian
        // declines bulk and takes one element call per item. Both decode to the same values.
        let bytes = field(&alloc::vec![1, 0, -2]);
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<i32>>(&bytes), alloc::vec![1, 0, -2]);
        assert_eq!(
            crate::hooks::snapshot(),
            if cfg!(target_endian = "little") {
                (1, 0)
            } else {
                (0, 3)
            }
        );
        let long: Vec<i32> = (0..200).collect();
        let long_bytes = field(&long);
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<i32>>(&long_bytes), long);
        assert_eq!(
            crate::hooks::snapshot(),
            if cfg!(target_endian = "little") {
                (1, 0)
            } else {
                (0, 200)
            }
        );
    }

    #[test]
    fn vec_i32_root_bulk_counts_and_roundtrip() {
        let v: Vec<i32> = alloc::vec![1, 0, -2];
        let expected = if cfg!(target_endian = "little") {
            (1, 0)
        } else {
            (0, 3)
        };
        crate::hooks::reset();
        let b = root(&v);
        assert_eq!(crate::hooks::snapshot(), expected);
        crate::hooks::reset();
        let mut d = Decoder::new(&b, &opts()).unwrap();
        let got = <Vec<i32>>::ace_decode(&mut d).unwrap();
        d.finish().unwrap();
        assert_eq!(crate::hooks::snapshot(), expected);
        assert_eq!(got, v);
    }

    #[test]
    fn vec_roundtrips_in_raw_and_packed_field_and_root() {
        let ints: Vec<i32> = alloc::vec![1, 0, -2];
        let strings = alloc::vec![String::from("a"), String::from("bc")];
        for packed in [false, true] {
            let b = packed_field(&ints, packed);
            assert_eq!(round_packed_field::<Vec<i32>>(&b, packed), ints);
            let b = packed_root(&ints, packed);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(<Vec<i32>>::ace_decode(&mut d).unwrap(), ints);
            d.finish().unwrap();
            let b = packed_field(&strings, packed);
            assert_eq!(round_packed_field::<Vec<String>>(&b, packed), strings);
            let b = packed_root(&strings, packed);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(<Vec<String>>::ace_decode(&mut d).unwrap(), strings);
            d.finish().unwrap();
        }
    }

    #[test]
    fn vec_option_string_none_positions_survive_both_modes() {
        let samples: [Vec<Option<String>>; 5] = [
            alloc::vec![None, Some(String::from("a")), Some(String::from("bc"))],
            alloc::vec![Some(String::from("a")), None, Some(String::from("bc"))],
            alloc::vec![Some(String::from("a")), Some(String::from("bc")), None],
            alloc::vec![None, None],
            alloc::vec![],
        ];
        for v in samples {
            for packed in [false, true] {
                let b = packed_field(&v, packed);
                assert_eq!(round_packed_field::<Vec<Option<String>>>(&b, packed), v);
                let b = packed_root(&v, packed);
                let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
                assert_eq!(<Vec<Option<String>>>::ace_decode(&mut d).unwrap(), v);
                d.finish().unwrap();
            }
        }
    }

    #[test]
    fn vec_empty_body_is_zero_count_and_differs_from_none() {
        let empty: Vec<i32> = Vec::new();
        assert_eq!(field_record_body(&field(&empty)), [0, 0, 0, 0]);
        assert_ne!(field(&empty), field(&None::<Vec<i32>>));
        assert_eq!(round_field::<Vec<i32>>(&field(&empty)), empty);
    }

    #[test]
    fn vec_length_boundaries_roundtrip() {
        for len in [65_535usize, 65_536] {
            let v: Vec<i32> = (0..len as i32).collect();
            for packed in [false, true] {
                let b = packed_field(&v, packed);
                assert_eq!(round_packed_field::<Vec<i32>>(&b, packed), v);
            }
        }
    }

    #[test]
    fn vec_collection_limit_is_enforced_both_ways() {
        let limited = Options {
            max_collection_len: 2,
            ..opts()
        };
        let mut e = Encoder::new(&limited);
        // 字段须位于对象帧内（设计 §1 第 2 项）。
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        assert_eq!(
            alloc::vec![1, 2, 3].ace_encode(&mut e),
            Err(Error::ResourceLimit)
        );
        assert!(!e.in_record());
        let oversized = [3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0];
        let mut e = Encoder::new(&opts());
        // 字段须位于对象帧内（设计 §1 第 2 项）。
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        e.encode_record(&oversized).unwrap();
        // 离开对象帧写入根槽（设计 §1 第 5 项，FR-W10）。
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &limited).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(<Vec<i32>>::ace_decode(&mut d), Err(Error::ResourceLimit));
    }

    #[test]
    fn vec_string_element_counts_and_bytes() {
        // AC-T01 element witness: strings take the per-element path, one element call each.
        crate::hooks::reset();
        let v = alloc::vec![String::from("ab"), String::from("\u{4e2d}")];
        let bytes = field(&v);
        assert_eq!(crate::hooks::snapshot().0, 0);
        assert_eq!(crate::hooks::snapshot().1, 2);
        let body = field_record_body(&bytes);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(<Vec<String>>::ace_decode(&mut d).unwrap(), v);
        // 与 enter_object 成对离开，finish 要求无未关闭对象帧（设计 §2 第 7、8 项）。
        d.leave_object().unwrap();
        d.finish().unwrap();
    }

    #[test]
    fn vec_trailing_byte_after_count_is_invalid_tag() {
        let mut e = Encoder::new(&opts());
        // 字段须位于对象帧内（设计 §1 第 2 项）。
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        e.encode_record(&[1, 0, 0, 0, 7, 0, 0, 0, 9]).unwrap();
        // 离开对象帧写入根槽（设计 §1 第 5 项，FR-W10）。
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(<Vec<i32>>::ace_decode(&mut d), Err(Error::InvalidTag));
    }

    /// ①的越界兜底：手写 bulk hook 自行把元素写进目标后，库仍要只推进游标；推进必须走与
    /// [`Decoder::read_body_bytes`] 同一个 `body_range` 边界检查，检查顺序也不得前移或合并。
    #[derive(Debug, PartialEq)]
    struct BulkHook<const CONSUMED: usize>;

    impl<const CONSUMED: usize> AceElement for BulkHook<CONSUMED> {
        fn element_kind() -> ElementKind {
            ElementKind::Value { size: 1 }
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.write_element_bytes(&[0])
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            d.read_element_bytes(1)?;
            Ok(Self)
        }
        fn decode_elements_bulk_into(
            _out: &mut Vec<Self>,
            _count: usize,
            _bytes: &[u8],
            consumed: &mut usize,
        ) -> Result<bool> {
            *consumed = CONSUMED;
            Ok(true)
        }
    }

    #[test]
    fn vec_bulk_hook_consumed_is_bounds_checked() {
        fn decode_body<T: AceElement>(body: &[u8]) -> Result<Vec<T>> {
            let mut e = Encoder::new(&opts());
            // 字段须位于对象帧内（设计 §1 第 2 项）。
            e.enter_object().unwrap();
            e.begin_field(1).unwrap();
            e.encode_record(body).unwrap();
            e.leave_object().unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &opts()).unwrap();
            let payload = d.read_root_scalar().unwrap();
            d.enter_object(u64::from_le_bytes(payload)).unwrap();
            d.begin_field(1).unwrap();
            <Vec<T>>::ace_decode(&mut d)
        }
        // count 1 + 5 字节元素体。
        let body = [1u8, 0, 0, 0, 7, 0, 0, 0, 9];
        // 谎报超长 `consumed`：仍是截断，不得越过记录视图。
        assert_eq!(
            decode_body::<BulkHook<4096>>(&body),
            Err(Error::TruncatedInput)
        );
        // 合法消费后仍有尾字节：仍是 `InvalidTag`（不是 `TruncatedInput`）。
        assert_eq!(decode_body::<BulkHook<1>>(&body), Err(Error::InvalidTag));
        // 恰好消费整个元素区：成功，尾部无字节。
        assert_eq!(decode_body::<BulkHook<5>>(&body), Ok(Vec::new()));
    }

    #[test]
    fn vec_decoded_budget_counts_body_once() {
        let v = alloc::vec![1i32, 0, -2];
        let bytes = field(&v);
        let body_len = field_record_body(&bytes).len();
        // enter_object 计入 8 字节对象体（decode/mod.rs enter_object；V4-5），两个上限各加 8。
        let exact = Options {
            max_decoded_bytes: 8 + body_len,
            ..opts()
        };
        let mut d = Decoder::new(&bytes, &exact).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(<Vec<i32>>::ace_decode(&mut d), Ok(v.clone()));
        let short = Options {
            max_decoded_bytes: 8 + body_len - 1,
            ..opts()
        };
        let mut d = Decoder::new(&bytes, &short).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(<Vec<i32>>::ace_decode(&mut d), Err(Error::ResourceLimit));
    }

    #[test]
    fn vec_string_wire_is_count_then_record_ids() {
        let v = alloc::vec![String::from("ab"), String::from("c")];
        let bytes = field(&v);
        let body = field_record_body(&bytes);
        assert_eq!(body.len(), 4 + 4 * 2);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段（设计 §2 第 2 项）。
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        let outer = u32::try_from(u64::from_le_bytes(d.read_field_scalar(1).unwrap())).unwrap();
        d.begin_record_view(outer).unwrap();
        let outer_body = d.read_body_to_end().unwrap();
        d.end_record_view();
        assert_eq!(outer_body, body);
        for (i, expected) in ["ab", "c"].iter().enumerate() {
            let id_bytes: [u8; 4] = body[4 + 4 * i..8 + 4 * i].try_into().unwrap();
            let id = u32::from_le_bytes(id_bytes);
            d.begin_record_view(id).unwrap();
            assert_eq!(d.read_body_to_end().unwrap(), expected.as_bytes());
            d.end_record_view();
        }
    }

    #[test]
    fn vec_guid_path_is_compact_16b_and_dual_mode() {
        // Guid is a padding-free 16-byte newtype: little-endian takes one bulk call per direction,
        // and big-endian declines bulk and takes one element call per item.
        let v = alloc::vec![
            value::Guid(core::array::from_fn(|i| i as u8)),
            value::Guid(core::array::from_fn(|i| 0x10 + i as u8)),
        ];
        let expected = if cfg!(target_endian = "little") {
            (1, 0)
        } else {
            (0, 2)
        };
        crate::hooks::reset();
        let bytes = field(&v);
        assert_eq!(crate::hooks::snapshot(), expected);
        let body = field_record_body(&bytes);
        assert_eq!(body.len(), 4 + 16 * 2);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        let elements = [value::encode_guid(v[0]), value::encode_guid(v[1])].concat();
        assert_eq!(&body[4..], elements.as_slice());
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<value::Guid>>(&bytes), v);
        assert_eq!(crate::hooks::snapshot(), expected);
        for packed in [false, true] {
            let bytes = packed_field(&v, packed);
            let back = round_packed_field::<Vec<value::Guid>>(&bytes, packed);
            assert_eq!(back, v);
            let bytes = packed_root(&v, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let back = <Vec<value::Guid>>::ace_decode(&mut d).unwrap();
            d.finish().unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn vec_datetimeoffset_path_is_10b_and_never_bulk() {
        // DateTimeOffset keeps the default bulk methods on every host (AC-T08), so both directions
        // take one element call per item.
        let v = alloc::vec![
            value::DateTimeOffset {
                clock_ticks: 0,
                offset_minutes: 0,
            },
            value::DateTimeOffset {
                clock_ticks: 1_000_000_000_000,
                offset_minutes: -840,
            },
        ];
        let first = value::encode_datetime_offset(0, 0).unwrap();
        let second = value::encode_datetime_offset(1_000_000_000_000, -840).unwrap();
        let elements = [first, second].concat();
        crate::hooks::reset();
        let bytes = field(&v);
        assert_eq!(crate::hooks::snapshot(), (0, 2));
        let body = field_record_body(&bytes);
        assert_eq!(body.len(), 4 + 10 * 2);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        assert_eq!(&body[4..], elements.as_slice());
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<value::DateTimeOffset>>(&bytes), v);
        assert_eq!(crate::hooks::snapshot(), (0, 2));
        for packed in [false, true] {
            let bytes = packed_field(&v, packed);
            let back = round_packed_field::<Vec<value::DateTimeOffset>>(&bytes, packed);
            assert_eq!(back, v);
            let bytes = packed_root(&v, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let back = <Vec<value::DateTimeOffset>>::ace_decode(&mut d).unwrap();
            d.finish().unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn vec_datetime_path_is_8b_element_and_never_bulk() {
        // DateTime has no bulk override, so both directions take one element call per item on
        // every host.
        let v = alloc::vec![
            value::DateTime {
                ticks: 0,
                kind: value::Kind::Unspecified,
            },
            value::DateTime {
                ticks: 1,
                kind: value::Kind::Utc,
            },
        ];
        let first = value::encode_datetime(0, value::Kind::Unspecified).unwrap();
        let second = value::encode_datetime(1, value::Kind::Utc).unwrap();
        let elements = [first.to_le_bytes(), second.to_le_bytes()].concat();
        crate::hooks::reset();
        let bytes = field(&v);
        assert_eq!(crate::hooks::snapshot(), (0, 2));
        let body = field_record_body(&bytes);
        assert_eq!(body.len(), 4 + 8 * 2);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        assert_eq!(&body[4..], elements.as_slice());
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<value::DateTime>>(&bytes), v);
        assert_eq!(crate::hooks::snapshot(), (0, 2));
        for packed in [false, true] {
            let bytes = packed_field(&v, packed);
            let back = round_packed_field::<Vec<value::DateTime>>(&bytes, packed);
            assert_eq!(back, v);
            let bytes = packed_root(&v, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let back = <Vec<value::DateTime>>::ace_decode(&mut d).unwrap();
            d.finish().unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn vec_intptr_uintptr_toolkits_paths_are_8b_and_dual_mode() {
        // All three are padding-free 8-byte newtypes: little-endian takes one bulk call per
        // direction, and big-endian declines bulk and takes one element call per item.
        let expected = if cfg!(target_endian = "little") {
            (1, 0)
        } else {
            (0, 2)
        };
        let ints = alloc::vec![value::IntPtr(-1), value::IntPtr(0x0102_0304_0506_0708)];
        let first = value::encode_intptr(-1);
        let second = value::encode_intptr(0x0102_0304_0506_0708);
        let elements = [first, second].concat();
        crate::hooks::reset();
        let bytes = field(&ints);
        assert_eq!(crate::hooks::snapshot(), expected);
        let body = field_record_body(&bytes);
        assert_eq!(body.len(), 4 + 8 * 2);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        assert_eq!(&body[4..], elements.as_slice());
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<value::IntPtr>>(&bytes), ints);
        assert_eq!(crate::hooks::snapshot(), expected);
        for packed in [false, true] {
            let bytes = packed_field(&ints, packed);
            let back = round_packed_field::<Vec<value::IntPtr>>(&bytes, packed);
            assert_eq!(back, ints);
            let bytes = packed_root(&ints, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let back = <Vec<value::IntPtr>>::ace_decode(&mut d).unwrap();
            d.finish().unwrap();
            assert_eq!(back, ints);
        }
        let uints = alloc::vec![value::UIntPtr(u64::MAX), value::UIntPtr(2)];
        let first = value::encode_uintptr(u64::MAX);
        let second = value::encode_uintptr(2);
        let elements = [first, second].concat();
        crate::hooks::reset();
        let bytes = field(&uints);
        assert_eq!(crate::hooks::snapshot(), expected);
        let body = field_record_body(&bytes);
        assert_eq!(body.len(), 4 + 8 * 2);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        assert_eq!(&body[4..], elements.as_slice());
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<value::UIntPtr>>(&bytes), uints);
        assert_eq!(crate::hooks::snapshot(), expected);
        for packed in [false, true] {
            let bytes = packed_field(&uints, packed);
            let back = round_packed_field::<Vec<value::UIntPtr>>(&bytes, packed);
            assert_eq!(back, uints);
            let bytes = packed_root(&uints, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let back = <Vec<value::UIntPtr>>::ace_decode(&mut d).unwrap();
            d.finish().unwrap();
            assert_eq!(back, uints);
        }
        let toolkits = alloc::vec![value::ToolkitsU64(u64::MAX), value::ToolkitsU64(2)];
        let first = value::encode_toolkits_guid(u64::MAX as i64);
        let second = value::encode_toolkits_guid(2);
        let elements = [first, second].concat();
        crate::hooks::reset();
        let bytes = field(&toolkits);
        assert_eq!(crate::hooks::snapshot(), expected);
        let body = field_record_body(&bytes);
        assert_eq!(body.len(), 4 + 8 * 2);
        assert_eq!(&body[..4], &2u32.to_le_bytes());
        assert_eq!(&body[4..], elements.as_slice());
        crate::hooks::reset();
        assert_eq!(round_field::<Vec<value::ToolkitsU64>>(&bytes), toolkits);
        assert_eq!(crate::hooks::snapshot(), expected);
        for packed in [false, true] {
            let bytes = packed_field(&toolkits, packed);
            let back = round_packed_field::<Vec<value::ToolkitsU64>>(&bytes, packed);
            assert_eq!(back, toolkits);
            let bytes = packed_root(&toolkits, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let back = <Vec<value::ToolkitsU64>>::ace_decode(&mut d).unwrap();
            d.finish().unwrap();
            assert_eq!(back, toolkits);
        }
    }

    #[test]
    fn vec_ace_decode_into_reuses_capacity() {
        let ints: Vec<i32> = alloc::vec![1, 0, -2];
        let strings = alloc::vec![String::from("a"), String::from("bc")];
        for packed in [false, true] {
            let b = packed_root(&ints, packed);
            let mut out: Vec<i32> = Vec::with_capacity(8);
            out.extend_from_slice(&[9, 9, 9, 9]);
            let ptr = out.as_ptr();
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            crate::hooks::reset();
            <Vec<i32> as AceDecode>::ace_decode_into(&mut d, &mut out).unwrap();
            d.finish().unwrap();
            assert_eq!(
                crate::hooks::snapshot(),
                if cfg!(target_endian = "little") {
                    (1, 0)
                } else {
                    (0, 3)
                }
            );
            assert_eq!(out.as_ptr(), ptr);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(out, <Vec<i32>>::ace_decode(&mut d).unwrap());
            d.finish().unwrap();

            let b = packed_root(&strings, packed);
            let mut out: Vec<String> = Vec::with_capacity(4);
            out.push(String::from("x"));
            out.push(String::from("yy"));
            let ptr = out.as_ptr();
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            <Vec<String> as AceDecode>::ace_decode_into(&mut d, &mut out).unwrap();
            d.finish().unwrap();
            assert_eq!(out.as_ptr(), ptr);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(out, <Vec<String>>::ace_decode(&mut d).unwrap());
            d.finish().unwrap();
        }
    }

    /// 映射键的测试替身：`Eq`/`Ord` 只比较 `id`，线上却还带 `label`。同 id 的旧键与新键因此在容器里
    /// 等价，键载荷却不同——只比较 Map 的 `Eq` 分不出解码留下的是哪一枚键。
    #[derive(Debug, Clone)]
    struct LabelKey {
        id: u32,
        label: String,
    }

    impl PartialEq for LabelKey {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
        }
    }

    impl Eq for LabelKey {}

    impl PartialOrd for LabelKey {
        fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for LabelKey {
        fn cmp(&self, other: &Self) -> core::cmp::Ordering {
            self.id.cmp(&other.id)
        }
    }

    impl AceElement for LabelKey {
        fn element_kind() -> ElementKind {
            ElementKind::Variable
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.write_element_bytes(&self.id.to_le_bytes())?;
            <String as AceElement>::encode_element(&self.label, e)
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
            Ok(Self {
                id,
                label: <String as AceElement>::decode_element(d)?,
            })
        }
    }

    #[test]
    fn btreemap_roundtrip_sorted_and_empty() {
        // 按 3、1、2 的顺序插入；线上顺序由键序决定，两种插入顺序的编码必须一致。
        let build = |keys: [i32; 3]| -> BTreeMap<i32, String> {
            keys.into_iter()
                .map(|k| (k, alloc::format!("v{k}")))
                .collect()
        };
        let shuffled = build([3, 1, 2]);
        let sorted = build([1, 2, 3]);
        assert_eq!(field(&shuffled), field(&sorted));
        let pairs = BTreeMap::<i32, i32>::from([(3, 30), (1, 10), (2, 20)]);
        let expected = [
            3u32.to_le_bytes(),
            1i32.to_le_bytes(),
            10i32.to_le_bytes(),
            2i32.to_le_bytes(),
            20i32.to_le_bytes(),
            3i32.to_le_bytes(),
            30i32.to_le_bytes(),
        ]
        .concat();
        assert_eq!(field_record_body(&field(&pairs)), expected);
        let empty = BTreeMap::<i32, String>::new();
        for packed in [false, true] {
            for map in [&shuffled, &empty] {
                let b = packed_field(map, packed);
                assert_eq!(
                    round_packed_field::<BTreeMap<i32, String>>(&b, packed),
                    *map
                );
                let b = packed_root(map, packed);
                let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
                assert_eq!(<BTreeMap<i32, String>>::ace_decode(&mut d).unwrap(), *map);
                d.finish().unwrap();
            }
        }
        // R22①：同键旧值经 `V::decode_element_into` 解码，容器缓冲不得重新分配；新键照旧解码新值，
        // 旧 Map 中线上已不存在的键被丢弃。只断言值相等无法区分「新建 Map + 逐值解码」的实现。
        let reuse_input = BTreeMap::from([(1i32, alloc::vec![1u16, 2, 3]), (2, alloc::vec![4u16])]);
        for packed in [false, true] {
            let b = packed_root(&reuse_input, packed);
            let mut out = BTreeMap::from([
                (1i32, {
                    let mut old = Vec::with_capacity(8);
                    old.extend_from_slice(&[9u16]);
                    old
                }),
                (3, alloc::vec![7u16]),
            ]);
            let pointer = out[&1].as_ptr();
            let capacity = out[&1].capacity();
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            <BTreeMap<i32, Vec<u16>> as AceDecode>::ace_decode_into(&mut d, &mut out).unwrap();
            d.finish().unwrap();
            assert_eq!(out, reuse_input, "新键解码新值，旧 Map 的多余键被丢弃");
            assert_eq!(out[&1].as_ptr(), pointer, "同键值的缓冲不得重新分配");
            assert_eq!(out[&1].capacity(), capacity, "同键值的容量不得变化");
        }
        // R22① 的键侧：同键命中时新 Map 里必须是**新键**。`LabelKey` 的 `Eq`/`Ord` 只比 `id`，
        // 输入键 `label="new"` 与目标旧键 `label="old"` 命中同一项，故只比较 Map 的 `Eq`
        // 分不出留下的是哪一枚键，必须直接检查键载荷。
        let key_input = BTreeMap::from([(
            LabelKey {
                id: 1,
                label: String::from("new"),
            },
            7u32,
        )]);
        for packed in [false, true] {
            let b = packed_root(&key_input, packed);
            let mut out = BTreeMap::from([(
                LabelKey {
                    id: 1,
                    label: String::from("old"),
                },
                0u32,
            )]);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            <BTreeMap<LabelKey, u32> as AceDecode>::ace_decode_into(&mut d, &mut out).unwrap();
            d.finish().unwrap();
            let decoded_key = out.keys().next().unwrap();
            assert_eq!(decoded_key.id, 1);
            assert_eq!(decoded_key.label, "new", "同键复用后留下的键必须是新键");
            assert_eq!(out[decoded_key], 7, "同键值照旧解码新值");
        }
    }

    /// 映射值（元素形式）的测试替身：体内是 `RefCell<u32>` 的内层标量，`decode_element_into`
    /// 逐字段写入既有单元格——与派生代码一样，写入前先取可变借用作为该单元格的可用性检查，
    /// 泄漏的借用返回 `1 InvalidArgument`（同 `RefCell::ace_decode_into`）。
    struct CellValue(RefCell<u32>);

    impl AceElement for CellValue {
        fn element_kind() -> ElementKind {
            ElementKind::Variable
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <RefCell<u32> as AceElement>::encode_element(&self.0, e)
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            Ok(CellValue(RefCell::new(u32::decode_element(d)?)))
        }
        fn decode_element_into(d: &mut Decoder<'_>, target: &mut Self) -> Result<()> {
            let mut cell = target
                .0
                .try_borrow_mut()
                .map_err(|_| Error::InvalidArgument)?;
            *cell = u32::decode_element(d)?;
            Ok(())
        }
    }

    impl AceEncode for CellValue {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            let cell = self
                .0
                .try_borrow()
                .map_err(|_| e.fail(Error::InvalidArgument))?;
            e.encode_scalar(u64::from(*cell).to_le_bytes())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }

    #[test]
    fn btreemap_into_keeps_rejected_cell_in_place() {
        // R17：同键旧值的逐字段写入因借用泄漏失败时，该单元格必须留在原键位置，内容与不可用状态
        // 都不变。已被移入新 Map 的其它键随失败的解码一并丢弃，不作承诺（不要求整个 `out` 回滚）。
        let input = BTreeMap::from([(1i32, CellValue(RefCell::new(7u32)))]);
        for packed in [false, true] {
            let b = packed_root(&input, packed);
            let mut out = BTreeMap::from([(1i32, CellValue(RefCell::new(5u32)))]);
            core::mem::forget(out[&1].0.borrow_mut());
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(
                <BTreeMap<i32, CellValue> as AceDecode>::ace_decode_into(&mut d, &mut out),
                Err(Error::InvalidArgument)
            );
            let cell = &mut out.get_mut(&1).expect("被拒写的单元格必须仍在原键位置").0;
            assert_eq!(*cell.get_mut(), 5, "被拒写的单元格内容不得改变");
            assert!(cell.try_borrow_mut().is_err(), "单元格的不可用状态必须仍在");
        }
    }

    #[test]
    fn btreemap_option_values_keep_none_positions() {
        // Some("") 与 None 必须区分，且两者都保留在各自的键位置上。
        let map: BTreeMap<i32, Option<String>> = BTreeMap::from([
            (1, None),
            (2, Some(String::new())),
            (3, Some(String::from("x"))),
            (4, None),
        ]);
        for packed in [false, true] {
            let b = packed_field(&map, packed);
            assert_eq!(
                round_packed_field::<BTreeMap<i32, Option<String>>>(&b, packed),
                map
            );
            let b = packed_root(&map, packed);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(
                <BTreeMap<i32, Option<String>>>::ace_decode(&mut d).unwrap(),
                map
            );
            d.finish().unwrap();
        }
    }

    #[test]
    fn btreemap_duplicate_key_is_rejected() {
        // 两对键均为 7：第二次插入时键已存在，解码必须返回 DuplicateKey，而不是静默覆盖。
        let body = [2, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 2, 0, 0, 0];
        let mut e = Encoder::new(&opts());
        e.write_root_record(&body).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(
            <BTreeMap<i32, i32>>::ace_decode(&mut d),
            Err(Error::DuplicateKey)
        );
    }

    #[test]
    fn btreemap_counts_are_bulk_zero() {
        // 每对计一次键元素调用和一次值元素调用，两对在每个方向各计 4 次，LE 与 BE 相同。
        let map: BTreeMap<i32, i32> = BTreeMap::from([(1, 10), (2, 20)]);
        crate::hooks::reset();
        let bytes = root(&map);
        assert_eq!(crate::hooks::snapshot(), (0, 4));
        crate::hooks::reset();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(<BTreeMap<i32, i32>>::ace_decode(&mut d).unwrap(), map);
        d.finish().unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 4));
    }

    #[test]
    fn map_string_key_budget_encode_boundary() {
        let small = Options {
            packed: false,
            max_map_string_key_bytes: 3,
            ..Options::default()
        };
        for key in ["ab", "abc"] {
            let map = BTreeMap::<String, i32>::from([(String::from(key), 1)]);
            let mut e = Encoder::new(&small);
            assert_eq!(map.ace_encode(&mut e), Ok(()));
        }
        let long = BTreeMap::<String, i32>::from([(String::from("abcd"), 1)]);
        let mut e = Encoder::new(&small);
        assert_eq!(long.ace_encode(&mut e), Err(Error::ResourceLimit));
        let some = BTreeMap::<Option<String>, i32>::from([(Some(String::from("abcd")), 1)]);
        let mut e = Encoder::new(&small);
        assert_eq!(some.ace_encode(&mut e), Err(Error::ResourceLimit));
        let none = BTreeMap::<Option<String>, i32>::from([(None, 1)]);
        let mut e = Encoder::new(&small);
        assert_eq!(none.ace_encode(&mut e), Ok(()));
    }

    #[test]
    fn map_string_key_budget_decode_boundary() {
        let small = Options {
            packed: false,
            max_map_string_key_bytes: 3,
            ..Options::default()
        };
        let long = BTreeMap::<String, i32>::from([(String::from("abcd"), 1)]);
        let wire = root(&long);
        let mut d = Decoder::new(&wire, &small).unwrap();
        assert_eq!(
            <BTreeMap<String, i32>>::ace_decode(&mut d),
            Err(Error::ResourceLimit)
        );
        for key in ["ab", "abc"] {
            let map = BTreeMap::<String, i32>::from([(String::from(key), 1)]);
            let wire = root(&map);
            let mut d = Decoder::new(&wire, &small).unwrap();
            assert_eq!(<BTreeMap<String, i32>>::ace_decode(&mut d).unwrap(), map);
            d.finish().unwrap();
        }
    }

    #[test]
    fn map_string_key_budget_does_not_limit_values() {
        let small = Options {
            packed: false,
            max_map_string_key_bytes: 3,
            ..Options::default()
        };
        let values = BTreeMap::<i32, String>::from([(1, String::from("abcdef"))]);
        let none = BTreeMap::<Option<String>, String>::from([(None, String::from("abcdef"))]);
        let mut e = Encoder::new(&small);
        values.ace_encode(&mut e).unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &small).unwrap();
        assert_eq!(<BTreeMap<i32, String>>::ace_decode(&mut d).unwrap(), values);
        d.finish().unwrap();
        let mut e = Encoder::new(&small);
        none.ace_encode(&mut e).unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &small).unwrap();
        assert_eq!(
            <BTreeMap<Option<String>, String>>::ace_decode(&mut d).unwrap(),
            none
        );
        d.finish().unwrap();
    }

    #[test]
    fn vec_of_vec_roundtrip_raw_packed_field_and_root() {
        fn check<T: AceEncode + AceDecode + PartialEq + core::fmt::Debug>(v: &T) {
            for packed in [false, true] {
                let o = Options {
                    packed,
                    ..Options::default()
                };
                let mut e = Encoder::new(&o);
                v.ace_encode(&mut e).unwrap();
                let bytes = e.finish().unwrap();
                let mut d = Decoder::new(&bytes, &o).unwrap();
                assert_eq!(&T::ace_decode(&mut d).unwrap(), v);
                d.finish().unwrap();
                let mut e = Encoder::new(&o);
                // 字段须位于对象帧内（设计 §1 第 2 项）。
                e.enter_object().unwrap();
                e.begin_field(1).unwrap();
                v.ace_encode(&mut e).unwrap();
                // 离开对象帧写入根槽（设计 §1 第 5 项，FR-W10）。
                e.leave_object().unwrap();
                let bytes = e.finish().unwrap();
                let mut d = Decoder::new(&bytes, &o).unwrap();
                // 根槽载荷是对象引用（FR-W10），须先 enter_object 再读字段。
                let payload = d.read_root_scalar().unwrap();
                d.enter_object(u64::from_le_bytes(payload)).unwrap();
                d.begin_field(1).unwrap();
                assert_eq!(&T::ace_decode(&mut d).unwrap(), v);
                // 与 enter_object 成对离开，finish 要求无未关闭对象帧。
                d.leave_object().unwrap();
                d.finish().unwrap();
            }
        }
        // 空内层与空外层各覆盖一次。
        let ints: Vec<Vec<i32>> = alloc::vec![alloc::vec![1, 2], Vec::new()];
        check(&ints);
        check(&Vec::<Vec<i32>>::new());
        let names = alloc::vec![String::from("中文"), String::new()];
        check(&alloc::vec![names, Vec::new()]);
        // Rc<u32> 只实现 AceEncode：元素体为 4 字节目标 id；读回须与 Box<u32> 的既有元素布局一致。
        let rcs = alloc::vec![alloc::rc::Rc::new(1u32), alloc::rc::Rc::new(2u32)];
        for packed in [false, true] {
            let o = Options {
                packed,
                ..Options::default()
            };
            let mut e = Encoder::new(&o);
            rcs.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(
                <Vec<Box<u32>>>::ace_decode(&mut d).unwrap(),
                alloc::vec![Box::new(1u32), Box::new(2u32)]
            );
            d.finish().unwrap();
        }
    }

    #[test]
    fn vec_of_option_vec_roundtrip() {
        // 样本覆盖 None、Some(空) 与 Some(非空)。
        let some = alloc::vec![1, -2];
        let v: Vec<Option<Vec<i32>>> = alloc::vec![None, Some(Vec::new()), Some(some)];
        let bytes = root(&v);
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(<Vec<Option<Vec<i32>>>>::ace_decode(&mut d).unwrap(), v);
        d.finish().unwrap();
    }

    #[test]
    fn vec_of_vec_counts_le_be() {
        // LE 下内层 i32 走 bulk 路径：编码侧只有非空内层有整段拷贝才计一次（空内层不计）；
        // 解码侧的空内层仍是一次 bulk 调用（消费 0 字节，本包未改解码计数）。BE 下 i32 无 bulk，
        // 两侧均为每个元素各计一次。
        let encode_expected = if cfg!(target_endian = "little") {
            (1, 2)
        } else {
            (0, 4)
        };
        let decode_expected = if cfg!(target_endian = "little") {
            (2, 2)
        } else {
            (0, 4)
        };
        let v: Vec<Vec<i32>> = alloc::vec![alloc::vec![1, 2], Vec::new()];
        crate::hooks::reset();
        let bytes = root(&v);
        assert_eq!(crate::hooks::snapshot(), encode_expected);
        crate::hooks::reset();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(<Vec<Vec<i32>>>::ace_decode(&mut d).unwrap(), v);
        d.finish().unwrap();
        assert_eq!(crate::hooks::snapshot(), decode_expected);
    }

    #[test]
    fn vec_of_vec_record_budget_boundary() {
        // 内层 2 条记录加外层 1 条记录，共 3 条。
        let v: Vec<Vec<i32>> = alloc::vec![alloc::vec![1], alloc::vec![2]];
        for (max_records, expected) in [(3, Ok(())), (2, Err(Error::ResourceLimit))] {
            let o = Options {
                packed: false,
                max_records,
                ..Options::default()
            };
            let mut e = Encoder::new(&o);
            assert_eq!(v.ace_encode(&mut e), expected);
        }
    }

    #[test]
    fn vec_of_vec_bad_inner_id_is_invalid_reference() {
        // 外层记录的唯一元素指向不存在的内层记录 id 99。
        let mut e = Encoder::new(&opts());
        e.begin_record().unwrap();
        e.write_body_bytes(&1u32.to_le_bytes()).unwrap();
        e.write_body_bytes(&99u32.to_le_bytes()).unwrap();
        let outer = e.end_record().unwrap();
        e.write_root_scalar(u64::from(outer).to_le_bytes()).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(
            <Vec<Vec<i32>>>::ace_decode(&mut d),
            Err(Error::InvalidReference)
        );
    }

    #[test]
    fn btreemap_vec_string_key_budget() {
        let small = Options {
            packed: false,
            max_map_string_key_bytes: 3,
            ..Options::default()
        };
        let key = alloc::vec![String::from("ab"), String::from("abc")];
        let ok = BTreeMap::<Vec<String>, i32>::from([(key, 1)]);
        let mut e = Encoder::new(&small);
        ok.ace_encode(&mut e).unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &small).unwrap();
        let decoded = <BTreeMap<Vec<String>, i32>>::ace_decode(&mut d).unwrap();
        assert_eq!(decoded, ok);
        d.finish().unwrap();

        let key = alloc::vec![String::from("abcd")];
        let long = BTreeMap::<Vec<String>, i32>::from([(key, 1)]);
        let mut e = Encoder::new(&small);
        assert_eq!(long.ace_encode(&mut e), Err(Error::ResourceLimit));
        // 默认预算下编码的记录，在小预算下解码时于复制键体前被拒绝。
        let wire = root(&long);
        let mut d = Decoder::new(&wire, &small).unwrap();
        assert_eq!(
            <BTreeMap<Vec<String>, i32>>::ace_decode(&mut d),
            Err(Error::ResourceLimit)
        );

        let key = Some(alloc::vec![String::from("abcd")]);
        let some = BTreeMap::<Option<Vec<String>>, i32>::from([(key, 1)]);
        let mut e = Encoder::new(&small);
        assert_eq!(some.ace_encode(&mut e), Err(Error::ResourceLimit));
    }

    #[test]
    fn vec_nesting_depth_limit() {
        let max0 = Options {
            packed: false,
            max_depth: 0,
            ..Options::default()
        };
        let max2 = Options {
            packed: false,
            max_depth: 2,
            ..Options::default()
        };
        // 根集合计入打开的容器数，自身进入时 k=0；`Vec<i32>` 的元素不是集合，故深度上限为零时仍可编解码。
        let flat = alloc::vec![1i32, 2];
        let mut e = Encoder::new(&max0);
        flat.ace_encode(&mut e).unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &max0).unwrap();
        assert_eq!(<Vec<i32>>::ace_decode(&mut d).unwrap(), flat);
        d.finish().unwrap();

        let nested = alloc::vec![alloc::vec![1i32, 2]];
        let mut e = Encoder::new(&max0);
        assert_eq!(nested.ace_encode(&mut e), Err(Error::DepthExceeded));
        let wire = root(&nested);
        let mut d = Decoder::new(&wire, &max0).unwrap();
        let decoded = <Vec<Vec<i32>>>::ace_decode(&mut d);
        assert_eq!(decoded, Err(Error::DepthExceeded));

        let three = alloc::vec![alloc::vec![alloc::vec![1i32]]];
        let mut e = Encoder::new(&max2);
        three.ace_encode(&mut e).unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &max2).unwrap();
        let decoded = <Vec<Vec<Vec<i32>>>>::ace_decode(&mut d).unwrap();
        assert_eq!(decoded, three);
        d.finish().unwrap();

        let four = alloc::vec![alloc::vec![alloc::vec![alloc::vec![1i32]]]];
        let mut e = Encoder::new(&max2);
        assert_eq!(four.ace_encode(&mut e), Err(Error::DepthExceeded));
        let wire = root(&four);
        let mut d = Decoder::new(&wire, &max2).unwrap();
        let decoded = <Vec<Vec<Vec<Vec<i32>>>>>::ace_decode(&mut d);
        assert_eq!(decoded, Err(Error::DepthExceeded));
    }

    #[test]
    fn btreemap_nested_collection_depth() {
        let max0 = Options {
            packed: false,
            max_depth: 0,
            ..Options::default()
        };
        let max1 = Options {
            packed: false,
            max_depth: 1,
            ..Options::default()
        };
        let values = BTreeMap::from([(1i32, alloc::vec![2i32])]);
        let mut e = Encoder::new(&max0);
        assert_eq!(values.ace_encode(&mut e), Err(Error::DepthExceeded));
        let mut e = Encoder::new(&max1);
        values.ace_encode(&mut e).unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &max1).unwrap();
        let decoded = <BTreeMap<i32, Vec<i32>>>::ace_decode(&mut d).unwrap();
        assert_eq!(decoded, values);
        d.finish().unwrap();

        let keys = BTreeMap::from([(alloc::vec![1i32], 2i32)]);
        let mut e = Encoder::new(&max0);
        assert_eq!(keys.ace_encode(&mut e), Err(Error::DepthExceeded));
        let wire = root(&keys);
        let mut d = Decoder::new(&wire, &max0).unwrap();
        let decoded = <BTreeMap<Vec<i32>, i32>>::ace_decode(&mut d);
        assert_eq!(decoded, Err(Error::DepthExceeded));
    }

    #[test]
    fn field_collection_counts_enclosing_object_depth() {
        // 依据 V4-1、V4-8：对象字段中的集合计入深度，字段内集合进入前的 open 含所在对象帧。
        let max1 = Options {
            packed: false,
            max_depth: 1,
            ..Options::default()
        };
        let max2 = Options {
            packed: false,
            max_depth: 2,
            ..Options::default()
        };
        let nested = alloc::vec![alloc::vec![1i32]];
        let mut e = Encoder::new(&max2);
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        nested.ace_encode(&mut e).unwrap();
        e.leave_object().unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &max2).unwrap();
        let root_scalar = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root_scalar)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(<Vec<Vec<i32>>>::ace_decode(&mut d).unwrap(), nested);
        d.leave_object().unwrap();
        d.finish().unwrap();

        let mut d = Decoder::new(&wire, &max1).unwrap();
        let root_scalar = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(root_scalar)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(
            <Vec<Vec<i32>>>::ace_decode(&mut d),
            Err(Error::DepthExceeded)
        );
        d.leave_object().unwrap();
        d.finish().unwrap();

        let mut e = Encoder::new(&max1);
        e.enter_object().unwrap();
        e.begin_field(1).unwrap();
        assert_eq!(nested.ace_encode(&mut e), Err(Error::DepthExceeded));
    }

    // 元素对象：字段 1 为 v；离开时占位记录的 record id 以 4 字节 LE 写入列表元素（设计 :131）。
    #[derive(Debug, Clone, PartialEq)]
    struct Item {
        v: i32,
    }
    impl NonOptional for Item {}
    impl AceElement for Item {
        fn element_kind() -> ElementKind {
            ElementKind::Ref
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let mut body = || -> Result<()> {
                e.begin_field(1)?;
                self.v.ace_encode(e)
            };
            let encoded = body();
            // 先离开对象，再传播编码错误（V4-11）。
            let id = e.leave_object();
            encoded?;
            let id = id?;
            e.write_element_bytes(&id.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
            d.enter_object(u64::from(id))?;
            let mut body = || -> Result<i32> {
                d.begin_field(1)?;
                i32::ace_decode(d)
            };
            let decoded = body();
            // 先离开对象，再传播解码错误（V4-11）。
            let left = d.leave_object();
            let v = decoded?;
            left?;
            Ok(Item { v })
        }
    }
    // Slot 形式仅适用于字段位置，根位置由 leave_object 写根槽，不得使用。
    impl AceEncode for Item {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let mut body = || -> Result<()> {
                e.begin_field(1)?;
                self.v.ace_encode(e)
            };
            let encoded = body();
            // 先离开对象，再传播编码错误（V4-11）。
            let id = e.leave_object();
            encoded?;
            e.encode_scalar(u64::from(id?).to_le_bytes())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }
    // 无字段元素对象：只占 8 字节占位记录，槽数为零；Some(Empty) 与 None 由 presence 字节区分。
    #[derive(Debug, Clone, PartialEq)]
    struct Empty;
    impl NonOptional for Empty {}
    impl AceElement for Empty {
        fn element_kind() -> ElementKind {
            ElementKind::Ref
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let id = e.leave_object()?;
            e.write_element_bytes(&id.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
            d.enter_object(u64::from(id))?;
            d.leave_object()?;
            Ok(Empty)
        }
    }
    // Slot 形式仅适用于字段位置，根位置由 leave_object 写根槽，不得使用。
    impl AceEncode for Empty {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let id = e.leave_object()?;
            e.encode_scalar(u64::from(id).to_le_bytes())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }
    // 根对象：字段 1 为集合内的元素对象列表（元素对象 k=2），字段 2 为 i32 尾部。
    #[derive(Debug, Clone, PartialEq)]
    struct Holder {
        items: Vec<Item>,
        tail: i32,
    }
    impl AceEncode for Holder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let mut body = || -> Result<()> {
                e.begin_field(1)?;
                self.items.ace_encode(e)?;
                e.begin_field(2)?;
                self.tail.ace_encode(e)
            };
            let encoded = body();
            // 先离开根对象，再传播编码错误（V4-11）。
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecode for Holder {
        fn ace_decode(d: &mut Decoder<'_>) -> Result<Self> {
            let p = d.decode_required_scalar()?;
            d.enter_object(u64::from_le_bytes(p))?;
            let mut body = || -> Result<(Vec<Item>, i32)> {
                d.begin_field(1)?;
                let items = Vec::<Item>::ace_decode(d)?;
                d.begin_field(2)?;
                let tail = i32::ace_decode(d)?;
                Ok((items, tail))
            };
            let decoded = body();
            // 先离开根对象，再传播解码错误（V4-11）。
            let left = d.leave_object();
            let (items, tail) = decoded?;
            left?;
            Ok(Holder { items, tail })
        }
    }

    #[test]
    fn object_elements_in_root_vec_are_depth_one_not_root() {
        // 根列表的元素对象 k=1，不是根（V4-7）：max_depth=0 时进入元素对象即 DepthExceeded。
        let items = alloc::vec![Item { v: 1 }, Item { v: 2 }, Item { v: 3 }];
        for packed in [false, true] {
            let max0 = Options {
                packed,
                max_depth: 0,
                ..Options::default()
            };
            let mut e = Encoder::new(&max0);
            assert_eq!(items.ace_encode(&mut e), Err(Error::DepthExceeded));

            let max1 = Options {
                packed,
                max_depth: 1,
                ..Options::default()
            };
            let mut e = Encoder::new(&max1);
            items.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &max1).unwrap();
            assert_eq!(<Vec<Item>>::ace_decode(&mut d).unwrap(), items);
            d.finish().unwrap();
        }
    }

    #[test]
    fn vec_option_object_none_positions_keep_order() {
        let v = alloc::vec![None, Some(Item { v: 5 }), None, Some(Item { v: 6 }), None];
        for packed in [false, true] {
            let o = Options {
                packed,
                ..Options::default()
            };
            let mut e = Encoder::new(&o);
            v.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(<Vec<Option<Item>>>::ace_decode(&mut d).unwrap(), v);
            d.finish().unwrap();
        }
    }

    #[test]
    fn vec_option_empty_object_some_is_not_none() {
        // Some(Empty) 写入 presence 字节 1 与占位记录 id，因而不同于 None。
        let v = alloc::vec![Some(Empty), None, Some(Empty)];
        for packed in [false, true] {
            let o = Options {
                packed,
                ..Options::default()
            };
            let mut e = Encoder::new(&o);
            v.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(<Vec<Option<Empty>>>::ace_decode(&mut d).unwrap(), v);
            d.finish().unwrap();
        }
    }

    #[test]
    fn object_collection_object_depth_zero_one_two() {
        // 根对象 k=0，集合 k=1，集合内对象 k=2，其字段槽深度为 3（V4-1、V4-2）。
        let holder = Holder {
            items: alloc::vec![Item { v: 1 }, Item { v: 2 }],
            tail: 9,
        };
        for packed in [false, true] {
            let max2 = Options {
                packed,
                max_depth: 2,
                ..Options::default()
            };
            let mut e = Encoder::new(&max2);
            holder.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &max2).unwrap();
            assert_eq!(Holder::ace_decode(&mut d).unwrap(), holder);
            d.finish().unwrap();
        }
    }

    #[test]
    fn object_collection_object_depth_overflow_is_rejected() {
        // 集合内对象 k=2 超过 max_depth=1：编码与解码均在 enter_object 处拒绝（V4-1）。
        let holder = Holder {
            items: alloc::vec![Item { v: 1 }, Item { v: 2 }],
            tail: 9,
        };
        for packed in [false, true] {
            let default_opts = Options {
                packed,
                ..Options::default()
            };
            let max1 = Options {
                packed,
                max_depth: 1,
                ..Options::default()
            };
            let mut e = Encoder::new(&max1);
            assert_eq!(holder.ace_encode(&mut e), Err(Error::DepthExceeded));

            let mut e = Encoder::new(&default_opts);
            holder.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &max1).unwrap();
            assert_eq!(Holder::ace_decode(&mut d), Err(Error::DepthExceeded));
        }
    }

    #[test]
    fn object_element_slots_count_toward_max_slots() {
        // 根预留 1 个槽加 3 个元素字段槽共 4 个；max_slots=3 在第 3 个字段槽前拒绝（V4-6）。
        let items = alloc::vec![Item { v: 1 }, Item { v: 2 }, Item { v: 3 }];
        for packed in [false, true] {
            let max3 = Options {
                packed,
                max_slots: 3,
                ..Options::default()
            };
            let mut e = Encoder::new(&max3);
            assert_eq!(items.ace_encode(&mut e), Err(Error::ResourceLimit));

            let max4 = Options {
                packed,
                max_slots: 4,
                ..Options::default()
            };
            let mut e = Encoder::new(&max4);
            items.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &max4).unwrap();
            assert_eq!(<Vec<Item>>::ace_decode(&mut d).unwrap(), items);
            d.finish().unwrap();
        }
    }

    use alloc::boxed::Box;

    #[test]
    fn box_i32_field_and_root_roundtrip() {
        let value = Box::new(-42i32);
        for packed in [false, true] {
            let o = Options {
                packed,
                ..Options::default()
            };
            for as_field in [false, true] {
                let mut e = Encoder::new(&o);
                if as_field {
                    e.enter_object().unwrap();
                    e.begin_field(1).unwrap();
                }
                value.ace_encode(&mut e).unwrap();
                if as_field {
                    e.leave_object().unwrap();
                }
                let bytes = e.finish().unwrap();
                let mut d = Decoder::new(&bytes, &o).unwrap();
                if as_field {
                    let payload = d.read_root_scalar().unwrap();
                    d.enter_object(u64::from_le_bytes(payload)).unwrap();
                    d.begin_field(1).unwrap();
                }
                assert_eq!(Box::<i32>::ace_decode(&mut d).unwrap(), value);
                if as_field {
                    d.leave_object().unwrap();
                }
                d.finish().unwrap();
            }
        }
    }

    #[test]
    fn box_string_roundtrip_is_nested_record() {
        let value = Box::new(String::from("boxed 中"));
        for packed in [false, true] {
            let o = Options {
                packed,
                ..Options::default()
            };
            let mut e = Encoder::new(&o);
            value.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            let box_id = u32::try_from(u64::from_le_bytes(d.read_root_scalar().unwrap())).unwrap();
            d.begin_record_view(box_id).unwrap();
            let body = d.read_body_to_end().unwrap();
            let string_id = u32::from_le_bytes(body.try_into().unwrap());
            d.end_record_view();
            assert_ne!(box_id, string_id);
            d.begin_record_view(string_id).unwrap();
            assert_eq!(d.read_body_to_end().unwrap(), value.as_bytes());
            d.end_record_view();
            d.finish().unwrap();

            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(Box::<String>::ace_decode(&mut d).unwrap(), value);
            d.finish().unwrap();
        }
    }

    #[test]
    fn box_object_roundtrip() {
        let value = Box::new(Item { v: 7 });
        for packed in [false, true] {
            let o = Options {
                packed,
                ..Options::default()
            };
            let mut e = Encoder::new(&o);
            value.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(Box::<Item>::ace_decode(&mut d).unwrap(), value);
            d.finish().unwrap();
        }
    }

    #[test]
    fn box_depth_matches_vec_depth() {
        let boxed = Box::new(Item { v: 7 });
        let items = alloc::vec![Item { v: 7 }];
        let box_wire = root(&boxed);
        let vec_wire = root(&items);
        for max_depth in [0, 1] {
            let o = Options {
                max_depth,
                ..opts()
            };
            let expected = if max_depth == 0 {
                Err(Error::DepthExceeded)
            } else {
                Ok(())
            };
            let mut box_encoder = Encoder::new(&o);
            let mut vec_encoder = Encoder::new(&o);
            let encoded = boxed.ace_encode(&mut box_encoder);
            assert_eq!(encoded, items.ace_encode(&mut vec_encoder));
            assert_eq!(encoded, expected);

            let mut box_decoder = Decoder::new(&box_wire, &o).unwrap();
            let mut vec_decoder = Decoder::new(&vec_wire, &o).unwrap();
            let decoded = Box::<Item>::ace_decode(&mut box_decoder).map(|_| ());
            assert_eq!(
                decoded,
                Vec::<Item>::ace_decode(&mut vec_decoder).map(|_| ())
            );
            assert_eq!(decoded, expected);
            box_decoder.finish().unwrap();
            vec_decoder.finish().unwrap();
        }
    }

    #[test]
    fn required_box_missing_is_missing_required_field() {
        let mut e = Encoder::new(&opts());
        e.enter_object().unwrap();
        e.leave_object().unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        let payload = d.read_root_scalar().unwrap();
        d.enter_object(u64::from_le_bytes(payload)).unwrap();
        d.begin_field(1).unwrap();
        assert_eq!(
            Box::<i32>::ace_decode(&mut d),
            Err(Error::MissingRequiredField)
        );
        d.leave_object().unwrap();
        d.finish().unwrap();
    }

    #[test]
    fn vec_box_roundtrip_raw_packed_field_and_root() {
        let ints = alloc::vec![Box::new(1i32), Box::new(0), Box::new(-2)];
        let strings = alloc::vec![Box::new(String::from("a")), Box::new(String::from("bc"))];
        for packed in [false, true] {
            let b = packed_field(&ints, packed);
            assert_eq!(round_packed_field::<Vec<Box<i32>>>(&b, packed), ints);
            let b = packed_root(&ints, packed);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(<Vec<Box<i32>>>::ace_decode(&mut d).unwrap(), ints);
            d.finish().unwrap();
            let b = packed_field(&strings, packed);
            assert_eq!(round_packed_field::<Vec<Box<String>>>(&b, packed), strings);
            let b = packed_root(&strings, packed);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(<Vec<Box<String>>>::ace_decode(&mut d).unwrap(), strings);
            d.finish().unwrap();
        }
    }

    #[test]
    fn vec_option_box_none_positions_keep_order() {
        let v = alloc::vec![
            None,
            Some(Box::new(String::new())),
            None,
            Some(Box::new(String::from("x"))),
            None,
        ];
        for packed in [false, true] {
            let bytes = packed_root(&v, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            assert_eq!(<Vec<Option<Box<String>>>>::ace_decode(&mut d).unwrap(), v);
            d.finish().unwrap();
        }
    }

    #[test]
    fn vec_box_depth_matches_vec_of_vec() {
        let boxed = alloc::vec![Box::new(Item { v: 7 })];
        let nested = alloc::vec![alloc::vec![Item { v: 7 }]];
        for max_depth in [1, 2] {
            let o = Options {
                max_depth,
                ..opts()
            };
            let expected = if max_depth == 1 {
                Err(Error::DepthExceeded)
            } else {
                Ok(())
            };
            let mut box_encoder = Encoder::new(&o);
            let mut vec_encoder = Encoder::new(&o);
            let encoded = boxed.ace_encode(&mut box_encoder);
            assert_eq!(encoded, nested.ace_encode(&mut vec_encoder));
            assert_eq!(encoded, expected);
        }
    }

    #[test]
    fn vec_box_trailing_byte_is_invalid_tag() {
        let mut e = Encoder::new(&opts());
        let box_id = e.alloc_record(&[7, 0, 0, 0, 9]).unwrap();
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&box_id.to_le_bytes());
        e.encode_record(&body).unwrap();
        let bytes = e.finish().unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        assert_eq!(<Vec<Box<i32>>>::ace_decode(&mut d), Err(Error::InvalidTag));
        d.finish().unwrap();
    }

    #[test]
    fn btreemap_box_string_key_budget() {
        let small = Options {
            max_map_string_key_bytes: 3,
            ..opts()
        };
        let long = BTreeMap::<Box<String>, i32>::from([(Box::new(String::from("abcd")), 1)]);
        let mut e = Encoder::new(&small);
        assert_eq!(long.ace_encode(&mut e), Err(Error::ResourceLimit));
    }

    #[test]
    fn vec_btreemap_roundtrip_raw_packed_field_and_root() {
        let maps = alloc::vec![
            BTreeMap::<String, i32>::new(),
            BTreeMap::from([(String::from("a"), 1), (String::from("bc"), -2)]),
        ];
        let optional = alloc::vec![
            None,
            Some(BTreeMap::<i32, i32>::new()),
            None,
            Some(BTreeMap::from([(1, 10)])),
            None,
        ];
        for packed in [false, true] {
            let b = packed_field(&maps, packed);
            assert_eq!(
                round_packed_field::<Vec<BTreeMap<String, i32>>>(&b, packed),
                maps
            );
            let b = packed_root(&maps, packed);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(
                <Vec<BTreeMap<String, i32>>>::ace_decode(&mut d).unwrap(),
                maps
            );
            d.finish().unwrap();

            let b = packed_field(&optional, packed);
            assert_eq!(
                round_packed_field::<Vec<Option<BTreeMap<i32, i32>>>>(&b, packed),
                optional
            );
            let b = packed_root(&optional, packed);
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            assert_eq!(
                <Vec<Option<BTreeMap<i32, i32>>>>::ace_decode(&mut d).unwrap(),
                optional
            );
            d.finish().unwrap();
        }
        // R22①：`Vec<BTreeMap<..>>` 的既有元素经 `BTreeMap::decode_element_into` 解码，同键值的
        // 内层 `Vec` 缓冲保留原有分配（Map 节点与键仍重建）。整体替换的实现会重建内层缓冲，
        // 只断言值相等无法发现。
        let reuse_input = alloc::vec![BTreeMap::from([(1i32, alloc::vec![1u16, 2, 3])])];
        for packed in [false, true] {
            let b = packed_root(&reuse_input, packed);
            let mut out: Vec<BTreeMap<i32, Vec<u16>>> = alloc::vec![BTreeMap::from([(1i32, {
                let mut old = Vec::with_capacity(8);
                old.extend_from_slice(&[9u16]);
                old
            })])];
            let pointer = out[0][&1].as_ptr();
            let capacity = out[0][&1].capacity();
            let mut d = Decoder::new(&b, &packed_opts(packed)).unwrap();
            <Vec<BTreeMap<i32, Vec<u16>>> as AceDecode>::ace_decode_into(&mut d, &mut out).unwrap();
            d.finish().unwrap();
            assert_eq!(out, reuse_input, "映射元素按线上值写入");
            assert_eq!(
                out[0][&1].as_ptr(),
                pointer,
                "映射元素的内层值缓冲不得重新分配"
            );
            assert_eq!(
                out[0][&1].capacity(),
                capacity,
                "映射元素的内层值容量不得变化"
            );
        }
    }

    #[test]
    fn vec_btreemap_depth_matches_vec_of_vec() {
        let maps = alloc::vec![BTreeMap::from([(1i32, Item { v: 7 })])];
        let nested = alloc::vec![alloc::vec![Item { v: 7 }]];
        let map_wire = root(&maps);
        let vec_wire = root(&nested);
        for max_depth in [1, 2] {
            let o = Options {
                max_depth,
                ..opts()
            };
            let expected = if max_depth == 1 {
                Err(Error::DepthExceeded)
            } else {
                Ok(())
            };
            let mut map_encoder = Encoder::new(&o);
            let mut vec_encoder = Encoder::new(&o);
            let encoded = maps.ace_encode(&mut map_encoder);
            assert_eq!(encoded, nested.ace_encode(&mut vec_encoder));
            assert_eq!(encoded, expected);

            let mut map_decoder = Decoder::new(&map_wire, &o).unwrap();
            let mut vec_decoder = Decoder::new(&vec_wire, &o).unwrap();
            let decoded = <Vec<BTreeMap<i32, Item>>>::ace_decode(&mut map_decoder).map(|_| ());
            assert_eq!(
                decoded,
                <Vec<Vec<Item>>>::ace_decode(&mut vec_decoder).map(|_| ())
            );
            assert_eq!(decoded, expected);
            map_decoder.finish().unwrap();
            vec_decoder.finish().unwrap();
        }
    }

    #[test]
    fn btreemap_element_string_key_budget() {
        let small = Options {
            max_map_string_key_bytes: 3,
            ..opts()
        };
        let ok = alloc::vec![BTreeMap::<String, i32>::from([(String::from("ab"), 1)])];
        let mut e = Encoder::new(&small);
        ok.ace_encode(&mut e).unwrap();
        let wire = e.finish().unwrap();
        let mut d = Decoder::new(&wire, &small).unwrap();
        assert_eq!(
            <Vec<BTreeMap<String, i32>>>::ace_decode(&mut d).unwrap(),
            ok
        );
        d.finish().unwrap();

        let long = alloc::vec![BTreeMap::<String, i32>::from([(String::from("abcd"), 1)])];
        let mut e = Encoder::new(&small);
        assert_eq!(long.ace_encode(&mut e), Err(Error::ResourceLimit));
        // 默认预算下编码的记录，在小预算下解码时于复制键体前被拒绝。
        let wire = root(&long);
        let mut d = Decoder::new(&wire, &small).unwrap();
        assert_eq!(
            <Vec<BTreeMap<String, i32>>>::ace_decode(&mut d),
            Err(Error::ResourceLimit)
        );
    }

    #[test]
    fn btreemap_key_value_string_budget() {
        let small = Options {
            max_map_string_key_bytes: 3,
            ..opts()
        };
        let inner = BTreeMap::<i32, String>::from([(1, String::from("abcd"))]);
        let outer = BTreeMap::<BTreeMap<i32, String>, i32>::from([(inner, 1)]);
        let mut e = Encoder::new(&small);
        assert_eq!(outer.ace_encode(&mut e), Err(Error::ResourceLimit));

        let wire = root(&outer);
        let mut d = Decoder::new(&wire, &small).unwrap();
        assert_eq!(
            <BTreeMap<BTreeMap<i32, String>, i32>>::ace_decode(&mut d),
            Err(Error::ResourceLimit)
        );
    }

    /// R3 测试用户对象：普通对象的一个 u32 字段（tag 1）。作为 Option 元素时以对象记录 id 引用；
    /// 仅在测试内定义，不进入库公开面。
    #[derive(Debug, PartialEq, Eq)]
    struct UserObj(u32);
    impl NonOptional for UserObj {}
    impl UserObj {
        /// 普通对象编码：进入对象、写字段、离开对象并返回对象记录 id；字段失败仍先离开对象。
        fn encode_object(&self, e: &mut Encoder<'_>) -> Result<u32> {
            e.enter_object()?;
            let encoded = e.begin_field(1).and_then(|()| self.0.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left
        }
    }
    impl AceEncode for UserObj {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            self.encode_object(e).map(|_| ())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }
    impl AceElement for UserObj {
        fn element_kind() -> ElementKind {
            ElementKind::Ref
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            let id = self.encode_object(e)?;
            e.write_element_bytes(&id.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            let id = u32::from_le_bytes(d.read_element_bytes(4)?.try_into().unwrap());
            d.enter_object(u64::from(id))?;
            let value = d.begin_field(1).and_then(|_| u32::ace_decode(d));
            let left = d.leave_object();
            let value = value?;
            left?;
            Ok(Self(value))
        }
    }
    impl AceDecodeCyclic for UserObj {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            ctx.object(|ctx| ctx.field::<u32>(1))
        }
        fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
            Self(plan)
        }
    }
    /// 根普通对象仅有一个 `UserObj` 字段：子对象先完成，父对象再把子对象 id 写入自己的字段槽。
    struct ParentOfUser {
        child: UserObj,
    }
    impl AceEncode for ParentOfUser {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let child = self.child.encode_object(e);
            let encoded = child.and_then(|id| e.write_field_scalar(0, u64::from(id).to_le_bytes()));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    /// 根普通对象先持 Weak，再持同一目标的强 Rc：Weak 先预留目标 id，强定义随后复用该 id。
    struct WeakFirstHolder {
        weak: alloc::rc::Weak<Option<UserObj>>,
        owner: alloc::rc::Rc<Option<UserObj>>,
    }
    impl AceEncode for WeakFirstHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.weak.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for WeakFirstHolder {
        type Plan = (u32, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<alloc::rc::Weak<Option<UserObj>>>(0)?,
                    ctx.field::<alloc::rc::Rc<Option<UserObj>>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                weak: alloc::rc::Weak::<Option<UserObj>>::build_from_plan(ctx, plan.0),
                owner: alloc::rc::Rc::<Option<UserObj>>::build_from_plan(ctx, plan.1),
            }
        }
    }
    /// 以给定 `max_depth` 编码根值，返回完整字节。
    fn r3_message<T: AceEncode>(value: &T, max_depth: usize) -> Result<Vec<u8>> {
        let mut encoder = Encoder::new(&Options {
            max_depth,
            ..opts()
        });
        value.ace_encode(&mut encoder)?;
        encoder.finish()
    }
    /// 按 `options` 编码根值并核对布局：raw 必须等于金样。
    /// packed 解包后前 L 字节须等于金样，其后仅允许 0..7 个对齐零字节。
    fn r3_encode_layout<T: AceEncode>(value: &T, options: &Options, expected: &[u8]) -> Vec<u8> {
        let mut encoder = Encoder::new(options);
        value.ace_encode(&mut encoder).unwrap();
        let bytes = encoder.finish().unwrap();
        if options.packed {
            let raw = crate::wire::packed::unpack(&bytes, &packed_opts(true)).unwrap();
            assert_eq!(&raw[..expected.len()], expected);
            assert!(raw.len() - expected.len() <= 7);
            assert!(raw[expected.len()..].iter().all(|&b| b == 0));
        } else {
            assert_eq!(bytes, expected);
        }
        bytes
    }
    #[test]
    fn r_s3_g_r3_vec_option_user_object_positions() {
        let value = alloc::vec![None, Some(UserObj(7)), None, Some(UserObj(9)), None];
        assert_eq!(r3_message(&value, 0), Err(Error::DepthExceeded));
        let main = [
            target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0]),
            // count=5；None 为 [0]，Some 为 [1] 加对象 id。
            target_rec(3, &[5, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(2, 1, 7u64.to_le_bytes()),
            target_slot(2, 1, 9u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 113);
        for packed in [false, true] {
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = Vec::<Option<UserObj>>::ace_decode(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded, value);
        }
    }
    #[test]
    fn r_s3_g_r3_map_option_user_object_positions() {
        let value = BTreeMap::from([
            (1u32, None),
            (2, Some(UserObj(7))),
            (3, None),
            (4, Some(UserObj(9))),
            (5, None),
        ]);
        assert_eq!(r3_message(&value, 0), Err(Error::DepthExceeded));
        let main = [
            target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0]),
            // count=5；每对为键（u32）后接值：None 为 [0]，Some 为 [1] 加对象 id。
            target_rec(
                3,
                &[
                    5, 0, 0, 0, 1, 0, 0, 0, 0, 2, 0, 0, 0, 1, 1, 0, 0, 0, 3, 0, 0, 0, 0, 4, 0, 0,
                    0, 1, 2, 0, 0, 0, 5, 0, 0, 0, 0,
                ],
            ),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(2, 1, 7u64.to_le_bytes()),
            target_slot(2, 1, 9u64.to_le_bytes()),
        ]
        .concat();
        let wire = target_raw(&main, &assist);
        assert_eq!(wire.len(), 133);
        for packed in [false, true] {
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&value, &options, &wire);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = BTreeMap::<u32, Option<UserObj>>::ace_decode(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded, value);
        }
    }
    #[test]
    fn r_s3_g_r3_rc_option_user_object_some_layout() {
        let value = alloc::rc::Rc::new(Some(UserObj(7)));
        // Option 目标预留 id 1；内层普通对象先完成得 id 2，Option body 为 presence 1 与 id 2。
        let main = [
            target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(1, &[1, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 7u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 69);
        for packed in [false, true] {
            let options = Options {
                max_depth: 0,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded =
                crate::decode::decode_cyclic::<alloc::rc::Rc<Option<UserObj>>>(&mut decoder)
                    .unwrap();
            decoder.finish().unwrap();
            assert_eq!(*decoded, Some(UserObj(7)));
        }
    }
    #[test]
    fn r_s3_g_r3_rc_option_user_object_none_layout() {
        let value = alloc::rc::Rc::new(None::<UserObj>);
        let expected = target_raw(&target_rec(1, &[0]), &target_slot(0, 0, 1u64.to_le_bytes()));
        assert_eq!(expected.len(), 33);
        for packed in [false, true] {
            let options = Options {
                max_depth: 0,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded =
                crate::decode::decode_cyclic::<alloc::rc::Rc<Option<UserObj>>>(&mut decoder)
                    .unwrap();
            decoder.finish().unwrap();
            assert_eq!(*decoded, None);
        }
    }
    #[test]
    fn r_s3_g_r3_rc_option_user_object_weak_first_id_order() {
        let owner = alloc::rc::Rc::new(Some(UserObj(7)));
        let value = WeakFirstHolder {
            weak: alloc::rc::Rc::downgrade(&owner),
            owner: alloc::rc::Rc::clone(&owner),
        };
        // Weak 预留 id 1；普通对象 id 2 先完成；持有者 id 3 最后完成。
        // 持有者为两字段对象，body 为 slot_offset、slot_count（8B），内层普通对象的块先占第 1 槽。
        let main = [
            target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(1, &[1, 2, 0, 0, 0]),
            target_rec(3, &[2, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(2, 1, 7u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 1u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 117);
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = crate::decode::decode_cyclic::<WeakFirstHolder>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            let upgraded = decoded.weak.upgrade().unwrap();
            assert!(alloc::rc::Rc::ptr_eq(&upgraded, &decoded.owner));
            assert_eq!(*decoded.owner, Some(UserObj(7)));
        }
    }
    #[test]
    fn r_s3_g_r3_element_completion_preserves_ordinary_object_roots() {
        // 子例一：普通根对象的根槽与字段槽不变。
        let object = UserObj(7);
        let main = target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]);
        let assist = [
            target_slot(0, 0, 1u64.to_le_bytes()),
            target_slot(1, 1, 7u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 56);
        for packed in [false, true] {
            let options = Options {
                max_depth: 0,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&object, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            assert_eq!(
                crate::decode::decode_cyclic::<UserObj>(&mut decoder),
                Ok(UserObj(7))
            );
            decoder.finish().unwrap();
        }

        // 子例二：child 先完成、父后完成；child 位于父对象内，深度 0 超限。
        // 本子例不进入元素作用域，用于守护既有普通嵌套分支。
        let parent = ParentOfUser { child: UserObj(7) };
        assert_eq!(r3_message(&parent, 0), Err(Error::DepthExceeded));
        let main = [
            target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 2u64.to_le_bytes()),
            target_slot(2, 1, 7u64.to_le_bytes()),
            target_slot(1, 0, 1u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 88);
        for packed in [false, true] {
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&parent, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let root_scalar = decoder.read_root_scalar().unwrap();
            decoder
                .enter_object(u64::from_le_bytes(root_scalar))
                .unwrap();
            let child_scalar = decoder.read_field_scalar(0).unwrap();
            decoder
                .enter_object(u64::from_le_bytes(child_scalar))
                .unwrap();
            decoder.begin_field(1).unwrap();
            assert_eq!(u32::ace_decode(&mut decoder), Ok(7));
            decoder.leave_object().unwrap();
            decoder.leave_object().unwrap();
            decoder.finish().unwrap();
        }
    }
    #[test]
    fn r_s3_g_r2_p1_value_size_hooks() {
        assert_eq!(i8::ace_element_value_size(), Some(1));
        assert_eq!(u8::ace_element_value_size(), Some(1));
        assert_eq!(bool::ace_element_value_size(), Some(1));
        assert_eq!(i16::ace_element_value_size(), Some(2));
        assert_eq!(u16::ace_element_value_size(), Some(2));
        assert_eq!(i32::ace_element_value_size(), Some(4));
        assert_eq!(u32::ace_element_value_size(), Some(4));
        assert_eq!(f32::ace_element_value_size(), Some(4));
        assert_eq!(char::ace_element_value_size(), Some(4));
        assert_eq!(i64::ace_element_value_size(), Some(8));
        assert_eq!(u64::ace_element_value_size(), Some(8));
        assert_eq!(f64::ace_element_value_size(), Some(8));
        assert_eq!(isize::ace_element_value_size(), Some(8));
        assert_eq!(usize::ace_element_value_size(), Some(8));
        assert_eq!(value::IntPtr::ace_element_value_size(), Some(8));
        assert_eq!(value::UIntPtr::ace_element_value_size(), Some(8));
        assert_eq!(value::ToolkitsU64::ace_element_value_size(), Some(8));
        assert_eq!(value::Guid::ace_element_value_size(), Some(16));
        assert_eq!(value::DateTime::ace_element_value_size(), Some(8));
        assert_eq!(value::DateTimeOffset::ace_element_value_size(), Some(10));
        assert_eq!(String::ace_element_value_size(), None);
    }
    #[test]
    fn r_s3_g_r2_p1_bulk_hooks_match_element_bulk() {
        #[cfg(target_endian = "little")]
        {
            let mut out = Vec::new();
            assert_eq!(
                i32::ace_encode_elements_bulk(&[1, 2, 3], &mut out),
                Ok(true)
            );
            assert_eq!(out, [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
            let mut consumed = 0;
            assert_eq!(
                i32::decode_elements_bulk_plan(3, &out, &mut consumed),
                Ok(Some(alloc::vec![1, 2, 3]))
            );
            assert_eq!(consumed, 12);
        }
        #[cfg(not(target_endian = "little"))]
        {
            // Bulk is little-endian only, so i32 declines and takes the per-element path.
            let mut out = Vec::new();
            assert_eq!(
                i32::ace_encode_elements_bulk(&[1, 2, 3], &mut out),
                Ok(false)
            );
            assert!(out.is_empty());
            let mut consumed = 0;
            assert_eq!(
                i32::decode_elements_bulk_plan(3, &[0; 12], &mut consumed),
                Ok(None)
            );
            assert_eq!(consumed, 0);
        }
        let date = value::DateTime {
            ticks: 1,
            kind: value::Kind::Utc,
        };
        let mut untouched = alloc::vec![9];
        assert_eq!(
            value::DateTime::ace_encode_elements_bulk(&[date], &mut untouched),
            Ok(false)
        );
        assert_eq!(untouched, [9]);
        let mut consumed = 7;
        assert_eq!(
            value::DateTime::decode_elements_bulk_plan(1, &[0; 8], &mut consumed),
            Ok(None)
        );
        assert_eq!(consumed, 7);
    }
    #[test]
    fn r_s3_g_r2_p2_fix1_transparent_value_size() {
        assert_eq!(
            <core::cell::RefCell<u32>>::ace_element_value_size(),
            Some(4)
        );
        #[cfg(feature = "std")]
        {
            assert_eq!(<std::sync::RwLock<u32>>::ace_element_value_size(), Some(4));
        }
    }

    use crate::decode::decode_cyclic;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// 读取 raw 中位于 `at` 的 LE `i32` 长度字段；信封与记录头的长度均为非负 `i32`。
    fn le_len(bytes: &[u8], at: usize) -> usize {
        i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
    }
    /// 解析 raw 的 record 与 slot 数（R/S），只读取信封长度，不依赖编码器或解码器内部状态。
    fn raw_counts(raw: &[u8]) -> (usize, usize) {
        let main_len = le_len(raw, 0);
        let mut records = 0;
        let mut offset = 0;
        while offset < main_len {
            offset += 8 + le_len(raw, 4 + offset + 4);
            records += 1;
        }
        let assist_len = le_len(raw, 4 + main_len);
        (records, assist_len / 16)
    }
    /// 去重后的分配地址数（I）：只按真实分配计数，不读取内部身份表。
    fn identity_count<T>(pointers: impl Iterator<Item = *const T>) -> usize {
        let mut pointers: Vec<*const T> = pointers.collect();
        pointers.sort_unstable();
        pointers.dedup();
        pointers.len()
    }
    /// 测试 04 与 08 共用的金样：Weak 先预留 id 1，Vec 记录 id 2 先完成，owner 复用 id 1，持有者 id 3 最后完成。
    fn weak_vec_holder_expected() -> Vec<u8> {
        let main = [
            target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(1, 0, 2u64.to_le_bytes()),
            target_slot(1, 1, 1u64.to_le_bytes()),
        ]
        .concat();
        target_raw(&main, &assist)
    }
    static WEAK_VEC_HOLDER_BEGINS: AtomicUsize = AtomicUsize::new(0);
    /// 测试 04 的最小持有者：字段 0 为 `Vec<Weak<u32>>`，字段 1 为同一目标的强 owner。
    struct WeakVecHolder {
        weaks: Vec<alloc::rc::Weak<u32>>,
        owner: alloc::rc::Rc<u32>,
    }
    impl AceEncode for WeakVecHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.weaks.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for WeakVecHolder {
        type Plan = (Vec<u32>, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                WEAK_VEC_HOLDER_BEGINS.fetch_add(1, Ordering::Relaxed);
                Ok((
                    ctx.field::<Vec<alloc::rc::Weak<u32>>>(0)?,
                    ctx.field::<alloc::rc::Rc<u32>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                weaks: Vec::<alloc::rc::Weak<u32>>::build_from_plan(ctx, plan.0),
                owner: alloc::rc::Rc::<u32>::build_from_plan(ctx, plan.1),
            }
        }
    }
    #[cfg(feature = "std")]
    static ARC_WEAK_VEC_HOLDER_BEGINS: AtomicUsize = AtomicUsize::new(0);
    /// 测试 08 的最小持有者：与 `WeakVecHolder` 同形，指针族换为 `Arc` 与 `sync::Weak`。
    #[cfg(feature = "std")]
    struct ArcWeakVecHolder {
        weaks: Vec<alloc::sync::Weak<u32>>,
        owner: alloc::sync::Arc<u32>,
    }
    #[cfg(feature = "std")]
    impl AceEncode for ArcWeakVecHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.weaks.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    #[cfg(feature = "std")]
    impl AceDecodeCyclic for ArcWeakVecHolder {
        type Plan = (Vec<u32>, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                ARC_WEAK_VEC_HOLDER_BEGINS.fetch_add(1, Ordering::Relaxed);
                Ok((
                    ctx.field::<Vec<alloc::sync::Weak<u32>>>(0)?,
                    ctx.field::<alloc::sync::Arc<u32>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                weaks: Vec::<alloc::sync::Weak<u32>>::build_from_plan(ctx, plan.0),
                owner: alloc::sync::Arc::<u32>::build_from_plan(ctx, plan.1),
            }
        }
    }

    #[test]
    fn r_s3_g_r2_01_vec_rc_shared_and_equal_distinct() {
        let a = alloc::rc::Rc::new(37u32);
        let b = alloc::rc::Rc::new(37u32);
        let value = alloc::vec![alloc::rc::Rc::clone(&a), a, alloc::rc::Rc::clone(&b), b];
        let main = [
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(2, &37u32.to_le_bytes()),
            target_rec(
                3,
                &[4, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0],
            ),
        ]
        .concat();
        let assist = target_slot(0, 0, 3u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 76);
        assert_eq!(raw_counts(&expected), (3, 1));
        for packed in [false, true] {
            // 默认 256 与 max_depth=0 均须成功：根集合进入时 open=0，不超过 max_depth=0。
            for options in [
                packed_opts(packed),
                Options {
                    max_depth: 0,
                    ..packed_opts(packed)
                },
            ] {
                let bytes = r3_encode_layout(&value, &options, &expected);
                let mut decoder = Decoder::new(&bytes, &options).unwrap();
                let decoded = decode_cyclic::<Vec<alloc::rc::Rc<u32>>>(&mut decoder).unwrap();
                decoder.finish().unwrap();
                assert!(decoded.iter().all(|item| **item == 37));
                assert!(alloc::rc::Rc::ptr_eq(&decoded[0], &decoded[1]));
                assert!(alloc::rc::Rc::ptr_eq(&decoded[2], &decoded[3]));
                assert!(!alloc::rc::Rc::ptr_eq(&decoded[0], &decoded[2]));
                assert_eq!(identity_count(decoded.iter().map(alloc::rc::Rc::as_ptr)), 2);
            }
        }
    }
    #[test]
    fn r_s3_g_r2_02_box_rc() {
        let value = alloc::boxed::Box::new(alloc::rc::Rc::new(37u32));
        let main = [
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(2, &1u32.to_le_bytes()),
        ]
        .concat();
        let assist = target_slot(0, 0, 2u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 48);
        assert_eq!(raw_counts(&expected), (2, 1));
        for packed in [false, true] {
            // 根 Box 进入一层集合（open=0），不超过 max_depth=0。
            for options in [
                packed_opts(packed),
                Options {
                    max_depth: 0,
                    ..packed_opts(packed)
                },
            ] {
                let bytes = r3_encode_layout(&value, &options, &expected);
                let mut decoder = Decoder::new(&bytes, &options).unwrap();
                let decoded =
                    decode_cyclic::<alloc::boxed::Box<alloc::rc::Rc<u32>>>(&mut decoder).unwrap();
                decoder.finish().unwrap();
                assert_eq!(**decoded, 37);
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                    1
                );
            }
        }
    }
    #[test]
    fn r_s3_g_r2_03_vec_option_rc() {
        let a = alloc::rc::Rc::new(37u32);
        let value = alloc::vec![Some(alloc::rc::Rc::clone(&a)), None, Some(a)];
        let main = [
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(2, &[3, 0, 0, 0, 1, 1, 0, 0, 0, 0, 1, 1, 0, 0, 0]),
        ]
        .concat();
        let assist = target_slot(0, 0, 2u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 59);
        assert_eq!(raw_counts(&expected), (2, 1));
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<Vec<Option<alloc::rc::Rc<u32>>>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded.len(), 3);
            assert!(decoded[1].is_none());
            let first = decoded[0].as_ref().unwrap();
            let third = decoded[2].as_ref().unwrap();
            assert_eq!(**first, 37);
            assert!(alloc::rc::Rc::ptr_eq(first, third));
            assert_eq!(
                identity_count(decoded.iter().flatten().map(alloc::rc::Rc::as_ptr)),
                1
            );
        }
    }
    #[test]
    fn r_s3_g_r2_04_vec_rc_weak_live_reserved() {
        let owner = alloc::rc::Rc::new(37u32);
        let value = WeakVecHolder {
            weaks: alloc::vec![alloc::rc::Rc::downgrade(&owner)],
            owner: alloc::rc::Rc::clone(&owner),
        };
        let expected = weak_vec_holder_expected();
        assert_eq!(expected.len(), 100);
        assert_eq!(raw_counts(&expected), (3, 3));
        for packed in [false, true] {
            // 字段内的 Vec 位于深度 1，编码与解码都需要 max_depth 至少为 1。
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&value, &options, &expected);
            WEAK_VEC_HOLDER_BEGINS.store(0, Ordering::Relaxed);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<WeakVecHolder>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(WEAK_VEC_HOLDER_BEGINS.load(Ordering::Relaxed), 1);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&decoded.owner)].into_iter()),
                1
            );
            assert_eq!(*decoded.owner, 37);
            assert_eq!(decoded.weaks.len(), 1);
            let upgraded = decoded.weaks[0].upgrade().unwrap();
            assert!(alloc::rc::Rc::ptr_eq(&upgraded, &decoded.owner));
        }
    }
    #[test]
    fn r_s3_g_r2_05_vec_rc_weak_expired() {
        // I=0 为结构推导，非直接计数（未新增编码计数器）。
        // 过期 Weak 的 ace_encode 与 Vec 元素路径 ace_encode_element（本文件 Weak<T> 实现）
        // 先判 upgrade().is_none() 并返回 InvalidArgument，不会再调用 resolve_weak（登记身份处），
        // 因此不产生身份记录。
        let value = alloc::vec![alloc::rc::Weak::<u32>::new()];
        for packed in [false, true] {
            let mut encoder = Encoder::new(&packed_opts(packed));
            assert_eq!(value.ace_encode(&mut encoder), Err(Error::InvalidArgument));
        }
    }
    #[test]
    fn r_s3_g_r2_06_vec_option_rc_weak_expired_none() {
        let value = alloc::vec![Some(alloc::rc::Weak::<u32>::new())];
        let main = target_rec(1, &[1, 0, 0, 0, 0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 37);
        assert_eq!(raw_counts(&expected), (1, 1));
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<Vec<Option<alloc::rc::Weak<u32>>>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded.len(), 1);
            assert!(decoded[0].is_none());
        }
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_r2_07_vec_arc_shared_std() {
        let a = alloc::sync::Arc::new(37u32);
        let value = alloc::vec![alloc::sync::Arc::clone(&a), a];
        let main = [
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]),
        ]
        .concat();
        let assist = target_slot(0, 0, 2u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 56);
        assert_eq!(raw_counts(&expected), (2, 1));
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<Vec<alloc::sync::Arc<u32>>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert!(decoded.iter().all(|item| **item == 37));
            assert!(alloc::sync::Arc::ptr_eq(&decoded[0], &decoded[1]));
            assert_eq!(
                identity_count(decoded.iter().map(alloc::sync::Arc::as_ptr)),
                1
            );
        }
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_r2_08_vec_sync_weak_live_std() {
        let owner = alloc::sync::Arc::new(37u32);
        let value = ArcWeakVecHolder {
            weaks: alloc::vec![alloc::sync::Arc::downgrade(&owner)],
            owner: alloc::sync::Arc::clone(&owner),
        };
        // 指针族不改变 wire：与测试 04 共用同一金样。
        let expected = weak_vec_holder_expected();
        assert_eq!(expected.len(), 100);
        assert_eq!(raw_counts(&expected), (3, 3));
        for packed in [false, true] {
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&value, &options, &expected);
            ARC_WEAK_VEC_HOLDER_BEGINS.store(0, Ordering::Relaxed);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<ArcWeakVecHolder>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(ARC_WEAK_VEC_HOLDER_BEGINS.load(Ordering::Relaxed), 1);
            assert_eq!(*decoded.owner, 37);
            assert_eq!(decoded.weaks.len(), 1);
            let upgraded = decoded.weaks[0].upgrade().unwrap();
            assert_eq!(
                identity_count(
                    [
                        alloc::sync::Arc::as_ptr(&decoded.owner),
                        alloc::sync::Arc::as_ptr(&upgraded),
                    ]
                    .into_iter()
                ),
                1
            );
            assert!(alloc::sync::Arc::ptr_eq(&upgraded, &decoded.owner));
        }
    }
    #[test]
    fn r_s3_g_r2_09_vec_u32_bulk_fast_path() {
        let value = alloc::vec![1u32, 2, 3];
        let main = target_rec(1, &[3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
        let assist = target_slot(0, 0, 1u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 48);
        assert_eq!(raw_counts(&expected), (1, 1));
        for packed in [false, true] {
            let options = packed_opts(packed);
            crate::hooks::reset();
            let bytes = r3_encode_layout(&value, &options, &expected);
            #[cfg(target_endian = "little")]
            {
                // 小端：编码走一次 bulk，不逐元素调用。
                assert_eq!(crate::hooks::snapshot(), (1, 0));
            }
            #[cfg(not(target_endian = "little"))]
            {
                assert_eq!(crate::hooks::snapshot(), (0, 3));
            }
            crate::hooks::reset();
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<Vec<u32>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded, value);
            #[cfg(target_endian = "little")]
            {
                // 小端：图解码同样只走一次 bulk。
                assert_eq!(crate::hooks::snapshot(), (1, 0));
            }
            #[cfg(not(target_endian = "little"))]
            {
                assert_eq!(crate::hooks::snapshot(), (0, 3));
            }
        }
    }

    /// 按 raw 与 packed、默认与 `max_depth=0` 各编码并读回；每种模式的 raw 字节都须等于金样。
    fn decode_cyclic_each_mode<T: AceEncode + AceDecodeCyclic>(v: &T, expected: &[u8]) -> Vec<T> {
        let mut decoded = Vec::new();
        for packed in [false, true] {
            for options in [
                packed_opts(packed),
                Options {
                    max_depth: 0,
                    ..packed_opts(packed)
                },
            ] {
                let bytes = r3_encode_layout(v, &options, expected);
                let mut decoder = Decoder::new(&bytes, &options).unwrap();
                decoded.push(decode_cyclic::<T>(&mut decoder).unwrap());
                decoder.finish().unwrap();
            }
        }
        decoded
    }
    #[test]
    fn r_s3_g_r2_13_option_vec_and_option_box_graph_elements() {
        fn assert_non_optional<T: NonOptional>() {}
        // Rc<u32> 不是 AceElement；Vec 与 Box 的 NonOptional 标记不要求元素为 AceElement。
        assert_non_optional::<Vec<alloc::rc::Rc<u32>>>();
        assert_non_optional::<Box<alloc::rc::Rc<u32>>>();
        // None 只有空信封；Some(空) 是一条 count=0 的记录，二者 raw 不同。
        let none_raw = target_raw(&[], &[]);
        let empty_raw = target_raw(
            &target_rec(1, &[0, 0, 0, 0]),
            &target_slot(0, 0, 1u64.to_le_bytes()),
        );
        assert_ne!(none_raw, empty_raw);
        assert_eq!(none_raw.len(), 8);
        assert_eq!(empty_raw.len(), 36);
        let shared_raw = target_raw(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]),
            ]
            .concat(),
            &target_slot(0, 0, 2u64.to_le_bytes()),
        );
        assert_eq!(shared_raw.len(), 56);
        // Option<Box<..>> 的 Some 与 Box 根同线格式（测试 02）。
        let boxed_raw = target_raw(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &1u32.to_le_bytes()),
            ]
            .concat(),
            &target_slot(0, 0, 2u64.to_le_bytes()),
        );
        assert_eq!(boxed_raw.len(), 48);

        let none_vec = None::<Vec<alloc::rc::Rc<u32>>>;
        for decoded in decode_cyclic_each_mode(&none_vec, &none_raw) {
            assert!(decoded.is_none());
        }
        let empty_vec = Some(Vec::<alloc::rc::Rc<u32>>::new());
        for decoded in decode_cyclic_each_mode(&empty_vec, &empty_raw) {
            assert_eq!(decoded.map(|items| items.len()), Some(0));
        }
        let shared = alloc::rc::Rc::new(37u32);
        let shared_vec = Some(alloc::vec![alloc::rc::Rc::clone(&shared), shared]);
        for decoded in decode_cyclic_each_mode(&shared_vec, &shared_raw) {
            let items = decoded.unwrap();
            assert!(items.iter().all(|item| **item == 37));
            assert!(alloc::rc::Rc::ptr_eq(&items[0], &items[1]));
            assert_eq!(identity_count(items.iter().map(alloc::rc::Rc::as_ptr)), 1);
        }
        let none_box = None::<Box<alloc::rc::Rc<u32>>>;
        for decoded in decode_cyclic_each_mode(&none_box, &none_raw) {
            assert!(decoded.is_none());
        }
        let some_box = Some(Box::new(alloc::rc::Rc::new(37u32)));
        for decoded in decode_cyclic_each_mode(&some_box, &boxed_raw) {
            assert_eq!(***decoded.as_ref().unwrap(), 37);
        }
        // 元素位置 Some 的调用链：Option::ace_encode_element → ace_encode_some_element（写 `[1]`）
        // → 容器 ace_encode_element（N01，写容器记录的 u32 id）；容器与目标 37 各为独立记录。
        // 金样：目标 37 为记录 1（首次到达时提交）；内层 Vec 为记录 2（count=2，两元素均为 id 1）；
        // 外层 Vec 为记录 3（count=1，元素为 `[1]` 加 id 2），根槽指向记录 3。
        // 内层 Vec 在 open=1 时进入，max_depth=1 为紧边界。
        let element_shared = alloc::rc::Rc::new(37u32);
        let shared_items = alloc::vec![alloc::rc::Rc::clone(&element_shared), element_shared];
        let element_vec = alloc::vec![Some(shared_items)];
        let element_vec_raw = target_raw(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]),
                target_rec(3, &[1, 0, 0, 0, 1, 2, 0, 0, 0]),
            ]
            .concat(),
            &target_slot(0, 0, 3u64.to_le_bytes()),
        );
        assert_eq!(element_vec_raw.len(), 73);
        for packed in [false, true] {
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&element_vec, &options, &element_vec_raw);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded: Vec<Option<Vec<alloc::rc::Rc<u32>>>> =
                decode_cyclic(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded.len(), 1);
            let items = decoded[0].as_ref().unwrap();
            assert_eq!(items.len(), 2);
            assert!(items.iter().all(|item| **item == 37));
            assert!(alloc::rc::Rc::ptr_eq(&items[0], &items[1]));
        }
        // 元素位置 Some(Box) 的调用链：Option::ace_encode_element → ace_encode_some_element（写 `[1]`）
        // → Box::ace_encode_element → 容器 ace_encode_element（N01，写 Box 记录的 u32 id）。
        // 金样：目标 37 为记录 1；Box 记录为记录 2（体为 id 1）；外层 Vec 为记录 3（元素为 `[1]` 加 id 2）。
        let element_box = alloc::vec![Some(Box::new(alloc::rc::Rc::new(37u32)))];
        let element_box_raw = target_raw(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &1u32.to_le_bytes()),
                target_rec(3, &[1, 0, 0, 0, 1, 2, 0, 0, 0]),
            ]
            .concat(),
            &target_slot(0, 0, 3u64.to_le_bytes()),
        );
        assert_eq!(element_box_raw.len(), 65);
        for packed in [false, true] {
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&element_box, &options, &element_box_raw);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded: Vec<Option<Box<alloc::rc::Rc<u32>>>> =
                decode_cyclic(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded.len(), 1);
            assert_eq!(***decoded[0].as_ref().unwrap(), 37);
        }
    }
    /// 测试 13 的 std 持有者：字段 0 为 `Option<Vec<sync::Weak<u32>>>`，字段 1 为同一目标的强 owner。
    #[cfg(feature = "std")]
    struct ArcOptionWeakVecHolder {
        weaks: Option<Vec<alloc::sync::Weak<u32>>>,
        owner: alloc::sync::Arc<u32>,
    }
    #[cfg(feature = "std")]
    impl AceEncode for ArcOptionWeakVecHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.weaks.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    #[cfg(feature = "std")]
    impl AceDecodeCyclic for ArcOptionWeakVecHolder {
        type Plan = (Option<Vec<u32>>, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<Option<Vec<alloc::sync::Weak<u32>>>>(0)?,
                    ctx.field::<alloc::sync::Arc<u32>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                weaks: Option::<Vec<alloc::sync::Weak<u32>>>::build_from_plan(ctx, plan.0),
                owner: alloc::sync::Arc::<u32>::build_from_plan(ctx, plan.1),
            }
        }
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_r2_13_std_arc_option_vec() {
        // Some(Vec<Arc>) 与 Vec<Arc> 同线格式，金样同测试 07。
        let a = alloc::sync::Arc::new(37u32);
        let shared_vec = Some(alloc::vec![alloc::sync::Arc::clone(&a), a]);
        let shared_raw = target_raw(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]),
            ]
            .concat(),
            &target_slot(0, 0, 2u64.to_le_bytes()),
        );
        assert_eq!(shared_raw.len(), 56);
        for decoded in decode_cyclic_each_mode(&shared_vec, &shared_raw) {
            let items = decoded.unwrap();
            assert!(items.iter().all(|item| **item == 37));
            assert!(alloc::sync::Arc::ptr_eq(&items[0], &items[1]));
            assert_eq!(
                identity_count(items.iter().map(alloc::sync::Arc::as_ptr)),
                1
            );
        }
        // Some(Box<Arc>) 与 Box<Arc> 同线格式，金样同测试 02 的 Arc 形式。
        let some_box = Some(alloc::boxed::Box::new(alloc::sync::Arc::new(37u32)));
        let boxed_raw = target_raw(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &1u32.to_le_bytes()),
            ]
            .concat(),
            &target_slot(0, 0, 2u64.to_le_bytes()),
        );
        assert_eq!(boxed_raw.len(), 48);
        for decoded in decode_cyclic_each_mode(&some_box, &boxed_raw) {
            assert_eq!(***decoded.as_ref().unwrap(), 37);
        }
        // sync::Weak 共享：Some 不改变 Vec<sync::Weak> 字段的线格式，金样同测试 08。
        let owner = alloc::sync::Arc::new(37u32);
        let holder = ArcOptionWeakVecHolder {
            weaks: Some(alloc::vec![alloc::sync::Arc::downgrade(&owner)]),
            owner: alloc::sync::Arc::clone(&owner),
        };
        let expected = weak_vec_holder_expected();
        assert_eq!(expected.len(), 100);
        for packed in [false, true] {
            // 根对象进入后 open=1；Option 不增加集合层，内部 Vec 恰在 open=1 进入，故 1 为紧边界。
            let options = Options {
                max_depth: 1,
                ..packed_opts(packed)
            };
            let bytes = r3_encode_layout(&holder, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<ArcOptionWeakVecHolder>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(*decoded.owner, 37);
            let weaks = decoded.weaks.as_ref().unwrap();
            assert_eq!(weaks.len(), 1);
            let upgraded = weaks[0].upgrade().unwrap();
            assert!(alloc::sync::Arc::ptr_eq(&upgraded, &decoded.owner));
        }
    }
    #[test]
    fn r_s3_g_r2_14_nonbulk_n01_and_cyclic_delta() {
        // (a) 非 bulk 族的 N01 与旧 AceElement 路径逐字节一致，两条路径都不计 bulk。
        fn assert_n01_matches_element<T: AceEncode + AceElement>(v: &T) {
            crate::hooks::reset();
            let n01 = n01_element_message(v);
            assert_eq!(crate::hooks::snapshot().0, 0);
            crate::hooks::reset();
            let element = ace_element_message(v);
            assert_eq!(crate::hooks::snapshot().0, 0);
            assert!(n01.is_ok(), "N01 encoding failed: {n01:?}");
            assert_eq!(n01, element);
        }
        fn cyclic_roundtrip<T: AceEncode + AceDecodeCyclic + PartialEq + core::fmt::Debug>(v: &T) {
            for packed in [false, true] {
                let bytes = packed_root(v, packed);
                let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
                assert_eq!(&decode_cyclic::<T>(&mut d).unwrap(), v);
                d.finish().unwrap();
            }
        }
        fn root_record_message(body: &[u8]) -> Vec<u8> {
            let mut e = Encoder::new(&opts());
            e.encode_record(body).unwrap();
            e.finish().unwrap()
        }
        fn assert_same_error<T: AceDecode + AceDecodeCyclic>(bytes: &[u8], code: Error) {
            let mut d = Decoder::new(bytes, &opts()).unwrap();
            assert_eq!(T::ace_decode(&mut d).map(|_| ()), Err(code));
            let mut d = Decoder::new(bytes, &opts()).unwrap();
            assert_eq!(decode_cyclic::<T>(&mut d).map(|_| ()), Err(code));
        }
        assert_n01_matches_element(&String::from("ab"));
        assert_n01_matches_element(&value::DateTime {
            ticks: 1,
            kind: value::Kind::Utc,
        });
        assert_n01_matches_element(&value::DateTimeOffset {
            clock_ticks: 1_000_000_000_000,
            offset_minutes: -840,
        });
        assert_n01_matches_element(&Some(String::from("bc")));
        assert_n01_matches_element(&None::<String>);
        assert_n01_matches_element(&alloc::vec![String::from("中文"), String::new()]);
        assert_n01_matches_element(&Box::new(String::from("boxed 中")));
        // presence：None 的元素体只有 [0]，Some 的元素体以 [1] 开头。
        let none = n01_element_message(&None::<String>).unwrap();
        assert_eq!(single_element_body(&none), [0u8]);
        let some = n01_element_message(&Some(String::from("bc"))).unwrap();
        let some_body = single_element_body(&some);
        assert_eq!(some_body.len(), 5);
        assert_eq!(some_body[0], 1);

        // (b) String 的元素体仍是指向独立 UTF-8 记录的 u32 id。
        let text = String::from("中文");
        let bytes = n01_element_message(&text).unwrap();
        let mut d = Decoder::new(&bytes, &opts()).unwrap();
        let wrapper = u32::try_from(u64::from_le_bytes(d.read_root_scalar().unwrap())).unwrap();
        d.begin_record_view(wrapper).unwrap();
        let element = d.read_body_to_end().unwrap();
        d.end_record_view();
        let text_id = u32::from_le_bytes(element.try_into().unwrap());
        assert_ne!(wrapper, text_id);
        d.begin_record_view(text_id).unwrap();
        assert_eq!(d.read_body_to_end().unwrap(), text.as_bytes());
        d.end_record_view();
        d.finish().unwrap();

        // (c) DateTime 与 DateTimeOffset 的 N01 恰好计一次元素调用，不伪造 bulk。
        let date = value::DateTime {
            ticks: 1,
            kind: value::Kind::Utc,
        };
        let offset = value::DateTimeOffset {
            clock_ticks: 1_000_000_000_000,
            offset_minutes: -840,
        };
        crate::hooks::reset();
        n01_element_message(&date).unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 1));
        crate::hooks::reset();
        n01_element_message(&offset).unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 1));
        let mut untouched = alloc::vec![9];
        assert_eq!(
            value::DateTime::ace_encode_elements_bulk(&[date], &mut untouched),
            Ok(false)
        );
        assert_eq!(
            value::DateTimeOffset::ace_encode_elements_bulk(&[offset], &mut untouched),
            Ok(false)
        );
        assert_eq!(untouched, [9]);
        let mut consumed = 7;
        assert_eq!(
            value::DateTime::decode_elements_bulk_plan(1, &[0; 8], &mut consumed),
            Ok(None)
        );
        assert_eq!(
            value::DateTimeOffset::decode_elements_bulk_plan(1, &[0; 10], &mut consumed),
            Ok(None)
        );
        assert_eq!(consumed, 7);

        // (d) decode_cyclic 读回 Plan=Self 纯值、Option presence、嵌套 Vec 与嵌套 Box。
        cyclic_roundtrip(&alloc::vec![String::from("ab"), String::from("\u{4e2d}")]);
        cyclic_roundtrip(&alloc::vec![
            value::DateTime {
                ticks: 0,
                kind: value::Kind::Unspecified,
            },
            date,
        ]);
        cyclic_roundtrip(&alloc::vec![
            value::DateTimeOffset {
                clock_ticks: 0,
                offset_minutes: 0,
            },
            offset,
        ]);
        cyclic_roundtrip(&alloc::vec![
            None,
            Some(String::from("a")),
            Some(String::from("bc")),
            None,
        ]);
        cyclic_roundtrip(&alloc::vec![
            alloc::vec![String::from("中文"), String::new()],
            Vec::new(),
        ]);
        cyclic_roundtrip(&alloc::vec![
            None,
            Some(Box::new(String::new())),
            None,
            Some(Box::new(String::from("x"))),
            None,
        ]);

        // (e) 同一坏包经新入口返回与原入口相同的错误码。
        assert_same_error::<Vec<value::DateTime>>(
            &root_record_message(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xc0]),
            Error::InvalidTag,
        );
        let mut offset_body = 1u32.to_le_bytes().to_vec();
        offset_body.extend_from_slice(&[0; 8]);
        offset_body.extend_from_slice(&841i16.to_le_bytes());
        assert_same_error::<Vec<value::DateTimeOffset>>(
            &root_record_message(&offset_body),
            Error::InvalidTag,
        );
        let mut e = Encoder::new(&opts());
        let text_record = e.alloc_record(&[0xff]).unwrap();
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&text_record.to_le_bytes());
        e.encode_record(&body).unwrap();
        assert_same_error::<Vec<String>>(&e.finish().unwrap(), Error::InvalidUtf8);
        assert_same_error::<Vec<Option<String>>>(
            &root_record_message(&[1, 0, 0, 0, 2]),
            Error::InvalidTag,
        );
        assert_same_error::<Vec<Vec<String>>>(
            &root_record_message(&[1, 0, 0, 0, 99, 0, 0, 0]),
            Error::InvalidReference,
        );
        let mut e = Encoder::new(&opts());
        let box_text = e.alloc_record(b"ab").unwrap();
        let mut box_body = box_text.to_le_bytes().to_vec();
        box_body.push(9);
        let boxed = e.alloc_record(&box_body).unwrap();
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&boxed.to_le_bytes());
        e.encode_record(&body).unwrap();
        assert_same_error::<Vec<Box<String>>>(&e.finish().unwrap(), Error::InvalidTag);
    }

    /// 测试 15 第 3 条的双 trait 元素：只实现 AceElement 与 AceEncode，不覆写 N01，
    /// 因而保留默认 `ace_encode_element` 的 InvalidArgument。
    #[derive(Debug, PartialEq, Eq)]
    struct DualDefault(u32);
    impl AceElement for DualDefault {
        fn element_kind() -> ElementKind {
            ElementKind::Value { size: 4 }
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            crate::hooks::note_element();
            e.write_element_bytes(&self.0.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            crate::hooks::note_element();
            let bytes = d.read_element_bytes(4)?;
            Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
        }
    }
    impl AceEncode for DualDefault {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.encode_scalar(u64::from(self.0).to_le_bytes())
        }
    }

    /// 测试 15 第 4 条的双 trait 元素：显式以 N01 转发 AceElement，使 widened 容器恢复逐项编码。
    #[derive(Debug, PartialEq, Eq)]
    struct DualForwarded(u32);
    impl AceElement for DualForwarded {
        fn element_kind() -> ElementKind {
            ElementKind::Value { size: 4 }
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            crate::hooks::note_element();
            e.write_element_bytes(&self.0.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            crate::hooks::note_element();
            let bytes = d.read_element_bytes(4)?;
            Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
        }
    }
    impl AceEncode for DualForwarded {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.encode_scalar(u64::from(self.0).to_le_bytes())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }

    /// 测试 15 第 4 条的 bulk 探针：覆写三个新方法，见证预检在复制前、双向 bulk 各计一次。
    #[derive(Debug, PartialEq, Eq)]
    struct BulkProbe(u32);
    impl AceEncode for BulkProbe {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.encode_scalar(u64::from(self.0).to_le_bytes())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            crate::hooks::note_element();
            e.write_element_bytes(&self.0.to_le_bytes())
        }
        fn ace_element_value_size() -> Option<usize>
        where
            Self: Sized,
        {
            Some(4)
        }
        fn ace_encode_elements_bulk(items: &[Self], out: &mut Vec<u8>) -> Result<bool>
        where
            Self: Sized,
        {
            for item in items {
                out.extend_from_slice(&item.0.to_le_bytes());
            }
            crate::hooks::note_bulk();
            Ok(true)
        }
    }
    impl AceDecodeCyclic for BulkProbe {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            <u32 as AceDecodeCyclic>::decode_plan(ctx)
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
            Self(<u32 as AceDecodeCyclic>::build_from_plan(ctx, plan))
        }
        fn decode_elements_bulk_plan(
            count: usize,
            bytes: &[u8],
            consumed: &mut usize,
        ) -> Result<Option<Vec<Self::Plan>>>
        where
            Self: Sized,
        {
            let needed = count.checked_mul(4).ok_or(Error::ResourceLimit)?;
            let slice = bytes.get(..needed).ok_or(Error::TruncatedInput)?;
            let mut plans = Vec::with_capacity(count);
            for index in 0..count {
                let start = index * 4;
                let chunk = slice.get(start..start + 4).ok_or(Error::TruncatedInput)?;
                plans.push(u32::from_le_bytes(chunk.try_into().unwrap()));
            }
            *consumed = needed;
            crate::hooks::note_bulk();
            Ok(Some(plans))
        }
    }

    #[test]
    fn r_s3_g_r2_15_dual_default_n01_unsupported() {
        // DualDefault 双 trait 但不覆写 N01：`AceEncode` 的两个静态方法保持默认（宽度未知、拒绝 bulk），
        // 容器只能逐项调用 N01，而默认 N01 直接返回 InvalidArgument。
        assert_eq!(DualDefault::ace_element_value_size(), None);
        let mut untouched = alloc::vec![9];
        assert_eq!(
            DualDefault::ace_encode_elements_bulk(&[DualDefault(1)], &mut untouched),
            Ok(false)
        );
        assert_eq!(untouched, [9]);
        let items = alloc::vec![DualDefault(7), DualDefault(9)];
        let boxed = Box::new(DualDefault(7));
        for packed in [false, true] {
            crate::hooks::reset();
            let mut e = Encoder::new(&packed_opts(packed));
            let error = items.ace_encode(&mut e).unwrap_err();
            assert_eq!(error, Error::InvalidArgument);
            assert_eq!(error.code(), 1);
            // 默认 N01 不写任何元素体：既未走 bulk，也未逐项写出。
            assert_eq!(crate::hooks::snapshot(), (0, 0));
            crate::hooks::reset();
            let mut e = Encoder::new(&packed_opts(packed));
            let error = boxed.ace_encode(&mut e).unwrap_err();
            assert_eq!(error, Error::InvalidArgument);
            assert_eq!(error.code(), 1);
            assert_eq!(crate::hooks::snapshot(), (0, 0));
        }
    }
    #[test]
    fn r_s3_g_r2_15_dual_forwarded_bulk_and_wire() {
        // (a) 显式转发 N01 后，widened Vec/Box 与旧 AceElement 路径逐字节一致。
        let items = alloc::vec![DualForwarded(7), DualForwarded(9)];
        let boxed = Box::new(DualForwarded(7));
        for (n01, element) in [
            (n01_element_message(&items), ace_element_message(&items)),
            (n01_element_message(&boxed), ace_element_message(&boxed)),
        ] {
            assert!(n01.is_ok(), "N01 encoding failed: {n01:?}");
            assert_eq!(n01, element);
        }
        // 两条路径的元素计数一致：两者均由容器元素编码入口（N01 或旧 AceElement）计一次，
        // 再由每个 DualForwarded 的元素 writer 各计一次；元素作用域的 begin/end 本身不计数。
        // 故 len=2 的 Vec 为 1+2=3 次，Box 为 1+1=2 次；均无 bulk。
        crate::hooks::reset();
        n01_element_message(&items).unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 3));
        crate::hooks::reset();
        ace_element_message(&items).unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 3));
        crate::hooks::reset();
        n01_element_message(&boxed).unwrap();
        assert_eq!(crate::hooks::snapshot(), (0, 2));
        // (b) 编码恢复成功：根容器经普通 AceDecode 读回等值（DualForwarded 无 Cyclic）。
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = packed_root(&items, packed);
            let decoded: Vec<DualForwarded> = crate::decode(&bytes, &options).unwrap();
            assert_eq!(decoded, items);
            let bytes = packed_root(&boxed, packed);
            let decoded: Box<DualForwarded> = crate::decode(&bytes, &options).unwrap();
            assert_eq!(decoded, boxed);
        }

        // (c) bulk 探针：预检在复制前拒绝，成功后双向各走一次 bulk、不逐项编码。
        let bulk_items = alloc::vec![BulkProbe(1), BulkProbe(2), BulkProbe(3)];
        for packed in [false, true] {
            crate::hooks::reset();
            let bytes = packed_root(&bulk_items, packed);
            assert_eq!(crate::hooks::snapshot(), (1, 0));
            crate::hooks::reset();
            let mut decoder = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let decoded = decode_cyclic::<Vec<BulkProbe>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(decoded, bulk_items);
            assert_eq!(crate::hooks::snapshot(), (1, 0));
        }
        // 定宽预检：body 长度超预算时在 bulk 复制前返回 ResourceLimit。
        let many: Vec<BulkProbe> = (0..100).map(BulkProbe).collect();
        let tight = Options {
            max_raw_bytes: 64,
            ..packed_opts(false)
        };
        crate::hooks::reset();
        let mut e = Encoder::new(&tight);
        assert_eq!(many.ace_encode(&mut e), Err(Error::ResourceLimit));
        assert_eq!(crate::hooks::snapshot(), (0, 0));
    }

    /// 测试 16 第 2 条的迁移探针：Plan 与 Self 不同，逐项规划后包装。
    #[derive(Debug, PartialEq, Eq)]
    struct PlannedU32(u32);
    impl AceElement for PlannedU32 {
        fn element_kind() -> ElementKind {
            ElementKind::Value { size: 4 }
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            crate::hooks::note_element();
            e.write_element_bytes(&self.0.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            crate::hooks::note_element();
            let bytes = d.read_element_bytes(4)?;
            Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
        }
    }
    impl AceEncode for PlannedU32 {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.encode_scalar(u64::from(self.0).to_le_bytes())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }
    static PLANNED_U32_DECODE_PLANS: AtomicUsize = AtomicUsize::new(0);
    static PLANNED_U32_BUILDS: AtomicUsize = AtomicUsize::new(0);
    impl AceDecodeCyclic for PlannedU32 {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            PLANNED_U32_DECODE_PLANS.fetch_add(1, Ordering::Relaxed);
            u32::decode_plan(ctx)
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            PLANNED_U32_BUILDS.fetch_add(1, Ordering::Relaxed);
            Self(u32::build_from_plan(ctx, plan))
        }
    }

    /// 测试 16 第 4 条的类型级夹具：携带短生命周期的值，用于证明 `AceDecodeCyclic`
    /// 自身未全局要求 `'static`（只在共享目标处由 Rc/Arc 的 impl 追加）。
    struct NonStaticValue<'a>(core::marker::PhantomData<&'a ()>);
    impl<'a> AceDecodeCyclic for NonStaticValue<'a> {
        type Plan = ();
        fn decode_plan(_ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            Ok(())
        }
        fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, _plan: Self::Plan) -> Self {
            Self(core::marker::PhantomData)
        }
    }

    #[test]
    fn r_s3_g_r2_16_cyclic_plan_migration_probe() {
        // (a) Plan=u32：Rc<Vec<PlannedU32>> 逐项 decode_plan 后逐项 build_from_plan。
        // `Rc<Vec<_>>` 不是可编码根（Rc 的身份捕获要求内部走 ace_encode_target 提交对象记录），
        // 故直接构造共享目标金样：唯一强目标记录 1 为 count=3 后接三个 4B 元素体，根槽指向记录 1。
        let items = alloc::vec![PlannedU32(7), PlannedU32(9), PlannedU32(11)];
        let main = target_rec(1, &[3, 0, 0, 0, 7, 0, 0, 0, 9, 0, 0, 0, 11, 0, 0, 0]);
        let raw = target_raw(&main, &target_slot(0, 0, 1u64.to_le_bytes()));
        assert_eq!(raw.len(), 48);
        for packed in [false, true] {
            let bytes = match packed {
                true => crate::wire::packed::pack(&raw, &packed_opts(true)).unwrap(),
                false => raw.clone(),
            };
            PLANNED_U32_DECODE_PLANS.store(0, Ordering::Relaxed);
            PLANNED_U32_BUILDS.store(0, Ordering::Relaxed);
            crate::hooks::reset();
            let mut decoder = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            let decoded = decode_cyclic::<alloc::rc::Rc<Vec<PlannedU32>>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(*decoded, items);
            assert_eq!(PLANNED_U32_DECODE_PLANS.load(Ordering::Relaxed), 3);
            assert_eq!(PLANNED_U32_BUILDS.load(Ordering::Relaxed), 3);
            // PlannedU32 未覆写 bulk：解码全程未走整段路径。
            assert_eq!(crate::hooks::snapshot().0, 0);
        }
        // 旧 Vec<Self> bulk 不会被误用为 Vec<Self::Plan>：默认解码 bulk 明确拒绝。
        let mut consumed = 7;
        assert_eq!(
            PlannedU32::decode_elements_bulk_plan(2, &[0; 8], &mut consumed),
            Ok(None)
        );
        assert_eq!(consumed, 7);

        // (b) 静态检查：共享目标处登记 S: 'static 与 S::Plan: 'static 的要求。
        fn assert_shared_target_bounds<T: AceDecodeCyclic + 'static>()
        where
            T::Plan: 'static,
        {
            fn assert_cyclic<U: AceDecodeCyclic>() {}
            assert_cyclic::<alloc::rc::Rc<Vec<T>>>();
            assert_cyclic::<alloc::rc::Rc<Box<T>>>();
        }
        assert_shared_target_bounds::<PlannedU32>();
        // 非共享的纯值容器不追加 'static：短生命周期元素仍满足 AceDecodeCyclic。
        fn accepts_cyclic<T: AceDecodeCyclic>() {}
        fn accepts_short_lived<'a>(_guard: &'a u32) {
            accepts_cyclic::<Vec<NonStaticValue<'a>>>();
            accepts_cyclic::<Box<NonStaticValue<'a>>>();
        }
        let guard = 0u32;
        accepts_short_lived(&guard);
    }

    /// S3-G R1 夹具：同时实现 `AceElement` 与 `AceEncode`，但不覆写 N01，N01 保持默认的错误 1。
    struct LegacyBoth(u32);
    impl AceElement for LegacyBoth {
        fn element_kind() -> ElementKind {
            ElementKind::Value { size: 4 }
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.write_element_bytes(&self.0.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            let bytes = d.read_element_bytes(4)?;
            Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
        }
    }
    impl AceEncode for LegacyBoth {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.encode_scalar(u64::from(self.0).to_le_bytes())
        }
    }
    /// 与 `LegacyBoth` 相同，但按附录 B 显式把 N01 转发到 `AceElement::encode_element`。
    struct AdaptedBoth(u32);
    impl AceElement for AdaptedBoth {
        fn element_kind() -> ElementKind {
            ElementKind::Value { size: 4 }
        }
        fn encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.write_element_bytes(&self.0.to_le_bytes())
        }
        fn decode_element(d: &mut Decoder<'_>) -> Result<Self> {
            let bytes = d.read_element_bytes(4)?;
            Ok(Self(u32::from_le_bytes(bytes.try_into().unwrap())))
        }
    }
    impl AceEncode for AdaptedBoth {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.encode_scalar(u64::from(self.0).to_le_bytes())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            <Self as AceElement>::encode_element(self, e)
        }
    }

    #[test]
    fn r_s3_g_r1_custom_element_encode_requires_n01() {
        // 未覆写 N01 的值：映射编码须返回错误 1，不得经旧元素路径成功。
        assert_eq!(
            target_message(&BTreeMap::from([(7u32, LegacyBoth(37))])),
            Err(Error::InvalidArgument)
        );
        // 附录 B 的显式 N01 转发后，同一输入的 raw 与旧元素路径金样一致（映射体 12B，raw 44B）。
        let expected = target_raw(
            &target_rec(1, &[1, 0, 0, 0, 7, 0, 0, 0, 37, 0, 0, 0]),
            &target_slot(0, 0, 1u64.to_le_bytes()),
        );
        assert_eq!(expected.len(), 44);
        for packed in [false, true] {
            r3_encode_layout(
                &BTreeMap::from([(7u32, AdaptedBoth(37))]),
                &packed_opts(packed),
                &expected,
            );
        }
    }

    /// 编码侧 B 计数：持有者成功进入对象并开始字段 0 时加一（解码读回不计）。
    static WEAK_MAP_HOLDER_BEGINS: AtomicUsize = AtomicUsize::new(0);
    /// S3-G R1 的最小持有者：字段 0 为 `BTreeMap<u32, Weak<u32>>`，字段 1 为同一目标的强 owner。
    struct WeakMapHolder {
        map: BTreeMap<u32, alloc::rc::Weak<u32>>,
        owner: alloc::rc::Rc<u32>,
    }
    impl AceEncode for WeakMapHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            WEAK_MAP_HOLDER_BEGINS.fetch_add(1, Ordering::Relaxed);
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.map.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for WeakMapHolder {
        type Plan = (BTreeMap<u32, u32>, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<BTreeMap<u32, alloc::rc::Weak<u32>>>(0)?,
                    ctx.field::<alloc::rc::Rc<u32>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                map: BTreeMap::<u32, alloc::rc::Weak<u32>>::build_from_plan(ctx, plan.0),
                owner: alloc::rc::Rc::<u32>::build_from_plan(ctx, plan.1),
            }
        }
    }

    #[test]
    fn r_s3_g_r1_rc_shared_and_distinct_values() {
        // 共享：按键序先见 7 的强值并预留目标 id 1，9 复用 id 1；Map 提交 id 2，根 id 为 2。
        let shared = alloc::rc::Rc::new(37u32);
        let value = BTreeMap::from([(9u32, alloc::rc::Rc::clone(&shared)), (7u32, shared)]);
        let main = [
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(
                2,
                &[2, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0, 1, 0, 0, 0],
            ),
        ]
        .concat();
        let assist = target_slot(0, 0, 2u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 64);
        assert_eq!(raw_counts(&expected), (2, 1));
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded: BTreeMap<u32, alloc::rc::Rc<u32>> = decode_cyclic(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(*decoded[&7], 37);
            assert!(alloc::rc::Rc::ptr_eq(&decoded[&7], &decoded[&9]));
            assert_eq!(
                identity_count(decoded.values().map(alloc::rc::Rc::as_ptr)),
                1
            );
        }
        // 等值不同实例：7 预留并提交目标 id 1，9 提交目标 id 2，Map 提交 id 3，根 id 为 3。
        let first = alloc::rc::Rc::new(37u32);
        let second = alloc::rc::Rc::new(37u32);
        let distinct = BTreeMap::from([(9u32, second), (7u32, first)]);
        let main = [
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(2, &37u32.to_le_bytes()),
            target_rec(
                3,
                &[2, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0, 2, 0, 0, 0],
            ),
        ]
        .concat();
        let assist = target_slot(0, 0, 3u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 76);
        assert_eq!(raw_counts(&expected), (3, 1));
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = r3_encode_layout(&distinct, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded: BTreeMap<u32, alloc::rc::Rc<u32>> = decode_cyclic(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert!(!alloc::rc::Rc::ptr_eq(&decoded[&7], &decoded[&9]));
            assert_eq!(
                identity_count(decoded.values().map(alloc::rc::Rc::as_ptr)),
                2
            );
        }
    }

    #[test]
    fn r_s3_g_r1_weak_value_reservation_and_expiry() {
        let owner = alloc::rc::Rc::new(37u32);
        let value = WeakMapHolder {
            map: BTreeMap::from([(7u32, alloc::rc::Rc::downgrade(&owner))]),
            owner: alloc::rc::Rc::clone(&owner),
        };
        // Weak 预留 1，Map 提交 2，强目标提交 1，持有者提交 3；持有者字段体只开始一次。
        let main = [
            target_rec(2, &[1, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(1, 0, 2u64.to_le_bytes()),
            target_slot(1, 1, 1u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 104);
        assert_eq!(raw_counts(&expected), (3, 3));
        for packed in [false, true] {
            let options = packed_opts(packed);
            WEAK_MAP_HOLDER_BEGINS.store(0, Ordering::Relaxed);
            let bytes = r3_encode_layout(&value, &options, &expected);
            assert_eq!(WEAK_MAP_HOLDER_BEGINS.load(Ordering::Relaxed), 1);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<WeakMapHolder>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(*decoded.owner, 37);
            let owner_address = alloc::rc::Rc::as_ptr(&decoded.owner);
            let weak_address = alloc::rc::Weak::as_ptr(&decoded.map[&7]);
            assert_eq!(identity_count([weak_address, owner_address].into_iter()), 1);
            let weak = decoded.map[&7].clone();
            let upgraded = weak.upgrade().unwrap();
            assert!(alloc::rc::Rc::ptr_eq(&upgraded, &decoded.owner));
            drop(upgraded);
            drop(decoded);
            assert!(weak.upgrade().is_none());
        }
        // 无持有者的失败用例：B 保持 0；I 为结构推导（未新增编码计数器），不直接计数。
        WEAK_MAP_HOLDER_BEGINS.store(0, Ordering::Relaxed);
        // 根 Map 持过期 Weak：编码返回错误 1，finish 不交付 raw。
        let expired = BTreeMap::from([(7u32, alloc::rc::Weak::<u32>::new())]);
        for packed in [false, true] {
            let options = packed_opts(packed);
            let mut encoder = Encoder::new(&options);
            assert_eq!(
                expired.ace_encode(&mut encoder),
                Err(Error::InvalidArgument)
            );
            assert_eq!(encoder.finish(), Err(Error::InvalidArgument));
        }
        // 根 Map 持存活 Weak、强目标仅在消息外：编码成功，finish 因悬空预留返回错误 1。
        let outside = alloc::rc::Rc::new(37u32);
        let live = BTreeMap::from([(7u32, alloc::rc::Rc::downgrade(&outside))]);
        for packed in [false, true] {
            let options = packed_opts(packed);
            let mut encoder = Encoder::new(&options);
            assert_eq!(live.ace_encode(&mut encoder), Ok(()));
            assert_eq!(encoder.finish(), Err(Error::InvalidArgument));
        }
        assert_eq!(WEAK_MAP_HOLDER_BEGINS.load(Ordering::Relaxed), 0);
    }

    /// std 版持有者：指针族换为 `Arc` 与 `sync::Weak`，计数器独立于 Rc 版。
    #[cfg(feature = "std")]
    static ARC_WEAK_MAP_HOLDER_BEGINS: AtomicUsize = AtomicUsize::new(0);
    #[cfg(feature = "std")]
    struct ArcWeakMapHolder {
        map: BTreeMap<u32, alloc::sync::Weak<u32>>,
        owner: alloc::sync::Arc<u32>,
    }
    #[cfg(feature = "std")]
    impl AceEncode for ArcWeakMapHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            ARC_WEAK_MAP_HOLDER_BEGINS.fetch_add(1, Ordering::Relaxed);
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.map.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    #[cfg(feature = "std")]
    impl AceDecodeCyclic for ArcWeakMapHolder {
        type Plan = (BTreeMap<u32, u32>, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<BTreeMap<u32, alloc::sync::Weak<u32>>>(0)?,
                    ctx.field::<alloc::sync::Arc<u32>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                map: BTreeMap::<u32, alloc::sync::Weak<u32>>::build_from_plan(ctx, plan.0),
                owner: alloc::sync::Arc::<u32>::build_from_plan(ctx, plan.1),
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_r1_arc_and_sync_weak_values() {
        // 共享：按键序先见 7 的强值并预留目标 id 1，9 复用 id 1；Map 提交 id 2，根 id 为 2。
        let shared = alloc::sync::Arc::new(37u32);
        let value = BTreeMap::from([(9u32, alloc::sync::Arc::clone(&shared)), (7u32, shared)]);
        let main = [
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(
                2,
                &[2, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0, 1, 0, 0, 0],
            ),
        ]
        .concat();
        let assist = target_slot(0, 0, 2u64.to_le_bytes());
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 64);
        assert_eq!(raw_counts(&expected), (2, 1));
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = r3_encode_layout(&value, &options, &expected);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded: BTreeMap<u32, alloc::sync::Arc<u32>> =
                decode_cyclic(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert!(alloc::sync::Arc::ptr_eq(&decoded[&7], &decoded[&9]));
            assert_eq!(
                identity_count(decoded.values().map(alloc::sync::Arc::as_ptr)),
                1
            );
        }
        // sync::Weak 持有者：Weak 预留 1，Map 提交 2，Arc 强目标提交 1，持有者提交 3；B=1。
        let owner = alloc::sync::Arc::new(37u32);
        let holder = ArcWeakMapHolder {
            map: BTreeMap::from([(7u32, alloc::sync::Arc::downgrade(&owner))]),
            owner: alloc::sync::Arc::clone(&owner),
        };
        let main = [
            target_rec(2, &[1, 0, 0, 0, 7, 0, 0, 0, 1, 0, 0, 0]),
            target_rec(1, &37u32.to_le_bytes()),
            target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
        ]
        .concat();
        let assist = [
            target_slot(0, 0, 3u64.to_le_bytes()),
            target_slot(1, 0, 2u64.to_le_bytes()),
            target_slot(1, 1, 1u64.to_le_bytes()),
        ]
        .concat();
        let expected = target_raw(&main, &assist);
        assert_eq!(expected.len(), 104);
        assert_eq!(raw_counts(&expected), (3, 3));
        for packed in [false, true] {
            let options = packed_opts(packed);
            ARC_WEAK_MAP_HOLDER_BEGINS.store(0, Ordering::Relaxed);
            let bytes = r3_encode_layout(&holder, &options, &expected);
            assert_eq!(ARC_WEAK_MAP_HOLDER_BEGINS.load(Ordering::Relaxed), 1);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<ArcWeakMapHolder>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert_eq!(*decoded.owner, 37);
            let owner_address = alloc::sync::Arc::as_ptr(&decoded.owner);
            let weak_address = alloc::sync::Weak::as_ptr(&decoded.map[&7]);
            assert_eq!(identity_count([weak_address, owner_address].into_iter()), 1);
            let upgraded = decoded.map[&7].upgrade().unwrap();
            assert!(alloc::sync::Arc::ptr_eq(&upgraded, &decoded.owner));
        }
    }

    #[test]
    fn r_s3_g_07_identity_budgets() {
        // 身份见证：计数 CountedTarget 身份节点的构造次数，而非普通根的组装。
        struct CountedTarget {
            value: u32,
        }
        static TARGET_BUILDS: AtomicUsize = AtomicUsize::new(0);
        impl AceEncode for CountedTarget {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                let encoded = e.begin_field(0).and_then(|()| self.value.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceDecodeCyclic for CountedTarget {
            type Plan = u32;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| ctx.field::<u32>(0))
            }
            fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                TARGET_BUILDS.fetch_add(1, Ordering::Relaxed);
                Self { value: plan }
            }
        }
        // 固定结构：Weak 先预留身份，两个强字段随后复用同一分配；身份条目数 U 为 1。
        struct Shared {
            weak: alloc::rc::Weak<CountedTarget>,
            first: alloc::rc::Rc<CountedTarget>,
            second: alloc::rc::Rc<CountedTarget>,
        }
        static SHARED_BUILDS: AtomicUsize = AtomicUsize::new(0);
        impl AceEncode for Shared {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                let encoded = e
                    .begin_field(0)
                    .and_then(|()| self.weak.ace_encode(e))
                    .and_then(|()| e.begin_field(1))
                    .and_then(|()| self.first.ace_encode(e))
                    .and_then(|()| e.begin_field(2))
                    .and_then(|()| self.second.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceDecodeCyclic for Shared {
            type Plan = (u32, u32, u32);
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| {
                    Ok((
                        ctx.field::<alloc::rc::Weak<CountedTarget>>(0)?,
                        ctx.field::<alloc::rc::Rc<CountedTarget>>(1)?,
                        ctx.field::<alloc::rc::Rc<CountedTarget>>(2)?,
                    ))
                })
            }
            fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                SHARED_BUILDS.fetch_add(1, Ordering::Relaxed);
                Self {
                    weak: alloc::rc::Weak::<CountedTarget>::build_from_plan(ctx, plan.0),
                    first: alloc::rc::Rc::<CountedTarget>::build_from_plan(ctx, plan.1),
                    second: alloc::rc::Rc::<CountedTarget>::build_from_plan(ctx, plan.2),
                }
            }
        }
        // 普通所有权：同一 Box 记录被两个字段引用，每次物化各计一次 record.size（FR-E05，不按 id 去重）。
        struct OwnedPair {
            first: Box<u32>,
            second: Box<u32>,
        }
        // 仅见证 OwnedPair 根的组装次数，不见证身份计划阶段。
        static OWNED_BUILDS: AtomicUsize = AtomicUsize::new(0);
        impl AceDecodeCyclic for OwnedPair {
            type Plan = (Box<u32>, Box<u32>);
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| Ok((ctx.field::<Box<u32>>(0)?, ctx.field::<Box<u32>>(1)?)))
            }
            fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                OWNED_BUILDS.fetch_add(1, Ordering::Relaxed);
                Self {
                    first: Box::<u32>::build_from_plan(ctx, plan.0),
                    second: Box::<u32>::build_from_plan(ctx, plan.1),
                }
            }
        }
        fn encode_shared(value: &Shared, options: &Options) -> Result<Vec<u8>> {
            let mut e = Encoder::new(options);
            value.ace_encode(&mut e)?;
            e.finish()
        }
        fn decode_shared(bytes: &[u8], options: &Options) -> Result<Shared> {
            let mut decoder = Decoder::new(bytes, options)?;
            let decoded = decode_cyclic::<Shared>(&mut decoder)?;
            decoder.finish()?;
            Ok(decoded)
        }
        fn decode_owned(bytes: &[u8], options: &Options) -> Result<OwnedPair> {
            let mut decoder = Decoder::new(bytes, options)?;
            let decoded = decode_cyclic::<OwnedPair>(&mut decoder)?;
            decoder.finish()?;
            Ok(decoded)
        }
        fn check_shared(decoded: &Shared) {
            assert_eq!(decoded.first.value, 37);
            assert!(alloc::rc::Rc::ptr_eq(&decoded.first, &decoded.second));
            let upgraded = decoded.weak.upgrade().unwrap();
            assert!(alloc::rc::Rc::ptr_eq(&upgraded, &decoded.first));
            let ptrs = [
                alloc::rc::Rc::as_ptr(&decoded.first),
                alloc::rc::Rc::as_ptr(&decoded.second),
                alloc::rc::Weak::as_ptr(&decoded.weak),
            ];
            assert_eq!(identity_count(ptrs.into_iter()), 1);
        }
        // 根对象体 8 字节于进入时计费，两个 Box 记录各 4 字节逐次计费：总计 16。
        fn owned_pair_bytes(packed: bool) -> Vec<u8> {
            let mut e = Encoder::new(&packed_opts(packed));
            e.enter_object().unwrap();
            e.begin_field(0).unwrap();
            e.begin_record().unwrap();
            e.begin_element();
            <u32 as AceElement>::encode_element(&7, &mut e).unwrap();
            e.end_element();
            let id = u64::from(e.end_record().unwrap()).to_le_bytes();
            e.encode_scalar(id).unwrap();
            e.write_field_scalar(1, id).unwrap();
            e.leave_object().unwrap();
            e.finish().unwrap()
        }
        let owner = alloc::rc::Rc::new(CountedTarget { value: 37 });
        let value = Shared {
            weak: alloc::rc::Rc::downgrade(&owner),
            first: alloc::rc::Rc::clone(&owner),
            second: alloc::rc::Rc::clone(&owner),
        };
        let owner_ptr = alloc::rc::Rc::as_ptr(&owner);
        let weak_ptr = alloc::rc::Weak::as_ptr(&value.weak);
        // 身份条目数 U：真实分配去重数；Weak 与两个强字段共用同一分配，只占一个条目。
        assert_eq!(identity_count([owner_ptr, weak_ptr].into_iter()), 1);
        for packed in [false, true] {
            let options = packed_opts(packed);
            let bytes = encode_shared(&value, &options).unwrap();
            check_shared(&decode_shared(&bytes, &options).unwrap());
            // max_references 为 U+1 与 U 时均成功，线上字节与默认选项逐字节相同。
            for max_references in [2, 1] {
                let limited = Options {
                    max_references,
                    ..packed_opts(packed)
                };
                assert_eq!(encode_shared(&value, &limited).unwrap(), bytes);
                check_shared(&decode_shared(&bytes, &limited).unwrap());
            }
            // U−1 = 0：编码与解码均返回 ResourceLimit；失败解码不构造节点，随后新调用成功。
            let zero_references = Options {
                max_references: 0,
                ..packed_opts(packed)
            };
            assert_eq!(
                encode_shared(&value, &zero_references).err(),
                Some(Error::ResourceLimit)
            );
            SHARED_BUILDS.store(0, Ordering::Relaxed);
            TARGET_BUILDS.store(0, Ordering::Relaxed);
            let error = decode_shared(&bytes, &zero_references).err();
            assert_eq!(error, Some(Error::ResourceLimit));
            assert_eq!(error.unwrap().code(), 6);
            assert_eq!(SHARED_BUILDS.load(Ordering::Relaxed), 0);
            assert_eq!(TARGET_BUILDS.load(Ordering::Relaxed), 0);
            TARGET_BUILDS.store(0, Ordering::Relaxed);
            check_shared(&decode_shared(&bytes, &options).unwrap());
            assert_eq!(SHARED_BUILDS.load(Ordering::Relaxed), 1);
            assert_eq!(TARGET_BUILDS.load(Ordering::Relaxed), 1);
            // 普通所有权物化计量取 max_decoded_bytes（limits.rs 文档与 FR-E05）：limit−1 失败，limit 与 limit+1 成功。
            let owned = owned_pair_bytes(packed);
            let with_decoded_limit = |max_decoded_bytes| Options {
                max_decoded_bytes,
                ..packed_opts(packed)
            };
            OWNED_BUILDS.store(0, Ordering::Relaxed);
            let error = decode_owned(&owned, &with_decoded_limit(15)).err();
            assert_eq!(error, Some(Error::ResourceLimit));
            assert_eq!(OWNED_BUILDS.load(Ordering::Relaxed), 0);
            for max_decoded_bytes in [16, 17] {
                let decoded = decode_owned(&owned, &with_decoded_limit(max_decoded_bytes)).unwrap();
                assert_eq!((*decoded.first, *decoded.second), (7, 7));
            }
            assert_eq!(OWNED_BUILDS.load(Ordering::Relaxed), 2);
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_08_mixed_rwlock_roundtrip_std() {
        // Rc 根两次持有同一个 Arc<RwLock<子>>，根上的 Weak 反向边指回根（与 r_s3_e_03 同形）。
        struct MixedRoot {
            first: alloc::sync::Arc<RwLock<MixedChild>>,
            second: alloc::sync::Arc<RwLock<MixedChild>>,
            back: alloc::rc::Weak<MixedRoot>,
            via: alloc::rc::Rc<BackNode>,
        }
        struct MixedChild {
            value: u32,
        }
        // 经 Rc 中间节点回指根（A→B→Weak A）；不放入 Arc，因 rc::Weak 非 Send/Sync。
        struct BackNode {
            to_root: alloc::rc::Weak<MixedRoot>,
        }
        impl AceEncode for MixedRoot {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                let encoded = e
                    .begin_field(0)
                    .and_then(|()| self.first.ace_encode(e))
                    .and_then(|()| e.begin_field(1))
                    .and_then(|()| self.second.ace_encode(e))
                    .and_then(|()| e.begin_field(2))
                    .and_then(|()| self.back.ace_encode(e))
                    .and_then(|()| e.begin_field(3))
                    .and_then(|()| self.via.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceEncode for MixedChild {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                let encoded = e.begin_field(0).and_then(|()| self.value.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceEncode for BackNode {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                let encoded = e.begin_field(0).and_then(|()| self.to_root.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceDecodeCyclic for MixedRoot {
            type Plan = (u32, u32, u32, u32);
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| {
                    Ok((
                        ctx.field::<alloc::sync::Arc<RwLock<MixedChild>>>(0)?,
                        ctx.field::<alloc::sync::Arc<RwLock<MixedChild>>>(1)?,
                        ctx.field::<alloc::rc::Weak<MixedRoot>>(2)?,
                        ctx.field::<alloc::rc::Rc<BackNode>>(3)?,
                    ))
                })
            }
            fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                Self {
                    first: alloc::sync::Arc::<RwLock<MixedChild>>::build_from_plan(ctx, plan.0),
                    second: alloc::sync::Arc::<RwLock<MixedChild>>::build_from_plan(ctx, plan.1),
                    back: alloc::rc::Weak::<MixedRoot>::build_from_plan(ctx, plan.2),
                    via: alloc::rc::Rc::<BackNode>::build_from_plan(ctx, plan.3),
                }
            }
        }
        impl AceDecodeCyclic for MixedChild {
            type Plan = u32;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| ctx.field::<u32>(0))
            }
            fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                Self { value: plan }
            }
        }
        impl AceDecodeCyclic for BackNode {
            type Plan = u32;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| ctx.field::<alloc::rc::Weak<MixedRoot>>(0))
            }
            fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                Self {
                    to_root: alloc::rc::Weak::<MixedRoot>::build_from_plan(ctx, plan),
                }
            }
        }
        // 锁强环：根对象字段 0 强引用自身，解码在计划阶段返回 14，且不构造任何节点。
        struct LockLoop {
            next: Option<alloc::sync::Arc<RwLock<LockLoop>>>,
        }
        static LOCK_LOOP_BUILDS: AtomicUsize = AtomicUsize::new(0);
        impl AceEncode for LockLoop {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                let encoded = e.begin_field(0).and_then(|()| self.next.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceDecodeCyclic for LockLoop {
            type Plan = Option<u32>;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| ctx.field::<Option<alloc::sync::Arc<RwLock<LockLoop>>>>(0))
            }
            fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                LOCK_LOOP_BUILDS.fetch_add(1, Ordering::Relaxed);
                Self {
                    next: Option::<alloc::sync::Arc<RwLock<LockLoop>>>::build_from_plan(ctx, plan),
                }
            }
        }
        // 强环 raw：根槽指向记录 1，记录 1 的字段 0 又以强引用指回记录 1。
        let cycle_raw = target_raw(
            &target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            &[
                target_slot(0, 0, 1u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
            ]
            .concat(),
        );
        for packed in [false, true] {
            let options = packed_opts(packed);
            let root = alloc::rc::Rc::new_cyclic(|weak| {
                let child = alloc::sync::Arc::new(RwLock::new(MixedChild { value: 37 }));
                MixedRoot {
                    first: alloc::sync::Arc::clone(&child),
                    second: child,
                    back: alloc::rc::Weak::clone(weak),
                    via: alloc::rc::Rc::new(BackNode {
                        to_root: alloc::rc::Weak::clone(weak),
                    }),
                }
            });
            let mut e = Encoder::new(&options);
            root.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let decoded = decode_cyclic::<alloc::rc::Rc<MixedRoot>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert!(alloc::sync::Arc::ptr_eq(&decoded.first, &decoded.second));
            let child = alloc::sync::Arc::clone(&decoded.first);
            assert_eq!(child.read().unwrap().value, 37);
            let back = decoded.back.clone();
            let via_back = alloc::rc::Weak::clone(&decoded.via.to_root);
            assert!(back
                .upgrade()
                .is_some_and(|upgraded| alloc::rc::Rc::ptr_eq(&upgraded, &decoded)));
            assert!(via_back
                .upgrade()
                .is_some_and(|upgraded| alloc::rc::Rc::ptr_eq(&upgraded, &decoded)));
            drop((decoded, child));
            assert!(back.upgrade().is_none());
            assert!(via_back.upgrade().is_none());
            let cycle = if packed {
                crate::wire::packed::pack(&cycle_raw, &options).unwrap()
            } else {
                cycle_raw.clone()
            };
            LOCK_LOOP_BUILDS.store(0, Ordering::Relaxed);
            let mut decoder = Decoder::new(&cycle, &options).unwrap();
            let error = decode_cyclic::<alloc::sync::Arc<RwLock<LockLoop>>>(&mut decoder).err();
            assert_eq!(error, Some(Error::CycleUnsupported));
            assert_eq!(error.unwrap().code(), 14);
            assert_eq!(LOCK_LOOP_BUILDS.load(Ordering::Relaxed), 0);
            let legal = alloc::sync::Arc::new(RwLock::new(LockLoop { next: None }));
            let mut e = Encoder::new(&options);
            legal.ace_encode(&mut e).unwrap();
            let legal_bytes = e.finish().unwrap();
            let mut decoder = Decoder::new(&legal_bytes, &options).unwrap();
            let node = decode_cyclic::<alloc::sync::Arc<RwLock<LockLoop>>>(&mut decoder).unwrap();
            decoder.finish().unwrap();
            assert!(node.read().unwrap().next.is_none());
            assert_eq!(LOCK_LOOP_BUILDS.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn r_s3_g_06_identity_depth_boundaries() {
        // 身份链：k 个节点的根深度为零、链深为 k−1，每个节点经 Rc 登记为身份。
        struct DepthNode {
            next: Option<alloc::rc::Rc<DepthNode>>,
        }
        impl AceEncode for DepthNode {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.enter_object()?;
                let encoded = e.begin_field(0).and_then(|()| self.next.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceDecodeCyclic for DepthNode {
            type Plan = Option<u32>;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
                ctx.object(|ctx| ctx.field::<Option<alloc::rc::Rc<DepthNode>>>(0))
            }
            fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
                Self {
                    next: Option::<alloc::rc::Rc<DepthNode>>::build_from_plan(ctx, plan),
                }
            }
        }
        fn chain(nodes: usize) -> alloc::rc::Rc<DepthNode> {
            let mut head = alloc::rc::Rc::new(DepthNode { next: None });
            for _ in 1..nodes {
                head = alloc::rc::Rc::new(DepthNode { next: Some(head) });
            }
            head
        }
        fn chain_len(mut node: &DepthNode) -> usize {
            let mut len = 1;
            while let Some(next) = node.next.as_deref() {
                len += 1;
                node = next;
            }
            len
        }
        for packed in [false, true] {
            let options = packed_opts(packed);
            // 身份链同时打开的构造闭包数等于链长；默认 max_build_nesting=256 不足以装配 257 节点链，
            // 故仅本测试放宽到 512。
            let relaxed_build = Options {
                max_build_nesting: 512,
                ..packed_opts(packed)
            };
            // 链深 255 与 256：默认 max_depth 下编码、解码均成功。
            for nodes in [256, 257] {
                let mut e = Encoder::new(&options);
                chain(nodes).ace_encode(&mut e).unwrap();
                let bytes = e.finish().unwrap();
                let mut decoder = Decoder::new(&bytes, &relaxed_build).unwrap();
                let decoded = decode_cyclic::<alloc::rc::Rc<DepthNode>>(&mut decoder).unwrap();
                decoder.finish().unwrap();
                assert_eq!(chain_len(&decoded), nodes);
            }
            // 链深 257（258 个节点）超过默认 max_depth：编码即返回 DepthExceeded。
            let mut e = Encoder::new(&options);
            let error = chain(258).ace_encode(&mut e).err();
            assert_eq!(error, Some(Error::DepthExceeded));
            assert_eq!(error.unwrap().code(), 5);
            // max_depth=300 可编码出合法线格式；默认选项解码同样返回 DepthExceeded。
            let wide_depth = Options {
                max_depth: 300,
                ..packed_opts(packed)
            };
            let mut e = Encoder::new(&wide_depth);
            chain(258).ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let error = decode_cyclic::<alloc::rc::Rc<DepthNode>>(&mut decoder).err();
            assert_eq!(error, Some(Error::DepthExceeded));
            assert_eq!(error.unwrap().code(), 5);
        }
    }

    /// 测试 05 的完整读回：只用 `decode_cyclic`，随后 finish 校验整条消息。
    fn decode_target_layout<T: AceDecodeCyclic>(bytes: &[u8], options: &Options) -> T {
        let mut decoder = Decoder::new(bytes, options).unwrap();
        let decoded = decode_cyclic::<T>(&mut decoder).unwrap();
        decoder.finish().unwrap();
        decoded
    }
    /// 测试 05 每行的公共断言：按设计 max_depth（`None` 为默认值）在 raw 与 packed 各编码一次，
    /// 断言与金样逐字节相等（packed 解包后同样逐字节相等）、R/S 取自 raw_counts、编码期 B 等于
    /// 设计值，两态各自用 `decode_cyclic` 读回并 finish；返回 raw、packed 两态读回值（按此顺序），
    /// 调用方须对每个读回值各执行一次值与身份断言。
    fn check_target_layout<T: AceEncode + AceDecodeCyclic>(
        value: &T,
        expected: &[u8],
        max_depth: Option<usize>,
        begins: usize,
        records: usize,
        slots: usize,
    ) -> [T; 2] {
        assert_eq!(raw_counts(expected), (records, slots));
        [false, true].map(|packed| {
            let options = match max_depth {
                Some(max_depth) => Options {
                    max_depth,
                    packed,
                    ..Options::default()
                },
                None => Options {
                    packed,
                    ..Options::default()
                },
            };
            NON_CONTAINER_NODE_BEGINS.with(|count| count.set(0));
            let bytes = r3_encode_layout(value, &options, expected);
            assert_eq!(NON_CONTAINER_NODE_BEGINS.with(Cell::get), begins);
            decode_target_layout::<T>(&bytes, &options)
        })
    }
    /// 设计 r4 T05 的深度边界：父对象已占一层，集合再进一层时，`max_depth=0` 的编码与读回同为 DepthExceeded（5）。
    fn assert_depth_zero_rejected<T: AceEncode + AceDecodeCyclic>(value: &T, shallow: &[u8]) {
        let options = Options {
            max_depth: 0,
            packed: false,
            ..Options::default()
        };
        let mut encoder = Encoder::new(&options);
        assert_eq!(value.ace_encode(&mut encoder), Err(Error::DepthExceeded));
        let mut decoder = Decoder::new(shallow, &options).unwrap();
        assert_eq!(
            decode_cyclic::<T>(&mut decoder).err(),
            Some(Error::DepthExceeded)
        );
    }
    /// 测试 05/06 的最小用户对象：只有字段 0 的 u32。
    struct TargetLayoutNode {
        value: u32,
    }
    extern crate std;
    std::thread_local! {
        /// 编码期 B：`TargetLayoutNode` 成功进入对象并开始字段 0 编码的次数；线程局部，避免并行测试互相计数。
        static NON_CONTAINER_NODE_BEGINS: Cell<usize> = const { Cell::new(0) };
    }
    impl AceEncode for TargetLayoutNode {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            e.begin_field(0)?;
            NON_CONTAINER_NODE_BEGINS.with(|count| count.set(count.get() + 1));
            let encoded = self.value.ace_encode(e);
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for TargetLayoutNode {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            ctx.object(|ctx| ctx.field::<u32>(0))
        }
        fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
            Self { value: plan }
        }
    }
    /// 测试 05 的最小普通父：单字段 0 持一个叶引用；目标记录不进入对象，故叶不加层。
    struct TargetLayoutParent<T> {
        leaf: T,
    }
    impl<T: AceEncode> AceEncode for TargetLayoutParent<T> {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.leaf.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl<T: AceDecodeCyclic> AceDecodeCyclic for TargetLayoutParent<T> {
        type Plan = T::Plan;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<T::Plan> {
            ctx.object(|ctx| ctx.field::<T>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: T::Plan) -> Self {
            Self {
                leaf: T::build_from_plan(ctx, plan),
            }
        }
    }
    /// 测试 05 行 633/634 的普通父：字段 0 与字段 1 持同一目标的强引用，两字段共享一个身份。
    struct TargetLayoutPair<T> {
        first: T,
        second: T,
    }
    impl<T: AceEncode> AceEncode for TargetLayoutPair<T> {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.first.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.second.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl<T: AceDecodeCyclic> AceDecodeCyclic for TargetLayoutPair<T> {
        type Plan = (T::Plan, T::Plan);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| Ok((ctx.field::<T>(0)?, ctx.field::<T>(1)?)))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                first: T::build_from_plan(ctx, plan.0),
                second: T::build_from_plan(ctx, plan.1),
            }
        }
    }
    /// 测试 05 行 642 的持有者：字段 0 为 `Rc<Weak<u32>>`，字段 1 为同一目标的强 owner。
    struct TargetLayoutWeakTarget {
        weak_target: alloc::rc::Rc<alloc::rc::Weak<u32>>,
        owner: alloc::rc::Rc<u32>,
    }
    impl AceEncode for TargetLayoutWeakTarget {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.weak_target.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for TargetLayoutWeakTarget {
        type Plan = (u32, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<alloc::rc::Rc<alloc::rc::Weak<u32>>>(0)?,
                    ctx.field::<alloc::rc::Rc<u32>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                weak_target: alloc::rc::Rc::<alloc::rc::Weak<u32>>::build_from_plan(ctx, plan.0),
                owner: alloc::rc::Rc::<u32>::build_from_plan(ctx, plan.1),
            }
        }
    }
    /// 测试 05 行 648 的持有者：字段 0 为 `Rc<Option<Weak<u32>>>`，字段 1 为同一目标的强 owner。
    struct TargetLayoutOptionWeak {
        weak_target: alloc::rc::Rc<Option<alloc::rc::Weak<u32>>>,
        owner: alloc::rc::Rc<u32>,
    }
    impl AceEncode for TargetLayoutOptionWeak {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e
                .begin_field(0)
                .and_then(|()| self.weak_target.ace_encode(e))
                .and_then(|()| e.begin_field(1))
                .and_then(|()| self.owner.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for TargetLayoutOptionWeak {
        type Plan = (u32, u32);
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| {
                Ok((
                    ctx.field::<alloc::rc::Rc<Option<alloc::rc::Weak<u32>>>>(0)?,
                    ctx.field::<alloc::rc::Rc<u32>>(1)?,
                ))
            })
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                weak_target: alloc::rc::Rc::<Option<alloc::rc::Weak<u32>>>::build_from_plan(
                    ctx, plan.0,
                ),
                owner: alloc::rc::Rc::<u32>::build_from_plan(ctx, plan.1),
            }
        }
    }

    #[test]
    fn r_s3_g_05_target_layout_non_container() {
        // 覆盖设计 r4 T05 表的非容器布局行。金样规则：record = 8B[id, len] + 体；
        // 槽 = 16B[depth, tag, payload]；对象块体的 8B 为 [起始槽号, 槽数]，槽号从 1 起、
        // 按块离开顺序接续，块槽接在根槽之后；raw = 8 + Σ(8 + 体长) + 16S。
        // B 为 `TargetLayoutNode` 进入对象并开始字段 0 的次数，无 Node 的行 B=0。
        fn layout(main: &[Vec<u8>], assist: &[Vec<u8>]) -> Vec<u8> {
            target_raw(&main.concat(), &assist.concat())
        }

        // 623 根 Rc<u32>(37)：目标体恰 4B LE，一个根引用；与金样 r_s3_g_01_leaf_rc_root 同形。
        let expected = layout(
            &[target_rec(1, &37u32.to_le_bytes())],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 36);
        for leaf in check_target_layout(&alloc::rc::Rc::new(37u32), &expected, None, 0, 1, 1) {
            assert_eq!(*leaf, 37);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&leaf)].into_iter()),
                1
            );
        }

        // 624 根 Rc<String>("中")：目标体是裸 UTF-8 三字节，不包元素 id；raw = 8+11+16 = 35。
        let expected = layout(
            &[target_rec(1, "中".as_bytes())],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 35);
        for leaf in check_target_layout(
            &alloc::rc::Rc::new(String::from("中")),
            &expected,
            None,
            0,
            1,
            1,
        ) {
            assert_eq!(*leaf, "中");
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&leaf)].into_iter()),
                1
            );
        }

        // 625 根 Rc<value::Guid>(固定非零 16B)：完整 16B 体、无额外包装；raw = 8+24+16 = 48。
        let guid = value::Guid(core::array::from_fn(|index| index as u8 + 1));
        let expected = layout(
            &[target_rec(1, &guid.0)],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 48);
        for leaf in check_target_layout(&alloc::rc::Rc::new(guid), &expected, None, 0, 1, 1) {
            assert_eq!(*leaf, guid);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&leaf)].into_iter()),
                1
            );
        }

        // 626 根 Rc<RefCell<u32>>(37)：RefCell 透明转发，与 u32 目标同体、无包装层；raw 36。
        let expected = layout(
            &[target_rec(1, &37u32.to_le_bytes())],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 36);
        for leaf in check_target_layout(
            &alloc::rc::Rc::new(RefCell::new(37u32)),
            &expected,
            None,
            0,
            1,
            1,
        ) {
            assert_eq!(*leaf.borrow(), 37);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&leaf)].into_iter()),
                1
            );
        }

        // 627 最小普通父一个上述叶引用，max_depth=0：字段槽 depth=1，叶不加层；父块只有一个槽，
        // 槽号从 1 起，父体 8B = [1, 1]。
        let expected = layout(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            ],
            &[
                target_slot(0, 0, 2u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
            ],
        );
        assert_eq!(expected.len(), 68);
        for parent in check_target_layout(
            &TargetLayoutParent {
                leaf: alloc::rc::Rc::new(37u32),
            },
            &expected,
            Some(0),
            0,
            2,
            2,
        ) {
            assert_eq!(*parent.leaf, 37);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&parent.leaf)].into_iter()),
                1
            );
        }
        let expected = layout(
            &[
                target_rec(1, "中".as_bytes()),
                target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            ],
            &[
                target_slot(0, 0, 2u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
            ],
        );
        assert_eq!(expected.len(), 67);
        for parent in check_target_layout(
            &TargetLayoutParent {
                leaf: alloc::rc::Rc::new(String::from("中")),
            },
            &expected,
            Some(0),
            0,
            2,
            2,
        ) {
            assert_eq!(*parent.leaf, "中");
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&parent.leaf)].into_iter()),
                1
            );
        }
        let expected = layout(
            &[
                target_rec(1, &guid.0),
                target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            ],
            &[
                target_slot(0, 0, 2u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
            ],
        );
        assert_eq!(expected.len(), 80);
        for parent in check_target_layout(
            &TargetLayoutParent {
                leaf: alloc::rc::Rc::new(guid),
            },
            &expected,
            Some(0),
            0,
            2,
            2,
        ) {
            assert_eq!(*parent.leaf, guid);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&parent.leaf)].into_iter()),
                1
            );
        }
        let expected = layout(
            &[
                target_rec(1, &37u32.to_le_bytes()),
                target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
            ],
            &[
                target_slot(0, 0, 2u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
            ],
        );
        assert_eq!(expected.len(), 68);
        for parent in check_target_layout(
            &TargetLayoutParent {
                leaf: alloc::rc::Rc::new(RefCell::new(37u32)),
            },
            &expected,
            Some(0),
            0,
            2,
            2,
        ) {
            assert_eq!(*parent.leaf.borrow(), 37);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&parent.leaf)].into_iter()),
                1
            );
        }

        // 635 根 Rc<Option<u32>> Some(37)：体恰 [1]+37u32 共 5B；与金样 r_s3_g_01_option_target_root
        // 同形；raw = 8+(8+5)+16 = 37。
        let expected = layout(
            &[target_rec(1, &[1, 37, 0, 0, 0])],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 37);
        for some in check_target_layout(&alloc::rc::Rc::new(Some(37u32)), &expected, None, 0, 1, 1)
        {
            assert_eq!(*some, Some(37u32));
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&some)].into_iter()),
                1
            );
        }

        // 636 同型 None：体恰 [0]，仍保留 Option 目标 record 与根引用，不等于根 Option::None；
        // raw = 8+(8+1)+16 = 33。
        let expected = layout(
            &[target_rec(1, &[0])],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 33);
        for none in check_target_layout(&alloc::rc::Rc::new(None::<u32>), &expected, None, 0, 1, 1)
        {
            assert_eq!(*none, None);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&none)].into_iter()),
                1
            );
        }

        // 637 根 Rc<Option<String>> Some("中")：Option 体为 [1]+u32 字符串 id 共 5B，字符串体 3B；
        // 物理 id 顺序 [2, 1]（字符串目标先完成）；raw = 8+(11+13)+16 = 48。
        let expected = layout(
            &[
                target_rec(2, "中".as_bytes()),
                target_rec(1, &[1, 2, 0, 0, 0]),
            ],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 48);
        for some in check_target_layout(
            &alloc::rc::Rc::new(Some(String::from("中"))),
            &expected,
            None,
            0,
            2,
            1,
        ) {
            assert_eq!(some.as_deref(), Some("中"));
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&some)].into_iter()),
                1
            );
        }

        // 641 根 Rc<Rc<u32>>(37)：外层体 4B 为内层 id、内层体 4B 为值，物理 [2, 1]；与金样
        // r_s3_g_01_leaf_rc_nested_order 同形；raw = 8+(12+12)+16 = 48。
        let expected = layout(
            &[
                target_rec(2, &37u32.to_le_bytes()),
                target_rec(1, &2u32.to_le_bytes()),
            ],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 48);
        for nested in check_target_layout(
            &alloc::rc::Rc::new(alloc::rc::Rc::new(37u32)),
            &expected,
            None,
            0,
            2,
            1,
        ) {
            assert_eq!(**nested, 37);
            assert_eq!(
                identity_count(
                    [
                        alloc::rc::Rc::as_ptr(&nested) as *const (),
                        alloc::rc::Rc::as_ptr(&*nested) as *const (),
                    ]
                    .into_iter()
                ),
                2
            );
        }

        // 642 普通父先持 Rc<Weak<u32>>、后持对应 Rc<u32>(37)：外层 id 1、弱预留 id 2、强定义填 2；
        // 物理 [1, 2, 3]，父块两个槽号从 1 起；与金样 r_s3_g_02_weak_target_in_rc_is_record_body
        // 同形；raw = 8+(12+12+16)+48 = 96。
        let expected = layout(
            &[
                target_rec(1, &[2, 0, 0, 0]),
                target_rec(2, &37u32.to_le_bytes()),
                target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
            ],
            &[
                target_slot(0, 0, 3u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
                target_slot(1, 1, 2u64.to_le_bytes()),
            ],
        );
        assert_eq!(expected.len(), 96);
        let owner = alloc::rc::Rc::new(37u32);
        let value = TargetLayoutWeakTarget {
            weak_target: alloc::rc::Rc::new(alloc::rc::Rc::downgrade(&owner)),
            owner: alloc::rc::Rc::clone(&owner),
        };
        for decoded in check_target_layout(&value, &expected, None, 0, 3, 3) {
            assert_eq!(*decoded.owner, 37);
            let upgraded = decoded.weak_target.upgrade().unwrap();
            assert!(alloc::rc::Rc::ptr_eq(&upgraded, &decoded.owner));
            let owner_address = alloc::rc::Rc::as_ptr(&decoded.owner) as *const ();
            let weak_address = alloc::rc::Weak::as_ptr(&decoded.weak_target) as *const ();
            let target_address = alloc::rc::Rc::as_ptr(&decoded.weak_target) as *const ();
            assert_eq!(weak_address, owner_address);
            assert_eq!(
                identity_count([target_address, owner_address].into_iter()),
                2
            );
        }

        // 643 同一 Rc<Weak<u32>> 独立作根、只有消息外 owner：Weak 预留没有消息内强目标填充，
        // finish 返回 InvalidArgument(1)。设计记 B/I = 0/2；身份表不可见，此处断言错误码与 B。
        let owner = alloc::rc::Rc::new(37u32);
        let weak_target = alloc::rc::Rc::new(alloc::rc::Rc::downgrade(&owner));
        for packed in [false, true] {
            NON_CONTAINER_NODE_BEGINS.with(|count| count.set(0));
            let mut encoder = Encoder::new(&packed_opts(packed));
            assert_eq!(weak_target.ace_encode(&mut encoder), Ok(()));
            let error = encoder.finish().err();
            assert_eq!(error, Some(Error::InvalidArgument));
            assert_eq!(error.unwrap().code(), 1);
            assert_eq!(NON_CONTAINER_NODE_BEGINS.with(Cell::get), 0);
        }

        // 644 根 Rc<RefCell<Option<u32>>> Some(37)：RefCell 透明转发 Option 目标，体 5B；raw 37。
        let expected = layout(
            &[target_rec(1, &[1, 37, 0, 0, 0])],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 37);
        for some in check_target_layout(
            &alloc::rc::Rc::new(RefCell::new(Some(37u32))),
            &expected,
            None,
            0,
            1,
            1,
        ) {
            assert_eq!(*some.borrow(), Some(37u32));
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&some)].into_iter()),
                1
            );
        }

        // 645 根 Rc<Option<Rc<u32>>> Some：presence + u32 内层 id，物理 [2, 1]；
        // raw = 8+(12+13)+16 = 49。
        let expected = layout(
            &[
                target_rec(2, &37u32.to_le_bytes()),
                target_rec(1, &[1, 2, 0, 0, 0]),
            ],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 49);
        for some in check_target_layout(
            &alloc::rc::Rc::new(Some(alloc::rc::Rc::new(37u32))),
            &expected,
            None,
            0,
            2,
            1,
        ) {
            let inner = (*some).as_ref().unwrap();
            assert_eq!(**inner, 37);
            assert_eq!(
                identity_count(
                    [
                        alloc::rc::Rc::as_ptr(&some) as *const (),
                        alloc::rc::Rc::as_ptr(inner) as *const (),
                    ]
                    .into_iter()
                ),
                2
            );
        }

        // 646 根 Rc<Option<Rc<Node>>>，Node 只含值 37：outer id 1 → Node id 2；只有 Node 进对象
        // （B=1）并计对象深度，Node 块一个槽（depth 1, tag 0, payload 37）；物理 [2, 1]；
        // raw = 8+(16+13)+32 = 69。
        let expected = layout(
            &[
                target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
                target_rec(1, &[1, 2, 0, 0, 0]),
            ],
            &[
                target_slot(0, 0, 1u64.to_le_bytes()),
                target_slot(1, 0, 37u64.to_le_bytes()),
            ],
        );
        assert_eq!(expected.len(), 69);
        for some in check_target_layout(
            &alloc::rc::Rc::new(Some(alloc::rc::Rc::new(TargetLayoutNode { value: 37 }))),
            &expected,
            None,
            1,
            2,
            2,
        ) {
            let node = (*some).as_ref().unwrap();
            assert_eq!(node.value, 37);
            assert_eq!(
                identity_count(
                    [
                        alloc::rc::Rc::as_ptr(&some) as *const (),
                        alloc::rc::Rc::as_ptr(node) as *const (),
                    ]
                    .into_iter()
                ),
                2
            );
        }

        // 647 根 Rc<Option<Weak<u32>>> 已失效：存在性在写前归一为 None，体仅 [0]，
        // 不登记内层 Weak 目标；与金样 r_s3_g_02_option_weak_target_expired_is_zero 同形；raw 33。
        let expected = layout(
            &[target_rec(1, &[0])],
            &[target_slot(0, 0, 1u64.to_le_bytes())],
        );
        assert_eq!(expected.len(), 33);
        for expired in check_target_layout(
            &alloc::rc::Rc::new(Some(alloc::rc::Weak::<u32>::new())),
            &expected,
            None,
            0,
            1,
            1,
        ) {
            assert!((*expired).is_none());
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&expired)].into_iter()),
                1
            );
        }

        // 648 普通父先持 Rc<Option<Weak<u32>>>、后持对应 Rc<u32>(37)：live Some 写 [1]+u32 id，
        // 强定义填弱预留 2；物理 [1, 2, 3]；与金样 r_s3_g_02_option_weak_target_live_shares_id
        // 同形；raw = 8+(13+12+16)+48 = 97。
        let expected = layout(
            &[
                target_rec(1, &[1, 2, 0, 0, 0]),
                target_rec(2, &37u32.to_le_bytes()),
                target_rec(3, &[1, 0, 0, 0, 2, 0, 0, 0]),
            ],
            &[
                target_slot(0, 0, 3u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
                target_slot(1, 1, 2u64.to_le_bytes()),
            ],
        );
        assert_eq!(expected.len(), 97);
        let owner = alloc::rc::Rc::new(37u32);
        let value = TargetLayoutOptionWeak {
            weak_target: alloc::rc::Rc::new(Some(alloc::rc::Rc::downgrade(&owner))),
            owner: alloc::rc::Rc::clone(&owner),
        };
        for decoded in check_target_layout(&value, &expected, None, 0, 3, 3) {
            let weak_option: &Option<alloc::rc::Weak<u32>> = &decoded.weak_target;
            let weak_ref = weak_option.as_ref().unwrap();
            let upgraded = weak_ref.upgrade().unwrap();
            assert_eq!(*upgraded, 37);
            assert!(alloc::rc::Rc::ptr_eq(&upgraded, &decoded.owner));
            assert_eq!(*decoded.owner, 37);
            let owner_address = alloc::rc::Rc::as_ptr(&decoded.owner) as *const ();
            let weak_address = alloc::rc::Weak::as_ptr(weak_ref) as *const ();
            let target_address = alloc::rc::Rc::as_ptr(&decoded.weak_target) as *const ();
            assert_eq!(weak_address, owner_address);
            assert_eq!(
                identity_count([target_address, owner_address].into_iter()),
                2
            );
        }
    }

    #[test]
    fn r_s3_g_container_target_layout() {
        // 设计 r4 目标布局表与 T05 逐例表的容器身份目标：628、629、630、631、632、633、634、649、650。
        // 金样按规则手写，不取自编码器：record = 8B[id, len] + 体；槽 = 16B[depth, tag, payload]；
        // raw = 8 + Σ(8 + 体长) + 16S。目标自身占用预留 id，元素引用另计一条记录。
        // Vec 体 = u32 count + 元素；Box 体 = 单个 T 元素体（无 count）；Map 体 = u32 count + 按 Ord 的键值对。
        // B 为 TargetLayoutNode 进入对象的次数；本测试不含该类型，故每行 B = 0。
        fn layout(main: &[Vec<u8>], assist: &[Vec<u8>]) -> Vec<u8> {
            target_raw(&main.concat(), &assist.concat())
        }
        let row_628 = || {
            // 628 根 Rc<BTreeMap<u32,u32>> {7:37}，max_depth=0（根集合即第 1 层）。
            // 目标 id 1；体 = count 1 + 键 7 + 值 37，各 4B LE，共 12B；根槽 depth 0、tag 0、payload 为 id 1。
            // raw = 8 + (8 + 12) + 16 = 44；R=1、S=1。
            let expected = layout(
                &[target_rec(1, &[1, 0, 0, 0, 7, 0, 0, 0, 37, 0, 0, 0])],
                &[target_slot(0, 0, 1u64.to_le_bytes())],
            );
            assert_eq!(expected.len(), 44);
            let value = alloc::rc::Rc::new(BTreeMap::from([(7u32, 37u32)]));
            for decoded in check_target_layout(&value, &expected, Some(0), 0, 1, 1) {
                assert_eq!(*decoded, BTreeMap::from([(7u32, 37u32)]));
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                    1
                );
            }
        };
        let row_629 = || {
            // 629 普通父（单字段 0）持 Rc<BTreeMap<u32,u32>> {7:37}，max_depth=1。
            // 父对象在根层，字段深度 1；Map 集合进入时 open=1，满足 1 ≤ max_depth；叶 u32 不加层。
            // Map 目标以预留 id 1 提交（12B 体）；父对象 id 2 在 leave 时分配，体 = [起始槽 1, 槽数 1]，成为根。
            // 字段槽 depth 1、tag 0、payload 为 id 1；根槽 depth 0、tag 0、payload 为 id 2。
            // raw = 8 + (8 + 12) + (8 + 8) + 32 = 76；R=2、S=2。
            let expected = layout(
                &[
                    target_rec(1, &[1, 0, 0, 0, 7, 0, 0, 0, 37, 0, 0, 0]),
                    target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
                ],
                &[
                    target_slot(0, 0, 2u64.to_le_bytes()),
                    target_slot(1, 0, 1u64.to_le_bytes()),
                ],
            );
            assert_eq!(expected.len(), 76);
            let parent = TargetLayoutParent {
                leaf: alloc::rc::Rc::new(BTreeMap::from([(7u32, 37u32)])),
            };
            for decoded in check_target_layout(&parent, &expected, Some(1), 0, 2, 2) {
                assert_eq!(*decoded.leaf, BTreeMap::from([(7u32, 37u32)]));
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded.leaf)].into_iter()),
                    1
                );
            }
            assert_depth_zero_rejected(&parent, &expected);
        };
        let row_630 = || {
            // 630 根 Rc<BTreeMap<u32,String>> {7:"中"}：Map 绑定预留 id 1；字符串普通分配 id 2 并先提交，物理 [2,1]。
            // 字符串记录体 = UTF-8 三字节（E4 B8 AD）；Map 体 = count 1 + 键 7 + 值引用 id 2，共 12B。
            // raw = 8 + (8 + 3) + (8 + 12) + 16 = 55；R=2、S=1。
            let expected = layout(
                &[
                    target_rec(2, "中".as_bytes()),
                    target_rec(1, &[1, 0, 0, 0, 7, 0, 0, 0, 2, 0, 0, 0]),
                ],
                &[target_slot(0, 0, 1u64.to_le_bytes())],
            );
            assert_eq!(expected.len(), 55);
            let value = alloc::rc::Rc::new(BTreeMap::from([(7u32, String::from("中"))]));
            for decoded in check_target_layout(&value, &expected, None, 0, 2, 1) {
                assert_eq!(*decoded, BTreeMap::from([(7u32, String::from("中"))]));
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                    1
                );
            }
        };
        let row_631 = || {
            // 631 根 Rc<Vec<u32>> [37]，max_depth=0：目标体 = count 1 + 37 = 8B，无多余 id 层。
            // raw = 8 + (8 + 8) + 16 = 40；R=1、S=1。
            let expected = layout(
                &[target_rec(1, &[1, 0, 0, 0, 37, 0, 0, 0])],
                &[target_slot(0, 0, 1u64.to_le_bytes())],
            );
            assert_eq!(expected.len(), 40);
            let value = alloc::rc::Rc::new(alloc::vec![37u32]);
            for decoded in check_target_layout(&value, &expected, Some(0), 0, 1, 1) {
                assert_eq!(*decoded, alloc::vec![37u32]);
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                    1
                );
            }
        };
        let row_632 = || {
            // 632 根 Rc<Box<u32>> 37，max_depth=0：目标体恰为元素体 37 的 4B，无 count。
            // raw = 8 + (8 + 4) + 16 = 36；R=1、S=1。
            let expected = layout(
                &[target_rec(1, &[37, 0, 0, 0])],
                &[target_slot(0, 0, 1u64.to_le_bytes())],
            );
            assert_eq!(expected.len(), 36);
            let value = alloc::rc::Rc::new(Box::new(37u32));
            for decoded in check_target_layout(&value, &expected, Some(0), 0, 1, 1) {
                assert_eq!(**decoded, 37);
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                    1
                );
            }
        };
        let row_633 = || {
            // 633 普通父两字段共享同一 Rc<Vec<u32>> [37]，max_depth=1：目标 id 1 只提交一次，第二字段复用 id 1。
            // 目标体 8B（count 1 + 37）；父对象 id 2 体 = [起始槽 1, 槽数 2]；字段槽 (1,0,id1)、(1,1,id1)；根槽 payload 为 id 2。
            // raw = 8 + (8 + 8) + (8 + 8) + 48 = 88；R=2、S=3。
            let expected = layout(
                &[
                    target_rec(1, &[1, 0, 0, 0, 37, 0, 0, 0]),
                    target_rec(2, &[1, 0, 0, 0, 2, 0, 0, 0]),
                ],
                &[
                    target_slot(0, 0, 2u64.to_le_bytes()),
                    target_slot(1, 0, 1u64.to_le_bytes()),
                    target_slot(1, 1, 1u64.to_le_bytes()),
                ],
            );
            assert_eq!(expected.len(), 88);
            let shared = alloc::rc::Rc::new(alloc::vec![37u32]);
            let pair = TargetLayoutPair {
                first: shared.clone(),
                second: shared,
            };
            for decoded in check_target_layout(&pair, &expected, Some(1), 0, 2, 3) {
                assert_eq!(*decoded.first, alloc::vec![37u32]);
                assert!(alloc::rc::Rc::ptr_eq(&decoded.first, &decoded.second));
                assert_eq!(
                    identity_count(
                        [
                            alloc::rc::Rc::as_ptr(&decoded.first),
                            alloc::rc::Rc::as_ptr(&decoded.second),
                        ]
                        .into_iter()
                    ),
                    1
                );
            }
            assert_depth_zero_rejected(&pair, &expected);
        };
        let row_634 = || {
            // 634 普通父两字段共享同一 Rc<Box<u32>> 37，max_depth=1：目标体恰为 4B 元素体 37，无 count。
            // 父对象 id 2 体 = [起始槽 1, 槽数 2]；字段槽与根槽同 633。
            // raw = 8 + (8 + 4) + (8 + 8) + 48 = 84；R=2、S=3。
            let expected = layout(
                &[
                    target_rec(1, &[37, 0, 0, 0]),
                    target_rec(2, &[1, 0, 0, 0, 2, 0, 0, 0]),
                ],
                &[
                    target_slot(0, 0, 2u64.to_le_bytes()),
                    target_slot(1, 0, 1u64.to_le_bytes()),
                    target_slot(1, 1, 1u64.to_le_bytes()),
                ],
            );
            assert_eq!(expected.len(), 84);
            let shared = alloc::rc::Rc::new(Box::new(37u32));
            let pair = TargetLayoutPair {
                first: shared.clone(),
                second: shared,
            };
            for decoded in check_target_layout(&pair, &expected, Some(1), 0, 2, 3) {
                assert_eq!(**decoded.first, 37);
                assert!(alloc::rc::Rc::ptr_eq(&decoded.first, &decoded.second));
                assert_eq!(
                    identity_count(
                        [
                            alloc::rc::Rc::as_ptr(&decoded.first),
                            alloc::rc::Rc::as_ptr(&decoded.second),
                        ]
                        .into_iter()
                    ),
                    1
                );
            }
            assert_depth_zero_rejected(&pair, &expected);
        };
        let row_649 = || {
            // 649 根 Rc<BTreeMap<u32,Vec<u32>>> {7:[37]}：Map 绑定预留 id 1；值 Vec 普通分配 id 2 并先提交（体 count 1 + 37 = 8B）。
            // Map 体 = count 1 + 键 7 + 值引用 id 2，共 12B；物理 [2,1]。
            // raw = 8 + (8 + 8) + (8 + 12) + 16 = 60；R=2、S=1。
            let expected = layout(
                &[
                    target_rec(2, &[1, 0, 0, 0, 37, 0, 0, 0]),
                    target_rec(1, &[1, 0, 0, 0, 7, 0, 0, 0, 2, 0, 0, 0]),
                ],
                &[target_slot(0, 0, 1u64.to_le_bytes())],
            );
            assert_eq!(expected.len(), 60);
            let value = alloc::rc::Rc::new(BTreeMap::from([(7u32, alloc::vec![37u32])]));
            for decoded in check_target_layout(&value, &expected, None, 0, 2, 1) {
                assert_eq!(*decoded, BTreeMap::from([(7u32, alloc::vec![37u32])]));
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                    1
                );
            }
        };
        let row_650 = || {
            // 650 根 Rc<BTreeMap<u32,Box<u32>>> {7:Box(37)}：值 Box 普通分配 id 2 并先提交（体仅 4B 元素体 37）；Map 绑定 id 1。
            // Map 体 12B；物理 [2,1]。raw = 8 + (8 + 4) + (8 + 12) + 16 = 56；R=2、S=1。
            let expected = layout(
                &[
                    target_rec(2, &[37, 0, 0, 0]),
                    target_rec(1, &[1, 0, 0, 0, 7, 0, 0, 0, 2, 0, 0, 0]),
                ],
                &[target_slot(0, 0, 1u64.to_le_bytes())],
            );
            assert_eq!(expected.len(), 56);
            let value = alloc::rc::Rc::new(BTreeMap::from([(7u32, Box::new(37u32))]));
            for decoded in check_target_layout(&value, &expected, None, 0, 2, 1) {
                assert_eq!(*decoded, BTreeMap::from([(7u32, Box::new(37u32))]));
                assert_eq!(
                    identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                    1
                );
            }
        };
        // 每行单独捕获 panic：一行失败不遮蔽其余行，失败信息逐行保留在最终断言中。
        let rows: [(&str, &dyn Fn()); 9] = [
            ("628", &row_628),
            ("629", &row_629),
            ("630", &row_630),
            ("631", &row_631),
            ("632", &row_632),
            ("633", &row_633),
            ("634", &row_634),
            ("649", &row_649),
            ("650", &row_650),
        ];
        let failures: Vec<String> = rows
            .iter()
            .filter_map(|(row, check)| {
                let payload =
                    std::panic::catch_unwind(core::panic::AssertUnwindSafe(check)).err()?;
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        payload
                            .downcast_ref::<&str>()
                            .map(|text| String::from(*text))
                    })
                    .unwrap_or_default();
                Some(alloc::format!("{row}: {message}"))
            })
            .collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
    #[test]
    fn r_s3_g_container_target_bulk_precheck_before_copy() {
        // 根 Rc<Vec<i32>> 的 100 个元素体为 400B，超出 max_raw_bytes=64。预检须在构造整段体与调用
        // bulk writer 之前返回 ResourceLimit（6）；bulk 与逐项 writer 均未执行，hooks 计数为 (0, 0)。
        let value = alloc::rc::Rc::new((0..100).collect::<Vec<i32>>());
        let options = Options {
            max_raw_bytes: 64,
            ..packed_opts(false)
        };
        let mut e = Encoder::new(&options);
        crate::hooks::reset();
        let error = value.ace_encode(&mut e).unwrap_err();
        assert_eq!(error, Error::ResourceLimit);
        assert_eq!(error.code(), 6);
        assert_eq!(crate::hooks::snapshot(), (0, 0));
    }
    #[test]
    fn r_s3_g_container_target_budget_boundary_no_double_charge() {
        // 631 的预算边界变体：根 Rc<Vec<u32>> [37]，max_records=1 且 raw 预算恰为 40B。
        // 预检只计新增体字节，名额与记录头已由 begin_target_record 计入；重复收费会在此边界超限。
        // B 为 0（本例不含 TargetLayoutNode），R=1、S=1，与 631 行一致。
        let value = alloc::rc::Rc::new(alloc::vec![37u32]);
        let expected = target_raw(
            &target_rec(1, &[1, 0, 0, 0, 37, 0, 0, 0]),
            &target_slot(0, 0, 1u64.to_le_bytes()),
        );
        assert_eq!(expected.len(), 40);
        assert_eq!(raw_counts(&expected), (1, 1));
        for packed in [false, true] {
            let options = Options {
                max_depth: 0,
                max_records: 1,
                max_raw_bytes: 40,
                packed,
                ..Options::default()
            };
            let bytes = r3_encode_layout(&value, &options, &expected);
            let decoded = decode_target_layout::<alloc::rc::Rc<Vec<u32>>>(&bytes, &options);
            assert_eq!(*decoded, alloc::vec![37u32]);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                1
            );
        }
    }
    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_05_rwlock_option_target_std() {
        // 根 Rc<Option<RwLock<u32>>>::Some(37)：RwLock 透明转发，Option 体 [1]+37u32 共 5B；
        // 金样与 Rc<Option<u32>> 同形（体字节与根槽回填规则一致）；raw = 8+(8+5)+16 = 37。
        let expected = target_raw(
            &target_rec(1, &[1, 37, 0, 0, 0]),
            &target_slot(0, 0, 1u64.to_le_bytes()),
        );
        assert_eq!(expected.len(), 37);
        for some in check_target_layout(
            &alloc::rc::Rc::new(Some(RwLock::new(37u32))),
            &expected,
            None,
            0,
            1,
            1,
        ) {
            let lock = (*some).as_ref().unwrap();
            assert_eq!(*lock.read().unwrap(), 37);
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&some)].into_iter()),
                1
            );
        }
    }

    /// 测试 06 的解码期构造计数：每次 `DefaultBoundaryNode` 进入阶段二构造时自增。
    static DEFAULT_BOUNDARY_BUILDS: AtomicUsize = AtomicUsize::new(0);
    /// 测试 06 的链节点：只有一个 next 字段，根深度为零、链深为节点数减一。
    struct DefaultBoundaryNode {
        next: Option<alloc::rc::Rc<DefaultBoundaryNode>>,
    }
    impl AceEncode for DefaultBoundaryNode {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.next.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for DefaultBoundaryNode {
        type Plan = Option<u32>;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| ctx.field::<Option<alloc::rc::Rc<DefaultBoundaryNode>>>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            DEFAULT_BOUNDARY_BUILDS.fetch_add(1, Ordering::Relaxed);
            Self {
                next: Option::<alloc::rc::Rc<DefaultBoundaryNode>>::build_from_plan(ctx, plan),
            }
        }
    }
    fn default_boundary_chain(nodes: usize) -> alloc::rc::Rc<DefaultBoundaryNode> {
        let mut head = alloc::rc::Rc::new(DefaultBoundaryNode { next: None });
        for _ in 1..nodes {
            head = alloc::rc::Rc::new(DefaultBoundaryNode { next: Some(head) });
        }
        head
    }
    fn default_boundary_chain_len(mut node: &DefaultBoundaryNode) -> usize {
        let mut len = 1;
        while let Some(next) = node.next.as_deref() {
            len += 1;
            node = next;
        }
        len
    }

    #[test]
    fn r_s3_g_06_default_build_nesting_boundary() {
        // 用户裁定 B 的回归锁：默认 Options（max_depth = max_build_nesting = 256）不放宽。
        // 链上每个 Rc 身份同时打开，构造闭包峰值等于链长：256 节点可编码可装配；257 节点编码
        // 成功（最深对象深度 256 未超 max_depth），解码在规划结束的构造预算检查处返回
        // ResourceLimit(6)，该检查位于构建之前，故 Node 构造计数为 0。
        for packed in [false, true] {
            let options = packed_opts(packed);
            let mut encoder = Encoder::new(&options);
            default_boundary_chain(256)
                .ace_encode(&mut encoder)
                .unwrap();
            let bytes = encoder.finish().unwrap();
            DEFAULT_BOUNDARY_BUILDS.store(0, Ordering::Relaxed);
            let decoded =
                decode_target_layout::<alloc::rc::Rc<DefaultBoundaryNode>>(&bytes, &options);
            assert_eq!(default_boundary_chain_len(&decoded), 256);
            assert_eq!(DEFAULT_BOUNDARY_BUILDS.load(Ordering::Relaxed), 256);

            let mut encoder = Encoder::new(&options);
            default_boundary_chain(257)
                .ace_encode(&mut encoder)
                .unwrap();
            let bytes = encoder.finish().unwrap();
            DEFAULT_BOUNDARY_BUILDS.store(0, Ordering::Relaxed);
            let mut decoder = Decoder::new(&bytes, &options).unwrap();
            let error = decode_cyclic::<alloc::rc::Rc<DefaultBoundaryNode>>(&mut decoder).err();
            assert_eq!(error, Some(Error::ResourceLimit));
            assert_eq!(error.unwrap().code(), 6);
            assert_eq!(DEFAULT_BOUNDARY_BUILDS.load(Ordering::Relaxed), 0);
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn r_s3_g_10_process_determinism_std() {
        const CHILD_ENV: &str = "ACE_S3_G_10_CHILD";
        const BEGIN: &str = "ACE_S3_G_10_BEGIN";
        const END: &str = "ACE_S3_G_10_END";
        // 每个拓扑按 raw 与 packed 各编码一次，返回两个十六进制字符串。
        fn encode_hex<T: AceEncode>(value: &T) -> Vec<String> {
            use core::fmt::Write;
            [false, true]
                .into_iter()
                .map(|packed| {
                    let mut e = Encoder::new(&packed_opts(packed));
                    value.ace_encode(&mut e).unwrap();
                    let mut hex = String::new();
                    for byte in e.finish().unwrap() {
                        write!(hex, "{byte:02x}").unwrap();
                    }
                    hex
                })
                .collect()
        }
        fn process_lines() -> Vec<String> {
            // 共享 Rc 与等值不同实例。
            let a = alloc::rc::Rc::new(37u32);
            let b = alloc::rc::Rc::new(37u32);
            let shared = alloc::vec![alloc::rc::Rc::clone(&a), a, alloc::rc::Rc::clone(&b), b,];
            // BTreeMap 倒序插入，编码结果须按键序且与插入顺序无关。
            let c = alloc::rc::Rc::new(37u32);
            let mut reversed = BTreeMap::new();
            reversed.insert(9u32, alloc::rc::Rc::clone(&c));
            reversed.insert(8u32, alloc::rc::Rc::new(37u32));
            reversed.insert(7u32, c);
            // 固定 NaN 位模式，不做规范化。
            let nan = f64::from_bits(0x7ff8_0000_0000_0001);
            let mut lines = Vec::new();
            for (name, hexes) in [
                ("shared-rc", encode_hex(&shared)),
                ("btreemap-reverse", encode_hex(&reversed)),
                ("nan-f64", encode_hex(&nan)),
            ] {
                for (mode, hex) in ["raw", "packed"].into_iter().zip(hexes) {
                    lines.push(alloc::format!("{name} {mode} {hex}"));
                }
            }
            lines
        }
        if std::env::var_os(CHILD_ENV).is_some() {
            std::println!("{BEGIN}");
            for line in process_lines() {
                std::println!("{line}");
            }
            std::println!("{END}");
            return;
        }
        let expected = process_lines();
        let mut children = Vec::new();
        for _ in 0..2 {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "schema::type_tests::r_s3_g_10_process_determinism_std",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "child failed: {:?}", output.status);
            let text = String::from_utf8(output.stdout).unwrap();
            let (_, rest) = text
                .split_once(BEGIN)
                .expect("child output has no begin marker");
            let (body, _) = rest
                .split_once(END)
                .expect("child output has no end marker");
            let lines: Vec<String> = body
                .lines()
                .filter(|line| !line.is_empty())
                .map(String::from)
                .collect();
            children.push(lines);
        }
        assert_eq!(children[0], children[1]);
        assert_eq!(children[0], expected);
    }
    #[test]
    fn r_s3_g_cov_d_rc_option_container_targets() {
        // 覆盖补测 COV-D（638–640）：Rc 包 Option 容器的成功路径。Option 只写 presence 与内层目标 id，不增加深度。
        // 金样按规则手写：Option 目标预留 id 1，内层容器先提交为 id 2，根槽 payload 为 id 1。
        // 638：根 Rc<Option<Vec<u32>>> 为 Some([37])，max_depth=0；raw 53B，R=2、S=1。
        let main = [
            target_rec(2, &[1, 0, 0, 0, 37, 0, 0, 0]),
            target_rec(1, &[1, 2, 0, 0, 0]),
        ]
        .concat();
        let expected = target_raw(&main, &target_slot(0, 0, 1u64.to_le_bytes()));
        assert_eq!(expected.len(), 53);
        let value = alloc::rc::Rc::new(Some(alloc::vec![37u32]));
        for decoded in check_target_layout(&value, &expected, Some(0), 0, 2, 1) {
            assert_eq!(*decoded, Some(alloc::vec![37u32]));
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                1
            );
        }
        // 639：根 Rc<Option<Box<u32>>> 为 Some(Box(37))，默认选项；raw 49B，R=2、S=1。
        let main = [
            target_rec(2, &[37, 0, 0, 0]),
            target_rec(1, &[1, 2, 0, 0, 0]),
        ]
        .concat();
        let expected = target_raw(&main, &target_slot(0, 0, 1u64.to_le_bytes()));
        assert_eq!(expected.len(), 49);
        let value = alloc::rc::Rc::new(Some(Box::new(37u32)));
        for decoded in check_target_layout(&value, &expected, None, 0, 2, 1) {
            assert_eq!(*decoded, Some(Box::new(37u32)));
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                1
            );
        }
        // 640：根 Rc<Option<BTreeMap<u32,u32>>> 为 Some({7:37})，默认选项；raw 57B，R=2、S=1。
        let main = [
            target_rec(2, &[1, 0, 0, 0, 7, 0, 0, 0, 37, 0, 0, 0]),
            target_rec(1, &[1, 2, 0, 0, 0]),
        ]
        .concat();
        let expected = target_raw(&main, &target_slot(0, 0, 1u64.to_le_bytes()));
        assert_eq!(expected.len(), 57);
        let value = alloc::rc::Rc::new(Some(BTreeMap::from([(7u32, 37u32)])));
        for decoded in check_target_layout(&value, &expected, None, 0, 2, 1) {
            assert_eq!(*decoded, Some(BTreeMap::from([(7u32, 37u32)])));
            assert_eq!(
                identity_count([alloc::rc::Rc::as_ptr(&decoded)].into_iter()),
                1
            );
        }
    }
    #[test]
    fn r_s3_g_cov_d_rc_option_object_without_element_entry() {
        // 覆盖补测 COV-D（651/652）：用户对象只有对象写法的 AceEncode，未覆写 N01（ace_encode_element）。
        // 计数器只记 ace_encode 进入次数：Some 在进入对象前即因 N01 默认错误失败；None 不调用任何元素入口。
        std::thread_local! {
            static NO_ELEMENT_ENCODE_ENTRIES: Cell<usize> = const { Cell::new(0) };
        }
        #[derive(Debug, PartialEq, Eq)]
        struct NoElement(u32);
        impl NonOptional for NoElement {}
        impl AceEncode for NoElement {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                NO_ELEMENT_ENCODE_ENTRIES.with(|count| count.set(count.get() + 1));
                e.enter_object()?;
                let encoded = e.begin_field(1).and_then(|()| self.0.ace_encode(e));
                let left = e.leave_object();
                encoded?;
                left?;
                Ok(())
            }
        }
        impl AceDecodeCyclic for NoElement {
            type Plan = u32;
            fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
                ctx.object(|ctx| ctx.field::<u32>(1))
            }
            fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
                Self(plan)
            }
        }
        for packed in [false, true] {
            NO_ELEMENT_ENCODE_ENTRIES.with(|count| count.set(0));
            let mut encoder = Encoder::new(&packed_opts(packed));
            let value = alloc::rc::Rc::new(Some(NoElement(37)));
            assert_eq!(value.ace_encode(&mut encoder), Err(Error::InvalidArgument));
            assert!(encoder.finish().is_err());
            assert_eq!(NO_ELEMENT_ENCODE_ENTRIES.with(Cell::get), 0);
        }
        for packed in [false, true] {
            NO_ELEMENT_ENCODE_ENTRIES.with(|count| count.set(0));
            let options = packed_opts(packed);
            let main = target_rec(1, &[0]);
            let assist = target_slot(0, 0, 1u64.to_le_bytes());
            let expected = target_raw(&main, &assist);
            assert_eq!(expected.len(), 33);
            let value = alloc::rc::Rc::new(None::<NoElement>);
            let bytes = r3_encode_layout(&value, &options, &expected);
            assert_eq!(NO_ELEMENT_ENCODE_ENTRIES.with(Cell::get), 0);
            let decoded: alloc::rc::Rc<Option<NoElement>> = decode_target_layout(&bytes, &options);
            assert_eq!(*decoded, None);
        }
    }
    #[test]
    fn r_s3_g_cov_d_default_target_single_non_object_record() {
        // 覆盖补测 COV-D（654）：默认目标只提交一条非对象 record，不得以单次提交冒充对象。
        struct NonObjectRecord;
        impl AceEncode for NonObjectRecord {
            fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
                e.begin_record()?;
                // write_element_bytes 需要元素作用域，与 element_message_with 同口径。
                e.begin_element();
                let written = e.write_element_bytes(&[1, 0, 0, 0]);
                e.end_element();
                written?;
                e.end_record()?;
                Ok(())
            }
        }
        // 先证明这条 record 本身可以提交，使下方的拒绝只能来自默认目标的对象守门。
        let mut direct = Encoder::new(&opts());
        assert_eq!(NonObjectRecord.ace_encode(&mut direct), Ok(()));
        let value = alloc::rc::Rc::new(NonObjectRecord);
        let mut encoder = Encoder::new(&opts());
        assert_eq!(value.ace_encode(&mut encoder), Err(Error::InvalidArgument));
    }
    /// 对象形态测试类型：只实现 `ace_encode` 与 `AceDecodeCyclic`，不覆写 `ace_encode_some`（FR-W04、AC-E01）。
    #[derive(Debug, PartialEq, Eq)]
    struct ShapeLeaf(u32);
    impl NonOptional for ShapeLeaf {}
    impl AceEncode for ShapeLeaf {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(1).and_then(|()| self.0.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for ShapeLeaf {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            ctx.object(|ctx| ctx.field::<u32>(1))
        }
        fn build_from_plan(_ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
            Self(plan)
        }
    }
    /// 直接拥有型字段：对象形态作为根对象的字段 0。
    #[derive(Debug, PartialEq, Eq)]
    struct ShapeHolder {
        leaf: ShapeLeaf,
    }
    impl AceEncode for ShapeHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.leaf.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for ShapeHolder {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            ctx.object(|ctx| ctx.field::<ShapeLeaf>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
            Self {
                leaf: ShapeLeaf::build_from_plan(ctx, plan),
            }
        }
    }
    /// 两级嵌套：根对象的字段 0 为 `ShapeHolder`，其字段 0 再为 `ShapeLeaf`。
    #[derive(Debug, PartialEq, Eq)]
    struct ShapeOuter {
        holder: ShapeHolder,
    }
    impl AceEncode for ShapeOuter {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.holder.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for ShapeOuter {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            ctx.object(|ctx| ctx.field::<ShapeHolder>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
            Self {
                holder: ShapeHolder::build_from_plan(ctx, plan),
            }
        }
    }
    /// `Option` 字段：`Some` 写入对象字段槽，`None` 不写槽。
    #[derive(Debug, PartialEq, Eq)]
    struct ShapeOptionHolder {
        leaf: Option<ShapeLeaf>,
    }
    impl AceEncode for ShapeOptionHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.leaf.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for ShapeOptionHolder {
        type Plan = Option<u32>;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Option<u32>> {
            ctx.object(|ctx| ctx.field::<Option<ShapeLeaf>>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Option<u32>) -> Self {
            Self {
                leaf: <Option<ShapeLeaf> as AceDecodeCyclic>::build_from_plan(ctx, plan),
            }
        }
    }
    /// `Rc` 字段（共享身份）：仅用于字节比对，字段槽只写目标 id。
    struct ShapeRcHolder {
        shared: alloc::rc::Rc<ShapeLeaf>,
    }
    impl AceEncode for ShapeRcHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.shared.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    /// `Vec` 与 `Box` 字段：元素为对象记录，仅用于字节比对。
    struct ShapeVecHolder {
        items: Vec<UserObj>,
    }
    impl AceEncode for ShapeVecHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.items.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    struct ShapeBoxHolder {
        item: alloc::boxed::Box<UserObj>,
    }
    impl AceEncode for ShapeBoxHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.item.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    // 循环解码往返：对象形态作字段时，子对象记录 id 须写入父对象字段槽（FR-W04、AC-E01）。
    fn shape_roundtrip<T: AceEncode + AceDecodeCyclic + PartialEq + core::fmt::Debug>(v: &T) {
        for packed in [false, true] {
            let bytes = packed_root(v, packed);
            let mut d = Decoder::new(&bytes, &packed_opts(packed)).unwrap();
            assert_eq!(&crate::decode::decode_cyclic::<T>(&mut d).unwrap(), v);
            d.finish().unwrap();
        }
    }
    #[test]
    fn field_owned_object_shape_roundtrip() {
        shape_roundtrip(&ShapeHolder { leaf: ShapeLeaf(7) });
    }
    #[test]
    fn field_two_level_object_shape_roundtrip() {
        shape_roundtrip(&ShapeOuter {
            holder: ShapeHolder { leaf: ShapeLeaf(7) },
        });
    }
    #[test]
    fn field_option_object_shape_some_roundtrip() {
        shape_roundtrip(&ShapeOptionHolder {
            leaf: Some(ShapeLeaf(7)),
        });
    }
    #[test]
    fn field_option_object_shape_none_roundtrip() {
        shape_roundtrip(&ShapeOptionHolder { leaf: None });
    }
    // 以下各例保持改动前字节：Rc 目标的记录与字段槽各一次；Vec 与 Box 的对象元素只经元素记录引用。
    #[test]
    fn field_rc_object_shape_bytes_unchanged() {
        let value = ShapeRcHolder {
            shared: alloc::rc::Rc::new(ShapeLeaf(7)),
        };
        let expected = target_raw(
            &[
                target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
                target_rec(2, &[2, 0, 0, 0, 1, 0, 0, 0]),
            ]
            .concat(),
            &[
                target_slot(0, 0, 2u64.to_le_bytes()),
                target_slot(2, 1, 7u64.to_le_bytes()),
                target_slot(1, 0, 1u64.to_le_bytes()),
            ]
            .concat(),
        );
        assert_eq!(packed_root(&value, false), expected);
    }
    #[test]
    fn field_vec_object_bytes_unchanged() {
        let value = ShapeVecHolder {
            items: alloc::vec![UserObj(7)],
        };
        let expected = target_raw(
            &[
                target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
                target_rec(2, &[1, 0, 0, 0, 1, 0, 0, 0]),
                target_rec(3, &[2, 0, 0, 0, 1, 0, 0, 0]),
            ]
            .concat(),
            &[
                target_slot(0, 0, 3u64.to_le_bytes()),
                target_slot(3, 1, 7u64.to_le_bytes()),
                target_slot(1, 0, 2u64.to_le_bytes()),
            ]
            .concat(),
        );
        assert_eq!(packed_root(&value, false), expected);
    }
    #[test]
    fn field_box_object_bytes_unchanged() {
        let value = ShapeBoxHolder {
            item: alloc::boxed::Box::new(UserObj(7)),
        };
        let expected = target_raw(
            &[
                target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
                target_rec(2, &[1, 0, 0, 0]),
                target_rec(3, &[2, 0, 0, 0, 1, 0, 0, 0]),
            ]
            .concat(),
            &[
                target_slot(0, 0, 3u64.to_le_bytes()),
                target_slot(3, 1, 7u64.to_le_bytes()),
                target_slot(1, 0, 2u64.to_le_bytes()),
            ]
            .concat(),
        );
        assert_eq!(packed_root(&value, false), expected);
    }
    #[test]
    fn root_object_shape_bytes_unchanged() {
        let expected = target_raw(
            &target_rec(1, &[1, 0, 0, 0, 1, 0, 0, 0]),
            &[
                target_slot(0, 0, 1u64.to_le_bytes()),
                target_slot(1, 1, 7u64.to_le_bytes()),
            ]
            .concat(),
        );
        assert_eq!(packed_root(&ShapeLeaf(7), false), expected);
    }
    // 字段对象的槽预算不足：离开子对象返回 ResourceLimit，且不分配子对象记录（R2）。
    #[test]
    fn field_object_slot_budget_failure_leaves_no_record() {
        let limited = Options {
            packed: false,
            max_slots: 2,
            ..Options::default()
        };
        // 根预留与子对象的字段槽占满两个槽位，父字段槽需要第三个槽位而超限。
        let run = || {
            let mut e = Encoder::new(&limited);
            e.enter_object().unwrap();
            e.begin_field(0).unwrap();
            assert_eq!(ShapeLeaf(7).ace_encode(&mut e), Err(Error::ResourceLimit));
            assert_eq!(e.leave_object(), Ok(1));
            e
        };
        assert_eq!(run().finish(), Err(Error::ResourceLimit));
        // 主区不得残留未被引用的子对象记录：输出与不含子对象的空根对象一致。
        let empty_root = target_raw(
            &target_rec(1, &[1, 0, 0, 0, 0, 0, 0, 0]),
            &target_slot(0, 0, 1u64.to_le_bytes()),
        );
        assert_eq!(run().finish_ignoring_latch(), Ok(empty_root));
    }
    /// 元素内含嵌套对象的测试元素：元素自身为对象，字段 0 为嵌套的 `ShapeLeaf` 对象。
    #[derive(Debug, PartialEq, Eq)]
    struct ShapeElemHolder {
        leaf: ShapeLeaf,
    }
    impl ShapeElemHolder {
        /// 对象编码：进入对象、写字段 0、离开对象并返回对象记录 id；字段失败仍先离开对象。
        fn encode_object(&self, e: &mut Encoder<'_>) -> Result<u32> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.leaf.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left
        }
    }
    impl AceEncode for ShapeElemHolder {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            self.encode_object(e).map(|_| ())
        }
        fn ace_encode_element(&self, e: &mut Encoder<'_>) -> Result<()> {
            let id = self.encode_object(e)?;
            e.write_element_bytes(&id.to_le_bytes())
        }
    }
    impl AceDecodeCyclic for ShapeElemHolder {
        type Plan = u32;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<u32> {
            ctx.object(|ctx| ctx.field::<ShapeLeaf>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: u32) -> Self {
            Self {
                leaf: ShapeLeaf::build_from_plan(ctx, plan),
            }
        }
    }
    /// `Box` 字段的根对象：字段 0 为装箱的元素对象。
    #[derive(Debug, PartialEq, Eq)]
    struct BoxElemRoot {
        holder: alloc::boxed::Box<ShapeElemHolder>,
    }
    impl AceEncode for BoxElemRoot {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.holder.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for BoxElemRoot {
        type Plan = alloc::boxed::Box<u32>;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| ctx.field::<alloc::boxed::Box<ShapeElemHolder>>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                holder: AceDecodeCyclic::build_from_plan(ctx, plan),
            }
        }
    }
    // 双层 Box 字段覆盖嵌套元素作用域；冗余分配检查对此类型为有意放行。
    #[allow(clippy::redundant_allocation)]
    #[derive(Debug, PartialEq, Eq)]
    struct BoxBoxElemRoot {
        holder: alloc::boxed::Box<alloc::boxed::Box<ShapeElemHolder>>,
    }
    impl AceEncode for BoxBoxElemRoot {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.holder.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for BoxBoxElemRoot {
        type Plan = alloc::boxed::Box<alloc::boxed::Box<u32>>;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| ctx.field::<alloc::boxed::Box<alloc::boxed::Box<ShapeElemHolder>>>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                holder: AceDecodeCyclic::build_from_plan(ctx, plan),
            }
        }
    }
    /// `Vec` 字段的根对象：字段 0 为元素对象序列。
    #[derive(Debug, PartialEq, Eq)]
    struct VecElemRoot {
        items: Vec<ShapeElemHolder>,
    }
    impl AceEncode for VecElemRoot {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.items.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for VecElemRoot {
        type Plan = Vec<u32>;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| ctx.field::<Vec<ShapeElemHolder>>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                items: AceDecodeCyclic::build_from_plan(ctx, plan),
            }
        }
    }
    /// 映射字段的根对象：字段 0 为以 `u32` 为键、元素对象为值的映射。
    #[derive(Debug, PartialEq, Eq)]
    struct BTreeElemRoot {
        map: alloc::collections::BTreeMap<u32, ShapeElemHolder>,
    }
    impl AceEncode for BTreeElemRoot {
        fn ace_encode(&self, e: &mut Encoder<'_>) -> Result<()> {
            e.enter_object()?;
            let encoded = e.begin_field(0).and_then(|()| self.map.ace_encode(e));
            let left = e.leave_object();
            encoded?;
            left?;
            Ok(())
        }
    }
    impl AceDecodeCyclic for BTreeElemRoot {
        type Plan = alloc::collections::BTreeMap<u32, u32>;
        fn decode_plan(ctx: &mut crate::decode::DecodeCtx) -> Result<Self::Plan> {
            ctx.object(|ctx| ctx.field::<alloc::collections::BTreeMap<u32, ShapeElemHolder>>(0))
        }
        fn build_from_plan(ctx: &mut crate::decode::BuildCtx<'_>, plan: Self::Plan) -> Self {
            Self {
                map: AceDecodeCyclic::build_from_plan(ctx, plan),
            }
        }
    }
    #[test]
    fn element_box_nested_object_roundtrip() {
        shape_roundtrip(&BoxElemRoot {
            holder: alloc::boxed::Box::new(ShapeElemHolder { leaf: ShapeLeaf(7) }),
        });
    }
    #[test]
    fn element_box_box_nested_object_roundtrip() {
        let inner = alloc::boxed::Box::new(ShapeElemHolder { leaf: ShapeLeaf(7) });
        shape_roundtrip(&BoxBoxElemRoot {
            holder: alloc::boxed::Box::new(inner),
        });
    }
    #[test]
    fn element_vec_nested_object_roundtrip() {
        shape_roundtrip(&VecElemRoot {
            items: alloc::vec![
                ShapeElemHolder { leaf: ShapeLeaf(7) },
                ShapeElemHolder { leaf: ShapeLeaf(8) },
            ],
        });
    }
    #[test]
    fn element_btree_nested_object_roundtrip() {
        shape_roundtrip(&BTreeElemRoot {
            map: alloc::collections::BTreeMap::from([(1, ShapeElemHolder { leaf: ShapeLeaf(7) })]),
        });
    }
}

#[cfg(test)]
mod inline_roundtrip_tests {
    use super::*;
    use crate::limits::Options;
    use core::fmt::Debug;
    fn check<T: AceEncode + AceDecode + PartialEq + Debug>(v: T) {
        for packed in [false, true] {
            let o = Options {
                packed,
                ..Options::default()
            };
            let mut e = Encoder::new(&o);
            v.ace_encode(&mut e).unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            assert_eq!(T::ace_decode(&mut d).unwrap(), v);
            d.finish().unwrap();
            let mut e = Encoder::new(&o);
            e.enter_object().unwrap();
            e.begin_field(1).unwrap();
            v.ace_encode(&mut e).unwrap();
            e.leave_object().unwrap();
            let bytes = e.finish().unwrap();
            let mut d = Decoder::new(&bytes, &o).unwrap();
            let root_scalar = d.read_root_scalar().unwrap();
            d.enter_object(u64::from_le_bytes(root_scalar)).unwrap();
            d.begin_field(1).unwrap();
            assert_eq!(T::ace_decode(&mut d).unwrap(), v);
            d.leave_object().unwrap();
            d.finish().unwrap();
        }
    }
    #[test]
    fn datetime_type_roundtrip() {
        check(value::DateTime {
            ticks: 42,
            kind: value::Kind::Local,
        });
    }
    #[test]
    fn intptr_type_roundtrip() {
        check(value::IntPtr(-1));
    }
    #[test]
    fn uintptr_type_roundtrip() {
        check(value::UIntPtr(u64::MAX));
    }
    #[test]
    fn toolkits_type_roundtrip() {
        check(value::ToolkitsU64(u64::MAX));
    }
    #[test]
    fn empty_string_type_roundtrip() {
        check(String::new());
    }
}
