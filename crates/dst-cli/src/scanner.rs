//! Determinism-leak scanner.
//!
//! Parses Rust source with [`syn`] and walks the AST looking for calls that
//! introduce non-determinism — wall-clock time, randomness, network I/O, and
//! unstructured concurrency. The scanner is deliberately **conservative**: it
//! only flags call/method syntax it can attribute to a known non-deterministic
//! API, preferring a missed leak (false negative) over a spurious one (false
//! positive).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
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

/// Classify a call path (the segments of `a::b::c` in `a::b::c(...)`).
///
/// Matching keys on the meaningful *tail* of the path (and, for crate-scoped
/// APIs like `fastrand`/`reqwest`/`tokio`, the head), so it works whether the
/// call is fully qualified or reached through a `use`.
fn classify_call(segs: &[String]) -> Option<Category> {
    if segs.is_empty() {
        return None;
    }
    let n = segs.len();
    let last = segs[n - 1].as_str();
    let prev = if n >= 2 { segs[n - 2].as_str() } else { "" };
    let first = segs[0].as_str();

    // ── Time ──
    if prev == "SystemTime" && last == "now" {
        return Some(Category::Time);
    }
    if prev == "Instant" && last == "now" {
        return Some(Category::Time);
    }
    if prev == "thread" && last == "sleep" {
        return Some(Category::Time);
    }
    // tokio::time::{sleep, sleep_until, interval, timeout}. Require the `time`
    // qualifier so a user's own free `sleep()`/`timeout()` is not flagged.
    if prev == "time" && matches!(last, "sleep" | "sleep_until" | "interval" | "timeout") {
        return Some(Category::Time);
    }
    if (prev == "Utc" || prev == "Local") && last == "now" {
        return Some(Category::Time);
    }

    // ── Random ──
    if prev == "rand" && last == "random" {
        return Some(Category::Random);
    }
    if last == "thread_rng" {
        return Some(Category::Random);
    }
    if first == "fastrand" {
        return Some(Category::Random);
    }
    if prev == "Uuid" && last == "new_v4" {
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

/// Classify a method call by its method name. Only the unambiguous `rand`
/// idioms `.gen()` / `.gen_range()` are matched — see the precision caveat in
/// the module docs.
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
        let snippet = snippet_from_span(self.lines, span);
        self.leaks.push(Leak {
            file: self.file.to_string(),
            line: start.line,
            col: start.column + 1,
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
        visit::visit_expr_method_call(self, node);
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
