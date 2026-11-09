#pragma once
#if defined(_WIN32)
  #if defined(SHLIB_BUILDING)
    #define SHLIB_API __declspec(dllexport)
  #else
    #define SHLIB_API __declspec(dllimport)
  #endif
#else
  #define SHLIB_API __attribute__((visibility("default")))
#endif
extern "C" SHLIB_API int shlib_bump(void);
