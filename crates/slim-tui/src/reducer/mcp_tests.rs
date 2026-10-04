//! `/mcp` manager keys, subcommands and the OAuth sign-in panel
//! (DESIGN-SLIM-TUI §15.7.2).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{reduce, Action, Effect};
use crate::api::{McpServerView, McpStatusView, SensitiveText, UiCommand, UiEvent};
use crate::app::{AppState, McpConfirm};

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(character: char) -> Action {
    key(KeyCode::Char(character))
}

fn ctrl(character: char) -> Action {
    Action::Key(KeyEvent::new(
        KeyCode::Char(character),
        KeyModifiers::CONTROL,
    ))
}

fn server(name: &str, transport: &'static str, status: McpStatusView) -> McpServerView {
    McpServerView {
        name: name.into(),
        transport,
        target: "target".into(),
        status,
        ..McpServerView::default()
    }
}

fn open(state: &mut AppState) {
    state.composer.insert_text("/mcp");
    reduce(state, key(KeyCode::Enter));
}

fn sent(effects: &[Effect]) -> Vec<UiCommand> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(command) => Some(command.clone()),
            _ => None,
        })
        .collect()
}

fn state_with(servers: Vec<McpServerView>) -> AppState {
    let mut state = AppState::new();
    state.mcp_servers = servers;
    open(&mut state);
    state
}

#[test]
fn a_toggles_enabled_from_the_selected_status() {
    let mut state = state_with(vec![
        server("on", "stdio", McpStatusView::Ready),
        server("off", "stdio", McpStatusView::Disabled),
    ]);
    let effects = reduce(&mut state, ch('a'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::McpEnable {
            name: "on".into(),
            enabled: false
        }]
    );
    reduce(&mut state, key(KeyCode::Down));
    let effects = reduce(&mut state, ch('a'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::McpEnable {
            name: "off".into(),
            enabled: true
        }]
    );
}

#[test]
fn l_signs_in_http_servers_only() {
    let mut state = state_with(vec![
        server("web", "http", McpStatusView::NeedsAuth),
        server("local", "stdio", McpStatusView::Ready),
    ]);
    let effects = reduce(&mut state, ch('l'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::McpLogin {
            name: "web".into(),
            redirect_url: None
        }]
    );
    reduce(&mut state, key(KeyCode::Down));
    let effects = reduce(&mut state, ch('l'));
    assert!(sent(&effects).is_empty());
    assert!(state
        .mcp_overlay
        .as_ref()
        .and_then(|overlay| overlay.notice.as_deref())
        .is_some_and(|notice| notice.contains("http")));
}

#[test]
fn o_signs_out_only_after_confirmation_and_only_http() {
    let mut state = state_with(vec![
        server("web", "http", McpStatusView::Ready),
        server("local", "stdio", McpStatusView::Ready),
    ]);
    let effects = reduce(&mut state, ch('o'));
    assert!(sent(&effects).is_empty(), "armed, nothing sent yet");
    assert_eq!(
        state.mcp_overlay.as_ref().unwrap().confirm,
        Some(McpConfirm::Logout("web".into()))
    );
    // Esc only cancels the confirmation; the overlay stays.
    let effects = reduce(&mut state, key(KeyCode::Esc));
    assert!(sent(&effects).is_empty());
    assert!(state.mcp_overlay.is_some());
    assert_eq!(state.mcp_overlay.as_ref().unwrap().confirm, None);

    reduce(&mut state, ch('o'));
    let effects = reduce(&mut state, ch('y'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::McpLogout { name: "web".into() }]
    );

    reduce(&mut state, key(KeyCode::Down));
    reduce(&mut state, ch('o'));
    assert_eq!(state.mcp_overlay.as_ref().unwrap().confirm, None);
}

#[test]
fn t_trusts_the_project_from_an_untrusted_server_only() {
    let mut state = state_with(vec![
        server("proj", "stdio", McpStatusView::Untrusted),
        server("ok", "stdio", McpStatusView::Ready),
    ]);
    let effects = reduce(&mut state, ch('t'));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::McpTrust {
            trust: true,
            name: Some("proj".into())
        }]
    );
    reduce(&mut state, key(KeyCode::Down));
    let effects = reduce(&mut state, ch('t'));
    assert!(sent(&effects).is_empty());
    assert!(state.mcp_overlay.as_ref().unwrap().notice.is_some());
}

#[test]
fn action_keys_clear_the_previous_notice_but_navigation_keeps_it() {
    let mut state = state_with(vec![
        server("a", "stdio", McpStatusView::Ready),
        server("b", "stdio", McpStatusView::Ready),
    ]);
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::Notification {
            message: "mcp a: reconectado".into(),
        }),
    );
    assert_eq!(
        state.mcp_overlay.as_ref().unwrap().notice.as_deref(),
        Some("mcp a: reconectado")
    );
    reduce(&mut state, key(KeyCode::Down));
    assert!(state.mcp_overlay.as_ref().unwrap().notice.is_some());
    reduce(&mut state, ch('r'));
    assert_eq!(state.mcp_overlay.as_ref().unwrap().notice, None);
}

