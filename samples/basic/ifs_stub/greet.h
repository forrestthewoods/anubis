#pragma once

#if !defined(_WIN32)
  #define GREET_API __attribute__((visibility("default")))
#elif defined(GREET_BUILDING)
  #define GREET_API __declspec(dllexport)
#else
  #define GREET_API __declspec(dllimport)
#endif

GREET_API int greet_answer(void);
