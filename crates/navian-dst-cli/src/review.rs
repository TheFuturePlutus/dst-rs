//! Adversarial invariant review.
//!
//! [`crate::presence`] certifies invariants are PRESENT. This module goes one step
//! further and critiques the ones that ARE there — deterministically, and offline.
//! It never decides whether an invariant is *correct* (that is domain policy the
//! author owns); it flags invariants that are structurally hollow and hands a
//! ready-to-run adversarial prompt to your LLM/agent for the domain-specific part.
//!
//! ## What it checks (deterministic, domain-agnostic, no network)
//!
//! For every `Invariant::new("name", |state| …)` it finds:
//!
//! * **Tautology** — the predicate can never be false (`|_| true`, a pure
//!   `|w| cond || true`, a body that is just `{ true }`). A check that can't fail is
//!   not a check.
//!   (Self-equality `w.x == w.x` is deliberately NOT treated as a tautology — without
//!   types it is unsound: `NaN != NaN`, and a custom `PartialEq` may be non-reflexive.)
//! * **Ignores state** — the predicate does not read the `state` parameter it is
//!   handed (`|_| CONST > 0`, or a bound name that never appears in the body). It
//!   asserts something, but nothing *about the run*.
//! * **Duplicate** — two invariants with the same (whitespace-normalized) predicate
//!   source. Redundant coverage masquerading as breadth.
//!
//! ## What it hands off (the domain part)
//!
//! "Which invariants are you *missing*?" is domain knowledge — conservation,
//! non-negativity, idempotency, ordering, reversal, whatever your system means. A
//! domain-agnostic tool must not hardcode that, so `review` emits the extracted
//! invariant set inside an adversarial critique prompt (`--format json` for an
//! agent, or a paste-ready block) for the LLM/agent you already use. The tool
//! votes; it never gates and never rewrites your policy.
//!
//! Like [`crate::scanner`]/[`crate::presence`] this is a NAME/AST heuristic, not a
//! sound analyzer: it errs toward NOT flagging (a hollow invariant slipping through
//! is safer than nagging about a real one), and it only sees `Invariant::new` spelled
//! plainly (no alias/type-alias resolution — a documented limitation). IGNORES-STATE
//! is also not scope-aware: `|w| { let w = GLOBAL; … }` (the param shadowed by a
//! local) reads "uses state" even though it doesn't — a false NEGATIVE, the safe
//! direction, so it is accepted rather than risk a false positive.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use walkdir::WalkDir;

/// Ceiling on macro-body re-parse recursion (see [`ExtractVisitor::visit_macro`]),
/// mirroring the leak scanner / presence gate.
const MAX_MACRO_DEPTH: u32 = 32;

/// The kind of structural weakness found in an invariant's predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Weakness {
    /// The predicate can never be false — a check that cannot fail.
    Tautology,
    /// The predicate never reads the state parameter it is handed.
    IgnoresState,
    /// Same predicate source as another invariant in the set.
    Duplicate,
}

impl Weakness {
    /// Short upper-case label for the human report.
    pub fn label(self) -> &'static str {
        match self {
            Weakness::Tautology => "TAUTOLOGY",
            Weakness::IgnoresState => "IGNORES-STATE",
            Weakness::Duplicate => "DUPLICATE",
        }
    }

    /// One-line explanation of why this weakens the invariant.
    pub fn why(self) -> &'static str {
        match self {
            Weakness::Tautology => "predicate can never be false — a check that cannot fail is not a check",
            Weakness::IgnoresState => "predicate never reads the state it is given — it asserts nothing about the run",
            Weakness::Duplicate => "same predicate as another invariant — redundant coverage, not breadth",
        }
    }
}

/// One extracted invariant, with any structural weaknesses found in it.
#[derive(Debug, Clone, Serialize)]
pub struct Invariant {
    /// The name literal passed to `Invariant::new`, or `<non-literal>` when it was
    /// not a plain string literal.
    pub name: String,
    /// The predicate source text, exactly as written (whitespace-collapsed for the
    /// report so long closures stay one line).
    pub predicate: String,
    pub file: String,
    pub line: usize,
    /// Structural weaknesses found (may be empty).
    pub weaknesses: Vec<Weakness>,
    /// Internal: the invariant SET this belongs to — `file#setN` for invariants in
    /// the same `vec!`, or a unique `file#soloN` for a loose call. DUPLICATE is
    /// detected only WITHIN one set, so two independent `vec!`s (or two tests) that
    /// each assert the same property are not duplicates of each other.
    #[serde(skip)]
    scope: String,
}

