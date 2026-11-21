#ifndef ACE_SERIALIZE_H
#define ACE_SERIALIZE_H

/* 本头为门 A 冻结候选 **v5**；门 A 已于 2026-11-20 关闭并登记；v5 按需求 v0.34 增补 `max_build_nesting` 对 C ABI 不适用的注释。 */

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 版本：仅查询协商用，不写入 wire，也不能从 wire 识别（FR-W11、FR-A05） */
#define ACE_ABI_VERSION  UINT32_C(0x00010000) /* 1.0.0 */
#define ACE_WIRE_VERSION UINT32_C(1)

/* 稳定错误码 0..16 闭集，只能末尾追加（FR-A05）；逐码语义见需求 §2.4 表 */
typedef enum {
    ACE_OK = 0,
    ACE_INVALID_ARGUMENT = 1,
    ACE_TRUNCATED_INPUT = 2,
    ACE_UNKNOWN_TYPE_KEY = 3,
    ACE_INVALID_TAG = 4,
    ACE_DEPTH_EXCEEDED = 5,
    ACE_RESOURCE_LIMIT = 6,
    ACE_INVALID_REFERENCE = 7,
    ACE_INVALID_UTF8 = 8,
    ACE_VERSION_MISMATCH = 9,
    ACE_PANIC_CAUGHT = 10,
    ACE_UNSUPPORTED = 11,
    ACE_NOT_FOUND = 12,
    ACE_TYPE_MISMATCH = 13,
    ACE_CYCLE_UNSUPPORTED = 14,
    ACE_MISSING_REQUIRED_FIELD = 15,
    ACE_DUPLICATE_KEY = 16        /* R24：Map 解码遇重复 key；Rust 独有码，C# 侧无对应 */
} AceError;

/* 动态值种类（FR-A01）；本期仅树状：无共享/环/Weak/别名/多态变体判别（FR-A00） */
typedef enum {
    ACE_KIND_NULL = 0, ACE_KIND_BOOL, ACE_KIND_I64, ACE_KIND_U64, ACE_KIND_F64,
    ACE_KIND_STR, ACE_KIND_BYTES, ACE_KIND_LIST, ACE_KIND_OBJECT
} AceKind;

/* schema 字段 kind（草案 v2；TBD-ABI-01/02 冻结门内）。
   具体宽度由 schema 决定，不按动态值种类一律 8B（FR-A03）。
   ace_type_prim 仅接受标量/宽值子集（ACE_FK_BOOL..ACE_FK_UINTPTR）；
   STR/BYTES 走 ace_type_str/ace_type_bytes；复合结构不再经 kind+补充函数表达，
   由 AceTypeRef 节点组合。ACE_FK_OPTION/LIST/MAP/OBJ/ENUM 保留作分类值：
   仅用于 ace_type_prim 入参拒绝与诊断，不是可经 ace_type_prim 构造的独立字段类型 */
typedef enum {
    ACE_FK_BOOL = 0, ACE_FK_I8, ACE_FK_U8, ACE_FK_I16, ACE_FK_U16,
    ACE_FK_I32, ACE_FK_U32, ACE_FK_I64, ACE_FK_U64,
    ACE_FK_F32, ACE_FK_F64, ACE_FK_CHAR,
    ACE_FK_STR, ACE_FK_BYTES,
    ACE_FK_GUID,                  /* 16B，RFC 4122 网络字节序（FR-T08①） */
    ACE_FK_DATETIME,              /* u64：低62位 ticks + 高2位 Kind（FR-T08②） */
    ACE_FK_DATETIME_OFFSET,       /* i64 clock ticks + i16 分钟偏移，10B（FR-T08③） */
    ACE_FK_INTPTR, ACE_FK_UINTPTR,/* 固定 8B，不随平台指针宽度（FR-T08④） */
    ACE_FK_ENUM,                  /* 整数枚举：由 AceEnumSchema 描述（repr/判别集/默认变体），经 ace_type_enum 引用 */
    ACE_FK_OPTION,                /* 分类值；结构由 ace_type_option(inner) 表达 */
    ACE_FK_LIST,                  /* 分类值；结构由 ace_type_list(elem) 表达 */
    ACE_FK_MAP,                   /* 分类值；ABI 表示 List<Object{k,v}>，结构由 ace_type_map(key,val) 表达 */
    ACE_FK_OBJ                    /* 分类值；目标 schema 由 ace_type_object(schema) 表达 */
} AceFieldKind;

