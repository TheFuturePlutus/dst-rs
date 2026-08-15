//! Determinism-leak scanner.
//!
//! Parses Rust source with [`syn`] and walks the AST looking for calls that
//! introduce non-determinism — wall-clock time, randomness, network I/O,
//! unstructured concurrency, environment/process reads, filesystem enumeration,
//! hash-container iteration order, and relaxed atomics.
//!
//! ## What this is (and is NOT)
//!
//! This is a **name-based heuristic lint**, not a sound analyzer. It flags calls
//! and paths whose *recognizable name or tail segments* match a known
//! determinism source (`SystemTime::now`, `thread_rng`, `OsRng`, `reqwest::*`,
//! `tokio::spawn`, …). It does only *lightweight* name resolution: a per-file
//! pre-pass collects the file's own item definitions and `use … as` aliases (see
//! "Shadow & qualifier guards" below). It is a starting point for making a crate
//! replayable, not a proof that one is deterministic.
//!
//! ## Confidence tiers
//!
//! A name-based lint cannot honestly claim uniform precision, so every finding
//! carries a [`Confidence`] tier that says how much to trust it:
//!
//! * [`High`](Confidence::High) — well-known std/ecosystem determinism sources
//!   that are unlikely to be shadowed by a same-named user symbol
//!   (`SystemTime::now`, `Instant::now`, `thread_rng`, `OsRng`, `getrandom`,
//!   `TcpStream::connect`, `thread::spawn`). **This is the tier the CI gate
//!   (`--deny`) fails on by default.**
//! * [`Medium`](Confidence::Medium) — recognizable but more shadowable or
//!   slightly lower-signal (`chrono::Utc::now`, `uuid::Uuid::new_v4`,
//!   `std::env::var`, `read_dir`).
//! * [`Advisory`](Confidence::Advisory) — fuzzy or often-intentional
//!   (`.gen()`, `HashMap`/`HashSet` iteration order, `Ordering::Relaxed`,
//!   `tokio::spawn`, rayon `par_iter`).
//!
//! `--deny` fails on High by default; `--deny-level {medium,advisory}` lowers
//! the threshold (advisory = fail on anything).
//!
//! ## Shadow & qualifier guards (why High never fires on benign code)
//!
//! `scan --deny` is a CI gate, so a High rule must **never** fire on
//! benign/shadowed/foreign code. Every High rule is therefore guarded:
//!
//! * **Qualifier guard.** A qualified High match is only High when the path is
//!   rooted at a *distinctive* qualifier for that rule: `std` for the std
//!   sources (`SystemTime`/`Instant`, `TcpStream`/`UdpSocket`/`TcpListener`,
//!   `thread::spawn`) and `rand` for `OsRng`/`thread_rng`. Generic std
//!   *sub-module* names (`time`, `net`) are deliberately NOT accepted as
//!   qualifiers, because a user `mod net`/`mod time` or `crate::net::TcpStream`
//!   would otherwise escape the guard and fire High. A **foreign** qualifier
//!   (`mymod::`, `mock::TcpStream`, `RngKind::OsRng`, `crate::rng::OsRng`,
//!   `crate::net::TcpStream`) produces **no High finding**. The consequence is a
//!   documented false negative: a module-qualified `use std::time; time::now()`
//!   without `std` in the written path is not High — write `std::time::…` or the
//!   bare imported form to keep the High signal.
//! * **Local-shadow guard.** A per-file pre-pass collects `struct`/`enum`/`fn`/
//!   `trait`/`type`/`mod`/`const`/`static`/`union` names (and, separately, the
//!   set of `mod` names). A **bare** High match (single-segment, e.g. `OsRng` or
//!   `thread_rng()`) whose name is defined in the same file is treated as the
//!   user's own symbol and dropped — so a local `struct SystemTime { fn now() }`
//!   or `fn thread_rng()` never trips the gate. The crate-named rules
//!   (`reqwest`, `tokio::net`, `getrandom`) are additionally suppressed when the
//!   crate segment is a locally-defined module (`mod reqwest { … }`).
//! * **Alias resolution.** The same pre-pass records `use path::Real as Alias;`
//!   and rewrites a leading `Alias::` segment back to its canonical path before
//!   classifying, so `use std::time::SystemTime as Sys; Sys::now()` **is**
//!   detected — while `use mymod::SystemTime as Sys; Sys::now()` is not (its
//!   canonical path is foreign-qualified).
//!
//! Medium/Advisory rules are intentionally left unguarded: they never fail the
//! default gate, so a same-named false positive there is cheap and is the safe
//! direction for a replay-safety lint. A renamed import of a non-aliased *bare*
//! source (e.g. `use std::time::SystemTime;` then `SystemTime::now()`) is still
//! flagged High; a value bound through an unseen alias with no `use … as` in the
//! same file remains a documented false negative.
//!
//! ## Rule IDs
//!
//! Every finding also carries a stable [`Rule`] with an ID like `DST-TIME-001`.
//! The full catalog (id → category → confidence → description) lives in
//! [`rules::CATALOG`] as the single source of truth; the detection logic returns
//! `&'static Rule` values from that table.
//!
//! ## Location reporting
//!
//! Line/column come from `proc_macro2`'s span locations (the `span-locations`
//! feature is enabled, and we parse from an in-memory `&str`, which is the
//! configuration under which `Span::start()`/`end()` carry real coordinates).
//! `line` is 1-based; `column` from `proc_macro2` is 0-based, so we report
//! `column + 1` to match what editors show. Snippets are sliced directly from
//! the source text over the node's span, so they are the real bytes on disk —
//! the original spelling (e.g. `Sys::now()`), not the alias-resolved path.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use walkdir::WalkDir;

/// The kind of non-determinism a leak introduces.
///
/// Matching is by name, so any variant can be a false positive on a same-named
/// user symbol. Severity is carried separately by [`Confidence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Time,
    Random,
    Network,
    Concurrency,
    /// `HashMap`/`HashSet` iteration order.
    Iteration,
    /// Environment / process reads (`env::var`, `process::id`, `Command`).
    Env,
    /// Filesystem enumeration (`read_dir`, `glob`, tempfiles).
    Filesystem,
    /// Relaxed atomics (`Ordering::Relaxed`).
    Atomic,
}

impl Category {
    /// A short, upper-case label used in the human report.
    pub fn label(self) -> &'static str {
        match self {
            Category::Time => "TIME",
            Category::Random => "RANDOM",
            Category::Network => "NETWORK",
            Category::Concurrency => "CONCURRENCY",
            Category::Iteration => "ITERATION",
            Category::Env => "ENV",
            Category::Filesystem => "FILESYSTEM",
            Category::Atomic => "ATOMIC",
        }
    }

    /// Every category, in report order.
    pub fn all() -> &'static [Category] {
        &[
            Category::Time,
            Category::Random,
            Category::Network,
            Category::Concurrency,
            Category::Iteration,
            Category::Env,
            Category::Filesystem,
            Category::Atomic,
        ]
    }
}

/// How much to trust a finding.
///
/// A name-based lint has genuinely different precision across sources, and the
/// gate should not fail on the fuzzy ones. `--deny` fails on [`High`] by
/// default; `--deny-level` can lower the threshold. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    /// Well-known, unlikely-to-be-shadowed source. The CI gate fails on these.
    High,
    /// Recognizable but more shadowable / lower signal.
    Medium,
    /// Fuzzy or often-intentional; surfaced for review, never gates by default.
    Advisory,
}

impl Confidence {
    /// Severity rank for threshold comparisons (higher = more severe).
    ///
    /// A finding fails a `--deny`/`--deny-level` gate when its rank is `>=` the
    /// threshold's rank.
    pub fn rank(self) -> u8 {
        match self {
            Confidence::High => 3,
            Confidence::Medium => 2,
            Confidence::Advisory => 1,
        }
    }

