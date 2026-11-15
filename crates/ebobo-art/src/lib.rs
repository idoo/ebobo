//! ebobo-art: pictures and proper names for the library's game cards.
//!
//! For each game (system + CRC-32) the best picture there is:
//! 1. its box art from the libretro thumbnails collection, found through the No-Intro names
//!    database by CRC (so the file's own name does not matter);
//! 2. without a match (hacks, homebrew), offline, or with downloads switched off: the game's
//!    own title screen, captured by running it for a few seconds.
//!
//! Everything is cached on disk under one folder; nothing here blocks the caller: the work
//! happens on one background thread and [`Art::poll`] takes in what it finished.
//!
//! Module ownership (one owner per file):
//! - `names.rs`: the names database (parse, lookup, where to download it and the pictures).
//! - `image.rs`: PNG in and out, shrinking. `net.rs`: downloading through the system `curl`.
//! - `title.rs`: capturing a title screen from a ROM.
//! - `shelf.rs`: favourites, play history, and the filtered, sorted view of the library.
//! - `lib.rs`: the [`Art`] service: queue, background thread, disk and memory caches.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use names::Names;
use net::FetchError;

pub use ebobo_console::System;

pub mod image;
pub mod names;
pub mod net;
pub mod shelf;
pub mod title;

/// Largest side of a cached picture, in pixels: a card is never drawn larger.
pub const MAX_SIDE: u32 = 320;

/// A picture: `w`×`h` RGBA8888, row by row, alpha not premultiplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// A game: the system and the CRC-32 the library knows it by (`ebobo_console::RomId::crc32`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub system: System,
    pub crc32: u32,
}

/// What a picture shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The box, any proportions (most are upright).
    BoxArt,
    /// The game's own screen, 4:3.
    TitleScreen,
}

/// Pictures kept in memory at most (about 300 KB each at most); beyond it the least recently
/// wanted are dropped and loaded again from disk when wanted.
const MAX_PICTURES: usize = 400;
/// Jobs waiting at most; beyond it the oldest are dropped (and asked again when shown).
const MAX_QUEUE: usize = 200;
/// A names database on disk older than this is downloaded again.
const NAMES_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The limit of one download, in seconds.
const TIMEOUT_SECS: u32 = 30;

/// The folder and file name part of a system in the cache.
fn system_name(system: System) -> &'static str {
    match system {
        System::Nes => "nes",
        System::MegaDrive => "md",
    }
}

/// The index of a system in the per-system arrays.
fn slot(system: System) -> usize {
    match system {
        System::Nes => 0,
        System::MegaDrive => 1,
    }
}

/// One request to the background thread.
struct Job {
    key: Key,
    /// The game's file; empty for an upgrade.
    rom: PathBuf,
    /// Only look for box art online (the title screen is shown already): nothing is reported
    /// when there is none.
    upgrade: bool,
}

/// What the background thread reports.
enum Done {
    /// `tried_online`: the box art was looked for (or need not be) in this session.
    Picture { key: Key, picture: Picture, kind: Kind, tried_online: bool },
    /// No picture for this game.
    Failed(Key),
    /// The names database of a system was loaded.
    Names(System, Arc<Names>),
}

