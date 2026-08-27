//! USB transport for the Numark NS6.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use rusb::{DeviceHandle, GlobalContext};

use crate::protocol as p;

/// How long to wait on a single bulk transfer.
const XFER_TIMEOUT: Duration = Duration::from_millis(500);
const CTRL_TIMEOUT: Duration = Duration::from_millis(2000);

#[derive(Debug)]
pub enum Error {
    NotFound,
    Usb(rusb::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(
                f,
                "Numark NS6 ({:04x}:{:04x}) not found, or the usbfs node is not accessible.\n\
                 If it is plugged in, grant access with:\n  \
                 sudo chown $USER /dev/bus/usb/<bus>/<dev>\n\
                 or install udev/70-numark-ns6.rules for a durable fix.",
                p::VID,
                p::PID
            ),
            Error::Usb(e) => write!(f, "USB error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusb::Error> for Error {
    fn from(e: rusb::Error) -> Self {
        Error::Usb(e)
    }
}

/// Counters shared with the caller for progress reporting.
#[derive(Default)]
pub struct Stats {
    pub out_ok: AtomicU64,
    pub out_err: AtomicU64,
    pub midi_out_bytes: AtomicU64,
}

pub struct Ns6 {
    handle: DeviceHandle<GlobalContext>,
}

impl Ns6 {
    /// Open the device and bring it to a streaming-ready state.
    ///
    /// Interfaces are claimed *before* any control transfer. Doing it the other
    /// way round - control transfers or `set_configuration` first - leaves
    /// `claim_interface` returning `Busy` permanently, with no kernel driver
    /// bound and no process holding the node.
    pub fn open() -> Result<Self, Error> {
        let handle = rusb::open_device_with_vid_pid(p::VID, p::PID).ok_or(Error::NotFound)?;
        handle.set_auto_detach_kernel_driver(true).ok();

        // SET_CONFIGURATION is off by default. The Windows driver issues
        // SELECT_CONFIGURATION during startup, but sending it here made no
        // difference to streaming and repeating it knocked the device off the
        // bus entirely, requiring a physical replug. Opt in with NS6_SET_CONFIG.
        if std::env::var("NS6_SET_CONFIG").is_ok() {
            if let Err(e) = handle.set_active_configuration(1) {
                eprintln!("set_configuration(1): {e} (continuing)");
            }
        }

        // NS6_IFACES lets a single binary try different claim sets while
        // hunting for whatever the device is waiting on.
        let ifaces: Vec<u8> = match std::env::var("NS6_IFACES") {
            Ok(v) => v
                .split(',')
                .filter_map(|x| x.trim().parse::<u8>().ok())
                .collect(),
            Err(_) => p::INTERFACES.to_vec(),
        };
        for iface in ifaces {
            handle.claim_interface(iface)?;
            // Straight to alt 1. The Windows driver selects alternate settings
            // via URB_FUNCTION_SELECT_CONFIGURATION and never visits alt 0, so
            // the cycle through it was speculative and made no difference.
            handle.set_alternate_setting(iface, p::ALT_SETTING)?;
        }

        Ok(Self { handle })
    }

    /// Read the 15-byte firmware version block (vendor request `'V'`).
    pub fn firmware(&self) -> Result<Vec<u8>, Error> {
        // The driver's second read uses wLength 5, not 15.
        let mut buf = [0u8; 5];
        let n = self.handle.read_control(
            p::VENDOR_IN,
            p::CMD_FIRMWARE,
            0,
            0,
            &mut buf,
            CTRL_TIMEOUT,
        )?;
        Ok(buf[..n].to_vec())
    }

    /// Read the hardware status register (vendor request `'I'`, register 0).
    pub fn status(&self) -> Result<u8, Error> {
        let mut buf = [0u8; 1];
        self.handle.read_control(
            p::VENDOR_IN,
            p::CMD_STATUS,
            0,
            p::REG_STATUS,
            &mut buf,
            CTRL_TIMEOUT,
        )?;
        Ok(buf[0])
    }

    /// Read back the sample rate the device reports for an endpoint (`GET_CUR`).
    ///
    /// The vendor driver does this after setting the rate; a rate that does not
    /// stick is a good early indicator that the endpoint is not configured the
    /// way the device expects.
    pub fn get_sample_rate(&self, ep: u8) -> Result<u32, Error> {
        let mut buf = [0u8; 3];
        self.handle.read_control(
            0xA2, // class, IN, recipient = endpoint
            0x81, // GET_CUR
            p::SET_CUR_VALUE,
            ep as u16,
            &mut buf,
            CTRL_TIMEOUT,
        )?;
        Ok(u32::from(buf[0]) | u32::from(buf[1]) << 8 | u32::from(buf[2]) << 16)
    }

    /// Set the sample rate, reporting the result per endpoint.
    pub fn set_sample_rate_verbose(&self, rate: u32) {
        for ep in [p::EP_ISO_OUT, p::EP_ISO_IN, p::EP_PCM_IN, p::EP_MIDI_OUT] {
            let bytes = p::encode_rate(rate);
            let set = self.handle.write_control(
                p::SET_CUR_TYPE,
                p::SET_CUR_REQ,
                p::SET_CUR_VALUE,
                ep as u16,
                &bytes,
                CTRL_TIMEOUT,
            );
            let got = self.get_sample_rate(ep);
            println!(
                "  ep 0x{ep:02X}: SET_CUR {} , GET_CUR {}",
                match set {
                    Ok(_) => "ok".to_string(),
                    Err(e) => format!("{e:?}"),
                },
                match got {
                    Ok(r) => format!("{r} Hz"),
                    Err(e) => format!("{e:?}"),
                }
            );
        }
    }

    /// Arm the device: read the status register and write it back with bit 5 set.
    ///
    /// Returns `(before, after)`. On this hardware the register goes `0x12` ->
    /// `0x32` and persists across power cycles. It is level-triggered, so
    /// re-arming an already-armed device is a no-op.
    pub fn arm(&self) -> Result<(u8, u8), Error> {
        let mut before = self.status()?;

        // In the Windows capture the device reads back 0x12 when the driver
        // arms it, so the write performs a real 0->1 transition on bit 5. The
        // register survives USB resets, so on a machine that has already run
        // this driver it reads 0x32 and the write is a no-op. Clear the bit
        // first so the device sees the same edge the vendor driver produces.
        if before & p::ARM_BIT != 0 {
            let cleared = before & !p::ARM_BIT;
            let wvalue = (cleared as i8) as i16 as u16;
            self.handle.write_control(
                p::VENDOR_OUT,
                p::CMD_STATUS,
                wvalue,
                p::REG_STATUS,
                &[],
                CTRL_TIMEOUT,
            )?;
            before = self.status()?;
        }
        let wvalue = p::arm_wvalue(before);
        self.handle.write_control(
            p::VENDOR_OUT,
            p::CMD_STATUS,
            wvalue,
            p::REG_STATUS,
            &[],
            CTRL_TIMEOUT,
        )?;
        let after = self.status()?;
        Ok((before, after))
    }

    /// Clear any halt condition left on the streaming endpoints.
    ///
    /// Defensive: a halted pipe is indistinguishable from a device that is simply
    /// ignoring us, and earlier failed transfers can leave one behind.
    pub fn clear_halts(&self) {
        for ep in [p::EP_MIDI_OUT, p::EP_MIDI_IN, p::EP_PCM_IN] {
            let _ = self.handle.clear_halt(ep);
        }
    }

    /// Perform the exact startup sequence captured from the Windows driver.
    ///
    /// Taken verbatim from a USBPcap trace of ns6_usb.sys bringing the device up
    /// (frames 1246-1285 of `captures/ns6.pcap`):
    ///
    /// ```text
    /// 'V' read, wLength 16
    /// 'V' read, wLength 5
    /// 'I' read status
    /// GET_CUR rate, wIndex 0
    /// SET_CUR 44100 -> 0x86, 0x02, 0x86, 0x02, 0x86   (five times, alternating)
    /// GET_CUR rate, wIndex 0x86
    /// 'I' read status
    /// 'I' write status | 0x30      (arm)
    /// ABORT_PIPE + SYNC_RESET_PIPE_AND_CLEAR_STALL on 0x86
    /// ```
    ///
    /// The repeated alternating SET_CUR is a documented quirk of this chipset
    /// family, not an accident of the capture.
    pub fn start(&self) -> Result<(), Error> {
        // The Windows driver re-reads descriptors over the wire during startup
        // even though the host has them cached. Off by default: it is the last
        // remaining difference from the driver's control traffic, but it made no
        // difference here. Opt in with NS6_DESCRIPTORS.
        if std::env::var("NS6_DESCRIPTORS").is_ok() {
            let mut dev_desc = [0u8; 18];
            let _ = self
                .handle
                .read_control(0x80, 0x06, 0x0100, 0, &mut dev_desc, CTRL_TIMEOUT);
            let mut cfg = [0u8; 512];
            for _ in 0..3 {
                let _ = self
                    .handle
                    .read_control(0x80, 0x06, 0x0200, 0, &mut cfg, CTRL_TIMEOUT);
            }
            let _ = self
                .handle
                .read_control(0x80, 0x06, 0x0100, 0, &mut dev_desc, CTRL_TIMEOUT);
        }

        // Firmware is read twice, at two different lengths.
        let mut wide = [0u8; 16];
        let _ =
            self.handle
                .read_control(p::VENDOR_IN, p::CMD_FIRMWARE, 0, 0, &mut wide, CTRL_TIMEOUT);
        let fw = self.firmware()?;
        println!(
            "firmware: {}",
            fw.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        );

        let _ = self.status()?;

        // Rate is read with wIndex 0 first, then set five times alternating
        // between the audio IN and isochronous OUT endpoints.
        let _ = self.get_sample_rate_at(0);
        let bytes = p::encode_rate(p::SAMPLE_RATE);
        for ep in [
            p::EP_PCM_IN,
            p::EP_ISO_OUT,
            p::EP_PCM_IN,
            p::EP_ISO_OUT,
            p::EP_PCM_IN,
        ] {
            self.handle.write_control(
                p::SET_CUR_TYPE,
                p::SET_CUR_REQ,
                p::SET_CUR_VALUE,
                ep as u16,
                &bytes,
                CTRL_TIMEOUT,
            )?;
        }
        match self.get_sample_rate(p::EP_PCM_IN) {
            Ok(r) => println!("sample rate: {r} Hz"),
            Err(e) => println!("sample rate read back failed: {e}"),
        }

        let (before, after) = self.arm()?;
        println!("armed: status 0x{before:02X} -> 0x{after:02X}");
        if after & p::ARM_BIT == 0 {
            eprintln!("warning: arm bit did not stick (status 0x{after:02X})");
        }

        // No CLEAR_FEATURE here. The Windows driver's ABORT_PIPE and
        // SYNC_RESET_PIPE_AND_CLEAR_STALL are host-side operations that put no
        // request on the wire - its control traffic contains no bmRequestType
        // 0x02 at all - so issuing one is a difference from the driver, not a
        // match to it. Set NS6_CLEAR_HALT=1 to restore the old behaviour.
        if std::env::var("NS6_CLEAR_HALT").is_ok() {
            let _ = self.handle.clear_halt(p::EP_PCM_IN);
        }
        Ok(())
    }

    /// `GET_CUR` with an explicit wIndex, used for the driver's initial
    /// wIndex-0 read.
    pub fn get_sample_rate_at(&self, windex: u16) -> Result<u32, Error> {
        let mut buf = [0u8; 3];
        self.handle
            .read_control(0xA2, 0x81, p::SET_CUR_VALUE, windex, &mut buf, CTRL_TIMEOUT)?;
        Ok(u32::from(buf[0]) | u32::from(buf[1]) << 8 | u32::from(buf[2]) << 16)
    }

    /// Write raw MIDI bytes to the controller (LEDs and display feedback).
    pub fn write_midi(&self, buf: &[u8]) -> Result<usize, Error> {
        Ok(self.handle.write_bulk(p::EP_MIDI_OUT, buf, XFER_TIMEOUT)?)
    }

    /// Raw libusb handle, for the isochronous streams.
    ///
    /// `rusb` has no isochronous API, so `iso::IsoStream` drives libusb directly.
    pub fn raw_handle(&self) -> *mut rusb::ffi::libusb_device_handle {
        self.handle.as_raw()
    }
}

/// Forward MIDI from the host to the controller over the bulk OUT pipe.
///
/// Audio output is the isochronous stream, not this endpoint, so nothing is
/// written here unless the host actually sends something.
pub fn run_midi_out(
    dev: Arc<Ns6>,
    stats: Arc<Stats>,
    running: Arc<AtomicBool>,
    midi_rx: Receiver<u8>,
) {
    let mut pending: Vec<u8> = Vec::new();

    while running.load(Ordering::Relaxed) {
        pending.extend(midi_rx.try_iter());
        if pending.is_empty() {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        match dev.write_midi(&pending) {
            Ok(n) => {
                stats.out_ok.fetch_add(1, Ordering::Relaxed);
                stats.midi_out_bytes.fetch_add(n as u64, Ordering::Relaxed);
                pending.drain(..n.min(pending.len()));
            }
            Err(_) => {
                stats.out_err.fetch_add(1, Ordering::Relaxed);
                pending.clear(); // do not wedge on a stuck pipe
            }
        }
    }
}
