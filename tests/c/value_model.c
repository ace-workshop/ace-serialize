/* P3-1 值模型真跑：构造含 list + object + str + bytes 的值，读回校验、替换对象字段、释放。
 *
 * 编译并链接 Rust cdylib：
 *   cargo build -p ace-serialize-ffi
 *   cc -std=c17 -Wall -Wextra -Iinclude tests/c/value_model.c \
 *      -Ltarget/debug -lace_serialize_ffi -Wl,-rpath,"$PWD/target/debug" -o target/debug/value_model_c
 *   ./target/debug/value_model_c
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

/* 中文/内嵌 NUL：h é l l o \0 w ö r l d，13 字节。 */
static const uint8_t text_bytes[] = {'h', 0xC3, 0xA9, 'l', 'l', 'o', 0x00,
                                     'w', 0xC3, 0xB6, 'r', 'l', 'd'};
static const uint8_t raw_bytes[] = {0x00, 0xFF, 0x10};

static int kind_is(const AceValue *value, AceKind expected) {
    AceKind kind = (AceKind)-1;
    if (ace_value_kind(value, &kind) != ACE_OK) {
        return 0;
    }
    return kind == expected;
}

int main(void) {
    /* —— 树形装配：root{0:list[7,"a\0b",node], 1:str, 2:bytes, 3:node} ——
     * R32⑤：set/list_add 成功只给父容器加一份引用，调用方句柄仍有效；每枚句柄各
     * ace_value_free 一次减引用（父引用存在时节点不释放）。 */
    AceValue *root = NULL;
    CHECK(ace_value_object("demo.root", &root) == ACE_OK);
    CHECK(root != NULL);

    AceValue *list = NULL;
    CHECK(ace_value_list(&list) == ACE_OK);

    AceValue *number = NULL;
    CHECK(ace_value_i64(7, &number) == ACE_OK);
    CHECK(ace_list_add(list, number) == ACE_OK);
    CHECK(ace_value_free(number) == ACE_OK);

    AceValue *short_text = NULL;
    CHECK(ace_value_str((const uint8_t *)"a\0b", 3, &short_text) == ACE_OK);
    CHECK(ace_list_add(list, short_text) == ACE_OK);
    CHECK(ace_value_free(short_text) == ACE_OK);

    AceValue *node = NULL;
    CHECK(ace_value_object(NULL, &node) == ACE_OK); /* type_key 可为 NULL */
    AceValue *count = NULL;
    CHECK(ace_value_u64(UINT64_MAX, &count) == ACE_OK);
    CHECK(ace_object_set(node, 0, count) == ACE_OK);
    CHECK(ace_value_free(count) == ACE_OK);
    AceValue *ratio = NULL;
    CHECK(ace_value_f64_bits(UINT64_C(0xC002000000000000), &ratio) == ACE_OK); /* -2.25 */
    CHECK(ace_object_set(node, 1, ratio) == ACE_OK);
    CHECK(ace_value_free(ratio) == ACE_OK);
    CHECK(ace_object_set(root, 0, list) == ACE_OK);
    CHECK(ace_list_add(list, node) == ACE_OK); /* node 进 list 第 2 位 */
    CHECK(ace_value_free(list) == ACE_OK);
    CHECK(ace_value_free(node) == ACE_OK);

    AceValue *text = NULL;
    CHECK(ace_value_str(text_bytes, sizeof(text_bytes), &text) == ACE_OK);
    CHECK(ace_object_set(root, 1, text) == ACE_OK); /* 可含内嵌 NUL */
    CHECK(ace_value_free(text) == ACE_OK);

    AceValue *blob = NULL;
    CHECK(ace_value_bytes(raw_bytes, sizeof(raw_bytes), &blob) == ACE_OK);
    CHECK(ace_object_set(root, 2, blob) == ACE_OK);
    CHECK(ace_value_free(blob) == ACE_OK);

    AceValue *other = NULL;
    CHECK(ace_value_object("demo.node", &other) == ACE_OK);
    AceValue *flag = NULL;
    CHECK(ace_value_bool(1, &flag) == ACE_OK);
    CHECK(ace_object_set(other, 0, flag) == ACE_OK);
    CHECK(ace_value_free(flag) == ACE_OK);
    CHECK(ace_object_set(root, 3, other) == ACE_OK);
    CHECK(ace_value_free(other) == ACE_OK);

    /* —— 读回校验 —— */
    CHECK(kind_is(root, ACE_KIND_OBJECT));

    const AceValue *field = NULL;
    CHECK(ace_object_get(root, 0, &field) == ACE_OK);
    CHECK(kind_is(field, ACE_KIND_LIST));

    size_t list_len = 0;
    CHECK(ace_list_len(field, &list_len) == ACE_OK);
    CHECK(list_len == 3);

    const AceValue *element = NULL;
    int64_t seven = 0;
    CHECK(ace_list_at(field, 0, &element) == ACE_OK);
    CHECK(kind_is(element, ACE_KIND_I64));
    CHECK(ace_value_get_i64(element, &seven) == ACE_OK);
    CHECK(seven == 7);

    const uint8_t *view = NULL;
    size_t view_len = 0;
    CHECK(ace_list_at(field, 1, &element) == ACE_OK);
    CHECK(ace_value_get_str(element, &view, &view_len) == ACE_OK);
    CHECK(view_len == 3);
    CHECK(view != NULL && memcmp(view, "a\0b", 3) == 0);

    CHECK(ace_list_at(field, 2, &element) == ACE_OK);
    CHECK(kind_is(element, ACE_KIND_OBJECT));
    uint64_t big = 0;
    CHECK(ace_object_get(element, 0, &field) == ACE_OK);
    CHECK(ace_value_get_u64(field, &big) == ACE_OK);
    CHECK(big == UINT64_MAX);
    uint64_t bits = 0;
    CHECK(ace_object_get(element, 1, &field) == ACE_OK);
    CHECK(ace_value_get_f64_bits(field, &bits) == ACE_OK);
    CHECK(bits == UINT64_C(0xC002000000000000));

    CHECK(ace_object_get(root, 1, &field) == ACE_OK);
    CHECK(kind_is(field, ACE_KIND_STR));
    CHECK(ace_value_get_str(field, &view, &view_len) == ACE_OK);
    CHECK(view_len == sizeof(text_bytes));
    CHECK(view != NULL && memcmp(view, text_bytes, sizeof(text_bytes)) == 0);

    CHECK(ace_object_get(root, 2, &field) == ACE_OK);
    CHECK(kind_is(field, ACE_KIND_BYTES));
    CHECK(ace_value_get_bytes(field, &view, &view_len) == ACE_OK);
    CHECK(view_len == sizeof(raw_bytes));
    CHECK(view != NULL && memcmp(view, raw_bytes, sizeof(raw_bytes)) == 0);

    CHECK(ace_object_get(root, 3, &field) == ACE_OK);
    CHECK(kind_is(field, ACE_KIND_OBJECT));
    uint8_t truth = 0;
    CHECK(ace_object_get(field, 0, &field) == ACE_OK);
    CHECK(ace_value_get_bool(field, &truth) == ACE_OK);
    CHECK(truth == 1);

    /* —— 替换对象字段：只减父对旧值的引用，失效只及该父的借用视图 —— */
    AceValue *replacement = NULL;
    CHECK(ace_value_object("demo.node2", &replacement) == ACE_OK);
    AceValue *replacement_flag = NULL;
    CHECK(ace_value_bool(0, &replacement_flag) == ACE_OK);
    CHECK(ace_object_set(replacement, 0, replacement_flag) == ACE_OK);
    CHECK(ace_value_free(replacement_flag) == ACE_OK);
    CHECK(ace_object_set(root, 3, replacement) == ACE_OK);
    CHECK(ace_value_free(replacement) == ACE_OK);
    CHECK(ace_object_get(root, 3, &field) == ACE_OK);
    CHECK(kind_is(field, ACE_KIND_OBJECT));
    CHECK(ace_object_get(field, 0, &field) == ACE_OK);
    CHECK(ace_value_get_bool(field, &truth) == ACE_OK);
    CHECK(truth == 0);

    /* —— 错误路径：合法查询无项 → 12，kind 不匹配 → 13，坏入参 → 1 —— */
    CHECK(ace_object_get(root, 42, &field) == ACE_NOT_FOUND);
    CHECK(ace_object_get(root, -1, &field) == ACE_NOT_FOUND);
    CHECK(ace_object_get(root, 1, &field) == ACE_OK);
    CHECK(ace_list_at(field, 0, &element) == ACE_TYPE_MISMATCH);
    CHECK(ace_value_get_i64(field, &seven) == ACE_TYPE_MISMATCH);
    CHECK(ace_object_get(root, 0, &field) == ACE_OK);
    CHECK(ace_list_at(field, 3, &element) == ACE_NOT_FOUND);
    CHECK(ace_value_null(NULL) == ACE_INVALID_ARGUMENT);

    AceValue *bad = NULL;
    CHECK(ace_value_bool(2, &bad) == ACE_INVALID_ARGUMENT);
    CHECK(bad == NULL);

    /* —— 释放：root 连同其子树由库释放（子树调用方句柄已各自减过引用） —— */
    CHECK(ace_value_free(root) == ACE_OK);
    CHECK(ace_value_free(NULL) == ACE_OK);

    printf("value_model.c: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