/// The waiting jobs, shared with the background thread.
#[derive(Default)]
struct Queue {
    /// The jobs, oldest first, and whether the service is gone.
    jobs: Mutex<(VecDeque<Job>, bool)>,
    ready: Condvar,
    /// Jobs the background thread took so far.
    started: AtomicUsize,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, (VecDeque<Job>, bool)> {
        self.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Adds a job; returns the oldest one when the queue became too long.
    fn push(&self, job: Job) -> Option<Job> {
        let mut guard = self.lock();
        guard.0.push_back(job);
        let dropped = if guard.0.len() > MAX_QUEUE { guard.0.pop_front() } else { None };
        drop(guard);
        self.ready.notify_one();
        dropped
    }

    /// Waits for a job and takes the newest; `None` once the service is gone.
    fn pop(&self) -> Option<Job> {
        let mut guard = self.lock();
        loop {
            if guard.1 {
                return None;
            }
            if let Some(job) = guard.0.pop_back() {
                self.started.fetch_add(1, Ordering::Relaxed);
                return Some(job);
            }
            guard = self.ready.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close(&self) {
        let mut guard = self.lock();
        guard.0.clear();
        guard.1 = true;
        drop(guard);
        self.ready.notify_all();
    }
}

/// Where a game's picture stands on the main side.
enum State {
    /// A job is waiting or running.
    Queued,
    Ready(Picture, Kind),
    /// No picture: not asked again in this session.
    Failed,
    /// Not in memory (evicted, or its job fell off the queue): the next `want` asks again.
    Dropped,
}

struct Entry {
    state: State,
    /// The value of `Art::tick` at the last `want`.
    last_use: u64,
    /// The box art of a game shown by its title screen was looked for in this session.
    upgrade_asked: bool,
}

/// The pictures and names of the library.
pub struct Art {
    online: Arc<AtomicBool>,
    queue: Arc<Queue>,
    done: Receiver<Done>,
    entries: HashMap<Key, Entry>,
    /// Entries in `State::Ready`.
    ready: usize,
    /// The names database per system (`slot`), once loaded.
    names: [Option<Arc<Names>>; 2],
    /// Counts `want` calls: the clock of "least recently wanted".
    tick: u64,
}

/// Reads a game's ROM bytes from its file for the title-screen capture (the caller knows how
/// to open its containers, a `.zip` for one). Runs on the background thread.
pub type Loader = Box<dyn Fn(&Path) -> Option<Vec<u8>> + Send>;

impl Art {
    /// `dir`: the cache folder (created when needed). `online`: downloads are allowed. ROM
    /// files are read as they are (see [`Art::with_loader`]).
    pub fn new(dir: PathBuf, online: bool) -> Art {
        Art::with_loader(dir, online, Box::new(|path| fs::read(path).ok()))
    }

    /// [`Art::new`] with the caller's way of reading a ROM file.
    pub fn with_loader(dir: PathBuf, online: bool, load: Loader) -> Art {
        let online = Arc::new(AtomicBool::new(online));
        let queue = Arc::new(Queue::default());
        let (sender, done) = mpsc::channel();
        let worker = Worker { dir, online: online.clone(), done: sender, names: Default::default(), load };
        let jobs = queue.clone();
        // Detached: it may be inside a download when the service is dropped. Without a thread
        // (the system refused one) the cards stay without pictures.
        let _ = std::thread::Builder::new().name("ebobo-art".into()).spawn(move || worker.work(&jobs));
        Art { online, queue, done, entries: HashMap::new(), ready: 0, names: [None, None], tick: 0 }
    }

    /// Switches downloads on or off (the setting). Off: only what is on disk already, and
    /// title screens.
    pub fn set_online(&mut self, online: bool) {
        self.online.store(online, Ordering::Relaxed);
    }

    /// Asks for the picture and name of a game. Cheap and idempotent: the library calls it
    /// for every card on screen, every frame. `rom` is the game's file, read on the
    /// background thread when the title screen has to be captured (a `.zip` gets box art
    /// only: this crate has no zip reader).
    pub fn want(&mut self, key: Key, rom: &Path) {
        self.tick += 1;
        let tick = self.tick;
        let upgrade = match self.entries.get_mut(&key) {
            Some(entry) => {
                entry.last_use = tick;
                match entry.state {
                    State::Dropped => {
                        entry.state = State::Queued;
                        false
                    }
                    // Shown by its title screen and downloads are on: look for the box art once.
                    State::Ready(_, Kind::TitleScreen) if !entry.upgrade_asked && self.online.load(Ordering::Relaxed) => {
                        entry.upgrade_asked = true;
                        true
                    }
                    _ => return,
                }
            }
            None => {
                self.entries.insert(key, Entry { state: State::Queued, last_use: tick, upgrade_asked: false });
                false
            }
        };
        let rom = if upgrade { PathBuf::new() } else { rom.to_path_buf() };
        if let Some(dropped) = self.queue.push(Job { key, rom, upgrade }) {
            // Fell off the queue: asked again when its card is shown.
            if let Some(entry) = self.entries.get_mut(&dropped.key) {
                if dropped.upgrade {
                    entry.upgrade_asked = false;
                } else {
                    entry.state = State::Dropped;
                }
            }
        }
    }

    /// The picture of a game, once it is ready, and what it shows.
    pub fn get(&self, key: Key) -> Option<(&Picture, Kind)> {
        match &self.entries.get(&key)?.state {
            State::Ready(picture, kind) => Some((picture, *kind)),
            _ => None,
        }
    }

    /// The game's proper name from the names database ("Chip 'n Dale - Rescue Rangers
    /// (Europe)"), once known; `None` for a game the database does not have.
    pub fn name(&self, key: Key) -> Option<&str> {
        self.names[slot(key.system)].as_ref()?.get(key.crc32)
    }

    /// Takes in what the background thread finished. True when a picture or a name arrived
    /// since the last call: the library should be redrawn.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(done) = self.done.try_recv() {
            match done {
                Done::Picture { key, picture, kind, tried_online } => {
                    if let Some(entry) = self.entries.get_mut(&key) {
                        if !matches!(entry.state, State::Ready(..)) {
                            self.ready += 1;
                        }
                        entry.upgrade_asked |= tried_online;
                        entry.state = State::Ready(picture, kind);
                        changed = true;
                    }
                }
                Done::Failed(key) => {
                    if let Some(entry) = self.entries.get_mut(&key) {
                        if matches!(entry.state, State::Queued) {
                            entry.state = State::Failed;
                        }
                    }
                }
                Done::Names(system, names) => {
                    self.names[slot(system)] = Some(names);
                    changed = true;
                }
            }
        }
        if self.ready > MAX_PICTURES {
            self.evict();
        }
        changed
    }

    /// Drops the least recently wanted pictures beyond `MAX_PICTURES`.
    fn evict(&mut self) {
        let mut held: Vec<(u64, Key)> =
            self.entries.iter().filter(|(_, entry)| matches!(entry.state, State::Ready(..))).map(|(key, entry)| (entry.last_use, *key)).collect();
        held.sort_unstable_by_key(|(last_use, _)| *last_use);
        for (_, key) in held.iter().take(held.len().saturating_sub(MAX_PICTURES)) {
            if let Some(entry) = self.entries.get_mut(key) {
                entry.state = State::Dropped;
            }
        }
        self.ready = held.len().min(MAX_PICTURES);
    }
}

impl Drop for Art {
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// The names database of one system on the background thread.
#[derive(Default)]
struct NamesSlot {
    names: Option<Arc<Names>>,
    /// The disk cache was looked at.
    disk_read: bool,
    /// What is loaded is younger than `NAMES_MAX_AGE`.
    fresh: bool,
    /// The download was tried in this session.
    download_tried: bool,
}

/// The background thread's side.
struct Worker {
    dir: PathBuf,
    online: Arc<AtomicBool>,
    done: Sender<Done>,
    names: [NamesSlot; 2],
    load: Loader,
}

impl Worker {
    /// Serves jobs until the service is dropped.
    fn work(mut self, queue: &Queue) {
        while let Some(job) = queue.pop() {
            // One game that breaks a core must not take the pictures of all the others with it.
            let done = catch_unwind(AssertUnwindSafe(|| self.run(&job))).unwrap_or(if job.upgrade { None } else { Some(Done::Failed(job.key)) });
            if let Some(done) = done {
                if self.done.send(done).is_err() {
                    return;
                }
            }
        }
    }

