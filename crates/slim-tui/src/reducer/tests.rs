use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{open_model_overlay, reduce, Action, Effect};
use crate::api::{
    LoginProvider, ModelAlias, OpenCodeCatalogSource, OpenCodeModelView, ReasoningEffort,
    UiCommand, UiEvent,
};
use crate::app::ModelRow;
use crate::app::{
    ActivityPhase, AppState, CancellationPhase, ConfirmedSetting, FrameClock, LoginStage,
    NotificationPriority, RunOutcomeKind,
};

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

#[test]
fn draft_edit_shortcuts_undo_redo_words_and_preserve_copy_binding() {
    let mut state = AppState::new();
    state.composer.insert_text("ação/path");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
    );
    assert_eq!(state.composer.cursor(), 5);
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
    );
    assert_eq!(state.composer.payload(), "ação/");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
    );
    assert_eq!(state.composer.payload(), "ação/path");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )),
    );
    assert_eq!(state.composer.payload(), "ação/");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
    );
    assert_eq!(state.composer.payload(), "ação/");
    state.composer.insert_text(" test");
    state.login_overlay = Some(crate::app::LoginOverlay::default());
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
    );
    assert_eq!(
        state.composer.payload(),
        "ação/ test",
        "modal owns keyboard"
    );
}

#[test]
fn retry_while_working_is_a_command_not_a_queued_prompt() {
    let mut state = AppState::new();
    state.working = true;
    state.composer.insert_text("/retry");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::RetryProvider)));
    assert!(state.queued_prompts.is_empty());
    assert!(state.composer.is_empty());
    assert!(
        !state.composer.undo(),
        "submitted command must not retain undo history"
    );
}

#[test]
fn saving_model_default_is_explicit_and_blocked_while_working() {
    let mut state = AppState::new();
    state.composer.insert_text("/model --default");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::SaveModelDefault)));
    state.working = true;
    state.composer.insert_text("/model --default");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(!effects.contains(&Effect::Send(UiCommand::SaveModelDefault)));
    assert_eq!(state.composer.payload(), "/model --default");
}

#[test]
fn login_command_opens_selector_and_selected_provider_is_sent() {
    let mut state = AppState::new();
    state.composer.insert_text("/login");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.login_overlay.is_some());

    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::StartLogin(
        LoginProvider::OpenAiCodex
    ))));
}

#[test]
fn login_failure_releases_overlay_instead_of_sticking_in_progress() {
    // G251: a terminal failure must reset `in_progress` — while set, the
    // reducer swallows every key, freezing the dialog on the error.
    let mut state = AppState::new();
    state.composer.insert_text("/login codex");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.login_overlay.as_ref().unwrap().in_progress);

    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::LoginFailed {
            message: "key save task failed boom".into(),
        }),
    );

    let overlay = state.login_overlay.as_ref().unwrap();
    assert!(
        !overlay.in_progress,
        "failure must release the overlay for retry"
    );
    assert_eq!(
        overlay.progress.as_deref(),
        Some("key save task failed boom"),
        "the error stays visible in the dialog"
    );
    // Esc now closes the dialog without issuing a bogus CancelLogin.
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.login_overlay.is_none());
    assert!(!effects.contains(&Effect::Send(UiCommand::CancelLogin)));
}

#[test]
fn opencode_login_collects_secret_and_emits_typed_command() {
    let mut state = AppState::new();
    state.composer.insert_text("/login");
    reduce(&mut state, Action::Key(enter()));
    for _ in 0..2 {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
    }
    reduce(&mut state, Action::Key(enter()));
    for character in "needle-secret".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }

    let effects = reduce(&mut state, Action::Key(enter()));

    assert!(matches!(
        effects.as_slice(),
        [
            Effect::Send(UiCommand::SaveApiKey {
                provider: LoginProvider::OpenCodeGo,
                api_key,
            }),
            Effect::RequestRender,
        ] if api_key.expose() == "needle-secret"
    ));
    assert!(!format!("{state:?}").contains("needle-secret"));
}

#[test]
fn opencode_login_paste_never_enters_composer() {
    let mut state = AppState::new();
    state.composer.insert_text("/login");
    reduce(&mut state, Action::Key(enter()));
    for _ in 0..2 {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
    }
    reduce(&mut state, Action::Key(enter()));

    reduce(&mut state, Action::Paste("pasted-secret".into()));

    assert!(state.composer.payload().is_empty());
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(matches!(
        effects.first(),
        Some(Effect::Send(UiCommand::SaveApiKey { api_key, .. }))
            if api_key.expose() == "pasted-secret"
    ));
}

#[test]
fn clipboard_pull_prefers_the_image_attachment() {
    // §20: one clipboard pull carries image and text; the composer keeps
    // the attachment and never echoes the raw clipboard text.
    let mut state = AppState::new();
    let effects = reduce(
        &mut state,
        Action::ClipboardPull {
            image: Some(r"C:\Temp\slim-paste-42\clipboard-0.png".into()),
            text: Some("also on the clipboard".into()),
        },
    );
    assert_eq!(
        effects,
        vec![
            Effect::Send(UiCommand::AttachImage(
                r"C:\Temp\slim-paste-42\clipboard-0.png".into()
            )),
            Effect::RequestRender,
        ]
    );
    assert!(state.composer.payload().is_empty());
}

#[test]
fn clipboard_pull_without_an_image_pastes_the_text() {
    let mut state = AppState::new();
    let effects = reduce(
        &mut state,
        Action::ClipboardPull {
            image: None,
            text: Some("pasted text".into()),
        },
    );
    assert_eq!(effects, vec![Effect::RequestRender]);
    assert_eq!(state.composer.payload(), "pasted text");

    // An empty clipboard stays a no-op: no empty paste segment.
    let mut empty = AppState::new();
    let effects = reduce(
        &mut empty,
        Action::ClipboardPull {
            image: None,
            text: None,
        },
    );
    assert_eq!(effects, vec![Effect::RequestRender]);
    assert!(empty.composer.payload().is_empty());
}

#[test]
fn clipboard_image_never_lands_on_the_login_api_key_field() {
    // The login field is a text target: an image on the clipboard must not
    // hijack the secret input.
    let mut state = AppState::new();
    state.composer.insert_text("/login");
    reduce(&mut state, Action::Key(enter()));
    for _ in 0..2 {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
    }
    reduce(&mut state, Action::Key(enter()));

    let effects = reduce(
        &mut state,
        Action::ClipboardPull {
            image: Some("clipboard-0.png".into()),
            text: Some("needle-secret".into()),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::AttachImage(_)))),
        "an image must not replace the API key paste"
    );
    let saved = reduce(&mut state, Action::Key(enter()));
    assert!(matches!(
        saved.first(),
        Some(Effect::Send(UiCommand::SaveApiKey { api_key, .. }))
            if api_key.expose() == "needle-secret"
    ));
}

#[test]
fn clipboard_image_is_ignored_under_a_stacked_modal() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenCodeGo);
    state.authenticated = true;
    state.composer.insert_text("/models");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.model_overlay.is_some());

    let effects = reduce(
        &mut state,
        Action::ClipboardPull {
            image: Some("clipboard-0.png".into()),
            text: None,
        },
    );
    assert_eq!(effects, vec![Effect::RequestRender]);
}

