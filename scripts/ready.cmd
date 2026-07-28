@echo off
REM ready.cmd — full pre-commit check. Runs clippy-fix, lint, tests.
REM If everything passes, safe to commit.

echo === Applying clippy auto-fixes ===
cargo clippy --release --workspace --all-targets --fix --allow-dirty
if errorlevel 1 goto :fail

echo === Running clippy (strict) ===
cargo clippy --release --workspace --all-targets
if errorlevel 1 goto :fail

echo === Running library tests ===
cargo test --release --workspace --lib
if errorlevel 1 goto :fail

echo === Running integration tests ===
cargo test --release --test bulk_scan_real_client
if errorlevel 1 goto :fail

echo.
echo ALL CHECKS PASSED — safe to commit.
exit /b 0

:fail
echo.
echo CHECKS FAILED — fix before committing.
exit /b 1
