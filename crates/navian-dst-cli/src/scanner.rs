//! Determinism-leak scanner.
//!
//! Parses Rust source with [`syn`] and walks the AST looking for calls that
//! introduce non-determinism — wall-clock time, randomness, network I/O, and
//! unstructured concurrency.
//!
//! ## What this is (and is NOT)
//!
//! This is a **name-based heuristic lint**, not a sound analyzer. It flags calls
//! and paths whose *recognizable name or tail segments* match a known
//! determinism source (`SystemTime::now`, `thread_rng`, `OsRng`, `reqwest::*`,
//! `tokio::spawn`, …). It does **no name resolution** — imports, aliases, glob
//! `use`, and lexical scope are not tracked — so it is a starting point for
//! making a crate replayable, not a proof that one is deterministic. Two
//! consequences follow directly from matching by name, and are accepted by
//! design (a replay-safety gate should err toward flagging):
//!
//! * **False positives** on user symbols that merely share a name. Because
//!   matching is on the name / tail segments, `my_time::SystemTime::now()` and
//!   `builder.gen()` are flagged even though they are not the real determinism
//!   sources. (Where a name is distinctive enough, a clearly foreign-qualified
//!   path such as `my_utils::thread_rng()` is left alone.) Flagging is the safe
//!   direction to be wrong in — review and dismiss the finding.
//! * **False negatives** on renamed imports. An `as` alias hides the
//!   recognizable name, so `use std::time::SystemTime as Sys; Sys::now()` and
//!   `use rand::rngs::OsRng as Rng; Rng::default()` are NOT flagged. Closing
//!   this gap would require full name resolution, which this tool deliberately
//!   does not do — prefer the unaliased spelling, and treat a clean scan as a
//!   prompt to review, not a proof.
//!
//! The [`migrate`](crate) codemod is a *separate*, deliberately conservative
//! pass: it rewrites only the leaks it can map without ambiguity and leaves
//! everything else untouched. The scanner flags aggressively; the codemod
//! changes cautiously.
//!
//! ## Classification
//!
//! Every recognized source is a single hard [`Category`] — time / random /
//! network / concurrency — and the `--deny` CI gate fails on any of them. There
//! is no lower-confidence tier: for a replay-safety gate, erring toward flagging
//! is the right default, and a name-based lint cannot honestly claim the
//! precision a "confident vs. maybe" split would imply.
//!
//! ## Location reporting
//!
//! Line/column come from `proc_macro2`'s span locations (the `span-locations`
//! feature is enabled, and we parse from an in-memory `&str`, which is the
//! configuration under which `Span::start()`/`end()` carry real coordinates).
//! `line` is 1-based; `column` from `proc_macro2` is 0-based, so we report
//! `column + 1` to match what editors show. Snippets are sliced directly from
//! the source text over the node's span, so they are the real bytes on disk,
//! not a pretty-printed reconstruction.

use std::path::Path;

use serde::Serialize;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use walkdir::WalkDir;

/// The kind of non-determinism a leak introduces.
///
/// Every variant is a hard, gate-failing category — there is intentionally no
/// low-confidence tier (see the module docs). Matching is by name, so any
/// variant can be a false positive on a same-named user symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Time,
    Random,
    Network,
    Concurrency,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Time => "TIME",
            Category::Random => "RANDOM",
            Category::Network => "NETWORK",
            Category::Concurrency => "CONCURRENCY",
        }
    }
}

/// A single detected determinism leak.
#[derive(Debug, Clone, Serialize)]
pub struct Leak {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub category: Category,
    pub snippet: String,
    /// Enclosing function name, if the leak sits inside a `fn`.
    #[serde(rename = "fn")]
    pub func: Option<String>,
}

/// Aggregate result of a scan run.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub leaks: Vec<Leak>,
    pub files_scanned: usize,
    pub files_parsed: usize,
    /// Files that were found but failed to parse (skipped, never fatal).
    pub parse_failures: Vec<String>,
}

