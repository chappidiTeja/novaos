//! The window manager: where windows are, which is in front, who has focus, what
//! a click or a key does, and how the whole desktop is drawn.
//!
//! It is pure logic: no system calls, no memory allocation. The `desktop`
//! program feeds it input events and the list of windows the kernel reports, and
//! carries out the `Action`s it returns (post an event to a window, start a
//! program, quit). That makes it testable on its own.
//!
//! # Layout (the screen tests re-derive it independently; keep them in step)
//!
//! A window's *client* area is its pixel buffer. Around it: a 2-pixel border on
//! the left, right, bottom and top, and a 28-pixel title bar above the client.
//!
//!   outer width  = client width  + 2*BORDER
//!   outer height = client height + TITLE_H + 2*BORDER
//!   title bar    = (x + BORDER, y + BORDER, client width, TITLE_H)
//!   client       = (x + BORDER, y + BORDER + TITLE_H)
//!   close button = 24x24, in the title bar's right end, 2 pixels from its top and right
//!
//! The taskbar is the bottom 40 pixels. Its Start button is at (4, top + 4),
//! 84x32; each window has a 150x32 button after it, 154 pixels apart starting at
//! x = 96, in the order the windows were created. The clock ("MM:SS" since boot)
//! is at the right end. The Start menu rises from the Start button.

#![cfg_attr(not(test), no_std)]

use gfx::{rgb, text_width, Surface};
use novaabi::*;

pub const MAX_WINDOWS: usize = 24;

pub const BORDER: i32 = 2;
pub const TITLE_H: i32 = 28;
pub const CLOSE_SIZE: i32 = 24;
pub const TASKBAR_H: i32 = 40;
pub const START_X: i32 = 4;
pub const START_W: i32 = 84;
pub const START_H: i32 = 32;
pub const TASK_BTN_X0: i32 = 96;
pub const TASK_BTN_W: i32 = 150;
pub const TASK_BTN_STEP: i32 = 154;
pub const TASK_BTN_H: i32 = 32;
pub const MENU_X: i32 = 4;
pub const MENU_W: i32 = 220;
pub const MENU_ITEM_H: i32 = 40;
pub const MENU_BORDER: i32 = 2;
/// Where the first window goes, and how far each next one is shifted.
pub const CASCADE_X: i32 = 80;
pub const CASCADE_Y: i32 = 60;
pub const CASCADE_STEP: i32 = 32;

pub const MENU_ITEMS: [&[u8]; 4] = [b"Terminal", b"Graphics demo", b"About NovaOS", b"Exit desktop"];
/// Menu entry that ends the desktop.
pub const MENU_QUIT: usize = 3;

pub const WALLPAPER: u32 = rgb(30, 60, 114);
pub const LOGO: u32 = rgb(60, 100, 160);
pub const TASKBAR_BG: u32 = rgb(24, 24, 32);
pub const TASKBAR_LINE: u32 = rgb(70, 70, 90);
pub const START_BG: u32 = rgb(45, 95, 185);
pub const START_HOT: u32 = rgb(70, 125, 220);
pub const BTN_BG: u32 = rgb(50, 52, 66);
pub const BTN_FOCUS: u32 = rgb(70, 110, 190);
pub const TEXT: u32 = rgb(240, 240, 240);
pub const BORDER_FOCUS: u32 = rgb(70, 130, 230);
pub const BORDER_IDLE: u32 = rgb(80, 86, 100);
pub const TITLE_FOCUS: u32 = rgb(50, 100, 200);
pub const TITLE_IDLE: u32 = rgb(62, 66, 78);
pub const CLOSE_BG: u32 = rgb(200, 60, 60);
pub const CLOSE_HOT: u32 = rgb(230, 90, 90);
pub const MENU_BG: u32 = rgb(36, 40, 52);
pub const MENU_HOT: u32 = rgb(50, 110, 200);
pub const MENU_EDGE: u32 = rgb(70, 76, 92);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

/// A window as the desktop sees it.
#[derive(Clone, Copy)]
pub struct Win {
    pub id: u32,
    /// Top-left corner of the outer frame.
    pub x: i32,
    pub y: i32,
    /// Size of the client area (the pixel buffer).
    pub cw: i32,
    pub ch: i32,
    pub title: [u8; 32],
    pub title_len: usize,
    /// The window's pixels (`cw * ch` of them), mapped read-only.
    pub pixels: *const u32,
    pub serial: u64,
}

