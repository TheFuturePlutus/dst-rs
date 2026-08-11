//! Determinism-leak *migrator* — Phase 3b.
//!
//! Where [`crate::scanner`] only *reports* leaks, `migrate` *rewrites* a
//! conservative, seam-safe subset of them so the crate keeps compiling and
//! becomes injectable with a simulated clock.
//!
//! ## What v1 handles (and only this)
//!
//! **Time** leaks that occur inside an *inherent* method of a **named-field
//! struct** (`impl Foo { fn m(&self) { … } }`). For each such struct `Foo`
//! that contains at least one *rewritable* time leak:
//!
//! 1. A field `time: std::sync::Arc<dyn dst_rs::Time>` is added to the struct.
//! 2. Every struct-literal site `Foo { … }` / `Self { … }` in the migrated
//!    path defaults the field to `Arc::new(dst_rs::ProductionTime::default())`
//!    — so every existing `Foo::new()` caller is unchanged (seam-safety).
//! 3. The leak call sites are rewritten:
//!    - `Instant::now()`               → `self.time.instant_now()`
//!    - `tokio::time::sleep(d).await`  → `self.time.sleep(d).await`
//!    - `SystemTime::now()…as_millis()`→ `self.time.now_ms()`
//!
//! Everything else — leaks in free functions, closures, trait-impl methods
//! (fixed signatures), tuple/unit structs, `std::thread::sleep`, `chrono`,
//! bare `SystemTime::now()` outside the millis idiom, and every non-Time
//! category — is **left untouched and reported** as a manual/agent task.
//!
//! ## Why this is safe by construction
//!
//! Constructors default the new field to the *real* production clock, so the
//! rewrite is invisible to callers. If any rewrite nonetheless fails to
//! type-check, the caller (`main`) restores the original bytes from an
//! in-memory snapshot and reports the failure — the tree is never left broken.
//!
//! ## Offsets
//!
//! All edits are computed as byte ranges via [`proc_macro2::Span::byte_range`]
//! (valid because we parse from an in-memory `&str` with the `span-locations`
//! feature) and applied to the original text sorted last-edit-first so earlier
//! offsets stay valid.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use walkdir::WalkDir;

// ── Public option / result surface ──────────────────────────────────────────

/// Which non-determinism trait families to migrate. v1: Time only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraitFamily {
    Time,
}

/// Options controlling a migrate run.
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    pub dry_run: bool,
    pub traits: Vec<TraitFamily>,
}

/// A single leak that was deliberately NOT rewritten, with a human reason.
#[derive(Debug, Clone)]
pub struct Skip {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub snippet: String,
    pub reason: String,
}

/// Per-file plan: original bytes, rewritten bytes, and a unified diff.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub original: String,
    pub modified: String,
    pub diff: String,
}

/// Outcome of `cargo check` after a (non-dry-run) apply.
#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// Not run (dry-run, or no files changed).
    Skipped,
    Passed,
    /// `cargo check` failed; files were restored. Carries captured stderr.
    Failed(String),
}

/// Aggregate result of a migrate run.
#[derive(Debug, Clone)]
pub struct MigrateResult {
    /// Files that changed (empty when nothing to do).
    pub changes: Vec<FileChange>,
    /// Struct names that were migrated (field added / leaks rewritten).
    pub structs_migrated: BTreeSet<String>,
    /// Number of individual leak call-sites rewritten.
    pub leaks_rewritten: usize,
    /// Leaks intentionally skipped, with reasons.
    pub skips: Vec<Skip>,
    /// Files found but unparseable (skipped, never fatal).
    pub parse_failures: Vec<String>,
    pub check: CheckOutcome,
    /// True if the migrator determined the tree was already migrated / had
    /// nothing rewritable (idempotent no-op).
    pub no_op: bool,
}

// ── Entry point ─────────────────────────────────────────────────────────────

