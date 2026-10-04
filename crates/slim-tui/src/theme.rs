use ratatui::style::Color;

/// Neutral fill for the currently focused row in modal menus. Keeping this
/// separate from the modal surface makes keyboard focus visible without
/// changing the semantic foreground/accent roles.
pub(crate) const MENU_SELECTION_BG: (u8, u8, u8) = (0x2A, 0x2A, 0x2A);
/// A short, event-driven acknowledgement of keyboard focus movement.
pub(crate) const MENU_FOCUS_FLASH_BG: (u8, u8, u8) = (0x34, 0x3B, 0x43);

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
    /// Reasoning and planning: thought rows and the Plan mode. A violet grey
    /// at the lightness of `secondary_text`, so it separates thought from
    /// action by hue, not by brightness.
    pub reasoning_accent: (u8, u8, u8),
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
    /// Foreground of everything behind a centered modal.
    pub backdrop: (u8, u8, u8),
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
        reduced_motion: reduced_motion_from(
            std::env::var_os("SLIM_REDUCED_MOTION").as_deref(),
            os_animations_enabled(),
        ),
    }
}

/// Motion preference. An explicit `SLIM_REDUCED_MOTION=1` or `=0` always
/// decides; without it the operating system's "animation effects" setting
/// does, and an unknown setting keeps motion on.
pub fn reduced_motion_from(
    explicit: Option<&std::ffi::OsStr>,
    os_animations_enabled: Option<bool>,
) -> bool {
    match explicit.and_then(|value| value.to_str()) {
        Some("1") => true,
        Some("0") => false,
        _ => os_animations_enabled == Some(false),
    }
}

/// Windows "Show animations" (Settings, Accessibility, Visual effects).
/// `None` when the system does not answer.
#[cfg(windows)]
fn os_animations_enabled() -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION,
    };
    let mut enabled: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL (i32) through the
    // pointer, which stays valid and exclusive for the call.
    let answered = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            std::ptr::addr_of_mut!(enabled).cast(),
            0,
        )
    };
    (answered != 0).then_some(enabled != 0)
}

#[cfg(not(windows))]
fn os_animations_enabled() -> Option<bool> {
    None
}

