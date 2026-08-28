//! Turn the device's MIDI stream into a control surface map.
//!
//! The NS6 announces the position of every fader, knob and switch as soon as it
//! starts streaming, so a single run enumerates the whole surface - but not what
//! any of it *is*. `ns6 learn` closes that gap: move one control, and it names
//! the messages that control emits and how they behave.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Instant;

/// One (channel, kind, number) triple and everything observed about it.
#[derive(Debug, Clone)]
pub struct Control {
    pub channel: u8,
    pub kind: Kind,
    pub number: u8,
    pub min: u8,
    pub max: u8,
    pub last: u8,
    pub count: u64,
    pub last_seen: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Cc,
    Note,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Cc => "CC  ",
            Kind::Note => "note",
        }
    }
}

/// A running MIDI parser plus the table it fills in.
#[derive(Default)]
pub struct Surface {
    status: Option<u8>,
    pending: Vec<u8>,
    controls: BTreeMap<(u8, Kind, u8), Control>,
}

impl Surface {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw MIDI bytes; returns the controls that changed.
    ///
    /// Running status is honoured, because the device relies on it: a 42-byte
    /// packet can end mid-message and the next one continues the byte stream.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Control> {
        let now = Instant::now();
        let mut changed = Vec::new();

        for &b in bytes {
            if b >= 0xF8 {
                continue; // real-time messages are interleavable and carry no control
            }
            if b & 0x80 != 0 {
                self.status = Some(b);
                self.pending.clear();
                continue;
            }
            let Some(status) = self.status else { continue };
            self.pending.push(b);

            let wanted = match status & 0xF0 {
                0xC0 | 0xD0 => 1,
                _ => 2,
            };
            if self.pending.len() < wanted {
                continue;
            }

            let (data1, data2) = (self.pending[0], *self.pending.last().unwrap());
            self.pending.clear();

            let kind = match status & 0xF0 {
                0xB0 => Kind::Cc,
                0x90 | 0x80 => Kind::Note,
                _ => continue,
            };
            let channel = status & 0x0F;
            let value = if kind == Kind::Note && status & 0xF0 == 0x80 {
                0
            } else {
                data2
            };

            let key = (channel, kind, data1);
            let entry = self.controls.entry(key).or_insert_with(|| Control {
                channel,
                kind,
                number: data1,
                min: value,
                max: value,
                last: value,
                count: 0,
                last_seen: now,
            });
            entry.min = entry.min.min(value);
            entry.max = entry.max.max(value);
            entry.last = value;
            entry.count += 1;
            entry.last_seen = now;
            changed.push(entry.clone());
        }
        changed
    }

    /// The whole table, as a report.
    pub fn report(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "{} controls seen\n", self.controls.len());
        let _ = writeln!(
            s,
            "ch  kind  num   hex    min   max  last      msgs  shape"
        );
        for c in self.controls.values() {
            let _ = writeln!(
                s,
                "{:<3} {:<4} {:>3} 0x{:02X}  {:>5} {:>5} {:>5}  {:>8}  {}",
                c.channel,
                c.kind.label().trim(),
                c.number,
                c.number,
                c.min,
                c.max,
                c.last,
                c.count,
                shape(c)
            );
        }
        s
    }

    /// Candidate 14-bit pairs: a controller at `n` whose LSB rides at `n + 32`.
    ///
    /// The NS6 sends its faders and jog wheels this way, so pairing them up is
    /// what turns a list of CC numbers into usable resolution.
    pub fn fourteen_bit_pairs(&self) -> Vec<(u8, u8)> {
        let mut pairs = Vec::new();
        for &(ch, kind, num) in self.controls.keys() {
            if kind != Kind::Cc || num >= 32 {
                continue;
            }
            if self.controls.contains_key(&(ch, Kind::Cc, num + 32)) {
                pairs.push((ch, num));
            }
        }
        pairs
    }
}

/// A one-word guess at what a control is, from how its values move.
fn shape(c: &Control) -> &'static str {
    match c.kind {
        Kind::Note => {
            if c.min == c.max {
                "button (held)"
            } else {
                "button"
            }
        }
        Kind::Cc => {
            if c.min == c.max {
                "static"
            } else if c.max - c.min <= 4 && c.count > 20 {
                "encoder/jog (relative)"
            } else if c.min == 0 && c.max >= 120 {
                "fader/knob (full travel)"
            } else {
                "continuous (partial travel)"
            }
        }
    }
}
