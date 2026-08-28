//! ns6 - userspace MIDI driver for the Numark NS6 DJ controller on Linux.
//!
//! The NS6 exposes only vendor-specific USB interfaces, so no kernel driver
//! binds it and no ALSA MIDI port appears. This talks the device's Ploytec
//! protocol over libusb and publishes an ALSA sequencer port instead.
//!
//! See `docs/PROTOCOL.md` for how the protocol was derived.

mod checklist;
mod device;
mod iso;
mod learn;
mod midi;
mod protocol;

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
             ns6 probe     report device state and sweep bulk OUT configurations\n    \
             ns6 learn     name the control surface: move one control at a time\n    \
             ns6 learn --guided\n                  \
                           walk the NS6's panel, one prompt at a time, and write\n                  \
                           the result to ns6-surface.toml\n    \
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
        "learn" => cmd_run(match std::env::args().nth(2).as_deref() {
            Some("--guided") | Some("-g") => Mode::Guided,
            _ => Mode::Learn,
        }),
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
    /// Also walk the panel checklist and write out a surface map.
    Guided,
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
    let mut last_beat = Instant::now();
    let mut warned = false;
    let mut surface = learn::Surface::new();
    let mut last_named: Option<learn::Key> = None;
    let mut quiet_since = Instant::now();
    let mut guided: Option<learn::Guided> = None;
    // Typed lines, read off a thread so the stream is never blocked on stdin.
    let (key_tx, key_rx) = mpsc::channel::<String>();

    if mode == Mode::Learn {
        println!(
            "learn mode: move ONE control, then wait a moment. Everything the device\n\
             announces at startup is listed first; after that each line is a control\n\
             you just moved. Ctrl-C prints the full table.\n"
        );
    }
    if mode == Mode::Guided {
        // The device announces every control's resting position the moment it
        // starts, and that burst must be absorbed before prompting - otherwise
        // the first answer is whatever happened to arrive last.
        println!("waiting for the device to finish announcing its state...");
        let settle = Instant::now();
        while running() && settle.elapsed() < Duration::from_secs(3) {
            if let Ok(mut q) = iso::MIDI_IN.lock() {
                if !q.is_empty() {
                    surface.feed(&q);
                    q.clear();
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        println!(
            "{}\n\n\
             Walking the panel. Move each control as asked and it will be recorded\n\
             automatically. Press Enter to skip one that does not exist or is\n\
             analogue-only, or type q then Enter to stop and write what you have.",
            surface.report().lines().next().unwrap_or("nothing announced")
        );
        let g = learn::Guided::new(&surface);
        g.prompt();
        guided = Some(g);
        thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                if key_tx.send(line.trim().to_string()).is_err() {
                    break;
                }
                line.clear();
            }
        });
    }

    while running() {
        // Surface events from the device -> ALSA.
        if let Ok(mut q) = iso::MIDI_IN.lock() {
            if !q.is_empty() {
                if learn {
                    for c in surface.feed(&q) {
                        if let Some(g) = guided.as_mut() {
                            g.observe(&c);
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

        if let Some(g) = guided.as_mut() {
            if g.poll() {
                g.prompt();
            }
            match key_rx.try_recv() {
                Ok(line) if line.eq_ignore_ascii_case("q") => break,
                Ok(_) => {
                    g.skip();
                    g.prompt();
                }
                Err(_) => {}
            }
            if g.done() {
                break;
            }
        }

        thread::sleep(Duration::from_millis(2));

        if guided.is_none() && last_beat.elapsed() >= Duration::from_secs(5) {
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

    if let Some(g) = guided.as_ref() {
        let path = std::path::Path::new("ns6-surface.toml");
        match std::fs::write(path, g.to_toml()) {
            Ok(()) => println!("\nwrote {} controls to {}", g.answers.len(), path.display()),
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
