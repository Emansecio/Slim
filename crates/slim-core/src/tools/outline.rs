//! Zero-dependency structural outline scanner for source, config, and Markdown files.
//!
//! Used by `read(outline: true)`, `read(symbols: [...])`, `@file::symbol` mentions,
//! and the dynamic `RepoMap` builder.

use std::fmt::Write as _;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutlineSymbol {
    pub kind: &'static str,
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutlineLang {
    Rust,
    TsJs,
    Python,
    Go,
    CFamily,
    JavaSwift,
    Markdown,
    Toml,
}

type Lang = OutlineLang;

impl OutlineLang {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => Some(Self::TsJs),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(Self::CFamily),
            "java" | "swift" | "kt" | "kts" => Some(Self::JavaSwift),
            "md" | "mdx" | "markdown" => Some(Self::Markdown),
            "toml" => Some(Self::Toml),
            _ => None,
        }
    }
}

/// Extracts top-level and member declarations with 1-based inclusive line spans.
pub fn extract_outline(path: &Path, content: &str) -> Vec<OutlineSymbol> {
    let Some(lang) = OutlineLang::from_path(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    extract_outline_lines(lang, &lines)
}

pub(crate) fn extract_outline_lines(lang: OutlineLang, lines: &[&str]) -> Vec<OutlineSymbol> {
    match lang {
        OutlineLang::Python => extract_python(lines),
        OutlineLang::Markdown => extract_markdown(lines),
        OutlineLang::Toml => extract_toml(lines),
        OutlineLang::Rust
        | OutlineLang::TsJs
        | OutlineLang::Go
        | OutlineLang::CFamily
        | OutlineLang::JavaSwift => extract_braced(lines, lang),
    }
}

/// Renders one compact line per symbol (`L10-L48 fn foo(...)`) plus line total.
pub fn format_outline(symbols: &[OutlineSymbol], total_lines: usize) -> String {
    if symbols.is_empty() {
        return format!("[outline: 0 symbols found across {total_lines} lines]");
    }
    let mut out = format!("[outline: {} symbols across {total_lines} lines]\n", symbols.len());
    for sym in symbols {
        let _ = writeln!(
            out,
            "L{}-L{} {} {}",
            sym.start_line, sym.end_line, sym.kind, sym.signature
        );
    }
    out.trim_end_matches('\n').to_owned()
}

/// Resolves requested symbol names to sorted, merged 1-based line ranges.
/// Prefers exact name matches; falls back to case-insensitive suffix/substring matches.
pub fn find_symbol_ranges(symbols: &[OutlineSymbol], names: &[String]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for raw_name in names {
        let query = raw_name.trim();
        if query.is_empty() {
            continue;
        }
        let mut matched: Vec<(usize, usize)> = symbols
            .iter()
            .filter(|sym| sym.name == query)
            .map(|sym| (sym.start_line, sym.end_line))
            .collect();
        if matched.is_empty() {
            let lower = query.to_ascii_lowercase();
            matched = symbols
                .iter()
                .filter(|sym| {
                    let sym_lower = sym.name.to_ascii_lowercase();
                    sym_lower == lower
                        || sym_lower.ends_with(&format!("::{lower}"))
                        || sym_lower.ends_with(&format!(".{lower}"))
                })
                .map(|sym| (sym.start_line, sym.end_line))
                .collect();
        }
        if matched.is_empty() {
            let lower = query.to_ascii_lowercase();
            matched = symbols
                .iter()
                .filter(|sym| sym.name.to_ascii_lowercase().contains(&lower))
                .map(|sym| (sym.start_line, sym.end_line))
                .collect();
        }
        ranges.extend(matched);
    }
    merge_line_ranges(ranges)
}

/// Sorts and merges overlapping or adjacent 1-based inclusive line ranges.
pub fn merge_line_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.retain(|(start, end)| *start >= 1 && *end >= *start);
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut merged = Vec::with_capacity(ranges.len());
    let mut cur = ranges[0];
    for (start, end) in ranges.into_iter().skip(1) {
        if start <= cur.1.saturating_add(1) {
            cur.1 = cur.1.max(end);
        } else {
            merged.push(cur);
            cur = (start, end);
        }
    }
    merged.push(cur);
    merged
}

