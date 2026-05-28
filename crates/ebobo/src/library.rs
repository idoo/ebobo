//! ROM library: folder scan, CRC index, .nes/.zip loading. Owner: canvas slice.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use ebobo_core::Region;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RomEntry {
    pub path: PathBuf,
    /// Display name: file stem without extension (zip: inner .nes stem).
    pub name: String,
    /// PRG+CHR CRC-32 (`RomInfo::crc32`), the netplay identity.
    pub crc32: u32,
    pub mapper: u16,
    /// Mapper is in `ebobo_core::SUPPORTED_MAPPERS`.
    pub supported: bool,
    pub region: Region,
}

const MAX_ROM_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 4;
const MAX_FILES: usize = 5000;

/// Folders scanned by default: ~/ROMs, ~/roms, ~/Games/NES, ~/Documents/ROMs, ~/Downloads,
/// `config::data_dir()/roms`, ~/Emulation/roms/nes and every `/run/media/**/Emulation/roms/nes`
/// (EmuDeck on Steam Deck, internal and SD card).
pub fn default_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in ["ROMs", "roms", "Games/NES", "Documents/ROMs", "Downloads"] {
            dirs.push(home.join(rel));
        }
        dirs.push(crate::config::data_dir().join("roms"));
        dirs.push(home.join("Emulation/roms/nes"));
    } else {
        dirs.push(crate::config::data_dir().join("roms"));
    }
    // /run/media/<user>/<card>/Emulation/roms/nes (and /run/media/<card>/... on some setups).
    let tail = Path::new("Emulation/roms/nes");
    for level1 in subdirs(Path::new("/run/media")) {
        dirs.push(level1.join(tail));
        for level2 in subdirs(&level1) {
            dirs.push(level2.join(tail));
        }
    }
    let mut seen = HashSet::new();
    dirs.retain(|d| d.is_dir() && seen.insert(d.clone()));
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

/// Reads the ROM bytes and, for a zip, the stem of the `.nes` entry inside.
fn read_rom(path: &Path) -> Result<(Vec<u8>, Option<String>), String> {
    let shown = path.display();
    let meta = fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if meta.len() > MAX_ROM_BYTES {
        return Err(format!("{shown}: file is larger than 8 MiB"));
    }
    match ext_lower(path).as_deref() {
        Some("zip") => {
            let file = fs::File::open(path).map_err(|e| format!("{shown}: {e}"))?;
            let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("{shown}: {e}"))?;
            for i in 0..zip.len() {
                let mut entry = zip.by_index(i).map_err(|e| format!("{shown}: {e}"))?;
                let name = entry.name().to_string();
                if !entry.is_file() || !name.to_ascii_lowercase().ends_with(".nes") {
                    continue;
                }
                if entry.size() > MAX_ROM_BYTES {
                    return Err(format!("{shown}: {name} is larger than 8 MiB"));
                }
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                (&mut entry)
                    .take(MAX_ROM_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("{shown}: {name}: {e}"))?;
                if bytes.len() as u64 > MAX_ROM_BYTES {
                    return Err(format!("{shown}: {name} is larger than 8 MiB"));
                }
                let inner = name.rsplit(['/', '\\']).next().unwrap_or(&name);
                return Ok((bytes, Some(stem(Path::new(inner)))));
            }
            Err(format!("{shown}: no .nes file inside the zip"))
        }
        _ => fs::read(path).map(|b| (b, None)).map_err(|e| format!("{shown}: {e}")),
    }
}

/// Reads a ROM file: `.nes` as is, `.zip` = first `.nes` entry inside.
pub fn load_rom_file(path: &Path) -> Result<Vec<u8>, String> {
    read_rom(path).map(|(bytes, _)| bytes)
}

fn make_entry(path: &Path, bytes: &[u8], inner_stem: Option<String>) -> Result<RomEntry, String> {
    let info = ebobo_core::cart::parse(bytes).map_err(|e| format!("{}: {e:?}", path.display()))?;
    Ok(RomEntry {
        path: path.to_path_buf(),
        name: inner_stem.unwrap_or_else(|| stem(path)),
        crc32: info.crc32,
        mapper: info.mapper,
        supported: ebobo_core::SUPPORTED_MAPPERS.contains(&info.mapper),
        region: info.region,
    })
}

