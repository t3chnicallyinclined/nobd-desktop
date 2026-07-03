//! HIDMaestro driver shared-memory INPUT ABI (client side).
//!
//! Ported from HIDMaestro `SharedMemoryIO.cs` + `driver.c`. We create the
//! `Global\HIDMaestroInput<N>` section and `Global\HIDMaestroInputEvent<N>`
//! event, then seqlock-write raw HID report bytes; the vendored UMDF2 driver
//! reads them (under the same seqlock) and delivers them up the HID stack.
//!
//! Load-bearing details verified against the driver source:
//!  - Section is exactly 362 bytes, `#[repr(C, packed)]`.
//!  - `Global\` objects need a permissive SDDL so WUDFHost (LocalService) can
//!    open them; creating `Global\` objects requires an elevated process.
//!  - Seqlock: bump SeqNo odd, write payload, clear ExtendedReportSize, bump
//!    SeqNo even, then SetEvent. The driver treats an unchanged SeqNo as
//!    "no new data", so the +2/frame is what triggers delivery.
//!  - `<N>` must match the devnode's `Device Parameters\ControllerIndex`.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::{fence, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{CreateEventExW, SetEvent};

/// Fixed size of the input section (`SHARED_INPUT_SIZE` in SharedMemoryIO.cs).
pub const SHARED_INPUT_SIZE: usize = 362;

// Field offsets into the section (driver.h `HIDMAESTRO_SHARED_INPUT`, pack(1)).
const OFF_SEQ: usize = 0; // u32 volatile seqlock
const OFF_DATA_SIZE: usize = 4; // u32
const OFF_DATA: usize = 8; // [u8; 256]
const OFF_GIP: usize = 264; // [u8; 14] — XUSB/XInput companion reads only this
const OFF_EXT_SIZE: usize = 278; // u32 — must be cleared to 0 every frame

/// Permissive DACL so the LocalService WUDFHost can open the section/event.
const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;LS)(A;;GR;;;WD)";
const SDDL_REVISION_1: u32 = 1;
const EVENT_MODIFY_STATE: u32 = 0x0002;
const SYNCHRONIZE: u32 = 0x0010_0000;

/// Documentation mirror of the driver's struct — we write via offsets (taking
/// references into a packed struct is UB), but this pins the layout + size.
#[repr(C, packed)]
#[allow(dead_code)]
struct SharedInput {
    seq_no: u32,
    data_size: u32,
    data: [u8; 256],
    gip_data: [u8; 14],
    extended_report_size: u32,
    extended_report_data: [u8; 80],
}
const _: () = assert!(core::mem::size_of::<SharedInput>() == SHARED_INPUT_SIZE);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Build a SECURITY_ATTRIBUTES from our SDDL. Returns the attrs plus the
/// LocalAlloc'd security descriptor pointer (free with `LocalFree` after the
/// Create* call consumes the attrs).
unsafe fn security_attributes() -> io::Result<(SECURITY_ATTRIBUTES, *mut c_void)> {
    let sddl = wide(SDDL);
    let mut psd: *mut c_void = std::ptr::null_mut();
    let ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl.as_ptr(),
        SDDL_REVISION_1,
        &mut psd,
        std::ptr::null_mut(),
    );
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd,
        bInheritHandle: 0,
    };
    Ok((sa, psd))
}

/// Owns the input section + event for one virtual controller. `submit` is the
/// per-frame hot path. Drop unmaps + closes handles (the section vanishes when
/// the last handle closes; the driver then stops receiving).
pub struct InputChannel {
    mapping: HANDLE,
    base: *mut u8,
    event: HANDLE, // 0 if the event couldn't be created (driver falls back to 500ms poll)
    seq: u32,
}

