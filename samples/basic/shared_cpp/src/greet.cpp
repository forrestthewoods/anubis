#include "greet.h"
#include "calc.h"
#include "util.h"
#include <cstring>
#include <string>

// State lives in the shared library: every user sees the same counter.
static int g_counter = 0;

size_t greet(const char* name, char* out, size_t out_size) {
    g_counter = calc_add(g_counter, 1);
    std::string s = "Hello, " + util_shout(name) + "!";
    if (out_size > 0) {
        size_t n = s.size() < out_size - 1 ? s.size() : out_size - 1;
        std::memcpy(out, s.data(), n);
        out[n] = '\0';
    }
    return s.size();
}

int greet_counter() { return g_counter; }

#if defined(SAMPLE_SHARED_RUNTIME)
#include <stdexcept>

std::string greet_string(const std::string& name) {
    return "Hello, " + util_shout(name) + "!";
}

void greet_check(const std::string& name) {
    if (name.empty()) {
        throw std::invalid_argument("greet: empty name");
    }
}
#endif