typedef struct AceValue   AceValue;   /* opaque，Rust 分配释放 */
typedef struct AceOptions AceOptions; /* opaque */
typedef struct AceSchema  AceSchema;  /* opaque */
typedef struct AceTypeRef    AceTypeRef;     /* opaque 可组合类型引用节点；Rust 分配释放 */
typedef struct AceEnumSchema AceEnumSchema;  /* opaque 整数枚举 schema；Rust 分配释放 */

/* {ptr,len,cap} 由 Rust 维护；调用方禁止手工 free、篡改 cap、重解释为 Rust 对象（FR-A02） */
typedef struct { uint8_t *ptr; size_t len; size_t cap; } AceBuffer;

uint32_t ace_abi_version(void);
uint32_t ace_wire_version(void);

/* 身份条目字节数 S 与对齐 A（需求 R27）。值随目标平台与构建 feature 变化，故用函数发布；
   调用方按「N×S 按 A 对齐 + 字节区」计算 scratch 最小容量；零身份槽不加对齐前导。
   该值与冻结的 Entry 布局一致；改变属破坏性变更。 */
uint32_t ace_identity_entry_size(void);
uint32_t ace_identity_entry_align(void);

/* —— AceValue 构造（成功移交调用方；失败 out 置空）—— */
int32_t ace_value_null(AceValue **out);
int32_t ace_value_bool(uint8_t value, AceValue **out);          /* 仅 0/1，否则 1 */
int32_t ace_value_i64(int64_t value, AceValue **out);
int32_t ace_value_u64(uint64_t value, AceValue **out);
/* F64→f32 按位映射规则（写死）**：±0（sign 复制、其余清零）与 ±inf（sign + 全 1 指数）恒成功；NaN 仅当 f64 fraction 低 29 位全 0 时接受，映射 `f32 = sign<<31 | exp全1 | ((F>>29)&0x7FFFFF)`（quiet 位 F[51]→f32[22]，payload F[50..29]→f32[21..0]），低 29 位非 0 → `13`；有限值仅当可精确表示为 f32（含 subnormal；判据：转换后转回 f64 与原位模式逐位一致）才成功，否则 `13`。反向 f32→f64（wire f32 → AceValue F64）恒无损：sign 复制、quiet/payload 左移 29 位、低 29 位补 0，有限值恒精确，不产生 `13`。
   错误码 13 即 ACE_TYPE_MISMATCH。 */
int32_t ace_value_f64_bits(uint64_t bits, AceValue **out);      /* 位模式入参，不做宿主浮点运算 */
int32_t ace_value_str(const uint8_t *utf8, size_t len, AceValue **out);   /* 按长度，可含内嵌 NUL */
int32_t ace_value_bytes(const uint8_t *bytes, size_t len, AceValue **out);
int32_t ace_value_list(AceValue **out);
int32_t ace_value_object(const char *type_key, AceValue **out); /* 可 NULL：由 ace_serialize 外给 key */

/* 让 out 指向与 value 同一个节点、该节点引用计数 +1（R32）：用于表达“两个字段是同一对象”。
   失败三分类（与 FR-A02 一致）：(a) out == NULL → 1，不访问输出；
   (b) out 合法且参数可判定无效（如 value == NULL）→ 1，且 *out = NULL；
   (c) 伪造/悬垂/非本库分配的 value 不新增安全保证（沿用 FR-A02）。
   身份只承诺对象与引用类容器（list/map/str/bytes）；标量与宽值 record 不承诺（FR-T12②）。
   身份范围仅限一次消息/一次反序列化，调用方不得跨调用缓存指针。 */
int32_t ace_value_share(const AceValue *value, AceValue **out);

/* —— AceValue 读取（借用视图；12 NotFound = 合法查询无项，非解码截断）—— */
int32_t ace_value_kind(const AceValue *value, AceKind *out);
int32_t ace_value_get_bool(const AceValue *value, uint8_t *out);
int32_t ace_value_get_i64(const AceValue *value, int64_t *out);
int32_t ace_value_get_u64(const AceValue *value, uint64_t *out);
int32_t ace_value_get_f64_bits(const AceValue *value, uint64_t *out);
int32_t ace_value_get_str(const AceValue *value, const uint8_t **ptr, size_t *len);
int32_t ace_value_get_bytes(const AceValue *value, const uint8_t **ptr, size_t *len);

