//! Walking the device's LED space to find out what lights up.
//!
//! The input side of the NS6 could be recorded by moving a control and reading
//! what came out. The output side has no such shortcut: nothing in the vendor
//! driver knows which note lights which button, because the driver only
//! forwards whatever the host sends. Serato holds that table, not `ns6_usb.sys`.
//!
//! So this asks the hardware. It sends one note at a time and leaves it lit
//! until the next, walking on its own, and you interrupt when something comes
//! on. That way the several hundred numbers that do nothing cost no attention.

use std::fmt::Write as _;

/// One message to try.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub channel: u8,
    pub note: u8,
}

impl Candidate {
    pub fn describe(&self) -> String {
        format!(
            "channel {} note 0x{:02X} ({})",
            self.channel + 1,
            self.note,
            self.note
        )
    }
}

/// A note that turned out to light something, and what.
pub struct Found {
    pub channel: u8,
    pub note: u8,
    pub description: String,
}

pub struct LedWalk {
    candidates: Vec<Candidate>,
    index: usize,
    pub found: Vec<Found>,
    /// While paused the walk holds position so the light can be examined.
    pub paused: bool,
}

impl LedWalk {
    /// Walk `notes` on each of `channels`, channel-major.
    ///
    /// Channel-major matters: a deck's LEDs are all on one channel, so walking
    /// this way keeps related lights together and makes a run of them obvious.
    pub fn new(channels: u8, notes: u8) -> Self {
        let mut candidates = Vec::new();
        for channel in 0..channels {
            for note in 0..notes {
                candidates.push(Candidate { channel, note });
            }
        }
        Self {
            candidates,
            index: 0,
            found: Vec::new(),
            paused: false,
        }
    }

    pub fn current(&self) -> Option<Candidate> {
        self.candidates.get(self.index).copied()
    }

    pub fn position(&self) -> (usize, usize) {
        (self.index + 1, self.candidates.len())
    }

    pub fn advance(&mut self) -> Option<Candidate> {
        self.index += 1;
        self.current()
    }

    /// Step back, for when a light is noticed one moment too late.
    pub fn back(&mut self) -> Option<Candidate> {
        self.index = self.index.saturating_sub(1);
        self.current()
    }

    pub fn record(&mut self, description: &str) {
        if let Some(c) = self.current() {
            self.found.push(Found {
                channel: c.channel,
                note: c.note,
                description: description.to_string(),
            });
        }
    }

    pub fn done(&self) -> bool {
        self.index >= self.candidates.len()
    }

    pub fn to_toml(&self) -> String {
        let mut s = String::from(
            "# Numark NS6 LED map, recorded with `ns6 leds`.\n\
             # Send the note to the device to light what is described.\n",
        );
        for f in &self.found {
            let _ = write!(
                s,
                "\n[[led]]\ndescription = \"{}\"\nchannel = {}\nnote = {}  # 0x{:02X}\n",
                f.description.replace('\\', "\\\\").replace('"', "\\\""),
                f.channel,
                f.note,
                f.note
            );
        }
        s
    }
}
