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
    -target x86_64-pc-windows ^
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
    -isystem %MSVC_ROOT%\VC\Tools\MSVC\14.42.34433\include ^
    -isystem %WINKIT_ROOT%\10\Include\10.0.26100.0\ucrt\ ^
    -isystem %WINKIT_ROOT%\10\Include\10.0.26100.0\um\ ^
    -isystem %WINKIT_ROOT%\10\Include\10.0.26100.0\shared\ ^
    -L%MSVC_ROOT%\VC\Tools\MSVC\14.42.34433\lib\x64 ^
	-L%WINKIT_ROOT%\10\Lib\10.0.26100.0\um\x64 ^
	-L%WINKIT_ROOT%\10\Lib\10.0.26100.0\ucrt\x64 ^
    -llibcmt.lib ^
    -o bin/program.exe ^
    main.cpp
 
if %ERRORLEVEL% NEQ 0 (
    echo Build failed with error %ERRORLEVEL%
    exit /b %ERRORLEVEL%
)

echo Build successful!
