//! ROM library: folder scan, CRC index, ROM file and .zip loading. Owner: canvas slice.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use ebobo_console::{Region, System};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RomEntry {
    pub path: PathBuf,
    /// Display name: file stem without extension (zip: the stem of the ROM entry inside).
    pub name: String,
    pub system: System,
    /// `RomId::crc32`, the netplay identity.
    pub crc32: u32,
    /// NES mapper number, 0 for other systems.
    pub mapper: u16,
    /// A `Console` can run it (`false`: a NES board ebobo does not emulate).
    pub supported: bool,
    pub region: Region,
    /// When the file was last modified, seconds since the Unix epoch (0 = unknown). Orders
    /// entries with the same name newest first and dates them in the picker.
    pub mtime: u64,
}

/// Extensions of ROM images, lower case: NES, then Mega Drive. `.bin` is used for many other
/// things too; such a file is read like the rest and dropped when `identify` does not know it.
const ROM_EXTS: [&str; 5] = ["nes", "md", "gen", "smd", "bin"];

fn is_rom_ext(path: &Path) -> bool {
    ext_lower(path).is_some_and(|e| ROM_EXTS.contains(&e.as_str()))
}

const MAX_ROM_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 4;
const MAX_FILES: usize = 5000;

/// Folders under the home directory that are scanned by default.
const HOME_DIRS: [&str; 7] = ["ROMs", "roms", "Games/NES", "Games/Genesis", "Games/MegaDrive", "Documents/ROMs", "Downloads"];
/// EmuDeck's folders under `Emulation/roms` (Steam Deck, internal and SD card).
const EMUDECK_DIRS: [&str; 3] = ["Emulation/roms/nes", "Emulation/roms/genesis", "Emulation/roms/megadrive"];

/// Folders scanned by default, those that exist: ~/ROMs, ~/roms, ~/Games/NES, ~/Games/Genesis,
/// ~/Games/MegaDrive, ~/Documents/ROMs, ~/Downloads, `config::data_dir()/roms`,
/// ~/Emulation/roms/{nes,genesis,megadrive} and the same three under every
/// `/run/media/**/Emulation/roms/` (EmuDeck on Steam Deck, internal and SD card). A folder
/// reached twice (a symlink, or ~/ROMs and ~/roms on a Mac's case-blind disk) is listed once,
/// under its first spelling.
pub fn default_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = candidate_dirs(home.as_deref(), Path::new("/run/media"));
    let mut seen = HashSet::new();
    dirs.retain(|d| d.is_dir() && seen.insert(std::fs::canonicalize(d).unwrap_or_else(|_| d.clone())));
    dirs
}

/// `default_dirs` before the check that a folder exists. `media` is `/run/media`.
fn candidate_dirs(home: Option<&Path>, media: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.extend(HOME_DIRS.iter().map(|rel| home.join(rel)));
        dirs.push(crate::config::data_dir().join("roms"));
        dirs.extend(EMUDECK_DIRS.iter().map(|rel| home.join(rel)));
    } else {
        dirs.push(crate::config::data_dir().join("roms"));
    }
    // <media>/<user>/<card>/Emulation/roms/<system> (and <media>/<card>/... on some setups).
    for level1 in subdirs(media) {
        dirs.extend(EMUDECK_DIRS.iter().map(|rel| level1.join(rel)));
        for level2 in subdirs(&level1) {
            dirs.extend(EMUDECK_DIRS.iter().map(|rel| level2.join(rel)));
        }
    }
    dirs
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

fn ext_lower(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Why a file cannot be read as a ROM. `Display` is English, for logs and tests; the
/// interface words it in its own language from the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RomFileError {
    /// The file, or the ROM inside a zip, is larger than `MAX_ROM_BYTES`.
    TooLarge,
    /// A zip with no entry that has one of `ROM_EXTS`.
    NoRomInZip,
    /// The bytes are not a ROM of a console ebobo knows.
    NotRom,
    /// The file (or the zip) could not be read; the system's message.
    Read(String),
}

impl std::fmt::Display for RomFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RomFileError::TooLarge => write!(f, "file is larger than 8 MiB"),
            RomFileError::NoRomInZip => write!(f, "no ROM file inside the zip"),
            RomFileError::NotRom => write!(f, "not a ROM"),
            RomFileError::Read(why) => write!(f, "{why}"),
        }
    }
}

