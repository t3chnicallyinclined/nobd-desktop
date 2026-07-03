// Raw-HID input backend — read a DInput/HID gamepad directly and decode buttons
// via the OS HID parser (no manual report-descriptor parsing, no extra crate).
//
// Enumerate -> open -> read reports -> HidP_GetUsages -> diff pressed-button set
// -> emit the SAME InputMsg::Pressed/Released(slot=0, Button, Instant) stream the
// XInput loop produces, so the whole cluster/stats/verdict pipeline is unchanged.
//
// Windows-only. Targets HID gamepads (usage page 0x01, usage 0x04 joystick / 0x05
// gamepad). Xbox/XInput pads bypass HID (xusb22) and are NOT valid targets here.

use crate::input::InputMsg;
use gilrs::Button;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData, HidD_GetProductString,
    HidD_FreePreparsedData, HidP_GetButtonCaps, HidP_GetCaps, HidP_GetUsageValue, HidP_GetUsages,
    HidP_MaxUsageListLength, HIDD_ATTRIBUTES, HIDP_BUTTON_CAPS, HIDP_CAPS, HidP_Input,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};

/// Stable identifier for a HID gamepad. `path` is the device-interface path
/// (what CreateFileW needs); `vid`/`pid` are the reconnect fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidDeviceId {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
}

impl HidDeviceId {
    /// The persisted/display string is just the interface path.
    pub fn to_persist(&self) -> String {
        self.path.clone()
    }
}

/// A discovered HID gamepad, for the device picker.
#[derive(Clone, Debug)]
pub struct HidDeviceInfo {
    pub id: HidDeviceId,
    pub product: String,
    // Diagnostic metadata (kept for the picker tooltip / future filtering).
    #[allow(dead_code)]
    pub usage_page: u16,
    #[allow(dead_code)]
    pub usage: u16,
    #[allow(dead_code)]
    pub button_count: u16,
}

impl HidDeviceInfo {
    pub fn id(&self) -> HidDeviceId {
        self.id.clone()
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Map a 0-based HID button index to a gilrs::Button. Indices 0..7 land on the 8
/// attack buttons so `is_attack()` works unchanged; 8.. are system/extra buttons.
/// Out-of-range indices return None (button ignored, never panics).
const HID_BUTTON_MAP: &[Button] = &[
    Button::South,         // btn 1
    Button::East,          // btn 2
    Button::West,          // btn 3
    Button::North,         // btn 4
    Button::LeftTrigger,   // btn 5
    Button::RightTrigger,  // btn 6
    Button::LeftTrigger2,  // btn 7
    Button::RightTrigger2, // btn 8
    Button::Select,        // btn 9
    Button::Start,         // btn 10
    Button::LeftThumb,     // btn 11
    Button::RightThumb,    // btn 12
    Button::Mode,          // btn 13
    Button::C,             // btn 14
    Button::Z,             // btn 15
    Button::Unknown,       // btn 16
];

fn map_button(idx: usize) -> Option<Button> {
    HID_BUTTON_MAP.get(idx).copied()
}

/// Enumerate connected HID gamepads (usage page 0x01, usage 0x04/0x05).
pub fn list_hid_gamepads() -> Vec<HidDeviceInfo> {
    let mut out = Vec::new();
    unsafe {
        let mut guid: GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);

        let hdev = SetupDiGetClassDevsW(
            &guid,
            std::ptr::null(),
            0,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if hdev == INVALID_HANDLE_VALUE {
            return out;
        }

        let mut index = 0u32;
        loop {
            let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            iface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(hdev, std::ptr::null(), &guid, index, &mut iface) == 0 {
                break; // ERROR_NO_MORE_ITEMS
            }
            index += 1;

            // Two-call sizing for the variable-length detail (device path).
            let mut required = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &iface,
                std::ptr::null_mut(),
                0,
                &mut required,
                std::ptr::null_mut(),
            );
            if required == 0 {
                continue;
            }
            let mut buf = vec![0u8; required as usize];
            // SP_DEVICE_INTERFACE_DETAIL_DATA_W: { u32 cbSize; u16 DevicePath[..] }.
            // cbSize is the fixed-part size: 8 on 64-bit, 6 on 32-bit. DevicePath
            // starts at byte offset 4.
            let cb_size: u32 = if cfg!(target_pointer_width = "64") { 8 } else { 6 };
            (buf.as_mut_ptr() as *mut u32).write(cb_size);
            if SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &iface,
                buf.as_mut_ptr() as *mut _,
                required,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
            {
                continue;
            }
            let path = wide_from_bytes(&buf[4..]);
            if path.is_empty() {
                continue;
            }

            if let Some(info) = probe_device(&path) {
                out.push(info);
            }
        }

        SetupDiDestroyDeviceInfoList(hdev);
    }
    out
}

/// Read DevicePath (UTF-16, null-terminated) from a byte slice at offset 0.
fn wide_from_bytes(bytes: &[u8]) -> String {
    let mut units = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let u = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        if u == 0 {
            break;
        }
        units.push(u);
        i += 2;
    }
    String::from_utf16_lossy(&units)
}