#[test]
fn modified_action_keys_do_nothing_for_the_new_actions() {
    let mut state = state_with(vec![server("web", "http", McpStatusView::Disabled)]);
    for character in ['a', 'l', 'o', 't'] {
        let effects = reduce(&mut state, ctrl(character));
        assert!(sent(&effects).is_empty(), "Ctrl+{character}");
    }
    assert_eq!(state.mcp_overlay.as_ref().unwrap().confirm, None);
}

#[test]
fn enable_disable_trust_subcommands_dispatch_with_quoted_names() {
    let mut state = AppState::new();
    state.composer.insert_text(r#"/mcp enable "my server""#);
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(effects.contains(&Effect::Send(UiCommand::McpEnable {
        name: "my server".into(),
        enabled: true
    })));

    let mut state = AppState::new();
    state.composer.insert_text("/mcp disable 'a b'");
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(effects.contains(&Effect::Send(UiCommand::McpEnable {
        name: "a b".into(),
        enabled: false
    })));

    let mut state = AppState::new();
    state.composer.insert_text("/mcp trust proj");
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(effects.contains(&Effect::Send(UiCommand::McpTrust {
        trust: true,
        name: Some("proj".into())
    })));

    let mut state = AppState::new();
    state.composer.insert_text("/mcp untrust");
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(effects.contains(&Effect::Send(UiCommand::McpTrust {
        trust: false,
        name: None
    })));
}

#[test]
fn enable_and_disable_need_a_server_name() {
    for command in ["/mcp enable", "/mcp disable"] {
        let mut state = AppState::new();
        state.composer.insert_text(command);
        let effects = reduce(&mut state, key(KeyCode::Enter));
        assert!(sent(&effects).is_empty(), "{command}");
        assert!(
            state
                .notifications
                .iter()
                .any(|note| note.message.contains("enable <nome>")),
            "{command}: usage expected"
        );
        assert_eq!(state.composer.payload(), command, "the draft is kept");
    }
}

fn authorize(state: &mut AppState, name: &str, url: &str) {
    reduce(
        state,
        Action::UiEventReceived(UiEvent::McpAuthorization {
            name: name.into(),
            url: SensitiveText::from(url.to_owned()),
            browser_opened: true,
        }),
    );
}

#[test]
fn the_authorization_event_opens_the_sign_in_panel_even_with_the_manager_closed() {
    let mut state = AppState::new();
    state.composer.insert_text("draft stays");
    authorize(&mut state, "web", "https://auth.example/authorize?x=1");
    let overlay = state.mcp_overlay.as_ref().expect("overlay");
    let signin = overlay.signin.as_ref().expect("panel");
    assert_eq!(signin.name, "web");
    assert_eq!(signin.url.expose(), "https://auth.example/authorize?x=1");
    assert!(overlay.signin_only);
    assert_eq!(state.composer.payload(), "draft stays");
}

#[test]
fn the_panel_takes_typing_and_paste_and_never_the_list_keys() {
    let mut state = AppState::new();
    authorize(&mut state, "web", "https://auth.example/a");
    // `r` and `x` would reconnect/disconnect in the list: here they are text.
    let mut sent_commands = Vec::new();
    for action in [ch('h'), ch('r'), ch('x'), ch('t')] {
        sent_commands.extend(sent(&reduce(&mut state, action)));
    }
    assert!(sent_commands.is_empty());
    let signin = state.mcp_overlay.as_ref().unwrap().signin.as_ref().unwrap();
    assert_eq!(signin.input.expose(), "hrxt");

    reduce(&mut state, key(KeyCode::Backspace));
    reduce(
        &mut state,
        Action::Paste("ttp://127.0.0.1:1/callback?code=c &state=s\r\n".into()),
    );
    let signin = state.mcp_overlay.as_ref().unwrap().signin.as_ref().unwrap();
    assert_eq!(
        signin.input.expose(),
        "hrxttp://127.0.0.1:1/callback?code=c&state=s",
        "whitespace and newlines are dropped"
    );
    assert!(state.composer.payload().is_empty(), "never the composer");
}

#[test]
fn enter_hands_the_pasted_redirect_to_the_sign_in_and_clears_the_field() {
    let mut state = AppState::new();
    authorize(&mut state, "web", "https://auth.example/a");
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert!(sent(&effects).is_empty(), "an empty field sends nothing");

    reduce(
        &mut state,
        Action::Paste("http://127.0.0.1:9/callback?code=c&state=s".into()),
    );
    let effects = reduce(&mut state, key(KeyCode::Enter));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::McpLogin {
            name: "web".into(),
            redirect_url: Some("http://127.0.0.1:9/callback?code=c&state=s".into()),
        }]
    );
    let signin = state.mcp_overlay.as_ref().unwrap().signin.as_ref().unwrap();
    assert!(signin.input.is_empty());
}

