//! The library's shelf: which games are favourites, when each was last played, and the list
//! a filter and a search text leave, in the order the cards are shown. Owner: shelf slice.
//!
//! Pure logic plus one small text file. Nothing here knows about pictures or the interface.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{Key, System, slot, system_name};

/// The first line of the shelf file.
const HEADER: &str = "ebobo shelf 1";

/// Which cards the library shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    System(System),
    /// Games played at least once, the latest first.
    Recent,
    Favourites,
}

/// One game of the library as the shelf sees it.
#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    pub key: Key,
    /// The name the card shows.
    pub name: &'a str,
    /// The file's own name without its extension; the search matches it too.
    pub file: &'a str,
}

/// What the shelf knows about one game. The default (nothing known) is never stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Entry {
    favourite: bool,
    /// Unix seconds of the last start; meaningful only when `plays` is not zero.
    last_played: u64,
    /// How many times the game was started.
    plays: u32,
}

/// Favourites and play history, kept in a text file.
#[derive(Clone, Debug, Default)]
pub struct Shelf {
    /// Never iterated for anything whose order shows: the file is written sorted.
    entries: HashMap<Key, Entry>,
    /// Where changes are written; `None` = nowhere.
    path: Option<PathBuf>,
}

impl Shelf {
    /// An empty shelf that is never written to disk (tests, and a data folder that cannot be
    /// used).
    pub fn in_memory() -> Shelf {
        Shelf::default()
    }

    /// Reads the shelf file at `path` (a missing or damaged file gives an empty shelf; lines
    /// it cannot read are skipped). Later changes are written back there.
    pub fn load(path: PathBuf) -> Shelf {
        let mut entries = HashMap::new();
        if let Ok(text) = fs::read_to_string(&path) {
            let mut lines = text.lines();
            if lines.next() == Some(HEADER) {
                for (key, entry) in lines.filter_map(parse_line) {
                    if entry != Entry::default() {
                        entries.insert(key, entry);
                    }
                }
            }
        }
        Shelf { entries, path: Some(path) }
    }

    pub fn is_favourite(&self, key: Key) -> bool {
        self.entries.get(&key).is_some_and(|e| e.favourite)
    }

    /// Flips the favourite mark, saves, and returns the new state.
    pub fn toggle_favourite(&mut self, key: Key) -> bool {
        let entry = self.entries.entry(key).or_default();
        entry.favourite = !entry.favourite;
        let now = entry.favourite;
        if *entry == Entry::default() {
            self.entries.remove(&key);
        }
        self.save();
        now
    }

    /// Notes that the game was started at `now` (Unix seconds) and saves.
    pub fn played(&mut self, key: Key, now: u64) {
        let entry = self.entries.entry(key).or_default();
        entry.last_played = now;
        entry.plays = entry.plays.saturating_add(1);
        self.save();
    }

    /// When the game was last started (Unix seconds).
    pub fn last_played(&self, key: Key) -> Option<u64> {
        self.entries.get(&key).filter(|e| e.plays > 0).map(|e| e.last_played)
    }

    /// Any game was ever played (the "recent" filter has something to show).
    pub fn has_history(&self) -> bool {
        self.entries.values().any(|e| e.plays > 0)
    }

