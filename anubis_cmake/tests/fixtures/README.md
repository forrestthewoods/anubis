End-to-end fixtures for `anubis-cmake` (see `../e2e.rs`). Each package
builds one executable whose output depends on a detail where CMake and Anubis
differ unless the export gets it right. `expected.txt` holds what Anubis
builds print; the test checks the CMake build prints the same, and with
`ANUBIS_CMAKE_COMPARE_ANUBIS=1` checks Anubis's own build too.
