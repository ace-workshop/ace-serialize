/* P3-5 共享身份真跑（R32）：`share` 后两枚句柄 `ptr ==`、`set` 后调用方句柄仍有效、
 * 编码只写一条 record（按未压缩信封的 assist/main 区核对）、减引用语义、强环 → 14。
 * R32②：list 节点共享与对象同规则——一条 list record + 两处引用 id。
 *
 * 编译并链接 Rust cdylib：
 *   cargo build -p ace-serialize-ffi
 *   cc -std=c17 -Wall -Wextra -Iinclude tests/c/share_model.c \
 *      -Ltarget/debug -lace_serialize_ffi -Wl,-rpath,"$PWD/target/debug" -o target/debug/share_model_c
 *   ./target/debug/share_model_c
 */
#include "ace_serialize.h"

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

static int32_t load_i32(const uint8_t *bytes) {
    return (int32_t)((uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
                     ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24));
}

static uint64_t load_u64(const uint8_t *bytes) {
    uint64_t value = 0;
    for (int index = 7; index >= 0; --index) {
        value = (value << 8) | bytes[index];
    }
    return value;
}

/* 未压缩信封（packed=false）：`[main_len i32][main][assist_len i32][assist]`。 */
static void unpack(const AceBuffer *buffer, const uint8_t **main, size_t *main_len,
                   const uint8_t **assist, size_t *assist_len) {
    *main_len = (size_t)load_i32(buffer->ptr);
    *main = buffer->ptr + 4;
    *assist_len = (size_t)load_i32(buffer->ptr + 4 + *main_len);
    *assist = buffer->ptr + 4 + *main_len + 4;
}

/* 根槽载荷里的记录 id。记录 id 从 1 起顺序分配、根记录最后写出，故它也是本次的记录条数。 */
static uint64_t record_count(const uint8_t *assist) {
    return load_u64(assist + 8);
}

