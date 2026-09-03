use slim_core::OperatingMode;
use slim_tui::image::{render_image, ImageCapabilities, ImageRender};
use slim_tui::input::cycle_mode;
use slim_tui::inspector::{CommandPalette, InspectorKind, InspectorState};
use slim_tui::theme::{detect_capabilities, glyph, resolve_theme, Capabilities, ColorDepth};

#[test]
fn mode_cycle_and_command_palette_are_deterministic() {
    assert_eq!(cycle_mode(OperatingMode::Auto), OperatingMode::ReadOnly);
    assert_eq!(cycle_mode(OperatingMode::ReadOnly), OperatingMode::Plan);
    assert_eq!(cycle_mode(OperatingMode::Plan), OperatingMode::Auto);
    let palette = CommandPalette {
        query: "/mode".into(),
    };
    assert_eq!(
        palette.matches(&["/mode", "/compact", "/model"]),
        vec!["/mode", "/model"]
    );
}

#[test]
fn inspectors_toggle_and_capabilities_degrade_safely() {
    let mut inspectors = InspectorState::default();
    inspectors.toggle(InspectorKind::Diagnostics);
    assert_eq!(inspectors.active, Some(InspectorKind::Diagnostics));
    inspectors.toggle(InspectorKind::Diagnostics);
    assert_eq!(inspectors.active, None);

    let caps = Capabilities {
        color_depth: ColorDepth::None,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    };
    assert_eq!(glyph(caps, '✓', '+'), '+');
    assert_eq!(resolve_theme(caps).foreground, (255, 255, 255));
    assert_eq!(resolve_theme(caps).background, (0x08, 0x0A, 0x0D));
    assert_eq!(
        render_image("img-1", ImageCapabilities { supported: false }),
        ImageRender::Placeholder {
            label: "[image unavailable: img-1]".into()
        }
    );
}

#[test]
fn capability_detection_defaults_to_safe_fallbacks() {
    let capabilities = detect_capabilities();
    assert!(matches!(
        capabilities.color_depth,
        ColorDepth::TrueColor | ColorDepth::Ansi16 | ColorDepth::None
    ));
    assert_eq!(resolve_theme(capabilities).background, (0x08, 0x0A, 0x0D));
}

#[test]
fn focused_composer_uses_the_normative_cyan_border() {
    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    };
    assert_eq!(resolve_theme(capabilities).border_focus, (0x4B, 0x78, 0x91));
}

#[test]
fn capability_matrix_covers_color_glyph_mouse_clipboard_and_image_fallbacks() {
    for depth in [
        ColorDepth::TrueColor,
        ColorDepth::Ansi256,
        ColorDepth::Ansi16,
        ColorDepth::None,
    ] {
        let capabilities = Capabilities {
            color_depth: depth,
            mouse: true,
            clipboard: true,
            images: true,
            reduced_motion: false,
        };
        let expected_glyph = if depth == ColorDepth::None {
            '+'
        } else {
            '✓'
        };
        assert_eq!(glyph(capabilities, '✓', '+'), expected_glyph);
        assert_eq!(resolve_theme(capabilities).background, (0x08, 0x0A, 0x0D));
        assert!(capabilities.mouse);
        assert!(capabilities.clipboard);
        assert_eq!(
            render_image("supported", ImageCapabilities { supported: true }),
            ImageRender::Inline {
                id: "supported".into()
            }
        );
    }
    assert_eq!(
        render_image("unsupported", ImageCapabilities { supported: false }),
        ImageRender::Placeholder {
            label: "[image unavailable: unsupported]".into()
        }
    );
}
