@echo off
REM build-all.cmd — full-workspace build with GUI-lock handling.
REM Equivalent to `just build-all` for anyone without just installed.

taskkill /F /IM xau-gui.exe >nul 2>&1
cargo build --release --workspace
exit /b %errorlevel%
