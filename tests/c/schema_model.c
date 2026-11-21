/* P3-2 schema 模型真跑：建含 map<String, list<option<i32>>> 字段的 schema、repr(i16) 枚举 schema，
 * 校验错误路径与释放顺序（先 schema，后未被收编的 type，最后被借用的 enum）。
 *
 * 编译并链接 Rust cdylib：
 *   cargo build -p ace-serialize-ffi
 *   cc -std=c17 -Wall -Wextra -Iinclude tests/c/schema_model.c \
 *      -Ltarget/debug -lace_serialize_ffi -Wl,-rpath,"$PWD/target/debug" -o target/debug/schema_model_c
 *   ./target/debug/schema_model_c
 */
#include "ace_serialize.h"

#include <stdio.h>

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

int main(void) {
    /* —— 1. 字段类型 map<String, list<option<i32>>> —— */
    AceTypeRef *key = NULL;
    AceTypeRef *i32_type = NULL;
    AceTypeRef *option_i32 = NULL;
    AceTypeRef *list_type = NULL;
    AceTypeRef *map_type = NULL;
    CHECK(ace_type_str(&key) == ACE_OK && key != NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK && i32_type != NULL);
    CHECK(ace_type_option(i32_type, &option_i32) == ACE_OK && option_i32 != NULL);
    /* 收编后子句柄失效：单独 type_free → 1（节点仍随父存活，故内存有效）。 */
    CHECK(ace_type_free(i32_type) == ACE_INVALID_ARGUMENT);
    CHECK(ace_type_list(option_i32, &list_type) == ACE_OK && list_type != NULL);
    CHECK(ace_type_free(option_i32) == ACE_INVALID_ARGUMENT);
    CHECK(ace_type_map(key, list_type, &map_type) == ACE_OK && map_type != NULL);
    CHECK(ace_type_free(key) == ACE_INVALID_ARGUMENT);
    CHECK(ace_type_free(list_type) == ACE_INVALID_ARGUMENT);

    /* —— 2. schema 收编字段类型；repr(i16) 枚举 schema，两个变体 + 默认 —— */
    AceSchema *schema = NULL;
    CHECK(ace_schema_new("demo.record", &schema) == ACE_OK && schema != NULL);
    CHECK(ace_schema_add_field(schema, 0, map_type) == ACE_OK);
    CHECK(ace_type_free(map_type) == ACE_INVALID_ARGUMENT);

    AceEnumSchema *color = NULL;
    CHECK(ace_enum_new("demo.color", &color) == ACE_OK && color != NULL);
    CHECK(ace_enum_set_repr(color, ACE_FK_I16) == ACE_OK);
    CHECK(ace_enum_add_variant(color, 0x0000u) == ACE_OK); /* 变体 0 */
    CHECK(ace_enum_add_variant(color, 0xFFFFu) == ACE_OK); /* i16 的 -1 位模式 */
    CHECK(ace_enum_set_default_variant(color, 0x0000u) == ACE_OK);

    AceTypeRef *enum_type = NULL;
    CHECK(ace_type_enum(color, &enum_type) == ACE_OK && enum_type != NULL);
    CHECK(ace_schema_add_field(schema, 1, enum_type) == ACE_OK);
    CHECK(ace_type_free(enum_type) == ACE_INVALID_ARGUMENT);

    /* —— 3. 错误路径 —— */
    /* prim 只收标量/宽值子集：STR/BYTES/复合/ENUM → 1。 */
    AceTypeRef *rejected = (AceTypeRef *)0x1;
    CHECK(ace_type_prim(ACE_FK_STR, &rejected) == ACE_INVALID_ARGUMENT && rejected == NULL);
    CHECK(ace_type_prim(ACE_FK_BYTES, &rejected) == ACE_INVALID_ARGUMENT && rejected == NULL);
    CHECK(ace_type_prim(ACE_FK_ENUM, &rejected) == ACE_INVALID_ARGUMENT && rejected == NULL);
    CHECK(ace_type_prim(ACE_FK_OPTION, &rejected) == ACE_INVALID_ARGUMENT && rejected == NULL);

    /* 内层不得再为 option：失败时不收编内层。 */
    AceTypeRef *inner_option = NULL;
    CHECK(ace_type_option(NULL, &inner_option) == ACE_INVALID_ARGUMENT && inner_option == NULL);
    CHECK(ace_type_prim(ACE_FK_I32, &i32_type) == ACE_OK);
    CHECK(ace_type_option(i32_type, &inner_option) == ACE_OK);
    CHECK(ace_type_option(inner_option, &rejected) == ACE_INVALID_ARGUMENT && rejected == NULL);
    CHECK(ace_type_free(inner_option) == ACE_OK);

    /* map 键须有线序：f32 键被拒，失败不收编 key/val。 */
    AceTypeRef *float_key = NULL;
    AceTypeRef *value = NULL;
    CHECK(ace_type_prim(ACE_FK_F32, &float_key) == ACE_OK);
    CHECK(ace_type_prim(ACE_FK_I32, &value) == ACE_OK);
    CHECK(ace_type_map(float_key, value, &rejected) == ACE_INVALID_ARGUMENT && rejected == NULL);
    CHECK(ace_type_free(float_key) == ACE_OK);
    CHECK(ace_type_free(value) == ACE_OK);

    /* 非法 repr：非 8 种定宽整数 → 1，repr 保持 i16。 */
    CHECK(ace_enum_set_repr(color, ACE_FK_BOOL) == ACE_INVALID_ARGUMENT);
    CHECK(ace_enum_set_repr(color, ACE_FK_F64) == ACE_INVALID_ARGUMENT);
    CHECK(ace_enum_set_repr(color, ACE_FK_CHAR) == ACE_INVALID_ARGUMENT);
    CHECK(ace_enum_set_repr(color, ACE_FK_UINTPTR) == ACE_INVALID_ARGUMENT);

    /* 位模式超宽/非规范扩展 → 1；截回后重复 → 1。 */
    CHECK(ace_enum_add_variant(color, 0x0001u) == ACE_OK);
    CHECK(ace_enum_add_variant(color, 0x0000u) == ACE_INVALID_ARGUMENT);     /* 重复 tag */
    CHECK(ace_enum_add_variant(color, 0x10000u) == ACE_INVALID_ARGUMENT); /* 超 i16 位宽 */
    CHECK(ace_enum_add_variant(color, 0xFFFFFFFFu) == ACE_INVALID_ARGUMENT); /* 非规范符号扩展 */
    /* u64 的 -1 是 0xFFFF 的规范符号扩展 → 截回后同值，仍是重复。 */
    CHECK(ace_enum_add_variant(color, UINT64_MAX) == ACE_INVALID_ARGUMENT);
    CHECK(ace_enum_set_default_variant(color, 0x0002u) == ACE_INVALID_ARGUMENT); /* 不在变体集 */

    /* schema：重复 field_id / 负 field_id → 1，失败均不收编。 */
    AceTypeRef *stray = NULL;
    CHECK(ace_type_prim(ACE_FK_I8, &stray) == ACE_OK);
    CHECK(ace_schema_add_field(schema, 0, stray) == ACE_INVALID_ARGUMENT);
    CHECK(ace_schema_add_field(schema, -1, stray) == ACE_INVALID_ARGUMENT);
    /* type_key 必填：NULL → 1 且 out 置空（用独立槽，以免覆盖存活句柄）。 */
    AceSchema *missing = NULL;
    CHECK(ace_schema_new(NULL, &missing) == ACE_INVALID_ARGUMENT && missing == NULL);

    AceTypeRef *object_type = NULL;
    CHECK(ace_type_object(schema, &object_type) == ACE_OK && object_type != NULL);
    CHECK(ace_schema_add_field(schema, 2, object_type) == ACE_INVALID_ARGUMENT);
    CHECK(ace_type_free(object_type) == ACE_OK); /* 失败未收编 */

    /* —— 4. 释放顺序 —— */
    /* 先 free schema：连同收编的字段类型节点（map/list/option/i32、enum 引用）一并释放；
       再 free 仍未收编的顶层节点（stray 归调用方）；被借用的 enum schema 必须晚于引用方释放。 */
    CHECK(ace_schema_free(schema) == ACE_OK);
    CHECK(ace_type_free(stray) == ACE_OK);
    CHECK(ace_enum_free(color) == ACE_OK);
    CHECK(ace_type_free(NULL) == ACE_OK);
    CHECK(ace_schema_free(NULL) == ACE_OK);
    CHECK(ace_enum_free(NULL) == ACE_OK);

    printf("schema_model: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
