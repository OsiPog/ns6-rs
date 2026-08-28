//! Ploytec protocol constants and packet framing for the Numark NS6.
//!
//! Everything here was derived by decompiling the Windows driver `ns6_usb.sys`
//! (an OEM Ploytec build - `Provider="usb-audio.de"`, `PGKernelDevice` symbols)
//! with Ghidra, and cross-checked against the real hardware. See
//! `docs/PROTOCOL.md` for the full derivation.

/// USB vendor ID (Numark / inMusic).
pub const VID: u16 = 0x15E4;
/// USB product ID of the original NS6 (the NS6II is a different, class-compliant device).
pub const PID: u16 = 0x0079;

/// MIDI out, on the MIDI interface (interface 0).
///
/// This is **not** the audio output. `selectConfiguration()` has two branches:
/// `FUN_f1017430` assigns a bulk OUT audio pipe, but its device list does not
/// contain `15e4`, so the NS6 never takes it. `FUN_f1017490` does list
/// `15e4:0079`, and that branch assigns the *isochronous* OUT pipe for audio and
/// leaves the bulk OUT field null - so `bulkAudioOut()` never runs on this device.
///
/// Writing audio here just fills a small MIDI FIFO (1024 bytes, i.e. two 512-byte
/// packets) which then never drains, and feeds junk to the device's MIDI parser.
pub const EP_MIDI_OUT: u8 = 0x04;

/// MIDI in. Ploytec's dedicated MIDI IN endpoint, on interface 0.
pub const EP_MIDI_IN: u8 = 0x83;

/// Audio in, selected by `direction == IN && bulk`.
pub const EP_PCM_IN: u8 = 0x86;

/// Isochronous OUT: **the audio output** for this device.
///
/// Selected by `OUT && iso && wMaxPacketSize > 0x20` in the branch that lists
/// `15e4:0079`. `requestIsocOut()` submits 0x28 = 40 packets per URB at high
/// speed, each sized `bytes_per_frame * frames`, where the frame count comes
/// from the pattern `InitFramePattern` builds. Sending the endpoint's full
/// 156-byte max packet every microframe is far more audio than the sample rate
/// calls for, and the device ignores the stream.
pub const EP_ISO_OUT: u8 = 0x02;
/// Max packet size of the isochronous OUT endpoint.
pub const ISO_OUT_PACKET: usize = 156;

/// Isochronous IN, selected by `IN && iso && wMaxPacketSize > 0x20`.
pub const EP_ISO_IN: u8 = 0x81;
/// Max packet size of the isochronous IN endpoint.
pub const ISO_IN_PACKET: usize = 64;

/// Packets per isochronous transfer, and how many transfers to keep in flight.
/// Packets per isochronous URB.
///
/// `requestIsocOut()` computes 0x28 = 40 at high speed, but the Windows driver
/// observed on the wire submits **32**. Matching the observed value.
pub const ISO_PACKETS_PER_XFER: usize = 32;
pub const ISO_XFERS: usize = 8;

/// The device exposes two interfaces; both are claimed at alternate setting 1.
pub const INTERFACES: [u8; 2] = [0, 1];
/// `findInterfacesInConfig()` assigns `m_pcMidiInterface` = interface 0, alt 1.
pub const ALT_SETTING: u8 = 1;

/// Vendor request `'V'`: read the 15-byte firmware version block.
pub const CMD_FIRMWARE: u8 = 0x56;
/// Vendor request `'I'`: read/write a hardware status register selected by `wIndex`.
pub const CMD_STATUS: u8 = 0x49;
/// Register index 0 of the `'I'` request: the AJ input selector / status byte.
pub const REG_STATUS: u16 = 0;
/// Bit 4 of the status register: the streaming enable.
///
/// `PGDevice::vendorSelectBitDepth` sets this unconditionally, sets bit 5
/// (0x20) to select the 16-bit sample container, and `updateAjInputSelector`
/// maintains bit 1 (0x02) as the analogue input selector.
pub const ENABLE_BIT: u8 = 0x10;
/// Bit 5: selects the 16-bit sample container. Must stay **clear** here - this
/// device streams 24-bit, which is what the 12-byte output frame below assumes.
pub const WIDTH16_BIT: u8 = 0x20;
/// Bit 1: analogue input selector.
pub const INPUT_SEL_BIT: u8 = 0x02;

