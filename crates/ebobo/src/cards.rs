//! The library screen: a grid of game cards with filter chips and search. Owner: cards slice.
//!
//! Self-contained: its own state, input handling (buttons, keys, pointer), layout and
//! drawing. `ui.rs` hands it the frame's input and the list of games and acts on what it
//! returns; the pictures come from `ebobo_art::Art`, favourites, history, hidden marks and
//! the user's own names from `ebobo_art::shelf::Shelf`.
//!
//! # How `ui.rs` uses it
//!
//! ```ignore
//! let games: Vec<cards::Game> = ...;            // one per library entry, borrowed from it
//! match cards.update(&input, &games, &mut shelf, now_unix, dt) {
//!     Some(Action::Start(i)) => start games[i] (and `shelf.played(games[i].key, now_unix)`),
//!     Some(Action::Rescan) => rescan the folders,
//!     Some(Action::Back) => leave the screen,
//!     Some(Action::EditSearch) => open the text field with `cards.search()`, later `cards.set_search(text)`,
//!     Some(Action::Menu(i)) => open the menu of games[i] (favourite, rename, hide, delete),
//!     None => {}
//! }
//! art.poll();
//! canvas.clear(BG);
//! cards.draw(&mut canvas, &mut font, &games, &shelf, &mut art, lang, title, scanning, now_unix);
//! // then the hint bar at y 228 (not drawn here) and anything modal
//! ```
//!
//! - `Game`: `name` is the name the card is sorted and searched by, `file` the file's name
//!   without its extension (searched too), `path` what `Art::want` gets, `note` the message of
//!   a game that cannot run (shown on the second detail line), `mtime` the file's date in Unix
//!   seconds (shown when another game has the same `name`).
//! - `Action::Start(i)` and `selected()` are indices into the `games` slice of that call.
//!   `Start` is only returned for a game with `runnable`.
//! - `update` and `draw` both notice by themselves that the games or the shelf changed (a
//!   cheap fingerprint each call) and rebuild the view then; nothing has to be told.
//! - The screen uses y 0..222: title y 4, chips y 16..28, grid y 30..190 (the first row's
//!   selection frame starts at y 30), detail lines y 196 and y 208. `draw` does not clear the
//!   canvas and does not call `Art::poll`.
//! - With no games at all it shows the "no games" line and the Rescan tile alone.
//! - Keys: `input.text` letters, digits and spaces go into the search (24 characters at
//!   most), `input.backspace` deletes one (with an empty search Backspace is `nav.back` as in
//!   every menu). A space is a search character only after the first one: on an empty search
//!   it is left to `nav.options`. Text input has to be on for typing to arrive
//!   (`Input::set_text_input(true)` while this screen shows).
//! - A game's menu (`Action::Menu`) opens three ways: `nav.options` on the selected game (pad
//!   Start, Space, the Menu key), a right click on its tile, or a press held on its tile for
//!   half a second without scrolling. The pointer ways select the tile first, and the release
//!   after a long press starts nothing. The favourite key is `nav.fav` (the pad's North).
//! - Chips: All, the two systems (when both have games), Recent (with history), the
//!   favourites star and, only while a game is hidden, a crossed-out eye for `Filter::Hidden`.
//!   Under that filter the title line says "Hidden" instead of the screen's title. A sorted
//!   and searched name is the user's own (`Shelf::name`) when there is one.
//! - Chips are hit-tested as last drawn, so the language never has to be passed to `update`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use ebobo_art::shelf::{self, Ago, Filter, Item, Shelf};
use ebobo_art::{Art, Key, System};

use crate::canvas::{Canvas, Color, Font, Placed, W};
use crate::i18n::{Lang, S};
use crate::input::{InputFrame, PointerEvent};

/// One game of the library, borrowed from the caller's list.
#[derive(Clone, Copy, Debug)]
pub struct Game<'a> {
    pub key: Key,
    /// The name the card is sorted and searched by.
    pub name: &'a str,
    /// The file's name without its extension.
    pub file: &'a str,
    pub path: &'a Path,
    /// 50 Hz.
    pub pal: bool,
    /// `false`: shown at half strength and cannot be started.
    pub runnable: bool,
    /// Why it cannot run, for the detail line.
    pub note: Option<&'a str>,
    /// The file's date, Unix seconds.
    pub mtime: u64,
}

/// What the screen asks its caller to do; `usize` is an index into the games slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start(usize),
    Rescan,
    Back,
    /// Open the text field with `Cards::search()`; give the result to `Cards::set_search`.
    EditSearch,
    /// Open the game's own menu (favourite, rename, hide, delete).
    Menu(usize),
}

