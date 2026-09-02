# Numark NS6 USB protocol

Reverse-engineered from the Windows driver `ns6_usb.sys` (v2.9.64) using Ghidra
headless decompilation, and cross-checked against real hardware.

## Why the device needs a driver at all

Every USB interface the NS6 exposes is **vendor-specific class 255**:

```
bDeviceClass  255 Vendor Specific Class
  Interface 0 (alt 1): class 255 - iso OUT 0x02 (156 B), bulk IN 0x83, bulk OUT 0x04
  Interface 1 (alt 1): class 255 - iso IN 0x81 (64 B),  bulk IN 0x86
```

There are no USB-Audio and no USB-MIDI-Streaming descriptors anywhere, so:

- no kernel driver binds it, and no ALSA card or MIDI port appears
- `snd-usb-audio` has no quirk for `15e4:*`
- Mixxx's USB-bulk backend only knows four Hercules devices, so that path needs a
  patched Mixxx

The NS6 does speak ordinary MIDI, but tunnelled over a proprietary transport.

## Provenance: it is a Ploytec device

`ns6_usb.inf` declares `Provider="usb-audio.de"` — Ploytec GmbH. The binary is full
of `PGKernelDevice` / `PGDevice` symbols and references to
`PROD_PLOYTEC_ALLEN_HEATH_XONE_2D/4D/DX`, so it is an OEM build of Ploytec's driver.