#[test]
fn clipboard_image_failures_are_visible_notifications() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::ClipboardImageFailed {
            message: "Clipboard image: bitmap is not supported".into(),
        },
    );
    assert_eq!(
        state.notifications.last().map(|notice| notice.as_str()),
        Some("Clipboard image: bitmap is not supported")
    );
}

#[test]
fn opencode_models_refresh_and_select_dynamic_catalog() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenCodeGo);
    state.authenticated = true;
    state.composer.insert_text("/models");

    let effects = reduce(&mut state, Action::Key(enter()));

    assert!(effects.contains(&Effect::Send(UiCommand::RefreshOpenCodeModels)));
    assert!(effects.contains(&Effect::Send(UiCommand::RefreshClinePassModels)));
    assert!(effects.contains(&Effect::Send(UiCommand::RefreshCommandCodeModels)));
    assert!(state.model_overlay.is_some());
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::OpenCodeCatalogLoaded {
            models: vec![
                OpenCodeModelView {
                    id: "deepseek-v4-flash".into(),
                    name: "DeepSeek V4 Flash".into(),
                    context_window_tokens: 1_000_000,
                    max_output_tokens: 384_000,
                    reasoning_levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
                    accepts_images: false,
                },
                OpenCodeModelView {
                    id: "glm-5.3".into(),
                    name: "GLM 5.3".into(),
                    context_window_tokens: 202_752,
                    max_output_tokens: 131_072,
                    reasoning_levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
                    accepts_images: false,
                },
            ],
            source: OpenCodeCatalogSource::Live,
        }),
    );
    // Filter down to the OpenCode group and select GLM.
    for character in "glm".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }

    // Group headers are rows, so counting Downs is not stable once other
    // groups also match the filter (ClinePass ships its own `glm-5.3`).
    // Position on the Go catalog row explicitly.
    let rows = state.model_overlay.clone().expect("overlay").rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    );
    let target = rows
        .iter()
        .position(|row| {
            matches!(row, ModelRow::Catalog(index)
                if state.open_code_models[*index].id == "glm-5.3")
        })
        .expect("glm-5.3 in the OpenCode Go group");
    for _ in 0..target {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
    }
    assert_eq!(
        state.model_overlay.as_ref().expect("overlay").selected,
        target
    );

    // The catalog levels are visible and adjustable in the model picker.
    let choice = rows[target]
        .choice(
            &state.open_code_models,
            &state.cline_pass_models,
            &state.command_code_models,
            &state.zen_models,
        )
        .expect("catalog choice");
    assert_eq!(
        choice.levels,
        &[ReasoningEffort::Low, ReasoningEffort::High]
    );
    assert_eq!(
        state
            .model_overlay
            .as_ref()
            .expect("overlay")
            .pending_effort(&choice, state.effort),
        ReasoningEffort::High,
        "the session effort is the initial selection"
    );
    // One confirmation sends model + effort together.
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::SetOpenCodeModel {
        model: "glm-5.3".into(),
        effort: ReasoningEffort::High,
    })));
    assert!(state.effort_overlay.is_none());
}

/// Positions the model overlay on the row matched by `predicate`, because
/// group headers are rows and counting Downs is brittle.
fn select_model_row(state: &mut AppState, predicate: impl Fn(&ModelRow) -> bool) {
    let rows = state.model_overlay.clone().expect("model overlay").rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    );
    let index = rows.iter().position(predicate).expect("row in the picker");
    reduce(
        state,
        Action::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
    );
    for _ in 0..index {
        reduce(
            state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
    }
    assert_eq!(
        state.model_overlay.as_ref().expect("overlay").selected,
        index
    );
}

fn zen_catalog_state(model: OpenCodeModelView) -> AppState {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenCodeZen);
    state.authenticated = true;
    // The picker expands the group of the model in use (G234), so the
    // fixture must be the active model for the Zen rows to be listed.
    state.model = model.id.clone();
    state.zen_models = vec![model];
    state.composer.insert_text("/models");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.model_overlay.is_some());
    state
}

fn zen_view(id: &str, levels: Vec<ReasoningEffort>) -> OpenCodeModelView {
    OpenCodeModelView {
        id: id.into(),
        name: id.into(),
        context_window_tokens: 1_048_576,
        max_output_tokens: 131_072,
        reasoning_levels: levels,
        accepts_images: true,
    }
}

#[test]
fn zen_model_with_levels_adjusts_effort_inline_before_sending() {
    // Muse Spark 1.3 Free declares low/medium/high/xhigh in the picker.
    let mut state = zen_catalog_state(zen_view(
        "muse-spark-1.3-contributor-free",
        vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ],
    ));
    state.effort = ReasoningEffort::Low;
    select_model_row(&mut state, |row| matches!(row, ModelRow::Zen(0)));

    // Tab is inert for a catalog target; Right moves through available levels.
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    );
    assert!(!state.model_overlay.as_ref().expect("picker").pending_fast);
    for _ in 0..3 {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        );
    }
    let choice = state.model_overlay.as_ref().unwrap().rows(
        &state.open_code_models,
        &state.cline_pass_models,
        &state.command_code_models,
        &state.zen_models,
    )[state.model_overlay.as_ref().unwrap().selected]
        .choice(
            &state.open_code_models,
            &state.cline_pass_models,
            &state.command_code_models,
            &state.zen_models,
        )
        .unwrap();
    assert_eq!(
        state
            .model_overlay
            .as_ref()
            .unwrap()
            .pending_effort(&choice, state.effort),
        ReasoningEffort::XHigh
    );

    let effects = reduce(&mut state, Action::Key(enter()));

    assert!(effects.contains(&Effect::Send(UiCommand::SetZenModel {
        model: "muse-spark-1.3-contributor-free".into(),
        effort: ReasoningEffort::XHigh,
    })));
    assert!(state.model_overlay.is_none());
}

#[test]
fn zen_model_without_levels_skips_the_effort_step() {
    // Big Pickle declares no reasoning knob: the picker keeps the direct
    // path and the CLI handler drops the unused effort.
    let mut state = zen_catalog_state(zen_view("big-pickle", Vec::new()));
    select_model_row(&mut state, |row| matches!(row, ModelRow::Zen(0)));

    let effects = reduce(&mut state, Action::Key(enter()));

    assert!(state.effort_overlay.is_none(), "no step without levels");
    assert!(state.model_overlay.is_none(), "selection closes the picker");
    assert!(effects.contains(&Effect::Send(UiCommand::SetZenModel {
        model: "big-pickle".into(),
        effort: ReasoningEffort::High,
    })));
}

#[test]
fn textual_model_command_selects_clinepass_model() {
    // G249: `/model <id>` works while ClinePass is connected, validating
    // against the static catalog and sending SetClinePassModel (High).
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::ClinePass);
    state.authenticated = true;
    state.composer.insert_text("/model cline-pass/kimi-k3");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(
        effects.contains(&Effect::Send(UiCommand::SetClinePassModel {
            model: "cline-pass/kimi-k3".into(),
            effort: ReasoningEffort::High,
        }))
    );
}

#[test]
fn textual_model_command_rejects_unknown_clinepass_model() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::ClinePass);
    state.authenticated = true;
    state.composer.insert_text("/model not-a-model");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(
        !effects.contains(&Effect::Send(UiCommand::SetClinePassModel {
            model: "not-a-model".into(),
            effort: ReasoningEffort::High,
        }))
    );
    // Unknown selection still surfaces a notification and does not hang.
    assert!(effects.contains(&Effect::RequestRender));
}

