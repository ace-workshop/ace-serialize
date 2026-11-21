#include "ace_serialize.h"

/* 门 A 冒烟：只证明本头在 c99/c11 下可编译、且新增符号可被引用；
   只编译不链接（FFI 侧尚未导出符号），故不实际调用。 */

/* c99 无 _Static_assert，用文件作用域 typedef 做编译期断言 */
typedef char gate_a_duplicate_key_is_16[(ACE_DUPLICATE_KEY == 16 &&
                                        ACE_MISSING_REQUIRED_FIELD == 15) ? 1 : -1];

static uint32_t (*const gate_a_identity_entry_size)(void) = ace_identity_entry_size;
static uint32_t (*const gate_a_identity_entry_align)(void) = ace_identity_entry_align;
static int32_t (*const gate_a_options_set_build_nesting)(AceOptions *, size_t) =
    ace_options_set_build_nesting;
/* R32：共享身份入口的取址形态（不执行，只证明声明可用且签名匹配） */
static int32_t (*const gate_a_value_share)(const AceValue *, AceValue **) = ace_value_share;

int main(void)
{
    return (gate_a_identity_entry_size != 0 && gate_a_identity_entry_align != 0 &&
            gate_a_options_set_build_nesting != 0 && gate_a_value_share != 0)
               ? 0
               : 1;
}
