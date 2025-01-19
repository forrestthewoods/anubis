// virtual_alloc.cpp : This file contains the 'main' function. Program execution begins and ends there.
//

#if _WIN32
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <Windows.h>
#include <psapi.h>
#else
#include <sys/mman.h>
//#include <sys/sysinfo.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#endif

#include <algorithm>
#include <assert.h>
#include <charconv>
#include <chrono>
#include <cstdint>
#include <format>
#include <iostream>
#include <vector>
#include <type_traits>

#define ENABLE_SUPERLUMINAL 0
#if ENABLE_SUPERLUMINAL
#include <Superluminal/PerformanceAPI_capi.h>
#else
#define PerformanceAPI_BeginEvent(a,b,c)
#define PerformanceAPI_EndEvent()
#endif

using namespace std::chrono;
using Clock = high_resolution_clock;

static const size_t ONE_KB = 1024;
static const size_t ONE_MB = 1024 * 1024;
static const size_t ONE_GB = 1024 * 1024 * 1024;
static const size_t ONE_TB = 1024llu * 1024 * 1024 * 1024;

static const size_t PAGE_SIZE = []() -> size_t {
#if _WIN32
    SYSTEM_INFO systemInfo;
    GetSystemInfo(&systemInfo);
    return systemInfo.dwPageSize;
#else
    return sysconf(_SC_PAGESIZE);
#endif
}();

static const size_t RESERVE_LIMIT = []() -> size_t {
#ifdef _WIN32
    MEMORYSTATUSEX memStatus;
    memStatus.dwLength = sizeof(memStatus);
    GlobalMemoryStatusEx(&memStatus);
    return memStatus.ullTotalVirtual;
#else
    // Can't figure out how to query this 
    // 2^47 = 128TiB
    return 140737488355328llu; 
    /*
    struct sysinfo si;
    if (sysinfo(&si) == 0) {
        // Total virtual memory = RAM + swap
        return (si.totalram + si.totalswap) * si.mem_unit;
    }
    return 0;
    */
#endif
}();

static const size_t COMMIT_LIMIT = []() -> size_t {
#if _WIN32
    PERFORMANCE_INFORMATION perfInfo;
    perfInfo.cb = sizeof(perfInfo);

    GetPerformanceInfo(&perfInfo, sizeof(perfInfo));
    size_t commitLimit = perfInfo.CommitLimit * perfInfo.PageSize;
    return commitLimit;
#else
    return ONE_GB * 32;
    //struct sysinfo si;
    //if (sysinfo(&si) == 0) {
    //    // On Linux, commit limit is typically RAM + swap
    //    return (si.totalram + si.totalswap) * si.mem_unit;
    //}
    //return 0;
#endif
}();


// Small Helpers
size_t roundUpToPage(size_t size) { return (size + PAGE_SIZE - 1) & ~(PAGE_SIZE - 1); }
size_t roundDownToPage(size_t size) { return size & ~(PAGE_SIZE - 1); }
void* offsetPtr(void* ptr, size_t offset) { return (void*)((char*)ptr + offset); }

std::string prettyTime(nanoseconds ns) {
    if (ns.count() < 1000) return std::format("{}ns", ns.count());
    else if (ns.count() < 1000 * 1000) return std::format("{}us", ns.count() / 1000);
    else if (ns.count() < 1000llu * 1000llu * 1000llu * 10llu) return std::format("{}ms", ns.count() / 1000 / 1000);
    else return std::format("{}s", ns.count() / 1000 / 1000 / 1000);
}

std::string prettyBytes(size_t bytes) {
    if (bytes < 1024llu) return std::format("{}bytes", bytes);
    else if (bytes < 1024llu * 1024) return std::format("{}KiB", bytes / 1024);
    else if (bytes < 1024llu * 1024 * 1024) return std::format("{}MiB", bytes / 1024 / 1024);
    else if (bytes < 1024llu * 1024 * 1024 * 1024) return std::format("{}GiB", bytes / 1024 / 1024 / 1024);
    else return std::format("{}TiB", bytes / 1024 / 1024 / 1024 / 1024);
}

