use ratatui::style::Color;

/// Neutral fill for the currently focused row in modal menus. Keeping this
/// separate from the modal surface makes keyboard focus visible without
/// changing the semantic foreground/accent roles.
pub(crate) const MENU_SELECTION_BG: (u8, u8, u8) = (0x2A, 0x2A, 0x2A);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorDepth {
    TrueColor,
    Ansi256,
    Ansi16,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub color_depth: ColorDepth,
    pub mouse: bool,
    pub clipboard: bool,
    pub images: bool,
    pub reduced_motion: bool,
}

/// Semantic theme tokens (DESIGN-SLIM-TUI §21.1/§21.3).
/// Values are normative truecolor RGB; `to_terminal_color` degrades per depth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub background: (u8, u8, u8),
    pub foreground: (u8, u8, u8),
    pub muted: (u8, u8, u8),
    pub secondary_text: (u8, u8, u8),
    pub accent: (u8, u8, u8),
    pub surface: (u8, u8, u8),
    pub surface_alt: (u8, u8, u8),
    pub composer_bg: (u8, u8, u8),
    pub user_prompt_bg: (u8, u8, u8),
    pub border: (u8, u8, u8),
    pub border_focus: (u8, u8, u8),
    pub operational_divider: (u8, u8, u8),
    pub scrollbar_track: (u8, u8, u8),
    pub scrollbar_thumb: (u8, u8, u8),
    pub user_accent: (u8, u8, u8),
    pub assistant_accent: (u8, u8, u8),
    pub thinking_accent: (u8, u8, u8),
    pub heading_accent: (u8, u8, u8),
    pub link_accent: (u8, u8, u8),
    pub tool_accent: (u8, u8, u8),
    pub success: (u8, u8, u8),
    pub warning: (u8, u8, u8),
    pub error: (u8, u8, u8),
    pub code_rail: (u8, u8, u8),
    pub selection: (u8, u8, u8),
    pub diff_add: (u8, u8, u8),
    pub diff_remove: (u8, u8, u8),
    pub diff_add_bg: (u8, u8, u8),
    pub diff_remove_bg: (u8, u8, u8),
    pub diff_add_emphasis_bg: (u8, u8, u8),
    pub diff_remove_emphasis_bg: (u8, u8, u8),
    pub code_bg: (u8, u8, u8),
}

pub fn env_flag_enabled(name: &str, default_enabled: bool) -> bool {
    env_flag_from_value(std::env::var_os(name).as_deref(), default_enabled)
}

pub fn env_flag_from_value(value: Option<&std::ffi::OsStr>, default_enabled: bool) -> bool {
    match value.and_then(|flag| flag.to_str()) {
        None => default_enabled,
        Some("0" | "off" | "false") => false,
        Some("1" | "on" | "true") => true,
        Some(_) => default_enabled,
    }
}

pub fn detect_capabilities() -> Capabilities {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let truecolor = std::env::var("COLORTERM")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
        .unwrap_or(false)
        || std::env::var_os("WT_SESSION").is_some();
    Capabilities {
        color_depth: if no_color {
            ColorDepth::None
        } else if truecolor {
            ColorDepth::TrueColor
        } else {
            ColorDepth::Ansi16
        },
        // Default on: capturing without in-app selection stole native copy/paste.
        // SLIM_MOUSE=0 restores the previous "leave the mouse to the terminal" mode.
        mouse: env_flag_enabled("SLIM_MOUSE", true),
        clipboard: cfg!(windows)
            || std::env::var_os("SLIM_CLIPBOARD").is_some_and(|value| value == "1"),
        images: std::env::var_os("SLIM_IMAGES").is_some_and(|value| value == "1"),
        reduced_motion: std::env::var_os("SLIM_REDUCED_MOTION").is_some_and(|v| v == "1"),
    }
}

