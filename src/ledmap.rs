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
    /// `0xB0` control change or `0x90` note on.
    pub kind: u8,
    pub number: u8,
}

impl Candidate {
    pub fn describe(&self) -> String {
        format!(
            "channel {} {} 0x{:02X} ({})",
            self.channel + 1,
            if self.kind == 0xB0 { "CC  " } else { "note" },
            self.number,
            self.number
        )
    }

    /// The three bytes to send to turn this on or off.
    pub fn message(&self, on: bool) -> [u8; 3] {
        [
            self.kind | (self.channel & 0x0F),
            self.number,
            if on { 0x7F } else { 0x00 },
        ]
    }
}

/// A message that turned out to light something, and what.
pub struct Found {
    pub channel: u8,
    pub kind: u8,
    pub number: u8,
    pub description: String,
}

pub struct LedWalk {
    candidates: Vec<Candidate>,
    index: usize,
    /// False until the first step, so that step lands on the first candidate
    /// rather than the second.
    started: bool,
    pub found: Vec<Found>,
}

impl LedWalk {
    /// Walk every `kind` on each of `channels`, kind-major then channel-major.
    ///
    /// Control change comes first because that is what the NS7 - same
    /// generation, same vendor - uses for its LEDs, so it is the likelier half
    /// of the space. Channel-major within that keeps a deck's lights together,
    /// which makes a run of them obvious as it goes past.
    pub fn new(channels: u8, kinds: &[u8], count: u8) -> Self {
        let mut candidates = Vec::new();
        for &kind in kinds {
            for channel in 0..channels {
                for number in 0..count {
                    candidates.push(Candidate {
                        channel,
                        kind,
                        number,
                    });
                }
            }
        }
        Self {
            candidates,
            index: 0,
            started: false,
            found: Vec::new(),
        }
    }

    pub fn current(&self) -> Option<Candidate> {
        self.candidates.get(self.index).copied()
    }

    pub fn position(&self) -> (usize, usize) {
        (self.index + 1, self.candidates.len())
    }

    pub fn advance(&mut self) -> Option<Candidate> {
        if self.started {
            self.index += 1;
        } else {
            self.started = true;
        }
        self.current()
    }

    /// Step back, for when a light is noticed one moment too late.
    pub fn back(&mut self) -> Option<Candidate> {
        self.started = true;
        self.index = self.index.saturating_sub(1);
        self.current()
    }

    /// Record a description for the candidate currently being shown. Recording
    /// the same one twice replaces the earlier description rather than adding a
    /// second entry.
    pub fn record(&mut self, description: &str) {
        if let Some(c) = self.current() {
            self.found
                .retain(|f| !(f.channel == c.channel && f.kind == c.kind && f.number == c.number));
            self.found.push(Found {
                channel: c.channel,
                kind: c.kind,
                number: c.number,
                description: description.to_string(),
            });
        }
    }

    pub fn to_toml(&self) -> String {
        let mut s = String::from(
            "# Numark NS6 LED map, recorded with `ns6 leds`.\n\
             # Send the message to the device to light what is described.\n",
        );
        for f in &self.found {
            let _ = write!(
                s,
                "\n[[led]]\ndescription = \"{}\"\nchannel = {}\nkind = \"{}\"\nnumber = {}  # 0x{:02X}\n",
                f.description.replace('\\', "\\\\").replace('"', "\\\""),
                f.channel,
                if f.kind == 0xB0 { "cc" } else { "note" },
                f.number,
                f.number
            );
        }
        s
    }
}

impl LedWalk {
    /// What this candidate was already recorded as, if anything. Shown while
    /// stepping so a second pass over the same ground is obvious.
    pub fn description_of(&self, c: Candidate) -> Option<&str> {
        self.found
            .iter()
            .find(|f| f.channel == c.channel && f.kind == c.kind && f.number == c.number)
            .map(|f| f.description.as_str())
    }
}
