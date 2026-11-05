#include <stdio.h>

int asm_answer(void);
int asm_twice(int x);

int main(void) {
    int answer = asm_answer();
    int twice = asm_twice(21);
    printf("asm_answer() = %d, asm_twice(21) = %d\n", answer, twice);
    return (answer == 42 && twice == 42) ? 0 : 1;
}
