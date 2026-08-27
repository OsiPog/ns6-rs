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
    pub pcm_in: AtomicU64,
    pub midi_in_bytes: AtomicU64,
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
        for ep in [p::EP_PCM_IN, p::EP_PCM_OUT] {
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
        for ep in [p::EP_PCM_OUT, p::EP_MIDI_IN, p::EP_PCM_IN] {
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

    /// Write one bulk OUT transfer. Returns the number of bytes accepted.
    pub fn write_pcm(&self, buf: &[u8]) -> Result<usize, Error> {
        Ok(self.handle.write_bulk(p::EP_PCM_OUT, buf, XFER_TIMEOUT)?)
    }

    /// Raw libusb handle, for the isochronous streams.
    ///
    /// `rusb` has no isochronous API, so `iso::IsoStream` drives libusb directly.
    pub fn raw_handle(&self) -> *mut rusb::ffi::libusb_device_handle {
        self.handle.as_raw()
    }

    /// Read from a bulk IN endpoint. Returns the number of bytes received.
    pub fn read_bulk(&self, ep: u8, buf: &mut [u8]) -> Result<usize, Error> {
        Ok(self.handle.read_bulk(ep, buf, XFER_TIMEOUT)?)
    }
}

/// Pump silence to the PCM OUT endpoint, embedding any MIDI from `midi_rx`.
///
/// This is what keeps the device streaming, and therefore what makes it report
/// its control surface at all.
pub fn run_pcm_out(
    dev: Arc<Ns6>,
    stats: Arc<Stats>,
    running: Arc<AtomicBool>,
    midi_rx: Receiver<u8>,
) {
    let mut buf = vec![0u8; p::OUT_XFER_SIZE];

    while running.load(Ordering::Relaxed) {
        let mut pending = midi_rx.try_iter();
        let consumed = p::fill_out_buffer(&mut buf, &mut pending);
        if consumed > 0 {
            stats
                .midi_out_bytes
                .fetch_add(consumed as u64, Ordering::Relaxed);
        }

        match dev.write_pcm(&buf) {
            Ok(_) => {
                stats.out_ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                stats.out_err.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Read the MIDI IN pipe, strip filler, and hand raw MIDI bytes to `sink`.
pub fn run_midi_in(
    dev: Arc<Ns6>,
    stats: Arc<Stats>,
    running: Arc<AtomicBool>,
    mut sink: impl FnMut(&[u8]),
) {
    let mut raw = vec![0u8; p::BLOCK];
    let mut clean = Vec::with_capacity(p::BLOCK);

    while running.load(Ordering::Relaxed) {
        match dev.read_bulk(p::EP_MIDI_IN, &mut raw) {
            Ok(0) => {}
            Ok(n) => {
                clean.clear();
                p::strip_midi_filler(&raw[..n], &mut clean);
                if !clean.is_empty() {
                    stats
                        .midi_in_bytes
                        .fetch_add(clean.len() as u64, Ordering::Relaxed);
                    sink(&clean);
                }
            }
            Err(_) => {} // timeouts are normal when the surface is idle
        }
    }
}

/// Drain the PCM IN pipe so the device is never blocked waiting on the host.
pub fn run_pcm_in(dev: Arc<Ns6>, stats: Arc<Stats>, running: Arc<AtomicBool>) {
    let mut buf = vec![0u8; 5120];

    while running.load(Ordering::Relaxed) {
        if let Ok(n) = dev.read_bulk(p::EP_PCM_IN, &mut buf) {
            if n > 0 {
                stats.pcm_in.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
