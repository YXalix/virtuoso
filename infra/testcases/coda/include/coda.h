/*
 * coda.h — C-side interface of the Virtuoso test framework (coda).
 *
 * C test bodies land via FFI on coda's (Rust) unified printing/counting
 * path: marker protocol v1 (frozen text) has a single source and the judge
 * treats C and Rust test cases alike. The macros pre-format with snprintf
 * and then call `coda_*`; usage matches the old test_common.h:
 *
 *     #include "coda.h"
 *     PASS("opened %s", path);
 *     FAIL("errno=%d", errno);
 *
 * Note: assertion output goes only through this header's macros; never
 * printf the protocol prefixes directly (`--- Running:`/`PASSED:`/
 * `FAILED:`/`Test Results:`/`TEST_COMPLETE:` belong to init's assembly
 * layer).
 */

#ifndef CODA_H
#define CODA_H

#include <stdio.h>

/* FFI: implemented in coda (Rust staticlib/rlib linked into this binary). */
void coda_pass(const char *msg);
void coda_fail(const char *msg);
void coda_skip(const char *msg);
void coda_info(const char *msg);

/* Counters (read-only snapshots): same source as coda::counters(). */
unsigned coda_count_passed(void);
unsigned coda_count_failed(void);
unsigned coda_count_skipped(void);

#define PASS(fmt, ...)                                                       \
    do {                                                                     \
        char _coda_buf[512];                                                 \
        snprintf(_coda_buf, sizeof(_coda_buf), fmt, ##__VA_ARGS__);          \
        coda_pass(_coda_buf);                                                \
    } while (0)

#define FAIL(fmt, ...)                                                       \
    do {                                                                     \
        char _coda_buf[512];                                                 \
        snprintf(_coda_buf, sizeof(_coda_buf), fmt, ##__VA_ARGS__);          \
        coda_fail(_coda_buf);                                                \
    } while (0)

#define SKIP(fmt, ...)                                                       \
    do {                                                                     \
        char _coda_buf[512];                                                 \
        snprintf(_coda_buf, sizeof(_coda_buf), fmt, ##__VA_ARGS__);          \
        coda_skip(_coda_buf);                                                \
    } while (0)

#define INFO(fmt, ...)                                                       \
    do {                                                                     \
        char _coda_buf[512];                                                 \
        snprintf(_coda_buf, sizeof(_coda_buf), fmt, ##__VA_ARGS__);          \
        coda_info(_coda_buf);                                                \
    } while (0)

#endif /* CODA_H */