/// The name a game goes by: the user's own (`Shelf::set_name`), else the proper name the art
/// database has for it, else `file_name` (the name its file gave it).
pub fn shown_name(key: Key, file_name: &str, shelf: &Shelf, art: &Art) -> String {
    match (shelf.name(key), art.name(key)) {
        (Some(own), _) => own.to_string(),
        (None, Some(proper)) => ebobo_art::names::display_name(proper),
        (None, None) => file_name.to_string(),
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    [r, g, b, 255]
}
// The interface's colours (`ui.rs`).
const BG: Color = rgb(0x14, 0x13, 0x1A);
const PANEL: Color = rgb(0x22, 0x20, 0x2B);
const ACCENT: Color = rgb(0xE4, 0x3B, 0x2C);
const WHITE: Color = rgb(0xF4, 0xF2, 0xF7);
const SEC: Color = rgb(0x9C, 0x98, 0xA8);
const DIS: Color = rgb(0x5C, 0x59, 0x68);
const WARN: Color = rgb(0xE8, 0xB8, 0x3A);

const COLS: usize = 5;
const TILE_W: i32 = 44;
const TILE_H: i32 = 62;
const GRID_X: i32 = 10;
const COL_STEP: i32 = 48;
const ROW_STEP: i32 = 70;
/// The grid viewport: tiles are cut outside `VIEW_TOP..VIEW_BOTTOM`.
const VIEW_TOP: i32 = 34;
const VIEW_BOTTOM: i32 = 190;
const VIEW_H: i32 = VIEW_BOTTOM - VIEW_TOP;
/// The selected tile is lifted 2 px and framed 2 px: it may draw from here.
const FRAME_TOP: i32 = VIEW_TOP - 4;
const TITLE_Y: i32 = 4;
const CHIP_Y: i32 = 16;
const CHIP_H: i32 = 12;
const NAME_Y: i32 = 196;
/// The second detail line; the caller's toasts lie on it.
pub(crate) const INFO_Y: i32 = 208;
/// A row change scrolls over this long (ease-out).
const SCROLL_TIME: f32 = 0.1;
/// A picture fades in over this many milliseconds.
const FADE_MS: u128 = 150;
/// A press that moves this far vertically becomes a scroll drag.
const DRAG_PX: i32 = 6;
/// A press held this long on a game's tile opens its menu.
const HOLD: Duration = Duration::from_millis(500);
const SEARCH_MAX: usize = 24;
/// Characters of the name line; a longer name scrolls.
const NAME_CHARS: usize = 30;
/// The name scrolls one character per this many milliseconds; it rests 5 steps (a second) at each end.
const NAME_STEP_MS: u128 = 200;
const NAME_REST: usize = 5;

/// 7x7 star, bit 6 = leftmost pixel.
pub(crate) const STAR: [u16; 7] = [0b0001000, 0b0001000, 0b1111111, 0b0111110, 0b0011100, 0b0110110, 0b0100010];
/// 9x7 crossed-out eye (the chip of the hidden games), bit 8 = leftmost pixel.
const HIDDEN: [u16; 7] = [0b000000010, 0b001111100, 0b010001010, 0b100010001, 0b010100010, 0b001111100, 0b010000000];
/// 11x11 refresh arrow, bit 10 = leftmost pixel.
const REFRESH: [u16; 11] = [
    0b00001111000,
    0b00110000101,
    0b01000000011,
    0b01000000111,
    0b10000000000,
    0b10000000000,
    0b10000000001,
    0b01000000010,
    0b01000000010,
    0b00110001100,
    0b00001110000,
];

/// A press on the grid that has not been released yet.
#[derive(Clone, Copy)]
struct Press {
    y: i32,
    /// Position in the grid of the tile pressed.
    tile: Option<usize>,
    /// The scroll target when it began.
    scroll: i32,
    drag: bool,
    /// How long it has been held still.
    held: Duration,
    /// It was held long enough to open the menu: its release is not a tap.
    menu: bool,
}

/// What the detail lines were formatted for: selected game, on the Rescan tile, view epoch,
/// language, the proper name is known, the hour (the "played" words change slowly).
type DetailKey = (Option<usize>, bool, u32, Lang, bool, u64);

/// The library screen's state.
pub struct Cards {
    filter: Filter,
    search: String,
    /// Indices into the games, in display order.
    view: Vec<usize>,
    /// The Rescan tile follows the games (unfiltered, unsearched list only).
    rescan: bool,
    /// Selected position: `0..view.len()` a game, `view.len()` the Rescan tile.
    sel: usize,
    /// The selected game, to find it again when the view changes.
    sel_key: Option<Key>,
    /// The view has to be rebuilt (filter or search changed).
    dirty: bool,
    /// The change was the user's filter or search: a selection that is gone goes to the first tile.
    to_first: bool,
    /// Fingerprint of the games and their shelf state the view was built from.
    fp: u64,
    /// Counts view rebuilds.
    epoch: u32,
    /// Which systems have games: NES, Mega Drive.
    systems: [bool; 2],
    /// The shelf has play history.
    history: bool,
    /// A game of the list is hidden: the Hidden chip exists.
    hidden: bool,
    /// Grid scroll in pixels: shown now, where the animation began, where it goes, its age.
    scroll: f32,
    scroll_from: f32,
    scroll_to: i32,
    scroll_t: Duration,
    press: Option<Press>,
    /// Time this screen has run, for fades.
    clock: Duration,
    /// Picture id → `clock` of the first frame it existed.
    fades: HashMap<u64, Duration>,
    /// Chips as last drawn: filter, x, width.
    chip_hit: [(Filter, i32, i32); 6],
    chip_n: usize,
    /// The search line and the language it was made in.
    search_line: String,
    search_lang: Option<Lang>,
    detail_key: Option<DetailKey>,
    name_line: String,
    name_len: usize,
    info_line: String,
    /// The info line is a "cannot run" note.
    info_warn: bool,
    /// How long the current name has been shown (it scrolls when long).
    name_t: Duration,
    /// Counts detail line formats.
    formats: u32,
}

impl Cards {
    pub fn new() -> Cards {
        Cards {
            filter: Filter::All,
            search: String::new(),
            view: Vec::new(),
            rescan: true,
            sel: 0,
            sel_key: None,
            dirty: true,
            to_first: false,
            fp: 0,
            epoch: 0,
            systems: [false; 2],
            history: false,
            scroll: 0.0,
            scroll_from: 0.0,
            scroll_to: 0,
            scroll_t: Duration::ZERO,
            hidden: false,
            press: None,
            clock: Duration::ZERO,
            fades: HashMap::new(),
            chip_hit: [(Filter::All, 0, 0); 6],
            chip_n: 0,
            search_line: String::new(),
            search_lang: None,
            detail_key: None,
            name_line: String::new(),
            name_len: 0,
            info_line: String::new(),
            info_warn: false,
            name_t: Duration::ZERO,
            formats: 0,
        }
    }

    /// The search text (empty = none).
    pub fn search(&self) -> &str {
        &self.search
    }

    /// Replaces the search text (the text field's result); 24 characters at most.
    pub fn set_search(&mut self, text: &str) {
        self.search.clear();
        self.search.extend(text.trim_start().chars().filter(|c| !c.is_control()).take(SEARCH_MAX));
        self.search_changed();
    }

    /// The selected game as an index into the games of the last `update` / `draw`; `None` on
    /// the Rescan tile or with nothing shown.
    #[cfg(test)]
    pub fn selected(&self) -> Option<usize> {
        self.view.get(self.sel).copied()
    }

    /// One frame of input. `now`: Unix seconds; `dt`: time since the last call.
    pub fn update(&mut self, input: &InputFrame, games: &[Game], shelf: &mut Shelf, now: u64, dt: Duration) -> Option<Action> {
        let _ = now;
        self.clock += dt;
        self.name_t += dt;
        self.sync(games, shelf);
        let mut action = None;

        // Search text. A space only continues a search; on an empty one it is the options key.
        let mut typed_space = false;
        for ch in input.text.chars() {
            let space = ch == ' ' && !self.search.is_empty();
            if (ch.is_alphanumeric() || space) && self.search.chars().count() < SEARCH_MAX {
                self.search.push(ch);
                self.search_changed();
                typed_space |= space;
            }
        }
        let nav = input.nav;
        if input.backspace && !self.search.is_empty() {
            self.search.pop();
            self.search_changed();
        } else if nav.back {
            if self.search.is_empty() {
                return Some(Action::Back);
            }
            self.search.clear();
            self.search_changed();
        }
        if nav.search {
            action = Some(Action::EditSearch);
        }

        // Chips.
        if nav.next_tab || nav.prev_tab {
            let (chips, n) = self.chips();
            let at = chips[..n].iter().position(|&f| f == self.filter).unwrap_or(0);
            let to = if nav.next_tab { (at + 1) % n } else { (at + n - 1) % n };
            self.set_filter(chips[to]);
        }
        self.sync(games, shelf);

        if nav.fav {
            if let Some(&i) = self.view.get(self.sel) {
                shelf.toggle_favourite(games[i].key);
                self.sync(games, shelf);
            }
        }
        if nav.options && !typed_space {
            if let Some(&i) = self.view.get(self.sel) {
                action = Some(Action::Menu(i));
            }
        }

        // The grid.
        let n = self.entries();
        if n > 0 {
            let before = self.sel;
            if nav.left && self.sel > 0 {
                self.sel -= 1;
            }
            if nav.right && self.sel + 1 < n {
                self.sel += 1;
            }
            if nav.up && self.sel >= COLS {
                self.sel -= COLS;
            }
            if nav.down && self.sel / COLS < (n - 1) / COLS {
                self.sel = (self.sel + COLS).min(n - 1);
            }
            if self.sel != before {
                self.ensure_visible();
            }
            if nav.confirm {
                action = self.activate(games).or(action);
            }
        }

        // Pointer.
        let pressed_before = self.press.is_some();
        for event in &input.pointer {
            match *event {
                PointerEvent::Down(Some((x, y))) => {
                    self.press = None;
                    if let Some(filter) = self.chip_at(x, y) {
                        self.set_filter(filter);
                        self.sync(games, shelf);
                    } else if (VIEW_TOP..VIEW_BOTTOM).contains(&y) {
                        let tile = self.tile_at(x, y);
                        if let Some(pos) = tile {
                            self.sel = pos;
                        }
                        self.press = Some(Press { y, tile, scroll: self.scroll_to, drag: false, held: Duration::ZERO, menu: false });
                    }
                }
                PointerEvent::Move(Some((_, y))) => {
                    let max = self.max_scroll();
                    if let Some(press) = &mut self.press {
                        press.drag |= !press.menu && (y - press.y).abs() >= DRAG_PX;
                        if press.drag {
                            // The grid follows the finger: no animation.
                            let to = (press.scroll - (y - press.y)).clamp(0, max);
                            self.scroll_to = to;
                            self.scroll = to as f32;
                        }
                    }
                }
                PointerEvent::Up(at) => {
                    if let Some(press) = self.press.take().filter(|p| !p.drag && !p.menu) {
                        let same = at.is_some_and(|(x, y)| press.tile.is_some() && self.tile_at(x, y) == press.tile);
                        if same {
                            action = self.activate(games).or(action);
                        }
                        if press.tile.is_some() {
                            self.ensure_visible();
                        }
                    }
                }
                PointerEvent::Wheel(notches) => {
                    let to = self.scroll_to.saturating_sub(notches.saturating_mul(ROW_STEP)).clamp(0, self.max_scroll());
                    self.scroll_target(to);
                }
                PointerEvent::Secondary(Some((x, y))) => {
                    if let Some((pos, &i)) = self.tile_at(x, y).and_then(|pos| Some((pos, self.view.get(pos)?))) {
                        self.press = None;
                        self.sel = pos;
                        self.ensure_visible();
                        action = Some(Action::Menu(i));
                    }
                }
                PointerEvent::Down(None) | PointerEvent::Move(None) | PointerEvent::Secondary(None) => {}
            }
        }
        // A press that stays down and still becomes the menu of its game. One that began this
        // frame starts counting with the next, so a long frame cannot turn a tap into a hold.
        if let Some(press) = self.press.as_mut().filter(|p| pressed_before && !p.drag && !p.menu) {
            press.held += dt;
            if press.held >= HOLD {
                if let Some((pos, &i)) = press.tile.and_then(|pos| Some((pos, self.view.get(pos)?))) {
                    press.menu = true;
                    self.sel = pos;
                    self.ensure_visible();
                    action = Some(Action::Menu(i));
                }
            }
        }
        self.sel_key = self.view.get(self.sel).map(|&i| games[i].key);

        // The scroll animation, ease-out.
        let to = self.scroll_to as f32;
        if self.scroll != to {
            self.scroll_t += dt;
            let p = (self.scroll_t.as_secs_f32() / SCROLL_TIME).min(1.0);
            let ease = 1.0 - (1.0 - p) * (1.0 - p);
            self.scroll = if p >= 1.0 { to } else { self.scroll_from + (to - self.scroll_from) * ease };
        }
        action
    }

    /// Draws the screen over a cleared canvas (y 0..222; the caller adds the hint bar).
    /// `title`: the screen's title, shown while there is no search text; `scanning`: the count
    /// turns WARN; `now`: Unix seconds.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        c: &mut Canvas,
        f: &mut Font,
        games: &[Game],
        shelf: &Shelf,
        art: &mut Art,
        lang: Lang,
        title: &str,
        scanning: bool,
        now: u64,
    ) {
        self.sync(games, shelf);

        // Title or search line, the count at the right.
        if self.search.is_empty() {
            // Under the Hidden filter the line names it: its chip is only an icon.
            let title = if self.filter == Filter::Hidden { lang.tr(S::FilterHidden) } else { title };
            f.draw(c, 8, TITLE_Y, title, WHITE, 1);
        } else {
            if self.search_lang != Some(lang) {
                self.search_lang = Some(lang);
                self.search_line.clear();
                let (head, tail) = lang.tr(S::SearchTitle).split_once("{text}").unwrap_or(("", ""));
                self.search_line.push_str(head);
                self.search_line.push_str(&self.search);
                self.search_line.push('_');
                self.search_line.push_str(tail);
            }
            f.draw(c, 8, TITLE_Y, &self.search_line, WHITE, 1);
        }
        let mut buf = [0u8; 20];
        let count = digits(self.view.len(), &mut buf);
        f.draw(c, W as i32 - 8 - f.width(count, 1), TITLE_Y, count, if scanning { WARN } else { SEC }, 1);

        // Chips.
        let (chips, n) = self.chips();
        let mut x = 8;
        for (slot, &filter) in chips[..n].iter().enumerate() {
            let label = match filter {
                Filter::All => lang.tr(S::FilterAll),
                Filter::System(System::Nes) => lang.tr(S::SystemNes),
                Filter::System(System::MegaDrive) => lang.tr(S::SystemMd),
                Filter::Recent => lang.tr(S::FilterRecent),
                Filter::Favourites | Filter::Hidden => "",
            };
            let sign: Option<(&[u16], i32)> = match filter {
                Filter::Favourites => Some((&STAR, 7)),
                Filter::Hidden => Some((&HIDDEN, 9)),
                _ => None,
            };
            let w = sign.map_or_else(|| f.width(label, 1), |(_, w)| w) + 8;
            let active = filter == self.filter;
            c.fill_rect(x, CHIP_Y, w, CHIP_H, if active { ACCENT } else { PANEL });
            let ink = if active { WHITE } else { SEC };
            if let Some((rows, iw)) = sign {
                icon(c, x + 4, CHIP_Y + 2, rows, iw, (0, CHIP_Y + CHIP_H), ink);
            } else {
                f.draw(c, x + 4, CHIP_Y + 2, label, ink, 1);
            }
            self.chip_hit[slot] = (filter, x, w);
            x += w + 4;
        }
        self.chip_n = n;

        // Tiles: the rows in view and one around (their pictures are asked for), the selected one last.
        let total = self.entries();
        let scroll = self.scroll.round() as i32;
        let first = ((scroll - ROW_STEP).max(0) / ROW_STEP) as usize * COLS;
        let last = (((scroll + VIEW_H + ROW_STEP) / ROW_STEP) as usize + 1) * COLS;
        for pos in first..last.min(total) {
            if pos != self.sel {
                self.tile(c, f, games, shelf, art, pos, scroll);
            }
        }
        if (first..last.min(total)).contains(&self.sel) {
            self.tile(c, f, games, shelf, art, self.sel, scroll);
        }

        // Empty states.
        if games.is_empty() {
            f.draw_centered(c, 124, lang.tr(S::NoRoms), SEC, 1);
        } else if total == 0 {
            let text = match self.filter {
                _ if !self.search.is_empty() => S::NothingFound,
                Filter::Favourites => S::NoFavourites,
                Filter::Recent => S::NoRecent,
                _ => S::NothingFound,
            };
            f.draw_centered(c, 104, lang.tr(text), SEC, 1);
        }

        // Detail lines.
        self.detail(games, shelf, art, lang, now);
        let fav = self.view.get(self.sel).is_some_and(|&i| shelf.is_favourite(games[i].key));
        f.draw(c, 8, NAME_Y, window(&self.name_line, self.name_offset(), NAME_CHARS), WHITE, 1);
        let room = if fav { NAME_CHARS - 2 } else { NAME_CHARS };
        f.draw(c, 8, INFO_Y, window(&self.info_line, 0, room), if self.info_warn { WARN } else { SEC }, 1);
        if fav {
            icon(c, 240, INFO_Y, &STAR, 7, (0, 240), WARN);
        }
    }

    /// Tiles shown: the view and the Rescan tile.
    fn entries(&self) -> usize {
        self.view.len() + usize::from(self.rescan)
    }

    /// The chips in order and how many.
    fn chips(&self) -> ([Filter; 6], usize) {
        let mut out = [Filter::All; 6];
        let mut n = 1;
        if self.systems[0] && self.systems[1] {
            out[n] = Filter::System(System::Nes);
            out[n + 1] = Filter::System(System::MegaDrive);
            n += 2;
        }
        if self.history {
            out[n] = Filter::Recent;
            n += 1;
        }
        out[n] = Filter::Favourites;
        n += 1;
        if self.hidden {
            out[n] = Filter::Hidden;
            n += 1;
        }
        (out, n)
    }

    fn set_filter(&mut self, filter: Filter) {
        if filter != self.filter {
            self.filter = filter;
            self.dirty = true;
            self.to_first = true;
        }
    }

    fn search_changed(&mut self) {
        self.dirty = true;
        self.to_first = true;
        self.search_lang = None;
    }

    /// Rebuilds the view when the games, the shelf, the filter or the search changed, keeping
    /// the selected game selected when it is still shown.
    fn sync(&mut self, games: &[Game], shelf: &Shelf) {
        let history = shelf.has_history();
        let fp = fingerprint(games, shelf);
        if !self.dirty && fp == self.fp && history == self.history {
            return;
        }
        // The first build starts on the first tile, not on Rescan, and so does a list that was
        // empty (the first scan still running: Rescan was the only tile to stand on).
        let was_rescan = self.epoch > 0 && self.rescan && !self.view.is_empty() && self.sel == self.view.len();
        // A system has its chip while it has games to show: a hidden game counts for none.
        self.systems = [false; 2];
        for game in games.iter().filter(|g| !shelf.is_hidden(g.key)) {
            self.systems[system_slot(game.key.system)] = true;
        }
        self.history = history;
        self.hidden = games.iter().any(|g| shelf.is_hidden(g.key));
        // A chip that is gone (one system left, history cleared) falls back to All.
        let (chips, n) = self.chips();
        if !chips[..n].contains(&self.filter) {
            self.filter = Filter::All;
        }
        let items: Vec<Item> = games.iter().map(|g| Item { key: g.key, name: shelf.name(g.key).unwrap_or(g.name), file: g.file }).collect();
        shelf.view(&items, self.filter, &self.search, &mut self.view);
        self.rescan = self.filter == Filter::All && self.search.is_empty();
        let total = self.entries();
        let found = self.sel_key.and_then(|key| self.view.iter().position(|&i| games[i].key == key));
        self.sel = match found {
            Some(pos) => pos,
            None if was_rescan && self.rescan && self.sel_key.is_none() && !self.to_first => self.view.len(),
            None if self.to_first || total == 0 => 0,
            // The selected game is gone (hidden, renamed out of a search, its file deleted):
            // the game that took its place or the last one, never the Rescan tile while there
            // is a game to stand on.
            None => self.sel.min(self.view.len().max(1) - 1),
        };
        self.sel_key = self.view.get(self.sel).map(|&i| games[i].key);
        self.dirty = false;
        self.to_first = false;
        self.fp = fp;
        self.epoch = self.epoch.wrapping_add(1);
        let max = self.max_scroll();
        if self.scroll_to > max {
            self.scroll_target(max);
        }
        self.ensure_visible();
    }

    /// The furthest scroll: the last row as the second row of the viewport.
    fn max_scroll(&self) -> i32 {
        let rows = self.entries().div_ceil(COLS) as i32;
        (rows - 2).max(0) * ROW_STEP
    }

    /// Starts the scroll animation toward `to` pixels.
    fn scroll_target(&mut self, to: i32) {
        if to != self.scroll_to {
            self.scroll_from = self.scroll;
            self.scroll_to = to;
            self.scroll_t = Duration::ZERO;
        }
    }

    /// Scrolls so that the selected tile's row is whole inside the viewport.
    fn ensure_visible(&mut self) {
        let top = (self.sel / COLS) as i32 * ROW_STEP;
        let to = if top < self.scroll_to {
            top
        } else if top + TILE_H > self.scroll_to + VIEW_H {
            top - ROW_STEP
        } else {
            self.scroll_to
        };
        self.scroll_target(to.clamp(0, self.max_scroll()));
    }

    /// Confirm on the selected tile.
    fn activate(&self, games: &[Game]) -> Option<Action> {
        match self.view.get(self.sel) {
            Some(&i) => games[i].runnable.then_some(Action::Start(i)),
            None if self.rescan && self.sel == self.view.len() => Some(Action::Rescan),
            None => None,
        }
    }

    /// The chip under a canvas point, as last drawn.
    fn chip_at(&self, x: i32, y: i32) -> Option<Filter> {
        if !(CHIP_Y - 1..CHIP_Y + CHIP_H + 1).contains(&y) {
            return None;
        }
        self.chip_hit[..self.chip_n].iter().find(|&&(_, cx, w)| (cx - 2..cx + w + 2).contains(&x)).map(|&(filter, _, _)| filter)
    }

    /// The grid position of the tile under a canvas point.
    fn tile_at(&self, x: i32, y: i32) -> Option<usize> {
        if !(VIEW_TOP..VIEW_BOTTOM).contains(&y) || x < GRID_X {
            return None;
        }
        let (col, in_col) = ((x - GRID_X) / COL_STEP, (x - GRID_X) % COL_STEP);
        let down = y - VIEW_TOP + self.scroll.round() as i32;
        let (row, in_row) = (down / ROW_STEP, down % ROW_STEP);
        if col >= COLS as i32 || in_col >= TILE_W || in_row >= TILE_H {
            return None;
        }
        let pos = row as usize * COLS + col as usize;
        (pos < self.entries()).then_some(pos)
    }

    /// Draws the tile at grid position `pos`, asks for its picture, and frames it when selected.
    #[allow(clippy::too_many_arguments)]
    fn tile(&mut self, c: &mut Canvas, f: &mut Font, games: &[Game], shelf: &Shelf, art: &mut Art, pos: usize, scroll: i32) {
        let selected = pos == self.sel;
        let x = GRID_X + (pos % COLS) as i32 * COL_STEP;
        let lift = if selected { 2 } else { 0 };
        let y = VIEW_TOP + (pos / COLS) as i32 * ROW_STEP - scroll - lift;
        let top = if selected { FRAME_TOP } else { VIEW_TOP };
        let span = (top, VIEW_BOTTOM);
        let visible = y < VIEW_BOTTOM && y + TILE_H > top;
        match self.view.get(pos) {
            None => {
                fill(c, x, y, TILE_W, TILE_H, span, PANEL);
                icon(c, x + (TILE_W - 11) / 2, y + (TILE_H - 11) / 2, &REFRESH, 11, span, if selected { WHITE } else { SEC });
            }
            Some(&i) => {
                let game = &games[i];
                art.want(game.key, game.path);
                let id = (game.key.system as u64) << 32 | u64::from(game.key.crc32);
                if art.get(game.key).is_some() {
                    let since = self.clock.saturating_sub(*self.fades.entry(id).or_insert(self.clock));
                    if visible {
                        let alpha = (since.as_millis() * 255 / FADE_MS).min(255) as u8;
                        let (y0, y1) = (y.max(top), (y + TILE_H).min(VIEW_BOTTOM));
                        c.picture(Placed {
                            id,
                            x,
                            y,
                            w: TILE_W,
                            h: TILE_H,
                            clip: (x, y0, TILE_W, y1 - y0),
                            back: PANEL,
                            alpha: if game.runnable { alpha } else { alpha / 2 },
                        });
                    }
                } else {
                    self.fades.remove(&id);
                    fill(c, x, y, TILE_W, TILE_H, span, PANEL);
                    let label = match game.key.system {
                        System::Nes => "NES",
                        System::MegaDrive => "SEGA",
                    };
                    let ty = y + (TILE_H - 8) / 2;
                    if ty >= top && ty + 8 <= VIEW_BOTTOM {
                        f.draw(c, x + (TILE_W - f.width(label, 1)) / 2, ty, label, DIS, 1);
                    }
                }
                if shelf.is_favourite(game.key) {
                    // A dark corner tag keeps the star readable on any art.
                    fill(c, x + TILE_W - 11, y, 11, 11, span, [BG[0], BG[1], BG[2], 230]);
                    icon(c, x + TILE_W - 9, y + 2, &STAR, 7, span, WARN);
                }
            }
        }
        if selected && visible {
            fill(c, x - 2, y - 2, TILE_W + 4, 2, span, ACCENT);
            fill(c, x - 2, y + TILE_H, TILE_W + 4, 2, span, ACCENT);
            fill(c, x - 2, y, 2, TILE_H, span, ACCENT);
            fill(c, x + TILE_W, y, 2, TILE_H, span, ACCENT);
        }
    }

    /// Formats the two detail lines when what they show changed.
    fn detail(&mut self, games: &[Game], shelf: &Shelf, art: &Art, lang: Lang, now: u64) {
        let pick = self.view.get(self.sel).copied();
        let on_rescan = pick.is_none() && self.rescan;
        let proper = pick.and_then(|i| art.name(games[i].key));
        let key: DetailKey = (pick, on_rescan, self.epoch, lang, proper.is_some(), now / 3600);
        if self.detail_key == Some(key) {
            return;
        }
        // The name restarts its scroll only when another tile is selected.
        if self.detail_key.is_none_or(|old| (old.0, old.1, old.4) != (key.0, key.1, key.4)) {
            self.name_t = Duration::ZERO;
        }
        self.detail_key = Some(key);
        self.formats += 1;
        self.name_line.clear();
        self.info_line.clear();
        self.info_warn = false;
        if let Some(i) = pick {
            let game = &games[i];
            self.name_line.push_str(&shown_name(game.key, game.name, shelf, art));
            if let Some(note) = game.note {
                self.info_line.push_str(note);
                self.info_warn = true;
            } else {
                let line = &mut self.info_line;
                line.push_str(lang.tr(match game.key.system {
                    System::Nes => S::SystemNes,
                    System::MegaDrive => S::SystemMd,
                }));
                if game.pal {
                    line.push_str("  PAL");
                }
                if let Some(then) = shelf.last_played(game.key) {
                    line.push_str("  ");
                    let (text, n) = match shelf::ago(now, then) {
                        Ago::Today => (S::PlayedToday, 0),
                        Ago::Yesterday => (S::PlayedYesterday, 0),
                        Ago::Days(n) => (S::PlayedDays, n),
                        Ago::Weeks(n) => (S::PlayedWeeks, n),
                        Ago::Months(n) => (S::PlayedMonths, n),
                        Ago::Years(n) => (S::PlayedYears, n),
                    };
                    match lang.tr(text).split_once("{n}") {
                        Some((head, tail)) => {
                            let _ = write!(line, "{head}{n}{tail}");
                        }
                        None => line.push_str(lang.tr(text)),
                    }
                }
                // Two builds of one game: the file's date tells them apart.
                if games.iter().enumerate().any(|(j, other)| j != i && same_name(other.name, game.name)) {
                    let (year, month, day) = date(game.mtime);
                    let _ = write!(line, "  {day:02}.{month:02}.{year}");
                }
            }
        } else if on_rescan {
            self.name_line.push_str(lang.tr(S::Rescan));
        }
        self.name_len = self.name_line.chars().count();
    }

    /// The first character of the name shown: a long name rests a second, scrolls to its end,
    /// rests, and starts over.
    fn name_offset(&self) -> usize {
        if self.name_len <= NAME_CHARS {
            return 0;
        }
        let max = self.name_len - NAME_CHARS;
        let step = (self.name_t.as_millis() / NAME_STEP_MS) as usize % (max + 2 * NAME_REST);
        step.saturating_sub(NAME_REST).min(max)
    }
}

