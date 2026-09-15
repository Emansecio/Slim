use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use slim_tui::api::{InteractionRequestId, UiCommand, UiEvent};
use slim_tui::app::AppState;
use slim_tui::block::{BlockKind, BlockLifecycle, InteractionRequestKind};
use slim_tui::reducer::{reduce, Action, Effect};
use slim_tui::testkit::render_terminal_text;

fn request_id(value: &str) -> InteractionRequestId {
    InteractionRequestId(value.into())
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn question_options_support_selection_and_custom_answer_without_sending_a_prompt() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-1"),
        question: "Which crate should change?".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "core".into(),
                description: "Runtime and protocol".into(),
            },
            slim_core::QuestionOption {
                label: "tui".into(),
                description: "Interface only".into(),
            },
        ],
        persisted: false,
    });

    let frame = render_terminal_text(&state, 40, 12);
    for expected in ["Which crate", "core", "Runtime", "tui", "Outro", "↑↓"] {
        assert!(frame.contains(expected), "missing {expected}\n{frame}");
    }
    assert!(frame.contains("Which crate"), "{frame}");
    assert!(frame.contains("[x] core"), "{frame}");
    assert!(!frame.contains("? >"), "{frame}");
    assert!(!frame.contains("Type your answer"), "{frame}");
    reduce(&mut state, Action::Key(key(KeyCode::Down)));
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    assert!(effects.contains(&Effect::Send(UiCommand::AnswerQuestion {
        request_id: request_id("question-1"),
        answer: slim_core::QuestionAnswer::option(1, "tui").expect("option"),
    })));
    assert!(effects
        .iter()
        .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
    state.apply_event(UiEvent::InteractionAcknowledged {
        request_id: request_id("question-1"),
        accepted: true,
        message: String::new(),
    });
    let done = render_terminal_text(&state, 40, 12);
    assert!(done.contains("tui"), "{done}");
    assert!(done.contains("· tui") || done.contains("  · tui"), "{done}");
    assert!(!done.contains("Outro"), "{done}");

    let mut custom = AppState::new();
    custom.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-2"),
        question: "Choose or explain".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "A".into(),
                description: String::new(),
            },
            slim_core::QuestionOption {
                label: "B".into(),
                description: String::new(),
            },
        ],
        persisted: false,
    });
    reduce(&mut custom, Action::Key(key(KeyCode::Down)));
    reduce(&mut custom, Action::Key(key(KeyCode::Down)));
    let activate = reduce(&mut custom, Action::Key(key(KeyCode::Enter)));
    assert_eq!(activate, vec![Effect::RequestRender]);
    reduce(&mut custom, Action::Key(key(KeyCode::Char('x'))));
    let effects = reduce(&mut custom, Action::Key(key(KeyCode::Enter)));
    assert!(effects.contains(&Effect::Send(UiCommand::AnswerQuestion {
        request_id: request_id("question-2"),
        answer: slim_core::QuestionAnswer::custom("x").expect("custom"),
    })));
}

#[test]
fn open_question_uses_the_composer_and_blocks_duplicate_submission() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-open"),
        question: "Explain the constraint".into(),
        options: Vec::new(),
        persisted: false,
    });
    reduce(&mut state, Action::Paste("Need a stable API".into()));
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    assert!(effects.contains(&Effect::Send(UiCommand::AnswerQuestion {
        request_id: request_id("question-open"),
        answer: slim_core::QuestionAnswer::custom("Need a stable API").expect("custom"),
    })));
    assert!(reduce(&mut state, Action::Key(key(KeyCode::Enter))).is_empty());
    assert!(reduce(&mut state, Action::Paste("late".into()))
        .iter()
        .all(|effect| !matches!(effect, Effect::Send(_))));
}

