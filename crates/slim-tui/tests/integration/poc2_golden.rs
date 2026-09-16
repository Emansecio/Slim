use slim_tui::app::AppState;
use slim_tui::layout::plan;
use slim_tui::render::render;

#[test]
fn approved_surface_sizes_keep_fixed_rows_and_deterministic_status() {
    let state = AppState::new();
    for (width, height, operational_rows) in [
        (40, 8, 1),
        (80, 24, 2),
        (99, 12, 2),
        (100, 24, 2),
        (139, 12, 2),
        (140, 40, 2),
        (200, 40, 2),
    ] {
        let regions = plan(width, height, 2, false);
        assert_eq!(regions.todo.height, 2, "size={width}x{height}");
        assert_eq!(regions.todo_divider.height, 1, "size={width}x{height}");
        assert_eq!(
            regions.composer.height,
            slim_tui::layout::composer_height(height),
            "size={width}x{height}"
        );
        assert_eq!(regions.op_divider.height, 0, "size={width}x{height}");
        assert_eq!(
            regions.operational.height, operational_rows,
            "size={width}x{height}"
        );
        assert_eq!(
            render(&state, width, height).lines.last(),
            Some(&"desconectado · /login".to_string()),
            "size={width}x{height}"
        );
    }
}

#[test]
fn composer_height_grows_with_content_without_starving_small_viewports() {
    assert_eq!(slim_tui::layout::composer_height_for_lines(24, 1), 3);
    assert_eq!(slim_tui::layout::composer_height_for_lines(24, 5), 7);
    assert_eq!(slim_tui::layout::composer_height_for_lines(24, 9), 7);
    assert_eq!(slim_tui::layout::composer_height_for_lines(12, 5), 3);
    assert_eq!(slim_tui::layout::composer_height_for_lines(7, 5), 1);
}
