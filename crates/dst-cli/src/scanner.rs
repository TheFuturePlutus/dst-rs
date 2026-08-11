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
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Time,
    Random,
    /// Lower-confidence randomness: a call shaped like a known RNG idiom
    /// (e.g. `*::from_entropy()`) but on an UNRECOGNIZED receiver, where we
    /// cannot prove it is an RNG. Reported so a human can confirm, but kept
    /// out of the hard `Random` set to avoid false positives (e.g. a user's
    /// `Config::from_entropy()`).
    PossibleRandom,
    Network,
    Concurrency,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Time => "TIME",
            Category::Random => "RANDOM",
            Category::PossibleRandom => "POSSIBLE-RANDOM",
            Category::Network => "NETWORK",
            Category::Concurrency => "CONCURRENCY",
        }
    }

    /// Whether this category is a HIGH-CONFIDENCE leak. `true` for the four hard
    /// categories (time / random / network / concurrency); `false` only for
    /// [`Category::PossibleRandom`], the explicitly low-confidence guess.
    ///
    /// The `--deny` CI gate fails only on hard categories, so a lone
    /// `PossibleRandom` (e.g. a user `Config::from_entropy()`) never breaks CI.
    pub fn is_hard(self) -> bool {
        !matches!(self, Category::PossibleRandom)
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
        os_rng_imported: file_imports_os_rng(&ast),
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
    // `thread_rng` qualified to the `rand::thread_rng` call or the bare
    // `use rand::thread_rng; thread_rng()` idiom — so a user's own
    // `my_utils::thread_rng()` is NOT falsely flagged.
    if last == "thread_rng" && (n == 1 || prev == "rand") {
        return Some(Category::Random);
    }
    if first == "fastrand" {
        return Some(Category::Random);
    }
    if prev == "Uuid" && matches!(last, "new_v4" | "now_v7") {
        return Some(Category::Random);
    }
    // Seeding a PRNG from OS entropy is non-deterministic:
    // `SmallRng::from_entropy()`, `StdRng::from_entropy()`, etc. Only a
    // recognizable RNG receiver is a HARD leak; an unknown `Foo::from_entropy()`
    // (e.g. a user `Config::from_entropy()`) is emitted lower-confidence so it is
    // not a false positive.
    if last == "from_entropy" {
        return Some(if is_rng_receiver(prev) {
            Category::Random
        } else {
            Category::PossibleRandom
        });
    }
    // NOTE: `OsRng` is NOT classified here. Because the bare name `OsRng` also
    // names unrelated symbols (e.g. a user `enum Source { OsRng }`), it must be
    // qualified against `rand::rngs` — or a proven `use rand::rngs::OsRng` — to
    // be a HARD leak. That needs import context, so it is handled in the visitor
    // (`visit_expr_call` / `visit_expr_path`) via `classify_os_rng`, which
    // downgrades unrecognized receivers to `PossibleRandom`.

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

/// Whether `seg` (the path segment immediately before `from_entropy`) names a
/// recognizable RNG type, so `<seg>::from_entropy()` is a hard RANDOM leak. The
/// `Rng` suffix covers the `rand` family (`SmallRng`, `StdRng`, `ChaCha20Rng`,
/// `OsRng`, …); a short allow-list covers common non-suffixed generators.
fn is_rng_receiver(seg: &str) -> bool {
    const KNOWN_RNGS: &[&str] = &["Pcg32", "Pcg64", "Lcg64Xsh32", "Xoshiro256PlusPlus"];
    seg.ends_with("Rng") || KNOWN_RNGS.contains(&seg)
}

/// Classify a use of the `OsRng` symbol given the path segment immediately
/// before it (`qualifier`) and whether the file provably imports
/// `rand::rngs::OsRng`.
///
/// Hard [`Category::Random`] ONLY for the real OS generator:
/// `rand::rngs::OsRng` / `rngs::OsRng` (qualifier is `rngs`), or a bare `OsRng`
/// under a proven `use rand::rngs::OsRng`. Any other receiver — e.g. a
/// `Source::OsRng` enum variant, or a bare `OsRng` we cannot tie to `rand` — is
/// downgraded to lower-confidence [`Category::PossibleRandom`] so it is reported
/// but never a hard leak / CI failure.
fn classify_os_rng(qualifier: Option<&str>, os_rng_imported: bool) -> Category {
    match qualifier {
        Some("rngs") => Category::Random,
        None if os_rng_imported => Category::Random,
        _ => Category::PossibleRandom,
    }
}

/// Whether the file's TOP-LEVEL `use` items bring the bare name `OsRng` into
/// scope from `rand::rngs`: `use rand::rngs::OsRng`, `use rand::rngs::{OsRng,…}`,
/// `use rand::{rngs::OsRng, …}`, or `use rand::rngs::*`. A rename (`… as X`) does
/// NOT (the bare name is not introduced).
///
/// We deliberately do NOT recurse into inline `mod` blocks: `mod m { use
/// rand::rngs::OsRng; }` does not bring `OsRng` into the file's outer scope, and
/// treating it as if it did would hard-flag an unrelated sibling `OsRng`
/// (unsound without a name resolver). A missed import merely downgrades a real
/// bare `OsRng` to `PossibleRandom`, which is the safe (conservative) direction.
fn file_imports_os_rng(ast: &syn::File) -> bool {
    ast.items.iter().any(|it| match it {
        syn::Item::Use(u) => use_tree_after_rand(&u.tree),
        _ => false,
    })
}

/// Match a `use` tree rooted at `rand`, descending toward `rngs::OsRng`.
fn use_tree_after_rand(tree: &syn::UseTree) -> bool {
    match tree {
        syn::UseTree::Path(p) if p.ident == "rand" => use_tree_after_rngs(&p.tree),
        _ => false,
    }
}

/// Match the `rngs::<…>` portion of a `rand::…` use tree.
fn use_tree_after_rngs(tree: &syn::UseTree) -> bool {
    match tree {
        syn::UseTree::Path(p) if p.ident == "rngs" => use_tree_names_os_rng(&p.tree),
        syn::UseTree::Group(g) => g.items.iter().any(use_tree_after_rngs),
        _ => false,
    }
}

/// Match the leaf naming `OsRng` (or a glob) under `rand::rngs`.
fn use_tree_names_os_rng(tree: &syn::UseTree) -> bool {
    match tree {
        syn::UseTree::Name(n) => n.ident == "OsRng",
        syn::UseTree::Glob(_) => true, // `rand::rngs::*` brings `OsRng` into scope
        syn::UseTree::Group(g) => g.items.iter().any(use_tree_names_os_rng),
        // A deeper `Path` or a `Rename` does not introduce the bare `OsRng`.
        syn::UseTree::Path(_) | syn::UseTree::Rename(_) => false,
    }
}

// ── AST visitor ────────────────────────────────────────────────────────────

struct LeakVisitor<'a> {
    file: &'a str,
    lines: &'a [Vec<char>],
    fn_stack: Vec<String>,
    leaks: Vec<Leak>,
    /// The file provably imports `rand::rngs::OsRng` into bare-name scope, so a
    /// bare `OsRng` value/receiver is the real generator (a HARD leak) rather
    /// than a lower-confidence guess. See [`file_imports_os_rng`].
    os_rng_imported: bool,
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
            } else if let Some(pos) = segs.iter().rposition(|s| s == "OsRng") {
                // Call form on the `OsRng` type (e.g. `OsRng::default()`,
                // `rand::rngs::OsRng::new(...)`). Qualify against the segment
                // before `OsRng` so a `Source::OsRng::…` variant is not a hard
                // leak. The bare-value/receiver forms are handled by
                // `visit_expr_path`; a call ends past `OsRng`, so no overlap.
                let qualifier = pos.checked_sub(1).map(|i| segs[i].as_str());
                self.record(
                    classify_os_rng(qualifier, self.os_rng_imported),
                    node.span(),
                );
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
        // `OsRng` used as a bare VALUE/path expression — the OS entropy source
        // bound to a local (`let mut rng = OsRng;`), used as a receiver
        // (`OsRng.next_u64()`), or passed by value. A call-form assoc access like
        // `OsRng::default` ends in `default`, so it is NOT matched here (it is
        // handled in `visit_expr_call`), avoiding a double count.
        //
        // Only the REAL generator is a HARD leak: `rand::rngs::OsRng` /
        // `rngs::OsRng`, or a bare `OsRng` under a proven import. A qualified
        // `Source::OsRng` (enum variant) or an unproven bare `OsRng` is
        // downgraded to `PossibleRandom` by `classify_os_rng`.
        let segs = &node.path.segments;
        if segs.last().map(|s| s.ident == "OsRng").unwrap_or(false) {
            let n = segs.len();
            let qualifier = if n >= 2 {
                Some(segs[n - 2].ident.to_string())
            } else {
                None
            };
            self.record(
                classify_os_rng(qualifier.as_deref(), self.os_rng_imported),
                node.span(),
            );
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

    #[test]
    fn os_rng_bound_to_a_local_is_flagged() {
        // `let mut rng = OsRng; rng.fill_bytes(..)` — OsRng appears as a bare
        // value/path expression, not as a method receiver. With a proven
        // `use rand::rngs::OsRng`, the bare form must still be a HARD leak
        // (regression: the receiver-only check missed this).
        let leaks = scan(
            "use rand::rngs::OsRng; fn f() { let mut rng = OsRng; let mut b = [0u8; 8]; rng.fill_bytes(&mut b); }",
        );
        let os: Vec<_> = leaks
            .iter()
            .filter(|l| l.category == Category::Random && l.snippet.contains("OsRng"))
            .collect();
        assert_eq!(os.len(), 1, "expected exactly one OsRng leak: {leaks:#?}");
    }

    #[test]
    fn bare_os_rng_without_import_is_possible_random_not_hard() {
        // Without a proven `use rand::rngs::OsRng`, a bare `OsRng` cannot be tied
        // to `rand` → lower-confidence POSSIBLE-RANDOM, never a hard leak.
        let leaks = scan("fn f() { let _rng = OsRng; }");
        assert!(
            !leaks.iter().any(|l| l.category == Category::Random),
            "unproven bare OsRng must not be hard RANDOM: {leaks:#?}"
        );
        assert!(
            leaks.iter().any(|l| l.category == Category::PossibleRandom),
            "unproven bare OsRng should be POSSIBLE-RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn imported_bare_os_rng_is_hard_random() {
        // `use rand::rngs::OsRng; let r = OsRng;` — the real generator, proven by
        // import → HARD RANDOM.
        let leaks = scan("use rand::rngs::OsRng; fn f() { let _r = OsRng; }");
        assert!(
            leaks
                .iter()
                .any(|l| l.category == Category::Random && l.snippet.contains("OsRng")),
            "imported bare OsRng must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn enum_variant_os_rng_is_not_hard_random() {
        // `enum Source { OsRng }` then `Source::OsRng` — a user enum variant that
        // merely shares the name must NOT be a hard RANDOM false positive.
        let leaks = scan("enum Source { OsRng } fn f() -> Source { Source::OsRng }");
        assert!(
            !leaks.iter().any(|l| l.category == Category::Random),
            "Source::OsRng variant must not be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn fully_qualified_os_rng_is_hard_random_without_import() {
        // `rand::rngs::OsRng.next_u64()` needs no import — the `rngs` qualifier
        // proves the real generator → HARD RANDOM.
        let leaks = scan("fn f() -> u64 { rand::rngs::OsRng.next_u64() }");
        assert!(
            leaks
                .iter()
                .any(|l| l.category == Category::Random && l.snippet.contains("OsRng")),
            "qualified OsRng must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn os_rng_as_method_receiver_is_flagged_exactly_once() {
        // The classic receiver form must still be flagged, and not double-counted
        // now that detection lives in the path visitor.
        let leaks = scan("fn f() -> u64 { rand::rngs::OsRng.next_u64() }");
        let os: Vec<_> = leaks
            .iter()
            .filter(|l| l.category == Category::Random && l.snippet.contains("OsRng"))
            .collect();
        assert_eq!(os.len(), 1, "expected exactly one OsRng leak: {leaks:#?}");
    }

    #[test]
    fn from_entropy_on_known_rng_is_hard_random() {
        // `SmallRng::from_entropy()` — a recognized RNG receiver → hard RANDOM.
        let leaks = scan("fn f() { let _ = rand::rngs::SmallRng::from_entropy(); }");
        assert!(
            leaks
                .iter()
                .any(|l| l.category == Category::Random && l.snippet.contains("from_entropy")),
            "SmallRng::from_entropy must be RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn from_entropy_on_unknown_receiver_is_possible_random_not_random() {
        // `Config::from_entropy()` is NOT a known RNG; it must not be a hard
        // RANDOM false positive — it is reported lower-confidence instead.
        let leaks = scan("fn f() { let _ = Config::from_entropy(); }");
        assert!(
            !leaks.iter().any(|l| l.category == Category::Random),
            "Config::from_entropy must not be hard RANDOM: {leaks:#?}"
        );
        assert!(
            leaks.iter().any(|l| l.category == Category::PossibleRandom),
            "Config::from_entropy should be POSSIBLE-RANDOM: {leaks:#?}"
        );
    }
}
