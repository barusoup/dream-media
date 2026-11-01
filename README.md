# Dream Media Handler

Dynamic media for [Dream](https://github.com/barusoup/dream) apps: still images,
video and audio, decoded by FFmpeg and delivered straight into Dream's custom
graphics pipeline.

This is the "I need to play a video / show a photo / play a track inside my
Dream app" crate. It is not an asset pipeline, a thumbnail service, a database
or a video editor. It opens a file, gives you pixels or a GPU texture, and gets
out of the way. Dream does not depend on this crate; the dependency only points
one way.

```rust
// A video that updates a BedFrame image every frame.
let mut video = VideoPlayer::open(&ctx, "intro.mp4", PlayerOptions::default())?;

// once per rendered frame:
if let Some(texture) = video.poll(Instant::now()) {
    ui.image(rect, &texture, Color::WHITE);
}
```

## What you get

| Need | Type | Cost |
| --- | --- | --- |
| A still on screen | `Image` → `Image::upload` | one decode, one upload |
| A video in a UI or a custom pass | `VideoPlayer` | decode thread + 2 GPU textures |
| Raw RGBA frames, own upload path | `Player` + `Frame` | decode thread + a frame pool |
| Audio only (music, voice) | `Player` with `audio: true` | decode thread + CPAL stream |
| Metadata before opening | `probe` → `MediaInfo` | one `ffprobe` |
| Animated GIF/WebP/APNG | `Player` | it is a video stream with no sound |

Formats are whatever the bundled FFmpeg reads: MP4/MOV/MKV/WebM/AVI/TS/FLV/OGG,
H.264/HEVC/AV1/VP9/MPEG-4, GIF/APNG animated images, MP3/AAC/FLAC/WAV/Opus/Vorbis,
and stills through FFmpeg's image demuxers — including AVIF and HEIC when the
FFmpeg build supports them.

## Deliberately not included

Subtitle tracks, playlists and concat demuxers, network/RTSP/HTTP sources,
frame-accurate editing, colour management, HDR tone mapping, and a second
decoder implementation. Media is a file on disk; if you need a URL or a
streaming protocol, download it first. Static images are a different trade: a
pre-decoded texture in an atlas will always beat opening a file, so use this
crate when the media is dynamic — chosen at runtime, played, seeked, streamed
from a user's folder.

## Requirements

- Rust edition 2024 (Rust 1.85 or newer).
- [Dream](https://github.com/barusoup/Dream) at a revision that includes
  `dream::texture::TextureUploader` — required by the default `gpu` feature.
  The dependency is fetched automatically; add Dream directly only if your code
  names its types.
- `ffmpeg` and `ffprobe` binaries. Discovered (once per process) from:
  1. `DREAM_FFMPEG` / `DREAM_FFPROBE` (explicit file paths),
  2. `DREAM_FFMPEG_DIR` (a directory holding both),
  3. `<exe dir>/ffmpeg/`, then `<exe dir>/`,
  4. absolute entries on `PATH`.

  `Tools::from_paths` skips discovery entirely for apps that ship their own
  build.
- Linux/macOS/Windows. Video and audio go through FFmpeg's C decoders; the GPU
  bridge needs a Vulkan 1.3 context.
- Build without `gpu` (`--no-default-features`) for a headless tool, a server
  or a test harness: `Image`, `Player`, `probe` and `Tools` do not need Dream
  or Vulkan in that configuration.

## Install

```toml
[dependencies]
dream-media = { git = "https://github.com/barusoup/dream-media.git", tag = "v0.1.0" }
```

The `gpu` feature is on by default and pulls Dream from its repository. If your
crate also names Dream types (`Arc<GpuContext>`, `GpuTexture`, …), add Dream
with the same source so Cargo resolves one copy:

```toml
dream = { git = "https://github.com/barusoup/Dream.git", branch = "main" }
```

CPU-only decoding, with no Dream dependency at all:

```toml
dream-media = { git = "https://github.com/barusoup/dream-media.git", tag = "v0.1.0", default-features = false }
```

## Usage

### Still image

```rust
use dream_media::Image;

// Longest side down to 1920; `0` means "decode at source size".
let image = Image::open("poster.jpg", 1920)?;
let texture = image.upload(&ctx);
ui.image(rect, &texture, Color::WHITE);
```

`Image::open` is synchronous and tuned for interactive loading. Decode it on a
worker thread if the file may be large; the result is a plain `Vec<u8>`, so it
moves across threads and uploads later.

### Video

```rust
use std::time::{Duration, Instant};
use dream_media::{PlayerOptions, VideoPlayer};

let mut video = VideoPlayer::open(&ctx, "clip.webm", PlayerOptions {
    max_size: (1280, 720),   // decode bound; never enlarges
    audio: true,
    looping: true,
    ..Default::default()
})?;

loop {
    let now = Instant::now();
    if let Some(texture) = video.poll(now) {
        ui.image(rect, &texture, Color::WHITE);
    }
    video.seek(4_000);              // coalesced; call it every pointer move
    if let Some(wake) = video.next_wake(now) {
        // optional: sleep until `wake` instead of spinning
    }
}
```

`poll` is non-blocking and cheap. Call it once per rendered frame: it advances
the clock, picks the newest frame that is due (older ones go back to the
decoder), and returns a texture only when a new frame was uploaded.

### Audio only

```rust
use dream_media::{Player, PlayerOptions, Status};

let mut player = Player::open("track.flac", PlayerOptions {
    max_size: (0, 0),
    audio: true,
    ..Default::default()
})?;
player.set_volume(0.8);

// keep polling; frames are `None`, but the clock, status and end-of-file
// handling all still run.
let _ = player.poll(Instant::now());
assert!(matches!(player.status(), Status::Playing | Status::Loading));
```

Audio-only files return an error from `Player::open` if the machine has no
output device — the caller decides whether that is fatal. A video with an
unplayable audio track still plays silently and reports the reason from
`Player::warning()`.

## How it works

**Decode.** One FFmpeg child per stream, `-f rawvideo` / `-f f32le` on a pipe,
scaled by `swscale` to the size you asked for instead of decoding at source
resolution and resizing on the CPU. A reader thread fills a pool of
preallocated RGBA buffers. The pool is fixed at open time (`48 MiB` divided by
frame size, clamped to 2–6 frames), and dropping a `Frame` returns its buffer,
so a slow UI applies backpressure instead of growing memory.

**Clock.** One timeline per player, anchored to an `Instant` and nudged by the
audio device's played-frame count while sound is running (audio is the master,
video follows). Every method takes `now`, so a poll is consistent and tests
never sleep to advance time.

**Seeking.** A seek restarts the decoder at a keyframe (`-ss` before `-i`) and
coalesces requests within 120 ms so scrubbing does not spawn a decoder per
pointer move. If a long-GOP file decodes nothing at the seek point, the player
retries with an exact seek automatically, once.

**Audio.** A preallocated ring (0.75 s) guarded by one mutex, filled by a
feeder thread and drained by the CPAL callback. No allocation, no polling: the
feeder sleeps on a condition variable that the callback signals after every
drain. Volume and mute are atomics read by the callback.

**GPU.** `VideoSurface` owns two textures. `poll` writes the new frame into the
one the previous frame was not sampling and submits the copy behind the frame
in flight, so a 60 Hz UI never waits on a transfer. Both the upload and the
texture contract come from `dream::texture::TextureUploader`.

**Still images.** PNG always decodes in process (faster than FFmpeg's pipe at
every size tested). JPEG decodes in process up to 3 megapixels, where FFmpeg's
SIMD decoder and scaler overtake it. Everything else uses the pipe, and the PAM
header supplies the final size so no second probe is needed. EXIF orientation is
applied in process; the FFmpeg path squares non-square pixels with `setsar=1`.
Both paths return top-row-first RGBA8 at display size, ready to upload.

## Measured

Apple Silicon, release build, macOS 15, FFmpeg 7.1, files generated by
`ffmpeg -f lavfi`. Method: `cargo test --release --no-default-features --test
perf -- --ignored --nocapture`.

| Workload | Result |
| --- | --- |
| 1080p H.264, 5 s, open | 41 ms |
| …to first frame | 59 ms |
| …frames delivered by `poll` | 330 fps (consumer-bound; the decoder alone sustains ~1000) |
| 720p with sound, open | 177 ms, of which ~90 ms is the first CoreAudio start |
| …to first frame | 0 ms — the picture decodes while the device opens |
| 1080p still (4K BMP → 1920) | 83 ms |
| 4K PNG → 1920 | 122 ms (in process) vs 173 ms (pipe) |
| 4K JPEG → 1920 | 61 ms (pipe) vs 77 ms (in process) |
| 720p JPEG → 1920 | 1.8 ms (in process) vs ~20 ms (pipe) |
| Steady-state allocations while playing | 0 in an 800 ms window |
| Frame buffer memory, 1080p | 5 × 8.3 MB = 41 MB |

`tests/alloc.rs` installs a counting allocator and asserts the frame loop
allocates nothing after warm-up. A unit test in `src/stream.rs` asserts the
buffer pool stays inside its byte budget at every common resolution.

## Tests

```sh
cargo test                                  # unit + integration
cargo test --no-default-features            # no Vulkan, no Dream dependency
cargo test --test perf  -- --ignored --nocapture
cargo test --test alloc -- --ignored --nocapture
```

Fixtures are generated with the system `ffmpeg` into a temp directory, so no
media files are committed. Tests skip cleanly when FFmpeg is absent. Coverage
includes rotation and non-square pixels, long-GOP seeks, pause/resume, looping,
play-after-end, decode-bound changes mid-playback, audio sync and end-of-file,
corrupt input, and prompt teardown of a playing player.

## Development

The `gpu` feature normally fetches Dream from GitHub. To build against a local
Dream checkout (the usual setup while changing both), copy the example config
and point it at your clone:

```sh
mkdir -p .cargo
cp .cargo/config.toml.example .cargo/config.toml
```

```toml
# .cargo/config.toml — gitignored
[patch."https://github.com/barusoup/Dream.git"]
dream = { path = "../Dream" }
```

Then `cargo test` uses the local engine. Without the patch, the same commands
resolve Dream from its repository.

## License

AGPL-3.0 with additional attribution terms, matching Dream. See
[`LICENSE`](LICENSE).

In short: use it in open-source projects freely; credit "Dream Media Handler",
"Dream Engine" and Hunter Lehr in your documentation, credits or about screen,
and in your README, with links to both repositories. Closed-source use requires
a separate commercial license from the copyright holder
(1hunterlehr@gmail.com).
