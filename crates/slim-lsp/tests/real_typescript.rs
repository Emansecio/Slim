//! Explicit gate against installed TypeScript Language Server and TypeScript.
//! Run with --ignored; optionally set SLIM_TEST_TYPESCRIPT_LANGUAGE_SERVER to
//! a server executable or its Node entrypoint. This test never installs a
//! dependency, changes PATH or uses a simulated server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use slim_core::codeintel::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelFileUpdate, CodeIntelOutcome,
    CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery, CodeIntelligence,
    EditVerification,
};
use slim_lsp::discovery::TYPESCRIPT_LANGUAGE_SERVER;
use slim_lsp::{LspCodeIntelligence, LspManagerConfig, ServerOptions};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slim-real-typescript-ação espaço-{}-{nonce}",
            std::process::id()
        ));
        for directory in ["src", "configs", "packages/nested/src"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        let fixture = Self(root);
        fixture.write(
            "package.json",
            r#"{"name":"slim-real-typescript","type":"module"}"#,
        );
        fixture.write(
            "configs/shared.jsonc",
            r#"{
            // An arbitrarily named extends input, including a trailing comma.
            "compilerOptions": {
                "target": "ES2020", "module": "NodeNext", "moduleResolution": "NodeNext",
                "strict": true, "allowJs": true, "checkJs": true,
                "jsx": "preserve", "noEmit": true, "skipLibCheck": true,
            },
        }"#,
        );
        fixture.write(
            "tsconfig.json",
            r#"{
            "extends": "./configs/shared.jsonc", "include": ["src/**/*", "types.d.ts"]
        }"#,
        );
        fixture.write(
            "packages/nested/tsconfig.json",
            r#"{
            "extends": "../../configs/shared.jsonc",
            "compilerOptions": {"strict": false, "noUnusedLocals": true},
            "include": ["src/**/*"]
        }"#,
        );
        fixture.write("types.d.ts", "type GlobalCount = number;\r\ndeclare namespace JSX { interface IntrinsicElements { span: { children?: string } } }\r\n");
        fixture.write(
            "src/library.ts",
            "export function realColdSymbol(value: number): number { return value + 1; }\r\n",
        );
        fixture.write("src/main.ts", "import { realColdSymbol } from './library.js';\r\nexport const broken: number = 'wrong';\r\nexport const answer = realColdSymbol(41);\r\nexport const unicode = '😀'; realColdSymbol(1);\r\nexport const declared: GlobalCount = 3;\r\n");
        fixture.write(
            "src/broken.js",
            "/** @type {number} */\r\nexport const jsBroken = 'wrong';\r\n",
        );
        fixture.write(
            "src/implicit.ts",
            "export function inferred(value) { return value; }\r\n",
        );
        fixture.write("src/post-edit.ts", "export const edited: number = 1;\r\n");
        fixture.write(
            "src/consumer.ts",
            "import { addedValue } from './added.js';\r\nexport const result = addedValue;\r\n",
        );
        fixture.write(
            "src/component.tsx",
            "export const view = <span>hello</span>;\r\n",
        );
        fixture.write(
            "src/component.jsx",
            "export const jsxView = <span>hello</span>;\r\n",
        );
        fixture.write("src/module.mts", "export const esmValue = 7;\r\n");
        fixture.write("src/module.cts", "export const cjsValue = 9;\r\n");
        fixture.write("src/module.mjs", "export const mjsValue = 5;\r\n");
        fixture.write("src/module.cjs", "exports.commonValue = 8;\r\n");
        fixture.write(
            "packages/nested/src/nested.ts",
            "const nestedUnused = 1;\r\nexport function nestedIdentity(value) { return value; }\r\n",
        );
        fixture
    }

    fn write(&self, relative: &str, text: &str) {
        std::fs::write(self.0.join(relative), text).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn position(root: &Path, file: &str, line: u32, symbol: &str) -> CodeIntelPositionQuery {
    let text = std::fs::read_to_string(root.join(file)).unwrap();
    let physical = text.lines().nth((line - 1) as usize).unwrap();
    let prefix = &physical[..physical.find(symbol).unwrap()];
    CodeIntelPositionQuery {
        workspace: root.into(),
        path: root.join(file),
        line,
        column: prefix.chars().count() as u32 + 1,
        symbol: Some(symbol.into()),
        max_results: 100,
        ..Default::default()
    }
}

fn ready(outcome: &CodeIntelOutcome) {
    assert_eq!(
        outcome.meta.server, TYPESCRIPT_LANGUAGE_SERVER,
        "{outcome:?}"
    );
    assert_eq!(
        outcome.meta.state,
        CodeIntelServerState::Ready,
        "{outcome:?}"
    );
    assert!(!outcome.meta.stale, "{outcome:?}");
    assert!(outcome.payload.get("error").is_none(), "{outcome:?}");
}

fn file_is(value: &serde_json::Value, expected: &str) -> bool {
    value
        .as_str()
        .is_some_and(|file| Path::new(file) == Path::new(expected))
}

async fn diagnostics(
    manager: &LspCodeIntelligence,
    root: &Path,
    file: &str,
    expected_code: Option<&str>,
) -> CodeIntelOutcome {
    let mut last = None;
    let mut matching_reads = 0;
    // TLS queues diagnostics for 300-800 ms and debounces publication for
    // another 50 ms. Repeat bounded semantic reads, allowing its syntax and
    // semantic passes to settle; the product's 400-ms wait remains unchanged.
    for _ in 0..16 {
        let outcome = manager
            .diagnostics(&CodeIntelDiagnosticsQuery {
                workspace: root.into(),
                path: Some(root.join(file)),
                max_results: 100,
                ..Default::default()
            })
            .await;
        assert_eq!(
            outcome.meta.server, TYPESCRIPT_LANGUAGE_SERVER,
            "{outcome:?}"
        );
        assert_eq!(
            outcome.meta.state,
            CodeIntelServerState::Ready,
            "{outcome:?}"
        );
        assert!(outcome.payload.get("error").is_none(), "{outcome:?}");
        assert_eq!(outcome.payload["storage_truncated"], false, "{outcome:?}");
        let publication = &outcome.payload["files"][0];
        assert!(file_is(&publication["file"], file), "{outcome:?}");
        let expected = expected_code.map_or_else(
            || outcome.payload["total"] == 0,
            |code| has_error(&outcome, code),
        );
        if publication["received"] == true
            && publication["stale"] == false
            && !outcome.meta.stale
            && expected
        {
            matching_reads += 1;
            // A first empty syntax pass may precede semantic diagnostics.
            // Require the expectation to survive another bounded read.
            if matching_reads < 2 {
                last = Some(outcome);
                continue;
            }
            ready(&outcome);
            let document_version = publication["document_version"].as_i64().unwrap();
            assert_eq!(
                outcome.meta.document_version,
                Some(document_version),
                "{outcome:?}"
            );
            if publication["diagnostic_version"].is_null() {
                // TLS's actual push payload is {uri, diagnostics}, without a
                // version. Receiving it cannot certify a document version.
                assert_eq!(
                    outcome.meta.completeness,
                    CodeIntelCompleteness::Unknown,
                    "versionless diagnostics must not certify completeness: {outcome:?}"
                );
            } else {
                assert_eq!(
                    publication["diagnostic_version"].as_i64(),
                    Some(document_version),
                    "{outcome:?}"
                );
            }
            return outcome;
        }
        matching_reads = 0;
        last = Some(outcome);
    }
    panic!("expected diagnostics for {file} did not settle within the bounded reads; expected code={expected_code:?}, last={last:?}");
}

fn has_error(outcome: &CodeIntelOutcome, code: &str) -> bool {
    outcome.payload["files"][0]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["severity"] == "error" && row["code"] == code)
}

