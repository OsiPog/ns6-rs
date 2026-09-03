//! Isochronous streams, via raw libusb async transfers.
//!
//! `rusb` has no isochronous support, so this drives libusb's async API directly.
//!
//! The NS6 needs these in *addition* to the bulk PCM pipes.
//! `selectConfiguration()` assigns four streaming pipes, not two:
//!
//! | field    | predicate                            | NS6 endpoint |
//! |----------|--------------------------------------|--------------|
//! | `0x16b0` | OUT && bulk                          | `0x04`       |
//! | `0x16a8` | IN  && bulk                          | `0x86`       |
//! | `0x16d8` | OUT && iso && wMaxPacketSize > 0x20  | `0x02`       |
//! | `0x16c0` | IN  && iso && wMaxPacketSize > 0x20  | `0x81`       |
//!
//! The driver's `requestIsocOut()` / `requestIOKeepAlive()` /
//! `isocWriteCompleteKeepAlive` machinery runs the isochronous side as a
//! keep-alive that clocks the device. Driving bulk alone gets one buffer accepted
//! and then permanent NAK, because nothing is clocking the engine.
//!
//! `0x81` is not only a keep-alive, though. It is an **explicit feedback
//! endpoint**, and what it says has to be acted on: see [`feedback_frames`] and
//! [`out_rate`].

use std::os::raw::{c_int, c_uint, c_void};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use rusb::ffi;

/// Set false to make every in-flight transfer retire instead of resubmitting.
pub static ISO_RUNNING: AtomicBool = AtomicBool::new(true);
pub static ISO_OUT_OK: AtomicU64 = AtomicU64::new(0);
pub static ISO_OUT_ERR: AtomicU64 = AtomicU64::new(0);
pub static ISO_IN_OK: AtomicU64 = AtomicU64::new(0);
pub static ISO_IN_DATA: AtomicU64 = AtomicU64::new(0);

/// Set true to hex-dump non-zero isochronous IN payloads, for protocol work.
pub static ISO_IN_DUMP: AtomicBool = AtomicBool::new(false);

/// Bresenham accumulator for the isochronous frame pattern, in sample-rate units.
static FRAME_ACC: AtomicU64 = AtomicU64::new(0);

/// Microframes per second at USB high speed.
const MICROFRAMES_PER_SEC: u64 = 8000;

/// Frames to place in the next isochronous OUT packet.
///
/// `requestIsocOut()` takes the count from a pattern table built by
/// `InitFramePattern`; at 44.1 kHz that averages 44100/8000 = 5.5125 frames per
/// microframe, so packets alternate between 5 and 6 frames. This reproduces the
/// same average with a Bresenham accumulator rather than a precomputed table.
///
/// The rate it averages to is not the nominal 44100 but whatever the device is
/// asking for - see [`out_rate`]. A table cannot do that, which is why this is
/// an accumulator.
///
/// `rate` is in [`RATE_FRAC`]ths of a frame per second, not whole hertz. A
/// whole hertz here is 23 ppm, which sounds like nothing and is not: whatever
/// the rounding fails to send accumulates in the device's buffer exactly as
/// the original mismatch did, only slower. One frame a second is a
/// millisecond of buffer every 44 seconds, which is the same fault again with
/// a longer fuse.
fn next_frame_count(rate: u64) -> u64 {
    let acc = FRAME_ACC.fetch_add(rate, Ordering::Relaxed) + rate;
    let frames = frames_for(acc);
    FRAME_ACC.fetch_sub(frames * PER_PACKET, Ordering::Relaxed);
    frames
}

/// The whole of the pattern: how many frames an accumulator has earned.
fn frames_for(acc: u64) -> u64 {
    acc / PER_PACKET
}

/// Accumulator units one packet is worth.
const PER_PACKET: u64 = MICROFRAMES_PER_SEC * RATE_FRAC;

/// Sub-hertz resolution on the OUT rate: it is carried in 256ths of a frame
/// per second, which is 0.09 ppm.
pub const RATE_FRAC: u64 = 256;

/// Size every packet of an isochronous OUT transfer from the frame pattern.
///
/// Sending the endpoint's full max packet size every microframe would be far
/// more audio than the sample rate calls for, and the device simply ignores the
/// stream.
unsafe fn size_out_packets(t: &mut ffi::libusb_transfer, bytes_per_frame: usize, rate: u64) {
    let descs =
        std::slice::from_raw_parts_mut(t.iso_packet_desc.as_mut_ptr(), t.num_iso_packets as usize);
    let mut total = 0usize;
    let mut frames = 0u64;
    for d in descs.iter_mut() {
        let n = next_frame_count(rate);
        frames += n;
        let len = n as usize * bytes_per_frame;
        d.length = len as c_uint;
        d.actual_length = 0;
        d.status = 0;
        total += len;
    }
    t.length = total as c_int;
    OUT_FRAMES.fetch_add(frames, Ordering::Relaxed);
}

