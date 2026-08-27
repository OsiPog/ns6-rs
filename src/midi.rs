//! ALSA sequencer port, so the NS6 appears as an ordinary MIDI device.
//!
//! Mixxx uses PortMidi on Linux, which enumerates ALSA sequencer ports. The
//! surface port is created with `READ | SUBS_READ` so it shows up as an
//! available MIDI *input* for other applications to read from.

use std::ffi::CString;

use alsa::seq::{EventType, MidiEvent, PortCap, PortInfo, PortType, Seq};

pub const CLIENT_NAME: &str = "Numark NS6";
pub const PORT_NAME: &str = "Numark NS6 MIDI";
pub const PORT_NAME_OUT: &str = "Numark NS6 MIDI OUT";

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

        // Readable + subscribable: this is what Mixxx binds to.
        let surface_port = {
            let mut info = PortInfo::empty()?;
            info.set_name(&CString::new(PORT_NAME).unwrap());
            info.set_capability(PortCap::READ | PortCap::SUBS_READ);
            info.set_type(PortType::MIDI_GENERIC | PortType::APPLICATION);
            seq.create_port(&info)?;
            info.get_port()
        };

        // Writable: LED / display feedback from the host back to the controller.
        // Registering it is all that is needed; events arrive via subscription.
        {
            let mut info = PortInfo::empty()?;
            info.set_name(&CString::new(PORT_NAME_OUT).unwrap());
            info.set_capability(PortCap::WRITE | PortCap::SUBS_WRITE);
            info.set_type(PortType::MIDI_GENERIC | PortType::APPLICATION);
            seq.create_port(&info)?;
        }

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
        println!("  \"{PORT_NAME}\"     -> read the control surface here (connect Mixxx to this)");
        println!("  \"{PORT_NAME_OUT}\" <- write LED feedback here");
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
