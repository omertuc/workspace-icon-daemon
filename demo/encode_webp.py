"""Read raw RGB frames on stdin and write a looping animated WebP.

libwebp's animation encoder, through Pillow; ffmpeg's libwebp_anim leaves
blocky corruption in frames that only partly change.
"""

import sys

from PIL import Image

width, height, fps, out = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3]), sys.argv[4]
size = width * height * 3
frames = []
while len(chunk := sys.stdin.buffer.read(size)) == size:
    frames.append(Image.frombytes("RGB", (width, height), chunk))
frames[0].save(
    out,
    save_all=True,
    append_images=frames[1:],
    duration=round(1000 / fps),
    loop=0,
    quality=80,
    method=5,
)
print(f"{out}: {len(frames)} frames", file=sys.stderr)