/// Reads the ROM bytes and, for a zip, the stem of the ROM entry inside: the first file with
/// one of `ROM_EXTS`.
fn read_rom(path: &Path) -> Result<(Vec<u8>, Option<String>), RomFileError> {
    let read = |e: &dyn std::fmt::Display| RomFileError::Read(e.to_string());
    let meta = fs::metadata(path).map_err(|e| read(&e))?;
    if meta.len() > MAX_ROM_BYTES {
        return Err(RomFileError::TooLarge);
    }
    match ext_lower(path).as_deref() {
        Some("zip") => {
            let file = fs::File::open(path).map_err(|e| read(&e))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| read(&e))?;
            for i in 0..zip.len() {
                let mut entry = zip.by_index(i).map_err(|e| read(&e))?;
                let name = entry.name().to_string();
                if !entry.is_file() || !is_rom_ext(Path::new(&name)) {
                    continue;
                }
                if entry.size() > MAX_ROM_BYTES {
                    return Err(RomFileError::TooLarge);
                }
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                (&mut entry).take(MAX_ROM_BYTES + 1).read_to_end(&mut bytes).map_err(|e| read(&e))?;
                if bytes.len() as u64 > MAX_ROM_BYTES {
                    return Err(RomFileError::TooLarge);
                }
                let inner = name.rsplit(['/', '\\']).next().unwrap_or(&name);
                return Ok((bytes, Some(stem(Path::new(inner)))));
            }
            Err(RomFileError::NoRomInZip)
        }
        _ => fs::read(path).map(|b| (b, None)).map_err(|e| read(&e)),
    }
}

/// Reads a ROM file: `.nes`, `.md`, `.gen`, `.smd`, `.bin` as is, `.zip` = the first entry
/// inside with one of those extensions.
pub fn load_rom_file(path: &Path) -> Result<Vec<u8>, RomFileError> {
    read_rom(path).map(|(bytes, _)| bytes)
}

/// The library entry of a file's bytes; an error when `ebobo_console::identify` does not know
/// them as a ROM.
fn make_entry(path: &Path, bytes: &[u8], inner_stem: Option<String>, mtime: u64) -> Result<RomEntry, RomFileError> {
    let id = ebobo_console::identify(bytes).ok_or(RomFileError::NotRom)?;
    Ok(RomEntry {
        path: path.to_path_buf(),
        name: inner_stem.unwrap_or_else(|| stem(path)),
        system: id.system,
        crc32: id.crc32,
        mapper: id.mapper,
        supported: id.supported,
        region: id.region,
        mtime,
    })
}

/// `RomEntry::supported` of a cached row, by this build's rules (the cache may be older than
/// the build): a NES game when its mapper is emulated; `identify` names other systems only
/// when they run.
fn supported(system: System, mapper: u16) -> bool {
    match system {
        System::Nes => ebobo_console::nes::SUPPORTED_MAPPERS.contains(&mapper),
        System::MegaDrive => true,
    }
}

/// Modification time as the cache stores it: nanoseconds since the Unix epoch, 0 = unknown.
fn mtime_nanos(meta: &fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
}

/// `RomEntry::mtime` from the cache's nanoseconds.
fn mtime_secs(nanos: u128) -> u64 {
    u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX)
}

/// Lower-cased name, then newest first, then path: two builds of one game sit together with
/// the latest on top.
fn sort_key(e: &RomEntry) -> (String, std::cmp::Reverse<u64>, &Path) {
    (e.name.to_lowercase(), std::cmp::Reverse(e.mtime), e.path.as_path())
}

/// Inserts keeping the list sorted by `sort_key`.
fn insert_sorted(list: &mut Vec<RomEntry>, entry: RomEntry) {
    let key = sort_key(&entry);
    let at = list.partition_point(|e| sort_key(e) < key);
    drop(key);
    list.insert(at, entry);
}

/// First line of the cache file. A file that starts with anything else was written by a build
/// with another row format (or another idea of what a ROM is): it is ignored and rebuilt.
const CACHE_HEADER: &str = "ebobo library cache 2";

/// What the cache keeps of a ROM.
#[derive(Clone)]
struct CachedRom {
    name: String,
    system: System,
    crc32: u32,
    mapper: u16,
    region: Region,
}

