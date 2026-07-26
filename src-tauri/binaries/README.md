# Bundled binaries

Place a static **`ffmpeg.exe`** in this folder before building. It is bundled
into the installer as a resource and used for MP4 conversion and WebM metadata fixes.

Sources for a static Windows build:

- https://www.gyan.dev/ffmpeg/builds/ (release essentials is enough)
- or copy `node_modules/ffmpeg-static/ffmpeg.exe` after `npm i ffmpeg-static`

The binary is intentionally **not** committed to git (~80 MB).
At runtime the app falls back to the `FFMPEG_PATH` env var, then `ffmpeg` on `PATH`.
