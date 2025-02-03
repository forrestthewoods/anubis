@echo off
SETLOCAL
rem Clear all environment variables
for /f "tokens=1 delims==" %%i in ('set') do (
    rem echo %%i
    set %%i=
)
set

:: Set paths relative to the build script
set LLVM_ROOT=..\..\toolchains\windows\llvm
set MSVC_ROOT=..\..\toolchains\windows\msvc
set WINKIT_ROOT="..\..\toolchains\windows\msvc\Windows Kits"
set ZIG_ROOT=..\..\toolchains\windows\zig

REM Nuke old
rmdir /s /q bin 
rmdir /s /q build 

REM Recreate build directory
mkdir bin
mkdir build

:: Compile and link
%LLVM_ROOT%\bin\clang++ ^
    -v ^
    -fuse-ld=lld ^
    -target x86_64-linux-gnu ^
    -ffreestanding ^
    -fno-builtin ^
    -nostdinc ^
    -nostdinc++ ^
    -nostdlib ^
    -nostdlibinc ^
	-nodefaultlibs ^
	--std=c++20 ^
    -resource-dir=..\..\toolchains\empty_dir ^
    -isysroot=..\..\toolchains\empty_dir ^
    -isystem %ZIG_ROOT%\lib\include ^
    -isystem %ZIG_ROOT%\lib\libcxx\include ^
    -isystem %ZIG_ROOT%\lib\libc\include\x86_64-linux-gnu ^
    -isystem %ZIG_ROOT%\lib\libc\include\generic-glibc ^
    -D_LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_FAST ^
    -D_LIBCPP_HAS_NO_VENDOR_AVAILABILITY_ANNOTATIONS ^
    -D_LIBCPP_DISABLE_AVAILABILITY ^
    -D_GNU_SOURCE ^
    -D__linux__ ^
    -D__x86_64__ ^
    -D__GLIBC__ ^
    -lc ^
    -o bin/program_linux ^
    main.cpp
 
if %ERRORLEVEL% NEQ 0 (
    echo Build failed with error %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

echo Build successful!
