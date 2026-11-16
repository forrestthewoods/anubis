// The same two functions for arm64 targets, which NASM can't assemble:
// a .S file in a cc rule's srcs, preprocessed and assembled by clang.
#ifdef __APPLE__
#define SYM(name) _##name  /* Mach-O C symbols carry a leading underscore */
#else
#define SYM(name) name
#endif

    .text
    .p2align 2
    .globl SYM(asm_answer)
SYM(asm_answer):
    mov w0, #42
    ret

    .p2align 2
    .globl SYM(asm_twice)
SYM(asm_twice):
    add w0, w0, w0
    ret