/// Plan and (unless `dry_run`) apply a migration over `root`.
///
/// `root` may be a single `.rs` file or a directory; only structs *defined*
/// under `root` are considered (scoped migration).
pub fn migrate_path(root: &Path, opts: &MigrateOptions) -> MigrateResult {
    // v1 only knows Time. Anything else is a caller error handled in main; here
    // we simply act on Time.
    debug_assert!(opts.traits.contains(&TraitFamily::Time));

    let files = collect_rs_files(root);
    let mut sources: Vec<(PathBuf, String)> = Vec::new();
    let mut read_failures = Vec::new();
    for path in files {
        match std::fs::read_to_string(&path) {
            Ok(s) => sources.push((path, s)),
            Err(_) => read_failures.push(path.display().to_string()),
        }
    }

    let mut result = plan_sources(&sources);
    result.parse_failures.extend(read_failures);

    // ── Apply / check / restore (only when not a dry run). ──
    result.check = if opts.dry_run || result.changes.is_empty() {
        CheckOutcome::Skipped
    } else {
        apply_and_check(root, &result.changes)
    };

    result
}

/// Pure planning core: given `(path, source)` pairs, compute the full set of
/// changes, migrated structs, and skips WITHOUT touching the filesystem.
/// Exposed for unit testing; `migrate_path` wraps it with IO + `cargo check`.
pub fn plan_sources(sources: &[(PathBuf, String)]) -> MigrateResult {
    let mut parse_failures = Vec::new();

    struct Parsed {
        path: PathBuf,
        display: String,
        source: String,
        ast: syn::File,
    }
    let mut parsed: Vec<Parsed> = Vec::new();
    for (path, source) in sources {
        let display = path.display().to_string();
        match syn::parse_file(source) {
            Ok(ast) => parsed.push(Parsed {
                path: path.clone(),
                display,
                source: source.clone(),
                ast,
            }),
            Err(_) => parse_failures.push(display),
        }
    }

    // ── Pass 1: collect every struct definition across the scoped files. ──
    let mut struct_defs: BTreeMap<String, StructDef> = BTreeMap::new();
    for p in &parsed {
        let mut c = StructCollector {
            file: &p.display,
            defs: &mut struct_defs,
        };
        c.visit_file(&p.ast);
    }

    // ── Pass 2: per-file transform walk — collect rewrites, literals, skips. ──
    struct FilePlan {
        idx: usize,
        rewrites: Vec<Rewrite>,
        literals: Vec<StructLiteral>,
        skips: Vec<Skip>,
    }
    let mut plans: Vec<FilePlan> = Vec::new();
    for (idx, p) in parsed.iter().enumerate() {
        let mut t = TransformVisitor {
            file: &p.display,
            source: &p.source,
            struct_defs: &struct_defs,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            closure_depth: 0,
            consumed: Vec::new(),
            rewrites: Vec::new(),
            literals: Vec::new(),
            skips: Vec::new(),
        };
        t.visit_file(&p.ast);
        plans.push(FilePlan {
            idx,
            rewrites: t.rewrites,
            literals: t.literals,
            skips: t.skips,
        });
    }

    // Structs that earned migration: they have >=1 rewritable leak in an
    // inherent method (rewrites are only ever recorded for such structs).
    let mut structs_migrated: BTreeSet<String> = BTreeSet::new();
    for plan in &plans {
        for r in &plan.rewrites {
            structs_migrated.insert(r.struct_name.clone());
        }
    }

    // ── Pass 3: turn the plan into concrete byte edits per file & apply. ──
    let mut changes: Vec<FileChange> = Vec::new();
    let mut leaks_rewritten = 0usize;
    let mut all_skips: Vec<Skip> = Vec::new();

    for plan in &plans {
        let p = &parsed[plan.idx];
        let mut edits: Vec<Edit> = Vec::new();

        // (a) Add the `time` field to migrated structs defined in THIS file
        //     (unless the struct already carries it — idempotency).
        for (name, def) in &struct_defs {
            if def.file != p.display {
                continue;
            }
            if !structs_migrated.contains(name) {
                continue;
            }
            if def.has_time {
                continue; // already migrated: don't re-add the field.
            }
            // Only named-field structs reach `structs_migrated` (see the
            // TransformVisitor's named-field gate), so brace_open is Some.
            if let Some(open_end) = def.field_brace_open_end {
                edits.push(Edit {
                    start: open_end,
                    end: open_end,
                    text: format!("\n    {FIELD_DECL},"),
                });
            }
        }

        // (b) Default the field in every struct-literal of a migrated struct
        //     that does not already set it.
        for lit in &plan.literals {
            if !structs_migrated.contains(&lit.struct_name) {
                continue;
            }
            if lit.already_has_time {
                continue;
            }
            edits.push(Edit {
                start: lit.brace_open_end,
                end: lit.brace_open_end,
                text: format!(" {FIELD_DEFAULT},"),
            });
        }

        // (c) Rewrite the leak call sites.
        for r in &plan.rewrites {
            leaks_rewritten += 1;
            edits.push(Edit {
                start: r.start,
                end: r.end,
                text: r.replacement.clone(),
            });
        }

        // Skips are reported regardless of whether the file changed.
        all_skips.extend(plan.skips.iter().cloned());

        if edits.is_empty() {
            continue;
        }
        let modified = apply_edits(&p.source, edits);
        let diff = unified_diff(&p.display, &p.source, &modified);
        changes.push(FileChange {
            path: p.path.clone(),
            original: p.source.clone(),
            modified,
            diff,
        });
    }

    let no_op = changes.is_empty();

    MigrateResult {
        changes,
        structs_migrated,
        leaks_rewritten,
        skips: all_skips,
        parse_failures,
        check: CheckOutcome::Skipped,
        no_op,
    }
}