pub fn resolve_theme(capabilities: Capabilities) -> Theme {
    let mut theme = Theme {
        // True-black transcript surface keeps the reading column quiet.
        background: (0x00, 0x00, 0x00),
        // Ivory text keeps long sessions comfortable without harsh pure white.
        foreground: (0xE8, 0xE5, 0xDB),
        muted: (0x99, 0x97, 0x8E),
        secondary_text: (0xBC, 0xB9, 0xAF),
        accent: (0x8A, 0xAD, 0xD4),
        surface: (0x00, 0x00, 0x00),
        // Overlays and secondary docks sit one quiet step above the transcript.
        surface_alt: (0x18, 0x18, 0x18),
        // Composer/footer share transcript depth; border carries shape.
        composer_bg: (0x00, 0x00, 0x00),
        // The user band and code sit one quiet step above the transcript so
        // turns read by depth rather than by color.
        user_prompt_bg: (0x14, 0x14, 0x14),
        border: (0x3A, 0x3A, 0x3A),
        // Cool focus is separate from green identity and success states.
        border_focus: (0x8A, 0xAD, 0xD4),
        operational_divider: (0x3A, 0x3A, 0x3A),
        scrollbar_track: (0x24, 0x23, 0x1A),
        scrollbar_thumb: (0x62, 0x67, 0x56),
        user_accent: (0xBC, 0xB9, 0xAF),
        assistant_accent: (0x72, 0xCC, 0x91),
        thinking_accent: (0xAF, 0xA9, 0x9D),
        reasoning_accent: (0xA9, 0x9F, 0xD6),
        // Headings carry hierarchy by weight on ivory; links use the cool
        // structural hue so green keeps meaning success and identity.
        heading_accent: (0xE8, 0xE5, 0xDB),
        link_accent: (0x8A, 0xAD, 0xD4),
        tool_accent: (0xBC, 0xB9, 0xAF),
        success: (0x72, 0xCC, 0x91),
        warning: (0xE7, 0xC1, 0x5A),
        error: (0xE8, 0x79, 0x73),
        code_rail: (0x68, 0x66, 0x5A),
        selection: (0x30, 0x38, 0x42),
        diff_add: (0x72, 0xCC, 0x91),
        diff_remove: (0xE8, 0x79, 0x73),
        diff_add_bg: (0x17, 0x2A, 0x1E),
        diff_remove_bg: (0x2A, 0x18, 0x16),
        diff_add_emphasis_bg: (0x22, 0x57, 0x34),
        diff_remove_emphasis_bg: (0x51, 0x2A, 0x25),
        code_bg: (0x14, 0x14, 0x14),
        backdrop: (0x4A, 0x4A, 0x4A),
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

/// Blend two token colors: `t = 0.0` is `from`, `t = 1.0` is `to`. Used for
/// the few shades that sit between two tokens, such as the sweep of the
/// thinking label.
pub(crate) fn mix_rgb(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    (lerp(from.0, to.0), lerp(from.1, to.1), lerp(from.2, to.2))
}

/// A terminal color moved `amount` of the way to white, re-quantized for
/// `depth`. `None` for colors that carry no RGB (named, reset, unset), where a
/// brighter shade cannot be computed.
pub(crate) fn lift_color(color: Color, depth: ColorDepth, amount: f32) -> Option<Color> {
    let rgb = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(index @ 16..=231) => {
            let levels = [0_u8, 95, 135, 175, 215, 255];
            let cube = usize::from(index - 16);
            (levels[cube / 36], levels[cube / 6 % 6], levels[cube % 6])
        }
        Color::Indexed(index @ 232..=255) => {
            let gray = 8 + 10 * (index - 232);
            (gray, gray, gray)
        }
        _ => return None,
    };
    Some(to_terminal_color(
        depth,
        mix_rgb(rgb, (255, 255, 255), amount),
    ))
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
    fn explicit_setting_beats_the_system_and_the_system_beats_the_default() {
        use std::ffi::OsStr;
        let on = Some(OsStr::new("1"));
        let off = Some(OsStr::new("0"));
        // The variable wins in both directions.
        assert!(super::reduced_motion_from(on, Some(true)));
        assert!(!super::reduced_motion_from(off, Some(false)));
        // Without it, the system's animation switch decides.
        assert!(super::reduced_motion_from(None, Some(false)));
        assert!(!super::reduced_motion_from(None, Some(true)));
        // Unknown system state or an unrecognized value keeps motion on.
        assert!(!super::reduced_motion_from(None, None));
        assert!(!super::reduced_motion_from(Some(OsStr::new("maybe")), None));
        assert!(super::reduced_motion_from(
            Some(OsStr::new("maybe")),
            Some(false)
        ));
    }

    #[test]
    fn mixing_two_tokens_is_exact_at_the_ends_and_clamps_outside_them() {
        let (black, white) = ((0, 0, 0), (200, 100, 50));
        assert_eq!(super::mix_rgb(black, white, 0.0), black);
        assert_eq!(super::mix_rgb(black, white, 1.0), white);
        assert_eq!(super::mix_rgb(black, white, 0.5), (100, 50, 25));
        assert_eq!(super::mix_rgb(black, white, -3.0), black);
        assert_eq!(super::mix_rgb(black, white, 7.0), white);
    }

    #[test]
    fn lifting_a_color_moves_it_toward_white_at_every_depth_that_has_shades() {
        use ratatui::style::Color;
        let rgb = Color::Rgb(100, 50, 0);
        assert_eq!(
            super::lift_color(rgb, super::ColorDepth::TrueColor, 0.0),
            Some(rgb)
        );
        assert_eq!(
            super::lift_color(rgb, super::ColorDepth::TrueColor, 1.0),
            Some(Color::Rgb(255, 255, 255))
        );
        assert_eq!(
            super::lift_color(rgb, super::ColorDepth::TrueColor, 0.5),
            Some(Color::Rgb(178, 153, 128))
        );
        // Indexed colors are read back from the xterm cube and gray ramp.
        for index in [16_u8, 59, 144, 231, 232, 244, 255] {
            assert_eq!(
                super::lift_color(Color::Indexed(index), super::ColorDepth::Ansi256, 0.0),
                Some(Color::Indexed(index)),
                "{index}"
            );
        }
        assert_eq!(
            super::lift_color(Color::Indexed(59), super::ColorDepth::Ansi256, 1.0),
            Some(Color::Indexed(231))
        );
        // Named, reset and the 16 system colors carry no RGB to lift.
        for color in [Color::Reset, Color::DarkGray, Color::Indexed(7)] {
            assert_eq!(
                super::lift_color(color, super::ColorDepth::Ansi256, 0.5),
                None
            );
        }
    }

    #[test]
    fn asking_the_system_for_its_animation_setting_does_not_panic() {
        // The answer depends on the machine; only the call itself is checked.
        let _ = super::os_animations_enabled();
    }

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
        assert_eq!(theme.user_prompt_bg, (0x14, 0x14, 0x14));
        assert_eq!(theme.code_bg, theme.user_prompt_bg);
        assert_eq!(theme.border, (0x3A, 0x3A, 0x3A));
        assert_eq!(theme.foreground, (0xE8, 0xE5, 0xDB));
        assert_eq!(theme.muted, (0x99, 0x97, 0x8E));
        assert_eq!(theme.secondary_text, (0xBC, 0xB9, 0xAF));
        assert_eq!(theme.accent, (0x8A, 0xAD, 0xD4));
        assert_eq!(theme.border_focus, theme.accent);
        assert_eq!(theme.heading_accent, theme.foreground);
        assert_eq!(theme.link_accent, theme.accent);
        assert_eq!(theme.assistant_accent, theme.success);
        assert_eq!(theme.tool_accent, theme.secondary_text);
        assert_ne!(theme.success, theme.accent);
        // Thought and plan have their own hue, apart from navigation blue,
        // and survive both quantizations as something other than a grey.
        assert_eq!(theme.reasoning_accent, (0xA9, 0x9F, 0xD6));
        assert_ne!(theme.reasoning_accent, theme.accent);
        assert_ne!(
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.reasoning_accent),
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.accent)
        );
        assert_eq!(
            super::to_terminal_color(super::ColorDepth::Ansi16, theme.reasoning_accent),
            ratatui::style::Color::LightMagenta
        );

        assert_eq!(
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.background),
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.surface)
        );
        // The raised user band must survive 256-color quantization.
        assert_ne!(
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.background),
            super::to_terminal_color(super::ColorDepth::Ansi256, theme.user_prompt_bg)
        );
        assert_eq!(
            super::to_terminal_color(super::ColorDepth::Ansi16, theme.accent),
            ratatui::style::Color::LightCyan
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
