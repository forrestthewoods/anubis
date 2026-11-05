#include <stdio.h>

#include <foo/foo.h>
#include <bar/bar.h>

// Public args propagate transitively: main -> foo -> util.
#if !defined(UTIL_TRANSITIVE_DEFINE)
#error "util's public_defines did not reach main"
#endif

int main() {
    printf("=== Nested Static Library Example ===\n");
    printf("Diamond dependency: main->foo->util, main->bar->util\n\n");

    foo::do_foo();
    bar::do_bar();

    return 0;
}
