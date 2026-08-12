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

use std::collections::HashMap;
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
        scopes: Vec::new(),
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
    // Fully-qualified `rand::thread_rng()` is unambiguously the rand generator.
    // The BARE `thread_rng()` idiom is scope-resolved in the visitor
    // (`classify_bare_thread_rng`): hard only when an in-scope `use rand::…`
    // binds the name, otherwise POSSIBLE-RANDOM. A user's own qualified
    // `my_utils::thread_rng()` (prev != "rand") is never flagged.
    if last == "thread_rng" && prev == "rand" {
        return Some(Category::Random);
    }
    if first == "fastrand" {
        return Some(Category::Random);
    }
    if prev == "Uuid" && matches!(last, "new_v4" | "now_v7") {
        return Some(Category::Random);
    }
    // NOTE: `from_entropy` (seeding a PRNG from OS entropy) and `OsRng` are NOT
    // classified here. Both hinge on whether a bare name actually resolves to a
    // `rand` symbol at the use site, which requires lexical scope. They are
    // handled by the visitor's scope-aware resolver (`classify_from_entropy`,
    // `classify_os_rng`, `classify_bare_thread_rng`).

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

// ── Scope-aware import resolution ────────────────────────────────────────────
//
// The tricky RNG symbols — `OsRng`, `thread_rng`, and the `from_entropy`
// receiver — are bare identifiers whose meaning depends entirely on what is in
// lexical scope. `OsRng` might be `rand::rngs::OsRng`, a local `enum` variant,
// a `struct` in a local `mod rngs`, or a `let` binding. A file-wide heuristic
// cannot tell these apart soundly, so we run a small, purpose-built resolver:
// a scope stack (file → module → fn body → block) that records, per scope, the
// `use` imports and local item/binding names, then resolves a bare name
// innermost-first. It only needs to recognize the handful of `rand` paths below
// — it is deliberately NOT a general name resolver.

/// What an in-scope name resolves to, for the small set of `rand` symbols the
/// scanner tracks. Names not relevant to `rand` are simply absent from the
/// scope maps (genuinely unresolved → treated as foreign / possible-random).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameRes {
    /// Resolves to `rand::rngs::OsRng` (via `use`, possibly renamed / grouped).
    RandOsRng,
    /// Resolves to the `rand::rngs` module (via `use rand::rngs;` / `as`).
    RandRngsMod,
    /// Resolves to the `rand` crate root (via `use rand;` / `as`).
    RandCrate,
    /// Resolves to `rand::thread_rng` (via `use rand::thread_rng;` / `as`).
    RandThreadRng,
    /// Bound to a LOCAL item or binding — a `struct`/`enum`/`fn`/`const`/
    /// `static`/`mod`/`type`/`trait`/`union`, a `let`, or a `use` of a
    /// non-`rand` symbol. Shadows any outer meaning and proves the name is NOT
    /// `rand`'s.
    Local,
}

/// One lexical scope: a file, a module body, a function body, or a block.
#[derive(Debug, Default)]
struct Scope {
    /// Name → resolution for names introduced in this scope.
    names: HashMap<String, NameRes>,
    /// `use rand::rngs::*;` (or an aliased equivalent) is in effect here.
    rand_rngs_glob: bool,
    /// `use rand::*;` / `use rand::prelude::*;` is in effect here.
    rand_glob: bool,
}

/// The name a top-level item introduces into its enclosing scope (used to
/// detect shadows). Returns `None` for items that bind no shadowing name
/// (`use`, `impl`, `extern`, …).
fn local_item_name(item: &syn::Item) -> Option<String> {
    Some(match item {
        syn::Item::Struct(i) => i.ident.to_string(),
        syn::Item::Enum(i) => i.ident.to_string(),
        syn::Item::Fn(i) => i.sig.ident.to_string(),
        syn::Item::Const(i) => i.ident.to_string(),
        syn::Item::Static(i) => i.ident.to_string(),
        syn::Item::Mod(i) => i.ident.to_string(),
        syn::Item::Type(i) => i.ident.to_string(),
        syn::Item::Trait(i) => i.ident.to_string(),
        syn::Item::TraitAlias(i) => i.ident.to_string(),
        syn::Item::Union(i) => i.ident.to_string(),
        _ => return None,
    })
}