#[test]
fn unknown_model_command_preserves_draft_for_correction() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::ClinePass);
    state.authenticated = true;
    state.composer.insert_text("/model not-a-model");
    let _ = reduce(&mut state, Action::Key(enter()));
    assert_eq!(state.composer.payload(), "/model not-a-model");
    assert!(
        state.notifications.iter().any(|notification| notification
            .as_str()
            .contains("Modelo ClinePass desconhecido")),
        "rejection must stay visible: {:?}",
        state.notifications
    );
}

#[test]
fn command_code_login_collects_secret_and_emits_typed_command() {
    let mut state = AppState::new();
    state.composer.insert_text("/login");
    reduce(&mut state, Action::Key(enter()));
    for _ in 0..4 {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );
    }
    reduce(&mut state, Action::Key(enter()));
    for character in "cmd-secret".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }

    let effects = reduce(&mut state, Action::Key(enter()));

    assert!(matches!(
        effects.as_slice(),
        [
            Effect::Send(UiCommand::SaveApiKey {
                provider: LoginProvider::CommandCode,
                api_key,
            }),
            Effect::RequestRender,
        ] if api_key.expose() == "cmd-secret"
    ));
    assert!(!format!("{state:?}").contains("cmd-secret"));
}

#[test]
fn login_command_code_slash_opens_api_key_stage() {
    let mut state = AppState::new();
    state.composer.insert_text("/login command-code");
    reduce(&mut state, Action::Key(enter()));
    let overlay = state.login_overlay.expect("overlay");
    assert_eq!(overlay.provider(), LoginProvider::CommandCode);
    assert!(matches!(overlay.stage, LoginStage::ApiKey(_)));
}

#[test]
fn textual_model_command_selects_command_code_model() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::CommandCode);
    state.authenticated = true;
    state
        .composer
        .insert_text("/model deepseek/deepseek-v4.1-flash");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(!effects.iter().any(|e| matches!(e, Effect::Send(_))));
    assert_eq!(
        state.effort_overlay.as_ref().unwrap().levels(),
        &[
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max
        ]
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(
        effects.contains(&Effect::Send(UiCommand::SetCommandCodeModel {
            model: "deepseek/deepseek-v4.1-flash".into(),
            effort: ReasoningEffort::Max,
        }))
    );
}

#[test]
fn gateway_picker_preserves_effort_and_escape_discards_inline_choice() {
    for (provider, id) in [
        (LoginProvider::ClinePass, "cline-pass/deepseek-v4-flash"),
        (LoginProvider::CommandCode, "deepseek/deepseek-v4.1-flash"),
    ] {
        let mut state = AppState::new();
        state.auth_provider = Some(provider);
        state.authenticated = true;
        state.model = id.into();
        state.effort = ReasoningEffort::Max;
        state.composer.insert_text("/models");
        reduce(&mut state, Action::Key(enter()));
        let index = if provider == LoginProvider::ClinePass {
            state
                .cline_pass_models
                .iter()
                .position(|m| m.id == id)
                .unwrap()
        } else {
            state
                .command_code_models
                .iter()
                .position(|m| m.id == id)
                .unwrap()
        };
        select_model_row(&mut state, |row| match row {
            ModelRow::ClinePass(i) => provider == LoginProvider::ClinePass && *i == index,
            ModelRow::CommandCode(i) => provider == LoginProvider::CommandCode && *i == index,
            _ => false,
        });
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        );
        assert!(state
            .model_overlay
            .as_ref()
            .is_some_and(|overlay| !overlay.pending_fast));
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(!effects.iter().any(|e| matches!(e, Effect::Send(_))));
        assert!(state.effort_overlay.is_none());
        assert!(state.model_overlay.is_none());
        assert_eq!(state.effort, ReasoningEffort::Max);
        state.composer.insert_text("/models");
        reduce(&mut state, Action::Key(enter()));
        select_model_row(&mut state, |row| match row {
            ModelRow::ClinePass(i) => provider == LoginProvider::ClinePass && *i == index,
            ModelRow::CommandCode(i) => provider == LoginProvider::CommandCode && *i == index,
            _ => false,
        });
        let effects = reduce(&mut state, Action::Key(enter()));
        let command = if provider == LoginProvider::ClinePass {
            UiCommand::SetClinePassModel {
                model: id.into(),
                effort: ReasoningEffort::Max,
            }
        } else {
            UiCommand::SetCommandCodeModel {
                model: id.into(),
                effort: ReasoningEffort::Max,
            }
        };
        assert!(effects.contains(&Effect::Send(command)));
    }
}

#[test]
fn textual_xai_selection_uses_model_specific_effort() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::Xai);
    state.authenticated = true;
    state.effort = ReasoningEffort::Low;
    state.composer.insert_text("/model grok-4.6");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(!effects.iter().any(|e| matches!(e, Effect::Send(_))));
    let overlay = state.effort_overlay.as_ref().unwrap();
    assert_eq!(overlay.effort(), ReasoningEffort::Low);
    assert!(overlay.levels().contains(&ReasoningEffort::XHigh));
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::SetXaiModel {
        model: "grok-4.6".into(),
        effort: ReasoningEffort::Low,
    })));
}

#[test]
fn ctrl_p_does_not_open_palette_over_login() {
    // G250: palette is gated behind the stacked overlays.
    let mut state = AppState::new();
    state.composer.insert_text("/login");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.login_overlay.is_some());
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
    );
    assert!(
        state.palette_query.is_none(),
        "Ctrl+P must not open the palette over a modal"
    );
}

#[test]
fn paste_does_not_edit_composer_under_model_overlay() {
    // G250: paste never reaches the composer beneath a stacked modal.
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenCodeGo);
    state.authenticated = true;
    state.composer.insert_text("/models");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.model_overlay.is_some());
    reduce(&mut state, Action::Paste("snuck in".into()));
    assert!(
        state.composer.payload().is_empty(),
        "paste must be ignored under a modal overlay"
    );
}

#[test]
fn ctrl_l_opens_model_overlay() {
    // G250: Ctrl+L is the missing spec §17.2 binding for the model overlay.
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenAiCodex);
    state.authenticated = true;
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
    );
    assert!(
        state.model_overlay.is_some(),
        "Ctrl+L must open the model overlay"
    );
    assert!(effects.contains(&Effect::Send(UiCommand::RefreshOpenCodeModels)));
}

#[test]
fn ctrl_t_toggles_todo_dock() {
    // G250: Ctrl+T is the missing spec §17.2 binding for the Todo dock.
    let mut state = AppState::new();
    assert!(!state.todo_dock_open);
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
    );
    assert!(state.todo_dock_open);
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
    );
    assert!(!state.todo_dock_open);
}

#[test]
fn model_command_confirms_model_and_effort_together() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenAiCodex);
    state.authenticated = true;
    state.composer.insert_text("/model");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.model_overlay.is_some());
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::SetModel {
        model: ModelAlias::Terra,
        effort: ReasoningEffort::High,
        fast: false,
    })));
}

#[test]
fn model_picker_escape_discards_the_pending_choice() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenAiCodex);
    state.authenticated = true;
    state.composer.insert_text("/model");
    reduce(&mut state, Action::Key(enter()));
    // Rows: Header, Sol, Terra — one Down lands on Terra.
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.model_overlay.is_none());
    assert!(state.effort_overlay.is_none());
    assert_ne!(state.model, ModelAlias::Terra.id());
}