int main(void) {
    /* —— 1. schema：demo.child{0:i32}；demo.parent{0:obj<child>, 1:obj<child>} —— */
    AceSchema *child_type = NULL;
    CHECK(ace_schema_new("demo.child", &child_type) == ACE_OK && child_type != NULL);
    AceTypeRef *i32_type = NULL;
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_schema_add_field(child_type, 0, i32_type) == ACE_OK);

    AceSchema *parent_type = NULL;
    CHECK(ace_schema_new("demo.parent", &parent_type) == ACE_OK && parent_type != NULL);
    for (int32_t field_id = 0; field_id < 2; ++field_id) {
        AceTypeRef *object_type = NULL;
        CHECK(ace_type_object(child_type, &object_type) == ACE_OK);
        CHECK(ace_schema_add_field(parent_type, field_id, object_type) == ACE_OK);
    }

    AceOptions *options = NULL;
    CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
    /* 未压缩信封：按字节核对记录数与引用 id。 */
    CHECK(ace_options_set_packed(options, 0) == ACE_OK);
    CHECK(ace_options_add_schema(options, parent_type) == ACE_OK);
    CHECK(ace_options_add_schema(options, child_type) == ACE_OK);

    /* —— 2. parent{0: child, 1: share(child)} —— */
    AceValue *child = NULL;
    CHECK(ace_value_object(NULL, &child) == ACE_OK && child != NULL);
    AceValue *number = NULL;
    CHECK(ace_value_i64(7, &number) == ACE_OK);
    CHECK(ace_object_set(child, 0, number) == ACE_OK);
    /* R32⑤：set 只加父引用，调用方句柄仍有效，须自行减引用。 */
    CHECK(ace_value_free(number) == ACE_OK);

    AceValue *parent = NULL;
    CHECK(ace_value_object(NULL, &parent) == ACE_OK && parent != NULL);
    CHECK(ace_object_set(parent, 0, child) == ACE_OK);
    AceValue *shared = NULL;
    CHECK(ace_value_share(child, &shared) == ACE_OK);
    CHECK(shared == child); /* 同一原生节点：ptr == */
    CHECK(ace_object_set(parent, 1, shared) == ACE_OK);

    const AceValue *a = NULL;
    const AceValue *b = NULL;
    CHECK(ace_object_get(parent, 0, &a) == ACE_OK);
    CHECK(ace_object_get(parent, 1, &b) == ACE_OK);
    CHECK(a == b);     /* 两个字段读出的借用视图是同一节点 */
    CHECK(a == child); /* 与调用方句柄也是同一节点 */

    /* —— 3. 编码：共享 DAG 只写一条 child record —— */
    AceBuffer buffer = {0};
    CHECK(ace_serialize("demo.parent", parent, options, &buffer) == ACE_OK);
    CHECK(buffer.ptr != NULL && buffer.len > 0 && buffer.cap == buffer.len);

    const uint8_t *main = NULL;
    const uint8_t *assist = NULL;
    size_t main_len = 0;
    size_t assist_len = 0;
    unpack(&buffer, &main, &main_len, &assist, &assist_len);
    (void)main;
    (void)main_len;
    /* assist = 根槽 16B + child 字段槽 16B + parent 两个字段槽 32B。 */
    CHECK(assist_len == 16 + 16 + 32);
    /* 共享 child 一条 + 根 parent 一条 = 2 条 record（重复内容各写一条时是 3 条）。 */
    CHECK(record_count(assist) == 2);
    {
        static const uint8_t child_record_id[8] = {1, 0, 0, 0, 0, 0, 0, 0};
        /* parent 两个字段槽（载荷偏移 40 与 56）指向同一条 child record id。 */
        CHECK(memcmp(assist + 40, assist + 56, 8) == 0);
        CHECK(memcmp(assist + 40, child_record_id, 8) == 0);
    }
    CHECK(ace_buffer_free(&buffer) == ACE_OK);

    /* —— 3b. 共享 list：parent 两个字段指向同一 list 节点（R32②） —— */
    AceSchema *lists_type = NULL;
    CHECK(ace_schema_new("demo.lists", &lists_type) == ACE_OK && lists_type != NULL);
    for (int32_t field_id = 0; field_id < 2; ++field_id) {
        AceTypeRef *i32_item = NULL;
        CHECK(ace_type_prim(ACE_FK_I32, &i32_item) == ACE_OK);
        AceTypeRef *list_type = NULL;
        CHECK(ace_type_list(i32_item, &list_type) == ACE_OK); /* 收编 i32_item */
        CHECK(ace_schema_add_field(lists_type, field_id, list_type) == ACE_OK);
    }
    CHECK(ace_options_add_schema(options, lists_type) == ACE_OK);

    AceValue *numbers = NULL;
    CHECK(ace_value_list(&numbers) == ACE_OK && numbers != NULL);
    for (int64_t number = 1; number <= 2; ++number) {
        AceValue *item = NULL;
        CHECK(ace_value_i64(number, &item) == ACE_OK);
        CHECK(ace_list_add(numbers, item) == ACE_OK);
        CHECK(ace_value_free(item) == ACE_OK);
    }
    AceValue *list_pair = NULL;
    CHECK(ace_value_object(NULL, &list_pair) == ACE_OK && list_pair != NULL);
    CHECK(ace_object_set(list_pair, 0, numbers) == ACE_OK);
    AceValue *shared_list = NULL;
    CHECK(ace_value_share(numbers, &shared_list) == ACE_OK);
    CHECK(shared_list == numbers); /* ptr ==：共享同一 list 节点 */
    CHECK(ace_object_set(list_pair, 1, shared_list) == ACE_OK);
    {
        const AceValue *left = NULL;
        const AceValue *right = NULL;
        CHECK(ace_object_get(list_pair, 0, &left) == ACE_OK);
        CHECK(ace_object_get(list_pair, 1, &right) == ACE_OK);
        CHECK(left == right && left == numbers);
    }

    AceBuffer list_buffer = {0};
    CHECK(ace_serialize("demo.lists", list_pair, options, &list_buffer) == ACE_OK);
    const uint8_t *list_main = NULL;
    const uint8_t *list_assist = NULL;
    size_t list_main_len = 0;
    size_t list_assist_len = 0;
    unpack(&list_buffer, &list_main, &list_main_len, &list_assist, &list_assist_len);
    /* assist = 根槽 16B + 根对象两个字段槽 32B（list 自身无槽块）。 */
    CHECK(list_assist_len == 16 + 32);
    /* 共享同一 list 节点：list 一条 + 根对象一条 = 2 条 record。 */
    CHECK(record_count(list_assist) == 2);
    /* 两个字段槽（载荷偏移 24 与 40）是同一个引用 id，且指向先落盘的 list 记录。 */
    CHECK(memcmp(list_assist + 24, list_assist + 40, 8) == 0);
    CHECK(load_u64(list_assist + 24) == 1);
    /* list 记录体 = u32 count(2) + 两个 4B i32（main 首条记录：id 1、size 12）。
       main = list 记录（8 + 12）+ 根对象记录（8 + 8B 体）。 */
    CHECK(list_main_len == (8 + 12) + (8 + 8));
    CHECK(load_i32(list_main) == 1);
    CHECK(load_i32(list_main + 4) == 12);
    CHECK(load_i32(list_main + 8) == 2);
    CHECK(load_i32(list_main + 12) == 1);
    CHECK(load_i32(list_main + 16) == 2);
    CHECK(ace_buffer_free(&list_buffer) == ACE_OK);

    CHECK(ace_value_free(shared_list) == ACE_OK);
    CHECK(ace_value_free(numbers) == ACE_OK);
    CHECK(ace_value_free(list_pair) == ACE_OK);

    /* —— 4. 减引用语义：两枚句柄各 free 一次，父引用存在时节点不释放 —— */
    CHECK(ace_value_free(shared) == ACE_OK);
    CHECK(ace_value_free(child) == ACE_OK);
    CHECK(ace_object_get(parent, 0, &a) == ACE_OK);
    CHECK(ace_object_get(parent, 1, &b) == ACE_OK);
    CHECK(a == b);
    {
        AceKind kind = (AceKind)-1;
        const AceValue *field = NULL;
        int64_t seven = 0;
        CHECK(ace_value_kind(b, &kind) == ACE_OK && kind == ACE_KIND_OBJECT);
        CHECK(ace_object_get(b, 0, &field) == ACE_OK);
        CHECK(ace_value_get_i64(field, &seven) == ACE_OK && seven == 7);
    }

    /* —— 5. share 失败三分类的 (a)(b)：(c) 伪造/悬垂不新增保证，不作断言 —— */
    CHECK(ace_value_share(parent, NULL) == ACE_INVALID_ARGUMENT); /* (a) 不访问输出 */
    {
        AceValue *rejected = parent; /* 预置非空：失败必须置空、不得沿用 */
        CHECK(ace_value_share(NULL, &rejected) == ACE_INVALID_ARGUMENT);
        CHECK(rejected == NULL);
    }
    CHECK(ace_value_free(parent) == ACE_OK);

    /* —— 6. 强环：parent 引用自身 → 14，不返回部分成功字节 —— */
    AceValue *cyclic = NULL;
    CHECK(ace_value_object(NULL, &cyclic) == ACE_OK && cyclic != NULL);
    AceValue *cyclic_child = NULL;
    CHECK(ace_value_object(NULL, &cyclic_child) == ACE_OK);
    CHECK(ace_object_set(cyclic, 0, cyclic_child) == ACE_OK);
    CHECK(ace_object_set(cyclic, 1, cyclic) == ACE_OK); /* 构造成功：强环是使用禁令 */
    {
        AceBuffer cycle_buffer = {0};
        CHECK(ace_serialize("demo.parent", cyclic, options, &cycle_buffer) ==
              ACE_CYCLE_UNSUPPORTED);
        CHECK(cycle_buffer.ptr == NULL && cycle_buffer.len == 0 && cycle_buffer.cap == 0);
    }
    /* 强环按引用计数语义不保证释放：不 free 环内节点（头 v4 已明示调用方不得构造强环）。 */

    /* —— 7. 释放顺序：options → 引用方 schema → 被借用 schema —— */
    CHECK(ace_options_free(options) == ACE_OK);
    CHECK(ace_schema_free(parent_type) == ACE_OK);
    CHECK(ace_schema_free(child_type) == ACE_OK);
    CHECK(ace_schema_free(lists_type) == ACE_OK);

    printf("share_model: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