/// Bytes per audio frame on the isochronous OUT endpoint, and the sample rate.
/// Set once at startup so the completion callback can resize packets.
static OUT_BYTES_PER_FRAME: AtomicU64 = AtomicU64::new(0);
static OUT_RATE: AtomicU64 = AtomicU64::new(0);

const LIBUSB_TRANSFER_TYPE_ISOCHRONOUS: u8 = 1;
const LIBUSB_TRANSFER_TYPE_BULK: u8 = 2;
const LIBUSB_TRANSFER_COMPLETED: c_int = 0;
const LIBUSB_TRANSFER_CANCELLED: c_int = 3;
/// The device has gone: unplugged, power-cycled, or otherwise off the bus.
/// Every handle we hold is dead and no amount of resubmitting brings it back.
const LIBUSB_TRANSFER_NO_DEVICE: c_int = 5;

/// Set when a transfer reports the device has vanished.
///
/// Without this the driver carries on happily with dead pipes: the counters
/// freeze, the ALSA port stays published, and anything reading it - Mixxx, a
/// capture - waits forever for events that can no longer arrive. Far better to
/// exit and let whoever started us start us again.
pub static DEVICE_GONE: AtomicBool = AtomicBool::new(false);

/* ------------------------------------------------ the feedback endpoint */

/// Frames the device has asked for, cumulative, as read from isochronous IN
/// `0x81`.
///
/// This pipe is not a keep-alive with nothing in it. It is an explicit feedback
/// endpoint: one packet per millisecond whose first byte is the number of audio
/// frames the device wants for that millisecond, `0x2c` or `0x2d` - 44 or 45,
/// averaging 44.1. That average is the device's own crystal, reported by the
/// device, and it is the only statement of it there is.
pub static FB_FRAMES: AtomicU64 = AtomicU64::new(0);
/// Feedback packets read.
pub static FB_PACKETS: AtomicU64 = AtomicU64::new(0);
/// Frames handed to the isochronous OUT pipe, cumulative.
pub static OUT_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Hex-dump this many more feedback packets, for reading the format off the
/// wire rather than off a decompilation.
static FB_DUMP_LEFT: AtomicU64 = AtomicU64::new(0);

/// Dump the next `n` feedback packets.
pub fn dump_feedback(n: u64) {
    FB_DUMP_LEFT.store(n, Ordering::Relaxed);
}

/// Read one feedback packet and add what it asks for to [`FB_FRAMES`].
///
/// libusb packs isochronous IN packets at the *requested* length, which for
/// this endpoint is never resized, so packet `index` starts at
/// `index * iso_packet_desc[0].length`.
unsafe fn take_feedback(t: &ffi::libusb_transfer, index: usize, len: usize) {
    let stride = (*t.iso_packet_desc.as_ptr()).length as usize;
    let data = std::slice::from_raw_parts(t.buffer.add(index * stride), len);

    let left = FB_DUMP_LEFT.load(Ordering::Relaxed);
    if left > 0 {
        FB_DUMP_LEFT.store(left - 1, Ordering::Relaxed);
        let hex: Vec<String> = data.iter().map(|b| format!("{b:02X}")).collect();
        eprintln!("  feedback[{len}B]: {}", hex.join(" "));
    }

    if let Some(frames) = feedback_frames(data) {
        FB_FRAMES.fetch_add(frames, Ordering::Relaxed);
        FB_PACKETS.fetch_add(1, Ordering::Relaxed);
        note_feedback_rate(frames);
    }
}

/// Frames asked for by one feedback packet.
///
/// The packet is three bytes, and they are the last three counts with the
/// newest first - which the device's own first packets spell out: `05 00 00`,
/// then `2C 05 00`, then `2C 2C 05`, then `2C 2C 2C`. So only the first byte
/// is new; the other two were already counted when they were, and adding them
/// again would treble the rate.
///
/// A value nowhere near a millisecond of audio is not a count at all - the
/// window is still filling - and is thrown away rather than being allowed to
/// steer the output rate off a zero.
pub fn feedback_frames(data: &[u8]) -> Option<u64> {
    let n = *data.first()? as u64;
    (FB_FRAMES_MIN..=FB_FRAMES_MAX).contains(&n).then_some(n)
}

/// The range a feedback count may plausibly take: 44.1 kHz is 44 or 45 frames
/// per millisecond, and a few percent either side of that is still a rate.
const FB_FRAMES_MIN: u64 = 40;
const FB_FRAMES_MAX: u64 = 49;

