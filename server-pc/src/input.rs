//! Mouse input injection via Win32 `SendInput`.
//!
//! Absolute moves use `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE` with the
//! 16-bit normalized coordinate space (`0..=0xFFFF` mapped to the primary
//! display's `[0..width-1] × [0..height-1]`). We deliberately don't combine
//! with `MOUSEEVENTF_VIRTUALDESK` — M3 targets the same monitor we capture,
//! which is the primary one.
//!
//! Failures are logged at the call site rather than torn down through the
//! whole connection: a transient driver hiccup shouldn't break the stream.

use anyhow::{bail, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
};

use crate::protocol::MouseButton;

/// One wheel notch. Documented in WinAPI.
const WHEEL_DELTA: i32 = 120;

pub fn move_to(x_norm: f32, y_norm: f32) -> Result<()> {
    let dx = (x_norm.clamp(0.0, 1.0) * 65535.0).round() as i32;
    let dy = (y_norm.clamp(0.0, 1.0) * 65535.0).round() as i32;
    send_mouse(MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: 0,
    })
}

pub fn button(b: MouseButton, down: bool) -> Result<()> {
    let flag = match (b, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    };
    send_mouse(MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: 0,
        dwFlags: flag,
        time: 0,
        dwExtraInfo: 0,
    })
}

pub fn scroll(dx: i32, dy: i32) -> Result<()> {
    if dy != 0 {
        send_mouse(MOUSEINPUT {
            dx: 0,
            dy: 0,
            // mouseData carries the signed wheel delta but the field is u32; cast preserves bits.
            mouseData: (dy.saturating_mul(WHEEL_DELTA)) as u32,
            dwFlags: MOUSEEVENTF_WHEEL,
            time: 0,
            dwExtraInfo: 0,
        })?;
    }
    if dx != 0 {
        send_mouse(MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: (dx.saturating_mul(WHEEL_DELTA)) as u32,
            dwFlags: MOUSEEVENTF_HWHEEL,
            time: 0,
            dwExtraInfo: 0,
        })?;
    }
    Ok(())
}

fn send_mouse(mi: MOUSEINPUT) -> Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    };
    unsafe {
        let n = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        if n != 1 {
            bail!("SendInput returned {n}");
        }
    }
    Ok(())
}

// ---- M3.5: keyboard ----

/// Inject a Unicode string as keyboard events (one KEYEVENTF_UNICODE down+up
/// per UTF-16 code unit). BMP scalars are one event pair; supplementary
/// scalars (e.g. emoji) are two surrogate pairs each. Bypasses PC IME — CJK
/// characters arrive in the focused field directly.
pub fn type_unicode(text: &str) -> Result<()> {
    let mut units = [0u16; 2];
    for ch in text.chars() {
        let encoded = ch.encode_utf16(&mut units);
        for &unit in encoded.iter() {
            send_kb(KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE,
                time: 0,
                dwExtraInfo: 0,
            })?;
            send_kb(KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            })?;
        }
    }
    Ok(())
}

/// Send one virtual-key down or up. `vk` is a Win32 VK_* code (1..=255).
pub fn vkey(vk: u32, down: bool) -> Result<()> {
    if vk == 0 || vk > 0xFF {
        bail!("vk {vk} out of range (1..=255)");
    }
    let mut flags = windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0);
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    send_kb(KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk as u16),
        wScan: 0,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    })
}

fn send_kb(ki: KEYBDINPUT) -> Result<()> {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    };
    unsafe {
        let n = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        if n != 1 {
            bail!("SendInput keyboard returned {n}");
        }
    }
    Ok(())
}
