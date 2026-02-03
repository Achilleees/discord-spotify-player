call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
cd /d c:\Users\zahac\Desktop\CS\Code\apps\discord-spotify-player
echo Starting cargo check...
cargo check
echo Done with exit code %ERRORLEVEL%
