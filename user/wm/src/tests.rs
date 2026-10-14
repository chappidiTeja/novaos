use super::*;
use std::vec::Vec;

/// A desktop plus the pixel buffers its windows "live" in.
struct World {
    desk: Desktop,
    pixels: Vec<(u32, Vec<u32>)>,
    infos: Vec<WinInfo>,
}

fn info(id: u32, w: u32, h: u32, title: &str, serial: u64) -> WinInfo {
    let mut t = [0u8; 32];
    t[..title.len()].copy_from_slice(title.as_bytes());
    WinInfo { id, width: w, height: h, title_len: title.len() as u32, owner: 100 + id as u64, title: t, serial }
}

impl World {
    fn new(w: i32, h: i32) -> World {
        World { desk: Desktop::new(w, h), pixels: Vec::new(), infos: Vec::new() }
    }

    /// Add (or update) a window: its pixels are filled with `color`.
    fn open(&mut self, id: u32, w: u32, h: u32, title: &str, color: u32) -> Actions {
        self.pixels.push((id, vec![color; (w * h) as usize]));
        self.infos.push(info(id, w, h, title, 0));
        self.sync()
    }

    fn sync(&mut self) -> Actions {
        let mut out = Actions::new();
        let pixels = &self.pixels;
        let mut map = |id: u32| pixels.iter().find(|(i, _)| *i == id).map(|(_, p)| p.as_ptr());
        self.desk.sync(&self.infos, &mut map, &mut out);
        out
    }

    fn close(&mut self, id: u32) -> Actions {
        self.infos.retain(|i| i.id != id);
        self.sync()
    }

    fn send(&mut self, event: Event) -> Vec<Action> {
        let mut out = Actions::new();
        self.desk.handle(event, &mut out);
        out.iter().collect()
    }

    fn move_to(&mut self, x: i32, y: i32) -> Vec<Action> {
        let buttons = self.desk.buttons;
        self.send(Event::new(EV_MOUSE_MOVE, x, y, buttons))
    }

    fn press(&mut self, x: i32, y: i32) -> Vec<Action> {
        self.send(Event::new(EV_MOUSE_MOVE, x, y, self.desk.buttons));
        self.send(Event::new(EV_MOUSE_BUTTON, x, y, BUTTON_LEFT | self.desk.buttons))
    }

    fn release(&mut self, x: i32, y: i32) -> Vec<Action> {
        self.send(Event::new(EV_MOUSE_MOVE, x, y, self.desk.buttons));
        self.send(Event::new(EV_MOUSE_BUTTON, x, y, self.desk.buttons & !BUTTON_LEFT))
    }

    fn click(&mut self, x: i32, y: i32) -> Vec<Action> {
        let mut all = self.press(x, y);
        all.extend(self.release(x, y));
        all
    }
}

fn post(id: u32, kind: u32, a: i32, b: i32, c: i32) -> Action {
    Action::Post(id, Event::new(kind, a, b, c))
}

fn win(world: &World, id: u32) -> Win {
    *world.desk.window(id).expect("window exists")
}

const W: i32 = 800;
const H: i32 = 600;

fn stack_ids(w: &World) -> Vec<u32> {
    w.desk.stacking().map(|x| x.id).collect()
}

// ---- placement, stacking, focus --------------------------------------------------------------

#[test]
fn a_new_desktop_is_empty_and_needs_drawing() {
    let mut d = Desktop::new(W, H);
    assert_eq!(d.window_count(), 0);
    assert_eq!(d.focus(), None);
    assert!(d.take_dirty());
    assert!(!d.take_dirty(), "taking the flag clears it");
    assert_eq!(d.pointer(), (W / 2, H / 2));
}

#[test]
fn windows_cascade_and_the_newest_gets_focus() {
    let mut w = World::new(W, H);
    let first = w.open(1, 200, 100, "one", 1);
    let a = win(&w, 1);
    assert_eq!((a.x, a.y), (CASCADE_X, CASCADE_Y));
    assert_eq!(w.desk.focus(), Some(1));
    assert_eq!(first.iter().collect::<Vec<_>>(), vec![post(1, EV_WIN_FOCUS, 1, 0, 0)]);

    let second = w.open(2, 200, 100, "two", 2);
    let b = win(&w, 2);
    assert_eq!((b.x, b.y), (CASCADE_X + CASCADE_STEP, CASCADE_Y + CASCADE_STEP));
    assert_eq!(w.desk.focus(), Some(2));
    assert_eq!(
        second.iter().collect::<Vec<_>>(),
        vec![post(1, EV_WIN_FOCUS, 0, 0, 0), post(2, EV_WIN_FOCUS, 1, 0, 0)],
        "the old window hears it lost focus, then the new one that it gained it"
    );
    w.open(3, 200, 100, "three", 3);
    let c = win(&w, 3);
    assert_eq!((c.x, c.y), (CASCADE_X + 2 * CASCADE_STEP, CASCADE_Y + 2 * CASCADE_STEP));
    assert_eq!(stack_ids(&w), vec![1, 2, 3], "newest in front");
}

