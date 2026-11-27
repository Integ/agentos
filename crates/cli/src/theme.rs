//! Themes: one palette for every surface AgentOS draws, switched in one place.
//!
//! `agentos theme <名字>` (or the desktop's settings) writes the chosen
//! theme's id to `~/.config/agentos/theme`. Everything that draws reads it
//! from there: the desktop page (CSS variables), the terminals the desktop
//! opens (foot colors passed with `-o`), sway's borders and background (a
//! snippet the desktop session includes, also applied live), and the
//! workbench's accent. One table here is the only copy of the colors, so the
//! surfaces cannot drift apart.
//!
//! A missing or unknown theme file is the default theme, never an error: the
//! file is a preference, and nothing should refuse to start over it.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The theme file, in the AgentOS configuration directory.
pub const FILE: &str = "theme";
/// sway commands for the theme, included by the desktop session's config.
pub const SWAY_FILE: &str = "sway-theme.conf";

/// A terminal's sixteen colors. `regular[0]` is the background, so
/// full-screen programs blend into the window; `bright[0]` is a raised
/// panel and `bright[4]` the accent (dialogs in the text setup use them).
pub struct Terminal {
    pub background: &'static str,
    pub foreground: &'static str,
    pub selection: &'static str,
    pub regular: [&'static str; 8],
    pub bright: [&'static str; 8],
}

/// One theme. Colors are six hex digits without `#`.
pub struct Theme {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    /// The desktop's base color, under its glows.
    pub base: &'static str,
    /// The three soft glows of the desktop background.
    pub glow: [&'static str; 3],
    /// Filled controls: approve buttons, the selected item. White text sits
    /// on it, so it is never a light color.
    pub accent: &'static str,
    /// Highlights on dark glass: focus rings, the cursor, links.
    pub soft: &'static str,
    /// Window borders: focused, then everything else.
    pub frame: [&'static str; 2],
    pub terminal: Terminal,
}

pub const THEMES: &[Theme] = &[
    Theme {
        id: "night",
        name: "夜空",
        summary: "深蓝玻璃，默认",
        base: "0b0f1c",
        glow: ["1f5bd8", "0f9b8e", "3a2a6b"],
        accent: "3b82f6",
        soft: "7cc4ff",
        frame: ["3a4a6b", "222d44"],
        terminal: Terminal {
            background: "0b1220",
            foreground: "e6ecf8",
            selection: "2c4a7e",
            regular: [
                "0b1220", "ff7a7a", "6ee7a8", "ffb86b", "5b9bff", "c4a7ff", "7cc4ff", "c3cde0",
            ],
            bright: [
                "172238", "ffb3b5", "9ff0c4", "ffd2a1", "3b82f6", "ddc9ff", "b5ddff", "ffffff",
            ],
        },
    },
    Theme {
        id: "forest",
        name: "森林",
        summary: "墨绿和青，安静",
        base: "0a120e",
        glow: ["15803d", "0f766e", "3f4a12"],
        accent: "16a34a",
        soft: "86efac",
        frame: ["2f5a43", "1c3327"],
        terminal: Terminal {
            background: "0d1712",
            foreground: "e3efe7",
            selection: "25523a",
            regular: [
                "0d1712", "f87171", "6ee7a8", "facc6b", "6aa9ff", "c4a7ff", "5eead4", "c5d6cb",
            ],
            bright: [
                "16261d", "fca5a5", "a7f3d0", "fde68a", "16a34a", "ddd6fe", "99f6e4", "ffffff",
            ],
        },
    },
    Theme {
        id: "dusk",
        name: "暮色",
        summary: "紫和玫红，傍晚",
        base: "120b1a",
        glow: ["6d28d9", "be185d", "1e3a8a"],
        accent: "9333ea",
        soft: "d8b4fe",
        frame: ["4b3566", "2c2140"],
        terminal: Terminal {
            background: "160f20",
            foreground: "f0e8f7",
            selection: "4a2b6b",
            regular: [
                "160f20", "fb7185", "86efac", "fdba74", "818cf8", "e879f9", "c4b5fd", "d6cce2",
            ],
            bright: [
                "241a33", "fda4af", "bbf7d0", "fed7aa", "9333ea", "f5d0fe", "ddd6fe", "ffffff",
            ],
        },
    },
    Theme {
        id: "ember",
        name: "炭火",
        summary: "暖橙和琥珀，夜里不刺眼",
        base: "140e0a",
        glow: ["c2410c", "a16207", "7f1d1d"],
        accent: "ea580c",
        soft: "fdba74",
        frame: ["5c4130", "33261c"],
        terminal: Terminal {
            background: "1a120d",
            foreground: "f5ebe3",
            selection: "5a3420",
            regular: [
                "1a120d", "f87171", "a3e635", "fbbf24", "7aa2f7", "f0abfc", "fdba74", "dccfc4",
            ],
            bright: [
                "2a1e16", "fca5a5", "d9f99d", "fde68a", "ea580c", "f5d0fe", "fed7aa", "ffffff",
            ],
        },
    },
    Theme {
        id: "mono",
        name: "石墨",
        summary: "只有灰，最少打扰",
        base: "0d0e10",
        glow: ["3f3f46", "27272a", "52525b"],
        accent: "52525b",
        soft: "d4d4d8",
        frame: ["52525b", "2a2b2f"],
        terminal: Terminal {
            background: "111214",
            foreground: "e8e8ea",
            selection: "3f3f46",
            regular: [
                "111214", "f87171", "86efac", "fcd34d", "93c5fd", "d8b4fe", "a5f3fc", "d4d4d8",
            ],
            bright: [
                "1f2023", "fca5a5", "bbf7d0", "fde68a", "71717a", "e9d5ff", "cffafe", "ffffff",
            ],
        },
    },
];

/// The theme used when none was chosen.
pub fn default_theme() -> &'static Theme {
    &THEMES[0]
}

/// The theme named `name`, by id (any case) or by its Chinese name.
pub fn find(name: &str) -> Option<&'static Theme> {
    let name = name.trim();
    THEMES
        .iter()
        .find(|theme| theme.id.eq_ignore_ascii_case(name) || theme.name == name)
}

/// The AgentOS configuration directory, given `XDG_CONFIG_HOME` and `HOME`.
/// A relative `XDG_CONFIG_HOME` is ignored, as the specification says.
pub fn config_dir_in(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_config_home.filter(|dir| Path::new(dir).is_absolute()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home.filter(|dir| Path::new(dir).is_absolute())?).join(".config"),
    };
    Some(base.join("agentos"))
}

/// The AgentOS configuration directory of this user.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_in(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The theme chosen in `dir`, or the default.
pub fn load(dir: &Path) -> &'static Theme {
    std::fs::read_to_string(dir.join(FILE))
        .ok()
        .and_then(|text| find(&text))
        .unwrap_or_else(default_theme)
}