impl InputChannel {
    /// Create `Global\HIDMaestroInput<index>` (+ event), zero-initialized.
    /// Requires an elevated process (create-global-object + the Global\ namespace).
    pub fn create(index: u32) -> io::Result<Self> {
        unsafe {
            let (sa, psd) = security_attributes()?;

            let name = wide(&format!("Global\\HIDMaestroInput{index}"));
            let mapping = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &sa,
                PAGE_READWRITE,
                0,
                SHARED_INPUT_SIZE as u32,
                name.as_ptr(),
            );
            if mapping == 0 {
                let e = io::Error::last_os_error();
                LocalFree(psd);
                return Err(e);
            }

            let view = MapViewOfFile(mapping, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, SHARED_INPUT_SIZE);
            if view.Value.is_null() {
                let e = io::Error::last_os_error();
                CloseHandle(mapping);
                LocalFree(psd);
                return Err(e);
            }
            let base = view.Value as *mut u8;
            core::ptr::write_bytes(base, 0, SHARED_INPUT_SIZE); // zero the whole section

            // Auto-reset, initially non-signaled event (dwFlags = 0). Non-fatal
            // if this fails — the driver's worker still polls every 500ms.
            let ename = wide(&format!("Global\\HIDMaestroInputEvent{index}"));
            let event = CreateEventExW(&sa, ename.as_ptr(), 0, EVENT_MODIFY_STATE | SYNCHRONIZE);

            LocalFree(psd);
            Ok(Self { mapping, base, event, seq: 0 })
        }
    }

    /// Whether the signal event is available (else the driver polls at ~500ms).
    pub fn has_event(&self) -> bool {
        self.event != 0
    }

    /// Seqlock-write a raw HID input report (no report-ID byte) and signal the
    /// driver. `report` is copied verbatim into `Data[]`; the driver zero-pads
    /// to the descriptor's InputReportByteLength.
    pub fn submit(&mut self, report: &[u8]) {
        let n = report.len().min(256);
        unsafe {
            let seq_ptr = self.base.add(OFF_SEQ) as *mut u32;

            let pending = self.seq.wrapping_add(1); // odd — write in progress
            seq_ptr.write_volatile(pending);
            fence(Ordering::SeqCst);

            (self.base.add(OFF_DATA_SIZE) as *mut u32).write_volatile(n as u32);
            core::ptr::copy_nonoverlapping(report.as_ptr(), self.base.add(OFF_DATA), n);
            // Mandatory per-frame: keep the legacy (non-extended) path selected.
            // ExtendedReportSize is at a packed (unaligned) offset — write it
            // byte-wise so the u32 volatile write never hits an unaligned pointer.
            for i in 0..4 {
                self.base.add(OFF_EXT_SIZE + i).write_volatile(0u8);
            }

            fence(Ordering::SeqCst);
            let done = pending.wrapping_add(1); // even — publish
            seq_ptr.write_volatile(done);
            self.seq = done;
        }
        if self.event != 0 {
            unsafe { SetEvent(self.event) };
        }
    }

    /// Seqlock-write the 14-byte GIP buffer (XUSB/XInput companion path) and
    /// signal. The companion reads only `GipData[14]`, so `Data[256]` is left
    /// untouched.
    pub fn submit_gip(&mut self, gip: &[u8; 14]) {
        unsafe {
            let seq_ptr = self.base.add(OFF_SEQ) as *mut u32;

            let pending = self.seq.wrapping_add(1);
            seq_ptr.write_volatile(pending);
            fence(Ordering::SeqCst);

            core::ptr::copy_nonoverlapping(gip.as_ptr(), self.base.add(OFF_GIP), gip.len());
            // ExtendedReportSize is at a packed (unaligned) offset — write it
            // byte-wise so the u32 volatile write never hits an unaligned pointer.
            for i in 0..4 {
                self.base.add(OFF_EXT_SIZE + i).write_volatile(0u8);
            }

            fence(Ordering::SeqCst);
            let done = pending.wrapping_add(1);
            seq_ptr.write_volatile(done);
            self.seq = done;
        }
        if self.event != 0 {
            unsafe { SetEvent(self.event) };
        }
    }
}

impl Drop for InputChannel {
    fn drop(&mut self) {
        unsafe {
            if !self.base.is_null() {
                let addr = windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.base as *mut c_void,
                };
                UnmapViewOfFile(addr);
            }
            if self.event != 0 {
                CloseHandle(self.event);
            }
            if self.mapping != 0 {
                CloseHandle(self.mapping);
            }
        }
    }
}