#[test]
fn the_cascade_starts_over_instead_of_running_off_screen() {
    let mut w = World::new(400, 300);
    w.open(1, 300, 120, "a", 0);
    w.open(2, 300, 120, "b", 0);
    let b = win(&w, 2);
    assert_eq!((b.x, b.y), (CASCADE_X, CASCADE_Y), "no room to shift: back to the corner");
}

#[test]
fn a_window_that_goes_away_hands_focus_to_the_next() {
    let mut w = World::new(W, H);
    w.open(1, 100, 100, "a", 0);
    w.open(2, 100, 100, "b", 0);
    w.open(3, 100, 100, "c", 0);
    let out = w.close(3);
    assert_eq!(stack_ids(&w), vec![1, 2]);
    assert_eq!(w.desk.focus(), Some(2));
    assert_eq!(out.iter().collect::<Vec<_>>(), vec![post(2, EV_WIN_FOCUS, 1, 0, 0)]);
    w.close(1);
    w.close(2);
    assert_eq!(w.desk.window_count(), 0);
    assert_eq!(w.desk.focus(), None);
}

#[test]
fn windows_the_desktop_cannot_map_are_skipped() {
    let mut desk = Desktop::new(W, H);
    let mut out = Actions::new();
    let infos = [info(1, 50, 50, "x", 0)];
    desk.sync(&infos, &mut |_| None, &mut out);
    assert_eq!(desk.window_count(), 0);
    // Absurd sizes from a confused kernel are ignored too.
    let infos = [info(2, 0, 50, "x", 0), info(3, 50, 5000, "x", 0)];
    desk.sync(&infos, &mut |_| panic!("must not map"), &mut out);
    assert_eq!(desk.window_count(), 0);
}

#[test]
fn only_a_changed_window_marks_the_desktop_dirty() {
    let mut w = World::new(W, H);
    w.open(1, 100, 100, "a", 0);
    w.desk.take_dirty();
    w.sync();
    assert!(!w.desk.take_dirty(), "nothing changed");
    w.infos[0].serial = 1;
    w.sync();
    assert!(w.desk.take_dirty(), "the window redrew itself");
    w.infos[0].title[0] = b'Z';
    w.sync();
    assert!(w.desk.take_dirty(), "the title changed");
}

#[test]
fn the_desktop_holds_a_full_set_of_windows_and_ignores_more() {
    let mut w = World::new(W, H);
    for id in 1..=(MAX_WINDOWS as u32 + 3) {
        w.open(id, 40, 40, "w", 0);
    }
    assert_eq!(w.desk.window_count(), MAX_WINDOWS);
}

// ---- hit testing -----------------------------------------------------------------------------

#[test]
fn what_is_under_the_pointer() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "win", 0);
    let a = win(&w, 1);
    let (x, y) = (a.x, a.y);
    assert_eq!(w.desk.hit(x, y), Hit::Border(1), "the outer corner is border");
    assert_eq!(w.desk.hit(x + BORDER, y + BORDER), Hit::Title(1));
    assert_eq!(w.desk.hit(x + BORDER + 5, y + BORDER + TITLE_H - 1), Hit::Title(1));
    assert_eq!(w.desk.hit(x + BORDER, y + BORDER + TITLE_H), Hit::Client(1));
    assert_eq!(w.desk.hit(x + BORDER + 199, y + BORDER + TITLE_H + 99), Hit::Client(1));
    assert_eq!(w.desk.hit(x + BORDER + 200, y + BORDER + TITLE_H + 50), Hit::Border(1), "right border");
    assert_eq!(w.desk.hit(x + BORDER + 50, y + BORDER + TITLE_H + 100), Hit::Border(1), "bottom border");
    let c = a.close_button();
    assert_eq!(w.desk.hit(c.x, c.y), Hit::Close(1));
    assert_eq!(w.desk.hit(c.x + c.w - 1, c.y + c.h - 1), Hit::Close(1));
    assert_eq!(w.desk.hit(c.x - 1, c.y), Hit::Title(1));
    assert_eq!(w.desk.hit(x - 1, y), Hit::Nothing);
    assert_eq!(w.desk.hit(5, 5), Hit::Nothing);
    assert_eq!(w.desk.hit(-10, -10), Hit::Nothing);
}

