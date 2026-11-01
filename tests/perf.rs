#![allow(dead_code)]

mod common;

use common::{FixtureDir, ffmpeg, path_str, sample_clip, tools};
use dream_media::{Image, Player, PlayerOptions, Status};
use std::time::{Duration, Instant};

/// Where the in-process PNG/JPEG decoder stops paying for itself.
///
/// Run with:
/// `cargo test -p dream-media --no-default-features --test perf -- --ignored --nocapture`
#[test]
#[ignore]
fn still_matrix() {
    let Some(tools) = tools() else {
        return;
    };
    let dir = FixtureDir::new("still-matrix");
    for (name, ext, filter) in [
        ("4k", "png", "rgb24"),
        ("4k", "jpg", "yuvj420p"),
        ("2k", "jpg", "yuvj420p"),
        ("1k", "png", "rgb24"),
        ("1k", "jpg", "yuvj420p"),
    ] {
        let size = match name {
            "4k" => "3840x2160",
            "2k" => "2560x1440",
            _ => "1280x720",
        };
        let file = dir.path(&format!("{name}.{ext}"));
        assert!(ffmpeg(
            &tools,
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size={size}"),
                "-frames:v",
                "1",
                "-pix_fmt",
                filter,
                path_str(&file),
            ],
        ));
        let iterations = 8;
        let mut in_process = Duration::ZERO;
        for _ in 0..iterations {
            let started = Instant::now();
            let _ = Image::open_with(&tools, &file, 1920).unwrap();
            in_process += started.elapsed();
        }
        println!(
            "{name}.{ext}: {:.1} ms (in process or pipe, whichever routes)",
            in_process.as_secs_f64() * 1000.0 / iterations as f64
        );
    }
}

/// Run with:
/// `cargo test -p dream-media --no-default-features --test perf -- --ignored --nocapture`
#[test]
#[ignore]
fn still_bench() {
    let Some(tools) = tools() else {
        return;
    };
    let dir = FixtureDir::new("still-bench");
    let source = dir.path("source.bmp");
    assert!(ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=3840x2160",
            "-frames:v",
            "1",
            path_str(&source),
        ],
    ));
    let poster = dir.path("poster.png");

    let iterations = 5;
    let mut baseline = Duration::ZERO;
    for _ in 0..iterations {
        let started = Instant::now();
        assert!(ffmpeg(
            &tools,
            &[
                "-i",
                path_str(&source),
                "-vf",
                "scale=1920:1080:flags=lanczos",
                "-frames:v",
                "1",
                "-c:v",
                "png",
                path_str(&poster),
            ],
        ));
        let image = Image::open(&poster, 0).unwrap();
        baseline += started.elapsed();
        assert_eq!(image.size(), (1920, 1080));
    }

    let mut raw = Duration::ZERO;
    for _ in 0..iterations {
        let started = Instant::now();
        let image = Image::open_with(&tools, &source, 1920).unwrap();
        raw += started.elapsed();
        assert_eq!(image.size(), (1920, 1080));
    }

    println!(
        "4K BMP -> 1920x1080: poster-to-PNG pipeline {:.1} ms/frame, raw RGBA pipe {:.1} ms/frame",
        baseline.as_secs_f64() * 1000.0 / iterations as f64,
        raw.as_secs_f64() * 1000.0 / iterations as f64,
    );
}

/// Run with:
/// `cargo test -p dream-media --no-default-features --test perf -- --ignored --nocapture`
#[test]
#[ignore]
fn video_bench() {
    let Some(tools) = tools() else {
        return;
    };
    let dir = FixtureDir::new("video-bench");
    let long = dir.path("long.mp4");
    let mut made = false;
    for codec in ["libx264", "mpeg4"] {
        if ffmpeg(
            &tools,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1920x1080:rate=30",
                "-t",
                "5",
                "-pix_fmt",
                "yuv420p",
                "-preset",
                "ultrafast",
                "-c:v",
                codec,
                path_str(&long),
            ],
        ) {
            made = true;
            break;
        }
    }
    if !made {
        let fallback = sample_clip(&tools, &dir);
        let _ = std::fs::copy(fallback, &long);
    }

    let started = Instant::now();
    let mut player = Player::open_with(
        &tools,
        &long,
        PlayerOptions {
            max_size: (1920, 1080),
            audio: false,
            ..Default::default()
        },
    )
    .unwrap();
    let open = started.elapsed();

    let first_started = Instant::now();
    let mut frames = 0usize;
    loop {
        if player.poll(Instant::now()).is_some() {
            frames += 1;
            break;
        }
        assert!(
            first_started.elapsed() < Duration::from_secs(20),
            "no first frame"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let first_frame = first_started.elapsed();

    // A far-future `now` makes every decoded frame immediately due, turning
    // the player into a decoder throughput probe without changing its code.
    let future = Instant::now() + Duration::from_secs(3600);
    let decode_started = Instant::now();
    loop {
        if player.poll(future).is_some() {
            frames += 1;
        }
        if player.status() == Status::Ended {
            break;
        }
        assert!(
            decode_started.elapsed() < Duration::from_secs(60),
            "decode did not finish, {} frames",
            frames
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let elapsed = decode_started.elapsed();
    // This is the rate `poll` can hand frames to a caller, not the decoder's
    // ceiling: every frame that falls behind is recycled rather than shown.
    println!(
        "1080p clip: open {:.1} ms, first frame {:.1} ms, {} frames through poll in {:.2} s ({:.1} fps)",
        open.as_secs_f64() * 1000.0,
        first_frame.as_secs_f64() * 1000.0,
        frames,
        elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64(),
    );

    // A file with sound: this pays the output-device setup, which the first
    // open in a process used to pay before the decoder was even spawned.
    let with_sound = dir.path("with-sound.mp4");
    let made_sound = ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "3",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            path_str(&with_sound),
        ],
    );
    if made_sound {
        let started = Instant::now();
        let mut player = Player::open_with(
            &tools,
            &with_sound,
            PlayerOptions {
                max_size: (1280, 720),
                audio: true,
                ..Default::default()
            },
        )
        .unwrap();
        let open = started.elapsed();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if player.poll(Instant::now()).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        println!(
            "720p with sound: open {:.1} ms, first frame {:.1} ms, warning {:?}",
            open.as_secs_f64() * 1000.0,
            started.elapsed().as_secs_f64() * 1000.0,
            player.warning(),
        );
    }
}
