#!/bin/bash
# render.sh IN OUT START DURATION WIDTH HEIGHT ZOOM
#
# Cuts a 3200x1800 recording, frames it (ZOOM is an ffmpeg zoompan expression,
# anchored top left), fades it in and out, and writes a looping animated WebP.
set -euo pipefail
in=$1 out=$2 start=$3 duration=$4 width=$5 height=$6 zoom=$7
fade_out=$(python3 -c "print($duration - 0.6)")

ffmpeg -loglevel error -y -ss "$start" -t "$duration" -i "$in" -vf "
    fps=30,scale=3200:1800:flags=lanczos,
    zoompan=d=1:fps=30:s=${width}x${height}:z='$zoom':x=0:y=0,
    fade=t=in:st=0:d=0.5,fade=t=out:st=$fade_out:d=0.6,fps=24" \
    -f rawvideo -pix_fmt rgb24 - |
    "${PYTHON:-python3}" "$(dirname "$0")/encode_webp.py" "$width" "$height" 24 "$out"