const FIELD_DECL: &str = "time: std::sync::Arc<dyn dst_rs::Time>";
const FIELD_DEFAULT: &str = "time: std::sync::Arc::new(dst_rs::ProductionTime::default())";

// ── Write / cargo-check / restore ───────────────────────────────────────────

/// Write every change, run `cargo check` on the owning crate, and restore the
/// originals from the in-memory snapshot if the check fails.
fn apply_and_check(root: &Path, changes: &[FileChange]) -> CheckOutcome {
    // Write modified bytes.
    for c in changes {
        if let Err(e) = std::fs::write(&c.path, &c.modified) {
            // Best-effort restore of whatever we already wrote, then report.
            restore(changes);
            return CheckOutcome::Failed(format!("failed to write {}: {e}", c.path.display()));
        }
    }

    let manifest =
        find_manifest(root).or_else(|| changes.first().and_then(|c| find_manifest(&c.path)));

    let output = {
        let mut cmd = std::process::Command::new("cargo");
        cmd.arg("check");
        if let Some(m) = &manifest {
            cmd.arg("--manifest-path").arg(m);
        }
        cmd.output()
    };

    match output {
        Ok(out) if out.status.success() => CheckOutcome::Passed,
        Ok(out) => {
            restore(changes);
            CheckOutcome::Failed(String::from_utf8_lossy(&out.stderr).into_owned())
        }
        Err(e) => {
            restore(changes);
            CheckOutcome::Failed(format!("could not run `cargo check`: {e}"))
        }
    }
}

fn restore(changes: &[FileChange]) {
    for c in changes {
        let _ = std::fs::write(&c.path, &c.original);
    }
}

/// Nearest ancestor directory containing a `Cargo.toml`, starting at `start`
/// (or its parent if `start` is a file).
fn find_manifest(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ── File collection ─────────────────────────────────────────────────────────

fn collect_rs_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if root.is_file() {
        if root.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(root.to_path_buf());
        }
        return out;
    }
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
    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|e| e.to_str()) == Some("rs")
        {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    out
}

// ── Struct definitions (pass 1) ─────────────────────────────────────────────

#[derive(Debug, Clone)]
struct StructDef {
    file: String,
    /// Byte offset just past the opening `{` of the named-field brace, or
    /// `None` for tuple/unit structs (which cannot receive a named field).
    field_brace_open_end: Option<usize>,
    /// The struct already has a field literally named `time` (idempotency).
    has_time: bool,
}

struct StructCollector<'a> {
    file: &'a str,
    defs: &'a mut BTreeMap<String, StructDef>,
}

impl<'ast> Visit<'ast> for StructCollector<'_> {
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        let name = node.ident.to_string();
        let (brace_open_end, has_time) = match &node.fields {
            syn::Fields::Named(named) => {
                let open_end = named.brace_token.span.open().byte_range().end;
                let has_time = named
                    .named
                    .iter()
                    .any(|f| f.ident.as_ref().map(|i| i == "time").unwrap_or(false));
                (Some(open_end), has_time)
            }
            _ => (None, false),
        };
        // First definition wins (there should only be one per name in scope).
        self.defs.entry(name).or_insert(StructDef {
            file: self.file.to_string(),
            field_brace_open_end: brace_open_end,
            has_time,
        });
        visit::visit_item_struct(self, node);
    }
}

