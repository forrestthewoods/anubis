#include <cstdio>
extern "C" int clib_value(void);
int c_as_cpp();
int main() { std::printf("pre=%d clib=%d c_as_cpp=%d\n", PRE_VALUE, clib_value(), c_as_cpp()); }
