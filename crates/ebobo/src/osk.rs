//! Built-in on-screen keyboard for the 256x240 UI canvas, driven by gamepad or touch: Steam's own
//! keyboard delivers no keys to ebobo in the Steam Deck's Game Mode.

use crate::canvas::{Canvas, Color, Font, H, W};
use ebobo_net::CODE_ALPHABET;

// Same values as ui.rs's palette.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    [r, g, b, 255]
}
const BG: Color = rgb(0x14, 0x13, 0x1A);
const PANEL: Color = rgb(0x22, 0x20, 0x2B);
const ACCENT: Color = rgb(0xE4, 0x3B, 0x2C);
const WHITE: Color = rgb(0xF4, 0xF2, 0xF7);
const SEC: Color = rgb(0x9C, 0x98, 0xA8);
const DIS: Color = rgb(0x5C, 0x59, 0x68);
const DIS_BAR: Color = rgb(0x3A, 0x37, 0x45);
const OK: Color = rgb(0x4C, 0xC6, 0x6A);
const WARN: Color = rgb(0xE8, 0xB8, 0x3A);

/// Top of the keyboard panel (canvas px): the field being edited must sit above it.
pub const TOP: i32 = 136;
/// Key height: 18 canvas px ≈ 60 px on the Deck's 1280x800 screen.
const KEY_H: i32 = 18;
const GAP: i32 = 2;
/// Pitch of one key column: a 19 px key plus the gap; 12 columns span 250 px.
const UNIT: i32 = 21;
const COLS: i32 = 12;
const ROWS: usize = 5;
const ROW0: i32 = TOP + 3;

/// Character rows, lowercase; shift gives `char::to_uppercase`.
const LATIN: [&str; 4] = ["1234567890-_", "qwertyuiop[]", "asdfghjkl:/", "zxcvbnm.,!?"];
const CYRILLIC: [&str; 4] = ["1234567890-_", "йцукенгшщзхъ", "фывапролджэ", "ячсмитьбюё.,"];

/// What the field accepts; keys that cannot produce an allowed character are dimmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
    /// `ebobo_net::CODE_ALPHABET`, always uppercase.
    RoomCode,
    /// Host or IP with port: letters, digits, `. : - [ ]`.
    Address,
    /// Letters, digits, `: / . - _`.
    Url,
    /// Any printable character; the only charset with the Latin/Cyrillic switch.
    Nickname,
}