// ── Transform walk (pass 2) ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Rewrite {
    struct_name: String,
    start: usize,
    end: usize,
    replacement: String,
}

#[derive(Debug, Clone)]
struct StructLiteral {
    struct_name: String,
    /// Byte offset just past the literal's opening `{`.
    brace_open_end: usize,
    already_has_time: bool,
}

/// What kind of enclosing function a leak sits in.
#[derive(Debug, Clone)]
enum FnCtx {
    /// Inherent method `impl Foo { fn … }`. The bool is whether the fn has a
    /// `self` receiver — an associated fn without `self` cannot use `self.time`.
    Inherent { struct_name: String, has_self: bool },
    /// Trait-impl method `impl Trait for Foo { fn … }`.
    Trait,
    /// Free function or anything else.
    Free,
}

#[derive(Debug, Clone)]
struct ImplCtx {
    self_ty: Option<String>,
    is_trait: bool,
}

struct TransformVisitor<'a> {
    file: &'a str,
    source: &'a str,
    struct_defs: &'a BTreeMap<String, StructDef>,
    fn_stack: Vec<FnCtx>,
    impl_stack: Vec<ImplCtx>,
    closure_depth: usize,
    /// Byte ranges already consumed by a `SystemTime::now()…as_millis()`
    /// match, so the inner `SystemTime::now()` call is not double-reported.
    consumed: Vec<(usize, usize)>,
    rewrites: Vec<Rewrite>,
    literals: Vec<StructLiteral>,
    skips: Vec<Skip>,
}

