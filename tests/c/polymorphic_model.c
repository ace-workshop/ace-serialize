/* R33 多态对象真跑（AC-A06①②）：`List<基类>` 里装两个派生对象，各按**自身 `type_key`**
 * 查已注册 schema 编码，解码时按同一 `type_key` 解回对应派生 schema 的对象。
 *
 * schema：
 *   demo.cbase  {0: i32, 1: str}                     ← 列表元素的静态字段 schema
 *   demo.cnode_b{0: i32, 1: str, 2: i32}             ← 派生：字段 2 是 i32
 *   demo.cnode_c{0: i32, 1: str, 2: str}             ← 派生：字段 2 是 str
 *   demo.cholder{0: list<obj<demo.cbase>>}           ← 容器
 * 派生字段 2 的类型故意不同：若按父容器的静态元素 schema 统一写出，「字段 2 的种类」就会与派生
 * schema 不符（甚至读不到该字段），故种类断言能直接辨别分派是否按节点自身 `type_key`。
 *
 * 断言（raw 与 packed 两态各一遍）：
 *   ① 编码→解码成功；两个元素都是对象，基类字段（0、1）逐字段相等；
 *   ② 派生字段 2 按各自派生 schema 解出：元素 0 是 i32、元素 1 是 str，值与编码前相等；
 *   ③ raw 态另核对 wire：两条元素记录带 FR-W07 逃生口体头（`u16 tag == 65535`），
 *      且 `type_name` 记录里就是各自的 `type_key`——沿用既有 Rust 多态规则，未新造格式。
 *
 * 另加 I1（R33②）回归：元素显式自带**与父容器内嵌副本同 key**的 `type_key`（`demo.cbase`）时，
 * 只注册 `demo.cholder` 也必须返回 `3`（不得因同 key 复用内嵌 schema）；把 `demo.cbase` 独立
 * 注册后同形状成功，且 raw 态仍写普通对象体（没有逃生口多态头）。
 *
 * 再加 R36 四条预算/优先级回归（体头不再用记录视图试读）：8B 普通体预算 8 成功、7 → 6；多态体在
 * 精确预算（33）成功、32 → 6；共享重复引用不额外物化（16 成功、15 → 6）；`max_depth` 不足时仍返
 * `5`，不被体头路由/预算改成 `6`。
 *
 * 另加两条形状回归：①手写 wire 让 bytes 字段先物化、object 字段再引用同一条合法 14B 多态记录 →
 * `13`（多态 `Done` 快返不得跨种类复用）；②`demo.ihold` 内嵌 `demo.ibase{0: i32}`、白名单另注册
 * 同 key 的 `{0: str}` → 往返后字段的种类与值都按注册布局解回。
 *
 * 另加 R33⑥（v0.33）三条回归（无自身 `type_key` + 同 key 异布局 → 编码 fail-closed）：①`demo.khold`
 * 内嵌 `demo.klay{0: i32, 1: str}`、白名单另注册同 key 的 `{0: str}`，值**不带** key → 编码 `13`
 * 且 `*out` 为空缓冲；②同 key **同布局**（`demo.slay`）→ 编码成功且字节与「完全未注册」逐字节相同；
 * ③值**自带** key（`demo.klay`）→ 仍走注册目标（同 key 写普通体、无多态头），往返后字段种类
 * 与值正确。
 *
 * 编译并链接 Rust cdylib：
 *   cargo build -p ace-serialize-ffi
 *   cc -std=c17 -Wall -Wextra -Iinclude tests/c/polymorphic_model.c \
 *      -Ltarget/debug -lace_serialize_ffi -Wl,-rpath,"$PWD/target/debug" -o target/debug/polymorphic_model_c
 *   ./target/debug/polymorphic_model_c
 */
#include "ace_serialize.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int checks;
static int failures;

#define CHECK(cond)                                                            \
    do {                                                                       \
        ++checks;                                                              \
        if (!(cond)) {                                                         \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);     \
            ++failures;                                                        \
        }                                                                      \
    } while (0)

static uint16_t load_u16(const uint8_t *bytes) {
    return (uint16_t)((uint16_t)bytes[0] | ((uint16_t)bytes[1] << 8));
}

static int32_t load_i32(const uint8_t *bytes) {
    return (int32_t)((uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
                     ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24));
}

static uint32_t load_u32(const uint8_t *bytes) {
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
}

static void store_u32(uint8_t *bytes, uint32_t value) {
    bytes[0] = (uint8_t)value;
    bytes[1] = (uint8_t)(value >> 8);
    bytes[2] = (uint8_t)(value >> 16);
    bytes[3] = (uint8_t)(value >> 24);
}

/* 手写 wire 用：追加一条 main 记录 `[id u32][size i32][body]`，返回新的写入位置。 */
static size_t push_record(uint8_t *out, size_t at, uint32_t id, const uint8_t *body,
                          size_t size) {
    store_u32(out + at, id);
    store_u32(out + at + 4, (uint32_t)size);
    memcpy(out + at + 8, body, size);
    return at + 8 + size;
}

/* 手写 wire 用：追加一个 assist 槽 `[depth i32][tag i32][payload 8B]`（16B），返回新的位置。 */
static size_t push_slot(uint8_t *out, size_t at, int32_t depth, int32_t tag, uint64_t payload) {
    store_u32(out + at, (uint32_t)depth);
    store_u32(out + at + 4, (uint32_t)tag);
    for (int shift = 0; shift < 8; ++shift) {
        out[at + 8 + (size_t)shift] = (uint8_t)(payload >> (8 * shift));
    }
    return at + 16;
}

/* 未压缩信封（packed=false）：`[main_len i32][main][assist_len i32][assist]`。 */
static void unpack(const AceBuffer *buffer, const uint8_t **main, size_t *main_len) {
    *main_len = (size_t)load_i32(buffer->ptr);
    *main = buffer->ptr + 4;
}

/* 在 main 里找记录体为 `[u16 65535][u32 type_name id][u32 offset][u32 count]`（14B）的记录，
 * 且其 type_name 记录体恰为 `type_key`：返回命中条数。 */
static int polymorphic_records(const uint8_t *main, size_t main_len, const char *type_key) {
    size_t key_len = strlen(type_key);
    int hits = 0;
    size_t offset = 0;
    while (offset + 8 <= main_len) {
        size_t size = (size_t)load_i32(main + offset + 4);
        const uint8_t *body = main + offset + 8;
        if (size == 14 && load_u16(body) == 65535) {
            uint32_t name_id = load_u32(body + 2);
            /* type_name 记录：`[id u32][size i32][UTF-8 字节]`。 */
            uint32_t walk = 1;
            size_t name_offset = 0;
            int found = 0;
            while (walk <= main_len && found == 0) {
                uint32_t id = load_u32(main + name_offset);
                size_t name_size = (size_t)load_i32(main + name_offset + 4);
                if (id == name_id && name_size == key_len &&
                    memcmp(main + name_offset + 8, type_key, key_len) == 0) {
                    found = 1;
                }
                name_offset += 8 + name_size;
                ++walk;
                if (name_offset + 8 > main_len) {
                    break;
                }
            }
            hits += found;
        }
        offset += 8 + size;
    }
    return hits;
}