fn system_slot(system: System) -> usize {
    match system {
        System::Nes => 0,
        System::MegaDrive => 1,
    }
}

/// A hash of everything the view depends on: the games in order (key, name, runnable) and
/// each one's favourite mark, hidden mark, last play and own name. No allocation.
fn fingerprint(games: &[Game], shelf: &Shelf) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    let mut mix = |value: u64| hash = (hash ^ value).wrapping_mul(0x0100_0000_01B3);
    for game in games {
        let key = game.key;
        let flags = u64::from(game.runnable) << 40
            | u64::from(shelf.is_favourite(key)) << 41
            | u64::from(shelf.is_hidden(key)) << 42
            | (system_slot(key.system) as u64) << 32;
        mix(flags | u64::from(key.crc32));
        mix(shelf.last_played(key).map_or(0, |t| t.wrapping_add(1)));
        for byte in game.name.bytes() {
            mix(u64::from(byte));
        }
        mix(0xFF);
        for byte in shelf.name(key).unwrap_or("").bytes() {
            mix(u64::from(byte));
        }
        mix(0xFE);
    }
    mix(games.len() as u64);
    hash
}

/// Filled rectangle cut to the rows `span.0..span.1`.
fn fill(c: &mut Canvas, x: i32, y: i32, w: i32, h: i32, span: (i32, i32), color: Color) {
    let (y0, y1) = (y.max(span.0), (y + h).min(span.1));
    if y1 > y0 {
        c.fill_rect(x, y0, w, y1 - y0, color);
    }
}

