//! System calls for the screen, input and windows (a child module of `syscall`,
//! so it shares that module's helpers for checking user pointers).
//!
//!   21 gfx_claim(out)              take the screen and input; out gets 8 u64: framebuffer
//!                                  address, width, height, pitch (bytes), red/green/blue
//!                                  shift, total bytes
//!   22 mem_alloc(pages)            fresh zeroed pages; returns their address
//!   23 input_next(out, timeout)    the desktop's next input event (16 bytes); 1 = got one,
//!                                  0 = timed out
//!   24 win_create(w, h, title, title_len, out)
//!                                  out gets two u64: window id, address of its pixels
//!   25 win_destroy(id)
//!   26 win_list(buf, max)          the desktop: up to max 64-byte descriptors: id u32,
//!                                  width u32, height u32, title length u32, owner pid u64,
//!                                  title[32], redraw counter u64; returns how many
//!   27 win_map(id)                 the desktop: map a window's pixels read-only; returns address
//!   28 win_post(id, event)         the desktop: send a window event to its owner
//!   29 win_event(id, out, timeout) the owner: next event for its window; 1 / 0 as above
//!   30 win_flush(id)               the owner: the window's pixels changed
//!
//! A timeout is in timer ticks; 0 only looks, u64::MAX waits as long as it takes.

use super::{copy_from_user, copy_to_user, walk_user, EBADF, EINVAL};
use crate::input::{self, Event};
use crate::{proc, winsys};
use alloc::vec::Vec;

fn timeout_arg(ticks: u64) -> Option<u64> {
    if ticks == u64::MAX {
        None
    } else {
        Some(ticks)
    }
}

fn window_id(arg: u64) -> Result<u32, i64> {
    u32::try_from(arg).map_err(|_| EINVAL)
}

fn encode_event(event: &Event) -> [u8; 16] {
    let mut raw = [0u8; 16];
    raw[0..4].copy_from_slice(&event.kind.to_le_bytes());
    raw[4..8].copy_from_slice(&event.a.to_le_bytes());
    raw[8..12].copy_from_slice(&event.b.to_le_bytes());
    raw[12..16].copy_from_slice(&event.c.to_le_bytes());
    raw
}

fn decode_event(raw: &[u8]) -> Event {
    let word = |at: usize| [raw[at], raw[at + 1], raw[at + 2], raw[at + 3]];
    Event {
        kind: u32::from_le_bytes(word(0)),
        a: i32::from_le_bytes(word(4)),
        b: i32::from_le_bytes(word(8)),
        c: i32::from_le_bytes(word(12)),
    }
}

/// The caller's pid if it owns the screen; otherwise the error to return.
fn require_compositor() -> Result<u64, i64> {
    let pid = proc::current_pid();
    if winsys::is_compositor(pid) {
        Ok(pid)
    } else {
        Err(winsys::EPERM)
    }
}

pub(super) fn sys_gfx_claim(out_ptr: u64) -> i64 {
    // Check the destination first: nothing is changed if it is bad.
    if let Err(e) = walk_user(out_ptr, 64, true, |_, _| {}) {
        return e;
    }
    let info = match winsys::claim_screen(proc::current_pid()) {
        Ok(info) => info,
        Err(e) => return e,
    };
    let fields = [
        winsys::FB_USER_BASE,
        info.width,
        info.height,
        info.pitch,
        info.red_shift,
        info.green_shift,
        info.blue_shift,
        info.bytes,
    ];
    let mut raw = [0u8; 64];
    for (i, value) in fields.iter().enumerate() {
        raw[8 * i..8 * i + 8].copy_from_slice(&value.to_le_bytes());
    }
    match copy_to_user(out_ptr, &raw) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_mem_alloc(pages: u64) -> i64 {
    let Some(mem) = proc::current_mem() else { return EBADF };
    match winsys::user_alloc(&mem, pages) {
        Ok(addr) => addr as i64,
        Err(e) => e,
    }
}

pub(super) fn sys_input_next(out_ptr: u64, timeout_ticks: u64) -> i64 {
    if let Err(e) = require_compositor() {
        return e;
    }
    if let Err(e) = walk_user(out_ptr, 16, true, |_, _| {}) {
        return e;
    }
    match input::next(timeout_arg(timeout_ticks)) {
        Some(event) => match copy_to_user(out_ptr, &encode_event(&event)) {
            Ok(()) => 1,
            Err(e) => e,
        },
        None => 0,
    }
}

pub(super) fn sys_win_create(w: u64, h: u64, title_ptr: u64, title_len: u64, out_ptr: u64) -> i64 {
    if title_len > 64 {
        return EINVAL;
    }
    let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else { return EINVAL };
    let title = match copy_from_user(title_ptr, title_len) {
        Ok(title) => title,
        Err(e) => return e,
    };
    // Check the destination before creating anything, so a bad pointer leaks nothing.
    if let Err(e) = walk_user(out_ptr, 16, true, |_, _| {}) {
        return e;
    }
    let Some(mem) = proc::current_mem() else { return EBADF };
    let pid = proc::current_pid();
    let (id, addr) = match winsys::create_window(pid, &mem, w, h, &title) {
        Ok(created) => created,
        Err(e) => return e,
    };
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&(id as u64).to_le_bytes());
    raw[8..16].copy_from_slice(&addr.to_le_bytes());
    match copy_to_user(out_ptr, &raw) {
        Ok(()) => 0,
        Err(e) => {
            let _ = winsys::destroy_window(pid, id);
            e
        }
    }
}

