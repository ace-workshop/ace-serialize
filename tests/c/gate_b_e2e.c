/* 门 B：cdylib 链接 + C 端到端一次完整往返。
 *
 * schema：
 *   gateb.flag      整数枚举 i32，变体集 {1, 2}
 *   gateb.child     {0: i32, 1: str, 2: enum<gateb.flag>}        ← 嵌套对象
 *   gateb.base      {0: i32, 1: str}                              ← 多态静态声明类型
 *   gateb.derived   {0: i32, 1: str, 2: i64}                      ← 多态派生：字段 2 只在这里
 *   gateb.holder    {0: obj<gateb.child>, 1: obj<gateb.child>,
 *                    2: list<obj<gateb.child>>, 3: obj<gateb.base>}
 *
 * 值：字段 0 与 1 指向**同一节点**（共享身份），字段 2 是 list，字段 3 的值自带
 * `gateb.derived` → 按自身 `type_key` 走 FR-W07 逃生口多态体。
 *
 * 断言：
 *   ① `ace_serialize` 成功、`cap == len`；
 *   ② `ace_deserialize` 成功；逐字段值与种类相等（含嵌套对象、list 两个元素、枚举判别值）；
 *   ③ 同一次解码内字段 0 与字段 1 的 `AceValue*` 满足 `ptr ==`（R32 共享身份）；
 *   ④ 多态位置读得到派生字段 2（基类 schema 没有该字段，读到 99/42 即证明按派生 key 分派）；
 *   ⑤ `ace_buffer_free` 后结构清零，`ace_value_free`、schema/type/enum/options 全部释放；
 *   ⑥ 晚失败（字段 0 已写出、字段 1/2 缺席）→ `15`，缓冲为空且随后仍可正常释放。
 *
 * 编译并运行（release cdylib）：
 *   cargo build -p ace-serialize-ffi --release
 *   cc -std=c11 -Wall -Wextra -Werror -I include tests/c/gate_b_e2e.c \
 *      -L target/release -lace_serialize_ffi -o target/release/gate_b_e2e
 *   ./target/release/gate_b_e2e
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

static AceValue *new_object(const char *type_key) {
    AceValue *value = NULL;
    CHECK(ace_value_object(type_key, &value) == ACE_OK && value != NULL);
    return value;
}

static AceValue *new_i64(int64_t number) {
    AceValue *value = NULL;
    CHECK(ace_value_i64(number, &value) == ACE_OK && value != NULL);
    return value;
}

static AceValue *new_str(const char *text) {
    AceValue *value = NULL;
    CHECK(ace_value_str((const uint8_t *)text, strlen(text), &value) == ACE_OK &&
          value != NULL);
    return value;
}

static AceValue *new_list(void) {
    AceValue *value = NULL;
    CHECK(ace_value_list(&value) == ACE_OK && value != NULL);
    return value;
}

/* set/add 成功即父容器持一份引用；装配代码随即归还调用方那份，不留多余句柄。 */
static void set_field(AceValue *object, int32_t field_id, AceValue *child) {
    CHECK(ace_object_set(object, field_id, child) == ACE_OK);
    CHECK(ace_value_free(child) == ACE_OK);
}

static void add_item(AceValue *list, AceValue *child) {
    CHECK(ace_list_add(list, child) == ACE_OK);
    CHECK(ace_value_free(child) == ACE_OK);
}

static int kind_of(const AceValue *value) {
    AceKind kind = (AceKind)-1;
    CHECK(ace_value_kind(value, &kind) == ACE_OK);
    return (int)kind;
}

static int64_t i64_of(const AceValue *value) {
    int64_t number = 0;
    CHECK(ace_value_get_i64(value, &number) == ACE_OK);
    return number;
}

static void check_text(const AceValue *value, const char *expected) {
    const uint8_t *bytes = NULL;
    size_t len = 0;
    size_t expected_len = strlen(expected);
    CHECK(ace_value_get_str(value, &bytes, &len) == ACE_OK);
    CHECK(bytes != NULL && len == expected_len && memcmp(bytes, expected, len) == 0);
}

/* 借用视图：字段值（`ace_object_get`）。 */
static const AceValue *field_of(const AceValue *object, int32_t field_id) {
    const AceValue *value = NULL;
    CHECK(ace_object_get(object, field_id, &value) == ACE_OK);
    return value;
}

/* 借用视图：列表元素（`ace_list_at`）。 */
static const AceValue *item_of(const AceValue *list, size_t index) {
    const AceValue *value = NULL;
    CHECK(ace_list_at(list, index, &value) == ACE_OK);
    return value;
}

/* `gateb.child` 值：0 → id，1 → name，2 → 枚举判别值 tag。 */
static AceValue *make_child(int64_t id, const char *name, int64_t tag) {
    AceValue *child = new_object(NULL);
    set_field(child, 0, new_i64(id));
    set_field(child, 1, new_str(name));
    set_field(child, 2, new_i64(tag));
    return child;
}