#[test]
fn altgr_printable_slash_reaches_composer() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )),
    );
    assert_eq!(state.composer.payload(), "/");
}

#[test]
fn signed_out_prompt_preserves_draft_without_sending() {
    let mut state = AppState::new();
    state.composer.insert_text("keep this prompt");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert_eq!(state.composer.payload(), "keep this prompt");
    assert!(effects.iter().all(|effect| !matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
    )));
    assert_eq!(
        state.notifications.last().map(|notice| notice.as_str()),
        Some("Nenhum provedor conectado. Use /login.")
    );
}

#[test]
fn enter_during_active_run_enqueues_draft_instead_of_sending() {
    // G244 (§7.4): submit while a run is active queues the draft as a
    // visible QueuedUser block instead of silently dropping it.
    let mut state = AppState::new();
    state.working = true;
    state.composer.insert_text("keep this draft");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert_eq!(state.composer.payload(), "");
    assert_eq!(state.queued_prompts.len(), 1);
    assert_eq!(
        state.queued_prompts.front().map(String::as_str),
        Some("keep this draft")
    );
    assert!(effects.iter().all(|effect| !matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
    )));
}

#[test]
fn ctrl_c_with_screen_selection_copies_instead_of_shutdown() {
    let mut state = AppState::new();
    state.selection_text = "copied from the frame".into();
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(!state.shutdown);
    assert!(effects.contains(&Effect::CopyToClipboard("copied from the frame".into())));
}

#[test]
fn right_click_copies_selection_or_requests_paste() {
    let mut state = AppState::new();
    let paste = reduce(&mut state, Action::MouseSecondary);
    assert_eq!(paste, vec![Effect::PasteFromClipboard]);
    state.selection_text = "block".into();
    let copy = reduce(&mut state, Action::MouseSecondary);
    assert!(copy.contains(&Effect::CopyToClipboard("block".into())));
}

#[test]
fn clipboard_confirmation_is_success_only_coalesced_and_transient() {
    let mut state = AppState::new();
    state.selection_text = "selected".into();
    reduce(&mut state, Action::MouseSecondary);
    assert!(
        state.notifications.is_empty(),
        "scheduling a copy is not success"
    );
    state.clock.elapsed_ms = 100;
    reduce(&mut state, Action::ClipboardCompleted { success: true });
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications[0].message, "Copiado");
    state.clock.elapsed_ms = 200;
    reduce(&mut state, Action::ClipboardCompleted { success: true });
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications[0].created_ms, 200);
    state.clock.elapsed_ms = 200 + crate::app::INFO_TOAST_TTL_MS;
    state.prune_notifications();
    assert!(state.notifications.is_empty());
    reduce(&mut state, Action::ClipboardCompleted { success: false });
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(
        state.notifications[0].message,
        "Área de transferência indisponível"
    );
}

#[test]
fn click_without_drag_clears_the_screen_selection() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::StartScreenSelection {
            x: 4,
            y: 1,
            area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
        },
    );
    assert!(state.selection.is_some());
    reduce(&mut state, Action::FinishScreenSelection);
    assert_eq!(state.selection, None);
}

#[test]
fn empty_selection_neither_pastes_nor_cancels_and_resize_clears_it() {
    let mut state = AppState::new();
    state.working = true;
    reduce(
        &mut state,
        Action::StartScreenSelection {
            x: 4,
            y: 1,
            area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
        },
    );
    reduce(&mut state, Action::UpdateScreenSelection { x: 8, y: 1 });
    assert_eq!(
        reduce(&mut state, Action::MouseSecondary),
        vec![Effect::RequestRender]
    );
    assert_eq!(
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        ),
        vec![Effect::RequestRender]
    );
    assert_eq!(
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        ),
        vec![Effect::RequestRender]
    );
    assert!(state.selection.is_none());
    reduce(
        &mut state,
        Action::StartScreenSelection {
            x: 4,
            y: 1,
            area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
        },
    );
    reduce(&mut state, Action::Resize);
    assert!(state.selection.is_none() && state.selection_area.is_none());
}

#[test]
fn new_content_preserves_coordinate_selection_snapshot() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::StartScreenSelection {
            x: 4,
            y: 1,
            area: Some(ratatui::layout::Rect::new(0, 0, 10, 3)),
        },
    );
    state.selection_text = "old content".into();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::AssistantDelta {
            text: "new content".into(),
        }),
    );
    assert!(state.selection.is_some() && state.selection_area.is_some());
    assert_eq!(state.selection_text, "old content");
}

#[test]
fn ctrl_c_idle_with_empty_draft_requests_shutdown_once() {
    let mut state = AppState::new();
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(state.shutdown);
    assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
}

#[test]
fn ctrl_c_while_working_cancels_run_and_stays_alive() {
    let mut state = AppState::new();
    state.working = true;
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(!state.shutdown);
    assert!(effects.contains(&Effect::Send(UiCommand::CancelRun)));
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

#[test]
fn login_api_key_esc_returns_to_provider_list() {
    let mut state = AppState::new();
    state.composer.insert_text("/login opencode");
    reduce(&mut state, Action::Key(enter()));
    assert!(matches!(
        state.login_overlay.as_ref().map(|overlay| &overlay.stage),
        Some(LoginStage::ApiKey(_))
    ));
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    let overlay = state
        .login_overlay
        .as_ref()
        .expect("Esc back keeps the login overlay open");
    assert!(
        matches!(overlay.stage, LoginStage::Providers),
        "API-key Esc returns to the provider list"
    );
    assert!(!state.shutdown);
}

#[test]
fn login_providers_esc_closes_overlay() {
    let mut state = AppState::new();
    state.composer.insert_text("/login");
    reduce(&mut state, Action::Key(enter()));
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.login_overlay.is_none());
    assert!(!state.shutdown);
}

#[test]
fn ctrl_c_on_model_overlay_still_exits_when_idle() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.auth_provider = Some(LoginProvider::OpenAiCodex);
    state.composer.insert_text("/model");
    reduce(&mut state, Action::Key(enter()));
    assert!(state.model_overlay.is_some());
    let effects = reduce(&mut state, Action::Key(ctrl_c()));
    assert!(state.shutdown);
    assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
}

#[test]
fn ctrl_c_on_palette_still_exits_when_idle() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
    );
    assert!(state.palette_query.is_some());
    let effects = reduce(&mut state, Action::Key(ctrl_c()));
    assert!(state.shutdown);
    assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
}

#[test]
fn ctrl_c_etx_idle_with_empty_draft_requests_shutdown() {
    let mut state = AppState::new();
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('\u{3}'), KeyModifiers::NONE)),
    );
    assert!(state.shutdown);
    assert!(effects.contains(&Effect::Send(UiCommand::Shutdown)));
}

#[test]
fn ctrl_c_idle_with_draft_clears_before_exit() {
    let mut state = AppState::new();
    state.composer.insert_text("do not lose this silently");
    let first = reduce(&mut state, Action::Key(ctrl_c()));
    assert!(!state.shutdown);
    assert!(state.composer.payload().is_empty());
    assert!(
        first
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(UiCommand::Shutdown))),
        "first Ctrl+C with a draft must not quit"
    );
    let second = reduce(&mut state, Action::Key(ctrl_c()));
    assert!(state.shutdown);
    assert!(second.contains(&Effect::Send(UiCommand::Shutdown)));
}