    /// One job: the cached picture, else the box art, else the title screen.
    fn run(&mut self, job: &Job) -> Option<Done> {
        let key = job.key;
        let online = self.online.load(Ordering::Relaxed);
        let base = self.dir.join(system_name(key.system)).join(format!("{:08x}", key.crc32));
        // The names on disk serve offline too.
        self.names(key.system, false);
        if !job.upgrade {
            if let Some(picture) = fs::read(base.with_extension("png")).ok().and_then(|bytes| image::decode_png(&bytes).ok()) {
                let kind = match fs::read_to_string(base.with_extension("kind")) {
                    Ok(word) if word.trim() == "title" => Kind::TitleScreen,
                    _ => Kind::BoxArt,
                };
                return Some(Done::Picture { key, picture, kind, tried_online: false });
            }
        }
        if online && !base.with_extension("none").exists() {
            if let Some(picture) = self.box_art(key, &base) {
                return Some(Done::Picture { key, picture, kind: Kind::BoxArt, tried_online: true });
            }
        }
        if job.upgrade {
            return None;
        }
        let title = (self.load)(&job.rom).filter(|bytes| ebobo_console::identify(bytes).is_some()).and_then(|bytes| title::capture(&bytes));
        match title {
            Some(picture) => {
                let picture = image::fit(picture, MAX_SIDE, MAX_SIDE);
                store(&base, &picture, "title");
                Some(Done::Picture { key, picture, kind: Kind::TitleScreen, tried_online: online })
            }
            None => Some(Done::Failed(key)),
        }
    }

