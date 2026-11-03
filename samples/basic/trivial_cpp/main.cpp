#include <stdio.h>

#include <mutex>
#include <stdexcept>
#include <thread>

// Regression checks for the toolchain, not just "it compiles":
//  - libc++ must be configured with threads (std::mutex, std::thread exist)
//  - C++ exceptions must actually be caught (Linux links need --eh-frame-hdr)
static void thrower() { throw std::runtime_error("caught"); }

int main() {
    printf("hello world\n");

    try {
        thrower();
    } catch (std::runtime_error const& e) {
        printf("exception: %s\n", e.what());
    }

    std::mutex m;
    int counter = 0;
    std::thread t([&] { std::lock_guard<std::mutex> lock(m); ++counter; });
    t.join();
    printf("thread ran: %d\n", counter);

    return counter == 1 ? 0 : 1;
}
