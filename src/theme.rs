use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

use crate::config::AppConfig;

const BUNDLED_CSS: &str = include_str!("../data/styles/ioexplorer.css");
pub const AUTO_GEN_START: &str = "/* IOEXPLORER AUTO_GEN */";
pub const AUTO_GEN_END: &str = "/* /IOEXPLORER AUTO_GEN */";

#[derive(Clone, Debug, PartialEq)]
pub struct ThemeSettings {
    pub window_background: gtk::gdk::RGBA,
    pub panel_background: gtk::gdk::RGBA,
    pub muted_background: gtk::gdk::RGBA,
    pub accent: gtk::gdk::RGBA,
    pub selection: gtk::gdk::RGBA,
    pub text: gtk::gdk::RGBA,
    pub border: gtk::gdk::RGBA,
    pub corner_radius: i32,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            window_background: rgba255(10, 12, 16, 0.74),
            panel_background: rgba255(13, 15, 20, 0.86),
            muted_background: rgba255(0, 0, 0, 0.24),
            accent: rgba255(82, 145, 238, 1.0),
            selection: rgba255(67, 123, 214, 0.34),
            text: rgba255(244, 247, 252, 0.94),
            border: rgba255(255, 255, 255, 0.12),
            corner_radius: 6,
        }
    }
}

#[derive(Clone)]
pub struct LiveTheme {
    provider: gtk::CssProvider,
}

impl LiveTheme {
    pub fn new() -> Option<Self> {
        let Some(display) = gtk::gdk::Display::default() else {
            tracing::warn!("no display available for live CSS provider");
            return None;
        };

        let provider = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
        );
        Some(Self { provider })
    }

    pub fn apply_css(&self, css: &str) {
        self.provider.load_from_string(css);
    }
}

pub fn install(config: &AppConfig) {
    let Some(display) = gtk::gdk::Display::default() else {
        tracing::warn!("no display available for CSS provider");
        return;
    };

    let provider = gtk::CssProvider::new();
    provider.load_from_string(BUNDLED_CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    if let Some(path) = &config.custom_css {
        match fs::read_to_string(path) {
            Ok(css) => {
                let custom = gtk::CssProvider::new();
                custom.load_from_string(&css);
                gtk::style_context_add_provider_for_display(
                    &display,
                    &custom,
                    gtk::STYLE_PROVIDER_PRIORITY_USER,
                );
            }
            Err(error) => tracing::warn!(?path, %error, "failed to load custom CSS"),
        }
    }
}

pub fn default_custom_css_path() -> Option<PathBuf> {
    ProjectDirs::from("io.github", "ionix", "ioexplorer")
        .map(|dirs| dirs.config_dir().join("theme.css"))
}

pub fn effective_custom_css_path(config: &AppConfig) -> Option<PathBuf> {
    config.custom_css.clone().or_else(default_custom_css_path)
}

pub fn load_generated_settings(path: Option<&Path>) -> ThemeSettings {
    let Some(path) = path else {
        return ThemeSettings::default();
    };

    fs::read_to_string(path)
        .ok()
        .and_then(|css| generated_settings_from_css(&css))
        .unwrap_or_default()
}

pub fn save_generated_theme(path: &Path, settings: &ThemeSettings) -> io::Result<String> {
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let updated = css_with_auto_generated_theme(&existing, settings);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &updated)?;
    Ok(updated)
}

pub fn css_with_auto_generated_theme(existing: &str, settings: &ThemeSettings) -> String {
    replace_auto_generated_css(existing, &generated_theme_css(settings))
}

pub fn replace_auto_generated_css(existing: &str, generated_css: &str) -> String {
    let block = format!("{AUTO_GEN_START}\n{}\n{AUTO_GEN_END}", generated_css.trim());

    if let Some(start) = existing.find(AUTO_GEN_START)
        && let Some(end_offset) = existing[start..].find(AUTO_GEN_END)
    {
        let end = start + end_offset + AUTO_GEN_END.len();
        let mut updated = String::with_capacity(existing.len() + block.len());
        updated.push_str(&existing[..start]);
        updated.push_str(&block);
        updated.push_str(&existing[end..]);
        return ensure_trailing_newline(updated);
    }

    if existing.trim().is_empty() {
        ensure_trailing_newline(block)
    } else {
        format!("{block}\n\n{existing}")
    }
}

