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

/// Messages that are known to take the device off the USB bus.
///
/// The MIDI OUT stream carries more than MIDI: the vendor driver bit-bangs a
/// serial register interface into an audio chip through it, clocking bits with
/// the byte patterns `addr | 0x00/0x40/0x80/0xC0/0xE0`. Some byte sequences
/// therefore reach hardware that has nothing to do with lighting buttons.
///
/// `(kind, channel, number)`. Found the hard way; the walk steps over them
/// rather than sending them, unless `NS6_LED_UNSAFE` is set.
pub const HAZARDS: &[(u8, u8, u8)] = &[
    // Confirmed twice: this one drops the device and needs a power cycle.
    (0xB0, 0, 57),
    // Found by the second LED walk, the same way: the walk reached it, the
    // device left the bus. Note it is a different number on a different
    // channel, so the two are not one register seen twice.
    (0xB0, 3, 59),
];

pub struct LedWalk {
    candidates: Vec<Candidate>,
    /// Messages found to drop the device, on top of the built-in [`HAZARDS`].
    /// Persisted with the map, so a resumed walk steps over them by itself.
    learned: Vec<(u8, u8, u8)>,
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
            learned: Vec::new(),
            index: 0,
            started: false,
            found: Vec::new(),
        }
    }

    pub fn current(&self) -> Option<Candidate> {
        self.candidates.get(self.index).copied()
    }

    /// Whether this candidate is known to be destructive.
    pub fn is_hazard(&self, c: Candidate) -> bool {
        let key = (c.kind, c.channel, c.number);
        HAZARDS.contains(&key) || self.learned.contains(&key)
    }

    /// Remember that this one took the device down, so a resume steps over it.
    pub fn mark_hazard(&mut self, c: Candidate) {
        let key = (c.kind, c.channel, c.number);
        if !self.learned.contains(&key) && !HAZARDS.contains(&key) {
            self.learned.push(key);
        }
    }

    /// Mark candidates by their displayed position, for `NS6_LED_SKIP`.
    pub fn mark_hazard_at(&mut self, position: usize) -> Option<Candidate> {
        let c = *self.candidates.get(position.checked_sub(1)?)?;
        self.mark_hazard(c);
        Some(c)
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
        for &(kind, channel, number) in &self.learned {
            let _ = write!(
                s,
                "\n[[hazard]]  # took the device off the bus\nchannel = {channel}\nkind = \"{}\"\nnumber = {number}  # 0x{number:02X}\n",
                if kind == 0xB0 { "cc" } else { "note" }
            );
        }
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

impl LedWalk {
    /// Jump to a position, for resuming a walk the device did not survive.
    pub fn seek(&mut self, index: usize) {
        self.index = index.min(self.candidates.len().saturating_sub(1));
        self.started = true;
    }

    /// Re-read a map written by an earlier run, so a resumed walk keeps what it
    /// already found and steps over what already broke it. Unparseable or
    /// missing files are ignored.
    pub fn load(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        // Section-aware, because hazards and described LEDs share field names
        // and only differ by which table they sit in.
        let mut in_hazard = false;
        let (mut description, mut channel, mut kind, mut number) = (None, None, None, None::<u8>);
        let field = |l: &str| l.split('=').nth(1).map(|v| v.trim().to_string());

        for line in text.lines() {
            let line = line.trim();
            if line.starts_with("[[") {
                in_hazard = line.starts_with("[[hazard]]");
                description = None;
                channel = None;
                kind = None;
                number = None;
                continue;
            }
            if line.starts_with("description") {
                description = field(line).map(|v| crate::learn::unescape(&v));
            } else if line.starts_with("channel") {
                channel = field(line).and_then(|v| v.parse::<u8>().ok());
            } else if line.starts_with("kind") {
                kind = field(line).map(|v| if v.contains("cc") { 0xB0u8 } else { 0x90 });
            } else if line.starts_with("number") {
                number = field(line)
                    .and_then(|v| v.split('#').next().map(str::trim).and_then(|n| n.parse().ok()));
            }

            match (in_hazard, &description, channel, kind, number) {
                (true, _, Some(c), Some(k), Some(n)) => {
                    if !self.learned.contains(&(k, c, n)) {
                        self.learned.push((k, c, n));
                    }
                    number = None;
                }
                (false, Some(d), Some(c), Some(k), Some(n)) => {
                    self.found.push(Found {
                        channel: c,
                        kind: k,
                        number: n,
                        description: d.clone(),
                    });
                    description = None;
                    number = None;
                }
                _ => {}
            }
        }
        if !self.learned.is_empty() {
            println!("carried over {} known-destructive messages", self.learned.len());
        }
        if !self.found.is_empty() {
            println!("carried over {} already-described LEDs", self.found.len());
        }
    }
}
