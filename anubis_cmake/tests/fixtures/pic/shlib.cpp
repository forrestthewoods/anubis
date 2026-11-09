#include "shlib.h"
int bump();
extern "C" int shlib_bump(void) { return bump(); }
