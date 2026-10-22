use std::{collections::BTreeMap, io::Write, path::PathBuf, sync::OnceLock};

use slint::ComponentHandle;

use crate::{AppWindow, I18n};

fn catalog() -> &'static BTreeMap<String, String> {
    static CATALOG: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("translations.json")).expect("valid translation catalog")
    })
}

// Source templates use Rust format placeholders. Match only complete messages,
// preserve their variable values, and allow translated templates to reorder them.
fn tokens(template: &str) -> (Vec<&str>, Vec<&str>) {
    let mut literals = Vec::new();
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        literals.push(&rest[..start]);
        names.push(&rest[start + 1..start + end]);
        rest = &rest[start + end + 1..];
    }
    literals.push(rest);
    (literals, names)
}

fn translate(source: &str, depth: u8) -> String {
    let catalog = catalog();
    if let Some(value) = catalog.get(source) {
        return value.clone();
    }
    if depth == 0
        || !source
            .chars()
            .any(|c| ('\u{0400}'..='\u{04ff}').contains(&c))
    {
        return source.to_owned();
    }
    static TEMPLATES: OnceLock<Vec<(&str, &str)>> = OnceLock::new();
    let templates = TEMPLATES.get_or_init(|| {
        let mut values: Vec<_> = catalog
            .iter()
            .filter(|(key, _)| key.contains('{'))
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        // Prefer a specific message over generic unit templates such as "{} MB".
        values.sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));
        values
    });
    for &(template, english) in templates {
        let (literals, names) = tokens(template);
        let Some(mut rest) = source.strip_prefix(literals[0]) else {
            continue;
        };
        let mut captures = Vec::new();
        let mut matched = true;
        for (index, literal) in literals[1..].iter().enumerate() {
            let end = if index == names.len() - 1 {
                rest.strip_suffix(literal).map(str::len)
            } else if literal.is_empty() {
                None
            } else {
                rest.find(literal)
            };
            let Some(end) = end else {
                matched = false;
                break;
            };
            captures.push(&rest[..end]);
            rest = &rest[end + literal.len()..];
        }
        if !matched || !rest.is_empty() {
            continue;
        }
        let (english_literals, english_names) = tokens(english);
        let mut result = english_literals[0].to_owned();
        let mut positional = 0;
        for (index, name) in english_names.iter().enumerate() {
            let source_index = if name.is_empty() || name.starts_with(':') {
                let index = names
                    .iter()
                    .enumerate()
                    .filter(|(_, n)| n.is_empty() || n.starts_with(':'))
                    .nth(positional)
                    .map(|(i, _)| i);
                positional += 1;
                index
            } else {
                names.iter().position(|n| n == name)
            };
            let Some(value) = source_index.and_then(|i| captures.get(i)) else {
                matched = false;
                break;
            };
            result.push_str(&translate(value, depth - 1));
            result.push_str(english_literals[index + 1]);
        }
        if matched {
            return result;
        }
    }
    // Composed progress lines carry already formatted values and multiple clauses.
    for separator in ["\n", " • "] {
        if source.contains(separator) {
            return source
                .split(separator)
                .map(|part| translate(part, depth - 1))
                .collect::<Vec<_>>()
                .join(separator);
        }
    }
    source.to_owned()
}

fn load_language(path: &std::path::Path) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<String>(&bytes).is_ok_and(|value| value == "en"),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "could not read language preference");
            }
            false
        }
    }
}

fn save_language(path: &std::path::Path, english: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(if english { b"\"en\"\n" } else { b"\"ru\"\n" })?;
    file.commit()
}

pub fn bind(window: &AppWindow, path: PathBuf) {
    window
        .global::<I18n>()
        .on_translate(|text| translate(&text, 6).into());
    window.global::<I18n>().set_english(load_language(&path));
    let weak = window.as_weak();
    window.global::<I18n>().on_select_language(move |english| {
        let Some(window) = weak.upgrade() else { return };
        // Keyboard and programmatic invocations obey the same operation guard.
        if window.get_loading()
            || window.get_settings_visible()
            || window.get_diagnostics_visible()
            || window.get_auth_visible()
            || window.get_update_visible()
            || window.get_java_visible()
        {
            return;
        }
        if let Err(error) = save_language(&path, english) {
            tracing::warn!(%error, "could not save language preference");
        }
        window.global::<I18n>().set_english(english);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_live_messages_and_preserves_values() {
        assert_eq!(translate("Играть", 6), "Play");
        assert_eq!(
            translate("Загрузка: 4.2 из 10.0 МБ", 6),
            "Downloading: 4.2 of 10.0 MB"
        );
        assert_eq!(
            translate("Сохранено 8192 МБ; допустимо 6144 МБ.", 6),
            "Saved: 8192 MB; allowed: 6144 MB."
        );
        assert_eq!(
            translate("Получено 48.0 МБ • 2.0 МБ/с • 17 с", 6),
            "Received 48.0 MB • 2.0 MB/s • 17 s"
        );
        assert_eq!(translate("Player_123", 6), "Player_123");
        assert_eq!(
            translate("https://launcher.nevermoreland.ru/", 6),
            "https://launcher.nevermoreland.ru/"
        );
    }

    #[test]
    fn catalog_covers_interface_and_preserves_template_variables() {
        for (source, english) in catalog() {
            assert!(!english.is_empty(), "empty translation: {source}");
            let (_, mut source_names) = tokens(source);
            let (_, mut english_names) = tokens(english);
            source_names.sort();
            english_names.sort();
            assert_eq!(source_names, english_names, "template variables: {source}");
        }
        let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/components");
        for entry in std::fs::read_dir(ui).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "slint") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for text in source.split("I18n.tr(\"").skip(1) {
                let literal = text.split("\")").next().unwrap().replace("\\n", "\n");
                assert!(
                    catalog().contains_key(&literal),
                    "missing translation in {}: {literal}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn language_preference_survives_restart_and_invalid_data_defaults_to_russian() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state/language.json");
        assert!(!load_language(&path));
        save_language(&path, true).unwrap();
        assert!(load_language(&path));
        save_language(&path, false).unwrap();
        assert!(!load_language(&path));
        std::fs::write(&path, b"invalid").unwrap();
        assert!(!load_language(&path));
    }
}
