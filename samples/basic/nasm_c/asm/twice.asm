section .text
global asm_twice
asm_twice:
    lea eax, [ARG0 + ARG0]
    ret
