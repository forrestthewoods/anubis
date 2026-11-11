#include "greeting.h"

#include <string>
#include <vector>

NSString* greeting_join(NSArray<NSString*>* names) {
    std::vector<std::string> parts;
    for (NSString* name in names) {
        parts.emplace_back(name.UTF8String);
    }
    std::string joined;
    for (size_t i = 0; i < parts.size(); ++i) {
        joined += (i ? ", " : "") + parts[i];
    }
    return [NSString stringWithUTF8String:joined.c_str()];
}
