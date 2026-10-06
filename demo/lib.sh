# Sourced by record.sh: a headless Sway in a throwaway home and D-Bus session,
# and the helpers scenes use to drive it.

DEMO=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(dirname "$DEMO")
WORK=${DEMO_WORK:-${XDG_CACHE_HOME:-$HOME/.cache}/workspace-icon-daemon-demo}
RUN=/tmp/wid-demo-$(id -u) # Wayland and IPC sockets need a short path.
OUT=$WORK/out
DAEMON=$WORK/bin/wid-demo # Not named workspace-icon-daemon, so `pkill` aimed at a real one misses it.

# Sites visited before recording, so their favicons are in the font.
SITES=(https://en.wikipedia.org/wiki/Tiling_window_manager https://www.openstreetmap.org https://claude.ai)

mkdir -p "$WORK"/{home,bin} "$OUT"
mkdir -m 700 "$RUN" 2>/dev/null || true

export CARGO_HOME=${CARGO_HOME:-$HOME/.cargo} RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}
export HOME=$WORK/home XDG_RUNTIME_DIR=$RUN
export XDG_CONFIG_HOME=$HOME/.config XDG_DATA_HOME=$HOME/.local/share
export XDG_CACHE_HOME=$HOME/.cache XDG_STATE_HOME=$HOME/.local/state
export XDG_CURRENT_DESKTOP=sway XDG_SESSION_TYPE=wayland
unset WAYLAND_DISPLAY SWAYSOCK DISPLAY DBUS_SESSION_BUS_ADDRESS I3SOCK

log() { printf '\e[1m%s\e[0m\n' "$*" >&2; }

build_daemon() {
    cargo build --release --quiet --manifest-path "$REPO/Cargo.toml"
    rm -f "$DAEMON" && cp "$REPO/target/release/workspace-icon-daemon" "$DAEMON"
}

# Files and a copy of this project for the terminal to open.
prepare_home() {
    mkdir -p "$XDG_CONFIG_HOME"/{sway,waybar,alacritty} "$HOME/workspace-icon-daemon"
    cp "$DEMO/config/waybar.json" "$XDG_CONFIG_HOME/waybar/config"
    cp "$DEMO/config/waybar.css" "$XDG_CONFIG_HOME/waybar/style.css"
    cp "$DEMO/config/bashrc" "$HOME/.bashrc"
    sed "s|@BASHRC@|$HOME/.bashrc|" "$DEMO/config/alacritty.toml.in" >"$XDG_CONFIG_HOME/alacritty/alacritty.toml"
    cp -r "$REPO"/{src,Cargo.toml,README.md} "$HOME/workspace-icon-daemon/"
    [ -f "$WORK/bg.png" ] || ffmpeg -loglevel error -y -i /usr/share/backgrounds/gnome/amber-d.jxl \
        -vf "scale=3200:1800:force_original_aspect_ratio=increase,crop=3200:1800" "$WORK/bg.png"
}

# A Firefox profile that has visited SITES, ranked so the daemon bakes their
# favicons into the font.
seed_firefox() {
    local profile=$HOME/.mozilla/firefox/demo
    [ -f "$profile/places.sqlite" ] && return
    log "Visiting ${SITES[*]}"
    mkdir -p "$profile"
    cp "$DEMO/config/firefox-user.js" "$profile/user.js"
    printf '[Profile0]\nName=demo\nIsRelative=1\nPath=demo\nDefault=1\n\n[General]\nStartWithLastProfile=1\nVersion=2\n' \
        >"$HOME/.mozilla/firefox/profiles.ini"
    dbus-run-session -- bash -c 'firefox --headless --no-remote --profile "$1" "${@:2}" & sleep 25; kill $!; wait' \
        _ "$profile" "${SITES[@]}" >/dev/null 2>&1
    # Firefox ranks sites lazily; the daemon only takes ranked ones.
    local hosts
    hosts=$(printf "'%s'," "${SITES[@]}" | sed -E "s#https://([^/']*)[^']*#\1#g; s/,$//")
    sqlite3 "$profile/places.sqlite" "update moz_origins set frecency = 1000 where host in ($hosts)"
}

