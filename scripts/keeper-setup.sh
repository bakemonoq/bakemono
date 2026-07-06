#!/usr/bin/env bash
# Bakemono keeper setup for Linux. Installs kubo + ipfs-cluster-follow from scratch,
# points a follower at one board, and runs both under systemd. Safe to re-run.
#
#   curl -fsSL https://raw.githubusercontent.com/bakemonoq/bakemono/main/scripts/keeper-setup.sh | sudo bash -s -- https://board.example
set -euo pipefail

BOARD_URL="${1:-${BAKEMONO_BOARD_URL:-}}"
CLUSTER_NAME="${BAKEMONO_CLUSTER_NAME:-bakemono}"
IPFS_USER="ipfs"
IPFS_HOME="/var/lib/ipfs"
IPFS_PATH="${IPFS_HOME}/.ipfs"

main() {
  preflight
  ensure_deps
  install_kubo
  install_cluster_follow
  ensure_user
  init_ipfs
  init_follower
  write_units
  start_services
  done_message
}

say() { printf '\033[1;35m::\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

preflight() {
  [ "$(uname -s)" = "Linux" ] || die "this installer is Linux only"
  [ "$(id -u)" -eq 0 ] || die "run as root, e.g. pipe into 'sudo bash'"
  [ -d /run/systemd/system ] || die "systemd is required"
  BOARD_URL="${BOARD_URL%/}"
  case "$BOARD_URL" in
    http://*|https://*) ;;
    *) die "pass the board URL, e.g. sudo bash -s -- https://board.example" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    armv7l|armv6l) ARCH="arm" ;;
    i386|i686) ARCH="386" ;;
    *) die "unsupported architecture $(uname -m)" ;;
  esac
  say "board $BOARD_URL, arch $ARCH"
}

ensure_deps() {
  need_cmd curl
  need_cmd tar
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 && return
  say "installing $1"
  if command -v apt-get >/dev/null 2>&1; then apt-get update -qq && apt-get install -y -qq "$1"
  elif command -v dnf >/dev/null 2>&1; then dnf install -y -q "$1"
  elif command -v yum >/dev/null 2>&1; then yum install -y -q "$1"
  elif command -v pacman >/dev/null 2>&1; then pacman -Sy --noconfirm "$1"
  elif command -v apk >/dev/null 2>&1; then apk add --no-cache "$1"
  elif command -v zypper >/dev/null 2>&1; then zypper install -y "$1"
  else die "install $1 manually, no known package manager found"
  fi
}

install_kubo() {
  if command -v ipfs >/dev/null 2>&1; then
    say "kubo already installed ($(ipfs --version))"
    return
  fi
  install_dist kubo ipfs
}

install_cluster_follow() {
  if command -v ipfs-cluster-follow >/dev/null 2>&1; then
    say "ipfs-cluster-follow already installed"
    return
  fi
  install_dist ipfs-cluster-follow ipfs-cluster-follow
}

# fetch the latest stable release of a dist.ipfs.tech component and drop its binary into /usr/local/bin
install_dist() {
  local component="$1" binary="$2" version url tmp
  version="$(curl -fsSL "https://dist.ipfs.tech/${component}/versions" | grep -v -- '-rc' | tail -n1)"
  [ -n "$version" ] || die "could not resolve latest $component version"
  say "installing $component $version"
  tmp="$(mktemp -d)"
  url="https://dist.ipfs.tech/${component}/${version}/${component}_${version}_linux-${ARCH}.tar.gz"
  curl -fsSL "$url" -o "${tmp}/pkg.tar.gz"
  tar -xzf "${tmp}/pkg.tar.gz" -C "$tmp"
  install -m0755 "${tmp}/${component}/${binary}" "/usr/local/bin/${binary}"
  rm -rf "$tmp"
}

ensure_user() {
  if ! id "$IPFS_USER" >/dev/null 2>&1; then
    say "creating system user $IPFS_USER"
    useradd --system --create-home --home-dir "$IPFS_HOME" --shell /usr/sbin/nologin "$IPFS_USER"
  fi
  install -d -o "$IPFS_USER" -g "$IPFS_USER" "$IPFS_HOME"
}

init_ipfs() {
  if [ -f "${IPFS_PATH}/config" ]; then
    say "ipfs repo already initialised"
  else
    say "initialising ipfs repo"
    as_ipfs env IPFS_PATH="$IPFS_PATH" ipfs init
  fi
  # serve blocks to peers (recent kubo ships the bitswap server off) and answer wants without DHT luck
  as_ipfs env IPFS_PATH="$IPFS_PATH" ipfs config --json Bitswap.ServerEnabled true
  as_ipfs env IPFS_PATH="$IPFS_PATH" ipfs config --json Internal.Bitswap.BroadcastControl.Enable false
  as_ipfs env IPFS_PATH="$IPFS_PATH" ipfs config --json Reprovider.Strategy '"roots"'
}

init_follower() {
  if [ -f "${IPFS_HOME}/.ipfs-cluster-follow/${CLUSTER_NAME}/service.json" ]; then
    say "follower already initialised"
    return
  fi
  say "initialising follower against ${BOARD_URL}/follower.json"
  as_ipfs env HOME="$IPFS_HOME" ipfs-cluster-follow "$CLUSTER_NAME" init "${BOARD_URL}/follower.json"
}

as_ipfs() { sudo -u "$IPFS_USER" "$@"; }

write_units() {
  say "writing systemd units"
  cat >/etc/systemd/system/ipfs.service <<UNIT
[Unit]
Description=IPFS daemon (Bakemono keeper)
After=network-online.target
Wants=network-online.target

[Service]
User=${IPFS_USER}
Environment=IPFS_PATH=${IPFS_PATH}
ExecStart=/usr/local/bin/ipfs daemon --enable-gc --migrate
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
UNIT

  cat >/etc/systemd/system/ipfs-cluster-follow.service <<UNIT
[Unit]
Description=IPFS Cluster follower (Bakemono keeper)
After=ipfs.service
Wants=ipfs.service

[Service]
User=${IPFS_USER}
Environment=HOME=${IPFS_HOME}
ExecStart=/usr/local/bin/ipfs-cluster-follow ${CLUSTER_NAME} run
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
UNIT
}

start_services() {
  systemctl daemon-reload
  say "starting ipfs"
  systemctl enable --now ipfs.service
  wait_for_ipfs
  say "starting follower"
  systemctl enable --now ipfs-cluster-follow.service
}

wait_for_ipfs() {
  for _ in $(seq 1 30); do
    if as_ipfs env IPFS_PATH="$IPFS_PATH" ipfs id >/dev/null 2>&1; then return; fi
    sleep 1
  done
  say "ipfs api slow to come up; the follower will retry on its own"
}

done_message() {
  cat <<MSG

$(say "keeper is up")
  systemctl status ipfs ipfs-cluster-follow    # health
  ipfs-cluster-follow ${CLUSTER_NAME} list      # pinset the follower tracks

Open port 4001 (TCP and UDP) so other peers can fetch from you. Removed content
unpins automatically and is freed on the next GC
MSG
}

main "$@"