enum class CommitStrategy { Page, Multiplier, All };
const double CommitMultiplier = 1.5;

template<typename T, CommitStrategy commitStrategy>
struct VirtualVec {
    T* data = nullptr;
    size_t count = 0;
    size_t numVirtualBytes = 0;
    void* nextPage = nullptr;
    void* endPage = nullptr; // one byte past end of last page

    VirtualVec() = default;
    ~VirtualVec() {
        if (data) {
            if constexpr (!std::is_trivially_destructible_v<T>) {
                for (size_t i = 0; i < count; ++i) {
                    data[i].~T(); // destroy!
                }
            }
#if _WIN32
            VirtualFree(data, 0, MEM_RELEASE);
#else
            munmap(data, numVirtualBytes);  // Note: needs original size used in mmap
#endif
        }
    }
    
    void init(size_t numElements) {
#if !_WIN32
        // Linux only supports ALL
        assert(commitStrategy == CommitStrategy::All);
#endif

        PerformanceAPI_BeginEvent("VirtualVec::init", nullptr, PERFORMANCEAPI_DEFAULT_COLOR);

        assert(data == nullptr); // don't handle re-init
        numVirtualBytes = roundUpToPage(numElements * sizeof(T)); // align to page size

#if _WIN32
        if constexpr (commitStrategy == CommitStrategy::All) {
            // reserve AND commit
            void* raw = VirtualAlloc(nullptr, numVirtualBytes, MEM_COMMIT, PAGE_READWRITE);
            assert(raw != nullptr);
            data = static_cast<T*>(raw);
            endPage = offsetPtr(data, numVirtualBytes);
            nextPage = endPage;
        }
        else {
            // reserve but do NOT commit
            void* raw = VirtualAlloc(nullptr, numVirtualBytes, MEM_RESERVE, PAGE_READWRITE);
            assert(raw != nullptr);
            data = static_cast<T*>(raw);
            nextPage = data;
            endPage = offsetPtr(data, numVirtualBytes);
        }
#else
        void* raw = mmap(nullptr, numVirtualBytes,
            PROT_READ | PROT_WRITE,          // Protection flags
            MAP_PRIVATE | MAP_ANONYMOUS,      // Mapping flags
            -1,                              // File descriptor (not used)
            0);                              // Offset (not used)
        assert(raw != nullptr);
        data = static_cast<T*>(raw);
        endPage = offsetPtr(data, numVirtualBytes);
        nextPage = endPage;
#endif

        PerformanceAPI_EndEvent();
    }

    void push_back(T const& v) {
        assert(data != nullptr); // must call init first

        T* nextSlot = data + count;
        while (nextSlot >= nextPage) {
            reserveMore();
        }

        *nextSlot = v;
        count += 1;
    }

    void ensureIndex(size_t idx) {
        char* lastByte = ((char*)data + idx * sizeof(T)) + (sizeof(T) - 1);
        while (lastByte >= nextPage) {
            reserveMore();
        }
    }

    void reserveMore() {
#if _WIN32
        assert(nextPage < endPage);

        if constexpr (commitStrategy == CommitStrategy::Page) {
            VirtualAlloc(nextPage, PAGE_SIZE, MEM_COMMIT, PAGE_READWRITE); // commit first page
            nextPage = static_cast<void*>(static_cast<char*>(nextPage) + PAGE_SIZE);
        }
        else if constexpr (commitStrategy == CommitStrategy::Multiplier) {
            size_t currCommitedBytes = ((char*)nextPage - (char*)data);
            void* targetPage = nullptr;
            if (currCommitedBytes == 0) {
                targetPage = offsetPtr(data, PAGE_SIZE);
            }
            else {
                targetPage = offsetPtr(data, size_t(double(currCommitedBytes) * CommitMultiplier));
            }
            targetPage = std::min(targetPage, endPage);

            while (nextPage < targetPage && nextPage < endPage) {
                VirtualAlloc(nextPage, PAGE_SIZE, MEM_COMMIT, PAGE_READWRITE); // commit first page
                nextPage = static_cast<void*>(static_cast<char*>(nextPage) + PAGE_SIZE);
            }
        }
        else {
        }
#else
        // Linux doesn't have to deal with this
        assert(false);
#endif
    }
};

