//! Getting rid of a game's file: into the system Trash where that works, else for good.
//! Owner: trash slice.
//!
//! On macOS the Trash is Finder's, reached through `NSFileManager`. Everywhere else it is a
//! freedesktop.org trash (Trash specification 1.0). A file is moved into one without copying,
//! so the trash has to be on the file's device: the home trash when the file is on the home
//! trash's device, else the trash at the top of the file's own volume (`.Trash/<uid>` or
//! `.Trash-<uid>`, as on a Steam Deck's SD card), which is the one file managers such as
//! Dolphin show. A file with no usable trash (a read-only card) can only be deleted for good,
//! and `plan` says so before the question is asked. `dispose` then does exactly what the
//! question said and never turns a move to the Trash into deleting for good.
//! The module has no dependencies on the rest of the crate, so it also compiles alone.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// What happens to a file that is disposed of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposal {
    /// It moves to the system Trash and can be put back from there.
    Trash,
    /// It is deleted for good.
    Permanent,
}

/// What `dispose` can do with `path` now, so the question can say it before asking: `Trash`
/// when there is a trash to move it into, else `Permanent`. Asking creates nothing.
pub fn plan(path: &Path) -> Disposal {
    #[cfg(target_os = "macos")]
    {
        let _ = path;
        Disposal::Trash
    }
    #[cfg(not(target_os = "macos"))]
    {
        match find_trash(path, home_trash().as_deref(), uid()) {
            Some(_) => Disposal::Trash,
            None => Disposal::Permanent,
        }
    }
}

/// Does `how` to the file, as the question said: `Trash` moves it into the trash `plan` finds
/// now and fails when there is none, `Permanent` deletes it for good. On an error the file is
/// still where it was; the message is for the user.
pub fn dispose(path: &Path, how: Disposal) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        match how {
            Disposal::Trash => mac::trash(path),
            Disposal::Permanent => fs::remove_file(path).map_err(|e| e.to_string()),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let place = match how {
            Disposal::Trash => find_trash(path, home_trash().as_deref(), uid()),
            Disposal::Permanent => None,
        };
        dispose_with(path, how, place)
    }
}

/// A freedesktop.org trash folder to move a file into.
#[derive(Debug, PartialEq, Eq)]
struct Place {
    /// The folder holding `files/` and `info/`.
    dir: PathBuf,
    /// The top directory of the volume for a trash on the file's own volume, which the
    /// record's `Path=` is relative to; `None` for the home trash, whose paths are absolute.
    top: Option<PathBuf>,
}

/// Does `how` to `path` with `place` as the trash: `Trash` without a place is an error and the
/// file stays, never a deletion for good.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn dispose_with(path: &Path, how: Disposal, place: Option<Place>) -> Result<(), String> {
    match (how, place) {
        (Disposal::Permanent, _) => fs::remove_file(path).map_err(|e| e.to_string()),
        (Disposal::Trash, Some(place)) => trash_into(path, &place.dir, place.top.as_deref()),
        (Disposal::Trash, None) => {
            Err(format!("there is no trash on the disk of {}, so it was left in place", path.display()))
        }
    }
}

/// The trash `path` would go to: the home trash `home` when the file is on its device, else
/// the trash at the top of the file's own volume for user `uid`. `None` when there is no
/// usable one (or the file is gone); nothing is created.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn find_trash(path: &Path, home: Option<&Path>, uid: Option<u32>) -> Option<Place> {
    device(path)?;
    if let Some(home) = home.filter(|home| plan_in(path, home) == Disposal::Trash) {
        return Some(Place { dir: home.to_path_buf(), top: None });
    }
    let top = volume_top(path)?;
    let dir = volume_trash(&top, uid?)?;
    Some(Place { dir, top: Some(top) })
}

/// The user's freedesktop.org home trash: `$XDG_DATA_HOME/Trash`, else `~/.local/share/Trash`.
/// `None` when neither variable names a place, and then there is no trash to move into.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn home_trash() -> Option<PathBuf> {
    let absolute = |name: &str| {
        std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute())
    };
    let data = absolute("XDG_DATA_HOME").or_else(|| Some(absolute("HOME")?.join(".local/share")))?;
    Some(data.join("Trash"))
}