fn compact_signature(line: &str) -> String {
    let trimmed = line
        .trim()
        .trim_end_matches('{')
        .trim_end_matches(':')
        .trim();
    let mut out = String::with_capacity(trimmed.len().min(120));
    let mut prev_ws = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
        if out.len() >= 118 {
            out.push_str("...");
            break;
        }
    }
    out
}

fn leading_ident(s: &str) -> Option<String> {
    let ident: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect();
    (!ident.is_empty()).then_some(ident)
}

fn extract_python(lines: &[&str]) -> Vec<OutlineSymbol> {
    struct Entry {
        kind: &'static str,
        name: String,
        start_line: usize,
        indent: usize,
        signature: String,
    }
    let mut raw = Vec::<Entry>::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let (kind, rest) = if let Some(r) = trimmed.strip_prefix("async def ") {
            ("fn", r)
        } else if let Some(r) = trimmed.strip_prefix("def ") {
            ("fn", r)
        } else if let Some(r) = trimmed.strip_prefix("class ") {
            ("class", r)
        } else {
            continue;
        };
        if let Some(name) = leading_ident(rest) {
            raw.push(Entry {
                kind,
                name,
                start_line: idx + 1,
                indent,
                signature: compact_signature(trimmed),
            });
        }
    }
    let total = lines.len();
    raw.iter()
        .enumerate()
        .map(|(i, entry)| {
            let mut end_line = total;
            for line_idx in entry.start_line..total {
                let candidate = lines[line_idx];
                let trimmed = candidate.trim_start();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let indent = candidate.len() - trimmed.len();
                if indent <= entry.indent && !trimmed.starts_with('@') {
                    end_line = line_idx;
                    break;
                }
            }
            if let Some(next) = raw.get(i + 1) {
                if next.indent <= entry.indent {
                    end_line = end_line.min(next.start_line.saturating_sub(1));
                }
            }
            while end_line > entry.start_line {
                let tail = lines[end_line - 1].trim();
                if tail.is_empty() || tail.starts_with('#') {
                    end_line -= 1;
                } else {
                    break;
                }
            }
            OutlineSymbol {
                kind: entry.kind,
                name: entry.name.clone(),
                start_line: entry.start_line,
                end_line: end_line.max(entry.start_line),
                signature: entry.signature.clone(),
            }
        })
        .collect()
}

fn extract_markdown(lines: &[&str]) -> Vec<OutlineSymbol> {
    struct Heading {
        level: usize,
        name: String,
        start_line: usize,
        signature: String,
    }
    let mut headings = Vec::new();
    let mut in_fence = false;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&level) {
            let title = trimmed[level..].trim();
            if !title.is_empty() {
                headings.push(Heading {
                    level,
                    name: title.to_owned(),
                    start_line: idx + 1,
                    signature: compact_signature(trimmed),
                });
            }
        }
    }
    let total = lines.len();
    headings
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let end_line = headings[i + 1..]
                .iter()
                .find(|next| next.level <= h.level)
                .map_or(total, |next| next.start_line.saturating_sub(1));
            OutlineSymbol {
                kind: "section",
                name: h.name.clone(),
                start_line: h.start_line,
                end_line: end_line.max(h.start_line),
                signature: h.signature.clone(),
            }
        })
        .collect()
}

fn extract_toml(lines: &[&str]) -> Vec<OutlineSymbol> {
    let mut headers = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed.trim_matches(|c| c == '[' || c == ']').trim();
            if !name.is_empty() {
                headers.push((name.to_owned(), idx + 1, trimmed.to_owned()));
            }
        }
    }
    let total = lines.len();
    headers
        .iter()
        .enumerate()
        .map(|(i, (name, start, sig))| {
            let end = headers
                .get(i + 1)
                .map_or(total, |next| next.1.saturating_sub(1));
            OutlineSymbol {
                kind: "table",
                name: name.clone(),
                start_line: *start,
                end_line: end.max(*start),
                signature: sig.clone(),
            }
        })
        .collect()
}

#[derive(Default)]
struct BraceScanState {
    in_block_comment: bool,
    raw_string_hashes: Option<usize>,
}

