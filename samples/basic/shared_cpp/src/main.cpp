#include <cstdio>
#include "calc.h"
#include "greet.h"
#include <stdexcept>

int main() {
    char a[64], b[64];
    greet("anubis", a, sizeof(a));
    greet("shared library", b, sizeof(b));
    std::printf("%s\n%s\n", a, b);
    if (greet_counter() != 2 || calc_add(40, 2) != 42) {
        std::printf("FAIL\n");
        return 1;
    }
#if defined(SAMPLE_SHARED_RUNTIME)
    // A std::string allocated in greet and freed here, and an exception
    // thrown in greet and caught here by its type.
    std::string s = greet_string("runtime");
    std::printf("%s\n", s.c_str());
    bool caught = false;
    try {
        greet_check("");
    } catch (const std::invalid_argument& e) {
        std::printf("caught: %s\n", e.what());
        caught = true;
    }
    if (s != "Hello, RUNTIME!" || !caught) {
        std::printf("FAIL\n");
        return 1;
    }
#endif
    std::printf("PASS\n");
    return 0;
}