/// Open the device (query access) and confirm it's a gamepad; return its info.
unsafe fn probe_device(path: &str) -> Option<HidDeviceInfo> {
    let wpath = to_wide(path);
    // Zero desired access so we can read caps even if a game holds the device.
    let h = CreateFileW(
        wpath.as_ptr(),
        0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        std::ptr::null(),
        OPEN_EXISTING,
        0,
        0,
    );
    if h == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut pp: isize = 0;
    if HidD_GetPreparsedData(h, &mut pp) == 0 {
        CloseHandle(h);
        return None;
    }

    let mut caps: HIDP_CAPS = std::mem::zeroed();
    if HidP_GetCaps(pp, &mut caps) != HIDP_STATUS_SUCCESS {
        HidD_FreePreparsedData(pp);
        CloseHandle(h);
        return None;
    }
    let is_gamepad = caps.UsagePage == 0x01 && (caps.Usage == 0x04 || caps.Usage == 0x05);
    if !is_gamepad {
        HidD_FreePreparsedData(pp);
        CloseHandle(h);
        return None;
    }

    // VID/PID
    let mut attrs: HIDD_ATTRIBUTES = std::mem::zeroed();
    attrs.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    HidD_GetAttributes(h, &mut attrs);

    // Product string
    let mut pbuf = [0u16; 128];
    let product = if HidD_GetProductString(
        h,
        pbuf.as_mut_ptr() as *mut c_void,
        (pbuf.len() * 2) as u32,
    ) != 0
    {
        let end = pbuf.iter().position(|&c| c == 0).unwrap_or(pbuf.len());
        let p = String::from_utf16_lossy(&pbuf[..end]);
        if p.trim().is_empty() {
            format!("{:04X}:{:04X}", attrs.VendorID, attrs.ProductID)
        } else {
            p
        }
    } else {
        format!("{:04X}:{:04X}", attrs.VendorID, attrs.ProductID)
    };

    let button_count = caps.NumberInputButtonCaps;

    HidD_FreePreparsedData(pp);
    CloseHandle(h);

    Some(HidDeviceInfo {
        id: HidDeviceId {
            path: path.to_string(),
            vid: attrs.VendorID,
            pid: attrs.ProductID,
        },
        product,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        button_count,
    })
}

/// HIDP_STATUS_SUCCESS — the NTSTATUS-style success code the HidP_* APIs return.
const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000u32 as i32;

