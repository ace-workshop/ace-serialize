#include "ace_serialize.h"

_Static_assert(ACE_MISSING_REQUIRED_FIELD + 1 == 16, "error code count");
_Static_assert(ACE_OK == 0 && ACE_TRUNCATED_INPUT == 2 && ACE_RESOURCE_LIMIT == 6, "error codes");
_Static_assert(ACE_TYPE_MISMATCH == 13 && ACE_CYCLE_UNSUPPORTED == 14 && ACE_MISSING_REQUIRED_FIELD == 15, "error codes");
_Static_assert(ACE_ABI_VERSION == 0x00010000 && ACE_WIRE_VERSION == 1, "versions");

uint32_t (*smoke_version)(void) = ace_abi_version;
int32_t (*smoke_enum)(AceEnumSchema *, uint64_t) = ace_enum_add_variant;
int32_t (*smoke_set)(AceValue *, int32_t, AceValue *) = ace_object_set;
int32_t (*smoke_serialize)(const char *, const AceValue *, const AceOptions *, AceBuffer *) = ace_serialize;
int32_t (*smoke_deserialize)(const char *, const uint8_t *, size_t, const AceOptions *, AceValue **) = ace_deserialize;
