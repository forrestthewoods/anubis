// Spellings no other alias rule covers (natron.toml header_aliases).
#include <stdio.h>

#ifdef _WIN32
#include <windows.h>
#include <BaseTsd.h>  // zstd's zlibWrapper and wslay spell it this way
#include <comdef.h>   // includes <OleCtl.h>

int main() {
    SSIZE_T n = 1;  // BaseTsd.h
    _com_error err(S_OK);  // comdef.h
    printf("header_case: %s\n", n == 1 && err.Error() == S_OK ? "ok" : "bad");
    return n == 1 && err.Error() == S_OK ? 0 : 1;
}
#else
int main() {
    printf("header_case: ok (not Windows)\n");
    return 0;
}
#endif
