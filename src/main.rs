//! ns6 - userspace MIDI driver for the Numark NS6 DJ controller on Linux.
//!
//! The NS6 exposes only vendor-specific USB interfaces, so no kernel driver
//! binds it and no ALSA MIDI port appears. This talks the device's Ploytec
//! protocol over libusb and publishes an ALSA sequencer port instead.
//!
//! See `docs/PROTOCOL.md` for how the protocol was derived.

mod device;
mod iso;
mod ledmap;
mod learn;
mod midi;
mod protocol;
mod term;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use device::{Ns6, Stats};
use protocol as p;

fn usage() -> ! {
    eprintln!(
        "ns6 - userspace MIDI driver for the Numark NS6\n\
         \n\
         USAGE:\n    \
             ns6 [run]     bridge the controller to an ALSA MIDI port (default)\n    \
             ns6 led       light the controller's buttons, to check LED output\n    \
             ns6 leds      walk the LED space and record what each note lights\n    \
             ns6 jog       measure how many ticks a platter reports per turn\n    \
             ns6 probe     report device state and sweep bulk OUT configurations\n    \
             ns6 learn     watch the control surface: move a control, see its MIDI\n    \
             ns6 map       move a control, say what it was; writes ns6-surface.toml\n    \
             ns6 test      emit synthetic MIDI on the ALSA port, no hardware needed\n\
         \n\
         The device's usbfs node must be writable; see udev/70-numark-ns6.rules."
    );
    std::process::exit(2)
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let result = match arg.as_str() {
        "run" => cmd_run(Mode::Bridge),
        "learn" => cmd_run(Mode::Learn),
        "map" => cmd_run(Mode::Map),
        "led" => cmd_led(),
        "leds" => cmd_leds(),
        "jog" => cmd_jog(),
        "probe" => cmd_probe(),
        "test" => cmd_test(),
        "-h" | "--help" | "help" => usage(),
        _ => usage(),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Set false by SIGINT/SIGTERM so every thread can wind down cleanly.
static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_signal(_sig: i32) {
    RUNNING.store(false, Ordering::Relaxed);
}

// Two calls is not worth a dependency on the libc crate.
extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

/// Wind down on Ctrl-C rather than dying mid-transfer.
fn install_signal_handler() {
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    unsafe {
        signal(SIGINT, on_signal);
        signal(SIGTERM, on_signal);
    }
}

fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// How deep each pipe's queue is and how many packets each isochronous URB
/// carries. Defaults mirror `captures/ns6.pcap`; every field is overridable so
/// configurations can be swept against the hardware without a rebuild.
#[derive(Debug)]
struct Geometry {
    pcm_in_depth: usize,
    midi_in_depth: usize,
    iso_in_packets: usize,
    iso_in_xfers: usize,
    iso_out_packets: usize,
    iso_out_xfers: usize,
}

impl Geometry {
    fn from_env() -> Self {
        fn v(name: &str, default: usize) -> usize {
            std::env::var(name)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        }
        Self {
            pcm_in_depth: v("NS6_PCM_IN_DEPTH", 7),
            midi_in_depth: v("NS6_MIDI_IN_DEPTH", 5),
            iso_in_packets: v("NS6_ISO_IN_PACKETS", 5),
            iso_in_xfers: v("NS6_ISO_IN_XFERS", 9),
            iso_out_packets: v("NS6_ISO_OUT_PACKETS", 40),
            iso_out_xfers: v("NS6_ISO_OUT_XFERS", 3),
        }
    }
}

/// What to do with the MIDI the device sends, on top of bridging it to ALSA.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Just bridge.
    Bridge,
    /// Also decode and report every control as it moves.
    Learn,
    /// Also record each control as it is moved and ask what it was.
    Map,
}

/// Bring the device up and bridge it to ALSA.
fn cmd_run(mode: Mode) -> Result<(), Box<dyn std::error::Error>> {
    let learn = mode != Mode::Bridge;
    let mut port = midi::MidiPort::open()?;
    port.describe();

    let dev = Arc::new(Ns6::open()?);

    // Re-run the init until the device starts sending, the way the vendor
    // driver does. A usbmon capture of Windows shows it repeating the full
    // sequence roughly once a second - 154 times in 150 seconds - which is not
    // a VM artefact but the driver retrying until the device latches. We only
    // ever ran it once.
    dev.start()?;

    let stats = Arc::new(Stats::default());
    // Threads watch this; main mirrors the global signal flag into it.
    let alive = Arc::new(AtomicBool::new(true));
    install_signal_handler();

    // Submission order, queue depths and isochronous packet counts, all taken
    // from `captures/ns6.pcap` - a capture of the driver on real Windows while
    // MIDI was actually flowing. (The later `windows.usbmon` capture from the
    // VM is *not* a working reference: in it endpoint 0x83 was cancelled 292
    // times without ever delivering a byte, so the VM's Windows driver was
    // failing in exactly the way ours does.)
    //
    // The working driver posts, within 600us of the last control transfer:
    //   7 x bulk IN  0x86, 131072 bytes each
    //   9 x iso  IN  0x81,  5 packets each  (bInterval 4 -> 1ms per packet)
    //   3 x iso  OUT 0x02, 40 packets each  (bInterval 1 -> 125us per packet)
    //   5 x bulk IN  0x83,  42 bytes each
    // and the device starts streaming ~7ms later.
    let g = Geometry::from_env();
    let _audio_in = unsafe {
        iso::BulkInStream::start(
            dev.raw_handle(),
            p::EP_PCM_IN,
            p::AUDIO_IN_XFER,
            g.pcm_in_depth,
            false,
        )
    }
    .map_err(|e| format!("audio in queue: libusb error {e}"))?;
    let _iso_in = unsafe {
        iso::IsoStream::start(
            dev.raw_handle(),
            p::EP_ISO_IN,
            p::ISO_IN_PACKET,
            g.iso_in_packets,
            g.iso_in_xfers,
            None,
        )
    }
    .map_err(|e| format!("iso IN stream: libusb error {e}"))?;
    let _iso_out = unsafe {
        iso::IsoStream::start(
            dev.raw_handle(),
            p::EP_ISO_OUT,
            p::ISO_OUT_PACKET,
            g.iso_out_packets,
            g.iso_out_xfers,
            Some((p::OUT_FRAME_BYTES, p::SAMPLE_RATE as u64)),
        )
    }
    .map_err(|e| format!("iso OUT stream: libusb error {e}"))?;
    let _midi_in = unsafe {
        iso::BulkInStream::start(
            dev.raw_handle(),
            p::EP_MIDI_IN,
            p::BLOCK,
            g.midi_in_depth,
            true,
        )
    }
    .map_err(|e| format!("MIDI in queue: libusb error {e}"))?;
    println!("geometry: {g:?}");
    println!("streams up: 0x86, 0x81, 0x02, 0x83");

    // Drives the isochronous and bulk completion callbacks.
    let pump = thread::spawn({
        let alive = alive.clone();
        move || {
            while alive.load(Ordering::Relaxed) {
                iso::pump_events();
            }
        }
    });

    // MIDI destined for the controller (LEDs), consumed by the PCM out thread.
    let (midi_tx, midi_rx) = mpsc::channel::<u8>();

    let mut threads = Vec::new();
    threads.push(thread::spawn({
        let (dev, stats, running) = (dev.clone(), stats.clone(), alive.clone());
        move || device::run_midi_out(dev, stats, running, midi_rx)
    }));
    println!(
        "\nbridge running - connect Mixxx to \"{}\". Ctrl-C to stop.\n",
        midi::CLIENT_NAME
    );

    let mut feedback = Vec::new();
    let mut gone = false;
    let mut last_beat = Instant::now();
    let mut warned = false;
    let mut surface = learn::Surface::new();
    let mut last_named: Option<learn::Key> = None;
    let mut quiet_since = Instant::now();
    let mut recorder: Option<learn::Recorder> = None;
    // Typed lines, read off a thread so the stream is never blocked on stdin.
    let (key_tx, key_rx) = mpsc::channel::<String>();

    if mode == Mode::Learn {
        println!(
            "learn mode: move ONE control, then wait a moment. Everything the device\n\
             announces at startup is listed first; after that each line is a control\n\
             you just moved. Ctrl-C prints the full table.\n"
        );
    }
    if mode == Mode::Map {
        // Two things have to be absorbed before recording. The device announces
        // every control's resting position when it starts streaming - though
        // only once per power-on, so whoever streamed first may already have
        // taken it - and the platter sensors chatter continuously. Watching a
        // few seconds of an untouched panel covers both.
        println!("calibrating - don't touch anything for a moment...");
        let settle = Instant::now();
        while running() && settle.elapsed() < Duration::from_secs(4) {
            if let Ok(mut q) = iso::MIDI_IN.lock() {
                if !q.is_empty() {
                    surface.feed(&q);
                    q.clear();
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        let seen = surface.report();
        println!(
            "{}\n\n\
             Move a control, then tell me what it was. Repeat for as many as you\n\
             like; Enter alone throws a reading away so you can try again, and\n\
             `q` finishes and writes {SURFACE_MAP}. Anything already in that\n\
             file is carried over and not asked about again, so a second run\n\
             only picks up what is still missing.\n\n\
             Ready - move something.",
            seen.lines().next().unwrap_or("0 controls seen")
        );
        let mut r = learn::Recorder::new(&surface);
        r.load(std::path::Path::new(SURFACE_MAP));
        recorder = Some(r);
        thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                if key_tx.send(line.trim_end().to_string()).is_err() {
                    break;
                }
                line.clear();
            }
        });
    }

    while running() {
        if iso::DEVICE_GONE.load(Ordering::Relaxed) {
            eprintln!(
                "\nthe device has gone off the USB bus - power cycled, unplugged, or\n\
                 switched off. Exiting so it can be started again against the new\n\
                 handle; the old one is dead and would silently deliver nothing."
            );
            gone = true;
            break;
        }

        // Surface events from the device -> ALSA.
        if let Ok(mut q) = iso::MIDI_IN.lock() {
            if !q.is_empty() {
                if learn {
                    for c in surface.feed(&q) {
                        if let Some(r) = recorder.as_mut() {
                            r.observe(&c);
                            continue;
                        }
                        let key = (c.channel, c.kind, c.number);
                        // One line per control per burst, not per message.
                        if last_named != Some(key) || quiet_since.elapsed() > Duration::from_millis(400) {
                            last_named = Some(key);
                            quiet_since = Instant::now();
                            println!(
                                "  ch{} {} {:>3} (0x{:02X})  value {:>3}  range {}..{}  x{}",
                                c.channel,
                                if c.kind == learn::Kind::Cc { "CC" } else { "note" },
                                c.number,
                                c.number,
                                c.last,
                                c.min,
                                c.max,
                                c.count
                            );
                        } else {
                            quiet_since = Instant::now();
                        }
                    }
                }
                port.send_bytes(&q);
                q.clear();
            }
        }

        // LED feedback from the host -> device.
        feedback.clear();
        port.recv_bytes(&mut feedback);
        for byte in feedback.iter().copied() {
            let _ = midi_tx.send(byte);
        }

        if let Some(r) = recorder.as_mut() {
            if let Some(summary) = r.poll() {
                println!("\n  got:\n{summary}");
                print!("  what was that? ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            match key_rx.try_recv() {
                Ok(line) if line.eq_ignore_ascii_case("q") => break,
                Ok(line) if !r.naming => {
                    // Typed before anything was moved; just re-prompt.
                    if !line.is_empty() {
                        println!("  (move a control first)");
                    }
                }
                Ok(line) if line.trim().is_empty() => {
                    r.discard();
                    println!("  discarded - move it again.");
                }
                Ok(line) => {
                    r.name(line.trim());
                    println!("  recorded #{}: {}\n", r.entries.len(), line.trim());
                }
                Err(_) => {}
            }
        }

        thread::sleep(Duration::from_millis(2));

        if recorder.is_none() && last_beat.elapsed() >= Duration::from_secs(5) {
            last_beat = Instant::now();
            let ok = stats.out_ok.load(Ordering::Relaxed);
            let err = stats.out_err.load(Ordering::Relaxed);
            println!(
                "  midi-out[ok:{ok} err:{err}]  iso-out:{}  iso-in:{}/{}B  bulk-in:{}B(err {} st {})  midi-in:{} bytes  midi-out:{} bytes",
                iso::ISO_OUT_OK.load(Ordering::Relaxed),
                iso::ISO_IN_OK.load(Ordering::Relaxed),
                iso::ISO_IN_DATA.load(Ordering::Relaxed),
                iso::BULK_IN_BYTES.load(Ordering::Relaxed),
                iso::BULK_IN_ERR.load(Ordering::Relaxed),
                iso::LAST_BULK_STATUS.load(Ordering::Relaxed),
                iso::MIDI_IN_BYTES.load(Ordering::Relaxed),
                stats.midi_out_bytes.load(Ordering::Relaxed),
            );
            if !warned && ok == 0 && err > 0 {
                warned = true;
                eprintln!(
                    "  note: the device is refusing every PCM OUT transfer, so it is not\n  \
                     streaming and will not report its surface. Run `ns6 probe` to sweep\n  \
                     transfer sizes and framings."
                );
            }
        }
    }

    if let Some(r) = recorder.as_ref() {
        let path = std::path::Path::new(SURFACE_MAP);
        match std::fs::write(path, r.to_toml()) {
            Ok(()) => println!("\nwrote {} controls to {}", r.entries.len(), path.display()),
            Err(e) => eprintln!("\ncould not write {}: {e}", path.display()),
        }
    }
    if learn {
        println!("\n{}", surface.report());
        let pairs = surface.fourteen_bit_pairs();
        if !pairs.is_empty() {
            println!("14-bit pairs (MSB at n, LSB at n+32):");
            for (ch, n) in pairs {
                println!("  ch{ch} CC{n} + CC{}", n + 32);
            }
        }
    }

    println!("\nshutting down");
    if gone {
        // Non-zero so systemd's Restart=on-failure picks the device up again.
        std::process::exit(1);
    }
    alive.store(false, Ordering::Relaxed);
    iso::ISO_RUNNING.store(false, Ordering::Relaxed);
    let _ = pump.join();
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

/// Report device state, then sweep bulk OUT configurations looking for one the
/// device accepts. A completed OUT transfer is the success signal, and unlike
/// isochronous counters it can actually fail.
fn cmd_probe() -> Result<(), Box<dyn std::error::Error>> {
    let dev = Ns6::open()?;

    let fw = dev.firmware()?;
    println!(
        "firmware : {}",
        fw.iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("status   : 0x{:02X}", dev.status()?);
    println!("sample rate:");
    dev.set_sample_rate_verbose(
        std::env::var("NS6_RATE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(p::SAMPLE_RATE),
    );
    let (before, after) = dev.arm()?;
    println!("armed    : 0x{before:02X} -> 0x{after:02X}");
    dev.clear_halts();

    // Start the isochronous streams first: this is what clocks the device.
    let iso_out = unsafe {
        iso::IsoStream::start(
            dev.raw_handle(),
            p::EP_ISO_OUT,
            p::ISO_OUT_PACKET,
            p::ISO_PACKETS_PER_XFER,
            p::ISO_XFERS,
            Some((p::OUT_FRAME_BYTES, p::SAMPLE_RATE as u64)),
        )
    };
    let iso_in = if std::env::var("NS6_NO_ISO_IN").is_ok() {
        Err(0)
    } else {
        unsafe {
            iso::IsoStream::start(
                dev.raw_handle(),
                p::EP_ISO_IN,
                p::ISO_IN_PACKET,
                p::ISO_PACKETS_PER_XFER,
                4,
                None,
            )
        }
    };
    match (&iso_out, &iso_in) {
        (Ok(_), Ok(_)) => println!("iso      : streams up on 0x02 and 0x81"),
        _ => println!(
            "iso      : FAILED to start (out err {:?}, in err {:?})",
            iso_out.as_ref().err(),
            iso_in.as_ref().err()
        ),
    }

    iso::ISO_IN_DUMP.store(true, Ordering::Relaxed);
    let pump_alive = Arc::new(AtomicBool::new(true));
    let pump = thread::spawn({
        let alive = pump_alive.clone();
        move || {
            while alive.load(Ordering::Relaxed) {
                iso::pump_events();
            }
        }
    });
    thread::sleep(Duration::from_millis(500));
    println!(
        "iso      : out {} pkts, in {} pkts / {} bytes after 0.5s",
        iso::ISO_OUT_OK.load(Ordering::Relaxed),
        iso::ISO_IN_OK.load(Ordering::Relaxed),
        iso::ISO_IN_DATA.load(Ordering::Relaxed),
    );

    // Async bulk IN queues, mirroring the driver's 8-deep URB queues. A single
    // blocking read leaves gaps where nothing is posted.
    let _midi_in =
        unsafe { iso::BulkInStream::start(dev.raw_handle(), p::EP_MIDI_IN, p::BLOCK, 1, true) };
    let _audio_in = unsafe {
        iso::BulkInStream::start(dev.raw_handle(), p::EP_PCM_IN, p::AUDIO_IN_XFER, 1, false)
    };
    match (&_midi_in, &_audio_in) {
        (Ok(_), Ok(_)) => println!("bulk in  : queues up on 0x83 and 0x86"),
        _ => println!(
            "bulk in  : FAILED (midi {:?}, audio {:?})",
            _midi_in.as_ref().err(),
            _audio_in.as_ref().err()
        ),
    }

    if std::env::var("NS6_ARM_AFTER").is_ok() {
        thread::sleep(Duration::from_millis(200));
        match dev.arm() {
            Ok((b, a)) => println!("re-armed after streaming: 0x{b:02X} -> 0x{a:02X}"),
            Err(e) => println!("re-arm failed: {e}"),
        }
    }

    println!("\nwatching for device activity for 20s (move controls now)\n");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = Instant::now();
    while Instant::now() < deadline {
        if let Ok(mut q) = iso::MIDI_IN.lock() {
            if !q.is_empty() {
                println!(
                    "  MIDI: {}",
                    q.iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                q.clear();
            }
        }
        thread::sleep(Duration::from_millis(20));
        if last.elapsed() >= Duration::from_secs(5) {
            last = Instant::now();
            println!(
                "  iso-out:{} iso-in:{}/{}B  bulk-in:{} xfers/{}B  midi:{}B",
                iso::ISO_OUT_OK.load(Ordering::Relaxed),
                iso::ISO_IN_OK.load(Ordering::Relaxed),
                iso::ISO_IN_DATA.load(Ordering::Relaxed),
                iso::BULK_IN_OK.load(Ordering::Relaxed),
                iso::BULK_IN_BYTES.load(Ordering::Relaxed),
                iso::MIDI_IN_BYTES.load(Ordering::Relaxed),
            );
        }
    }
    let any = iso::ISO_IN_DATA.load(Ordering::Relaxed) > 0
        || iso::BULK_IN_BYTES.load(Ordering::Relaxed) > 0;

    pump_alive.store(false, Ordering::Relaxed);
    iso::ISO_RUNNING.store(false, Ordering::Relaxed);
    let _ = pump.join();
    drop(iso_out);
    drop(iso_in);

    if !any {
        println!("\nNo audio came back from the device, so it is still not streaming.");
    }
    Ok(())
}

/// Emit synthetic MIDI so the ALSA path can be verified without hardware.
fn cmd_test() -> Result<(), Box<dyn std::error::Error>> {
    let mut port = midi::MidiPort::open()?;
    port.describe();
    println!("\ntest mode: emitting synthetic MIDI (no hardware).");
    println!("Watch with: aseqdump -p \"{}\"\n", midi::CLIENT_NAME);

    install_signal_handler();

    let mut value = 0u8;
    while running() {
        port.send_bytes(&[0xB0, 0x0E, value]);
        if value.is_multiple_of(20) {
            println!("  -> CC 14 = {value}");
        }
        value = (value + 1) % 128;
        thread::sleep(Duration::from_millis(50));
    }
    println!("\nstopped");
    Ok(())
}

/// Drive the controller's LEDs, with no ALSA port and no streaming threads.
///
/// The device lights a button when it is sent the note that button sends, so
/// this is also a way to read the LED map off the hardware: it walks every note
/// on every channel and you watch what comes on.
///
///     ns6 led                     light everything at once, then clear
///     ns6 led sweep               walk note by note, to read the LED map off
///     ns6 led 1 0x11 127          one note: channel, number, velocity
///     ns6 led cc 0 0x31 127       one control change instead
///     ns6 led cc 0                every control change on channel 1 at once
///     ns6 led cc                  every control change on every channel
fn cmd_led() -> Result<(), Box<dyn std::error::Error>> {
    let (dev, _streams) = open_for_output()?;
    install_signal_handler();

    let args: Vec<String> = std::env::args().skip(2).collect();
    let parse = |s: &String| -> Option<u8> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix("0x") {
            u8::from_str_radix(hex, 16).ok()
        } else {
            s.parse().ok()
        }
    };

    // An optional leading "cc" or "note" picks the message type; LEDs on this
    // device are control change, so being able to send either matters.
    let (kind, kind_name, rest): (u8, &str, &[String]) =
        match args.first().map(String::as_str) {
            Some("cc") => (0xB0, "CC", &args[1..]),
            Some("note") => (0x90, "note", &args[1..]),
            _ => (0x90, "note", &args[..]),
        };

    if rest.len() == 3 {
        let (ch, num, val) = (
            parse(&rest[0]).unwrap_or(0),
            parse(&rest[1]).unwrap_or(0),
            parse(&rest[2]).unwrap_or(0),
        );
        let hold = Duration::from_secs(
            std::env::var("NS6_LED_HOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
        );
        println!(
            "channel {ch} {kind_name} 0x{num:02X} ({num}) value {val} - holding {}s",
            hold.as_secs()
        );
        dev.write_midi(&[kind | (ch & 0x0F), num, val])?;
        thread::sleep(hold);
    } else if args.first().map(String::as_str) == Some("sweep") {
        println!(
            "Walking every note on channels 1-5. Each lights for a moment, then\n\
             clears. Note which button responds to which number.\n"
        );
        for ch in 0..5u8 {
            for note in 0..0x60u8 {
                if !running() {
                    break;
                }
                print!("\r  channel {} note 0x{note:02X}   ", ch + 1);
                let _ = std::io::Write::flush(&mut std::io::stdout());
                dev.write_midi(&[0x90 | ch, note, 0x7F])?;
                thread::sleep(Duration::from_millis(400));
                dev.write_midi(&[0x90 | ch, note, 0x00])?;
            }
        }
        println!();
    } else {
        // Everything of one kind at once. Blunt, but it answers "can this light
        // be driven at all" in a single look at the panel, which a walk of
        // several hundred candidates does not.
        let hold = Duration::from_secs(
            std::env::var("NS6_LED_HOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
        );
        let channels: Vec<u8> = match rest.first().and_then(parse) {
            Some(ch) => vec![ch],
            None => (0..5u8).collect(),
        };
        println!(
            "lighting every {kind_name} on channel(s) {} for {}s...",
            channels
                .iter()
                .map(|c| (c + 1).to_string())
                .collect::<Vec<_>>()
                .join(", "),
            hold.as_secs()
        );
        let mut skipped = 0;
        for &ch in &channels {
            for num in 0..0x80u8 {
                // Never send a message known to drop the device off the bus.
                if ledmap::HAZARDS.contains(&(kind, ch, num)) {
                    skipped += 1;
                    continue;
                }
                dev.write_midi(&[kind | ch, num, 0x7F])?;
            }
        }
        if skipped > 0 {
            println!("  ({skipped} known-destructive message(s) skipped)");
        }
        thread::sleep(hold);
    }

    // Leave nothing lit.
    for ch in 0..5u8 {
        for note in 0..0x60u8 {
            let _ = dev.write_midi(&[0x90 | ch, note, 0x00]);
        }
    }

    Ok(())
}

/// Bring the streams up without an ALSA port, for the LED commands.
///
/// The device accepts MIDI only while it is streaming, so the pipes have to be
/// running even though nothing here reads from them.
fn open_for_output() -> Result<(Ns6, Streams), Box<dyn std::error::Error>> {
    let dev = Ns6::open()?;
    dev.start()?;
    let audio_in = unsafe {
        iso::BulkInStream::start(dev.raw_handle(), p::EP_PCM_IN, p::AUDIO_IN_XFER, 7, false)
    }
    .map_err(|e| format!("audio in queue: libusb error {e}"))?;
    let iso_in = unsafe {
        iso::IsoStream::start(dev.raw_handle(), p::EP_ISO_IN, p::ISO_IN_PACKET, 5, 9, None)
    }
    .map_err(|e| format!("iso IN stream: libusb error {e}"))?;
    let iso_out = unsafe {
        iso::IsoStream::start(
            dev.raw_handle(),
            p::EP_ISO_OUT,
            p::ISO_OUT_PACKET,
            40,
            3,
            Some((p::OUT_FRAME_BYTES, p::SAMPLE_RATE as u64)),
        )
    }
    .map_err(|e| format!("iso OUT stream: libusb error {e}"))?;
    let alive = Arc::new(AtomicBool::new(true));
    let pump = thread::spawn({
        let alive = alive.clone();
        move || {
            while alive.load(Ordering::Relaxed) {
                iso::pump_events();
            }
        }
    });
    Ok((
        dev,
        Streams {
            alive,
            pump: Some(pump),
            _audio_in: audio_in,
            _iso_in: iso_in,
            _iso_out: iso_out,
        },
    ))
}

/// Keeps the streams and their event pump alive for as long as it is held.
struct Streams {
    alive: Arc<AtomicBool>,
    pump: Option<thread::JoinHandle<()>>,
    _audio_in: iso::BulkInStream,
    _iso_in: iso::IsoStream,
    _iso_out: iso::IsoStream,
}

impl Drop for Streams {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
    }
}

/// Walk the LED space, recording what each note lights.
///
/// Runs on its own with one LED lit at a time, because most of the several
/// hundred notes will do nothing and prompting for each would be unusable.
/// Enter interrupts, which is when the typing starts.
/// Where the LED map is read back from and written to.
const LED_MAP: &str = "ns6-leds.toml";
const SURFACE_MAP: &str = "ns6-surface.toml";

fn cmd_leds() -> Result<(), Box<dyn std::error::Error>> {
    let (dev, _streams) = open_for_output()?;
    install_signal_handler();

    let mut walk = ledmap::LedWalk::new(5, &[0xB0, 0x90], 0x80);
    // The device can be knocked off the bus by this sweep - the MIDI OUT byte
    // stream doubles as a serial register interface into an audio chip - so the
    // walk has to be resumable and has to keep what has been found so far.
    if let Ok(start) = std::env::var("NS6_LED_START") {
        if let Ok(n) = start.parse::<usize>() {
            walk.seek(n.saturating_sub(1));
            println!("resuming at {n}");
        }
    }
    walk.load(std::path::Path::new(LED_MAP));
    // Positions as displayed, so a message that dropped the device can be ruled
    // out without waiting to trip over it again.
    if let Ok(list) = std::env::var("NS6_LED_SKIP") {
        for n in list.split(',').filter_map(|v| v.trim().parse::<usize>().ok()) {
            if let Some(c) = walk.mark_hazard_at(n) {
                println!("skipping [{n}] {}", c.describe());
            }
        }
    }
    let (_, total) = walk.position();
    println!(
        "\n{total} candidates: control change then note on, channels 1-5.\n\n\
         The walk does not move on its own - nothing goes past while you are\n\
         looking at the panel.\n\n    \
         right arrow   send the next one; hold to skim\n    \
         left arrow    go back; hold to skim backwards\n    \
         Enter         describe what is lit; Enter again saves it\n    \
         q             stop and write ns6-leds.toml\n\n\
         Messages known to take the device off the bus are stepped over rather\n\
         than sent. NS6_LED_UNSAFE=1 sends them anyway.\n"
    );

    let Some(raw) = term::RawMode::enable() else {
        return Err("ns6 leds needs a terminal: it reads arrow keys".into());
    };

    let send = |c: ledmap::Candidate, on: bool| -> Result<(), device::Error> {
        dev.write_midi(&c.message(on)).map(|_| ())
    };

    // Nothing is sent until the first right arrow, so the panel starts dark.
    let mut lit: Option<ledmap::Candidate> = None;

    let show = |walk: &ledmap::LedWalk, c: ledmap::Candidate| {
        let (n, total) = walk.position();
        let named = walk.description_of(c);
        print!(
            "\r\x1b[K  [{n}/{total}] {}{}",
            c.describe(),
            named.map(|d| format!("   = {d}")).unwrap_or_default()
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    };

    // Holding an arrow skims; the dwell is how fast.
    let dwell = Duration::from_millis(
        std::env::var("NS6_LED_DWELL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120),
    );
    let mut last_step = Instant::now() - dwell;
    let mut quit = false;

    while running() && !quit {
        let pending = term::drain();

        for c in &pending.chars {
            if *c == 'q' || *c == 'Q' {
                quit = true;
            }
        }
        if quit {
            break;
        }

        if let Some(arrow) = pending.arrow {
            if last_step.elapsed() >= dwell {
                if let Some(c) = lit {
                    send(c, false)?;
                }
                let next = if arrow == term::Key::Right {
                    walk.advance()
                } else {
                    walk.back()
                };
                match next {
                    Some(c) if walk.is_hazard(c) && std::env::var("NS6_LED_UNSAFE").is_err() => {
                        lit = None;
                        let (n, total) = walk.position();
                        print!(
                            "\r\x1b[K  [{n}/{total}] {}   SKIPPED - known to drop the device",
                            c.describe()
                        );
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    Some(c) => {
                        if let Err(e) = send(c, true) {
                            let (n, _) = walk.position();
                            // Remember it, so the resume steps straight over it.
                            walk.mark_hazard(c);
                            let _ = std::fs::write(LED_MAP, walk.to_toml());
                            println!(
                                "\n\n  the device stopped responding at [{n}] {}\n  \
                                 ({e})\n\n  Recorded as destructive. Power-cycle it, then:\n      \
                                 NS6_LED_START={n} ns6 leds",
                                c.describe()
                            );
                            break;
                        }
                        lit = Some(c);
                        show(&walk, c);
                    }
                    None => {
                        lit = None;
                        println!("\n  end of the list. q to finish.");
                    }
                }
                last_step = Instant::now();
            }
        }

        if pending.enter {
            if let Some(c) = walk.current() {
                let cooked = raw.cooked();
                println!("\n  {}", c.describe());
                print!("  what is lit? ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                    let line = line.trim();
                    if line.is_empty() {
                        println!("  (nothing recorded)");
                    } else {
                        walk.record(line);
                        // Written now rather than at exit: this sweep can take
                        // the device down without warning.
                        if let Err(e) = std::fs::write(LED_MAP, walk.to_toml()) {
                            eprintln!("  could not save: {e}");
                        }
                        println!("  recorded: {line}");
                    }
                }
                drop(cooked);
                show(&walk, c);
                last_step = Instant::now();
            }
        }

        thread::sleep(Duration::from_millis(5));
    }
    drop(raw);

    // Leave nothing lit.
    for kind in [0xB0u8, 0x90] {
        for ch in 0..5u8 {
            for n in 0..0x80u8 {
                let _ = dev.write_midi(&[kind | ch, n, 0x00]);
            }
        }
    }

    let path = std::path::Path::new(LED_MAP);
    match std::fs::write(path, walk.to_toml()) {
        Ok(()) => println!("\n\nwrote {} LEDs to {}", walk.found.len(), path.display()),
        Err(e) => eprintln!("\ncould not write {}: {e}", path.display()),
    }
    Ok(())
}

/// Measure the platter's resolution.
///
/// The platter reports 14-bit absolute position that wraps, so a mapping has to
/// know how many ticks make one revolution before it can turn movement into
/// scratching at the right speed. That number is not in the descriptors or the
/// vendor driver; it has to come off the hardware.
fn cmd_jog() -> Result<(), Box<dyn std::error::Error>> {
    let dev = Arc::new(Ns6::open()?);
    dev.start()?;
    install_signal_handler();

    let _audio_in = unsafe {
        iso::BulkInStream::start(dev.raw_handle(), p::EP_PCM_IN, p::AUDIO_IN_XFER, 7, false)
    }
    .map_err(|e| format!("audio in queue: libusb error {e}"))?;
    let _midi_in =
        unsafe { iso::BulkInStream::start(dev.raw_handle(), p::EP_MIDI_IN, p::BLOCK, 5, true) }
            .map_err(|e| format!("MIDI in queue: libusb error {e}"))?;
    let _iso_in = unsafe {
        iso::IsoStream::start(dev.raw_handle(), p::EP_ISO_IN, p::ISO_IN_PACKET, 5, 9, None)
    }
    .map_err(|e| format!("iso IN stream: libusb error {e}"))?;
    let _iso_out = unsafe {
        iso::IsoStream::start(
            dev.raw_handle(),
            p::EP_ISO_OUT,
            p::ISO_OUT_PACKET,
            40,
            3,
            Some((p::OUT_FRAME_BYTES, p::SAMPLE_RATE as u64)),
        )
    }
    .map_err(|e| format!("iso OUT stream: libusb error {e}"))?;
    let alive = Arc::new(AtomicBool::new(true));
    let pump = thread::spawn({
        let alive = alive.clone();
        move || {
            while alive.load(Ordering::Relaxed) {
                iso::pump_events();
            }
        }
    });

    println!(
        "\nPut a mark on one platter and turn it slowly through exactly one full\n\
         revolution, then stop. Ctrl-C to finish.\n\n\
         Turning it several times and dividing is more accurate than one turn.\n"
    );

    let mut surface = learn::Surface::new();
    // Per (channel, msb) accumulated travel and last raw position.
    let mut msb: std::collections::BTreeMap<u8, u8> = Default::default();
    let mut last: std::collections::BTreeMap<u8, i32> = Default::default();
    let mut travel: std::collections::BTreeMap<u8, i64> = Default::default();
    let mut last_report = Instant::now();

    while running() {
        if let Ok(mut q) = iso::MIDI_IN.lock() {
            if !q.is_empty() {
                for c in surface.feed(&q) {
                    if c.kind != learn::Kind::Cc {
                        continue;
                    }
                    match c.number {
                        0 => {
                            msb.insert(c.channel, c.last);
                        }
                        32 => {
                            let hi = msb.get(&c.channel).copied().unwrap_or(0) as i32;
                            let position = (hi << 7) | c.last as i32;
                            if let Some(prev) = last.insert(c.channel, position) {
                                // Shortest way round a 14-bit circle.
                                let mut d = position - prev;
                                if d > 8192 {
                                    d -= 16384;
                                } else if d < -8192 {
                                    d += 16384;
                                }
                                *travel.entry(c.channel).or_insert(0) += d.abs() as i64;
                            }
                        }
                        _ => {}
                    }
                }
                q.clear();
            }
        }
        if last_report.elapsed() >= Duration::from_millis(250) && !travel.is_empty() {
            last_report = Instant::now();
            let line: Vec<String> = travel
                .iter()
                .map(|(ch, t)| format!("deck {}: {t} ticks", ch))
                .collect();
            print!("\r\x1b[K  {}", line.join("   "));
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        thread::sleep(Duration::from_millis(2));
    }

    println!("\n");
    for (ch, t) in &travel {
        println!("  deck {ch}: {t} ticks travelled");
        for turns in 1..=4 {
            println!("      if that was {turns} turn(s): {} ticks per revolution", t / turns);
        }
    }
    alive.store(false, Ordering::Relaxed);
    let _ = pump.join();
    Ok(())
}
