/*
 * Compatibility wrapper for extensible build
 * Maps sqlite_world string types/functions to sqlite_extensible equivalents
 * The export types (exports_sqlite_wasm_*) are already the same in both headers
 */
#ifndef SQLITE_WORLD_COMPAT_H
#define SQLITE_WORLD_COMPAT_H

#include "sqlite_extensible.h"

/* String type mapping - this is the key difference between worlds */
typedef sqlite_extensible_string_t sqlite_world_string_t;
#define sqlite_world_string_dup sqlite_extensible_string_dup
#define sqlite_world_string_free sqlite_extensible_string_free
#define sqlite_world_string_set sqlite_extensible_string_set

/* List types */
typedef sqlite_extensible_list_u8_t sqlite_world_list_u8_t;
typedef sqlite_extensible_list_string_t sqlite_world_list_string_t;

#endif /* SQLITE_WORLD_COMPAT_H */