fn rust_raw_string_start(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    // Matches r#"..."#, r"...", br#"..."#, br"..." when not glued to a preceding identifier.
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        return None;
    }
    let after_prefix = if bytes[i] == b'r' {
        i + 1
    } else if bytes[i] == b'b' && bytes.get(i + 1) == Some(&b'r') {
        i + 2
    } else {
        return None;
    };
    let mut hashes = 0usize;
    while bytes.get(after_prefix + hashes) == Some(&b'#') {
        hashes += 1;
    }
    if bytes.get(after_prefix + hashes) == Some(&b'"') {
        Some((after_prefix + hashes + 1, hashes))
    } else {
        None
    }
}

fn skip_rust_char_or_lifetime(line: &str, i: usize) -> usize {
    let bytes = line.as_bytes();
    let Some(&next) = bytes.get(i + 1) else {
        return i + 1;
    };
    if next == b'\\' {
        // Escaped char literal like `'\''`, `'\n'`, `'\x7f'`, `'\u{1f600}'`.
        let mut j = i + 3;
        while j < bytes.len() && j <= i + 14 {
            if bytes[j] == b'\'' {
                return j + 1;
            }
            j += 1;
        }
        return i + 1;
    }
    if let Some(ch) = line[i + 1..].chars().next() {
        let after_ch = i + 1 + ch.len_utf8();
        if bytes.get(after_ch) == Some(&b'\'') {
            // Normal single-character literal `'x'` -> advance past closing quote.
            return after_ch + 1;
        }
    }
    // Lifetime token such as `'a`, `'static`, `'_` -> do not treat as string quote.
    i + 1
}

