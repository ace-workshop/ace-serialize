/* P3-3a options 模型真跑：建 options、设置 packed/limits/build_nesting、注册两个含字段 schema
 * （其中一个带 enum 字段）、重复 type_key 判重；随后释放调用方侧全部结构，仍可对 options 继续操作
 * 并正常释放（本包无 options 只读入口，故用依赖库内状态的行为间接验证副本仍存活）。
 *
 * 编译并链接 Rust cdylib：
 *   cargo build -p ace-serialize-ffi
 *   cc -std=c17 -Wall -Wextra -Iinclude tests/c/options_model.c \
 *      -Ltarget/debug -lace_serialize_ffi -Wl,-rpath,"$PWD/target/debug" -o target/debug/options_model_c
 *   ./target/debug/options_model_c
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
    /* —— 1. options 默认构造 + setter —— */
    AceOptions *options = NULL;
    CHECK(ace_options_new(&options) == ACE_OK && options != NULL);
    CHECK(ace_options_new(NULL) == ACE_INVALID_ARGUMENT);

    CHECK(ace_options_set_packed(options, 0) == ACE_OK);
    CHECK(ace_options_set_packed(options, 1) == ACE_OK);
    /* 仅 0/1：其余取值 → 1。 */
    CHECK(ace_options_set_packed(options, 2) == ACE_INVALID_ARGUMENT);

    /* 头里的参数序：depth, input, raw, decoded, records, slots, collection_len,
       references, map_string_key_bytes。 */
    CHECK(ace_options_set_limits(options, 64, 1u << 20, 1u << 20, 1u << 20, 1024, 2048,
                                 4096, 512, 256) == ACE_OK);
    /* 零值合法：该维度变最严格上限，不是禁用限制。 */
    CHECK(ace_options_set_limits(options, 0, 0, 0, 0, 0, 0, 0, 0, 0) == ACE_OK);

    /* 解码侧身份构造闭包峰值（R25）：默认 256，与 max_depth 分列。 */
    CHECK(ace_options_set_build_nesting(options, 64) == ACE_OK);
    CHECK(ace_options_set_build_nesting(options, 0) == ACE_OK);

    CHECK(ace_options_set_packed(NULL, 1) == ACE_INVALID_ARGUMENT);
    CHECK(ace_options_set_limits(NULL, 0, 0, 0, 0, 0, 0, 0, 0, 0) == ACE_INVALID_ARGUMENT);
    CHECK(ace_options_set_build_nesting(NULL, 1) == ACE_INVALID_ARGUMENT);

    /* —— 2. 两个含字段 schema，其中之一带 enum 字段（含 list<enum> 借用链）—— */
    AceSchema *record = NULL;
    CHECK(ace_schema_new("demo.record", &record) == ACE_OK && record != NULL);
    AceTypeRef *count = NULL;
    CHECK(ace_type_prim(ACE_FK_I32, &count) == ACE_OK);
    CHECK(ace_schema_add_field(record, 0, count) == ACE_OK);
    AceTypeRef *text = NULL;
    CHECK(ace_type_str(&text) == ACE_OK);
    CHECK(ace_schema_add_field(record, 1, text) == ACE_OK);

    AceEnumSchema *color = NULL;
    CHECK(ace_enum_new("demo.color", &color) == ACE_OK && color != NULL);
    CHECK(ace_enum_set_repr(color, ACE_FK_I16) == ACE_OK);
    CHECK(ace_enum_add_variant(color, 0x0000u) == ACE_OK);
    CHECK(ace_enum_add_variant(color, 0xFFFFu) == ACE_OK);
    CHECK(ace_enum_set_default_variant(color, 0xFFFFu) == ACE_OK);

    AceSchema *paint = NULL;
    CHECK(ace_schema_new("demo.paint", &paint) == ACE_OK && paint != NULL);
    AceTypeRef *color_field = NULL;
    CHECK(ace_type_enum(color, &color_field) == ACE_OK);
    CHECK(ace_schema_add_field(paint, 0, color_field) == ACE_OK);
    AceTypeRef *palette_enum = NULL;
    CHECK(ace_type_enum(color, &palette_enum) == ACE_OK);
    AceTypeRef *palette = NULL;
    CHECK(ace_type_list(palette_enum, &palette) == ACE_OK);
    CHECK(ace_schema_add_field(paint, 1, palette) == ACE_OK);

    /* 深拷贝进库：成功后调用方仍持有自己那份。 */
    CHECK(ace_options_add_schema(options, record) == ACE_OK);
    CHECK(ace_options_add_schema(options, paint) == ACE_OK);

    /* 收编的字段节点仍归 schema：单独 type_free → 1。 */
    CHECK(ace_type_free(count) == ACE_INVALID_ARGUMENT);
    CHECK(ace_type_free(color_field) == ACE_INVALID_ARGUMENT);
    CHECK(ace_type_free(palette) == ACE_INVALID_ARGUMENT);

    /* NULL options / NULL schema → 1。 */
    CHECK(ace_options_add_schema(options, NULL) == ACE_INVALID_ARGUMENT);
    CHECK(ace_options_add_schema(NULL, record) == ACE_INVALID_ARGUMENT);

    /* —— 3. 重复 type_key → 1，且不影响已注册集合 —— */
    AceSchema *duplicate = NULL;
    CHECK(ace_schema_new("demo.record", &duplicate) == ACE_OK);
    CHECK(ace_options_add_schema(options, duplicate) == ACE_INVALID_ARGUMENT);
    CHECK(ace_schema_free(duplicate) == ACE_OK);

    /* —— 4. 释放调用方侧全部结构（先 free 引用方 schema，再 free 被借用 enum）—— */
    CHECK(ace_schema_free(record) == ACE_OK);
    CHECK(ace_schema_free(paint) == ACE_OK);
    CHECK(ace_enum_free(color) == ACE_OK);
    CHECK(ace_enum_free(NULL) == ACE_OK);

    /* 调用方结构已全部释放：以下行为依赖库内副本，且不崩溃。 */
    /* ① 同一 type_key 再注册仍判重：白名单键是库内副本，不是调用方指针/内存。 */
    AceSchema *again = NULL;
    CHECK(ace_schema_new("demo.record", &again) == ACE_OK);
    CHECK(ace_options_add_schema(options, again) == ACE_INVALID_ARGUMENT);
    CHECK(ace_schema_free(again) == ACE_OK);

    /* ② 新 key 仍可注册，句柄仍接受 setter。 */
    AceSchema *later = NULL;
    CHECK(ace_schema_new("demo.later", &later) == ACE_OK);
    AceTypeRef *flag = NULL;
    CHECK(ace_type_prim(ACE_FK_BOOL, &flag) == ACE_OK);
    CHECK(ace_schema_add_field(later, 0, flag) == ACE_OK);
    CHECK(ace_options_add_schema(options, later) == ACE_OK);
    CHECK(ace_schema_free(later) == ACE_OK);
    CHECK(ace_options_set_packed(options, 0) == ACE_OK);
    CHECK(ace_options_set_build_nesting(options, 32) == ACE_OK);

    /* ③ 最终释放正常；NULL 视为已释放。 */
    CHECK(ace_options_free(options) == ACE_OK);
    CHECK(ace_options_free(NULL) == ACE_OK);

    printf("options_model: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
