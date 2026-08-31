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

/// Records the surface the other way round from a checklist: you move a
/// control, it shows what arrived, and you say what it was.
///
/// Two things keep the batches clean. Everything the device announces at
/// startup is claimed as a baseline, so a control that has only reported its
/// resting position is never mistaken for one you moved. And anything that
/// chatters while nothing is being touched - the platter sensors do - is marked
/// noisy and kept out of a batch unless it is the thing that dominated it.
pub struct Recorder {
    claimed: BTreeMap<Key, usize>,
    baseline: BTreeMap<Key, u64>,
    noisy: std::collections::BTreeSet<Key>,
    pending: BTreeMap<Key, Control>,
    settled: Option<Instant>,
    /// Set while the user is typing a name; incoming messages are ignored so
    /// that idle chatter cannot join the batch being named.
    pub naming: bool,
    pub entries: Vec<Entry>,
}

/// One named physical control and the messages it emits.
pub struct Entry {
    pub description: String,
    pub keys: Vec<Key>,
    /// What the control looked like when it was recorded, for the report.
    pub notes: Vec<String>,
}

/// Messages a control must produce before it is taken seriously.
const BUTTON_EVENTS: u64 = 1;
const CONTINUOUS_EVENTS: u64 = 4;
/// How long everything must stay quiet before the batch is offered for naming,
/// so a fader sweep is captured whole rather than cut part way along.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(600);
/// A chattering control joins a batch only if it accounts for at least this
/// fraction of the busiest control's messages.
const NOISE_FLOOR: u64 = 2;

impl Recorder {
    /// `surface` must already hold the device's startup announcement.
    pub fn new(surface: &Surface) -> Self {
        Self {
            claimed: BTreeMap::new(),
            baseline: surface.controls.iter().map(|(k, c)| (*k, c.count)).collect(),
            // More than a couple of messages before anyone touched anything
            // means this control talks on its own.
            noisy: surface
                .controls
                .iter()
                .filter(|(_, c)| c.count > 3)
                .map(|(k, _)| *k)
                .collect(),
            pending: BTreeMap::new(),
            settled: None,
            naming: false,
            entries: Vec::new(),
        }
    }

    /// Note activity on a control.
    pub fn observe(&mut self, c: &Control) {
        if self.naming {
            return;
        }
        let key = (c.channel, c.kind, c.number);
        if self.claimed.contains_key(&key) {
            return;
        }
        let needed = if c.kind == Kind::Note {
            BUTTON_EVENTS
        } else {
            CONTINUOUS_EVENTS
        };
        // Only count what this control has done since it announced itself.
        let since = c.count - self.baseline.get(&key).copied().unwrap_or(0).min(c.count);
        if since < needed {
            return;
        }
        self.pending.insert(key, c.clone());
        self.settled = Some(Instant::now());
    }

    /// Call regularly. Returns a description of the batch once movement has
    /// stopped, at which point the caller should ask for a name.
    pub fn poll(&mut self) -> Option<String> {
        if self.naming || self.pending.is_empty() {
            return None;
        }
        if self.settled?.elapsed() < SETTLE {
            return None;
        }
        self.drop_chatter();
        if self.pending.is_empty() {
            self.settled = None;
            return None;
        }
        self.naming = true;
        Some(self.summary())
    }

    /// Remove controls that only chattered alongside whatever was really moved.
    fn drop_chatter(&mut self) {
        let loudest = self
            .pending
            .values()
            .map(|c| self.moved(c))
            .max()
            .unwrap_or(0);
        let noisy = self.noisy.clone();
        let baseline = self.baseline.clone();
        self.pending.retain(|k, c| {
            if !noisy.contains(k) {
                return true;
            }
            let moved = c.count - baseline.get(k).copied().unwrap_or(0).min(c.count);
            moved * NOISE_FLOOR >= loudest
        });
    }

    fn moved(&self, c: &Control) -> u64 {
        let key = (c.channel, c.kind, c.number);
        c.count - self.baseline.get(&key).copied().unwrap_or(0).min(c.count)
    }

