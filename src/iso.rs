//! Isochronous streams, via raw libusb async transfers.
//!
//! `rusb` has no isochronous support, so this drives libusb's async API directly.
//!
//! The NS6 needs these in *addition* to the bulk PCM pipes.
//! `selectConfiguration()` assigns four streaming pipes, not two:
//!
//! | field    | predicate                            | NS6 endpoint |
//! |----------|--------------------------------------|--------------|
//! | `0x16b0` | OUT && bulk                          | `0x04`       |
//! | `0x16a8` | IN  && bulk                          | `0x86`       |
//! | `0x16d8` | OUT && iso && wMaxPacketSize > 0x20  | `0x02`       |
//! | `0x16c0` | IN  && iso && wMaxPacketSize > 0x20  | `0x81`       |
//!
//! The driver's `requestIsocOut()` / `requestIOKeepAlive()` /
//! `isocWriteCompleteKeepAlive` machinery runs the isochronous side as a
//! keep-alive that clocks the device. Driving bulk alone gets one buffer accepted
//! and then permanent NAK, because nothing is clocking the engine.

use std::os::raw::{c_int, c_uint, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rusb::ffi;

/// Set false to make every in-flight transfer retire instead of resubmitting.
pub static ISO_RUNNING: AtomicBool = AtomicBool::new(true);
pub static ISO_OUT_OK: AtomicU64 = AtomicU64::new(0);
pub static ISO_OUT_ERR: AtomicU64 = AtomicU64::new(0);
pub static ISO_IN_OK: AtomicU64 = AtomicU64::new(0);
pub static ISO_IN_DATA: AtomicU64 = AtomicU64::new(0);

/// Set true to hex-dump non-zero isochronous IN payloads, for protocol work.
pub static ISO_IN_DUMP: AtomicBool = AtomicBool::new(false);

const LIBUSB_TRANSFER_TYPE_ISOCHRONOUS: u8 = 1;
const LIBUSB_TRANSFER_COMPLETED: c_int = 0;
const LIBUSB_TRANSFER_CANCELLED: c_int = 3;

/// One submitted isochronous transfer, together with the buffer it points at.
struct IsoTransfer {
    ptr: *mut ffi::libusb_transfer,
    _buffer: Box<[u8]>,
}

/// A running isochronous stream. Cancels its transfers on drop.
pub struct IsoStream {
    transfers: Vec<IsoTransfer>,
}

extern "system" fn on_complete(xfer: *mut ffi::libusb_transfer) {
    unsafe {
        let t = &*xfer;
        if t.status == LIBUSB_TRANSFER_CANCELLED {
            return;
        }

        let is_input = t.endpoint & 0x80 != 0;
        let descs =
            std::slice::from_raw_parts(t.iso_packet_desc.as_ptr(), t.num_iso_packets as usize);

        for (i, d) in descs.iter().enumerate() {
            if d.status == LIBUSB_TRANSFER_COMPLETED {
                if is_input {
                    ISO_IN_OK.fetch_add(1, Ordering::Relaxed);
                    if d.actual_length > 0 {
                        ISO_IN_DATA.fetch_add(d.actual_length as u64, Ordering::Relaxed);
                        if ISO_IN_DUMP.load(Ordering::Relaxed) {
                            dump_iso_in(t, i, d.actual_length as usize);
                        }
                    }
                } else {
                    ISO_OUT_OK.fetch_add(1, Ordering::Relaxed);
                }
            } else if !is_input {
                ISO_OUT_ERR.fetch_add(1, Ordering::Relaxed);
            }
        }

        if ISO_RUNNING.load(Ordering::Relaxed) {
            ffi::libusb_submit_transfer(xfer);
        }
    }
}

/// Hex-dump one isochronous IN packet if it carries anything but zeros.
///
/// libusb packs isochronous IN packets back-to-back at `max_packet_size`
/// stride, so packet `index` starts at `index * iso_packet_desc[0].length`.
unsafe fn dump_iso_in(t: &ffi::libusb_transfer, index: usize, len: usize) {
    let stride = (*t.iso_packet_desc.as_ptr()).length as usize;
    let base = t.buffer.add(index * stride);
    let data = std::slice::from_raw_parts(base, len);

    if data.iter().all(|&b| b == 0) {
        return;
    }
    let hex: Vec<String> = data.iter().map(|b| format!("{b:02X}")).collect();
    println!("      iso-in[{index}] {len}B: {}", hex.join(" "));
}

impl IsoStream {
    /// Start an isochronous stream on `endpoint`.
    ///
    /// # Safety
    /// `handle` must be a live libusb device handle with the owning interface
    /// claimed, and must outlive the returned stream.
    pub unsafe fn start(
        handle: *mut ffi::libusb_device_handle,
        endpoint: u8,
        packet_size: usize,
        packets_per_transfer: usize,
        transfer_count: usize,
    ) -> Result<Self, i32> {
        let mut transfers = Vec::with_capacity(transfer_count);

        for _ in 0..transfer_count {
            let total = packet_size * packets_per_transfer;
            let mut buffer = vec![0u8; total].into_boxed_slice();

            let ptr = ffi::libusb_alloc_transfer(packets_per_transfer as c_int);
            if ptr.is_null() {
                return Err(-1);
            }

            {
                let t = &mut *ptr;
                t.dev_handle = handle;
                t.endpoint = endpoint;
                t.transfer_type = LIBUSB_TRANSFER_TYPE_ISOCHRONOUS;
                t.timeout = 1000;
                t.buffer = buffer.as_mut_ptr();
                t.length = total as c_int;
                t.num_iso_packets = packets_per_transfer as c_int;
                t.callback = on_complete;
                t.user_data = std::ptr::null_mut::<c_void>();
                t.flags = 0;
                t.status = 0;
                t.actual_length = 0;

                // libusb_set_iso_packet_lengths is an inline helper in C, so set
                // the descriptors directly.
                let descs = std::slice::from_raw_parts_mut(
                    t.iso_packet_desc.as_mut_ptr(),
                    packets_per_transfer,
                );
                for d in descs.iter_mut() {
                    d.length = packet_size as c_uint;
                    d.actual_length = 0;
                    d.status = 0;
                }
            }

            let rc = ffi::libusb_submit_transfer(ptr);
            if rc != 0 {
                ffi::libusb_free_transfer(ptr);
                return Err(rc);
            }

            transfers.push(IsoTransfer {
                ptr,
                _buffer: buffer,
            });
        }

        Ok(Self { transfers })
    }
}

impl Drop for IsoStream {
    fn drop(&mut self) {
        unsafe {
            for t in &self.transfers {
                ffi::libusb_cancel_transfer(t.ptr);
            }
            // Let the cancellations complete before the buffers are freed.
            for _ in 0..20 {
                let tv = libc_timeval {
                    tv_sec: 0,
                    tv_usec: 20_000,
                };
                ffi::libusb_handle_events_timeout_completed(
                    std::ptr::null_mut(),
                    &tv as *const _ as *const _,
                    std::ptr::null_mut(),
                );
            }
            for t in &self.transfers {
                ffi::libusb_free_transfer(t.ptr);
            }
        }
    }
}

#[repr(C)]
struct libc_timeval {
    tv_sec: i64,
    tv_usec: i64,
}

/// Pump libusb events so isochronous callbacks fire.
///
/// `rusb`'s synchronous transfers drive the default context too; libusb
/// serialises event handling internally, so both can coexist.
pub fn pump_events() {
    let tv = libc_timeval {
        tv_sec: 0,
        tv_usec: 100_000,
    };
    unsafe {
        ffi::libusb_handle_events_timeout_completed(
            std::ptr::null_mut(),
            &tv as *const _ as *const _,
            std::ptr::null_mut(),
        );
    }
}