/// Scan `root` recursively for determinism leaks.
///
/// Walks every `.rs` file under `root`, skipping `target/` and hidden
/// directories. A file that fails to parse is counted and skipped — it never
/// aborts the scan.
pub fn scan_path(root: &Path) -> ScanReport {
    let mut report = ScanReport::default();

    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        // Never descend into `target/` or hidden dirs (but keep the root itself,
        // which may legitimately be "." or a dotted path the user passed).
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        if e.file_type().is_dir() {
            name != "target" && !name.starts_with('.')
        } else {
            true
        }
    });

    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        report.files_scanned += 1;

        let display = path.display().to_string();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                report.parse_failures.push(display);
                continue;
            }
        };
        scan_source(&display, &content, &mut report);
    }

    report.leaks.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });
    report
}

/// Scan a single in-memory source string. Exposed for testing.
pub fn scan_source(file: &str, content: &str, report: &mut ScanReport) {
    // `catch_unwind` is belt-and-suspenders: syn returns Err on bad syntax, but
    // pathological input should never crash the whole run.
    let parsed =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| syn::parse_file(content)));

    let ast = match parsed {
        Ok(Ok(ast)) => ast,
        _ => {
            report.parse_failures.push(file.to_string());
            return;
        }
    };
    report.files_parsed += 1;

    let lines: Vec<Vec<char>> = content.lines().map(|l| l.chars().collect()).collect();
    let mut visitor = LeakVisitor {
        file,
        lines: &lines,
        fn_stack: Vec::new(),
        leaks: Vec::new(),
    };
    visitor.visit_file(&ast);
    report.leaks.append(&mut visitor.leaks);
}

// ── Classification ─────────────────────────────────────────────────────────
//
// A NAME-BASED heuristic, not a sound analyzer. Matching is on the recognizable
// name or tail segments of a call/path — there is NO import, alias, or scope
// resolution. A leading `::` is transparent: it leaves the segment idents
// unchanged, so `::rand::random` matches `rand::random`. Because we match by
// name we accept false positives on same-named user symbols (e.g.
// `my_time::SystemTime::now()`) and miss renamed imports (`SystemTime as Sys`).

/// Classify a call path (the segments of `a::b::c` in `a::b::c(...)`).
///
/// Purely name/segment matching: no resolution. Returns the hard [`Category`]
/// for a recognized source, else `None`.
fn classify_call(segs: &[String]) -> Option<Category> {
    if segs.is_empty() {
        return None;
    }
    let n = segs.len();
    let last = segs[n - 1].as_str();
    let prev = if n >= 2 { segs[n - 2].as_str() } else { "" };
    let first = segs[0].as_str();

    // ── Time ──
    // `SystemTime::now` / `Instant::now` / chrono `Utc::now` / `Local::now`.
    if matches!(prev, "SystemTime" | "Instant" | "Utc" | "Local") && last == "now" {
        return Some(Category::Time);
    }
    if prev == "thread" && last == "sleep" {
        return Some(Category::Time);
    }
    // tokio::time::{sleep, sleep_until, interval, timeout}. Require the `time`
    // segment so a bare user `sleep()`/`timeout()` (no `time::` qualifier) is
    // not flagged.
    if prev == "time" && matches!(last, "sleep" | "sleep_until" | "interval" | "timeout") {
        return Some(Category::Time);
    }

    // ── Random ──
    // `rand::random()` — this one needs the `rand` segment, because a bare
    // `random()` is far too common a name to flag on its own.
    if prev == "rand" && last == "random" {
        return Some(Category::Random);
    }
    // `thread_rng` — bare `thread_rng()` (likely a `use rand::thread_rng`) or a
    // `rand`-qualified path. A clearly foreign-qualified `my_utils::thread_rng()`
    // is NOT flagged, so we avoid the obvious false positive on a same-named
    // user function.
    if last == "thread_rng" && (n == 1 || segs[..n - 1].iter().any(|s| s == "rand")) {
        return Some(Category::Random);
    }
    if first == "fastrand" {
        return Some(Category::Random);
    }
    if prev == "Uuid" && matches!(last, "new_v4" | "now_v7") {
        return Some(Category::Random);
    }
    // `<receiver>::from_entropy()` — seeding a PRNG from OS entropy. Requires a
    // receiver segment: a bare free `from_entropy()` (`n == 1`) is NOT flagged,
    // as it carries no signal that it is an RNG constructor.
    if n >= 2 && last == "from_entropy" {
        return Some(Category::Random);
    }
    // Call form `OsRng::default()` (bare or `rand::rngs::OsRng::default()`).
    // The value/path form of `OsRng` is caught in `visit_expr_path`; that
    // visitor never sees this one, whose last segment is `default`.
    if prev == "OsRng" && last == "default" {
        return Some(Category::Random);
    }

    // ── Network ──
    if first == "reqwest" {
        return Some(Category::Network);
    }
    if segs
        .iter()
        .any(|s| matches!(s.as_str(), "TcpStream" | "TcpListener" | "UdpSocket"))
    {
        return Some(Category::Network);
    }
    if n >= 2 && first == "tokio" && segs[1] == "net" {
        return Some(Category::Network);
    }

    // ── Concurrency ──
    if prev == "thread" && last == "spawn" {
        return Some(Category::Concurrency);
    }
    // tokio::spawn and tokio::task::spawn.
    if first == "tokio" && last == "spawn" {
        return Some(Category::Concurrency);
    }

    None
}