void run_subprocess(const char* exePath, size_t test, size_t subtest, size_t extra) {
    PerformanceAPI_BeginEvent("run_subprocess", nullptr, PERFORMANCEAPI_DEFAULT_COLOR);

#if _WIN32
    HANDLE pipe_read, pipe_write;
    SECURITY_ATTRIBUTES sa = {
        .nLength = sizeof(SECURITY_ATTRIBUTES),
        .lpSecurityDescriptor = NULL,
        .bInheritHandle = TRUE,
    };

    // Create pipe for stdout
    if (!CreatePipe(&pipe_read, &pipe_write, &sa, 0)) {
        // Handle error
    }

    // Ensure the read handle is not inherited
    SetHandleInformation(pipe_read, HANDLE_FLAG_INHERIT, 0);

    STARTUPINFOA si = {
        .cb = sizeof(STARTUPINFO),
        .dwFlags = STARTF_USESTDHANDLES,
        .hStdOutput = pipe_write,
        .hStdError = pipe_write,
    };
    PROCESS_INFORMATION pi;

    std::string cli = std::format("{} {} {} {}", exePath, test, subtest, extra);

    if (!CreateProcessA(
        NULL,           // Application name
        cli.data(),        // Command line 
        NULL,           // Process security attributes
        NULL,           // Thread security attributes
        TRUE,           // Inherit handles
        0,             // Creation flags
        NULL,          // Use parent's environment
        NULL,          // Use parent's directory
        &si,           // STARTUPINFO pointer
        &pi            // PROCESS_INFORMATION pointer
    )) {
        // Handle error
    }

    // Close the write end of pipe in parent
    CloseHandle(pipe_write);

    // Read from pipe
    char buffer[4096];
    DWORD bytes_read;
    while (ReadFile(pipe_read, buffer, sizeof(buffer) - 1, &bytes_read, NULL)) {
        if (bytes_read == 0) break;
        buffer[bytes_read] = '\0';
        // Process output here
        printf("%s", buffer);
    }

    // Wait for process to complete
    WaitForSingleObject(pi.hProcess, INFINITE);

    // Cleanup
    CloseHandle(pipe_read);
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
#else
int pipe_fd[2];
    if (pipe(pipe_fd) == -1) {
        perror("pipe");
        return;
    }

    pid_t pid = fork();
    if (pid == -1) {
        perror("fork");
        close(pipe_fd[0]);
        close(pipe_fd[1]);
        return;
    }

    if (pid == 0) {  // Child process
        // Close read end of pipe
        close(pipe_fd[0]);

        // Redirect stdout and stderr to pipe
        dup2(pipe_fd[1], STDOUT_FILENO);
        dup2(pipe_fd[1], STDERR_FILENO);
        close(pipe_fd[1]);

        // Convert arguments to strings
        std::string test_str = std::to_string(test);
        std::string subtest_str = std::to_string(subtest);
        std::string extra_str = std::to_string(extra);

        // Execute the program
        execl(exePath, exePath, 
              test_str.c_str(), 
              subtest_str.c_str(), 
              extra_str.c_str(), 
              (char*)nullptr);

        // If execl returns, there was an error
        perror("execl");
        exit(1);
    } else {  // Parent process
        // Close write end of pipe
        close(pipe_fd[1]);

        // Read from pipe
        char buffer[4096];
        ssize_t bytes_read;
        while ((bytes_read = read(pipe_fd[0], buffer, sizeof(buffer) - 1)) > 0) {
            buffer[bytes_read] = '\0';
            printf("%s", buffer);
        }

        // Close read end of pipe
        close(pipe_fd[0]);

        // Wait for child process to complete
        int status;
        waitpid(pid, &status, 0);
    }
#endif

    PerformanceAPI_EndEvent();
}

