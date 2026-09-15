#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutRegions {
    pub session_rail: Rect,
    pub activity_rail: Rect,
    pub scrollback: Rect,
    pub todo: Rect,
    pub todo_divider: Rect,
    pub composer: Rect,
    pub op_divider: Rect,
    pub operational: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutError {
    TerminalTooSmall,
    InsufficientHeight,
}

/// Composer box height (§15.3): one to five content rows plus the border on
/// regular viewports. Compact and emergency layouts keep their fixed budget.
pub fn composer_height(viewport_height: u16) -> u16 {
    composer_height_for_lines(viewport_height, 1)
}

pub fn composer_height_for_lines(viewport_height: u16, content_lines: usize) -> u16 {
    if viewport_height >= 16 {
        (content_lines.clamp(1, 5) as u16) + 2
    } else if viewport_height >= 8 {
        3
    } else {
        1
    }
}

/// Keep metadata close to the draft without taking editing space on short screens.
pub fn operational_height(viewport_height: u16) -> u16 {
    if viewport_height >= 12 {
        2
    } else {
        u16::from(viewport_height > 0)
    }
}

/// Todo dock height (§14.3): compact 1 row, expanded up to 6, 0 when empty.
/// The in-progress item shares the header row, so it needs no row of its own.
pub fn todo_height(expanded: bool, item_count: usize, has_active: bool) -> u16 {
    if item_count == 0 {
        return 0;
    }
    if expanded {
        let rows = if has_active {
            item_count
        } else {
            item_count + 1
        };
        (rows as u16).clamp(2, 6)
    } else {
        1
    }
}

/// Normal layout (§14.1/§15.3/§21.4). Activity and Todo degrade before the
/// approved composer silhouette; its bottom border directly precedes footer.
pub fn plan_checked(
    width: u16,
    height: u16,
    todo_rows: u16,
    working: bool,
) -> Result<LayoutRegions, LayoutError> {
    plan_checked_internal(width, height, todo_rows, working, false, 1)
}

fn plan_checked_internal(
    width: u16,
    height: u16,
    todo_rows: u16,
    working: bool,
    show_session_rail: bool,
    composer_lines: usize,
) -> Result<LayoutRegions, LayoutError> {
    if width < 40 || height < 8 {
        return Err(LayoutError::TerminalTooSmall);
    }
    let mut session = u16::from(show_session_rail && width >= 80 && height >= 12);
    let mut activity = u16::from(working);
    let mut operational = operational_height(height);
    let composer = composer_height_for_lines(height, composer_lines);
    let mut todo = todo_rows;
    let mut use_dividers = true;
    let mut todo_div;
    loop {
        todo_div = u16::from(use_dividers && todo > 0);
        let fixed = session + activity + operational + composer + todo + todo_div;
        if height > fixed {
            break;
        }
        // Degradation order (§14.4): SessionRail, ActivityRail, Todo; never composer.
        if operational > 1 {
            operational -= 1;
        } else if session > 0 {
            session = 0;
        } else if activity > 0 {
            activity = 0;
        } else if use_dividers {
            use_dividers = false;
        } else if todo > 1 {
            todo = 1;
        } else if todo > 0 {
            todo = 0;
        } else {
            return Err(LayoutError::InsufficientHeight);
        }
    }
    let scrollback_height = height - session - activity - operational - composer - todo - todo_div;
    let mut y = 0;
    let session_rail = Rect {
        x: 0,
        y,
        width,
        height: session,
    };
    y += session;
    let scrollback = Rect {
        x: 0,
        y,
        width,
        height: scrollback_height,
    };
    y += scrollback_height;
    let todo_rect = Rect {
        x: 0,
        y,
        width,
        height: todo,
    };
    y += todo;
    let todo_divider = Rect {
        x: 0,
        y,
        width,
        height: todo_div,
    };
    y += todo_div;
    let activity_rail = Rect {
        x: 0,
        y,
        width,
        height: activity,
    };
    y += activity;
    let composer_rect = Rect {
        x: 0,
        y,
        width,
        height: composer,
    };
    y += composer;
    let op_divider = Rect {
        x: 0,
        y,
        width,
        height: 0,
    };
    Ok(LayoutRegions {
        session_rail,
        activity_rail,
        scrollback,
        todo: todo_rect,
        todo_divider,
        composer: composer_rect,
        op_divider,
        operational: Rect {
            x: 0,
            y: height - operational,
            width,
            height: operational,
        },
    })
}

/// Emergency layout for terminals below 40x8 (§14.5): dividers dropped,
/// todo truncated to one row, one composer row, one operational row.
pub fn plan(width: u16, height: u16, todo_rows: u16, working: bool) -> LayoutRegions {
    plan_with_session_rail(width, height, todo_rows, working, false)
}

pub fn plan_with_session_rail(
    width: u16,
    height: u16,
    todo_rows: u16,
    working: bool,
    show_session_rail: bool,
) -> LayoutRegions {
    plan_with_session_rail_and_composer(width, height, todo_rows, working, show_session_rail, 1)
}

pub fn plan_with_session_rail_and_composer(
    width: u16,
    height: u16,
    todo_rows: u16,
    working: bool,
    show_session_rail: bool,
    composer_lines: usize,
) -> LayoutRegions {
    if let Ok(layout) = plan_checked_internal(
        width,
        height,
        todo_rows,
        working,
        show_session_rail,
        composer_lines,
    ) {
        return layout;
    }
    let operational_height = u16::from(height > 0);
    let composer_height = u16::from(height > operational_height);
    let todo_height = u16::from(todo_rows > 0 && height > operational_height + composer_height);
    let scrollback_height =
        height.saturating_sub(todo_height + composer_height + operational_height);
    LayoutRegions {
        session_rail: Rect {
            x: 0,
            y: 0,
            width,
            height: 0,
        },
        activity_rail: Rect {
            x: 0,
            y: 0,
            width,
            height: 0,
        },
        scrollback: Rect {
            x: 0,
            y: 0,
            width,
            height: scrollback_height,
        },
        todo: Rect {
            x: 0,
            y: scrollback_height,
            width,
            height: todo_height,
        },
        todo_divider: Rect {
            x: 0,
            y: scrollback_height + todo_height,
            width,
            height: 0,
        },
        composer: Rect {
            x: 0,
            y: scrollback_height + todo_height,
            width,
            height: composer_height,
        },
        op_divider: Rect {
            x: 0,
            y: scrollback_height + todo_height + composer_height,
            width,
            height: 0,
        },
        operational: Rect {
            x: 0,
            y: height.saturating_sub(operational_height),
            width,
            height: operational_height,
        },
    }
}

pub fn visible_range(
    total_rows: usize,
    offset: usize,
    viewport_rows: usize,
) -> std::ops::Range<usize> {
    let start = offset.min(total_rows);
    let end = start.saturating_add(viewport_rows).min(total_rows);
    start..end
}