impl TransformVisitor<'_> {
    fn line_col(&self, byte: usize) -> (usize, usize) {
        // 1-based line, 1-based col from a byte offset into `source`.
        let mut line = 1usize;
        let mut col = 1usize;
        for (i, ch) in self.source.char_indices() {
            if i >= byte {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    fn snippet(&self, start: usize, end: usize) -> String {
        let raw: String = self.source[start..end.min(self.source.len())]
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if raw.chars().count() > 80 {
            let short: String = raw.chars().take(79).collect();
            format!("{short}…")
        } else {
            raw
        }
    }

    fn push_skip(&mut self, start: usize, end: usize, reason: &str) {
        let (line, col) = self.line_col(start);
        self.skips.push(Skip {
            file: self.file.to_string(),
            line,
            col,
            snippet: self.snippet(start, end),
            reason: reason.to_string(),
        });
    }

    fn is_consumed(&self, start: usize, end: usize) -> bool {
        self.consumed.iter().any(|(s, e)| start >= *s && end <= *e)
    }

    /// Decide how to handle a *rewritable-shaped* time leak given the current
    /// lexical context. Returns the migrated struct name if it should be
    /// rewritten, else records a skip and returns None.
    fn resolve_rewrite_target(&mut self, start: usize, end: usize) -> Option<String> {
        if self.closure_depth > 0 {
            self.push_skip(start, end, "inside a closure — not rewritten (v1)");
            return None;
        }
        match self.fn_stack.last() {
            Some(FnCtx::Inherent {
                struct_name,
                has_self,
            }) => {
                if !has_self {
                    self.push_skip(
                        start,
                        end,
                        "associated fn without a `self` receiver — cannot use self.time — map manually",
                    );
                    return None;
                }
                let name = struct_name.clone();
                match self.struct_defs.get(&name) {
                    Some(def) if def.field_brace_open_end.is_some() => Some(name),
                    Some(_) => {
                        self.push_skip(
                            start,
                            end,
                            "enclosing struct is not a named-field struct — cannot inject `time`",
                        );
                        None
                    }
                    None => {
                        self.push_skip(
                            start,
                            end,
                            "enclosing struct is not defined in the scanned path — out of scope",
                        );
                        None
                    }
                }
            }
            Some(FnCtx::Trait) => {
                self.push_skip(
                    start,
                    end,
                    "trait-impl method (fixed signature) — needs manual/agent handling",
                );
                None
            }
            Some(FnCtx::Free) | None => {
                self.push_skip(
                    start,
                    end,
                    "not inside a struct method (free fn) — needs manual/agent handling",
                );
                None
            }
        }
    }
}

/// The path segments `a::b::c` of a call target, as strings.
fn path_segs(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

/// Follow a `.as_millis()` receiver chain down to a root `SystemTime::now()`
/// call. Returns that call's span if the chain roots there.
fn system_now_root(expr: &syn::Expr) -> Option<proc_macro2::Span> {
    match expr {
        syn::Expr::Call(call) => {
            if let syn::Expr::Path(p) = &*call.func {
                let segs = path_segs(&p.path);
                let n = segs.len();
                if n >= 2 && segs[n - 2] == "SystemTime" && segs[n - 1] == "now" {
                    return Some(call.span());
                }
            }
            None
        }
        syn::Expr::MethodCall(m) => system_now_root(&m.receiver),
        syn::Expr::Try(t) => system_now_root(&t.expr),
        syn::Expr::Paren(p) => system_now_root(&p.expr),
        syn::Expr::Unary(u) => system_now_root(&u.expr),
        _ => None,
    }
}

impl<'ast> Visit<'ast> for TransformVisitor<'_> {
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let self_ty = match &*node.self_ty {
            syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        };
        self.impl_stack.push(ImplCtx {
            self_ty,
            is_trait: node.trait_.is_some(),
        });
        visit::visit_item_impl(self, node);
        self.impl_stack.pop();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.fn_stack.push(FnCtx::Free);
        visit::visit_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let has_self = matches!(node.sig.inputs.first(), Some(syn::FnArg::Receiver(_)));
        let ctx = match self.impl_stack.last() {
            Some(ImplCtx {
                self_ty: Some(name),
                is_trait: false,
            }) => FnCtx::Inherent {
                struct_name: name.clone(),
                has_self,
            },
            Some(ImplCtx { is_trait: true, .. }) => FnCtx::Trait,
            _ => FnCtx::Free,
        };
        self.fn_stack.push(ctx);
        visit::visit_impl_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        // Method in a `trait` definition (default body). Fixed signature.
        self.fn_stack.push(FnCtx::Trait);
        visit::visit_trait_item_fn(self, node);
        self.fn_stack.pop();
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.closure_depth += 1;
        visit::visit_expr_closure(self, node);
        self.closure_depth -= 1;
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        // Resolve the literal's struct name (`Self` → enclosing impl's type).
        let last = node.path.segments.last().map(|s| s.ident.to_string());
        let name = match last.as_deref() {
            Some("Self") => self.impl_stack.last().and_then(|c| c.self_ty.clone()),
            Some(other) => Some(other.to_string()),
            None => None,
        };
        if let Some(name) = name {
            if self.struct_defs.contains_key(&name) {
                let already_has_time = node.fields.iter().any(|f| match &f.member {
                    syn::Member::Named(i) => i == "time",
                    syn::Member::Unnamed(_) => false,
                });
                let brace_open_end = node.brace_token.span.open().byte_range().end;
                self.literals.push(StructLiteral {
                    struct_name: name,
                    brace_open_end,
                    already_has_time,
                });
            }
        }
        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        // SystemTime::now()…as_millis()  →  self.time.now_ms()
        if node.method == "as_millis" {
            if let Some(now_span) = system_now_root(&node.receiver) {
                let whole = node.span().byte_range();
                let nr = now_span.byte_range();
                // Mark the inner now() call consumed so it isn't reported as a
                // bare leak when we recurse into it.
                self.consumed.push((nr.start, nr.end));
                if let Some(target) = self.resolve_rewrite_target(whole.start, whole.end) {
                    self.rewrites.push(Rewrite {
                        struct_name: target,
                        start: whole.start,
                        end: whole.end,
                        replacement: "self.time.now_ms()".to_string(),
                    });
                }
            }
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            let segs = path_segs(&p.path);
            let n = segs.len();
            let last = segs.last().map(String::as_str).unwrap_or("");
            let prev = if n >= 2 { segs[n - 2].as_str() } else { "" };

            let func_span = node.func.span().byte_range();
            let whole_span = node.span().byte_range();

            // Instant::now()  →  self.time.instant_now()   (replace callee only)
            if prev == "Instant" && last == "now" {
                if let Some(target) = self.resolve_rewrite_target(whole_span.start, whole_span.end)
                {
                    self.rewrites.push(Rewrite {
                        struct_name: target,
                        start: func_span.start,
                        end: func_span.end,
                        replacement: "self.time.instant_now".to_string(),
                    });
                }
            }
            // tokio::time::sleep(d)  →  self.time.sleep(d)   (replace callee only)
            else if prev == "time" && last == "sleep" {
                if let Some(target) = self.resolve_rewrite_target(whole_span.start, whole_span.end)
                {
                    self.rewrites.push(Rewrite {
                        struct_name: target,
                        start: func_span.start,
                        end: func_span.end,
                        replacement: "self.time.sleep".to_string(),
                    });
                }
            }
            // SystemTime::now() NOT already consumed by an as_millis match.
            else if prev == "SystemTime"
                && last == "now"
                && !self.is_consumed(whole_span.start, whole_span.end)
            {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "bare SystemTime::now() (not a recognizable millis idiom) — map manually",
                );
            }
            // std::thread::sleep — blocking; Time::sleep is async.
            else if prev == "thread" && last == "sleep" {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "std::thread::sleep is blocking; Time::sleep is async — map manually",
                );
            }
            // chrono Utc::now / Local::now — returns DateTime, not a Time method.
            else if (prev == "Utc" || prev == "Local") && last == "now" {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "chrono time (returns DateTime) — no direct Time trait mapping — map manually",
                );
            }
            // Other tokio::time::* with no direct Time equivalent.
            else if prev == "time" && matches!(last, "sleep_until" | "interval" | "timeout") {
                self.push_skip(
                    whole_span.start,
                    whole_span.end,
                    "tokio::time::* with no direct Time trait equivalent — map manually",
                );
            }
        }
        visit::visit_expr_call(self, node);
    }
}