/// One cache row: file identity (size + mtime) → parse result (`None` = not a ROM).
#[derive(Clone)]
struct CacheRow {
    size: u64,
    mtime: u128,
    rom: Option<CachedRom>,
}

/// Rows of the cache file: `CACHE_HEADER`, then one line per file of tab-separated path, size,
/// mtime (nanoseconds), CRC-32 (hex), system (`nes`, `md`), mapper, region (`N`, `P`, `J`) and
/// name; a file that is not a ROM has `-` in the last five.
fn load_cache(path: &Path) -> HashMap<PathBuf, CacheRow> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else { return map };
    let mut lines = text.lines();
    if lines.next() != Some(CACHE_HEADER) {
        return map;
    }
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 8 {
            continue;
        }
        let (Ok(size), Ok(mtime)) = (f[1].parse(), f[2].parse()) else { continue };
        let rom = if f[3] == "-" {
            None
        } else {
            let (Ok(crc32), Ok(mapper)) = (u32::from_str_radix(f[3], 16), f[5].parse()) else { continue };
            let system = match f[4] {
                "nes" => System::Nes,
                "md" => System::MegaDrive,
                _ => continue,
            };
            let region = match f[6] {
                "P" => Region::Pal,
                "J" => Region::Japan,
                _ => Region::Ntsc,
            };
            Some(CachedRom { name: f[7].to_string(), system, crc32, mapper, region })
        };
        map.insert(PathBuf::from(f[0]), CacheRow { size, mtime, rom });
    }
    map
}

fn save_cache(path: &Path, rows: &HashMap<PathBuf, CacheRow>) {
    let mut text = format!("{CACHE_HEADER}\n");
    for (p, row) in rows {
        let Some(ps) = p.to_str() else { continue };
        let clean = |s: &str| !s.contains(['\t', '\n', '\r']);
        if !clean(ps) || row.rom.as_ref().is_some_and(|r| !clean(&r.name)) {
            continue;
        }
        match &row.rom {
            Some(CachedRom { name, system, crc32, mapper, region }) => {
                let s = match system {
                    System::Nes => "nes",
                    System::MegaDrive => "md",
                };
                let r = match region {
                    Region::Ntsc => "N",
                    Region::Pal => "P",
                    Region::Japan => "J",
                };
                text.push_str(&format!("{ps}\t{}\t{}\t{crc32:08x}\t{s}\t{mapper}\t{r}\t{name}\n", row.size, row.mtime));
            }
            None => text.push_str(&format!("{ps}\t{}\t{}\t-\t-\t-\t-\t-\n", row.size, row.mtime)),
        }
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tsv.tmp");
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

#[derive(Default)]
struct ScanState {
    scanning: bool,
    /// A rescan was requested while scanning: run once more.
    pending: bool,
}

struct Shared {
    dirs: Vec<PathBuf>,
    cache_path: PathBuf,
    entries: Mutex<Vec<RomEntry>>,
    /// Files added by `add_file`, kept across rescans.
    extra: Mutex<Vec<RomEntry>>,
    state: Mutex<ScanState>,
    /// Files actually read from disk (cache misses); lets tests prove cache hits.
    reads: AtomicUsize,
    /// Bumped after every change of `entries`; lets the app skip re-cloning an unchanged list.
    generation: AtomicU64,
}

impl Shared {
    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn read_entry(&self, path: &Path) -> Result<RomEntry, RomFileError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let (bytes, inner) = read_rom(path)?;
        let mtime = fs::metadata(path).map_or(0, |m| mtime_secs(mtime_nanos(&m)));
        make_entry(path, &bytes, inner, mtime)
    }

    fn scan_loop(self: Arc<Self>) {
        loop {
            self.scan_once();
            let mut st = lock(&self.state);
            if st.pending {
                st.pending = false;
            } else {
                st.scanning = false;
                return;
            }
        }
    }

    fn scan_once(&self) {
        let old_cache = load_cache(&self.cache_path);
        let mut new_cache = HashMap::new();
        // First scan: entries appear in the shared list as they are found. Rescans keep the old
        // list on screen and swap once, so nothing flickers away and back.
        let live = lock(&self.entries).is_empty();
        let mut found: Vec<RomEntry> = Vec::new();
        let mut crcs = HashSet::new();
        let mut budget = MAX_FILES;
        let mut add = |entry: RomEntry, found: &mut Vec<RomEntry>| {
            if crcs.insert(entry.crc32) {
                if live {
                    insert_sorted(&mut lock(&self.entries), entry.clone());
                    self.bump();
                }
                insert_sorted(found, entry);
            }
        };
        for dir in &self.dirs {
            let mut files = Vec::new();
            collect_files(dir, 0, &mut files, &mut budget);
            for (path, size, mtime) in files {
                let row = match old_cache.get(&path) {
                    Some(row) if row.size == size && row.mtime == mtime => row.clone(),
                    _ => CacheRow {
                        size,
                        mtime,
                        rom: self.read_entry(&path).ok().map(|e| CachedRom {
                            name: e.name,
                            system: e.system,
                            crc32: e.crc32,
                            mapper: e.mapper,
                            region: e.region,
                        }),
                    },
                };
                if let Some(CachedRom { name, system, crc32, mapper, region }) = row.rom.clone() {
                    let supported = supported(system, mapper);
                    let entry = RomEntry { path: path.clone(), name, system, crc32, mapper, supported, region, mtime: mtime_secs(mtime) };
                    add(entry, &mut found);
                }
                new_cache.insert(path, row);
            }
        }
        for entry in lock(&self.extra).iter() {
            add(entry.clone(), &mut found);
        }
        *lock(&self.entries) = found;
        self.bump();
        save_cache(&self.cache_path, &new_cache);
    }
}

/// Collects files with a ROM extension (`ROM_EXTS`) or `.zip`, ≤ 8 MiB, under `dir` in name
/// order, depth-first.
fn collect_files(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, u64, u128)>, budget: &mut usize) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut items: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    items.sort();
    for path in items {
        if *budget == 0 {
            return;
        }
        let hidden = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
        if hidden {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else { continue };
        if meta.is_dir() {
            if depth < MAX_DEPTH {
                collect_files(&path, depth + 1, out, budget);
            }
        } else if meta.is_file()
            && (is_rom_ext(&path) || ext_lower(&path).as_deref() == Some("zip"))
            && meta.len() <= MAX_ROM_BYTES
        {
            *budget -= 1;
            out.push((path, meta.len(), mtime_nanos(&meta)));
        }
    }
}