/// The rate the device is asking for, smoothed: frames per second in
/// [`FB_RATE_SHIFT`] fixed point. Zero until the first packet is read.
static FB_RATE: AtomicI64 = AtomicI64::new(0);

/// Feedback packets to see before [`out_rate`] will believe [`FB_RATE`].
///
/// The first few say `05 00 00`, `2C 05 00`, `2C 2C 05`: the packet carries the
/// last three counts, newest first, and the window has to fill before the
/// numbers in it are counts of anything.
const FB_WARMUP: u64 = 200;

/// Fixed-point shift on [`FB_RATE`], so a rate a fraction of a hertz off
/// nominal is still representable in an integer. Wider than [`RATE_FRAC`],
/// because the average it holds is of numbers only ever 44 or 45: all of the
/// rate is in the fraction.
const FB_RATE_SHIFT: u32 = 20;

/// How much of each packet's count to believe.
///
/// The counts themselves are only ever 44 or 45; the rate is in the proportion
/// of one to the other, so it only exists as an average. At one packet per
/// millisecond a shift of 10 averages over about a second - fast enough to
/// follow the device's own correction, which moves over tens of seconds, and
/// slow enough that the pattern does not jitter with the last packet received.
const FB_RATE_SHIFT_ALPHA: u32 = 10;

/// Fold one feedback count into the smoothed rate.
fn note_feedback_rate(frames: u64) {
    let r = FB_RATE.load(Ordering::Relaxed);
    FB_RATE.store(blend_rate(r, frames), Ordering::Relaxed);
}

/// One step of that average, in [`FB_RATE_SHIFT`] fixed point.
fn blend_rate(prev: i64, frames: u64) -> i64 {
    // A count is frames per millisecond; a rate is frames per second.
    let v = ((frames * 1000) as i64) << FB_RATE_SHIFT;
    // The first packet has nothing to average with, so it is the average.
    if prev == 0 {
        v
    } else {
        prev + ((v - prev) >> FB_RATE_SHIFT_ALPHA)
    }
}

/// The rate the isochronous OUT pattern should average to, in frames/s.
///
/// **This is the fix for the distortion that arrives partway through a long
/// tone.** The device has its own crystal and its own buffer, and it says on
/// `0x81` how many frames a millisecond it wants. Sending the nominal 44100
/// instead - a fixed pattern computed from the host's microframe clock - leaves
/// that request unanswered, so the device's buffer level is whatever the
/// difference between two independent clocks has integrated to. Measured here,
/// it did not merely drift: the device kept adjusting what it asked for,
/// nothing adjusted, and `asked - sent` swung +-1500 frames - +-34 ms of
/// buffer - on a cycle of about a minute. That is far more than the device
/// holds, so it wraps, and a wrap in the middle of a steady tone is a burst of
/// gross distortion. Roughly once a minute, which is exactly when it was heard.
///
/// Answering the request closes the loop the device is already trying to run:
/// give it the rate it asks for and its own correction settles, because the
/// thing it was correcting for has gone.
///
/// `nominal` is in whole frames per second; the answer is in [`RATE_FRAC`]ths
/// of one. Nominal is used until the feedback has been running long enough to
/// mean anything, and the answer is clamped to [`FB_RATE_TOLERANCE`] of it,
/// because a rate wildly off nominal is a misread packet rather than a crystal.
pub fn out_rate(nominal: u64) -> u64 {
    let floor = nominal * RATE_FRAC;
    if NO_FEEDBACK.load(Ordering::Relaxed) || FB_PACKETS.load(Ordering::Relaxed) < FB_WARMUP {
        return floor;
    }
    let rate = (FB_RATE.load(Ordering::Relaxed) as u64 * RATE_FRAC) >> FB_RATE_SHIFT;
    let slack = floor * FB_RATE_TOLERANCE / 100;
    rate.clamp(floor - slack, floor + slack)
}

/// How far from nominal the device is allowed to ask for, as a percentage.
///
/// Two clocks a few percent apart are not two crystals, so a number out here
/// is a bug on this side and must not be allowed to drive the hardware.
const FB_RATE_TOLERANCE: u64 = 2;

/// Set to ignore the feedback endpoint and send the nominal rate, which is what
/// this driver did before the rate was measured rather than assumed.
///
/// Kept because the difference is only visible against hardware, and a claim
/// about hardware wants to be checkable in one run.
pub static NO_FEEDBACK: AtomicBool = AtomicBool::new(false);

/// One submitted isochronous transfer, together with the buffer it points at.
struct IsoTransfer {
    ptr: *mut ffi::libusb_transfer,
    _buffer: Box<[u8]>,
}

