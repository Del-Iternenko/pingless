//! Thin binding to WinDivert 2.2, loaded at runtime from `WinDivert.dll`.
//!
//! Loading at runtime instead of linking keeps the binary starting when the driver
//! files are missing, so we can tell the user what is wrong instead of failing to start.

use std::ffi::{CString, c_void};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
use libloading::Library;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};

pub const LAYER_NETWORK: u32 = 0;
pub const LAYER_SOCKET: u32 = 3;

pub const EVENT_SOCKET_BIND: u8 = 3;
pub const EVENT_SOCKET_CONNECT: u8 = 4;
pub const EVENT_SOCKET_CLOSE: u8 = 7;

pub const FLAG_SNIFF: u64 = 0x1;
pub const FLAG_RECV_ONLY: u64 = 0x4;

const SHUTDOWN_BOTH: u32 = 0x3;

/// `WINDIVERT_ADDRESS`, 80 bytes. The union is kept as raw bytes with accessors.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Address {
    pub timestamp: i64,
    bits: u32,
    reserved2: u32,
    data: [u64; 8],
}

const _: () = assert!(std::mem::size_of::<Address>() == 80);

impl Address {
    fn bytes(&self) -> &[u8; 64] {
        // SAFETY: [u64; 8] and [u8; 64] have the same size and u8 has no alignment needs.
        unsafe { &*(self.data.as_ptr() as *const [u8; 64]) }
    }
    fn bytes_mut(&mut self) -> &mut [u8; 64] {
        // SAFETY: as above.
        unsafe { &mut *(self.data.as_mut_ptr() as *mut [u8; 64]) }
    }
    fn u32_at(&self, off: usize) -> u32 {
        u32::from_ne_bytes(self.bytes()[off..off + 4].try_into().unwrap())
    }
    fn u16_at(&self, off: usize) -> u16 {
        u16::from_ne_bytes(self.bytes()[off..off + 2].try_into().unwrap())
    }
    fn bit(&self, n: u32) -> bool {
        self.bits >> n & 1 != 0
    }
    fn set_bit(&mut self, n: u32, v: bool) {
        if v {
            self.bits |= 1 << n;
        } else {
            self.bits &= !(1 << n);
        }
    }

    pub fn event(&self) -> u8 {
        (self.bits >> 8 & 0xff) as u8
    }
    pub fn outbound(&self) -> bool {
        self.bit(17)
    }
    pub fn loopback(&self) -> bool {
        self.bit(18)
    }

    // NETWORK layer.
    pub fn if_idx(&self) -> u32 {
        self.u32_at(0)
    }
    pub fn sub_if_idx(&self) -> u32 {
        self.u32_at(4)
    }

    // SOCKET layer.
    pub fn process_id(&self) -> u32 {
        self.u32_at(16)
    }
    pub fn local_port(&self) -> u16 {
        self.u16_at(52)
    }
    pub fn protocol(&self) -> u8 {
        self.bytes()[56]
    }

    /// Address for injecting an inbound packet on the interface a flow uses.
    pub fn inbound(if_idx: u32, sub_if_idx: u32, ipv6: bool) -> Address {
        let mut a = Address {
            bits: LAYER_NETWORK,
            ..Default::default()
        };
        a.set_bit(17, false); // inbound
        a.set_bit(20, ipv6);
        // We compute checksums ourselves, so tell the stack they are valid.
        a.set_bit(21, !ipv6);
        a.set_bit(23, true);
        let b = a.bytes_mut();
        b[0..4].copy_from_slice(&if_idx.to_ne_bytes());
        b[4..8].copy_from_slice(&sub_if_idx.to_ne_bytes());
        a
    }

    /// The same packet address turned around: inject a rewritten outbound IPv4 TCP
    /// packet as inbound on the interface it was seen on.
    pub fn turned_inbound_tcp(&self) -> Address {
        let mut a = *self;
        a.set_bit(17, false);
        a.set_bit(21, true); // IP checksum valid
        a.set_bit(22, true); // TCP checksum valid
        a
    }
}

type OpenFn = unsafe extern "C" fn(*const i8, u32, i16, u64) -> HANDLE;
type RecvFn = unsafe extern "C" fn(HANDLE, *mut c_void, u32, *mut u32, *mut Address) -> i32;
type SendFn = unsafe extern "C" fn(HANDLE, *const c_void, u32, *mut u32, *const Address) -> i32;
type HandleFn = unsafe extern "C" fn(HANDLE) -> i32;
type ShutdownFn = unsafe extern "C" fn(HANDLE, u32) -> i32;

