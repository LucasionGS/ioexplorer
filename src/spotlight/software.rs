//! The software catalog: a two-level set of installable applications, and the
//! grammar that walks it.
//!
//! Pure over its inputs — no I/O, no GTK — so the whole catalog model, including
//! how a user's config merges into the built-in one, is testable without a main
//! loop. The same reasoning as [`crate::spotlight::prefixes::resolve_with_ai`].

use crate::config::{
    SpotlightSoftwareCategoryConfig, SpotlightSoftwareConfig, SpotlightSoftwareItemConfig,
};

/// Artwork for the prefix itself, and for a category that names none.
pub const SOFTWARE_ICON: &str = "system-software-install-symbolic";

/// One installable application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item {
    pub name: String,
    pub description: String,
    pub icon: String,
    /// The command line that installs it. Run verbatim, in a terminal.
    pub command: String,
    /// Extra search terms beyond the name.
    pub keywords: Vec<String>,
}

/// One category of the catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Category {
    /// What the user types to enter it, and the key config entries merge on.
    pub id: String,
    pub label: String,
    pub icon: String,
    pub items: Vec<Item>,
}

/// The resolved catalog: the built-in one with the user's config merged in.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Catalog {
    categories: Vec<Category>,
}

/// What a software query is asking for, once the category level has been split
/// off the prefix's argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SoftwareQuery<'a> {
    /// The top level: browse the categories, and search across every app.
    Categories { filter: &'a str },
    /// Inside a category: browse the apps it holds.
    Items {
        category: &'a Category,
        filter: &'a str,
    },
}

impl Catalog {
    /// Merges the user's categories into the built-in catalog.
    ///
    /// Built-ins come first, minus anything in `disabled_categories`. A config
    /// category whose `id` matches one of them *merges* into it — label and icon
    /// override when given, items replace-or-append by name — so adding one app
    /// to Creativity does not cost the user the ones already there. An unknown
    /// `id` appends a new category.
    pub fn resolve(config: &SpotlightSoftwareConfig) -> Self {
        let mut categories = builtins()
            .into_iter()
            .filter(|category| !config.disabled_categories.contains(&category.id))
            .collect::<Vec<_>>();

        for entry in &config.categories {
            merge_category(&mut categories, entry);
        }

        categories.retain(|category| !category.items.is_empty());
        Self { categories }
    }

    pub fn categories(&self) -> &[Category] {
        &self.categories
    }

    /// Every app in the catalog, paired with the category it came from.
    pub fn items(&self) -> impl Iterator<Item = (&Category, &Item)> {
        self.categories
            .iter()
            .flat_map(|category| category.items.iter().map(move |item| (category, item)))
    }
}

/// Splits the prefix argument into a level.
///
/// Categories are scanned longest-first over both the id and the label, and a
/// match needs trailing whitespace to commit — the same rule
/// [`crate::spotlight::query::parse`] applies to the prefix itself, so
/// `install kr` keeps searching rather than jumping into a category named `k`.
///
/// Matching is ASCII-case-insensitive: `Gaming ` and `gaming ` both enter the
/// category, while a label with non-ASCII letters has to be typed as written.
pub fn parse_arg<'a>(catalog: &'a Catalog, arg: &'a str) -> SoftwareQuery<'a> {
    let text = arg.trim_start();

    let mut keys = Vec::new();
    for category in catalog.categories() {
        keys.push((category.id.as_str(), category));
        if !category.label.eq_ignore_ascii_case(&category.id) {
            keys.push((category.label.as_str(), category));
        }
    }
    keys.sort_by_key(|(key, _)| std::cmp::Reverse(key.len()));

    for (key, category) in keys {
        // `get` rather than a slice: a key whose byte length lands inside a
        // multi-byte character is simply not a match, and must not panic.
        let Some(head) = text.get(..key.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(key) {
            continue;
        }
        let rest = &text[key.len()..];
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        return SoftwareQuery::Items {
            category,
            filter: rest.trim_start(),
        };
    }

    SoftwareQuery::Categories { filter: text }
}

/// The text that enters a category, i.e. what a category row rewrites the entry
/// to. The trailing space is the commit.
pub fn category_query(prefix_key: &str, category: &Category) -> String {
    format!("{prefix_key} {} ", category.id)
}

/// The command line that installs `item`.
///
/// With `keep_open` the terminal is held open once the install finishes, because
/// otherwise the window closes on the last line of output and takes the result
/// with it. [`crate::launcher::spawn::spawn_in_terminal`] runs the line through
/// `sh -c` as a single argument, so the chaining below is safe.
pub fn install_line(item: &Item, keep_open: bool) -> String {
    match keep_open {
        true => format!(
            "{}; printf '\\n[press Enter to close] '; read -r _",
            item.command
        ),
        false => item.command.clone(),
    }
}

/// Lowercases and hyphenates a name, for use as a frecency key.
pub fn slug(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch.is_alphanumeric() {
            true => slug.extend(ch.to_lowercase()),
            false if slug.ends_with('-') => {}
            false => slug.push('-'),
        }
    }
    slug.trim_matches('-').to_string()
}

