#include <cstdio>
long restate_lib_std();
int main() { std::printf("lib=%ld exe=%ld\n", restate_lib_std(), (long)__cplusplus); }