// ── Edit application ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Apply byte-range edits to `source`. Edits are applied last-first so earlier
/// offsets remain valid; overlapping edits are dropped defensively.
fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    // Sort by start descending; on a tie, longer/earlier-inserted first.
    edits.sort_by(|a, b| b.start.cmp(&a.start).then(b.end.cmp(&a.end)));
    let mut out = source.to_string();
    let mut last_start = usize::MAX;
    for e in edits {
        // Defensive: skip an edit that overlaps one we already applied.
        if e.end > last_start {
            continue;
        }
        out.replace_range(e.start..e.end, &e.text);
        last_start = e.start;
    }
    out
}

// ── Unified diff (dependency-free) ──────────────────────────────────────────

#[derive(Clone, Copy)]
enum DiffOp {
    /// (index into a, index into b)
    Equal(usize, usize),
    /// index into a (deleted line)
    Del(usize),
    /// index into b (inserted line)
    Ins(usize),
}

impl DiffOp {
    fn is_equal(&self) -> bool {
        matches!(self, DiffOp::Equal(_, _))
    }
}

/// Produce a git-style unified diff between `old` and `new` for `path`.
/// Empty string when the two are identical.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = diff_ops(&a, &b);

    if ops.iter().all(DiffOp::is_equal) {
        return String::new();
    }

    const CTX: usize = 3;

    // Split the op stream into hunks: maximal windows of change surrounded by
    // up to CTX context lines, merging changes separated by <= 2*CTX equals.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if ops[i].is_equal() {
            i += 1;
            continue;
        }
        // Start a hunk with CTX lines of leading context.
        let start = i.saturating_sub(CTX);
        // Extend across changes and short equal-runs.
        let mut end = i;
        while end < ops.len() {
            if ops[end].is_equal() {
                // Count the equal run.
                let run_start = end;
                while end < ops.len() && ops[end].is_equal() {
                    end += 1;
                }
                let run_len = end - run_start;
                let more_changes = end < ops.len();
                if !more_changes || run_len > 2 * CTX {
                    // Close the hunk with up to CTX trailing context.
                    end = run_start + run_len.min(CTX);
                    break;
                }
                // else: absorb the short equal run and keep extending.
            } else {
                end += 1;
            }
        }
        hunks.push((start, end.min(ops.len())));
        i = end;
    }

    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for (s, e) in hunks {
        emit_hunk(&mut out, &ops[s..e], &a, &b);
    }
    out
}

/// Classic LCS diff over line indices. Adequate for the small files migrate
/// touches; deterministic output.
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<DiffOp> {
    let n = a.len();
    let m = b.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(DiffOp::Equal(i, j));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push(DiffOp::Del(i));
            i += 1;
        } else {
            ops.push(DiffOp::Ins(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffOp::Del(i));
        i += 1;
    }
    while j < m {
        ops.push(DiffOp::Ins(j));
        j += 1;
    }
    ops
}

