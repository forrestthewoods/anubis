#include "util.h"
#include <cctype>

std::string util_shout(const std::string& s) {
    std::string out = s;
    for (char& c : out) c = (char)std::toupper((unsigned char)c);
    return out;
}
