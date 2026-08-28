//! ns6 - userspace MIDI driver for the Numark NS6 DJ controller on Linux.
//!
//! The NS6 exposes only vendor-specific USB interfaces, so no kernel driver
//! binds it and no ALSA MIDI port appears. This talks the device's Ploytec
//! protocol over libusb and publishes an ALSA sequencer port instead.
//!
//! See `docs/PROTOCOL.md` for how the protocol was derived.

mod device;
mod iso;
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

    // Submit in the order the Windows driver does, which a usbmon capture of
    // it shows to be: audio in, MIDI in, then the isochronous streams. We had
    // MIDI last, after both iso streams were already running.
    let _audio_in = unsafe {
        iso::BulkInStream::start(dev.raw_handle(), p::EP_PCM_IN, p::AUDIO_IN_XFER, 1, false)
    }
    .map_err(|e| format!("audio in queue: libusb error {e}"))?;
    thread::sleep(Duration::from_micros(900));
    let _midi_in =
        unsafe { iso::BulkInStream::start(dev.raw_handle(), p::EP_MIDI_IN, p::BLOCK, 1, true) }
            .map_err(|e| format!("MIDI in queue: libusb error {e}"))?;

    // The vendor driver waits ~3.5ms here and ~8ms before the isochronous OUT
    // stream, letting the IN side settle first. We were firing everything
    // within ~100us of the last control transfer.
    thread::sleep(Duration::from_micros(3500));
    let _iso_in = unsafe {
        iso::IsoStream::start(
            dev.raw_handle(),
            p::EP_ISO_IN,
            p::ISO_IN_PACKET,
            p::ISO_PACKETS_PER_XFER,
            4,
            None,
        )
    }
    .map_err(|e| format!("iso IN stream: libusb error {e}"))?;
    thread::sleep(Duration::from_millis(8));
    let _iso_out = unsafe {
        iso::IsoStream::start(
            dev.raw_handle(),
            p::EP_ISO_OUT,
            p::ISO_OUT_PACKET,
            p::ISO_PACKETS_PER_XFER,
            p::ISO_XFERS,
            Some((p::OUT_FRAME_BYTES, p::SAMPLE_RATE as u64)),
        )
    }
    .map_err(|e| format!("iso OUT stream: libusb error {e}"))?;
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
    // MIDI and audio in come off async bulk queues, drained in the main loop.
    thread::sleep(Duration::from_micros(900));
    let _midi_in =
        unsafe { iso::BulkInStream::start(dev.raw_handle(), p::EP_MIDI_IN, p::BLOCK, 1, true) }
            .map_err(|e| format!("MIDI in queue: libusb error {e}"))?;

    // The vendor driver waits ~3.5ms here and ~8ms before the isochronous OUT
    // stream, letting the IN side settle first. We were firing everything
    // within ~100us of the last control transfer.
    thread::sleep(Duration::from_micros(3500));
    let _audio_in = unsafe {
        iso::BulkInStream::start(dev.raw_handle(), p::EP_PCM_IN, p::AUDIO_IN_XFER, 1, false)
    }
    .map_err(|e| format!("audio in queue: libusb error {e}"))?;

    println!(
        "\nbridge running - connect Mixxx to \"{}\". Ctrl-C to stop.\n",
        midi::CLIENT_NAME
    );

    let mut feedback = Vec::new();
    let mut last_beat = Instant::now();
    let mut warned = false;

    while running() {
        // Surface events from the device -> ALSA.
        if let Ok(mut q) = iso::MIDI_IN.lock() {
            if !q.is_empty() {
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

        thread::sleep(Duration::from_millis(2));

        if last_beat.elapsed() >= Duration::from_secs(5) {
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
