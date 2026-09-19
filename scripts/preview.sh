#!/bin/sh
# VNC-served X display, telemetry listener, and punktfunk host within docker:deploy.
set -eu

cd "$(dirname "$0")/.."

# Host runs in parallel; package install takes minutes.
# Prefix lets host and client log to shared terminal without collision.
{ ./scripts/host.sh || echo "exited $?"; } 2>&1 | sed 's/^/[host] /' >&2 &

export DISPLAY=:0
Xvfb :0 -screen 0 1920x1080x24 -nolisten tcp &
until [ -e /tmp/.X11-unix/X0 ]; do sleep 0.1; done
x11vnc -forever -shared -nopw -quiet -rfbport 5900 \
  -threads -nonap -wait 10 -defer 5 -noxdamage &
websockify --web /usr/share/novnc 6080 localhost:5900 >/dev/null 2>&1 &
echo "preview: http://localhost:6080/vnc.html" >&2

nc -lk 9000 &
export SDL_AUDIO_DRIVER=dummy
# `vendored-sdl3`: this builds for the HOST target, which has no libSDL3 to link against
# (Debian bookworm ships none) and no webOS prefix to point at — see Cargo.toml.
exec cargo run --bin punktfunk-webos --release --features vendored-sdl3 -- \
  "{\"telemetry\":\"127.0.0.1:9000\",\"telemetry_level\":\"${TELEMETRY_LEVEL}\",\"webos_sdk\":\"${WEBOS_SDK}\"}"
