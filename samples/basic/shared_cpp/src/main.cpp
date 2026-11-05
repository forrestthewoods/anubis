#include <cstdio>
#include "calc.h"
#include "greet.h"

int main() {
    std::string a = greet("anubis");
    std::string b = greet("shared library");
    std::printf("%s\n%s\n", a.c_str(), b.c_str());
    if (greet_counter() != 2 || calc_add(40, 2) != 42) {
        std::printf("FAIL\n");
        return 1;
    }
    std::printf("PASS\n");
    return 0;
}