The open-source [Ozzy](https://github.com/mischa85/Ozzy) project documents the same
chipset for Allen & Heath Xone hardware. It does **not** know vendor `15e4`, and the
NS6 differs from the Xone in two important ways (endpoint numbers, and the control
byte value), so its constants cannot be adopted wholesale.

## A note on the captures

Comments in the source and the tables below cite `captures/ns6.pcap`,
`captures/windows.usbmon` and friends. Those files are not distributed with this
repository — `ns6.pcap` alone is 36 MB, and they are recordings of one particular
unit on one particular machine. The citations are kept because they say where
each claim came from; everything they establish is written down here.

## Endpoints

`findInterfacesInConfig()` first looks for an interface with `class == 1 && subclass
== 3` (USB Audio / MIDI Streaming). No NS6 interface matches, so it falls through to
a table of vendor-specific devices — which lists `15e4:0079` explicitly — and assigns:

```c
m_pcMidiInterface = getInterface(config, 0);      // interface 0
m_pcMidiInterface = getAltSetting(iface, 1);      // alt setting 1
```

`selectConfiguration()` then picks the streaming pipes by predicate. Decoding those
predicates (`+0x10` = `bEndpointAddress`, `+0x12` = `bmAttributes`, `+0x18` =
`wMaxPacketSize`; type bits 1=iso, 2=bulk, 3=interrupt):

| Field | Predicate | NS6 endpoint |
|---|---|---|
| `0x16b0` | `OUT && bmAttributes&3 == 2` (bulk) | **`0x04`** |
| `0x16a8` | `IN && bmAttributes&3 == 2` (bulk) | **`0x86`** |
| `0x16a8` fallback | `IN && type == 3` (interrupt) | none |
| `0x16c8` | `OUT && iso && wMaxPacketSize == 4` (feedback) | none |
| `0x16d8` | `OUT && iso && wMaxPacketSize > 0x20` | **`0x02`** |
| `0x16c0` | `IN && iso && wMaxPacketSize > 0x20` | **`0x81`** |

So **PCM out = bulk `0x04`, PCM in = bulk `0x86`, MIDI in = bulk `0x83`** — and the
driver *also* runs **isochronous `0x02` and `0x81`** alongside them. The isochronous
side is the keep-alive that clocks the device (`requestIsocOut`,
`requestIOKeepAlive`, `m_pcOutPipeKeepAlive`, `isocWriteCompleteKeepAlive`).

Both halves are needed. Driving isochronous alone achieves nothing; driving bulk
alone gets exactly one buffer accepted and then permanent NAK.

> **Isochronous transfers are unacknowledged**, so an iso-only implementation reports
> success no matter what the device does with the data. During this work 7.19 million
> iso packets were sent with "zero errors" while nothing happened. Any success metric
> that cannot fail is not a success metric — for this device the meaningful signal is
> whether **bulk** OUT transfers complete.

## Channel layout

`findInterfacesInConfig()` sets, for high-speed operation:

```c
if (!high_speed) { inCh = 2; outCh = 2; }
else             { inCh = 4; outCh = 4; }
// then, for 15e4 PIDs 0x71, 0x79, 0x2c, 0x26, 0x75:
inCh = 2;
inBps = 0x18;   // 24-bit
outBps = 0x18;  // 24-bit
```

So the NS6 is **4 channels out, 2 in, 24-bit**. That gives 4 × 3 = 12 bytes per output
frame, and 480 ÷ 12 = **exactly 40 frames per block**. The framing below is only
consistent if those numbers are right, so the integer division is a useful check.

## Bulk OUT framing

`PGDevice::bulkAudioOut()` builds each transfer as 256 blocks of 512 bytes
(`TransferBufferLength = 0x20000` = 131072), with 8 IRPs in flight. The decompiled
inner loop:

```c
local_20 = 1;
for (local_10 = 0; local_10 < 0x100; local_10++) {
    *(local_28 + 0x1e0) = 0xfd;                     // MIDI slot: idle
    if (local_20 != 0) {
        local_20 = queue_pop(param_3, param_4, 1);  // pop a command byte
        *param_4 = *param_4 | 0x18;                 // always set 0x18
    }
    *(local_28 + 0x1e1) = *param_4;                 // control slot
    local_28 = local_28 + 0x200;                    // next 512-byte block
}
```

Per 512-byte block:

```
bytes 0..479     audio (40 frames x 12 bytes)
byte  480        MIDI out byte, 0xFD when idle
byte  481        control byte
bytes 482..511   padding
```

The control byte is a queued command OR'd with `0x18`. The queue is empty in steady
state and the backing field is zero-initialised, so in practice it is **`0x18`**.

> This is **not** the `0xFF` sync byte used by the Xone members of the same family.
> Taking `0xFF` from the Xone documentation rather than this device's own driver was
> one of the wrong turns during this work.

MIDI out is rate-limited to one byte per block by `USBMidiPattern::initForBulk`,
which builds a 1/0 pattern from the sample rate so the stream stays inside MIDI's
31250 baud.

## MIDI in

Raw MIDI bytes arrive on bulk `0x83`, padded with `0xFD`. A real captured packet:

```
42 bytes: B0 0E 16 FD FD FD ... FD
```

`B0 0E 16` is a Control Change on channel 1, controller 14, value 0x16. Strip `0xFD`
and `0x00` and what remains is an ordinary MIDI byte stream.

## Vendor requests

The generic helper is
`deviceRequest(this, dir, type, recipient, bRequest, wValue, wIndex, buf, &len, ...)`
where type `0x40` = vendor and `0x20` = class; the recipient maps to URB functions
`0x17`/`0x18`/`0x19`.

| Request | Direction | Meaning |
|---|---|---|
| `'V'` = `0x56` | `0xC0` read, 15 bytes | Firmware version. This unit returns `31 01 03 02 02`. |
| `'I'` = `0x49` | `0xC0` read / `0x40` write | Hardware status register, selected by `wIndex`. |
| `SET_CUR` | `0x22`, `bRequest 0x01`, `wValue 0x0100` | Sample rate, `wIndex` = endpoint, 3 bytes LE. |

Note `wIndex 0x0005` (used by the Xone) does not exist on the NS6; the rate goes to
`0x86` and `0x04`.

## Starting the device

Read `'I'` register 0, then write the register back with the **whole value in
`wValue`**, sign-extended through `int8` — the driver's cast is `(short)(char)`,
so a byte with bit 7 set has to produce `0xFFxx`:

```rust
pub fn status_wvalue(value: u8) -> u16 {
    (value as i8) as i16 as u16
}
```

The value is what matters, and it is not a single "arm" bit.
`PGDevice::vendorSelectBitDepth` builds the byte from three independent fields:

| Bit | Meaning |
|---|---|
| `0x10` | Streaming enable. Set unconditionally. |
| `0x20` | Selects the **16-bit** sample container. |
| `0x02` | Analogue input selector, maintained by `updateAjInputSelector`. |

This unit reads `0x12` at power-up. The Windows driver writes back `0x32`
(`0x12 | 0x20`), and copying that value verbatim is what kept this driver from
ever working. Writing **`0x10`** starts it.

Both of the other bits have to be clear. Swept against the hardware, from a
power cycle each time:

| Written | Result |
|---|---|
| `0x10` | streams: audio on `0x86`, feedback on `0x81`, MIDI on `0x83` |
| `0x12` | mute |
| `0x32` (what Windows writes) | mute |
| `0x33` | mute |
| `0x3a` | mute |

Clearing `0x20` is the part that matters: it selects the 24-bit container, which
is what the 12-byte, 4-channel output frame described above actually carries. Ask
for 16-bit and the device accepts the whole init, answers exactly one 3-byte
feedback packet, and then goes silent for good — which is a remarkably good
imitation of a device that is simply ignoring you.

Register 0 **persists across power cycles**, and the write is level-triggered:
there is no need to cycle the enable bit to produce an edge.

## There is no "enable MIDI" command

`PGDevice::startStreaming()` special-cases only VIDs `0x0dba`, `0x0a4a`, `0x1502`,
`0x200c`, `0x0d8c` and (in a rate-code helper) `0x0926`. `15e4` matches none of them,
so the NS6 takes the generic path.

When a MIDI client opens the port, `ns6_midi.sys` sends
`IRP_MJ_INTERNAL_DEVICE_CONTROL` with code `0x1ffd10` to `ns6_usb.sys` and receives a
table of function pointers. That exchange is **entirely internal** — no USB traffic.

So MIDI is not unlocked by a command. It is a consequence of the device streaming,
and the device streams when the status register says so.

## Stream geometry

Taken from `captures/ns6.pcap`, a capture of the vendor driver on real Windows
while MIDI was flowing. Within 600 µs of the last control transfer it posts:

| Pipe | Depth | Size |
|---|---|---|
| bulk IN `0x86` (audio in) | 7 | 131072 bytes each |
| iso IN `0x81` (feedback) | 9 | 5 packets of 3 bytes |
| iso OUT `0x02` (audio out) | 3 | 40 packets, ~2646 bytes total |
| bulk IN `0x83` (MIDI in) | 5 | 42 bytes each |

in that order, and the device starts streaming about 7 ms later.

Isochronous URBs never set `USBD_START_ISO_TRANSFER_ASAP`; every one carries an
explicit absolute start frame, chosen as `current + 4` and advanced per
submission.

The iso OUT packet lengths alternate 72 and 60 bytes — 6 and 5 audio frames of
12 bytes — with an extra 72 roughly every 40 packets. That averages 5.5125
frames per microframe, which is 44100/8000. The feedback endpoint answers with
one byte per millisecond, `0x2c` or `0x2d`: 44 or 45 frames, the same rate seen
from the other side.

## Gotchas

**Claim interfaces before any control transfer.** Issuing control transfers, or
`libusb_set_configuration`, before claiming leaves `libusb_claim_interface` returning
`LIBUSB_ERROR_BUSY` permanently — with no kernel driver bound and no process holding
the node. This is not documented anywhere and cost hours to find.

**MIDI-OX cannot see this device on Windows.** It is a legacy MME application, and
the NS6 appears under Windows' newer MIDI Services stack ("MIDI endpoints" in Device
Manager). Use MIDIberry or Pocket MIDI. A silent MIDI-OX is not evidence about the
hardware.