/// Whether two game names are the same whatever their letter case (no allocation).
fn same_name(a: &str, b: &str) -> bool {
    a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase))
}

/// A one-colour bitmap, `w` pixels wide (bit `w - 1` = leftmost), cut to the rows `span.0..span.1`.
pub(crate) fn icon(c: &mut Canvas, x: i32, y: i32, rows: &[u16], w: i32, span: (i32, i32), color: Color) {
    for (row, &bits) in rows.iter().enumerate() {
        let py = y + row as i32;
        if py < span.0 || py >= span.1 {
            continue;
        }
        for col in 0..w {
            if bits >> (w - 1 - col) & 1 != 0 {
                c.fill_rect(x + col, py, 1, 1, color);
            }
        }
    }
}

/// `n` in decimal without allocating.
fn digits(mut n: usize, buf: &mut [u8; 20]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("")
}

/// `len` characters of `text` from character `from`.
fn window(text: &str, from: usize, len: usize) -> &str {
    let at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    &text[at(from)..at(from + len)]
}

/// Year, month, day (UTC) of a Unix time.
fn date(unix: u64) -> (u64, u64, u64) {
    // Days since 1970-01-01 to a civil date (the usual era arithmetic, eras of 400 years).
    let days = unix / 86_400 + 719_468;
    let (era, day_of_era) = (days / 146_097, days % 146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let m = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * m + 2) / 5 + 1;
    let month = if m < 10 { m + 3 } else { m - 9 };
    (year_of_era + era * 400 + u64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::H;
    use ebobo_art::Picture;
    use ebobo_art::image::{decode_png, encode_png, fit};
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    struct Rom {
        key: Key,
        name: String,
        path: PathBuf,
        pal: bool,
        runnable: bool,
        note: Option<String>,
        mtime: u64,
    }

    fn rom(system: System, crc32: u32, name: &str) -> Rom {
        let path = PathBuf::from(format!("/nonexistent/{name}.rom"));
        Rom { key: Key { system, crc32 }, name: name.into(), path, pal: false, runnable: true, note: None, mtime: NOW - 400 * DAY }
    }

    /// `nes` NES games then `md` Mega Drive games, named so that the view's order is their order.
    fn library(nes: u32, md: u32) -> Vec<Rom> {
        (0..nes + md).map(|i| rom(if i < nes { System::Nes } else { System::MegaDrive }, 0x100 + i, &format!("Game {i:02}"))).collect()
    }

    fn games(roms: &[Rom]) -> Vec<Game<'_>> {
        roms.iter()
            .map(|r| Game {
                key: r.key,
                name: &r.name,
                file: &r.name,
                path: &r.path,
                pal: r.pal,
                runnable: r.runnable,
                note: r.note.as_deref(),
                mtime: r.mtime,
            })
            .collect()
    }

    /// One 16 ms frame with the input `set` makes.
    fn step(cards: &mut Cards, games: &[Game], shelf: &mut Shelf, set: impl FnOnce(&mut InputFrame)) -> Option<Action> {
        let mut input = InputFrame::default();
        set(&mut input);
        cards.update(&input, games, shelf, NOW, Duration::from_millis(16))
    }

    fn idle(cards: &mut Cards, games: &[Game], shelf: &mut Shelf, frames: usize) {
        for _ in 0..frames {
            assert_eq!(step(cards, games, shelf, |_| {}), None);
        }
    }

    fn tap(input: &mut InputFrame, x: i32, y: i32) {
        input.pointer.push(PointerEvent::Down(Some((x, y))));
        input.pointer.push(PointerEvent::Up(Some((x, y))));
    }

    /// The middle of the tile at grid position `pos` with no scroll.
    fn tile_mid(pos: usize) -> (i32, i32) {
        (GRID_X + (pos % COLS) as i32 * COL_STEP + TILE_W / 2, VIEW_TOP + (pos / COLS) as i32 * ROW_STEP + TILE_H / 2)
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-cards-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn grid_navigation_and_its_edges() {
        // 12 games and the Rescan tile: rows of 5, 5 and 3.
        let roms = library(12, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!((cards.entries(), cards.selected()), (13, Some(0)));
        step(&mut cards, &games, &mut shelf, |i| i.nav.left = true);
        assert_eq!(cards.sel, 0, "left at the very first tile stays");
        step(&mut cards, &games, &mut shelf, |i| i.nav.up = true);
        assert_eq!(cards.sel, 0, "up from the top row stays");
        for _ in 0..4 {
            step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        }
        assert_eq!(cards.sel, 4);
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        assert_eq!(cards.sel, 5, "right at the last column wraps to the next row's start");
        step(&mut cards, &games, &mut shelf, |i| i.nav.left = true);
        assert_eq!(cards.sel, 4, "left at the first column wraps to the previous row's end");
        step(&mut cards, &games, &mut shelf, |i| i.nav.down = true);
        assert_eq!(cards.sel, 9);
        step(&mut cards, &games, &mut shelf, |i| i.nav.down = true);
        assert_eq!(cards.sel, 12, "down above a shorter last row lands on its last tile");
        assert_eq!(cards.selected(), None, "that tile is Rescan");
        step(&mut cards, &games, &mut shelf, |i| i.nav.down = true);
        assert_eq!(cards.sel, 12, "down from the last row stays");
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        assert_eq!(cards.sel, 12, "right at the very last tile stays");
        step(&mut cards, &games, &mut shelf, |i| i.nav.up = true);
        assert_eq!(cards.sel, 7);
    }

    #[test]
    fn confirm_starts_runnable_games_and_rescans() {
        let mut roms = library(3, 0);
        roms[1].runnable = false;
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.nav.confirm = true), Some(Action::Start(0)));
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.nav.confirm = true), None, "not runnable");
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.nav.confirm = true), Some(Action::Start(2)));
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.nav.confirm = true), Some(Action::Rescan));
    }

    #[test]
    fn games_found_by_the_first_scan_take_the_selection_from_rescan() {
        let roms = library(3, 0);
        let all = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        // The scan has found nothing yet: Rescan is the only tile.
        idle(&mut cards, &[], &mut shelf, 2);
        assert_eq!((cards.entries(), cards.selected()), (1, None));
        idle(&mut cards, &all[..2], &mut shelf, 1);
        assert_eq!(cards.selected(), Some(0), "the first game, not Rescan");
        // Standing on Rescan by choice: more games arriving leave the selection there.
        step(&mut cards, &all[..2], &mut shelf, |i| i.nav.right = true);
        step(&mut cards, &all[..2], &mut shelf, |i| i.nav.right = true);
        assert_eq!(cards.selected(), None);
        idle(&mut cards, &all, &mut shelf, 1);
        assert_eq!((cards.entries(), cards.selected()), (4, None));
    }

    #[test]
    fn chips_cycle_with_one_system_and_no_history() {
        let roms = library(4, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        step(&mut cards, &games, &mut shelf, |i| i.nav.next_tab = true);
        assert_eq!(cards.filter, Filter::Favourites);
        step(&mut cards, &games, &mut shelf, |i| i.nav.next_tab = true);
        assert_eq!(cards.filter, Filter::All);
        step(&mut cards, &games, &mut shelf, |i| i.nav.prev_tab = true);
        assert_eq!(cards.filter, Filter::Favourites);
    }

    #[test]
    fn chips_cycle_with_two_systems_and_history() {
        let roms = library(3, 2);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        shelf.played(roms[1].key, NOW - DAY);
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        let mut seen = Vec::new();
        for _ in 0..5 {
            step(&mut cards, &games, &mut shelf, |i| i.nav.next_tab = true);
            seen.push(cards.filter);
        }
        let all = [Filter::System(System::Nes), Filter::System(System::MegaDrive), Filter::Recent, Filter::Favourites, Filter::All];
        assert_eq!(seen, all);
        step(&mut cards, &games, &mut shelf, |i| i.nav.prev_tab = true);
        assert_eq!(cards.filter, Filter::Favourites);
        // Recent shows the played game only; the Rescan tile is in the unfiltered list only.
        step(&mut cards, &games, &mut shelf, |i| i.nav.prev_tab = true);
        assert_eq!((cards.filter, cards.entries(), cards.selected()), (Filter::Recent, 1, Some(1)));
        // One system, no history: only All and Favourites.
        let one = library(3, 0);
        let games = self::games(&one);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.chips().1, 2);
    }

    #[test]
    fn favourite_toggles_the_selected_game() {
        let roms = library(4, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        step(&mut cards, &games, &mut shelf, |i| i.nav.fav = true);
        assert!(shelf.is_favourite(roms[1].key));
        assert_eq!(cards.selected(), Some(1));
        step(&mut cards, &games, &mut shelf, |i| i.nav.fav = true);
        assert!(!shelf.is_favourite(roms[1].key));
        // On the Rescan tile there is nothing to mark.
        for _ in 0..4 {
            step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        }
        assert_eq!(cards.selected(), None);
        step(&mut cards, &games, &mut shelf, |i| i.nav.fav = true);
        assert!(roms.iter().all(|r| !shelf.is_favourite(r.key)));
    }

    #[test]
    fn search_typing_clearing_and_back() {
        let roms = library(12, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        step(&mut cards, &games, &mut shelf, |i| i.text = " 1!".into());
        assert_eq!(cards.search(), "1", "a leading space and punctuation are not search text");
        assert_eq!(cards.view, [1, 10, 11]);
        assert_eq!(cards.entries(), 3, "no Rescan tile in a search");
        step(&mut cards, &games, &mut shelf, |i| i.text = "1".into());
        assert_eq!((cards.search(), cards.selected()), ("11", Some(11)));
        // Backspace arrives as both `backspace` and `nav.back`: it deletes one character.
        step(&mut cards, &games, &mut shelf, |i| {
            i.backspace = true;
            i.nav.back = true;
        });
        assert_eq!(cards.search(), "1");
        // A space inside a search is text, not the options key.
        let spaced = step(&mut cards, &games, &mut shelf, |i| {
            i.text = " ".into();
            i.nav.options = true;
        });
        assert_eq!((spaced, cards.search()), (None, "1 "));
        step(&mut cards, &games, &mut shelf, |i| i.text = "x".repeat(40));
        assert_eq!(cards.search().chars().count(), SEARCH_MAX);
        assert_eq!(cards.entries(), 0);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.nav.back = true), None, "back clears the search first");
        assert_eq!((cards.search(), cards.entries()), ("", 13));
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.nav.back = true), Some(Action::Back));
        // The text field route.
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.nav.search = true), Some(Action::EditSearch));
        cards.set_search("game 07");
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!((cards.search(), cards.selected(), cards.entries()), ("game 07", Some(7), 1));
    }

    #[test]
    fn selection_survives_a_filter_change() {
        let roms = library(6, 4);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        // Game 07 is a Mega Drive game and a favourite.
        for _ in 0..7 {
            step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        }
        step(&mut cards, &games, &mut shelf, |i| i.nav.fav = true);
        assert_eq!(cards.selected(), Some(7));
        step(&mut cards, &games, &mut shelf, |i| i.nav.prev_tab = true);
        assert_eq!((cards.filter, cards.selected(), cards.sel), (Filter::Favourites, Some(7), 0));
        step(&mut cards, &games, &mut shelf, |i| i.nav.next_tab = true);
        assert_eq!((cards.filter, cards.selected()), (Filter::All, Some(7)), "still shown: still selected");
        step(&mut cards, &games, &mut shelf, |i| i.nav.next_tab = true);
        assert_eq!((cards.filter, cards.selected()), (Filter::System(System::Nes), Some(0)), "gone: the first");
        step(&mut cards, &games, &mut shelf, |i| i.nav.next_tab = true);
        assert_eq!((cards.filter, cards.selected()), (Filter::System(System::MegaDrive), Some(6)));
    }

    #[test]
    fn view_and_detail_lines_are_rebuilt_only_on_change() {
        let roms = library(8, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut art = Art::new(folder("idle"), false);
        let (mut canvas, mut font) = (Canvas::new(), Font::new());
        let mut cards = Cards::new();
        for _ in 0..5 {
            idle(&mut cards, &games, &mut shelf, 1);
            cards.draw(&mut canvas, &mut font, &games, &shelf, &mut art, Lang::En, "Games", false, NOW);
        }
        assert_eq!((cards.epoch, cards.formats), (1, 1));
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        cards.draw(&mut canvas, &mut font, &games, &shelf, &mut art, Lang::En, "Games", false, NOW);
        assert_eq!((cards.epoch, cards.formats), (1, 2), "a move formats the lines, the view stays");
        assert_eq!(cards.name_line, "Game 01");
        // The shelf changed behind its back (a game was played): it notices.
        shelf.played(roms[3].key, NOW);
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!((cards.epoch, cards.chips().1), (2, 3));
        // A rescan found fewer games: the selection stays on its game.
        let fewer = self::games(&roms[1..]);
        idle(&mut cards, &fewer, &mut shelf, 1);
        assert_eq!((cards.epoch, cards.selected()), (3, Some(0)));
    }

    #[test]
    fn pointer_taps_tiles_and_chips() {
        let mut roms = library(6, 2);
        roms[2].runnable = false;
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut art = Art::new(folder("tap"), false);
        let (mut canvas, mut font) = (Canvas::new(), Font::new());
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        cards.draw(&mut canvas, &mut font, &games, &shelf, &mut art, Lang::Ru, "Игры", false, NOW);
        // Press selects, release on the same tile starts.
        let (x, y) = tile_mid(6);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Down(Some((x, y))))), None);
        assert_eq!(cards.selected(), Some(6));
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Up(Some((x, y))))), Some(Action::Start(6)));
        // Released over another tile: selected, not started.
        let (x2, y2) = tile_mid(1);
        let moved = step(&mut cards, &games, &mut shelf, |i| {
            i.pointer.push(PointerEvent::Down(Some((x2, y2))));
            i.pointer.push(PointerEvent::Up(Some((x, y))));
        });
        assert_eq!((moved, cards.selected()), (None, Some(1)));
        // A game that cannot run, the gap between tiles, the Rescan tile.
        let (x, y) = tile_mid(2);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| tap(i, x, y)), None);
        assert_eq!(cards.selected(), Some(2));
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| tap(i, GRID_X + TILE_W + 2, y)), None);
        assert_eq!(cards.selected(), Some(2));
        let (x, y) = tile_mid(8);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| tap(i, x, y)), Some(Action::Rescan));
        // Chips: Все, NES, Sega, star.
        assert_eq!(cards.chip_n, 4);
        let (_, cx, w) = cards.chip_hit[2];
        step(&mut cards, &games, &mut shelf, |i| tap(i, cx + w / 2, CHIP_Y + 6));
        assert_eq!((cards.filter, cards.view.as_slice()), (Filter::System(System::MegaDrive), &[6, 7][..]));
        let (_, cx, w) = cards.chip_hit[3];
        step(&mut cards, &games, &mut shelf, |i| tap(i, cx + w / 2, CHIP_Y + 6));
        assert_eq!((cards.filter, cards.entries()), (Filter::Favourites, 0));
    }

    #[test]
    fn drag_scrolls_and_cancels_the_tap() {
        let roms = library(24, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        let (x, y) = tile_mid(6);
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Down(Some((x, y)))));
        // 5 px is still a tap.
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Move(Some((x, y - 5)))));
        assert_eq!(cards.scroll_to, 0);
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Move(Some((x, y - 40)))));
        assert_eq!((cards.scroll_to, cards.scroll), (40, 40.0), "the grid follows the finger");
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Up(Some((x, y - 40))))), None);
        // Dragging back past the start stops at the top.
        step(&mut cards, &games, &mut shelf, |i| {
            i.pointer.push(PointerEvent::Down(Some((x, y))));
            i.pointer.push(PointerEvent::Move(Some((x, y + 100))));
            i.pointer.push(PointerEvent::Up(Some((x, y + 100))));
        });
        assert_eq!(cards.scroll_to, 0);
    }

    #[test]
    fn right_click_opens_the_menu_of_a_game_tile() {
        let roms = library(6, 2);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        let (x, y) = tile_mid(6);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Secondary(Some((x, y))))), Some(Action::Menu(6)));
        assert_eq!(cards.selected(), Some(6));
        // The Rescan tile, the gap between tiles, outside the canvas: nothing, the selection stays.
        let (rx, ry) = tile_mid(8);
        for at in [Some((rx, ry)), Some((GRID_X + TILE_W + 2, y)), Some((x, CHIP_Y + 4)), None] {
            assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Secondary(at))), None, "{at:?}");
            assert_eq!(cards.selected(), Some(6));
        }
    }

    #[test]
    fn long_press_opens_the_menu_once_and_its_release_starts_nothing() {
        let roms = library(24, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let (x, y) = tile_mid(6);
        let down = |cards: &mut Cards, shelf: &mut Shelf, x: i32, y: i32| {
            idle(cards, &games, shelf, 1);
            assert_eq!(step(cards, &games, shelf, |i| i.pointer.push(PointerEvent::Down(Some((x, y))))), None);
        };

        // Held still: 31 frames are 496 ms, the 32nd passes half a second.
        let mut cards = Cards::new();
        down(&mut cards, &mut shelf, x, y);
        idle(&mut cards, &games, &mut shelf, 31);
        assert_eq!(step(&mut cards, &games, &mut shelf, |_| {}), Some(Action::Menu(6)));
        assert_eq!(cards.selected(), Some(6));
        idle(&mut cards, &games, &mut shelf, 60);
        // The finger sliding afterwards does not scroll, lifting it does not start the game.
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Move(Some((x, y - 40))))), None);
        assert_eq!(cards.scroll_to, 0);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Up(Some((x, y))))), None);
        // The next tap is an ordinary one.
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| tap(i, x, y)), Some(Action::Start(6)));

        // A hold that became a drag scrolls and opens nothing.
        let mut cards = Cards::new();
        down(&mut cards, &mut shelf, x, y);
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Move(Some((x, y - 40)))));
        idle(&mut cards, &games, &mut shelf, 60);
        assert_eq!(cards.scroll_to, 40);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Up(Some((x, y - 40))))), None);

        // A short press is still a tap.
        let mut cards = Cards::new();
        down(&mut cards, &mut shelf, x, y);
        idle(&mut cards, &games, &mut shelf, 20);
        assert_eq!(step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Up(Some((x, y))))), Some(Action::Start(6)));

        // The Rescan tile has no menu: held or not, its release rescans.
        let few = &games[..3];
        let (rx, ry) = tile_mid(3);
        let mut cards = Cards::new();
        idle(&mut cards, few, &mut shelf, 1);
        assert_eq!(step(&mut cards, few, &mut shelf, |i| i.pointer.push(PointerEvent::Down(Some((rx, ry))))), None);
        idle(&mut cards, few, &mut shelf, 60);
        assert_eq!(step(&mut cards, few, &mut shelf, |i| i.pointer.push(PointerEvent::Up(Some((rx, ry))))), Some(Action::Rescan));
    }

    #[test]
    fn hidden_games_leave_the_list_and_get_their_own_chip() {
        let roms = library(3, 2);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        shelf.played(roms[0].key, NOW - DAY);
        let mut art = Art::new(folder("hidden"), false);
        let (mut canvas, mut font) = (Canvas::new(), Font::new());
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.chips().1, 5);
        shelf.set_hidden(roms[1].key, true);
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.view, [0, 2, 3, 4], "a hidden game is not in All");
        let (chips, n) = cards.chips();
        assert_eq!((n, chips[5]), (6, Filter::Hidden));
        // All six chips fit the line in both languages.
        for lang in [Lang::Ru, Lang::En] {
            cards.draw(&mut canvas, &mut font, &games, &shelf, &mut art, lang, "Games", false, NOW);
            let (filter, cx, w) = cards.chip_hit[5];
            assert_eq!((cards.chip_n, filter), (6, Filter::Hidden));
            assert!(cx + w <= 248, "{lang:?}: the chip row ends at {}", cx + w);
        }
        // Its chip shows the hidden game alone, without the Rescan tile.
        let (_, cx, w) = cards.chip_hit[5];
        step(&mut cards, &games, &mut shelf, |i| tap(i, cx + w / 2, CHIP_Y + 6));
        assert_eq!((cards.filter, cards.view.as_slice(), cards.entries()), (Filter::Hidden, &[1][..], 1));
        assert_eq!(cards.selected(), Some(1));
        // Put back: the chip goes and the filter with it.
        shelf.set_hidden(roms[1].key, false);
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!((cards.filter, cards.chips().1, cards.view.len()), (Filter::All, 5, 5));
    }

    #[test]
    fn hiding_the_selected_game_keeps_the_selection_on_a_game() {
        let roms = library(2, 1);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.chips().1, 4, "All, both systems, favourites");
        // The last game (the only Mega Drive one) goes: the one before it is selected, not the
        // Rescan tile that took its place, and a system with nothing to show has no chip.
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        assert_eq!(cards.selected(), Some(2));
        shelf.set_hidden(roms[2].key, true);
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.selected(), Some(1));
        let (chips, n) = cards.chips();
        assert_eq!(chips[..n], [Filter::All, Filter::Favourites, Filter::Hidden]);
        // A game in the middle goes: the next one takes its place under the selection.
        step(&mut cards, &games, &mut shelf, |i| i.nav.left = true);
        shelf.set_hidden(roms[0].key, true);
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.selected(), Some(1));
        // The last game of all: only the Rescan tile is left to stand on.
        shelf.set_hidden(roms[1].key, true);
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!((cards.selected(), cards.entries()), (None, 1));
    }

    #[test]
    fn a_renamed_game_is_sorted_and_found_by_its_new_name() {
        let roms = library(3, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.view, [0, 1, 2]);
        shelf.set_name(roms[0].key, "Zebra");
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.view, [1, 2, 0], "the new name sorts it last");
        cards.set_search("zeb");
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.view, [0]);
        // The file's name still finds it.
        cards.set_search("game 00");
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.view, [0]);
        // The name taken back: the old order.
        cards.set_search("");
        shelf.set_name(roms[0].key, "");
        idle(&mut cards, &games, &mut shelf, 1);
        assert_eq!(cards.view, [0, 1, 2]);
    }

    #[test]
    fn wheel_scrolls_a_row_per_notch() {
        // 24 games and Rescan: 5 rows, the furthest scroll shows rows 4 and 5.
        let roms = library(24, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Wheel(-1)));
        assert_eq!(cards.scroll_to, ROW_STEP);
        assert!(cards.scroll > 0.0 && cards.scroll < ROW_STEP as f32, "animated");
        idle(&mut cards, &games, &mut shelf, 8);
        assert_eq!(cards.scroll, ROW_STEP as f32);
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Wheel(-9)));
        assert_eq!(cards.scroll_to, 3 * ROW_STEP);
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Wheel(1)));
        assert_eq!(cards.scroll_to, 2 * ROW_STEP);
        step(&mut cards, &games, &mut shelf, |i| i.pointer.push(PointerEvent::Wheel(9)));
        assert_eq!(cards.scroll_to, 0);
    }

    #[test]
    fn selected_tile_stays_inside_the_viewport() {
        let roms = library(33, 0);
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        let mut cards = Cards::new();
        let inside = |cards: &Cards, scroll: i32| {
            let y = VIEW_TOP + (cards.sel / COLS) as i32 * ROW_STEP - scroll;
            assert!(y >= VIEW_TOP && y + TILE_H <= VIEW_BOTTOM, "row {} at y {y}", cards.sel / COLS);
        };
        for down in [true, false] {
            for _ in 0..10 {
                step(&mut cards, &games, &mut shelf, |i| (i.nav.down, i.nav.up) = (down, !down));
                inside(&cards, cards.scroll_to);
                idle(&mut cards, &games, &mut shelf, 8);
                inside(&cards, cards.scroll.round() as i32);
            }
            assert_eq!(cards.sel / COLS, if down { 6 } else { 0 });
        }
    }

    #[test]
    fn detail_lines_say_what_is_known() {
        let mut roms = library(2, 1);
        roms[0].pal = true;
        roms[1].note = Some("Mapper 99 is not supported".into());
        roms.push(rom(System::MegaDrive, 0x999, "Game 02"));
        roms[2].mtime = 1_700_000_000; // 2023-11-14; equal names sort by their index
        let games = games(&roms);
        let mut shelf = Shelf::in_memory();
        shelf.played(roms[0].key, NOW - 3 * DAY);
        let art = Art::new(folder("detail"), false);
        let mut cards = Cards::new();
        idle(&mut cards, &games, &mut shelf, 1);
        let mut lines = Vec::new();
        for _ in 0..5 {
            cards.detail(&games, &shelf, &art, Lang::En, NOW);
            lines.push((cards.name_line.clone(), cards.info_line.clone(), cards.info_warn));
            step(&mut cards, &games, &mut shelf, |i| i.nav.right = true);
        }
        let line = |name: &str, info: &str, warn: bool| (name.to_string(), info.to_string(), warn);
        assert_eq!(lines[0], line("Game 00", "NES  PAL  3 d ago", false));
        assert_eq!(lines[1], line("Game 01", "Mapper 99 is not supported", true));
        assert_eq!(lines[2], line("Game 02", "Genesis  14.11.2023", false));
        assert_eq!(lines[4], line("Rescan", "", false));
        for _ in 0..4 {
            step(&mut cards, &games, &mut shelf, |i| i.nav.left = true);
        }
        cards.detail(&games, &shelf, &art, Lang::Ru, NOW);
        assert_eq!((cards.name_line.as_str(), cards.info_line.as_str()), ("Game 00", "NES  PAL  3 дн. назад"));
        assert_eq!(date(0), (1970, 1, 1));
        assert_eq!(date(951_782_400), (2000, 2, 29));
        // A long name rests, scrolls to its end and rests again.
        cards.name_len = NAME_CHARS + 4;
        let at = |cards: &mut Cards, ms: u64| {
            cards.name_t = Duration::from_millis(ms);
            cards.name_offset()
        };
        assert_eq!([at(&mut cards, 900), at(&mut cards, 1200), at(&mut cards, 1800), at(&mut cards, 2700)], [0, 1, 4, 4]);
        assert_eq!(window("абвгд", 1, 3), "бвг");
        assert_eq!(window("ab", 1, 30), "b");
    }

    // ---------------------------------------------------------------------------------------
    // Pictures

    fn seed(dir: &Path, key: Key, picture: &Picture, kind: &str) {
        let folder = dir.join(match key.system {
            System::Nes => "nes",
            System::MegaDrive => "md",
        });
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(format!("{:08x}.png", key.crc32)), encode_png(picture)).unwrap();
        fs::write(folder.join(format!("{:08x}.kind", key.crc32)), kind).unwrap();
    }

    /// A stand-in picture: a gradient of the game's own colour with its first letter.
    fn generated(font: &mut Font, name: &str, w: u32, h: u32, seed: u32) -> Picture {
        let mut glyph = Canvas::new();
        glyph.clear_transparent();
        font.draw(&mut glyph, 0, 0, &name[..1], WHITE, 1);
        let tint = [(seed * 67 % 160) as u8 + 60, (seed * 131 % 160) as u8 + 40, (seed * 29 % 160) as u8 + 60];
        let cell = (w / 12) as i32;
        let (gx, gy) = ((w as i32 - 8 * cell) / 2, (h as i32 - 8 * cell) / 2);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let (u, v) = ((x - gx).div_euclid(cell), (y - gy).div_euclid(cell));
                let lit = (0..8).contains(&u) && (0..8).contains(&v) && glyph.px[(v as usize * W + u as usize) * 4 + 3] != 0;
                let shade = 255 - (y as u32 * 150 / h) as u16;
                let px = tint.map(|t| if lit { 245 } else { (u16::from(t) * shade / 255) as u8 });
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        Picture { w, h, rgba }
    }

    /// The canvas at 4x (nearest) with every placed picture under it, fitted in its box.
    fn snapshot(canvas: &Canvas, art: &Art, name: &str, lang: &str) {
        const K: i32 = 4;
        let (w, h) = (W as i32 * K, H as i32 * K);
        let mut under = vec![0u8; (w * h * 4) as usize];
        let mut put = |x: i32, y: i32, clip: (i32, i32, i32, i32), color: [u8; 4]| {
            let (cx, cy, cw, ch) = (clip.0 * K, clip.1 * K, clip.2 * K, clip.3 * K);
            if x < cx.max(0) || y < cy.max(0) || x >= (cx + cw).min(w) || y >= (cy + ch).min(h) {
                return;
            }
            let at = ((y * w + x) * 4) as usize;
            let a = u32::from(color[3]);
            for k in 0..3 {
                under[at + k] = ((u32::from(color[k]) * a + u32::from(under[at + k]) * (255 - a)) / 255) as u8;
            }
        };
        for placed in &canvas.placed {
            for y in placed.y * K..(placed.y + placed.h) * K {
                for x in placed.x * K..(placed.x + placed.w) * K {
                    put(x, y, placed.clip, placed.back);
                }
            }
            let system = if placed.id >> 32 == 0 { System::Nes } else { System::MegaDrive };
            let Some((picture, _)) = art.get(Key { system, crc32: placed.id as u32 }) else { continue };
            let small = fit(picture.clone(), (placed.w * K) as u32, (placed.h * K) as u32);
            let (ox, oy) = (placed.x * K + (placed.w * K - small.w as i32) / 2, placed.y * K + (placed.h * K - small.h as i32) / 2);
            for y in 0..small.h as i32 {
                for x in 0..small.w as i32 {
                    let at = ((y * small.w as i32 + x) * 4) as usize;
                    let alpha = (u32::from(small.rgba[at + 3]) * u32::from(placed.alpha) / 255) as u8;
                    put(ox + x, oy + y, placed.clip, [small.rgba[at], small.rgba[at + 1], small.rgba[at + 2], alpha]);
                }
            }
        }
        let mut rgba = Vec::with_capacity(under.len());
        for y in 0..h {
            for x in 0..w {
                let at = ((y / K) as usize * W + (x / K) as usize) * 4;
                let below = ((y * w + x) * 4) as usize;
                let a = u32::from(canvas.px[at + 3]);
                for k in 0..3 {
                    rgba.push(((u32::from(canvas.px[at + k]) * a + u32::from(under[below + k]) * (255 - a)) / 255) as u8);
                }
                rgba.push(255);
            }
        }
        let png = encode_png(&Picture { w: w as u32, h: h as u32, rgba });
        assert!(!png.is_empty());
        fs::write(format!("/tmp/ebobo-cards-{name}-{lang}.png"), png).unwrap();
    }

    /// Runs the screen for `frames` frames after `prepare` and writes its picture.
    #[allow(clippy::too_many_arguments)]
    fn shoot(
        name: &str,
        roms: &[Rom],
        shelf: &mut Shelf,
        art: &mut Art,
        font: &mut Font,
        frames: usize,
        prepare: impl Fn(&mut Cards, &[Game], &mut Shelf),
    ) {
        let games = games(roms);
        for (lang, code, title) in [(Lang::Ru, "ru", "Одиночная игра"), (Lang::En, "en", "Solo game")] {
            let mut cards = Cards::new();
            let mut canvas = Canvas::new();
            idle(&mut cards, &games, shelf, 1);
            prepare(&mut cards, &games, shelf);
            for _ in 0..frames {
                step(&mut cards, &games, shelf, |_| {});
                art.poll();
                canvas.clear(BG);
                cards.draw(&mut canvas, font, &games, shelf, art, lang, title, false, NOW);
            }
            snapshot(&canvas, art, name, code);
        }
    }

    /// Writes `/tmp/ebobo-cards-<name>-<lang>.png` (1024x960) for the states of the screen.
    #[test]
    #[ignore]
    fn cards_render_snapshots() {
        use System::{MegaDrive as Md, Nes};
        // File in /tmp/art-probe (real box art when there), name, system, kind.
        let list = [
            ("Battletoads_USA_", "Battletoads", Nes, "none"),
            ("Castlevania_USA_", "Castlevania", Nes, "title"),
            ("Chip_n_Dale_Rescue_Rangers_Europe_", "Chip 'n Dale Rescue Rangers", Nes, "box"),
            ("Contra_USA_", "Contra", Nes, "box"),
            ("Darkwing_Duck_USA_", "Darkwing Duck", Nes, "box"),
            ("Double_Dragon_II_The_Revenge_USA_", "Double Dragon II - The Revenge", Nes, "box"),
            ("Duck_Tales_USA_", "DuckTales", Nes, "box"),
            ("Mega_Man_2_USA_", "Mega Man 2", Nes, "box"),
            ("Mortal_Kombat_3_USA_", "Mortal Kombat 3", Md, "box"),
            ("Ninja_Gaiden_USA_", "Ninja Gaiden", Nes, "box"),
            ("Sonic_The_Hedgehog_USA_Europe_", "Sonic The Hedgehog", Md, "box"),
            ("Streets_of_Rage_2_USA_", "Streets of Rage 2", Md, "box"),
            ("Teenage_Mutant_Ninja_Turtles_III_The_Manhattan_Project_USA_", "Teenage Mutant Ninja Turtles III - The Manhattan Project", Nes, "box"),
            ("Tiny_Toon_Adventures_USA_", "Tiny Toon Adventures", Nes, "box"),
        ];
        let dir = folder("art");
        let mut font = Font::new();
        let mut roms = Vec::new();
        let mut seeded = Vec::new();
        for (n, &(file, name, system, kind)) in list.iter().enumerate() {
            let entry = rom(system, 0xA000 + n as u32, name);
            let real = fs::read(format!("/tmp/art-probe/{file}.png")).ok().and_then(|bytes| decode_png(&bytes).ok());
            let picture = match (kind, real) {
                ("none", _) => None,
                ("box", Some(real)) => Some(fit(real, ebobo_art::MAX_SIDE, ebobo_art::MAX_SIDE)),
                ("box", None) => Some(generated(&mut font, name, 228, 320, n as u32 + 1)),
                _ => Some(generated(&mut font, name, 256, 240, n as u32 + 1)),
            };
            if let Some(picture) = picture {
                seed(&dir, entry.key, &picture, kind);
                seeded.push(entry.key);
            }
            roms.push(entry);
        }
        roms[2].pal = true;
        roms[13].runnable = false;
        roms[13].note = Some("Mapper 99 is not supported".into());

        let mut art = Art::new(dir.clone(), false);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            for (entry, _) in roms.iter().zip(&list) {
                art.want(entry.key, &entry.path);
            }
            art.poll();
            if seeded.iter().all(|&key| art.get(key).is_some()) {
                break;
            }
            assert!(Instant::now() < deadline, "the seeded pictures did not load");
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut shelf = Shelf::in_memory();
        shelf.toggle_favourite(roms[3].key);
        shelf.toggle_favourite(roms[10].key);
        shelf.played(roms[0].key, NOW - 3 * DAY);
        shelf.played(roms[10].key, NOW - 3600);
        shelf.played(roms[12].key, NOW - 40 * DAY);
        let down = |cards: &mut Cards, games: &[Game], shelf: &mut Shelf| {
            step(cards, games, shelf, |i| i.nav.down = true);
        };

        shoot("library", &roms, &mut shelf, &mut art, &mut font, 30, |_, _, _| {});
        shoot("scrolled", &roms, &mut shelf, &mut art, &mut font, 30, |cards, games, shelf| {
            down(cards, games, shelf);
            down(cards, games, shelf);
            step(cards, games, shelf, |i| i.nav.right = true);
        });
        shoot("search", &roms, &mut shelf, &mut art, &mut font, 30, |cards, games, shelf| {
            step(cards, games, shelf, |i| i.text = "du".into());
        });
        shoot("favourites-empty", &roms, &mut Shelf::in_memory(), &mut art, &mut font, 30, |cards, games, shelf| {
            step(cards, games, shelf, |i| i.nav.prev_tab = true);
        });
        shoot("small", &roms[2..5], &mut shelf, &mut art, &mut font, 30, |cards, games, shelf| {
            step(cards, games, shelf, |i| i.nav.right = true);
        });
        // 2.9 s in: the long name has rested a second and scrolled.
        shoot("long-name", &roms, &mut shelf, &mut art, &mut font, 180, |cards, games, shelf| {
            down(cards, games, shelf);
            down(cards, games, shelf);
            for _ in 0..2 {
                step(cards, games, shelf, |i| i.nav.right = true);
            }
        });
        // One game hidden: the crossed-out eye closes the chip row; then its own list.
        shelf.set_hidden(roms[4].key, true);
        shoot("hidden-chip", &roms, &mut shelf, &mut art, &mut font, 30, |_, _, _| {});
        shoot("hidden-filter", &roms, &mut shelf, &mut art, &mut font, 30, |cards, games, shelf| {
            step(cards, games, shelf, |i| i.nav.prev_tab = true);
        });
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn same_name_ignores_letter_case() {
        assert!(same_name("Sonic the Hedgehog", "SONIC THE HEDGEHOG"));
        assert!(same_name("Чип и Дейл", "ЧИП И ДЕЙЛ"));
        assert!(!same_name("Sonic", "Sonic 2"));
    }
}
