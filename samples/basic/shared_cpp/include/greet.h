#pragma once
#include <cstddef>
#include "export.h"

#if defined(GREET_BUILDING)
  #define GREET_API SAMPLE_EXPORT_IMPL
#else
  #define GREET_API SAMPLE_IMPORT_IMPL
#endif

// Writes "Hello, NAME!" into `out` (truncated to `out_size`) and returns
// the full length.
GREET_API size_t greet(const char* name, char* out, size_t out_size);
GREET_API int greet_counter();

// C++ types and exceptions across the boundary need every module to share
// one C++ runtime: runtime_linkage = "dynamic" (the default modes). With a
// static runtime each module has its own copy: on Windows its own heap, so a
// std::string freed by another module asserts in the debug CRT; on Linux its
// own typeinfo, so an exception can't be caught by type in another module.
#if defined(SAMPLE_SHARED_RUNTIME)
#include <string>
GREET_API std::string greet_string(const std::string& name);
// Throws std::invalid_argument for an empty name.
GREET_API void greet_check(const std::string& name);
#endif