#[test]
fn backtab_cycles_mode_through_boundary() {
    let mut state = AppState::new();
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
    );
    assert!(effects.contains(&Effect::Send(UiCommand::SetMode(
        slim_core::OperatingMode::ReadOnly
    ))));
}

#[test]
fn alt_tab_cycles_mode_like_shift_tab() {
    for (from, expected) in [
        (
            slim_core::OperatingMode::Auto,
            slim_core::OperatingMode::ReadOnly,
        ),
        (
            slim_core::OperatingMode::ReadOnly,
            slim_core::OperatingMode::Plan,
        ),
        (
            slim_core::OperatingMode::Plan,
            slim_core::OperatingMode::Auto,
        ),
    ] {
        let mut state = AppState::new();
        state.mode = from;
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::ALT)),
        );
        assert!(
            effects.contains(&Effect::Send(UiCommand::SetMode(expected))),
            "{from:?} via Alt+Tab"
        );
    }
}

#[test]
fn mode_switch_is_refused_while_a_run_is_active() {
    let mut state = AppState::new();
    state.working = true;
    for key in [
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Tab, KeyModifiers::ALT),
    ] {
        let effects = reduce(&mut state, Action::Key(key));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(UiCommand::SetMode(_)))));
    }
}

#[test]
fn mode_slash_command_accepts_explicit_modes() {
    let mut state = AppState::new();
    state.composer.insert_text("/mode plan");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::SetMode(
        slim_core::OperatingMode::Plan
    ))));

    let mut state = AppState::new();
    state.composer.insert_text("/mode bogus");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::SetMode(_)))));
}

#[test]
fn mode_slash_command_cycles_like_shift_tab() {
    let mut state = AppState::new();
    state.composer.insert_text("/mode");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::SetMode(
        slim_core::OperatingMode::ReadOnly
    ))));
    assert!(state.composer.payload().is_empty());
}

#[test]
fn model_picker_survives_an_empty_result() {
    let mut state = AppState::new();
    let _ = open_model_overlay(&mut state);
    let mut overlay = state.model_overlay.clone().expect("overlay");
    // Nothing matches the filter: the list is empty rather than showing
    // dead headers, and no model can be selected.
    overlay.filter = "zzz-no-such-model".into();
    state.model_overlay = Some(overlay);

    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::SetModel { .. })
            | Effect::Send(UiCommand::SetOpenCodeModel { .. })
            | Effect::Send(UiCommand::SetZenModel { .. })
            | Effect::Send(UiCommand::SetClinePassModel { .. })
            | Effect::Send(UiCommand::SetCommandCodeModel { .. })
    )));

    // Esc still leaves: the interface never sticks.
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.model_overlay.is_none());
}

#[test]
fn api_key_login_collects_the_secret_masked() {
    let mut state = AppState::new();
    state.composer.insert_text("/login opencode-zen");
    reduce(&mut state, Action::Key(enter()));
    assert_eq!(
        state.login_overlay.as_ref().expect("overlay").provider(),
        LoginProvider::OpenCodeZen
    );
    for character in "ts-fixture-secret".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }

    let effects = reduce(&mut state, Action::Key(enter()));

    assert!(matches!(
        effects.as_slice(),
        [
            Effect::Send(UiCommand::SaveApiKey {
                provider: LoginProvider::OpenCodeZen,
                ..
            }),
            Effect::RequestRender
        ]
    ));
    assert!(!format!("{state:?}").contains("ts-fixture-secret"));
}

#[test]
fn resume_command_dispatches_only_while_idle() {
    let mut state = AppState::new();
    state.composer.insert_text("/resume");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(matches!(
        effects.as_slice(),
        [
            Effect::Send(UiCommand::ListSessions { .. }),
            Effect::RequestRender
        ]
    ));
    assert!(state.session_picker.is_some());
    state.session_picker = None;

    state.working = true;
    for character in "/re".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
    assert!(state.slash_suggestions.is_some());
    let effects = reduce(&mut state, Action::Key(enter()));
    assert_eq!(state.composer.payload().trim(), "/resume");
    assert!(state.session_picker.is_none());
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::ListSessions { .. }))));
}

#[test]
fn compact_command_keeps_optional_instructions_out_of_user_prompt() {
    let mut state = AppState::new();
    state.authenticated = true;
    state
        .composer
        .insert_text("/compact preserve build evidence");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::Compact {
        instructions: "preserve build evidence".into(),
    })));
    assert!(effects.iter().all(|effect| !matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
    )));
}

#[test]
fn compact_command_during_run_goes_to_safe_boundary_not_prompt_queue() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.working = true;
    state.composer.insert_text("/compact");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::Compact {
        instructions: String::new(),
    })));
    assert!(state.queued_prompts.is_empty());
}

#[test]
fn paste_too_large_is_visible_error_not_silent_drop() {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::Paste("x".repeat(crate::composer::MAX_DRAFT_CHARS + 1)),
    );
    assert!(!state.notifications.is_empty());
}

#[test]
fn info_toasts_expire_after_five_seconds_on_tick() {
    let mut state = AppState::new();
    state.clock.elapsed_ms = 1_000;
    state.apply_event(UiEvent::Notification {
        message: "Connected: OpenAI Codex — ChatGPT Plus/Pro".into(),
    });
    assert_eq!(state.notifications.len(), 1);

    reduce(
        &mut state,
        Action::Tick(FrameClock {
            frame: 60,
            elapsed_ms: 6_000,
        }),
    );
    assert!(
        state.notifications.is_empty(),
        "info toast must expire at 5s: {:?}",
        state.notifications
    );
}

#[test]
fn mode_command_via_alias_sends_set_model() {
    let mut state = AppState::new();
    state.auth_provider = Some(LoginProvider::OpenAiCodex);
    state.authenticated = true;
    state.composer.insert_text("/model luna");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::SetModel {
        model: ModelAlias::Luna,
        effort: ReasoningEffort::High,
        fast: false,
    })));
    assert!(state.composer.payload().is_empty());
}
#[test]
fn mcp_command_opens_overlay_and_starts_watch() {
    let mut state = AppState::new();
    state.composer.insert_text("/mcp");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(state.mcp_overlay.is_some());
    assert!(effects.contains(&Effect::Send(UiCommand::McpRefresh)));
    assert!(effects.contains(&Effect::Send(UiCommand::McpWatch { on: true })));
    // Esc closes the overlay and stops the worker-side watch.
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    assert!(state.mcp_overlay.is_none());
    assert!(effects.contains(&Effect::Send(UiCommand::McpWatch { on: false })));
}

#[test]
fn mcp_overlay_remove_requires_confirmation() {
    let mut state = AppState::new();
    state.mcp_servers = vec![crate::api::McpServerView {
        name: "fs".into(),
        transport: "stdio",
        target: "npx fs".into(),
        status: crate::api::McpStatusView::Ready,
        tools: Some(3),
        error: None,
        ..Default::default()
    }];
    state.composer.insert_text("/mcp");
    reduce(&mut state, Action::Key(enter()));
    // d arms confirmation; nothing is sent yet.
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
    );
    assert!(effects
        .iter()
        .all(|effect| !matches!(effect, Effect::Send(UiCommand::McpRemove { .. }))));
    // A non-confirming key cancels the armed removal.
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
    );
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
    );
    let _ = effects;
    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
    );
    assert!(effects.contains(&Effect::Send(UiCommand::McpRemove { name: "fs".into() })));
}