fn emit_hunk(out: &mut String, ops: &[DiffOp], a: &[&str], b: &[&str]) {
    let mut a_start = None;
    let mut b_start = None;
    let mut a_count = 0;
    let mut b_count = 0;
    for op in ops {
        match op {
            DiffOp::Equal(ia, ib) => {
                a_start.get_or_insert(*ia);
                b_start.get_or_insert(*ib);
                a_count += 1;
                b_count += 1;
            }
            DiffOp::Del(ia) => {
                a_start.get_or_insert(*ia);
                a_count += 1;
            }
            DiffOp::Ins(ib) => {
                b_start.get_or_insert(*ib);
                b_count += 1;
            }
        }
    }
    let a0 = a_start.map(|s| s + 1).unwrap_or(0);
    let b0 = b_start.map(|s| s + 1).unwrap_or(0);
    out.push_str(&format!("@@ -{a0},{a_count} +{b0},{b_count} @@\n"));
    for op in ops {
        match op {
            DiffOp::Equal(ia, _) => {
                out.push(' ');
                out.push_str(a[*ia]);
                out.push('\n');
            }
            DiffOp::Del(ia) => {
                out.push('-');
                out.push_str(a[*ia]);
                out.push('\n');
            }
            DiffOp::Ins(ib) => {
                out.push('+');
                out.push_str(b[*ib]);
                out.push('\n');
            }
        }
    }
}