fn empty_publication(outcome: &CodeIntelOutcome) {
    assert_eq!(
        outcome.payload["total"], 0,
        "expected a received empty publication, not an assertion of certification: {outcome:?}"
    );
    assert_eq!(
        outcome.payload["files"][0]["diagnostics"],
        serde_json::json!([]),
        "{outcome:?}"
    );
}

async fn update_and_check_post_edit(
    manager: &LspCodeIntelligence,
    root: &Path,
    file: &str,
    text: &str,
    versioned_baseline: bool,
) -> EditVerification {
    let path = root.join(file);
    std::fs::write(&path, text).unwrap();
    manager
        .notify_file_updated(
            root,
            &path,
            CodeIntelFileUpdate {
                text: text.into(),
                patch: None,
            },
        )
        .await;
    let report = manager
        .diagnostics_after_edits(root, &[path], Duration::from_millis(1_500), None)
        .await
        .expect("warm post-edit report");
    assert_eq!(report.server, TYPESCRIPT_LANGUAGE_SERVER, "{report:?}");
    assert_eq!(report.files.len(), 1, "{report:?}");
    let result = &report.files[0];
    assert_eq!(Path::new(&result.path), Path::new(file), "{report:?}");
    assert_eq!(
        result.server.as_deref(),
        Some(TYPESCRIPT_LANGUAGE_SERVER),
        "{report:?}"
    );
    if versioned_baseline {
        assert_eq!(
            result.verification,
            EditVerification::Verified,
            "{report:?}"
        );
    } else {
        assert_eq!(result.verification, EditVerification::Unverified,
            "TLS publishes no document version, so neither exact validation nor a certified baseline may be inferred: {report:?}");
        assert!(
            result.errors.is_empty(),
            "an unverified report must not claim introduced errors: {report:?}"
        );
    }
    result.verification
}

