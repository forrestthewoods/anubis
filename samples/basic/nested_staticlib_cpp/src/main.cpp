#include <stdio.h>

#include <foo/foo.h>
#include <bar/bar.h>

int main() {
    printf("=== Nested Static Library Example ===\n");
    printf("Diamond dependency: main -> foo, bar -> util\n\n");

    foo::do_foo();
    bar::do_bar();

    printf("\nDone!\n");
    return 0;
}