/// The HID reader loop (background thread). Opens `id`, reads reports, decodes
/// the pressed-button set with the OS HID parser, diffs, and emits InputMsg on
/// slot 0. Returns when the device errors out or the channel is dropped.
pub(crate) fn run_reader(id: HidDeviceId, tx: Sender<InputMsg>) {
    unsafe {
        let wpath = to_wide(&id.path);
        let h = CreateFileW(
            wpath.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            0,
        );
        if h == INVALID_HANDLE_VALUE {
            return;
        }

        let mut pp: isize = 0;
        if HidD_GetPreparsedData(h, &mut pp) == 0 {
            CloseHandle(h);
            return;
        }

        let mut caps: HIDP_CAPS = std::mem::zeroed();
        if HidP_GetCaps(pp, &mut caps) != HIDP_STATUS_SUCCESS {
            HidD_FreePreparsedData(pp);
            CloseHandle(h);
            return;
        }
        let report_len = caps.InputReportByteLength as usize;
        if report_len == 0 {
            HidD_FreePreparsedData(pp);
            CloseHandle(h);
            return;
        }

        // Button caps: usage page + UsageMin (so pressed index = usage - UsageMin).
        let mut nbtn = caps.NumberInputButtonCaps;
        let mut btn_caps: HIDP_BUTTON_CAPS = std::mem::zeroed();
        if nbtn == 0
            || HidP_GetButtonCaps(HidP_Input, &mut btn_caps, &mut nbtn, pp) != HIDP_STATUS_SUCCESS
        {
            HidD_FreePreparsedData(pp);
            CloseHandle(h);
            return;
        }
        let button_page = btn_caps.UsagePage;
        let usage_min = btn_caps.Anonymous.Range.UsageMin;

        let max_usages = HidP_MaxUsageListLength(HidP_Input, button_page, pp).max(1) as usize;

        // Product name for the Connected message.
        let mut pbuf = [0u16; 128];
        let name = if HidD_GetProductString(h, pbuf.as_mut_ptr() as *mut c_void, (pbuf.len() * 2) as u32) != 0 {
            let end = pbuf.iter().position(|&c| c == 0).unwrap_or(pbuf.len());
            let p = String::from_utf16_lossy(&pbuf[..end]);
            if p.trim().is_empty() { format!("{:04X}:{:04X}", id.vid, id.pid) } else { p }
        } else {
            format!("{:04X}:{:04X}", id.vid, id.pid)
        };

        timeBeginPeriod(1);
        if tx.send(InputMsg::Connected(vec![(0, name.clone())], 0.0)).is_err() {
            cleanup(h, pp);
            return;
        }

        // Overlapped read (manual-reset event, reset before each issue so a stale
        // signal from a prior completion can't be mistaken for a new one).
        let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        if event == 0 {
            cleanup(h, pp);
            return;
        }
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = event;

        let mut report = vec![0u8; report_len];
        let mut usage_buf = vec![0u16; max_usages];
        let mut prev: Vec<usize> = Vec::new();
        let mut last_change: Option<Instant> = None;
        let mut min_interval_ms = f64::INFINITY;
        let mut last_publish = Instant::now();

        loop {
            ResetEvent(event);
            let mut got: u32 = 0;
            let rf = ReadFile(h, report.as_mut_ptr(), report_len as u32, std::ptr::null_mut(), &mut ov);
            if rf == 0 {
                if GetLastError() != ERROR_IO_PENDING {
                    break; // device error / unplug
                }
                // Wait for THIS read to complete. On each 250ms timeout do
                // housekeeping (heartbeat + channel-drop check) WITHOUT re-issuing
                // the read — the same operation stays pending into `report`/`ov`.
                loop {
                    if WaitForSingleObject(event, 250) == WAIT_OBJECT_0 {
                        break;
                    }
                    if publish_due(&mut last_publish) {
                        let iv = if min_interval_ms.is_finite() { min_interval_ms } else { 0.0 };
                        if tx.send(InputMsg::Connected(vec![(0, name.clone())], iv)).is_err() {
                            CancelIo(h);
                            cleanup(h, pp);
                            return;
                        }
                    }
                }
            }
            // Read complete (sync or async) — fetch the byte count.
            if GetOverlappedResult(h, &ov, &mut got, 0) == 0 {
                break;
            }

            // One timestamp per report — same-report presses read a 0ms gap.
            let now = Instant::now();
            if let Some(p) = last_change {
                let d = now.duration_since(p).as_secs_f64() * 1000.0;
                if d > 0.05 && d < 100.0 && d < min_interval_ms {
                    min_interval_ms = d;
                }
            }
            last_change = Some(now);

            // Decode the currently-pressed button usages.
            let mut count = usage_buf.len() as u32;
            let st = HidP_GetUsages(
                HidP_Input,
                button_page,
                0,
                usage_buf.as_mut_ptr(),
                &mut count,
                pp,
                report.as_mut_ptr() as *mut u8,
                got,
            );
            let cur: Vec<usize> = if st == HIDP_STATUS_SUCCESS {
                usage_buf[..count as usize]
                    .iter()
                    .map(|&u| (u.wrapping_sub(usage_min)) as usize)
                    .collect()
            } else {
                // Wrong report id / length mismatch on a multi-report device → no change.
                prev.clone()
            };

            // Diff: pressed = cur - prev, released = prev - cur.
            for &idx in &cur {
                if !prev.contains(&idx) {
                    if let Some(b) = map_button(idx) {
                        if tx.send(InputMsg::Pressed(0, b, now)).is_err() {
                            cleanup(h, pp);
                            return;
                        }
                    }
                }
            }
            for &idx in &prev {
                if !cur.contains(&idx) {
                    if let Some(b) = map_button(idx) {
                        if tx.send(InputMsg::Released(0, b, now)).is_err() {
                            cleanup(h, pp);
                            return;
                        }
                    }
                }
            }
            prev = cur;

            if publish_due(&mut last_publish) {
                let iv = if min_interval_ms.is_finite() { min_interval_ms } else { 0.0 };
                if tx.send(InputMsg::Connected(vec![(0, name.clone())], iv)).is_err() {
                    break;
                }
            }
        }

        CancelIo(h);
        cleanup(h, pp);
    }
}