async fn exercise(manager: Arc<LspCodeIntelligence>, root: PathBuf) {
    let cold_start = Instant::now();
    let cold_symbols = manager
        .symbols(&CodeIntelSymbolQuery {
            workspace: root.clone(),
            query: Some("realColdSymbol".into()),
            max_results: 100,
            ..Default::default()
        })
        .await;
    let cold_ms = cold_start.elapsed().as_millis();
    ready(&cold_symbols);
    assert_eq!(
        cold_symbols.payload["kind"], "workspace",
        "{cold_symbols:?}"
    );
    assert!(cold_symbols.payload["symbols"].as_array().unwrap().iter().any(|symbol| {
        symbol["name"] == "realColdSymbol" && file_is(&symbol["file"], "src/library.ts")
    }), "workspace/symbol must work as the first semantic query, before any explicit document open: {cold_symbols:?}");

    let call = position(&root, "src/main.ts", 3, "realColdSymbol");
    let definition = manager.definition(&call).await;
    ready(&definition);
    assert_eq!(definition.payload["found"], true, "{definition:?}");
    assert!(
        file_is(&definition.payload["file"], "src/library.ts"),
        "{definition:?}"
    );
    assert_eq!(definition.payload["line"], 1, "{definition:?}");
    assert_eq!(definition.payload["column"], 17, "{definition:?}");
    let warm_start = Instant::now();
    let hover = manager.hover(&call).await;
    let warm_ms = warm_start.elapsed().as_millis();
    ready(&hover);
    assert_eq!(hover.payload["found"], true, "{hover:?}");
    let text = hover.payload["text"].as_str().unwrap();
    assert!(
        text.contains("realColdSymbol") && text.contains("number"),
        "{hover:?}"
    );

    let unicode = position(&root, "src/main.ts", 4, "realColdSymbol");
    let unicode_definition = manager.definition(&unicode).await;
    ready(&unicode_definition);
    assert!(
        file_is(&unicode_definition.payload["file"], "src/library.ts"),
        "{unicode_definition:?}"
    );
    let references = manager.references(&call).await;
    ready(&references);
    assert!(references.payload["files"].as_array().unwrap().iter().any(|file| {
        file_is(&file["file"], "src/main.ts") && file["results"].as_array().unwrap().iter().any(|row| {
            row["line"] == 4 && row["column"] == unicode.column
        })
    }), "references must retain the Unicode scalar column after an emoji in CRLF source: {references:?}");

    let declared = manager
        .definition(&position(&root, "src/main.ts", 5, "GlobalCount"))
        .await;
    ready(&declared);
    assert!(
        file_is(&declared.payload["file"], "types.d.ts"),
        "{declared:?}"
    );
    for (file, symbol) in [
        ("src/module.mts", "esmValue"),
        ("src/module.cts", "cjsValue"),
        ("src/module.mjs", "mjsValue"),
        ("src/module.cjs", "commonValue"),
    ] {
        let hover = manager.hover(&position(&root, file, 1, symbol)).await;
        ready(&hover);
        assert_eq!(hover.payload["found"], true, "{file}: {hover:?}");
        assert!(
            hover.payload["text"].as_str().unwrap().contains(symbol),
            "{file}: {hover:?}"
        );
    }
    for (file, symbol) in [
        ("src/component.tsx", "view"),
        ("src/component.jsx", "jsxView"),
    ] {
        let symbols = manager
            .symbols(&CodeIntelSymbolQuery {
                workspace: root.clone(),
                path: Some(root.join(file)),
                max_results: 100,
                ..Default::default()
            })
            .await;
        ready(&symbols);
        assert!(
            symbols.payload["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["name"] == symbol),
            "{file}: {symbols:?}"
        );
    }

    let broken = diagnostics(&manager, &root, "src/main.ts", Some("2322")).await;
    assert!(
        has_error(&broken, "2322"),
        "TypeScript must report the real string-to-number error: {broken:?}"
    );
    let js = diagnostics(&manager, &root, "src/broken.js", Some("2322")).await;
    assert!(
        has_error(&js, "2322"),
        "checkJs must report the JSDoc type error: {js:?}"
    );
    // Only the nested tsconfig turns an unused local into an error (an
    // inferred project reports it as a hint), and its strict=false must
    // override the shared config's implicit-any error.
    let nested = diagnostics(
        &manager,
        &root,
        "packages/nested/src/nested.ts",
        Some("6133"),
    )
    .await;
    assert!(
        has_error(&nested, "6133"),
        "the nested tsconfig must own its file: {nested:?}"
    );
    assert!(
        !has_error(&nested, "7006"),
        "nested strict=false must override the shared config: {nested:?}"
    );

    let text = std::fs::read_to_string(root.join("src/main.ts")).unwrap();
    std::fs::write(
        root.join("src/main.ts"),
        text.replace("= 'wrong';", "= 42;"),
    )
    .unwrap();
    let corrected = diagnostics(&manager, &root, "src/main.ts", None).await;
    empty_publication(&corrected);
    assert!(
        corrected.meta.document_version > broken.meta.document_version,
        "external edits must advance the mirrored version: before={broken:?}, after={corrected:?}"
    );

    let baseline = diagnostics(&manager, &root, "src/post-edit.ts", None).await;
    empty_publication(&baseline);
    let introduced_verification = update_and_check_post_edit(
        &manager,
        &root,
        "src/post-edit.ts",
        "export const edited: number = 'wrong';\r\n",
        baseline.payload["files"][0]["diagnostic_version"]
            .as_i64()
            .is_some(),
    )
    .await;
    let introduced = diagnostics(&manager, &root, "src/post-edit.ts", Some("2322")).await;
    assert!(
        has_error(&introduced, "2322"),
        "the unverified post-edit outcome does not mean no type errors: {introduced:?}"
    );
    let corrected_verification = update_and_check_post_edit(
        &manager,
        &root,
        "src/post-edit.ts",
        "export const edited: number = 2;\r\n",
        introduced.payload["files"][0]["diagnostic_version"]
            .as_i64()
            .is_some(),
    )
    .await;
    empty_publication(&diagnostics(&manager, &root, "src/post-edit.ts", None).await);
    let versionless_diagnostics = baseline.payload["files"][0]["diagnostic_version"].is_null()
        && introduced.payload["files"][0]["diagnostic_version"].is_null();
    let post_edit_unverified = introduced_verification == EditVerification::Unverified
        && corrected_verification == EditVerification::Unverified;

    let missing = diagnostics(&manager, &root, "src/consumer.ts", Some("2307")).await;
    assert!(
        has_error(&missing, "2307"),
        "initially missing module: {missing:?}"
    );
    std::fs::write(
        root.join("src/added.ts"),
        "export const addedValue = 42;\r\n",
    )
    .unwrap();
    let created = manager
        .definition(&position(&root, "src/consumer.ts", 2, "addedValue"))
        .await;
    ready(&created);
    assert!(
        file_is(&created.payload["file"], "src/added.ts"),
        "module creation must refresh resolution: {created:?}"
    );
    empty_publication(&diagnostics(&manager, &root, "src/consumer.ts", None).await);
    std::fs::remove_file(root.join("src/added.ts")).unwrap();
    let removed = diagnostics(&manager, &root, "src/consumer.ts", Some("2307")).await;
    assert!(
        has_error(&removed, "2307"),
        "module removal must restore the unresolved-import diagnostic: {removed:?}"
    );

    let implicit = diagnostics(&manager, &root, "src/implicit.ts", Some("7006")).await;
    assert!(
        has_error(&implicit, "7006"),
        "inherited strict config must be active: {implicit:?}"
    );
    let shared = root.join("configs/shared.jsonc");
    let text = std::fs::read_to_string(&shared).unwrap();
    std::fs::write(
        &shared,
        text.replace(
            "\"strict\": true,",
            "\"strict\": true, \"noImplicitAny\": false,",
        ),
    )
    .unwrap();
    empty_publication(&diagnostics(&manager, &root, "src/implicit.ts", None).await);
    println!("real_typescript cold_workspace_symbol_ms={cold_ms} warm_hover_ms={warm_ms} extensions=8 unicode_crlf=true external_refresh=true jsonc_extends_refresh=true versionless_diagnostics={versionless_diagnostics} post_edit_unverified={post_edit_unverified}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires installed typescript-language-server and compatible TypeScript; never installs dependencies"]
async fn real_typescript_native_navigation_diagnostics_and_refresh() {
    let fixture = Fixture::new();
    let mut config = LspManagerConfig {
        idle_shutdown: None,
        request_timeout: Duration::from_secs(20),
        ..Default::default()
    };
    if let Some(path) = std::env::var_os("SLIM_TEST_TYPESCRIPT_LANGUAGE_SERVER") {
        config.servers.insert(
            TYPESCRIPT_LANGUAGE_SERVER.into(),
            ServerOptions {
                path: Some(path.into()),
                ..Default::default()
            },
        );
    }
    let manager = LspCodeIntelligence::from_config(config);
    assert!(manager.supports_workspace(&fixture.0));
    let pool = manager.pool().clone();
    assert_eq!(pool.running_servers().await, 0);
    let mut run = tokio::spawn(exercise(manager, fixture.0.clone()));
    let result = tokio::time::timeout(Duration::from_secs(120), &mut run).await;
    if result.is_err() {
        run.abort();
        let _ = run.await;
    }
    pool.close_all().await;
    assert_eq!(pool.running_servers().await, 0);
    result
        .expect("real TypeScript gate exceeded its global deadline")
        .expect("real TypeScript gate failed");
}
