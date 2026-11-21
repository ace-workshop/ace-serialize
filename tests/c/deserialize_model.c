/* P3-6 `ace_deserialize` 真跑：注册 `demo.cdeser`（0、1 两个字段都是 `obj<demo.cinner>`）与
 * `demo.cinner`（0 → i32），构造「两处指向同一对象」的值，编码后反序列化，断言：
 *   ① 同一次反序列化内两处字段读出的 `AceValue*` 的 `ptr ==`、`kind` 一致、内层字段值一致
 *      （R32②③、AC-A05③）；
 *   ② 再解一次并让两份结果同时存活——各自内部共享成立，**跨结果 `ptr !=`**（AC-A05⑤：
 *      `ace_deserialize` 不得跨调用缓存节点）；
 *   ③ 未注册 `type_key` → 3、失败输出为 NULL（FR-A02）、`ace_value_free` 正常；
 *   ④ 元素位置的对象（`list<object>`）元素体是 4B 记录 id：非环往返成功；若读错成外层槽载荷，
 *      该元素会命中外层 list 的「解码中」标记而误报 14。元素对象用零字段 schema，保留为接缝前
 *      的最小证据（只钉元素体读法，不涉字段槽深度）；
 *   ⑤ 集合内**带字段**对象（`list<obj{demo.cinner}>`）：编码→解码成功、逐字段相等，不得是
 *      4（字段槽 depth 差 1）也不得是 14（R35 解码集合深度接缝）。
 *
 * 编译并链接 Rust cdylib：
 *   cargo build -p ace-serialize-ffi
 *   cc -std=c17 -Wall -Wextra -Iinclude tests/c/deserialize_model.c \
 *      -Ltarget/debug -lace_serialize_ffi -Wl,-rpath,"$PWD/target/debug" -o target/debug/deserialize_model_c
 *   ./target/debug/deserialize_model_c
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

static AceValue *make_object(void) {
    AceValue *value = NULL;
    CHECK(ace_value_object(NULL, &value) == ACE_OK && value != NULL);
    return value;
}

static AceValue *make_i64(int64_t number) {
    AceValue *value = NULL;
    CHECK(ace_value_i64(number, &value) == ACE_OK && value != NULL);
    return value;
}

/* 0、1 两个字段都是 `obj<demo.cinner>`：各自一个类型引用节点，指向同一个 schema。 */
static void add_object_fields(AceSchema *schema, AceSchema *inner) {
    for (int32_t field_id = 0; field_id < 2; ++field_id) {
        AceTypeRef *object = NULL;
        CHECK(ace_type_object(inner, &object) == ACE_OK && object != NULL);
        CHECK(ace_schema_add_field(schema, field_id, object) == ACE_OK);
    }
}

static AceSchema *make_inner_schema(void) {
    AceSchema *inner = NULL;
    CHECK(ace_schema_new("demo.cinner", &inner) == ACE_OK && inner != NULL);
    AceTypeRef *number = NULL;
    CHECK(ace_type_prim(ACE_FK_I32, &number) == ACE_OK);
    CHECK(ace_schema_add_field(inner, 0, number) == ACE_OK);
    return inner;
}

static AceSchema *make_outer_schema(AceSchema *inner) {
    AceSchema *schema = NULL;
    CHECK(ace_schema_new("demo.cdeser", &schema) == ACE_OK && schema != NULL);
    add_object_fields(schema, inner);
    return schema;
}

/* `demo.cempty`：零字段 schema（见文件头 ④）。 */
static AceSchema *make_empty_schema(void) {
    AceSchema *empty = NULL;
    CHECK(ace_schema_new("demo.cempty", &empty) == ACE_OK && empty != NULL);
    return empty;
}

/* `demo.clist`：字段 0 → `list<obj<element>>`。 */
static AceSchema *make_list_schema(AceSchema *element) {
    AceSchema *schema = NULL;
    CHECK(ace_schema_new("demo.clist", &schema) == ACE_OK && schema != NULL);
    AceTypeRef *object = NULL;
    CHECK(ace_type_object(element, &object) == ACE_OK && object != NULL);
    AceTypeRef *list = NULL;
    CHECK(ace_type_list(object, &list) == ACE_OK && list != NULL); /* 收编 object */
    CHECK(ace_schema_add_field(schema, 0, list) == ACE_OK);
    return schema;
}

