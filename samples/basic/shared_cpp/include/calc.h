#pragma once
#include "export.h"

#if defined(CALC_BUILDING)
  #define CALC_API SAMPLE_EXPORT_IMPL
#else
  #define CALC_API SAMPLE_IMPORT_IMPL
#endif

extern "C" CALC_API int calc_add(int a, int b);
