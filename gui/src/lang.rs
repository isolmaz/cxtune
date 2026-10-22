//! The language of the interface: Turkish or English.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use windows::Win32::Globalization::GetUserDefaultUILanguage;

/// The primary language identifier of Turkish; every `tr-*` language has it in the low ten bits.
const LANG_TURKISH: u16 = 0x1F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Turkish,
    English,
}

impl Lang {
    /// The code of the bundled translation.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Turkish => "tr",
            Lang::English => "en",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        [Lang::Turkish, Lang::English]
            .into_iter()
            .find(|lang| lang.code() == code)
    }

    /// Turkish for a Turkish language identifier, English for any other.
    pub fn from_langid(id: u16) -> Lang {
        if id & 0x3FF == LANG_TURKISH {
            Lang::Turkish
        } else {
            Lang::English
        }
    }

    /// The display language of Windows for this user.
    pub fn of_windows() -> Lang {
        // SAFETY: a plain query without arguments.
        Lang::from_langid(unsafe { GetUserDefaultUILanguage() })
    }
}

/// `%APPDATA%\cxtune\gui.toml`, next to the engine's settings file.
pub fn preferences_path() -> Option<PathBuf> {
    cxtune::config::path()
        .ok()
        .map(|config| config.with_file_name("gui.toml"))
}

fn parse(text: &str) -> Option<Lang> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != "language" {
            return None;
        }
        Lang::from_code(value.trim().trim_matches('"'))
    })
}

fn render(lang: Lang) -> String {
    format!("language = \"{}\"\n", lang.code())
}

/// The language chosen earlier, or `None` if there is none or the file cannot be understood.
pub fn load(path: &Path) -> Option<Lang> {
    parse(&std::fs::read_to_string(path).ok()?)
}

/// Writes the choice to a temporary file next to `path` and renames it over `path`, so the file
/// is never half written. On failure `path` is untouched and the temporary file is removed.
pub fn save(path: &Path, lang: Lang) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let write = || -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(render(lang).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_turkish_language_identifier_is_turkish_and_every_other_english() {
        // tr-TR, and the identifiers of other languages
        assert_eq!(Lang::from_langid(0x041F), Lang::Turkish);
        assert_eq!(Lang::from_langid(0x001F), Lang::Turkish);
        for other in [0x0409, 0x0809, 0x0407, 0x040C, 0x042C, 0x0000, 0x0C0A] {
            assert_eq!(Lang::from_langid(other), Lang::English, "{other:#06x}");
        }
    }

    #[test]
    fn the_language_of_windows_is_one_of_the_two() {
        assert!(matches!(Lang::of_windows(), Lang::Turkish | Lang::English));
    }

    #[test]
    fn codes_name_the_bundled_translations() {
        for lang in [Lang::Turkish, Lang::English] {
            assert_eq!(Lang::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Lang::from_code("TR"), None);
        assert_eq!(Lang::from_code(""), None);
    }

    #[test]
    fn a_chosen_language_is_read_back() {
        for lang in [Lang::Turkish, Lang::English] {
            assert_eq!(parse(&render(lang)), Some(lang));
        }
        assert_eq!(parse("# comment\nlanguage = \"en\"\n"), Some(Lang::English));
        assert_eq!(parse("language=tr"), Some(Lang::Turkish));
    }

    #[test]
    fn a_file_that_does_not_name_a_language_means_no_choice() {
        for text in [
            "",
            "language = \"de\"\n",
            "theme = \"tr\"\n",
            "language\n",
            "\u{0}\u{1}",
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    /// A directory that is removed when the test ends, also when it fails.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_choice_is_kept_in_a_file() {
        let dir =
            Scratch(std::env::temp_dir().join(format!("cxtune-gui-lang-{}", std::process::id())));
        let file = dir.0.join("nested").join("gui.toml");
        assert_eq!(load(&file), None);
        save(&file, Lang::English).unwrap();
        assert_eq!(load(&file), Some(Lang::English));
        save(&file, Lang::Turkish).unwrap();
        assert_eq!(load(&file), Some(Lang::Turkish));
    }

    #[test]
    fn saving_leaves_no_temporary_file_and_a_failed_save_leaves_the_path_alone() {
        let dir = Scratch(
            std::env::temp_dir().join(format!("cxtune-gui-lang-atomic-{}", std::process::id())),
        );
        let file = dir.0.join("gui.toml");
        save(&file, Lang::English).unwrap();
        let names = |dir: &Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(names(&dir.0), ["gui.toml"]);

        // a directory cannot be replaced by a file: the rename fails
        let blocked = dir.0.join("blocked.toml");
        std::fs::create_dir(&blocked).unwrap();
        assert!(save(&blocked, Lang::Turkish).is_err());
        assert!(blocked.is_dir());
        let mut left = names(&dir.0);
        left.sort();
        assert_eq!(left, ["blocked.toml", "gui.toml"]);
        assert_eq!(load(&file), Some(Lang::English));
    }
}