    fn summary(&self) -> String {
        let mut lines = Vec::new();
        for c in self.pending.values() {
            lines.push(format!(
                "    ch{} {:<4} {:>3}   value {:>3}   range {}..{}   {} msgs   {}",
                c.channel,
                if c.kind == Kind::Cc { "CC" } else { "note" },
                c.number,
                c.last,
                c.min,
                c.max,
                self.moved(c),
                shape(c)
            ));
        }
        lines.join("\n")
    }

    /// Accept the batch under `description`.
    pub fn name(&mut self, description: &str) {
        let index = self.entries.len();
        let batch = std::mem::take(&mut self.pending);
        let mut keys = Vec::new();
        let mut notes = Vec::new();
        for (key, c) in batch {
            self.claimed.insert(key, index);
            notes.push(format!(
                "ch{} {} {} ({}..{}, {})",
                c.channel,
                if c.kind == Kind::Cc { "CC" } else { "note" },
                c.number,
                c.min,
                c.max,
                shape(&c)
            ));
            keys.push(key);
        }
        self.entries.push(Entry {
            description: description.to_string(),
            keys,
            notes,
        });
        self.settled = None;
        self.naming = false;
    }

    /// Throw the batch away without claiming anything, so it can be retried.
    pub fn discard(&mut self) {
        self.pending.clear();
        self.settled = None;
        self.naming = false;
    }

    /// The recorded surface, as TOML.
    pub fn to_toml(&self) -> String {
        let mut s = String::from(
            "# Numark NS6 control surface, recorded with `ns6 map`.\n\
             # Each entry is one physical control and the MIDI it emits.\n",
        );
        for e in &self.entries {
            let _ = write!(s, "\n[[control]]\ndescription = \"{}\"\n", escape(&e.description));
            let _ = writeln!(s, "# {}", e.notes.join(", "));
            let _ = writeln!(s, "messages = [");
            for (ch, kind, num) in &e.keys {
                let k = if *kind == Kind::Cc { "cc" } else { "note" };
                let _ = writeln!(
                    s,
                    "  {{ channel = {ch}, kind = \"{k}\", number = {num} }},"
                );
            }
            let _ = writeln!(s, "]");
        }
        s
    }
}

impl Recorder {
    /// Re-read a surface written by an earlier run, so a second session adds to
    /// it instead of starting over.
    ///
    /// This matters more than it looks. Recording the panel takes a while, and
    /// what is left at the end is always a handful of stragglers - the controls
    /// that were missed, or that turned out to need a hardware switch set
    /// somewhere else first. Without this, collecting those six meant naming the
    /// other seventy again.
    ///
    /// Every message of a carried-over control is claimed, so those controls are
    /// not offered a second time: move one and nothing happens, which is the
    /// point. Only what is not yet in the file gets asked about. Unparseable or
    /// missing files are ignored, since the alternative is refusing to record
    /// anything because of a typo in a comment.
    pub fn load(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let entries = parse_surface(&text);
        if entries.is_empty() {
            return;
        }
        for (index, e) in entries.iter().enumerate() {
            for key in &e.keys {
                self.claimed.insert(*key, index);
            }
        }
        let messages: usize = entries.iter().map(|e| e.keys.len()).sum();
        println!(
            "carried over {} already-named controls ({messages} messages); they \
             will not be offered again",
            entries.len()
        );
        self.entries = entries;
    }
}

/// Parse a surface written by [`Recorder::to_toml`].
///
/// Lenient by design: anything it cannot make sense of is skipped rather than
/// refused, because the alternative is declining to record a panel over a typo
/// in a comment. The notes line is carried through verbatim rather than
/// re-parsed - `to_toml` joins notes with ", " and the shapes inside contain
/// commas of their own, so keeping it whole is what round-trips.
fn parse_surface(text: &str) -> Vec<Entry> {
    let mut description: Option<String> = None;
    let mut notes: Option<String> = None;
    let mut keys: Vec<Key> = Vec::new();
    let mut entries = Vec::new();

    fn flush(
        description: &mut Option<String>,
        notes: &mut Option<String>,
        keys: &mut Vec<Key>,
        entries: &mut Vec<Entry>,
    ) {
        match (description.take(), keys.is_empty()) {
            (Some(d), false) => entries.push(Entry {
                description: d,
                keys: std::mem::take(keys),
                notes: notes.take().into_iter().collect(),
            }),
            _ => {
                keys.clear();
                *notes = None;
            }
        }
    }

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[[control]]") {
            flush(&mut description, &mut notes, &mut keys, &mut entries);
        } else if let Some(rest) = line.strip_prefix("description") {
            description = rest.split_once('=').map(|(_, v)| unescape(v));
        } else if line.starts_with('#') && description.is_some() && notes.is_none() {
            notes = Some(line.trim_start_matches('#').trim().to_string());
        } else if line.starts_with('{') {
            if let Some(key) = parse_message(line) {
                keys.push(key);
            }
        }
    }
    flush(&mut description, &mut notes, &mut keys, &mut entries);
    entries
}

