#include <stdio.h>

// Declared here rather than through greet.h: on Linux this program sees only
// the stub, which has no headers.
int greet_answer(void);

int main(void) {
    printf("answer: %d\n", greet_answer());
    return 0;
}