    /// Lower-case label used in reports and for `--deny-level` parsing.
    pub fn label(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Advisory => "advisory",
        }
    }

    /// SARIF `result.level` for this tier: `error` / `warning` / `note`.
    ///
    /// The mapping is the conventional SARIF severity ladder — a High finding
    /// is the tier the default gate fails on, so it maps to `error`.
    pub fn sarif_level(self) -> &'static str {
        match self {
            Confidence::High => "error",
            Confidence::Medium => "warning",
            Confidence::Advisory => "note",
        }
    }

    /// Parse a `--deny-level` value.
    pub fn parse(s: &str) -> Option<Confidence> {
        match s {
            "high" => Some(Confidence::High),
            "medium" => Some(Confidence::Medium),
            "advisory" => Some(Confidence::Advisory),
            _ => None,
        }
    }
}

/// A stable rule: the single unit of "what the scanner recognizes".
///
/// The [`rules::CATALOG`] table is the source of truth; detection returns
/// `&'static Rule` values from it. `id` is stable across releases so downstream
/// tooling can pin or suppress specific rules.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    /// Stable identifier, e.g. `DST-TIME-001`.
    pub id: &'static str,
    /// The category this rule belongs to.
    pub category: Category,
    /// How much to trust a match.
    pub confidence: Confidence,
    /// One-line human description of the source.
    pub description: &'static str,
}

/// Look up a rule by its stable id in [`rules::CATALOG`].
pub fn rule_by_id(id: &str) -> Option<&'static Rule> {
    rules::CATALOG.iter().copied().find(|r| r.id == id)
}

/// The rule catalog — the single source of truth for
/// `id → category → confidence → description`.
pub mod rules {
    use super::{
        Category::{Atomic, Concurrency, Env, Filesystem, Iteration, Network, Random, Time},
        Confidence::{Advisory, High, Medium},
        Rule,
    };

    // ── Time ──────────────────────────────────────────────────────────────
    /// `SystemTime::now` — wall-clock read.
    pub const TIME_SYSTEMTIME_NOW: Rule = Rule {
        id: "DST-TIME-001",
        category: Time,
        confidence: High,
        description: "SystemTime::now — wall-clock read",
    };
    /// `Instant::now` — monotonic-clock read.
    pub const TIME_INSTANT_NOW: Rule = Rule {
        id: "DST-TIME-002",
        category: Time,
        confidence: High,
        description: "Instant::now — monotonic-clock read",
    };
    /// `chrono::Utc::now` / `chrono::Local::now`.
    pub const TIME_CHRONO_NOW: Rule = Rule {
        id: "DST-TIME-003",
        category: Time,
        confidence: Medium,
        description: "chrono Utc::now / Local::now — wall-clock read",
    };
    /// `time::OffsetDateTime::now_utc` / `now_local`.
    pub const TIME_OFFSETDATETIME_NOW: Rule = Rule {
        id: "DST-TIME-004",
        category: Time,
        confidence: Medium,
        description: "time::OffsetDateTime::now_utc — wall-clock read",
    };
    /// `tokio::time::Instant::now`.
    pub const TIME_TOKIO_INSTANT_NOW: Rule = Rule {
        id: "DST-TIME-005",
        category: Time,
        confidence: Medium,
        description: "tokio::time::Instant::now — runtime-clock read",
    };
    /// `std::thread::sleep`.
    pub const TIME_THREAD_SLEEP: Rule = Rule {
        id: "DST-TIME-006",
        category: Time,
        confidence: Medium,
        description: "thread::sleep — timing-dependent OS sleep",
    };
    /// `tokio::time::{sleep, sleep_until, interval, timeout}`.
    pub const TIME_TOKIO_TIMER: Rule = Rule {
        id: "DST-TIME-007",
        category: Time,
        confidence: Medium,
        description: "tokio::time timer (sleep/sleep_until/interval/timeout)",
    };

    // ── Random ────────────────────────────────────────────────────────────
    /// `rand::thread_rng`.
    pub const RAND_THREAD_RNG: Rule = Rule {
        id: "DST-RAND-001",
        category: Random,
        confidence: High,
        description: "rand::thread_rng — OS-seeded PRNG handle",
    };
    /// `rand::random`.
    pub const RAND_RANDOM: Rule = Rule {
        id: "DST-RAND-002",
        category: Random,
        confidence: High,
        description: "rand::random — one-shot OS-seeded RNG",
    };
    /// `OsRng` — value or `default()` form.
    pub const RAND_OSRNG: Rule = Rule {
        id: "DST-RAND-003",
        category: Random,
        confidence: High,
        description: "OsRng — direct OS entropy source",
    };
    /// `<Rng>::from_entropy`.
    pub const RAND_FROM_ENTROPY: Rule = Rule {
        id: "DST-RAND-004",
        category: Random,
        confidence: Medium,
        description: "PRNG::from_entropy — seed a PRNG from OS entropy",
    };
    /// `getrandom::getrandom`.
    pub const RAND_GETRANDOM: Rule = Rule {
        id: "DST-RAND-005",
        category: Random,
        confidence: High,
        description: "getrandom — raw OS entropy",
    };
    /// `uuid::Uuid::new_v4` / `now_v7`.
    pub const RAND_UUID: Rule = Rule {
        id: "DST-RAND-006",
        category: Random,
        confidence: Medium,
        description: "uuid Uuid::new_v4 / now_v7 — random UUID",
    };
    /// `fastrand::*`.
    pub const RAND_FASTRAND: Rule = Rule {
        id: "DST-RAND-007",
        category: Random,
        confidence: Medium,
        description: "fastrand — thread-local PRNG",
    };
    /// `.gen()` / `.gen_range()` method idiom.
    pub const RAND_GEN_METHOD: Rule = Rule {
        id: "DST-RAND-008",
        category: Random,
        confidence: Advisory,
        description: "rand .gen()/.gen_range() method idiom (name-based)",
    };

    // ── Network ───────────────────────────────────────────────────────────
    /// `std::net::{TcpStream, TcpListener, UdpSocket}`.
    pub const NET_STD: Rule = Rule {
        id: "DST-NET-001",
        category: Network,
        confidence: High,
        description: "std::net TcpStream/TcpListener/UdpSocket",
    };
    /// `reqwest::*`.
    pub const NET_REQWEST: Rule = Rule {
        id: "DST-NET-002",
        category: Network,
        confidence: High,
        description: "reqwest — HTTP client",
    };
    /// `tokio::net::*`.
    pub const NET_TOKIO: Rule = Rule {
        id: "DST-NET-003",
        category: Network,
        confidence: High,
        description: "tokio::net — async socket",
    };

    // ── Concurrency ───────────────────────────────────────────────────────
    /// `std::thread::spawn`.
    pub const CONC_THREAD_SPAWN: Rule = Rule {
        id: "DST-CONC-001",
        category: Concurrency,
        confidence: High,
        description: "thread::spawn — unstructured OS thread",
    };
    /// `tokio::spawn` / `tokio::task::spawn`.
    pub const CONC_TOKIO_SPAWN: Rule = Rule {
        id: "DST-CONC-002",
        category: Concurrency,
        confidence: Advisory,
        description: "tokio::spawn — detached async task (scheduling order)",
    };
    /// rayon `par_iter` / `into_par_iter`.
    pub const CONC_RAYON: Rule = Rule {
        id: "DST-CONC-003",
        category: Concurrency,
        confidence: Advisory,
        description: "rayon par_iter — parallel iteration (order/reduction)",
    };