fn sort_key(e: &RomEntry) -> (String, &Path) {
    (e.name.to_lowercase(), e.path.as_path())
}

/// Inserts keeping the list sorted by lower-cased name (then path).
fn insert_sorted(list: &mut Vec<RomEntry>, entry: RomEntry) {
    let key = sort_key(&entry);
    let at = list.partition_point(|e| sort_key(e) < key);
    drop(key);
    list.insert(at, entry);
}

/// One cache row: file identity (size + mtime) → parse result (`None` = not a ROM).
#[derive(Clone)]
struct CacheRow {
    size: u64,
    mtime: u128,
    rom: Option<(String, u32, u16, Region)>,
}

fn load_cache(path: &Path) -> HashMap<PathBuf, CacheRow> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(path) else { return map };
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            continue;
        }
        let (Ok(size), Ok(mtime)) = (f[1].parse(), f[2].parse()) else { continue };
        let rom = if f[3] == "-" {
            None
        } else {
            let (Ok(crc), Ok(mapper)) = (u32::from_str_radix(f[3], 16), f[4].parse()) else { continue };
            let region = if f[5] == "P" { Region::Pal } else { Region::Ntsc };
            Some((f[6].to_string(), crc, mapper, region))
        };
        map.insert(PathBuf::from(f[0]), CacheRow { size, mtime, rom });
    }
    map
}

fn save_cache(path: &Path, rows: &HashMap<PathBuf, CacheRow>) {
    let mut text = String::new();
    for (p, row) in rows {
        let Some(ps) = p.to_str() else { continue };
        let clean = |s: &str| !s.contains(['\t', '\n', '\r']);
        if !clean(ps) || row.rom.as_ref().is_some_and(|r| !clean(&r.0)) {
            continue;
        }
        match &row.rom {
            Some((name, crc, mapper, region)) => {
                let r = if *region == Region::Pal { "P" } else { "N" };
                text.push_str(&format!("{ps}\t{}\t{}\t{crc:08x}\t{mapper}\t{r}\t{name}\n", row.size, row.mtime));
            }
            None => text.push_str(&format!("{ps}\t{}\t{}\t-\t-\t-\t-\n", row.size, row.mtime)),
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

    fn read_entry(&self, path: &Path) -> Result<RomEntry, String> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let (bytes, inner) = read_rom(path)?;
        make_entry(path, &bytes, inner)
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
                        rom: self.read_entry(&path).ok().map(|e| (e.name, e.crc32, e.mapper, e.region)),
                    },
                };
                if let Some((name, crc32, mapper, region)) = row.rom.clone() {
                    let supported = ebobo_core::SUPPORTED_MAPPERS.contains(&mapper);
                    add(RomEntry { path: path.clone(), name, crc32, mapper, supported, region }, &mut found);
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

/// Collects `.nes`/`.zip` files ≤ 8 MiB under `dir` in name order, depth-first.
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
            && matches!(ext_lower(&path).as_deref(), Some("nes" | "zip"))
            && meta.len() <= MAX_ROM_BYTES
        {
            *budget -= 1;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            out.push((path, meta.len(), mtime));
        }
    }
}

/// Background-scanned list of ROMs. Scan: recursive (depth ≤ 4), `.nes` and `.zip` files
/// ≤ 8 MiB, ≤ 5000 files, entries sorted by name, unparseable files skipped. A cache file in
/// `config::data_dir()` (path, size, mtime → crc/mapper/region) makes rescans fast. Cloning is
/// cheap and shares the same list (the netplay guest lookup holds a clone).
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
    pub fn add_file(&self, path: &Path) -> Result<RomEntry, String> {
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
        ebobo_core::cart::parse(bytes).unwrap().crc32
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
    fn library_zip_without_nes_is_an_error() {
        let root = temp_dir("zip");
        let p = root.join("empty.zip");
        let mut z = zip::ZipWriter::new(fs::File::create(&p).unwrap());
        z.start_file("a.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.finish().unwrap();
        assert!(load_rom_file(&p).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
