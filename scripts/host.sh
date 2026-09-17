#!/bin/sh
# Install packages as root, re-exec as `pf` to run daemons.
# Started in background so install doesn't block the client.
set -eu

REPO="https://git.unom.io/api/packages/unom/debian"
KEY=/etc/apt/keyrings/punktfunk.asc

CFG="$HOME/.config/punktfunk"
PAIRED="$CFG/punktfunk1-paired.json"
MGMT_PORT=47990
CONSOLE_PORT=47992

pkg() { apt-get -y -qq --no-install-recommends "$@" >/dev/null; }

install_as_root() {
  export DEBIAN_FRONTEND=noninteractive
  command -v curl >/dev/null || { pkg update && pkg install ca-certificates curl; }

  # Host packages are amd64-only; on the arm64 build image they run through the container
  # runtime's qemu binfmt handler. The list is arch-pinned so apt doesn't ask for arm64.
  dpkg --add-architecture amd64
  curl -fsSL "$REPO/repository.key" -o "$KEY"
  echo "deb [arch=amd64 signed-by=$KEY] $REPO stable main" >/etc/apt/sources.list.d/punktfunk.list
  pkg update  # not scoped to that list: apt would prune the Debian indexes as orphaned
  pkg install punktfunk-host punktfunk-web

  # Host won't run as root.
  id -u pf >/dev/null 2>&1 || useradd -m pf
  chown -R pf:pf /home/pf  # config volume mounts root-owned
}

# serve snapshots $PAIRED at startup, so clients pairing later are unseen.
# Restart on pairing changes to pick up new pairings.
serve() {
  fingerprint() { cksum "$PAIRED" 2>/dev/null || true; }
  while :; do
    punktfunk-host serve --native-port 9778 --no-mdns &
    pid=$! seen=$(fingerprint)
    while [ "$(fingerprint)" = "$seen" ]; do sleep 2; done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true  # let it release the port before the restart
  done
}

console() {
  export PUNKTFUNK_MGMT_URL="https://127.0.0.1:$MGMT_PORT" HOST=0.0.0.0 PORT="$CONSOLE_PORT" \
    PUNKTFUNK_UI_SECURE=1 PUNKTFUNK_UI_TLS_CERT="$CFG/cert.pem" PUNKTFUNK_UI_TLS_KEY="$CFG/key.pem"
  punktfunk-web-server
}

if [ "$(id -u)" = 0 ]; then
  SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"  # `su -` resets cwd, so absolutize
  install_as_root
  exec su - pf -c "$SELF"
fi

export PUNKTFUNK_MGMT_TOKEN=0000000000000000 PUNKTFUNK_UI_PASSWORD=0000
export PUNKTFUNK_ENCODER=software  # openh264; `auto` never picks it on Linux, and there's no GPU

serve &
until [ -s "$CFG/cert.pem" ]; do sleep 0.1; done  # serve writes the console's TLS pair
console &

echo "console: https://localhost:$CONSOLE_PORT  password: $PUNKTFUNK_UI_PASSWORD"

# serve can't stream in container (no DRM/compositor); use synthetic frames for stream.
# --allow-tofu doesn't persist pairing so serve's mTLS library API 401s; --allow-pairing needed.
exec punktfunk-host punktfunk1-host \
  --source synthetic \
  --allow-pairing \
  --pairing-pin 0000 \
  --data-port 47999
