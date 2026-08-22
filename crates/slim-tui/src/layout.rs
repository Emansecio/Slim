#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutRegions {
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

/// Composer box height (§15.3): every supported viewport keeps the approved
/// three-row silhouette; only the emergency layout below 40x8 collapses it.
pub fn composer_height(viewport_height: u16) -> u16 {
    if viewport_height >= 8 {
        3
    } else {
        1
    }
}

/// Todo dock height (§14.3): compact 2 rows, expanded up to 6, 0 when closed.
pub fn todo_height(expanded: bool, item_count: usize) -> u16 {
    if item_count == 0 {
        return 0;
    }
    if expanded {
        (item_count as u16 + 1).clamp(2, 6)
    } else {
        2
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
    if width < 40 || height < 8 {
        return Err(LayoutError::TerminalTooSmall);
    }
    let mut activity = u16::from(working);
    let composer = composer_height(height);
    let mut todo = todo_rows;
    let mut use_dividers = true;
    let mut todo_div;
    loop {
        todo_div = u16::from(use_dividers && todo > 0);
        let fixed = activity + 1 + composer + todo + todo_div;
        if height > fixed {
            break;
        }
        // Degradation order (§14.4): rail, Todo divider, Todo; never composer.
        if activity > 0 {
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
    let scrollback_height = height - activity - 1 - composer - todo - todo_div;
    let mut y = 0;
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
        activity_rail,
        scrollback,
        todo: todo_rect,
        todo_divider,
        composer: composer_rect,
        op_divider,
        operational: Rect {
            x: 0,
            y: height - 1,
            width,
            height: 1,
        },
    })
}

/// Emergency layout for terminals below 40x8 (§14.5): dividers dropped,
/// todo truncated to one row, one composer row, one operational row.
pub fn plan(width: u16, height: u16, todo_rows: u16, working: bool) -> LayoutRegions {
    if let Ok(layout) = plan_checked(width, height, todo_rows, working) {
        return layout;
    }
    let operational_height = u16::from(height > 0);
    let composer_height = u16::from(height > operational_height);
    let todo_height = u16::from(todo_rows > 0 && height > operational_height + composer_height);
    let scrollback_height =
        height.saturating_sub(todo_height + composer_height + operational_height);
    LayoutRegions {
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
