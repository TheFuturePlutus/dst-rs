//! Invariant-presence checker.
//!
//! Seeded replay proves a run is REPRODUCIBLE — it says nothing about whether the
//! run asserts anything. A DST test can drive the whole simulation surface
//! (`SimScheduler`, `FaultSchedule`, `SimulatedRandom`, …) through thousands of
//! steps and register **zero** invariants, then pass green having checked nothing.
//! This module is the static gate that closes that hole: it flags files that
//! *exercise the simulation* but *assert nothing*.
//!
//! ## What it can and cannot say
//!
//! Like [`crate::scanner`], this is a **name-based heuristic**, not a sound
//! analyzer. It certifies **that assertions exist**, never **that they are
//! correct** — deciding whether an invariant encodes the right rule is domain
//! policy the author owns, not something a tool can know. A clean result here
//! means "you asserted something", not "your invariants are right".
//!
//! ## Two tiers
//!
//! * [`Status::Missing`] — a simulation site with **no assertions of any kind**
//!   (no `Invariant::new`, no `.check()`, no `assert!` family macro). An UNUSED
//!   `InvariantEngine::new(vec![])` — an empty engine that is never `.check`ed —
//!   counts as missing: it registers nothing. This is the tier the `--deny` gate
//!   fails on.
//! * [`Status::RawOnly`] — a simulation site that asserts only through raw
//!   `assert!`/`assert_eq!`/… macros and registers no declarative invariant.
//!   Legitimate, but weaker than a declared invariant set; surfaced as ADVISORY
//!   and fails the gate only under `--deny-raw`.
//! * [`Status::Ok`] — a simulation site that constructs a real invariant
//!   (`Invariant::new`, or a non-empty `InvariantEngine::new([...])`) or evaluates
//!   one (`.check` / `.check_all`). NOTE: an empty engine that IS `.check`ed lands
//!   here too — an accepted false OK, see "Safe direction" below.
//!
//! A file that does **not** touch the simulation surface is not a site at all and
//! is never reported — this gate is about DST tests, not arbitrary code.
//!
//! ## Safe direction
//!
//! `Missing` is the gate-failing tier, so — exactly like the leak scanner's High
//! rules — it must never fire on a file that genuinely asserts. When the
//! invariant signal is ambiguous the checker leans toward NOT reporting `Missing`
//! (a vacuous file slipping to `Ok` is the safe error; failing a good test is
//! not). Concretely: an empty engine that is later `.check`ed is classified `Ok`,
//! not `Missing` — file-global analysis cannot tell it from a legitimately-checked
//! helper engine in the same file, and catching it would risk a forbidden false
//! `Missing`. The unused empty engine (constructed, never checked) is still caught.
//!
//! ## Shadow guard
//!
//! A file that *defines* a name (`struct SimScheduler { … }`) is not *using* the
//! navian-dst type of that name, so locally-defined markers are ignored — the
//! same local-shadow discipline the leak scanner uses. This keeps the gate from
//! firing on the navian-dst library's own source.
//!
//! ## Waiver (the delegated-check escape hatch)
//!
//! The one residual false-`Missing` this static, per-file check cannot rule out is
//! the *delegated* pattern: a per-scenario test file that only drives the sim and
//! calls a shared harness helper which builds the engine and runs `.check()`
//! internally. Nothing in that file asserts locally, yet the run genuinely checks
//! invariants. For those files the author drops [`WAIVER_MARKER`] in a comment; the
//! site is still reported (as waived, so it stays auditable) but never fails the
//! gate.
//!
//! ## Known limitations (documented false negatives)
//!
//! Detection is name/path-based and does NOT do the leak scanner's alias
//! resolution. An invariant reached ONLY through an aliased or type-aliased path —
//! `use …::Invariant as I; I::new(…)`, `type Inv = Invariant<W>; Inv::new(…)`, or a
//! qself `<Invariant<W>>::new(…)` — is not recognized. If such a form is a file's
//! *only* invariant signal the file reads as `Missing`; the [`WAIVER_MARKER`]
//! covers it until alias resolution lands. These are false negatives in the safe
//! direction only for files that also use a plainly-spelled invariant elsewhere.
//!
//! Analysis is per FILE, not per function. A file that holds several tests is a
//! single site, so a vacuous simulation in one test is masked by an assertion in a
//! *different* test in the same file — a false `Ok` (the accepted safe direction).
//! Per-test attribution is a deliberate non-goal for v1: the naive version — treat
//! each `fn` as its own site — would flag every ordinary `fn make_sim() ->
//! SimScheduler` helper as `Missing` (a false *positive*, which the cardinal rule
//! forbids). Doing it right needs test-attribute scoping plus cross-function
//! awareness, and is left as a fast-follow.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use walkdir::WalkDir;