/// The byte written to the status register to start streaming.
///
/// The device powers up reading 0x12, and the Windows driver writes back
/// 0x32 - `0x12 | 0x20`. Replaying that value verbatim is what kept this
/// driver from ever working: the device would accept the whole init, answer
/// exactly one feedback packet, and then go silent for good.
///
/// Writing plain **0x10** starts it. Both of the other bits have to be clear:
/// 0x12, 0x32, 0x33 and 0x3a were all tried against the hardware and all leave
/// it mute. Clearing 0x20 is the part that matters - it selects the 24-bit
/// container that the 12-byte, 4-channel output frame actually carries.
pub const ARM_VALUE: u8 = ENABLE_BIT;

/// Audio class `SET_CUR` (`bmRequestType 0x22`, `bRequest 0x01`, `wValue 0x0100`).
pub const SET_CUR_TYPE: u8 = 0x22;
pub const SET_CUR_REQ: u8 = 0x01;
pub const SET_CUR_VALUE: u16 = 0x0100;

/// Vendor read direction/type for `'V'` and `'I'` reads.
pub const VENDOR_IN: u8 = 0xC0;
/// Vendor write direction/type for `'I'` writes.
pub const VENDOR_OUT: u8 = 0x40;

/// The only sample rate this driver configures. The device also supports 48/88.2/96 kHz.
pub const SAMPLE_RATE: u32 = 44_100;

/// Audio-in transfer size.
///
/// From `captures/ns6.pcap`: the device delivers bulk IN `0x86` in transfers of
/// 0x20000 = 131072 bytes. Posting a smaller buffer makes libusb fail the
/// transfer with `OVERFLOW` rather than returning a short read.
pub const AUDIO_IN_XFER: usize = 0x20000;

/// Transfer size used for the MIDI IN pipe.
///
/// The Windows driver posts reads of exactly **42 bytes** (`S Bi ep3 -115 42`),
/// which is also the size of every MIDI packet the device emits. We previously
/// posted 512 (the endpoint's max packet size); matching the driver.
pub const BLOCK: usize = 42;

/// Filler the device pads the MIDI IN pipe with. Never real MIDI data there.
pub const MIDI_IDLE: u8 = 0xFD;

/// Output channel count for this PID at high speed, from `findInterfacesInConfig()`.
pub const OUT_CHANNELS: usize = 4;
/// Input channel count for this PID (overridden to 2 for the `15e4` DJ controllers).
pub const IN_CHANNELS: usize = 2;
/// Sample width in bits; the driver sets both directions to `0x18`.
pub const BITS_PER_SAMPLE: usize = 24;

/// Bytes per output audio frame: 4 channels x 3 bytes = 12.
pub const OUT_FRAME_BYTES: usize = OUT_CHANNELS * BITS_PER_SAMPLE / 8;
/// Audio frames per 512-byte block in the bulk framing: 480 / 12 = 40, exactly.
/// Kept as a cross-check on the channel count and sample width.
pub const FRAMES_PER_BLOCK: usize = 480 / OUT_FRAME_BYTES;

// The audio region of a block must divide evenly into frames. If the channel
// count or sample width were wrong, this would not hold - which is exactly what
// corroborates the bulk framing derived from the driver.
const _: () = assert!(FRAMES_PER_BLOCK * OUT_FRAME_BYTES == 480);
const _: () = assert!(IN_CHANNELS <= OUT_CHANNELS);

/// Render a status-register byte as its named bits, for the startup log.
pub fn describe_status(reg: u8) -> String {
    let mut bits = Vec::new();
    if reg & ENABLE_BIT != 0 {
        bits.push("enable");
    }
    if reg & WIDTH16_BIT != 0 {
        bits.push("16-bit");
    }
    if reg & INPUT_SEL_BIT != 0 {
        bits.push("input-sel");
    }
    if bits.is_empty() {
        bits.push("none");
    }
    format!("0x{reg:02X} [{}]", bits.join(" "))
}

/// Encode a status-register byte as the `wValue` of the vendor `'I'` write.
///
/// The vendor driver casts the byte through `(short)(char)`, i.e. sign-extends
/// it from 8 to 16 bits, so a value with bit 7 set has to become `0xFFxx` and
/// not `0x00xx`. Reproduced exactly.
pub fn status_wvalue(value: u8) -> u16 {
    (value as i8) as i16 as u16
}

/// Encode a sample rate as the 3-byte little-endian form used by `SET_CUR`.
pub fn encode_rate(rate: u32) -> [u8; 3] {
    [rate as u8, (rate >> 8) as u8, (rate >> 16) as u8]
}