    /// Downloads the box art of a game the names database has, shrinks it and caches it.
    /// The server not having it (404) leaves the `.none` marker.
    fn box_art(&mut self, key: Key, base: &Path) -> Option<Picture> {
        let names = self.names(key.system, true)?;
        let url = names::box_art_url(key.system, names.get(key.crc32)?);
        let tmp = base.with_extension("tmp");
        let fetched = net::fetch(&url, &tmp, TIMEOUT_SECS);
        let bytes = fs::read(&tmp);
        let _ = fs::remove_file(&tmp);
        match fetched {
            Ok(()) => {
                let picture = image::fit(image::decode_png(&bytes.ok()?).ok()?, MAX_SIDE, MAX_SIDE);
                store(base, &picture, "box");
                Some(picture)
            }
            Err(FetchError::NotFound) => {
                let _ = fs::write(base.with_extension("none"), "");
                None
            }
            Err(FetchError::Failed(_)) => None,
        }
    }

    /// The names database of `system`: read from the disk cache the first time, and with
    /// `online` downloaded (once per session) when there is none or it is old. Every load is
    /// reported to the main side.
    fn names(&mut self, system: System, online: bool) -> Option<Arc<Names>> {
        let slot = &mut self.names[slot(system)];
        if slot.disk_read && (!online || slot.fresh || slot.download_tried) {
            return slot.names.clone();
        }
        let cache = self.dir.join(format!("names-{}.tsv", system_name(system)));
        if !slot.disk_read {
            slot.disk_read = true;
            let names = fs::read_to_string(&cache).map(|text| Names::from_cache(&text)).unwrap_or_default();
            if !names.is_empty() {
                let age = fs::metadata(&cache).and_then(|meta| meta.modified()).ok().map(|at| at.elapsed().unwrap_or_default());
                slot.fresh = age.is_some_and(|age| age < NAMES_MAX_AGE);
                let names = Arc::new(names);
                slot.names = Some(names.clone());
                let _ = self.done.send(Done::Names(system, names));
            }
        }
        if online && !slot.fresh && !slot.download_tried {
            slot.download_tried = true;
            let tmp = cache.with_extension("tmp");
            let fetched = net::fetch(&names::database_url(system), &tmp, TIMEOUT_SECS);
            let bytes = fs::read(&tmp);
            let _ = fs::remove_file(&tmp);
            if let (Ok(()), Ok(bytes)) = (fetched, bytes) {
                let names = Names::parse(&String::from_utf8_lossy(&bytes));
                if !names.is_empty() {
                    if fs::write(&tmp, names.to_cache()).is_ok() {
                        let _ = fs::rename(&tmp, &cache);
                    }
                    // The new database may name games the old one did not: ask the server again.
                    remove_none_markers(&self.dir.join(system_name(system)));
                    slot.fresh = true;
                    let names = Arc::new(names);
                    slot.names = Some(names.clone());
                    let _ = self.done.send(Done::Names(system, names));
                }
            }
        }
        slot.names.clone()
    }
}

/// Writes a picture to the cache as `<base>.png` with its kind word in `<base>.kind`.
/// A failure is silent: the picture is made again next time.
fn store(base: &Path, picture: &Picture, kind: &str) {
    let bytes = image::encode_png(picture);
    if bytes.is_empty() {
        return;
    }
    if let Some(folder) = base.parent() {
        let _ = fs::create_dir_all(folder);
    }
    // The picture appears complete or not at all, and never before its kind.
    let part = base.with_extension("part");
    if fs::write(&part, bytes).is_ok() && fs::write(base.with_extension("kind"), kind).is_ok() {
        let _ = fs::rename(&part, base.with_extension("png"));
    }
}

/// Deletes the "no box art" markers of a system's folder.
fn remove_none_markers(folder: &Path) {
    let Ok(files) = fs::read_dir(folder) else { return };
    for file in files.flatten() {
        let path = file.path();
        if path.extension().is_some_and(|extension| extension == "none") {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// An empty folder of this test.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-art-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn nes(crc32: u32) -> Key {
        Key { system: System::Nes, crc32 }
    }

    fn picture(shade: u8) -> Picture {
        Picture { w: 2, h: 2, rgba: [shade, 0x40, 0x80, 0xFF].repeat(4) }
    }

    /// Puts a picture into the cache folder as the service would.
    fn seed(dir: &Path, key: Key, picture: &Picture, kind: &str) {
        store(&dir.join(system_name(key.system)).join(format!("{:08x}", key.crc32)), picture, kind);
    }

    /// Polls until `ready` holds, for `secs` seconds at most.
    fn wait(art: &mut Art, secs: u64, ready: impl Fn(&Art) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            art.poll();
            if ready(art) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn failed(art: &Art, key: Key) -> bool {
        matches!(art.entries.get(&key), Some(Entry { state: State::Failed, .. }))
    }

    fn started(art: &Art) -> usize {
        art.queue.started.load(Ordering::Relaxed)
    }

    #[test]
    fn cached_picture_is_served() {
        let dir = folder("cached");
        seed(&dir, nes(0x2A), &picture(1), "title");
        seed(&dir, nes(0x2B), &picture(2), "box");
        let mut art = Art::new(dir.clone(), false);
        art.want(nes(0x2A), Path::new("none.nes"));
        art.want(nes(0x2B), Path::new("none.nes"));
        assert_eq!(art.get(nes(0x2A)), None);
        assert!(wait(&mut art, 2, |art| art.get(nes(0x2A)).is_some() && art.get(nes(0x2B)).is_some()));
        assert_eq!(art.get(nes(0x2A)), Some((&picture(1), Kind::TitleScreen)));
        assert_eq!(art.get(nes(0x2B)), Some((&picture(2), Kind::BoxArt)));
        // Offline: the title screen is not sent to look for its box art.
        art.want(nes(0x2A), Path::new("none.nes"));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(started(&art), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_game_fails_once() {
        let dir = folder("unknown");
        let rom = dir.join("garbage.nes");
        fs::write(&rom, b"not a game at all").unwrap();
        let mut art = Art::new(dir.join("cache"), false);
        art.want(nes(7), &rom);
        assert!(wait(&mut art, 5, |art| failed(art, nes(7))));
        assert_eq!(art.get(nes(7)), None);
        assert_eq!(started(&art), 1);
        art.want(nes(7), &rom);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!art.poll());
        assert_eq!(started(&art), 1);
        assert!(failed(&art, nes(7)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn want_twice_queues_once() {
        let dir = folder("twice");
        let mut art = Art::new(dir.clone(), false);
        art.want(nes(9), Path::new("none.nes"));
        art.want(nes(9), Path::new("none.nes"));
        assert!(wait(&mut art, 5, |art| failed(art, nes(9))));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(started(&art), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn memory_cap_evicts_the_least_recently_wanted() {
        let dir = folder("cap");
        let count = MAX_PICTURES as u32 + 5;
        for crc in 0..count {
            seed(&dir, nes(crc), &picture(crc as u8), "box");
        }
        let mut art = Art::new(dir.clone(), false);
        // In batches below the queue's length, so none falls off it.
        for batch in (0..count).collect::<Vec<u32>>().chunks(100) {
            for &crc in batch {
                art.want(nes(crc), Path::new("none.nes"));
            }
            assert!(wait(&mut art, 20, |art| batch.iter().all(|&crc| !matches!(art.entries[&nes(crc)].state, State::Queued))));
        }
        let held = (0..count).filter(|&crc| art.get(nes(crc)).is_some()).count();
        assert_eq!(held, MAX_PICTURES);
        assert_eq!(art.ready, MAX_PICTURES);
        assert_eq!(art.get(nes(0)), None);
        assert_eq!(art.get(nes(4)), None);
        assert!(art.get(nes(5)).is_some());
        assert!(art.get(nes(count - 1)).is_some());
        // Wanted again, it comes back from disk and the next oldest goes.
        art.want(nes(0), Path::new("none.nes"));
        assert!(wait(&mut art, 20, |art| art.get(nes(0)).is_some()));
        assert_eq!(art.get(nes(0)), Some((&picture(0), Kind::BoxArt)));
        assert_eq!(art.get(nes(5)), None);
        assert_eq!(art.ready, MAX_PICTURES);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn names_answer_from_the_seeded_database() {
        let dir = folder("names");
        let names = Names::from_cache("84F7FC31\tChip 'n Dale - Rescue Rangers (Europe)\n");
        fs::write(dir.join("names-nes.tsv"), names.to_cache()).unwrap();
        let mut art = Art::new(dir.clone(), false);
        assert_eq!(art.name(nes(0x84F7_FC31)), None);
        art.want(nes(0x84F7_FC31), Path::new("none.nes"));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut changed = false;
        while !changed && Instant::now() < deadline {
            changed = art.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(changed, "poll reports the names");
        assert_eq!(art.name(nes(0x84F7_FC31)), Some("Chip 'n Dale - Rescue Rangers (Europe)"));
        assert_eq!(art.name(nes(1)), None);
        assert_eq!(art.name(Key { system: System::MegaDrive, crc32: 0x84F7_FC31 }), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn queue_serves_the_newest_and_drops_the_oldest() {
        let queue = Queue::default();
        for crc in 0..MAX_QUEUE as u32 {
            assert!(queue.push(Job { key: nes(crc), rom: PathBuf::new(), upgrade: false }).is_none());
        }
        let dropped = queue.push(Job { key: nes(1000), rom: PathBuf::new(), upgrade: false });
        assert_eq!(dropped.map(|job| job.key), Some(nes(0)));
        assert_eq!(queue.pop().map(|job| job.key), Some(nes(1000)));
        assert_eq!(queue.pop().map(|job| job.key), Some(nes(MAX_QUEUE as u32 - 1)));
        queue.close();
        assert!(queue.pop().is_none());
    }

    /// Online: the real database and the real picture server.
    #[test]
    #[ignore]
    fn online_box_art_and_name() {
        let dir = folder("online");
        let key = nes(0x84F7_FC31);
        let mut art = Art::new(dir.clone(), true);
        art.want(key, Path::new("none.nes"));
        assert!(wait(&mut art, 20, |art| art.get(key).is_some()), "no picture within 20 s");
        let (picture, kind) = art.get(key).unwrap();
        assert_eq!(kind, Kind::BoxArt);
        assert!(picture.w <= MAX_SIDE && picture.h <= MAX_SIDE && picture.w.max(picture.h) == MAX_SIDE, "{}x{}", picture.w, picture.h);
        assert_eq!(art.name(key), Some("Chip 'n Dale - Rescue Rangers (Europe)"));
        assert!(dir.join("names-nes.tsv").is_file());
        assert!(dir.join("nes/84f7fc31.png").is_file());
        assert_eq!(fs::read_to_string(dir.join("nes/84f7fc31.kind")).unwrap(), "box");
        assert!(!dir.join("nes/84f7fc31.tmp").exists());
        let _ = fs::remove_dir_all(dir);
    }
}