#[test]
fn where_windows_overlap_the_front_one_gets_the_pointer() {
    let mut w = World::new(W, H);
    w.open(1, 300, 200, "back", 0);
    w.open(2, 300, 200, "front", 0);
    let (back, front) = (win(&w, 1), win(&w, 2));

    // A point inside both windows' client areas.
    let (x, y) = (front.client().x + 10, front.client().y + 10);
    assert!(back.client().contains(x, y), "the test point is really in both windows");
    assert_eq!(w.desk.hit(x, y), Hit::Client(2));

    // The front window's title bar over the back window's client area.
    let (tx, ty) = (front.title_bar().x + 10, front.title_bar().y + 5);
    assert!(back.client().contains(tx, ty));
    assert_eq!(w.desk.hit(tx, ty), Hit::Title(2));

    // A click there reaches the front window, in its own coordinates.
    let down = w.press(x, y);
    assert!(down.contains(&post(2, EV_WIN_BUTTON, 10, 10, BUTTON_LEFT)));
    assert!(!down.iter().any(|a| matches!(a, Action::Post(1, e) if e.kind == EV_WIN_BUTTON)));
    w.release(x, y);

    // Raise the back window: now it owns both spots.
    w.click(back.x + 20, back.y + 10);
    assert_eq!(w.desk.hit(x, y), Hit::Client(1));
    assert_eq!(w.desk.hit(tx, ty), Hit::Client(1));
    let down = w.press(x, y);
    assert!(down.iter().any(|a| matches!(a, Action::Post(1, e) if e.kind == EV_WIN_BUTTON)));
}

#[test]
fn what_is_in_the_taskbar() {
    let mut w = World::new(W, H);
    w.open(7, 100, 80, "a", 0);
    w.open(3, 100, 80, "b", 0);
    let top = H - TASKBAR_H;
    assert_eq!(w.desk.hit(START_X + 1, top + 5), Hit::Start);
    assert_eq!(w.desk.hit(START_X + START_W, top + 5), Hit::Taskbar, "just right of the button");
    assert_eq!(w.desk.hit(START_X + 1, top), Hit::Taskbar, "above the button is the bar's edge");
    // Buttons follow the creation order (by id), not the stacking order.
    assert_eq!(w.desk.hit(TASK_BTN_X0 + 5, top + 5), Hit::TaskButton(3));
    assert_eq!(w.desk.hit(TASK_BTN_X0 + TASK_BTN_STEP + 5, top + 5), Hit::TaskButton(7));
    assert_eq!(w.desk.hit(TASK_BTN_X0 + 2 * TASK_BTN_STEP + 5, top + 5), Hit::Taskbar);
    assert_eq!(w.desk.hit(W - 5, H - 2), Hit::Taskbar);
}

#[test]
fn a_window_cannot_cover_the_taskbar() {
    let mut w = World::new(W, H);
    w.open(1, 700, 500, "big", 0);
    // Drag it far down: the bar stays on top.
    let a = win(&w, 1);
    w.press(a.x + 20, a.y + 10);
    w.move_to(a.x + 20, H + 500);
    assert_eq!(w.desk.hit(W - 30, H - 10), Hit::Taskbar, "the bar is in front of whatever is behind it");
    assert!(win(&w, 1).y + TITLE_H + BORDER <= H - TASKBAR_H, "its title bar stays above the taskbar");
}

// ---- clicking, raising, dragging --------------------------------------------------------------

#[test]
fn clicking_a_window_raises_it_and_moves_focus() {
    let mut w = World::new(W, H);
    w.open(1, 300, 200, "a", 0);
    w.open(2, 300, 200, "b", 0);
    assert_eq!(stack_ids(&w), vec![1, 2]);
    // Window 1's title bar sticks out above window 2.
    let a = win(&w, 1);
    let actions = w.click(a.x + 20, a.y + 10);
    assert_eq!(stack_ids(&w), vec![2, 1]);
    assert_eq!(w.desk.focus(), Some(1));
    assert!(actions.contains(&post(2, EV_WIN_FOCUS, 0, 0, 0)));
    assert!(actions.contains(&post(1, EV_WIN_FOCUS, 1, 0, 0)));
}

