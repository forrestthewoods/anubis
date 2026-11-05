#pragma once
#include <cstddef>
#include "export.h"

#if defined(GREET_BUILDING)
  #define GREET_API SAMPLE_EXPORT_IMPL
#else
  #define GREET_API SAMPLE_IMPORT_IMPL
#endif

// Plain C types across the boundary. With the static Windows CRT each DLL
// has its own heap, so memory (e.g. a std::string) allocated in one module
// must not be freed in another; the debug CRT asserts if it is.
// Writes "Hello, NAME!" into `out` (truncated to `out_size`) and returns
// the full length.
GREET_API size_t greet(const char* name, char* out, size_t out_size);
GREET_API int greet_counter();
