#include <cstdio>
#include "calc.h"
#include "greet.h"

int main() {
    char a[64], b[64];
    greet("anubis", a, sizeof(a));
    greet("shared library", b, sizeof(b));
    std::printf("%s\n%s\n", a, b);
    if (greet_counter() != 2 || calc_add(40, 2) != 42) {
        std::printf("FAIL\n");
        return 1;
    }
    std::printf("PASS\n");
    return 0;
}