fn count_brace_delta(
    line: &str,
    lang: OutlineLang,
    state: &mut BraceScanState,
) -> (bool, bool, isize) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut saw_open = false;
    let mut saw_close = false;
    let mut delta = 0isize;
    let mut string_quote: Option<u8> = None;

    while i < bytes.len() {
        if state.in_block_comment {
            if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                state.in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(hashes) = state.raw_string_hashes {
            if bytes[i] == b'"' {
                let end = i + 1 + hashes;
                if end <= bytes.len() && bytes[i + 1..end].iter().all(|&b| b == b'#') {
                    state.raw_string_hashes = None;
                    i = end;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        if let Some(q) = string_quote {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == q {
                string_quote = None;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            state.in_block_comment = true;
            i += 2;
            continue;
        }
        if lang == OutlineLang::Rust {
            if let Some((next_idx, hashes)) = rust_raw_string_start(bytes, i) {
                state.raw_string_hashes = Some(hashes);
                i = next_idx;
                continue;
            }
            if bytes[i] == b'\'' {
                i = skip_rust_char_or_lifetime(line, i);
                continue;
            }
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' || bytes[i] == b'`' {
            string_quote = Some(bytes[i]);
            i += 1;
            continue;
        }
        match bytes[i] {
            b'{' => {
                saw_open = true;
                delta += 1;
            }
            b'}' => {
                saw_close = true;
                delta -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    (saw_open, saw_close, delta)
}

fn parse_braced_decl(trimmed: &str, lang: Lang, brace_depth: isize) -> Option<(&'static str, String)> {
    match lang {
        Lang::Rust => parse_rust_decl(trimmed),
        Lang::TsJs => parse_tsjs_decl(trimmed, brace_depth),
        Lang::Go => parse_go_decl(trimmed),
        Lang::CFamily => parse_c_decl(trimmed, brace_depth),
        Lang::JavaSwift => parse_javaswift_decl(trimmed),
        _ => None,
    }
}

fn strip_rust_prefixes(mut s: &str) -> &str {
    loop {
        let prev = s;
        s = s.trim_start();
        for kw in ["pub(crate)", "pub(super)", "pub(in ", "pub"] {
            if let Some(rest) = s.strip_prefix(kw) {
                if kw == "pub(in " {
                    if let Some((_, after)) = rest.split_once(')') {
                        s = after.trim_start();
                    }
                } else if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                    s = rest.trim_start();
                }
            }
        }
        for qual in ["default", "unsafe", "async", "const", "extern \"C\"", "extern"] {
            if let Some(rest) = s.strip_prefix(qual) {
                if rest.starts_with(char::is_whitespace) {
                    s = rest.trim_start();
                }
            }
        }
        if s == prev {
            break;
        }
    }
    s
}

fn parse_rust_decl(trimmed: &str) -> Option<(&'static str, String)> {
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('#') {
        return None;
    }
    let s = strip_rust_prefixes(trimmed);
    let (kind, rest) = if let Some(r) = s.strip_prefix("fn ") {
        ("fn", r)
    } else if let Some(r) = s.strip_prefix("struct ") {
        ("struct", r)
    } else if let Some(r) = s.strip_prefix("enum ") {
        ("enum", r)
    } else if let Some(r) = s.strip_prefix("trait ") {
        ("trait", r)
    } else if let Some(r) = s.strip_prefix("mod ") {
        ("mod", r)
    } else if let Some(r) = s.strip_prefix("type ") {
        ("type", r)
    } else if let Some(r) = s.strip_prefix("macro_rules! ") {
        ("macro", r)
    } else if let Some(r) = s.strip_prefix("impl") {
        if r.starts_with(char::is_whitespace) || r.starts_with('<') {
            let target = r
                .trim_end_matches('{')
                .split(" where ")
                .next()
                .unwrap_or(r)
                .trim();
            return (!target.is_empty()).then(|| ("impl", target.to_owned()));
        }
        return None;
    } else {
        return None;
    };
    leading_ident(rest.trim_start()).map(|name| (kind, name))
}

fn parse_tsjs_decl(trimmed: &str, brace_depth: isize) -> Option<(&'static str, String)> {
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return None;
    }
    let mut s = trimmed;
    for kw in ["export default ", "export ", "declare ", "abstract ", "async "] {
        if let Some(r) = s.strip_prefix(kw) {
            s = r.trim_start();
        }
    }
    if let Some(r) = s.strip_prefix("function* ").or_else(|| s.strip_prefix("function ")) {
        return leading_ident(r.trim_start()).map(|n| ("fn", n));
    }
    if let Some(r) = s.strip_prefix("class ") {
        return leading_ident(r.trim_start()).map(|n| ("class", n));
    }
    if let Some(r) = s.strip_prefix("interface ") {
        return leading_ident(r.trim_start()).map(|n| ("interface", n));
    }
    if let Some(r) = s.strip_prefix("type ") {
        return leading_ident(r.trim_start()).map(|n| ("type", n));
    }
    if let Some(r) = s.strip_prefix("enum ") {
        return leading_ident(r.trim_start()).map(|n| ("enum", n));
    }
    if let Some(r) = s.strip_prefix("const ").or_else(|| s.strip_prefix("let ")) {
        if (r.contains("=>") || r.contains("= function")) && (brace_depth == 0 || trimmed.starts_with("export")) {
            return leading_ident(r.trim_start()).map(|n| ("fn", n));
        }
    }
    // Class methods at depth 1 (inside `class ... {`)
    if brace_depth == 1 && !s.starts_with("if") && !s.starts_with("for") && !s.starts_with("while") && !s.starts_with("switch") && !s.starts_with("return") {
        let mut m = s;
        loop {
            let prev = m;
            for kw in [
                "public ",
                "private ",
                "protected ",
                "readonly ",
                "static ",
                "override ",
                "abstract ",
                "async ",
            ] {
                if let Some(rest) = m.strip_prefix(kw) {
                    m = rest.trim_start();
                }
            }
            if m == prev {
                break;
            }
        }
        if let Some(name) = leading_ident(m) {
            let after = m[name.len()..].trim_start();
            if after.starts_with('(') && s.contains('{') {
                return Some(("method", name));
            }
        }
    }
    None
}

fn parse_go_decl(trimmed: &str) -> Option<(&'static str, String)> {
    if trimmed.starts_with("//") || trimmed.starts_with("/*") {
        return None;
    }
    if let Some(r) = trimmed.strip_prefix("func ") {
        let r = r.trim_start();
        let after_recv = if r.starts_with('(') {
            r.split_once(')')?.1.trim_start()
        } else {
            r
        };
        return leading_ident(after_recv).map(|n| ("fn", n));
    }
    if let Some(r) = trimmed.strip_prefix("type ") {
        let name = leading_ident(r.trim_start())?;
        let rest = r.trim_start()[name.len()..].trim_start();
        let kind = if rest.starts_with("struct") {
            "struct"
        } else if rest.starts_with("interface") {
            "interface"
        } else {
            "type"
        };
        return Some((kind, name));
    }
    None
}

fn parse_c_decl(trimmed: &str, brace_depth: isize) -> Option<(&'static str, String)> {
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('#') {
        return None;
    }
    for (kw, kind) in [
        ("namespace ", "namespace"),
        ("class ", "class"),
        ("struct ", "struct"),
        ("enum ", "enum"),
    ] {
        if let Some(r) = trimmed.strip_prefix(kw) {
            if let Some(name) = leading_ident(r.trim_start()) {
                return Some((kind, name));
            }
        }
    }
    if brace_depth <= 1
        && trimmed.contains('(')
        && !trimmed.ends_with(';')
        && !trimmed.starts_with("if")
        && !trimmed.starts_with("for")
        && !trimmed.starts_with("while")
        && !trimmed.starts_with("switch")
    {
        if let Some((before_paren, _)) = trimmed.split_once('(') {
            let candidate = before_paren
                .split_whitespace()
                .last()
                .map(|tok| tok.trim_start_matches(['*', '&']));
            if let Some(name) = candidate.and_then(|t| {
                let tail = t.rsplit("::").next().unwrap_or(t);
                leading_ident(tail)
            }) {
                return Some(("fn", name));
            }
        }
    }
    None
}

fn parse_javaswift_decl(trimmed: &str) -> Option<(&'static str, String)> {
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('@') {
        return None;
    }
    let mut s = trimmed;
    loop {
        let prev = s;
        for kw in [
            "public ",
            "private ",
            "protected ",
            "internal ",
            "fileprivate ",
            "open ",
            "final ",
            "static ",
            "abstract ",
            "override ",
            "suspend ",
        ] {
            if let Some(r) = s.strip_prefix(kw) {
                s = r.trim_start();
            }
        }
        if s == prev {
            break;
        }
    }
    for (kw, kind) in [
        ("class ", "class"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("interface ", "interface"),
        ("protocol ", "protocol"),
        ("extension ", "extension"),
        ("func ", "fn"),
        ("fun ", "fn"),
    ] {
        if let Some(r) = s.strip_prefix(kw) {
            return leading_ident(r.trim_start()).map(|n| (kind, n));
        }
    }
    None
}

fn extract_braced(lines: &[&str], lang: OutlineLang) -> Vec<OutlineSymbol> {
    struct Pending {
        kind: &'static str,
        name: String,
        start_line: usize,
        signature: String,
        target_depth: isize,
        saw_open_brace: bool,
    }
    let mut depth: isize = 0;
    let mut scan_state = BraceScanState::default();
    let mut stack: Vec<Pending> = Vec::new();
    let mut finished: Vec<OutlineSymbol> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        let depth_before = depth;
        let skip_decl = scan_state.in_block_comment || scan_state.raw_string_hashes.is_some();
        let (saw_open, _saw_close, delta) = count_brace_delta(line, lang, &mut scan_state);

        if !skip_decl && !trimmed.is_empty() {
            if let Some((kind, name)) = parse_braced_decl(trimmed, lang, depth_before) {
                stack.push(Pending {
                    kind,
                    name,
                    start_line: line_no,
                    signature: compact_signature(trimmed),
                    target_depth: depth_before,
                    saw_open_brace: saw_open,
                });
            }
        }

        depth += delta;

        // If a pending declaration has its `{` on a subsequent line (multi-line signature),
        // track once it enters its body or abort if `;` terminates a prototype before `{`.
        for item in &mut stack {
            if !item.saw_open_brace {
                if saw_open {
                    item.saw_open_brace = true;
                } else if trimmed.ends_with(';') && !trimmed.contains('{') && item.start_line == line_no {
                    item.saw_open_brace = false;
                }
            }
        }

        // Remove single-line forward declarations/signatures ending in `;` with no body.
        stack.retain(|item| {
            !(item.start_line == line_no && !item.saw_open_brace && trimmed.ends_with(';'))
        });

        while let Some(top) = stack.last() {
            if top.saw_open_brace && depth <= top.target_depth {
                let done = stack.pop().unwrap();
                finished.push(OutlineSymbol {
                    kind: done.kind,
                    name: done.name,
                    start_line: done.start_line,
                    end_line: line_no,
                    signature: done.signature,
                });
            } else {
                break;
            }
        }
    }

    let last_line = lines.len();
    for rem in stack {
        finished.push(OutlineSymbol {
            kind: rem.kind,
            name: rem.name,
            start_line: rem.start_line,
            end_line: last_line.max(rem.start_line),
            signature: rem.signature,
        });
    }

    finished.sort_by(|a, b| a.start_line.cmp(&b.start_line).then_with(|| a.end_line.cmp(&b.end_line)));
    finished
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols_with_brace_spans() {
        let src = r#"
pub struct Config {
    pub limit: usize,
}

impl Config {
    pub async fn run(&self, name: &str) -> bool {
        let s = "{ not a brace }";
        if !s.is_empty() {
            return name == "ok";
        }
        false
    }
}
"#;
        let outline = extract_outline(Path::new("src/lib.rs"), src);
        let names: Vec<(&str, &str, usize, usize)> = outline
            .iter()
            .map(|s| (s.kind, s.name.as_str(), s.start_line, s.end_line))
            .collect();
        assert_eq!(
            names,
            vec![
                ("struct", "Config", 2, 4),
                ("impl", "Config", 6, 14),
                ("fn", "run", 7, 13),
            ]
        );
        assert_eq!(find_symbol_ranges(&outline, &["run".into()]), vec![(7, 13)]);
    }

    #[test]
    fn extracts_python_indentation_and_markdown_sections() {
        let py = "class Worker:\n    def step(self):\n        return 1\n\ndef top():\n    pass\n";
        let py_out = extract_outline(Path::new("worker.py"), py);
        assert_eq!(py_out.len(), 3);
        assert_eq!((py_out[0].start_line, py_out[0].end_line), (1, 3));
        assert_eq!((py_out[1].start_line, py_out[1].end_line), (2, 3));
        assert_eq!((py_out[2].start_line, py_out[2].end_line), (5, 6));

        let md = "# Intro\ntext\n## Details\nbody\n# Next\nend\n";
        let md_out = extract_outline(Path::new("README.md"), md);
        assert_eq!(md_out.len(), 3);
        assert_eq!((md_out[0].start_line, md_out[0].end_line), (1, 4));
        assert_eq!((md_out[1].start_line, md_out[1].end_line), (3, 4));
        assert_eq!((md_out[2].start_line, md_out[2].end_line), (5, 6));
    }

    #[test]
    fn merges_overlapping_line_ranges() {
        assert_eq!(
            merge_line_ranges(vec![(10, 20), (15, 25), (26, 30), (40, 45)]),
            vec![(10, 30), (40, 45)]
        );
    }

    #[test]
    fn handles_rust_lifetimes_raw_strings_and_ts_class_methods() {
        let rust_src = r##"
pub fn parse_slice<'a>(input: &'a str) -> &'a str {
    let raw = r#"{ not a brace }"#;
    let escaped = '\'';
    let brace_ch = '{';
    if !raw.is_empty() && escaped != brace_ch {
        return input;
    }
    input
}

pub fn after_slice() -> usize {
    42
}
"##;
        let rust_out = extract_outline(Path::new("parser.rs"), rust_src);
        let rust_names: Vec<(&str, &str, usize, usize)> = rust_out
            .iter()
            .map(|s| (s.kind, s.name.as_str(), s.start_line, s.end_line))
            .collect();
        assert_eq!(
            rust_names,
            vec![
                ("fn", "parse_slice", 2, 10),
                ("fn", "after_slice", 12, 14),
            ]
        );

        let ts_src = r#"
export class Service {
    public run(id: string): boolean {
        return id.length > 0;
    }
    private async fetchItem(id: string): Promise<void> {
        await Promise.resolve(id);
    }
}
"#;
        let ts_out = extract_outline(Path::new("service.ts"), ts_src);
        let ts_names: Vec<(&str, &str, usize, usize)> = ts_out
            .iter()
            .map(|s| (s.kind, s.name.as_str(), s.start_line, s.end_line))
            .collect();
        assert_eq!(
            ts_names,
            vec![
                ("class", "Service", 2, 9),
                ("method", "run", 3, 5),
                ("method", "fetchItem", 6, 8),
            ]
        );
    }
}