**MIDI flows on Windows with only the MIDI port open** — no audio application is
required. Whatever makes the device report is done by the driver at startup, not by
an audio client.

**USBPcap** needs a reboot after installation before its filter devices exist, and
`-A` to capture a device that is not plugged in yet.

**Check that a "working" reference capture actually worked.** The usbmon capture
taken through the Windows VM (`captures/windows.usbmon`) looks like a reference
trace and is not one: in it endpoint `0x83` was submitted and cancelled 292 times
without ever delivering a byte, and only four audio transfers completed. The
guest's Windows driver was failing in exactly the way ours was, and its init loop
repeating ~154 times was the symptom, not the recipe. Matching our traffic to it
was matching a failure. The only capture in this repository that shows the device
working is `captures/ns6.pcap`, from real hardware.

## Audio in is not PCM

Everything above describes what the Windows driver *does*. What the device actually
sends on bulk IN `0x86` was measured on hardware, and it is not the 24-bit PCM the
channel-layout arithmetic implies.

The pipe delivers 5.6448 MB/s at 44.1 kHz — 21x the 264.6 kB/s that 2 channels of
24-bit audio need. Every byte pair is `xx 00` where `xx` is `0x00` or `0x01`: one
bit per two-byte unit, in bit 0 of the even byte, with the odd byte always zero.
That is 2.8224 M units/s, exactly **64 x 44100**.

