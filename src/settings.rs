//! User settings, persisted to `~/.config/bishop/config.toml` and applied live.
//!
//! The current settings live in a main-thread global. Windows subscribe with `subscribe`,
//! and `update` saves the file and notifies every open window.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use gtk::glib;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub behavior: Behavior,
    pub agent: Agent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// Id of the selected theme: a built-in one, or `custom:<file stem>` for a saved one.
    pub theme: String,
    /// Use the colors below instead of the theme's. They are empty until first customized.
    pub custom_colors: bool,
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub selection: String,
    pub selection_text: String,
    /// The 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan, white, then
    /// their bright variants.
    pub palette: Vec<String>,
    pub font_family: String,
    pub font_size: u32,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    /// Opacity of the terminal background, in percent (0–100). Text stays fully opaque.
    pub background_opacity: u32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: BUILTIN_THEMES[0].id.to_owned(),
            custom_colors: false,
            foreground: String::new(),
            background: String::new(),
            cursor: String::new(),
            selection: String::new(),
            selection_text: String::new(),
            palette: Vec::new(),
            font_family: "Monospace".to_owned(),
            font_size: 12,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            background_opacity: 100,
        }
    }
}

/// A complete set of terminal colors, as `#rrggbb` strings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Colors {
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub selection: String,
    pub selection_text: String,
    pub palette: Vec<String>,
}

impl Appearance {
    /// The colors in effect: the custom set when enabled and complete, else the theme's.
    pub fn colors(&self) -> Colors {
        let theme = theme(&self.theme).colors;
        if !self.custom_colors {
            return theme;
        }
        let pick = |custom: &String, fallback: String| if custom.is_empty() { fallback } else { custom.clone() };
        Colors {
            foreground: pick(&self.foreground, theme.foreground),
            background: pick(&self.background, theme.background),
            cursor: pick(&self.cursor, theme.cursor),
            selection: pick(&self.selection, theme.selection),
            selection_text: pick(&self.selection_text, theme.selection_text),
            palette: if self.palette.len() == 16 { self.palette.clone() } else { theme.palette },
        }
    }

    /// Loads a theme: its colors (turning custom colors off) and its font.
    pub fn use_theme(&mut self, theme: &Theme) {
        self.theme = theme.id.clone();
        self.custom_colors = false;
        self.font_family = theme.font_family.clone();
        self.font_size = theme.font_size;
    }

    /// Whether custom colors have ever been set.
    pub fn has_custom_colors(&self) -> bool {
        self.palette.len() == 16 && !self.foreground.is_empty()
    }

    /// Copies every color from the current theme into the custom set.
    pub fn reset_custom_colors(&mut self) {
        let colors = theme(&self.theme).colors;
        self.foreground = colors.foreground;
        self.background = colors.background;
        self.cursor = colors.cursor;
        self.selection = colors.selection;
        self.selection_text = colors.selection_text;
        self.palette = colors.palette;
    }
}

pub const PALETTE_NAMES: [&str; 8] = ["Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "White"];

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    Block,
    Ibeam,
    Underline,
}

impl CursorShape {
    pub const ALL: [(CursorShape, &str); 3] = [
        (CursorShape::Block, "Block"),
        (CursorShape::Ibeam, "I-Beam"),
        (CursorShape::Underline, "Underline"),
    ];