/// Background-scanned list of ROMs. Scan: recursive (depth ≤ 4), ROM (`ROM_EXTS`) and `.zip`
/// files ≤ 8 MiB, ≤ 5000 files, entries sorted by name, files that are not ROMs skipped. A
/// cache file in `config::data_dir()` (path, size, mtime → crc/system/mapper/region) makes
/// rescans fast. Cloning is cheap and shares the same list (the netplay guest lookup holds a
/// clone).
#[derive(Clone)]
pub struct Library {
    shared: Arc<Shared>,
}

impl Library {
    /// Starts scanning `dirs` on a background thread.
    pub fn new(dirs: Vec<PathBuf>) -> Library {
        Library::with_cache(dirs, crate::config::data_dir().join("library-cache.tsv"))
    }

    fn with_cache(dirs: Vec<PathBuf>, cache_path: PathBuf) -> Library {
        let lib = Library {
            shared: Arc::new(Shared {
                dirs,
                cache_path,
                entries: Mutex::new(Vec::new()),
                extra: Mutex::new(Vec::new()),
                state: Mutex::new(ScanState::default()),
                reads: AtomicUsize::new(0),
                generation: AtomicU64::new(0),
            }),
        };
        lib.rescan();
        lib
    }

    pub fn entries(&self) -> Vec<RomEntry> {
        lock(&self.shared.entries).clone()
    }

