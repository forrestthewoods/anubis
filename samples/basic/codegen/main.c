#include <stdio.h>

/* Defined in generated/greeting.c, which //samples/basic/codegen:generate
 * writes from greeting.txt. */
const char* greeting(void);

int main(void) {
    printf("%s", greeting());
    return 0;
}
