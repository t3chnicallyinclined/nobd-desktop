//! NOBD Bulk input source -- the Extreme Low Latency path.
//!
//! Reads the stick's WinUSB bulk stream (firmware NOBD Bulk mode, VID_CAFE&PID_4030) at wire rate
//! (~10 kHz) and publishes the latest frame into a lock-free `BulkSnapshot`. The sync loop reads that
//! snapshot -> sync window -> XUSB companion, exactly like the HID path -- but the stick->app hop is
//! ~90us (bulk) instead of ~500us (the stick's own XInput poll). The 20-byte payload matches the
//! firmware contract [seq | edge_us | XInput-format buttons/sticks/triggers].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA,
};
use windows_sys::Win32::Devices::Usb::{
    WinUsb_Free, WinUsb_Initialize, WinUsb_QueryPipe, WinUsb_ReadPipe, WinUsb_SetPipePolicy,
    WinUsb_SetPowerPolicy, UsbdPipeTypeBulk, RAW_IO, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

// WinUSB power-policy type AUTO_SUSPEND; set the value to 0 to keep the device out of USB selective
// suspend (otherwise Windows suspends it after idle and the bulk stream dies).
const WINUSB_AUTO_SUSPEND: u32 = 0x81;

pub const NOBD_BULK_VID: u16 = 0xCAFE;
pub const NOBD_BULK_PID: u16 = 0x4030;

// GUID_DEVINTERFACE_USB_DEVICE -- every USB device exposes this, so we find ours by VID/PID in the
// device path (robust to whatever WinUSB device-interface GUID auto-bind or Zadig registered).
const GUID_DEVINTERFACE_USB_DEVICE: GUID = GUID {
    data1: 0xA5DC_BF10, data2: 0x6530, data3: 0x11D2,
    data4: [0x90, 0x1F, 0x00, 0xC0, 0x4F, 0xB9, 0x51, 0xED],
};

/// Lock-free latest-frame publish (writer: the reader thread; readers: the sync loop + the UI).
pub struct BulkSnapshot {
    btn_lt_rt: AtomicU32, // buttons<<16 | lt<<8 | rt
    lx_ly: AtomicU32,     // (lx as u16)<<16 | (ly as u16)
    rx_ry: AtomicU32,
    edge_us: AtomicU32,   // firmware send timestamp of the latest frame
    drops: AtomicU32,     // cumulative dropped sequences (contiguity gaps)
    rate_hz: AtomicU32,   // measured payloads/sec -- the stick->app stream freshness
    present: AtomicBool,  // the stream is open + delivering
    stop: AtomicBool,
}

impl BulkSnapshot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            btn_lt_rt: AtomicU32::new(0),
            lx_ly: AtomicU32::new(0),
            rx_ry: AtomicU32::new(0),
            edge_us: AtomicU32::new(0),
            drops: AtomicU32::new(0),
            rate_hz: AtomicU32::new(0),
            present: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        })
    }
    pub fn stop(&self) { self.stop.store(true, Ordering::Relaxed); }
    pub fn present(&self) -> bool { self.present.load(Ordering::Relaxed) }
    pub fn drops(&self) -> u32 { self.drops.load(Ordering::Relaxed) }
    pub fn rate_hz(&self) -> u32 { self.rate_hz.load(Ordering::Relaxed) }
    pub fn edge_us(&self) -> u32 { self.edge_us.load(Ordering::Relaxed) }

    /// (buttons, lt, rt, lx, ly, rx, ry) -- the fields the sync loop feeds to the companion.
    pub fn get(&self) -> (u16, u8, u8, i16, i16, i16, i16) {
        let a = self.btn_lt_rt.load(Ordering::Relaxed);
        let ls = self.lx_ly.load(Ordering::Relaxed);
        let rs = self.rx_ry.load(Ordering::Relaxed);
        (
            (a >> 16) as u16, (a >> 8) as u8, a as u8,
            (ls >> 16) as i16, ls as i16, (rs >> 16) as i16, rs as i16,
        )
    }
}

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