/// Aggregate result of a review run.
#[derive(Debug, Default, Serialize)]
pub struct ReviewReport {
    /// Every `Invariant::new` found, in file/line order.
    pub invariants: Vec<Invariant>,
    #[serde(skip)]
    pub files_scanned: usize,
    #[serde(skip)]
    pub files_parsed: usize,
    /// Files that could not be read or parsed (skipped).
    pub parse_failures: Vec<String>,
}

impl ReviewReport {
    /// Count of invariants carrying at least one weakness.
    pub fn flagged(&self) -> usize {
        self.invariants
            .iter()
            .filter(|i| !i.weaknesses.is_empty())
            .count()
    }
}

/// Walk `root` recursively and review every `Invariant::new` in every `.rs` file.
/// Skips `target/` and hidden directories, mirroring the other subcommands.
pub fn review_path(root: &Path) -> ReviewReport {
    let mut report = ReviewReport::default();

    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
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

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let where_ = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown path>".to_string());
                report.parse_failures.push(format!("{where_}: {e}"));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        report.files_scanned += 1;
        let display = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(content) => review_source(&display, &content, &mut report),
            Err(_) => report.parse_failures.push(display),
        }
    }

    report.invariants.sort_by(|a, b| {
        a.file.cmp(&b.file).then(a.line.cmp(&b.line))
    });
    // Duplicate detection is a SET property, so it runs once the whole file's
    // invariants are collected — see `mark_duplicates`.
    mark_duplicates(&mut report.invariants);
    report
}

/// Review a single in-memory source string. Exposed for testing.
pub fn review_source(file: &str, content: &str, report: &mut ReviewReport) {
    // Reject a pathologically deep file before parsing — a stack overflow in syn
    // aborts the process (catch_unwind can't catch it). Shared with the presence gate.
    if crate::presence::exceeds_nesting_depth(content) {
        report.parse_failures.push(file.to_string());
        return;
    }
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

    // Local shadow guard: if the file defines its own `Invariant` type, a BARE
    // `Invariant::new(...)` is the user's own — but an explicitly-qualified
    // `navian_dst::Invariant::new(...)` in the same file is still the real one. So
    // the guard is applied per-call (on bare paths), not by skipping the whole file.
    let mut defs = LocalDefs::default();
    defs.visit_file(&ast);

    let lines: Vec<Vec<char>> = content.lines().map(|l| l.chars().collect()).collect();
    let mut v = ExtractVisitor {
        file,
        lines: &lines,
        out: Vec::new(),
        macro_depth: 0,
        set_stack: Vec::new(),
        next_id: 0,
        local_invariant: defs.defines_invariant,
    };
    v.visit_file(&ast);
    report.invariants.append(&mut v.out);
}

/// Mark invariants whose (normalized) predicate source is shared by another IN THE
/// SAME SET (see [`Invariant::scope`] — one `vec!`). The FIRST occurrence in a set
/// is not flagged; each subsequent duplicate is. Scoping matters: two independent
/// `vec!`s (or two tests) that each legitimately assert `|w| w.balance >= 0` are NOT
/// duplicates.
fn mark_duplicates(invariants: &mut [Invariant]) {
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for inv in invariants.iter_mut() {
        let pred = normalize_ws(&inv.predicate);
        // An empty/degenerate predicate string is not a meaningful duplicate key.
        if pred.is_empty() {
            continue;
        }
        let count = seen.entry((inv.scope.clone(), pred)).or_insert(0);
        if *count > 0 && !inv.weaknesses.contains(&Weakness::Duplicate) {
            inv.weaknesses.push(Weakness::Duplicate);
        }
        *count += 1;
    }
}

/// Collapse runs of whitespace to a single space and trim — the normalized form
/// used for duplicate detection and the report snippet.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── Local definitions (shadow guard) ─────────────────────────────────────────

#[derive(Default)]
struct LocalDefs {
    defines_invariant: bool,
}