/// The theme this user chose, or the default.
pub fn current() -> &'static Theme {
    config_dir().map_or_else(default_theme, |dir| load(&dir))
}

/// Choose `theme` in `dir`: the theme file and the sway snippet. The
/// directory also holds `agentos.env` with API keys, so one created here is
/// private to the user.
pub fn save(dir: &Path, theme: &Theme) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    let mut sway = format!(
        "# AgentOS 主题「{}」，由 agentos theme 生成，会被覆盖。\n",
        theme.name
    );
    for command in sway_commands(theme) {
        sway.push_str(&command);
        sway.push('\n');
    }
    agentos_core::durable::write_atomic(&dir.join(SWAY_FILE), sway.as_bytes())?;
    // The theme file last: once it names the theme, everything for it exists.
    agentos_core::durable::write_atomic(&dir.join(FILE), format!("{}\n", theme.id).as_bytes())
}

/// sway commands for the desktop session: background and window borders.
pub fn sway_commands(theme: &Theme) -> Vec<String> {
    let t = &theme.terminal;
    let [focused, other] = theme.frame;
    vec![
        format!("output * bg #{} solid_color", theme.base),
        format!(
            "client.focused #{focused} #{} #ffffff #{} #{focused}",
            t.background, theme.soft
        ),
        format!(
            "client.focused_inactive #{other} #{} #{} #{} #{other}",
            t.background, t.regular[7], theme.soft
        ),
        format!(
            "client.unfocused #{other} #{} #{} #{} #{other}",
            t.background, t.regular[7], theme.soft
        ),
    ]
}

