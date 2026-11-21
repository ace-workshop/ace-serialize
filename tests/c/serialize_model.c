/* P3-3b `ace_serialize` 真跑：注册 schema（str + map<str,i32> + 两个 option + repr(i8) 枚举 +
 * list<i32>），构造含 Map 乱序插入、Option none/some、枚举的嵌套值，编码出 `AceBuffer`，
 * 校验长度/指针/容量口径与 free 后清零、二次 free 安全，并核对错误码（3/1/13/15）。
 *
 * 编译并链接 Rust cdylib：
 *   cargo build -p ace-serialize-ffi
 *   cc -std=c17 -Wall -Wextra -Iinclude tests/c/serialize_model.c \
 *      -Ltarget/debug -lace_serialize_ffi -Wl,-rpath,"$PWD/target/debug" -o target/debug/serialize_model_c
 *   ./target/debug/serialize_model_c
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

static int check_empty(const AceBuffer *buffer) {
    return buffer->ptr == NULL && buffer->len == 0 && buffer->cap == 0;
}

static AceValue *make_object(void) {
    AceValue *value = NULL;
    CHECK(ace_value_object(NULL, &value) == ACE_OK && value != NULL);
    return value;
}

static AceValue *make_str(const char *text) {
    AceValue *value = NULL;
    CHECK(ace_value_str((const uint8_t *)text, strlen(text), &value) == ACE_OK && value != NULL);
    return value;
}

/* Map 的 ABI 值：List<Object{k: order 0, v: order 1}>，故意按 b、a 的乱序插入。 */
static AceValue *make_tags(void) {
    static const char *keys[2] = {"b", "a"};
    static const int64_t numbers[2] = {2, 1};
    AceValue *tags = NULL;
    CHECK(ace_value_list(&tags) == ACE_OK && tags != NULL);
    for (int index = 0; index < 2; ++index) {
        AceValue *entry = make_object();
        AceValue *number = NULL;
        CHECK(ace_value_i64(numbers[index], &number) == ACE_OK && number != NULL);
        CHECK(ace_object_set(entry, 0, make_str(keys[index])) == ACE_OK);
        CHECK(ace_object_set(entry, 1, number) == ACE_OK);
        CHECK(ace_list_add(tags, entry) == ACE_OK);
    }
    return tags;
}

/* variant：0 = 合法值；1 = 0 号字段（schema 要 str）给 i64；2 = 缺 0 号必写字段。 */
static AceValue *make_value(int variant) {
    AceValue *value = make_object();
    if (variant == 0) {
        CHECK(ace_object_set(value, 0, make_str("c模型")) == ACE_OK);
    } else if (variant == 1) {
        AceValue *number = NULL;
        CHECK(ace_value_i64(7, &number) == ACE_OK);
        CHECK(ace_object_set(value, 0, number) == ACE_OK);
    }
    CHECK(ace_object_set(value, 1, make_tags()) == ACE_OK);
    {
        /* 2 号 option<i32> = Some(5)；3 号 option<str> 缺席 = None。 */
        AceValue *some = NULL;
        CHECK(ace_value_i64(5, &some) == ACE_OK);
        CHECK(ace_object_set(value, 2, some) == ACE_OK);
        AceValue *color = NULL;
        CHECK(ace_value_u64(2, &color) == ACE_OK); /* 枚举 Green（repr i8） */
        CHECK(ace_object_set(value, 4, color) == ACE_OK);
        AceValue *numbers = NULL;
        CHECK(ace_value_list(&numbers) == ACE_OK);
        for (int64_t index = 1; index <= 3; ++index) {
            AceValue *item = NULL;
            CHECK(ace_value_i64(index, &item) == ACE_OK);
            CHECK(ace_list_add(numbers, item) == ACE_OK);
        }
        CHECK(ace_object_set(value, 5, numbers) == ACE_OK);
    }
    return value;
}

