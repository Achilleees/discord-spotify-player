@echo off
rem Runs cargo check with the MSVC build environment loaded.
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
cd /d "%~dp0"
echo Starting cargo check...
cargo check
echo Done with exit code %ERRORLEVEL%