pub fn resolve_theme(capabilities: Capabilities) -> Theme {
    let mut theme = Theme {
        // True-black transcript surface keeps the reading column quiet.
        background: (0x00, 0x00, 0x00),
        // Ivory text keeps long sessions comfortable without harsh pure white.
        foreground: (0xE8, 0xE5, 0xDB),
        muted: (0x99, 0x97, 0x8E),
        secondary_text: (0xBC, 0xB9, 0xAF),
        accent: (0x72, 0xCC, 0x91),
        surface: (0x00, 0x00, 0x00),
        // Overlays and secondary docks sit one quiet step above the transcript.
        surface_alt: (0x18, 0x18, 0x18),
        // Composer/footer share transcript depth; border carries shape.
        composer_bg: (0x00, 0x00, 0x00),
        user_prompt_bg: (0x10, 0x10, 0x10),
        border: (0x3A, 0x3A, 0x3A),
        border_focus: (0x72, 0xCC, 0x91),
        operational_divider: (0x3A, 0x3A, 0x3A),
        scrollbar_track: (0x24, 0x23, 0x1A),
        scrollbar_thumb: (0x62, 0x67, 0x56),
        user_accent: (0xBC, 0xB9, 0xAF),
        assistant_accent: (0x72, 0xCC, 0x91),
        thinking_accent: (0xAF, 0xA9, 0x9D),
        heading_accent: (0x91, 0xC2, 0x8F),
        link_accent: (0x7E, 0xAB, 0x7F),
        tool_accent: (0x72, 0xCC, 0x91),
        success: (0x72, 0xCC, 0x91),
        warning: (0xE7, 0xC1, 0x5A),
        error: (0xE8, 0x79, 0x73),
        code_rail: (0x68, 0x66, 0x5A),
        selection: (0x2A, 0x4C, 0x38),
        diff_add: (0x72, 0xCC, 0x91),
        diff_remove: (0xE8, 0x79, 0x73),
        diff_add_bg: (0x17, 0x2A, 0x1E),
        diff_remove_bg: (0x2A, 0x18, 0x16),
        diff_add_emphasis_bg: (0x22, 0x57, 0x34),
        diff_remove_emphasis_bg: (0x51, 0x2A, 0x25),
        code_bg: (0x0A, 0x0A, 0x0A),
    };
    if capabilities.color_depth == ColorDepth::None {
        let white = (255, 255, 255);
        theme.foreground = white;
        theme.muted = white;
        theme.secondary_text = white;
        theme.scrollbar_track = white;
        theme.scrollbar_thumb = white;
    }
    theme
}

/// Convert a token RGB to the best color the terminal supports.
pub fn to_terminal_color(depth: ColorDepth, rgb: (u8, u8, u8)) -> Color {
    match depth {
        ColorDepth::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
        ColorDepth::Ansi256 => Color::Indexed(to_ansi256(rgb)),
        ColorDepth::Ansi16 | ColorDepth::None => to_ansi16(rgb),
    }
}

fn to_ansi256(rgb: (u8, u8, u8)) -> u8 {
    let (r, g, b) = (rgb.0 as u16, rgb.1 as u16, rgb.2 as u16);
    // Cube candidate (6x6x6) and grayscale candidate; nearest wins so the
    // near-black surface levels stay distinguishable (spec §21.2).
    let levels = [0_u16, 95, 135, 175, 215, 255];
    let index = |value: u16| -> usize {
        levels
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| value.abs_diff(**level))
            .map(|(index, _)| index)
            .unwrap_or(0)
    };
    let (ir, ig, ib) = (index(r), index(g), index(b));
    let cube = (16 + 36 * ir + 6 * ig + ib) as u16;
    let cube_rgb = (levels[ir], levels[ig], levels[ib]);
    let gray_index = if r == g && g == b && r < 8 {
        0
    } else if r == g && g == b && r > 248 {
        23
    } else {
        ((r.max(g).max(b)).saturating_sub(8) / 10).min(23)
    };
    let gray_value = 8 + gray_index * 10;
    let gray = 232 + gray_index;
    let dist = |c: (u16, u16, u16)| {
        let (cr, cg, cb) = c;
        cr.abs_diff(r) + cg.abs_diff(g) + cb.abs_diff(b)
    };
    let cube_cost = dist(cube_rgb);
    let gray_cost = dist((gray_value, gray_value, gray_value));
    if gray_cost < cube_cost {
        gray as u8
    } else {
        cube as u8
    }
}

