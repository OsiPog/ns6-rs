//! ns6 - userspace MIDI driver for the Numark NS6 DJ controller on Linux.
//!
//! The NS6 exposes only vendor-specific USB interfaces, so no kernel driver
//! binds it and no ALSA MIDI port appears. This talks the device's Ploytec
//! protocol over libusb and publishes an ALSA sequencer port instead.
//!
//! See `docs/PROTOCOL.md` for how the protocol was derived.

mod device;
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
             ns6 test      emit synthetic MIDI on the ALSA port, no hardware needed\n\
         \n\
         The device's usbfs node must be writable; see udev/70-numark-ns6.rules."
    );
    std::process::exit(2)
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let result = match arg.as_str() {
        "run" => cmd_run(),
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

fn cmd_run() -> Result<(), Box<dyn std::error::Error>> {
    let mut port = midi::MidiPort::open()?;
    port.describe();

    let dev = Arc::new(Ns6::open()?);
    dev.start()?;

    let stats = Arc::new(Stats::default());
    // Threads watch this; main mirrors the global signal flag into it.
    let alive = Arc::new(AtomicBool::new(true));
    install_signal_handler();

    // MIDI destined for the controller (LEDs), consumed by the PCM out thread.
    let (midi_tx, midi_rx) = mpsc::channel::<u8>();
    // MIDI arriving from the controller, published on the ALSA port by main.
    // MidiPort holds a raw snd_midi_event_t and so cannot cross threads.
    let (surface_tx, surface_rx) = mpsc::channel::<Vec<u8>>();

    let mut threads = Vec::new();
    threads.push(thread::spawn({
        let (dev, stats, running) = (dev.clone(), stats.clone(), alive.clone());
        move || device::run_pcm_out(dev, stats, running, midi_rx)
    }));
    threads.push(thread::spawn({
        let (dev, stats, running) = (dev.clone(), stats.clone(), alive.clone());
        move || {
            device::run_midi_in(dev, stats, running, |bytes| {
                let _ = surface_tx.send(bytes.to_vec());
            })
        }
    }));
    threads.push(thread::spawn({
        let (dev, stats, running) = (dev.clone(), stats.clone(), alive.clone());
        move || device::run_pcm_in(dev, stats, running)
    }));

    println!(
        "\nbridge running - connect Mixxx to \"{}\". Ctrl-C to stop.\n",
        midi::CLIENT_NAME
    );

    let mut feedback = Vec::new();
    let mut last_beat = Instant::now();
    let mut warned = false;

    while running() {
        // Surface events from the device -> ALSA.
        for chunk in surface_rx.try_iter() {
            port.send_bytes(&chunk);
        }

        // LED feedback from the host -> device.
        feedback.clear();
        port.recv_bytes(&mut feedback);
        for byte in feedback.iter().copied() {
            let _ = midi_tx.send(byte);
        }

        thread::sleep(Duration::from_millis(2));

        if last_beat.elapsed() >= Duration::from_secs(5) {
            last_beat = Instant::now();
            let ok = stats.out_ok.load(Ordering::Relaxed);
            let err = stats.out_err.load(Ordering::Relaxed);
            println!(
                "  pcm-out[ok:{ok} err:{err}]  pcm-in:{}  midi-in:{} bytes  midi-out:{} bytes",
                stats.pcm_in.load(Ordering::Relaxed),
                stats.midi_in_bytes.load(Ordering::Relaxed),
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

    println!("\nshutting down");
    alive.store(false, Ordering::Relaxed);
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
    dev.set_sample_rate(p::SAMPLE_RATE)?;
    let (before, after) = dev.arm()?;
    println!("armed    : 0x{before:02X} -> 0x{after:02X}");
    dev.clear_halts();

    println!(
        "\nsweeping bulk OUT 0x{:02X} (5 attempts each)",
        p::EP_PCM_OUT
    );
    println!("{:<9} {:<10} {:<6} accepted", "size", "framing", "ctrl");

    let sizes = [
        512usize,
        1024,
        2048,
        4096,
        8192,
        16384,
        65536,
        p::OUT_XFER_SIZE,
    ];
    let mut any = false;

    for size in sizes {
        for (label, framed, ctrl) in [
            ("zeros", false, 0x00u8),
            ("ploytec", true, p::CTRL_IDLE),
            ("ploytec", true, 0xFF),
        ] {
            let mut buf = vec![0u8; size];
            if framed {
                for block in buf.chunks_exact_mut(p::BLOCK) {
                    block[p::MIDI_SLOT] = p::MIDI_IDLE;
                    block[p::CTRL_SLOT] = ctrl;
                }
            }

            let mut ok = 0;
            for _ in 0..5 {
                if dev.write_pcm(&buf).is_ok() {
                    ok += 1;
                }
            }
            println!("{size:<9} {label:<10} 0x{ctrl:02X}   {ok}/5");
            if ok > 0 {
                any = true;
                println!("  *** accepted - draining MIDI pipe for 2s");
                drain_midi(&dev, Duration::from_secs(2));
            }
        }
    }

    if !any {
        println!(
            "\nNo bulk OUT configuration was accepted. The device is not entering its\n\
             streaming state, so something earlier in the sequence is still missing."
        );
    }
    Ok(())
}

fn drain_midi(dev: &Ns6, how_long: Duration) {
    let deadline = Instant::now() + how_long;
    let mut raw = vec![0u8; p::BLOCK];
    let mut clean = Vec::new();

    while Instant::now() < deadline {
        if let Ok(n) = dev.read_bulk(p::EP_MIDI_IN, &mut raw) {
            if n == 0 {
                continue;
            }
            clean.clear();
            p::strip_midi_filler(&raw[..n], &mut clean);
            if !clean.is_empty() {
                println!(
                    "      MIDI: {}",
                    clean
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
    }
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