/// A running isochronous stream. Cancels its transfers on drop.
pub struct IsoStream {
    transfers: Vec<IsoTransfer>,
}

extern "system" fn on_complete(xfer: *mut ffi::libusb_transfer) {
    unsafe {
        let t = &*xfer;
        if t.status == LIBUSB_TRANSFER_CANCELLED {
            return;
        }

        let is_input = t.endpoint & 0x80 != 0;
        let descs =
            std::slice::from_raw_parts(t.iso_packet_desc.as_ptr(), t.num_iso_packets as usize);

        for (i, d) in descs.iter().enumerate() {
            if d.status == LIBUSB_TRANSFER_COMPLETED {
                if is_input {
                    ISO_IN_OK.fetch_add(1, Ordering::Relaxed);
                    if d.actual_length > 0 {
                        ISO_IN_DATA.fetch_add(d.actual_length as u64, Ordering::Relaxed);
                        // Only `0x81` carries the rate. Any other isochronous
                        // IN pipe added later would be some other kind of
                        // payload, and taking it for a frame count would steer
                        // the hardware off it silently.
                        if t.endpoint == crate::protocol::EP_ISO_IN {
                            take_feedback(t, i, d.actual_length as usize);
                        }
                        if ISO_IN_DUMP.load(Ordering::Relaxed) {
                            dump_iso_in(t, i, d.actual_length as usize);
                        }
                    }
                } else {
                    ISO_OUT_OK.fetch_add(1, Ordering::Relaxed);
                }
            } else if !is_input {
                ISO_OUT_ERR.fetch_add(1, Ordering::Relaxed);
            }
        }

        if ISO_RUNNING.load(Ordering::Relaxed) {
            if !is_input {
                let bpf = OUT_BYTES_PER_FRAME.load(Ordering::Relaxed) as usize;
                let rate = OUT_RATE.load(Ordering::Relaxed);
                if bpf > 0 && rate > 0 {
                    size_out_packets(&mut *xfer, bpf, out_rate(rate));
                    if crate::audio::PLAY_ON.load(Ordering::Relaxed) {
                        fill_out_pcm(&mut *xfer);
                    } else if TONE_ON.load(Ordering::Relaxed) {
                        fill_out_tone(&mut *xfer, bpf);
                    } else {
                        silence_out(&mut *xfer);
                    }
                }
            }
            ffi::libusb_submit_transfer(xfer);
        }
    }
}

/// Hex-dump one isochronous IN packet if it carries anything but zeros.
///
/// libusb packs isochronous IN packets back-to-back at `max_packet_size`
/// stride, so packet `index` starts at `index * iso_packet_desc[0].length`.
unsafe fn dump_iso_in(t: &ffi::libusb_transfer, index: usize, len: usize) {
    let stride = (*t.iso_packet_desc.as_ptr()).length as usize;
    let base = t.buffer.add(index * stride);
    let data = std::slice::from_raw_parts(base, len);

    if data.iter().all(|&b| b == 0) {
        return;
    }
    let hex: Vec<String> = data.iter().map(|b| format!("{b:02X}")).collect();
    println!("      iso-in[{index}] {len}B: {}", hex.join(" "));
}

impl IsoStream {
    /// Start an isochronous stream on `endpoint`.
    ///
    /// # Safety
    /// `handle` must be a live libusb device handle with the owning interface
    /// claimed, and must outlive the returned stream.
    pub unsafe fn start(
        handle: *mut ffi::libusb_device_handle,
        endpoint: u8,
        packet_size: usize,
        packets_per_transfer: usize,
        transfer_count: usize,
        frame_pattern: Option<(usize, u64)>,
    ) -> Result<Self, i32> {
        if let Some((bytes_per_frame, rate)) = frame_pattern {
            OUT_BYTES_PER_FRAME.store(bytes_per_frame as u64, Ordering::Relaxed);
            OUT_RATE.store(rate, Ordering::Relaxed);
        }
        let mut transfers = Vec::with_capacity(transfer_count);

        for _ in 0..transfer_count {
            let total = packet_size * packets_per_transfer;
            let mut buffer = vec![0u8; total].into_boxed_slice();

            let ptr = ffi::libusb_alloc_transfer(packets_per_transfer as c_int);
            if ptr.is_null() {
                return Err(-1);
            }

            {
                let t = &mut *ptr;
                t.dev_handle = handle;
                t.endpoint = endpoint;
                t.transfer_type = LIBUSB_TRANSFER_TYPE_ISOCHRONOUS;
                t.timeout = 1000;
                t.buffer = buffer.as_mut_ptr();
                t.length = total as c_int;
                t.num_iso_packets = packets_per_transfer as c_int;
                t.callback = on_complete;
                t.user_data = std::ptr::null_mut::<c_void>();
                t.flags = 0;
                t.status = 0;
                t.actual_length = 0;

                // libusb_set_iso_packet_lengths is an inline helper in C, so set
                // the descriptors directly.
                let descs = std::slice::from_raw_parts_mut(
                    t.iso_packet_desc.as_mut_ptr(),
                    packets_per_transfer,
                );
                for d in descs.iter_mut() {
                    d.length = packet_size as c_uint;
                    d.actual_length = 0;
                    d.status = 0;
                }

                // OUT packets carry only as much audio as the sample rate calls for.
                if let Some((bytes_per_frame, rate)) = frame_pattern {
                    size_out_packets(t, bytes_per_frame, out_rate(rate));
                    if TONE_ON.load(Ordering::Relaxed) {
                        fill_out_tone(t, bytes_per_frame);
                    }
                }
            }

            let rc = ffi::libusb_submit_transfer(ptr);
            if rc != 0 {
                ffi::libusb_free_transfer(ptr);
                return Err(rc);
            }

            transfers.push(IsoTransfer {
                ptr,
                _buffer: buffer,
            });
        }

        Ok(Self { transfers })
    }
}