static AceValue *make_i64(int64_t number) {
    AceValue *value = NULL;
    CHECK(ace_value_i64(number, &value) == ACE_OK && value != NULL);
    return value;
}

static AceValue *make_str(const char *text) {
    AceValue *value = NULL;
    CHECK(ace_value_str((const uint8_t *)text, strlen(text), &value) == ACE_OK &&
          value != NULL);
    return value;
}

/* 携带自身 `type_key` 的对象值：字段 0/1 是基类字段，字段 2 是派生字段。 */
static AceValue *make_node(const char *type_key, int64_t id, const char *name,
                           AceValue *extra) {
    AceValue *value = NULL;
    CHECK(ace_value_object(type_key, &value) == ACE_OK && value != NULL);
    AceValue *number = make_i64(id);
    CHECK(ace_object_set(value, 0, number) == ACE_OK);
    CHECK(ace_value_free(number) == ACE_OK);
    AceValue *text = make_str(name);
    CHECK(ace_object_set(value, 1, text) == ACE_OK);
    CHECK(ace_value_free(text) == ACE_OK);
    CHECK(ace_object_set(value, 2, extra) == ACE_OK);
    CHECK(ace_value_free(extra) == ACE_OK);
    return value;
}

/* 携带自身 `type_key` 的基类值：只有基类的 0/1 字段（用来验证同 key 也要过白名单）。 */
static AceValue *make_base_node(int64_t id, const char *name) {
    AceValue *value = NULL;
    CHECK(ace_value_object("demo.cbase", &value) == ACE_OK && value != NULL);
    AceValue *number = make_i64(id);
    CHECK(ace_object_set(value, 0, number) == ACE_OK);
    CHECK(ace_value_free(number) == ACE_OK);
    AceValue *text = make_str(name);
    CHECK(ace_object_set(value, 1, text) == ACE_OK);
    CHECK(ace_value_free(text) == ACE_OK);
    return value;
}

static AceValue *make_list(void) {
    AceValue *value = NULL;
    CHECK(ace_value_list(&value) == ACE_OK && value != NULL);
    return value;
}

static void add_item(AceValue *list, AceValue *item) {
    CHECK(ace_list_add(list, item) == ACE_OK);
    CHECK(ace_value_free(item) == ACE_OK);
}

static const AceValue *field(const AceValue *object, int32_t field_id) {
    const AceValue *value = NULL;
    CHECK(ace_object_get(object, field_id, &value) == ACE_OK && value != NULL);
    return value;
}

static const AceValue *item(const AceValue *list, size_t index) {
    const AceValue *value = NULL;
    CHECK(ace_list_at(list, index, &value) == ACE_OK && value != NULL);
    return value;
}

static int64_t i64_of(const AceValue *value) {
    int64_t number = 0;
    CHECK(ace_value_get_i64(value, &number) == ACE_OK);
    return number;
}

static AceKind kind_of(const AceValue *value) {
    AceKind kind = ACE_KIND_NULL;
    CHECK(ace_value_kind(value, &kind) == ACE_OK);
    return kind;
}

static void check_text(const AceValue *value, const char *expected) {
    const uint8_t *bytes = NULL;
    size_t len = 0;
    CHECK(ace_value_get_str(value, &bytes, &len) == ACE_OK);
    CHECK(len == strlen(expected));
    CHECK(len == 0 || memcmp(bytes, expected, len) == 0);
}

/* —— R36 回归的夹具 ——
 *
 * `ace_options_set_limits` 一次写全九个维度，故只想改 `max_depth`/`max_decoded_bytes` 时，其余
 * 维度按 `Options::default()` 的公开默认值（`crates/ace-serialize/src/limits.rs`）显式传入。 */
#define LIMIT_DEPTH ((size_t)256)
#define LIMIT_INPUT_BYTES ((size_t)64 * 1024 * 1024)
#define LIMIT_RAW_BYTES ((size_t)64 * 1024 * 1024)
#define LIMIT_DECODED_BYTES ((size_t)64 * 1024 * 1024)
#define LIMIT_RECORDS ((size_t)1 << 20)
#define LIMIT_SLOTS ((size_t)1 << 20)
#define LIMIT_COLLECTION_LEN ((size_t)1 << 24)
#define LIMIT_REFERENCES ((size_t)1 << 20)
#define LIMIT_MAP_KEY_BYTES ((size_t)1024 * 1024)

/* 注册整组 schema，并只把 `depth`/`decoded_bytes` 按参数写入的 options（raw 态）。 */
static AceOptions *limit_options(const AceSchema *const *schemas, size_t schemas_len,
                                 size_t depth, size_t decoded_bytes) {
    AceOptions *options = NULL;
    CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
    CHECK(ace_options_set_packed(options, 0) == ACE_OK);
    for (size_t index = 0; index < schemas_len; ++index) {
        CHECK(ace_options_add_schema(options, schemas[index]) == ACE_OK);
    }
    CHECK(ace_options_set_limits(options, depth, LIMIT_INPUT_BYTES, LIMIT_RAW_BYTES,
                                 decoded_bytes, LIMIT_RECORDS, LIMIT_SLOTS,
                                 LIMIT_COLLECTION_LEN, LIMIT_REFERENCES,
                                 LIMIT_MAP_KEY_BYTES) == ACE_OK);
    return options;
}

/* 按给定限额解码一次；结果节点自持字段、不借用 options，故先释放 options 再交回结果。 */
static int32_t decode_with_limits(const char *type_key, const AceBuffer *buffer,
                                  const AceSchema *const *schemas, size_t schemas_len,
                                  size_t depth, size_t decoded_bytes, AceValue **out) {
    AceOptions *options = limit_options(schemas, schemas_len, depth, decoded_bytes);
    int32_t code = ace_deserialize(type_key, buffer->ptr, buffer->len, options, out);
    CHECK(ace_options_free(options) == ACE_OK);
    return code;
}