#[test]
fn clicking_the_desktop_background_changes_nothing() {
    let mut w = World::new(W, H);
    w.open(1, 100, 100, "a", 0);
    let actions = w.click(700, 100);
    assert!(actions.is_empty());
    assert_eq!(w.desk.focus(), Some(1), "focus stays put");
}

#[test]
fn dragging_the_title_bar_moves_the_window_by_exactly_the_pointer_movement() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let a = win(&w, 1);
    let (grab_x, grab_y) = (a.x + 30, a.y + 10);
    w.press(grab_x, grab_y);
    w.move_to(grab_x + 50, grab_y + 30);
    let moved = win(&w, 1);
    assert_eq!((moved.x, moved.y), (a.x + 50, a.y + 30));
    w.move_to(grab_x - 20, grab_y + 100);
    let moved = win(&w, 1);
    assert_eq!((moved.x, moved.y), (a.x - 20, a.y + 100));
    w.release(grab_x - 20, grab_y + 100);
    // After the release the window stays where it was dropped.
    w.move_to(400, 300);
    let still = win(&w, 1);
    assert_eq!((still.x, still.y), (a.x - 20, a.y + 100));
}

#[test]
fn dragging_keeps_part_of_the_window_reachable() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let a = win(&w, 1);
    let outer_w = a.cw + 2 * BORDER;
    w.press(a.x + 30, a.y + 10);
    w.move_to(-5000, -5000);
    let m = win(&w, 1);
    assert_eq!((m.x, m.y), (-(outer_w - 40), 0), "left and top limits");
    w.move_to(9000, 9000);
    let m = win(&w, 1);
    assert_eq!(m.x, W - 40, "right limit");
    assert_eq!(m.y, H - TASKBAR_H - TITLE_H - BORDER, "bottom limit");
}

#[test]
fn a_drag_is_not_started_by_the_right_button_or_in_the_client_area() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let a = win(&w, 1);
    // A press in the client area captures the pointer; moving does not drag the window.
    w.press(a.x + 50, a.y + BORDER + TITLE_H + 20);
    w.move_to(a.x + 150, a.y + 200);
    let m = win(&w, 1);
    assert_eq!((m.x, m.y), (a.x, a.y));
}

// ---- the close button ----------------------------------------------------------------------------

#[test]
fn press_and_release_on_the_close_button_asks_the_window_to_close() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let c = win(&w, 1).close_button();
    let down = w.press(c.x + 5, c.y + 5);
    assert!(!down.iter().any(|a| matches!(a, Action::Post(_, e) if e.kind == EV_WIN_CLOSE)), "not on press");
    let up = w.release(c.x + 6, c.y + 6);
    assert_eq!(up, vec![post(1, EV_WIN_CLOSE, 0, 0, 0)]);
    assert_eq!(w.desk.window_count(), 1, "the owner decides; the desktop does not remove it itself");
}

#[test]
fn pressing_close_but_releasing_elsewhere_does_nothing() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let a = win(&w, 1);
    let c = a.close_button();
    w.press(c.x + 5, c.y + 5);
    w.move_to(c.x - 100, c.y + 5);
    let up = w.release(c.x - 100, c.y + 5);
    assert!(up.is_empty());
    // It was no drag either.
    assert_eq!((win(&w, 1).x, win(&w, 1).y), (a.x, a.y));
    // Pressing elsewhere and releasing on the button does nothing either.
    w.press(a.x + 30, a.y + 10);
    let up = w.release(c.x + 5, c.y + 5);
    assert!(!up.iter().any(|x| matches!(x, Action::Post(_, e) if e.kind == EV_WIN_CLOSE)));
}

// ---- events for the window's own program ----------------------------------------------------------

#[test]
fn clicks_in_the_client_area_arrive_in_window_coordinates() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let c = win(&w, 1).client();
    let down = w.press(c.x + 5, c.y + 7);
    assert!(down.contains(&post(1, EV_WIN_BUTTON, 5, 7, BUTTON_LEFT)));
    // Moving while the button is held reaches the window even outside it.
    let away = w.move_to(c.x + 500, c.y - 40);
    assert_eq!(away, vec![post(1, EV_WIN_MOVE, 500, -40, BUTTON_LEFT)]);
    let up = w.release(c.x + 500, c.y - 40);
    assert!(up.contains(&post(1, EV_WIN_BUTTON, 500, -40, 0)));
    // After the release the capture is over.
    let free = w.move_to(700, 500);
    assert!(free.is_empty());
}