/// Collect the identifiers a `let` pattern binds (so they shadow outer names).
/// Handles the common forms; anything more exotic simply binds nothing here.
fn pat_names(pat: &syn::Pat, out: &mut Vec<String>) {
    match pat {
        syn::Pat::Ident(pi) => out.push(pi.ident.to_string()),
        syn::Pat::Type(pt) => pat_names(&pt.pat, out),
        syn::Pat::Reference(pr) => pat_names(&pr.pat, out),
        _ => {}
    }
}

// ── AST visitor ────────────────────────────────────────────────────────────

struct LeakVisitor<'a> {
    file: &'a str,
    lines: &'a [Vec<char>],
    fn_stack: Vec<String>,
    /// Lexical scope stack, outermost (file) first. Resolution walks it in
    /// reverse (innermost-first).
    scopes: Vec<Scope>,
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

    // ── Resolution ──────────────────────────────────────────────────────────

    /// Resolve `name` against the scope stack, innermost-first.
    fn lookup(&self, name: &str) -> Option<NameRes> {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.names.get(name).copied())
    }

    fn rand_rngs_glob_in_scope(&self) -> bool {
        self.scopes.iter().any(|s| s.rand_rngs_glob)
    }

    fn rand_glob_in_scope(&self) -> bool {
        self.scopes.iter().any(|s| s.rand_glob)
    }

    /// Classify a use of `OsRng` given the path segments up to and including the
    /// `OsRng` segment. `None` means "provably not `rand`'s" (a local shadow) —
    /// suppressed, not even reported low-confidence.
    fn classify_os_rng(&self, segs: &[String]) -> Option<Category> {
        if segs.len() == 1 {
            // Bare `OsRng` — meaning depends entirely on scope.
            match self.lookup("OsRng") {
                Some(NameRes::RandOsRng) => Some(Category::Random),
                Some(NameRes::Local) => None, // shadowed by a local item/binding
                // Bound to some other rand alias (module/crate/thread_rng) but
                // used bare as `OsRng` — nonsensical; stay low-confidence.
                Some(_) => Some(Category::PossibleRandom),
                None => {
                    // No explicit binding: a `use rand::rngs::*;` glob makes a
                    // bare `OsRng` genuinely in-scope → hard; otherwise foreign.
                    if self.rand_rngs_glob_in_scope() {
                        Some(Category::Random)
                    } else {
                        Some(Category::PossibleRandom)
                    }
                }
            }
        } else {
            Some(self.classify_os_rng_path(segs))
        }
    }

    /// Classify a multi-segment path ending in `OsRng`.
    fn classify_os_rng_path(&self, segs: &[String]) -> Category {
        let n = segs.len();
        // Exact fully-qualified real generator. A leading `::` leaves the
        // segment idents unchanged, so `::rand::rngs::OsRng` matches too.
        if n == 3 && segs[0] == "rand" && segs[1] == "rngs" && segs[2] == "OsRng" {
            return Category::Random;
        }
        // `<rand-alias>::rngs::OsRng` where the head is `use rand as x;`.
        if n == 3
            && segs[1] == "rngs"
            && segs[2] == "OsRng"
            && self.lookup(&segs[0]) == Some(NameRes::RandCrate)
        {
            return Category::Random;
        }
        // `<rngs-alias>::OsRng` where the head is `use rand::rngs;` / `as`.
        if n == 2 && segs[1] == "OsRng" && self.lookup(&segs[0]) == Some(NameRes::RandRngsMod) {
            return Category::Random;
        }
        // A different `...::OsRng` (foreign crate, enum variant, …): unproven.
        Category::PossibleRandom
    }

    /// Classify a BARE `thread_rng()` call by resolving the name in scope.
    /// `None` = a local `fn thread_rng` (user's own) → suppressed.
    fn classify_bare_thread_rng(&self) -> Option<Category> {
        match self.lookup("thread_rng") {
            Some(NameRes::RandThreadRng) => Some(Category::Random),
            Some(NameRes::Local) => None,
            Some(_) => Some(Category::PossibleRandom),
            None => Some(if self.rand_glob_in_scope() {
                Category::Random
            } else {
                Category::PossibleRandom
            }),
        }
    }

    /// Classify a `<receiver>::from_entropy()` call. `segs` is the full call
    /// path (last segment is `from_entropy`, so `segs.len() >= 2`).
    fn classify_from_entropy(&self, segs: &[String]) -> Category {
        let n = segs.len();
        let receiver = segs[n - 2].as_str();
        let bare_receiver = n == 2;
        if is_rng_receiver(receiver) {
            // Recognized RNG-typed receiver. If it is a BARE name that actually
            // resolves to a LOCAL type/binding (e.g. a user's own `struct
            // FooRng`), it is not rand's → downgrade. A qualified receiver
            // (n > 2) is trusted by its path.
            if bare_receiver && self.lookup(receiver) == Some(NameRes::Local) {
                Category::PossibleRandom
            } else {
                Category::Random
            }
        } else {
            // Unrecognized receiver: hard only if the bare name resolves to
            // rand's `OsRng` (e.g. `use rand::rngs::OsRng as R; R::from_entropy()`);
            // otherwise low-confidence (e.g. a user `Config::from_entropy()`).
            if bare_receiver && self.lookup(receiver) == Some(NameRes::RandOsRng) {
                Category::Random
            } else {
                Category::PossibleRandom
            }
        }
    }

    // ── Scope construction ───────────────────────────────────────────────────

    /// Build a [`Scope`] from a list of items (module/file body) plus any
    /// `let`-binding names (block body). Resolution of aliased `use` heads reads
    /// the outer scopes (already on the stack) and the pass-A locals.
    fn build_scope(&self, items: &[syn::Item], lets: &[String]) -> Scope {
        let mut scope = Scope::default();
        // Pass A: local item definitions + `let` bindings shadow names.
        for item in items {
            if let Some(name) = local_item_name(item) {
                scope.names.insert(name, NameRes::Local);
            }
        }
        for l in lets {
            scope.names.insert(l.clone(), NameRes::Local);
        }
        // Snapshot pass-A locals for head resolution of aliased `use` paths
        // (avoids borrowing `scope` while mutating it in pass B).
        let base = scope.names.clone();
        // Pass B: `use` items.
        for item in items {
            if let syn::Item::Use(u) = item {
                let mut prefix = Vec::new();
                self.collect_use_tree(&u.tree, &mut prefix, &base, &mut scope);
            }
        }
        scope
    }

    /// Walk a `use` tree, accumulating the path `prefix`, and record every leaf
    /// / glob into `out`.
    fn collect_use_tree(
        &self,
        tree: &syn::UseTree,
        prefix: &mut Vec<String>,
        base: &HashMap<String, NameRes>,
        out: &mut Scope,
    ) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.collect_use_tree(&p.tree, prefix, base, out);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                let leaf = n.ident.to_string();
                self.record_use_leaf(prefix, &leaf, &leaf, base, out);
            }
            syn::UseTree::Rename(r) => {
                let leaf = r.ident.to_string();
                let bind = r.rename.to_string();
                self.record_use_leaf(prefix, &leaf, &bind, base, out);
            }
            syn::UseTree::Glob(_) => self.record_use_glob(prefix, base, out),
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    let mut branch = prefix.clone();
                    self.collect_use_tree(t, &mut branch, base, out);
                }
            }
        }
    }

    fn record_use_leaf(
        &self,
        prefix: &[String],
        leaf: &str,
        binding: &str,
        base: &HashMap<String, NameRes>,
        out: &mut Scope,
    ) {
        let mut full: Vec<String> = prefix.to_vec();
        full.push(leaf.to_string());
        match self.resolve_use_path(&full, base) {
            Some(res) => {
                out.names.insert(binding.to_string(), res);
            }
            None => {
                // Not a tracked `rand` symbol. If it binds a name we resolve
                // bare (`OsRng` / `thread_rng`), it SHADOWS the rand meaning.
                if binding == "OsRng" || binding == "thread_rng" {
                    out.names.insert(binding.to_string(), NameRes::Local);
                }
            }
        }
    }

    fn record_use_glob(&self, prefix: &[String], base: &HashMap<String, NameRes>, out: &mut Scope) {
        let s: Vec<&str> = prefix.iter().map(String::as_str).collect();
        match s.as_slice() {
            ["rand", "rngs"] => out.rand_rngs_glob = true,
            ["rand"] | ["rand", "prelude"] => out.rand_glob = true,
            [head] => match self.resolve_head(base, head) {
                Some(NameRes::RandRngsMod) => out.rand_rngs_glob = true,
                Some(NameRes::RandCrate) => out.rand_glob = true,
                _ => {}
            },
            _ => {}
        }
    }

    /// Map a (possibly aliased) `use` path to the `rand` symbol it names.
    fn resolve_use_path(
        &self,
        full: &[String],
        base: &HashMap<String, NameRes>,
    ) -> Option<NameRes> {
        let s: Vec<&str> = full.iter().map(String::as_str).collect();
        match s.as_slice() {
            ["rand", "rngs", "OsRng"] => return Some(NameRes::RandOsRng),
            ["rand", "rngs"] => return Some(NameRes::RandRngsMod),
            ["rand"] => return Some(NameRes::RandCrate),
            ["rand", "thread_rng"] => return Some(NameRes::RandThreadRng),
            _ => {}
        }
        // Aliased head: `use rand as r; use r::rngs::OsRng;` and friends.
        let tail = &s[1..];
        match (self.resolve_head(base, &full[0]), tail) {
            (Some(NameRes::RandCrate), ["rngs", "OsRng"]) => Some(NameRes::RandOsRng),
            (Some(NameRes::RandCrate), ["rngs"]) => Some(NameRes::RandRngsMod),
            (Some(NameRes::RandCrate), ["thread_rng"]) => Some(NameRes::RandThreadRng),
            (Some(NameRes::RandRngsMod), ["OsRng"]) => Some(NameRes::RandOsRng),
            _ => None,
        }
    }

    /// Resolve a path head against the pass-A locals of the scope being built
    /// and the already-pushed outer scopes.
    fn resolve_head(&self, base: &HashMap<String, NameRes>, name: &str) -> Option<NameRes> {
        base.get(name).copied().or_else(|| self.lookup(name))
    }
}