impl<'ast> Visit<'ast> for LocalDefs {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        if i.ident == "Invariant" {
            self.defines_invariant = true;
        }
        visit::visit_item_struct(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        if i.ident == "Invariant" {
            self.defines_invariant = true;
        }
        visit::visit_item_enum(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        if i.ident == "Invariant" {
            self.defines_invariant = true;
        }
        visit::visit_item_type(self, i);
    }
}

// ── Extraction ───────────────────────────────────────────────────────────────

struct ExtractVisitor<'a> {
    file: &'a str,
    lines: &'a [Vec<char>],
    out: Vec<Invariant>,
    /// Depth of macro bodies currently being re-parsed. syn does not descend into
    /// macro token streams, so `InvariantEngine::new(vec![Invariant::new(...)])` —
    /// the overwhelmingly common shape — would be invisible without this.
    macro_depth: u32,
    /// Ids of the `vec!` invariant SETS we are currently inside, innermost last.
    set_stack: Vec<u64>,
    /// Monotonic id source for sets AND for "solo" invariants (unique per call).
    next_id: u64,
    /// The file defines its own `Invariant` type, so a BARE `Invariant::new` is the
    /// user's own — but a qualified `navian_dst::Invariant::new` is still extracted.
    local_invariant: bool,
}

impl ExtractVisitor<'_> {
    /// Scope key for the invariant about to be recorded. Invariants inside the same
    /// `vec!` share the set id (so DUPLICATE compares within one collection); an
    /// invariant NOT inside a `vec!` set gets a UNIQUE id, so it is never flagged a
    /// duplicate of anything (we can't prove two loose calls share a set).
    fn next_scope(&mut self) -> String {
        match self.set_stack.last() {
            Some(id) => format!("{}#set{}", self.file, id),
            None => {
                let id = self.next_id;
                self.next_id += 1;
                format!("{}#solo{}", self.file, id)
            }
        }
    }
}

impl ExtractVisitor<'_> {
    /// Slice the source text spanned by `node` out of the original file, so the
    /// predicate is reported exactly as written. Returns a whitespace-normalized
    /// single line, or an empty string if span coordinates are absent (they are
    /// present here — we parse from an in-memory string with `span-locations` on).
    fn source_of(&self, node: &impl syn::spanned::Spanned) -> String {
        let span = node.span();
        let (s_line, s_col) = (span.start().line, span.start().column);
        let (e_line, e_col) = (span.end().line, span.end().column);
        // `line` is 1-based; our `lines` vec is 0-based.
        if s_line == 0 || s_line > self.lines.len() || e_line > self.lines.len() {
            return String::new();
        }
        let mut buf = String::new();
        for ln in s_line..=e_line {
            let row = &self.lines[ln - 1];
            let from = if ln == s_line { s_col } else { 0 };
            let to = if ln == e_line { e_col.min(row.len()) } else { row.len() };
            if from <= to && from <= row.len() {
                buf.extend(row[from..to.min(row.len())].iter());
            }
            buf.push(' ');
        }
        normalize_ws(&buf)
    }
}

impl<'ast> Visit<'ast> for ExtractVisitor<'_> {
    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func {
            // A BARE `Invariant::new` (2 segments) in a file that defines its own
            // `Invariant` type is the user's symbol — skip it. Qualified paths
            // (`navian_dst::Invariant::new`) are always the real one.
            let bare = p.path.segments.len() == 2;
            if is_invariant_new(&p.path) && !(bare && self.local_invariant) {
                let name = c.args.first().and_then(name_of).unwrap_or_else(|| "<non-literal>".to_string());
                if let Some(pred) = c.args.get(1) {
                    let predicate = self.source_of(pred);
                    let weaknesses = predicate_weaknesses(pred);
                    let line = predicate_line(pred).max(1);
                    let scope = self.next_scope();
                    self.out.push(Invariant {
                        name,
                        predicate,
                        file: self.file.to_string(),
                        line,
                        weaknesses,
                        scope,
                    });
                }
            }
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // Only descend into `vec!` — the shape that actually builds an invariant
        // list (`InvariantEngine::new(vec![Invariant::new(...), ...])`). Re-parsing
        // ARBITRARY macros would extract FAKE invariants from token-data macros
        // (`stringify!`, `quote!`, custom DSLs) that construct nothing. Each `vec!`
        // is one invariant SET: its invariants share a scope so DUPLICATE compares
        // only within the same declared collection.
        let is_vec = mac.path.segments.last().is_some_and(|s| s.ident == "vec");
        if is_vec && self.macro_depth < MAX_MACRO_DEPTH {
            if let Ok(exprs) =
                mac.parse_body_with(Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated)
            {
                let set_id = self.next_id;
                self.next_id += 1;
                self.set_stack.push(set_id);
                self.macro_depth += 1;
                for e in &exprs {
                    self.visit_expr(e);
                }
                self.macro_depth -= 1;
                self.set_stack.pop();
            }
        }
        visit::visit_macro(self, mac);
    }
}