#[test]
fn moving_over_a_client_area_without_a_button_is_reported_too() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let c = win(&w, 1).client();
    assert_eq!(w.move_to(c.x + 10, c.y + 20), vec![post(1, EV_WIN_MOVE, 10, 20, 0)]);
    // Over the title bar or the desktop it is not.
    let t = win(&w, 1).title_bar();
    assert!(w.move_to(t.x + 10, t.y + 10).is_empty());
    assert!(w.move_to(700, 500).is_empty());
}

#[test]
fn the_other_buttons_go_to_the_window_and_focus_it() {
    let mut w = World::new(W, H);
    w.open(1, 300, 200, "a", 0);
    w.open(2, 300, 200, "b", 0);
    let c1 = win(&w, 1).client();
    // A spot of window 1's client area that window 2 does not cover.
    let (x, y) = (c1.x + 5, c1.y + 5);
    assert_eq!(w.desk.hit(x, y), Hit::Client(1));
    w.send(Event::new(EV_MOUSE_MOVE, x, y, 0));
    let out = w.send(Event::new(EV_MOUSE_BUTTON, x, y, BUTTON_RIGHT));
    assert!(out.contains(&post(1, EV_WIN_BUTTON, 5, 5, BUTTON_RIGHT)));
    assert_eq!(w.desk.focus(), Some(1));
    assert_eq!(stack_ids(&w), vec![2, 1]);
    // Releasing it reports again, without changing the focus.
    let out = w.send(Event::new(EV_MOUSE_BUTTON, x, y, 0));
    assert_eq!(out, vec![post(1, EV_WIN_BUTTON, 5, 5, 0)]);
}

#[test]
fn keys_go_to_the_focused_window_only() {
    let mut w = World::new(W, H);
    assert!(w.send(Event::new(EV_KEY, b'x' as i32, 0, 0)).is_empty(), "nobody to send it to");
    w.open(1, 100, 100, "a", 0);
    w.open(2, 100, 100, "b", 0);
    assert_eq!(w.send(Event::new(EV_KEY, b'x' as i32, 0, 0)), vec![post(2, EV_WIN_KEY, b'x' as i32, 0, 0)]);
    let a = win(&w, 1);
    w.click(a.x + 20, a.y + 10);
    assert_eq!(w.send(Event::new(EV_KEY, 10, 0, 0)), vec![post(1, EV_WIN_KEY, 10, 0, 0)]);
}

#[test]
fn a_damage_notice_only_asks_for_a_redraw() {
    let mut w = World::new(W, H);
    w.desk.take_dirty();
    assert!(w.send(Event::new(EV_DAMAGE, 0, 0, 0)).is_empty());
    assert!(w.desk.take_dirty());
    assert!(w.send(Event::new(99, 1, 2, 3)).is_empty(), "unknown events are ignored");
}

// ---- the Start menu and the taskbar ---------------------------------------------------------------------

fn start_center(w: &World) -> (i32, i32) {
    let r = w.desk.start_button();
    (r.x + 5, r.y + 5)
}

#[test]
fn the_start_button_opens_and_closes_the_menu() {
    let mut w = World::new(W, H);
    let (sx, sy) = start_center(&w);
    w.click(sx, sy);
    assert!(w.desk.menu_open());
    w.click(sx, sy);
    assert!(!w.desk.menu_open(), "pressing Start again closes it");
}

#[test]
fn choosing_a_menu_entry_launches_it_and_closes_the_menu() {
    for i in 0..MENU_QUIT {
        let mut w = World::new(W, H);
        let (sx, sy) = start_center(&w);
        w.click(sx, sy);
        let r = w.desk.menu_item_rect(i);
        let out = w.click(r.x + 10, r.y + 10);
        assert_eq!(out, vec![Action::Launch(i)], "entry {i}");
        assert!(!w.desk.menu_open());
    }
}

#[test]
fn the_exit_entry_quits() {
    let mut w = World::new(W, H);
    let (sx, sy) = start_center(&w);
    w.click(sx, sy);
    let r = w.desk.menu_item_rect(MENU_QUIT);
    assert_eq!(w.click(r.x + 10, r.y + 10), vec![Action::Quit]);
}

