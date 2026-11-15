use std::collections::BTreeMap;

pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("ru", "Русский"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("it", "Italiano"),
];
pub struct Catalog {
    base: BTreeMap<String, String>,
    selected: BTreeMap<String, String>,
}
impl Catalog {
    pub fn new(language: &str) -> Self {
        let base = serde_json::from_str(include_str!("../locales/en.json"))
            .expect("embedded English catalog");
        let text = match language {
            "ru" => include_str!("../locales/ru.json"),
            "fr" => include_str!("../locales/fr.json"),
            "de" => include_str!("../locales/de.json"),
            "es" => include_str!("../locales/es.json"),
            "it" => include_str!("../locales/it.json"),
            _ => include_str!("../locales/en.json"),
        };
        Self {
            base,
            selected: serde_json::from_str(text).expect("embedded language catalog"),
        }
    }
    pub fn text<'a>(&'a self, key: &'a str) -> &'a str {
        self.selected
            .get(key)
            .or_else(|| self.base.get(key))
            .map(String::as_str)
            .unwrap_or(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_six_catalogs_are_complete() {
        let base = Catalog::new("en");
        for (code, _) in LANGUAGES {
            let catalog = Catalog::new(code);
            assert_eq!(
                base.base.keys().collect::<Vec<_>>(),
                catalog.selected.keys().collect::<Vec<_>>(),
                "{code}"
            );
            assert!(catalog.selected.values().all(|v| !v.trim().is_empty()));
        }
    }
}