/* 两个零字段元素对象的列表（`ace_list_add` 成功后归还调用方那份引用）。 */
static AceValue *make_list_value(void) {
    AceValue *list = NULL;
    CHECK(ace_value_list(&list) == ACE_OK && list != NULL);
    for (int index = 0; index < 2; ++index) {
        AceValue *item = make_object();
        CHECK(ace_list_add(list, item) == ACE_OK);
        CHECK(ace_value_free(item) == ACE_OK);
    }
    return list;
}

/* `demo.cfields`：字段 0 → `list<obj<element>>`，元素对象带字段（见文件头 ⑤）。 */
static AceSchema *make_field_list_schema(AceSchema *element) {
    AceSchema *schema = NULL;
    CHECK(ace_schema_new("demo.cfields", &schema) == ACE_OK && schema != NULL);
    AceTypeRef *object = NULL;
    CHECK(ace_type_object(element, &object) == ACE_OK && object != NULL);
    AceTypeRef *list = NULL;
    CHECK(ace_type_list(object, &list) == ACE_OK && list != NULL); /* 收编 object */
    CHECK(ace_schema_add_field(schema, 0, list) == ACE_OK);
    return schema;
}

/* 两个带字段元素对象（0 → i32）的列表；调用方负责 free 返回的列表。 */
static AceValue *make_field_list_value(void) {
    static const int64_t numbers[] = {7, -2};
    AceValue *list = NULL;
    CHECK(ace_value_list(&list) == ACE_OK && list != NULL);
    for (size_t index = 0; index < sizeof(numbers) / sizeof(numbers[0]); ++index) {
        AceValue *item = make_object();
        AceValue *number = make_i64(numbers[index]);
        CHECK(ace_object_set(item, 0, number) == ACE_OK);
        CHECK(ace_value_free(number) == ACE_OK);
        CHECK(ace_list_add(list, item) == ACE_OK);
        CHECK(ace_value_free(item) == ACE_OK);
    }
    return list;
}

/* 两处引用同一个内层对象：成功 set 即引用 +1，调用方句柄仍须各自 free（R32⑤）。 */
static AceValue *make_value(void) {
    AceValue *inner = make_object();
    AceValue *number = make_i64(42);
    CHECK(ace_object_set(inner, 0, number) == ACE_OK);
    CHECK(ace_value_free(number) == ACE_OK);

    AceValue *value = make_object();
    CHECK(ace_object_set(value, 0, inner) == ACE_OK);
    AceValue *shared = NULL;
    CHECK(ace_value_share(inner, &shared) == ACE_OK && shared == inner);
    CHECK(ace_object_set(value, 1, shared) == ACE_OK);
    CHECK(ace_value_free(shared) == ACE_OK);
    CHECK(ace_value_free(inner) == ACE_OK);
    return value;
}

/* 读出某结果中两处字段指向的内层节点；断言 ptr == 与 kind/字段值一致。 */
static const AceValue *check_shared_fields(const AceValue *result, const char *label) {
    const AceValue *first = NULL;
    const AceValue *second = NULL;
    CHECK(ace_object_get(result, 0, &first) == ACE_OK && first != NULL);
    CHECK(ace_object_get(result, 1, &second) == ACE_OK && second != NULL);
    CHECK(first == second);

    AceKind kind = ACE_KIND_NULL;
    CHECK(ace_value_kind(first, &kind) == ACE_OK);
    CHECK(kind == ACE_KIND_OBJECT);

    int64_t number = 0;
    const AceValue *payload = NULL;
    CHECK(ace_object_get(first, 0, &payload) == ACE_OK && payload != NULL);
    CHECK(ace_value_get_i64(payload, &number) == ACE_OK);
    CHECK(number == 42);

    printf("  %s: field0=%p field1=%p kind=%d inner=%lld\n", label, (const void *)first,
           (const void *)second, (int)kind, (long long)number);
    return first;
}