/// Apply the theme to the running sway session, if there is one. Best
/// effort: the snippet takes effect at the next login anyway.
pub fn apply_sway(theme: &Theme) -> bool {
    if std::env::var_os("SWAYSOCK").is_none() {
        return false;
    }
    std::process::Command::new("swaymsg")
        .arg(sway_commands(theme).join("; "))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `-o` overrides that give foot the theme's colors; the rest of its
/// configuration (font, padding, transparency) stays the system's.
pub fn foot_overrides(theme: &Theme) -> Vec<String> {
    let t = &theme.terminal;
    let mut overrides = vec![
        format!("colors.background={}", t.background),
        format!("colors.foreground={}", t.foreground),
        format!("colors.selection-background={}", t.selection),
        "colors.selection-foreground=ffffff".to_string(),
    ];
    for (n, color) in t.regular.iter().enumerate() {
        overrides.push(format!("colors.regular{n}={color}"));
    }
    for (n, color) in t.bright.iter().enumerate() {
        overrides.push(format!("colors.bright{n}={color}"));
    }
    overrides
}

/// The theme as the desktop page takes it, with the others for its picker.
pub fn page(theme: &Theme) -> Value {
    let hex = |color: &str| format!("#{color}");
    json!({
        "type": "theme",
        "id": theme.id,
        "name": theme.name,
        "base": hex(theme.base),
        "glow": theme.glow.map(hex),
        "accent": hex(theme.accent),
        "soft": hex(theme.soft),
        "themes": THEMES.iter().map(|t| json!({
            "id": t.id,
            "name": t.name,
            "summary": t.summary,
            "accent": hex(t.accent),
            "glow": t.glow.map(hex),
        })).collect::<Vec<_>>(),
    })
}

/// `agentos theme`: list the themes, or switch to `choice` and tell the
/// running desktop and sway. Returns what to print.
pub fn command(choice: Option<&str>, as_json: bool) -> io::Result<String> {
    let dir = config_dir().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "没有 HOME，不知道把主题存在哪里")
    })?;
    let Some(theme) = choice.and_then(find) else {
        return Ok(listing(load(&dir), as_json));
    };
    save(&dir, theme)?;
    crate::style::set_theme(theme);
    let desktop = crate::desktop::send(crate::desktop::Signal::Theme).is_ok();
    let sway = apply_sway(theme);
    if as_json {
        return Ok(json!({"current": theme.id, "desktop": desktop, "sway": sway}).to_string());
    }
    let mut lines = vec![format!("已换成「{}」主题（{}）。", theme.name, theme.id)];
    // Only what was actually reached is said to have changed.
    match (desktop, sway) {
        (true, true) => lines.push("桌面、窗口边框和背景已经换好。".into()),
        (true, false) => lines.push("桌面已经换好。".into()),
        (false, true) => lines.push("窗口边框和背景已经换好。".into()),
        (false, false) => {}
    }
    lines.push(
        "之后打开的终端窗口用新配色，已经开着的不变；其他开着的工作台重新打开后换成新的强调色。"
            .into(),
    );
    Ok(lines.join("\n"))
}

fn listing(current: &Theme, as_json: bool) -> String {
    if as_json {
        return json!({
            "current": current.id,
            "themes": THEMES.iter().map(|t| json!({
                "id": t.id, "name": t.name, "summary": t.summary, "accent": format!("#{}", t.accent),
            })).collect::<Vec<_>>(),
        })
        .to_string();
    }
    use crate::style::{paint, swatch, Tone};
    let mut lines = vec![paint(
        &format!("主题（当前：{}）", current.name),
        Tone::Strong,
    )];
    for theme in THEMES {
        let mark = if theme.id == current.id { "●" } else { " " };
        let block = swatch(accent_rgb(theme));
        let block = if block.is_empty() {
            block
        } else {
            format!("{block} ")
        };
        lines.push(format!(
            "{mark} {block}{}  {:<7}{}",
            theme.name,
            theme.id,
            paint(theme.summary, Tone::Quiet)
        ));
    }
    lines.push(String::new());
    lines.push("agentos theme <名字> 切换；桌面、终端、窗口边框和工作台一起换。".into());
    lines.join("\n")
}