// ── Unit tests (pure, in-memory — no filesystem, no cargo) ───────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(src: &str) -> MigrateResult {
        let sources = vec![(PathBuf::from("lib.rs"), src.to_string())];
        plan_sources(&sources)
    }

    fn modified(src: &str) -> String {
        let r = plan(src);
        r.changes
            .first()
            .map(|c| c.modified.clone())
            .unwrap_or_else(|| src.to_string())
    }

    #[test]
    fn rewrites_instant_now_in_inherent_method() {
        let src = r#"
struct Foo { n: u64 }
impl Foo {
    fn new() -> Self { Foo { n: 0 } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        // Field added to the struct.
        assert!(
            out.contains("time: std::sync::Arc<dyn dst_rs::Time>"),
            "got:\n{out}"
        );
        // Constructor literal defaulted.
        assert!(
            out.contains("time: std::sync::Arc::new(dst_rs::ProductionTime::default())"),
            "got:\n{out}"
        );
    }

    #[test]
    fn rewrites_tokio_sleep_preserving_await_and_args() {
        let src = r#"
struct Foo;
impl Foo {
    async fn wait(&self, d: std::time::Duration) {
        tokio::time::sleep(d).await;
    }
}
"#;
        // Foo is a unit struct -> cannot inject a named field, so this must be
        // SKIPPED, not rewritten.
        let r = plan(src);
        assert!(r.changes.is_empty(), "unit struct must not be migrated");
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("not a named-field struct"));
    }

    #[test]
    fn rewrites_tokio_sleep_in_named_struct() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    async fn wait(&self, d: std::time::Duration) {
        tokio::time::sleep(d).await;
    }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.sleep(d).await"), "got:\n{out}");
    }

    #[test]
    fn rewrites_systemtime_millis_idiom() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn now(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }
}
"#;
        let out = modified(src);
        assert!(out.contains("self.time.now_ms() as i64"), "got:\n{out}");
        assert!(
            !out.contains("SystemTime::now"),
            "old call still present:\n{out}"
        );
        // Exactly one leak rewritten (the whole chain), not two.
        let r = plan(src);
        assert_eq!(r.leaks_rewritten, 1);
        assert!(r.skips.is_empty(), "unexpected skips: {:?}", r.skips);
    }

    #[test]
    fn skips_free_function_leak() {
        let src = r#"
fn stamp() -> std::time::Instant { Instant::now() }
"#;
        let r = plan(src);
        assert!(r.changes.is_empty());
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("free fn"));
    }

    #[test]
    fn skips_trait_impl_method_leak() {
        let src = r#"
struct Foo { x: u8 }
trait Ticker { fn tick(&self) -> std::time::Instant; }
impl Ticker for Foo {
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let r = plan(src);
        assert!(r.changes.is_empty(), "trait-impl leak must not migrate");
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("trait-impl"));
    }

    #[test]
    fn skips_closure_leak() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn go(&self) {
        let f = || Instant::now();
        let _ = f();
    }
}
"#;
        let r = plan(src);
        assert!(r.changes.is_empty(), "closure leak must not migrate");
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("closure"));
    }

    #[test]
    fn skips_thread_sleep_and_chrono() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn nap(&self) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    fn wall(&self) -> i64 { chrono::Utc::now().timestamp() }
}
"#;
        let r = plan(src);
        // Neither maps cleanly -> both skipped, nothing migrated.
        assert!(r.changes.is_empty());
        assert_eq!(r.skips.len(), 2);
        assert!(r.skips.iter().any(|s| s.reason.contains("blocking")));
        assert!(r.skips.iter().any(|s| s.reason.contains("chrono")));
    }

    #[test]
    fn idempotent_when_time_field_present() {
        // A struct that already carries `time` and whose leaks are already
        // rewritten must be a no-op.
        let src = r#"
struct Foo {
    time: std::sync::Arc<dyn dst_rs::Time>,
    x: u8,
}
impl Foo {
    fn new() -> Self {
        Self { time: std::sync::Arc::new(dst_rs::ProductionTime::default()), x: 0 }
    }
    fn now(&self) -> i64 { self.time.now_ms() }
}
"#;
        let r = plan(src);
        assert!(r.no_op, "already-migrated code must be a no-op");
        assert!(r.changes.is_empty());
        assert!(r.skips.is_empty());
    }

    #[test]
    fn second_run_is_noop_end_to_end() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn tick(&self) -> std::time::Instant { Instant::now() }
}
"#;
        let once = modified(src);
        // Feed the migrated output back in — must not change again.
        let twice = modified(&once);
        assert_eq!(once, twice, "migrate is not idempotent");
        let r2 = plan(&once);
        assert!(r2.no_op, "second run should be a no-op");
    }

    #[test]
    fn skips_associated_fn_without_self() {
        // A constructor that reads the clock has no `self`, so it cannot be
        // rewritten to `self.time…`; it must be skipped, not broken.
        let src = r#"
struct Foo { last: i64 }
impl Foo {
    fn new() -> Self {
        Self { last: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64 }
    }
}
"#;
        let r = plan(src);
        assert!(r.changes.is_empty(), "must not rewrite a self-less fn");
        assert_eq!(r.skips.len(), 1);
        assert!(
            r.skips[0].reason.contains("without a `self` receiver"),
            "reason: {}",
            r.skips[0].reason
        );
    }

    #[test]
    fn bare_system_now_is_skipped_not_rewritten() {
        let src = r#"
struct Foo { x: u8 }
impl Foo {
    fn new() -> Self { Self { x: 0 } }
    fn stamp(&self) -> std::time::SystemTime { SystemTime::now() }
}
"#;
        let r = plan(src);
        assert!(
            r.changes.is_empty(),
            "bare SystemTime::now must not migrate"
        );
        assert_eq!(r.skips.len(), 1);
        assert!(r.skips[0].reason.contains("bare SystemTime::now"));
    }

    #[test]
    fn unified_diff_roundtrips() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let d = unified_diff("x.rs", old, new);
        assert!(d.contains("--- a/x.rs"));
        assert!(d.contains("+++ b/x.rs"));
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
        // Identical inputs -> empty diff.
        assert!(unified_diff("x.rs", old, old).is_empty());
    }

    #[test]
    fn byte_offsets_are_correct_for_multibyte_source() {
        // A non-ASCII comment before the leak shifts byte offsets vs char
        // offsets; the rewrite must still land exactly on the call.
        let src = "struct Foo { x: u8 }\n// café ☕ comment\nimpl Foo {\n    fn new() -> Self { Self { x: 0 } }\n    fn t(&self) -> std::time::Instant { Instant::now() }\n}\n";
        let out = modified(src);
        assert!(out.contains("self.time.instant_now()"), "got:\n{out}");
        assert!(!out.contains("Instant::now()"), "stale call:\n{out}");
        // The café comment must be preserved intact.
        assert!(out.contains("// café ☕ comment"), "corrupted:\n{out}");
    }
}
