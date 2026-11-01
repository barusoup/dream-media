//! Audio output: a CPAL device stream fed from a preallocated ring.
//!
//! The decoder thread writes interleaved stereo f32 into the ring; the device
//! callback drains it. A single mutex guards the ring, the played-frame count
//! and the generation counter, so a seek's reset is atomic with respect to the
//! callback. No audio-path operation allocates after construction.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Decoded audio that may wait in the ring: 0.75 seconds. Small enough that a
/// seek discards little, large enough to ride out a decoder tick.
const RING_NUMER: usize = 3;
const RING_DENOM: usize = 4;

/// The device output. Stays on the thread that opened it; only [`AudioShared`]
/// crosses threads.
pub(crate) struct AudioOutput {
    _stream: Stream,
    shared: Arc<AudioShared>,
    rate: u32,
    channels: u16,
}

impl AudioOutput {
    /// Open the default output device at its preferred configuration.
    /// `None` means there is no usable device; video still plays silently on
    /// the wall clock.
    pub(crate) fn open() -> Option<Self> {
        let device = cpal::default_host().default_output_device()?;
        let supported = device.default_output_config().ok()?;
        let format = supported.sample_format();
        let config = supported.config();
        let rate = config.sample_rate.0;
        let channels = config.channels;
        let device_channels = usize::from(channels.max(1));
        let shared = Arc::new(AudioShared::new(rate, channels));
        let on_error = |error| eprintln!("dream-media audio output: {error}");
        let stream = match format {
            SampleFormat::F32 => {
                let shared = Arc::clone(&shared);
                // Reused by the callback; cpal buffers are normally a few
                // thousand samples, so this never grows in steady state.
                let mut scratch = vec![0.0f32; 16 * 1024];
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if channels == 2 {
                                shared.fill(data);
                            } else {
                                let frames = data.len() / device_channels;
                                let needed = frames * 2;
                                if scratch.len() < needed {
                                    scratch.resize(needed, 0.0);
                                }
                                let scratch = &mut scratch[..needed];
                                shared.fill(scratch);
                                for (frame, chunk) in data.chunks_mut(device_channels).enumerate() {
                                    for (channel, sample) in chunk.iter_mut().enumerate() {
                                        *sample = match channel {
                                            0 => scratch[frame * 2],
                                            1 => scratch[frame * 2 + 1],
                                            _ => scratch[frame * 2],
                                        };
                                    }
                                }
                            }
                        },
                        on_error,
                        None,
                    )
                    .ok()?
            }
            SampleFormat::I16 => {
                let shared = Arc::clone(&shared);
                let mut scratch = vec![0.0f32; 16 * 1024];
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                            let frames = data.len() / device_channels;
                            let needed = frames * 2;
                            if scratch.len() < needed {
                                scratch.resize(needed, 0.0);
                            }
                            let scratch = &mut scratch[..needed];
                            shared.fill(scratch);
                            for (frame, chunk) in data.chunks_mut(device_channels).enumerate() {
                                for (channel, sample) in chunk.iter_mut().enumerate() {
                                    let value = match channel {
                                        0 => scratch[frame * 2],
                                        1 => scratch[frame * 2 + 1],
                                        _ => scratch[frame * 2],
                                    };
                                    *sample = (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                                }
                            }
                        },
                        on_error,
                        None,
                    )
                    .ok()?
            }
            SampleFormat::U16 => {
                let shared = Arc::clone(&shared);
                let mut scratch = vec![0.0f32; 16 * 1024];
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                            let frames = data.len() / device_channels;
                            let needed = frames * 2;
                            if scratch.len() < needed {
                                scratch.resize(needed, 0.0);
                            }
                            let scratch = &mut scratch[..needed];
                            shared.fill(scratch);
                            for (frame, chunk) in data.chunks_mut(device_channels).enumerate() {
                                for (channel, sample) in chunk.iter_mut().enumerate() {
                                    let value = match channel {
                                        0 => scratch[frame * 2],
                                        1 => scratch[frame * 2 + 1],
                                        _ => scratch[frame * 2],
                                    };
                                    let centred = (value.clamp(-1.0, 1.0) + 1.0) * 0.5;
                                    *sample = (centred * f32::from(u16::MAX)) as u16;
                                }
                            }
                        },
                        on_error,
                        None,
                    )
                    .ok()?
            }
            _ => return None,
        };
        stream.play().ok()?;
        Some(Self {
            _stream: stream,
            shared,
            rate,
            channels,
        })
    }

    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    pub(crate) fn channels(&self) -> u16 {
        self.channels
    }

    pub(crate) fn shared(&self) -> Arc<AudioShared> {
        Arc::clone(&self.shared)
    }
}