/// The number of the device a file sits on; `None` when it cannot be asked (no such file, or a
/// platform without device numbers).
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn device(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Whether a file can be moved into the trash without copying: both devices are known and are
/// the same one. Anything else means that trash cannot take the file.
fn same_device(file: Option<u64>, trash: Option<u64>) -> bool {
    matches!((file, trash), (Some(a), Some(b)) if a == b)
}

/// Whether the home trash `trash` can take `path` (`Trash`) or not (`Permanent`, unless the
/// file's own volume has a trash). A trash folder that does not exist yet is judged by its
/// nearest ancestor that does, since that is where it will be made.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn plan_in(path: &Path, trash: &Path) -> Disposal {
    let trash_device = trash.ancestors().find_map(device);
    if same_device(device(path), trash_device) { Disposal::Trash } else { Disposal::Permanent }
}

/// The top directory of the volume `path` is on: the highest ancestor of its folder that is
/// still on the same device. `None` when the folder cannot be found or asked.
fn volume_top(path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    let mut top = fs::canonicalize(absolute.parent()?).ok()?;
    let own = device(&top)?;
    while let Some(parent) = top.parent() {
        if device(parent) != Some(own) {
            break;
        }
        top = parent.to_path_buf();
    }
    Some(top)
}

/// The trash of user `uid` on the volume whose top directory is `top`, as the specification
/// orders them: `<top>/.Trash/<uid>` when `<top>/.Trash` is a real directory with the sticky
/// bit, else `<top>/.Trash-<uid>`; each only when `usable`. Nothing is created.
fn volume_trash(top: &Path, uid: u32) -> Option<PathBuf> {
    let shared = top.join(".Trash");
    if real_dir(&shared) && sticky(&shared) {
        let own = shared.join(uid.to_string());
        if usable(&own) {
            return Some(own);
        }
    }
    let own = top.join(format!(".Trash-{uid}"));
    usable(&own).then_some(own)
}

/// Whether a trash folder can take files: it is a real, writable directory, or it is missing
/// and its parent is one, so that it can be made at the moment of trashing.
fn usable(dir: &Path) -> bool {
    match fs::symlink_metadata(dir) {
        Ok(meta) => meta.is_dir() && writable(dir),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            dir.parent().is_some_and(|parent| real_dir(parent) && writable(parent))
        }
        Err(_) => false,
    }
}