pub fn generated_theme_css(settings: &ThemeSettings) -> String {
    let radius = settings.corner_radius.clamp(0, 18);
    let window_background = rgba_css(&settings.window_background);
    let panel_background = rgba_css(&settings.panel_background);
    let muted_background = rgba_css(&settings.muted_background);
    let accent = rgba_css(&settings.accent);
    let selection = rgba_css(&settings.selection);
    let text = rgba_css(&settings.text);
    let border = rgba_css(&settings.border);

    format!(
        "/* theme.window_background={window_background} */\n\
/* theme.panel_background={panel_background} */\n\
/* theme.muted_background={muted_background} */\n\
/* theme.accent={accent} */\n\
/* theme.selection={selection} */\n\
/* theme.text={text} */\n\
/* theme.border={border} */\n\
/* theme.corner_radius={radius} */\n\
\n\
window {{\n\
  background: {window_background};\n\
  color: {text};\n\
}}\n\
\n\
.topbar,\n\
.tab-strip,\n\
.sidebar,\n\
.start-menu-surface {{\n\
  background: {panel_background};\n\
  border-color: {border};\n\
}}\n\
\n\
.toolbar-group,\n\
.breadcrumbs,\n\
.path-stack,\n\
.file-tab,\n\
.start-menu-search,\n\
.start-menu-launcher,\n\
.start-menu-result,\n\
.start-menu-power,\n\
.start-menu-footer {{\n\
  background: {muted_background};\n\
}}\n\
\n\
.toolbar-group,\n\
.breadcrumbs,\n\
.path-stack,\n\
.sidebar-nav-button,\n\
.content-list row,\n\
.computer-volume-list row,\n\
.settings-tabs button,\n\
.settings-actions-list row,\n\
.context-menu,\n\
.context-menu-item,\n\
.file-tab,\n\
.tab-new-button,\n\
.start-menu-surface,\n\
.start-menu-search,\n\
.start-menu-launcher,\n\
.start-menu-result,\n\
.start-menu-power {{\n\
  border-radius: {radius}px;\n\
}}\n\
\n\
.file-tab-button:checked,\n\
.sidebar-nav-button:checked,\n\
.sidebar-list row:selected,\n\
.content-list row.entry-selected,\n\
.content-grid flowboxchild.entry-selected,\n\
.start-menu-launcher:active,\n\
.start-menu-result:active {{\n\
  background: {selection};\n\
}}\n\
\n\
.start-menu-window,\n\
.start-menu-surface,\n\
.start-menu-search,\n\
.start-menu-launcher,\n\
.start-menu-result,\n\
.start-menu-user,\n\
.start-menu-section-title {{\n\
    color: {text};\n\
}}\n\
\n\
.start-menu-surface {{\n\
    box-shadow: 0 26px 58px alpha(black, 0.36);\n\
}}\n\
\n\
.start-menu-backdrop {{\n\
    background: alpha({window_background}, 0.4);\n\
}}\n\
\n\
.start-menu-search,\n\
.start-menu-launcher,\n\
.start-menu-result,\n\
.start-menu-power,\n\
.start-menu-footer {{\n\
    border: 1px solid alpha({border}, 0.9);\n\
}}\n\
\n\
.start-menu-launcher:hover,\n\
.start-menu-result:hover,\n\
.start-menu-power:hover {{\n\
    background: alpha({accent}, 0.18);\n\
}}\n\
\n\
.computer-volume-progress progress {{\n\
  background: {accent};\n\
}}\n\
\n\
.status-label,\n\
.settings-value,\n\
.settings-action-command {{\n\
  color: alpha({text}, 0.72);\n\
}}"
    )
}

pub fn generated_settings_from_css(css: &str) -> Option<ThemeSettings> {
    let block = auto_generated_block(css)?;
    let mut settings = ThemeSettings::default();
    let mut found_any = false;

    for line in block.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix("/* theme.") else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let Some(value) = value.strip_suffix(" */") else {
            continue;
        };

        match name {
            "window_background" => found_any |= set_color(&mut settings.window_background, value),
            "panel_background" => found_any |= set_color(&mut settings.panel_background, value),
            "muted_background" => found_any |= set_color(&mut settings.muted_background, value),
            "accent" => found_any |= set_color(&mut settings.accent, value),
            "selection" => found_any |= set_color(&mut settings.selection, value),
            "text" => found_any |= set_color(&mut settings.text, value),
            "border" => found_any |= set_color(&mut settings.border, value),
            "corner_radius" => {
                if let Ok(radius) = value.parse::<i32>() {
                    settings.corner_radius = radius.clamp(0, 18);
                    found_any = true;
                }
            }
            _ => {}
        }
    }

    found_any.then_some(settings)
}