impl Drop for IsoStream {
    fn drop(&mut self) {
        unsafe {
            for t in &self.transfers {
                ffi::libusb_cancel_transfer(t.ptr);
            }
            // Let the cancellations complete before the buffers are freed.
            for _ in 0..20 {
                let tv = libc_timeval {
                    tv_sec: 0,
                    tv_usec: 20_000,
                };
                ffi::libusb_handle_events_timeout_completed(
                    std::ptr::null_mut(),
                    &tv as *const _ as *const _,
                    std::ptr::null_mut(),
                );
            }
            for t in &self.transfers {
                ffi::libusb_free_transfer(t.ptr);
            }
        }
    }
}

#[repr(C)]
struct libc_timeval {
    tv_sec: i64,
    tv_usec: i64,
}

/// Pump libusb events so isochronous callbacks fire.
///
/// `rusb`'s synchronous transfers drive the default context too; libusb
/// serialises event handling internally, so both can coexist.
pub fn pump_events() {
    let tv = libc_timeval {
        tv_sec: 0,
        tv_usec: 100_000,
    };
    unsafe {
        ffi::libusb_handle_events_timeout_completed(
            std::ptr::null_mut(),
            &tv as *const _ as *const _,
            std::ptr::null_mut(),
        );
    }
}

/* ------------------------------------------------------------------ bulk */

use std::sync::Mutex;

/// Raw MIDI bytes received from the controller, drained by the main loop.
pub static MIDI_IN: Mutex<Vec<u8>> = Mutex::new(Vec::new());
pub static MIDI_IN_BYTES: AtomicU64 = AtomicU64::new(0);
pub static BULK_IN_BYTES: AtomicU64 = AtomicU64::new(0);
pub static BULK_IN_OK: AtomicU64 = AtomicU64::new(0);
pub static BULK_IN_ERR: AtomicU64 = AtomicU64::new(0);
pub static LAST_BULK_STATUS: AtomicU64 = AtomicU64::new(0);

/// Endpoint whose payloads should be treated as MIDI (filler stripped).
static MIDI_IN_EP: AtomicU64 = AtomicU64::new(0);

