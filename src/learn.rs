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

/// A control's identity on the wire: channel, message kind, and number.
pub type Key = (u8, Kind, u8);

/// Walks the checklist, asking for one control at a time.
///
/// A control counts as identified once it has produced enough activity to be
/// unambiguous - a couple of messages for a button, a handful for a fader - and
/// has not already been claimed by an earlier answer. Everything the device
/// announced at startup is claimed up front, so a control that has merely
/// reported its resting position is never mistaken for one the user moved.
pub struct Guided {
    index: usize,
    claimed: BTreeMap<Key, &'static str>,
    baseline: std::collections::BTreeSet<Key>,
    pub answers: Vec<(&'static str, Vec<Key>)>,
    pending: Vec<Key>,
    settled: Option<Instant>,
}

/// Messages needed before a control is taken seriously. Buttons send one event
/// per press, continuous controls send a stream.
const BUTTON_EVENTS: u64 = 1;
const CONTINUOUS_EVENTS: u64 = 6;
/// How long a control must stay quiet before the answer is accepted, so that a
/// fader sweep is recorded as one control rather than truncated mid-move.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(700);

impl Guided {
    pub fn new(surface: &Surface) -> Self {
        Self {
            index: 0,
            claimed: BTreeMap::new(),
            baseline: surface.controls.keys().copied().collect(),
            answers: Vec::new(),
            pending: Vec::new(),
            settled: None,
        }
    }

    pub fn current(&self) -> Option<&'static crate::checklist::Item> {
        crate::checklist::ITEMS.get(self.index)
    }

    pub fn prompt(&self) {
        match self.current() {
            Some(item) => println!(
                "\n[{}/{}] Move: {}",
                self.index + 1,
                crate::checklist::ITEMS.len(),
                item.prompt
            ),
            None => println!("\nChecklist finished."),
        }
    }

    /// Note activity on a control. Returns true when the current item is done.
    pub fn observe(&mut self, c: &Control) -> bool {
        let key = (c.channel, c.kind, c.number);
        if self.claimed.contains_key(&key) {
            return false;
        }
        let needed = if c.kind == Kind::Note {
            BUTTON_EVENTS
        } else {
            CONTINUOUS_EVENTS
        };
        // A control seen at startup has already reported its resting value, so
        // only count what it has done since.
        let floor = u64::from(self.baseline.contains(&key));
        if c.count.saturating_sub(floor) < needed {
            return false;
        }
        if !self.pending.contains(&key) {
            self.pending.push(key);
        }
        self.settled = Some(Instant::now());
        false
    }

    /// Call regularly; accepts the answer once the user has stopped moving.
    pub fn poll(&mut self) -> bool {
        let Some(at) = self.settled else { return false };
        if at.elapsed() < SETTLE || self.pending.is_empty() {
            return false;
        }
        self.accept()
    }

    fn accept(&mut self) -> bool {
        let Some(item) = self.current() else {
            return false;
        };
        let keys = std::mem::take(&mut self.pending);
        for k in &keys {
            self.claimed.insert(*k, item.id);
        }
        println!("  -> {} = {}", item.id, describe_keys(&keys));
        self.answers.push((item.id, keys));
        self.index += 1;
        self.settled = None;
        true
    }

    /// Move on without recording anything - the control does not exist on this
    /// unit, or is analogue-only and sends no MIDI.
    pub fn skip(&mut self) -> bool {
        if let Some(item) = self.current() {
            println!("  -> {} skipped", item.id);
            self.answers.push((item.id, Vec::new()));
        }
        self.pending.clear();
        self.settled = None;
        self.index += 1;
        self.index < crate::checklist::ITEMS.len()
    }

    pub fn done(&self) -> bool {
        self.index >= crate::checklist::ITEMS.len()
    }

    /// The whole result, as TOML the mapping generator reads.
    pub fn to_toml(&self) -> String {
        let mut s = String::from(
            "# Numark NS6 control surface, recorded by `ns6 learn --guided`.\n\
             # Each entry lists the MIDI messages one physical control emits.\n\n",
        );
        for (id, keys) in &self.answers {
            let _ = writeln!(s, "[\"{id}\"]");
            if keys.is_empty() {
                let _ = writeln!(s, "messages = []  # sends nothing\n");
                continue;
            }
            let _ = writeln!(s, "messages = [");
            for (ch, kind, num) in keys {
                let k = if *kind == Kind::Cc { "cc" } else { "note" };
                let _ = writeln!(
                    s,
                    "  {{ channel = {ch}, kind = \"{k}\", number = {num} }},"
                );
            }
            let _ = writeln!(s, "]\n");
        }
        s
    }
}

fn describe_keys(keys: &[Key]) -> String {
    keys.iter()
        .map(|(ch, kind, n)| {
            let k = if *kind == Kind::Cc { "CC" } else { "note" };
            format!("ch{ch} {k}{n}")
        })
        .collect::<Vec<_>>()
        .join(" + ")
}
