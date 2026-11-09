#include <cstdio>
#include "cfg.h"
int dep_value();
int main() {
    std::printf("cfg=%s char=%s value=%d\n", CFG, (char)-1 < 0 ? "signed" : "unsigned", VALUE + dep_value() - 1);
}
