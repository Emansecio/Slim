use ratatui::style::Color;

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
        mouse: std::env::var_os("SLIM_MOUSE").is_some_and(|value| value == "1"),
        clipboard: cfg!(windows)
            || std::env::var_os("SLIM_CLIPBOARD").is_some_and(|value| value == "1"),
        images: std::env::var_os("SLIM_IMAGES").is_some_and(|value| value == "1"),
        reduced_motion: std::env::var_os("SLIM_REDUCED_MOTION").is_some_and(|v| v == "1"),
    }
}

pub fn resolve_theme(capabilities: Capabilities) -> Theme {
    let mut theme = Theme {
        // Normative truecolor palette (DESIGN-SLIM-TUI §21.3).
        background: (0x08, 0x0A, 0x0D),
        // text é cinza-suave (não branco): texto ~91% branco sobre fundo
        // near-black sofre halation e parece negrito (feedback W3, ref. Grok).
        foreground: (0xC6, 0xCD, 0xD5),
        muted: (0x74, 0x7B, 0x84),
        secondary_text: (0xA9, 0xB0, 0xB8),
        accent: (0x7D, 0xCF, 0xFF),
        surface: (0x0D, 0x10, 0x14),
        surface_alt: (0x12, 0x16, 0x1B),
        // W8: composer/footer share transcript depth; border carries shape.
        composer_bg: (0x0D, 0x10, 0x14),
        user_prompt_bg: (0x1B, 0x20, 0x26),
        border: (0x2B, 0x31, 0x39),
        border_focus: (0x4B, 0x78, 0x91),
        operational_divider: (0x25, 0x2B, 0x33),
        scrollbar_track: (0x15, 0x1A, 0x20),
        scrollbar_thumb: (0x4B, 0x55, 0x63),
        user_accent: (0xA9, 0xB0, 0xB8),
        assistant_accent: (0x78, 0xD9, 0x9B),
        thinking_accent: (0x9A, 0xA4, 0xAF),
        heading_accent: (0x82, 0xAF, 0xFF),
        link_accent: (0x8C, 0xB4, 0xFF),
        tool_accent: (0x7D, 0xCF, 0xFF),
        success: (0x78, 0xD9, 0x9B),
        warning: (0xE6, 0xB4, 0x50),
        error: (0xF0, 0x71, 0x78),
        code_rail: (0x46, 0x50, 0x5C),
        selection: (0x26, 0x3A, 0x30),
        diff_add: (0x78, 0xD9, 0x9B),
        diff_remove: (0xF0, 0x71, 0x78),
        diff_add_bg: (0x0B, 0x1A, 0x10),
        diff_remove_bg: (0x1C, 0x0D, 0x11),
        diff_add_emphasis_bg: (0x16, 0x3D, 0x22),
        diff_remove_emphasis_bg: (0x42, 0x18, 0x20),
        code_bg: (0x0B, 0x0E, 0x12),
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
    if g >= r && g >= b {
        Color::Green
    } else if r >= g && r >= b {
        Color::Red
    } else {
        Color::Blue
    }
}

pub fn glyph(capabilities: Capabilities, preferred: char, ascii_fallback: char) -> char {
    if capabilities.color_depth == ColorDepth::None {
        ascii_fallback
    } else {
        preferred
    }
}