/// Names whose *use* marks a file as a simulation site — the deterministic,
/// test-side harness constructs. Matching is by the identifier's segment
/// (`navian_dst::SimScheduler` and a bare imported `SimScheduler` both match),
/// after the local-shadow guard. `Simulated*` covers the injectable fakes;
/// `SimScheduler`/`FaultSchedule` cover the scheduler and fault driver; `ddmin`
/// is the trace-shrinker (only ever used to minimize a failing simulation).
const SIM_MARKERS: &[&str] = &[
    "SimScheduler",
    "SimulatedRandom",
    "SimulatedExecutor",
    "SimulatedNetwork",
    "SimulatedTime",
    "FaultSchedule",
    "ddmin",
];

/// Method names that evaluate an `navian_dst::InvariantEngine`. A `.check()` /
/// `.check_all()` call is counted as declarative-invariant usage even when the
/// engine value came from a helper and the type name never appears in the file.
const INVARIANT_CHECK_METHODS: &[&str] = &["check", "check_all"];

/// Assertion macros that count as "asserted something", raw tier.
const ASSERT_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
];

/// In-source escape hatch. A file containing this marker anywhere (typically in a
/// comment) is a simulation site whose invariants live elsewhere — e.g. a shared
/// harness helper that builds the engine and runs `.check()` internally, called
/// from per-scenario test files. Such a file legitimately asserts nothing
/// *locally*; the author waives the gate for it by dropping this marker in. The
/// site still appears in the report (as waived), so the waiver is auditable.
const WAIVER_MARKER: &str = "navian-dst:invariants-elsewhere";

/// Secondary bound on how deep the macro-body re-parse (see
/// [`PresenceVisitor::visit_macro`]) will recurse. The PRIMARY defense against
/// stack exhaustion is [`exceeds_nesting_depth`], which rejects an over-deep file
/// before it ever reaches `syn`; this ceiling is a cheap belt-and-suspenders that
/// caps the re-parse loop regardless. Invariant signals nested this deep are
/// vanishingly rare.
const MAX_MACRO_DEPTH: u32 = 32;

/// Maximum `()[]{}` nesting depth we will hand to `syn`. Rust's recursive-descent
/// parser — and our AST walk — recurse once per nesting level, so a pathologically
/// deep file (hand-written junk, minified/generated code, or a fuzzer artifact in a
/// scanned tree) can recurse the stack to death. A stack overflow ABORTS the process
/// — it does not unwind, so `catch_unwind` cannot save it — which would take down the
/// whole run. The cap sits far above any hand-written Rust yet well below the depth
/// that overflows the parser.
const MAX_NESTING_DEPTH: usize = 512;

/// True if `content`'s running `()[]{}` nesting ever exceeds [`MAX_NESTING_DEPTH`].
/// A cheap O(n) byte scan and a deliberate over-approximation — it counts bracket
/// bytes inside strings and comments too — used only to reject pathological input
/// before parsing. A rejected file is treated as a read/parse failure (skipped, and
/// it makes a `--deny` gate uncertifiable → exit 2), never a false pass and never a
/// crash. Over-rejection can only make the gate stricter, the safe direction.
pub(crate) fn exceeds_nesting_depth(content: &str) -> bool {
    let mut depth: usize = 0;
    for &b in content.as_bytes() {
        match b {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return true;
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// Presence status for one simulation site (file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// A real invariant is registered or evaluated — the strongest signal.
    Ok,
    /// Only raw `assert!`-family macros; no declarative invariant.
    RawOnly,
    /// A simulation site that asserts nothing at all. The gate fails on this.
    Missing,
}

impl Status {
    /// Short upper-case label for the human report.
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::RawOnly => "RAW-ONLY",
            Status::Missing => "MISSING",
        }
    }
}

