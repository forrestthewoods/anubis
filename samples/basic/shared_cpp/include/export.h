#pragma once

// Symbols a shared library exports. Windows needs explicit dllexport when
// building the DLL and dllimport when using it; ELF exports everything by
// default.
#if defined(_WIN32)
  #define SAMPLE_EXPORT_IMPL __declspec(dllexport)
  #define SAMPLE_IMPORT_IMPL __declspec(dllimport)
#else
  #define SAMPLE_EXPORT_IMPL __attribute__((visibility("default")))
  #define SAMPLE_IMPORT_IMPL
#endif