/// Classify a method call by its method name. Only the `rand` idioms `.gen()` /
/// `.gen_range()` are matched. By name, so a user `builder.gen()` is flagged
/// too (an accepted false positive — see the module docs).
fn classify_method(method: &str) -> Option<Category> {
    match method {
        "gen" | "gen_range" => Some(Category::Random),
        _ => None,
    }
}

// ── AST visitor ────────────────────────────────────────────────────────────

struct LeakVisitor<'a> {
    file: &'a str,
    lines: &'a [Vec<char>],
    fn_stack: Vec<String>,
    leaks: Vec<Leak>,
}

impl LeakVisitor<'_> {
    fn record(&mut self, cat: Category, span: proc_macro2::Span) {
        let start = span.start();
        let line = start.line;
        let col = start.column + 1;
        // Deduplicate a NESTED leak that shares the same start position and
        // category as one already recorded. The canonical case is
        // `rand::thread_rng().gen()`: the outer `.gen()` method call and the
        // inner `thread_rng()` call both begin at the same `line:col`, so the
        // site would otherwise be reported twice. The outer/most-specific match
        // is visited first (a method call is recorded before we recurse into its
        // receiver), so keeping the first-seen leak at a `(line, col, category)`
        // drops only the nested duplicate. Genuinely distinct leaks never share a
        // start position, so this never merges separate findings.
        if self
            .leaks
            .iter()
            .any(|l| l.line == line && l.col == col && l.category == cat)
        {
            return;
        }
        let snippet = snippet_from_span(self.lines, span);
        self.leaks.push(Leak {
            file: self.file.to_string(),
            line,
            col,
            category: cat,
            snippet,
            func: self.fn_stack.last().cloned(),
        });
    }
}

impl<'ast> Visit<'ast> for LeakVisitor<'_> {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.fn_stack.push(node.sig.ident.to_string());
        visit::visit_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.fn_stack.push(node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        self.fn_stack.push(node.sig.ident.to_string());
        visit::visit_trait_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            let segs: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if let Some(cat) = classify_call(&segs) {
                self.record(cat, node.span());
            }
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if let Some(cat) = classify_method(&node.method.to_string()) {
            self.record(cat, node.span());
        }
        // NOTE: `OsRng` as a method receiver (`OsRng.next_u64()`) is caught by
        // `visit_expr_path` below when we recurse into the receiver — no special
        // case needed here.
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        // `OsRng` used as a VALUE/path expression — bound to a local
        // (`let mut rng = OsRng;`), used as a receiver (`OsRng.next_u64()`), or
        // passed by value. Matched by tail name, so a bare imported `OsRng`, a
        // fully-qualified `rand::rngs::OsRng`, and a same-named user
        // `Source::OsRng` variant are all flagged (the last is an accepted false
        // positive). The call form `OsRng::default()` ends in `default`, not
        // `OsRng`, so it is handled by `classify_call`, not here.
        if node.path.segments.last().map(|s| s.ident.to_string()) == Some("OsRng".to_string()) {
            self.record(Category::Random, node.span());
        }
        visit::visit_expr_path(self, node);
    }
}