# session_start SCALE FONT TITLE_SIZE GAPS_INNER GAPS_OUTER
session_start() {
    session_stop
    sed -e "s|@SCALE@|$1|; s|@FONT@|$2|; s|@TITLE_SIZE@|$3|; s|@GAPS_INNER@|$4|; s|@GAPS_OUTER@|$5|" \
        -e "s|@BG@|$WORK/bg.png|; s|@DAEMON@|$DAEMON|; s|@LOG@|$OUT/daemon.log|; s|@READY@|$RUN/ready|" \
        "$DEMO/config/sway.in" >"$XDG_CONFIG_HOME/sway/config"
    rm -f "$RUN"/ready "$RUN"/sway-ipc.*
    (
        cd "$HOME" || exit
        export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1
        export ADW_DEBUG_COLOR_SCHEME=prefer-dark GTK_THEME=Adwaita:dark MOZ_ENABLE_WAYLAND=1
        setsid dbus-run-session -- sway >"$OUT/sway.log" 2>&1 </dev/null &
    )
    for _ in $(seq 100); do [ -f "$RUN/ready" ] && break; sleep 0.2; done
    local socket
    for socket in "$RUN"/sway-ipc.*.sock; do export SWAYSOCK=$socket; done
    for socket in "$RUN"/wayland-?; do export WAYLAND_DISPLAY=${socket##*/}; done
    sleep 3
}

session_stop() {
    [ -S "${SWAYSOCK:-}" ] && swaymsg exit >/dev/null 2>&1 && sleep 2
    unset SWAYSOCK WAYLAND_DISPLAY
}

# The compositor and bar load the font at login, and the daemon installs a new
# one when it finds something missing (the first start only builds it), so
# log in until a session needs nothing new.
build_font() {
    for _ in 1 2 3; do
        log "Logging in to check the icon font"
        session_start 2 18 12 14 6
        for _ in $(seq 300); do
            grep -q "Daemon is running" "$OUT/daemon.log" || ! pgrep -x wid-demo >/dev/null && break
            sleep 0.2
        done
        session_stop
        grep -q "Installed icon font" "$OUT/daemon.log" || return 0
    done
}

# Scene helpers.

# Criteria that match nothing (an empty desktop) are not an error here.
m() { swaymsg "$@" >/dev/null || true; }
wait_for() { for _ in $(seq 100); do swaymsg -t get_tree | grep -q "\"app_id\": \"$1\"" && return; sleep 0.2; done; }
# Launch an app off camera so a scene can bring it in already started.
stage() { local id=$1; shift; m "exec $*"; wait_for "$id"; sleep 1.5; m "[app_id=\"$id\"] move container to workspace stage"; }
bring() { m "[app_id=\"$1\"] move container to workspace number $2"; }
reset_desktop() {
    swaymsg -t get_outputs | grep -q HEADLESS-2 || m create_output
    m '[app_id=".*"] kill'; sleep 2
    m 'workspace number 9'; m 'workspace number 1'
}
# wtype drops the first key after it connects, so a harmless Shift goes first.
typ() { wtype -k Shift_L -s 150 -d 55 "$1" -s 250 -k Return -s 300; }
keys() { wtype -k Shift_L -s 150 "$@" -s 300; }
kb() { wtype -k Shift_L -s 100 -M logo -k "$1" -m logo -s 100; }

rec_start() {
    # -D: a frame per tick even when nothing moves, so holds keep their length.
    wf-recorder -y -D -o HEADLESS-1 -r 30 -c libx264 -p crf=10 -p preset=ultrafast \
        -f "$OUT/$1.mp4" >"$OUT/rec.log" 2>&1 &
    REC=$!
    sleep 1
}
rec_stop() { sleep 0.3; kill -INT "$REC"; while kill -0 "$REC" 2>/dev/null; do sleep 0.2; done; }
