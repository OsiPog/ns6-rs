//! Just enough terminal handling to read arrow keys.
//!
//! Stepping through a few hundred candidate messages wants single keypresses,
//! not lines: press right, look at the panel, press right again. That needs the
//! terminal out of canonical mode, which is all this does.

use std::io::Read;

/// A keypress, reduced to what the walker cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Right,
    Left,
    Enter,
    Char(char),
    /// Something we do not act on, reported so the caller can ignore it.
    Other,
}

/// Puts the terminal in raw mode and restores it when dropped.
///
/// Restoring matters more than usual here: leaving a terminal without echo
/// after a crash makes the shell look broken. `ISIG` stays on so Ctrl-C still
/// signals rather than arriving as a byte.
pub struct RawMode {
    saved: libc::termios,
    fd: i32,
}

impl RawMode {
    pub fn enable() -> Option<Self> {
        let fd = 0; // stdin
        unsafe {
            if libc::isatty(fd) != 1 {
                return None;
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            // Non-blocking: a read returns immediately with whatever is there.
            // The caller polls, so that holding a key can be rate-limited
            // instead of arriving as fast as the terminal repeats it.
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self { saved, fd })
        }
    }

    /// Return to canonical mode for as long as the returned guard lives, so a
    /// description can be typed with the shell's own line editing.
    pub fn cooked(&self) -> Cooked<'_> {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
        Cooked { owner: self }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

pub struct Cooked<'a> {
    owner: &'a RawMode,
}

impl Drop for Cooked<'_> {
    fn drop(&mut self) {
        unsafe {
            let mut raw = self.owner.saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(self.owner.fd, libc::TCSANOW, &raw);
        }
    }
}

/// Read a keypress if one is waiting, decoding the arrow-key escape sequences.
///
/// Returns `None` when nothing is pending. Arrows arrive as `ESC [ C` and
/// `ESC [ D`; the bytes after the ESC can lag it by a moment, so this waits
/// briefly for them rather than reporting a bare escape.
pub fn poll_key() -> Option<Key> {
    let first = read_byte()?;
    match first {
        b'\r' | b'\n' => Some(Key::Enter),
        0x1B => {
            let bracket = read_byte_waiting()?;
            if bracket != b'[' {
                return Some(Key::Other);
            }
            match read_byte_waiting()? {
                b'C' => Some(Key::Right),
                b'D' => Some(Key::Left),
                _ => Some(Key::Other),
            }
        }
        c if c.is_ascii_graphic() || c == b' ' => Some(Key::Char(c as char)),
        _ => Some(Key::Other),
    }
}

fn read_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    match std::io::stdin().read(&mut b) {
        Ok(1) => Some(b[0]),
        _ => None,
    }
}

/// The rest of an escape sequence, which may not have landed yet.
fn read_byte_waiting() -> Option<u8> {
    for _ in 0..40 {
        if let Some(b) = read_byte() {
            return Some(b);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    None
}

/// Drain everything pending, returning the last arrow seen and whether Enter or
/// a quit was pressed.
///
/// Coalescing is the point: holding an arrow key produces terminal autorepeat
/// far faster than the walk should move, and without collapsing a burst into
/// one step the backlog would keep scrolling after the key was released.
pub struct Pending {
    pub arrow: Option<Key>,
    pub enter: bool,
    pub chars: Vec<char>,
}

pub fn drain() -> Pending {
    let mut p = Pending {
        arrow: None,
        enter: false,
        chars: Vec::new(),
    };
    while let Some(k) = poll_key() {
        match k {
            Key::Right | Key::Left => p.arrow = Some(k),
            Key::Enter => p.enter = true,
            Key::Char(c) => p.chars.push(c),
            Key::Other => {}
        }
    }
    p
}