fn publish_due(last: &mut Instant) -> bool {
    if last.elapsed() >= Duration::from_secs(1) {
        *last = Instant::now();
        true
    } else {
        false
    }
}

// ─────────────────────── full-state reader (for the sync service) ───────────────────────
//
// The Gap Tester reader above emits button *events* for chord analysis. The sync
// service instead needs the COMPLETE gamepad state each report, in XInput layout,
// so it can run the NOBD window and re-emit the grouped result to the XUSB
// companion. `run_state_reader` decodes buttons (via the same GP2040-matched map),
// the d-pad (hat, usage 0x39), and L2/R2 into a lock-free `HidSnapshot`.

/// XInput-layout snapshot of a HID gamepad, packed into two atomics so the ~1 kHz
/// reader and the ~1 kHz sync loop never block each other.
pub struct HidSnapshot {
    /// buttons[0..16] (XInput wButtons layout) | lt[16..24] | rt[24..32]
    btn_trig: AtomicU32,
    /// lx (low 16, i16-as-u16) | ly (high 16)
    left_stick: AtomicU32,
    present: AtomicBool,
    stop: AtomicBool,
}

impl HidSnapshot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            btn_trig: AtomicU32::new(0),
            left_stick: AtomicU32::new(0), // 0/0 = centered
            present: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        })
    }

    /// Ask the reader thread to exit (device handle closes, thread returns).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether the reader has an open device delivering reports.
    pub fn is_present(&self) -> bool {
        self.present.load(Ordering::Relaxed)
    }

    /// Latest state as `(buttons, lt, rt, lx, ly)` — the args the sync loop feeds
    /// to `NobdController::submit` (right stick stays centered for a stick).
    pub fn read(&self) -> (u16, u8, u8, i16, i16) {
        let bt = self.btn_trig.load(Ordering::Relaxed);
        let ls = self.left_stick.load(Ordering::Relaxed);
        (
            (bt & 0xFFFF) as u16,
            ((bt >> 16) & 0xFF) as u8,
            ((bt >> 24) & 0xFF) as u8,
            (ls & 0xFFFF) as u16 as i16,
            ((ls >> 16) & 0xFFFF) as u16 as i16,
        )
    }

    fn write(&self, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16) {
        let bt = (buttons as u32) | ((lt as u32) << 16) | ((rt as u32) << 24);
        let ls = (lx as u16 as u32) | ((ly as u16 as u32) << 16);
        self.btn_trig.store(bt, Ordering::Relaxed);
        self.left_stick.store(ls, Ordering::Relaxed);
    }
}