/// One analyzed simulation site.
#[derive(Debug, Clone, Serialize)]
pub struct Site {
    pub file: String,
    pub status: Status,
    /// Distinct sim-surface markers seen (sorted), e.g. `["FaultSchedule",
    /// "SimScheduler"]` — evidence for why this file is a site.
    pub sim_markers: Vec<String>,
    /// 1-based line of the first sim-surface reference, for a clickable location.
    pub sim_line: usize,
    /// Whether a real (non-empty) declarative invariant was found.
    pub has_invariant: bool,
    /// Whether an EMPTY `InvariantEngine::new([])` was seen anywhere in the file.
    /// When the site is `Missing`, this is surfaced as the specific reason. It can
    /// also co-occur with an `Ok` verdict (`has_invariant` true) — e.g. an empty
    /// engine that is `.check`ed (an accepted false OK), or an empty engine sitting
    /// beside a real invariant — so it is informational, not a `Missing` implier.
    pub empty_engine: bool,
    /// Count of raw `assert!`-family macro invocations.
    pub assert_macros: usize,
    /// The author dropped [`WAIVER_MARKER`] in this file: its invariants live
    /// elsewhere (a shared harness). The `status` is still computed and reported,
    /// but a waived site never fails the gate — see [`PresenceReport::any_missing`].
    pub waived: bool,
}

/// Aggregate result of a presence run.
#[derive(Debug, Default)]
pub struct PresenceReport {
    /// Simulation sites, sorted by file. Non-sim files are not included.
    pub sites: Vec<Site>,
    pub files_scanned: usize,
    pub files_parsed: usize,
    /// Files that could not be read or parsed (skipped, but they make a `--deny`
    /// gate uncertifiable — see [`PresenceReport::uncertifiable`]).
    pub parse_failures: Vec<String>,
}

impl PresenceReport {
    /// Is any NON-WAIVED site [`Status::Missing`] — i.e. does the default `--deny`
    /// gate bite? Waived sites (see [`Site::waived`]) are excluded so an author
    /// can opt a delegated-check file out of the gate.
    pub fn any_missing(&self) -> bool {
        self.sites
            .iter()
            .any(|s| s.status == Status::Missing && !s.waived)
    }

    /// Is any NON-WAIVED site [`Status::RawOnly`] — the `--deny-raw` gate.
    pub fn any_raw_only(&self) -> bool {
        self.sites
            .iter()
            .any(|s| s.status == Status::RawOnly && !s.waived)
    }

    /// A read/parse failure means a `--deny` gate cannot honestly certify the tree
    /// — the caller should exit 2 rather than pass green on files it never saw.
    pub fn uncertifiable(&self) -> bool {
        !self.parse_failures.is_empty()
    }

    /// Count of sites at each status, for the summary line.
    pub fn tally(&self, status: Status) -> usize {
        self.sites.iter().filter(|s| s.status == status).count()
    }
}

/// Walk `root` recursively and analyze every `.rs` file for invariant presence.
///
/// Skips `target/` and hidden directories, mirroring [`crate::scanner::scan_path`].
/// A file that fails to read/parse is recorded in `parse_failures` and skipped;
/// it never aborts the run.
pub fn check_path(root: &Path) -> PresenceReport {
    let mut report = PresenceReport::default();

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
        // A traversal error (an unreadable directory, a broken symlink, a
        // permission failure) must NOT be silently dropped: under a `--deny` gate
        // that would let the run exit 0 on a tree it never fully walked. Record it
        // as a failure so the gate becomes uncertifiable (exit 2).
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
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                report.parse_failures.push(display);
                continue;
            }
        };
        check_source(&display, &content, &mut report);
    }

    report.sites.sort_by(|a, b| a.file.cmp(&b.file));
    report
}