    // ── Iteration order ───────────────────────────────────────────────────
    /// `HashMap`/`HashSet` iteration.
    pub const ITER_HASH: Rule = Rule {
        id: "DST-ITER-001",
        category: Iteration,
        confidence: Advisory,
        description: "HashMap/HashSet iteration order — prefer BTreeMap/IndexMap",
    };

    // ── Env / Process ─────────────────────────────────────────────────────
    /// `std::env::var` / `vars` / `args`.
    pub const ENV_VAR: Rule = Rule {
        id: "DST-ENV-001",
        category: Env,
        confidence: Medium,
        description: "std::env var/vars/args — ambient process input",
    };
    /// `std::process::id`.
    pub const ENV_PROCESS_ID: Rule = Rule {
        id: "DST-ENV-002",
        category: Env,
        confidence: Medium,
        description: "std::process::id — OS-assigned PID",
    };
    /// `std::process::Command`.
    pub const ENV_COMMAND: Rule = Rule {
        id: "DST-ENV-003",
        category: Env,
        confidence: Medium,
        description: "std::process::Command — external process",
    };

    // ── Filesystem ────────────────────────────────────────────────────────
    /// `std::fs::read_dir`.
    pub const FS_READ_DIR: Rule = Rule {
        id: "DST-FS-001",
        category: Filesystem,
        confidence: Medium,
        description: "std::fs::read_dir — unordered directory listing",
    };
    /// `glob::glob`.
    pub const FS_GLOB: Rule = Rule {
        id: "DST-FS-002",
        category: Filesystem,
        confidence: Medium,
        description: "glob::glob — filesystem-ordered matches",
    };
    /// `tempfile` / `TempDir::new` / `NamedTempFile::new`.
    pub const FS_TEMPFILE: Rule = Rule {
        id: "DST-FS-003",
        category: Filesystem,
        confidence: Medium,
        description: "tempfile / TempDir::new — random temp path",
    };

    // ── Atomics ───────────────────────────────────────────────────────────
    /// `Ordering::Relaxed`.
    pub const ATOMIC_RELAXED: Rule = Rule {
        id: "DST-ATOMIC-001",
        category: Atomic,
        confidence: Advisory,
        description: "Ordering::Relaxed — unordered atomic (observation order)",
    };

    /// The full catalog, in ID order. Single source of truth.
    pub const CATALOG: &[&Rule] = &[
        &TIME_SYSTEMTIME_NOW,
        &TIME_INSTANT_NOW,
        &TIME_CHRONO_NOW,
        &TIME_OFFSETDATETIME_NOW,
        &TIME_TOKIO_INSTANT_NOW,
        &TIME_THREAD_SLEEP,
        &TIME_TOKIO_TIMER,
        &RAND_THREAD_RNG,
        &RAND_RANDOM,
        &RAND_OSRNG,
        &RAND_FROM_ENTROPY,
        &RAND_GETRANDOM,
        &RAND_UUID,
        &RAND_FASTRAND,
        &RAND_GEN_METHOD,
        &NET_STD,
        &NET_REQWEST,
        &NET_TOKIO,
        &CONC_THREAD_SPAWN,
        &CONC_TOKIO_SPAWN,
        &CONC_RAYON,
        &ITER_HASH,
        &ENV_VAR,
        &ENV_PROCESS_ID,
        &ENV_COMMAND,
        &FS_READ_DIR,
        &FS_GLOB,
        &FS_TEMPFILE,
        &ATOMIC_RELAXED,
    ];
}

/// A single detected determinism leak.
#[derive(Debug, Clone, Serialize)]
pub struct Leak {
    /// Stable rule id, e.g. `DST-TIME-001`.
    pub rule_id: &'static str,
    /// How much to trust the finding.
    pub confidence: Confidence,
    /// The kind of non-determinism.
    pub category: Category,
    pub file: String,
    pub line: usize,
    pub col: usize,
    /// Enclosing function name, if the leak sits inside a `fn`.
    pub function: Option<String>,
    pub snippet: String,
}

/// A stable, **line-independent** fingerprint of a finding.
///
/// This is the shared identity used by both the SARIF `partialFingerprints`
/// (`dstFingerprintV1`) and the `baseline` suppression store, so the two always
/// agree on what "the same finding" means.
///
/// The hash is taken over `rule_id + enclosing function + normalized snippet`
/// and deliberately does **NOT** include the file path, line, or column — so a
/// finding that MOVES (blank lines inserted above it, the enclosing block
/// shifted, the file reformatted) keeps the same fingerprint and stays
/// suppressed. The snippet is whitespace-normalized (runs of whitespace
/// collapsed to a single space, ends trimmed) so pure re-indentation does not
/// change the identity either.
///
/// The digest is a 64-bit FNV-1a rendered as 16 lowercase hex chars. FNV-1a is
/// used rather than [`std::hash::DefaultHasher`] on purpose: `DefaultHasher`'s
/// output is explicitly NOT guaranteed stable across Rust releases, and a
/// baseline must survive a toolchain bump.
pub fn fingerprint(leak: &Leak) -> String {
    let func = leak.function.as_deref().unwrap_or("");
    let snippet_norm = leak.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
    // U+001F (unit separator) delimits the fields so distinct field boundaries
    // can never be forged by adjacent content (e.g. a snippet that ends in the
    // same text a function name begins with).
    let composed = format!("{}\u{1f}{}\u{1f}{}", leak.rule_id, func, snippet_norm);
    format!("{:016x}", fnv1a64(composed.as_bytes()))
}

/// 64-bit FNV-1a. Deterministic and stable across platforms/toolchains, which
/// is why the fingerprint uses it (see [`fingerprint`]).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Aggregate result of a scan run.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub leaks: Vec<Leak>,
    pub files_scanned: usize,
    pub files_parsed: usize,
    /// Files that were found but failed to READ or PARSE (skipped, never fatal
    /// to the scan — but they make a `--deny` gate uncertifiable; see
    /// [`ScanReport::uncertifiable`]).
    pub parse_failures: Vec<String>,
}

impl ScanReport {
    /// The highest-severity finding at or above `threshold`, if any — i.e. does
    /// a `--deny`/`--deny-level` gate at `threshold` bite?
    pub fn any_at_or_above(&self, threshold: Confidence) -> bool {
        self.leaks
            .iter()
            .any(|l| l.confidence.rank() >= threshold.rank())
    }

    /// True when a target file could not be read or parsed, so a `--deny` gate
    /// cannot honestly certify the tree clean — the caller should exit 2 rather
    /// than pass green on files it never saw.
    pub fn uncertifiable(&self) -> bool {
        !self.parse_failures.is_empty()
    }
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

    // Pre-pass: collect the file's own item definitions and `use … as` aliases,
    // used by the shadow/qualifier guards below.
    let mut defs = Defs::default();
    defs.visit_file(&ast);
    let ctx = FileCtx {
        local_names: defs.local_names,
        mod_names: defs.mod_names,
        aliases: defs.aliases,
    };

    let lines: Vec<Vec<char>> = content.lines().map(|l| l.chars().collect()).collect();
    let mut visitor = LeakVisitor {
        file,
        lines: &lines,
        ctx: &ctx,
        fn_stack: Vec::new(),
        map_vars: vec![HashSet::new()],
        leaks: Vec::new(),
    };
    visitor.visit_file(&ast);
    report.leaks.append(&mut visitor.leaks);
}

// ── Per-file context: local definitions + import aliases ─────────────────────