/// Whether `path` is a directory itself, not a symlink to one.
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Whether the directory has the sticky bit, which a shared `.Trash` must have.
fn sticky(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(dir).is_ok_and(|meta| meta.permissions().mode() & 0o1000 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

#[cfg(unix)]
mod sys {
    use std::ffi::{c_char, c_int};

    unsafe extern "C" {
        pub fn getuid() -> u32;
        pub fn access(path: *const c_char, mode: c_int) -> c_int;
    }
}

/// The user's id, which names their trash on a volume; `None` off unix.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getuid takes nothing and cannot fail.
        Some(unsafe { sys::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Whether this user may create and remove entries in the directory: false on a read-only
/// volume too.
fn writable(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        const X_OK: std::ffi::c_int = 1;
        const W_OK: std::ffi::c_int = 2;
        let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a valid NUL-terminated path that outlives the call.
        unsafe { sys::access(c_path.as_ptr(), W_OK | X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        fs::metadata(dir).is_ok_and(|meta| !meta.permissions().readonly())
    }
}

/// Makes a volume trash folder with mode 0700 when it is missing; one that is there must be a
/// real directory.
fn make_own_dir(dir: &Path) -> Result<(), String> {
    if real_dir(dir) {
        return Ok(());
    }
    if fs::symlink_metadata(dir).is_ok() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match builder.create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists && real_dir(dir) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// `original` as a path relative to the volume's top directory `top`, for a volume trash's
/// record. `top` is a resolved path, so the file's folder is resolved too when needed.
fn relative_to(original: &Path, top: &Path) -> Result<PathBuf, String> {
    if let Ok(relative) = original.strip_prefix(top) {
        return Ok(relative.to_path_buf());
    }
    let resolved = original
        .parent()
        .and_then(|folder| fs::canonicalize(folder).ok())
        .zip(original.file_name())
        .map(|(folder, name)| folder.join(name));
    resolved
        .as_deref()
        .and_then(|path| path.strip_prefix(top).ok())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("{} is not under {}", original.display(), top.display()))
}

/// The `n`th name to try for a file in the trash: the name itself, then `<stem> 2.<ext>`,
/// `<stem> 3.<ext>` and so on, so that games of one name lie side by side.
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} {n}.{ext}"),
        _ => format!("{name} {n}"),
    }
}

/// A path as the trash's info file wants it: every byte outside the unreserved set as `%XX`,
/// slashes kept.
fn percent_encode(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let mut out = String::with_capacity(bytes.len());
    for b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether `s` reads `YYYY-MM-DDThh:mm:ss`.
fn is_date_shape(s: &str) -> bool {
    s.len() == 19
        && s.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            10 => b == b'T',
            13 | 16 => b == b':',
            _ => b.is_ascii_digit(),
        })
}

/// Seconds since 1970 as a UTC date and time, `YYYY-MM-DDThh:mm:ss`.
fn utc_date(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a calendar date, counting eras of 400 years from 0000-03-01.
    let z = days + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// The moment of deletion for the info file. The trash wants local time, which only the
/// system knows how to work out, so the `date` command is asked; without it the time is UTC.
fn deletion_date() -> String {
    let local = std::process::Command::new("date")
        .arg("+%Y-%m-%dT%H:%M:%S")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| is_date_shape(s));
    local.unwrap_or_else(|| {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        utc_date(now.map_or(0, |d| d.as_secs()))
    })
}

/// Moves `path` into the freedesktop.org trash folder `trash`: a record in `info/` saying where
/// the file was and when it went, then the file itself into `files/` under the record's name.
/// `top` is the volume's top directory for a trash on the file's own volume: the folder is
/// then made with mode 0700 when missing, and the record's path is relative to `top`; for the
/// home trash (`None`) it is absolute.
/// The record is made first and with `create_new`, which is what claims the name, so two
/// files of one name never overwrite each other. On an error the file has not moved and no
/// record of it is left.
fn trash_into(path: &Path, trash: &Path, top: Option<&Path>) -> Result<(), String> {
    let text = |e: std::io::Error| e.to_string();
    fs::symlink_metadata(path).map_err(text)?;
    let original = std::path::absolute(path).map_err(text)?;
    let name = original
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{} is not a file", original.display()))?;
    let recorded = match top {
        Some(top) => {
            let relative = relative_to(&original, top)?;
            make_own_dir(trash)?;
            relative
        }
        None => original.clone(),
    };
    let (files, info) = (trash.join("files"), trash.join("info"));
    fs::create_dir_all(&files).map_err(text)?;
    fs::create_dir_all(&info).map_err(text)?;

    let mut n = 1;
    let (taken, record_path, mut record) = loop {
        let candidate = numbered(&name, n);
        n += 1;
        if fs::symlink_metadata(files.join(&candidate)).is_ok() {
            continue;
        }
        let record_path = info.join(format!("{candidate}.trashinfo"));
        match fs::OpenOptions::new().write(true).create_new(true).open(&record_path) {
            Ok(record) => break (candidate, record_path, record),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(text(e)),
        }
    };
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(&recorded),
        deletion_date()
    );
    let moved = record
        .write_all(body.as_bytes())
        .and_then(|()| fs::rename(&original, files.join(&taken)));
    drop(record);
    moved.map_err(|e| {
        let _ = fs::remove_file(&record_path);
        text(e)
    })
}

