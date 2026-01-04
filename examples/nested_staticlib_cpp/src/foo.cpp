#include <stdio.h>

#include <foo/foo.h>
#include <util/util.h>

namespace foo {
    void do_foo() {
        printf("foo::do_foo\n");
        util::print_message("foo");
    }
}