struct Api {
    _lib: Library,
    open: OpenFn,
    recv: RecvFn,
    send: SendFn,
    shutdown: ShutdownFn,
    close: HandleFn,
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api> {
    API.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn load() -> Result<Api> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let path = exe_dir.join("WinDivert.dll");
    // SAFETY: loading a known library; symbols are checked against the 2.2 header.
    unsafe {
        let lib = Library::new(&path).with_context(|| {
            format!(
                "cannot load {} (put WinDivert.dll and WinDivert64.sys next to the exe)",
                path.display()
            )
        })?;
        Ok(Api {
            open: *lib.get::<OpenFn>(b"WinDivertOpen\0")?,
            recv: *lib.get::<RecvFn>(b"WinDivertRecv\0")?,
            send: *lib.get::<SendFn>(b"WinDivertSend\0")?,
            shutdown: *lib.get::<ShutdownFn>(b"WinDivertShutdown\0")?,
            close: *lib.get::<HandleFn>(b"WinDivertClose\0")?,
            _lib: lib,
        })
    }
}

/// An open WinDivert handle. Recv and send may be called from different threads.
pub struct Handle {
    raw: HANDLE,
    api: &'static Api,
}

// SAFETY: WinDivert handles are kernel file handles usable from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    pub fn open(filter: &str, layer: u32, priority: i16, flags: u64) -> Result<Arc<Handle>> {
        let api = api()?;
        let c = CString::new(filter)?;
        // 1058/1072: another program (e.g. Zapret's "remove services") deleted the
        // WinDivert service while the driver was in use. The service stays "marked for
        // deletion" (shown as disabled) until every handle to it closes, then the next
        // open reinstalls it. Give that a few seconds before giving up.
        let mut attempt = 0;
        loop {
            // SAFETY: valid NUL-terminated filter string.
            let raw = unsafe { (api.open)(c.as_ptr(), layer, priority, flags) };
            if raw != INVALID_HANDLE_VALUE && !raw.is_null() {
                return Ok(Arc::new(Handle { raw, api }));
            }
            let code = unsafe { GetLastError() };
            attempt += 1;
            if !matches!(code, 1058 | 1072) || attempt >= 10 {
                bail!("WinDivertOpen(\"{filter}\") failed: {}", explain(code));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    /// Blocks until a packet arrives. Returns the packet length.
    pub fn recv(&self, buf: &mut [u8], addr: &mut Address) -> Result<usize> {
        let mut len = 0u32;
        // SAFETY: buffer and address outlive the call.
        let ok = unsafe {
            (self.api.recv)(
                self.raw,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut len,
                addr,
            )
        };
        if ok == 0 {
            bail!("WinDivertRecv failed: error {}", unsafe { GetLastError() });
        }
        Ok(len as usize)
    }

    pub fn send(&self, pkt: &[u8], addr: &Address) -> Result<()> {
        let mut len = 0u32;
        // SAFETY: packet and address outlive the call.
        let ok = unsafe {
            (self.api.send)(
                self.raw,
                pkt.as_ptr().cast(),
                pkt.len() as u32,
                &mut len,
                addr,
            )
        };
        if ok == 0 {
            bail!("WinDivertSend failed: error {}", unsafe { GetLastError() });
        }
        Ok(())
    }

    /// Wakes a thread blocked in `recv`; it then returns an error.
    pub fn shutdown(&self) {
        // SAFETY: handle is open until drop.
        unsafe { (self.api.shutdown)(self.raw, SHUTDOWN_BOTH) };
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: closed exactly once.
        unsafe { (self.api.close)(self.raw) };
    }
}

fn explain(code: u32) -> String {
    let hint = match code {
        2 => "driver files not found: WinDivert64.sys must be next to WinDivert.dll",
        5 => "access denied: run as Administrator",
        87 => "invalid parameter or filter",
        577 => "driver signature rejected (Secure Boot / HVCI policy?)",
        654 => "an incompatible WinDivert driver version is already loaded; reboot or stop it",
        1060 => "driver service missing",
        1058 | 1072 => {
            "the WinDivert service was removed by another program (Zapret 'remove services', \
             GoodbyeDPI...) while in use; close that program and the Services window, or reboot"
        }
        1275 => "driver blocked by the system or antivirus",
        1753 => "Base Filtering Engine service is not running",
        _ => "",
    };
    if hint.is_empty() {
        format!("error {code}")
    } else {
        format!("error {code} ({hint})")
    }
}
