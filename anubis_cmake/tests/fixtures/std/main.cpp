#include <cstdio>
long lib_std();
int main() { std::printf("lib=%ld exe=%ld\n", lib_std(), (long)__cplusplus); }
