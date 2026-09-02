# ns6

Userspace driver for the **Numark NS6** DJ controller on Linux: MIDI and audio.

The NS6 exposes only vendor-specific USB interfaces, so no kernel driver binds it and
no ALSA device appears — the controller is invisible to Mixxx and everything else.
`ns6` speaks the device's Ploytec protocol over libusb, publishes an ordinary ALSA
sequencer MIDI port in its place, and streams audio in and out of the sound card.

> This is for the **original NS6**, USB ID `15e4:0079`. The NS6II is a different,
> genuinely class-compliant device that already works on Linux and has a Mixxx mapping.

## Status

Working, both the control surface and the audio.

| Piece | State |
|---|---|
| Ploytec chipset identified, protocol documented | done |
| Vendor handshake (`'V'`, `'I'`, sample rate, start) | done, verified on hardware |
| Endpoint roles and packet framing | done, from driver decompilation |
| Device streaming: audio in, feedback, MIDI | done, verified on hardware |
| Audio out: a sine written to the iso pipe comes out of the device | done, measured |
| Audio in: the input bitstream decoded to PCM | done, measured |
| Usable as a PipeWire sink and source | done, plays and records |
| ALSA MIDI port visible to Mixxx/PortMidi | done, verified with `aseqdump` |
| Control surface enumerated (`ns6 learn`) | done |
| Mixxx mapping | in progress |

