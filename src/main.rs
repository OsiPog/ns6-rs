//! ns6 - userspace MIDI driver for the Numark NS6 DJ controller on Linux.
//!
//! The NS6 exposes only vendor-specific USB interfaces, so no kernel driver
//! binds it and no ALSA MIDI port appears. This talks the device's Ploytec
//! protocol over libusb and publishes an ALSA sequencer port instead.
//!
//! See `docs/PROTOCOL.md` for how the protocol was derived.

mod audio;
mod device;
mod iso;
mod ledmap;
mod learn;
mod midi;
mod protocol;
mod pw;
mod term;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
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
             ns6 led       light specific LEDs; `led cc 1:17=127 1:40` holds several\n    \
             ns6 leds      walk the LED space and record what each note lights\n    \
             ns6 bars      hunt the level displays; `bars step 1` drives one by hand\n    \
             ns6 jog       measure how many ticks a platter reports per turn\n    \
             ns6 probe     report device state and sweep bulk OUT configurations\n    \
             ns6 learn     watch the control surface: move a control, see its MIDI\n    \
             ns6 map       move a control, say what it was; writes ns6-surface.toml\n    \
             ns6 test      emit synthetic MIDI on the ALSA port, no hardware needed\n    \
             ns6 audio     stream a test tone out and capture the audio input\n    \
             ns6 play F    play 44100 Hz s32le stereo from a file, pipe or -\n    \
             ns6 rec F     capture 44100 Hz s32le stereo to a file, pipe or -\n    \
             ns6 duplex P C  do both at once, from P and to C\n\
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
        "bars" => cmd_bars(),
        "jog" => cmd_jog(),
        "probe" => cmd_probe(),
        "test" => cmd_test(),
        "audio" => cmd_audio(),
        "play" => cmd_stream(true, false),
        "rec" => cmd_stream(false, true),
        "duplex" => cmd_stream(true, true),
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

/// Whether the audio side still has a device to talk to.
///
/// The PipeWire loop runs on its own thread and has to wind down with the rest
/// of the driver - which includes the controller being switched off mid-run,
/// not just a Ctrl-C.
fn audio_alive() -> bool {
    running() && !iso::DEVICE_GONE.load(Ordering::Relaxed)
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
    // Audio is a pair of PipeWire nodes, published for as long as the bridge
    // runs so the controller is a sound card whether or not anything is playing
    // through it. Naming a path takes the audio side over instead: that is what
    // `NS6_PLAY`/`NS6_REC` are for, and a pipe or a file is still how to get at
    // the streams on a machine with no sound server at all.
    // The last look at the audio before the device has it, for when a question
    // about what is actually being sent cannot be answered any other way.
    if let Ok(path) = std::env::var("NS6_OUT_DUMP") {
        iso::dump_out_to(&path)?;
        println!("dumping wire frames to {path}");
    }
    // The feedback endpoint's own bytes, for reading its format off the wire.
    if let Ok(n) = std::env::var("NS6_FB_DUMP") {
        iso::dump_feedback(n.parse().unwrap_or(32));
    }
    // Send the nominal rate and ignore what the device asks for, which is what
    // this driver did before the feedback endpoint was read. Kept because the
    // difference between the two is only visible against hardware.
    if std::env::var("NS6_NO_FEEDBACK").is_ok() {
        iso::NO_FEEDBACK.store(true, Ordering::Relaxed);
        println!("feedback: ignored, sending the nominal rate");
    }
    let audio_paths = AudioPaths::from_env();
    let no_pipewire = std::env::var("NS6_NO_PIPEWIRE").is_ok();
    let with_audio = audio_paths.any() || !no_pipewire;
    if audio_paths.any() {
        if let Some(p) = audio_paths.play {
            println!("audio out: reading {p}");
            threads.push(spawn_play(p));
        }
        if let Some(p) = audio_paths.rec {
            println!("audio in : writing {p}");
            threads.push(spawn_rec(p));
        }
    } else if !no_pipewire {
        threads.push(pw::spawn(audio_alive));
    }
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
            if with_audio {
                println!("  {}", audio_status());
                println!("  {}", clock_status());
            }
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
    let args: Vec<String> = std::env::args().skip(2).collect();
    if args.first().map(String::as_str) == Some("seq") {
        return cmd_seq(&args[1..]);
    }

    let (dev, _streams) = open_for_output()?;
    install_signal_handler();
    let parse = |s: &String| -> Option<u8> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix("0x") {
            u8::from_str_radix(hex, 16).ok()
        } else {
            s.parse().ok()
        }
    };

    // Everything sent, so the end of the command can clear exactly that. The
    // old code cleared notes 0x00..0x60 on every channel regardless, which
    // cleared nothing at all after a `cc` run - and LEDs on this device are
    // control change, so that was every run that lit anything.
    let mut lit: Vec<[u8; 3]> = Vec::new();
    let unsafe_ok = std::env::var("NS6_LED_UNSAFE").is_ok();

    // An optional leading "cc" or "note" picks the message type; LEDs on this
    // device are control change, so being able to send either matters.
    let (kind, kind_name, rest): (u8, &str, &[String]) =
        match args.first().map(String::as_str) {
            Some("cc") => (0xB0, "CC", &args[1..]),
            Some("note") => (0x90, "note", &args[1..]),
            _ => (0x90, "note", &args[..]),
        };

    let hold_secs = |default: u64| {
        Duration::from_secs(
            std::env::var("NS6_LED_HOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default),
        )
    };

    if rest.iter().any(|a| a.contains(':')) {
        // Several named messages, held together. This is the only way to ask
        // whether two lights are independent lamps or two faces of one state:
        // the walk in `ns6 leds` clears each candidate before sending the next,
        // so it can never show what a combination does.
        //
        // Channels here are MIDI channels, 1-5, as the recorded maps write them.
        let specs: Vec<(u8, u8, u8)> = rest
            .iter()
            .filter(|a| a.contains(':'))
            .filter_map(|a| {
                let (ch, rest) = a.split_once(':')?;
                let (num, val) = match rest.split_once('=') {
                    Some((n, v)) => (n, parse(&v.to_string())?),
                    None => (rest, 0x7F),
                };
                let ch: u8 = parse(&ch.to_string())?;
                Some((ch.saturating_sub(1) & 0x0F, parse(&num.to_string())?, val))
            })
            .collect();
        if specs.len() != rest.iter().filter(|a| a.contains(':')).count() {
            return Err("could not read a spec; each is channel:number[=value], \
                        as in `ns6 led cc 1:17=127 1:40`"
                .into());
        }
        let hold = hold_secs(20);
        println!(
            "sending {} message(s) together, holding {}s:",
            specs.len(),
            hold.as_secs()
        );
        for &(ch, num, val) in &specs {
            if ledmap::is_hazard_number(kind, num) && !unsafe_ok {
                println!(
                    "  ch{} {kind_name} 0x{num:02X} ({num})  SKIPPED - takes the device \
                     off the bus. NS6_LED_UNSAFE=1 sends it anyway.",
                    ch + 1
                );
                continue;
            }
            println!("  ch{} {kind_name} 0x{num:02X} ({num}) = {val}", ch + 1);
            let msg = [kind | ch, num, val];
            dev.write_midi(&msg)?;
            lit.push(msg);
        }
        println!("\nlook at the panel. Everything above is lit at once.");
        thread::sleep(hold);
    } else if rest.len() == 3 {
        let (ch, num, val) = (
            parse(&rest[0]).unwrap_or(0),
            parse(&rest[1]).unwrap_or(0),
            parse(&rest[2]).unwrap_or(0),
        );
        if ledmap::is_hazard_number(kind, num) && !unsafe_ok {
            return Err(format!(
                "channel {} {kind_name} 0x{num:02X} ({num}) takes the device off the \
                 bus and needs a power cycle. NS6_LED_UNSAFE=1 sends it anyway.",
                ch + 1
            )
            .into());
        }
        let hold = hold_secs(5);
        println!(
            "channel {ch} {kind_name} 0x{num:02X} ({num}) value {val} - holding {}s",
            hold.as_secs()
        );
        let msg = [kind | (ch & 0x0F), num, val];
        dev.write_midi(&msg)?;
        lit.push(msg);
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
                if ledmap::is_hazard_number(0x90, note) && !unsafe_ok {
                    continue;
                }
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
        let hold = hold_secs(8);
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
                if ledmap::is_hazard_number(kind, num) && !unsafe_ok {
                    skipped += 1;
                    continue;
                }
                let msg = [kind | ch, num, 0x7F];
                dev.write_midi(&msg)?;
                lit.push(msg);
            }
        }
        if skipped > 0 {
            println!("  ({skipped} known-destructive message(s) skipped)");
        }
        thread::sleep(hold);
    }

    // Leave nothing lit - and clear the same messages that were sent, rather
    // than a fixed range of notes, which missed control change entirely.
    for msg in &lit {
        let _ = dev.write_midi(&[msg[0], msg[1], 0x00]);
    }

    Ok(())
}