/// State shared by the feeder thread, the UI thread and the device callback.
pub(crate) struct AudioShared {
    ring: Mutex<Ring>,
    /// Wakes the feeder when the callback drains room into the ring. Without
    /// it, the feeder would have to poll, and a poll loop is a thread waking
    /// hundreds of times a second for the whole of playback.
    space: Condvar,
    channels: u16,
    capacity: usize,
    running: AtomicBool,
    /// `f32` bits, so volume changes never take the ring lock.
    volume: AtomicU32,
    muted: AtomicBool,
}

struct Ring {
    samples: Vec<f32>,
    read: usize,
    write: usize,
    len: usize,
    played_frames: u64,
    generation: u64,
    finished: bool,
}

impl AudioShared {
    pub(crate) fn new(rate: u32, channels: u16) -> Self {
        let channels = channels.max(1);
        let capacity = (rate.max(1) as usize * usize::from(channels) * RING_NUMER / RING_DENOM)
            .max(usize::from(channels) * 2);
        // Keep the ring a whole number of frames so `room` never splits one.
        let capacity = capacity / usize::from(channels) * usize::from(channels);
        Self {
            ring: Mutex::new(Ring {
                samples: vec![0.0; capacity.max(usize::from(channels))],
                read: 0,
                write: 0,
                len: 0,
                played_frames: 0,
                generation: 0,
                finished: false,
            }),
            space: Condvar::new(),
            channels,
            capacity: capacity.max(usize::from(channels)),
            running: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::Release);
    }

