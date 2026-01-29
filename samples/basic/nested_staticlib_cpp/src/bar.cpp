#include <stdio.h>

#include <bar/bar.h>
#include <util/util.h>

namespace bar {
    void do_bar() {
        printf("bar::do_bar\n");
        util::print_message("bar");
    }
}
