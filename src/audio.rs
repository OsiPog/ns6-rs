//! Real audio in both directions.
//!
//! Two wire formats meet here, and they are not the same shape.
//!
//! **Out** (isochronous `0x02`) is ordinary interleaved PCM: 4 channels of
//! 24-bit little-endian, 12 bytes per frame, exactly as `protocol.rs` derived
//! it from the driver. Measured on hardware, one slot at a time: slots 0 and 1
//! are the **master** output and slots 2 and 3 the **headphone** jack. Nothing
//! about that is on the wire to be read off it, and two earlier readings of it
//! were wrong - see [`encode_frame_out`].
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
/// Frames dropped because the host was running ahead of the device.
pub static PLAY_DROPS: AtomicU64 = AtomicU64::new(0);
/// Frames the device asked for that nothing had filled - audible as a gap.
pub static PLAY_UNDERRUNS: AtomicU64 = AtomicU64::new(0);
pub static REC_FRAMES: AtomicU64 = AtomicU64::new(0);
/// Frames dropped because nobody was draining the capture queue fast enough.
pub static REC_OVERRUNS: AtomicU64 = AtomicU64::new(0);
/// Frames a recorder asked for that the device had not sent - written out as
/// silence, because there is nothing else honest to put there.
///
/// Counted for the same reason [`PLAY_UNDERRUNS`] is: a gap that nothing
/// counts is a gap nothing can be shown, and the capture path's whole failure
/// mode is quiet.
pub static REC_UNDERRUNS: AtomicU64 = AtomicU64::new(0);

/// About 250 ms of slack in each direction. Enough to ride out a scheduling
/// hiccup, small enough that latency stays usable.
const PLAY_LIMIT: usize = 11_025 * OUT_FRAME;
const REC_LIMIT: usize = 11_025 * IN_FRAME;