/// HID button index (GP2040 DInput order, matching `HID_BUTTON_MAP`) → XInput
/// state. L2/R2 are digital on a stick, so they deflect the analog triggers fully.
fn apply_hid_button(idx: usize, buttons: &mut u16, lt: &mut u8, rt: &mut u8) {
    match idx {
        0 => *buttons |= 0x1000,  // A / Cross
        1 => *buttons |= 0x2000,  // B / Circle
        2 => *buttons |= 0x4000,  // X / Square
        3 => *buttons |= 0x8000,  // Y / Triangle
        4 => *buttons |= 0x0100,  // LB / L1
        5 => *buttons |= 0x0200,  // RB / R1
        6 => *lt = 255,           // LT / L2
        7 => *rt = 255,           // RT / R2
        8 => *buttons |= 0x0020,  // Back / Select
        9 => *buttons |= 0x0010,  // Start
        10 => *buttons |= 0x0040, // L3
        11 => *buttons |= 0x0080, // R3
        12 => *buttons |= 0x0400, // Guide
        _ => {}
    }
}

/// 8-way hat (usage 0x39, 0=Up … 7=Up-Left, >=8 neutral) → XInput d-pad bits +
/// mirrored left stick, so games reading either the d-pad or the stick both move.
fn apply_hat(dir: u32, buttons: &mut u16, lx: &mut i16, ly: &mut i16) {
    let (up, right, down, left) = match dir {
        0 => (true, false, false, false),
        1 => (true, true, false, false),
        2 => (false, true, false, false),
        3 => (false, true, true, false),
        4 => (false, false, true, false),
        5 => (false, false, true, true),
        6 => (false, false, false, true),
        7 => (true, false, false, true),
        _ => return, // neutral
    };
    if up {
        *buttons |= 0x0001;
        *ly = 32767;
    }
    if down {
        *buttons |= 0x0002;
        *ly = -32768;
    }
    if left {
        *buttons |= 0x0004;
        *lx = -32768;
    }
    if right {
        *buttons |= 0x0008;
        *lx = 32767;
    }
}