/// Enumerate USB device interfaces and return the device path whose id contains VID_xxxx&PID_yyyy.
pub fn find_device_path(vid: u16, pid: u16) -> Option<Vec<u16>> {
    unsafe {
        let di: HDEVINFO = SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_USB_DEVICE, std::ptr::null(), 0 as HANDLE,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if di == INVALID_HANDLE_VALUE { return None; }
        let needle = format!("vid_{vid:04x}&pid_{pid:04x}");
        let mut result = None;
        let mut idx = 0u32;
        loop {
            let mut ifd: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            ifd.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(di, std::ptr::null(), &GUID_DEVINTERFACE_USB_DEVICE, idx, &mut ifd) == 0 {
                break;
            }
            idx += 1;
            // detail buffer: [u32 cbSize][WCHAR DevicePath[]]. cbSize = 8 on x64.
            let mut buf = vec![0u8; 1024];
            buf[0..4].copy_from_slice(&8u32.to_le_bytes());
            let detail = buf.as_mut_ptr() as *mut windows_sys::Win32::Devices::DeviceAndDriverInstallation::SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            if SetupDiGetDeviceInterfaceDetailW(di, &ifd, detail, buf.len() as u32, std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                let path_ptr = buf.as_ptr().add(4) as *const u16; // DevicePath starts after cbSize
                let mut len = 0usize;
                while *path_ptr.add(len) != 0 { len += 1; }
                let path = std::slice::from_raw_parts(path_ptr, len);
                let s = String::from_utf16_lossy(path).to_lowercase();
                if s.contains(&needle) {
                    let mut v: Vec<u16> = path.to_vec(); v.push(0);
                    result = Some(v);
                    break;
                }
            }
        }
        SetupDiDestroyDeviceInfoList(di);
        result
    }
}