#[test]
fn clicking_away_from_the_menu_only_closes_it() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let a = win(&w, 1);
    let (sx, sy) = start_center(&w);
    w.click(sx, sy);
    // Click on the window's client area: the menu closes, the window does not get the click.
    let out = w.click(a.x + 50, a.y + BORDER + TITLE_H + 20);
    assert!(!w.desk.menu_open());
    assert!(out.is_empty(), "the click was spent closing the menu: {out:?}");
    // The menu's own frame (not an entry) also just closes it.
    w.click(sx, sy);
    let m = w.desk.menu_rect();
    let out = w.click(m.x + 1, m.y + 1);
    assert!(out.is_empty());
    assert!(!w.desk.menu_open());
}

#[test]
fn escape_closes_the_menu_and_is_not_typed_into_a_window() {
    let mut w = World::new(W, H);
    w.open(1, 100, 100, "a", 0);
    let (sx, sy) = start_center(&w);
    w.click(sx, sy);
    assert!(w.send(Event::new(EV_KEY, 27, 0, 0)).is_empty());
    assert!(!w.desk.menu_open());
    // Other keys while the menu is open are not typed into windows either.
    w.click(sx, sy);
    assert!(w.send(Event::new(EV_KEY, b'a' as i32, 0, 0)).is_empty());
    assert!(w.desk.menu_open());
}

#[test]
fn taskbar_buttons_raise_and_focus_their_window() {
    let mut w = World::new(W, H);
    w.open(1, 300, 200, "a", 0);
    w.open(2, 300, 200, "b", 0);
    let b0 = w.desk.task_button(0);
    let out = w.click(b0.x + 5, b0.y + 5);
    assert_eq!(w.desk.focus(), Some(1));
    assert_eq!(stack_ids(&w), vec![2, 1]);
    assert!(out.contains(&post(1, EV_WIN_FOCUS, 1, 0, 0)));
}

// ---- drawing -------------------------------------------------------------------------------------------------

struct Screen {
    data: Vec<u32>,
    w: usize,
    h: usize,
}

impl Screen {
    fn new(w: usize, h: usize) -> Screen {
        Screen { data: vec![0; w * h], w, h }
    }
    fn surface(&mut self) -> Surface {
        unsafe { Surface::from_raw(self.data.as_mut_ptr(), self.w, self.h, self.w) }
    }
    fn at(&self, x: i32, y: i32) -> u32 {
        self.data[y as usize * self.w + x as usize]
    }
}

fn draw(world: &World, uptime: u64) -> Screen {
    let (w, h) = world.desk.size();
    let mut screen = Screen::new(w as usize, h as usize);
    world.desk.render(&mut screen.surface(), uptime);
    screen
}

#[test]
fn the_empty_desktop() {
    let mut w = World::new(W, H);
    // Park the pointer in a corner so it hides nothing we check.
    w.send(Event::new(EV_MOUSE_MOVE, W - 1, 0, 0));
    let s = draw(&w, 0);
    assert_eq!(s.at(5, 5), WALLPAPER);
    assert_eq!(s.at(W - 20, H - 60), WALLPAPER);
    assert_eq!(s.at(300, H - TASKBAR_H - 1), WALLPAPER, "just above the taskbar");
    assert_eq!(s.at(300, H - TASKBAR_H), TASKBAR_LINE);
    assert_eq!(s.at(300, H - TASKBAR_H + 1), TASKBAR_BG);
    assert_eq!(s.at(W - 1, H - 1), TASKBAR_BG);
    let start = w.desk.start_button();
    assert_eq!(s.at(start.x, start.y), START_BG);
    assert_eq!(s.at(start.x + start.w - 1, start.y + start.h - 1), START_BG);
    assert_eq!(s.at(start.x + start.w, start.y), TASKBAR_BG, "the button ends where it should");
    // The logo is drawn in the free area, in the logo colour, and nowhere near the edges.
    assert!(s.data.iter().any(|&p| p == LOGO));
}

