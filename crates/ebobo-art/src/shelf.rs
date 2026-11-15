//! The library's shelf: which games are favourites, when each was last played, and the list
//! a filter and a search text leave, in the order the cards are shown. Owner: shelf slice.
//!
//! Pure logic plus one small text file. Nothing here knows about pictures or the interface.

use std::path::PathBuf;

use crate::{Key, System};

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

/// Favourites and play history, kept in a text file.
#[derive(Clone, Debug, Default)]
pub struct Shelf {
    _todo: (),
}

impl Shelf {
    /// An empty shelf that is never written to disk (tests, and a data folder that cannot be
    /// used).
    pub fn in_memory() -> Shelf {
        Shelf::default()
    }

    /// Reads the shelf file at `path` (a missing or damaged file gives an empty shelf; lines
    /// it cannot read are skipped). Later changes are written back there.
    pub fn load(_path: PathBuf) -> Shelf {
        todo!("owner: shelf slice")
    }

    pub fn is_favourite(&self, _key: Key) -> bool {
        todo!("owner: shelf slice")
    }

    /// Flips the favourite mark, saves, and returns the new state.
    pub fn toggle_favourite(&mut self, _key: Key) -> bool {
        todo!("owner: shelf slice")
    }

    /// Notes that the game was started at `now` (Unix seconds) and saves.
    pub fn played(&mut self, _key: Key, _now: u64) {
        todo!("owner: shelf slice")
    }

    /// When the game was last started (Unix seconds).
    pub fn last_played(&self, _key: Key) -> Option<u64> {
        todo!("owner: shelf slice")
    }

    /// Any game was ever played (the "recent" filter has something to show).
    pub fn has_history(&self) -> bool {
        todo!("owner: shelf slice")
    }

    /// Fills `out` with the indices into `items` of the games that pass `filter` and `search`,
    /// in display order: by name without regard to case (ties by index), except `Recent`,
    /// which is latest first. `search` is matched without regard to case as a substring of
    /// the name or the file name; every word of it must match somewhere; empty = everything.
    pub fn view(&self, _items: &[Item], _filter: Filter, _search: &str, _out: &mut Vec<usize>) {
        todo!("owner: shelf slice")
    }
}

/// The systems that have games in `items`, in a fixed order (NES first).
pub fn systems(_items: &[Item]) -> Vec<System> {
    todo!("owner: shelf slice")
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

pub fn ago(_now: u64, _then: u64) -> Ago {
    todo!("owner: shelf slice")
}
