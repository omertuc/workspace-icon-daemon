#!/bin/bash
# Records the README's clips into docs/assets/, in a headless Sway that
# leaves your own session alone.
#
#   demo/record.sh [hero|browser|terminal]...    all of them by default
#   RENDER_HOST=box demo/record.sh               render over ssh instead
#
# Needs sway, waybar, wf-recorder, wtype, ffmpeg, sqlite3, firefox, alacritty,
# nvim, nautilus, gnome-text-editor, gnome-system-monitor, loupe,
# gnome-calculator, the Inter font, and Python with Pillow wherever rendering
# runs (installed in a venv on RENDER_HOST).
set -euo pipefail
source "$(dirname "$0")/lib.sh"
trap session_stop EXIT

render() {
    local name=$1 out=$REPO/docs/assets/$1.webp
    if [ -z "${RENDER_HOST:-}" ]; then
        "$DEMO/render.sh" "$OUT/$name.mp4" "$out" "${RENDER[@]}"
        return
    fi
    # Not every ffmpeg decodes wf-recorder's H.264 profile; MJPEG they all do.
    ffmpeg -loglevel error -y -i "$OUT/$name.mp4" -c:v mjpeg -q:v 2 -pix_fmt yuvj444p "$OUT/$name.avi"
    local dir=.cache/workspace-icon-daemon-render
    ssh "$RENDER_HOST" "mkdir -p $dir"
    scp -q "$DEMO/render.sh" "$DEMO/encode_webp.py" "$OUT/$name.avi" "$RENDER_HOST:$dir/"
    ssh "$RENDER_HOST" "cd $dir && { [ -x venv/bin/python ] || { python3 -m venv venv && venv/bin/pip -q --disable-pip-version-check install pillow; }; } &&
        PYTHON=venv/bin/python ./render.sh $name.avi $name.webp $(printf '%q ' "${RENDER[@]}") && rm $name.avi"
    scp -q "$RENDER_HOST:$dir/$name.webp" "$out"
    rm -f "$OUT/$name.avi"
}

scenes=("$@")
[ ${#scenes[@]} -gt 0 ] || scenes=(hero browser terminal)

build_daemon
prepare_home
seed_firefox
build_font

for name in "${scenes[@]}"; do
    # shellcheck source=/dev/null
    source "$DEMO/scenes/$name.sh"
    log "Recording $name"
    session_start "${PROFILE[@]}"
    scene
    session_stop
    log "Rendering $name"
    render "$name"
done
