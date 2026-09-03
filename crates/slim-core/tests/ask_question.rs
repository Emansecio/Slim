use slim_core::{
    ask_question_definition, interaction_route, AskQuestion, EventKind, InteractionError,
    InteractionRequestId, QuestionAnswer,
};

#[test]
fn question_accepts_open_or_multiple_choice_payloads() {
    let open = AskQuestion::parse(r#"{"question":"Explain the constraint"}"#)
        .expect("open question should parse");
    assert!(open.options.is_empty());

    let choices = AskQuestion::parse(
        r#"{
            "question":"Choose a crate",
            "options":[
                {"label":"core","description":"Protocol and runtime"},
                {"label":"tui","description":"Reducer and render"}
            ]
        }"#,
    )
    .expect("multiple-choice question should parse");
    assert_eq!(choices.options.len(), 2);
}

#[test]
fn question_rejects_unknown_fields_at_every_level() {
    assert!(AskQuestion::parse(r#"{"question":"Choose","unexpected":true}"#).is_err());
    assert!(AskQuestion::parse(
        r#"{
            "question":"Choose",
            "options":[
                {"label":"core","description":"Runtime","unexpected":true},
                {"label":"tui","description":"Interface"}
            ]
        }"#,
    )
    .is_err());
}

#[tokio::test]
async fn responder_delivers_once_to_the_matching_request() {
    let (route, responder) = interaction_route();
    let request_id = InteractionRequestId::new("call-1").expect("valid request id");
    let pending = route
        .register(request_id.clone())
        .expect("register request");

    responder
        .answer(
            request_id.clone(),
            QuestionAnswer::custom("custom details").expect("valid answer"),
        )
        .expect("answer pending request");
    assert_eq!(
        pending.receive().await.expect("receive answer").answer,
        "custom details"
    );
    assert!(matches!(
        responder.answer(
            request_id,
            QuestionAnswer::custom("late").expect("valid answer")
        ),
        Err(InteractionError::StaleRequest { .. })
    ));
}

#[test]
fn sequential_questions_are_not_capped_by_completed_history() {
    let (route, responder) = interaction_route();
    for index in 0..65 {
        let request_id =
            InteractionRequestId::new(format!("call-{index}")).expect("valid request id");
        let pending = route
            .register(request_id.clone())
            .expect("register sequential question");
        responder
            .answer(
                request_id,
                QuestionAnswer::custom("ok").expect("valid answer"),
            )
            .expect("answer sequential question");
        drop(pending);
    }
}

#[test]
fn question_event_and_provider_schema_preserve_structured_options() {
    let definition = ask_question_definition();
    assert_eq!(definition["name"], "ask_question");
    assert_eq!(
        definition["input_schema"]["properties"]["options"]["maxItems"],
        5
    );

    let json = serde_json::to_value(EventKind::QuestionRequired {
        request_id: "call-1".into(),
        question: "Choose a crate".into(),
        options: vec![
            slim_core::QuestionOption {
                label: "core".into(),
                description: "Protocol".into(),
            },
            slim_core::QuestionOption {
                label: "tui".into(),
                description: "Interface".into(),
            },
        ],
        persisted: false,
    })
    .expect("serialize question event");
    assert_eq!(json["type"], "QuestionRequired");
    assert_eq!(json["options"][1]["label"], "tui");
}