#[test]
fn input_request_renders_answers_and_completes_only_after_matching_ack() {
    let request = UiEvent::InputRequired {
        request_id: request_id("input-1"),
        prompt: "Choose a target".into(),
        options: vec!["core".into(), "tui".into()],
        persisted: true,
    };
    let mut state = AppState::new();
    state.apply_event(request.clone());

    let frame = render_terminal_text(&state, 80, 24);
    for expected in ["Choose a target", "core", "tui", "persisted"] {
        assert!(frame.contains(expected), "missing {expected}\n{frame}");
    }
    assert_eq!(state.blocks().len(), 1);
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Pending);

    state.composer.insert_text("tui");
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    assert!(effects.contains(&Effect::Send(UiCommand::AnswerInput {
        request_id: request_id("input-1"),
        answer: "tui".into(),
    })));
    assert!(effects
        .iter()
        .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Pending);
    let BlockKind::InteractionRequest(pending) = state.blocks()[0].kind() else {
        panic!("interaction request")
    };
    assert!(pending.response_pending);

    let after_response = state.clone();
    state.apply_event(request);
    assert_eq!(
        state, after_response,
        "replayed request must ignore mutable response state"
    );
    assert!(reduce(&mut state, Action::Key(key(KeyCode::Char('x')))).is_empty());
    assert_eq!(state.composer.payload(), "");
    let paste = reduce(&mut state, Action::Paste("late paste".into()));
    assert_eq!(paste, vec![Effect::RequestRender]);
    assert_eq!(state.composer.payload(), "");

    state.apply_event(UiEvent::InteractionAcknowledged {
        request_id: request_id("input-1"),
        accepted: true,
        message: "answer accepted".into(),
    });
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Complete);
    assert!(state.pending_interaction().is_none());
    assert!(render_terminal_text(&state, 80, 24).contains("answer accepted"));

    let after_first_ack = state.clone();
    state.apply_event(UiEvent::InteractionAcknowledged {
        request_id: request_id("input-1"),
        accepted: true,
        message: "duplicate".into(),
    });
    assert_eq!(state, after_first_ack, "duplicate ack must be idempotent");
}

#[test]
fn approval_uses_y_n_and_rejected_ack_is_visible() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ApprovalRequired {
        request_id: request_id("approval-1"),
        summary: "Apply the proposed plan".into(),
        persisted: false,
    });

    let frame = render_terminal_text(&state, 80, 24);
    assert!(frame.contains("Apply the proposed plan"), "{frame}");
    assert!(frame.contains("Y approve"), "{frame}");
    assert!(frame.contains("ephemeral"), "{frame}");

    let release = KeyEvent::new_with_kind(
        KeyCode::Char('n'),
        KeyModifiers::NONE,
        crossterm::event::KeyEventKind::Release,
    );
    assert!(reduce(&mut state, Action::Key(release)).is_empty());
    assert!(state.composer.payload().is_empty());
    let enter = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    assert!(enter
        .iter()
        .all(|effect| !matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
    assert!(reduce(&mut state, Action::Key(key(KeyCode::Char('x')))).is_empty());
    assert!(state.composer.payload().is_empty());

    let effects = reduce(&mut state, Action::Key(key(KeyCode::Char('n'))));
    assert!(effects.contains(&Effect::Send(UiCommand::Reject {
        request_id: request_id("approval-1"),
    })));
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Pending);

    state.apply_event(UiEvent::InteractionAcknowledged {
        request_id: request_id("approval-1"),
        accepted: false,
        message: "interaction route unavailable".into(),
    });
    assert_eq!(state.blocks()[0].lifecycle, BlockLifecycle::Failed);
    let frame = render_terminal_text(&state, 80, 24);
    assert!(frame.contains("interaction route unavailable"), "{frame}");
}

#[test]
fn duplicate_conflict_stale_ack_terminal_and_replay_are_deterministic() {
    let request = UiEvent::InputRequired {
        request_id: request_id("input-replay"),
        prompt: "Original question".into(),
        options: Vec::new(),
        persisted: true,
    };
    let mut state = AppState::new();
    state.apply_event(UiEvent::run_started(7));
    state.apply_event(request.clone());
    state.apply_event(request.clone());
    assert_eq!(state.blocks().len(), 1, "identical request is idempotent");

    state.apply_event(UiEvent::InputRequired {
        request_id: request_id("input-replay"),
        prompt: "Conflicting question".into(),
        options: Vec::new(),
        persisted: true,
    });
    assert_eq!(state.blocks().len(), 1);
    assert!(state.snapshot_resync_needed());
    let BlockKind::InteractionRequest(interaction) = state.blocks()[0].kind() else {
        panic!("interaction request")
    };
    assert!(matches!(
        &interaction.kind,
        InteractionRequestKind::Input { prompt, .. } if prompt == "Original question"
    ));

    let before_stale = state.clone();
    state.apply_event(UiEvent::InteractionAcknowledged {
        request_id: request_id("unknown"),
        accepted: true,
        message: "stale".into(),
    });
    assert_eq!(state, before_stale, "stale ack must not mutate state");

    state.apply_event(UiEvent::RunStopped {
        run_id: 7,
        message: "input_required".into(),
    });
    assert!(state.pending_interaction().is_some());
    state.composer.insert_text("answer after terminal");
    let effects = reduce(&mut state, Action::Key(key(KeyCode::Enter)));
    assert!(effects.contains(&Effect::Send(UiCommand::AnswerInput {
        request_id: request_id("input-replay"),
        answer: "answer after terminal".into(),
    })));

    let mut replay = AppState::new();
    replay.apply_event(request);
    let replayed = replay.pending_interaction().expect("replayed pending");
    assert_eq!(replayed.request_id, request_id("input-replay"));
    assert!(replayed.persisted);
}