/* 只带字段 0 的 i32 对象；`type_key` 为 NULL 时沿用位置 schema（不写多态头）。 */
static AceValue *make_i32_object(const char *type_key, int64_t number) {
    AceValue *value = NULL;
    CHECK(ace_value_object(type_key, &value) == ACE_OK && value != NULL);
    AceValue *number_value = make_i64(number);
    CHECK(ace_object_set(value, 0, number_value) == ACE_OK);
    CHECK(ace_value_free(number_value) == ACE_OK);
    return value;
}

/* 不带 `type_key` 的对象值：字段 0 是 i64、字段 1 是 str（位置内嵌副本的布局）。 */
static AceValue *make_keyless(int64_t number, const char *text) {
    AceValue *value = NULL;
    CHECK(ace_value_object(NULL, &value) == ACE_OK && value != NULL);
    AceValue *number_value = make_i64(number);
    CHECK(ace_object_set(value, 0, number_value) == ACE_OK);
    CHECK(ace_value_free(number_value) == ACE_OK);
    AceValue *text_value = make_str(text);
    CHECK(ace_object_set(value, 1, text_value) == ACE_OK);
    CHECK(ace_value_free(text_value) == ACE_OK);
    return value;
}

/* 派生多态体 `demo.rder`：字段 0 是 i32、字段 1 是 str。 */
static AceValue *make_derived(int64_t number, const char *text) {
    AceValue *value = NULL;
    CHECK(ace_value_object("demo.rder", &value) == ACE_OK && value != NULL);
    AceValue *number_value = make_i64(number);
    CHECK(ace_object_set(value, 0, number_value) == ACE_OK);
    CHECK(ace_value_free(number_value) == ACE_OK);
    AceValue *text_value = make_str(text);
    CHECK(ace_object_set(value, 1, text_value) == ACE_OK);
    CHECK(ace_value_free(text_value) == ACE_OK);
    return value;
}

/* 单字段对象的容器；`child` 随本对象收编（与 `add_item` 同一口径）。 */
static AceValue *make_holder(const char *type_key, AceValue *child) {
    AceValue *value = NULL;
    CHECK(ace_value_object(type_key, &value) == ACE_OK && value != NULL);
    CHECK(ace_object_set(value, 0, child) == ACE_OK);
    CHECK(ace_value_free(child) == ACE_OK);
    return value;
}