/// Per-file name context, built by [`Defs`] before the leak walk.
///
/// NOTE: `local_names`/`mod_names` are flat across the whole file (all modules
/// merged). A file that both defines `struct OsRng` in one module and genuinely
/// uses `rand`'s `OsRng` bare in another would suppress the real bare use — an
/// accepted, rare false negative that keeps the shadow guard simple and errs
/// toward *not* emitting a High false positive.
struct FileCtx {
    /// Names of items defined in this file (struct/enum/fn/trait/type/mod/…),
    /// used by the bare local-shadow guard.
    local_names: HashSet<String>,
    /// Names of `mod`s defined in this file, used to shadow-guard the
    /// crate-named rules (`reqwest`/`tokio`/`getrandom`). Kept separate from
    /// `local_names` so a `fn getrandom` inside some module does not suppress a
    /// genuine `getrandom::getrandom` elsewhere in the same file.
    mod_names: HashSet<String>,
    /// `use path::Real as Alias;` → the canonical segments of `path::Real`.
    aliases: HashMap<String, Vec<String>>,
}

impl FileCtx {
    /// Rewrite a leading alias segment back to its canonical path, so an aliased
    /// import classifies as the source it really is. Only the first segment is
    /// substituted (that is where a `use … as` alias appears in a path).
    fn resolve(&self, segs: &[String]) -> Vec<String> {
        if let Some(first) = segs.first() {
            if let Some(canon) = self.aliases.get(first) {
                let mut out = canon.clone();
                out.extend_from_slice(&segs[1..]);
                return out;
            }
        }
        segs.to_vec()
    }
}

/// Pre-pass visitor that gathers item definitions and import aliases.
#[derive(Default)]
struct Defs {
    local_names: HashSet<String>,
    mod_names: HashSet<String>,
    aliases: HashMap<String, Vec<String>>,
}

impl Defs {
    fn collect_use(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.collect_use(&p.tree, prefix);
                prefix.pop();
            }
            // A plain `use a::b::Name;` is NOT a shadow and NOT an alias — do not
            // record it (recording `Name` would wrongly suppress a real bare
            // source imported by its own name).
            syn::UseTree::Name(_) | syn::UseTree::Glob(_) => {}
            syn::UseTree::Rename(r) => {
                let alias = r.rename.to_string();
                if alias != "_" {
                    let mut canon = prefix.clone();
                    canon.push(r.ident.to_string());
                    self.aliases.insert(alias, canon);
                }
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    self.collect_use(t, prefix);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for Defs {
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.local_names.insert(i.ident.to_string());
        visit::visit_item_struct(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.local_names.insert(i.ident.to_string());
        visit::visit_item_enum(self, i);
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.local_names.insert(i.ident.to_string());
        visit::visit_item_union(self, i);
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.local_names.insert(i.sig.ident.to_string());
        visit::visit_item_fn(self, i);
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.local_names.insert(i.ident.to_string());
        visit::visit_item_trait(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.local_names.insert(i.ident.to_string());
        visit::visit_item_type(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.local_names.insert(i.ident.to_string());
        self.mod_names.insert(i.ident.to_string());
        visit::visit_item_mod(self, i);
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.local_names.insert(i.ident.to_string());
        visit::visit_item_const(self, i);
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.local_names.insert(i.ident.to_string());
        visit::visit_item_static(self, i);
    }
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        self.collect_use(&i.tree, &mut Vec::new());
    }
}

// ── Classification ─────────────────────────────────────────────────────────
//
// A NAME-BASED heuristic, not a sound analyzer. Matching is on the recognizable
// name or tail segments of a call/path. High rules are additionally guarded by
// [`guard_high`] (qualifier + local-shadow) so the CI gate never fires on
// benign/shadowed/foreign code. A leading `::` is transparent.

/// Recognized qualifier segments per High rule family (see module docs).
///
/// These are deliberately *distinctive* roots (`std`, `rand`) — NOT generic std
/// sub-module names like `time`/`net`, which collide with common user module
/// names and would let `crate::net::TcpStream` / a local `mod time` escape the
/// guard and fire High.
const Q_TIME: &[&str] = &["std"];
const Q_RAND: &[&str] = &["rand"];
const Q_NET: &[&str] = &["std"];
const Q_THREAD: &[&str] = &["std"];

/// Apply the qualifier + local-shadow guards to a candidate High match.
///
/// `key_idx` is the index of the rule's distinctive segment (e.g. `SystemTime`,
/// `OsRng`, `TcpStream`, `thread_rng`). Returns:
/// * `None` — the match is dropped: it is either a foreign-qualified path (no
///   recognized qualifier) or a bare name that is locally defined (the user's
///   own symbol). Either way, **no High finding** is emitted.
/// * `Some(rule)` — a genuine High match (bare-and-unshadowed, or carrying a
///   recognized qualifier).
fn guard_high(
    rule: &'static Rule,
    segs: &[String],
    key_idx: usize,
    strong: &[&str],
    ctx: &FileCtx,
) -> Option<&'static Rule> {
    let key = segs[key_idx].as_str();
    if key_idx == 0 {
        // Bare, single-qualifier match (e.g. `OsRng`, `thread_rng()`): safe
        // unless the same file defines that name, in which case it is the
        // user's own symbol, not the std/ecosystem source.
        if ctx.local_names.contains(key) {
            return None;
        }
        return Some(rule);
    }
    // Qualified: require a recognized distinctive qualifier (`std`/`rand`)
    // somewhere in the path that is NOT itself a locally-defined module. A
    // foreign qualifier (`mymod::`, `mock::`, `RngKind::`, `crate::rng::`) — or a
    // local `mod rand`/`mod std` shadowing the real one — yields no High finding.
    if segs.iter().enumerate().any(|(i, s)| {
        i != key_idx && strong.contains(&s.as_str()) && !ctx.mod_names.contains(s.as_str())
    }) {
        Some(rule)
    } else {
        None
    }
}

/// Classify a call path (the segments of `a::b::c` in `a::b::c(...)`), with the
/// per-file context available for guarding High matches. `segs` are already
/// alias-resolved. Returns the matched [`Rule`], else `None`.
fn classify_call(segs: &[String], ctx: &FileCtx) -> Option<&'static Rule> {
    if segs.is_empty() {
        return None;
    }
    let n = segs.len();
    let last = segs[n - 1].as_str();
    let prev = if n >= 2 { segs[n - 2].as_str() } else { "" };
    let first = segs[0].as_str();
    let has = |name: &str| segs.iter().any(|s| s == name);

    // ── Time ──
    if prev == "SystemTime" && last == "now" {
        return guard_high(&rules::TIME_SYSTEMTIME_NOW, segs, n - 2, Q_TIME, ctx);
    }
    if prev == "Instant" && last == "now" {
        // `tokio::time::Instant::now` is the runtime clock (Medium, unguarded);
        // a std/bare `Instant::now` is High (guarded).
        if has("tokio") {
            return Some(&rules::TIME_TOKIO_INSTANT_NOW);
        }
        return guard_high(&rules::TIME_INSTANT_NOW, segs, n - 2, Q_TIME, ctx);
    }
    if matches!(prev, "Utc" | "Local") && last == "now" {
        return Some(&rules::TIME_CHRONO_NOW);
    }
    if prev == "OffsetDateTime" && matches!(last, "now_utc" | "now_local") {
        return Some(&rules::TIME_OFFSETDATETIME_NOW);
    }
    if prev == "thread" && last == "sleep" {
        return Some(&rules::TIME_THREAD_SLEEP);
    }
    // tokio::time::{sleep, sleep_until, interval, timeout}. Require the `time`
    // segment so a bare user `sleep()`/`timeout()` is not flagged.
    if prev == "time" && matches!(last, "sleep" | "sleep_until" | "interval" | "timeout") {
        return Some(&rules::TIME_TOKIO_TIMER);
    }

    // ── Random ──
    // `rand::random()` — inherently `rand`-qualified (prev == "rand"), so High.
    if prev == "rand" && last == "random" {
        return Some(&rules::RAND_RANDOM);
    }
    // `thread_rng` — bare (likely `use rand::thread_rng`) or `rand`-qualified.
    if last == "thread_rng" && (n == 1 || has("rand")) {
        return guard_high(&rules::RAND_THREAD_RNG, segs, n - 1, Q_RAND, ctx);
    }
    if first == "fastrand" {
        return Some(&rules::RAND_FASTRAND);
    }
    // `getrandom::getrandom(...)` — require the crate qualifier (n >= 2). A bare
    // `getrandom(...)` is too easily a local helper and is intentionally NOT
    // matched; a foreign `mycrate::getrandom(...)` has `first != "getrandom"`.
    // A local `mod getrandom { … }` shadows the crate → drop.
    if first == "getrandom" && n >= 2 && !ctx.mod_names.contains("getrandom") {
        return Some(&rules::RAND_GETRANDOM);
    }
    if prev == "Uuid" && matches!(last, "new_v4" | "now_v7") {
        return Some(&rules::RAND_UUID);
    }
    // `<receiver>::from_entropy()` — Medium; a bare free `from_entropy()` carries
    // no signal that it is an RNG constructor and is not matched.
    if n >= 2 && last == "from_entropy" {
        return Some(&rules::RAND_FROM_ENTROPY);
    }
    // Call form `OsRng::default()` (bare or `rand::rngs::OsRng::default()`).
    if prev == "OsRng" && last == "default" {
        return guard_high(&rules::RAND_OSRNG, segs, n - 2, Q_RAND, ctx);
    }

    // ── Network ──
    // Crate-named rules trust the crate segment, but a local `mod reqwest`/
    // `mod tokio` shadows it → drop to avoid a High false positive.
    if first == "reqwest" && !ctx.mod_names.contains("reqwest") {
        return Some(&rules::NET_REQWEST);
    }
    // tokio::net::* — checked before the std TcpStream/UdpSocket family so a
    // `tokio::net::TcpStream` is attributed to the tokio rule, not std::net.
    if n >= 2 && first == "tokio" && segs[1] == "net" && !ctx.mod_names.contains("tokio") {
        return Some(&rules::NET_TOKIO);
    }
    if let Some(idx) = segs
        .iter()
        .position(|s| matches!(s.as_str(), "TcpStream" | "TcpListener" | "UdpSocket"))
    {
        return guard_high(&rules::NET_STD, segs, idx, Q_NET, ctx);
    }

    // ── Env / Process (Medium, unguarded) ──
    if has("env") && matches!(last, "var" | "vars" | "var_os" | "vars_os" | "args" | "args_os") {
        return Some(&rules::ENV_VAR);
    }
    if has("process") && last == "id" {
        return Some(&rules::ENV_PROCESS_ID);
    }
    if prev == "Command" {
        return Some(&rules::ENV_COMMAND);
    }

    // ── Filesystem (Medium, unguarded) ──
    if last == "read_dir" && (n == 1 || has("fs")) {
        return Some(&rules::FS_READ_DIR);
    }
    if first == "glob" && last == "glob" {
        return Some(&rules::FS_GLOB);
    }
    if first == "tempfile" || (matches!(prev, "TempDir" | "NamedTempFile") && last == "new") {
        return Some(&rules::FS_TEMPFILE);
    }

    // ── Concurrency ──
    if prev == "thread" && last == "spawn" {
        return guard_high(&rules::CONC_THREAD_SPAWN, segs, n - 2, Q_THREAD, ctx);
    }
    if first == "tokio" && last == "spawn" {
        return Some(&rules::CONC_TOKIO_SPAWN);
    }

    None
}

/// Classify a method call by its method name. Name-based, so a user
/// `builder.gen()` is flagged too (an accepted Advisory false positive).
/// `HashMap`/`HashSet` iteration methods are handled separately in the visitor
/// because they need receiver context.
fn classify_method(method: &str) -> Option<&'static Rule> {
    match method {
        "gen" | "gen_range" => Some(&rules::RAND_GEN_METHOD),
        "par_iter" | "par_iter_mut" | "into_par_iter" => Some(&rules::CONC_RAYON),
        _ => None,
    }
}