/// The theme's accent as 8-bit red, green and blue.
pub fn accent_rgb(theme: &Theme) -> (u8, u8, u8) {
    rgb(theme.accent)
}

/// The theme's soft highlight as 8-bit red, green and blue.
pub fn soft_rgb(theme: &Theme) -> (u8, u8, u8) {
    rgb(theme.soft)
}

fn rgb(color: &str) -> (u8, u8, u8) {
    let channel = |at: usize| u8::from_str_radix(&color[at..at + 2], 16).unwrap_or(0);
    (channel(0), channel(2), channel(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agentos-theme-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn every_color_is_six_hex_digits_and_ids_and_names_are_unique() {
        let hex = |c: &str| c.len() == 6 && c.chars().all(|ch| ch.is_ascii_hexdigit());
        for theme in THEMES {
            let t = &theme.terminal;
            let mut colors = vec![theme.base, theme.accent, theme.soft];
            colors.extend(theme.glow);
            colors.extend(theme.frame);
            colors.extend([t.background, t.foreground, t.selection]);
            colors.extend(t.regular);
            colors.extend(t.bright);
            assert!(colors.into_iter().all(hex), "{}", theme.id);
            // The terminal background is the first slot, so full-screen
            // programs blend into the window.
            assert_eq!(t.regular[0], t.background, "{}", theme.id);
            assert_eq!(find(theme.id).unwrap().id, theme.id);
            assert_eq!(find(theme.name).unwrap().id, theme.id);
        }
        let mut ids: Vec<_> = THEMES.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), THEMES.len());
    }

    #[test]
    fn white_text_stays_readable_on_every_accent() {
        // Approve buttons put white text on the accent: WCAG's 3:1 for
        // large and bold text is the floor.
        let luminance = |(r, g, b): (u8, u8, u8)| {
            let linear = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        };
        for theme in THEMES {
            let ratio = 1.05 / (luminance(accent_rgb(theme)) + 0.05);
            assert!(ratio >= 3.0, "{}: {ratio:.2}", theme.id);
        }
    }

    #[test]
    fn the_config_directory_follows_xdg_and_ignores_relative_paths() {
        assert_eq!(
            config_dir_in(Some("/x/cfg"), Some("/home/a")),
            Some(PathBuf::from("/x/cfg/agentos"))
        );
        assert_eq!(
            config_dir_in(Some("cfg"), Some("/home/a")),
            Some(PathBuf::from("/home/a/.config/agentos"))
        );
        assert_eq!(config_dir_in(None, Some("relative")), None);
        assert_eq!(config_dir_in(None, None), None);
    }

    #[test]
    fn a_missing_or_unknown_choice_is_the_default_and_a_saved_one_reads_back() {
        let dir = temp_dir("roundtrip");
        assert_eq!(load(&dir).id, default_theme().id);
        save(&dir, find("森林").unwrap()).unwrap();
        assert_eq!(load(&dir).id, "forest");
        let sway = std::fs::read_to_string(dir.join(SWAY_FILE)).unwrap();
        assert!(sway.contains("output * bg #0a120e solid_color"), "{sway}");
        assert!(sway.contains("client.focused #2f5a43"), "{sway}");
        std::fs::write(dir.join(FILE), "neon\n").unwrap();
        assert_eq!(load(&dir).id, default_theme().id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the directory also holds API keys");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn foot_gets_every_terminal_color_and_nothing_else() {
        let overrides = foot_overrides(find("dusk").unwrap());
        assert_eq!(overrides.len(), 4 + 16);
        assert!(overrides.iter().all(|o| o.starts_with("colors.")));
        assert!(overrides.contains(&"colors.bright4=9333ea".to_string()));
    }

    #[test]
    fn the_page_gets_css_colors_and_every_theme_for_its_picker() {
        let page = page(find("ember").unwrap());
        assert_eq!(page["type"], "theme");
        assert_eq!(page["accent"], "#ea580c");
        assert_eq!(page["glow"][0], "#c2410c");
        assert_eq!(page["themes"].as_array().unwrap().len(), THEMES.len());
    }
}
