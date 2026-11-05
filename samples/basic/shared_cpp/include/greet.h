#pragma once
#include <string>
#include "export.h"

#if defined(GREET_BUILDING)
  #define GREET_API SAMPLE_EXPORT_IMPL
#else
  #define GREET_API SAMPLE_IMPORT_IMPL
#endif

GREET_API std::string greet(const std::string& name);
GREET_API int greet_counter();
