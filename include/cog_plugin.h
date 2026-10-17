#ifndef COG_PLUGIN_V1_H
#define COG_PLUGIN_V1_H
#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32)
#define COG_EXPORT __declspec(dllexport)
#else
#define COG_EXPORT __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Trusted, synchronous, calling-thread-only ABI. All views are borrowed.
 * No exception/panic may cross C. See docs/SDK.md for the complete contract. */
typedef uint64_t CogHandle;
typedef uint32_t CogStatus;
enum { COG_OK=0, COG_FAILURE=1, COG_INVALID=2, COG_TYPE_MISMATCH=3,
       COG_LIMIT=4, COG_PANIC=5, COG_ABI_VERSION=1 };
enum { COG_ANY=0, COG_INT=1, COG_STR=2, COG_BYTES=3, COG_HANDLE=4,
       COG_NIL=5, COG_NUMBER=6, COG_ARRAY=7, COG_RECORD=8, COG_FLOAT=9,
       COG_BOOL=10, COG_STRBUF=11, COG_ERROR=12, COG_OPAQUE=255 };
enum { COG_OUTPUT=1, COG_INPUT=2, COG_FS_READ=4, COG_FS_WRITE=8,
       COG_PROCESS=16, COG_PROCESS_INFO=32, COG_CLOCK=64, COG_SLEEP=128,
       COG_ENTROPY=256 };
typedef struct { const uint8_t *data; size_t len; } CogBytes;
typedef struct {
    uint32_t kind;
    int64_t integer;
    double number;
    CogBytes bytes;
    const CogHandle *items;
    size_t len;
} CogValueView;
typedef struct {
    uint32_t size, version;
    CogStatus (*inspect)(void *, CogHandle, CogValueView *);
    CogStatus (*make)(void *, const CogValueView *, CogHandle *);
    CogStatus (*array_get)(void *, CogHandle, size_t, CogHandle *);
    CogStatus (*array_set)(void *, CogHandle, size_t, CogHandle);
    CogStatus (*write_output)(void *, CogBytes);
    CogStatus (*set_error)(void *, CogBytes);
} CogHostV1;
typedef CogStatus (*CogCallbackV1)(void *, const CogHostV1 *, void *,
                                  const CogHandle *, size_t, CogHandle *);
typedef struct {
    uint32_t size;
    CogBytes name, parameters;
    uint8_t result;
    uint16_t effects;
    uint64_t mutations;
    CogCallbackV1 invoke;
} CogFunctionV1;
typedef struct {
    uint32_t size, version;
    size_t function_count;
    const CogFunctionV1 *functions;
    CogStatus (*create)(void **);
    void (*destroy)(void *);
} CogPluginV1;

COG_EXPORT const CogPluginV1 *cog_plugin_v1(void);

#ifdef __cplusplus
}
#endif
#endif
