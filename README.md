# ns6

Userspace driver for the **Numark NS6** DJ controller on Linux: MIDI and audio.

The NS6 exposes only vendor-specific USB interfaces, so no kernel driver binds it and
no ALSA device appears — the controller is invisible to Mixxx and everything else.
`ns6` speaks the device's Ploytec protocol over libusb and puts it back where it
belongs: an ordinary ALSA sequencer MIDI port for the control surface, and a PipeWire
sink and source of its own for the audio, both there for as long as the driver runs.

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
| Publishes its own PipeWire sink and source | done, plays and records |
| Output rate locked to the device's own clock | done, from the feedback endpoint |
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

Without Nix, you need the `libusb-1.0`, `alsa-lib` and `pipewire` development
packages, plus `libclang` (the PipeWire bindings are generated at build time), then
`cargo build --release`.

## Usage

```sh
ns6            # bridge the controller: ALSA MIDI port + PipeWire sink and source
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
systemctl --user stop ns6 && ns6 map ; systemctl --user start ns6
```

## Audio

The NS6 is a sound card as well as a control surface: four line/phono inputs, RCA
master, balanced XLR, and a headphone jack. Over USB it carries four output channels
— the master output and the headphone jack, a stereo feed each — and a stereo input,
all 24-bit at 44.1 kHz. Both directions work.

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

### As a sound card

Running the bridge is all of it:

```sh
ns6
```

Two nodes appear in the graph and stay there until the driver stops:

| Node | Shows up as | Carries |
|---|---|---|
| `ns6` | **Numark NS6 (master 1-2, phones 3-4)**, a sink | four channels: the master output and the headphone jack |
| `ns6-mix` | **Numark NS6 (mixer output)**, a source | what the controller sends back |

Both are s32le at 44.1 kHz, the device's own rate; PipeWire converts whatever an
application speaks into that, so anything can select them — Mixxx, a browser, a
recorder — with no modules to load and nothing to configure. `wpctl status` lists
them; `pw-play --target ns6 something.wav` is the one-line check.

### Master on 1-2, headphones on 3-4

The four output channels are the device's two outputs, in plain interleaved order:

| Channel | Slot | Comes out of |
|---|---|---|
| 1 | 0 | master, left |
| 2 | 1 | master, right |
| 3 | 2 | headphones, left |
| 4 | 3 | headphones, right |

Measured one slot at a time — `NS6_TONE_CH=0x1` through `0x8`, listening at both
outputs — and then confirmed through the sink with a different pitch on each of the
four channels.

So **playing to channels 3-4 is heard in the headphones only**, which is the whole
point of the split: cue a track without it going to the room. Turn the phones blend
knob to CUE to hear that feed alone; it blends towards PGM, which is the master
feed on 1-2.

In Mixxx, pick this one device and assign **Master → channels 1-2** and
**Headphones → channels 3-4**. A plain stereo application lands on 1-2, so a
browser plays out of the master and not into somebody's ears.

#### Mixxx has to reach the sink directly

Mixxx offers only the ALSA API here, where the device to choose is `pipewire`.
That device does not have a fixed channel count: PipeWire's ALSA plugin takes the
count from whatever node the stream is *routed to at the moment it is created*.
Point it at the NS6 and it makes four ports; let anything stereo intercept it and
it makes two, folding channels 3-4 into 1-2 inside the plugin - before PipeWire
sees them at all. Mixxx then looks correctly configured while everything comes out
of the master, and no setting inside Mixxx can help, because the damage is done
before its audio reaches the graph.

So the NS6 has to be the **default sink** when Mixxx opens the device, with nothing
stereo in between. The usual culprit is an effects host: Easy Effects with "process
all output streams" enabled captures every new stream into its own stereo sink, and
that is enough to break this. Turning that off, with the NS6 as the default sink,
is all it takes.

Check it rather than trusting it - four ports means four channels:

```sh
pw-link -Io | grep mixxx        # four: output_FL, FR, RL, RR
pw-link -Il | grep -A2 mixxx    # each landing on ns6:playback_FL/FR/RL/RR
```

