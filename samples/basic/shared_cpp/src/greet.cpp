#include "greet.h"
#include "calc.h"
#include "util.h"

// State lives in the shared library: every user sees the same counter.
static int g_counter = 0;

std::string greet(const std::string& name) {
    g_counter = calc_add(g_counter, 1);
    return "Hello, " + util_shout(name) + "!";
}

int greet_counter() { return g_counter; }