    /// Changes whenever `entries()` would return a different list.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    pub fn is_scanning(&self) -> bool {
        lock(&self.shared.state).scanning
    }

    pub fn rescan(&self) {
        let mut st = lock(&self.shared.state);
        if st.scanning {
            st.pending = true;
            return;
        }
        st.scanning = true;
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new().name("library-scan".into()).spawn(move || shared.scan_loop());
        if spawned.is_err() {
            st.scanning = false;
        }
    }

    /// Entry whose CRC-32 matches (netplay guest lookup).
    pub fn find_crc(&self, crc32: u32) -> Option<RomEntry> {
        lock(&self.shared.entries).iter().find(|e| e.crc32 == crc32).cloned()
    }

    /// Adds a file that is outside the scanned folders (drag and drop, command line).
    pub fn add_file(&self, path: &Path) -> Result<RomEntry, RomFileError> {
        let entry = self.shared.read_entry(path)?;
        {
            let mut extra = lock(&self.shared.extra);
            if !extra.iter().any(|e| e.path == entry.path) {
                extra.push(entry.clone());
            }
        }
        let mut entries = lock(&self.shared.entries);
        if !entries.iter().any(|e| e.crc32 == entry.crc32) {
            insert_sorted(&mut entries, entry.clone());
            self.shared.bump();
        }
        Ok(entry)
    }

    /// Drops the entry of a file that is gone (deleted from the game's menu) without waiting
    /// for a rescan.
    pub fn forget(&self, path: &Path) {
        lock(&self.shared.extra).retain(|e| e.path != path);
        let mut entries = lock(&self.shared.entries);
        let before = entries.len();
        entries.retain(|e| e.path != path);
        if entries.len() != before {
            self.shared.bump();
        }
    }
}