/// Whether a call path is `Invariant::new` (bare or qualified).
fn is_invariant_new(path: &syn::Path) -> bool {
    let n = path.segments.len();
    n >= 2 && path.segments[n - 2].ident == "Invariant" && path.segments[n - 1].ident == "new"
}

/// Extract a string-literal name argument.
fn name_of(expr: &syn::Expr) -> Option<String> {
    if let syn::Expr::Lit(lit) = expr {
        if let syn::Lit::Str(s) = &lit.lit {
            return Some(s.value());
        }
    }
    None
}

/// 1-based line of the predicate expression's start.
fn predicate_line(expr: &syn::Expr) -> usize {
    use syn::spanned::Spanned;
    expr.span().start().line
}

// ── Static weakness detection ────────────────────────────────────────────────

/// Structural weaknesses of a single predicate expression (the 2nd arg to
/// `Invariant::new`). Duplicate is a set property handled separately.
fn predicate_weaknesses(pred: &syn::Expr) -> Vec<Weakness> {
    let mut w = Vec::new();
    if let syn::Expr::Closure(cl) = pred {
        if is_trivially_true(&cl.body) {
            w.push(Weakness::Tautology);
        }
        if closure_ignores_state(cl) {
            w.push(Weakness::IgnoresState);
        }
    }
    w
}

/// Whether an expression is trivially, statically `true` — a predicate that can
/// never be false. Deliberately conservative: only unambiguous shapes.
fn is_trivially_true(expr: &syn::Expr) -> bool {
    match expr {
        // `true`
        syn::Expr::Lit(l) => matches!(&l.lit, syn::Lit::Bool(b) if b.value),
        // `( … )`
        syn::Expr::Paren(p) => is_trivially_true(&p.expr),
        // `{ …; trailing }` — trivially true iff the trailing expr is.
        syn::Expr::Block(b) => block_is_trivially_true(&b.block),
        // `pure || true` / `true || _`. The left operand is evaluated FIRST, so
        // `_ || true` is a tautology only when that left side cannot itself fail —
        // `w.opt.unwrap() > 0 || true` can panic before the `|| true` is reached.
        // NOTE: we deliberately do NOT treat `a == a` as a tautology — without types
        // it is unsound (`NaN != NaN` makes `w.f == w.f` a real not-NaN check, and a
        // custom `PartialEq` may be non-reflexive).
        syn::Expr::Binary(bin) => match bin.op {
            syn::BinOp::Or(_) => {
                is_trivially_true(&bin.left)
                    || (is_trivially_true(&bin.right) && is_pure_operand(&bin.left))
            }
            _ => false,
        },
        _ => false,
    }
}

/// Whether an expression cannot itself fail/panic or observe side effects — no
/// calls, method calls, macros, indexing, `?`, or `.await`. Used to decide whether
/// the left side of `_ || true` is safe (a panic there would make the predicate
/// failable, so `_ || true` would not be a tautology).
fn is_pure_operand(e: &syn::Expr) -> bool {
    struct Impure {
        found: bool,
    }
    impl<'ast> Visit<'ast> for Impure {
        fn visit_expr_call(&mut self, _: &'ast syn::ExprCall) {
            self.found = true;
        }
        fn visit_expr_method_call(&mut self, _: &'ast syn::ExprMethodCall) {
            self.found = true;
        }
        fn visit_expr_macro(&mut self, _: &'ast syn::ExprMacro) {
            self.found = true;
        }
        fn visit_expr_index(&mut self, _: &'ast syn::ExprIndex) {
            self.found = true;
        }
        fn visit_expr_try(&mut self, _: &'ast syn::ExprTry) {
            self.found = true;
        }
        fn visit_expr_await(&mut self, _: &'ast syn::ExprAwait) {
            self.found = true;
        }
    }
    let mut v = Impure { found: false };
    v.visit_expr(e);
    !v.found
}