// ── AST visitor ────────────────────────────────────────────────────────────

/// Method names that iterate a collection and, on a `HashMap`/`HashSet`, expose
/// a nondeterministic order.
const ITER_METHODS: &[&str] = &[
    "iter",
    "iter_mut",
    "into_iter",
    "keys",
    "values",
    "values_mut",
    "drain",
];

struct LeakVisitor<'a> {
    file: &'a str,
    lines: &'a [Vec<char>],
    ctx: &'a FileCtx,
    fn_stack: Vec<String>,
    /// Stack (one frame per enclosing `fn`) of local variable idents known to
    /// hold a `HashMap`/`HashSet`, inferred from `let` type annotations and
    /// constructor calls. Used to flag hash-container iteration order without
    /// type resolution. This only catches the declare-then-iterate case within a
    /// function; variables of unknown type are a documented false negative.
    map_vars: Vec<HashSet<String>>,
    leaks: Vec<Leak>,
}

impl LeakVisitor<'_> {
    fn record(&mut self, rule: &'static Rule, span: proc_macro2::Span) {
        let start = span.start();
        let line = start.line;
        let col = start.column + 1;
        // Deduplicate a NESTED leak that shares the same start position and
        // category as one already recorded. The canonical case is
        // `rand::thread_rng().gen()`: the outer `.gen()` method call and the
        // inner `thread_rng()` call both begin at the same `line:col`. The outer
        // is visited first, so a naive keep-first would drop the more specific
        // inner match. Instead we keep the HIGHER-confidence finding, so the gate
        // never loses severity to an Advisory duplicate.
        //
        // KNOWN LIMITATION (minor): two genuinely distinct same-category calls
        // that happen to share a start column (e.g. a chained `x.gen().gen()`)
        // collapse to one finding, under-counting. Severity/gate outcome are
        // unaffected (same tier), so this is left as-is rather than risk
        // regressing the nested-dedup case above.
        let snippet = snippet_from_span(self.lines, span);
        let function = self.fn_stack.last().cloned();
        if let Some(existing) = self
            .leaks
            .iter_mut()
            .find(|l| l.line == line && l.col == col && l.category == rule.category)
        {
            if rule.confidence.rank() > existing.confidence.rank() {
                // Upgrade every field together so the reported rule_id, snippet,
                // and function all describe the winning (higher-confidence) match
                // rather than a mix of the two overlapping spans.
                existing.rule_id = rule.id;
                existing.confidence = rule.confidence;
                existing.snippet = snippet;
                existing.function = function;
            }
            return;
        }
        self.leaks.push(Leak {
            rule_id: rule.id,
            confidence: rule.confidence,
            category: rule.category,
            file: self.file.to_string(),
            line,
            col,
            function,
            snippet,
        });
    }

    /// Is `ident` a local we believe holds a `HashMap`/`HashSet`?
    fn is_map_var(&self, ident: &str) -> bool {
        self.map_vars.last().is_some_and(|s| s.contains(ident))
    }

    /// Record `ident` as a hash-container local in the current fn scope.
    fn note_map_var(&mut self, ident: String) {
        if let Some(scope) = self.map_vars.last_mut() {
            scope.insert(ident);
        }
    }

    /// Does `expr` (after wrapper stripping) name a hash container we can flag?
    fn iter_base_is_hash(&self, expr: &syn::Expr) -> bool {
        let base = strip_iter_wrappers(expr);
        if let syn::Expr::Path(p) = base {
            if p.path.segments.len() == 1 && self.is_map_var(&p.path.segments[0].ident.to_string()) {
                return true;
            }
        }
        expr_is_hash_ctor(base)
    }
}

/// Does a type path name a `HashMap`/`HashSet` (by its last segment)?
fn ty_is_hash(ty: &syn::Type) -> bool {
    if let syn::Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            let id = seg.ident.to_string();
            return id == "HashMap" || id == "HashSet";
        }
    }
    false
}

