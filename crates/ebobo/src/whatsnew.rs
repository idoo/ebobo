//! The "what's new" box shown once after ebobo was updated.

use std::path::Path;

/// What changed in the version this build is, one short line per change: (Russian, English).
pub const NOTES: &[(&str, &str)] = &[
    ("Игры сразу при запуске", "Games first when ebobo opens"),
    ("Игра продолжается с того же места", "A game continues where you left"),
    ("По сети: несколько игроков с одного устройства", "Online: several players on one device"),
    ("В идущую сетевую игру можно войти и вернуться", "Join or come back to a running online game"),
    ("В комнате: кто может сохранять", "In the room: who may save"),
    ("Картинка в слоте сохранения", "A picture in each save slot"),
    ("Пауза, когда окно не впереди", "Pause while the window is behind"),
    ("Версия для Windows", "A Windows version"),
];

/// Files only an install that already ran writes: their presence without `seen-version`
/// means ebobo was updated from a version before this box existed.
const EARLIER_RUN: [&str; 3] = ["last-update", "shelf.tsv", "library-cache.tsv"];

/// Whether to show the notes now, and remembers `version` as seen.
pub fn due(data_dir: &Path, version: &str) -> bool {
    let seen = data_dir.join("seen-version");
    let due = match std::fs::read_to_string(&seen) {
        Ok(text) => text.trim() != version,
        // A brand-new install has nothing new to tell.
        Err(_) => EARLIER_RUN.iter().any(|name| data_dir.join(name).exists()),
    };
    // Not being able to remember only means the box shows again next time.
    let _ = std::fs::create_dir_all(data_dir).and_then(|()| std::fs::write(&seen, version));
    due
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// An empty directory of its own for one test.
    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-whatsnew-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn seen(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("seen-version")).unwrap()
    }

    #[test]
    fn whatsnew_same_version_is_quiet() {
        let d = dir("same");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("seen-version"), "0.5.0").unwrap();
        assert!(!due(&d, "0.5.0"));
        assert_eq!(seen(&d), "0.5.0");
    }

    #[test]
    fn whatsnew_after_update_shows_once() {
        let d = dir("update");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("seen-version"), "0.4.2").unwrap();
        assert!(due(&d, "0.5.0"));
        assert_eq!(seen(&d), "0.5.0");
        assert!(!due(&d, "0.5.0"), "only once");
    }

    #[test]
    fn whatsnew_update_from_before_the_box() {
        let d = dir("old");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("shelf.tsv"), "").unwrap();
        assert!(due(&d, "0.5.0"));
        assert_eq!(seen(&d), "0.5.0");
    }

    #[test]
    fn whatsnew_new_install_is_quiet() {
        let d = dir("fresh");
        assert!(!due(&d, "0.5.0"));
        assert_eq!(seen(&d), "0.5.0", "the data dir is created");
    }
}