The one thing that had to be got right was the value written to the vendor
status register: `0x10`, not the `0x32` the Windows driver writes. See
[Starting the device](docs/PROTOCOL.md#starting-the-device).

## Building

With Nix (flake):

```sh
nix build              # produces ./result/bin/ns6
nix run .              # or run directly
nix develop            # dev shell with cargo, clippy, alsa-utils, usbutils
```

Without Nix, you need `libusb-1.0` and `alsa-lib` development packages, then
`cargo build --release`.

## Usage

```sh
ns6            # bridge the controller to an ALSA MIDI port (default)
ns6 map        # move a control, say what it was; writes ns6-surface.toml
ns6 learn      # watch the control surface: move a control, see its MIDI
ns6 probe      # report device state and sweep bulk OUT configurations
ns6 test       # emit synthetic MIDI on the ALSA port, no hardware needed
ns6 audio      # send a test tone out, prove the audio path on hardware
ns6 play F     # play 44100 Hz s32le stereo from a file, a pipe, or -
ns6 rec F      # capture 44100 Hz s32le stereo to a file, a pipe, or -
ns6 duplex P C # both at once
```

`ns6 test` is the quickest way to confirm the ALSA half works:

```sh
ns6 test &
aseqdump -p "Numark NS6"
```

The driver publishes **one** ALSA port that is both readable and writable. That
matters for Mixxx: PortMidi enumerates such a port twice, once as an input and
once as an output, and Mixxx pairs an input with an output by matching name.
Two separately-named ports leave it opening only the input, so the surface works
and the LEDs silently cannot.

### Lighting specific LEDs

`ns6 led` sends messages you name and holds them, which is how a question about
one light gets answered without walking the whole space:

```sh
ns6 led cc 1:17=127            # MIDI channel 1, CC 17, value 127
ns6 led cc 1:17 1:40           # both at once, value 127 each
ns6 led cc 1:82=64             # some other value
```

Channels are MIDI channels, 1-5, as the recorded maps write them. Messages known
to take the device off the bus are refused unless `NS6_LED_UNSAFE=1`, and
`NS6_LED_HOLD` sets the seconds.

Holding *several* is the part the walk below cannot do: it clears each candidate
before sending the next, so it can show what one message lights but never what a
combination does. That distinction is what says whether two lights are
independent lamps or two faces of one state.

### Mapping the LEDs

`ns6 leds` walks the output space so you can record what each message lights.
Right steps forward and sends, left steps back, Enter describes what is lit.
Holding an arrow skims at `NS6_LED_DWELL` per step.

**This can knock the device off the bus.** The MIDI OUT byte stream is not only
MIDI: the vendor driver bit-bangs a serial register interface into an audio chip
through the same pipe, using byte patterns `addr | 0x00/0x40/0x80/0xC0/0xE0` as
clock and data. Sweeping arbitrary bytes therefore clocks arbitrary bits into
that chip. A power cycle recovers it.

One message is confirmed to do it: **CC 57**, which kills the device on every
channel tried. Others may well exist and there is no way to predict them from the
protocol, so the walk collects them rather than being told about them: a message
that drops the device is written into `ns6-leds.toml` as a `[[hazard]]` and
stepped over from then on. `NS6_LED_SKIP=255,300` rules out positions by hand,
and `NS6_LED_UNSAFE=1` sends everything regardless.

**Treat a collected hazard as a suspect, not a verdict.** The walk blames whichever
candidate was lit when the device vanished, and that is a guess: CC 59 was recorded
this way and stepped over for a long time before being sent on its own, which it
survives on channel 2 at value 5 and on channel 4 at both 5 and 127. The walk had
blamed the wrong candidate. A wrong entry is not free either - it removes a number
from every later walk, which is how a real display can stay hidden - so confirm a
suspect by sending it alone with `ns6 led` before trusting it:

```sh
NS6_LED_UNSAFE=1 ns6 led cc 4:59=127     # survives; not a hazard
NS6_LED_UNSAFE=1 ns6 led cc 1:57=127     # takes the device off the bus
```

Descriptions are saved as you give them, so a crash costs nothing already
recorded, and the walk resumes:

```sh
NS6_LED_START=58 ns6 leds
```

which picks up where it stopped and carries over everything already in
`ns6-leds.toml`.

### Mapping the control surface

`ns6 map` records the panel the way you actually think about it: move
something, and it tells you what arrived and asks what it was.

```
  got:
    ch0 CC   20   value  93   range 0..127   214 msgs   fader/knob (full travel)
    ch0 CC   52   value  16   range 0..127   198 msgs   fader/knob (full travel)
  what was that? crossfader
  recorded #1: crossfader
```

It pairs up the MSB and LSB halves of a 14-bit control on its own, so a fader
comes back as one entry with two messages. Enter alone throws a reading away if
something else got caught in it, and `q` writes `ns6-surface.toml`.

A run picks up where the last one left off. Anything already in
`ns6-surface.toml` is carried over and its messages claimed, so moving a control
that is already named does nothing and only what is still missing gets asked
about. That is what the end of a recording session actually looks like: a
handful of stragglers, usually the ones that needed a hardware switch set
somewhere else first. Without it, collecting six more meant naming the other
hundred again.

Two things keep the readings clean. It spends a few seconds at startup watching
an untouched panel, so anything that chatters on its own - the platter sensors
do, constantly - is kept out of later readings unless it is what dominated
them. And a reading is only offered once movement has stopped for 600 ms, so a
fader sweep is captured whole.

The driver claims the device exclusively, so stop the service first if you
installed it:

```sh
systemctl stop ns6 && ns6 map ; systemctl start ns6
```

## Audio

The NS6 is a sound card as well as a control surface: four line/phono inputs, RCA
master, balanced XLR, and a headphone jack, mixed by the device itself. Over USB it
carries four output channels and a stereo input, all 24-bit at 44.1 kHz, and both
directions work.

The quickest check that the hardware is doing anything at all needs no cables:

```sh
ns6 audio            # a 1 kHz tone on all four output channels for six seconds
NS6_TONE_SPREAD=1 ns6 audio    # a different frequency per channel
```

The input listens to the device's own mixer, not to the input jacks directly, so a
tone played out over USB comes back on the input pipe. That round trip - out of the
host, through the mixer, back into the host - is the fastest way to prove both
directions at once:

```sh
NS6_PCM_DUMP=/tmp/in.raw ns6 audio 6      # tone out, raw input stream in
```

### As a PipeWire sink and source

PipeWire's pipe modules turn the two streams into an ordinary sink and source that
any application can select. Load them once:

```sh
pactl load-module module-pipe-sink   file=/tmp/ns6.sink   sink_name=NS6 \
      format=s32le rate=44100 channels=2
pactl load-module module-pipe-source file=/tmp/ns6.source source_name=NS6cap \
      format=s32le rate=44100 channels=2
```

Then run the bridge with audio, which publishes the MIDI port and streams at the
same time:

```sh
NS6_PLAY=/tmp/ns6.sink NS6_REC=/tmp/ns6.source ns6
```

Anything playing to the **NS6** sink now comes out of the controller, and **NS6cap**
records what the controller's mixer is putting out. Without the two environment
variables the bridge is MIDI-only, as before.

Tuning:

| Variable | Default | What it does |
|---|---|---|
| `NS6_PLAY_MS` | `40` | Audio queued ahead of the device, in ms. Latency against safety. |
| `NS6_GAIN` | `1.0` | Output gain applied before the 24-bit clamp. |
| `NS6_OUT_PAIRS` | `both` | Which of the two output pairs a stereo signal is written to: `a`, `b`, or `both`. |

Round trip through the hardware measures 10-30 ms. Nothing resamples: the device's
clock and the host's are independent, so a long session will eventually drift. Over
half a minute the measured rate holds at 44.09-44.13 kHz and underruns stay at a few
milliseconds, all of them at start-up.

### After a power cycle, wait

The device will happily stream USB - control transfers accepted, isochronous OUT
with zero errors, bulk IN at full rate - while its analogue side is still dead,
delivering digital silence in and nothing out. Give it ~15 s after power-on. A short
wait looks exactly like a routing bug.

## Device access

`ns6` talks to the raw USB device, so its usbfs node must be writable by you.
Quick and temporary:

```sh
sudo chown $USER /dev/bus/usb/<bus>/<dev>     # resets on replug
```

Durable, via the included rule:

```sh
sudo cp udev/70-numark-ns6.rules /etc/udev/rules.d/
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=usb --attr-match=idVendor=15e4
```

On NixOS, import the flake's module:

```nix
{
  imports = [ inputs.ns6.nixosModules.default ];
  environment.systemPackages = [ inputs.ns6.packages.${system}.default ];
}
```

## How it works

```
NS6 <--iso  OUT 0x02-- audio out: 4 x 24-bit, 12 bytes per frame
NS6 --bulk IN  0x86--> audio in: a raw I2S bitstream, decoded to stereo
NS6 --bulk IN  0x83--> raw MIDI bytes (0xFD filler) --> ALSA seq --> Mixxx
NS6 <--bulk OUT 0x04-- MIDI out, framed to 42 bytes <-- ALSA seq <-- Mixxx
```

The control surface only reports while the host is actively streaming, so the driver
keeps the isochronous pipe running even if you only care about MIDI - with silence
in it when nothing is playing.

Full derivation, including the endpoint predicates and the traps worth knowing about,
is in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Layout

| Path | Purpose |
|---|---|
| `src/protocol.rs` | Constants, framing and the invariants, with unit tests |
| `src/audio.rs` | Audio both ways: the I2S input decoder, the output frame, the queues |
| `src/device.rs` | USB transport: handshake, arming, streaming threads |
| `src/midi.rs` | ALSA sequencer port |
| `src/main.rs` | CLI: `run`, `probe`, `test`, `audio`, `play`, `rec`, `duplex` |
| `docs/PROTOCOL.md` | How the protocol was reverse-engineered |
| `udev/` | Device access rule |

## Licence

MIT.

The protocol description was obtained by decompiling Numark's Windows driver for the
purpose of interoperability — making hardware I own work with the operating system I
use. No vendor code is included or derived from; this is a clean implementation
against a documented wire protocol.

## Working on the hardware

Almost every startup parameter can be overridden from the environment, so
configurations can be swept against the device without a rebuild:

| Variable | Default | What it changes |
|---|---|---|
| `NS6_ARM` | `10` | Byte written to the vendor status register, in hex |
| `NS6_ISO_OUT_PACKETS` / `NS6_ISO_OUT_XFERS` | 40 / 3 | Isochronous OUT geometry |
| `NS6_ISO_IN_PACKETS` / `NS6_ISO_IN_XFERS` | 5 / 9 | Feedback pipe geometry |
| `NS6_PCM_IN_DEPTH` / `NS6_MIDI_IN_DEPTH` | 7 / 5 | Bulk IN queue depths |
| `NS6_ALT_ORDER` | `0,1` | Order the interfaces are set to alt 1 |
| `NS6_IFACES` | `0,1` | Which interfaces to claim |
| `NS6_NO_SET_CONFIG`, `NS6_NO_CLEAR_HALT`, `NS6_DESCRIPTORS` | off | Init steps to drop or add |

These exist because that is how the register value was found. This device is
stateful across runs and its failure mode is silence, so each candidate has to
be tried against hardware that has been power-cycled first. The rig used here
drives the controller's mains through a Zigbee smart plug and wraps a cycle, a
timed run and a `usbmon` capture into one line that says whether the device
streamed — roughly:

```sh
NS6_ARM=32 trial 10 windows-value    # mute
NS6_ARM=10 trial 10 candidate        # 45 MB of audio, feedback, MIDI
```

Those scripts are not in this repository; they hard-code one particular MQTT
broker and smart plug. The environment variables are the reusable part.

Neither are the USB captures the source comments cite — `ns6.pcap` alone is
36 MB. What they establish is written down in [docs/PROTOCOL.md](docs/PROTOCOL.md).