#[test]
fn mcp_overlay_enter_tests_selected_server() {
    let mut state = AppState::new();
    state.mcp_servers = vec![
        crate::api::McpServerView {
            name: "fs".into(),
            transport: "stdio",
            target: "npx fs".into(),
            status: crate::api::McpStatusView::Disconnected,
            tools: None,
            error: None,
            ..Default::default()
        },
        crate::api::McpServerView {
            name: "web".into(),
            transport: "http",
            target: "https://mcp.example.com".into(),
            status: crate::api::McpStatusView::Disconnected,
            tools: None,
            error: None,
            ..Default::default()
        },
    ];
    state.composer.insert_text("/mcp");
    reduce(&mut state, Action::Key(enter()));
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpTest { name: "web".into() })));
}

#[test]
fn mcp_add_parses_stdio_and_http_forms() {
    let mut state = AppState::new();
    state.composer.insert_text("/mcp add fs npx -y fs-server");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpAdd {
        name: "fs".into(),
        command: Some("npx".into()),
        args: vec!["-y".into(), "fs-server".into()],
        url: None,
        global: false,
    })));

    let mut state = AppState::new();
    state
        .composer
        .insert_text("/mcp add web --url https://mcp.example.com --global");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpAdd {
        name: "web".into(),
        command: None,
        args: Vec::new(),
        url: Some("https://mcp.example.com".into()),
        global: true,
    })));
}

#[test]
fn mcp_add_accepts_quoted_arguments_shell_style() {
    let mut state = AppState::new();
    state.composer.insert_text(
        r#"/mcp add fs "C:\Program Files\node\node.exe" --flag "two words" 'single $x' "" "say \"hi\"""#,
    );
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpAdd {
        name: "fs".into(),
        command: Some(r"C:\Program Files\node\node.exe".into()),
        args: vec![
            "--flag".into(),
            "two words".into(),
            "single $x".into(),
            String::new(),
            r#"say "hi""#.into(),
        ],
        url: None,
        global: false,
    })));

    let mut state = AppState::new();
    state
        .composer
        .insert_text(r#"/mcp add web --url "https://mcp.example.com/a b" --global"#);
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpAdd {
        name: "web".into(),
        command: None,
        args: Vec::new(),
        url: Some("https://mcp.example.com/a b".into()),
        global: true,
    })));
}

#[test]
fn mcp_add_with_unterminated_quote_keeps_the_draft_and_sends_nothing() {
    let mut state = AppState::new();
    state.composer.insert_text(r#"/mcp add fs npx "oops"#);
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects
        .iter()
        .all(|effect| !matches!(effect, Effect::Send(UiCommand::McpAdd { .. }))));
    assert!(state
        .notifications
        .iter()
        .any(|note| note.contains("aspas não fechadas")));
    assert!(state.composer.payload().contains("/mcp add fs npx"));
}

#[test]
fn mcp_trust_and_untrust_subcommands_dispatch_trust_commands() {
    let mut state = AppState::new();
    state.composer.insert_text("/mcp trust");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpTrust {
        trust: true,
        name: None,
    })));

    let mut state = AppState::new();
    state.composer.insert_text("/mcp untrust");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpTrust {
        trust: false,
        name: None,
    })));
}

#[test]
fn mcp_login_and_logout_subcommands_dispatch_oauth_commands() {
    let mut state = AppState::new();
    state.composer.insert_text("/mcp login notion");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpLogin {
        name: "notion".into(),
        redirect_url: None,
    })));

    // A pasted redirect URL goes to the sign-in already running.
    let mut state = AppState::new();
    state
        .composer
        .insert_text("/mcp login notion http://127.0.0.1:5000/callback?code=abc&state=xyz");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpLogin {
        name: "notion".into(),
        redirect_url: Some("http://127.0.0.1:5000/callback?code=abc&state=xyz".into()),
    })));

    let mut state = AppState::new();
    state.composer.insert_text("/mcp logout notion");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(effects.contains(&Effect::Send(UiCommand::McpLogout {
        name: "notion".into(),
    })));
}

#[test]
fn mcp_login_and_logout_need_a_server_name() {
    for command in ["/mcp login", "/mcp logout"] {
        let mut state = AppState::new();
        state.composer.insert_text(command);
        let effects = reduce(&mut state, Action::Key(enter()));
        assert!(effects.iter().all(|effect| !matches!(
            effect,
            Effect::Send(UiCommand::McpLogin { .. } | UiCommand::McpLogout { .. })
        )));
        assert!(
            state
                .notifications
                .iter()
                .any(|note| note.contains("login <nome>")),
            "{command}: usage expected"
        );
        assert_eq!(state.composer.payload(), command, "the draft is kept");
    }
}

fn mcp_server(name: &str) -> crate::api::McpServerView {
    crate::api::McpServerView {
        name: name.into(),
        transport: "stdio",
        target: "cmd".into(),
        status: crate::api::McpStatusView::Disconnected,
        tools: None,
        error: None,
        ..Default::default()
    }
}

fn open_mcp(state: &mut AppState) {
    state.composer.insert_text("/mcp");
    reduce(state, Action::Key(enter()));
}

#[test]
fn mcp_overlay_ignores_modified_destructive_keys() {
    let mut state = AppState::new();
    state.mcp_servers = vec![mcp_server("fs")];
    open_mcp(&mut state);
    for code in [
        KeyCode::Char('r'),
        KeyCode::Char('x'),
        KeyCode::Char('d'),
        KeyCode::Char('y'),
    ] {
        let effects = reduce(
            &mut state,
            Action::Key(KeyEvent::new(code, KeyModifiers::CONTROL)),
        );
        assert!(
            effects.iter().all(|effect| !matches!(
                effect,
                Effect::Send(UiCommand::McpReconnect { .. })
                    | Effect::Send(UiCommand::McpDisconnect { .. })
                    | Effect::Send(UiCommand::McpRemove { .. })
            )),
            "Ctrl+{code:?} must not fire an MCP action"
        );
    }
    assert!(state.mcp_overlay.is_some());
}

#[test]
fn mcp_command_during_run_opens_overlay_instead_of_queuing() {
    let mut state = AppState::new();
    state.working = true;
    state.composer.insert_text("/mcp");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(state.mcp_overlay.is_some());
    assert!(state.queued_prompts.is_empty());
    assert!(effects.contains(&Effect::Send(UiCommand::McpRefresh)));
    assert!(effects.contains(&Effect::Send(UiCommand::McpWatch { on: true })));
}

#[test]
fn mcp_mutating_subcommand_during_run_is_dispatched_not_queued() {
    // The worker's active-run catch-all rejects it with a notification;
    // what must never happen is the text reaching the prompt queue.
    let mut state = AppState::new();
    state.working = true;
    state.composer.insert_text("/mcp reconnect fs");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(state.queued_prompts.is_empty());
    assert!(effects.contains(&Effect::Send(UiCommand::McpReconnect { name: "fs".into() })));
    assert!(effects.iter().all(|effect| !matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
    )));
}