/// One number, several values in turn, holding each.
///
/// Comparing values on the same display is otherwise four separate runs with a
/// device open and a clear between each, which puts the display dark for longer
/// than it is lit and makes the order hard to follow. Here they follow each other
/// directly, in one session.
///
///     ns6 led seq cc 2:58=21,53,85,117
///
/// `NS6_LED_STEP` sets the milliseconds per value, default 2000.
fn cmd_seq(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (kind, kind_name, rest): (u8, &str, &[String]) = match args.first().map(String::as_str) {
        Some("note") => (0x90, "note", &args[1..]),
        Some("cc") => (0xB0, "CC", &args[1..]),
        _ => (0xB0, "CC", args),
    };
    let spec = rest
        .first()
        .ok_or("usage: ns6 led seq cc <channel>:<number>=<v1,v2,...>")?;
    let (target, values) = spec
        .split_once('=')
        .ok_or("no values: expected <channel>:<number>=<v1,v2,...>")?;
    let (ch_s, num_s) = target
        .split_once(':')
        .ok_or("expected <channel>:<number>=<v1,v2,...>")?;
    let midi_ch: u8 = ch_s.trim().parse().map_err(|_| "bad channel")?;
    let ch = midi_ch.saturating_sub(1) & 0x0F;
    let number: u8 = num_s.trim().parse().map_err(|_| "bad number")?;
    let values: Vec<u8> = values
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    if values.is_empty() {
        return Err("no values could be read".into());
    }
    if ledmap::is_hazard_number(kind, number) && std::env::var("NS6_LED_UNSAFE").is_err() {
        return Err(format!(
            "{kind_name} {number} takes the device off the bus. NS6_LED_UNSAFE=1 sends it anyway."
        )
        .into());
    }

    let step = Duration::from_millis(
        std::env::var("NS6_LED_STEP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000),
    );

    let (dev, _streams) = open_for_output()?;
    install_signal_handler();

    println!(
        "channel {midi_ch} {kind_name} {number}, {} values, {} ms each:\n",
        values.len(),
        step.as_millis()
    );
    for (i, &v) in values.iter().enumerate() {
        if !running() {
            break;
        }
        println!("  [{}/{}] value {v}", i + 1, values.len());
        dev.write_midi(&[kind | ch, number, v])?;
        thread::sleep(step);
    }
    let _ = dev.write_midi(&[kind | ch, number, 0x00]);
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
/// Where the multi-segment displays are recorded. Separate from the LED map
/// because the entries are a different shape: a *range* of numbers driven
/// together at a *value*, rather than one message that turns one lamp on.
const DISPLAY_MAP: &str = "ns6-displays.toml";

/// One described display: the numbers that drive it, held at the value that
/// showed it.
///
/// The numbers are stored out in full rather than as a first..last range. A
/// block is eight *unaccounted* numbers, and the accounted-for ones are skipped
/// in between, so the eight are sparse - the first block on channel 1 is
/// 0, 1, 2, 6, 19, 20, 29, 30. Written as a range that reads as 0..30, which is
/// thirty-one numbers, twenty-three of which were never sent. The file is the
/// only record of a session at the panel, and it has to be replayable.
struct Display {
    channel: u8,
    kind: u8,
    numbers: Vec<u8>,
    value: u8,
    description: String,
}

fn displays_to_toml(found: &[Display]) -> String {
    use std::fmt::Write as _;
    let mut s = String::from(
        "# Numark NS6 multi-segment displays, recorded with `ns6 bars blocks`.\n\
         # Send every number in `numbers` at `value` to show what is described.\n\
         #\n\
         # These are not in ns6-leds.toml because the walk that produced that file\n\
         # could not see them: it sends one message at a time, and one message to a\n\
         # bar or a ring lights a single segment.\n\
         #\n\
         # `value` is not a brightness. What it means differs per display and has\n\
         # to be recorded, not assumed: the FX rings and the Serato bar light the\n\
         # single LED at position `value`, while the strip search fills `value` of\n\
         # its fifteen. Either way 127 is out of range, which is why a sweep at 127\n\
         # found none of them.\n",
    );
    for d in found {
        let numbers = d
            .numbers
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            s,
            "\n[[display]]\ndescription = \"{}\"\nchannel = {}\nkind = \"{}\"\nnumbers = [{}]\nvalue = {}\n",
            learn::escape(&d.description),
            d.channel,
            if d.kind == 0xB0 { "cc" } else { "note" },
            numbers,
            d.value
        );
    }
    s
}

fn displays_load(path: &std::path::Path) -> Vec<Display> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let (mut desc, mut ch, mut kind, mut value) = (None, None, None, None);
    let mut numbers: Option<Vec<u8>> = None;
    let field = |l: &str| l.split('=').nth(1).map(|v| v.trim().to_string());
    let numeric = |v: Option<String>| -> Option<u8> {
        v.and_then(|v| v.split('#').next().map(str::trim).and_then(|n| n.parse().ok()))
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[[display]]") {
            desc = None;
            ch = None;
            kind = None;
            numbers = None;
            value = None;
        } else if line.starts_with("description") {
            desc = field(line).map(|v| learn::unescape(&v));
        } else if line.starts_with("channel") {
            ch = numeric(field(line));
        } else if line.starts_with("kind") {
            kind = field(line).map(|v| if v.contains("cc") { 0xB0u8 } else { 0x90 });
        } else if line.starts_with("numbers") {
            numbers = field(line).map(|v| {
                v.trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .filter_map(|n| n.trim().parse::<u8>().ok())
                    .collect()
            });
        } else if line.starts_with("value") {
            value = numeric(field(line));
        }
        if let (Some(d), Some(c), Some(k), Some(n), Some(v)) = (&desc, ch, kind, &numbers, value) {
            out.push(Display {
                channel: c,
                kind: k,
                numbers: n.clone(),
                value: v,
                description: d.clone(),
            });
            desc = None;
            value = None;
        }
    }
    if !out.is_empty() {
        println!("carried over {} already-described displays", out.len());
    }
    out
}

