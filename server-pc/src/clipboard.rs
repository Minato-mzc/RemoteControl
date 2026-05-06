//! Win32 system clipboard read/write — text only (CF_UNICODETEXT).
//!
//! Both functions take/return UTF-8 strings. The Win32 layer uses UTF-16
//! internally; we round-trip through `encode_utf16` / `from_utf16_lossy`.
//!
//! Each call opens, manipulates, and closes the clipboard inside the same
//! function — Windows requires `CloseClipboard` to be paired with each
//! `OpenClipboard`. We pass `HWND::default()` (no owner window) which is
//! fine for one-shot reads/writes.

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

pub fn read_text() -> Result<String> {
    unsafe {
        OpenClipboard(HWND::default()).context("OpenClipboard")?;
        let result = (|| {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).context("GetClipboardData")?;
            if h.0.is_null() {
                bail!("clipboard has no CF_UNICODETEXT");
            }
            let hg = windows::Win32::Foundation::HGLOBAL(h.0);
            let ptr = GlobalLock(hg) as *const u16;
            if ptr.is_null() {
                bail!("GlobalLock returned null");
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
                if len > 16 * 1024 * 1024 {
                    let _ = GlobalUnlock(hg);
                    bail!("clipboard text larger than 16 MiB");
                }
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let s = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(hg);
            Ok(s)
        })();
        let _ = CloseClipboard();
        result
    }
}

pub fn write_text(text: &str) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0); // null terminator
    let bytes = utf16.len() * 2;

    unsafe {
        OpenClipboard(HWND::default()).context("OpenClipboard")?;
        let result = (|| {
            EmptyClipboard().context("EmptyClipboard")?;
            let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes).context("GlobalAlloc")?;
            let dst = GlobalLock(hmem) as *mut u16;
            if dst.is_null() {
                bail!("GlobalLock(write) returned null");
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
            let _ = GlobalUnlock(hmem);
            // SetClipboardData takes ownership of the HGLOBAL on success.
            SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(hmem.0))
                .context("SetClipboardData")?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}
