//! Real audio in both directions.
//!
//! Two wire formats meet here, and they are not the same shape.
//!
//! **Out** (isochronous `0x02`) is ordinary interleaved PCM: 4 channels of
//! 24-bit little-endian, 12 bytes per frame, exactly as `protocol.rs` derived
//! it from the driver. Measured on hardware: slots 0 and 1 come out of one side
//! of the phones jack, slots 2 and 3 out of the other, so the device takes two
//! stereo pairs whose slots are grouped by side rather than by pair.
//!
//! **In** (bulk `0x86`) is not PCM at all. The device sends its ADC's raw I2S
//! line: one bit per two-byte unit (bit 0 of the even byte, the odd byte is
//! always zero), 64 units per audio frame - two 32-bit slots of 24 data bits
//! MSB-first followed by 8 zero bits. That is 5.6 MB/s for 44.1 kHz stereo,
//! which is why draining and discarding it looked like such an odd thing for
//! the driver to be doing: it is 21x the size of the audio it carries.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// Bytes per outgoing frame: 4 channels x 24-bit.
pub const OUT_FRAME: usize = 12;
/// Bytes per decoded incoming frame: 2 channels x 24-bit.
pub const IN_FRAME: usize = 6;
/// Units (2 bytes each) per incoming audio frame.
pub const IN_UNITS_PER_FRAME: usize = 64;
/// Data bits in an I2S slot; the remaining 8 of the 32 are always zero.
const SLOT_BITS: usize = 24;

/// Frames waiting to go out, already in wire format.
pub static PLAY: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());
/// Decoded frames waiting to be written out to whoever is recording.
pub static REC: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());

/// Feed the isochronous OUT pipe from [`PLAY`] instead of silence.
pub static PLAY_ON: AtomicBool = AtomicBool::new(false);
/// Decode bulk IN 0x86 into [`REC`].
pub static REC_ON: AtomicBool = AtomicBool::new(false);

pub static PLAY_FRAMES: AtomicU64 = AtomicU64::new(0);
/// Frames the device asked for that nothing had filled - audible as a gap.
pub static PLAY_UNDERRUNS: AtomicU64 = AtomicU64::new(0);
pub static REC_FRAMES: AtomicU64 = AtomicU64::new(0);
/// Frames dropped because nobody was draining the capture queue fast enough.
pub static REC_OVERRUNS: AtomicU64 = AtomicU64::new(0);

/// About 250 ms of slack in each direction. Enough to ride out a scheduling
/// hiccup, small enough that latency stays usable.
const PLAY_LIMIT: usize = 11_025 * OUT_FRAME;
const REC_LIMIT: usize = 11_025 * IN_FRAME;

/// Queue wire-format frames for playback, dropping the oldest if the writer is
/// running ahead of the device.
pub fn push_play(bytes: &[u8]) {
    if let Ok(mut q) = PLAY.lock() {
        q.extend(bytes.iter().copied());
        while q.len() > PLAY_LIMIT {
            q.pop_front();
        }
    }
}

/// How many bytes are queued for playback.
pub fn play_queued() -> usize {
    PLAY.lock().map(|q| q.len()).unwrap_or(0)
}

/// Fill `dst` with queued frames, zero-filling what is missing.
///
/// Called from the libusb completion callback, so it must never block on
/// anything but the queue's own lock.
pub fn take_play(dst: &mut [u8]) {
    let mut filled = 0;
    if let Ok(mut q) = PLAY.lock() {
        while filled < dst.len() {
            match q.pop_front() {
                Some(b) => {
                    dst[filled] = b;
                    filled += 1;
                }
                None => break,
            }
        }
    }
    if filled < dst.len() {
        dst[filled..].fill(0);
        // An empty queue with nothing playing is not an underrun, it is silence.
        // Only a gap in the middle of a stream is worth counting.
        if filled > 0 {
            PLAY_UNDERRUNS.fetch_add(((dst.len() - filled) / OUT_FRAME) as u64, Ordering::Relaxed);
        }
    }
    PLAY_FRAMES.fetch_add((filled / OUT_FRAME) as u64, Ordering::Relaxed);
}

/// Which of the two stereo pairs a stereo source is written to.
#[derive(Clone, Copy, PartialEq)]
pub enum Pairs {
    A,
    B,
    Both,
}

impl Pairs {
    pub fn from_env(v: Option<&str>) -> Self {
        match v {
            Some("a") => Pairs::A,
            Some("b") => Pairs::B,
            _ => Pairs::Both,
        }
    }
}

/// Write one stereo frame into the device's four output slots.
///
/// Slots 0/1 drive one side of the output and slots 2/3 the other, so a stereo
/// pair is (slot 0, slot 2) or (slot 1, slot 3).
pub fn encode_frame(left: i32, right: i32, pairs: Pairs, out: &mut [u8; OUT_FRAME]) {
    let put = |out: &mut [u8; OUT_FRAME], slot: usize, v: i32| {
        let le = v.to_le_bytes();
        out[slot * 3..slot * 3 + 3].copy_from_slice(&le[..3]);
    };
    out.fill(0);
    if pairs != Pairs::B {
        put(out, 0, right);
        put(out, 2, left);
    }
    if pairs != Pairs::A {
        put(out, 1, right);
        put(out, 3, left);
    }
}