/// Hunt for the panel's *level* displays - the FX PARAM rings, the strip search
/// bars, the Serato strip, the MASTER meter.
///
/// `ns6 leds` cannot find these, and it is worth being precise about why, because
/// it is not simply that it missed them. It walks control change and note on over
/// **five** channels and sends every one at value **127**. Three assumptions are
/// baked into that, and a bar display breaks all of them:
///
///   - **Five channels.** MIDI has sixteen. Channels 6-16 have never been sent
///     anything at all.
///   - **Control change and note on only.** A 14-bit level has an obvious
///     carrier in MIDI - pitch bend - and it was never tried. Nor was program
///     change.
///   - **Value 127.** The layer indicators already proved the value can carry
///     meaning rather than just on-ness, and for a display that reads its value
///     as a level, one fixed value is the worst possible probe: it cannot
///     animate, so it looks like a lamp if it responds at all.
///
/// So this ramps instead of holding. A level display sweeps, which is unmistakable
/// out of the corner of an eye; a plain lamp just blinks once at its threshold.
/// That difference is the whole point.
fn cmd_bars() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    if args.first().map(String::as_str) == Some("blocks") {
        return cmd_blocks(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("step") {
        return cmd_step(&args[1..]);
    }

    let (dev, _streams) = open_for_output()?;
    install_signal_handler();

    let kind = args.first().map(String::as_str).unwrap_or("pb");

    // Channels to try, as MIDI channels 1-16. `6-16` covers exactly the space
    // `ns6 leds` never reached.
    let range = args.iter().find(|a| a.contains('-') || a.parse::<u8>().is_ok());
    let (first, last) = match range.map(String::as_str) {
        Some(spec) => match spec.split_once('-') {
            Some((a, b)) => (
                a.trim().parse::<u8>().unwrap_or(1).max(1),
                b.trim().parse::<u8>().unwrap_or(16).min(16),
            ),
            None => {
                let c = spec.trim().parse::<u8>().unwrap_or(1).clamp(1, 16);
                (c, c)
            }
        },
        None => (1, 16),
    };

    // Seconds each channel gets. One ramp up and back down inside it.
    let dwell = Duration::from_millis(
        std::env::var("NS6_BARS_DWELL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2500),
    );
    // Pin the value instead of ramping. This is what separates "many numbers at
    // once" from "the animation itself": same messages, same simultaneity, no
    // movement.
    let fixed = std::env::var("NS6_BARS_VALUE")
        .ok()
        .and_then(|v| v.parse::<u8>().ok());

    let steps: u32 = std::env::var("NS6_BARS_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if kind == "pb" || kind == "pc" { 48 } else { 16 });
    let unsafe_ok = std::env::var("NS6_LED_UNSAFE").is_ok();

    println!(
        "\nRamping {kind} over MIDI channels {first}-{last}, {} ms each.\n\n\
         Watch for anything that *sweeps* rather than blinks: the FX PARAM rings,\n\
         the strip search bars, the Serato strip, the MASTER meter. A lamp can only\n\
         switch on; a level display follows the ramp, which is what gives it away.\n",
        dwell.as_millis()
    );
    if kind == "cc" || kind == "note" || kind == "unknown" {
        println!(
            "  note: this drives every number on the channel at once, so the\n  \
             ordinary button lights will flicker too. Ignore anything that is\n  \
             merely on or off; only a sweep matters.\n"
        );
    }

    // For the `unknown` mode: every message the recorded map says lights
    // something, so those can be left alone. Driving them too would set a
    // hundred button lights flickering and bury the one thing being hunted -
    // which is the whole difficulty with sweeping this space at all.
    //
    // Channel is deliberately ignored here. The panel-wide lights answer on any
    // channel, so a number excluded only on the channel it was recorded from
    // would light up again the moment channels 6-16 are driven - which is most
    // of what this is for.
    let mut known: Vec<(u8, u8)> = Vec::new();
    if kind == "unknown" {
        let mut map = ledmap::LedWalk::new(16, &[0xB0, 0x90], 0x80);
        map.load(std::path::Path::new(LED_MAP));
        known = map.found.iter().map(|f| (f.kind, f.number)).collect();
        known.sort_unstable();
        known.dedup();
        println!(
            "  leaving {} already-described numbers alone, on every channel, so the\n  \
             panel stays dark except for whatever answers. Any movement is a find.\n",
            known.len()
        );
    }

    // The pipe takes 39 bytes per write, which is thirteen three-byte messages.
    // Driving a whole channel one transfer at a time is thirteen times the USB
    // round trips for nothing, and slow enough to make a ramp visibly step.
    let mut batch: Vec<u8> = Vec::new();

    let mut lit: Vec<[u8; 3]> = Vec::new();
    for midi_ch in first..=last {
        let ch = midi_ch - 1;
        for step in 0..=steps {
            if !running() {
                break;
            }
            // Up then back down, so a display that only moves one way still shows.
            let phase = if step * 2 <= steps { step * 2 } else { 2 * steps - step * 2 };
            let level7 = fixed.unwrap_or((phase * 127 / steps).min(127) as u8);
            let level14 = match fixed {
                Some(v) => v as u16 * 129,
                None => (phase * 16383 / steps).min(16383) as u16,
            };

            // The value on screen, so the moment something lights can be read
            // off rather than guessed at afterwards.
            print!("\r\x1b[K  channel {midi_ch:>2}   value {level7:>3}");
            let _ = std::io::Write::flush(&mut std::io::stdout());

            match kind {
                // Pitch bend carries 14 bits and has no number: one message per
                // channel, so the whole space is 16 messages. The cheapest probe
                // there is, and the likeliest shape for a level.
                "pb" => {
                    dev.write_midi(&[0xE0 | ch, (level14 & 0x7F) as u8, (level14 >> 7) as u8])?;
                }
                // Program change is two bytes, and a display could read the
                // program number as a level.
                "pc" => {
                    dev.write_midi(&[0xC0 | ch, level7])?;
                }
                // Every number on the channel, ramping together.
                _ => {
                    let statuses: &[u8] = if kind == "note" {
                        &[0x90]
                    } else if kind == "unknown" {
                        &[0xB0, 0x90]
                    } else {
                        &[0xB0]
                    };
                    for &status in statuses {
                        for num in 0..0x80u8 {
                            if ledmap::is_hazard_number(status, num) && !unsafe_ok {
                                continue;
                            }
                            if kind == "unknown" && known.contains(&(status, num)) {
                                continue;
                            }
                            let msg = [status | ch, num, level7];
                            if batch.len() + 3 > p::MIDI_OUT_PAYLOAD {
                                dev.write_midi(&batch)?;
                                batch.clear();
                            }
                            batch.extend_from_slice(&msg);
                            lit.push(msg);
                        }
                    }
                    if !batch.is_empty() {
                        dev.write_midi(&batch)?;
                        batch.clear();
                    }
                }
            }
            thread::sleep(dwell / (steps + 1));
        }
        if !running() {
            break;
        }
    }
    println!("\r\x1b[K  done - channels {first}-{last} of {kind}.");

    // Put back whatever was driven.
    for midi_ch in first..=last {
        let ch = midi_ch - 1;
        match kind {
            // Centre, not zero: zero is full deflection one way.
            "pb" => {
                let _ = dev.write_midi(&[0xE0 | ch, 0x00, 0x40]);
            }
            "pc" => {
                let _ = dev.write_midi(&[0xC0 | ch, 0]);
            }
            _ => {}
        }
    }
    lit.sort_unstable();
    lit.dedup();
    for msg in &lit {
        if batch.len() + 3 > p::MIDI_OUT_PAYLOAD {
            let _ = dev.write_midi(&batch);
            batch.clear();
        }
        batch.extend_from_slice(&[msg[0], msg[1], 0x00]);
    }
    if !batch.is_empty() {
        let _ = dev.write_midi(&batch);
    }

    Ok(())
}

/// Every message the recorded map does not account for.
///
/// Channel is not a parameter, because nothing here depends on it. Numbers known
/// to light something are excluded on every channel, since the panel-wide lights
/// answer anywhere and excluding them per channel would put a hundred lit lamps
/// next to the one thing being looked for. Hazards are excluded on every channel
/// for the harder-won reason in `ledmap::is_hazard_number`.
fn unaccounted() -> Vec<(u8, u8)> {
    let mut map = ledmap::LedWalk::new(16, &[0xB0, 0x90], 0x80);
    map.load(std::path::Path::new(LED_MAP));
    let mut known: Vec<(u8, u8)> = map.found.iter().map(|f| (f.kind, f.number)).collect();
    known.sort_unstable();
    known.dedup();

    let unsafe_ok = std::env::var("NS6_LED_UNSAFE").is_ok();
    let mut out = Vec::new();
    for status in [0xB0u8, 0x90] {
        for n in 0..0x80u8 {
            if ledmap::is_hazard_number(status, n) && !unsafe_ok {
                continue;
            }
            if known.contains(&(status, n)) {
                continue;
            }
            out.push((status, n));
        }
    }
    out
}

/// Drive the unaccounted numbers on one channel and move the *value* by hand.
///
/// A ramp proved these displays read their value as a level, and then proved
/// itself useless for reading the mapping off: it went past too quickly to see
/// which value did what, and value 127 - the one the original walk used - turns
/// out to be the one value that does nothing at all.
///
/// So the value stops moving on its own. Sit on one, look at the panel, step to
/// the next.
fn cmd_step(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let midi_ch = args
        .first()
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(1)
        .clamp(1, 16);
    let ch = midi_ch - 1;

    let (dev, _streams) = open_for_output()?;
    install_signal_handler();
    let candidates = unaccounted();

    println!(
        "\nMIDI channel {midi_ch}, {} unaccounted numbers, all held at one value.\n\n    \
         right / left   value + 1 / - 1\n    \
         ] / [          value + 8 / - 8\n    \
         q              stop\n\n\
         Nothing moves unless you move it, so a level can be looked at for as long\n\
         as it takes. Value 127 does nothing here; the interesting end is low.\n",
        candidates.len()
    );

    let Some(_raw) = term::RawMode::enable() else {
        return Err("ns6 bars step needs a terminal: it reads arrow keys".into());
    };

    let send = |value: u8| -> Result<(), device::Error> {
        let mut buf: Vec<u8> = Vec::new();
        for &(status, n) in &candidates {
            if buf.len() + 3 > p::MIDI_OUT_PAYLOAD {
                dev.write_midi(&buf)?;
                buf.clear();
            }
            buf.extend_from_slice(&[status | ch, n, value]);
        }
        if !buf.is_empty() {
            dev.write_midi(&buf)?;
        }
        Ok(())
    };

    let mut value: i32 = 0;
    let mut shown: Option<u8> = None;
    let dwell = Duration::from_millis(90);
    let mut last = Instant::now() - dwell;

    send(0)?;
    print!("  value {value:>3}");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    while running() {
        let pending = term::drain();
        if pending.chars.iter().any(|c| *c == 'q' || *c == 'Q') {
            break;
        }
        let mut delta = 0i32;
        for c in &pending.chars {
            match c {
                ']' => delta += 8,
                '[' => delta -= 8,
                _ => {}
            }
        }
        if let Some(arrow) = pending.arrow {
            delta += if arrow == term::Key::Right { 1 } else { -1 };
        }
        if delta != 0 && last.elapsed() >= dwell {
            last = Instant::now();
            value = (value + delta).clamp(0, 127);
            send(value as u8)?;
            shown = Some(value as u8);
            print!("\r\x1b[K  value {value:>3}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        thread::sleep(Duration::from_millis(2));
    }

    if shown.is_some() {
        let _ = send(0);
    }
    println!();
    Ok(())
}

/// The unaccounted numbers, walked in *blocks* rather than one at a time.
///
/// This exists because of how the first LED map went wrong. `ns6 leds` sends one
/// message and asks what lit, which is exactly right for a button and exactly
/// wrong for everything else on this panel. A bar, a ring, a meter is many LEDs
/// with a number each, so the honest answer to "what did CC 96 light?" is *one
/// segment* - a single dim LED somewhere among a hundred candidates, easy to miss
/// and easier to dismiss while you are busy naming buttons. Every multi-segment
/// display on the NS6 was invisible to that method, and stayed unmapped for it.
///
/// Lighting a run of consecutive numbers together turns the same hardware into
/// something nobody could miss: a filled arc, a lit bar. Then the range is
/// narrowed by halving the block.
fn cmd_blocks(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let num = |i: usize, default: usize| -> usize {
        args.get(i).and_then(|v| v.parse().ok()).unwrap_or(default)
    };
    let midi_ch = num(0, 1).clamp(1, 16) as u8;
    let size = num(1, 8).max(1);
    // An optional `96-110`, to walk one region closely. Each of these displays
    // looks like a single number whose value is a position, so the useful shape
    // is a wide first pass to find the region and then size 1 across it.
    let (lo, hi) = match args.get(2).and_then(|a| a.split_once('-')) {
        Some((a, b)) => (
            a.trim().parse::<u8>().unwrap_or(0),
            b.trim().parse::<u8>().unwrap_or(127),
        ),
        None => (0, 127),
    };
    let mut value: i32 = std::env::var("NS6_BARS_VALUE")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(5);
    let ch = midi_ch - 1;

    let (dev, _streams) = open_for_output()?;
    install_signal_handler();

    // Blocked per status, not across both: a block that straddled the control
    // change / note boundary would be labelled with only one of them, and the
    // label is the entire output of this walk.
    let candidates: Vec<(u8, u8)> = unaccounted()
        .into_iter()
        .filter(|&(_, n)| n >= lo && n <= hi)
        .collect();
    if candidates.is_empty() {
        return Err(format!(
            "nothing unaccounted for on channel {midi_ch} in {lo}..{hi}"
        )
        .into());
    }
    let mut blocks: Vec<Vec<(u8, u8)>> = Vec::new();
    for status in [0xB0u8, 0x90] {
        let of_kind: Vec<(u8, u8)> = candidates.iter().copied().filter(|c| c.0 == status).collect();
        blocks.extend(of_kind.chunks(size).map(<[(u8, u8)]>::to_vec));
    }

    let mut found = displays_load(std::path::Path::new(DISPLAY_MAP));
    println!(
        "\nMIDI channel {midi_ch}, numbers {lo}..{hi}: {} unaccounted, in {} blocks of {size}.\n\n    \
         right / left   next block / back\n    \
         ] / [          value + 1 / - 1, to find a level that shows\n    \
         Enter          describe what is lit; it is saved at once\n    \
         q              stop\n\n\
         A button gives one lamp. A bar, ring or meter gives a run of them - that\n\
         is the thing to watch for, and what the one-at-a-time walk could not show.\n\
         The value is a *position*, not a level: it picks which segment lights, so\n\
         ] and [ walk the lit LED along the display. Value starts at {value};\n\
         127 is out of range and shows nothing, which is why the original walk\n\
         missed every one of these.\n",
        candidates.len(),
        blocks.len()
    );

    let Some(_raw) = term::RawMode::enable() else {
        return Err("ns6 bars blocks needs a terminal: it reads arrow keys".into());
    };

    let send = |block: &[(u8, u8)], level: u8| -> Result<(), device::Error> {
        let mut buf: Vec<u8> = Vec::new();
        for &(status, n) in block {
            if buf.len() + 3 > p::MIDI_OUT_PAYLOAD {
                dev.write_midi(&buf)?;
                buf.clear();
            }
            buf.extend_from_slice(&[status | ch, n, level]);
        }
        if !buf.is_empty() {
            dev.write_midi(&buf)?;
        }
        Ok(())
    };

    let label = |i: usize, value: i32| {
        let block: &Vec<(u8, u8)> = &blocks[i];
        format!(
            "[{}/{}] {} {}  value {}",
            i + 1,
            blocks.len(),
            if block[0].0 == 0xB0 { "CC" } else { "note" },
            block
                .iter()
                .map(|&(_, n)| n.to_string())
                .collect::<Vec<_>>()
                .join(","),
            value
        )
    };

    let dwell = Duration::from_millis(150);
    let mut last_step = Instant::now() - dwell;
    let mut index: usize = 0;
    let mut shown: Option<usize> = None;

    while running() {
        let pending = term::drain();
        if pending.chars.iter().any(|c| *c == 'q' || *c == 'Q') {
            break;
        }
        let mut level_delta = 0i32;
        for c in &pending.chars {
            match c {
                ']' => level_delta += 1,
                '[' => level_delta -= 1,
                _ => {}
            }
        }

        if (pending.arrow.is_some() || level_delta != 0) && last_step.elapsed() >= dwell {
            last_step = Instant::now();
            if let Some(i) = shown {
                send(&blocks[i], 0)?;
            }
            if let Some(arrow) = pending.arrow {
                match arrow {
                    term::Key::Right if shown.is_some() => index = (index + 1).min(blocks.len() - 1),
                    term::Key::Left => index = index.saturating_sub(1),
                    _ => {}
                }
            }
            value = (value + level_delta).clamp(0, 127);
            send(&blocks[index], value as u8)?;
            shown = Some(index);
            print!("\r\x1b[K  {}", label(index, value));
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }

        if pending.enter && shown.is_some() {
            let cooked = _raw.cooked();
            println!("\n  {}", label(index, value));
            print!("  what is lit? ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                let line = line.trim();
                if line.is_empty() {
                    println!("  (nothing recorded)");
                } else {
                    let block = &blocks[index];
                    let numbers: Vec<u8> = block.iter().map(|&(_, n)| n).collect();
                    // The value is part of the key. It was not, and that lost
                    // work: the platter ring shows four colours on one number,
                    // and describing the second overwrote the first because both
                    // were "CC 58 on channel 2". A number whose value means
                    // something needs a description per value, which is the
                    // whole point of these entries.
                    found.retain(|d| {
                        !(d.channel == ch
                            && d.kind == block[0].0
                            && d.numbers == numbers
                            && d.value == value as u8)
                    });
                    found.push(Display {
                        channel: ch,
                        kind: block[0].0,
                        numbers,
                        value: value as u8,
                        description: line.to_string(),
                    });
                    // Saved now rather than at exit: this walk can take the
                    // device down without warning, and a description is the
                    // expensive half of it.
                    if let Err(e) = std::fs::write(DISPLAY_MAP, displays_to_toml(&found)) {
                        eprintln!("  could not save: {e}");
                    }
                    println!("  recorded: {line}");
                }
            }
            drop(cooked);
            print!("\r\x1b[K  {}", label(index, value));
            let _ = std::io::Write::flush(&mut std::io::stdout());
            last_step = Instant::now();
        }
        thread::sleep(Duration::from_millis(2));
    }

    if let Some(i) = shown {
        let _ = send(&blocks[i], 0);
    }
    println!();
    if !found.is_empty() {
        println!("{} display(s) in {DISPLAY_MAP}", found.len());
    }
    Ok(())
}

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

/// Drive real audio in both directions, for hardware testing.
///
/// The bridge streams silence out and throws the input away, so neither
/// direction had ever been proven. This writes a sine into the isochronous OUT
/// pipe and dumps bulk IN 0x86 to a file, so both can be measured against a
/// loopback rig.
fn cmd_audio() -> Result<(), Box<dyn std::error::Error>> {
    fn env_u64(key: &str, default: u64) -> u64 {
        std::env::var(key)
            .ok()
            .and_then(|v| {
                if let Some(hex) = v.strip_prefix("0x") {
                    u64::from_str_radix(hex, 16).ok()
                } else {
                    v.parse().ok()
                }
            })
            .unwrap_or(default)
    }

    let secs = std::env::args()
        .nth(2)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(6);
    let hz = env_u64("NS6_TONE_HZ", 1000);
    let mask = env_u64("NS6_TONE_CH", 0xF);
    let amp = env_u64("NS6_TONE_AMP", 50);
    let dump = std::env::var("NS6_PCM_DUMP").ok();

    iso::TONE_HZ.store(hz, Ordering::Relaxed);
    iso::TONE_MASK.store(mask, Ordering::Relaxed);
    iso::TONE_AMP.store(amp, Ordering::Relaxed);
    iso::TONE_SPREAD.store(std::env::var("NS6_TONE_SPREAD").is_ok(), Ordering::Relaxed);
    iso::TONE_ON.store(std::env::var("NS6_NO_TONE").is_err(), Ordering::Relaxed);

    println!(
        "tone: {hz} Hz, {amp}% FS, channel mask 0x{mask:X}, {} s",
        secs
    );

    let (dev, _streams) = open_for_output()?;
    install_signal_handler();
    if let Ok(path) = std::env::var("NS6_OUT_DUMP") {
        iso::dump_out_to(&path)?;
        println!("dumping wire frames to {path}");
    }

    if let Some(path) = &dump {
        iso::capture_pcm_to(path)?;
        println!("capturing bulk IN 0x86 to {path}");
    }

    let start = Instant::now();
    while running() && start.elapsed() < Duration::from_secs(secs) {
        if iso::DEVICE_GONE.load(Ordering::Relaxed) {
            eprintln!("device left the bus");
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    iso::stop_pcm_capture();
    iso::TONE_ON.store(false, Ordering::Relaxed);

    println!(
        "iso out: {} ok / {} err   bulk in: {} xfers / {} B   captured: {} B",
        iso::ISO_OUT_OK.load(Ordering::Relaxed),
        iso::ISO_OUT_ERR.load(Ordering::Relaxed),
        iso::BULK_IN_OK.load(Ordering::Relaxed),
        iso::BULK_IN_BYTES.load(Ordering::Relaxed),
        iso::PCM_DUMP_BYTES.load(Ordering::Relaxed),
    );
    drop(dev);
    Ok(())
}

/// Set when the playback source hits end of file.
static PLAY_EOF: AtomicBool = AtomicBool::new(false);

/// Where audio is read from and written to, when audio is running.
///
/// Both ends speak plain 32-bit little-endian stereo at 44.1 kHz: the device's
/// own formats - four 24-bit output channels grouped by side, and an input that
/// is a raw I2S bitstream - are awkward to hand to anything else, while s32le
/// stereo is what `pw-cat`, `sox` and PipeWire's pipe modules all take without
/// argument.
struct AudioPaths {
    play: Option<String>,
    rec: Option<String>,
}

impl AudioPaths {
    /// Paths for the bridge, which takes them from the environment because its
    /// positional arguments belong to the MIDI side.
    fn from_env() -> Self {
        Self {
            play: std::env::var("NS6_PLAY").ok(),
            rec: std::env::var("NS6_REC").ok(),
        }
    }

    fn any(&self) -> bool {
        self.play.is_some() || self.rec.is_some()
    }
}

/// Read audio from `path` and queue it for the device.
///
/// A FIFO is a sink that comes and goes - PipeWire closes its end whenever the
/// sink suspends - so end of file on one is a wait, not a stop.
fn spawn_play(path: String) -> thread::JoinHandle<()> {
    use std::io::Read;

    let out_to = audio::Out::from_env(std::env::var("NS6_OUT_PAIRS").ok().as_deref());
    // How much audio to keep queued ahead of the device: latency against
    // robustness. 40 ms rides out a scheduling hiccup without being felt.
    let target_ms = std::env::var("NS6_PLAY_MS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(40);
    let gain = std::env::var("NS6_GAIN")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(1.0);
    let queue_limit = 44100 / 1000 * target_ms * audio::OUT_FRAME;

    thread::spawn(move || {
        let fifo = is_fifo(&path);
        // A FIFO has to be opened non-blocking: with no writer yet, a plain
        // open blocks forever, which would also hang shutdown.
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        if fifo {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NONBLOCK);
        }
        let mut src: Box<dyn Read> = if path == "-" {
            Box::new(std::io::stdin())
        } else {
            match opts.open(&path) {
                Ok(f) => Box::new(f),
                Err(e) => {
                    eprintln!("cannot read {path}: {e}");
                    return;
                }
            }
        };
        audio::PLAY_ON.store(true, Ordering::Relaxed);

        let mut buf = vec![0u8; 8 * 1024];
        let mut carry: Vec<u8> = Vec::new();
        let mut wire = Vec::with_capacity(8 * 1024 * 3 / 2);
        while running() {
            // Reading faster than the device plays would swallow a file whole
            // and drop most of it at the queue's own limit.
            if audio::play_queued() > queue_limit {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let n = match src.read(&mut buf) {
                // On a FIFO, end of file means the writer closed - PipeWire
                // suspending the sink - and the next one may still turn up. The
                // read end stays valid, so there is nothing to reopen.
                Ok(0) if fifo => {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            carry.extend_from_slice(&buf[..n]);
            wire.clear();
            let used = audio::encode_host(&carry, gain, out_to, &mut wire);
            carry.drain(..used);
            audio::push_play(&wire);
        }
        PLAY_EOF.store(true, Ordering::Relaxed);
    })
}

/// Decode the device's audio input and write it to `path`.
///
/// Opening a FIFO for writing fails with `ENXIO` until something is reading it,
/// and PipeWire only reads while the source is in use. Decoding 5.6 MB/s of
/// bitstream into a pipe nobody holds open is pure waste, so capture stays off
/// until there is a reader.
fn spawn_rec(path: String) -> thread::JoinHandle<()> {
    use std::io::Write;

    thread::spawn(move || {
        let fifo = is_fifo(&path);
        let open = || -> Option<Box<dyn Write>> {
            if path == "-" {
                return Some(Box::new(std::io::stdout()));
            }
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true);
            if fifo {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_NONBLOCK);
            } else {
                opts.create(true).truncate(true);
            }
            match opts.open(&path) {
                Ok(f) => Some(Box::new(f)),
                // ENXIO is "no reader yet", which is not an error.
                Err(e) if fifo && e.raw_os_error() == Some(libc::ENXIO) => None,
                Err(e) => {
                    eprintln!("cannot write {path}: {e}");
                    None
                }
            }
        };

        let mut dst: Option<Box<dyn Write>> = None;
        let mut out = Vec::new();
        while running() {
            if dst.is_none() {
                audio::REC_ON.store(false, Ordering::Relaxed);
                dst = open();
                if dst.is_none() {
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
                // Start the new listener on a clean stream.
                iso::reset_rec_align();
                let _ = audio::drain_rec();
                audio::REC_ON.store(true, Ordering::Relaxed);
            }
            let pcm = audio::drain_rec();
            if pcm.is_empty() {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            out.clear();
            audio::to_host(&pcm, &mut out);
            match dst.as_mut().unwrap().write_all(&out) {
                Ok(()) => {}
                // The reader is not keeping up; newest audio wins.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => dst = None,
            }
        }
        if let Some(d) = dst.as_mut() {
            let _ = d.flush();
        }
        audio::REC_ON.store(false, Ordering::Relaxed);
    })
}

fn is_fifo(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| std::os::unix::fs::FileTypeExt::is_fifo(&m.file_type()))
        .unwrap_or(false)
}

/// Frame counters at the last status print, for the rate estimate.
/// What the device says its clock is doing, and whether the isochronous OUT
/// pipe is keeping up with it.
///
/// `asked` is the device's own count of the frames it wants, read off the
/// feedback endpoint; `sent` is what the OUT pipe actually sized itself for.
/// The two are one clock only for as long as the difference between them stays
/// put. Left to itself it does not: see `iso::out_rate`, where answering the
/// request is the whole of the fix for a tone that breaks up partway through.
fn clock_status() -> String {
    let asked = iso::FB_FRAMES.load(Ordering::Relaxed);
    let sent = iso::OUT_FRAMES.load(Ordering::Relaxed);
    let packets = iso::FB_PACKETS.load(Ordering::Relaxed);
    let rate = if packets > 0 {
        asked as f64 * 1000.0 / packets as f64
    } else {
        0.0
    };
    format!(
        "clock: device asks {rate:.2} Hz over {packets} ms, sending {:.2} Hz; sent - asked = {} frames",
        iso::out_rate(p::SAMPLE_RATE as u64) as f64 / iso::RATE_FRAC as f64,
        sent as i64 - asked as i64,
    )
}

static LAST_STATUS: Mutex<Option<(Instant, u64, u64)>> = Mutex::new(None);

/// One line of audio state, for the periodic status print.
///
/// The rates are the device's own clock, not ours: frames it took and frames it
/// sent between two prints. They are worth watching, because nothing here
/// resamples - if the device runs faster than whatever is feeding it, the
/// difference comes out as underruns.
fn audio_status() -> String {
    let (out, inn) = (
        audio::PLAY_FRAMES.load(Ordering::Relaxed),
        audio::REC_FRAMES.load(Ordering::Relaxed),
    );
    let now = Instant::now();
    let mut rates = String::new();
    if let Ok(mut last) = LAST_STATUS.lock() {
        if let Some((t, po, pi)) = *last {
            let secs = now.duration_since(t).as_secs_f64();
            if secs > 0.5 {
                rates = format!(
                    "  [{:.0} Hz out, {:.0} Hz in]",
                    (out - po) as f64 / secs,
                    (inn - pi) as f64 / secs
                );
            }
        }
        *last = Some((now, out, inn));
    }
    format!(
        "audio: {out} frames out ({} underruns, {} dropped), {inn} frames in ({} dropped){rates}  queue {:.0} ms",
        audio::PLAY_UNDERRUNS.load(Ordering::Relaxed),
        // Drops mean the host is running ahead of the device's own clock, so
        // this counter is the drift made visible: nothing here resamples.
        audio::PLAY_DROPS.load(Ordering::Relaxed),
        audio::REC_OVERRUNS.load(Ordering::Relaxed),
        // Where the rate matching has settled. It should sit near NS6_PLAY_MS
        // and stay there; a queue walking towards 250 ms or towards nothing is
        // the clock drift going uncorrected.
        audio::play_queued() as f64 / audio::OUT_FRAME as f64 / 44.1,
    )
}

/// Stream audio without the MIDI bridge: `ns6 play`, `ns6 rec`, `ns6 duplex`.
fn cmd_stream(play: bool, rec: bool) -> Result<(), Box<dyn std::error::Error>> {
    // In duplex the two paths are separate: `ns6 duplex <play> <capture>`.
    let first = std::env::args().nth(2).unwrap_or_else(|| "-".into());
    let paths = AudioPaths {
        play: play.then(|| first.clone()),
        rec: rec.then(|| {
            if play {
                std::env::args().nth(3).unwrap_or_else(|| "-".into())
            } else {
                first.clone()
            }
        }),
    };

    audio::PLAY_ON.store(false, Ordering::Relaxed);
    audio::REC_ON.store(false, Ordering::Relaxed);
    iso::reset_rec_align();

    let (dev, _streams) = open_for_output()?;
    install_signal_handler();
    eprintln!(
        "streaming {} at 44100 Hz, s32le stereo",
        match (play, rec) {
            (true, true) => "both ways",
            (true, false) => "to the device",
            _ => "from the device",
        }
    );

    let mut threads = Vec::new();
    if let Some(p) = paths.play {
        threads.push(spawn_play(p));
    }
    if let Some(p) = paths.rec {
        threads.push(spawn_rec(p));
    }

    let mut last = Instant::now();
    while running() && !iso::DEVICE_GONE.load(Ordering::Relaxed) {
        // A file read to the end and played out is done; a live source - a pipe
        // that is still open - is not.
        if PLAY_EOF.load(Ordering::Relaxed) && audio::play_queued() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(100));
        if last.elapsed() >= Duration::from_secs(5) {
            last = Instant::now();
            eprintln!("  {}", audio_status());
        }
    }
    audio::PLAY_ON.store(false, Ordering::Relaxed);
    audio::REC_ON.store(false, Ordering::Relaxed);
    eprintln!("  {}", audio_status());
    drop(dev);
    Ok(())
}

#[cfg(test)]
mod display_map {
    use super::*;

    /// The display map is written after every description because the walk can
    /// take the device down mid-way, so reading it back has to be exact - it is
    /// the only record of a session at the panel.
    #[test]
    fn a_written_display_map_reads_back_identically() {
        let original = vec![
            Display {
                channel: 0,
                kind: 0xB0,
                // Sparse on purpose: this is what a block actually is.
                numbers: vec![0, 1, 2, 6, 19, 20, 29, 30],
                value: 5,
                description: "serato strip, left half".into(),
            },
            Display {
                channel: 1,
                kind: 0x90,
                numbers: vec![8],
                value: 3,
                description: "fx a param ring, \"outer\" arc".into(),
            },
        ];
        let text = displays_to_toml(&original);
        let path = std::env::temp_dir().join("ns6-display-map-roundtrip.toml");
        std::fs::write(&path, &text).unwrap();
        let back = displays_load(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(back.len(), 2, "wrong number of displays read back");
        for (a, b) in original.iter().zip(back.iter()) {
            assert_eq!(a.channel, b.channel);
            assert_eq!(a.kind, b.kind);
            // The sparse set must survive exactly. A range would not have.
            assert_eq!(a.numbers, b.numbers);
            assert_eq!(a.value, b.value);
        }
        // Descriptions carry quotes, so the escaping has to survive a round trip
        // even though the reader is deliberately lenient about everything else.
        assert_eq!(back[0].description, "serato strip, left half");
        assert_eq!(displays_to_toml(&back), text);
    }
}
