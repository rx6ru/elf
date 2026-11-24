//! Audio formats and capture.

pub mod capture;
pub mod encode;
pub mod wav;

pub const RATE: u32 = 16_000;
/// 20 ms of 16-bit mono audio.
pub const FRAME_BYTES: usize = 640;
pub const BYTES_PER_MS: usize = 32;

pub fn duration_ms(pcm_bytes: usize) -> u64 {
    (pcm_bytes / BYTES_PER_MS) as u64
}