    /// Fills `out` with the indices into `items` of the games that pass `filter` and `search`,
    /// in display order: by name without regard to case (ties by index), except `Recent`,
    /// which is latest first. `search` is matched without regard to case as a substring of
    /// the name or the file name; every word of it must match somewhere; empty = everything.
    pub fn view(&self, items: &[Item], filter: Filter, search: &str, out: &mut Vec<usize>) {
        out.clear();
        let search = search.to_lowercase();
        let words: Vec<&str> = search.split_whitespace().collect();
        let recent = filter == Filter::Recent;
        // The sort keys of the kept items: the lowercased name, or the time of the last start.
        let mut by_name: Vec<(String, usize)> = Vec::new();
        let mut by_time: Vec<(u64, usize)> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let passes = match filter {
                Filter::All => true,
                Filter::System(system) => item.key.system == system,
                Filter::Recent => self.last_played(item.key).is_some(),
                Filter::Favourites => self.is_favourite(item.key),
            };
            if !passes {
                continue;
            }
            let name = if recent && words.is_empty() { String::new() } else { item.name.to_lowercase() };
            if !words.is_empty() {
                let file = item.file.to_lowercase();
                if !words.iter().all(|word| name.contains(word) || file.contains(word)) {
                    continue;
                }
            }
            if recent {
                by_time.push((self.last_played(item.key).unwrap_or(0), index));
            } else {
                by_name.push((name, index));
            }
        }
        if recent {
            by_time.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            out.extend(by_time.iter().map(|&(_, index)| index));
        } else {
            by_name.sort_unstable();
            out.extend(by_name.iter().map(|(_, index)| *index));
        }
    }

    /// Writes the shelf to its file, if it has one: a temporary file in the same folder, then
    /// a rename, so a crash leaves the old file or the new one. A failure is reported once on
    /// standard error and otherwise ignored (the shelf stays right in memory).
    fn save(&self) {
        let Some(path) = &self.path else { return };
        let mut rows: Vec<(Key, Entry)> = self.entries.iter().filter(|(_, e)| **e != Entry::default()).map(|(k, e)| (*k, *e)).collect();
        rows.sort_unstable_by_key(|(key, _)| (slot(key.system), key.crc32));
        let mut text = String::with_capacity(HEADER.len() + 1 + rows.len() * 32);
        text.push_str(HEADER);
        text.push('\n');
        for (key, entry) in rows {
            let system = system_name(key.system);
            let favourite = u8::from(entry.favourite);
            text.push_str(&format!("{system}\t{:08x}\t{favourite}\t{}\t{}\n", key.crc32, entry.last_played, entry.plays));
        }
        if let Err(error) = write_atomically(path, &text) {
            eprintln!("ebobo: cannot save the shelf to {}: {error}", path.display());
        }
    }
}

/// Writes `text` to `path` through a temporary file next to it, creating the folder.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    if let Some(folder) = path.parent().filter(|folder| !folder.as_os_str().is_empty()) {
        fs::create_dir_all(folder)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let temporary = path.with_file_name(name);
    let result = fs::write(&temporary, text).and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// One line of the shelf file after the header, or `None` if it is not one.
fn parse_line(line: &str) -> Option<(Key, Entry)> {
    let mut fields = line.split('\t');
    let system = match fields.next()? {
        "nes" => System::Nes,
        "md" => System::MegaDrive,
        _ => return None,
    };
    let crc = fields.next()?;
    if crc.len() != 8 || !crc.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let crc32 = u32::from_str_radix(crc, 16).ok()?;
    let favourite = match fields.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let last_played = fields.next()?.parse().ok()?;
    let plays = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((Key { system, crc32 }, Entry { favourite, last_played, plays }))
}

/// The systems that have games in `items`, in a fixed order (NES first).
pub fn systems(items: &[Item]) -> Vec<System> {
    let mut present = [false; 2];
    for item in items {
        present[slot(item.key.system)] = true;
    }
    [System::Nes, System::MegaDrive].into_iter().filter(|&system| present[slot(system)]).collect()
}

/// How long ago `then` was, seen from `now` (Unix seconds), in whole units for a card's
/// detail line: `Today`, `Yesterday`, then days up to 13, weeks up to 8, months up to 23,
/// then years. Local midnight is not known here: "today" is the last 20 hours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ago {
    Today,
    Yesterday,
    Days(u32),
    Weeks(u32),
    Months(u32),
    Years(u32),
}