    pub(crate) fn set_volume(&self, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.volume.store(volume.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Clear the ring, zero the played count and start a new generation.
    /// Everything happens under one lock, so the callback either counted a
    /// frame before the reset or none after it. The samples are not zeroed:
    /// an empty ring is never read past `len`, and a seek should not pay to
    /// clear a buffer it is about to overwrite.
    pub(crate) fn reset(&self) -> u64 {
        let mut ring = self.lock();
        ring.read = 0;
        ring.write = 0;
        ring.len = 0;
        ring.played_frames = 0;
        ring.finished = false;
        ring.generation += 1;
        ring.generation
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Append samples if they fit and the generation is still current; false
    /// means a seek superseded this feeder. The feeder itself uses
    /// [`Self::push_wait`], which blocks instead of dropping samples.
    #[cfg(test)]
    pub(crate) fn push(&self, generation: u64, samples: &[f32]) -> bool {
        let mut ring = self.lock();
        if ring.generation != generation || samples.len() > self.capacity - ring.len {
            return false;
        }
        ring.push(samples);
        true
    }

    /// Free space in samples (not frames).
    #[cfg(test)]
    pub(crate) fn room(&self) -> usize {
        self.capacity - self.lock().len
    }

    /// Push all of `samples`, sleeping on the ring's condvar when it is full
    /// rather than polling. Returns false once a seek makes this feeder stale.
    /// Only whole frames are pushed, so the callback's frame count is exact.
    pub(crate) fn push_wait(&self, generation: u64, mut samples: &[f32]) -> bool {
        let channels = usize::from(self.channels);
        let mut ring = self.lock();
        while !samples.is_empty() {
            if ring.generation != generation {
                return false;
            }
            let room = (self.capacity - ring.len) / channels * channels;
            if room == 0 {
                // The timeout only guards against a lost wake-up; the
                // callback notifies after every drain.
                ring = self
                    .space
                    .wait_timeout(ring, Duration::from_millis(20))
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
                continue;
            }
            let take = room.min(samples.len());
            ring.push(&samples[..take]);
            samples = &samples[take..];
        }
        true
    }

    pub(crate) fn buffered_frames(&self) -> usize {
        self.lock().len / usize::from(self.channels)
    }

    pub(crate) fn played_frames(&self) -> u64 {
        self.lock().played_frames
    }

    pub(crate) fn set_finished(&self, generation: u64) {
        let mut ring = self.lock();
        if ring.generation == generation {
            ring.finished = true;
        }
    }

    pub(crate) fn finished(&self) -> bool {
        self.lock().finished
    }

    /// The device callback's body. Kept here so it can be tested without a
    /// device. Shortfalls are zeroed and only real frames are counted.
    pub(crate) fn fill(&self, out: &mut [f32]) {
        if !self.running.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }
        let gain = if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.volume.load(Ordering::Relaxed))
        };
        let mut ring = self.lock();
        let channels = usize::from(self.channels);
        ring.read_into(out, gain, channels);
        drop(ring);
        self.space.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, Ring> {
        self.ring
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Ring {
    fn push(&mut self, samples: &[f32]) {
        let capacity = self.samples.len();
        let first = samples.len().min(capacity - self.write);
        self.samples[self.write..self.write + first].copy_from_slice(&samples[..first]);
        let rest = samples.len() - first;
        if rest > 0 {
            self.samples[..rest].copy_from_slice(&samples[first..]);
        }
        self.write = (self.write + samples.len()) % capacity;
        self.len += samples.len();
    }

    fn read_into(&mut self, out: &mut [f32], gain: f32, channels: usize) {
        let capacity = self.samples.len();
        let take = out.len().min(self.len);
        let first = take.min(capacity - self.read);
        for (slot, sample) in out[..first]
            .iter_mut()
            .zip(&self.samples[self.read..self.read + first])
        {
            *slot = *sample * gain;
        }
        let rest = take - first;
        if rest > 0 {
            for (slot, sample) in out[first..take].iter_mut().zip(&self.samples[..rest]) {
                *slot = *sample * gain;
            }
        }
        out[take..].fill(0.0);
        self.read = (self.read + take) % capacity;
        self.len -= take;
        self.played_frames += (take / channels.max(1)) as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1000 Hz stereo: 0.75 s = 750 frames = 1500 samples.
    fn shared() -> AudioShared {
        AudioShared::new(1000, 2)
    }

    #[test]
    fn capacity_is_three_quarters_of_a_second() {
        let shared = shared();
        assert_eq!(shared.room(), 1500);
        assert!(shared.push(shared.generation(), &[0.5; 1000]));
        assert_eq!(shared.room(), 500);
        assert_eq!(shared.buffered_frames(), 500);
    }

    #[test]
    fn reset_bumps_the_generation_and_clears_state() {
        let shared = shared();
        let generation = shared.generation();
        shared.push(generation, &[0.1; 200]);
        shared.set_running(true);
        let mut out = [0.0; 200];
        shared.fill(&mut out);
        shared.set_finished(generation);
        let next = shared.reset();
        assert_eq!(next, generation + 1);
        assert_eq!(shared.buffered_frames(), 0);
        assert_eq!(shared.played_frames(), 0);
        assert!(!shared.finished());
    }

    #[test]
    fn a_stale_generation_cannot_push_or_finish() {
        let shared = shared();
        let old = shared.generation();
        let new = shared.reset();
        assert!(!shared.push(old, &[1.0; 4]));
        shared.set_finished(old);
        assert!(!shared.finished());
        assert!(shared.push(new, &[1.0; 4]));
        shared.set_finished(new);
        assert!(shared.finished());
    }

    #[test]
    fn stopped_output_is_silent_and_consumes_nothing() {
        let shared = shared();
        shared.push(shared.generation(), &[0.5; 8]);
        let mut out = [9.0; 8];
        shared.fill(&mut out);
        assert_eq!(out, [0.0; 8]);
        assert_eq!(shared.buffered_frames(), 4);
        assert_eq!(shared.played_frames(), 0);
    }

    #[test]
    fn running_output_consumes_and_counts_frames() {
        let shared = shared();
        shared.push(shared.generation(), &[0.25; 8]);
        shared.set_running(true);
        let mut out = [0.0; 4];
        shared.fill(&mut out);
        assert_eq!(out, [0.25; 4]);
        assert_eq!(shared.played_frames(), 2);
        assert_eq!(shared.buffered_frames(), 2);
    }

    #[test]
    fn a_shortfall_is_zeroed_and_only_real_frames_count() {
        let shared = shared();
        shared.push(shared.generation(), &[0.5; 4]);
        shared.set_running(true);
        let mut out = [7.0; 8];
        shared.fill(&mut out);
        assert_eq!(out, [0.5, 0.5, 0.5, 0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(shared.played_frames(), 2);
    }

    #[test]
    fn volume_and_mute_scale_without_stopping_the_clock() {
        let shared = shared();
        shared.set_running(true);
        shared.push(shared.generation(), &[1.0; 12]);
        shared.set_volume(0.5);
        let mut out = [0.0; 4];
        shared.fill(&mut out);
        assert_eq!(out, [0.5; 4]);
        shared.set_muted(true);
        let mut out = [0.0; 4];
        shared.fill(&mut out);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(shared.played_frames(), 4);
    }

    #[test]
    fn the_ring_wraps_across_reads_and_writes() {
        let shared = shared();
        shared.set_running(true);
        let generation = shared.generation();
        // Consume most of the ring, then push across the wrap point.
        assert!(shared.push(generation, &[1.0; 1400]));
        let mut out = [0.0; 1400];
        shared.fill(&mut out);
        assert_eq!(shared.room(), 1500);
        assert!(shared.push(generation, &[2.0; 1500]));
        let mut out = [0.0; 1500];
        shared.fill(&mut out);
        assert_eq!(out[0], 2.0);
        assert_eq!(out[1499], 2.0);
    }

    #[test]
    fn a_full_ring_wakes_the_feeder_when_the_callback_drains_it() {
        let shared = Arc::new(shared());
        let generation = shared.generation();
        assert!(shared.push_wait(generation, &[0.5; 1500]));
        let feeder = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.push_wait(generation, &[1.0; 500]))
        };
        // The feeder is parked on the condvar; consuming 500 samples makes
        // exactly the room its 500-sample push needs.
        std::thread::sleep(Duration::from_millis(20));
        shared.set_running(true);
        let mut out = [0.0; 500];
        shared.fill(&mut out);
        assert!(feeder.join().unwrap());
        assert_eq!(shared.buffered_frames(), 750);
    }

    #[test]
    fn a_blocked_feeder_gives_up_when_a_seek_resets_the_ring() {
        let shared = Arc::new(shared());
        let generation = shared.generation();
        assert!(shared.push_wait(generation, &[0.5; 1500]));
        let feeder = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.push_wait(generation, &[1.0; 1500]))
        };
        std::thread::sleep(Duration::from_millis(20));
        assert_ne!(shared.reset(), generation);
        assert!(!feeder.join().unwrap());
        assert_eq!(shared.buffered_frames(), 0);
    }

    #[test]
    fn volume_is_clamped_and_rejects_non_finite_values() {
        let shared = shared();
        shared.set_volume(3.0);
        assert_eq!(f32::from_bits(shared.volume.load(Ordering::Relaxed)), 1.0);
        shared.set_volume(-1.0);
        assert_eq!(f32::from_bits(shared.volume.load(Ordering::Relaxed)), 0.0);
        shared.set_volume(f32::NAN);
        assert_eq!(f32::from_bits(shared.volume.load(Ordering::Relaxed)), 0.0);
    }
}