#[test]
fn a_window_is_drawn_with_border_title_close_button_and_its_own_pixels() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "win", 0x00AB_CDEF);
    w.send(Event::new(EV_MOUSE_MOVE, W - 1, 0, 0));
    let a = win(&w, 1);
    let s = draw(&w, 0);
    let (x, y) = (a.x, a.y);
    assert_eq!(s.at(x, y), BORDER_FOCUS);
    assert_eq!(s.at(x + 1, y + 1), BORDER_FOCUS);
    assert_eq!(s.at(x + BORDER, y + BORDER + 20), TITLE_FOCUS, "title bar below the text");
    let c = a.close_button();
    assert_eq!(s.at(c.x, c.y), CLOSE_BG);
    assert_eq!(s.at(c.x + c.w - 1, c.y + c.h - 1), CLOSE_BG);
    let cl = a.client();
    assert_eq!(s.at(cl.x, cl.y), 0x00AB_CDEF);
    assert_eq!(s.at(cl.x + cl.w - 1, cl.y + cl.h - 1), 0x00AB_CDEF);
    assert_eq!(s.at(cl.x - 1, cl.y), BORDER_FOCUS, "left border");
    assert_eq!(s.at(cl.x + cl.w, cl.y), BORDER_FOCUS, "right border");
    assert_eq!(s.at(cl.x, cl.y + cl.h), BORDER_FOCUS, "bottom border");
    assert_eq!(s.at(x - 1, y), WALLPAPER);
    // The title text is white-ish on the title bar: some pixels are text-coloured.
    let t = a.title_bar();
    let mut text_pixels = 0;
    for ty in t.y..t.y + t.h {
        for tx in t.x..c.x {
            if s.at(tx, ty) == TEXT {
                text_pixels += 1;
            }
        }
    }
    assert!(text_pixels > 20, "the title is drawn ({text_pixels} pixels)");
}

#[test]
fn an_unfocused_window_is_drawn_dimmer() {
    let mut w = World::new(W, H);
    w.open(1, 150, 80, "back", 1);
    w.open(2, 150, 80, "front", 2);
    w.send(Event::new(EV_MOUSE_MOVE, W - 1, 0, 0));
    let a = win(&w, 1);
    let s = draw(&w, 0);
    assert_eq!(s.at(a.x, a.y), BORDER_IDLE);
    assert_eq!(s.at(a.x + BORDER, a.y + BORDER + 20), TITLE_IDLE);
    let b = win(&w, 2);
    assert_eq!(s.at(b.x, b.y), BORDER_FOCUS);
}

#[test]
fn windows_overlap_with_the_front_one_on_top() {
    let mut w = World::new(W, H);
    w.open(1, 200, 150, "back", 0x11);
    w.open(2, 200, 150, "front", 0x22);
    w.send(Event::new(EV_MOUSE_MOVE, W - 1, 0, 0));
    let c1 = win(&w, 1).client();
    let c2 = win(&w, 2).client();
    // A pixel in both client areas shows the front window's colour.
    let (x, y) = (c2.x + 10, c2.y + 10);
    assert!(c1.contains(x, y));
    let s = draw(&w, 0);
    assert_eq!(s.at(x, y), 0x22);
    // Raise the back window: now it wins.
    let a = win(&w, 1);
    w.click(a.x + 20, a.y + 10);
    w.send(Event::new(EV_MOUSE_MOVE, W - 1, 0, 0));
    let s = draw(&w, 0);
    assert_eq!(s.at(x, y), 0x11);
}

#[test]
fn a_window_partly_off_screen_draws_without_trouble() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "off", 0x77);
    let a = win(&w, 1);
    w.press(a.x + 30, a.y + 10);
    w.move_to(-5000, 400);
    w.release(-5000, 400);
    let m = win(&w, 1);
    assert!(m.x < 0);
    let s = draw(&w, 0);
    assert_eq!(s.at(0, m.y + BORDER + TITLE_H + 5), 0x77, "the visible part of the client area");
}

#[test]
fn a_long_title_stops_before_the_close_button() {
    let mut w = World::new(W, H);
    w.open(1, 150, 60, "a long title that cannot fit", 0);
    w.send(Event::new(EV_MOUSE_MOVE, W - 1, 0, 0));
    let a = win(&w, 1);
    let s = draw(&w, 0);
    let t = a.title_bar();
    let c = a.close_button();
    // Everything between the end of the text area and the close button is plain title bar.
    let room = (150 - 8 - CLOSE_SIZE - 6) / 16;
    let text_end = t.x + 8 + room * 16;
    for ty in t.y..t.y + t.h {
        for tx in text_end..c.x {
            assert_eq!(s.at(tx, ty), TITLE_FOCUS, "({tx},{ty})");
        }
    }
}