int main(void) {
    /* —— 1. schema：根对象两个字段都是 obj<demo.cinner> —— */
    AceSchema *inner = make_inner_schema();
    AceSchema *schema = make_outer_schema(inner);

    AceOptions *options = NULL;
    CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
    CHECK(ace_options_add_schema(options, schema) == ACE_OK);

    /* —— 2. 编码：共享节点只写一条 record、其余位置写引用 id —— */
    AceValue *value = make_value();
    AceBuffer buffer = {0};
    CHECK(ace_serialize("demo.cdeser", value, options, &buffer) == ACE_OK);
    CHECK(buffer.ptr != NULL && buffer.len > 0 && buffer.cap == buffer.len);
    printf("ace_serialize: code=0 len=%zu cap=%zu\n", buffer.len, buffer.cap);

    /* —— 3. 解码①：同一次调用内两处字段是同一个原生节点（R32③、AC-A05③） —— */
    AceValue *first_run = NULL;
    CHECK(ace_deserialize("demo.cdeser", buffer.ptr, buffer.len, options, &first_run) == ACE_OK);
    CHECK(first_run != NULL);
    const AceValue *shared_first = check_shared_fields(first_run, "run1");

    /* —— 4. 解码②：两份结果同时存活——各自内部共享成立、跨结果不共享（AC-A05⑤） —— */
    AceValue *second_run = NULL;
    CHECK(ace_deserialize("demo.cdeser", buffer.ptr, buffer.len, options, &second_run) == ACE_OK);
    CHECK(second_run != NULL && second_run != first_run);
    const AceValue *shared_second = check_shared_fields(second_run, "run2");
    CHECK(shared_first != shared_second);

    /* —— 5. 失败路径：未注册 key → 3，且 *out 置空（FR-A02） —— */
    AceValue *rejected = (AceValue *)(uintptr_t)0x8;
    CHECK(ace_deserialize("demo.cunknown", buffer.ptr, buffer.len, options, &rejected) ==
          ACE_UNKNOWN_TYPE_KEY);
    CHECK(rejected == NULL);
    /* NULL options = 空白名单 → 3；NULL out / NULL key → 1。 */
    CHECK(ace_deserialize("demo.cdeser", buffer.ptr, buffer.len, NULL, &rejected) ==
          ACE_UNKNOWN_TYPE_KEY);
    CHECK(rejected == NULL);
    CHECK(ace_deserialize("demo.cdeser", buffer.ptr, buffer.len, options, NULL) ==
          ACE_INVALID_ARGUMENT);
    CHECK(ace_deserialize(NULL, buffer.ptr, buffer.len, options, &rejected) == ACE_INVALID_ARGUMENT);
    CHECK(rejected == NULL);
    /* 截断输入 → 2，仍不留下半成品。 */
    CHECK(ace_deserialize("demo.cdeser", buffer.ptr, buffer.len - 1, options, &rejected) ==
          ACE_TRUNCATED_INPUT);
    CHECK(rejected == NULL);

    /* —— 5b. 元素位置的对象：非环 `list<object>` 编码→解码成功（不是 14） —— */
    AceSchema *empty = make_empty_schema();
    AceSchema *list_schema = make_list_schema(empty);
    CHECK(ace_options_add_schema(options, list_schema) == ACE_OK);
    AceValue *holder = make_object();
    AceValue *list_value = make_list_value();
    CHECK(ace_object_set(holder, 0, list_value) == ACE_OK);
    CHECK(ace_value_free(list_value) == ACE_OK);
    AceBuffer list_buffer = {0};
    CHECK(ace_serialize("demo.clist", holder, options, &list_buffer) == ACE_OK);
    CHECK(list_buffer.ptr != NULL && list_buffer.len > 0);
    AceValue *list_result = NULL;
    CHECK(ace_deserialize("demo.clist", list_buffer.ptr, list_buffer.len, options, &list_result) ==
          ACE_OK);
    CHECK(list_result != NULL);
    const AceValue *decoded_list = NULL;
    CHECK(ace_object_get(list_result, 0, &decoded_list) == ACE_OK && decoded_list != NULL);
    AceKind list_kind = ACE_KIND_NULL;
    CHECK(ace_value_kind(decoded_list, &list_kind) == ACE_OK && list_kind == ACE_KIND_LIST);
    size_t item_count = 0;
    CHECK(ace_list_len(decoded_list, &item_count) == ACE_OK && item_count == 2);
    for (size_t index = 0; index < item_count; ++index) {
        const AceValue *item = NULL;
        CHECK(ace_list_at(decoded_list, index, &item) == ACE_OK && item != NULL);
        AceKind item_kind = ACE_KIND_NULL;
        CHECK(ace_value_kind(item, &item_kind) == ACE_OK && item_kind == ACE_KIND_OBJECT);
        /* 零字段对象：借出的字段视图不存在（合法查询无项 = 12）。 */
        const AceValue *absent = NULL;
        CHECK(ace_object_get(item, 0, &absent) == ACE_NOT_FOUND);
    }
    printf("  list<object>: code=0 len=%zu kind=%d\n", item_count, (int)list_kind);

    /* —— 5c. 集合内的带字段对象：R35 让元素对象的字段深度与编码侧一致 —— */
    AceSchema *field_list_schema = make_field_list_schema(inner);
    CHECK(ace_options_add_schema(options, field_list_schema) == ACE_OK);
    AceValue *field_holder = make_object();
    AceValue *field_items = make_field_list_value();
    CHECK(ace_object_set(field_holder, 0, field_items) == ACE_OK);
    CHECK(ace_value_free(field_items) == ACE_OK);
    AceBuffer field_buffer = {0};
    CHECK(ace_serialize("demo.cfields", field_holder, options, &field_buffer) == ACE_OK);
    CHECK(field_buffer.ptr != NULL && field_buffer.len > 0);
    AceValue *field_result = NULL;
    int field_code =
        ace_deserialize("demo.cfields", field_buffer.ptr, field_buffer.len, options, &field_result);
    /* 接缝前这里是 4（字段槽 depth 差 1）；若读错元素体则是 14。 */
    CHECK(field_code == ACE_OK);
    CHECK(field_result != NULL);
    const AceValue *decoded_fields = NULL;
    CHECK(ace_object_get(field_result, 0, &decoded_fields) == ACE_OK && decoded_fields != NULL);
    size_t field_count = 0;
    CHECK(ace_list_len(decoded_fields, &field_count) == ACE_OK && field_count == 2);
    static const int64_t expected_numbers[] = {7, -2};
    for (size_t index = 0; index < field_count; ++index) {
        const AceValue *item = NULL;
        CHECK(ace_list_at(decoded_fields, index, &item) == ACE_OK && item != NULL);
        AceKind item_kind = ACE_KIND_NULL;
        CHECK(ace_value_kind(item, &item_kind) == ACE_OK && item_kind == ACE_KIND_OBJECT);
        const AceValue *number = NULL;
        int64_t decoded_number = 0;
        CHECK(ace_object_get(item, 0, &number) == ACE_OK && number != NULL);
        CHECK(ace_value_get_i64(number, &decoded_number) == ACE_OK);
        CHECK(decoded_number == expected_numbers[index]);
    }
    printf("  list<object{i32}>: code=%d len=%zu\n", field_code, field_count);

    /* —— 6. 释放：两份解码结果各 free 一次（共享节点由父容器的两个引用共同持有） —— */
    CHECK(ace_value_free(first_run) == ACE_OK);
    CHECK(ace_value_free(second_run) == ACE_OK);
    CHECK(ace_value_free(value) == ACE_OK);
    CHECK(ace_buffer_free(&buffer) == ACE_OK);
    CHECK(buffer.ptr == NULL && buffer.len == 0 && buffer.cap == 0);
    CHECK(ace_value_free(list_result) == ACE_OK);
    CHECK(ace_value_free(holder) == ACE_OK);
    CHECK(ace_buffer_free(&list_buffer) == ACE_OK);
    CHECK(list_buffer.ptr == NULL && list_buffer.len == 0 && list_buffer.cap == 0);
    CHECK(ace_value_free(field_result) == ACE_OK);
    CHECK(ace_value_free(field_holder) == ACE_OK);
    CHECK(ace_buffer_free(&field_buffer) == ACE_OK);
    CHECK(field_buffer.ptr == NULL && field_buffer.len == 0 && field_buffer.cap == 0);

    CHECK(ace_options_free(options) == ACE_OK);
    CHECK(ace_schema_free(schema) == ACE_OK);
    CHECK(ace_schema_free(field_list_schema) == ACE_OK);
    CHECK(ace_schema_free(inner) == ACE_OK);
    CHECK(ace_schema_free(list_schema) == ACE_OK);
    CHECK(ace_schema_free(empty) == ACE_OK);

    printf("deserialize_model: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
