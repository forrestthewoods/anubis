#include <cstdio>
#include "shlib.h"
int main() {
    int a = shlib_bump();
    int b = shlib_bump();
    std::printf("bump=%d bump=%d\n", a, b);
}
