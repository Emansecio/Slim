use slim_tui::app::AppState;
use slim_tui::layout::plan;
use slim_tui::render::render;

#[test]
fn approved_surface_sizes_keep_fixed_rows_and_deterministic_status() {
    let state = AppState::new();
    for (width, height) in [
        (40, 8),
        (80, 24),
        (99, 12),
        (100, 24),
        (139, 12),
        (140, 40),
        (200, 40),
    ] {
        let regions = plan(width, height, 2, false);
        assert_eq!(regions.todo.height, 2, "size={width}x{height}");
        assert_eq!(regions.todo_divider.height, 1, "size={width}x{height}");
        assert_eq!(
            regions.composer.height,
            slim_tui::layout::composer_height(height),
            "size={width}x{height}"
        );
        assert_eq!(regions.op_divider.height, 1, "size={width}x{height}");
        assert_eq!(regions.operational.height, 1, "size={width}x{height}");
        assert_eq!(
            render(&state, width, height).lines.last(),
            Some(&"SLIM  Auto · signed out · /login  GPT-5.6 Sol · high · ↑0 ↓0".to_string()),
            "size={width}x{height}"
        );
    }
}