/// Keep the NOBD Bulk stream attached for the life of the sync service. Retries the open forever
/// (short backoff) until `snap.stop` is set -- so a device that's momentarily busy (another process
/// still tearing down its handle) or unplugged/replugged re-attaches on its own, instead of the reader
/// giving up after one failed open. This is the fix for the "force-killed app -> bulk never came back"
/// bug: `stream_once` returns on any open failure or device loss, and we just try again.
pub fn run_reader(snap: Arc<BulkSnapshot>) {
    while !snap.stop.load(Ordering::Relaxed) {
        stream_once(&snap);
        snap.present.store(false, Ordering::Relaxed);
        snap.rate_hz.store(0, Ordering::Relaxed);
        // Back off ~250 ms before retrying the open, but wake promptly on stop.
        for _ in 0..10 {
            if snap.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// One attach: find + open the device, then stream the 20-byte payloads into `snap` until the device
/// is lost or `snap.stop` is set. Returns (rather than looping on the open) so `run_reader` can back
/// off and retry -- every early `return` here is "couldn't attach this time, try again later".
fn stream_once(snap: &Arc<BulkSnapshot>) {
    let path = match find_device_path(NOBD_BULK_VID, NOBD_BULK_PID) {
        Some(p) => p,
        None => return, // device not present / not WinUSB-bound yet
    };
    unsafe {
        let h: HANDLE = CreateFileW(
            path.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(), OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED, 0 as HANDLE,
        );
        if h == INVALID_HANDLE_VALUE { return; }
        let mut wu: WINUSB_INTERFACE_HANDLE = std::mem::zeroed();
        if WinUsb_Initialize(h, &mut wu) == 0 { CloseHandle(h); return; }

        // Find the bulk IN pipe.
        let mut pipe_id = 0u8;
        for i in 0..16u8 {
            let mut pi: WINUSB_PIPE_INFORMATION = std::mem::zeroed();
            if WinUsb_QueryPipe(wu, 0, i, &mut pi) == 0 { break; }
            if (pi.PipeId & 0x80) != 0 && pi.PipeType == UsbdPipeTypeBulk { pipe_id = pi.PipeId; }
        }
        if pipe_id == 0 { WinUsb_Free(wu); CloseHandle(h); return; }
        let raw: u8 = 1;
        WinUsb_SetPipePolicy(wu, pipe_id, RAW_IO, 1, &raw as *const u8 as *const _);
        // Pin the device awake -- without this Windows selective-suspends it after idle and the bulk
        // stream stops (the "works, then dies after idle, toggle NOBD to fix" bug).
        let awake: u8 = 0;
        WinUsb_SetPowerPolicy(wu, WINUSB_AUTO_SUSPEND, 1, &awake as *const u8 as *const _);

        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        let mut buf = [0u8; 512];
        let mut last_seq = 0u32;
        let mut have = false;
        snap.present.store(true, Ordering::Relaxed);
        let mut rate_frames = 0u32;
        let mut rate_t0 = Instant::now();

        while !snap.stop.load(Ordering::Relaxed) {
            let mut got = 0u32;
            let ok = WinUsb_ReadPipe(wu, pipe_id, buf.as_mut_ptr(), buf.len() as u32, &mut got, &mut ov);
            if ok == 0 {
                // Read is pending. Wait on the event with a timeout instead of blocking forever: a
                // ~10 kHz stream never gaps 250 ms, so a timeout means it stalled (suspend / wedge) --
                // cancel + break so run_reader re-attaches. Also lets us notice stop promptly.
                if WaitForSingleObject(ov.hEvent, 250) != WAIT_OBJECT_0 || snap.stop.load(Ordering::Relaxed) {
                    CancelIoEx(h, &ov);
                    let mut reap = 0u32;
                    GetOverlappedResult(h, &ov, &mut reap, 1); // reap the cancelled transfer before cleanup
                    break;
                }
                if GetOverlappedResult(h, &ov, &mut got, 0) == 0 { break; }
            }
            let mut off = 0usize;
            while off + 20 <= got as usize {
                let seq = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                let edge = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
                let buttons = u16::from_le_bytes(buf[off + 8..off + 10].try_into().unwrap());
                let lt = buf[off + 10]; let rt = buf[off + 11];
                let lx = i16::from_le_bytes(buf[off + 12..off + 14].try_into().unwrap());
                let ly = i16::from_le_bytes(buf[off + 14..off + 16].try_into().unwrap());
                let rx = i16::from_le_bytes(buf[off + 16..off + 18].try_into().unwrap());
                let ry = i16::from_le_bytes(buf[off + 18..off + 20].try_into().unwrap());
                if have && seq > last_seq + 1 {
                    snap.drops.fetch_add(seq - last_seq - 1, Ordering::Relaxed);
                }
                last_seq = seq; have = true;
                snap.btn_lt_rt.store(((buttons as u32) << 16) | ((lt as u32) << 8) | rt as u32, Ordering::Relaxed);
                snap.lx_ly.store(((lx as u16 as u32) << 16) | (ly as u16 as u32), Ordering::Relaxed);
                snap.rx_ry.store(((rx as u16 as u32) << 16) | (ry as u16 as u32), Ordering::Relaxed);
                snap.edge_us.store(edge, Ordering::Relaxed);
                rate_frames += 1;
                off += 20;
            }
            if rate_t0.elapsed().as_millis() >= 200 {
                snap.rate_hz.store(
                    (rate_frames as f64 / rate_t0.elapsed().as_secs_f64()) as u32,
                    Ordering::Relaxed,
                );
                rate_frames = 0;
                rate_t0 = Instant::now();
            }
        }

        snap.present.store(false, Ordering::Relaxed);
        if ov.hEvent != 0 { CloseHandle(ov.hEvent); }
        WinUsb_Free(wu);
        CloseHandle(h);
    }
}