enum Tests {
    VirtualPageCommit,
    VirtualGrowCommit,
    VirtualAllCommit,
    AllocCost,
    AllocCostCommitSome,
};

int main(int argc, char** argv) {
    using NumType = int;
    const std::vector<size_t> testBytes = { ONE_MB, ONE_MB * 10, ONE_MB * 100, ONE_GB, ONE_GB * 4, ONE_GB * 16, ONE_GB * 128, ONE_TB };
    const std::vector<std::string> testBytesStr = { "1 MiB", "10 MiB", "100 MiB", "1 GiB", "4 GiB", "16GiB", "128GiB", "1TiB" };
    const size_t COMMIT_SAFE_BYTES = 6;
    const std::vector<size_t> numAllocs = { 1, 5, 10, 25, 50, 75, 100, 250, 500, /*750, 1000, 2500, 5000, 7500, 10000, 25000, 50000, 75000, 100000*/ };
    const std::vector<size_t> allocSizes = { ONE_MB, ONE_MB * 5, ONE_MB * 10, ONE_MB * 50, ONE_MB * 100, ONE_MB * 500, ONE_GB, ONE_GB * 4, ONE_GB * 8, ONE_GB * 16 };

    if (argc == 1) {
        std::cout << std::format("Page Size: {}\n", PAGE_SIZE);
        std::cout << std::format("Reserve Limit: {}\n", prettyBytes(RESERVE_LIMIT)); // ~127TB
        std::cout << std::format("Commit Limit: {}\n\n", prettyBytes(COMMIT_LIMIT)); // ~100GB

#if 1
#if _WIN32
        std::cout << "VirtualVec, reserve, no commit, no write\n";
        for (int i = 0; i < testBytes.size(); ++i) { run_subprocess(argv[0], VirtualPageCommit, i, 0); }
        std::cout << "\n";
#endif

        std::cout << "VirtualVec, commit all, no write\n";
        for (int i = 0; i < COMMIT_SAFE_BYTES; ++i) { run_subprocess(argv[0], VirtualAllCommit, i, 0); }
        std::cout << "\n";

#if _WIN32
        std::cout << "VirtualVec, commit page at a time, write all\n";
        for (int i = 0; i < COMMIT_SAFE_BYTES; ++i) { run_subprocess(argv[0], VirtualPageCommit, i, 1); }
        std::cout << "\n";

        std::cout << "VirtualVec, commit by 1.5x, write all\n";
        for (int i = 0; i < COMMIT_SAFE_BYTES; ++i) { run_subprocess(argv[0], VirtualGrowCommit, i, 1); }
        std::cout << "\n";
#endif

        std::cout << "VirtualVec, commit all, write all\n";
        for (int i = 0; i < COMMIT_SAFE_BYTES; ++i) { run_subprocess(argv[0], VirtualAllCommit, i, 1); }
        std::cout << "\n";

        // VirtualVec, alloc lots, no commit, no write
        for (int j = 0; j < allocSizes.size(); ++j) {
            size_t allocSize = allocSizes[j];
            std::cout << std::format("Call VirtualAlloc({}) N times, no commit, no write\n", prettyBytes(allocSize));
            for (int i = 0; i < numAllocs.size(); ++i) {
                size_t virtualSize = allocSize * numAllocs[i];
                if (virtualSize < RESERVE_LIMIT / 2) {
                    run_subprocess(argv[0], AllocCost, i, allocSize);
                }
            }
            std::cout << "\n";
        }
#endif

        // VirtualVec, alloc lots, commit 4gb of pages
        for (int j = 0; j < allocSizes.size(); ++j) {
            size_t allocSize = allocSizes[j];
            std::cout << std::format("Call VirtualAlloc({}) N times, commit pages by writing one byte per page\n", prettyBytes(allocSize));
            for (int i = 0; i < numAllocs.size(); ++i) {
                size_t virtualSize = allocSize * numAllocs[i];
                if (virtualSize < RESERVE_LIMIT / 2) {
                    run_subprocess(argv[0], AllocCostCommitSome, i, allocSize);
                }
            }
            std::cout << "\n";
        }



        return 0;
    }

    size_t test = 0, subtest = 0, extra = 0; 
    std::from_chars(argv[1], argv[1] + strlen(argv[1]), test);
    std::from_chars(argv[2], argv[2] + strlen(argv[2]), subtest);
    std::from_chars(argv[3], argv[3] + strlen(argv[3]), extra);

    // Virtual, Page Commit
    if (test == VirtualPageCommit) {
        auto start = Clock::now();

        VirtualVec<NumType, CommitStrategy::Page> nums;
        size_t numBytes = testBytes[subtest];
        size_t numElements = numBytes / sizeof(NumType);
        nums.init(numElements);
        if (extra == 1) {
            for (int i = 0; i < numElements; ++i) {
                nums.push_back(i);
            }
        }

        auto end = Clock::now();
        std::cout << "    Bytes: " << testBytesStr[subtest] << "  Time: " << prettyTime(end - start) << "\n";
    }

    // Virtual, 1.5x Commit
    if (test == VirtualGrowCommit) {
        auto start = Clock::now();

        VirtualVec<NumType, CommitStrategy::Multiplier> nums;
        size_t numBytes = testBytes[subtest];
        size_t numElements = numBytes / sizeof(NumType);
        nums.init(numElements);
        if (extra == 1) {
            for (int i = 0; i < numElements; ++i) {
                nums.push_back(i);
            }
        }

        auto end = Clock::now();
        std::cout << "    Bytes: " << testBytesStr[subtest] << "  Time: " << prettyTime(end - start) << "\n";
    }

    // Virtual, Full Commit
    if (test == VirtualAllCommit) {
        auto start = Clock::now();

        VirtualVec<NumType, CommitStrategy::All> nums;
        size_t numBytes = testBytes[subtest];
        size_t numElements = numBytes / sizeof(NumType);
        nums.init(numElements);
        if (extra == 1) {
            for (int i = 0; i < numElements; ++i) {
                nums.push_back(i);
            }
        }

        auto end = Clock::now();
        std::cout << "    Bytes: " << testBytesStr[subtest] << "  Time: " << prettyTime(end - start) << "\n";
    }
        
    // Virtual Alloc cost
    if (test == AllocCost) {
        const size_t num_bytes = extra;
        const size_t num_elements = num_bytes / sizeof(NumType);

        size_t numAlloc = numAllocs[subtest];

#if _WIN32
        std::vector<VirtualVec<NumType, CommitStrategy::Page>> vecs;
#else
        std::vector<VirtualVec<NumType, CommitStrategy::All>> vecs;
#endif
        vecs.resize(numAlloc);
        auto start = Clock::now();
        duration<long long, std::nano> lastAlloc = {};
        for (int j = 0; j < vecs.size(); ++j) {
            if (j == vecs.size() - 1) {
                auto s = Clock::now();
                vecs[j].init(num_elements);
                auto e = Clock::now();
                lastAlloc = e - s;
            }
            else {
                vecs[j].init(num_elements);
            }
        }
        auto end = Clock::now();
        auto elapsedTotal = duration_cast<nanoseconds>(end - start);
        auto elapsedPer = elapsedTotal / numAlloc;
        size_t totalBytes = num_bytes * numAlloc;
        std::cout << "    N: " << numAllocs[subtest] 
            << "  TotalTime: "  << prettyTime(elapsedTotal) 
            << "  TotalReserved: " << prettyBytes(totalBytes)
            << "  PerVirtualAlloc: " << elapsedPer << " / " << prettyTime(elapsedPer) 
            << "  PerVirtualMib: " << prettyTime(elapsedTotal / (totalBytes / ONE_MB))
            << "\n";
    }

    if (test == AllocCostCommitSome) {
        const size_t num_bytes = extra;
        const size_t num_elements = num_bytes / sizeof(NumType);
        size_t numAlloc = numAllocs[subtest];

        // Allocate
#if _WIN32
        std::vector<VirtualVec<NumType, CommitStrategy::Page>> vecs;
#else
        std::vector<VirtualVec<NumType, CommitStrategy::All>> vecs;
#endif
        vecs.resize(numAlloc);
        auto start = Clock::now();
        duration<long long, std::nano> lastAlloc = {};
        for (int j = 0; j < vecs.size(); ++j) {
            if (j == vecs.size() - 1) {
                auto s = Clock::now();
                vecs[j].init(num_elements);
                auto e = Clock::now();
                lastAlloc = e - s;
            }
            else {
                vecs[j].init(num_elements);
            }
        }
        auto end = Clock::now();
        auto elapsedAlloc = duration_cast<nanoseconds>(end - start);
        auto elapsedPerAlloc = elapsedAlloc / numAlloc;
        size_t totalBytes = num_bytes * numAlloc;


        // Commit a lot of pages by writing first byte of page
        start = Clock::now();
        const size_t commitTotal = ONE_GB * 4;
        int64_t commitLeft = commitTotal;
        int nextVecIdx = 0;
        std::vector<size_t> nextPages;
        nextPages.resize(vecs.size());
        size_t numPagesWritten = 0;
        while (commitLeft > 0) {
            auto& vec = vecs[nextVecIdx];

            // Find address to write
            size_t nextPage = nextPages[nextVecIdx];
            nextPages[nextVecIdx] += 1;
            size_t byteOffset = nextPage * PAGE_SIZE;
            if (byteOffset >= vec.numVirtualBytes) {
                break;
            }

            // Write single byte
            size_t elementIndex = byteOffset / sizeof(NumType);
            vec.ensureIndex(elementIndex);
            vec.data[elementIndex] = 42;

            // Increment
            nextVecIdx = (nextVecIdx + 1) % vecs.size();
            numPagesWritten += 1;
            commitLeft -= PAGE_SIZE;
        }
        end = Clock::now();
        auto elapsedWrite = duration_cast<nanoseconds>(end - start);
        auto elapsedPerPage = elapsedWrite / numPagesWritten;

        // Spew
        std::cout << "    N: " << numAllocs[subtest]
            << "  TotalTime: " << prettyTime(elapsedAlloc)
            << "  TotalReserved: " << prettyBytes(totalBytes)
            << "  TotalCommitted: " << prettyBytes(numPagesWritten * PAGE_SIZE)
            << "  PerVirtualAlloc: " << elapsedPerAlloc << " / " << prettyTime(elapsedPerAlloc)
            << "  PerVirtualMib: " << prettyTime(elapsedAlloc / (totalBytes / ONE_MB))
            << "  PerPageWrite: " << elapsedPerPage << " / " << prettyTime(elapsedPerPage)
            << "\n";
    }
}

// https://devblogs.microsoft.com/oldnewthing/20160318-00/?p=93181
// https://alwaysprocessing.blog/2022/02/20/size-matters
// over 100,000 virtual allocs and it sometimes hangs

// Linux: clang++ main.cpp -std=c++20 -stdlib=libc++ -lc++ -lc++abi -o virtual_alloc && ./virtual_alloc
