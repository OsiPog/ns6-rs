# ns6

Userspace MIDI driver for the **Numark NS6** DJ controller on Linux.

The NS6 exposes only vendor-specific USB interfaces, so no kernel driver binds it and
no ALSA MIDI port appears — the controller is invisible to Mixxx and everything else.
`ns6` speaks the device's Ploytec protocol over libusb and publishes an ordinary ALSA
sequencer MIDI port in its place.

> This is for the **original NS6**, USB ID `15e4:0079`. The NS6II is a different,
> genuinely class-compliant device that already works on Linux and has a Mixxx mapping.

## Status

| Piece | State |
|---|---|
| Ploytec chipset identified, protocol documented | done |
| Vendor handshake (`'V'`, `'I'`, sample rate, arm) | done, verified on hardware |
| Endpoint roles and packet framing | done, from driver decompilation |
| ALSA MIDI port visible to Mixxx/PortMidi | done, verified with `aseqdump` |
| Isochronous keep-alive streams | running, verified on hardware |
| **Device accepting the bulk PCM OUT stream** | **not yet working** |
| MIDI from the control surface | blocked on the above |
| Mixxx mapping | not written — needs a real control capture first |

The device currently refuses (NAKs) every bulk OUT transfer, so it never enters its
streaming state and never reports its surface. `ns6 probe` exists to attack exactly
that. See [docs/PROTOCOL.md](docs/PROTOCOL.md) for what is known and how.

## Building

With Nix (flake):

```sh
nix build              # produces ./result/bin/ns6
nix run . -- probe     # or run directly
nix develop            # dev shell with cargo, clippy, alsa-utils, usbutils
```

Without Nix, you need `libusb-1.0` and `alsa-lib` development packages, then
`cargo build --release`.

## Usage

```sh
ns6            # bridge the controller to an ALSA MIDI port (default)
ns6 probe      # report device state and sweep bulk OUT configurations
ns6 test       # emit synthetic MIDI on the ALSA port, no hardware needed
```

`ns6 test` is the quickest way to confirm the ALSA half works:

```sh
ns6 test &
aseqdump -p "Numark NS6"
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