/// Does an expression *syntactically* denote a `HashMap`/`HashSet` — a
/// constructor call (`HashMap::new()`, `HashSet::from(...)`) or a path whose
/// segments include `HashMap`/`HashSet`?
fn expr_is_hash_ctor(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Call(c) => {
            if let syn::Expr::Path(p) = &*c.func {
                return p
                    .path
                    .segments
                    .iter()
                    .any(|s| s.ident == "HashMap" || s.ident == "HashSet");
            }
            false
        }
        syn::Expr::Path(p) => p
            .path
            .segments
            .iter()
            .any(|s| s.ident == "HashMap" || s.ident == "HashSet"),
        _ => false,
    }
}

/// The binding ident of a simple `let` pattern (`x` or `mut x`, incl. a typed
/// `x: T`), if any.
fn binding_ident(pat: &syn::Pat) -> Option<String> {
    match pat {
        syn::Pat::Ident(pi) => Some(pi.ident.to_string()),
        syn::Pat::Type(pt) => binding_ident(&pt.pat),
        _ => None,
    }
}

/// Strip `&`/`&mut` and trailing `.iter()/.iter_mut()/.into_iter()` wrappers to
/// reach the base expression being iterated, so `for x in &map.iter()` and
/// `for x in map` both resolve to `map`.
fn strip_iter_wrappers(expr: &syn::Expr) -> &syn::Expr {
    match expr {
        syn::Expr::Reference(r) => strip_iter_wrappers(&r.expr),
        syn::Expr::MethodCall(m)
            if matches!(m.method.to_string().as_str(), "iter" | "iter_mut" | "into_iter") =>
        {
            strip_iter_wrappers(&m.receiver)
        }
        other => other,
    }
}

