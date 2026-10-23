//! Checks that every text in the interface file has a Turkish translation, and that no
//! translation is left over.

use std::collections::BTreeSet;

/// Every file of the interface.
const FILES: [&str; 6] = [
    include_str!("../ui/app.slint"),
    include_str!("../ui/components.slint"),
    include_str!("../ui/contract.slint"),
    include_str!("../ui/pages.slint"),
    include_str!("../ui/texts.slint"),
    include_str!("../ui/theme.slint"),
];
const TEXTS: &str = include_str!("../ui/texts.slint");
const TURKISH: &str = include_str!("../lang/tr/LC_MESSAGES/cxtune-gui.po");

/// Texts that read the same in Turkish.
const SAME_IN_TURKISH: [&str; 13] = [
    "cxtune: {}",
    "{} ms",
    "AGC",
    "Sidetone",
    "Test",
    "+{} dB",
    "{} dB",
    "{} dBFS",
    "{}",
    "{} · {}",
    "{} / {} dBFS",
    "{}: {}, {}: {}, {}: {}",
    "≈ {} ms",
];

/// The string literal that starts at `text[0]` (an opening quote), unescaped, and the rest.
fn literal(text: &str) -> (String, &str) {
    assert!(text.starts_with('"'), "{text:.40}");
    let mut out = String::new();
    let mut chars = text[1..].char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return (out, &text[at + 2..]),
            '\\' => match chars.next().expect("an escaped character").1 {
                'n' => out.push('\n'),
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    panic!("an unterminated string: {text:.40}");
}

/// The `(context, text)` of every `@tr("context" => "text", ...)` in `source`, in order.
fn texts_in(source: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find("@tr(") {
        let (context, after) = literal(&rest[at + 4..]);
        let (text, after) = literal(after.strip_prefix(" => ").expect("an explicit context"));
        found.push((context, text));
        rest = after;
    }
    found
}

/// The texts of every file of the interface.
fn used_texts() -> BTreeSet<(String, String)> {
    FILES.iter().flat_map(|file| texts_in(file)).collect()
}

/// The `(context, text, translation)` of every entry of the Turkish catalogue.
fn translations() -> Vec<(String, String, String)> {
    let mut entries = Vec::new();
    let (mut context, mut text) = (None, None);
    for line in TURKISH.lines() {
        if let Some(value) = line.strip_prefix("msgctxt ") {
            context = Some(literal(value).0);
        } else if let Some(value) = line.strip_prefix("msgid ") {
            text = Some(literal(value).0);
        } else if let Some(value) = line.strip_prefix("msgstr ")
            && let (Some(context), Some(text)) = (context.take(), text.take())
        {
            entries.push((context, text, literal(value).0));
        }
    }
    entries
}

fn placeholders(text: &str) -> usize {
    text.matches("{}").count()
}

#[test]
fn every_text_has_a_turkish_translation_and_every_translation_a_text() {
    let used = used_texts();
    assert!(used.len() > 50, "the scan found only {} texts", used.len());
    let translated: BTreeSet<_> = translations()
        .into_iter()
        .map(|(context, text, _)| (context, text))
        .collect();
    let untranslated: Vec<_> = used.difference(&translated).collect();
    assert!(
        untranslated.is_empty(),
        "no translation for {untranslated:#?}"
    );
    let unused: Vec<_> = translated.difference(&used).collect();
    assert!(unused.is_empty(), "translations of no text: {unused:#?}");
}

#[test]
fn a_translation_is_a_translation_with_the_same_placeholders() {
    let entries = translations();
    assert!(entries.len() > 50);
    for (_, text, translation) in entries {
        assert!(!translation.trim().is_empty(), "{text}");
        assert_eq!(
            placeholders(&text),
            placeholders(&translation),
            "{text} / {translation}"
        );
        if !SAME_IN_TURKISH.contains(&text.as_str()) {
            assert_ne!(text, translation, "left untranslated");
        }
    }
}

#[test]
fn texts_that_name_a_button_use_the_label_of_the_button() {
    let entries = translations();
    let turkish = |english: &str| {
        entries
            .iter()
            .find(|(_, text, _)| text == english)
            .map(|(_, _, translation)| translation.clone())
            .unwrap_or_else(|| panic!("{english}"))
    };
    let button = turkish("Re-apply");
    for text in [
        "Press Re-apply, or replug the dongle.",
        "Close it, then press Re-apply.",
        "Press Re-apply, or restart cxtune.",
        "Fix or delete it, then press Re-apply.",
        "Press Re-apply to try again.",
    ] {
        assert!(FILES.iter().any(|file| file.contains(text)), "{text}");
        assert!(turkish(text).contains(&button), "{text}");
    }
    let mode = turkish("Lowest noise");
    let help = "Used only by Lowest noise. After audio starts it stays muted for this time plus about 150 ms.";
    assert!(FILES.iter().any(|file| file.contains(help)));
    assert!(turkish(help).contains(&mode));
}

/// The texts of the functions of `texts.slint` from the one that starts with `from` up to the
/// one that starts with `to`.
fn texts_between(from: &str, to: &str) -> Vec<String> {
    let start = TEXTS.find(from).expect(from);
    let end = TEXTS.find(to).expect(to);
    texts_in(&TEXTS[start..end])
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

#[test]
fn the_status_lines_and_the_about_lines_fit_one_line_in_both_languages() {
    // the footer and the About card show each line on one line and elide what does not fit; the
    // snapshots of the longest of these were looked at, and these are their lengths
    let entries = translations();
    let turkish = |english: &str| {
        entries
            .iter()
            .find(|(_, text, _)| text == english)
            .map(|(_, _, translation)| translation.clone())
            .unwrap_or_else(|| panic!("{english}"))
    };
    for (from, to, count, limit) in [
        ("function title(", "function hint(", 21, 36),
        ("function hint(", "function pill(", 14, 54),
        ("function about-line(", "function level-db(", 4, 76),
    ] {
        let texts = texts_between(from, to);
        assert_eq!(texts.len(), count, "{from}");
        for text in texts {
            for line in [text.clone(), turkish(&text)] {
                assert!(
                    line.chars().count() <= limit,
                    "{line} is longer than {limit}"
                );
            }
        }
    }
}

#[test]
fn the_about_lines_name_the_vendors_and_the_usb_id_in_both_languages() {
    let entries = translations();
    for (english, needles) in [
        (
            "Not affiliated with HyperX, HP or Synaptics.",
            ["HyperX", "HP", "Synaptics"].as_slice(),
        ),
        (
            "Made for the HyperX Cloud II USB sound card (USB 0951:16A4).",
            ["HyperX", "0951:16A4"].as_slice(),
        ),
    ] {
        let (_, _, turkish) = entries
            .iter()
            .find(|(_, text, _)| text == english)
            .unwrap_or_else(|| panic!("{english}"));
        for text in [english, turkish] {
            for needle in needles {
                assert!(text.contains(needle), "{needle} is missing from {text}");
            }
        }
    }
}