pub(super) fn sys_win_destroy(id: u64) -> i64 {
    let id = match window_id(id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match winsys::destroy_window(proc::current_pid(), id) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_win_flush(id: u64) -> i64 {
    let id = match window_id(id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match winsys::flush_window(proc::current_pid(), id) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_win_list(buf_ptr: u64, max: u64) -> i64 {
    if let Err(e) = require_compositor() {
        return e;
    }
    if max == 0 || max > 64 {
        return EINVAL;
    }
    if let Err(e) = walk_user(buf_ptr, max * 64, true, |_, _| {}) {
        return e;
    }
    let windows = winsys::list_windows();
    let count = windows.len().min(max as usize);
    let mut raw: Vec<u8> = Vec::with_capacity(count * 64);
    for window in windows.iter().take(count) {
        let mut entry = [0u8; 64];
        entry[0..4].copy_from_slice(&window.id.to_le_bytes());
        entry[4..8].copy_from_slice(&window.w.to_le_bytes());
        entry[8..12].copy_from_slice(&window.h.to_le_bytes());
        entry[12..16].copy_from_slice(&(window.title_len as u32).to_le_bytes());
        entry[16..24].copy_from_slice(&window.owner.to_le_bytes());
        entry[24..56].copy_from_slice(&window.title);
        entry[56..64].copy_from_slice(&window.serial().to_le_bytes());
        raw.extend_from_slice(&entry);
    }
    match copy_to_user(buf_ptr, &raw) {
        Ok(()) => count as i64,
        Err(e) => e,
    }
}

pub(super) fn sys_win_map(id: u64) -> i64 {
    if let Err(e) = require_compositor() {
        return e;
    }
    let id = match window_id(id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let Some(mem) = proc::current_mem() else { return EBADF };
    match winsys::map_window(&mem, id) {
        Ok(addr) => addr as i64,
        Err(e) => e,
    }
}

pub(super) fn sys_win_post(id: u64, event_ptr: u64) -> i64 {
    if let Err(e) = require_compositor() {
        return e;
    }
    let id = match window_id(id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let raw = match copy_from_user(event_ptr, 16) {
        Ok(raw) => raw,
        Err(e) => return e,
    };
    let event = decode_event(&raw);
    // Only window events may be posted; the desktop cannot forge input events.
    if !(input::EV_WIN_KEY..=input::EV_WIN_FOCUS).contains(&event.kind) {
        return EINVAL;
    }
    match winsys::post_event(id, event) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(super) fn sys_win_event(id: u64, out_ptr: u64, timeout_ticks: u64) -> i64 {
    let id = match window_id(id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    if let Err(e) = walk_user(out_ptr, 16, true, |_, _| {}) {
        return e;
    }
    match winsys::next_window_event(proc::current_pid(), id, timeout_arg(timeout_ticks)) {
        Ok(Some(event)) => match copy_to_user(out_ptr, &encode_event(&event)) {
            Ok(()) => 1,
            Err(e) => e,
        },
        Ok(None) => 0,
        Err(e) => e,
    }
}