int main(void) {
    /* —— 1. schema：0 str、1 map<str,i32>、2 option<i32>、3 option<str>、
       4 enum(repr i8)、5 list<i32> —— */
    AceSchema *schema = NULL;
    CHECK(ace_schema_new("demo.cmodel", &schema) == ACE_OK && schema != NULL);
    CHECK(ace_schema_add_field(schema, 0, (AceTypeRef *)0) == ACE_INVALID_ARGUMENT);
    {
        AceTypeRef *name = NULL;
        CHECK(ace_type_str(&name) == ACE_OK);
        CHECK(ace_schema_add_field(schema, 0, name) == ACE_OK);
    }
    {
        AceTypeRef *key = NULL;
        AceTypeRef *value = NULL;
        AceTypeRef *tags = NULL;
        CHECK(ace_type_str(&key) == ACE_OK);
        CHECK(ace_type_prim(ACE_FK_I32, &value) == ACE_OK);
        CHECK(ace_type_map(key, value, &tags) == ACE_OK);
        CHECK(ace_schema_add_field(schema, 1, tags) == ACE_OK);
    }
    {
        AceTypeRef *inner = NULL;
        AceTypeRef *option = NULL;
        CHECK(ace_type_prim(ACE_FK_I32, &inner) == ACE_OK);
        CHECK(ace_type_option(inner, &option) == ACE_OK);
        CHECK(ace_schema_add_field(schema, 2, option) == ACE_OK);
    }
    {
        AceTypeRef *inner = NULL;
        AceTypeRef *option = NULL;
        CHECK(ace_type_str(&inner) == ACE_OK);
        CHECK(ace_type_option(inner, &option) == ACE_OK);
        CHECK(ace_schema_add_field(schema, 3, option) == ACE_OK);
    }
    AceEnumSchema *color = NULL;
    CHECK(ace_enum_new("demo.color", &color) == ACE_OK && color != NULL);
    CHECK(ace_enum_set_repr(color, ACE_FK_I8) == ACE_OK);
    CHECK(ace_enum_add_variant(color, 1) == ACE_OK); /* 默认变体 Red */
    CHECK(ace_enum_add_variant(color, 2) == ACE_OK); /* Green */
    CHECK(ace_enum_set_default_variant(color, 1) == ACE_OK);
    {
        AceTypeRef *color_type = NULL;
        CHECK(ace_type_enum(color, &color_type) == ACE_OK);
        CHECK(ace_schema_add_field(schema, 4, color_type) == ACE_OK);
    }
    {
        AceTypeRef *item = NULL;
        AceTypeRef *numbers = NULL;
        CHECK(ace_type_prim(ACE_FK_I32, &item) == ACE_OK);
        CHECK(ace_type_list(item, &numbers) == ACE_OK);
        CHECK(ace_schema_add_field(schema, 5, numbers) == ACE_OK);
    }

    AceOptions *options = NULL;
    CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
    CHECK(ace_options_add_schema(options, schema) == ACE_OK);

    /* —— 2. 编码：len>0、ptr 非空、cap 与 len 同口径 —— */
    AceValue *value = make_value(0);
    AceBuffer buffer = {0};
    CHECK(ace_serialize("demo.cmodel", value, options, &buffer) == ACE_OK);
    CHECK(buffer.ptr != NULL);
    CHECK(buffer.len > 0);
    CHECK(buffer.cap == buffer.len);
    printf("ace_serialize: code=0 len=%zu cap=%zu head=", buffer.len, buffer.cap);
    for (size_t index = 0; index < 8 && index < buffer.len; ++index) {
        printf("%02x", buffer.ptr[index]);
    }
    printf("\n");

    /* —— 3. free：清零结构，二次 free 安全，NULL → 1 —— */
    CHECK(ace_buffer_free(&buffer) == ACE_OK);
    CHECK(check_empty(&buffer));
    CHECK(ace_buffer_free(&buffer) == ACE_OK);
    CHECK(check_empty(&buffer));
    /* NULL 视为已释放（同 ace_value_free）。 */
    CHECK(ace_buffer_free(NULL) == ACE_OK);

    /* —— 4. 错误路径：未注册 key → 3、NULL 参数 → 1、kind 不匹配 → 13、缺字段 → 15 —— */
    AceBuffer rejected = {0};
    CHECK(ace_serialize("demo.unknown", value, options, &rejected) == ACE_UNKNOWN_TYPE_KEY);
    CHECK(check_empty(&rejected));
    CHECK(ace_serialize(NULL, value, options, &rejected) == ACE_INVALID_ARGUMENT);
    CHECK(check_empty(&rejected));
    CHECK(ace_serialize("demo.cmodel", NULL, options, &rejected) == ACE_INVALID_ARGUMENT);
    CHECK(check_empty(&rejected));
    /* NULL options = 空白名单 → 3（头条款）。 */
    CHECK(ace_serialize("demo.cmodel", value, NULL, &rejected) == ACE_UNKNOWN_TYPE_KEY);
    CHECK(check_empty(&rejected));
    CHECK(ace_serialize("demo.cmodel", value, options, NULL) == ACE_INVALID_ARGUMENT);

    AceValue *mismatch = make_value(1);
    CHECK(ace_serialize("demo.cmodel", mismatch, options, &rejected) == ACE_TYPE_MISMATCH);
    CHECK(check_empty(&rejected));
    CHECK(ace_value_free(mismatch) == ACE_OK);

    AceValue *missing = make_value(2);
    CHECK(ace_serialize("demo.cmodel", missing, options, &rejected) == ACE_MISSING_REQUIRED_FIELD);
    CHECK(check_empty(&rejected));
    CHECK(ace_value_free(missing) == ACE_OK);

    /* —— 5. 释放顺序：值 → options → 引用方 schema → 被借用 enum —— */
    CHECK(ace_value_free(value) == ACE_OK);
    CHECK(ace_options_free(options) == ACE_OK);
    CHECK(ace_schema_free(schema) == ACE_OK);
    CHECK(ace_enum_free(color) == ACE_OK);

    printf("serialize_model: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
