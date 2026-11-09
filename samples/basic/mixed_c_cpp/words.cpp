#include "words.h"
#include <sstream>
#include <stdexcept>
#include <string>

int count_words(const char* text) {
    try {
        if (!text) {
            throw std::invalid_argument("null text");
        }
        std::istringstream in{std::string(text)};
        std::string word;
        int n = 0;
        while (in >> word) {
            ++n;
        }
        return n;
    } catch (const std::exception&) {
        return -1;
    }
}
