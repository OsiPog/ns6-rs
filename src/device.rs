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

        for iface in p::INTERFACES {
            handle.claim_interface(iface)?;
            // Force an alt 0 -> alt 1 transition. Alt 0 is the zero-bandwidth
            // idle setting; going straight to alt 1 when the device is already
            // there is a no-op and never restarts its streaming engine.
            let _ = handle.set_alternate_setting(iface, 0);
            handle.set_alternate_setting(iface, p::ALT_SETTING)?;
        }

        Ok(Self { handle })
    }

    /// Read the 15-byte firmware version block (vendor request `'V'`).
    pub fn firmware(&self) -> Result<Vec<u8>, Error> {
        let mut buf = [0u8; 15];
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

    /// Set the sample rate on both PCM endpoints (audio class `SET_CUR`).
    pub fn set_sample_rate(&self, rate: u32) -> Result<(), Error> {
        let bytes = p::encode_rate(rate);
        // The audio endpoints: isochronous out/in plus the bulk audio in.
        // 0x04 is MIDI, so it gets no sample rate.
        for ep in [p::EP_ISO_OUT, p::EP_ISO_IN, p::EP_PCM_IN] {
            self.handle.write_control(
                p::SET_CUR_TYPE,
                p::SET_CUR_REQ,
                p::SET_CUR_VALUE,
                ep as u16,
                &bytes,
                CTRL_TIMEOUT,
            )?;
        }
        Ok(())
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
        let before = self.status()?;
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

    /// Perform the full startup sequence.
    pub fn start(&self) -> Result<(), Error> {
        let fw = self.firmware()?;
        println!(
            "firmware: {}",
            fw.iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        );

        self.set_sample_rate(p::SAMPLE_RATE)?;
        println!("sample rate: {} Hz", p::SAMPLE_RATE);

        let (before, after) = self.arm()?;
        println!("armed: status 0x{before:02X} -> 0x{after:02X}");
        if after & p::ARM_BIT == 0 {
            eprintln!("warning: arm bit did not stick (status 0x{after:02X})");
        }

        self.clear_halts();
        Ok(())
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