impl<'ast> Visit<'ast> for LeakVisitor<'_> {
    // ── Scope stack ──────────────────────────────────────────────────────────
    // Each of these pre-collects the imports / local names of the scope it
    // opens BEFORE recursing (Rust items are order-independent within a scope,
    // so a `use` at the bottom of a module still applies to a fn at the top).

    fn visit_file(&mut self, node: &'ast syn::File) {
        let scope = self.build_scope(&node.items, &[]);
        self.scopes.push(scope);
        visit::visit_file(self, node);
        self.scopes.pop();
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        // Only an inline `mod m { … }` opens a scope; `mod m;` has no body here.
        if let Some((_, items)) = &node.content {
            let scope = self.build_scope(items, &[]);
            self.scopes.push(scope);
            visit::visit_item_mod(self, node);
            self.scopes.pop();
        } else {
            visit::visit_item_mod(self, node);
        }
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        // Block-local `use`/items and `let` bindings both shadow outer names.
        let mut items: Vec<syn::Item> = Vec::new();
        let mut lets: Vec<String> = Vec::new();
        for stmt in &node.stmts {
            match stmt {
                syn::Stmt::Item(it) => items.push(it.clone()),
                syn::Stmt::Local(local) => pat_names(&local.pat, &mut lets),
                _ => {}
            }
        }
        let scope = self.build_scope(&items, &lets);
        self.scopes.push(scope);
        visit::visit_block(self, node);
        self.scopes.pop();
    }

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
            let n = segs.len();
            if let Some(cat) = classify_call(&segs) {
                self.record(cat, node.span());
            } else if n == 1 && segs[0] == "thread_rng" {
                // Bare `thread_rng()` — hard only if an in-scope `use rand::…`
                // binds the name; otherwise possible-random (a user's own
                // `thread_rng` is suppressed as a local shadow).
                if let Some(cat) = self.classify_bare_thread_rng() {
                    self.record(cat, node.span());
                }
            } else if n >= 2 && segs[n - 1] == "from_entropy" {
                // `<receiver>::from_entropy()` — RNG seeding from OS entropy.
                let cat = self.classify_from_entropy(&segs);
                self.record(cat, node.span());
            } else if let Some(pos) = segs.iter().rposition(|s| s == "OsRng") {
                // Call form on the `OsRng` type (e.g. `OsRng::default()`,
                // `rand::rngs::OsRng::new(...)`). Classify on the path UP TO AND
                // INCLUDING `OsRng` via the scope resolver; the bare-value /
                // receiver forms are handled by `visit_expr_path` (a call ends
                // past `OsRng`, so no overlap).
                if let Some(cat) = self.classify_os_rng(&segs[..=pos]) {
                    self.record(cat, node.span());
                }
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
        // Classification is scope-aware (`classify_os_rng`): a bare `OsRng`
        // imported via `use rand::rngs::OsRng` is HARD, while a local shadow
        // (enum variant, `mod rngs::OsRng`, `let OsRng`) or a foreign
        // `my_crate::rngs::OsRng` is not.
        let idents: Vec<String> = node
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        match idents.last() {
            Some(last) if last == "OsRng" => {
                if let Some(cat) = self.classify_os_rng(&idents) {
                    self.record(cat, node.span());
                }
            }
            // A bare single-segment name that resolves to `rand::rngs::OsRng`
            // through an aliasing import (`use rand::rngs::OsRng as Rng;`).
            Some(_) if idents.len() == 1 && self.lookup(&idents[0]) == Some(NameRes::RandOsRng) => {
                self.record(Category::Random, node.span());
            }
            _ => {}
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
    fn os_rng_bound_to_a_local_via_full_path_is_hard_random() {
        // `let mut rng = rand::rngs::OsRng; rng.fill_bytes(..)` — OsRng appears as
        // a bare value/path expression (not a method receiver), but is FULLY
        // QUALIFIED, so it is the real generator → HARD RANDOM.
        let leaks = scan(
            "fn f() { let mut rng = rand::rngs::OsRng; let mut b = [0u8; 8]; rng.fill_bytes(&mut b); }",
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

    // Convenience predicates over a scan result.
    fn has_hard_os_rng(leaks: &[Leak]) -> bool {
        leaks
            .iter()
            .any(|l| l.category == Category::Random && l.snippet.contains("OsRng"))
    }
    fn any_hard_random(leaks: &[Leak]) -> bool {
        leaks.iter().any(|l| l.category == Category::Random)
    }
    fn any_possible_random(leaks: &[Leak]) -> bool {
        leaks.iter().any(|l| l.category == Category::PossibleRandom)
    }

    // ── Scope-aware `OsRng` resolution: one test per enumerated case ──────────

    #[test]
    fn case1_imported_bare_os_rng_is_hard_random() {
        // `use rand::rngs::OsRng; let r = OsRng;` → HARD. This is the round-4
        // false-NEGATIVE: a real OS-entropy leak that the old exact-path-only
        // heuristic downgraded to possible-random and `--deny` missed.
        let leaks = scan("use rand::rngs::OsRng; fn f() { let _r = OsRng; }");
        assert!(
            has_hard_os_rng(&leaks),
            "imported bare OsRng must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case2_aliased_import_is_hard_random() {
        // `use rand::rngs::OsRng as Rng; let r = Rng;` → HARD (the alias `Rng`
        // resolves to `rand::rngs::OsRng`).
        let leaks = scan("use rand::rngs::OsRng as Rng; fn f() { let _r = Rng; }");
        assert!(
            any_hard_random(&leaks),
            "aliased OsRng import must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case3_grouped_import_is_hard_random() {
        // `use rand::rngs::{OsRng, StdRng}; let r = OsRng;` → HARD.
        let leaks = scan("use rand::rngs::{OsRng, StdRng}; fn f() { let _r = OsRng; }");
        assert!(
            has_hard_os_rng(&leaks),
            "grouped OsRng import must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case4_module_import_then_qualified_is_hard_random() {
        // `use rand::rngs; let r = rngs::OsRng;` → HARD (the `rngs` alias points
        // at `rand::rngs`).
        let leaks = scan("use rand::rngs; fn f() { let _r = rngs::OsRng; }");
        assert!(
            has_hard_os_rng(&leaks),
            "rand::rngs module import + rngs::OsRng must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case5_glob_import_is_hard_random() {
        // `use rand::rngs::*; let r = OsRng;` → HARD (a genuine rand glob makes
        // the bare name possibly-in-scope; treated as hard).
        let leaks = scan("use rand::rngs::*; fn f() { let _r = OsRng; }");
        assert!(
            has_hard_os_rng(&leaks),
            "glob-imported OsRng must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case6_foreign_rngs_os_rng_is_not_hard() {
        // `my_crate::rngs::OsRng` with no rand import → NOT hard (possible).
        let leaks = scan("fn f() { let _r = my_crate::rngs::OsRng; }");
        assert!(
            !any_hard_random(&leaks),
            "my_crate::rngs::OsRng must NOT be hard RANDOM: {leaks:#?}"
        );
        assert!(
            any_possible_random(&leaks),
            "my_crate::rngs::OsRng should be POSSIBLE-RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case7_local_shadow_import_is_not_hard() {
        // `mod rngs { pub struct OsRng; } use rngs::OsRng; let r = OsRng;` — the
        // bare `OsRng` resolves to the LOCAL struct, not rand's → NOT hard.
        let leaks =
            scan("mod rngs { pub struct OsRng; } use rngs::OsRng; fn f() { let _r = OsRng; }");
        assert!(
            !any_hard_random(&leaks),
            "locally-shadowed OsRng must NOT be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case8_enum_variant_os_rng_is_not_hard() {
        // `enum Source { OsRng } let s = Source::OsRng;` → NOT hard.
        let leaks = scan("enum Source { OsRng } fn f() -> Source { let s = Source::OsRng; s }");
        assert!(
            !any_hard_random(&leaks),
            "Source::OsRng variant must NOT be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case9_bare_os_rng_without_import_is_not_hard() {
        // Bare `OsRng` with NO import in scope → NOT hard (possible-random).
        let leaks = scan("fn f() { let _r = OsRng; }");
        assert!(
            !any_hard_random(&leaks),
            "unimported bare OsRng must NOT be hard RANDOM: {leaks:#?}"
        );
        assert!(
            any_possible_random(&leaks),
            "unimported bare OsRng should be POSSIBLE-RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn case10_sibling_module_import_does_not_leak_across_scopes() {
        // An import inside `mod a` must NOT make a bare `OsRng` in `mod b` hard.
        let leaks = scan("mod a { use rand::rngs::OsRng; } mod b { fn f() { let _r = OsRng; } }");
        assert!(
            !any_hard_random(&leaks),
            "sibling-module import must not make OsRng in another module hard: {leaks:#?}"
        );
    }

    #[test]
    fn case11_fully_qualified_os_rng_is_hard_without_import() {
        // `rand::rngs::OsRng` fully qualified, no import → HARD.
        let leaks = scan("fn f() -> u64 { rand::rngs::OsRng.next_u64() }");
        assert!(
            has_hard_os_rng(&leaks),
            "fully-qualified OsRng must be hard RANDOM: {leaks:#?}"
        );
    }

    // ── Scope discipline applied to `thread_rng` / `from_entropy` ─────────────

    #[test]
    fn imported_bare_thread_rng_is_hard_random() {
        // `use rand::thread_rng; thread_rng()` → HARD.
        let leaks = scan("use rand::thread_rng; fn f() { let _ = thread_rng(); }");
        assert!(
            any_hard_random(&leaks),
            "imported bare thread_rng must be hard RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn unimported_bare_thread_rng_is_not_hard() {
        // Bare `thread_rng()` with no rand import → NOT hard (possible-random).
        let leaks = scan("fn f() { let _ = thread_rng(); }");
        assert!(
            !any_hard_random(&leaks),
            "unimported bare thread_rng must NOT be hard RANDOM: {leaks:#?}"
        );
        assert!(
            any_possible_random(&leaks),
            "unimported bare thread_rng should be POSSIBLE-RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn local_fn_thread_rng_is_not_flagged() {
        // A user's own `fn thread_rng()` called bare must NOT be flagged at all.
        let leaks = scan("fn thread_rng() -> u32 { 0 } fn f() { let _ = thread_rng(); }");
        assert!(
            leaks.is_empty(),
            "user-defined thread_rng must not be flagged: {leaks:#?}"
        );
    }

    #[test]
    fn local_rng_type_from_entropy_is_downgraded() {
        // A local `struct FooRng` whose bare `FooRng::from_entropy()` merely
        // looks like the RNG idiom must NOT be a hard false positive.
        let leaks =
            scan("struct FooRng; impl FooRng { fn from_entropy() -> Self { FooRng } } fn f() { let _ = FooRng::from_entropy(); }");
        assert!(
            !any_hard_random(&leaks),
            "local FooRng::from_entropy must NOT be hard RANDOM: {leaks:#?}"
        );
        assert!(
            any_possible_random(&leaks),
            "local FooRng::from_entropy should be POSSIBLE-RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn non_rand_rngs_os_rng_is_possible_random_not_hard() {
        // `my_crate::rngs::OsRng` shares the `rngs::OsRng` tail but is NOT the
        // real generator — the exact path is `rand::rngs::OsRng`. The old
        // penultimate-segment (`rngs`) check false-flagged this; it must now be
        // downgraded to POSSIBLE-RANDOM.
        let leaks = scan("fn f() { let _r = my_crate::rngs::OsRng; }");
        assert!(
            !leaks.iter().any(|l| l.category == Category::Random),
            "my_crate::rngs::OsRng must NOT be hard RANDOM: {leaks:#?}"
        );
        assert!(
            leaks
                .iter()
                .any(|l| l.category == Category::PossibleRandom && l.snippet.contains("OsRng")),
            "my_crate::rngs::OsRng should be POSSIBLE-RANDOM: {leaks:#?}"
        );
    }

    #[test]
    fn local_mod_rngs_os_rng_is_not_hard_random() {
        // A local `mod rngs { pub struct OsRng; }` with a bare `OsRng` use must
        // NOT be a hard RANDOM false positive — bare `OsRng` is never fully
        // qualified, so it stays POSSIBLE-RANDOM regardless of any local `rngs`.
        let leaks = scan("mod rngs { pub struct OsRng; } fn f() { let _r = OsRng; }");
        assert!(
            !leaks.iter().any(|l| l.category == Category::Random),
            "local mod rngs::OsRng must NOT be hard RANDOM: {leaks:#?}"
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