fn auto_generated_block(css: &str) -> Option<&str> {
    let start = css.find(AUTO_GEN_START)? + AUTO_GEN_START.len();
    let end = css[start..].find(AUTO_GEN_END)? + start;
    Some(&css[start..end])
}

fn set_color(target: &mut gtk::gdk::RGBA, value: &str) -> bool {
    match gtk::gdk::RGBA::parse(value.trim()) {
        Ok(color) => {
            *target = color;
            true
        }
        Err(error) => {
            tracing::warn!(value, %error, "failed to parse generated theme color");
            false
        }
    }
}

fn rgba_css(color: &gtk::gdk::RGBA) -> String {
    let red = color_component(color.red());
    let green = color_component(color.green());
    let blue = color_component(color.blue());
    let alpha = color.alpha().clamp(0.0, 1.0);

    if alpha >= 0.999 {
        format!("rgb({red},{green},{blue})")
    } else {
        format!("rgba({red},{green},{blue},{})", alpha_css(alpha))
    }
}

fn color_component(value: f32) -> i32 {
    (value.clamp(0.0, 1.0) * 255.0).round() as i32
}

fn alpha_css(alpha: f32) -> String {
    let mut text = format!("{alpha:.3}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn rgba255(red: u8, green: u8, blue: u8, alpha: f32) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(
        f32::from(red) / 255.0,
        f32::from(green) / 255.0,
        f32::from(blue) / 255.0,
        alpha,
    )
}

fn ensure_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepends_generated_block_when_missing() {
        let css = replace_auto_generated_css(".extra { color: red; }\n", "window { color: blue; }");

        assert!(css.starts_with(AUTO_GEN_START));
        assert!(css.contains("window { color: blue; }"));
        assert!(css.ends_with(".extra { color: red; }\n"));
    }

    #[test]
    fn replaces_existing_generated_block() {
        let existing =
            format!(".before {{}}\n{AUTO_GEN_START}\nold css\n{AUTO_GEN_END}\n.after {{}}\n");

        let css = replace_auto_generated_css(&existing, "new css");

        assert!(css.contains(".before {}"));
        assert!(css.contains("new css"));
        assert!(!css.contains("old css"));
        assert!(css.contains(".after {}"));
    }

    #[test]
    fn reads_generated_theme_metadata() {
        let settings = ThemeSettings {
            accent: rgba255(255, 0, 128, 1.0),
            corner_radius: 12,
            ..Default::default()
        };
        let css = css_with_auto_generated_theme("", &settings);
        let parsed = generated_settings_from_css(&css).expect("theme metadata");

        assert_eq!(parsed.accent, settings.accent);
        assert_eq!(parsed.corner_radius, 12);
    }

    #[test]
    fn generated_theme_css_preserves_transparent_colors() {
        let settings = ThemeSettings {
            panel_background: gtk::gdk::RGBA::new(1.0, 0.0, 0.0, 0.0),
            ..Default::default()
        };

        let css = generated_theme_css(&settings);
        let wrapped = css_with_auto_generated_theme("", &settings);
        let parsed = generated_settings_from_css(&wrapped).expect("theme metadata");

        assert!(css.contains("/* theme.panel_background=rgba(255,0,0,0) */"));
        assert!(css.contains("background: rgba(255,0,0,0);"));
        assert_eq!(parsed.panel_background, settings.panel_background);
    }

    #[test]
    fn generated_theme_css_applies_selection_to_sidebar() {
        let css = generated_theme_css(&ThemeSettings::default());

        assert!(css.contains(".sidebar-nav-button:checked,"));
        assert!(css.contains(".sidebar-list row:selected,"));
    }

    #[test]
    fn generated_theme_css_applies_theme_to_start_menu() {
        let css = generated_theme_css(&ThemeSettings::default());

        assert!(css.contains(".start-menu-surface {"));
        assert!(css.contains(".start-menu-search,"));
        assert!(css.contains(".start-menu-launcher:hover,"));
    }
}