/* —— 对象/列表成员（R32⑤ 所有权口径）——
   set/list_add 成功 = 该父对 child 增加一个引用（引用计数 +1）：
   · 调用方句柄**仍然有效**，须由调用方自行 ace_value_free 减引用（不 free 即泄漏）；
   · 节点在仍有任何引用（父/调用方/其他共享方）时不会被释放；
   · 失败不增加引用，child 仍归调用方。
   替换同 field_id 时：库对旧值**减一次父引用**；失效只及**该父返回的借用视图**，
   调用方自有的拥有/共享引用只要仍有效，仍可读、可 ace_value_share、可 ace_value_free。
   不得把对象自身或其祖先 set/add 进任何对象（强环）：这是**调用方使用禁令**，不是未定义使用；
   若已构造，ace_serialize 仍返回 14 CycleUnsupported，ace_value_free 对强环按引用计数语义不保证释放。
   get/at 返回借用：寿命≤父、父修改/释放后失效、不得 free、不得 set/add 收编。
   field_id 即 order；reserved/缺席/未知 → 12；at 越界 → 12。 */
int32_t ace_object_set(AceValue *object, int32_t field_id, AceValue *child);
int32_t ace_object_get(const AceValue *object, int32_t field_id, const AceValue **out);
int32_t ace_list_add(AceValue *list, AceValue *child);
int32_t ace_list_len(const AceValue *list, size_t *out);
int32_t ace_list_at(const AceValue *list, size_t index, const AceValue **out);

/* —— 类型引用节点（FR-A03；草案 v2，TBD-ABI-01/02 门内）——
   AceTypeRef 是可组合类型引用节点，可任意嵌套表达 List<Option<i32>>、List<List<i32>>、
   Map<String,Object> 等；FR-A00"仅树状"约束解码产物 AceValue 值图拓扑，不限制本层描述能力。
   所有权：节点由库分配。ace_type_list/map/option 成功即收编 child 节点（子句柄失效、归父，
   不得再 free；失败未收编，child 仍归调用方）；未被收编的顶层节点由调用方 ace_type_free 释放。
   ace_type_object/ace_type_enum 对 AceSchema/AceEnumSchema 取借用（不收编）：被引用实体在
   构造与 add_field 期间必须存活，仍由调用方 ace_schema_free/ace_enum_free 释放。
   ace_type_free 仅对"未被任何 schema 收编"的节点合法；已收编节点随所属 schema 释放。 */
int32_t ace_type_prim(AceFieldKind kind, AceTypeRef **out);   /* 仅标量/宽值子集；复合/STR/BYTES/ENUM → 1 */
int32_t ace_type_bytes(AceTypeRef **out);
int32_t ace_type_str(AceTypeRef **out);
int32_t ace_type_list(AceTypeRef *elem, AceTypeRef **out);    /* 成功收编 elem */
int32_t ace_type_map(AceTypeRef *key, AceTypeRef *val, AceTypeRef **out); /* 收编 key/val；键须线序（对齐 BTreeMap K:Ord，FR-T03） */
int32_t ace_type_option(AceTypeRef *inner, AceTypeRef **out); /* 收编 inner；内层不得再为 option（FR-T02） */
int32_t ace_type_object(AceSchema *schema, AceTypeRef **out); /* 借用；schema 须已 ace_schema_new，否则 1 */
int32_t ace_type_enum(AceEnumSchema *enum_schema, AceTypeRef **out); /* 借用 */
int32_t ace_type_free(AceTypeRef *type_ref);                  /* 仅未被收编的节点 */

/* —— 整数枚举 schema（FR-T01；草案 v2）——
   repr 仅需求允许的 8 种定宽整数（i8/u8/i16/u16/i32/u32/i64/u64），其余 → 1；
   未显式 set_repr 按 i32（FR-T01）。
   变体判别值一律以 uint64_t 位模式承载（bit-pattern, not numeric conversion）：
   repr(i8)=-1 传 0xFF、repr(i32)=-1 传 0xFFFFFFFF、repr(u64)=0xFFFF_FFFF_FFFF_FFFF
   原值传递；读回/比较按本 enum 的 repr 宽度把 u64 截回对应位宽再比较，不做符号/数值换算。
   位模式合法性：传入位模式必须落在该 repr 位宽内（高位全 0，或为该位宽值的规范符号
   扩展），否则 → 1 InvalidArgument；重复 tag（截回后同值）→ 1。
   AceValue 无独立枚举 kind：枚举判别值经 I64/U64 以同一 uint64 位模式承载与读取
   （ace_value_get_i64/get_u64），截回 repr 宽度后与已注册变体集比较；解码遇到不在
   已注册变体集合内的判别值 → 4 InvalidTag。
   FR-A03 对普通 I64/U64→目标整数的无损范围转换规则原样保留，不经本条改变。
   set_default_variant 可选：仅当存在同 tag（截回后同值）变体，标记该变体可省略（FR-W04③）。 */
