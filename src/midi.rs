//! ALSA sequencer port, so the NS6 appears as an ordinary MIDI device.
//!
//! Mixxx uses PortMidi on Linux, which enumerates ALSA sequencer ports. A port
//! that is both readable and writable is enumerated twice - once as an input
//! device, once as an output - under the same name, and Mixxx pairs an input
//! with an output **by matching name**.
//!
//! That pairing is why there is one port here rather than two. With separate
//! "Numark NS6 MIDI" and "Numark NS6 MIDI OUT" ports, Mixxx found both, paired
//! neither, and opened only the input - so the control surface worked and the
//! LEDs could not, however correct the mapping was.

use std::ffi::CString;

use alsa::seq::{EventType, MidiEvent, PortCap, PortInfo, PortType, Seq};

pub const CLIENT_NAME: &str = "Numark NS6";
pub const PORT_NAME: &str = "Numark NS6 MIDI";

pub struct MidiPort {
    seq: Seq,
    surface_port: i32,
    encoder: MidiEvent,
    decoder: MidiEvent,
}

impl MidiPort {
    pub fn open() -> Result<Self, alsa::Error> {
        let seq = Seq::open(None, None, true)?;
        seq.set_client_name(&CString::new(CLIENT_NAME).unwrap())?;

        // One bidirectional port: the control surface is read from it and LED
        // feedback is written to it. Both directions have to carry the same
        // name or Mixxx will not pair them.
        let surface_port = {
            let mut info = PortInfo::empty()?;
            info.set_name(&CString::new(PORT_NAME).unwrap());
            info.set_capability(
                PortCap::READ | PortCap::SUBS_READ | PortCap::WRITE | PortCap::SUBS_WRITE,
            );
            info.set_type(PortType::MIDI_GENERIC | PortType::APPLICATION);
            seq.create_port(&info)?;
            info.get_port()
        };

        let encoder = MidiEvent::new(256)?;
        // Expand running status, so consumers always see a complete message.
        encoder.enable_running_status(false);
        let decoder = MidiEvent::new(256)?;

        Ok(Self {
            seq,
            surface_port,
            encoder,
            decoder,
        })
    }

    pub fn client_id(&self) -> i32 {
        self.seq.client_id().unwrap_or(-1)
    }

    pub fn describe(&self) {
        println!("ALSA client {}: \"{CLIENT_NAME}\"", self.client_id());
        println!("  \"{PORT_NAME}\" - control surface out, LED feedback in (connect Mixxx here)");
    }

    /// Encode raw MIDI bytes from the device and publish them on the surface port.
    pub fn send_bytes(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            match self.encoder.encode(&bytes[offset..]) {
                Ok((used, Some(mut ev))) => {
                    offset += used.max(1);
                    ev.set_source(self.surface_port);
                    ev.set_subs();
                    ev.set_direct();
                    let _ = self.seq.event_output(&mut ev);
                    let _ = self.seq.drain_output();
                }
                // Consumed bytes but the message is not complete yet.
                Ok((used, None)) if used > 0 => offset += used,
                _ => break,
            }
        }
    }

    /// Drain events the host sent to the feedback port, returning raw MIDI bytes.
    pub fn recv_bytes(&mut self, out: &mut Vec<u8>) {
        let mut input = self.seq.input();
        while input.event_input_pending(true).unwrap_or(0) > 0 {
            let Ok(mut ev) = input.event_input() else {
                break;
            };
            if ev.get_type() == EventType::None {
                continue;
            }
            let mut buf = [0u8; 64];
            if let Ok(n) = self.decoder.decode(&mut buf, &mut ev) {
                out.extend_from_slice(&buf[..n]);
            }
        }
    }
}