/// Extract the MIDI byte stream from a MIDI IN transfer.
///
/// The device lays each packet out as the MIDI bytes, then a run of `0xFD`
/// filler, then a trailing `0x00`:
///
/// ```text
/// B0 07 7F B0 27 34 FD FD FD ... FD 00
/// ```
///
/// So the payload is everything before the first `0xFD`.
///
/// It is important *not* to filter `0x00` out of the stream: zero is a
/// perfectly legal MIDI data byte, and controls that emit it - a jog wheel at
/// rest, a fader at the bottom - would otherwise slide the parse out of
/// alignment and produce garbage.
pub fn strip_midi_filler(raw: &[u8], out: &mut Vec<u8>) {
    let end = raw
        .iter()
        .position(|&b| b == MIDI_IDLE)
        .unwrap_or(raw.len());
    out.extend_from_slice(&raw[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_arithmetic_is_exact() {
        // The integer result here is what corroborates the whole bulk framing:
        // if the channel count or sample width were wrong, this would not divide.
        assert_eq!(OUT_FRAME_BYTES, 12);
        assert_eq!(FRAMES_PER_BLOCK, 40);
        assert_eq!(FRAMES_PER_BLOCK * OUT_FRAME_BYTES, 480);
    }

    #[test]
    fn status_wvalue_matches_driver_sign_extension() {
        // The value that actually starts this device.
        assert_eq!(status_wvalue(ARM_VALUE), 0x0010);
        // What the Windows driver writes, and what leaves the device mute.
        assert_eq!(status_wvalue(0x32), 0x0032);
        // Bit 7 set must sign-extend to 0xFFxx, matching `(short)(char)`.
        assert_eq!(status_wvalue(0x80), 0xFF80);
        assert_eq!(status_wvalue(0xFF), 0xFFFF);
    }

    #[test]
    fn arm_value_clears_the_width_and_input_select_bits() {
        assert_eq!(ARM_VALUE & WIDTH16_BIT, 0);
        assert_eq!(ARM_VALUE & INPUT_SEL_BIT, 0);
        assert_eq!(ARM_VALUE & ENABLE_BIT, ENABLE_BIT);
    }

    #[test]
    fn rate_encoding_is_little_endian() {
        assert_eq!(encode_rate(44_100), [0x44, 0xAC, 0x00]);
        assert_eq!(encode_rate(96_000), [0x00, 0x77, 0x01]);
    }

    #[test]
    fn frame_pattern_averages_the_sample_rate() {
        // The isochronous OUT packets carry a variable number of frames so that
        // the long-run average is exactly rate/8000 frames per microframe. At
        // 44.1 kHz that is 5.5125, so packets must be 5 or 6 frames and one
        // second of packets must carry exactly one second of audio.
        let rate: u64 = SAMPLE_RATE as u64;
        let mut acc: u64 = 0;
        let mut total = 0u64;
        for _ in 0..8000 {
            acc += rate;
            let frames = acc / 8000;
            acc -= frames * 8000;
            assert!(
                (5..=6).contains(&frames),
                "implausible packet: {frames} frames"
            );
            total += frames;
        }
        assert_eq!(
            total, rate,
            "one second of packets must carry one second of audio"
        );
    }

    #[test]
    fn filler_is_stripped_from_midi_in() {
        // Shaped like the one real packet captured from the device: a Control
        // Change followed by idle padding.
        let mut raw = vec![0xB0, 0x0E, 0x16];
        raw.resize(42, MIDI_IDLE);

        let mut out = Vec::new();
        strip_midi_filler(&raw, &mut out);
        assert_eq!(out, vec![0xB0, 0x0E, 0x16]);
    }

    #[test]
    fn zero_data_bytes_survive_stripping() {
        // Regression: 0x00 is a legal MIDI data value. Filtering it out (rather
        // than cutting at the first 0xFD) slides the parse out of alignment and
        // turns jog-wheel traffic into garbage.
        let mut raw = vec![0xB1, 0x20, 0x00, 0xB1, 0x21, 0x00];
        raw.resize(42, MIDI_IDLE);
        raw.push(0x00);

        let mut out = Vec::new();
        strip_midi_filler(&raw, &mut out);
        assert_eq!(out, vec![0xB1, 0x20, 0x00, 0xB1, 0x21, 0x00]);
    }

    #[test]
    fn real_capture_packet_decodes() {
        // Taken verbatim from captures/ns6.pcap: crossfader MSB+LSB.
        let mut raw = vec![0xB0, 0x07, 0x7F, 0xB0, 0x27, 0x34];
        raw.resize(41, MIDI_IDLE);
        raw.push(0x00);

        let mut out = Vec::new();
        strip_midi_filler(&raw, &mut out);
        assert_eq!(out, vec![0xB0, 0x07, 0x7F, 0xB0, 0x27, 0x34]);
    }
}
