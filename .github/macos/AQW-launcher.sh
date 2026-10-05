#!/bin/sh
# Copied into AQW.app/Contents/MacOS/AQW at bundle time.
#
# A .app carries exactly one CFBundleExecutable, and double-clicking the bare
# Ruffle binary would open Ruffle's own "open a file" dialog instead of AQW.
# This script is that executable: it supplies the same arguments and environment
# the Windows launcher (aqw_launcher/src/main.rs) passes to AQW.exe.
#
# No --graphics flag: on macOS wgpu resolves its backend to Metal either way.
DIR="$(cd "$(dirname "$0")" && pwd)"
SWF=https://game.aq.com/game/gamefiles/Loader3.swf

ARTIX_RUFFLE_GAME=aqw \
ARTIX_RUFFLE_WINDOW_TITLE="Artix Entertainment - AdventureQuest Worlds V3.2" \
RUFFLE_AQW_SUPERSAMPLE=1.25 \
exec "$DIR/aqw-real" "$SWF" \
  --spoof-url "$SWF" \
  --base https://game.aq.com/game/gamefiles/ \
  --quality low \
  --frame-rate 24 \
  --scale show-all \
  --letterbox on \
  --upgrade-to-https \
  --player-version 32 \
  -m 60 \
  --no-gui \
  --tcp-connections allow
