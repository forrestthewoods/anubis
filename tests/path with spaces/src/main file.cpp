#include <cstdio>

#include "greet lib/greet.h"

int main() {
    std::printf("%s %s\n", greet_first(), greet_second());
    return 0;
}