extern "system" fn on_bulk_in(xfer: *mut ffi::libusb_transfer) {
    unsafe {
        let t = &*xfer;
        if t.status == LIBUSB_TRANSFER_CANCELLED {
            return;
        }
        if t.status != LIBUSB_TRANSFER_COMPLETED {
            // Silently resubmitting on error hides real faults - notably
            // LIBUSB_ERROR_OVERFLOW when the posted buffer is smaller than the
            // transfer the device wants to send.
            BULK_IN_ERR.fetch_add(1, Ordering::Relaxed);
            LAST_BULK_STATUS.store(t.status as u64, Ordering::Relaxed);
            if t.status == LIBUSB_TRANSFER_NO_DEVICE {
                DEVICE_GONE.store(true, Ordering::Relaxed);
                return;
            }
        }
        if t.status == LIBUSB_TRANSFER_COMPLETED && t.actual_length > 0 {
            BULK_IN_OK.fetch_add(1, Ordering::Relaxed);
            BULK_IN_BYTES.fetch_add(t.actual_length as u64, Ordering::Relaxed);

            if t.endpoint as u64 != MIDI_IN_EP.load(Ordering::Relaxed) {
                if crate::audio::REC_ON.load(Ordering::Relaxed) {
                    let data = std::slice::from_raw_parts(t.buffer, t.actual_length as usize);
                    let mut pcm = Vec::with_capacity(data.len() / 21);
                    if let Ok(mut a) = REC_ALIGN.lock() {
                        crate::audio::decode_i2s(data, &mut a, &mut pcm);
                    }
                    crate::audio::push_rec(&pcm);
                }
                if let Ok(mut slot) = PCM_DUMP.lock() {
                    if let Some(f) = slot.as_mut() {
                        let data = std::slice::from_raw_parts(t.buffer, t.actual_length as usize);
                        use std::io::Write;
                        if f.write_all(data).is_ok() {
                            PCM_DUMP_BYTES.fetch_add(t.actual_length as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
            if t.endpoint as u64 == MIDI_IN_EP.load(Ordering::Relaxed) {
                let data = std::slice::from_raw_parts(t.buffer, t.actual_length as usize);
                let mut clean = Vec::with_capacity(data.len());
                crate::protocol::strip_midi_filler(data, &mut clean);
                if !clean.is_empty() {
                    MIDI_IN_BYTES.fetch_add(clean.len() as u64, Ordering::Relaxed);
                    if let Ok(mut q) = MIDI_IN.lock() {
                        q.extend_from_slice(&clean);
                    }
                }
            }
        }
        if ISO_RUNNING.load(Ordering::Relaxed) {
            ffi::libusb_submit_transfer(xfer);
        }
    }
}

/// A queue of asynchronous bulk IN transfers, mirroring the vendor driver's
/// 8-deep URB queues. A single blocking read leaves gaps where nothing is
/// posted, which a device expecting continuous I/O can stall on.
pub struct BulkInStream {
    transfers: Vec<IsoTransfer>,
}

impl BulkInStream {
    /// # Safety
    /// `handle` must be a live libusb handle with the owning interface claimed,
    /// and must outlive the stream.
    pub unsafe fn start(
        handle: *mut ffi::libusb_device_handle,
        endpoint: u8,
        buf_size: usize,
        count: usize,
        is_midi: bool,
    ) -> Result<Self, i32> {
        if is_midi {
            MIDI_IN_EP.store(endpoint as u64, Ordering::Relaxed);
        }
        let mut transfers = Vec::with_capacity(count);

        for _ in 0..count {
            let mut buffer = vec![0u8; buf_size].into_boxed_slice();
            let ptr = ffi::libusb_alloc_transfer(0);
            if ptr.is_null() {
                return Err(-1);
            }
            {
                let t = &mut *ptr;
                t.dev_handle = handle;
                t.endpoint = endpoint;
                t.transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
                t.timeout = 0; // no timeout: stay posted until data arrives
                t.buffer = buffer.as_mut_ptr();
                t.length = buf_size as c_int;
                t.num_iso_packets = 0;
                t.callback = on_bulk_in;
                t.user_data = std::ptr::null_mut::<c_void>();
                t.flags = 0;
                t.status = 0;
                t.actual_length = 0;
            }
            let rc = ffi::libusb_submit_transfer(ptr);
            if rc != 0 {
                ffi::libusb_free_transfer(ptr);
                return Err(rc);
            }
            transfers.push(IsoTransfer {
                ptr,
                _buffer: buffer,
            });
        }
        Ok(Self { transfers })
    }
}

impl Drop for BulkInStream {
    fn drop(&mut self) {
        unsafe {
            for t in &self.transfers {
                ffi::libusb_cancel_transfer(t.ptr);
            }
            for _ in 0..20 {
                let tv = libc_timeval {
                    tv_sec: 0,
                    tv_usec: 20_000,
                };
                ffi::libusb_handle_events_timeout_completed(
                    std::ptr::null_mut(),
                    &tv as *const _ as *const _,
                    std::ptr::null_mut(),
                );
            }
            for t in &self.transfers {
                ffi::libusb_free_transfer(t.ptr);
            }
        }
    }
}

/* --------------------------------------------------------- audio testing */
//
// Everything below exists to answer one question on real hardware: does the
// isochronous OUT pipe carry audio the device actually plays, and does bulk IN
// 0x86 carry what is plugged into the analogue inputs? Both directions are
// silence in the bridge, so neither had ever been driven.

/// Fill isochronous OUT packets with a test tone instead of silence.
pub static TONE_ON: AtomicBool = AtomicBool::new(false);
/// Tone frequency in Hz.
pub static TONE_HZ: AtomicU64 = AtomicU64::new(1000);
/// Which of the four output channels the tone is written to, one bit each.
pub static TONE_MASK: AtomicU64 = AtomicU64::new(0xF);
/// Amplitude in percent of full scale.
pub static TONE_AMP: AtomicU64 = AtomicU64::new(50);
/// Give channel `c` the frequency `TONE_HZ * (c + 1)`, so one capture shows
/// where every output channel lands.
pub static TONE_SPREAD: AtomicBool = AtomicBool::new(false);
/// Sample index, so phase is continuous across transfers.
static TONE_PHASE: AtomicU64 = AtomicU64::new(0);

/// Write a 24-bit little-endian sine into every frame of a sized OUT transfer.
///
/// For an OUT transfer libusb reads the packets back-to-back out of the buffer
/// at the lengths `size_out_packets` just set, so walking the descriptors with
/// a running offset lands on the right bytes.
unsafe fn fill_out_tone(t: &mut ffi::libusb_transfer, bytes_per_frame: usize) {
    let rate = OUT_RATE.load(Ordering::Relaxed) as f64;
    if rate <= 0.0 {
        return;
    }
    let hz = TONE_HZ.load(Ordering::Relaxed) as f64;
    let amp = (TONE_AMP.load(Ordering::Relaxed) as f64 / 100.0).clamp(0.0, 1.0);
    let mask = TONE_MASK.load(Ordering::Relaxed);
    let spread = TONE_SPREAD.load(Ordering::Relaxed);
    let channels = bytes_per_frame / 3;
    let mut n = TONE_PHASE.load(Ordering::Relaxed);

    let descs = std::slice::from_raw_parts(t.iso_packet_desc.as_ptr(), t.num_iso_packets as usize);
    let mut off = 0usize;
    for d in descs.iter() {
        let frames = d.length as usize / bytes_per_frame;
        for _ in 0..frames {
            for c in 0..channels {
                let on = (mask >> c) & 1 == 1;
                // One frequency per channel makes the output mapping readable in
                // a single capture, instead of one run per channel.
                let f = if spread { hz * (c as f64 + 1.0) } else { hz };
                let phase = 2.0 * std::f64::consts::PI * f * (n as f64) / rate;
                let sample = (phase.sin() * amp * 8_388_607.0) as i32;
                let le = sample.to_le_bytes();
                let p = t.buffer.add(off + c * 3);
                *p = if on { le[0] } else { 0 };
                *p.add(1) = if on { le[1] } else { 0 };
                *p.add(2) = if on { le[2] } else { 0 };
            }
            off += bytes_per_frame;
            n += 1;
        }
    }
    TONE_PHASE.store(n, Ordering::Relaxed);
    dump_out(std::slice::from_raw_parts(t.buffer, t.length as usize));
}

/// Wire frames as handed to the isochronous OUT pipe, when dumping.
///
/// The last place the audio exists before the device has it, which is where a
/// question about what the device is actually being sent has to be answered.
/// Everything upstream - the graph, the resampler, the queue - can be measured
/// some other way; this cannot.
pub static OUT_DUMP: Mutex<Option<std::fs::File>> = Mutex::new(None);
/// Bytes written to that file.
pub static OUT_DUMP_BYTES: AtomicU64 = AtomicU64::new(0);

/// Start writing every frame sent to the device to `path`.
///
/// 12 bytes per frame: four 24-bit little-endian slots, master then phones.
pub fn dump_out_to(path: &str) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    *OUT_DUMP.lock().unwrap() = Some(f);
    Ok(())
}

/// Raw bulk IN 0x86 payloads, written straight to a file when capturing.
pub static PCM_DUMP: Mutex<Option<std::fs::File>> = Mutex::new(None);
/// Bytes written to that file.
pub static PCM_DUMP_BYTES: AtomicU64 = AtomicU64::new(0);

/// Start writing every audio-in payload to `path`.
pub fn capture_pcm_to(path: &str) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    *PCM_DUMP.lock().unwrap() = Some(f);
    Ok(())
}

/// Stop capturing and flush.
pub fn stop_pcm_capture() {
    *PCM_DUMP.lock().unwrap() = None;
}

/// I2S bit phase on the audio-in pipe, found once and kept.
static REC_ALIGN: Mutex<Option<usize>> = Mutex::new(None);

/// Forget the capture alignment, so the next payload re-locks.
pub fn reset_rec_align() {
    if let Ok(mut a) = REC_ALIGN.lock() {
        *a = None;
    }
}

/// Fill a sized OUT transfer from the playback queue.
/// Write silence into an isochronous OUT transfer.
///
/// Not filling a transfer is not the same as sending nothing. The buffer is
/// resubmitted exactly as it stands, so whatever was last written to it goes
/// out of the device again, and again: a few packets of old audio looping at
/// packet rate, which is heard as a steady buzz for as long as the driver runs.
/// Silence is a thing that has to be written.
unsafe fn silence_out(t: &mut ffi::libusb_transfer) {
    std::slice::from_raw_parts_mut(t.buffer, t.length as usize).fill(0);
}

unsafe fn fill_out_pcm(t: &mut ffi::libusb_transfer) {
    let len = t.length as usize;
    let dst = std::slice::from_raw_parts_mut(t.buffer, len);
    crate::audio::take_play(dst);
    dump_out(dst);
}

/// Write what is about to go out to the dump file, if one was opened.
///
/// Called from the completion callback, so it does block on a file - which is
/// why it only happens when asked for, and why the file wants to be somewhere
/// fast.
unsafe fn dump_out(frames: &[u8]) {
    use std::io::Write;
    if let Ok(mut slot) = OUT_DUMP.lock() {
        if let Some(f) = slot.as_mut() {
            if f.write_all(frames).is_ok() {
                OUT_DUMP_BYTES.fetch_add(frames.len() as u64, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device's first feedback packets are `05 00 00`, `2C 05 00`,
    /// `2C 2C 05`: the packet is a sliding window of the last three counts,
    /// newest first, and until it has filled the bytes in it are not counts.
    /// Letting one through would steer the output rate off a zero.
    #[test]
    fn only_plausible_feedback_counts_are_believed() {
        assert_eq!(feedback_frames(&[0x2C, 0x2C, 0x2C]), Some(44));
        assert_eq!(feedback_frames(&[0x2D, 0x2C, 0x2C]), Some(45));
        assert_eq!(feedback_frames(&[0x05, 0x00, 0x00]), None);
        assert_eq!(feedback_frames(&[0x00, 0x00, 0x00]), None);
        assert_eq!(feedback_frames(&[]), None);
    }

    /// The rate the device asks for is not a whole number of hertz, and the
    /// pattern has to average to it anyway: whatever it fails to send
    /// accumulates in the device's buffer exactly as the original mismatch
    /// did, only slower.
    #[test]
    fn the_frame_pattern_averages_a_fractional_rate() {
        for hz in [44_100.0f64, 44_101.6, 44_099.25, 44_102.87] {
            let rate = (hz * RATE_FRAC as f64) as u64;
            let mut acc = 0u64;
            let mut total = 0u64;
            for _ in 0..MICROFRAMES_PER_SEC {
                acc += rate;
                let n = frames_for(acc);
                acc -= n * PER_PACKET;
                assert!(
                    (5..=6).contains(&n),
                    "implausible packet at {hz} Hz: {n} frames"
                );
                total += n;
            }
            assert_eq!(
                total,
                rate / RATE_FRAC,
                "one second of packets must carry one second of audio at {hz} Hz"
            );
        }
    }

    /// The counts are only ever 44 or 45, so the rate the device is asking for
    /// exists only as the proportion of one to the other, and has to be
    /// averaged out of them rather than read off one.
    #[test]
    fn the_smoothed_rate_settles_on_the_average_count() {
        // 44.1 kHz is nine 44s to every 45.
        let mut r = 0i64;
        for i in 0..200_000u64 {
            r = blend_rate(r, if i % 10 == 0 { 45 } else { 44 });
        }
        let hz = (r >> FB_RATE_SHIFT) as f64;
        assert!((hz - 44_100.0).abs() < 5.0, "settled on {hz} Hz, not 44100");
    }

    /// A number out past the tolerance is this side misreading the pipe, not a
    /// crystal, and must not reach the hardware. Nor may an unread pipe leave
    /// the nominal rate behind.
    #[test]
    fn a_wild_rate_cannot_drive_the_hardware() {
        let nominal = 44_100u64;
        let floor = nominal * RATE_FRAC;
        let slack = floor * FB_RATE_TOLERANCE / 100;

        // Nothing read yet: the nominal rate, exactly.
        FB_PACKETS.store(0, Ordering::Relaxed);
        assert_eq!(out_rate(nominal), floor);

        FB_PACKETS.store(FB_WARMUP, Ordering::Relaxed);
        FB_RATE.store(60_000i64 << FB_RATE_SHIFT, Ordering::Relaxed);
        assert_eq!(out_rate(nominal), floor + slack);
        FB_RATE.store(8_000i64 << FB_RATE_SHIFT, Ordering::Relaxed);
        assert_eq!(out_rate(nominal), floor - slack);

        // And the switch back to what this driver used to do still works.
        NO_FEEDBACK.store(true, Ordering::Relaxed);
        assert_eq!(out_rate(nominal), floor);

        NO_FEEDBACK.store(false, Ordering::Relaxed);
        FB_PACKETS.store(0, Ordering::Relaxed);
        FB_RATE.store(0, Ordering::Relaxed);
    }
}
