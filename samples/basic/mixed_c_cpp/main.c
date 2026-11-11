#include <stdio.h>
#include "words.h"

int main(void) {
    int n = count_words("a C program using a C++ library");
    int bad = count_words(0);
    printf("words: %d, null: %d\n", n, bad);
    if (n != 7 || bad != -1) {
        printf("FAIL\n");
        return 1;
    }
    printf("PASS\n");
    return 0;
}
