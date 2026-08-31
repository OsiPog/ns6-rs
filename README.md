# ns6

Userspace MIDI driver for the **Numark NS6** DJ controller on Linux.

The NS6 exposes only vendor-specific USB interfaces, so no kernel driver binds it and
no ALSA MIDI port appears — the controller is invisible to Mixxx and everything else.
`ns6` speaks the device's Ploytec protocol over libusb and publishes an ordinary ALSA
sequencer MIDI port in its place.

> This is for the **original NS6**, USB ID `15e4:0079`. The NS6II is a different,
> genuinely class-compliant device that already works on Linux and has a Mixxx mapping.

## Status

Working. The controller shows up as an ALSA sequencer port and reports its
surface.

| Piece | State |
|---|---|
| Ploytec chipset identified, protocol documented | done |
| Vendor handshake (`'V'`, `'I'`, sample rate, start) | done, verified on hardware |
| Endpoint roles and packet framing | done, from driver decompilation |
| Device streaming: audio in, feedback, MIDI | done, verified on hardware |
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

### Mapping the LEDs

`ns6 leds` walks the output space so you can record what each message lights.
Right steps forward and sends, left steps back, Enter describes what is lit.
Holding an arrow skims at `NS6_LED_DWELL` per step.

**This can knock the device off the bus.** The MIDI OUT byte stream is not only
MIDI: the vendor driver bit-bangs a serial register interface into an audio chip
through the same pipe, using byte patterns `addr | 0x00/0x40/0x80/0xC0/0xE0` as
clock and data. Sweeping arbitrary bytes therefore clocks arbitrary bits into
that chip. A power cycle recovers it.

Two messages are confirmed to do it: **CC 57 on channel 1**, the 58th
candidate, and **CC 59 on channel 4**. They share neither a number nor a
channel, so there is no pattern to extrapolate from and others almost certainly
exist. There is no way to predict them from the protocol either, so the walk
collects them rather than being told about them: a message
that drops the device is written into `ns6-leds.toml` as a `[[hazard]]` and
stepped over from then on. `NS6_LED_SKIP=255,300` rules out positions by hand,
and `NS6_LED_UNSAFE=1` sends everything regardless.

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
NS6 --bulk OUT 0x04--> silence keeps the device streaming
NS6 --bulk IN  0x83--> raw MIDI bytes (0xFD filler) --> ALSA seq --> Mixxx
NS6 --bulk IN  0x86--> PCM in (drained)
NS6 <--MIDI byte at offset 480 of each out block <-- ALSA seq <-- Mixxx
```

The control surface only reports while the host is actively streaming PCM, so the
driver must keep pumping silence even if you only care about MIDI. Each 512-byte
output block carries 480 bytes of audio, one MIDI byte, and one control byte.

Full derivation, including the endpoint predicates and the traps worth knowing about,
is in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Layout

| Path | Purpose |
|---|---|
| `src/protocol.rs` | Constants, framing and the invariants, with unit tests |
| `src/device.rs` | USB transport: handshake, arming, streaming threads |
| `src/midi.rs` | ALSA sequencer port |
| `src/main.rs` | CLI: `run`, `probe`, `test` |
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
