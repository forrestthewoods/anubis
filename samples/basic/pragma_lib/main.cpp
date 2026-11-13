// Library names as real code spells them (the first three are
// OpenCASCADE's). The SDK ships Ole32.Lib, psapi.lib, WSock32.Lib and
// Version.Lib, and a Windows link finds them under any casing.
#include <stdio.h>

#ifdef _WIN32
#include <winsock2.h>
#include <windows.h>
#include <objbase.h>
#include <psapi.h>
#pragma comment(lib, "Ole32.lib")
#pragma comment(lib, "Psapi.lib")
#pragma comment(lib, "WSOCK32.LIB")
#pragma comment(lib, "vErSiOn.LiB")

int main() {
    CoTaskMemFree(nullptr);                    // ole32
    PROCESS_MEMORY_COUNTERS pmc = {};
    GetProcessMemoryInfo(GetCurrentProcess(), &pmc, sizeof(pmc));  // psapi
    unsigned short port = htons(1);            // wsock32
    GetFileVersionInfoSizeA("", nullptr);      // version
    printf("pragma_lib: %s\n", port == 0x0100 ? "ok" : "bad");
    return port == 0x0100 ? 0 : 1;
}
#else
int main() {
    printf("pragma_lib: ok (not Windows)\n");
    return 0;
}
#endif