#[test]
fn ctrl_y_copies_the_whole_authorization_url() {
    let url = format!("https://auth.example/authorize?{}", "q=1&".repeat(300));
    let mut state = AppState::new();
    authorize(&mut state, "web", &url);
    let effects = reduce(&mut state, ctrl('y'));
    assert!(effects.contains(&Effect::CopyToClipboard(url)));
}

#[test]
fn esc_and_ctrl_c_cancel_the_sign_in_and_close_the_panel() {
    for cancel in [key(KeyCode::Esc), ctrl('c')] {
        let mut state = AppState::new();
        authorize(&mut state, "web", "https://auth.example/a");
        let effects = reduce(&mut state, cancel);
        assert_eq!(
            sent(&effects),
            vec![UiCommand::McpLoginCancel { name: "web".into() }]
        );
        assert!(state.mcp_overlay.is_none(), "the panel carried the overlay");
        assert!(
            !state.shutdown,
            "Ctrl+C must not quit while the panel is up"
        );
    }

    // Opened over the list: dismissing the panel returns to the list.
    let mut state = state_with(vec![server("web", "http", McpStatusView::NeedsAuth)]);
    authorize(&mut state, "web", "https://auth.example/a");
    assert!(!state.mcp_overlay.as_ref().unwrap().signin_only);
    let effects = reduce(&mut state, key(KeyCode::Esc));
    assert_eq!(
        sent(&effects),
        vec![UiCommand::McpLoginCancel { name: "web".into() }]
    );
    let overlay = state.mcp_overlay.as_ref().expect("list stays");
    assert!(overlay.signin.is_none());
}

#[test]
fn the_end_of_the_sign_in_closes_the_panel_it_belongs_to() {
    let mut state = AppState::new();
    authorize(&mut state, "web", "https://auth.example/a");
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::McpLoginEnded {
            name: "other".into(),
        }),
    );
    assert!(
        state.mcp_overlay.is_some(),
        "another server's end is ignored"
    );
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::McpLoginEnded { name: "web".into() }),
    );
    assert!(state.mcp_overlay.is_none());

    let mut state = state_with(vec![server("web", "http", McpStatusView::NeedsAuth)]);
    authorize(&mut state, "web", "https://auth.example/a");
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::McpLoginEnded { name: "web".into() }),
    );
    let overlay = state.mcp_overlay.as_ref().expect("list stays");
    assert!(overlay.signin.is_none());
}

#[test]
fn oversized_pastes_are_refused_with_a_notice_on_the_modal() {
    let mut state = AppState::new();
    authorize(&mut state, "web", "https://auth.example/a");
    reduce(&mut state, Action::Paste("a".repeat(4_096)));
    reduce(&mut state, Action::Paste("b".into()));
    let overlay = state.mcp_overlay.as_ref().unwrap();
    assert_eq!(overlay.signin.as_ref().unwrap().input.char_len(), 4_096);
    // Toasts are hidden while /mcp is open: the notice row is where it shows.
    assert!(
        overlay
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("URL grande demais")),
        "{:?}",
        overlay.notice
    );
    assert!(state.notifications.is_empty(), "{:?}", state.notifications);
}

#[test]
fn clipboard_text_reaches_the_field_and_an_image_never_attaches() {
    let mut state = AppState::new();
    authorize(&mut state, "web", "https://auth.example/a");
    let effects = reduce(
        &mut state,
        Action::ClipboardPull {
            image: Some("C:\\tmp\\shot.png".into()),
            text: Some("http://127.0.0.1:9/callback?code=1&state=2".into()),
        },
    );
    assert!(sent(&effects).is_empty());
    let signin = state.mcp_overlay.as_ref().unwrap().signin.as_ref().unwrap();
    assert_eq!(
        signin.input.expose(),
        "http://127.0.0.1:9/callback?code=1&state=2"
    );
}

#[test]
fn debug_output_never_prints_the_authorization_url_or_the_pasted_redirect() {
    let mut state = AppState::new();
    authorize(
        &mut state,
        "web",
        "https://auth.example/a?state=SECRETSTATE",
    );
    reduce(
        &mut state,
        Action::Paste("http://127.0.0.1:9/callback?code=SECRETCODE".into()),
    );
    let printed = format!("{:?}", state.mcp_overlay);
    assert!(!printed.contains("SECRETSTATE"), "{printed}");
    assert!(!printed.contains("SECRETCODE"), "{printed}");
}

#[test]
fn a_snapshot_dropping_the_server_disarms_a_pending_logout_confirmation() {
    let mut state = state_with(vec![server("web", "http", McpStatusView::Ready)]);
    reduce(&mut state, ch('o'));
    assert!(state.mcp_overlay.as_ref().unwrap().confirm.is_some());
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::McpServersChanged {
            servers: Vec::new(),
        }),
    );
    assert_eq!(state.mcp_overlay.as_ref().unwrap().confirm, None);
}