Two ports means the fold already happened. Failing all else, a stream can be aimed
at the sink explicitly, though only by node id, which is assigned afresh each run:

```sh
PIPEWIRE_NODE=$(pw-dump ns6 | grep -m1 '"id"' | grep -oE '[0-9]+') mixxx
```

This is only true with the controller's panel switched to **PC**. In that mode its
faders, EQ and cue buttons send MIDI and stop touching the audio path — the mixing
is the host's job, and these two feeds are what the host sends back. The knobs that
remain live are master level, phones level and the phones blend.

Neither direction costs anything while nothing is linked to it. That matters more
than it sounds: the input is 5.6 MB/s of bitstream to decode for 176 kB/s of audio,
and it only runs while something is actually recording. Note that a level meter left
open — pavucontrol's input tab, say — counts as something recording.

The driver waits for PipeWire rather than requiring it. With no daemon to publish to
it says so once, keeps the MIDI half running and retries, so a restarted PipeWire
gets its nodes back and a driver started before the session's own services catches up
on its own.

The old path is still there for a machine with no sound server, or for getting at the
raw streams: naming `NS6_PLAY` and/or `NS6_REC` takes the audio side over, reading
and writing 44100 Hz s32le stereo through a file, a pipe or a FIFO, and publishes no
nodes at all.

Tuning:

| Variable | Default | What it does |
|---|---|---|
| `NS6_PLAY_MS` | `40` | Audio queued ahead of the device, in ms. Latency against safety. |
| `NS6_GAIN` | `1.0` | Output gain applied before the 24-bit clamp. |
| `NS6_OUT_PAIRS` | `both` | Where a *stereo* source goes: `master`, `phones`, or `both` (`a` and `b` still work). The file and pipe path only — the sink addresses both outputs itself. |
| `NS6_PLAY`, `NS6_REC` | unset | Stream through these paths instead of publishing nodes. |
| `NS6_NO_PIPEWIRE` | unset | Publish nothing; MIDI only. |
| `NS6_PW_DEBUG` | unset | Say when either node gains or loses its last link. |
| `NS6_NO_FEEDBACK` | unset | Send the nominal 44100 and ignore the rate the device asks for. |
| `NS6_FB_DUMP` | unset | Hex-dump this many feedback packets, then stop. |
| `NS6_OUT_DUMP` | unset | Write every wire frame handed to the device to this path. |

Round trip through the hardware measures 10-30 ms.

### The device's clock is the one that counts

The device has its own crystal, and it says what it wants: isochronous IN `0x81`
is an **explicit feedback endpoint**, answering one byte per millisecond with the
number of frames to send for that millisecond. The driver sends that rate rather
than the nominal 44100, and PipeWire's resampler is then steered to keep the
queue that feeds it at a constant depth. So the whole chain runs on the device's
clock and nothing drifts.

It has to be that way round. Sending a fixed 44100 - a pattern computed from the
*host's* microframe clock - leaves the request unanswered, and the difference
between two independent clocks accumulates inside the device, where nothing on
the host can see it. That is not a slow drift: the device runs a correction of
its own, and with nothing answering it, frames-sent-minus-frames-asked-for swung
over about 6500 frames - **147 ms of the device's buffer** - on a cycle of half a
minute or so. Far more buffer than it has, so it wraps, and a wrap in the middle
of a steady tone is a burst of gross distortion, once a minute or thereabouts.
Answering the request settles the device's own correction and holds the
difference to 53 frames - 1.2 ms - over three minutes.

The driver reports it, because the only way to see any of this is from both sides
at once:

```
clock: device asks 44101.10 Hz over 125018 ms, sending 44099.73 Hz; sent - asked = 641 frames
```

That last number staying put is the whole of it. `NS6_NO_FEEDBACK=1` sends the
nominal rate instead, which is what this driver did before the pipe was read, and
is how the two are compared in one run on the same binary.

Underruns stay at a few milliseconds, all of them at start-up.

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
| `src/pw.rs` | The PipeWire sink and source, and what keeps them there |
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