fn to_ansi16(rgb: (u8, u8, u8)) -> Color {
    let (r, g, b) = rgb;
    let max = r.max(g).max(b);
    if max < 48 {
        return Color::Black;
    }
    let channel_spread = r.max(g.max(b)).saturating_sub(r.min(g.min(b)));
    if channel_spread < 32 {
        return match max {
            0..=96 => Color::DarkGray,
            97..=176 => Color::Gray,
            _ => Color::White,
        };
    }
    // Preserve mixed hues: an amber warning must not degrade to error red.
    // Bright variants keep chromatic text readable on the dark surfaces.
    let high = |channel: u8| u16::from(channel) * 4 >= u16::from(max) * 3;
    if high(r) && high(g) {
        Color::Yellow
    } else if high(g) && high(b) {
        Color::LightCyan
    } else if high(r) && high(b) {
        Color::LightMagenta
    } else if g >= r && g >= b {
        Color::LightGreen
    } else if r >= g && r >= b {
        Color::LightRed
    } else {
        Color::LightBlue
    }
}

pub fn glyph(capabilities: Capabilities, preferred: char, ascii_fallback: char) -> char {
    if capabilities.color_depth == ColorDepth::None {
        ascii_fallback
    } else {
        preferred
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mouse_flag_defaults_on_and_can_be_disabled() {
        use std::ffi::OsStr;
        assert!(super::env_flag_from_value(None, true));
        assert!(!super::env_flag_from_value(Some(OsStr::new("0")), true));
        assert!(!super::env_flag_from_value(Some(OsStr::new("off")), true));
        assert!(super::env_flag_from_value(Some(OsStr::new("1")), false));
        assert!(super::env_flag_from_value(Some(OsStr::new("maybe")), true));
    }

    #[test]
    fn dark_palette_preserves_semantic_roles_and_fallbacks() {
        let capabilities = super::Capabilities {
            color_depth: super::ColorDepth::TrueColor,
            mouse: false,
            clipboard: false,
            images: false,
            reduced_motion: false,
        };
        let theme = super::resolve_theme(capabilities);

        assert_eq!(theme.background, (0x00, 0x00, 0x00));
        assert_eq!(theme.surface, (0x00, 0x00, 0x00));
        assert_eq!(theme.composer_bg, theme.surface);
        assert_eq!(theme.surface_alt, (0x18, 0x18, 0x18));
        assert_eq!(theme.user_prompt_bg, (0x10, 0x10, 0x10));
        assert_eq!(theme.code_bg, (0x0A, 0x0A, 0x0A));
        assert_eq!(theme.border, (0x3A, 0x3A, 0x3A));
        assert_eq!(theme.foreground, (0xE8, 0xE5, 0xDB));
        assert_eq!(theme.muted, (0x99, 0x97, 0x8E));
        assert_eq!(theme.secondary_text, (0xBC, 0xB9, 0xAF));
        assert_eq!(theme.accent, (0x72, 0xCC, 0x91));
        assert_eq!(theme.border_focus, theme.accent);
        assert_eq!(theme.assistant_accent, theme.accent);
        assert_eq!(theme.tool_accent, theme.accent);
        assert_eq!(theme.success, theme.accent);

        assert_eq!(
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.background),
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.surface)
        );
        assert_ne!(
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.background),
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.user_prompt_bg)
        );
        assert_eq!(
            super::to_terminal_color(super::ColorDepth::Ansi16, theme.accent),
            ratatui::style::Color::LightGreen
        );
        assert_eq!(
            super::to_terminal_color(super::ColorDepth::Ansi16, theme.warning),
            ratatui::style::Color::Yellow
        );
        assert_eq!(
            super::to_terminal_color(super::ColorDepth::Ansi16, theme.error),
            ratatui::style::Color::LightRed
        );

        let no_color = super::resolve_theme(super::Capabilities {
            color_depth: super::ColorDepth::None,
            ..capabilities
        });
        assert_eq!(no_color.foreground, (255, 255, 255));
        assert_eq!(no_color.muted, (255, 255, 255));
        assert_eq!(no_color.secondary_text, (255, 255, 255));
    }
}