/// One `{ channel = 0, kind = "cc", number = 20 }` line.
fn parse_message(line: &str) -> Option<Key> {
    let body = line.trim_start_matches('{').trim_end_matches(&[',', '}'][..]);
    let (mut channel, mut kind, mut number) = (None, None, None);
    for field in body.trim_end_matches('}').split(',') {
        let (name, value) = field.split_once('=')?;
        match name.trim() {
            "channel" => channel = value.trim().parse::<u8>().ok(),
            "number" => number = value.trim().parse::<u8>().ok(),
            "kind" => {
                kind = Some(if value.contains("cc") {
                    Kind::Cc
                } else {
                    Kind::Note
                })
            }
            _ => {}
        }
    }
    Some((channel?, kind?, number?))
}

pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Undo [`escape`], and strip the surrounding quotes.
///
/// Every one of these maps is written by the tool and read back by it, so a
/// description that survives the trip only if it contains no quotation marks is
/// a bug waiting for the first person to write `deck a "play"`. Trimming the
/// quotes is not enough on its own.
pub fn unescape(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A button that has been pressed a few times.
    fn pressed(channel: u8, number: u8) -> Control {
        Control {
            channel,
            kind: Kind::Note,
            number,
            min: 0,
            max: 127,
            last: 127,
            count: 8,
            last_seen: Instant::now(),
        }
    }

    /// A recording that has been written out and read back must be unchanged,
    /// because that file is the only record of a session with the hardware.
    #[test]
    fn a_written_surface_reads_back_identically() {
        let original = "\
# Numark NS6 control surface, recorded with `ns6 map`.
# Each entry is one physical control and the MIDI it emits.

[[control]]
description = \"crossfader\"
# ch0 CC 7 (0..127, fader/knob (full travel)), ch0 CC 39 (0..127, fader/knob (full travel))
messages = [
  { channel = 0, kind = \"cc\", number = 7 },
  { channel = 0, kind = \"cc\", number = 39 },
]

[[control]]
description = \"deck a play\"
# ch1 note 17 (0..127, button)
messages = [
  { channel = 1, kind = \"note\", number = 17 },
]
";
        let entries = parse_surface(original);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].description, "crossfader");
        assert_eq!(entries[0].keys.len(), 2);
        assert_eq!(entries[1].keys, vec![(1, Kind::Note, 17)]);

        let mut r = Recorder::new(&Surface::new());
        r.entries = entries;
        assert_eq!(r.to_toml(), original);
    }

    /// The point of loading: a control already in the file is not offered a
    /// second time, however much it is moved.
    #[test]
    fn a_carried_over_control_is_not_offered_again() {
        let mut r = Recorder::new(&Surface::new());
        let entries = parse_surface(
            "[[control]]\ndescription = \"deck a play\"\nmessages = [\n  \
             { channel = 1, kind = \"note\", number = 17 },\n]\n",
        );
        for (index, e) in entries.iter().enumerate() {
            for key in &e.keys {
                r.claimed.insert(*key, index);
            }
        }

        r.observe(&pressed(1, 17));
        assert!(r.poll().is_none(), "a claimed control was offered for naming");

        // ... while something not in the file still is.
        r.observe(&pressed(0, 0x45));
        r.settled = Some(Instant::now() - SETTLE - std::time::Duration::from_millis(1));
        assert!(r.poll().is_some(), "an unrecorded control was not offered");
    }
}