#[test]
fn the_taskbar_shows_a_button_per_window_in_creation_order() {
    let mut w = World::new(W, H);
    w.open(5, 100, 80, "five", 0);
    w.open(2, 100, 80, "two", 0);
    w.send(Event::new(EV_MOUSE_MOVE, W - 1, 0, 0));
    let s = draw(&w, 0);
    let b0 = w.desk.task_button(0);
    let b1 = w.desk.task_button(1);
    let b2 = w.desk.task_button(2);
    // Button 0 is window 2 (the lower id), button 1 is window 5; 5 was opened
    // first but the order is by id, and the focused one (2, opened last) is highlighted.
    assert_eq!(s.at(b0.x, b0.y), BTN_FOCUS);
    assert_eq!(s.at(b1.x, b1.y), BTN_BG);
    assert_eq!(s.at(b2.x, b2.y), TASKBAR_BG, "no third button");
}

#[test]
fn the_clock_shows_minutes_and_seconds() {
    let mut w = World::new(W, H);
    w.send(Event::new(EV_MOUSE_MOVE, 0, 0, 0));
    let a = draw(&w, 83); // 01:23
    // The same text drawn separately on a blank taskbar must match.
    let mut reference = Screen::new(W as usize, H as usize);
    reference.surface().fill_rect(0, 0, W, H, TASKBAR_BG);
    reference.surface().draw_text(w.desk.clock_x(), H - TASKBAR_H + 12, b"01:23", 2, TEXT);
    let top = H - TASKBAR_H + 12;
    for y in top..top + 16 {
        for x in w.desk.clock_x()..w.desk.clock_x() + 80 {
            assert_eq!(a.at(x, y), reference.at(x, y), "({x},{y})");
        }
    }
    // A different time looks different.
    let b = draw(&w, 3599); // 59:59
    assert_ne!(a.data, b.data);
    // The digits wrap rather than overflow (100 minutes shows 00).
    let c = draw(&w, 6000);
    let d = draw(&w, 0);
    assert_eq!(c.data, d.data);
}

#[test]
fn the_menu_is_drawn_above_the_start_button_with_the_hovered_entry_lit() {
    let mut w = World::new(W, H);
    let (sx, sy) = start_center(&w);
    w.click(sx, sy);
    let r1 = w.desk.menu_item_rect(1);
    w.send(Event::new(EV_MOUSE_MOVE, r1.x + 3, r1.y + 3, 0));
    let s = draw(&w, 0);
    let m = w.desk.menu_rect();
    assert_eq!(s.at(m.x, m.y), MENU_EDGE);
    assert_eq!(s.at(m.x + m.w - 1, m.y + m.h - 1), MENU_EDGE);
    let r0 = w.desk.menu_item_rect(0);
    assert_eq!(s.at(r0.x + r0.w - 1, r0.y + r0.h - 1), MENU_BG);
    assert_eq!(s.at(r1.x + r1.w - 1, r1.y + r1.h - 1), MENU_HOT, "hovered");
    assert_eq!(m.y + m.h, H - TASKBAR_H, "the menu sits right on the taskbar");
    // While the menu is open the Start button is lit.
    let st = w.desk.start_button();
    assert_eq!(s.at(st.x, st.y), START_HOT);
}

#[test]
fn the_pointer_is_drawn_last_on_top_of_everything() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0x55);
    let c = win(&w, 1).client();
    w.send(Event::new(EV_MOUSE_MOVE, c.x + 20, c.y + 20, 0));
    let s = draw(&w, 0);
    assert_eq!(s.at(c.x + 20, c.y + 20), gfx::CURSOR_OUTLINE, "the tip");
    assert_eq!(s.at(c.x + 21, c.y + 24), gfx::CURSOR_FILL);
    assert_eq!(s.at(c.x + 10, c.y + 10), 0x55);
}

#[test]
fn the_close_button_lights_up_under_the_pointer() {
    let mut w = World::new(W, H);
    w.open(1, 200, 100, "a", 0);
    let c = win(&w, 1).close_button();
    // Away from it, then on it (checking a corner the pointer does not cover).
    w.send(Event::new(EV_MOUSE_MOVE, c.x - 30, c.y + 5, 0));
    assert_eq!(draw(&w, 0).at(c.x + c.w - 1, c.y + c.h - 1), CLOSE_BG);
    w.send(Event::new(EV_MOUSE_MOVE, c.x + 2, c.y + 2, 0));
    assert_eq!(draw(&w, 0).at(c.x + c.w - 1, c.y + c.h - 1), CLOSE_HOT);
}