#[test]
fn mcp_overlay_open_clears_search() {
    // Search captures keys before overlays do; the palette (Ctrl+P) is
    // the only route that can stack /mcp over it — the overlay must win.
    let mut state = AppState::new();
    state.search = Some(crate::inspector::SearchState::default());
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
    );
    for character in "mcp".chars() {
        reduce(
            &mut state,
            Action::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
        );
    }
    reduce(&mut state, Action::Key(enter()));
    assert!(state.search.is_none());
    assert!(state.mcp_overlay.is_some());
}

#[test]
fn mcp_selection_follows_server_name_across_snapshots() {
    let mut state = AppState::new();
    state.mcp_servers = vec![mcp_server("a"), mcp_server("b"), mcp_server("c")];
    open_mcp(&mut state);
    // Select "c" (index 2).
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    assert_eq!(state.mcp_overlay.as_ref().unwrap().selected, 2);
    // Snapshot with "a" removed: "c" now sits at index 1 and the cursor
    // must follow the name, not the numeric position.
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::McpServersChanged {
            servers: vec![mcp_server("b"), mcp_server("c")],
        }),
    );
    let overlay = state.mcp_overlay.as_ref().unwrap();
    assert_eq!(overlay.selected, 1);
    assert_eq!(state.mcp_servers[overlay.selected].name, "c");
    // Selected server removed entirely: clamp into range.
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::McpServersChanged {
            servers: vec![mcp_server("b")],
        }),
    );
    assert_eq!(state.mcp_overlay.as_ref().unwrap().selected, 0);
}

#[test]
fn mcp_malformed_subcommand_shows_usage_and_keeps_draft() {
    let mut state = AppState::new();
    state.composer.insert_text("/mcp frobnicate");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(state.composer.payload().contains("/mcp frobnicate"));
    assert!(effects.iter().all(|effect| !matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
    )));
    assert!(state
        .notifications
        .iter()
        .any(|notification| notification.message.starts_with("Uso: /mcp")));
}

#[test]
fn parallel_tool_activity_keeps_running_call_until_each_identity_ends() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunStarted {
        run_id: 1,
        max_mutating_tool_calls: 8,
        max_read_tool_calls: 8,
        max_turns: 4,
    });
    let batch_a = crate::api::ToolBatchId("batch-a".into());
    let call_a = crate::api::ToolCallId("call-a".into());
    let batch_b = crate::api::ToolBatchId("batch-b".into());
    let call_b = crate::api::ToolCallId("call-b".into());

    state.apply_event(UiEvent::ToolAdmitted {
        batch_id: batch_a.clone(),
        call_id: call_a.clone(),
        name: "search".into(),
    });
    assert_eq!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(&ActivityPhase::QueuedTool("search".into()))
    );
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch_a.clone(),
        call_id: call_a.clone(),
        name: "search".into(),
        arguments_summary: "{}".into(),
    });
    state.apply_event(UiEvent::ToolAdmitted {
        batch_id: batch_b.clone(),
        call_id: call_b.clone(),
        name: "write".into(),
    });
    assert_eq!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(&ActivityPhase::RunningTool("search".into()))
    );
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch_b,
        call_id: call_b,
        name: "write".into(),
        arguments_summary: "{}".into(),
    });
    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch_a,
        call_id: call_a,
        name: "search".into(),
        success: true,
        duration_ms: 10,
    });
    assert_eq!(
        state.activity.as_ref().map(|activity| &activity.phase),
        Some(&ActivityPhase::RunningTool("write".into()))
    );
}

#[test]
fn terminal_tool_output_is_applied_before_the_matching_call_closes() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunStarted {
        run_id: 9,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    });
    let batch_id = crate::api::ToolBatchId("batch-mcp".into());
    let call_id = crate::api::ToolCallId("old-call".into());
    state.apply_event(UiEvent::ToolStarted {
        batch_id: batch_id.clone(),
        call_id: call_id.clone(),
        name: "mcp".into(),
        arguments_summary: "{}".into(),
    });
    state.apply_event(UiEvent::ToolOutput {
        batch_id: batch_id.clone(),
        call_id: call_id.clone(),
        name: "mcp".into(),
        output: "mcp operation outcome is uncertain".into(),
        content_handle: None,
    });
    let tool_state = state
        .blocks()
        .iter()
        .find_map(|block| match block.kind() {
            crate::block::BlockKind::Tool(tool)
                if tool.batch_id == batch_id && tool.call_id == call_id =>
            {
                Some(tool)
            }
            _ => None,
        })
        .expect("matching MCP block receives its final output");
    assert!(tool_state.preview.contains("outcome is uncertain"));

    state.apply_event(UiEvent::ToolEnded {
        batch_id: batch_id.clone(),
        call_id: call_id.clone(),
        name: "mcp".into(),
        success: false,
        duration_ms: 12,
    });
    let tool_state = state
        .blocks()
        .iter()
        .find_map(|block| match block.kind() {
            crate::block::BlockKind::Tool(tool)
                if tool.batch_id == batch_id && tool.call_id == call_id =>
            {
                Some(tool)
            }
            _ => None,
        })
        .expect("terminal MCP block remains visible");
    assert!(tool_state.preview.contains("outcome is uncertain"));
    assert_eq!(tool_state.duration_ms, Some(12));
}

#[test]
fn retry_state_is_cleared_by_provider_phase_and_late_retry_is_ignored() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunStarted {
        run_id: 7,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    });
    state.apply_event(UiEvent::RetryScheduled {
        attempt: 1,
        limit: 3,
        wait_ms: 250,
        reason: Some("timeout".into()),
    });
    assert!(state.retry.is_some());
    state.apply_event(UiEvent::ProviderPhaseChanged {
        phase: slim_core::ProviderPhase::HeadersReceived,
        label: "headers".into(),
        elapsed_ms: 11,
    });
    assert!(state.retry.is_none());
    state.apply_event(UiEvent::RunCompleted { run_id: 7 });
    state.apply_event(UiEvent::RetryScheduled {
        attempt: 2,
        limit: 3,
        wait_ms: 500,
        reason: None,
    });
    assert!(state.retry.is_none());
}

#[test]
fn explicit_cancel_pauses_queue_until_deliberate_resume() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunStarted {
        run_id: 3,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    });
    state.enqueue_queued_prompt("first".into());
    state.enqueue_queued_prompt("second".into());
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert_eq!(
        state.cancellation.map(|cancellation| cancellation.phase),
        Some(CancellationPhase::Requested)
    );
    let effects = reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::RunStopped {
            run_id: 3,
            message: "cancelled".into(),
        }),
    );
    assert!(state.queue_paused);
    assert_eq!(state.queue_len(), 2);
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
    )));

    state.composer.insert_text("/queue resume");
    let effects = reduce(&mut state, Action::Key(enter()));
    assert!(!state.queue_paused);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { prompt, .. }) if prompt == "first"
    )));
    assert_eq!(state.queue_len(), 1);

    state.composer.insert_text("/queue edit 1");
    reduce(&mut state, Action::Key(enter()));
    assert_eq!(state.composer.payload(), "second");
    assert_eq!(state.queue_len(), 0);
}