/// Finder's Trash, through the Objective-C runtime: the one way on macOS that lets the user
/// put the file back from the Trash afterwards.
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::mem::transmute;
    use std::path::Path;
    use std::ptr;

    type Id = *mut c_void;
    type Sel = *const c_void;

    #[link(name = "Foundation", kind = "framework")]
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        /// Has no signature of its own: each call casts it to the method's exact type.
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    fn class(name: &CStr) -> Id {
        unsafe { objc_getClass(name.as_ptr()) }
    }

    fn sel(name: &CStr) -> Sel {
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// Moves the file to the Trash; the error is the system's own sentence about what went
    /// wrong, in the user's language.
    pub fn trash(path: &Path) -> Result<(), String> {
        let unusable = || format!("{} is not a name the Trash can take", path.display());
        let c_path = path.to_str().and_then(|s| CString::new(s).ok()).ok_or_else(unusable)?;
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let result = trash_in_pool(&c_path).map_err(|e| e.unwrap_or_else(unusable));
            objc_autoreleasePoolPop(pool);
            result
        }
    }

    /// The calls themselves, inside the caller's autorelease pool. `Err(None)` means the path
    /// could not be made into a URL; `Err(Some(_))` carries the system's message, copied out
    /// because the error object dies with the pool.
    unsafe fn trash_in_pool(path: &CStr) -> Result<(), Option<String>> {
        unsafe {
            let msg = objc_msgSend as unsafe extern "C" fn();
            let get: unsafe extern "C" fn(Id, Sel) -> Id = transmute(msg);
            let from_c_string: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id = transmute(msg);
            let from_object: unsafe extern "C" fn(Id, Sel, Id) -> Id = transmute(msg);
            let trash_item: unsafe extern "C" fn(Id, Sel, Id, *mut Id, *mut Id) -> u8 =
                transmute(msg);
            let c_string: unsafe extern "C" fn(Id, Sel) -> *const c_char = transmute(msg);

            let string = from_c_string(class(c"NSString"), sel(c"stringWithUTF8String:"), path.as_ptr());
            if string.is_null() {
                return Err(None);
            }
            let url = from_object(class(c"NSURL"), sel(c"fileURLWithPath:"), string);
            if url.is_null() {
                return Err(None);
            }
            let manager = get(class(c"NSFileManager"), sel(c"defaultManager"));
            let mut error: Id = ptr::null_mut();
            let done = trash_item(
                manager,
                sel(c"trashItemAtURL:resultingItemURL:error:"),
                url,
                ptr::null_mut(),
                &mut error,
            );
            if done != 0 {
                return Ok(());
            }
            if error.is_null() {
                return Err(Some("The file could not be moved to the Trash".to_string()));
            }
            let description = get(error, sel(c"localizedDescription"));
            let utf8 = if description.is_null() {
                ptr::null()
            } else {
                c_string(description, sel(c"UTF8String"))
            };
            if utf8.is_null() {
                return Err(Some("The file could not be moved to the Trash".to_string()));
            }
            Err(Some(CStr::from_ptr(utf8).to_string_lossy().into_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of the test's own, empty at the start.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-trash-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn trashed_file_moves_with_its_record() {
        let dir = scratch("record");
        let trash = dir.join("Trash");
        let rom = dir.join("my игра.nes");
        fs::write(&rom, b"NES\x1a rom bytes").unwrap();

        trash_into(&rom, &trash, None).unwrap();

        assert!(!rom.exists(), "the file is still in its place");
        assert_eq!(fs::read(trash.join("files/my игра.nes")).unwrap(), b"NES\x1a rom bytes");
        let record = fs::read_to_string(trash.join("info/my игра.nes.trashinfo")).unwrap();
        let lines: Vec<&str> = record.lines().collect();
        assert_eq!(lines.len(), 3, "{record}");
        assert_eq!(lines[0], "[Trash Info]");
        let path = lines[1].strip_prefix("Path=").expect("a Path line");
        assert!(path.starts_with('/'), "{path}");
        assert!(path.ends_with("/my%20%D0%B8%D0%B3%D1%80%D0%B0.nes"), "{path}");
        assert!(path.bytes().all(|b| b.is_ascii_graphic()), "{path}");
        let date = lines[2].strip_prefix("DeletionDate=").expect("a DeletionDate line").as_bytes();
        assert_eq!(date.len(), 19, "{}", lines[2]);
        for (i, &b) in date.iter().enumerate() {
            let fits = match i {
                4 | 7 => b == b'-',
                10 => b == b'T',
                13 | 16 => b == b':',
                _ => b.is_ascii_digit(),
            };
            assert!(fits, "{} at {i}", lines[2]);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_name_gets_a_number() {
        let dir = scratch("number");
        let trash = dir.join("Trash");
        for (folder, bytes) in [("a", b"first"), ("b", b"secnd"), ("c", b"third")] {
            fs::create_dir_all(dir.join(folder)).unwrap();
            let rom = dir.join(folder).join("game.nes");
            fs::write(&rom, bytes).unwrap();
            trash_into(&rom, &trash, None).unwrap();
            assert!(!rom.exists());
        }

        assert_eq!(fs::read(trash.join("files/game.nes")).unwrap(), b"first");
        assert_eq!(fs::read(trash.join("files/game 2.nes")).unwrap(), b"secnd");
        assert_eq!(fs::read(trash.join("files/game 3.nes")).unwrap(), b"third");
        let second = fs::read_to_string(trash.join("info/game 2.nes.trashinfo")).unwrap();
        assert!(second.contains("/b/game.nes\n"), "{second}");
        assert!(trash.join("info/game.nes.trashinfo").exists());
        assert!(trash.join("info/game 3.nes.trashinfo").exists());
        assert_eq!(numbered("README", 2), "README 2");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_leaves_no_record() {
        let dir = scratch("missing");
        let trash = dir.join("Trash");

        let result = trash_into(&dir.join("gone.nes"), &trash, None);

        assert!(result.is_err());
        let records = fs::read_dir(trash.join("info")).map_or(0, |entries| entries.count());
        assert_eq!(records, 0, "a record of a file that never moved");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_file_on_the_trash_device_is_trashed() {
        assert!(same_device(Some(7), Some(7)));
        assert!(!same_device(Some(7), Some(8)), "an SD card is another device");
        assert!(!same_device(None, Some(7)));
        assert!(!same_device(Some(7), None));
        assert!(!same_device(None, None));
    }

    #[cfg(unix)]
    #[test]
    fn trash_not_made_yet_is_judged_by_its_parent() {
        let dir = scratch("plan");
        let rom = dir.join("game.nes");
        fs::write(&rom, b"x").unwrap();

        assert_eq!(plan_in(&rom, &dir.join("share/Trash")), Disposal::Trash);
        assert_eq!(plan_in(&dir.join("gone.nes"), &dir.join("share/Trash")), Disposal::Permanent);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A trash of the test's own on a "card" whose top directory is the scratch folder.
    fn card(test: &str) -> PathBuf {
        fs::canonicalize(scratch(test)).unwrap()
    }

    #[test]
    fn card_trash_takes_the_file_with_a_relative_path() {
        let top = card("card");
        fs::create_dir_all(top.join("roms")).unwrap();
        let rom = top.join("roms/My Game (U).nes");
        fs::write(&rom, b"card bytes").unwrap();
        let uid = uid().unwrap_or(1000);
        let own = top.join(format!(".Trash-{uid}"));

        assert_eq!(volume_trash(&top, uid), Some(own.clone()));
        assert!(!own.exists(), "asking made the trash");
        let place = Place { dir: own.clone(), top: Some(top.clone()) };
        dispose_with(&rom, Disposal::Trash, Some(place)).unwrap();

        assert!(!rom.exists(), "the file is still in its place");
        assert_eq!(fs::read(own.join("files/My Game (U).nes")).unwrap(), b"card bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&own).unwrap().permissions().mode() & 0o7777, 0o700);
        }
        let record = fs::read_to_string(own.join("info/My Game (U).nes.trashinfo")).unwrap();
        assert!(record.contains("\nPath=roms/My%20Game%20%28U%29.nes\n"), "{record}");
        let _ = fs::remove_dir_all(&top);
    }

    #[cfg(unix)]
    #[test]
    fn shared_trash_needs_a_sticky_real_directory() {
        use std::os::unix::fs::PermissionsExt;
        let top = card("sticky");
        let shared = top.join(".Trash");
        let fallback = top.join(".Trash-1000");
        fs::create_dir(&shared).unwrap();

        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(volume_trash(&top, 1000), Some(fallback.clone()), "not sticky");

        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(volume_trash(&top, 1000), Some(shared.join("1000")));
        assert!(!shared.join("1000").exists(), "asking made the trash");
        let rom = top.join("game.nes");
        fs::write(&rom, b"x").unwrap();
        let place = Place { dir: shared.join("1000"), top: Some(top.clone()) };
        dispose_with(&rom, Disposal::Trash, Some(place)).unwrap();
        assert!(shared.join("1000/files/game.nes").exists());
        let mode = fs::metadata(shared.join("1000")).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);

        // A symlinked `.Trash` is refused even when it points at a sticky directory.
        let elsewhere = top.join("elsewhere");
        fs::rename(&shared, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &shared).unwrap();
        assert_eq!(volume_trash(&top, 1000), Some(fallback), "symlinked");
        let _ = fs::remove_dir_all(&top);
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_top_has_no_trash() {
        use std::os::unix::fs::PermissionsExt;
        if uid() == Some(0) {
            return; // root may write anywhere
        }
        let top = card("readonly");
        fs::set_permissions(&top, fs::Permissions::from_mode(0o555)).unwrap();
        let found = volume_trash(&top, 1000);
        fs::set_permissions(&top, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(found, None);
        let _ = fs::remove_dir_all(&top);
    }

    #[test]
    fn trash_without_a_trash_leaves_the_file() {
        let top = card("notrash");
        let rom = top.join("game.nes");
        fs::write(&rom, b"x").unwrap();

        assert!(dispose_with(&rom, Disposal::Trash, None).is_err());
        assert!(rom.exists(), "deleted although the Trash was promised");
        let blocker = top.join(".Trash-1000");
        fs::write(&blocker, b"not a folder").unwrap();
        let place = Place { dir: blocker, top: Some(top.clone()) };
        assert!(dispose_with(&rom, Disposal::Trash, Some(place)).is_err());
        assert!(rom.exists(), "deleted although the Trash was promised");

        dispose_with(&rom, Disposal::Permanent, None).unwrap();
        assert!(!rom.exists());
        let _ = fs::remove_dir_all(&top);
    }

    #[cfg(unix)]
    #[test]
    fn volume_top_is_the_highest_folder_on_the_device() {
        let dir = card("top");
        let rom = dir.join("game.nes");
        fs::write(&rom, b"x").unwrap();

        let top = volume_top(&rom).unwrap();
        assert!(dir.starts_with(&top), "{} is not above {}", top.display(), dir.display());
        assert_eq!(device(&top), device(&rom));
        assert!(top.parent().is_none_or(|parent| device(parent) != device(&rom)));
        assert_eq!(volume_top(&dir.join("gone/game.nes")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn utc_fallback_reads_the_calendar() {
        assert_eq!(utc_date(0), "1970-01-01T00:00:00");
        assert_eq!(utc_date(951_782_400), "2000-02-29T00:00:00");
        assert_eq!(utc_date(1_700_000_000), "2023-11-14T22:13:20");
        assert!(is_date_shape(&deletion_date()));
    }

    /// Moves a real file into the real Trash of whoever runs it, so it is run by hand.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn system_trash_takes_a_file() {
        let rom = std::env::temp_dir().join("ebobo-trash-test.txt");
        fs::write(&rom, b"ebobo trash test").unwrap();

        assert_eq!(plan(&rom), Disposal::Trash);
        let result = dispose(&rom, Disposal::Trash);
        println!("dispose: {result:?}");
        assert_eq!(result, Ok(()));
        assert!(!rom.exists(), "the file is still in its place");
    }
}