impl Win {
    pub fn outer(&self) -> Rect {
        Rect::new(self.x, self.y, self.cw + 2 * BORDER, self.ch + TITLE_H + 2 * BORDER)
    }

    pub fn title_bar(&self) -> Rect {
        Rect::new(self.x + BORDER, self.y + BORDER, self.cw, TITLE_H)
    }

    pub fn client(&self) -> Rect {
        Rect::new(self.x + BORDER, self.y + BORDER + TITLE_H, self.cw, self.ch)
    }

    pub fn close_button(&self) -> Rect {
        let t = self.title_bar();
        Rect::new(t.x + t.w - CLOSE_SIZE - 2, t.y + 2, CLOSE_SIZE, CLOSE_SIZE)
    }

    pub fn title(&self) -> &[u8] {
        &self.title[..self.title_len.min(32)]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Send this event to the window's owner.
    Post(u32, Event),
    /// Start the program for this Start menu entry (0..MENU_QUIT).
    Launch(usize),
    /// Leave the desktop.
    Quit,
}

/// A fixed-size list of actions (there is no heap in user programs).
pub struct Actions {
    items: [Option<Action>; 64],
    len: usize,
}

impl Actions {
    pub const fn new() -> Actions {
        Actions { items: [None; 64], len: 0 }
    }