int32_t ace_enum_new(const char *type_key, AceEnumSchema **out);
int32_t ace_enum_free(AceEnumSchema *enum_schema);
int32_t ace_enum_set_repr(AceEnumSchema *enum_schema, AceFieldKind kind);
int32_t ace_enum_add_variant(AceEnumSchema *enum_schema, uint64_t tag);         /* 位模式承载；超位宽/重复 → 1 */
int32_t ace_enum_set_default_variant(AceEnumSchema *enum_schema, uint64_t tag); /* 位模式承载；可选 */

/* —— schema 白名单（FR-A03；草案 v2）——
   type_key：调用方维护的稳定 UTF-8、NUL 结尾、不含内嵌 NUL，不是 Rust/C# 类型名。
   add_field 的 field_id 即 order；重复 field_id → 1；成功即收编 field_type（子句柄失效、
   归 schema 所有、不得再 free；失败未收编，仍归调用方）。
   ace_options_add_schema 深拷贝 schema 图（含收编的类型节点与被借用的枚举 schema）进入库；
   重复 type_key → 1；复制完成后调用方仍可 free 全部调用方侧结构，不影响库内副本。
   未注册 type_key（含空白名单/NULL options）→ 3：fail-closed 全拒绝，理由见架构正文。
   释放顺序：ace_schema_free 释放 schema 连同其收编的全部字段类型节点；被
   ace_type_object/ace_type_enum 借用的 AceSchema/AceEnumSchema 必须晚于引用它的
   AceTypeRef（含已被其他 schema 收编者）释放，即先 free 引用方 schema，再 free 被引用实体。 */
int32_t ace_schema_new(const char *type_key, AceSchema **out);
int32_t ace_schema_free(AceSchema *schema);
int32_t ace_schema_add_field(AceSchema *schema, int32_t field_id, AceTypeRef *field_type);

/* —— options（Rust 管理句柄；配置值全部复制进入库；NULL options = 默认限额 + 空白名单）—— */
int32_t ace_options_new(AceOptions **out);
int32_t ace_options_free(AceOptions *options);
int32_t ace_options_set_packed(AceOptions *options, uint8_t enabled);
int32_t ace_options_set_limits(AceOptions *options, size_t depth, size_t input_bytes,
                               size_t raw_bytes, size_t decoded_bytes, size_t records,
                               size_t slots, size_t collection_len, size_t references,
                               size_t map_string_key_bytes);
/* 解码身份构造闭包峰值上限（需求 R25）；默认 256；超限 6 ResourceLimit；编码侧不使用该参数。
   **不适用于 C ABI**（需求 v0.34 增补）：`ace_deserialize` 不消费该值（C 侧只承诺树/共享 DAG，
   无强环、无 Weak，不存在身份构造闭包峰值）；本函数接受并保存该值，但不影响 C 解码结果与错误码，
   C 侧结构深度仍由 `max_depth` 约束（超限 5 DepthExceeded）；该语义对 Rust 图解码不变 */
int32_t ace_options_set_build_nesting(AceOptions *options, size_t limit);
int32_t ace_options_add_schema(AceOptions *options, const AceSchema *schema);

/* —— codec（FR-A01）。NULL+非零长度 → 1；零长度可 NULL；所有 out 必须>非 NULL，失败置空。
   type_key 与 object 自带 key 冲突 → 1；key 为 NULL 仅当 object 自带可解析 key；未知 → 3。
   失败输出必须为空、不泄漏半成品；成功移交调用方。 */
int32_t ace_serialize(const char *type_key, const AceValue *value,
                      const AceOptions *options, AceBuffer *out);
int32_t ace_deserialize(const char *type_key, const uint8_t *buf, size_t len,
                        const AceOptions *options, AceValue **out);

/* —— 释放：由 Rust 释放对应分配（FR-A02）——
   仅 AceBuffer 承诺 free 后清零结构、同一已清零结构二次 free 安全（AC-A02）；
   AceValue/AceOptions/AceTypeRef/AceSchema/AceEnumSchema 为拥有型句柄，只释放一次，
   二次释放非空句柄与伪造/悬垂同样不承诺安全；不因此改用二重指针签名。 */
int32_t ace_buffer_free(AceBuffer *buffer);
int32_t ace_value_free(AceValue *value);

#ifdef __cplusplus
}
#endif
#endif /* ACE_SERIALIZE_H */