#[test]
fn explicit_cancel_keeps_queue_paused_even_if_completion_wins_race() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::RunStarted {
        run_id: 4,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    });
    state.enqueue_queued_prompt("after cancel".into());
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    let effects = reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::RunCompleted { run_id: 4 }),
    );
    assert!(state.queue_paused);
    assert_eq!(state.queue_len(), 1);
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(UiCommand::PreparePrompt { .. } | UiCommand::SendPrompt(_))
    )));
    assert_eq!(
        state.last_execution.as_ref().map(|summary| summary.outcome),
        Some(RunOutcomeKind::Completed)
    );
}

#[test]
fn preparation_cancel_terminal_overtaking_run_start_does_not_replay_or_resume_queue() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("possibly sent");
    let admission = reduce(&mut state, Action::Key(enter()))
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Send(UiCommand::PreparePrompt { admission, .. }) => Some(admission),
            _ => None,
        })
        .expect("direct prompt admitted");
    reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    );
    state.enqueue_queued_prompt("next prompt".into());

    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::PromptRunCancelled {
            admission,
            run_id: 11,
        }),
    );
    assert!(state.queue_paused);
    assert_eq!(state.queue_len(), 1);
    assert!(state.prompt_preparation().is_none());
    assert!(!state.working);

    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::PromptRunStarted {
            admission,
            run_id: 11,
            max_mutating_tool_calls: 8,
            max_read_tool_calls: 8,
            max_turns: 8,
        }),
    );
    assert!(!state.working);
    assert_eq!(state.queue_len(), 1);
    assert_eq!(state.queued_prompt(0), Some("next prompt"));
}

#[test]
fn execution_summary_survives_next_run_and_tracks_outcome() {
    let mut state = AppState::new();
    state.clock.elapsed_ms = 100;
    state.apply_event(UiEvent::RunStarted {
        run_id: 1,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    });
    state.clock.elapsed_ms = 275;
    state.apply_event(UiEvent::RunCompleted { run_id: 1 });
    let summary = state.last_execution.as_ref().expect("completed summary");
    assert_eq!(summary.run_id, 1);
    assert_eq!(summary.duration_ms, 175);
    assert_eq!(summary.outcome, RunOutcomeKind::Completed);
    state.apply_event(UiEvent::RunStarted {
        run_id: 2,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    });
    assert_eq!(
        state.last_execution.as_ref().map(|summary| summary.run_id),
        Some(1)
    );
    state.clock.elapsed_ms = 300;
    state.apply_event(UiEvent::RunStopped {
        run_id: 2,
        message: "stopped".into(),
    });
    assert_eq!(state.execution_history.len(), 2);
    assert_eq!(
        state.last_execution.as_ref().map(|summary| summary.outcome),
        Some(RunOutcomeKind::Interrupted)
    );
}

#[test]
fn todo_dock_preference_survives_todo_updates() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::TodoChanged {
        items: vec![crate::api::TodoItemView {
            reason: None,
            id: None,
            title: "pending".into(),
            status: crate::api::TodoItemStatus::Pending,
        }],
    });
    assert!(!state.todo_dock_open);
    reduce(&mut state, Action::ToggleTodoDock);
    assert!(state.todo_dock_open);
    state.apply_event(UiEvent::TodoChanged {
        items: vec![crate::api::TodoItemView {
            reason: None,
            id: None,
            title: "completed".into(),
            status: crate::api::TodoItemStatus::Completed,
        }],
    });
    assert!(state.todo_dock_open);
    assert_eq!(state.todo_dock_user_preference, Some(true));
    reduce(&mut state, Action::ToggleTodoDock);
    state.apply_event(UiEvent::TodoChanged {
        items: vec![crate::api::TodoItemView {
            reason: None,
            id: None,
            title: "still pending".into(),
            status: crate::api::TodoItemStatus::Pending,
        }],
    });
    assert!(!state.todo_dock_open);
    assert_eq!(state.todo_dock_user_preference, Some(false));
}

#[test]
fn confirmed_setting_records_effective_changes_only() {
    let mut state = AppState::new();
    state.clock.elapsed_ms = 10;
    state.apply_event(UiEvent::ModeChanged {
        mode: slim_core::OperatingMode::Auto,
    });
    assert!(state.confirmed_setting.is_none());
    state.apply_event(UiEvent::ModeChanged {
        mode: slim_core::OperatingMode::ReadOnly,
    });
    assert_eq!(state.confirmed_setting, Some((ConfirmedSetting::Mode, 10)));
    state.clock.elapsed_ms = 20;
    state.apply_event(UiEvent::EffortChanged {
        effort: ReasoningEffort::Low,
    });
    assert_eq!(
        state.confirmed_setting,
        Some((ConfirmedSetting::Effort, 20))
    );
    state.clock.elapsed_ms = 30;
    state.apply_event(UiEvent::ModelChanged {
        model: "gpt-5.6-terra".into(),
    });
    assert_eq!(state.confirmed_setting, Some((ConfirmedSetting::Model, 30)));
}

#[test]
fn notification_coalescing_preserves_priority_and_history() {
    let mut state = AppState::new();
    state.clock.elapsed_ms = 1;
    state.push_notification("same".into());
    state.clock.elapsed_ms = 2;
    state.push_notification("same".into());
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications[0].repeat_count, 2);
    assert_eq!(state.notification_history()[0].repeat_count, 2);
    state.push_notification_with_priority("same".into(), NotificationPriority::Warning);
    state.push_notification_with_priority("failure".into(), NotificationPriority::Error);
    let toast = state.visible_toast_tail(3);
    assert_eq!(toast.len(), 1);
    assert_eq!(toast[0].message, "failure");
    assert_eq!(toast[0].priority, NotificationPriority::Error);
}

#[test]
fn thinking_preview_retention_uses_thinking_start_and_releases_at_boundary() {
    let mut state = AppState::new();
    state.clock.elapsed_ms = 10;
    state.apply_event(UiEvent::RunStarted {
        run_id: 9,
        max_mutating_tool_calls: 1,
        max_read_tool_calls: 1,
        max_turns: 1,
    });
    state.clock.elapsed_ms = 20;
    state.apply_event(UiEvent::ThinkingStarted);
    state.clock.elapsed_ms = 50;
    state.apply_event(UiEvent::ThinkingDelta {
        text: "reason".into(),
    });
    let thinking_id = state
        .blocks()
        .iter()
        .find(|block| matches!(block.kind(), crate::block::BlockKind::Thinking(_)))
        .map(|block| block.id.clone())
        .expect("thinking block");
    let thinking = state
        .blocks()
        .iter()
        .find(|block| block.id == thinking_id)
        .unwrap();
    assert_eq!(thinking.started_ms, Some(20));
    state.clock.elapsed_ms = 60;
    state.apply_event(UiEvent::ThinkingEnded);
    assert!(state
        .blocks()
        .iter()
        .find(|block| block.id == thinking_id)
        .is_some_and(|block| block.ended_ms == Some(60)
            && block.lifecycle == crate::block::BlockLifecycle::Complete));
}

#[test]
fn approval_decision_waits_until_runtime_marks_content_accessible() {
    let mut state = AppState::new();
    let request_id = crate::api::InteractionRequestId("approval-1".into());
    state.apply_event(UiEvent::ApprovalRequired {
        request_id: request_id.clone(),
        summary: "run command".into(),
        persisted: false,
    });
    let blocked = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
    );
    assert!(!blocked
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::Approve { .. }))));
    reduce(&mut state, Action::SetApprovalContentAccessible(true));
    let approved = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
    );
    assert!(approved
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::Approve { request_id: id }) if id == &request_id)));
}
