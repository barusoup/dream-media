#![allow(dead_code)]

mod common;

use common::{FixtureDir, sample_clip, tools};
use dream_media::{Player, PlayerOptions, Status};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Counting;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run with:
/// `cargo test -p dream-media --no-default-features --test alloc -- --ignored --nocapture`
#[test]
#[ignore]
fn steady_state_playback_does_not_allocate() {
    let Some(tools) = tools() else {
        return;
    };
    let dir = FixtureDir::new("alloc");
    let clip = sample_clip(&tools, &dir);
    let mut player = Player::open_with(
        &tools,
        &clip,
        PlayerOptions {
            max_size: (320, 240),
            audio: false,
            ..Default::default()
        },
    )
    .unwrap();

    // Warm the session: decoder startup, initial buffers and the first frames.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(600) {
        let _ = player.poll(Instant::now());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(player.status(), Status::Playing);

    ALLOCATIONS.store(0, Ordering::SeqCst);
    let window = Instant::now();
    while window.elapsed() < Duration::from_millis(800) {
        let now = Instant::now();
        let _ = player.poll(now);
        // Exercise the scheduling helper too; it is part of a frame loop.
        let _ = player.next_wake(now);
        std::thread::sleep(Duration::from_millis(2));
    }
    let allocations = ALLOCATIONS.load(Ordering::SeqCst);
    println!("steady-state allocations in 800 ms: {allocations}");
    assert_eq!(allocations, 0, "the frame loop allocated");
}
