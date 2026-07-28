#!/usr/bin/env bash
# Capture the running app's screen to a PNG.
#
#   scripts/shoot.sh <name> [seconds-to-settle]
#
# Exists because getting a valid PNG off an emulator from a Windows shell
# has three separate traps, and rediscovering them each time is how a
# review loop stops happening:
#
#   1. `adb exec-out screencap -p > file` CORRUPTS the image in
#      PowerShell — the stream is treated as text, so it arrives with a
#      UTF-8 BOM and replacement characters instead of PNG magic. Write
#      device-side and pull instead.
#   2. Git Bash rewrites `/sdcard/...` into `C:/Program Files/Git/sdcard/...`
#      before adb ever sees it. MSYS_NO_PATHCONV=1 stops that.
#   3. ...but that same flag also stops the *destination* being converted,
#      and adb is a Windows binary that cannot follow a /c/... path. So
#      the destination must be given in Windows form.
#
# Screens settle slowly on a software-rendered emulator; the default wait
# is deliberately generous, because a screenshot taken mid-animation is
# worse than useless — it invites redesigning something that was only
# half-drawn.
set -euo pipefail

NAME="${1:?usage: shoot.sh <name> [settle-seconds]}"
SETTLE="${2:-6}"

ADB="${ADB:-/c/Android/sdk/platform-tools/adb.exe}"
OUT_UNIX="${SHOT_DIR_UNIX:-/c/xau-mobile/shots}"
OUT_WIN="${SHOT_DIR_WIN:-C:\\xau-mobile\\shots}"

mkdir -p "$OUT_UNIX"

if ! "$ADB" get-state >/dev/null 2>&1; then
  echo "no device — is the emulator running?" >&2
  exit 1
fi

sleep "$SETTLE"

(
  export MSYS_NO_PATHCONV=1
  "$ADB" shell screencap -p /sdcard/_shot.png
  "$ADB" pull /sdcard/_shot.png "${OUT_WIN}\\${NAME}.png" >/dev/null
  "$ADB" shell rm -f /sdcard/_shot.png
)

FILE="${OUT_UNIX}/${NAME}.png"
# Trust nothing: a corrupted capture looks like a successful one until
# something tries to decode it.
if [ "$(head -c 4 "$FILE" | xxd -p)" != "89504e47" ]; then
  echo "capture is not a PNG — it was mangled in transit" >&2
  exit 1
fi

echo "$FILE  ($(stat -c%s "$FILE") bytes)"
