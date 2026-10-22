@echo off
REM Android app build for xau-mobile, from cmd or a double-click.
REM
REM The build itself is build-apk.sh (Git Bash): the Rust core with the
REM NDK, then Gradle, on every core. It finds the JDK, Android SDK/NDK
REM and Gradle in their default per-user locations.
REM
REM   build-apk.cmd           release APK
REM   build-apk.cmd install   ...then install it on the phone over adb
"%ProgramFiles%\Git\bin\bash.exe" "%~dp0build-apk.sh" %*
exit /b %errorlevel%