impl<'ast> Visit<'ast> for LeakVisitor<'_> {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.fn_stack.push(node.sig.ident.to_string());
        self.map_vars.push(HashSet::new());
        visit::visit_item_fn(self, node);
        self.map_vars.pop();
        self.fn_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.fn_stack.push(node.sig.ident.to_string());
        self.map_vars.push(HashSet::new());
        visit::visit_impl_item_fn(self, node);
        self.map_vars.pop();
        self.fn_stack.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        self.fn_stack.push(node.sig.ident.to_string());
        self.map_vars.push(HashSet::new());
        visit::visit_trait_item_fn(self, node);
        self.map_vars.pop();
        self.fn_stack.pop();
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        // Track `let m: HashMap<..> = ..` / `let m = HashMap::new()` so we can
        // flag iteration over `m` later. Statements are visited in source order,
        // so a declare-before-iterate binding is in scope by the loop.
        if let Some(name) = binding_ident(&node.pat) {
            let typed_hash = matches!(&node.pat, syn::Pat::Type(pt) if ty_is_hash(&pt.ty));
            let init_hash = node
                .init
                .as_ref()
                .is_some_and(|init| expr_is_hash_ctor(&init.expr));
            if typed_hash || init_hash {
                self.note_map_var(name);
            }
        }
        visit::visit_local(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        if self.iter_base_is_hash(&node.expr) {
            self.record(&rules::ITER_HASH, node.expr.span());
        }
        visit::visit_expr_for_loop(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            let segs: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            let resolved = self.ctx.resolve(&segs);
            if let Some(rule) = classify_call(&resolved, self.ctx) {
                self.record(rule, node.span());
            }
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let method = node.method.to_string();
        if let Some(rule) = classify_method(&method) {
            self.record(rule, node.span());
        }
        if ITER_METHODS.contains(&method.as_str()) && self.iter_base_is_hash(&node.receiver) {
            self.record(&rules::ITER_HASH, node.span());
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        let segs: Vec<String> = node
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        let resolved = self.ctx.resolve(&segs);
        let n = resolved.len();
        let last = resolved[n - 1].as_str();
        // `OsRng` used as a VALUE/path expression — bound to a local, used as a
        // receiver, or passed by value. Guarded like the call forms: bare must
        // not be locally shadowed, qualified must carry `rand`.
        if last == "OsRng" {
            if let Some(rule) = guard_high(&rules::RAND_OSRNG, &resolved, n - 1, Q_RAND, self.ctx) {
                self.record(rule, node.span());
            }
        }
        // `Ordering::Relaxed` — relaxed atomic ordering (Advisory, unguarded).
        if last == "Relaxed" && n >= 2 && resolved[n - 2] == "Ordering" {
            self.record(&rules::ATOMIC_RELAXED, node.span());
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
    fn rule_of<'a>(leaks: &'a [Leak], needle: &str) -> Option<&'a Leak> {
        leaks.iter().find(|l| l.snippet.contains(needle))
    }
    fn any_high(leaks: &[Leak]) -> bool {
        leaks.iter().any(|l| l.confidence == Confidence::High)
    }

    // ── Catalog integrity ─────────────────────────────────────────────────

    #[test]
    fn catalog_ids_are_unique() {
        let mut ids: Vec<&str> = rules::CATALOG.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate rule id in CATALOG");
    }

    // ── Fully-qualified forms → correct category ──────────────────────────

    #[test]
    fn fully_qualified_time_forms() {
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
                "tokio::time::{form} must be TIME"
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
        assert!(has(
            &scan("fn f() { time::OffsetDateTime::now_utc(); }"),
            Category::Time,
            "now_utc"
        ));
        assert!(has(
            &scan("fn f() { tokio::time::Instant::now(); }"),
            Category::Time,
            "Instant::now"
        ));
    }

    #[test]
    fn time_confidence_tiers() {
        assert_eq!(
            rule_of(&scan("fn f() { SystemTime::now(); }"), "SystemTime::now")
                .unwrap()
                .confidence,
            Confidence::High
        );
        assert_eq!(
            rule_of(&scan("fn f() { chrono::Utc::now(); }"), "Utc::now")
                .unwrap()
                .confidence,
            Confidence::Medium
        );
        assert_eq!(
            rule_of(
                &scan("fn f() { tokio::time::Instant::now(); }"),
                "Instant::now"
            )
            .unwrap()
            .confidence,
            Confidence::Medium
        );
    }

    #[test]
    fn fully_qualified_random_forms() {
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
            &scan("fn f() { getrandom::getrandom(&mut b); }"),
            Category::Random,
            "getrandom"
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
        assert!(any_of(
            &scan("fn f() { let x = rng.gen_range(0..10); }"),
            Category::Random
        ));
        assert!(any_of(
            &scan("fn f() { let x: u8 = rng.gen(); }"),
            Category::Random
        ));
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
    fn random_confidence_tiers() {
        for (src, needle) in [
            ("fn f() { thread_rng(); }", "thread_rng"),
            ("fn f() { rand::random::<u8>(); }", "rand::random"),
            ("fn f() { let r = OsRng; }", "OsRng"),
            ("fn f() { getrandom::getrandom(&mut b); }", "getrandom"),
        ] {
            assert_eq!(
                rule_of(&scan(src), needle).unwrap().confidence,
                Confidence::High,
                "{needle} must be HIGH"
            );
        }
        assert_eq!(
            rule_of(&scan("fn f() { uuid::Uuid::new_v4(); }"), "new_v4")
                .unwrap()
                .confidence,
            Confidence::Medium
        );
        assert_eq!(
            rule_of(&scan("fn f() { let x: u8 = rng.gen(); }"), "gen")
                .unwrap()
                .confidence,
            Confidence::Advisory
        );
    }

    #[test]
    fn fully_qualified_network_forms_are_high() {
        for (src, needle) in [
            ("fn f() { reqwest::get(u); }", "reqwest::get"),
            ("fn f() { std::net::TcpStream::connect(a); }", "TcpStream"),
            ("fn f() { std::net::TcpListener::bind(a); }", "TcpListener"),
            ("fn f() { std::net::UdpSocket::bind(a); }", "UdpSocket"),
            ("fn f() { tokio::net::TcpStream::connect(a); }", "tokio::net"),
        ] {
            let leaks = scan(src);
            assert!(has(&leaks, Category::Network, needle), "missing {needle}");
            assert_eq!(
                rule_of(&leaks, needle).unwrap().confidence,
                Confidence::High,
                "{needle} must be HIGH"
            );
        }
    }

    #[test]
    fn concurrency_forms_and_tiers() {
        assert_eq!(
            rule_of(
                &scan("fn f() { std::thread::spawn(|| {}); }"),
                "thread::spawn"
            )
            .unwrap()
            .confidence,
            Confidence::High
        );
        assert_eq!(
            rule_of(&scan("fn f() { tokio::spawn(async {}); }"), "tokio::spawn")
                .unwrap()
                .confidence,
            Confidence::Advisory
        );
        let leaks = scan("fn f() { v.par_iter().for_each(|_| {}); }");
        assert!(any_of(&leaks, Category::Concurrency));
        assert_eq!(
            rule_of(&leaks, "par_iter").unwrap().confidence,
            Confidence::Advisory
        );
    }

    // ── New categories ────────────────────────────────────────────────────

    #[test]
    fn env_and_process_are_medium() {
        for (src, needle) in [
            ("fn f() { std::env::var(\"X\"); }", "env::var"),
            ("fn f() { std::env::args(); }", "env::args"),
            ("fn f() { std::process::id(); }", "process::id"),
            (
                "fn f() { std::process::Command::new(\"ls\"); }",
                "Command::new",
            ),
        ] {
            let leaks = scan(src);
            assert!(any_of(&leaks, Category::Env), "missing env leak for {needle}");
            assert_eq!(
                rule_of(&leaks, needle).unwrap().confidence,
                Confidence::Medium
            );
        }
    }

    #[test]
    fn filesystem_is_medium() {
        for (src, needle) in [
            ("fn f() { std::fs::read_dir(\".\"); }", "read_dir"),
            ("fn f() { glob::glob(\"*.rs\"); }", "glob"),
            ("fn f() { tempfile::tempdir(); }", "tempfile"),
            ("fn f() { TempDir::new(); }", "TempDir::new"),
        ] {
            let leaks = scan(src);
            assert!(
                any_of(&leaks, Category::Filesystem),
                "missing fs leak for {needle}"
            );
            assert_eq!(
                rule_of(&leaks, needle).unwrap().confidence,
                Confidence::Medium
            );
        }
    }

    #[test]
    fn atomic_relaxed_is_advisory() {
        let leaks = scan("fn f() { x.store(1, Ordering::Relaxed); }");
        assert!(any_of(&leaks, Category::Atomic));
        assert_eq!(
            rule_of(&leaks, "Relaxed").unwrap().confidence,
            Confidence::Advisory
        );
        assert!(!any_of(
            &scan("fn f() { let _ = Foo::Relaxed; }"),
            Category::Atomic
        ));
    }

    #[test]
    fn hash_iteration_forms() {
        assert!(any_of(
            &scan("fn f() { let m: HashMap<u8, u8> = x; for (k, v) in &m {} }"),
            Category::Iteration
        ));
        assert!(any_of(
            &scan("fn f() { let m = HashMap::new(); for k in m.keys() {} }"),
            Category::Iteration
        ));
        assert!(any_of(
            &scan("fn f() { for x in HashSet::from([1, 2]).iter() {} }"),
            Category::Iteration
        ));
    }

    #[test]
    fn hash_iteration_confidence_is_advisory() {
        let leaks = scan("fn f() { let m: HashMap<u8, u8> = x; for (k, v) in &m {} }");
        assert_eq!(
            leaks
                .iter()
                .find(|l| l.category == Category::Iteration)
                .unwrap()
                .confidence,
            Confidence::Advisory
        );
    }

    #[test]
    fn vec_and_btree_iteration_are_not_flagged() {
        assert!(!any_of(
            &scan("fn f() { let v: Vec<u8> = x; for i in v.iter() {} }"),
            Category::Iteration
        ));
        assert!(!any_of(
            &scan("fn f() { let m: BTreeMap<u8, u8> = x; for k in m.keys() {} }"),
            Category::Iteration
        ));
    }

    // ── Shadow & qualifier guards: High must never fire on benign code ──────

    #[test]
    fn foreign_qualified_high_sources_are_not_high() {
        // A foreign qualifier means it is not the std/ecosystem source. None of
        // these may emit a HIGH finding (they break the default `--deny` gate).
        for src in [
            "fn f() { my_time::SystemTime::now(); }",
            "fn f() -> Kind { Kind::OsRng }",
            "fn f() -> u64 { mymod::rngs::OsRng.next_u64() }",
            "fn f() { let r = crate::rng::OsRng; }",
            "fn f() { mock::TcpStream::pair(); }",
            "fn f() { let _ = my_utils::thread_rng(); }",
            "fn f() { mymod::thread::spawn(|| {}); }",
            // Generic std sub-module names must NOT count as qualifiers: a user
            // `crate::net`/`crate::time` module must not escape the guard.
            "fn f() { crate::net::TcpStream::connect(\"x\"); }",
            "fn f() { crate::time::Instant::now(); }",
            "fn f() { crate::time::SystemTime::now(); }",
        ] {
            let leaks = scan(src);
            assert!(
                !any_high(&leaks),
                "foreign-qualified source must NOT be HIGH in `{src}`: {leaks:#?}"
            );
        }
    }

    #[test]
    fn local_module_named_like_std_submodule_is_not_high() {
        // A file defining `mod net`/`mod time` and calling through it must not
        // fire High even though the path segment matches a std sub-module.
        for src in [
            "mod net { pub struct TcpStream; impl TcpStream { pub fn connect(_: &str) {} } } \
             fn f() { net::TcpStream::connect(\"x\"); }",
            "mod time { pub struct Instant; impl Instant { pub fn now() {} } } \
             fn f() { time::Instant::now(); }",
        ] {
            let leaks = scan(src);
            assert!(
                !any_high(&leaks),
                "local mod shadowing a std sub-module must NOT be HIGH in `{src}`: {leaks:#?}"
            );
        }
    }

    #[test]
    fn local_module_shadowing_a_crate_rule_is_not_high() {
        // The crate-named rules must be suppressed by a same-named local `mod`.
        for src in [
            "mod reqwest { pub fn get(_: &str) {} } fn f() { reqwest::get(\"x\"); }",
            "mod tokio { pub mod net { pub struct TcpStream; } } \
             fn f() { tokio::net::TcpStream::connect(\"x\"); }",
            "mod getrandom { pub fn getrandom(_: &mut [u8]) {} } \
             fn f() { let mut b = [0u8]; getrandom::getrandom(&mut b); }",
            // A local `mod rand` shadows the qualifier of the guarded rand rules.
            "mod rand { pub fn thread_rng() -> u8 { 0 } } fn f() { rand::thread_rng(); }",
        ] {
            let leaks = scan(src);
            assert!(
                !any_high(&leaks),
                "local mod shadowing a crate rule must NOT be HIGH in `{src}`: {leaks:#?}"
            );
        }
    }

    #[test]
    fn fn_named_like_a_crate_does_not_suppress_the_real_crate_call() {
        // A `fn getrandom` inside SOME module must NOT suppress a genuine
        // `getrandom::getrandom` in the same file (mod_names, not local_names,
        // drives the crate-rule shadow guard).
        let leaks = scan(
            "mod helpers { pub fn getrandom(_: &mut [u8]) {} } \
             fn f() { let mut b = [0u8]; getrandom::getrandom(&mut b); }",
        );
        assert!(
            leaks
                .iter()
                .any(|l| l.confidence == Confidence::High && l.category == Category::Random),
            "genuine getrandom::getrandom must stay HIGH: {leaks:#?}"
        );
    }

    #[test]
    fn locally_shadowed_bare_sources_are_not_high() {
        // A file that defines the name itself → the bare use is the user's own
        // symbol, so no HIGH finding.
        for src in [
            "struct SystemTime; impl SystemTime { fn now() {} } fn f() { SystemTime::now(); }",
            "fn thread_rng() -> u8 { 0 } fn f() { let _ = thread_rng(); }",
            "struct OsRng; fn f() { let _r = OsRng; }",
            "fn getrandom(_b: &mut [u8]) {} fn f() { let mut b = [0u8]; getrandom(&mut b); }",
        ] {
            let leaks = scan(src);
            assert!(
                !any_high(&leaks),
                "locally-shadowed bare source must NOT be HIGH in `{src}`: {leaks:#?}"
            );
        }
    }

    #[test]
    fn genuine_bare_and_qualified_sources_stay_high() {
        // The guards must not over-suppress: real bare imports and std-qualified
        // paths (no local shadow) remain HIGH so the gate still bites.
        for (src, needle) in [
            ("fn f() { SystemTime::now(); }", "SystemTime::now"),
            ("fn f() { let _r = OsRng; }", "OsRng"),
            ("fn f() { let _ = thread_rng(); }", "thread_rng"),
            ("fn f() { TcpStream::connect(a); }", "TcpStream"),
            (
                "fn f() { std::net::TcpStream::connect(a); }",
                "TcpStream",
            ),
            ("fn f() { std::thread::spawn(|| {}); }", "thread::spawn"),
        ] {
            let leaks = scan(src);
            assert_eq!(
                rule_of(&leaks, needle).map(|l| l.confidence),
                Some(Confidence::High),
                "genuine source must stay HIGH in `{src}`: {leaks:#?}"
            );
        }
    }

    // ── Alias resolution (FIX 4) ───────────────────────────────────────────

    #[test]
    fn aliased_std_import_is_detected_high() {
        // `use std::time::SystemTime as Sys; Sys::now()` — resolves to the std
        // path and IS detected HIGH.
        let leaks = scan("use std::time::SystemTime as Sys; fn f() { Sys::now(); }");
        assert!(
            leaks.iter().any(|l| l.confidence == Confidence::High
                && l.category == Category::Time),
            "aliased std SystemTime must be HIGH: {leaks:#?}"
        );
    }

    #[test]
    fn aliased_foreign_import_is_not_high() {
        // `use mymod::SystemTime as Sys;` resolves to a foreign path → not HIGH.
        let leaks = scan("use mymod::SystemTime as Sys; fn f() { Sys::now(); }");
        assert!(!any_high(&leaks), "aliased foreign type must not be HIGH: {leaks:#?}");
    }

    #[test]
    fn unaliased_unknown_prefix_is_not_flagged() {
        // No `use … as` in view and an unknown prefix → documented false
        // negative (nothing to resolve, nothing flagged).
        assert!(scan("fn f() { Sys::now(); }").is_empty());
    }

    // ── Dedup / OsRng edge cases (regression-locked from v1) ───────────────

    #[test]
    fn os_rng_default_call_form_is_one_random() {
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
            assert_eq!(os.len(), 1, "OsRng::default() must be one RANDOM in `{src}`");
        }
    }

    #[test]
    fn receiver_from_entropy_is_random() {
        assert!(has(
            &scan("fn f() { let _ = Config::from_entropy(); }"),
            Category::Random,
            "from_entropy"
        ));
    }

    #[test]
    fn bare_free_from_entropy_is_not_flagged() {
        assert!(scan("fn f() { let _ = from_entropy(); }").is_empty());
    }

    #[test]
    fn nested_thread_rng_gen_is_one_finding_keeps_high() {
        let leaks = scan("fn f() { let x: u8 = rand::thread_rng().gen(); }");
        let random: Vec<_> = leaks
            .iter()
            .filter(|l| l.category == Category::Random)
            .collect();
        assert_eq!(random.len(), 1, "must dedup to one RANDOM: {leaks:#?}");
        assert_eq!(
            random[0].confidence,
            Confidence::High,
            "dedup must keep the HIGH finding, not the Advisory one"
        );
    }

    #[test]
    fn distinct_rng_calls_on_same_line_are_not_merged() {
        let leaks = scan("fn f() { let _ = (thread_rng(), thread_rng()); }");
        let random: Vec<_> = leaks
            .iter()
            .filter(|l| l.category == Category::Random)
            .collect();
        assert_eq!(random.len(), 2);
    }

    // ── Accepted false positives (name-based, Advisory/Medium only) ─────────

    #[test]
    fn accepted_fp_user_gen_method() {
        // `builder.gen()` is an Advisory (never-gates) FP — that is acceptable.
        let leaks = scan("fn f() { builder.gen(); }");
        assert!(any_of(&leaks, Category::Random));
        assert!(!any_high(&leaks));
    }

    // ── Documented false negatives ─────────────────────────────────────────

    #[test]
    fn any_at_or_above_threshold() {
        let mut report = ScanReport::default();
        scan_source("t.rs", "fn f() { chrono::Utc::now(); }", &mut report);
        assert!(!report.any_at_or_above(Confidence::High));
        assert!(report.any_at_or_above(Confidence::Medium));
        assert!(report.any_at_or_above(Confidence::Advisory));
    }

    #[test]
    fn uncertifiable_flags_unparseable_input() {
        let mut report = ScanReport::default();
        scan_source("bad.rs", "fn f( { this is not rust", &mut report);
        assert!(report.uncertifiable(), "a parse failure must be uncertifiable");
        assert_eq!(report.files_parsed, 0);
    }

    // ── Fingerprint: stable identity shared by SARIF + baseline ─────────────

    fn leak_at(line: usize, col: usize) -> Leak {
        Leak {
            rule_id: "DST-TIME-001",
            confidence: Confidence::High,
            category: Category::Time,
            file: "src/lib.rs".to_string(),
            line,
            col,
            function: Some("handler".to_string()),
            snippet: "SystemTime::now()".to_string(),
        }
    }

    #[test]
    fn fingerprint_is_line_and_column_independent() {
        // Same rule + function + snippet at a different line/column (the finding
        // "moved" because blank lines were inserted above it) → identical id.
        assert_eq!(
            fingerprint(&leak_at(10, 9)),
            fingerprint(&leak_at(42, 13)),
            "fingerprint must not depend on line/column"
        );
    }

    #[test]
    fn fingerprint_is_indentation_independent() {
        // Pure re-indentation of the snippet must not change the identity.
        let mut a = leak_at(1, 1);
        a.snippet = "SystemTime::now()".to_string();
        let mut b = leak_at(1, 1);
        b.snippet = "SystemTime::now()   ".to_string();
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn fingerprint_differs_on_rule_function_or_snippet() {
        let base = leak_at(1, 1);
        let mut other_rule = base.clone();
        other_rule.rule_id = "DST-TIME-002";
        let mut other_fn = base.clone();
        other_fn.function = Some("other".to_string());
        let mut other_snip = base.clone();
        other_snip.snippet = "Instant::now()".to_string();
        assert_ne!(fingerprint(&base), fingerprint(&other_rule));
        assert_ne!(fingerprint(&base), fingerprint(&other_fn));
        assert_ne!(fingerprint(&base), fingerprint(&other_snip));
    }

    #[test]
    fn fingerprint_is_16_hex_chars() {
        let fp = fingerprint(&leak_at(1, 1));
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
