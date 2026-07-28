@echo off
REM Android app build for xau-mobile.
REM
REM Mirrors build-android.cmd's role for the Rust side: neither Gradle
REM nor a usable JDK is on PATH by default here.
REM
REM JAVA_HOME on this machine points at a JRE (no javac), which AGP
REM cannot use -- it fails with a confusing "Unsupported class file
REM major version" rather than saying the compiler is missing. Point it
REM at the JDK explicitly.
set "JAVA_HOME=C:\Program Files\Eclipse Adoptium\jdk-21.0.11.10-hotspot"
set "ANDROID_HOME=C:\Android\sdk"
set "ANDROID_SDK_ROOT=C:\Android\sdk"
set "GRADLE=C:\Gradle\gradle-8.9\bin\gradle.bat"

cd /d "C:\xau-mobile\android"
call "%GRADLE%" %*
