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
    /// The Ionix palette, matching the tokens at the top of the bundled sheet.
    fn default() -> Self {
        Self {
            window_background: rgba255(15, 10, 24, 0.74),
            panel_background: rgba255(26, 16, 37, 0.86),
            muted_background: rgba255(15, 10, 24, 0.32),
            accent: rgba255(168, 85, 247, 1.0),
            selection: rgba255(168, 85, 247, 0.34),
            text: rgba255(226, 217, 243, 0.94),
            border: rgba255(168, 85, 247, 0.2),
            corner_radius: 8,
        }
    }
}

/// The user's CSS, held in a provider we keep a handle on so the file can be
/// re-read into it after an external edit.
///
/// GTK takes its own reference once the provider is installed, so this handle
/// exists purely to reload — dropping it does not uninstall the stylesheet.
#[derive(Clone)]
pub struct UserCss {
    provider: gtk::CssProvider,
}

impl UserCss {
    /// Installs an empty provider just above `STYLE_PROVIDER_PRIORITY_USER`, so
    /// the user's CSS wins over the bundled stylesheet. `None` without a display.
    pub fn new() -> Option<Self> {
        let Some(display) = gtk::gdk::Display::default() else {
            tracing::warn!("no display available for the user CSS provider");
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

    pub fn load(&self, css: &str) {
        self.provider.load_from_string(css);
    }

    pub fn clear(&self) {
        self.provider.load_from_string("");
    }
}

/// Installs the bundled stylesheet and the user's CSS, returning the handle the
/// latter can be reloaded through.
pub fn install(config: &AppConfig) -> Option<UserCss> {
    install_bundled();

    let user_css = UserCss::new()?;
    if let Some(path) = effective_custom_css_path(config) {
        match fs::read_to_string(&path) {
            Ok(css) => user_css.load(&css),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(?path, %error, "failed to load custom CSS"),
        }
    }

    Some(user_css)
}

pub fn install_bundled() {
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
}

/// Forces the desktop surface transparent, above every other stylesheet.
///
/// Not themeable, deliberately. The bundled stylesheet paints `window` with an
/// opaque wash and a user theme is free to replace it with its own — but a
/// desktop that honoured either would hide the wallpaper it exists to sit on
/// top of. Priority matters more than specificity here: a user theme loads at
/// `PRIORITY_USER + 1`, so a `.desktop-window` rule sitting in the bundled CSS
/// at `PRIORITY_APPLICATION` loses to that theme's plain `window` rule no
/// matter how specific it is. This provider goes above both.
///
/// Scoped to `.desktop-window` and its own children, so it cannot leak into the
/// file manager's windows.
pub fn install_desktop_transparency() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };

    let provider = gtk::CssProvider::new();
    // The window itself keeps the faintest possible tint rather than going
    // fully transparent. A window that paints nothing produces no content for
    // the compositor to hit-test, and a screen holding no icons then receives
    // no pointer events at all — no context menu, and nothing to drop onto.
    // One part in 255 forces a real full-surface paint and is not perceptible:
    // measured against a wallpaper it moves mean brightness by under half a
    // level out of 255. Everything inside stays genuinely transparent.
    provider.load_from_string(
        ".desktop-window {\n\
           background-color: rgba(0, 0, 0, 0.004);\n\
           background-image: none;\n\
           box-shadow: none;\n\
         }\n\
         .desktop-window > *,\n\
         .desktop-window > * > *,\n\
         .desktop-surface {\n\
           background: none;\n\
           background-color: transparent;\n\
           background-image: none;\n\
           box-shadow: none;\n\
         }\n",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 2,
    );
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

    crate::config::write_atomic(path, &updated)?;
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

/// The generated block: the eight base tokens, set where the bundled sheet
/// declares them. Every surface in the bundled sheet is painted from these, so
/// the block never has to name a widget.
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
window,\n\
tooltip {{\n\
  --io-window-bg: {window_background};\n\
  --io-panel-bg: {panel_background};\n\
  --io-muted-bg: {muted_background};\n\
  --io-accent: {accent};\n\
  --io-selection: {selection};\n\
  --io-text: {text};\n\
  --io-border: {border};\n\
  --io-radius: {radius}px;\n\
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
        assert!(css.contains("--io-panel-bg: rgba(255,0,0,0);"));
        assert_eq!(parsed.panel_background, settings.panel_background);
    }

    #[test]
    fn generated_theme_css_sets_every_base_token() {
        let css = generated_theme_css(&ThemeSettings::default());

        for token in [
            "--io-window-bg:",
            "--io-panel-bg:",
            "--io-muted-bg:",
            "--io-accent:",
            "--io-selection:",
            "--io-text:",
            "--io-border:",
            "--io-radius: 8px;",
        ] {
            assert!(css.contains(token), "missing {token}");
        }
    }

    /// The generated block only sets tokens, so it reaches a surface only if the
    /// bundled sheet paints that surface from them.
    #[test]
    fn the_bundled_sheet_declares_the_tokens_the_theme_sets() {
        let defaults = generated_theme_css(&ThemeSettings::default());
        let declarations = defaults
            .lines()
            .filter_map(|line| line.trim().split_once(':'))
            .map(|(name, _)| name)
            .filter(|name| name.starts_with("--io-"));

        for name in declarations {
            assert!(
                BUNDLED_CSS.contains(&format!("  {name}: ")),
                "the bundled sheet does not declare {name}"
            );
        }
    }

    #[test]
    fn the_bundled_sheet_paints_every_program_from_the_tokens() {
        for rule in [
            ".sidebar-list row:selected",
            ".start-menu-surface",
            ".spotlight-surface",
            ".spotlight-preview",
            ".spotlight-row:selected",
            ".quick-card",
        ] {
            let body = rule_body(rule).unwrap_or_else(|| panic!("no rule for {rule}"));
            assert!(body.contains("var(--io-"), "{rule} is not themed: {body}");
        }
    }

    /// The error bubble keeps its own red so it stays legible under any
    /// palette; if someone themes it later this test should fail loudly.
    #[test]
    fn the_chat_error_and_approval_keep_their_own_colours() {
        for rule in [".spotlight-chat-error", ".spotlight-chat-approval"] {
            let body = rule_body(rule).unwrap_or_else(|| panic!("no rule for {rule}"));
            assert!(!body.contains("var(--io-"), "{rule} follows the palette");
        }
    }

    /// The body of the first rule whose selector list ends with `selector`.
    fn rule_body(selector: &str) -> Option<&'static str> {
        let start = BUNDLED_CSS.find(&format!("{selector} {{"))?;
        let body = &BUNDLED_CSS[start..];
        let end = body.find('}')?;
        Some(&body[..end])
    }
}