pub fn ago(now: u64, then: u64) -> Ago {
    const HOUR: u64 = 3600;
    let seconds = now.saturating_sub(then);
    if seconds < 20 * HOUR {
        return Ago::Today;
    }
    if seconds < 44 * HOUR {
        return Ago::Yesterday;
    }
    let days = seconds / (24 * HOUR);
    let whole = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
    // 44 to 48 hours is one whole day, which "yesterday" already said: call it two.
    match days {
        0..=13 => Ago::Days(whole(days.max(2))),
        14..=62 => Ago::Weeks(whole(days / 7)),
        63..=719 => Ago::Months(whole(days / 30)),
        _ => Ago::Years(whole(days / 365)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nes(crc32: u32) -> Key {
        Key { system: System::Nes, crc32 }
    }

    fn md(crc32: u32) -> Key {
        Key { system: System::MegaDrive, crc32 }
    }

    /// An empty folder of this test's own.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-shelf-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn shown(shelf: &Shelf, items: &[Item], filter: Filter, search: &str) -> Vec<usize> {
        let mut out = vec![99];
        shelf.view(items, filter, search, &mut out);
        out
    }

    const ITEMS: [Item; 6] = [
        Item { key: Key { system: System::Nes, crc32: 1 }, name: "zelda II", file: "Zelda 2 (U)" },
        Item { key: Key { system: System::MegaDrive, crc32: 2 }, name: "Sonic the Hedgehog", file: "sonic1" },
        Item { key: Key { system: System::Nes, crc32: 3 }, name: "Contra", file: "probotector (E)" },
        Item { key: Key { system: System::MegaDrive, crc32: 4 }, name: "contra", file: "Contra - Hard Corps" },
        Item { key: Key { system: System::Nes, crc32: 5 }, name: "Ёжик Sonic", file: "hack" },
        Item { key: Key { system: System::Nes, crc32: 6 }, name: "CONTRA", file: "contra force" },
    ];

    #[test]
    fn changes_survive_a_round_trip() {
        let dir = folder("round");
        let path = dir.join("deep").join("shelf.txt");
        let mut shelf = Shelf::load(path.clone());
        assert!(!shelf.has_history());
        assert!(shelf.toggle_favourite(nes(0xDEAD_BEEF)));
        shelf.played(md(0x12), 1_700_000_000);
        shelf.played(md(0x12), 1_700_000_500);
        shelf.played(nes(7), 5);

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "ebobo shelf 1\nnes\t00000007\t0\t5\t1\nnes\tdeadbeef\t1\t0\t0\nmd\t00000012\t0\t1700000500\t2\n");
        assert!(!dir.join("deep").join("shelf.txt.tmp").exists());

        let again = Shelf::load(path);
        assert!(again.is_favourite(nes(0xDEAD_BEEF)));
        assert!(!again.is_favourite(md(0x12)));
        assert_eq!(again.last_played(md(0x12)), Some(1_700_000_500));
        assert_eq!(again.last_played(nes(7)), Some(5));
        assert_eq!(again.last_played(nes(0xDEAD_BEEF)), None);
        assert_eq!(again.entries[&md(0x12)].plays, 2);
        assert!(again.has_history());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_entries_are_not_written() {
        let dir = folder("default");
        let path = dir.join("shelf.txt");
        let mut shelf = Shelf::load(path.clone());
        assert!(shelf.toggle_favourite(nes(1)));
        assert!(shelf.toggle_favourite(nes(2)));
        assert!(!shelf.toggle_favourite(nes(1)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "ebobo shelf 1\nnes\t00000002\t1\t0\t0\n");
        assert!(!shelf.toggle_favourite(nes(2)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "ebobo shelf 1\n");
        assert!(shelf.entries.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_file_loads_what_it_can() {
        let dir = folder("damaged");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shelf.txt");

        fs::write(&path, [0xFF, 0xFE, 0x00, 0x80, b'\n']).unwrap();
        assert!(Shelf::load(path.clone()).entries.is_empty());

        fs::write(&path, "ebobo shelf 2\nnes\t00000001\t1\t0\t0\n").unwrap();
        assert!(Shelf::load(path.clone()).entries.is_empty());

        fs::write(&path, "").unwrap();
        assert!(Shelf::load(path.clone()).entries.is_empty());

        let text = "ebobo shelf 1\r\nnes\t00000001\t1\t10\t1\r\n\nsnes\t00000002\t1\t0\t0\nnes\t123\t1\t0\t0\nnes\t0000000g\t1\t0\t0\n\
                    nes\t00000003\t2\t0\t0\nnes\t00000004\t1\t-1\t0\nnes\t00000005\t1\t0\nnes\t00000006\t1\t0\t0\textra\ngarbage\n\
                    nes\t00000008\t0\t0\t0\nmd\tABCDEF01\t0\t99\t3\n";
        fs::write(&path, text).unwrap();
        let shelf = Shelf::load(path.clone());
        assert_eq!(shelf.entries.len(), 2);
        assert!(shelf.is_favourite(nes(1)));
        assert_eq!(shelf.last_played(nes(1)), Some(10));
        assert_eq!(shelf.last_played(md(0xABCD_EF01)), Some(99));

        assert!(Shelf::load(dir.join("missing.txt")).entries.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn in_memory_never_creates_a_file() {
        let dir = folder("memory");
        fs::create_dir_all(&dir).unwrap();
        let before = std::env::current_dir().ok().and_then(|d| fs::read_dir(d).ok()).map(|d| d.count());
        let mut shelf = Shelf::in_memory();
        assert!(shelf.path.is_none());
        assert!(shelf.toggle_favourite(nes(1)));
        shelf.played(nes(1), 42);
        assert!(shelf.is_favourite(nes(1)));
        assert_eq!(shelf.last_played(nes(1)), Some(42));
        assert!(shelf.has_history());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let after = std::env::current_dir().ok().and_then(|d| fs::read_dir(d).ok()).map(|d| d.count());
        assert_eq!(before, after);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn view_filters_and_orders_by_name() {
        let mut shelf = Shelf::in_memory();
        // Names without regard to case; the three Contras keep their order in the list.
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, ""), [2, 3, 5, 1, 0, 4]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "  \t "), [2, 3, 5, 1, 0, 4]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::System(System::Nes), ""), [2, 5, 0, 4]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::System(System::MegaDrive), ""), [3, 1]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::Favourites, ""), [] as [usize; 0]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::Recent, ""), [] as [usize; 0]);

        shelf.toggle_favourite(ITEMS[0].key);
        shelf.toggle_favourite(ITEMS[3].key);
        assert_eq!(shown(&shelf, &ITEMS, Filter::Favourites, ""), [3, 0]);
        // The same CRC on the other system is another game.
        shelf.toggle_favourite(md(1));
        assert_eq!(shown(&shelf, &ITEMS, Filter::Favourites, ""), [3, 0]);
        assert_eq!(shown(&shelf, &[], Filter::All, ""), [] as [usize; 0]);
    }

    #[test]
    fn view_recent_is_latest_first() {
        let mut shelf = Shelf::in_memory();
        shelf.played(ITEMS[2].key, 100);
        shelf.played(ITEMS[1].key, 300);
        shelf.played(ITEMS[5].key, 200);
        shelf.played(ITEMS[0].key, 200);
        shelf.toggle_favourite(ITEMS[4].key);
        assert_eq!(shown(&shelf, &ITEMS, Filter::Recent, ""), [1, 0, 5, 2]);
        shelf.played(ITEMS[2].key, 400);
        assert_eq!(shown(&shelf, &ITEMS, Filter::Recent, ""), [2, 1, 0, 5]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::Recent, "contra"), [2, 5]);
    }

    #[test]
    fn view_search_matches_name_or_file() {
        let shelf = Shelf::in_memory();
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "SONIC"), [1, 4]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "ёжик"), [4]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "ЁЖ son"), [4]);
        // A file name alone, and one word in the name with the other in the file.
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "probo"), [2]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "  hard   CONTRA "), [3]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "zelda (u)"), [0]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "contra"), [2, 3, 5]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::System(System::Nes), "contra"), [2, 5]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "contra zelda"), [] as [usize; 0]);
        assert_eq!(shown(&shelf, &ITEMS, Filter::All, "nothing"), [] as [usize; 0]);
    }

    #[test]
    fn systems_are_nes_first_and_only_those_present() {
        assert_eq!(systems(&[]), []);
        assert_eq!(systems(&ITEMS), [System::Nes, System::MegaDrive]);
        assert_eq!(systems(&ITEMS[1..2]), [System::MegaDrive]);
        assert_eq!(systems(&ITEMS[2..3]), [System::Nes]);
        assert_eq!(systems(&[ITEMS[1], ITEMS[0]]), [System::Nes, System::MegaDrive]);
    }

    #[test]
    fn ago_boundaries() {
        const HOUR: u64 = 3600;
        const DAY: u64 = 86_400;
        let now = 2_000_000_000;
        let at = |seconds: u64| ago(now, now - seconds);
        assert_eq!(ago(now, now + 500), Ago::Today);
        assert_eq!(ago(5, u64::MAX), Ago::Today);
        assert_eq!(at(0), Ago::Today);
        assert_eq!(at(20 * HOUR - 1), Ago::Today);
        assert_eq!(at(20 * HOUR), Ago::Yesterday);
        assert_eq!(at(44 * HOUR - 1), Ago::Yesterday);
        assert_eq!(at(44 * HOUR), Ago::Days(2));
        assert_eq!(at(2 * DAY), Ago::Days(2));
        assert_eq!(at(3 * DAY - 1), Ago::Days(2));
        assert_eq!(at(3 * DAY), Ago::Days(3));
        assert_eq!(at(14 * DAY - 1), Ago::Days(13));
        assert_eq!(at(14 * DAY), Ago::Weeks(2));
        assert_eq!(at(63 * DAY - 1), Ago::Weeks(8));
        assert_eq!(at(63 * DAY), Ago::Months(2));
        assert_eq!(at(720 * DAY - 1), Ago::Months(23));
        assert_eq!(at(720 * DAY), Ago::Years(1));
        assert_eq!(at(730 * DAY - 1), Ago::Years(1));
        assert_eq!(at(730 * DAY), Ago::Years(2));
        assert_eq!(ago(u64::MAX, 0), Ago::Years(u32::MAX));
    }
}