int main(void) {
    /* —— schema —— */
    AceSchema *base = NULL;
    CHECK(ace_schema_new("demo.cbase", &base) == ACE_OK && base != NULL);
    AceTypeRef *i32_type = NULL;
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(base, 0, i32_type) == ACE_OK);
    AceTypeRef *str_type = NULL;
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(base, 1, str_type) == ACE_OK);

    AceSchema *derived_b = NULL;
    CHECK(ace_schema_new("demo.cnode_b", &derived_b) == ACE_OK && derived_b != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(derived_b, 0, i32_type) == ACE_OK);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(derived_b, 1, str_type) == ACE_OK);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(derived_b, 2, i32_type) == ACE_OK);

    AceSchema *derived_c = NULL;
    CHECK(ace_schema_new("demo.cnode_c", &derived_c) == ACE_OK && derived_c != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(derived_c, 0, i32_type) == ACE_OK);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(derived_c, 1, str_type) == ACE_OK);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(derived_c, 2, str_type) == ACE_OK);

    AceSchema *holder = NULL;
    CHECK(ace_schema_new("demo.cholder", &holder) == ACE_OK && holder != NULL);
    AceTypeRef *object_type = NULL;
    CHECK(ace_type_object(base, &object_type) == ACE_OK);
    AceTypeRef *list_type = NULL;
    CHECK(ace_type_list(object_type, &list_type) == ACE_OK);
    CHECK(ace_schema_add_field(holder, 0, list_type) == ACE_OK);

    /* —— raw 与 packed 两态各跑一遍 —— */
    for (int packed = 0; packed <= 1; ++packed) {
        AceOptions *options = NULL;
        CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
        CHECK(ace_options_set_packed(options, (uint8_t)packed) == ACE_OK);
        CHECK(ace_options_add_schema(options, base) == ACE_OK);
        CHECK(ace_options_add_schema(options, derived_b) == ACE_OK);
        CHECK(ace_options_add_schema(options, derived_c) == ACE_OK);
        CHECK(ace_options_add_schema(options, holder) == ACE_OK);

        AceValue *items = make_list();
        add_item(items, make_node("demo.cnode_b", 1, "b", make_i64(7)));
        add_item(items, make_node("demo.cnode_c", 2, "c", make_str("deep")));
        AceValue *value = NULL;
        CHECK(ace_value_object("demo.cholder", &value) == ACE_OK && value != NULL);
        CHECK(ace_object_set(value, 0, items) == ACE_OK);
        CHECK(ace_value_free(items) == ACE_OK);

        AceBuffer buffer = {NULL, 0, 0};
        CHECK(ace_serialize("demo.cholder", value, options, &buffer) == ACE_OK);
        CHECK(buffer.ptr != NULL && buffer.len > 0);
        CHECK(buffer.cap == buffer.len);
        CHECK(ace_value_free(value) == ACE_OK);

        if (packed == 0) {
            const uint8_t *main = NULL;
            size_t main_len = 0;
            unpack(&buffer, &main, &main_len);
            /* 两个元素各写一条多态记录，别名记录里就是各自的 `type_key`。 */
            CHECK(polymorphic_records(main, main_len, "demo.cnode_b") == 1);
            CHECK(polymorphic_records(main, main_len, "demo.cnode_c") == 1);
        }

        AceValue *decoded = NULL;
        CHECK(ace_deserialize("demo.cholder", buffer.ptr, buffer.len, options, &decoded) ==
              ACE_OK);
        CHECK(decoded != NULL);
        CHECK(ace_buffer_free(&buffer) == ACE_OK);

        const AceValue *elements = field(decoded, 0);
        CHECK(kind_of(elements) == ACE_KIND_LIST);
        size_t len = 0;
        CHECK(ace_list_len(elements, &len) == ACE_OK);
        CHECK(len == 2);

        /* 元素 0 → demo.cnode_b：基类字段 + 派生 i32 字段。 */
        const AceValue *node_b = item(elements, 0);
        CHECK(kind_of(node_b) == ACE_KIND_OBJECT);
        CHECK(i64_of(field(node_b, 0)) == 1);
        check_text(field(node_b, 1), "b");
        CHECK(kind_of(field(node_b, 2)) == ACE_KIND_I64);
        CHECK(i64_of(field(node_b, 2)) == 7);

        /* 元素 1 → demo.cnode_c：基类字段 + 派生 str 字段。 */
        const AceValue *node_c = item(elements, 1);
        CHECK(kind_of(node_c) == ACE_KIND_OBJECT);
        CHECK(i64_of(field(node_c, 0)) == 2);
        check_text(field(node_c, 1), "c");
        CHECK(kind_of(field(node_c, 2)) == ACE_KIND_STR);
        check_text(field(node_c, 2), "deep");

        /* 同一元素位置、两个派生 schema：字段 2 的种类不同即证明分派按节点自身 `type_key`。 */
        CHECK(kind_of(field(node_b, 2)) != kind_of(field(node_c, 2)));

        CHECK(ace_value_free(decoded) == ACE_OK);
        CHECK(ace_options_free(options) == ACE_OK);
    }

    /* —— I1（R33②）：元素显式自带 base 的 `type_key`，即使与父容器内嵌副本同 key 也要过白名单 —— */
    for (int packed = 0; packed <= 1; ++packed) {
        /* ① 白名单只有容器：内嵌的 base 副本不是注册项，元素的 `demo.cbase` → 3。 */
        AceOptions *embedded_only = NULL;
        CHECK(ace_options_new(&embedded_only) == ACE_OK && embedded_only != NULL);
        CHECK(ace_options_set_packed(embedded_only, (uint8_t)packed) == ACE_OK);
        CHECK(ace_options_add_schema(embedded_only, holder) == ACE_OK);
        AceValue *unregistered_items = make_list();
        add_item(unregistered_items, make_base_node(1, "base"));
        AceValue *unregistered_value = NULL;
        CHECK(ace_value_object("demo.cholder", &unregistered_value) == ACE_OK &&
              unregistered_value != NULL);
        CHECK(ace_object_set(unregistered_value, 0, unregistered_items) == ACE_OK);
        CHECK(ace_value_free(unregistered_items) == ACE_OK);
        AceBuffer rejected = {NULL, 0, 0};
        CHECK(ace_serialize("demo.cholder", unregistered_value, embedded_only, &rejected) ==
              ACE_UNKNOWN_TYPE_KEY);
        CHECK(rejected.ptr == NULL && rejected.len == 0 && rejected.cap == 0);
        CHECK(ace_value_free(unregistered_value) == ACE_OK);
        CHECK(ace_options_free(embedded_only) == ACE_OK);

        /* ② 对照：base 独立注册后同形状成功；同 key 不写多态头，解码按普通体还原。 */
        AceOptions *registered = NULL;
        CHECK(ace_options_new(&registered) == ACE_OK && registered != NULL);
        CHECK(ace_options_set_packed(registered, (uint8_t)packed) == ACE_OK);
        CHECK(ace_options_add_schema(registered, base) == ACE_OK);
        CHECK(ace_options_add_schema(registered, holder) == ACE_OK);
        AceValue *registered_items = make_list();
        add_item(registered_items, make_base_node(1, "base"));
        AceValue *registered_value = NULL;
        CHECK(ace_value_object("demo.cholder", &registered_value) == ACE_OK &&
              registered_value != NULL);
        CHECK(ace_object_set(registered_value, 0, registered_items) == ACE_OK);
        CHECK(ace_value_free(registered_items) == ACE_OK);
        AceBuffer same_key = {NULL, 0, 0};
        CHECK(ace_serialize("demo.cholder", registered_value, registered, &same_key) == ACE_OK);
        CHECK(same_key.ptr != NULL && same_key.len > 0);
        CHECK(ace_value_free(registered_value) == ACE_OK);
        if (packed == 0) {
            const uint8_t *main = NULL;
            size_t main_len = 0;
            unpack(&same_key, &main, &main_len);
            CHECK(polymorphic_records(main, main_len, "demo.cbase") == 0);
        }
        AceValue *same_key_decoded = NULL;
        CHECK(ace_deserialize("demo.cholder", same_key.ptr, same_key.len, registered,
                              &same_key_decoded) == ACE_OK);
        CHECK(ace_buffer_free(&same_key) == ACE_OK);
        const AceValue *element = item(field(same_key_decoded, 0), 0);
        CHECK(kind_of(element) == ACE_KIND_OBJECT);
        CHECK(i64_of(field(element, 0)) == 1);
        check_text(field(element, 1), "base");
        CHECK(ace_value_free(same_key_decoded) == ACE_OK);
        CHECK(ace_options_free(registered) == ACE_OK);
    }

    /* —— B2（R32④）：多态 `Done` 快返不得跨种类复用 ——
     *
     * 身份表既登记多态对象也登记 bytes/str/list/map。这里手写一份编码侧**拒绝**写出的 wire：
     *   demo.bhold{0: bytes, 1: obj<demo.bpos>} 的两个字段引用**同一个**记录 id，而该记录是
     *   合法的 14B 多态对象体（type_name = 已注册的 `demo.bshape`）。
     * 字段 0 先把它物化为 `Shape::Bytes`；字段 1 的体长 > 8 走多态路径——若快返无条件接受任意
     * 已完成形状，就会直接交出 bytes 节点（解码成功但种类错）。正确行为是 `13`，且不进入、不读
     * 名称、不收费。
     *
     * 槽布局：0 = 根槽（记录 1）；1/2 = 根的两个字段槽（都为记录 2）；3/4 = 多态体的字段槽。 */
    {
        uint8_t shared_body[8];
        store_u32(shared_body, 1);
        store_u32(shared_body + 4, 2);

        uint8_t poly_body[14];
        store_u32(poly_body, 65535);
        store_u32(poly_body + 2, 3);
        store_u32(poly_body + 6, 3);
        store_u32(poly_body + 10, 2);

        uint8_t main[128];
        size_t main_len = 0;
        main_len = push_record(main, main_len, 1, shared_body, sizeof(shared_body));
        main_len = push_record(main, main_len, 2, poly_body, sizeof(poly_body));
        main_len = push_record(main, main_len, 3, (const uint8_t *)"demo.bshape", 11);
        main_len = push_record(main, main_len, 4, (const uint8_t *)"ab", 2);

        uint8_t assist[128];
        size_t assist_len = 0;
        assist_len = push_slot(assist, assist_len, 0, 0, 1);
        assist_len = push_slot(assist, assist_len, 1, 0, 2);
        assist_len = push_slot(assist, assist_len, 1, 1, 2);
        assist_len = push_slot(assist, assist_len, 2, 0, 5);
        assist_len = push_slot(assist, assist_len, 2, 1, 4);

        uint8_t wire[256];
        store_u32(wire, (uint32_t)main_len);
        memcpy(wire + 4, main, main_len);
        size_t wire_len = 4 + main_len;
        store_u32(wire + wire_len, (uint32_t)assist_len);
        wire_len += 4;
        memcpy(wire + wire_len, assist, assist_len);
        wire_len += assist_len;

        AceSchema *bshape = NULL;
        CHECK(ace_schema_new("demo.bshape", &bshape) == ACE_OK && bshape != NULL);
        CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
        CHECK(ace_schema_add_field(bshape, 0, i32_type) == ACE_OK);
        CHECK(ace_type_str(&str_type) == ACE_OK);
        CHECK(ace_schema_add_field(bshape, 1, str_type) == ACE_OK);

        AceSchema *bpos = NULL;
        CHECK(ace_schema_new("demo.bpos", &bpos) == ACE_OK && bpos != NULL);
        CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
        CHECK(ace_schema_add_field(bpos, 0, i32_type) == ACE_OK);

        AceSchema *bhold = NULL;
        CHECK(ace_schema_new("demo.bhold", &bhold) == ACE_OK && bhold != NULL);
        AceTypeRef *bytes_type = NULL;
        CHECK(ace_type_bytes(&bytes_type) == ACE_OK);
        CHECK(ace_schema_add_field(bhold, 0, bytes_type) == ACE_OK);
        CHECK(ace_type_object(bpos, &object_type) == ACE_OK);
        CHECK(ace_schema_add_field(bhold, 1, object_type) == ACE_OK);

        AceOptions *b2_options = NULL;
        CHECK(ace_options_new(&b2_options) == ACE_OK && b2_options != NULL);
        CHECK(ace_options_set_packed(b2_options, 0) == ACE_OK);
        CHECK(ace_options_add_schema(b2_options, bshape) == ACE_OK);
        CHECK(ace_options_add_schema(b2_options, bhold) == ACE_OK);

        AceValue *b2_decoded = NULL;
        CHECK(ace_deserialize("demo.bhold", wire, wire_len, b2_options, &b2_decoded) ==
              ACE_TYPE_MISMATCH);
        CHECK(b2_decoded == NULL);

        CHECK(ace_options_free(b2_options) == ACE_OK);
        CHECK(ace_schema_free(bhold) == ACE_OK);
        CHECK(ace_schema_free(bpos) == ACE_OK);
        CHECK(ace_schema_free(bshape) == ACE_OK);
    }

    /* —— I1（R33②）：普通体同 key 的解码目标也是注册目标 ——
     *
     * `demo.ihold` 的字段 0 内嵌 `demo.ibase{0: i32}` 副本，白名单另注册**同 key** 的
     * `demo.ibase{0: str}`。子值自带该 key、字段 0 是 str：编码按注册的 str 布局写普通 8B 体
     * （同 key 不写多态头），解码必须按同一条规则解析到注册目标，否则会把 str 记录 id 当整数解。 */
    AceSchema *i_embedded = NULL;
    CHECK(ace_schema_new("demo.ibase", &i_embedded) == ACE_OK && i_embedded != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(i_embedded, 0, i32_type) == ACE_OK);

    AceSchema *i_registered = NULL;
    CHECK(ace_schema_new("demo.ibase", &i_registered) == ACE_OK && i_registered != NULL);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(i_registered, 0, str_type) == ACE_OK);

    AceSchema *i_holder = NULL;
    CHECK(ace_schema_new("demo.ihold", &i_holder) == ACE_OK && i_holder != NULL);
    CHECK(ace_type_object(i_embedded, &object_type) == ACE_OK);
    CHECK(ace_schema_add_field(i_holder, 0, object_type) == ACE_OK);

    for (int packed = 0; packed <= 1; ++packed) {
        AceOptions *options = NULL;
        CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
        CHECK(ace_options_set_packed(options, (uint8_t)packed) == ACE_OK);
        CHECK(ace_options_add_schema(options, i_holder) == ACE_OK);
        CHECK(ace_options_add_schema(options, i_registered) == ACE_OK);

        AceValue *child = NULL;
        CHECK(ace_value_object("demo.ibase", &child) == ACE_OK && child != NULL);
        AceValue *text = make_str("hi");
        CHECK(ace_object_set(child, 0, text) == ACE_OK);
        CHECK(ace_value_free(text) == ACE_OK);
        AceValue *holder_value = make_holder("demo.ihold", child);

        AceBuffer buffer = {NULL, 0, 0};
        CHECK(ace_serialize("demo.ihold", holder_value, options, &buffer) == ACE_OK);
        CHECK(buffer.ptr != NULL && buffer.len > 0);
        CHECK(ace_value_free(holder_value) == ACE_OK);
        if (packed == 0) {
            const uint8_t *main = NULL;
            size_t main_len = 0;
            unpack(&buffer, &main, &main_len);
            /* 同 key 仍写普通体：没有逃生口多态头，编码用的是注册的 str 布局。 */
            CHECK(polymorphic_records(main, main_len, "demo.ibase") == 0);
        }

        AceValue *decoded = NULL;
        CHECK(ace_deserialize("demo.ihold", buffer.ptr, buffer.len, options, &decoded) ==
              ACE_OK);
        CHECK(ace_buffer_free(&buffer) == ACE_OK);
        const AceValue *element = field(decoded, 0);
        CHECK(kind_of(element) == ACE_KIND_OBJECT);
        CHECK(kind_of(field(element, 0)) == ACE_KIND_STR);
        check_text(field(element, 0), "hi");
        CHECK(ace_value_free(decoded) == ACE_OK);
        CHECK(ace_options_free(options) == ACE_OK);
    }
    /* 引用方先释放：`i_holder` 借用了 `i_embedded`。 */
    CHECK(ace_schema_free(i_holder) == ACE_OK);
    CHECK(ace_schema_free(i_registered) == ACE_OK);
    CHECK(ace_schema_free(i_embedded) == ACE_OK);

    /* —— R33⑥（v0.33）：无自身 `type_key` + 同 key 异布局 → 编码 fail-closed `13` ——
     *
     * `demo.khold` 的字段 0 内嵌 `demo.klay{0: i32, 1: str}` 副本，白名单另注册**同 key** 的
     * `demo.klay{0: str}`（两个独立句柄 → 库内是两份布局不同的副本）。普通对象体（8B）在 wire 上
     * 不携带「值是否自带 `type_key`」，故值**不带** key 时位置内嵌布局与注册目标布局无法分辨：
     * ① 不带 key + 异布局 → 编码 `13`，`*out` 为空缓冲；
     * ② 不带 key + **同布局**（`demo.slay`）→ 成功，且字节与「完全未注册」时逐字节相同；
     * ③ 自带 key → 仍走注册目标（同 key 写普通体、无多态头），往返后字段种类与值正确。 */
    AceSchema *k_embedded = NULL;
    CHECK(ace_schema_new("demo.klay", &k_embedded) == ACE_OK && k_embedded != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(k_embedded, 0, i32_type) == ACE_OK);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(k_embedded, 1, str_type) == ACE_OK);

    AceSchema *k_registered = NULL;
    CHECK(ace_schema_new("demo.klay", &k_registered) == ACE_OK && k_registered != NULL);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(k_registered, 0, str_type) == ACE_OK);

    AceSchema *k_holder = NULL;
    CHECK(ace_schema_new("demo.khold", &k_holder) == ACE_OK && k_holder != NULL);
    CHECK(ace_type_object(k_embedded, &object_type) == ACE_OK);
    CHECK(ace_schema_add_field(k_holder, 0, object_type) == ACE_OK);

    AceOptions *k_options = NULL;
    CHECK(ace_options_new(&k_options) == ACE_OK && k_options != NULL);
    CHECK(ace_options_set_packed(k_options, 0) == ACE_OK);
    CHECK(ace_options_add_schema(k_options, k_holder) == ACE_OK);
    CHECK(ace_options_add_schema(k_options, k_registered) == ACE_OK);

    /* ① 无 key + 异布局：值恰按内嵌布局写，若无 R33⑥ 本会成功。 */
    AceValue *k_value = make_holder("demo.khold", make_keyless(711, "kx"));
    AceBuffer k_mismatch = {NULL, 0, 0};
    CHECK(ace_serialize("demo.khold", k_value, k_options, &k_mismatch) == ACE_TYPE_MISMATCH);
    CHECK(k_mismatch.ptr == NULL && k_mismatch.len == 0 && k_mismatch.cap == 0);
    CHECK(ace_buffer_free(&k_mismatch) == ACE_OK);
    CHECK(ace_value_free(k_value) == ACE_OK);

    /* ③ 自带 key + 异布局：仍按 R33② 走注册目标；注册布局的字段 0 是 str，内嵌副本是 i32，
     * 故解出的字段种类直接辨别用的是哪套布局。 */
    AceValue *k_keyed_child = NULL;
    CHECK(ace_value_object("demo.klay", &k_keyed_child) == ACE_OK && k_keyed_child != NULL);
    AceValue *k_text = make_str("hi");
    CHECK(ace_object_set(k_keyed_child, 0, k_text) == ACE_OK);
    CHECK(ace_value_free(k_text) == ACE_OK);
    AceValue *k_keyed = make_holder("demo.khold", k_keyed_child);
    AceBuffer k_bytes = {NULL, 0, 0};
    CHECK(ace_serialize("demo.khold", k_keyed, k_options, &k_bytes) == ACE_OK);
    CHECK(k_bytes.ptr != NULL && k_bytes.len > 0);
    CHECK(ace_value_free(k_keyed) == ACE_OK);
    {
        const uint8_t *main = NULL;
        size_t main_len = 0;
        unpack(&k_bytes, &main, &main_len);
        /* 同 key 仍写普通体：没有逃生口多态头。 */
        CHECK(polymorphic_records(main, main_len, "demo.klay") == 0);
    }
    AceValue *k_decoded = NULL;
    CHECK(ace_deserialize("demo.khold", k_bytes.ptr, k_bytes.len, k_options, &k_decoded) ==
          ACE_OK);
    CHECK(ace_buffer_free(&k_bytes) == ACE_OK);
    const AceValue *k_element = field(k_decoded, 0);
    CHECK(kind_of(k_element) == ACE_KIND_OBJECT);
    CHECK(kind_of(field(k_element, 0)) == ACE_KIND_STR);
    check_text(field(k_element, 0), "hi");
    CHECK(ace_value_free(k_decoded) == ACE_OK);

    /* ② 无 key + 同布局：内嵌副本与注册副本都是 `{0: i32, 1: str}`（句柄仍不同）。 */
    AceSchema *s_embedded = NULL;
    CHECK(ace_schema_new("demo.slay", &s_embedded) == ACE_OK && s_embedded != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(s_embedded, 0, i32_type) == ACE_OK);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(s_embedded, 1, str_type) == ACE_OK);

    AceSchema *s_registered = NULL;
    CHECK(ace_schema_new("demo.slay", &s_registered) == ACE_OK && s_registered != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(s_registered, 0, i32_type) == ACE_OK);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(s_registered, 1, str_type) == ACE_OK);

    AceSchema *s_holder = NULL;
    CHECK(ace_schema_new("demo.shold", &s_holder) == ACE_OK && s_holder != NULL);
    CHECK(ace_type_object(s_embedded, &object_type) == ACE_OK);
    CHECK(ace_schema_add_field(s_holder, 0, object_type) == ACE_OK);

    AceOptions *s_plain = NULL;
    CHECK(ace_options_new(&s_plain) == ACE_OK && s_plain != NULL);
    CHECK(ace_options_set_packed(s_plain, 0) == ACE_OK);
    CHECK(ace_options_add_schema(s_plain, s_holder) == ACE_OK);

    AceOptions *s_complete = NULL;
    CHECK(ace_options_new(&s_complete) == ACE_OK && s_complete != NULL);
    CHECK(ace_options_set_packed(s_complete, 0) == ACE_OK);
    CHECK(ace_options_add_schema(s_complete, s_holder) == ACE_OK);
    CHECK(ace_options_add_schema(s_complete, s_registered) == ACE_OK);

    AceValue *s_value = make_holder("demo.shold", make_keyless(722, "sx"));
    AceBuffer s_plain_bytes = {NULL, 0, 0};
    CHECK(ace_serialize("demo.shold", s_value, s_plain, &s_plain_bytes) == ACE_OK);
    AceBuffer s_registered_bytes = {NULL, 0, 0};
    CHECK(ace_serialize("demo.shold", s_value, s_complete, &s_registered_bytes) == ACE_OK);
    CHECK(ace_value_free(s_value) == ACE_OK);
    CHECK(s_plain_bytes.len > 0);
    CHECK(s_plain_bytes.len == s_registered_bytes.len);
    CHECK(s_plain_bytes.ptr != NULL && s_registered_bytes.ptr != NULL);
    CHECK(memcmp(s_plain_bytes.ptr, s_registered_bytes.ptr, s_plain_bytes.len) == 0);

    /* 同 key 同布局下解码也成立：注册目标与内嵌布局一致，字段种类与值不变。 */
    AceValue *s_decoded = NULL;
    CHECK(ace_deserialize("demo.shold", s_registered_bytes.ptr, s_registered_bytes.len,
                          s_complete, &s_decoded) == ACE_OK);
    const AceValue *s_element = field(s_decoded, 0);
    CHECK(kind_of(s_element) == ACE_KIND_OBJECT);
    CHECK(kind_of(field(s_element, 0)) == ACE_KIND_I64);
    CHECK(i64_of(field(s_element, 0)) == 722);
    CHECK(kind_of(field(s_element, 1)) == ACE_KIND_STR);
    check_text(field(s_element, 1), "sx");
    CHECK(ace_value_free(s_decoded) == ACE_OK);
    CHECK(ace_buffer_free(&s_registered_bytes) == ACE_OK);
    CHECK(ace_buffer_free(&s_plain_bytes) == ACE_OK);
    CHECK(ace_options_free(s_complete) == ACE_OK);
    CHECK(ace_options_free(s_plain) == ACE_OK);
    CHECK(ace_options_free(k_options) == ACE_OK);
    /* 引用方先释放：`k_holder` 借用了 `k_embedded`、`s_holder` 借用了 `s_embedded`。 */
    CHECK(ace_schema_free(k_holder) == ACE_OK);
    CHECK(ace_schema_free(k_registered) == ACE_OK);
    CHECK(ace_schema_free(k_embedded) == ACE_OK);
    CHECK(ace_schema_free(s_holder) == ACE_OK);
    CHECK(ace_schema_free(s_registered) == ACE_OK);
    CHECK(ace_schema_free(s_embedded) == ACE_OK);

    /* —— R36：预算按实际物化字节计费，体头路由不得重复收费、不得改写错误优先级 ——
     *
     * 体头路由改用 `Decoder::record_body_len`（不计入 `max_decoded_bytes`、不物化、不改游标）。
     * 旧实现用 `begin_record_view` + 试读 9 字节判体头，而记录视图按**完整体长**收费：同一对象体
     * 被「探测 + 进入」各记一次，并在核心入口的深度判定之前先报 `6`。以下四条即该缺陷的回归
     * （③ 另加多态体子例 ③b：同一派生节点被两处引用）。
     * 物化预算与传输态无关（packed 只影响输入/raw 预算），故本节只在 raw 态跑。 */
    AceSchema *r1 = NULL;
    CHECK(ace_schema_new("demo.r1", &r1) == ACE_OK && r1 != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(r1, 0, i32_type) == ACE_OK);

    /* 多态元素的静态位置 schema。 */
    AceSchema *rbase = NULL;
    CHECK(ace_schema_new("demo.rbase", &rbase) == ACE_OK && rbase != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(rbase, 0, i32_type) == ACE_OK);

    /* 派生多态体：多一个 str 字段。 */
    AceSchema *rder = NULL;
    CHECK(ace_schema_new("demo.rder", &rder) == ACE_OK && rder != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(rder, 0, i32_type) == ACE_OK);
    CHECK(ace_type_str(&str_type) == ACE_OK);
    CHECK(ace_schema_add_field(rder, 1, str_type) == ACE_OK);

    /* 单字段容器：多态体与深度两组共用。 */
    AceSchema *rhold = NULL;
    CHECK(ace_schema_new("demo.rhold", &rhold) == ACE_OK && rhold != NULL);
    CHECK(ace_type_object(rbase, &object_type) == ACE_OK);
    CHECK(ace_schema_add_field(rhold, 0, object_type) == ACE_OK);

    /* 两个字段都指向同一个位置 schema：共享重复引用。 */
    AceSchema *sshold = NULL;
    CHECK(ace_schema_new("demo.sshold", &sshold) == ACE_OK && sshold != NULL);
    CHECK(ace_type_object(rbase, &object_type) == ACE_OK);
    CHECK(ace_schema_add_field(sshold, 0, object_type) == ACE_OK);
    CHECK(ace_type_object(rbase, &object_type) == ACE_OK);
    CHECK(ace_schema_add_field(sshold, 1, object_type) == ACE_OK);

    const AceSchema *r36_schemas[] = {r1, rbase, rder, rhold, sshold};
    const size_t r36_count = sizeof(r36_schemas) / sizeof(r36_schemas[0]);

    /* 编码用默认限额的 options（`max_decoded_bytes` 只作用于解码）。 */
    AceOptions *r36_encode = limit_options(r36_schemas, r36_count, LIMIT_DEPTH,
                                          LIMIT_DECODED_BYTES);

    /* ① 8B 普通对象：根自带同 key，故写普通体（8B）；预算恰为 8 成功，差 1 → 6。 */
    AceValue *plain_value = make_i32_object("demo.r1", 42);
    AceBuffer plain_wire = {NULL, 0, 0};
    CHECK(ace_serialize("demo.r1", plain_value, r36_encode, &plain_wire) == ACE_OK);
    CHECK(plain_wire.ptr != NULL && plain_wire.len > 0);
    CHECK(ace_value_free(plain_value) == ACE_OK);
    AceValue *plain_decoded = NULL;
    CHECK(decode_with_limits("demo.r1", &plain_wire, r36_schemas, r36_count, LIMIT_DEPTH, 8,
                             &plain_decoded) == ACE_OK);
    CHECK(plain_decoded != NULL && i64_of(field(plain_decoded, 0)) == 42);
    CHECK(ace_value_free(plain_decoded) == ACE_OK);
    AceValue *plain_short = NULL;
    CHECK(decode_with_limits("demo.r1", &plain_wire, r36_schemas, r36_count, LIMIT_DEPTH, 7,
                             &plain_short) == ACE_RESOURCE_LIMIT);
    CHECK(plain_short == NULL);
    CHECK(ace_buffer_free(&plain_wire) == ACE_OK);

    /* ② 多态体精确预算：root 体 8 + 多态体 14 + type_name 记录 9（"demo.rder"）+ str 体 2
     * = 33；33 成功，32 → 6。 */
    AceValue *poly_value = make_holder("demo.rhold", make_derived(5, "ab"));
    AceBuffer poly_wire = {NULL, 0, 0};
    CHECK(ace_serialize("demo.rhold", poly_value, r36_encode, &poly_wire) == ACE_OK);
    CHECK(poly_wire.ptr != NULL && poly_wire.len > 0);
    CHECK(ace_value_free(poly_value) == ACE_OK);
    AceValue *poly_decoded = NULL;
    CHECK(decode_with_limits("demo.rhold", &poly_wire, r36_schemas, r36_count, LIMIT_DEPTH, 33,
                             &poly_decoded) == ACE_OK);
    CHECK(poly_decoded != NULL);
    CHECK(i64_of(field(field(poly_decoded, 0), 0)) == 5);
    check_text(field(field(poly_decoded, 0), 1), "ab");
    CHECK(ace_value_free(poly_decoded) == ACE_OK);
    AceValue *poly_short = NULL;
    CHECK(decode_with_limits("demo.rhold", &poly_wire, r36_schemas, r36_count, LIMIT_DEPTH, 32,
                             &poly_short) == ACE_RESOURCE_LIMIT);
    CHECK(poly_short == NULL);
    CHECK(ace_buffer_free(&poly_wire) == ACE_OK);

    /* ③ 共享重复引用：同一节点被两处引用，wire 只一条 record；物化字节 = root 体 8 +
     * 被引用对象体 8 = 16；16 成功（第二处命中身份表，不重建也不收费），15 → 6。 */
    AceValue *shared_child = make_i32_object("demo.rbase", 7);
    AceValue *shared_value = NULL;
    CHECK(ace_value_object("demo.sshold", &shared_value) == ACE_OK && shared_value != NULL);
    CHECK(ace_object_set(shared_value, 0, shared_child) == ACE_OK);
    CHECK(ace_object_set(shared_value, 1, shared_child) == ACE_OK);
    CHECK(ace_value_free(shared_child) == ACE_OK);
    AceBuffer shared_wire = {NULL, 0, 0};
    CHECK(ace_serialize("demo.sshold", shared_value, r36_encode, &shared_wire) == ACE_OK);
    CHECK(shared_wire.ptr != NULL && shared_wire.len > 0);
    CHECK(ace_value_free(shared_value) == ACE_OK);
    AceValue *shared_decoded = NULL;
    CHECK(decode_with_limits("demo.sshold", &shared_wire, r36_schemas, r36_count, LIMIT_DEPTH,
                             16, &shared_decoded) == ACE_OK);
    CHECK(shared_decoded != NULL);
    /* 指针身份：两处引用解出同一节点（故第二次没有物化，也没重复计费）。 */
    CHECK(field(shared_decoded, 0) == field(shared_decoded, 1));
    CHECK(i64_of(field(field(shared_decoded, 0), 0)) == 7);
    CHECK(ace_value_free(shared_decoded) == ACE_OK);
    AceValue *shared_short = NULL;
    CHECK(decode_with_limits("demo.sshold", &shared_wire, r36_schemas, r36_count, LIMIT_DEPTH,
                             15, &shared_short) == ACE_RESOURCE_LIMIT);
    CHECK(shared_short == NULL);
    CHECK(ace_buffer_free(&shared_wire) == ACE_OK);

    /* ③b 共享重复引用 + 多态体：同一派生节点被两处引用，第二处命中身份表后不得重新进入对象、
     * 不读 `type_name`、不再收费——预算与单次引用相同（③ 的同一容器，物化字节 = root 8 +
     * 多态体 14 + type_name 记录 9 + str 体 2 = 33）。 */
    AceValue *shared_poly = make_derived(9, "ab");
    AceValue *shared_poly_value = NULL;
    CHECK(ace_value_object("demo.sshold", &shared_poly_value) == ACE_OK &&
          shared_poly_value != NULL);
    CHECK(ace_object_set(shared_poly_value, 0, shared_poly) == ACE_OK);
    CHECK(ace_object_set(shared_poly_value, 1, shared_poly) == ACE_OK);
    CHECK(ace_value_free(shared_poly) == ACE_OK);
    AceBuffer shared_poly_wire = {NULL, 0, 0};
    CHECK(ace_serialize("demo.sshold", shared_poly_value, r36_encode, &shared_poly_wire) ==
          ACE_OK);
    CHECK(shared_poly_wire.ptr != NULL && shared_poly_wire.len > 0);
    CHECK(ace_value_free(shared_poly_value) == ACE_OK);
    AceValue *shared_poly_decoded = NULL;
    CHECK(decode_with_limits("demo.sshold", &shared_poly_wire, r36_schemas, r36_count,
                             LIMIT_DEPTH, 33, &shared_poly_decoded) == ACE_OK);
    CHECK(shared_poly_decoded != NULL);
    CHECK(field(shared_poly_decoded, 0) == field(shared_poly_decoded, 1));
    CHECK(i64_of(field(field(shared_poly_decoded, 0), 0)) == 9);
    check_text(field(field(shared_poly_decoded, 0), 1), "ab");
    CHECK(ace_value_free(shared_poly_decoded) == ACE_OK);
    AceValue *shared_poly_short = NULL;
    CHECK(decode_with_limits("demo.sshold", &shared_poly_wire, r36_schemas, r36_count,
                             LIMIT_DEPTH, 32, &shared_poly_short) == ACE_RESOURCE_LIMIT);
    CHECK(shared_poly_short == NULL);
    CHECK(ace_buffer_free(&shared_poly_wire) == ACE_OK);

    CHECK(ace_options_free(r36_encode) == ACE_OK);

    /* ④ 深度错误优先：`max_depth = 0` 只容根对象，嵌套对象在 k = 1 处 → 5。预算恰为根体 8 时
     * 也不得被体头路由/预算改写为 6（旧法在路由时先花 8B，故 8 会报 6）。 */
    AceValue *deep_value = make_holder("demo.rhold", make_i32_object(NULL, 3));
    AceBuffer deep_wire = {NULL, 0, 0};
    AceOptions *deep_encode = limit_options(r36_schemas, r36_count, LIMIT_DEPTH,
                                           LIMIT_DECODED_BYTES);
    CHECK(ace_serialize("demo.rhold", deep_value, deep_encode, &deep_wire) == ACE_OK);
    CHECK(ace_value_free(deep_value) == ACE_OK);
    CHECK(ace_options_free(deep_encode) == ACE_OK);
    AceValue *deep_tight = NULL;
    CHECK(decode_with_limits("demo.rhold", &deep_wire, r36_schemas, r36_count, 0, 8,
                             &deep_tight) == ACE_DEPTH_EXCEEDED);
    CHECK(deep_tight == NULL);
    AceValue *deep_loose = NULL;
    CHECK(decode_with_limits("demo.rhold", &deep_wire, r36_schemas, r36_count, 0,
                             LIMIT_DECODED_BYTES, &deep_loose) == ACE_DEPTH_EXCEEDED);
    CHECK(deep_loose == NULL);
    CHECK(ace_buffer_free(&deep_wire) == ACE_OK);

    /* 引用方先释放：`rhold`/`sshold` 借用了 `rbase`。 */
    CHECK(ace_schema_free(sshold) == ACE_OK);
    CHECK(ace_schema_free(rhold) == ACE_OK);
    CHECK(ace_schema_free(rder) == ACE_OK);
    CHECK(ace_schema_free(rbase) == ACE_OK);
    CHECK(ace_schema_free(r1) == ACE_OK);

    CHECK(ace_schema_free(holder) == ACE_OK);
    CHECK(ace_schema_free(derived_c) == ACE_OK);
    CHECK(ace_schema_free(derived_b) == ACE_OK);
    CHECK(ace_schema_free(base) == ACE_OK);

    printf("polymorphic_model: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