/// A block is trivially true ONLY when it is a single trailing trivially-true expr
/// (`{ true }`, `{ cond || true }`). Any PRIOR statement is disqualifying: it could
/// early-exit (`return false`, a value-bearing `break`) OR fail by panicking on the
/// state (`assert!(w.ok); true`, `w.opt.unwrap(); true`) — the engine does not catch
/// a predicate panic, so such a block CAN fail and is not a tautology. Requiring a
/// lone trailing expression rules out that whole class conservatively.
fn block_is_trivially_true(block: &syn::Block) -> bool {
    block.stmts.len() == 1
        && matches!(block.stmts.first(), Some(syn::Stmt::Expr(e, None)) if is_trivially_true(e))
}

/// Whether a closure never reads the state parameter it is handed. `|_| …` ignores
/// it definitionally; `|w| …` ignores it if `w` never appears in the body. Any other
/// pattern (tuple/ref/struct destructure) is NOT flagged — we cannot prove it unused,
/// and the safe direction is never to nag about a possibly-real invariant.
fn closure_ignores_state(cl: &syn::ExprClosure) -> bool {
    let Some(first) = cl.inputs.first() else {
        // No parameter at all → cannot read the state.
        return true;
    };
    match strip_type(first) {
        // `_` wildcard: ignores state by construction.
        syn::Pat::Wild(_) => true,
        // Simple binding: ignores state iff the name never appears in the body.
        syn::Pat::Ident(pi) => {
            let name = pi.ident.to_string();
            let mut u = IdentUse { target: &name, found: false };
            u.visit_expr(&cl.body);
            !u.found
        }
        // Anything else (destructuring, ref patterns): can't prove unused → clean.
        _ => false,
    }
}

/// Unwrap a type-ascribed pattern (`w: &W` → `w`) to the inner pattern.
fn strip_type(pat: &syn::Pat) -> &syn::Pat {
    match pat {
        syn::Pat::Type(pt) => strip_type(&pt.pat),
        other => other,
    }
}

/// Visitor that records whether a target identifier is referenced anywhere.
struct IdentUse<'a> {
    target: &'a str,
    found: bool,
}

impl<'ast> Visit<'ast> for IdentUse<'_> {
    fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
        if i == self.target {
            self.found = true;
        }
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // syn does not walk macro token streams, so a state read that lives ONLY
        // inside a macro — `matches!(w.phase, …)`, `check!(w.x >= 0)` — would look
        // like the state is unused. Scan the raw tokens for the target ident.
        if tokens_contain_ident(&mac.tokens, self.target) {
            self.found = true;
        }
        visit::visit_macro(self, mac);
    }
}