/// Find the bit phase at which the I2S pad bits line up.
///
/// Positions 24..32 and 56..64 of every 64-unit frame are always zero, which is
/// enough to lock on to without a word clock. Returns the offset in units.
pub fn find_alignment(units: &[u8]) -> Option<usize> {
    let frames = (units.len() / IN_UNITS_PER_FRAME).min(64);
    if frames < 4 {
        return None;
    }
    'phase: for phase in 0..IN_UNITS_PER_FRAME {
        for f in 0..frames - 1 {
            let base = phase + f * IN_UNITS_PER_FRAME;
            for i in SLOT_BITS..32 {
                if units[base + i] != 0 || units[base + 32 + i] != 0 {
                    continue 'phase;
                }
            }
        }
        return Some(phase);
    }
    None
}

/// Decode one bulk IN 0x86 payload into interleaved 24-bit little-endian stereo.
///
/// `align` is remembered across payloads: the device keeps its framing, so the
/// phase only has to be found once.
pub fn decode_i2s(payload: &[u8], align: &mut Option<usize>, out: &mut Vec<u8>) {
    // One bit per two-byte unit.
    let units: Vec<u8> = payload.iter().step_by(2).map(|b| b & 1).collect();
    let phase = match *align {
        Some(p) => p,
        None => match find_alignment(&units) {
            Some(p) => {
                *align = Some(p);
                p
            }
            None => return,
        },
    };

    let word = |bits: &[u8]| -> i32 {
        let mut v: i32 = 0;
        for &b in &bits[..SLOT_BITS] {
            v = (v << 1) | b as i32;
        }
        (v << 8) >> 8 // sign-extend from 24 bits
    };

    let mut i = phase;
    while i + IN_UNITS_PER_FRAME <= units.len() {
        for slot in [0usize, 32] {
            let v = word(&units[i + slot..i + slot + SLOT_BITS]);
            out.extend_from_slice(&v.to_le_bytes()[..3]);
        }
        i += IN_UNITS_PER_FRAME;
    }
}

/// Hand decoded frames to whoever is recording.
pub fn push_rec(bytes: &[u8]) {
    if let Ok(mut q) = REC.lock() {
        q.extend(bytes.iter().copied());
        REC_FRAMES.fetch_add((bytes.len() / IN_FRAME) as u64, Ordering::Relaxed);
        while q.len() > REC_LIMIT {
            let n = IN_FRAME.min(q.len());
            q.drain(..n);
            REC_OVERRUNS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Take everything currently captured.
pub fn drain_rec() -> Vec<u8> {
    match REC.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the unit stream the device would send for one stereo frame.
    fn units_for(left: i32, right: i32) -> Vec<u8> {
        let mut u = Vec::new();
        for v in [left, right] {
            for i in (0..SLOT_BITS).rev() {
                u.push(((v >> i) & 1) as u8);
            }
            u.extend(std::iter::repeat_n(0, 8));
        }
        u
    }

    #[test]
    fn decodes_what_the_device_sends() {
        let mut payload = Vec::new();
        for _ in 0..8 {
            for u in units_for(0x123456, -0x2000) {
                payload.push(u);
                payload.push(0);
            }
        }
        let mut align = None;
        let mut out = Vec::new();
        decode_i2s(&payload, &mut align, &mut out);
        assert_eq!(align, Some(0));
        assert_eq!(&out[..3], &[0x56, 0x34, 0x12]);
        let r = i32::from_le_bytes([
            out[3],
            out[4],
            out[5],
            if out[5] & 0x80 != 0 { 0xFF } else { 0 },
        ]);
        assert_eq!(r, -0x2000);
    }

    #[test]
    fn underrun_is_silence_not_stale_audio() {
        PLAY.lock().unwrap().clear();
        let mut buf = [0xAAu8; OUT_FRAME * 2];
        push_play(&[1u8; OUT_FRAME]);
        take_play(&mut buf);
        assert_eq!(&buf[..OUT_FRAME], &[1u8; OUT_FRAME]);
        assert_eq!(&buf[OUT_FRAME..], &[0u8; OUT_FRAME]);
    }

    #[test]
    fn stereo_lands_in_the_slots_the_hardware_uses() {
        let mut f = [0u8; OUT_FRAME];
        encode_frame(0x010203, 0x040506, Pairs::A, &mut f);
        assert_eq!(&f[0..3], &[0x06, 0x05, 0x04]); // slot 0: right
        assert_eq!(&f[3..6], &[0, 0, 0]); // slot 1: pair B, unused
        assert_eq!(&f[6..9], &[0x03, 0x02, 0x01]); // slot 2: left
    }
}