/// Queue wire-format frames for playback, dropping the oldest if the writer is
/// running ahead of the device.
///
/// Dropping happens in **whole frames**. A single byte would shift every frame
/// boundary after it, so each 12-byte frame would then be assembled from parts
/// of two and every 24-bit sample would come out byte-rotated: harsh distortion
/// with the bass gone, lasting until some later drop happened to bring the
/// offset back to a multiple of the frame size. A frame-aligned drop is a click
/// instead, and only where the drop was.
pub fn push_play(bytes: &[u8]) {
    if let Ok(mut q) = PLAY.lock() {
        q.extend(bytes.iter().copied());
        while q.len() > PLAY_LIMIT {
            let n = OUT_FRAME.min(q.len());
            q.drain(..n);
            PLAY_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// How many bytes are queued for playback.
pub fn play_queued() -> usize {
    PLAY.lock().map(|q| q.len()).unwrap_or(0)
}

/// How many bytes of captured audio are waiting to be collected.
pub fn rec_queued() -> usize {
    REC.lock().map(|q| q.len()).unwrap_or(0)
}

/// Fill `dst` with queued frames, zero-filling what is missing.
///
/// Called from the libusb completion callback, so it must never block on
/// anything but the queue's own lock.
pub fn take_play(dst: &mut [u8]) {
    let mut filled = 0;
    if let Ok(mut q) = PLAY.lock() {
        // Whole frames only, for the same reason [`push_play`] drops whole
        // frames: a queue that runs short should leave a clean gap, not half a
        // frame for the next packet to build the other half of.
        let take = dst.len().min(q.len() - q.len() % OUT_FRAME);
        for b in q.drain(..take) {
            dst[filled] = b;
            filled += 1;
        }
    }
    if filled < dst.len() {
        dst[filled..].fill(0);
        // Every frame that had to be invented is counted, including a packet
        // that got nothing at all. Counting only partial fills hid the worst
        // case: a stream whose queue is empty for whole packets at a time
        // reported nothing while stepping the waveform to zero and back.
        if PLAY_ON.load(Ordering::Relaxed) {
            PLAY_UNDERRUNS.fetch_add(((dst.len() - filled) / OUT_FRAME) as u64, Ordering::Relaxed);
        }
    }
    PLAY_FRAMES.fetch_add((filled / OUT_FRAME) as u64, Ordering::Relaxed);
}

/// Which of the device's two outputs a stereo source is written to.
#[derive(Clone, Copy, PartialEq)]
pub enum Out {
    Master,
    Phones,
    Both,
}

impl Out {
    /// `a` and `b` are still taken, from when the two outputs were thought to
    /// be a pair of anonymous stereo pairs rather than master and headphones.
    pub fn from_env(v: Option<&str>) -> Self {
        match v {
            Some("a") | Some("master") => Out::Master,
            Some("b") | Some("phones") => Out::Phones,
            _ => Out::Both,
        }
    }
}

/// Write one frame of master and headphones into the device's four slots.
///
/// The four slots are two outputs, not two decks and not two sides of one
/// jack: **slots 0 and 1 are the master output, slots 2 and 3 the headphone
/// jack**, in plain interleaved order. Measured by putting a tone in one slot
/// at a time - `NS6_TONE_CH=0x1` through `0x8` - and listening at both.
///
/// This is the layout of a controller doing its mixing in software, which is
/// what the NS6 is once its panel is switched to PC: the faders and cue
/// buttons then only send MIDI and the internal mixer is out of the path, so
/// the host mixes and sends a master feed and a cue feed. The headphone
/// blend knob still works, and picks between this master feed and the cue one.
pub fn encode_frame_out(master: (i32, i32), phones: (i32, i32), out: &mut [u8; OUT_FRAME]) {
    let put = |out: &mut [u8; OUT_FRAME], slot: usize, v: i32| {
        let le = v.to_le_bytes();
        out[slot * 3..slot * 3 + 3].copy_from_slice(&le[..3]);
    };
    out.fill(0);
    put(out, 0, master.0);
    put(out, 1, master.1);
    put(out, 2, phones.0);
    put(out, 3, phones.1);
}

/// Write one stereo frame to the output or outputs named.
pub fn encode_frame(left: i32, right: i32, to: Out, out: &mut [u8; OUT_FRAME]) {
    let silence = (0, 0);
    let stereo = (left, right);
    match to {
        Out::Master => encode_frame_out(stereo, silence, out),
        Out::Phones => encode_frame_out(silence, stereo, out),
        Out::Both => encode_frame_out(stereo, stereo, out),
    }
}

/// Find the bit phase at which the I2S pad bits line up.
///
/// Positions 24..32 and 56..64 of every 64-unit frame are always zero, which is
/// enough to lock on to without a word clock. Returns the offset in units.
///
/// A window with no set bits in it at all is refused. Every phase fits it, so
/// it would lock on the first one tried and be wrong as often as not - and the
/// way to see an all-zero window is to be reading the odd bytes, which are
/// always zero. Waiting for a bit to appear costs the silence it was going to
/// decode to anyway; locking on nothing costs every sample after it.
pub fn find_alignment(units: &[u8]) -> Option<usize> {
    let frames = (units.len() / IN_UNITS_PER_FRAME).min(64);
    if frames < 4 {
        return None;
    }
    if !units[..frames * IN_UNITS_PER_FRAME].iter().any(|&b| b != 0) {
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

/// What the input decoder carries between payloads.
///
/// A bulk IN payload is not a whole number of audio frames unless it happens
/// to be, and the bitstream does not restart at each one: it is one continuous
/// I2S line cut into transfers wherever the transfers ended. So whatever is
/// left of a frame at the end of a payload is the front of a frame, and it has
/// to be kept.
#[derive(Default)]
pub struct Rec {
    /// Whether the bit phase has been found. Once it has, [`Rec::units`] is
    /// kept frame-aligned, so it never has to be found again.
    locked: bool,
    /// Units not yet decoded: what was left of the last payload, then the next.
    units: Vec<u8>,
    /// Index within the next payload of the first byte that carries a bit.
    ///
    /// The bits are in the even bytes *of the stream*, and which end of a byte
    /// pair a payload begins on depends on the lengths of all the payloads
    /// before it. Assuming every payload starts on an even one is right only
    /// while they all have even lengths: one odd-length transfer and every
    /// payload after it reads the odd bytes, which are always zero, so the
    /// input goes silent and stays silent.
    start: usize,
}

impl Rec {
    /// A decoder that has seen nothing. `const`, so the one the bulk callback
    /// uses can live in a `static` without a lazy initialiser.
    pub const fn new() -> Self {
        Self {
            locked: false,
            units: Vec::new(),
            start: 0,
        }
    }

    /// Forget everything, so the next payload re-locks.
    pub fn reset(&mut self) {
        self.locked = false;
        self.units.clear();
        self.start = 0;
    }
}

/// Units to hold while still hunting for the phase.
///
/// [`find_alignment`] looks at up to 64 frames, so more than that is no help
/// in locking on and an unbounded queue of it is a leak.
const MAX_HUNT: usize = 64 * IN_UNITS_PER_FRAME;

/// One 24-bit I2S slot, MSB first, sign-extended.
fn word(bits: &[u8]) -> i32 {
    let mut v: i32 = 0;
    for &b in &bits[..SLOT_BITS] {
        v = (v << 1) | b as i32;
    }
    (v << 8) >> 8 // sign-extend from 24 bits
}

/// Decode one bulk IN 0x86 payload into interleaved 24-bit little-endian stereo.
///
/// The phase is found once and then held by keeping `st` frame-aligned, rather
/// than by remembering an offset into each payload. Remembering the offset
/// works only for as long as every payload is a whole number of frames: the
/// leftover units at the end of one and the phase-many at the start of the next
/// are one frame between them, which was being dropped - and, worse, a payload
/// whose length was *not* a multiple of a frame moved the boundary for every
/// payload after it, with nothing to notice or re-lock.
pub fn decode_i2s(payload: &[u8], st: &mut Rec, out: &mut Vec<u8>) {
    // One bit per two-byte unit, in the even bytes of the *stream* - see
    // [`Rec::start`] for why that is not the same as the even bytes of this
    // payload.
    let mut i = st.start;
    while i < payload.len() {
        st.units.push(payload[i] & 1);
        i += 2;
    }
    // `i` is now the first index past the end, so it is one byte or two beyond
    // it, and the difference is where the next payload starts.
    st.start = i - payload.len();

    if !st.locked {
        match find_alignment(&st.units) {
            Some(p) => {
                st.units.drain(..p);
                st.locked = true;
            }
            None => {
                // Silence decodes to silence at every phase, so it can be
                // handed on without committing to one - and it has to be,
                // because an idle mixer sends bit-exact zeros for as long as
                // it is idle. Refusing to lock *and* emitting nothing took the
                // whole capture path dark until something was played: twelve
                // minutes of it decoded not one frame.
                let frames = st.units.len() / IN_UNITS_PER_FRAME;
                if frames > 0 && !st.units.iter().any(|&b| b != 0) {
                    out.resize(out.len() + frames * IN_FRAME, 0);
                    st.units.drain(..frames * IN_UNITS_PER_FRAME);
                    return;
                }
                // Not silence, and not yet enough to lock on to. Keep only as
                // much as locking on can use.
                if st.units.len() > MAX_HUNT {
                    let drop = st.units.len() - MAX_HUNT;
                    st.units.drain(..drop);
                }
                return;
            }
        }
    }

    let frames = st.units.len() / IN_UNITS_PER_FRAME;
    for f in 0..frames {
        let base = f * IN_UNITS_PER_FRAME;
        for slot in [0usize, 32] {
            let v = word(&st.units[base + slot..base + slot + SLOT_BITS]);
            out.extend_from_slice(&v.to_le_bytes()[..3]);
        }
    }
    st.units.drain(..frames * IN_UNITS_PER_FRAME);
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

/// Take at most `max` bytes of captured audio, rounded down to whole frames.
///
/// A PipeWire buffer asks for a fixed number of frames and has nowhere to put
/// the rest, so taking everything would mean throwing away the remainder.
pub fn take_rec(max: usize) -> Vec<u8> {
    match REC.lock() {
        Ok(mut q) => {
            let n = max.min(q.len());
            q.drain(..n - n % IN_FRAME).collect()
        }
        Err(_) => Vec::new(),
    }
}

/// Take everything currently captured.
pub fn drain_rec() -> Vec<u8> {
    take_rec(usize::MAX)
}

/// Drop whatever is queued for playback.
///
/// A stream that stopped and started again is a new stream; what was left over
/// from the last one is stale audio nobody asked to hear.
pub fn clear_play() {
    if let Ok(mut q) = PLAY.lock() {
        q.clear();
    }
}

/// Bytes per host-side frame: 32-bit little-endian stereo.
///
/// The device's own formats are awkward to hand to anything else - four 24-bit
/// output slots grouped by side, and an input that is a raw I2S bitstream - so
/// everything on the host side of this module speaks s32le stereo, which is
/// what the PipeWire nodes, `pw-cat` and `sox` all take without argument.
pub const HOST_FRAME: usize = 8;

/// The range a 24-bit slot can carry.
const SLOT_MIN: i32 = -8_388_608;
const SLOT_MAX: i32 = 8_388_607;

/// Convert host frames to wire frames, appending them to `out`.
///
/// Returns how many bytes of `src` were used, which is a whole number of
/// frames: a caller reading from a pipe gets handed arbitrary boundaries and
/// has to carry the remainder into the next call.
pub fn encode_host(src: &[u8], gain: f32, to: Out, out: &mut Vec<u8>) -> usize {
    let mut used = 0;
    let mut frame = [0u8; OUT_FRAME];
    while src.len() - used >= HOST_FRAME {
        let s = |o: usize| -> i32 {
            i32::from_le_bytes([
                src[used + o],
                src[used + o + 1],
                src[used + o + 2],
                src[used + o + 3],
            ])
        };
        // s32 -> the device's 24-bit slots.
        let l = ((s(0) >> 8) as f32 * gain) as i32;
        let r = ((s(4) >> 8) as f32 * gain) as i32;
        encode_frame(
            l.clamp(SLOT_MIN, SLOT_MAX),
            r.clamp(SLOT_MIN, SLOT_MAX),
            to,
            &mut frame,
        );
        out.extend_from_slice(&frame);
        used += HOST_FRAME;
    }
    used
}

/// Bytes per host-side frame carrying all four channels: master, then phones.
pub const QUAD_FRAME: usize = 16;

/// Convert four-channel host frames to wire frames, appending them to `out`.
///
/// Channels 1-2 are the master output and 3-4 the headphones, which is what
/// the device's four slots are. Returns the bytes of `src` used, a whole
/// number of frames, as [`encode_host`] does.
pub fn encode_host_quad(src: &[u8], gain: f32, out: &mut Vec<u8>) -> usize {
    let mut used = 0;
    let mut frame = [0u8; OUT_FRAME];
    while src.len() - used >= QUAD_FRAME {
        let s = |o: usize| -> i32 {
            let v = i32::from_le_bytes([
                src[used + o],
                src[used + o + 1],
                src[used + o + 2],
                src[used + o + 3],
            ]);
            (((v >> 8) as f32 * gain) as i32).clamp(SLOT_MIN, SLOT_MAX)
        };
        encode_frame_out((s(0), s(4)), (s(8), s(12)), &mut frame);
        out.extend_from_slice(&frame);
        used += QUAD_FRAME;
    }
    used
}

/// Widen decoded 24-bit frames to s32le, appending them to `out`.
pub fn to_host(pcm: &[u8], out: &mut Vec<u8>) {
    for s in pcm.chunks_exact(3) {
        out.extend_from_slice(&[0, s[0], s[1], s[2]]);
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
        let mut st = Rec::default();
        let mut out = Vec::new();
        decode_i2s(&payload, &mut st, &mut out);
        assert!(st.locked);
        assert_eq!(&out[..3], &[0x56, 0x34, 0x12]);
        let r = i32::from_le_bytes([
            out[3],
            out[4],
            out[5],
            if out[5] & 0x80 != 0 { 0xFF } else { 0 },
        ]);
        assert_eq!(r, -0x2000);
    }

    /// The queue overflows once a minute or so, because nothing here resamples
    /// and the host's clock is not the device's. What must never happen is that
    /// a drop shifts the frame boundary: everything after it would be built
    /// from parts of two frames, which is heard as distortion with no bass
    /// until a later drop happens to restore the phase.
    #[test]
    fn a_full_queue_drops_whole_frames() {
        PLAY.lock().unwrap().clear();
        // Fill past the limit by an amount that is not a whole frame.
        let over = PLAY_LIMIT + OUT_FRAME * 3 + 5;
        push_play(&vec![7u8; over - over % OUT_FRAME]);
        push_play(&[7u8; OUT_FRAME]);

        let left = PLAY.lock().unwrap().len();
        assert!(left <= PLAY_LIMIT, "queue is still over its limit");
        assert_eq!(left % OUT_FRAME, 0, "a drop broke the frame boundary");
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

    /// The bitstream is one continuous I2S line, and the transfers it arrives
    /// in are cut wherever they were cut. So decoding it in pieces has to give
    /// the same answer as decoding it whole, whatever the pieces are - and in
    /// particular the frame straddling a boundary has to come out, once.
    #[test]
    fn a_payload_boundary_does_not_cost_a_frame() {
        let mut payload = Vec::new();
        for i in 0..40i32 {
            for u in units_for(0x1000 + i, -i) {
                payload.push(u);
                payload.push(0);
            }
        }

        let mut whole = Vec::new();
        decode_i2s(&payload, &mut Rec::default(), &mut whole);
        assert_eq!(whole.len(), 40 * IN_FRAME, "40 frames in, 40 frames out");

        // Lengths deliberately not multiples of a frame, and one of them odd,
        // which is what a short transfer would look like.
        let mut st = Rec::default();
        let mut pieces = Vec::new();
        let mut at = 0;
        for len in [300usize, 1024, 71, 4096, 2] {
            let end = (at + len).min(payload.len());
            decode_i2s(&payload[at..end], &mut st, &mut pieces);
            at = end;
        }
        decode_i2s(&payload[at..], &mut st, &mut pieces);
        assert_eq!(pieces, whole, "a boundary changed the audio");
    }

    /// The bits live in the even bytes of the stream. One odd-length payload
    /// moves every payload after it onto the odd bytes, which are always zero,
    /// so the input would go silent and never recover.
    #[test]
    fn an_odd_length_payload_does_not_silence_the_input() {
        let mut payload = Vec::new();
        for i in 0..40i32 {
            for u in units_for(0x4000 + i, i) {
                payload.push(u);
                payload.push(0);
            }
        }
        let mut whole = Vec::new();
        decode_i2s(&payload, &mut Rec::default(), &mut whole);

        // Split so that the first piece has an odd length.
        let mut st = Rec::default();
        let mut pieces = Vec::new();
        decode_i2s(&payload[..1025], &mut st, &mut pieces);
        decode_i2s(&payload[1025..], &mut st, &mut pieces);
        assert_eq!(pieces, whole, "an odd-length payload changed the audio");
        assert!(
            pieces.iter().any(|&b| b != 0),
            "the input went silent, which is the failure this guards"
        );
    }

    /// An all-zero window fits every phase, so locking on one would be a coin
    /// toss - and a phase locked wrongly during silence decodes the audio that
    /// follows it bit-rotated.
    #[test]
    fn silence_is_not_something_to_lock_on_to() {
        assert_eq!(find_alignment(&[0u8; 64 * 8]), None);
    }

    /// But it still has to come out. An idle mixer sends bit-exact zeros for
    /// as long as it is idle, and silence is silence at every phase, so it can
    /// be passed on without committing to one. Not doing that took the capture
    /// path dark until something was played - measured, twelve minutes of it
    /// decoded not one frame.
    #[test]
    fn silence_is_still_passed_on() {
        let mut st = Rec::default();
        let mut out = Vec::new();
        // Ten frames of digital silence, as the device actually sends them.
        decode_i2s(&[0u8; 10 * IN_UNITS_PER_FRAME * 2], &mut st, &mut out);
        assert_eq!(out.len(), 10 * IN_FRAME, "silence has to arrive as frames");
        assert!(out.iter().all(|&b| b == 0));
        assert!(!st.locked, "and still without committing to a phase");

        // And audio arriving after it locks and decodes properly.
        let mut payload = Vec::new();
        for _ in 0..8 {
            for u in units_for(0x123456, -0x2000) {
                payload.push(u);
                payload.push(0);
            }
        }
        out.clear();
        decode_i2s(&payload, &mut st, &mut out);
        assert!(st.locked, "audio after silence has to lock");
        assert_eq!(&out[..3], &[0x56, 0x34, 0x12]);
    }

    /// A new listener starts on a freshly aligned stream, not on whatever the
    /// last one left half-decoded.
    #[test]
    fn a_reset_decoder_starts_over() {
        let mut st = Rec::default();
        let mut out = Vec::new();
        decode_i2s(&[0x01, 0x00, 0x01, 0x00], &mut st, &mut out);
        assert!(!st.locked, "four bytes cannot lock a phase");
        assert!(!st.units.is_empty());
        st.reset();
        assert!(st.units.is_empty());
    }

    /// A PipeWire buffer and a pipe read both hand over whatever length they
    /// happen to have, so a partial frame has to survive to the next call
    /// rather than being played as half a sample.
    #[test]
    fn a_partial_host_frame_is_carried_not_consumed() {
        let mut host = Vec::new();
        for v in [0x0102_0300i32, 0x0405_0600] {
            host.extend_from_slice(&v.to_le_bytes());
        }
        host.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // a frame and a bit

        let mut wire = Vec::new();
        let used = encode_host(&host, 1.0, Out::Master, &mut wire);
        assert_eq!(used, HOST_FRAME, "only whole frames may be consumed");
        assert_eq!(wire.len(), OUT_FRAME);
        // s32 -> 24-bit slots: the low byte is dropped, not rounded.
        assert_eq!(&wire[0..3], &[0x03, 0x02, 0x01]); // slot 0: master left
        assert_eq!(&wire[3..6], &[0x06, 0x05, 0x04]); // slot 1: master right
        assert_eq!(&wire[6..12], &[0; 6]); // phones: not asked for
    }

    /// Four channels are two outputs, and getting them the wrong way round puts
    /// the master mix in the headphones and the cue feed in the room.
    #[test]
    fn four_host_channels_split_master_from_phones() {
        let mut host = Vec::new();
        for v in [0x0100_0000i32, 0x0200_0000, 0x0300_0000, 0x0400_0000] {
            host.extend_from_slice(&v.to_le_bytes());
        }
        let mut wire = Vec::new();
        let used = encode_host_quad(&host, 1.0, &mut wire);
        assert_eq!(used, QUAD_FRAME);
        assert_eq!(wire.len(), OUT_FRAME);
        assert_eq!(&wire[0..3], &[0, 0, 0x01]); // slot 0: master left
        assert_eq!(&wire[3..6], &[0, 0, 0x02]); // slot 1: master right
        assert_eq!(&wire[6..9], &[0, 0, 0x03]); // slot 2: phones left
        assert_eq!(&wire[9..12], &[0, 0, 0x04]); // slot 3: phones right
    }

    #[test]
    fn capture_widens_into_the_high_bytes() {
        let mut out = Vec::new();
        to_host(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66], &mut out);
        // 24-bit little-endian, so the sample's own bytes keep their order and
        // the padding goes where the bits that were never sent would be.
        assert_eq!(out, vec![0, 0x11, 0x22, 0x33, 0, 0x44, 0x55, 0x66]);
    }

    /// Asking for the headphones alone has to leave the master silent, which is
    /// the whole point of there being two outputs.
    #[test]
    fn a_stereo_source_goes_only_where_it_was_sent() {
        let mut f = [0u8; OUT_FRAME];
        encode_frame(0x010203, 0x040506, Out::Phones, &mut f);
        assert_eq!(&f[0..6], &[0; 6]); // master: silent
        assert_eq!(&f[6..9], &[0x03, 0x02, 0x01]); // slot 2: phones left
        assert_eq!(&f[9..12], &[0x06, 0x05, 0x04]); // slot 3: phones right
    }
}