/// Whether a token stream mentions `target` anywhere (recursing into groups).
fn tokens_contain_ident(ts: &proc_macro2::TokenStream, target: &str) -> bool {
    ts.clone().into_iter().any(|tt| match tt {
        proc_macro2::TokenTree::Ident(i) => i == target,
        proc_macro2::TokenTree::Group(g) => tokens_contain_ident(&g.stream(), target),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(src: &str) -> ReviewReport {
        let mut report = ReviewReport::default();
        review_source("t.rs", src, &mut report);
        mark_duplicates(&mut report.invariants);
        report
    }

    fn one(src: &str) -> Invariant {
        let r = review(src);
        assert_eq!(r.invariants.len(), 1, "expected one invariant");
        r.invariants.into_iter().next().unwrap()
    }

    #[test]
    fn extracts_name_and_predicate() {
        let inv = one(r#"fn f() { let _ = Invariant::new("under_cap", |w: &W| w.v <= w.cap); }"#);
        assert_eq!(inv.name, "under_cap");
        assert!(inv.predicate.contains("w.v <= w.cap"));
        assert!(inv.weaknesses.is_empty());
    }

    #[test]
    fn flags_literal_true_tautology() {
        let inv = one(r#"fn f() { Invariant::new("always", |_w: &W| true); }"#);
        assert!(inv.weaknesses.contains(&Weakness::Tautology));
    }

    #[test]
    fn self_equality_is_not_treated_as_tautology() {
        // Unsound without types: `NaN != NaN`, and custom PartialEq may be
        // non-reflexive. `|w| w.a == w.a` must NOT be flagged.
        let inv = one(r#"fn f() { Invariant::new("not_nan", |w: &W| w.latency == w.latency); }"#);
        assert!(!inv.weaknesses.contains(&Weakness::Tautology), "self-eq is not a proven tautology");
    }

    #[test]
    fn flags_pure_or_true_tautology() {
        let inv = one(r#"fn f() { Invariant::new("x", |w: &W| w.a > 0 || true); }"#);
        assert!(inv.weaknesses.contains(&Weakness::Tautology));
    }

    #[test]
    fn impure_or_true_is_not_tautology() {
        // The left side can panic before `|| true` is reached → failable.
        let inv = one(r#"fn f() { Invariant::new("x", |w: &W| w.opt.unwrap() > 0 || true); }"#);
        assert!(
            !inv.weaknesses.contains(&Weakness::Tautology),
            "impure left of || true can fail; got {:?}",
            inv.weaknesses
        );
    }

    #[test]
    fn single_true_block_is_tautology() {
        let inv = one(r#"fn f() { Invariant::new("x", |_w: &W| { true }); }"#);
        assert!(inv.weaknesses.contains(&Weakness::Tautology));
    }

    #[test]
    fn block_with_prior_statement_is_not_tautology() {
        // A prior statement can panic on state (`assert!`, `unwrap`) → the predicate
        // CAN fail, so `{ stmt; true }` is not a proven tautology.
        for body in [
            "{ assert!(w.ok); true }",
            "{ w.opt.unwrap(); true }",
            "{ if w.bad { return false; } true }",
        ] {
            let src = format!("fn f() {{ Invariant::new(\"x\", |w: &W| {body}); }}");
            let inv = one(&src);
            assert!(
                !inv.weaknesses.contains(&Weakness::Tautology),
                "`{body}` can fail; not a tautology, got {:?}",
                inv.weaknesses
            );
        }
    }

    #[test]
    fn guard_clause_with_early_return_is_not_tautology() {
        // Ends in `true` but CAN return false via an early return — a real, failable
        // invariant that must NOT be flagged.
        let inv = one(
            r#"fn f() { Invariant::new("cap", |w: &W| { if w.count > w.cap { return false; } true }); }"#,
        );
        assert!(!inv.weaknesses.contains(&Weakness::Tautology), "guard clause is not a tautology");
    }

    #[test]
    fn matches_macro_reading_state_is_not_ignores_state() {
        // Reads state only inside `matches!` — must count as reading state.
        let inv = one(
            r#"fn f() { Invariant::new("phase", |w: &W| matches!(w.phase, Phase::Ok | Phase::Done)); }"#,
        );
        assert!(
            !inv.weaknesses.contains(&Weakness::IgnoresState),
            "matches! on state reads state; got {:?}",
            inv.weaknesses
        );
    }

    #[test]
    fn labelled_break_false_is_not_tautology() {
        // Ends in `true` but can yield `false` via a labelled break → failable.
        let inv = one(
            r#"fn f() { Invariant::new("x", |w: &W| 'c: { if w.bad { break 'c false; } true }); }"#,
        );
        assert!(
            !inv.weaknesses.contains(&Weakness::Tautology),
            "labelled break false is an early exit; got {:?}",
            inv.weaknesses
        );
    }

    #[test]
    fn flags_ignores_state_wildcard() {
        let inv = one(r#"fn f() { Invariant::new("x", |_| SOME_CONST > 0); }"#);
        assert!(inv.weaknesses.contains(&Weakness::IgnoresState));
    }

    #[test]
    fn flags_ignores_state_unused_binding() {
        let inv = one(r#"fn f() { Invariant::new("x", |w: &W| GLOBAL_LIMIT < 100); }"#);
        assert!(inv.weaknesses.contains(&Weakness::IgnoresState));
    }

    #[test]
    fn healthy_invariant_reading_state_is_clean() {
        let inv = one(r#"fn f() { Invariant::new("bal", |w: &W| w.balance >= 0); }"#);
        assert!(inv.weaknesses.is_empty(), "got {:?}", inv.weaknesses);
    }

    #[test]
    fn flags_duplicate_predicates_within_one_set() {
        // Two identical predicates in the SAME vec! set → the second is a duplicate.
        let src = r#"
            fn f() {
                let eng = InvariantEngine::new(vec![
                    Invariant::new("one", |w: &W| w.balance >= 0),
                    Invariant::new("two", |w: &W| w.balance >= 0),
                ]);
            }
        "#;
        let r = review(src);
        assert_eq!(r.invariants.len(), 2);
        assert!(!r.invariants[0].weaknesses.contains(&Weakness::Duplicate));
        assert!(r.invariants[1].weaknesses.contains(&Weakness::Duplicate));
    }

    #[test]
    fn same_predicate_in_two_different_sets_is_not_duplicate() {
        // Two independent engines in the SAME fn, each asserting the same property,
        // are NOT duplicates of each other — dedup is per-set, not per-fn.
        let src = r#"
            fn f() {
                let e1 = InvariantEngine::new(vec![Invariant::new("a", |w: &W| w.balance >= 0)]);
                let e2 = InvariantEngine::new(vec![Invariant::new("b", |w: &W| w.balance >= 0)]);
            }
        "#;
        let r = review(src);
        assert_eq!(r.invariants.len(), 2);
        assert!(r.invariants.iter().all(|i| !i.weaknesses.contains(&Weakness::Duplicate)));
    }

    #[test]
    fn fake_invariant_in_stringify_macro_is_ignored() {
        // A token-data macro that merely MENTIONS Invariant::new constructs nothing;
        // it must not be extracted (no fake findings).
        let src = r#"fn f() { let _ = stringify!(Invariant::new("x", |_| true)); }"#;
        let r = review(src);
        assert!(r.invariants.is_empty(), "stringify! must not yield invariants");
    }

    #[test]
    fn same_predicate_in_different_functions_is_not_duplicate() {
        // Two independent tests each legitimately asserting the same property are
        // NOT duplicates — duplicate detection is scoped to one invariant set.
        let src = r#"
            fn test_a() { let _ = Invariant::new("bal", |w: &W| w.balance >= 0); }
            fn test_b() { let _ = Invariant::new("bal", |w: &W| w.balance >= 0); }
        "#;
        let r = review(src);
        assert_eq!(r.invariants.len(), 2);
        assert!(r.invariants.iter().all(|i| !i.weaknesses.contains(&Weakness::Duplicate)),
            "cross-function repeats are not duplicates");
    }

    #[test]
    fn non_literal_name_is_recorded() {
        let inv = one(r#"fn f() { Invariant::new(NAME, |w: &W| w.x > 0); }"#);
        assert_eq!(inv.name, "<non-literal>");
    }

    #[test]
    fn invariants_inside_vec_macro_are_found() {
        // The overwhelmingly common shape: invariants live inside
        // `InvariantEngine::new(vec![Invariant::new(...), ...])` — a macro syn does
        // not descend into without the re-parse.
        let src = r#"
            fn build() {
                let eng = InvariantEngine::new(vec![
                    Invariant::new("a", |w: &W| w.x >= 0),
                    Invariant::new("b", |_w: &W| true),
                ]);
            }
        "#;
        let r = review(src);
        assert_eq!(r.invariants.len(), 2, "both invariants inside vec! must be found");
        assert!(r.invariants.iter().any(|i| i.name == "b" && i.weaknesses.contains(&Weakness::Tautology)));
    }

    #[test]
    fn qualified_invariant_new_is_found() {
        let inv = one(r#"fn f() { navian_dst::Invariant::new("x", |w: &W| w.a >= 0); }"#);
        assert_eq!(inv.name, "x");
    }

    #[test]
    fn local_invariant_type_shadows_only_bare_calls() {
        // A file that defines its own `Invariant` type: the BARE call is the user's
        // own (skipped), but a qualified `navian_dst::Invariant::new` is still real.
        let src = r#"
            struct Invariant;
            impl Invariant { fn new(_a: &str, _b: u8) {} }
            fn f() {
                Invariant::new("mine", 3);
                navian_dst::Invariant::new("real", |w: &W| w.balance >= 0);
            }
        "#;
        let r = review(src);
        assert_eq!(r.invariants.len(), 1, "only the qualified real one is reviewed");
        assert_eq!(r.invariants[0].name, "real");
    }

    #[test]
    fn flagged_counts_weakened_invariants() {
        let src = r#"
            fn f() {
                Invariant::new("ok", |w: &W| w.a >= 0);
                Invariant::new("bad", |_| true);
            }
        "#;
        let r = review(src);
        assert_eq!(r.flagged(), 1);
    }
}