/// Slice the real source text spanned by `span`, capped to a readable length.
fn snippet_from_span(lines: &[Vec<char>], span: proc_macro2::Span) -> String {
    let start = span.start();
    let end = span.end();
    if start.line == 0 || start.line > lines.len() {
        return String::new();
    }
    let sline = &lines[start.line - 1];
    let scol = start.column.min(sline.len());

    let raw = if start.line == end.line {
        let ecol = end.column.min(sline.len()).max(scol);
        sline[scol..ecol].iter().collect::<String>()
    } else {
        let rest: String = sline[scol..].iter().collect();
        format!("{} …", rest.trim_end())
    };

    let trimmed = raw.trim();
    if trimmed.chars().count() > 80 {
        let short: String = trimmed.chars().take(79).collect();
        format!("{short}…")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(src: &str) -> Vec<Leak> {
        let mut report = ScanReport::default();
        scan_source("t.rs", src, &mut report);
        report.leaks
    }

    fn any_of(leaks: &[Leak], cat: Category) -> bool {
        leaks.iter().any(|l| l.category == cat)
    }
    fn has(leaks: &[Leak], cat: Category, needle: &str) -> bool {
        leaks
            .iter()
            .any(|l| l.category == cat && l.snippet.contains(needle))
    }

    // ── Fully-qualified forms → correct HARD category ─────────────────────────

    #[test]
    fn fully_qualified_time_forms_are_hard_time() {
        assert!(has(
            &scan("fn f() { std::time::SystemTime::now(); }"),
            Category::Time,
            "SystemTime::now"
        ));
        assert!(has(
            &scan("fn f() { std::time::Instant::now(); }"),
            Category::Time,
            "Instant::now"
        ));
        assert!(has(
            &scan("fn f() { std::thread::sleep(d); }"),
            Category::Time,
            "thread::sleep"
        ));
        for form in ["sleep", "sleep_until", "interval", "timeout"] {
            let src = format!("fn f() {{ tokio::time::{form}(x); }}");
            assert!(
                has(&scan(&src), Category::Time, form),
                "tokio::time::{form} must be hard TIME"
            );
        }
        assert!(has(
            &scan("fn f() { chrono::Utc::now(); }"),
            Category::Time,
            "Utc::now"
        ));
        assert!(has(
            &scan("fn f() { chrono::Local::now(); }"),
            Category::Time,
            "Local::now"
        ));
    }

    #[test]
    fn fully_qualified_random_forms_are_hard_random() {
        assert!(has(
            &scan("fn f() { rand::random::<u32>(); }"),
            Category::Random,
            "rand::random"
        ));
        assert!(has(
            &scan("fn f() { rand::thread_rng(); }"),
            Category::Random,
            "thread_rng"
        ));
        assert!(has(
            &scan("fn f() { fastrand::u32(0..10); }"),
            Category::Random,
            "fastrand"
        ));
        assert!(has(
            &scan("fn f() { uuid::Uuid::new_v4(); }"),
            Category::Random,
            "new_v4"
        ));
        assert!(has(
            &scan("fn f() { uuid::Uuid::now_v7(); }"),
            Category::Random,
            "now_v7"
        ));
        // `.gen()` / `.gen_range()` method idioms.
        assert!(any_of(
            &scan("fn f() { let x = rng.gen_range(0..10); }"),
            Category::Random
        ));
        assert!(any_of(
            &scan("fn f() { let x: u8 = rng.gen(); }"),
            Category::Random
        ));
        // Fully-qualified from_entropy and OsRng.
        assert!(has(
            &scan("fn f() { rand::rngs::SmallRng::from_entropy(); }"),
            Category::Random,
            "from_entropy"
        ));
        assert!(has(
            &scan("fn f() -> u64 { rand::rngs::OsRng.next_u64() }"),
            Category::Random,
            "OsRng"
        ));
        assert!(has(
            &scan("fn f() { let mut rng = rand::rngs::OsRng; }"),
            Category::Random,
            "OsRng"
        ));
    }

    #[test]
    fn fully_qualified_network_forms_are_hard_network() {
        assert!(has(
            &scan("fn f() { reqwest::get(u); }"),
            Category::Network,
            "reqwest::get"
        ));
        assert!(has(
            &scan("fn f() { std::net::TcpStream::connect(a); }"),
            Category::Network,
            "TcpStream"
        ));
        assert!(has(
            &scan("fn f() { std::net::TcpListener::bind(a); }"),
            Category::Network,
            "TcpListener"
        ));
        assert!(has(
            &scan("fn f() { std::net::UdpSocket::bind(a); }"),
            Category::Network,
            "UdpSocket"
        ));
        assert!(has(
            &scan("fn f() { tokio::net::TcpStream::connect(a); }"),
            Category::Network,
            "tokio::net"
        ));
    }

    #[test]
    fn fully_qualified_concurrency_forms_are_hard_concurrency() {
        assert!(has(
            &scan("fn f() { std::thread::spawn(|| {}); }"),
            Category::Concurrency,
            "thread::spawn"
        ));
        assert!(has(
            &scan("fn f() { tokio::spawn(async {}); }"),
            Category::Concurrency,
            "tokio::spawn"
        ));
    }

    // ── Bare / imported forms are ALSO flagged (name-based, err toward flag) ───

    #[test]
    fn bare_imported_os_rng_value_is_hard_random() {
        // `use rand::rngs::OsRng; … let _r = OsRng;` — the import is gone from
        // the AST, but the bare name resolves to the real RNG. Flag it.
        let leaks = scan("fn f() { let _r = OsRng; }");
        assert!(
            has(&leaks, Category::Random, "OsRng"),
            "bare OsRng value must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn bare_imported_thread_rng_call_is_hard_random() {
        // `use rand::thread_rng; … thread_rng()` — flagged by name.
        let leaks = scan("fn f() { let _ = thread_rng(); }");
        assert!(
            has(&leaks, Category::Random, "thread_rng"),
            "bare thread_rng() must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn os_rng_default_call_form_is_hard_random() {
        // Bug fix: the associated-fn CALL form `OsRng::default()` was dropped
        // (its last segment is `default`, so the path visitor never saw it).
        // Both the bare and fully-qualified spellings must be flagged, exactly
        // once each, including when chained into `.next_u64()`.
        for src in [
            "fn f() { let _r = OsRng::default(); }",
            "fn f() -> u64 { OsRng::default().next_u64() }",
            "fn f() -> u64 { rand::rngs::OsRng::default().next_u64() }",
        ] {
            let leaks = scan(src);
            let os: Vec<_> = leaks
                .iter()
                .filter(|l| l.category == Category::Random)
                .collect();
            assert_eq!(
                os.len(),
                1,
                "OsRng::default() must be exactly one RANDOM leak in `{src}`: {leaks:#?}"
            );
        }
    }

    #[test]
    fn receiver_from_entropy_is_hard_random() {
        // `<receiver>::from_entropy()` on ANY receiver is flagged (the receiver
        // segment is the signal it is an RNG constructor).
        let leaks = scan("fn f() { let _ = Config::from_entropy(); }");
        assert!(
            has(&leaks, Category::Random, "from_entropy"),
            "Config::from_entropy() must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn bare_free_from_entropy_is_not_flagged() {
        // Bug fix: a bare free `from_entropy()` (no receiver segment) carries no
        // signal that it is an RNG constructor and must NOT be flagged.
        let leaks = scan("fn f() { let _ = from_entropy(); }");
        assert!(
            leaks.is_empty(),
            "bare free from_entropy() must NOT be flagged: {leaks:#?}"
        );
    }

    #[test]
    fn fully_qualified_os_rng_is_flagged_exactly_once() {
        // The receiver form must be flagged, and not double-counted.
        let leaks = scan("fn f() -> u64 { rand::rngs::OsRng.next_u64() }");
        let os: Vec<_> = leaks
            .iter()
            .filter(|l| l.category == Category::Random && l.snippet.contains("OsRng"))
            .collect();
        assert_eq!(os.len(), 1, "expected exactly one OsRng leak: {leaks:#?}");
    }

    // ── Accepted false positives (name-based ⇒ same-named user symbols flag) ───
    //
    // These lock the honest contract: the scanner matches by name, so user
    // symbols that share a determinism-source name ARE flagged. That is the
    // intended, safe-direction behavior for a replay-safety gate — not a bug.

    #[test]
    fn accepted_fp_user_system_time_now() {
        // `my_time::SystemTime::now()` — a user type named `SystemTime` with a
        // `now` assoc fn. Matched on the `SystemTime::now` tail → flagged.
        let leaks = scan("fn f() { my_time::SystemTime::now(); }");
        assert!(
            has(&leaks, Category::Time, "SystemTime::now"),
            "same-named user SystemTime::now() IS flagged (accepted FP): {leaks:#?}"
        );
    }

    #[test]
    fn accepted_fp_user_gen_method() {
        // `builder.gen()` — a user builder with a `gen()` method. Matched on the
        // `.gen()` method name → flagged.
        let leaks = scan("fn f() { builder.gen(); }");
        assert!(
            any_of(&leaks, Category::Random),
            "same-named user .gen() IS flagged (accepted FP): {leaks:#?}"
        );
    }

    #[test]
    fn foreign_qualified_thread_rng_is_not_flagged() {
        // A clearly foreign-qualified `my_utils::thread_rng()` is NOT flagged —
        // the distinctive name lets us skip the obvious false positive.
        let leaks = scan("fn f() { let _ = my_utils::thread_rng(); }");
        assert!(
            !has(&leaks, Category::Random, "thread_rng"),
            "foreign my_utils::thread_rng() must not be flagged: {leaks:#?}"
        );
    }

    #[test]
    fn bare_thread_rng_is_flagged() {
        // A bare `thread_rng()` (likely a `use rand::thread_rng`) IS flagged.
        let leaks = scan("fn f() { let _ = thread_rng(); }");
        assert!(
            has(&leaks, Category::Random, "thread_rng"),
            "bare thread_rng() must be flagged: {leaks:#?}"
        );
    }

    #[test]
    fn accepted_fp_user_os_rng_variant() {
        // `enum Source { OsRng }` then `Source::OsRng` — a user enum variant that
        // shares the name IS flagged (accepted FP): matching is by tail name.
        let leaks = scan("enum Source { OsRng } fn f() -> Source { Source::OsRng }");
        assert!(
            any_of(&leaks, Category::Random),
            "user Source::OsRng variant IS flagged (accepted FP): {leaks:#?}"
        );
    }

    #[test]
    fn nested_thread_rng_gen_is_one_finding_not_two() {
        // `rand::thread_rng().gen()` fires for BOTH the outer `.gen()` method
        // call and the inner `thread_rng()` call, at the SAME `line:col`. It must
        // deduplicate to exactly one RANDOM finding (the outermost / most
        // specific), not two.
        let leaks = scan("fn f() { let x: u8 = rand::thread_rng().gen(); }");
        let random: Vec<_> = leaks
            .iter()
            .filter(|l| l.category == Category::Random)
            .collect();
        assert_eq!(
            random.len(),
            1,
            "nested thread_rng().gen() must yield exactly one RANDOM leak: {leaks:#?}"
        );
    }

    #[test]
    fn distinct_rng_calls_on_same_line_are_not_merged() {
        // Two genuinely distinct RNG calls (different columns) must both be
        // reported — dedup keys on the exact start position, so it never merges
        // separate findings that merely share a line.
        let leaks = scan("fn f() { let _ = (thread_rng(), thread_rng()); }");
        let random: Vec<_> = leaks
            .iter()
            .filter(|l| l.category == Category::Random)
            .collect();
        assert_eq!(
            random.len(),
            2,
            "two distinct thread_rng() calls must both be flagged: {leaks:#?}"
        );
    }

    // ── Documented false negatives (renamed imports slip through) ──────────────

    #[test]
    fn renamed_import_is_a_documented_false_negative() {
        // `use std::time::SystemTime as Sys; Sys::now()` — the alias hides the
        // recognizable name, so it is NOT flagged. Documented limitation: no
        // name resolution.
        let leaks = scan("fn f() { Sys::now(); }");
        assert!(
            leaks.is_empty(),
            "aliased Sys::now() is a documented false negative (not flagged): {leaks:#?}"
        );
    }
}