Taking the density of bit positions modulo several candidate frame lengths settles
the framing. At period 64, and only at period 64, eight positions are *always* zero:

```
pos  0..23   ~0.49 density   slot A data
pos 24..31    0.00           slot A pad
pos 32..55   ~0.50           slot B data
pos 56..63    0.00           slot B pad
```

So the device streams its ADC's raw **I2S line**: 64 bit-clocks per audio frame, two
32-bit slots, 24 significant bits MSB-first followed by 8 zeros. One bit per two
bytes is a wasteful way to carry it, which is presumably why the vendor driver
posts such enormous transfers. Decoding is in `src/audio.rs`; the pad bits are also
what the decoder locks its bit phase onto, since there is no word clock on the wire.

A 131072-byte transfer is 65536 units, which is exactly 1024 audio frames — the
transfer size is frame-aligned, so the phase only has to be found once.

## Audio out, measured

Output is what `protocol.rs` always said: interleaved 24-bit little-endian, 4
channels, 12 bytes per frame, written into the isochronous OUT packets at the frame
counts the Bresenham pattern produces. Writing a sine into it comes out of the
device, at the frequency asked for.

The channel mapping was measured by giving each slot its own frequency
(`NS6_TONE_SPREAD=1`) and looking at where each one landed:

| Slot | Bytes | Where it comes out |
|---|---|---|
| 0 | 0..3 | one side of the output |
| 1 | 3..6 | the same side as slot 0 |
| 2 | 6..9 | the other side |
| 3 | 9..12 | the same side as slot 2 |

So the slots are grouped by **side**, not by pair: a stereo signal is (slot 0,
slot 2) or (slot 1, slot 3), and the two pairs sum. Which analogue jack each pair
reaches depends on the mixer's own routing, which is not on this wire.

## What the input actually listens to

The ADC does not hear the line inputs directly. It hears the mixer, and the mixer
is the device's own: with nothing routed to it, bulk IN `0x86` is not noise but
*every bit zero*. Playing audio out over USB makes it appear on the input pipe
within the same second, so a complete loop — USB out, through the mixer, back in
over USB — can be measured with no cables attached at all. That loop is the
quickest way to prove both directions still work.

## Give it time after a power cycle

Streaming can come up perfectly — control transfers accepted, isochronous OUT with
zero errors, bulk IN flowing at full rate — while the analogue side of the device is
still dead, delivering digital silence in and producing nothing out. Waiting ~15 s
after power-on before opening the device fixes it. A short wait looks exactly like a
routing bug, and cost an hour of chasing one.