impl Charset {
    fn allows(self, c: char) -> bool {
        match self {
            Charset::RoomCode => c.is_ascii() && CODE_ALPHABET.contains(&(c as u8)),
            Charset::Address => c.is_ascii_alphanumeric() || ".:-[]".contains(c),
            Charset::Url => c.is_ascii_alphanumeric() || ":/._-".contains(c),
            Charset::Nickname => !c.is_control(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OskNav {
    Up,
    Down,
    Left,
    Right,
    Press,
    Backspace,
    Shift,
    Layout,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OskOutput {
    Char(char),
    Backspace,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Latin,
    Cyrillic,
}

/// Off → one capital → caps lock → off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Off,
    Once,
    Lock,
}

/// A key's rectangle on the canvas; drawn and tapped area are the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl KeyRect {
    #[cfg(test)]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    fn center_x(&self) -> i32 {
        self.x + self.w / 2
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyKind {
    /// Lowercase form.
    Char(char),
    Shift,
    Space,
    Backspace,
    Layout,
    Done,
}

#[derive(Clone, Copy, Debug)]
struct Key {
    kind: KeyKind,
    row: usize,
    rect: KeyRect,
}

fn upper(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

fn build_keys(layout: Layout, layout_key: bool) -> Vec<Key> {
    let rows = match layout {
        Layout::Latin => LATIN,
        Layout::Cyrillic => CYRILLIC,
    };
    let mut keys = Vec::with_capacity(52);
    let rect = |x: i32, row: usize, units: i32| KeyRect { x, y: ROW0 + row as i32 * (KEY_H + GAP), w: units * UNIT - GAP, h: KEY_H };
    for (row, chars) in rows.iter().enumerate() {
        let n = chars.chars().count() as i32;
        let x0 = (W as i32 - (n * UNIT - GAP)) / 2;
        for (i, ch) in chars.chars().enumerate() {
            keys.push(Key { kind: KeyKind::Char(ch), row, rect: rect(x0 + i as i32 * UNIT, row, 1) });
        }
    }
    let bottom: &[(KeyKind, i32)] = if layout_key {
        &[(KeyKind::Shift, 2), (KeyKind::Layout, 2), (KeyKind::Space, 4), (KeyKind::Backspace, 2), (KeyKind::Done, 2)]
    } else {
        &[(KeyKind::Shift, 2), (KeyKind::Space, 6), (KeyKind::Backspace, 2), (KeyKind::Done, 2)]
    };
    let mut x = (W as i32 - (COLS * UNIT - GAP)) / 2;
    for &(kind, units) in bottom {
        keys.push(Key { kind, row: ROWS - 1, rect: rect(x, ROWS - 1, units) });
        x += units * UNIT;
    }
    keys
}

pub struct Osk {
    charset: Charset,
    layout: Layout,
    shift: Shift,
    keys: Vec<Key>,
    /// Gamepad cursor; always on an enabled key.
    cursor: usize,
    /// Key held under a finger or mouse button.
    pressed: Option<usize>,
}

impl Osk {
    pub fn new(charset: Charset) -> Osk {
        let mut osk = Osk {
            charset,
            layout: Layout::Latin,
            shift: Shift::Off,
            keys: build_keys(Layout::Latin, charset == Charset::Nickname),
            cursor: 0,
            pressed: None,
        };
        // Start on the letters, the most typed row.
        let x = osk.keys.iter().find(|k| k.row == 1).map_or(0, |k| k.rect.center_x());
        osk.cursor = osk.nearest_in_row(1, x).or_else(|| (0..osk.keys.len()).find(|&i| osk.enabled(i))).unwrap_or(0);
        osk
    }

    /// The key that types `c` in the current layout (tests).
    #[cfg(test)]
    pub fn key_of(&self, c: char) -> Option<usize> {
        self.keys.iter().position(|k| matches!(k.kind, KeyKind::Char(l) if l == c.to_lowercase().next().unwrap_or(c)))
    }

    #[cfg(test)]
    pub fn layout(&self) -> Layout {
        self.layout
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Gamepad input. Suggested mapping: d-pad moves (wrapping), A presses, B backspace,
    /// Y shift, X layout, Start OK.
    pub fn nav(&mut self, n: OskNav) -> Option<OskOutput> {
        match n {
            OskNav::Up => self.move_row(ROWS - 1),
            OskNav::Down => self.move_row(1),
            OskNav::Left => self.move_col(false),
            OskNav::Right => self.move_col(true),
            OskNav::Press => return self.press_key(self.cursor),
            OskNav::Backspace => return Some(OskOutput::Backspace),
            OskNav::Done => return Some(OskOutput::Done),
            OskNav::Shift => self.cycle_shift(),
            OskNav::Layout => self.switch_layout(),
        }
        None
    }

    /// The enabled key under canvas point (x, y); dimmed keys and the gaps take no taps.
    #[cfg(test)]
    pub fn key_at(&self, x: i32, y: i32) -> Option<usize> {
        self.key_rects().find(|(_, r)| r.contains(x, y)).map(|(i, _)| i)
    }

    /// Enabled keys and their rectangles, for tap regions.
    pub fn key_rects(&self) -> impl Iterator<Item = (usize, KeyRect)> + '_ {
        self.keys.iter().enumerate().filter(|(i, _)| self.enabled(*i)).map(|(i, k)| (i, k.rect))
    }

    /// Presses key `idx` (a tap's release) and moves the cursor there. Dimmed keys do nothing.
    pub fn press_key(&mut self, idx: usize) -> Option<OskOutput> {
        if !self.enabled(idx) {
            return None;
        }
        self.cursor = idx;
        match self.keys[idx].kind {
            KeyKind::Char(lower) => {
                let c = self.output(lower);
                if self.shift == Shift::Once && lower != upper(lower) {
                    self.shift = Shift::Off;
                }
                Some(OskOutput::Char(c))
            }
            KeyKind::Space => Some(OskOutput::Char(' ')),
            KeyKind::Backspace => Some(OskOutput::Backspace),
            KeyKind::Done => Some(OskOutput::Done),
            KeyKind::Shift => {
                self.cycle_shift();
                None
            }
            KeyKind::Layout => {
                self.switch_layout();
                None
            }
        }
    }

    /// The key held down under a finger (touch-down), drawn pressed until `None`.
    pub fn set_pressed(&mut self, idx: Option<usize>) {
        self.pressed = idx.filter(|&i| self.enabled(i));
    }

    fn output(&self, lower: char) -> char {
        if self.charset == Charset::RoomCode || self.shift != Shift::Off {
            upper(lower)
        } else {
            lower
        }
    }

    fn enabled(&self, idx: usize) -> bool {
        let Some(key) = self.keys.get(idx) else { return false };
        match key.kind {
            KeyKind::Char(lower) => self.charset.allows(self.output(lower)),
            KeyKind::Shift => self.charset != Charset::RoomCode,
            KeyKind::Space => self.charset.allows(' '),
            KeyKind::Backspace | KeyKind::Done | KeyKind::Layout => true,
        }
    }

    /// Enabled key of `row` whose centre is closest to `x` (the leftmost on a tie).
    fn nearest_in_row(&self, row: usize, x: i32) -> Option<usize> {
        (0..self.keys.len())
            .filter(|&i| self.keys[i].row == row && self.enabled(i))
            .min_by_key(|&i| (self.keys[i].rect.center_x() - x).abs())
    }

    /// Next row down by `step` (mod ROWS) that has an enabled key, nearest the cursor's column.
    fn move_row(&mut self, step: usize) {
        let cur = self.keys[self.cursor];
        for k in 1..=ROWS {
            let row = (cur.row + step * k) % ROWS;
            if let Some(i) = self.nearest_in_row(row, cur.rect.center_x()) {
                self.cursor = i;
                return;
            }
        }
    }

    /// Next enabled key in the cursor's row, wrapping at the ends.
    fn move_col(&mut self, right: bool) {
        let row = self.keys[self.cursor].row;
        let idx: Vec<usize> = (0..self.keys.len()).filter(|&i| self.keys[i].row == row).collect();
        let Some(pos) = idx.iter().position(|&i| i == self.cursor) else { return };
        let n = idx.len();
        for k in 1..n {
            let p = if right { (pos + k) % n } else { (pos + n - k) % n };
            if self.enabled(idx[p]) {
                self.cursor = idx[p];
                return;
            }
        }
    }

    fn cycle_shift(&mut self) {
        if self.charset == Charset::RoomCode {
            return;
        }
        self.shift = match self.shift {
            Shift::Off => Shift::Once,
            Shift::Once => Shift::Lock,
            Shift::Lock => Shift::Off,
        };
        self.fix_cursor();
    }

    fn switch_layout(&mut self) {
        if self.charset != Charset::Nickname {
            return;
        }
        let cur = self.keys[self.cursor];
        self.layout = match self.layout {
            Layout::Latin => Layout::Cyrillic,
            Layout::Cyrillic => Layout::Latin,
        };
        self.keys = build_keys(self.layout, true);
        self.pressed = None;
        self.cursor = self.nearest_in_row(cur.row, cur.rect.center_x()).unwrap_or(0);
        self.fix_cursor();
    }

    /// Keeps the cursor off dimmed keys after a state change.
    fn fix_cursor(&mut self) {
        if self.enabled(self.cursor) {
            return;
        }
        let cur = self.keys[self.cursor];
        let row = cur.row;
        self.cursor = self.nearest_in_row(row, cur.rect.center_x()).unwrap_or(self.cursor);
        if !self.enabled(self.cursor) {
            self.move_row(1);
        }
    }

    /// Draws the keyboard over the bottom of the canvas (y ≥ `TOP`).
    pub fn draw(&self, c: &mut Canvas, f: &mut Font) {
        c.fill_rect(0, TOP, W as i32, H as i32 - TOP, BG);
        c.fill_rect(0, TOP, W as i32, 1, DIS_BAR);
        for (i, key) in self.keys.iter().enumerate() {
            let r = key.rect;
            let on = self.enabled(i);
            let held = on && self.pressed == Some(i);
            let ink = if !on {
                c.rect(r.x, r.y, r.w, r.h, DIS_BAR);
                DIS
            } else if held {
                c.fill_rect(r.x, r.y, r.w, r.h, WHITE);
                BG
            } else if key.kind == KeyKind::Done {
                c.fill_rect(r.x, r.y, r.w, r.h, OK);
                BG
            } else {
                c.fill_rect(r.x, r.y, r.w, r.h, PANEL);
                WHITE
            };
            let cx = r.center_x();
            match key.kind {
                KeyKind::Char(lower) => label(c, f, r, &self.output(lower).to_string(), ink),
                KeyKind::Done => label(c, f, r, "OK", ink),
                KeyKind::Layout => {
                    let target = match self.layout {
                        Layout::Latin => "АБВ",
                        Layout::Cyrillic => "ABC",
                    };
                    label(c, f, r, target, if held { ink } else { SEC });
                }
                KeyKind::Shift => {
                    let col = match (held, self.shift) {
                        (true, _) => ink,
                        (false, Shift::Off) => if on { SEC } else { DIS },
                        (false, _) => WARN,
                    };
                    // Up arrow; caps lock adds a bar under it.
                    let top = r.y + 4;
                    for k in 0..4 {
                        c.fill_rect(cx - k, top + k, 2 * k + 1, 1, col);
                    }
                    c.fill_rect(cx - 1, top + 4, 3, 4, col);
                    if self.shift == Shift::Lock {
                        c.fill_rect(cx - 3, top + 10, 7, 1, col);
                    }
                }
                KeyKind::Backspace => {
                    // Left arrow, 12 px wide.
                    let (x0, cy) = (cx - 6, r.y + r.h / 2);
                    for k in 0..4 {
                        c.fill_rect(x0 + k, cy - k, 1, 2 * k + 1, ink);
                    }
                    c.fill_rect(x0 + 4, cy - 1, 8, 3, ink);
                }
                KeyKind::Space => {
                    let y = r.y + 11;
                    c.fill_rect(cx - 10, y, 21, 1, ink);
                    c.fill_rect(cx - 10, y - 3, 1, 3, ink);
                    c.fill_rect(cx + 10, y - 3, 1, 3, ink);
                }
            }
            if i == self.cursor {
                c.rect(r.x - 1, r.y - 1, r.w + 2, r.h + 2, ACCENT);
                c.rect(r.x, r.y, r.w, r.h, ACCENT);
            }
        }
    }
}

/// Text centred in a key (8 px glyph cells).
fn label(c: &mut Canvas, f: &mut Font, r: KeyRect, text: &str, color: Color) {
    let x = r.x + (r.w - f.width(text, 1) + 1) / 2;
    f.draw(c, x, r.y + (r.h - 8) / 2, text, color, 1);
}

#[cfg(test)]
#[path = "app/png.rs"]
mod png;

#[cfg(test)]
mod tests {
    use super::*;

    fn find(osk: &Osk, kind: KeyKind) -> usize {
        osk.keys.iter().position(|k| k.kind == kind).expect("key exists")
    }

    fn cursor_char(osk: &Osk) -> Option<char> {
        match osk.keys[osk.cursor].kind {
            KeyKind::Char(c) => Some(c),
            _ => None,
        }
    }

    #[test]
    fn osk_geometry_fits_bottom_of_canvas() {
        for cs in [Charset::RoomCode, Charset::Address, Charset::Url, Charset::Nickname] {
            let mut osk = Osk::new(cs);
            for _ in 0..2 {
                for k in &osk.keys {
                    assert!(k.rect.h >= 18 && k.rect.y >= TOP && k.rect.y + k.rect.h < H as i32);
                    assert!(k.rect.x >= 1 && k.rect.x + k.rect.w < W as i32);
                }
                osk.nav(OskNav::Layout);
            }
            assert!(H as i32 - TOP <= 110);
            assert_eq!(osk.keys.iter().any(|k| k.kind == KeyKind::Layout), cs == Charset::Nickname);
        }
    }

    #[test]
    fn osk_nav_wraps_and_skips_dimmed_keys() {
        let mut osk = Osk::new(Charset::RoomCode);
        // Digits row: 0, 1, '-', '_' are dimmed; Right from 9 wraps to 2.
        assert_eq!(osk.press_key(find(&osk, KeyKind::Char('9'))), Some(OskOutput::Char('9')));
        osk.nav(OskNav::Right);
        assert_eq!(cursor_char(&osk), Some('2'));
        osk.nav(OskNav::Left);
        assert_eq!(cursor_char(&osk), Some('9'));
        // Letters row: I and O are dimmed.
        osk.press_key(find(&osk, KeyKind::Char('u')));
        osk.nav(OskNav::Right);
        assert_eq!(cursor_char(&osk), Some('p'));
        osk.nav(OskNav::Right);
        assert_eq!(cursor_char(&osk), Some('q'), "wraps past the dimmed brackets");
        // Up from the top row wraps to the bottom row; Down comes back.
        osk.press_key(find(&osk, KeyKind::Char('2')));
        osk.nav(OskNav::Up);
        assert_eq!(osk.keys[osk.cursor].row, ROWS - 1);
        osk.nav(OskNav::Down);
        assert_eq!(osk.keys[osk.cursor].row, 0);
        // Shift is dimmed for codes: Left from Space-less bottom row never lands on it.
        osk.press_key(find(&osk, KeyKind::Backspace));
        for _ in 0..8 {
            osk.nav(OskNav::Left);
            assert_ne!(osk.keys[osk.cursor].kind, KeyKind::Shift);
        }
        // Wherever the d-pad goes, the cursor stays on enabled keys.
        let moves = [OskNav::Up, OskNav::Right, OskNav::Right, OskNav::Down, OskNav::Left, OskNav::Down, OskNav::Down];
        for _ in 0..20 {
            for m in moves {
                osk.nav(m);
                assert!(osk.enabled(osk.cursor));
            }
        }
    }

    #[test]
    fn osk_room_code_yields_only_code_chars() {
        let mut osk = Osk::new(Charset::RoomCode);
        osk.nav(OskNav::Shift);
        let mut typed = Vec::new();
        for i in 0..osk.keys.len() {
            if let Some(OskOutput::Char(c)) = osk.press_key(i) {
                typed.push(c as u8);
            }
        }
        assert!(typed.iter().all(|b| CODE_ALPHABET.contains(b)), "{:?}", String::from_utf8_lossy(&typed));
        typed.sort();
        let mut want = CODE_ALPHABET.to_vec();
        want.sort();
        assert_eq!(typed, want, "every code char is typeable exactly once");
    }

    #[test]
    fn osk_address_and_url_charsets() {
        let mut addr = Osk::new(Charset::Address);
        let mut url = Osk::new(Charset::Url);
        assert_eq!(addr.press_key(find(&addr, KeyKind::Char('['))), Some(OskOutput::Char('[')));
        assert_eq!(addr.press_key(find(&addr, KeyKind::Char('/'))), None);
        assert_eq!(url.press_key(find(&url, KeyKind::Char('/'))), Some(OskOutput::Char('/')));
        assert_eq!(url.press_key(find(&url, KeyKind::Char('['))), None);
        assert_eq!(url.press_key(find(&url, KeyKind::Space)), None);
        url.nav(OskNav::Layout);
        assert_eq!(url.layout(), Layout::Latin);
    }

    #[test]
    fn osk_cyrillic_types_ve() {
        let mut osk = Osk::new(Charset::Nickname);
        osk.nav(OskNav::Layout);
        assert_eq!(osk.layout(), Layout::Cyrillic);
        let ve = find(&osk, KeyKind::Char('в'));
        osk.nav(OskNav::Shift);
        assert_eq!(osk.press_key(ve), Some(OskOutput::Char('В')));
        // One-shot shift: the next letter is lowercase.
        assert_eq!(osk.nav(OskNav::Press), Some(OskOutput::Char('в')));
        assert_eq!(osk.press_key(find(&osk, KeyKind::Space)), Some(OskOutput::Char(' ')));
        // Caps lock stays on.
        osk.nav(OskNav::Shift);
        osk.nav(OskNav::Shift);
        assert_eq!(osk.press_key(ve), Some(OskOutput::Char('В')));
        assert_eq!(osk.press_key(ve), Some(OskOutput::Char('В')));
        // Tapping the layout key switches back.
        assert_eq!(osk.press_key(find(&osk, KeyKind::Layout)), None);
        assert_eq!(osk.layout(), Layout::Latin);
        assert_eq!(osk.keys[osk.cursor].kind, KeyKind::Layout);
    }

    #[test]
    fn osk_key_at_hits_keys_not_gaps() {
        let osk = Osk::new(Charset::Nickname);
        let q = find(&osk, KeyKind::Char('q'));
        let r = osk.keys[q].rect;
        assert_eq!(osk.key_at(r.x, r.y), Some(q));
        assert_eq!(osk.key_at(r.x + r.w - 1, r.y + r.h - 1), Some(q));
        assert_eq!(osk.key_at(r.x + r.w, r.y + 5), None, "gap to the right");
        assert_eq!(osk.key_at(r.x + 5, r.y + r.h), None, "gap below");
        assert_eq!(osk.key_at(r.x + 5, TOP - 4), None, "above the keyboard");
        assert_eq!(osk.key_at(0, 239), None, "left edge");
        assert_eq!(osk.key_at(-5, 150), None);
        // Dimmed keys take no taps.
        let code = Osk::new(Charset::RoomCode);
        let o = code.keys[find(&code, KeyKind::Char('o'))].rect;
        assert_eq!(code.key_at(o.x + 5, o.y + 5), None);
        assert_eq!(code.key_rects().count(), 32 + 2, "code chars + backspace + OK");
    }

    #[test]
    fn osk_done_and_backspace() {
        let mut osk = Osk::new(Charset::Url);
        assert_eq!(osk.nav(OskNav::Done), Some(OskOutput::Done));
        assert_eq!(osk.nav(OskNav::Backspace), Some(OskOutput::Backspace));
        assert_eq!(osk.press_key(find(&osk, KeyKind::Done)), Some(OskOutput::Done));
        assert_eq!(osk.press_key(find(&osk, KeyKind::Backspace)), Some(OskOutput::Backspace));
        assert_eq!(osk.nav(OskNav::Press), Some(OskOutput::Backspace), "cursor followed the tap");
        osk.set_pressed(Some(find(&osk, KeyKind::Space)));
        assert_eq!(osk.pressed, None, "dimmed key is never drawn held");
        osk.set_pressed(Some(find(&osk, KeyKind::Done)));
        assert!(osk.pressed.is_some());
    }

    /// Renders the keyboards to /tmp for a visual check:
    /// `cargo test -p ebobo osk_png -- --ignored`.
    #[test]
    #[ignore]
    fn osk_png() {
        let mut f = Font::new();
        let shots = [("latin", Charset::Nickname, false), ("cyr", Charset::Nickname, true), ("code", Charset::RoomCode, false)];
        for (name, cs, cyr) in shots {
            let mut osk = Osk::new(cs);
            if cyr {
                osk.nav(OskNav::Layout);
                osk.nav(OskNav::Shift);
            }
            let held = osk.key_rects().nth(5).map(|(i, _)| i);
            osk.set_pressed(held);
            let mut c = Canvas::new();
            c.clear(BG);
            c.fill_rect(8, 70, 240, 20, PANEL);
            c.rect(8, 70, 240, 20, ACCENT);
            f.draw(&mut c, 12, 76, if cyr { "Ваня_" } else { "ivan_" }, WHITE, 1);
            osk.draw(&mut c, &mut f);
            let k = 4;
            let (w, h) = (W * k, H * k);
            let mut big = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let s = ((y / k) * W + x / k) * 4;
                    big[(y * w + x) * 4..][..4].copy_from_slice(&c.px[s..s + 4]);
                }
            }
            std::fs::write(format!("/tmp/osk-{name}.png"), png::encode_rgba(w as u32, h as u32, &big)).unwrap();
        }
    }
}