/// Full-state HID reader loop (background thread). Opens `id`, and on each report
/// decodes the whole gamepad into `snap` (XInput layout). Returns when the device
/// errors out or `snap.stop()` is called.
pub fn run_state_reader(id: HidDeviceId, snap: Arc<HidSnapshot>) {
    unsafe {
        let wpath = to_wide(&id.path);
        let h = CreateFileW(
            wpath.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            0,
        );
        if h == INVALID_HANDLE_VALUE {
            return;
        }

        let mut pp: isize = 0;
        if HidD_GetPreparsedData(h, &mut pp) == 0 {
            CloseHandle(h);
            return;
        }
        let mut caps: HIDP_CAPS = std::mem::zeroed();
        if HidP_GetCaps(pp, &mut caps) != HIDP_STATUS_SUCCESS {
            cleanup(h, pp);
            return;
        }
        let report_len = caps.InputReportByteLength as usize;
        if report_len == 0 {
            cleanup(h, pp);
            return;
        }

        let mut nbtn = caps.NumberInputButtonCaps;
        let mut btn_caps: HIDP_BUTTON_CAPS = std::mem::zeroed();
        if nbtn == 0
            || HidP_GetButtonCaps(HidP_Input, &mut btn_caps, &mut nbtn, pp) != HIDP_STATUS_SUCCESS
        {
            cleanup(h, pp);
            return;
        }
        let button_page = btn_caps.UsagePage;
        let usage_min = btn_caps.Anonymous.Range.UsageMin;
        let max_usages = HidP_MaxUsageListLength(HidP_Input, button_page, pp).max(1) as usize;

        let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        if event == 0 {
            cleanup(h, pp);
            return;
        }
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = event;

        let mut report = vec![0u8; report_len];
        let mut usage_buf = vec![0u16; max_usages];
        snap.present.store(true, Ordering::Relaxed);

        loop {
            if snap.stop.load(Ordering::Relaxed) {
                break;
            }
            ResetEvent(event);
            let mut got: u32 = 0;
            let rf = ReadFile(h, report.as_mut_ptr(), report_len as u32, std::ptr::null_mut(), &mut ov);
            if rf == 0 {
                if GetLastError() != ERROR_IO_PENDING {
                    break;
                }
                // Wait for THIS read; wake every 100 ms to honour stop().
                loop {
                    if WaitForSingleObject(event, 100) == WAIT_OBJECT_0 {
                        break;
                    }
                    if snap.stop.load(Ordering::Relaxed) {
                        CancelIo(h);
                        cleanup(h, pp);
                        snap.present.store(false, Ordering::Relaxed);
                        return;
                    }
                }
            }
            if GetOverlappedResult(h, &ov, &mut got, 0) == 0 {
                break;
            }

            // Decode the full state for this report.
            let (mut buttons, mut lt, mut rt, mut lx, mut ly) = (0u16, 0u8, 0u8, 0i16, 0i16);

            let mut count = usage_buf.len() as u32;
            let st = HidP_GetUsages(
                HidP_Input,
                button_page,
                0,
                usage_buf.as_mut_ptr(),
                &mut count,
                pp,
                report.as_mut_ptr() as *mut u8,
                got,
            );
            if st == HIDP_STATUS_SUCCESS {
                for &u in &usage_buf[..count as usize] {
                    apply_hid_button((u.wrapping_sub(usage_min)) as usize, &mut buttons, &mut lt, &mut rt);
                }
            }

            // D-pad: generic-desktop (0x01) hat switch (0x39).
            let mut hat: u32 = 0;
            if HidP_GetUsageValue(
                HidP_Input,
                0x01,
                0,
                0x39,
                &mut hat,
                pp,
                report.as_mut_ptr() as *mut u8,
                got,
            ) == HIDP_STATUS_SUCCESS
            {
                apply_hat(hat, &mut buttons, &mut lx, &mut ly);
            }

            snap.write(buttons, lt, rt, lx, ly);
        }

        CancelIo(h);
        cleanup(h, pp);
        snap.present.store(false, Ordering::Relaxed);
    }
}

unsafe fn cleanup(h: HANDLE, pp: isize) {
    HidD_FreePreparsedData(pp);
    CloseHandle(h);
}

#[cfg(test)]
mod tests {
    // Exercises the full SetupAPI + HID enumeration FFI against whatever is
    // plugged into the test machine. Can't assert a count (hardware-dependent),
    // but proves the FFI path doesn't access-violate / panic and that any
    // returned device has a valid path.
    #[test]
    fn enumerate_does_not_crash() {
        let devices = super::list_hid_gamepads();
        for d in &devices {
            assert!(!d.id.path.is_empty(), "device path must be non-empty");
        }
        eprintln!("enumerate_does_not_crash: found {} HID gamepad(s)", devices.len());
        for d in &devices {
            eprintln!(
                "  {} [{:04x}:{:04x}] usage {:#06x}/{:#06x} buttons={}",
                d.product, d.id.vid, d.id.pid, d.usage_page, d.usage, d.button_count
            );
        }
    }
}
