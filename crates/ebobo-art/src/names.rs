//! The names database: which game a CRC is, and where its pictures are. Owner: names slice.
//!
//! The database is the No-Intro list as libretro publishes it (one text file per system in
//! `libretro/libretro-database`, `metadat/no-intro/`), and the pictures are the libretro
//! thumbnails collection, named after the same game names.

use crate::System;

/// CRC-32 → proper game name, for one system.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    /// Sorted by CRC, one entry per CRC.
    entries: Vec<(u32, Box<str>)>,
}

impl Names {
    /// Reads a database file as downloaded from [`database_url`]. Entries it cannot read are
    /// skipped; garbage gives an empty list.
    ///
    /// Every `rom ( … crc XXXXXXXX … )` line of a `game ( … )` block gives one entry under the
    /// block's `name`: a NES game has both the CRC with the iNES header and the one without.
    /// When two games share a CRC the first one in the file wins.
    pub fn parse(text: &str) -> Names {
        let mut entries: Vec<(u32, Box<str>)> = Vec::new();
        // The name of the block being read; `None` outside a block and before its `name` line.
        let mut game: Option<String> = None;
        for line in text.lines() {
            let line = line.trim();
            if line == ")" || line.starts_with("game (") {
                game = None;
            } else if let Some(rest) = line.strip_prefix("name \"") {
                game = quoted(rest).map(|(name, _)| name);
            } else if let Some(rest) = line.strip_prefix("rom (") {
                let Some(name) = &game else { continue };
                // The file name of the rom is skipped as a quoted string: it may hold brackets
                // and the word "crc".
                let rest = match rest.trim_start().strip_prefix("name \"") {
                    Some(after) => match quoted(after) {
                        Some((_, tail)) => tail,
                        None => continue,
                    },
                    None => rest,
                };
                let mut tokens = rest.split_ascii_whitespace();
                while let Some(token) = tokens.next() {
                    if token == "crc" {
                        if let Some(crc) = tokens.next().and_then(parse_crc) {
                            entries.push((crc, name.as_str().into()));
                        }
                        break;
                    }
                }
            }
        }
        Names::from_entries(entries)
    }

    /// Reads the compact form [`Names::to_cache`] wrote.
    pub fn from_cache(text: &str) -> Names {
        let entries = text
            .lines()
            .filter_map(|line| {
                let (crc, name) = line.split_once('\t')?;
                let name = name.trim_end_matches('\r');
                if name.is_empty() {
                    return None;
                }
                Some((parse_crc(crc)?, name.into()))
            })
            .collect();
        Names::from_entries(entries)
    }

    /// A compact text form for the disk cache: one `crc<TAB>name` line per game.
    pub fn to_cache(&self) -> String {
        let mut out = String::with_capacity(self.entries.iter().map(|(_, name)| name.len() + 10).sum());
        for (crc, name) in &self.entries {
            out.push_str(&format!("{crc:08X}\t{name}\n"));
        }
        out
    }

    /// The game with this CRC-32 (as the library computes it: without a copier or iNES
    /// header).
    pub fn get(&self, crc32: u32) -> Option<&str> {
        let at = self.entries.binary_search_by_key(&crc32, |(crc, _)| *crc).ok()?;
        Some(&self.entries[at].1)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sorts by CRC; of the entries with one CRC the first in `entries` stays.
    fn from_entries(mut entries: Vec<(u32, Box<str>)>) -> Names {
        entries.sort_by_key(|(crc, _)| *crc);
        entries.dedup_by_key(|(crc, _)| *crc);
        Names { entries }
    }
}

/// A CRC as the database writes it: one to eight hex digits.
fn parse_crc(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 8 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

/// Reads a quoted string whose opening quote was just consumed: the string (a backslash
/// keeps the character after it) and the text after the closing quote. `None` without a
/// closing quote or for an empty string.
fn quoted(text: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return if out.is_empty() { None } else { Some((out, &text[at + 1..])) },
            '\\' => out.push(chars.next()?.1),
            _ => out.push(c),
        }
    }
    None
}

/// The name of a system in the libretro database and thumbnails collection.
fn system_dir(system: System) -> &'static str {
    match system {
        System::Nes => "Nintendo - Nintendo Entertainment System",
        System::MegaDrive => "Sega - Mega Drive - Genesis",
    }
}

/// Appends `text` as one URL path segment: letters, digits and `-._~!'()*` stay, every other
/// byte of the UTF-8 form becomes `%XX`.
fn percent_encode(out: &mut String, text: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!'()*".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
}