#[test]
fn question_overlay_wraps_on_words_and_does_not_use_approval_chrome() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-wrap"),
        question: "Suporte a streamcash.to implementado e validado (análise real concluída: 1 vídeo HLS). Quer que eu execute o download completo agora?".into(),
        options: Vec::new(),
        persisted: false,
    });
    let frame = render_terminal_text(&state, 40, 12);
    assert!(frame.contains("Question"), "{frame}");
    assert!(frame.contains("download"), "{frame}");
    assert!(
        !frame.contains("downlo\n")
            && !frame
                .lines()
                .any(|line| line.trim_end().ends_with("downlo")),
        "question must wrap on a word boundary\n{frame}"
    );
    assert!(!frame.contains("Ask a question"), "{frame}");
    assert!(
        !frame.contains("Esc/Y/N") && !frame.contains("Y approve"),
        "open questions must not reuse approval shortcuts\n{frame}"
    );
    assert!(
        frame.contains("answer") && frame.contains("Enter"),
        "{frame}"
    );
}

#[test]
fn question_overlay_lists_options_with_selection_chrome() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-overlay"),
        question: "Which crate should change?".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "core".into(),
                description: "Runtime and protocol".into(),
            },
            slim_core::QuestionOption {
                label: "tui".into(),
                description: "Interface only".into(),
            },
        ],
        persisted: false,
    });
    let frame = render_terminal_text(&state, 48, 16);
    assert!(frame.contains("Question"), "{frame}");
    assert!(frame.contains("Which crate should change?"), "{frame}");
    assert!(frame.contains("[x] core"), "{frame}");
    assert!(frame.contains("Runtime and protocol"), "{frame}");
    assert!(frame.contains("[ ] tui"), "{frame}");
    assert!(frame.contains("Outro"), "{frame}");
    assert!(frame.contains("↑↓"), "{frame}");
    assert!(!frame.contains("Y approve"), "{frame}");
    assert!(!frame.contains("1-5"), "{frame}");
}

#[test]
fn question_card_stays_contained_on_a_wide_terminal() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-card"),
        question: "Which crate should change?".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "core".into(),
                description: String::new(),
            },
            slim_core::QuestionOption {
                label: "tui".into(),
                description: String::new(),
            },
        ],
        persisted: false,
    });
    let frame = render_terminal_text(&state, 100, 24);
    let title = frame
        .lines()
        .find(|line| line.contains("Question"))
        .expect("question title");
    assert!(
        title.starts_with(" ╭"),
        "card must align with the one-cell composer inset\n{title}"
    );
    let border = title.chars().filter(|glyph| *glyph == '─').count();
    assert!(border <= 72, "question card must stay contained\n{title}");
    assert!(frame.contains("[x] core"), "{frame}");
    assert!(frame.contains("[ ] tui"), "{frame}");
}

#[test]
fn compact_question_keeps_long_tail_and_focused_option_visible() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::QuestionRequired {
        request_id: request_id("question-compact"),
        question: "Select the crate and preserve this final qualifier FINAL_QUALIFIER".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "core".into(),
                description: "An intentionally long core description that must yield space".into(),
            },
            slim_core::QuestionOption {
                label: "tui".into(),
                description: "FOCUSED_DESCRIPTION remains visible when the card is compact".into(),
            },
        ],
        persisted: false,
    });
    reduce(&mut state, Action::Key(key(KeyCode::Down)));

    let frame = render_terminal_text(&state, 48, 13);
    assert!(
        frame.contains("FINAL_QUALIFIER"),
        "question tail must survive compact wrapping\n{frame}"
    );
    assert!(
        frame.contains("[x] tui"),
        "focused option must stay visible\n{frame}"
    );
    assert!(
        frame.contains("FOCUSED_DESCRIPTION"),
        "focused description should use remaining compact rows\n{frame}"
    );
}
