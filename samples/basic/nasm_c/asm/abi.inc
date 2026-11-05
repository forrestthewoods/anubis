; First integer argument register for the target ABI.
%ifidn __OUTPUT_FORMAT__, win64
    %define ARG0 ecx
%else
    %define ARG0 edi
    section .note.GNU-stack noalloc noexec nowrite progbits
%endif