/// Where the names database of `system` is downloaded from.
pub fn database_url(system: System) -> String {
    let mut url = String::from("https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/");
    percent_encode(&mut url, system_dir(system));
    url.push_str(".dat");
    url
}

/// Where the box art of the game `name` (a name from the database) is downloaded from: the
/// collection's own file-name rule applied (the characters it replaces with `_`) and the URL
/// percent-encoded.
pub fn box_art_url(system: System, name: &str) -> String {
    let file: String = name.chars().map(|c| if "&*/:`<>?\\|\"".contains(c) { '_' } else { c }).collect();
    let mut url = String::from("https://thumbnails.libretro.com/");
    percent_encode(&mut url, system_dir(system));
    url.push_str("/Named_Boxarts/");
    percent_encode(&mut url, &file);
    url.push_str(".png");
    url
}

/// A name as a card shows it: without the trailing groups in brackets ("(Europe)", "(Rev
/// 1)", "[b]"), an article moved to the end brought back to the front ("Legend of Zelda,
/// The" → "The Legend of Zelda").
///
/// The article may also stand before a subtitle: "Legend of Zelda, The - A Link to the Past"
/// → "The Legend of Zelda - A Link to the Past". A name that is nothing but a group in
/// brackets stays as it is.
pub fn display_name(name: &str) -> String {
    let mut base = name.trim();
    loop {
        let open = match base.as_bytes().last() {
            Some(b')') => '(',
            Some(b']') => '[',
            _ => break,
        };
        let Some(at) = base.rfind(open) else { break };
        let cut = base[..at].trim_end();
        if cut.is_empty() {
            break;
        }
        base = cut;
    }
    for article in ["The", "An", "A"] {
        let tail = format!(", {article}");
        if let Some(head) = base.strip_suffix(&tail) {
            return format!("{article} {head}");
        }
        let middle = format!(", {article} - ");
        if let Some(at) = base.find(&middle) {
            return format!("{article} {} - {}", &base[..at], &base[at + middle.len()..]);
        }
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "clrmamepro (\n\tname \"Nintendo - Nintendo Entertainment System\"\n\tdescription \"sample\"\n)\n\n\
        game (\n\tname \"Chip 'n Dale - Rescue Rangers (Europe)\"\n\tregion \"Europe\"\n\
        \trom ( name \"Chip 'n Dale - Rescue Rangers (Europe).nes\" size 262160 crc 11111111 md5 AA sha1 BB )\n)\n\n\
        game (\n\tname \"Chip 'n Dale - Rescue Rangers (Europe)\"\n\tregion \"Europe\"\n\
        \trom ( name \"Chip 'n Dale - Rescue Rangers (Europe).unh\" size 262144 crc 84F7FC31 md5 AA sha1 BB )\n)\n\n\
        this is garbage\n\trom ( name \"orphan.bin\" size 1 crc 0BADF00D )\n)\n\
        game (\n\tname \"Tom & Jerry (and Tuffy) (USA)\"\r\n\
        \trom ( name \"Tom & Jerry ) crc DEADBEEF (USA).nes\" size 1 crc 0000abcd md5 AA )\n\
        \trom ( name \"Track 2.bin\" size 2 crc 22222222 )\n\
        \trom ( name \"No crc.bin\" size 2 )\n\
        \trom ( name \"Bad crc.bin\" size 2 crc XYZ )\n)\n\
        game (\n\tname \"Say \\\"Hi\\\" (World)\"\n\trom ( name \"Say \\\"Hi\\\" (World).nes\" size 3 crc 33333333 )\n)\n\
        game (\n\tname \"Late Duplicate (Japan)\"\n\trom ( name \"dup.nes\" size 3 crc 84F7FC31 )\n)\n\
        game (\n\trom ( name \"nameless.nes\" size 3 crc 44444444 )\n)\n";

    #[test]
    fn parses_the_sample() {
        let names = Names::parse(SAMPLE);
        assert_eq!(names.len(), 5);
        assert_eq!(names.get(0x1111_1111), Some("Chip 'n Dale - Rescue Rangers (Europe)"));
        assert_eq!(names.get(0x84F7_FC31), Some("Chip 'n Dale - Rescue Rangers (Europe)"));
        assert_eq!(names.get(0x0000_ABCD), Some("Tom & Jerry (and Tuffy) (USA)"));
        assert_eq!(names.get(0x2222_2222), Some("Tom & Jerry (and Tuffy) (USA)"));
        assert_eq!(names.get(0x3333_3333), Some("Say \"Hi\" (World)"));
        assert_eq!(names.get(0xDEAD_BEEF), None);
        assert_eq!(names.get(0x0BAD_F00D), None);
        assert_eq!(names.get(0x4444_4444), None);
    }

    #[test]
    fn garbage_is_empty() {
        assert!(Names::parse("").is_empty());
        assert!(Names::parse("<html>404</html>\n)\n(((\nrom ( crc 12345678 )\nname \"\n").is_empty());
        assert!(Names::from_cache("nothing\nZZZZZZZZ\tname\n12345678\t\n").is_empty());
    }

    #[test]
    fn cache_round_trip() {
        let names = Names::parse(SAMPLE);
        let cache = names.to_cache();
        assert!(cache.contains("84F7FC31\tChip 'n Dale - Rescue Rangers (Europe)\n"));
        assert!(cache.starts_with("0000ABCD\tTom & Jerry (and Tuffy) (USA)\n"));
        assert_eq!(Names::from_cache(&cache), names);
    }

    /// The real database files, when the probe left them on disk.
    fn real(file: &str) -> Option<Names> {
        let text = std::fs::read_to_string(format!("/tmp/art-probe/{file}")).ok()?;
        let names = Names::parse(&text);
        assert_eq!(Names::from_cache(&names.to_cache()), names);
        Some(names)
    }

    #[test]
    fn real_nes_database() {
        let Some(names) = real("Nintendo_Entertainment_System.dat") else { return };
        assert!(names.len() > 10_000, "{}", names.len());
        assert_eq!(names.get(0x84F7_FC31), Some("Chip 'n Dale - Rescue Rangers (Europe)"));
    }

    #[test]
    fn real_mega_drive_database() {
        let Some(names) = real("Genesis.dat") else { return };
        assert!(names.len() > 2_000, "{}", names.len());
        assert_eq!(names.get(0xF939_4E97), Some("Sonic The Hedgehog (USA, Europe)"));
    }

    #[test]
    fn urls() {
        assert_eq!(
            database_url(System::Nes),
            "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/\
             Nintendo%20-%20Nintendo%20Entertainment%20System.dat"
        );
        assert_eq!(
            database_url(System::MegaDrive),
            "https://raw.githubusercontent.com/libretro/libretro-database/master/metadat/no-intro/Sega%20-%20Mega%20Drive%20-%20Genesis.dat"
        );
        assert_eq!(
            box_art_url(System::Nes, "Chip 'n Dale - Rescue Rangers (Europe)"),
            "https://thumbnails.libretro.com/Nintendo%20-%20Nintendo%20Entertainment%20System/Named_Boxarts/\
             Chip%20'n%20Dale%20-%20Rescue%20Rangers%20(Europe).png"
        );
        assert_eq!(
            box_art_url(System::MegaDrive, "Sonic The Hedgehog (USA, Europe)"),
            "https://thumbnails.libretro.com/Sega%20-%20Mega%20Drive%20-%20Genesis/Named_Boxarts/\
             Sonic%20The%20Hedgehog%20(USA%2C%20Europe).png"
        );
        assert_eq!(
            box_art_url(System::MegaDrive, "Tom & Jerry: What? Pokémon"),
            "https://thumbnails.libretro.com/Sega%20-%20Mega%20Drive%20-%20Genesis/Named_Boxarts/\
             Tom%20_%20Jerry_%20What_%20Pok%C3%A9mon.png"
        );
    }

    #[test]
    fn display_names() {
        assert_eq!(display_name("Legend of Zelda, The (USA) (Rev 1)"), "The Legend of Zelda");
        assert_eq!(display_name("Chip 'n Dale - Rescue Rangers (Europe)"), "Chip 'n Dale - Rescue Rangers");
        assert_eq!(display_name("Sonic The Hedgehog (USA, Europe)"), "Sonic The Hedgehog");
        assert_eq!(display_name("Legend of Zelda, The - A Link to the Past (USA) [b]"), "The Legend of Zelda - A Link to the Past");
        assert_eq!(display_name("Boy and His Blob, A - Trouble on Blobolonia (USA)"), "A Boy and His Blob - Trouble on Blobolonia");
        assert_eq!(display_name("American Tail, An (USA) [!]"), "An American Tail");
        assert_eq!(display_name("Tetris"), "Tetris");
        assert_eq!(display_name("(Unknown)"), "(Unknown)");
        assert_eq!(display_name(""), "");
    }
}