/// Locks `m`, recovering the data if a scan thread panicked while holding it: the guarded
/// lists stay structurally valid, so a poisoned lock must not take the UI down with it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};

    fn rom(mapper: u8, pal: bool, fill: u8) -> Vec<u8> {
        let mut b = vec![0u8; 16 + 16384 + 8192];
        b[..4].copy_from_slice(b"NES\x1A");
        b[4] = 1;
        b[5] = 1;
        b[6] = (mapper & 0x0F) << 4;
        b[7] = mapper & 0xF0;
        b[9] = pal as u8;
        b[16..].fill(fill);
        b
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ebobo-lib-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn wait(lib: &Library) {
        let start = Instant::now();
        while lib.is_scanning() {
            assert!(start.elapsed() < Duration::from_secs(10), "scan finishes");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn zip_with(path: &Path, inner: &str, bytes: &[u8]) {
        let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("readme.txt", opts).unwrap();
        z.write_all(b"hi").unwrap();
        z.start_file(inner, opts).unwrap();
        z.write_all(bytes).unwrap();
        z.finish().unwrap();
    }

    fn crc(bytes: &[u8]) -> u32 {
        ebobo_console::identify(bytes).unwrap().crc32
    }

    #[test]
    fn library_scan_dedup_and_cache() {
        let root = temp_dir("scan");
        let a = root.join("a");
        let b = root.join("b/deep/er");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let mario = rom(0, false, 1);
        let zelda = rom(1, true, 2);
        let dup = rom(4, false, 3);
        fs::write(a.join("Super Mario Bros. (World).nes"), &mario).unwrap();
        zip_with(&a.join("zelda.ZIP"), "roms/Legend of Zelda, The (Europe).NES", &zelda);
        fs::write(a.join("junk.nes"), b"not a rom at all").unwrap();
        fs::write(a.join("notes.txt"), b"ignore").unwrap();
        fs::write(a.join("zzz copy.nes"), &dup).unwrap();
        fs::write(b.join("aaa original.nes"), &dup).unwrap();
        let cache = root.join("cache/library-cache.tsv");

        let lib = Library::with_cache(vec![a.clone(), root.join("b")], cache.clone());
        wait(&lib);
        let entries = lib.entries();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Legend of Zelda, The (Europe)", "Super Mario Bros. (World)", "zzz copy"]);
        let zel = &entries[0];
        assert_eq!((zel.crc32, zel.mapper, zel.region, zel.supported), (crc(&zelda), 1, Region::Pal, true));
        assert_eq!(zel.path, a.join("zelda.ZIP"));
        let mar = &entries[1];
        assert_eq!((mar.crc32, mar.mapper, mar.region), (crc(&mario), 0, Region::Ntsc));
        // Duplicate CRC: the earlier directory's file wins.
        assert_eq!(lib.find_crc(crc(&dup)).unwrap().path, a.join("zzz copy.nes"));
        assert_eq!(load_rom_file(&a.join("zelda.ZIP")).unwrap(), zelda);
        let reads = lib.shared.reads.load(Ordering::Relaxed);
        assert_eq!(reads, 5, "mario, zelda, junk and both dups read once");

        // Rescan in the same library: everything comes from the cache.
        lib.rescan();
        wait(&lib);
        assert_eq!(lib.entries(), entries);
        assert_eq!(lib.shared.reads.load(Ordering::Relaxed), reads);

        // A fresh library reads the cache file; only the changed file is read again.
        fs::write(a.join("Super Mario Bros. (World).nes"), rom(2, false, 9)).unwrap();
        let lib2 = Library::with_cache(vec![a.clone(), root.join("b")], cache);
        wait(&lib2);
        assert_eq!(lib2.shared.reads.load(Ordering::Relaxed), 1);
        assert_eq!(lib2.entries().len(), 3);
        assert_eq!(lib2.find_crc(crc(&rom(2, false, 9))).unwrap().mapper, 2);

        // add_file: outside the folders, survives rescans.
        let outside = root.join("Dropped (USA).nes");
        fs::write(&outside, rom(7, false, 5)).unwrap();
        let before = lib2.generation();
        let added = lib2.add_file(&outside).unwrap();
        assert_eq!((added.name.as_str(), added.mapper), ("Dropped (USA)", 7));
        let after = lib2.generation();
        assert_ne!(after, before, "new entry bumps the generation");
        lib2.add_file(&outside).unwrap();
        assert_eq!(lib2.generation(), after, "no change, same generation");
        // A clone shares the live list (netplay guest lookup).
        let shared = lib2.clone();
        lib2.rescan();
        wait(&lib2);
        assert!(shared.find_crc(added.crc32).is_some());
        assert!(lib2.add_file(&a.join("junk.nes")).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn library_forgets_a_deleted_file_and_finds_its_copy() {
        let root = temp_dir("forget");
        let (first, copy, dropped) = (root.join("a.nes"), root.join("b/copy.nes"), std::env::temp_dir().join(format!("ebobo-lib-forget-drop-{}.nes", std::process::id())));
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(&first, rom(0, false, 1)).unwrap();
        fs::write(&copy, rom(0, false, 1)).unwrap();
        fs::write(&dropped, rom(1, false, 2)).unwrap();
        let lib = Library::with_cache(vec![root.clone()], root.join("cache.tsv"));
        wait(&lib);
        lib.add_file(&dropped).unwrap();
        let paths = |lib: &Library| lib.entries().into_iter().map(|e| e.path).collect::<Vec<_>>();
        assert_eq!(paths(&lib), [first.clone(), dropped.clone()], "one card per game");
        // The file is gone: its card goes at once, and the rescan shows the game's other file.
        fs::remove_file(&first).unwrap();
        let before = lib.generation();
        lib.forget(&first);
        assert_eq!(paths(&lib), [dropped.clone()]);
        assert_ne!(lib.generation(), before);
        lib.rescan();
        wait(&lib);
        assert_eq!(paths(&lib), [copy, dropped.clone()]);
        // A dropped file does not come back with the next rescan.
        fs::remove_file(&dropped).unwrap();
        lib.forget(&dropped);
        lib.rescan();
        wait(&lib);
        assert_eq!(lib.entries().len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn library_zip_without_nes_is_an_error() {
        let root = temp_dir("zip");
        let p = root.join("empty.zip");
        let mut z = zip::ZipWriter::new(fs::File::create(&p).unwrap());
        z.start_file("a.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.finish().unwrap();
        assert_eq!(load_rom_file(&p), Err(RomFileError::NoRomInZip));
        let _ = fs::remove_dir_all(&root);
    }

    fn set_mtime(path: &Path, unix_secs: u64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(unix_secs)).unwrap();
    }

    /// Two builds of one game (same file name, different CRC) in different folders, as after
    /// receiving a newer build from a netplay host: both are listed, the newer file first,
    /// whatever the folder order.
    #[test]
    fn library_same_name_builds_are_listed_newest_first() {
        let root = temp_dir("builds");
        let home = root.join("ROMs");
        let received = root.join("data/roms");
        let (old, new) = (rom(1, true, 1), rom(1, true, 2));
        let old_path = home.join("Chip 4P (Europe).nes");
        // Received ROMs sit one folder deep, `<data dir>/roms/<crc32>/<name>.nes`.
        let new_path = received.join(format!("{:08x}", crc(&new))).join("Chip 4P (Europe).nes");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::write(&old_path, &old).unwrap();
        fs::write(&new_path, &new).unwrap();
        fs::write(home.join("Zelda.nes"), rom(1, false, 3)).unwrap();
        fs::write(home.join("Abc.nes"), rom(1, false, 4)).unwrap();
        set_mtime(&old_path, 1_794_000_000);
        set_mtime(&new_path, 1_794_600_000);

        let lib = Library::with_cache(vec![home.clone(), received.clone()], root.join("cache.tsv"));
        wait(&lib);
        let got: Vec<(String, u32, u64)> = lib.entries().into_iter().map(|e| (e.name, e.crc32, e.mtime)).collect();
        let name = "Chip 4P (Europe)".to_string();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].0, "Abc");
        assert_eq!(got[1], (name.clone(), crc(&new), 1_794_600_000));
        assert_eq!(got[2], (name.clone(), crc(&old), 1_794_000_000));
        assert_eq!(got[3].0, "Zelda");

        // The order follows the file dates, not the folders: with the dates swapped (and the
        // cache rows stale) the home copy comes first.
        set_mtime(&old_path, 1_794_700_000);
        lib.rescan();
        wait(&lib);
        let paths: Vec<PathBuf> = lib.entries().into_iter().filter(|e| e.name == name).map(|e| e.path).collect();
        assert_eq!(paths, [old_path.clone(), new_path.clone()]);

        // A file added by hand (drag and drop) is dated and placed the same way.
        let dropped = root.join("dropped/Chip 4P (Europe).nes");
        fs::create_dir_all(dropped.parent().unwrap()).unwrap();
        fs::write(&dropped, rom(1, true, 5)).unwrap();
        set_mtime(&dropped, 1_794_650_000);
        assert_eq!(lib.add_file(&dropped).unwrap().mtime, 1_794_650_000);
        let paths: Vec<PathBuf> = lib.entries().into_iter().filter(|e| e.name == name).map(|e| e.path).collect();
        assert_eq!(paths, [old_path, dropped, new_path]);
        let _ = fs::remove_dir_all(&root);
    }

    /// A cache file's rows are trusted without reading the files (here the file is not a ROM
    /// at all); the entry's date comes from the file.
    #[test]
    fn library_cache_rows_are_used_without_reading_files() {
        let root = temp_dir("cacherows");
        let dir = root.join("roms");
        fs::create_dir_all(&dir).unwrap();
        // Not a ROM at all: the entry can only come from the cache row.
        let path = dir.join("whatever.nes");
        fs::write(&path, b"0123456789").unwrap();
        set_mtime(&path, 1_794_600_000);
        let gone = dir.join("gone.nes");
        let cache = root.join("library-cache.tsv");
        let text = format!(
            "{CACHE_HEADER}\n{}\t10\t1794600000000000000\t0123abcd\tnes\t4\tP\tCached Game (Europe)\n{}\t5\t7\t-\t-\t-\t-\t-\n",
            path.display(),
            gone.display()
        );
        fs::write(&cache, text).unwrap();

        let lib = Library::with_cache(vec![dir], cache);
        wait(&lib);
        assert_eq!(lib.shared.reads.load(Ordering::Relaxed), 0, "the cache row is used, the file is not read");
        assert_eq!(
            lib.entries(),
            [RomEntry {
                path,
                name: "Cached Game (Europe)".into(),
                system: System::Nes,
                crc32: 0x0123_abcd,
                mapper: 4,
                supported: ebobo_console::nes::SUPPORTED_MAPPERS.contains(&4),
                region: Region::Pal,
                mtime: 1_794_600_000,
            }]
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A cache file written by a build before the systems column (no header line, seven
    /// fields) is ignored: the file is read again and the cache is rewritten in today's format.
    #[test]
    fn library_cache_from_an_older_build_is_ignored() {
        let root = temp_dir("oldcache");
        let dir = root.join("roms");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("whatever.nes");
        fs::write(&path, b"0123456789").unwrap();
        set_mtime(&path, 1_794_600_000);
        let cache = root.join("library-cache.tsv");
        let old = format!("{}\t10\t1794600000000000000\t0123abcd\t4\tP\tCached Game (Europe)\n", path.display());
        fs::write(&cache, &old).unwrap();
        assert!(load_cache(&cache).is_empty());

        let lib = Library::with_cache(vec![dir], cache.clone());
        wait(&lib);
        assert_eq!(lib.shared.reads.load(Ordering::Relaxed), 1, "the old row is not trusted, the file is read");
        assert_eq!(lib.entries(), []);
        let written = fs::read_to_string(&cache).unwrap();
        assert_eq!(written, format!("{CACHE_HEADER}\n{}\t10\t1794600000000000000\t-\t-\t-\t-\t-\n", path.display()));
        assert_eq!(load_cache(&cache).len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    /// A system and a region other than the NES defaults survive the cache file.
    #[test]
    fn library_cache_keeps_system_and_region() {
        let root = temp_dir("cachecols");
        let cache = root.join("library-cache.tsv");
        let rom = CachedRom { name: "Sonic (Japan)".into(), system: System::MegaDrive, crc32: 0xdead_beef, mapper: 0, region: Region::Japan };
        let rows = HashMap::from([(root.join("Sonic.md"), CacheRow { size: 3, mtime: 9, rom: Some(rom) })]);
        save_cache(&cache, &rows);
        let back = load_cache(&cache);
        let row = &back[&root.join("Sonic.md")];
        let got = row.rom.as_ref().unwrap();
        assert_eq!((row.size, row.mtime), (3, 9));
        assert_eq!((got.name.as_str(), got.system, got.crc32, got.mapper, got.region), ("Sonic (Japan)", System::MegaDrive, 0xdead_beef, 0, Region::Japan));
        let _ = fs::remove_dir_all(&root);
    }

    /// Mega Drive images are picked up by any of their extensions and from a zip; copies of
    /// one game make one entry; a `.bin` that is something else is skipped without an error.
    #[test]
    fn library_lists_mega_drive_images() {
        let root = temp_dir("md");
        let mut image = vec![0u8; 0x400];
        image[0x100..0x110].copy_from_slice(b"SEGA MEGA DRIVE ");
        let id = ebobo_console::identify(&image).expect("a Mega Drive image");
        assert_eq!(id.system, System::MegaDrive);
        for name in ["Sonic.md", "Sonic.gen", "Sonic.SMD", "Sonic.bin"] {
            fs::write(root.join(name), &image).unwrap();
        }
        zip_with(&root.join("Sonic.zip"), "Sonic (World).md", &image);
        fs::write(root.join("firmware.bin"), b"something else entirely").unwrap();
        fs::write(root.join("Mario.nes"), rom(0, false, 1)).unwrap();
        assert_eq!(load_rom_file(&root.join("Sonic.zip")).unwrap(), image, "a zip gives its ROM entry whatever the system");

        let lib = Library::with_cache(vec![root.clone()], root.join("cache/library-cache.tsv"));
        wait(&lib);
        let entries = lib.entries();
        let systems: Vec<System> = entries.iter().map(|e| e.system).collect();
        assert_eq!(systems, [System::Nes, System::MegaDrive], "{entries:?}");
        assert!(entries[1].name.starts_with("Sonic"));
        assert!(entries[1].supported);
        assert_eq!(entries[1].crc32, id.crc32);
        assert_eq!(lib.shared.reads.load(Ordering::Relaxed), 7, "every file with a ROM extension is read once");
        // The answer "not a ROM" is cached like any other.
        lib.rescan();
        wait(&lib);
        assert_eq!(lib.shared.reads.load(Ordering::Relaxed), 7);
        assert!(lib.add_file(&root.join("firmware.bin")).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn library_default_dirs_cover_mega_drive() {
        let root = temp_dir("dirs");
        let (home, media) = (root.join("home"), root.join("media"));
        fs::create_dir_all(media.join("deck/card")).unwrap();
        let dirs = candidate_dirs(Some(&home), &media);
        for rel in ["Games/NES", "Games/Genesis", "Games/MegaDrive", "Emulation/roms/nes", "Emulation/roms/genesis", "Emulation/roms/megadrive"] {
            assert!(dirs.contains(&home.join(rel)), "{rel}");
        }
        for system in ["nes", "genesis", "megadrive"] {
            let rel = Path::new("Emulation/roms").join(system);
            assert!(dirs.contains(&media.join("deck").join(&rel)), "{system} on a card mounted at the first level");
            assert!(dirs.contains(&media.join("deck/card").join(&rel)), "{system} on a card mounted under the user");
        }
        // Without a home folder only the data folder and the cards are left.
        assert!(!candidate_dirs(None, &media).iter().any(|d| d.starts_with(&home)));
        let _ = fs::remove_dir_all(&root);
    }
}