static AceSchema *make_text_schema(const char *type_key) {
    AceSchema *schema = NULL;
    CHECK(ace_schema_new(type_key, &schema) == ACE_OK && schema != NULL);
    AceTypeRef *number = NULL;
    CHECK(ace_type_prim(ACE_FK_I32, &number) == ACE_OK);
    CHECK(ace_schema_add_field(schema, 0, number) == ACE_OK);
    AceTypeRef *text = NULL;
    CHECK(ace_type_str(&text) == ACE_OK);
    CHECK(ace_schema_add_field(schema, 1, text) == ACE_OK);
    return schema;
}

int main(void) {
    /* —— 0. 版本与布局发布（门 A 冻结头 v4：AC-A01 要求 C 进程可查询）—— */
    {
        uint32_t s = ace_identity_entry_size();
        uint32_t a = ace_identity_entry_align();
        CHECK(ace_abi_version() == ACE_ABI_VERSION);
        CHECK(ace_wire_version() == ACE_WIRE_VERSION);
        CHECK(s > 0);
        CHECK(a > 0 && (a & (a - 1)) == 0); /* A 为 2 的幂 */
        CHECK(s % a == 0);
    }

    /* —— 1. schema 图：枚举 + 嵌套对象 + list + 多态基类/派生 —— */
    AceEnumSchema *flag = NULL;
    CHECK(ace_enum_new("gateb.flag", &flag) == ACE_OK && flag != NULL);
    CHECK(ace_enum_set_repr(flag, ACE_FK_I32) == ACE_OK);
    CHECK(ace_enum_add_variant(flag, 1) == ACE_OK);
    CHECK(ace_enum_add_variant(flag, 2) == ACE_OK);

    AceSchema *child = NULL;
    CHECK(ace_schema_new("gateb.child", &child) == ACE_OK && child != NULL);
    {
        AceTypeRef *number = NULL;
        CHECK(ace_type_prim(ACE_FK_I32, &number) == ACE_OK);
        CHECK(ace_schema_add_field(child, 0, number) == ACE_OK);
        AceTypeRef *text = NULL;
        CHECK(ace_type_str(&text) == ACE_OK);
        CHECK(ace_schema_add_field(child, 1, text) == ACE_OK);
        /* 借用枚举 schema：`gateb.child` 必须先于 `flag` 释放（头「释放顺序」）。 */
        AceTypeRef *kind = NULL;
        CHECK(ace_type_enum(flag, &kind) == ACE_OK);
        CHECK(ace_schema_add_field(child, 2, kind) == ACE_OK);
    }

    AceSchema *base = make_text_schema("gateb.base");

    AceSchema *derived = make_text_schema("gateb.derived");
    {
        AceTypeRef *extra = NULL;
        CHECK(ace_type_prim(ACE_FK_I64, &extra) == ACE_OK);
        CHECK(ace_schema_add_field(derived, 2, extra) == ACE_OK);
    }

    AceSchema *holder = NULL;
    CHECK(ace_schema_new("gateb.holder", &holder) == ACE_OK && holder != NULL);
    for (int32_t field_id = 0; field_id < 2; ++field_id) {
        AceTypeRef *object_type = NULL;
        CHECK(ace_type_object(child, &object_type) == ACE_OK);
        CHECK(ace_schema_add_field(holder, field_id, object_type) == ACE_OK);
    }
    {
        AceTypeRef *object_type = NULL;
        CHECK(ace_type_object(child, &object_type) == ACE_OK);
        AceTypeRef *list_type = NULL;
        CHECK(ace_type_list(object_type, &list_type) == ACE_OK);
        CHECK(ace_schema_add_field(holder, 2, list_type) == ACE_OK);
    }
    {
        AceTypeRef *object_type = NULL;
        CHECK(ace_type_object(base, &object_type) == ACE_OK);
        CHECK(ace_schema_add_field(holder, 3, object_type) == ACE_OK);
    }

    /* 未被任何 schema 收编的类型节点由调用方 `ace_type_free` 释放（覆盖 type 释放路径）。 */
    AceTypeRef *scratch = NULL;
    CHECK(ace_type_prim(ACE_FK_U64, &scratch) == ACE_OK && scratch != NULL);
    CHECK(ace_type_free(scratch) == ACE_OK);

    AceOptions *options = NULL;
    CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
    CHECK(ace_options_add_schema(options, holder) == ACE_OK);
    CHECK(ace_options_add_schema(options, child) == ACE_OK);
    CHECK(ace_options_add_schema(options, base) == ACE_OK);
    CHECK(ace_options_add_schema(options, derived) == ACE_OK);

    /* —— 2. 值：字段 0/1 是同一节点 —— */
    AceValue *shared = make_child(7, "seed", 2);
    AceValue *value = new_object("gateb.holder");
    CHECK(ace_object_set(value, 0, shared) == ACE_OK);
    CHECK(ace_object_set(value, 1, shared) == ACE_OK);
    /* 两份父引用已建立，调用方那份额外引用随即归还（头「set/list_add 所有权口径」）。 */
    CHECK(ace_value_free(shared) == ACE_OK);
    {
        const AceValue *first = field_of(value, 0);
        const AceValue *second = field_of(value, 1);
        CHECK(first != NULL && first == second); /* 编码前：同一节点 */
    }

    AceValue *items = new_list();
    add_item(items, make_child(11, "left", 1));
    add_item(items, make_child(12, "right", 1));
    set_field(value, 2, items);

    /* 多态位置：静态 schema 是 `obj<gateb.base>`，值自带 `gateb.derived`。 */
    {
        AceValue *node = new_object("gateb.derived");
        set_field(node, 0, new_i64(3));
        set_field(node, 1, new_str("poly"));
        set_field(node, 2, new_i64(99));
        set_field(value, 3, node);
    }

    /* —— 3. 序列化 → 反序列化 —— */
    AceBuffer buffer = {NULL, 0, 0};
    CHECK(ace_serialize("gateb.holder", value, options, &buffer) == ACE_OK);
    CHECK(buffer.ptr != NULL && buffer.len > 0);
    CHECK(buffer.cap == buffer.len);
    CHECK(ace_value_free(value) == ACE_OK);

    AceValue *decoded = NULL;
    CHECK(ace_deserialize("gateb.holder", buffer.ptr, buffer.len, options, &decoded) ==
          ACE_OK);
    CHECK(decoded != NULL);
    CHECK(ace_buffer_free(&buffer) == ACE_OK);
    CHECK(buffer.ptr == NULL && buffer.len == 0 && buffer.cap == 0); /* AC-A02 清零 */

    /* —— 4. 逐字段校验 + 共享 `ptr ==` + 多态往返 —— */
    CHECK(kind_of(decoded) == ACE_KIND_OBJECT);

    const AceValue *first = field_of(decoded, 0);
    const AceValue *second = field_of(decoded, 1);
    CHECK(first != NULL && first == second); /* 解码后：仍是同一节点 */
    CHECK(kind_of(first) == ACE_KIND_OBJECT);
    {
        const AceValue *id = field_of(first, 0);
        CHECK(kind_of(id) == ACE_KIND_I64 && i64_of(id) == 7);
        check_text(field_of(first, 1), "seed");
        const AceValue *tag = field_of(first, 2);
        CHECK(kind_of(tag) == ACE_KIND_I64 && i64_of(tag) == 2);
    }

    {
        const AceValue *listed = field_of(decoded, 2);
        size_t len = 0;
        CHECK(kind_of(listed) == ACE_KIND_LIST);
        CHECK(ace_list_len(listed, &len) == ACE_OK && len == 2);
        const AceValue *left = item_of(listed, 0);
        CHECK(kind_of(left) == ACE_KIND_OBJECT);
        CHECK(i64_of(field_of(left, 0)) == 11);
        check_text(field_of(left, 1), "left");
        CHECK(i64_of(field_of(left, 2)) == 1);
        const AceValue *right = item_of(listed, 1);
        CHECK(i64_of(field_of(right, 0)) == 12);
        check_text(field_of(right, 1), "right");
        CHECK(i64_of(field_of(right, 2)) == 1);
    }

    {
        const AceValue *polymorphic = field_of(decoded, 3);
        CHECK(kind_of(polymorphic) == ACE_KIND_OBJECT);
        const AceValue *id = field_of(polymorphic, 0);
        CHECK(kind_of(id) == ACE_KIND_I64 && i64_of(id) == 3);
        check_text(field_of(polymorphic, 1), "poly");
        /* 基类 schema 没有字段 2：读得到 99 就证明按派生 `type_key` 分派解码。 */
        const AceValue *extra = field_of(polymorphic, 2);
        CHECK(kind_of(extra) == ACE_KIND_I64 && i64_of(extra) == 99);
    }

    /* —— 5. 晚失败：字段 0 已写出记录，字段 1（str）/2（enum）缺席 → 15 ——
       alloc/free 配对的证据在 Rust 侧计数分配器探针（`allocation_and_free_pair_up_across_rounds`
       的 late-failure 轮），C 端此处只断言错误码、输出干净与句柄仍可正常归还。 */
    {
        AceValue *partial = new_object(NULL);
        set_field(partial, 0, new_i64(1));
        AceBuffer failed = {NULL, 0, 0};
        CHECK(ace_serialize("gateb.child", partial, options, &failed) ==
              ACE_MISSING_REQUIRED_FIELD);
        CHECK(failed.ptr == NULL && failed.len == 0 && failed.cap == 0);
        CHECK(ace_buffer_free(&failed) == ACE_OK); /* 已清空结构二次 free 安全（AC-A02） */
        CHECK(ace_value_free(partial) == ACE_OK);
    }

    /* —— 6. 全部句柄释放：值 → options → 引用方 schema → 被引用实体 → 枚举 —— */
    CHECK(ace_value_free(decoded) == ACE_OK);
    CHECK(ace_options_free(options) == ACE_OK);
    CHECK(ace_schema_free(holder) == ACE_OK);
    CHECK(ace_schema_free(derived) == ACE_OK);
    CHECK(ace_schema_free(base) == ACE_OK);
    CHECK(ace_schema_free(child) == ACE_OK);
    CHECK(ace_enum_free(flag) == ACE_OK);

    printf("gate_b_e2e: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
