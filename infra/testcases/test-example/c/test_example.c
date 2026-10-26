/*
 * Example C test body — compiled into the test-example binary via
 * coda-build (build-dep). Assertions go only through the coda.h macros
 * (counting/printing shares the source with the Rust side); the protocol
 * assembly-layer prefixes (--- Running: / PASSED: / ...) are printed by
 * init; do not touch them here.
 */

#include "coda.h"

static void test_basic(void)
{
    INFO("C section: basic functionality");
    PASS("Basic test passed");
}

void run_c_tests(void)
{
    test_basic();
}