    pub fn push(&mut self, action: Action) {
        // 64 per batch is far more than one input event can produce.
        if self.len < self.items.len() {
            self.items[self.len] = Some(action);
            self.len += 1;
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn get(&self, i: usize) -> Option<Action> {
        if i < self.len {
            self.items[i]
        } else {
            None
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Action> + '_ {
        self.items[..self.len].iter().filter_map(|a| *a)
    }
}

/// What is under a point on the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Nothing,
    Start,
    /// A taskbar button for this window.
    TaskButton(u32),
    /// An entry of the Start menu.
    MenuItem(usize),
    /// The Start menu's frame.
    Menu,
    Close(u32),
    Title(u32),
    Client(u32),
    Border(u32),
    /// The taskbar itself, not a button.
    Taskbar,
}

pub struct Desktop {
    width: i32,
    height: i32,
    wins: [Option<Win>; MAX_WINDOWS],
    /// Slots of `wins` from the back of the screen to the front.
    order: [usize; MAX_WINDOWS],
    count: usize,
    focus: Option<u32>,
    pointer: (i32, i32),
    buttons: i32,
    /// A title-bar drag in progress: window id and where inside the window it was grabbed.
    drag: Option<(u32, i32, i32)>,
    /// The window whose close button the left button went down on.
    close_press: Option<u32>,
    /// The window that got the press in its client area; it gets the moves and
    /// the release too, wherever the pointer goes.
    capture: Option<u32>,
    menu_open: bool,
    next_x: i32,
    next_y: i32,
    dirty: bool,
}

impl Desktop {
    pub fn new(width: i32, height: i32) -> Desktop {
        Desktop {
            width,
            height,
            wins: [None; MAX_WINDOWS],
            order: [0; MAX_WINDOWS],
            count: 0,
            focus: None,
            pointer: (width / 2, height / 2),
            buttons: 0,
            drag: None,
            close_press: None,
            capture: None,
            menu_open: false,
            next_x: CASCADE_X,
            next_y: CASCADE_Y,
            dirty: true,
        }
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub fn pointer(&self) -> (i32, i32) {
        self.pointer
    }

    pub fn focus(&self) -> Option<u32> {
        self.focus
    }

    pub fn menu_open(&self) -> bool {
        self.menu_open
    }

    pub fn window_count(&self) -> usize {
        self.count
    }

    /// True once if anything changed that needs the screen redrawn.
    pub fn take_dirty(&mut self) -> bool {
        core::mem::replace(&mut self.dirty, false)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    // ---- the window table ---------------------------------------------------------------

    fn slot_of(&self, id: u32) -> Option<usize> {
        self.wins.iter().position(|w| matches!(w, Some(w) if w.id == id))
    }

    /// Windows from back to front.
    pub fn stacking(&self) -> impl Iterator<Item = &Win> + '_ {
        self.order[..self.count].iter().filter_map(move |&slot| self.wins[slot].as_ref())
    }

    pub fn window(&self, id: u32) -> Option<&Win> {
        self.wins.iter().flatten().find(|w| w.id == id)
    }

    /// Windows in the order they were created (the taskbar order).
    fn ids_by_age(&self, out: &mut [u32; MAX_WINDOWS]) -> usize {
        let mut n = 0;
        for w in self.wins.iter().flatten() {
            out[n] = w.id;
            n += 1;
        }
        out[..n].sort_unstable();
        n
    }

    fn raise_slot(&mut self, slot: usize) {
        if let Some(pos) = self.order[..self.count].iter().position(|&s| s == slot) {
            self.order.copy_within(pos + 1..self.count, pos);
            self.order[self.count - 1] = slot;
        }
    }

    fn remove_slot(&mut self, slot: usize) {
        if let Some(pos) = self.order[..self.count].iter().position(|&s| s == slot) {
            self.order.copy_within(pos + 1..self.count, pos);
            self.count -= 1;
        }
        self.wins[slot] = None;
    }

    fn set_focus(&mut self, id: Option<u32>, out: &mut Actions) {
        if self.focus == id {
            return;
        }
        if let Some(old) = self.focus {
            if self.slot_of(old).is_some() {
                out.push(Action::Post(old, Event::new(EV_WIN_FOCUS, 0, 0, 0)));
            }
        }
        self.focus = id;
        if let Some(new) = id {
            out.push(Action::Post(new, Event::new(EV_WIN_FOCUS, 1, 0, 0)));
        }
        self.dirty = true;
    }

    /// Bring a window to the front and give it focus.
    fn activate(&mut self, id: u32, out: &mut Actions) {
        if let Some(slot) = self.slot_of(id) {
            self.raise_slot(slot);
            self.set_focus(Some(id), out);
            self.dirty = true;
        }
    }

    /// Bring the desktop's picture of the windows up to date with what the
    /// kernel reports: new windows appear in front (cascaded), vanished ones
    /// go, redrawn ones are marked. `map` maps a new window's pixels into this
    /// process and returns their address (None if that failed: the window is
    /// then skipped until the next time).
    pub fn sync(&mut self, infos: &[WinInfo], map: &mut dyn FnMut(u32) -> Option<*const u32>, out: &mut Actions) {
        // Windows that are gone.
        for slot in 0..MAX_WINDOWS {
            let Some(win) = self.wins[slot] else { continue };
            if !infos.iter().any(|i| i.id == win.id) {
                if self.focus == Some(win.id) {
                    self.focus = None;
                }
                if self.drag.map(|d| d.0) == Some(win.id) {
                    self.drag = None;
                }
                if self.close_press == Some(win.id) {
                    self.close_press = None;
                }
                if self.capture == Some(win.id) {
                    self.capture = None;
                }
                self.remove_slot(slot);
                self.dirty = true;
            }
        }
        // New and changed windows.
        for info in infos {
            if info.width == 0 || info.height == 0 || info.width > 4096 || info.height > 4096 {
                continue;
            }
            if let Some(slot) = self.slot_of(info.id) {
                let win = self.wins[slot].as_mut().unwrap();
                if win.serial != info.serial || win.title != info.title {
                    win.serial = info.serial;
                    win.title = info.title;
                    win.title_len = (info.title_len as usize).min(32);
                    self.dirty = true;
                }
                continue;
            }
            let Some(free) = self.wins.iter().position(|w| w.is_none()) else { continue };
            let Some(pixels) = map(info.id) else { continue };
            let (cw, ch) = (info.width as i32, info.height as i32);
            let (mut x, mut y) = (self.next_x, self.next_y);
            // Start over at the top-left when the cascade would run off the screen.
            if x + cw + 2 * BORDER > self.width || y + ch + TITLE_H + 2 * BORDER > self.height - TASKBAR_H {
                x = CASCADE_X;
                y = CASCADE_Y;
            }
            self.next_x = x + CASCADE_STEP;
            self.next_y = y + CASCADE_STEP;
            self.wins[free] = Some(Win {
                id: info.id,
                x,
                y,
                cw,
                ch,
                title: info.title,
                title_len: (info.title_len as usize).min(32),
                pixels,
                serial: info.serial,
            });
            self.order[self.count] = free;
            self.count += 1;
            self.dirty = true;
            self.activate(info.id, out);
        }
        // If the focused window went away, the one now in front takes over.
        if self.focus.is_none() && self.count > 0 {
            let top = self.wins[self.order[self.count - 1]].map(|w| w.id);
            self.set_focus(top, out);
        }
    }

    // ---- where things are ---------------------------------------------------------------------

    pub fn taskbar_top(&self) -> i32 {
        self.height - TASKBAR_H
    }

    pub fn start_button(&self) -> Rect {
        Rect::new(START_X, self.taskbar_top() + 4, START_W, START_H)
    }

    pub fn task_button(&self, index: usize) -> Rect {
        Rect::new(TASK_BTN_X0 + index as i32 * TASK_BTN_STEP, self.taskbar_top() + 4, TASK_BTN_W, TASK_BTN_H)
    }

    pub fn menu_rect(&self) -> Rect {
        let h = MENU_ITEMS.len() as i32 * MENU_ITEM_H + 2 * MENU_BORDER;
        Rect::new(MENU_X, self.taskbar_top() - h, MENU_W, h)
    }

    pub fn menu_item_rect(&self, i: usize) -> Rect {
        let m = self.menu_rect();
        Rect::new(m.x + MENU_BORDER, m.y + MENU_BORDER + i as i32 * MENU_ITEM_H, MENU_W - 2 * MENU_BORDER, MENU_ITEM_H)
    }

    pub fn clock_x(&self) -> i32 {
        self.width - 88
    }

    pub fn hit(&self, x: i32, y: i32) -> Hit {
        if self.menu_open {
            for i in 0..MENU_ITEMS.len() {
                if self.menu_item_rect(i).contains(x, y) {
                    return Hit::MenuItem(i);
                }
            }
            if self.menu_rect().contains(x, y) {
                return Hit::Menu;
            }
        }
        if y >= self.taskbar_top() {
            if self.start_button().contains(x, y) {
                return Hit::Start;
            }
            let mut ids = [0u32; MAX_WINDOWS];
            let n = self.ids_by_age(&mut ids);
            for (i, &id) in ids[..n].iter().enumerate() {
                if self.task_button(i).contains(x, y) {
                    return Hit::TaskButton(id);
                }
            }
            return Hit::Taskbar;
        }
        for &slot in self.order[..self.count].iter().rev() {
            let Some(w) = self.wins[slot] else { continue };
            if !w.outer().contains(x, y) {
                continue;
            }
            if w.close_button().contains(x, y) {
                return Hit::Close(w.id);
            }
            if w.title_bar().contains(x, y) {
                return Hit::Title(w.id);
            }
            if w.client().contains(x, y) {
                return Hit::Client(w.id);
            }
            return Hit::Border(w.id);
        }
        Hit::Nothing
    }

    // ---- input ---------------------------------------------------------------------------------

    /// React to one input event from the kernel.
    pub fn handle(&mut self, event: Event, out: &mut Actions) {
        match event.kind {
            EV_MOUSE_MOVE => self.on_move(event.a, event.b, event.c, out),
            EV_MOUSE_BUTTON => self.on_button(event.a, event.b, event.c, out),
            EV_KEY => self.on_key(event.a, out),
            EV_DAMAGE => self.dirty = true,
            _ => {}
        }
    }

    fn client_relative(&self, id: u32, x: i32, y: i32) -> (i32, i32) {
        match self.window(id) {
            Some(w) => {
                let c = w.client();
                (x - c.x, y - c.y)
            }
            None => (0, 0),
        }
    }

    fn on_move(&mut self, x: i32, y: i32, buttons: i32, out: &mut Actions) {
        self.pointer = (x, y);
        self.buttons = buttons;
        self.dirty = true;
        if let Some((id, grab_x, grab_y)) = self.drag {
            self.move_window(id, x - grab_x, y - grab_y);
            return;
        }
        if let Some(id) = self.capture {
            let (rx, ry) = self.client_relative(id, x, y);
            out.push(Action::Post(id, Event::new(EV_WIN_MOVE, rx, ry, buttons)));
            return;
        }
        if let Hit::Client(id) = self.hit(x, y) {
            let (rx, ry) = self.client_relative(id, x, y);
            out.push(Action::Post(id, Event::new(EV_WIN_MOVE, rx, ry, buttons)));
        }
    }

    fn move_window(&mut self, id: u32, x: i32, y: i32) {
        let (width, bottom) = (self.width, self.taskbar_top());
        if let Some(slot) = self.slot_of(id) {
            let w = self.wins[slot].as_mut().unwrap();
            // Keep the title bar reachable: at least 40 pixels of it on screen
            // horizontally, and never above the top or under the taskbar.
            let outer_w = w.cw + 2 * BORDER;
            w.x = x.clamp(-(outer_w - 40), width - 40);
            w.y = y.clamp(0, bottom - TITLE_H - BORDER);
            self.dirty = true;
        }
    }

    fn on_button(&mut self, x: i32, y: i32, buttons: i32, out: &mut Actions) {
        let was_left = self.buttons & BUTTON_LEFT != 0;
        let is_left = buttons & BUTTON_LEFT != 0;
        let changed = self.buttons ^ buttons;
        self.pointer = (x, y);
        self.buttons = buttons;
        self.dirty = true;

        if is_left && !was_left {
            self.left_pressed(x, y, buttons, out);
        } else if !is_left && was_left {
            self.left_released(x, y, buttons, out);
        } else if changed != 0 {
            // Another button changed: it goes to the window under the pointer
            // (or the one that captured the pointer), and a press also focuses it.
            let target = self.capture.map(Hit::Client).unwrap_or_else(|| self.hit(x, y));
            if let Hit::Client(id) = target {
                if changed & buttons != 0 {
                    self.activate(id, out);
                }
                let (rx, ry) = self.client_relative(id, x, y);
                out.push(Action::Post(id, Event::new(EV_WIN_BUTTON, rx, ry, buttons)));
            }
        }
    }

    fn left_pressed(&mut self, x: i32, y: i32, buttons: i32, out: &mut Actions) {
        let hit = self.hit(x, y);
        if self.menu_open {
            self.menu_open = false;
            match hit {
                Hit::MenuItem(i) if i == MENU_QUIT => out.push(Action::Quit),
                Hit::MenuItem(i) => out.push(Action::Launch(i)),
                // Anything else just closes the menu; the Start button toggles it.
                _ => {}
            }
            return;
        }
        match hit {
            Hit::Start => self.menu_open = true,
            Hit::TaskButton(id) => self.activate(id, out),
            Hit::Close(id) => {
                self.activate(id, out);
                self.close_press = Some(id);
            }
            Hit::Title(id) => {
                self.activate(id, out);
                if let Some(w) = self.window(id) {
                    self.drag = Some((id, x - w.x, y - w.y));
                }
            }
            Hit::Client(id) => {
                self.activate(id, out);
                self.capture = Some(id);
                let (rx, ry) = self.client_relative(id, x, y);
                out.push(Action::Post(id, Event::new(EV_WIN_BUTTON, rx, ry, buttons)));
            }
            Hit::Border(id) => self.activate(id, out),
            _ => {}
        }
    }

    fn left_released(&mut self, x: i32, y: i32, buttons: i32, out: &mut Actions) {
        self.drag = None;
        if let Some(id) = self.close_press.take() {
            // Only a press and release both on the button close the window.
            if self.window(id).map(|w| w.close_button().contains(x, y)).unwrap_or(false) {
                out.push(Action::Post(id, Event::new(EV_WIN_CLOSE, 0, 0, 0)));
            }
        }
        if let Some(id) = self.capture.take() {
            let (rx, ry) = self.client_relative(id, x, y);
            out.push(Action::Post(id, Event::new(EV_WIN_BUTTON, rx, ry, buttons)));
        }
    }

    fn on_key(&mut self, key: i32, out: &mut Actions) {
        if self.menu_open {
            if key == 27 {
                self.menu_open = false;
                self.dirty = true;
            }
            return;
        }
        if let Some(id) = self.focus {
            out.push(Action::Post(id, Event::new(EV_WIN_KEY, key, 0, 0)));
        }
    }

    // ---- drawing ---------------------------------------------------------------------------------

    /// Draw the whole desktop onto `back` (which must be `width` x `height`).
    pub fn render(&self, back: &mut Surface, uptime_secs: u64) {
        back.clear(WALLPAPER);
        // A big dim name in the middle of the free area.
        let logo = b"NovaOS";
        let lx = (self.width - text_width(logo.len(), 6)) / 2;
        let ly = self.taskbar_top() / 2 - 24;
        back.draw_text(lx, ly, logo, 6, LOGO);

        for w in self.stacking() {
            self.draw_window(back, w);
        }
        self.draw_taskbar(back, uptime_secs);
        if self.menu_open {
            self.draw_menu(back);
        }
        back.draw_cursor(self.pointer.0, self.pointer.1);
    }

    fn draw_window(&self, back: &mut Surface, w: &Win) {
        let focused = self.focus == Some(w.id);
        let o = w.outer();
        back.fill_rect(o.x, o.y, o.w, o.h, if focused { BORDER_FOCUS } else { BORDER_IDLE });
        let t = w.title_bar();
        back.fill_rect(t.x, t.y, t.w, t.h, if focused { TITLE_FOCUS } else { TITLE_IDLE });

        // The title, cut short so it stays clear of the close button.
        let room = (w.cw - 8 - CLOSE_SIZE - 6).max(0) / 16;
        let shown = w.title().len().min(room as usize);
        back.draw_text(t.x + 8, t.y + 6, &w.title()[..shown], 2, TEXT);

        let c = w.close_button();
        let hot = self.hit(self.pointer.0, self.pointer.1) == Hit::Close(w.id);
        back.fill_rect(c.x, c.y, c.w, c.h, if hot { CLOSE_HOT } else { CLOSE_BG });
        back.draw_char(c.x + 4, c.y + 4, b'X', 2, TEXT);

        // The window's own pixels.
        let cl = w.client();
        let source = unsafe { Surface::from_raw(w.pixels as *mut u32, w.cw as usize, w.ch as usize, w.cw as usize) };
        back.blit(cl.x, cl.y, &source);
    }

    fn draw_taskbar(&self, back: &mut Surface, uptime_secs: u64) {
        let top = self.taskbar_top();
        back.fill_rect(0, top, self.width, TASKBAR_H, TASKBAR_BG);
        back.fill_rect(0, top, self.width, 1, TASKBAR_LINE);

        let s = self.start_button();
        let hot = self.menu_open || self.hit(self.pointer.0, self.pointer.1) == Hit::Start;
        back.fill_rect(s.x, s.y, s.w, s.h, if hot { START_HOT } else { START_BG });
        back.draw_text(s.x + 2, s.y + 8, b"Start", 2, TEXT);

        let mut ids = [0u32; MAX_WINDOWS];
        let n = self.ids_by_age(&mut ids);
        for (i, &id) in ids[..n].iter().enumerate() {
            let Some(w) = self.window(id) else { continue };
            let b = self.task_button(i);
            if b.x >= self.width {
                break;
            }
            let focused = self.focus == Some(id);
            back.fill_rect(b.x, b.y, b.w, b.h, if focused { BTN_FOCUS } else { BTN_BG });
            let shown = w.title().len().min(8);
            back.draw_text(b.x + 8, b.y + 8, &w.title()[..shown], 2, TEXT);
        }

        let secs = uptime_secs;
        let clock = [
            b'0' + ((secs / 60 / 10) % 10) as u8,
            b'0' + ((secs / 60) % 10) as u8,
            b':',
            b'0' + ((secs % 60) / 10) as u8,
            b'0' + (secs % 10) as u8,
        ];
        back.draw_text(self.clock_x(), top + 12, &clock, 2, TEXT);
    }

    fn draw_menu(&self, back: &mut Surface) {
        let m = self.menu_rect();
        back.fill_rect(m.x, m.y, m.w, m.h, MENU_EDGE);
        let hover = match self.hit(self.pointer.0, self.pointer.1) {
            Hit::MenuItem(i) => Some(i),
            _ => None,
        };
        for (i, label) in MENU_ITEMS.iter().enumerate() {
            let r = self.menu_item_rect(i);
            back.fill_rect(r.x, r.y, r.w, r.h, if hover == Some(i) { MENU_HOT } else { MENU_BG });
            back.draw_text(r.x + 16, r.y + 12, label, 2, TEXT);
        }
    }
}

#[cfg(test)]
mod tests;
