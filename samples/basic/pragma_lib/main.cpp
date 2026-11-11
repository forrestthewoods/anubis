// Spellings as OpenCASCADE writes them. The SDK ships Ole32.Lib/ole32.lib,
// psapi.lib and WSock32.Lib, so none of these exists on a case-sensitive
// filesystem without an alias.
#include <stdio.h>

#ifdef _WIN32
#include <winsock2.h>
#include <windows.h>
#include <objbase.h>
#include <psapi.h>
#pragma comment(lib, "Ole32.lib")
#pragma comment(lib, "Psapi.lib")
#pragma comment(lib, "WSOCK32.LIB")

int main() {
    CoTaskMemFree(nullptr);                    // ole32
    PROCESS_MEMORY_COUNTERS pmc = {};
    GetProcessMemoryInfo(GetCurrentProcess(), &pmc, sizeof(pmc));  // psapi
    unsigned short port = htons(1);            // wsock32
    printf("pragma_lib: %s\n", port == 0x0100 ? "ok" : "bad");
    return port == 0x0100 ? 0 : 1;
}
#else
int main() {
    printf("pragma_lib: ok (not Windows)\n");
    return 0;
}
#endif