    pub fn to_vte(self) -> vte4::CursorShape {
        match self {
            CursorShape::Block => vte4::CursorShape::Block,
            CursorShape::Ibeam => vte4::CursorShape::Ibeam,
            CursorShape::Underline => vte4::CursorShape::Underline,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Behavior {
    pub scrollback_lines: u32,
    pub audible_bell: bool,
    /// Empty means `$SHELL`.
    pub shell: String,
}

impl Default for Behavior {
    fn default() -> Self {
        Self { scrollback_lines: 10_000, audible_bell: false, shell: String::new() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agent {
    /// Empty means find `claude` on PATH.
    pub claude_path: String,
    /// Empty means Claude Code's default; otherwise passed to `--model`.
    pub model: String,
    pub automatic_mode: bool,
}

impl Agent {
    pub const MODELS: [(&str, &str); 4] =
        [("", "Default"), ("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")];

    pub fn claude_program(&self) -> Option<PathBuf> {
        if self.claude_path.is_empty() {
            // Launched from the app grid, PATH may lack ~/.local/bin; also check Claude Code's
            // usual install locations.
            let home = glib::home_dir();
            glib::find_program_in_path("claude").or_else(|| {
                [".local/bin/claude", ".claude/local/claude"].iter().map(|p| home.join(p)).find(|p| p.is_file())
            })
        } else {
            Some(PathBuf::from(&self.claude_path)).filter(|p| p.is_file())
        }
    }
}

/// A theme that ships with Bishop. All use the default font.
struct Builtin {
    id: &'static str,
    name: &'static str,
    foreground: &'static str,
    background: &'static str,
    cursor: &'static str,
    /// Background of selected text.
    selection: &'static str,
    palette: [&'static str; 16],
}

const BUILTIN_THEMES: &[Builtin] = &[
    Builtin {
        id: "bishop-dark",
        name: "Bishop Dark",
        foreground: "#e4dcf5",
        background: "#17131f",
        cursor: "#b77cff",
        selection: "#3d2f5c",
        palette: [
            "#241e30", "#ff5c7a", "#5fd7a7", "#f2c66d", "#7aa2f7", "#b77cff", "#5fd0e6", "#d6cfe6",
            "#4a4060", "#ff7d95", "#7fe8bf", "#ffd98a", "#9bb8ff", "#cf9dff", "#85e2f2", "#f5f0ff",
        ],
    },
    Builtin {
        id: "bishop-light",
        name: "Bishop Light",
        foreground: "#2a2238",
        background: "#faf8ff",
        cursor: "#7b3fd6",
        selection: "#dccdf5",
        palette: [
            "#2a2238", "#c4314b", "#2c8a5f", "#a26b00", "#3657c4", "#7b3fd6", "#1d7f93", "#c9c2d6",
            "#5a5070", "#e04760", "#37a573", "#c18a10", "#4e70e0", "#9558f0", "#2a9bb2", "#f2eefa",
        ],
    },
    Builtin {
        id: "tango-dark",
        name: "Tango Dark",
        foreground: "#d3d7cf",
        background: "#2e3436",
        cursor: "#d3d7cf",
        selection: "#555753",
        palette: [
            "#2e3436", "#cc0000", "#4e9a06", "#c4a000", "#3465a4", "#75507b", "#06989a", "#d3d7cf",
            "#555753", "#ef2929", "#8ae234", "#fce94f", "#729fcf", "#ad7fa8", "#34e2e2", "#eeeeec",
        ],
    },
    Builtin {
        id: "solarized-dark",
        name: "Solarized Dark",
        foreground: "#839496",
        background: "#002b36",
        cursor: "#93a1a1",
        selection: "#073642",
        palette: SOLARIZED,
    },
    Builtin {
        id: "solarized-light",
        name: "Solarized Light",
        foreground: "#657b83",
        background: "#fdf6e3",
        cursor: "#586e75",
        selection: "#eee8d5",
        palette: SOLARIZED,
    },
    Builtin {
        id: "nord",
        name: "Nord",
        foreground: "#d8dee9",
        background: "#2e3440",
        cursor: "#d8dee9",
        selection: "#434c5e",
        palette: [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
            "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
        ],
    },
    Builtin {
        id: "dracula",
        name: "Dracula",
        foreground: "#f8f8f2",
        background: "#282a36",
        cursor: "#f8f8f2",
        selection: "#44475a",
        palette: [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ],
    },
];

const SOLARIZED: [&str; 16] = [
    "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
    "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
];

const DEFAULT_FONT: &str = "Monospace";
const DEFAULT_FONT_SIZE: u32 = 12;

/// A complete look: colors plus font. Saved themes are TOML files in `themes_dir()`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    #[serde(skip)]
    pub id: String,
    pub name: String,
    #[serde(skip)]
    pub builtin: bool,
    pub font_family: String,
    pub font_size: u32,
    #[serde(flatten)]
    pub colors: Colors,
}

impl Builtin {
    fn theme(&self) -> Theme {
        Theme {
            id: self.id.to_owned(),
            name: self.name.to_owned(),
            builtin: true,
            font_family: DEFAULT_FONT.to_owned(),
            font_size: DEFAULT_FONT_SIZE,
            colors: Colors {
                foreground: self.foreground.to_owned(),
                background: self.background.to_owned(),
                cursor: self.cursor.to_owned(),
                selection: self.selection.to_owned(),
                selection_text: self.foreground.to_owned(),
                palette: self.palette.iter().map(|c| (*c).to_owned()).collect(),
            },
        }
    }
}

const CUSTOM_PREFIX: &str = "custom:";

/// Built-in themes first, then saved ones by name.
pub fn themes() -> Vec<Theme> {
    let mut themes: Vec<Theme> = BUILTIN_THEMES.iter().map(Builtin::theme).collect();
    themes.extend(CUSTOM_THEMES.with_borrow(Clone::clone));
    themes
}

/// The theme with `id`, or the default theme if there's none (e.g. it was deleted).
pub fn theme(id: &str) -> Theme {
    themes().into_iter().find(|t| t.id == id).unwrap_or_else(|| BUILTIN_THEMES[0].theme())
}

/// `~/.config/bishop/themes`
pub fn themes_dir() -> PathBuf {
    glib::user_config_dir().join("bishop").join("themes")
}

/// Saves `appearance`'s current colors and font as a theme named `name`, replacing a saved
/// theme of the same name, and returns it.
pub fn save_theme(name: &str, appearance: &Appearance) -> Result<Theme, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the theme a name".into());
    }
    if BUILTIN_THEMES.iter().any(|t| t.name.eq_ignore_ascii_case(name)) {
        return Err(format!("“{name}” is a built-in theme; choose another name"));
    }
    let theme =
        write_theme(&themes_dir(), name, appearance).map_err(|e| format!("Could not save the theme: {e}"))?;
    load_custom_themes();
    Ok(theme)
}

pub fn delete_theme(id: &str) -> Result<(), String> {
    let stem = id.strip_prefix(CUSTOM_PREFIX).ok_or("Built-in themes can't be deleted")?;
    std::fs::remove_file(themes_dir().join(format!("{stem}.toml")))
        .map_err(|e| format!("Could not delete the theme: {e}"))?;
    load_custom_themes();
    Ok(())
}

fn write_theme(dir: &Path, name: &str, appearance: &Appearance) -> std::io::Result<Theme> {
    let stem = slug(name);
    let theme = Theme {
        id: format!("{CUSTOM_PREFIX}{stem}"),
        name: name.to_owned(),
        builtin: false,
        font_family: appearance.font_family.clone(),
        font_size: appearance.font_size,
        colors: appearance.colors(),
    };
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(&theme).map_err(std::io::Error::other)?;
    std::fs::write(dir.join(format!("{stem}.toml")), text)?;
    Ok(theme)
}

fn read_themes(dir: &Path) -> Vec<Theme> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut themes: Vec<Theme> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|path| {
            let stem = path.file_stem()?.to_str()?.to_owned();
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str::<Theme>(&text).map_err(|e| e.to_string()))
                .and_then(|theme| validate(&theme).map(|()| theme));
            match parsed {
                Ok(theme) => Some(Theme { id: format!("{CUSTOM_PREFIX}{stem}"), ..theme }),
                Err(e) => {
                    eprintln!("bishop: skipping theme {}: {e}", path.display());
                    None
                }
            }
        })
        .collect();
    themes.sort_by_key(|t| t.name.to_lowercase());
    themes
}

fn validate(theme: &Theme) -> Result<(), String> {
    let c = &theme.colors;
    if c.palette.len() != 16 {
        return Err(format!("palette has {} colors, expected 16", c.palette.len()));
    }
    let named = [&c.foreground, &c.background, &c.cursor, &c.selection, &c.selection_text];
    match named.into_iter().chain(&c.palette).find(|color| gtk::gdk::RGBA::parse(color.as_str()).is_err()) {
        Some(bad) => Err(format!("invalid color {bad:?}")),
        None => Ok(()),
    }
}

/// A file-name-safe form of a theme name: `My Theme!` becomes `my-theme`.
fn slug(name: &str) -> String {
    let dashed: String = name.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let slug = dashed.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    if slug.is_empty() { "theme".to_owned() } else { slug }
}

fn load_custom_themes() {
    CUSTOM_THEMES.set(read_themes(&themes_dir()));
}

type Listener = Box<dyn Fn(&Settings) -> bool>;

thread_local! {
    static CURRENT: RefCell<Settings> = RefCell::new(Settings::default());
    static LISTENERS: RefCell<Vec<Listener>> = const { RefCell::new(Vec::new()) };
    static CUSTOM_THEMES: RefCell<Vec<Theme>> = const { RefCell::new(Vec::new()) };
}

pub fn path() -> PathBuf {
    glib::user_config_dir().join("bishop").join("config.toml")
}

/// Loads the config file into the current settings. A file that fails to parse is kept
/// as `config.toml.bak` so a later save doesn't silently destroy it.
pub fn load() {
    load_custom_themes();
    let path = path();
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    match toml::from_str::<Settings>(&text) {
        Ok(settings) => CURRENT.with_borrow_mut(|current| *current = settings),
        Err(e) => {
            eprintln!("bishop: ignoring invalid {}: {e}", path.display());
            let _ = std::fs::copy(&path, path.with_extension("toml.bak"));
        }
    }
}

pub fn get() -> Settings {
    CURRENT.with_borrow(Clone::clone)
}

/// Changes the settings, saves them, and notifies every subscriber.
pub fn update(change: impl FnOnce(&mut Settings)) {
    let settings = CURRENT.with_borrow_mut(|current| {
        let before = current.clone();
        change(current);
        (*current != before).then(|| current.clone())
    });
    let Some(settings) = settings else { return };
    if let Err(e) = save(&settings) {
        eprintln!("bishop: could not save {}: {e}", path().display());
    }
    // Take the listeners out while calling them so a listener may subscribe or update.
    let listeners = LISTENERS.take();
    let mut kept: Vec<Listener> = listeners.into_iter().filter(|l| l(&settings)).collect();
    LISTENERS.with_borrow_mut(|added| kept.append(added));
    LISTENERS.set(kept);
}

/// Calls `listener` on every change until it returns false.
pub fn subscribe(listener: impl Fn(&Settings) -> bool + 'static) {
    LISTENERS.with_borrow_mut(|listeners| listeners.push(Box::new(listener)));
}

fn save(settings: &Settings) -> std::io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(settings).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let text = toml::to_string_pretty(&Settings::default()).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), Settings::default());
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let settings: Settings = toml::from_str(
            "[appearance]\ntheme = \"nord\"\ncursor_shape = \"ibeam\"\n[agent]\nmodel = \"opus\"\n",
        )
        .unwrap();
        assert_eq!(settings.appearance.theme, "nord");
        assert_eq!(settings.appearance.cursor_shape, CursorShape::Ibeam);
        assert_eq!(settings.appearance.font_size, 12);
        assert_eq!(settings.behavior, Behavior::default());
        assert_eq!(settings.agent.model, "opus");
    }

    #[test]
    fn custom_colors_override_theme() {
        let mut appearance = Appearance { theme: "nord".into(), ..Appearance::default() };
        assert_eq!(appearance.colors(), theme("nord").colors);
        // Enabled but never customized: still the theme's colors.
        appearance.custom_colors = true;
        assert!(!appearance.has_custom_colors());
        assert_eq!(appearance.colors(), theme("nord").colors);
        appearance.reset_custom_colors();
        appearance.palette[1] = "#ff0000".into();
        appearance.selection = "#123456".into();
        let colors = appearance.colors();
        assert_eq!(colors.palette[1], "#ff0000");
        assert_eq!(colors.selection, "#123456");
        assert_eq!(colors.background, theme("nord").colors.background);
        // Disabled again: back to the theme, edits kept for next time.
        appearance.custom_colors = false;
        assert_eq!(appearance.colors(), theme("nord").colors);
        assert_eq!(appearance.palette[1], "#ff0000");
    }

    #[test]
    fn unknown_theme_falls_back_to_first() {
        assert_eq!(theme("no-such-theme").id, BUILTIN_THEMES[0].id);
    }

    #[test]
    fn builtin_themes_are_valid() {
        for theme in BUILTIN_THEMES.iter().map(Builtin::theme) {
            assert_eq!(validate(&theme), Ok(()), "{}", theme.id);
        }
    }

    #[test]
    fn saved_themes_round_trip_with_font() {
        let dir = std::env::temp_dir().join(format!("bishop-theme-test-{}", std::process::id()));
        let mut appearance =
            Appearance { font_family: "JetBrains Mono".into(), font_size: 15, ..Appearance::default() };
        appearance.reset_custom_colors();
        appearance.custom_colors = true;
        appearance.palette[2] = "#00ff00".into();

        let saved = write_theme(&dir, "My Theme!", &appearance).unwrap();
        assert_eq!(saved.id, "custom:my-theme");
        let loaded = read_themes(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(loaded, vec![saved.clone()]);
        assert_eq!(loaded[0].colors.palette[2], "#00ff00");

        // Selecting the theme brings back its colors and font.
        let mut other = Appearance::default();
        other.use_theme(&saved);
        assert_eq!((other.font_family.as_str(), other.font_size), ("JetBrains Mono", 15));
        assert!(!other.custom_colors);
    }

    #[test]
    fn skips_broken_theme_files() {
        let dir = std::env::temp_dir().join(format!("bishop-theme-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.toml"), "name = \"Bad\"\nfont_family = \"x\"\nfont_size = 12\n").unwrap();
        let loaded = read_themes(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn slugs_are_file_safe() {
        assert_eq!(slug("My Theme!"), "my-theme");
        assert_eq!(slug("  ../etc/passwd "), "etc-passwd");
        assert_eq!(slug("!!!"), "theme");
    }
}