/// Analyze a single in-memory source string, appending a [`Site`] to `report`
/// only if the file is a simulation site. Exposed for testing.
pub fn check_source(file: &str, content: &str, report: &mut PresenceReport) {
    // Reject a pathologically deep file BEFORE parsing: `syn`'s recursive parser
    // (and our AST walk) would overflow the stack on deeply-nested input, and a
    // stack overflow aborts the process rather than unwinding — `catch_unwind`
    // cannot catch it. Treat it as a parse failure (skip + make the gate
    // uncertifiable), honoring the "skip one file, never abort the run" contract.
    if exceeds_nesting_depth(content) {
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

    // Pre-pass: collect names DEFINED in this file, so a file that defines a
    // marker type (the navian-dst library itself) is not mistaken for a USER of
    // it. Same local-shadow discipline as the leak scanner.
    let mut defs = LocalDefs::default();
    defs.visit_file(&ast);

    let mut v = PresenceVisitor {
        local_defs: &defs.names,
        sim_markers: Vec::new(),
        sim_line: 0,
        real_invariant: false,
        checked: false,
        empty_engine: false,
        assert_macros: 0,
        macro_depth: 0,
    };
    v.visit_file(&ast);

    // Not a simulation site → not our business; emit nothing.
    if v.sim_markers.is_empty() {
        return;
    }

    // A real invariant was CONSTRUCTED (`Invariant::new`, a non-empty engine) or an
    // engine was EVALUATED (a 2-arg `.check`). An UNUSED empty engine
    // (`InvariantEngine::new(vec![])` with no check) has neither signal → MISSING,
    // the common "forgot to add invariants" mistake.
    //
    // We deliberately do NOT subtract the file-global `empty_engine` flag here. An
    // empty engine that IS later `.check`ed is an accepted false OK: file-global
    // analysis cannot distinguish that empty engine from a legitimately-checked
    // helper-built engine elsewhere in the same multi-test file, and the cardinal
    // rule forbids risking a false MISSING (failing a file that genuinely asserts)
    // to catch it. False OK is the safe direction; see the module docs.
    let has_invariant = v.real_invariant || v.checked;

    let status = if has_invariant {
        Status::Ok
    } else if v.assert_macros > 0 {
        Status::RawOnly
    } else {
        Status::Missing
    };

    let mut sim_markers = v.sim_markers;
    sim_markers.sort();
    sim_markers.dedup();

    report.sites.push(Site {
        file: file.to_string(),
        status,
        sim_markers,
        sim_line: v.sim_line,
        has_invariant,
        empty_engine: v.empty_engine,
        assert_macros: v.assert_macros,
        // An author opt-out: the file declares its invariants live elsewhere.
        // Matched textually so it works from any comment, not just doc comments
        // (which are all syn preserves).
        waived: content.contains(WAIVER_MARKER),
    });
}

/// Names defined in this file (types/fns/traits/etc.), used to shadow-guard the
/// markers so a file that DEFINES a marker name is not counted as USING it.
#[derive(Default)]
struct LocalDefs {
    names: HashSet<String>,
}

impl<'ast> Visit<'ast> for LocalDefs {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.names.insert(i.ident.to_string());
        visit::visit_item_struct(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.names.insert(i.ident.to_string());
        visit::visit_item_enum(self, i);
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.names.insert(i.ident.to_string());
        visit::visit_item_union(self, i);
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.names.insert(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.names.insert(i.ident.to_string());
        visit::visit_item_trait(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.names.insert(i.ident.to_string());
        visit::visit_item_type(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.names.insert(i.ident.to_string());
        visit::visit_item_mod(self, i);
    }
}

/// Last two segments of a call path, e.g. `navian_dst::Invariant::new` →
/// `Some(("Invariant", "new"))`. Used to recognize the invariant constructors
/// regardless of how they are qualified.
fn tail2(path: &syn::Path) -> Option<(String, String)> {
    let n = path.segments.len();
    if n < 2 {
        return None;
    }
    Some((
        path.segments[n - 2].ident.to_string(),
        path.segments[n - 1].ident.to_string(),
    ))
}

/// Whether an expression is an EMPTY collection literal — the "registers nothing"
/// argument to `InvariantEngine::new`. Recognizes `vec![]`, `Vec::new()`, and
/// `[]`. Anything else (a variable, a non-empty literal, a builder call) is
/// treated as possibly-non-empty and does NOT count as empty.
fn is_empty_collection(expr: &syn::Expr) -> bool {
    match expr {
        // `vec![]` — the `vec` macro with no tokens. Match on the path's LAST
        // segment so a qualified `std::vec![]` / `alloc::vec![]` is recognized too.
        syn::Expr::Macro(m) => {
            m.mac.path.segments.last().is_some_and(|s| s.ident == "vec")
                && m.mac.tokens.is_empty()
        }
        // `[]` — an empty array literal.
        syn::Expr::Array(a) => a.elems.is_empty(),
        // `Vec::new()` — a zero-arg call whose path tail is `Vec::new`.
        syn::Expr::Call(c) => {
            if !c.args.is_empty() {
                return false;
            }
            if let syn::Expr::Path(p) = &*c.func {
                matches!(tail2(&p.path), Some((ty, m)) if ty == "Vec" && m == "new")
            } else {
                false
            }
        }
        // Parenthesized: unwrap and recurse.
        syn::Expr::Paren(p) => is_empty_collection(&p.expr),
        _ => false,
    }
}

/// Walks a parsed file, recording sim-surface use, real-invariant use, and
/// assertion-macro count.
struct PresenceVisitor<'a> {
    local_defs: &'a HashSet<String>,
    sim_markers: Vec<String>,
    sim_line: usize,
    /// A real invariant was CONSTRUCTED — `Invariant::new`, or a non-empty
    /// `InvariantEngine::new([...])`. This alone means the file has invariants.
    real_invariant: bool,
    /// An engine was EVALUATED — a 2-arg `.check`/`.check_all`, or the UFCS
    /// `InvariantEngine::check(...)` form. Counts toward `Ok` on its own (an engine
    /// built by a helper is checked here even though its type name never appears).
    /// An empty engine that is `.check`ed is thus an accepted false OK — see the
    /// classification note in `check_source`.
    checked: bool,
    empty_engine: bool,
    assert_macros: usize,
    /// Depth of macro bodies we are currently walking. syn does not descend into
    /// macro token streams by default, so `vec![Invariant::new(...)]` would be
    /// invisible; we re-parse macro bodies (below) and visit them. But we only
    /// let INVARIANT signals count from inside a macro — sim-surface markers are
    /// NOT recorded there, so a stray `SimScheduler` in a `println!` cannot
    /// fabricate a new site and cause a false MISSING. Invariant detection from
    /// inside a macro only ever moves a site toward OK, the safe direction.
    macro_depth: u32,
}

impl PresenceVisitor<'_> {
    /// Record a sim-surface marker for `ident`, applying the local-shadow guard.
    /// `bare` is true when `ident` is the FIRST segment of its path (unqualified) —
    /// the local-shadow guard applies ONLY to bare paths, so a local `struct
    /// SimScheduler` never suppresses an explicitly-qualified `navian_dst::
    /// SimScheduler` use. `line` is the 1-based span line, kept as the site's
    /// location.
    fn note_sim_marker(&mut self, ident: &str, bare: bool, line: usize) {
        // Inside a macro body: do not let sim markers create/extend a site (see
        // `macro_depth`). Invariant signals are still honored elsewhere.
        if self.macro_depth > 0 {
            return;
        }
        // A BARE name defined in this same file is the user's own symbol, not the
        // navian-dst type — ignore it. A qualified path (e.g. `navian_dst::…`) is
        // NOT shadow-guarded: the explicit namespace names the real type.
        if bare && self.local_defs.contains(ident) {
            return;
        }
        if SIM_MARKERS.contains(&ident) {
            self.sim_markers.push(ident.to_string());
            if self.sim_line == 0 || (line != 0 && line < self.sim_line) {
                self.sim_line = line;
            }
        }
    }
}

impl<'ast> Visit<'ast> for PresenceVisitor<'_> {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        // Sim-surface detection: every segment is a candidate
        // (`navian_dst::SimScheduler`, a bare `SimScheduler`, …). The shadow guard
        // applies only to the FIRST (unqualified) segment, so a local marker name
        // never suppresses an explicitly-qualified `navian_dst::` use.
        for (idx, seg) in p.segments.iter().enumerate() {
            let ident = seg.ident.to_string();
            let line = seg.ident.span().start().line;
            self.note_sim_marker(&ident, idx == 0, line);
        }
        // `Invariant::new` as a bare path (e.g. mapped/collected, not directly
        // the call form handled in visit_expr_call). Shadow-guarded on the type
        // name so a local `struct Invariant` does not count.
        if let Some((ty, method)) = tail2(p) {
            if ty == "Invariant" && method == "new" && !self.local_defs.contains("Invariant") {
                self.real_invariant = true;
            }
        }
        visit::visit_path(self, p);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func {
            if let Some((ty, method)) = tail2(&p.path) {
                if method == "new" {
                    // `Invariant::new(...)` — a real invariant is constructed.
                    if ty == "Invariant" && !self.local_defs.contains("Invariant") {
                        self.real_invariant = true;
                    }
                    // `InvariantEngine::new(arg)` — a real invariant only if the arg
                    // is NOT an empty collection literal. An empty engine registers
                    // nothing (and later suppresses a `.check` on it, see below).
                    if ty == "InvariantEngine" && !self.local_defs.contains("InvariantEngine") {
                        match c.args.first() {
                            Some(arg) if is_empty_collection(arg) => self.empty_engine = true,
                            Some(_) => self.real_invariant = true,
                            None => self.empty_engine = true,
                        }
                    }
                } else if INVARIANT_CHECK_METHODS.contains(&method.as_str())
                    && ty == "InvariantEngine"
                    && !self.local_defs.contains("InvariantEngine")
                {
                    // UFCS form: `InvariantEngine::check(&eng, step, &state)`. The
                    // fully-qualified type makes this unambiguous, so — unlike the
                    // receiver-syntax arm below — no arity narrowing is needed. It is
                    // an EVALUATION signal (`checked`), not construction.
                    self.checked = true;
                }
            }
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        // `engine.check(step, state)` / `engine.check_all(step, state)` — invariant
        // evaluation, even if the engine type name never appears in this file.
        // Require the real 2-argument arity: `check`/`check_all` are extremely
        // common method names, and without this an unrelated `config.check()` or
        // `resp.check(x)` would trivially satisfy the gate. The library signature
        // is fixed at two args, so this never drops a genuine engine check.
        let method = m.method.to_string();
        if INVARIANT_CHECK_METHODS.contains(&method.as_str()) && m.args.len() == 2 {
            self.checked = true;
        }
        visit::visit_expr_method_call(self, m);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // Count `assert!`-family macro invocations by the macro path's last
        // segment (so `std::assert!` and a bare `assert!` both count).
        if let Some(last) = mac.path.segments.last() {
            let name = last.ident.to_string();
            if ASSERT_MACROS.contains(&name.as_str()) {
                self.assert_macros += 1;
            }
        }
        // syn does not walk macro token streams, so `vec![Invariant::new(...)]`,
        // `assert!(eng.check(...).is_none())`, etc. would be invisible. Best-effort
        // re-parse the body as a comma-separated expression list and visit those
        // exprs (under `macro_depth`, which suppresses sim-marker recording). Non-
        // expression bodies (`matches!(x, Some(_))`, `vec![0; n]`) fail to parse
        // and are simply skipped. Over-deep files are already rejected before
        // parsing (see `exceeds_nesting_depth`); the `macro_depth` ceiling is a
        // secondary bound on this re-parse loop.
        if self.macro_depth < MAX_MACRO_DEPTH {
            if let Ok(exprs) =
                mac.parse_body_with(Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated)
            {
                self.macro_depth += 1;
                for e in &exprs {
                    self.visit_expr(e);
                }
                self.macro_depth -= 1;
            }
        }
        visit::visit_macro(self, mac);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str) -> PresenceReport {
        let mut report = PresenceReport::default();
        check_source("test.rs", src, &mut report);
        report
    }

    fn one_site(src: &str) -> Site {
        let report = analyze(src);
        assert_eq!(report.sites.len(), 1, "expected exactly one sim site");
        report.sites.into_iter().next().unwrap()
    }

    #[test]
    fn sim_without_assertions_is_missing() {
        let src = r#"
            fn run() {
                let mut sched = SimScheduler::new(0);
                let rng = SimulatedRandom::from_seed(1);
                for _ in 0..1000 { sched.step(&rng); }
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Missing);
        assert!(!site.has_invariant);
        assert_eq!(site.assert_macros, 0);
        assert!(site.sim_markers.contains(&"SimScheduler".to_string()));
        assert!(site.sim_markers.contains(&"SimulatedRandom".to_string()));
    }

    #[test]
    fn empty_invariant_engine_is_missing() {
        // The ADR's canonical vacuous shape: an engine that registers nothing.
        for empty in ["vec![]", "Vec::new()", "[]"] {
            let src = format!(
                "fn run() {{ let s = SimScheduler::new(0); let e = InvariantEngine::new({empty}); }}"
            );
            let site = one_site(&src);
            assert_eq!(site.status, Status::Missing, "empty engine `{empty}` → missing");
            assert!(site.empty_engine, "empty engine flagged for `{empty}`");
            assert!(!site.has_invariant);
        }
    }

    #[test]
    fn sim_with_raw_assert_is_raw_only() {
        let src = r#"
            fn run() {
                let mut sched = SimScheduler::new(0);
                let world = step(&mut sched);
                assert!(world.balance >= 0);
                assert_eq!(world.count, 1000);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::RawOnly);
        assert!(!site.has_invariant);
        assert_eq!(site.assert_macros, 2);
    }

    #[test]
    fn nonempty_invariant_engine_is_ok() {
        let src = r#"
            fn run() {
                let mut sched = SimScheduler::new(0);
                let eng = InvariantEngine::new(vec![
                    Invariant::new("under_cap", |w: &W| w.v <= w.cap),
                ]);
                for step in 0..1000 { eng.check(step, &world); }
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok);
        assert!(site.has_invariant);
    }

    #[test]
    fn invariant_engine_from_variable_is_ok() {
        // Engine built from a vec assembled elsewhere: the arg is a variable, not
        // an empty literal, so we must NOT call this missing.
        let src = r#"
            fn run() {
                let s = SimScheduler::new(0);
                let invs = build_invariants();
                let eng = InvariantEngine::new(invs);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok);
        assert!(site.has_invariant);
    }

    #[test]
    fn empty_engine_then_checked_is_accepted_false_ok() {
        // `InvariantEngine::new(vec![])` that is later `.check`ed IS vacuous, but we
        // classify it OK on purpose: file-global analysis cannot tell this empty
        // engine from a legit helper-checked engine in the same file, and the
        // cardinal rule forbids risking a false MISSING to catch it. Documented
        // false OK — the UNUSED empty-engine mistake is still caught (see
        // `empty_invariant_engine_is_missing`).
        let src = r#"
            fn run() {
                let s = SimScheduler::new(0);
                let eng = InvariantEngine::new(vec![]);
                for i in 0..1000 { let _ = eng.check(i, &world); }
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok, "accepted false OK, per cardinal rule");
        assert!(site.empty_engine, "still recorded as having an empty engine");
    }

    #[test]
    fn empty_engine_beside_real_helper_check_is_ok_not_missing() {
        // The regression the cardinal rule protects: an empty-engine baseline test
        // next to a real helper-checked test in the SAME file must NOT false-MISSING.
        let src = r#"
            fn baseline() {
                let s = SimScheduler::new(0);
                let eng = InvariantEngine::new(vec![]);
            }
            fn real_test() {
                let s = SimScheduler::new(0);
                let eng = make_engine();
                for i in 0..100 { let _ = eng.check(i, &world); }
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok, "helper-checked engine must keep the file OK");
    }

    #[test]
    fn invariant_via_check_method_only_is_ok() {
        // The engine came from a helper; only the `.check()` call reveals it.
        let src = r#"
            fn run() {
                let mut sched = SimScheduler::new(0);
                let eng = make_engine();
                for step in 0..10 { let _ = eng.check_all(step, &world); }
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok);
        assert!(site.has_invariant);
    }

    #[test]
    fn bare_invariant_new_construction_is_ok() {
        // Invariants collected without the engine type name present in the file.
        let src = r#"
            fn run() {
                let s = FaultSchedule::new(3);
                let invs = vec![Invariant::new("x", |_w: &u8| true)];
                drive(invs);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok);
        assert!(site.has_invariant);
    }

    #[test]
    fn qualified_use_survives_a_local_shadow() {
        // The file defines its own `SimScheduler` AND uses the real qualified type.
        // The local shadow must NOT suppress the explicitly-qualified use, so the
        // file is still detected as a site.
        let src = r#"
            struct SimScheduler;
            fn run() {
                let real = navian_dst::SimScheduler::new(0);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.sim_markers, vec!["SimScheduler"]);
        assert_eq!(site.status, Status::Missing);
    }

    #[test]
    fn qualified_empty_vec_engine_is_missing() {
        // `std::vec![]` is still an empty engine → MISSING, not OK.
        let src = r#"
            fn run() {
                let s = SimScheduler::new(0);
                let eng = InvariantEngine::new(std::vec![]);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Missing);
        assert!(site.empty_engine);
    }

    #[test]
    fn qualified_sim_path_is_detected() {
        let src = r#"
            fn run() {
                let s = navian_dst::SimScheduler::new(0);
                let f = navian_dst::FaultSchedule::new(2);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Missing);
        assert_eq!(site.sim_markers, vec!["FaultSchedule", "SimScheduler"]);
    }

    #[test]
    fn non_sim_file_is_not_a_site() {
        let src = r#"
            fn add(a: i64, b: i64) -> i64 { a + b }
            #[test] fn t() { assert_eq!(add(2, 2), 4); }
        "#;
        let report = analyze(src);
        assert!(report.sites.is_empty(), "no sim surface → not a site");
        assert_eq!(report.files_parsed, 1);
    }

    #[test]
    fn file_defining_a_marker_is_shadow_guarded() {
        // The navian-dst library itself defines `SimScheduler` — defining a name
        // must not count as using the navian-dst type.
        let src = r#"
            pub struct SimScheduler { seed: u64 }
            impl SimScheduler {
                pub fn new(seed: u64) -> Self { Self { seed } }
            }
        "#;
        let report = analyze(src);
        assert!(
            report.sites.is_empty(),
            "a file DEFINING SimScheduler is not a sim site"
        );
    }

    #[test]
    fn partial_shadow_still_flags_other_markers() {
        // Defines SimScheduler locally (shadowed) but genuinely uses FaultSchedule.
        let src = r#"
            struct SimScheduler;
            fn run() {
                let f = FaultSchedule::new(1);
                let _ = SimScheduler;
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.sim_markers, vec!["FaultSchedule"]);
        assert_eq!(site.status, Status::Missing);
    }

    #[test]
    fn report_tallies_and_gate_predicates() {
        let mut report = PresenceReport::default();
        check_source(
            "miss.rs",
            "fn a() { let s = SimScheduler::new(0); }",
            &mut report,
        );
        check_source(
            "raw.rs",
            "fn b() { let s = SimScheduler::new(0); assert!(true); }",
            &mut report,
        );
        check_source(
            "ok.rs",
            "fn c() { let s = SimScheduler::new(0); let e = InvariantEngine::new(vec![Invariant::new(\"i\", |_w: &u8| true)]); }",
            &mut report,
        );
        assert_eq!(report.tally(Status::Missing), 1);
        assert_eq!(report.tally(Status::RawOnly), 1);
        assert_eq!(report.tally(Status::Ok), 1);
        assert!(report.any_missing());
        assert!(report.any_raw_only());
        assert!(!report.uncertifiable());
    }

    #[test]
    fn sim_line_points_at_first_sim_reference() {
        let src = "fn r() {\n    let x = 1;\n    let s = SimScheduler::new(0);\n}";
        let site = one_site(src);
        assert_eq!(site.sim_line, 3, "sim reference is on line 3");
    }

    #[test]
    fn ufcs_engine_check_is_ok() {
        // `InvariantEngine::check(&eng, step, &state)` — the associated-function
        // form, not receiver syntax. Must still count as evaluation.
        let src = r#"
            fn run() {
                let s = SimScheduler::new(0);
                let eng = make_engine();
                InvariantEngine::check(&eng, 0, &world);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok);
        assert!(site.has_invariant);
    }

    #[test]
    fn unrelated_check_call_does_not_satisfy_gate() {
        // A vacuous sim that happens to call an unrelated `.check()` (wrong arity)
        // must NOT be marked OK — otherwise the gate is trivially defeated.
        let src = r#"
            fn run() {
                let s = SimScheduler::new(0);
                config.check();          // 0 args
                resp.check(&thing);      // 1 arg
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Missing);
        assert!(!site.has_invariant);
    }

    #[test]
    fn two_arg_check_on_helper_engine_is_ok() {
        // The real receiver-syntax arity (`check(step, state)`) still counts.
        let src = r#"
            fn run() {
                let s = SimScheduler::new(0);
                let eng = helper();
                let _ = eng.check(3, &world);
            }
        "#;
        let site = one_site(src);
        assert_eq!(site.status, Status::Ok);
    }

    #[test]
    fn waiver_marker_exempts_a_delegated_site() {
        // A file that drives the sim and delegates checking to a harness helper.
        // It asserts nothing locally, so it is MISSING — but the waiver keeps it
        // out of the gate while remaining visible in the report.
        let src = r#"
            // navian-dst:invariants-elsewhere — checked in the shared harness.
            fn scenario_a() {
                let mut sched = SimScheduler::new(1);
                let rng = SimulatedRandom::from_seed(2);
                run_checked(&mut sched, &rng);
            }
        "#;
        let mut report = PresenceReport::default();
        check_source("scenario_a.rs", src, &mut report);
        let site = &report.sites[0];
        assert_eq!(site.status, Status::Missing, "still computed as missing");
        assert!(site.waived, "but marked waived");
        assert!(!report.any_missing(), "waived site does not fail the gate");
    }

    #[test]
    fn moderate_nesting_under_cap_still_parses() {
        // Well under MAX_NESTING_DEPTH: parses normally and is a sim site.
        let mut src = String::from("fn run() { let s = SimScheduler::new(0); let _ = ");
        let n = 200;
        for _ in 0..n {
            src.push_str("vec![");
        }
        src.push('0');
        for _ in 0..n {
            src.push(']');
        }
        src.push_str("; }");
        let mut report = PresenceReport::default();
        check_source("nested.rs", &src, &mut report);
        assert_eq!(report.parse_failures.len(), 0, "200-deep is fine");
        assert_eq!(report.sites.len(), 1);
    }

    #[test]
    fn pathological_nesting_is_skipped_not_crashed() {
        // Over MAX_NESTING_DEPTH — must be rejected up front as a parse failure,
        // never handed to syn (which would overflow the stack and abort). Uses
        // bare parens, which touch no macro re-parse path at all.
        let depth = MAX_NESTING_DEPTH + 50;
        let mut src = String::from("fn run() { let s = SimScheduler::new(0); let _ = ");
        src.push_str(&"(".repeat(depth));
        src.push('0');
        src.push_str(&")".repeat(depth));
        src.push_str("; }");
        let mut report = PresenceReport::default();
        check_source("bomb.rs", &src, &mut report);
        assert_eq!(report.parse_failures, vec!["bomb.rs".to_string()]);
        assert_eq!(report.files_parsed, 0);
        assert!(report.sites.is_empty());
        assert!(report.uncertifiable(), "an over-deep file makes a gate uncertifiable");
    }
}
