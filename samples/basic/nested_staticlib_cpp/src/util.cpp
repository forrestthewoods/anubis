#include <stdio.h>

#include <util/util.h>

namespace util {
    void print_message(const char* source) {
        printf("util::print_message called from %s\n", source);
    }
}