fn merge_category(categories: &mut Vec<Category>, entry: &SpotlightSoftwareCategoryConfig) {
    let id = entry.id.trim();
    if id.is_empty() {
        tracing::warn!("ignoring a software category with no id");
        return;
    }

    let index = match categories.iter().position(|existing| existing.id == id) {
        Some(index) => index,
        None => {
            categories.push(Category {
                id: id.to_string(),
                label: id.to_string(),
                icon: SOFTWARE_ICON.to_string(),
                items: Vec::new(),
            });
            categories.len() - 1
        }
    };

    let category = &mut categories[index];
    if let Some(label) = trimmed(entry.label.as_deref()) {
        category.label = label.to_string();
    }
    if let Some(icon) = trimmed(entry.icon.as_deref()) {
        category.icon = icon.to_string();
    }

    let fallback_icon = category.icon.clone();
    for item in &entry.items {
        let Some(item) = item_from_config(item, &fallback_icon) else {
            continue;
        };
        match category
            .items
            .iter()
            .position(|existing| existing.name.eq_ignore_ascii_case(&item.name))
        {
            Some(index) => category.items[index] = item,
            None => category.items.push(item),
        }
    }
}

fn item_from_config(entry: &SpotlightSoftwareItemConfig, fallback_icon: &str) -> Option<Item> {
    let name = entry.name.trim();
    let command = entry.command.trim();
    if name.is_empty() || command.is_empty() {
        tracing::warn!(
            name = entry.name,
            "ignoring a software item with no name or no command"
        );
        return None;
    }

    Some(Item {
        name: name.to_string(),
        description: trimmed(entry.description.as_deref())
            .unwrap_or(command)
            .to_string(),
        icon: trimmed(entry.icon.as_deref())
            .unwrap_or(fallback_icon)
            .to_string(),
        command: command.to_string(),
        keywords: entry
            .keywords
            .iter()
            .map(|keyword| keyword.trim().to_string())
            .filter(|keyword| !keyword.is_empty())
            .collect(),
    })
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// A small helper so the catalog below reads as data rather than as five lines
/// of construction per app.
fn item(name: &str, description: &str, command: &str, keywords: &[&str], icon: &str) -> Item {
    Item {
        name: name.to_string(),
        description: description.to_string(),
        icon: icon.to_string(),
        command: command.to_string(),
        keywords: keywords.iter().map(|keyword| keyword.to_string()).collect(),
    }
}

/// The catalog that ships with spotlight.
///
/// Commands go through `yay` rather than `pacman`, because half of them are only
/// in the AUR and one package manager for the whole list beats remembering which
/// app needs which. Anything here can be replaced from `config.toml`.
fn builtins() -> Vec<Category> {
    const CREATIVITY_ICON: &str = "applications-graphics-symbolic";
    const GAMING_ICON: &str = "applications-games-symbolic";
    const COMMUNICATION_ICON: &str = "chat-message-new-symbolic";
    const DEVELOPMENT_ICON: &str = "applications-engineering-symbolic";

    vec![
        Category {
            id: "creativity".to_string(),
            label: "Creativity".to_string(),
            icon: CREATIVITY_ICON.to_string(),
            items: vec![
                item(
                    "GIMP",
                    "Image editor",
                    "yay -S --needed gimp",
                    &["photo", "image", "editor", "photoshop"],
                    CREATIVITY_ICON,
                ),
                item(
                    "Krita",
                    "Digital painting",
                    "yay -S --needed krita",
                    &["paint", "drawing", "art"],
                    CREATIVITY_ICON,
                ),
            ],
        },
        Category {
            id: "gaming".to_string(),
            label: "Gaming".to_string(),
            icon: GAMING_ICON.to_string(),
            items: vec![
                item(
                    "Steam",
                    "Game store and launcher",
                    "yay -S --needed steam",
                    &["valve", "games"],
                    GAMING_ICON,
                ),
                item(
                    "CurseForge",
                    "Minecraft mod manager",
                    "yay -S --needed curseforge",
                    &["minecraft", "mods"],
                    GAMING_ICON,
                ),
            ],
        },
        Category {
            id: "communication".to_string(),
            label: "Communication".to_string(),
            icon: COMMUNICATION_ICON.to_string(),
            items: vec![item(
                "Discord",
                "Voice and text chat",
                "yay -S --needed discord",
                &["chat", "voice"],
                COMMUNICATION_ICON,
            )],
        },
        Category {
            id: "development".to_string(),
            label: "Development".to_string(),
            icon: DEVELOPMENT_ICON.to_string(),
            items: vec![item(
                "Visual Studio Code",
                "Code editor",
                "yay -S --needed visual-studio-code-bin",
                &["vscode", "editor", "ide"],
                DEVELOPMENT_ICON,
            )],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(categories: Vec<SpotlightSoftwareCategoryConfig>) -> SpotlightSoftwareConfig {
        SpotlightSoftwareConfig {
            categories,
            ..Default::default()
        }
    }

    fn item_config(name: &str, command: &str) -> SpotlightSoftwareItemConfig {
        SpotlightSoftwareItemConfig {
            name: name.to_string(),
            command: command.to_string(),
            ..Default::default()
        }
    }

    fn category<'a>(catalog: &'a Catalog, id: &str) -> Option<&'a Category> {
        catalog.categories().iter().find(|entry| entry.id == id)
    }

    fn names(catalog: &Catalog, id: &str) -> Vec<String> {
        category(catalog, id)
            .expect("category")
            .items
            .iter()
            .map(|item| item.name.clone())
            .collect()
    }

    #[test]
    fn the_builtin_catalog_resolves_without_any_config() {
        let catalog = Catalog::resolve(&SpotlightSoftwareConfig::default());

        assert_eq!(catalog.categories().len(), 4);
        assert_eq!(catalog.items().count(), 6);
        assert_eq!(names(&catalog, "creativity"), ["GIMP", "Krita"]);
    }

    #[test]
    fn a_config_category_merges_into_the_builtin_one() {
        let catalog = Catalog::resolve(&config(vec![SpotlightSoftwareCategoryConfig {
            id: "creativity".to_string(),
            items: vec![item_config("Inkscape", "yay -S --needed inkscape")],
            ..Default::default()
        }]));

        assert_eq!(names(&catalog, "creativity"), ["GIMP", "Krita", "Inkscape"]);
    }

    #[test]
    fn an_item_of_the_same_name_replaces_the_builtin_one() {
        let catalog = Catalog::resolve(&config(vec![SpotlightSoftwareCategoryConfig {
            id: "creativity".to_string(),
            items: vec![item_config(
                "gimp",
                "flatpak install -y flathub org.gimp.GIMP",
            )],
            ..Default::default()
        }]));

        let items = &category(&catalog, "creativity").expect("category").items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].command, "flatpak install -y flathub org.gimp.GIMP");
    }

    #[test]
    fn an_unknown_id_appends_a_new_category() {
        let catalog = Catalog::resolve(&config(vec![SpotlightSoftwareCategoryConfig {
            id: "office".to_string(),
            label: Some("Office".to_string()),
            items: vec![item_config(
                "LibreOffice",
                "yay -S --needed libreoffice-fresh",
            )],
            ..Default::default()
        }]));

        assert_eq!(catalog.categories().len(), 5);
        assert_eq!(
            category(&catalog, "office").expect("category").label,
            "Office"
        );
    }

    #[test]
    fn disabled_categories_are_dropped() {
        let catalog = Catalog::resolve(&SpotlightSoftwareConfig {
            disabled_categories: vec!["gaming".to_string()],
            ..Default::default()
        });

        assert!(category(&catalog, "gaming").is_none());
        assert_eq!(catalog.categories().len(), 3);
    }

    #[test]
    fn an_item_without_a_command_is_ignored() {
        let catalog = Catalog::resolve(&config(vec![SpotlightSoftwareCategoryConfig {
            id: "creativity".to_string(),
            items: vec![item_config("Broken", "   ")],
            ..Default::default()
        }]));

        assert_eq!(names(&catalog, "creativity"), ["GIMP", "Krita"]);
    }

    #[test]
    fn a_category_left_with_no_items_is_not_listed() {
        let catalog = Catalog::resolve(&config(vec![SpotlightSoftwareCategoryConfig {
            id: "empty".to_string(),
            ..Default::default()
        }]));

        assert!(category(&catalog, "empty").is_none());
    }

    #[test]
    fn an_empty_argument_is_the_category_level() {
        let catalog = Catalog::resolve(&SpotlightSoftwareConfig::default());

        assert_eq!(
            parse_arg(&catalog, ""),
            SoftwareQuery::Categories { filter: "" }
        );
    }

    #[test]
    fn entering_a_category_needs_a_trailing_space() {
        let catalog = Catalog::resolve(&SpotlightSoftwareConfig::default());

        assert_eq!(
            parse_arg(&catalog, "gaming"),
            SoftwareQuery::Categories { filter: "gaming" }
        );
        assert!(matches!(
            parse_arg(&catalog, "gaming "),
            SoftwareQuery::Items {
                category,
                filter: ""
            } if category.id == "gaming"
        ));
    }

    #[test]
    fn a_category_is_matched_case_insensitively_and_keeps_its_filter() {
        let catalog = Catalog::resolve(&SpotlightSoftwareConfig::default());

        assert!(matches!(
            parse_arg(&catalog, "Creativity  kri"),
            SoftwareQuery::Items {
                category,
                filter: "kri"
            } if category.id == "creativity"
        ));
    }

    #[test]
    fn a_multi_word_label_is_matched_whole() {
        let catalog = Catalog::resolve(&config(vec![SpotlightSoftwareCategoryConfig {
            id: "media".to_string(),
            label: Some("Audio and Video".to_string()),
            items: vec![item_config("Audacity", "yay -S --needed audacity")],
            ..Default::default()
        }]));

        assert!(matches!(
            parse_arg(&catalog, "audio and video aud"),
            SoftwareQuery::Items {
                category,
                filter: "aud"
            } if category.id == "media"
        ));
    }

    #[test]
    fn install_line_only_waits_when_asked() {
        let item = item("GIMP", "", "yay -S gimp", &[], SOFTWARE_ICON);

        assert_eq!(install_line(&item, false), "yay -S gimp");
        assert!(install_line(&item, true).starts_with("yay -S gimp; printf "));
    }

    #[test]
    fn slugs_are_lowercase_and_hyphenated() {
        assert_eq!(slug("Visual Studio Code"), "visual-studio-code");
        assert_eq!(slug("GIMP"), "gimp");
        assert_eq!(slug("  C++ / Rust  "), "c-rust");
    }
}
