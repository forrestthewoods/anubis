%include "answer.inc"
%include "abi.inc"

section .text
global asm_answer
asm_answer:
    mov eax, ANSWER
    ret
