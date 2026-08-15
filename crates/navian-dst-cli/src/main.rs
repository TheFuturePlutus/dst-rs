//! `navian-dst` — command-line tools for the navian-dst deterministic-simulation-testing
//! substrate.
//!
//! Currently ships one subcommand, [`scan`](Commands::Scan): a static
//! determinism-leak detector that finds calls into wall-clock time, RNG,
//! network, and unstructured concurrency APIs — the sources of non-determinism
//! that break replay-based testing.

#![warn(missing_docs)]

mod migrate;
mod scanner;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use migrate::{migrate_path, CheckOutcome, MigrateOptions, MigrateResult, TraitFamily};
use scanner::{rule_by_id, scan_path, Category, Confidence, ScanReport};

/// Exit code for a tool/usage error (bad args, unreadable path).
const EXIT_USAGE: u8 = 2;

#[derive(Parser)]
#[command(
    name = "navian-dst",
    version,
    about = "Tools for deterministic-simulation testing with navian-dst"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan Rust source for determinism leaks (time, random, network, concurrency).
    ///
    /// A NAME-BASED heuristic, not a sound analyzer: it flags calls and paths
    /// whose recognizable name or tail segments match a known determinism source
    /// (e.g. `SystemTime::now`, `thread_rng`, `OsRng`, `tokio::spawn`). It does
    /// NO name resolution, so it MAY false-positive on same-named user symbols
    /// (`my_time::SystemTime::now()`, `builder.gen()`) and MAY false-negative on
    /// renamed imports (`use ... as Sys; Sys::now()`). It errs toward flagging,
    /// which is the right default for a replay-safety gate — a clean scan is a
    /// prompt to review, not a proof of determinism. The `migrate` codemod is a
    /// separate, conservative pass.
    Scan {
        /// Directory (or file) to scan. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Emit machine-readable JSON instead of a human report.
        #[arg(long)]
        json: bool,

        /// Turn `scan` into a CI gate: exit non-zero if any finding is at or
        /// above the deny threshold. Fails on `high`-confidence findings by
        /// default; lower the bar with `--deny-level`.
        #[arg(long)]
        deny: bool,

        /// Confidence threshold the `--deny` gate fails on: `high` (default),
        /// `medium`, or `advisory` (fail on anything). Implies `--deny`.
        #[arg(long, value_name = "high|medium|advisory")]
        deny_level: Option<String>,
    },

    /// Rewrite a conservative, seam-safe subset of determinism leaks so the
    /// crate keeps compiling and becomes injectable with a simulated clock.
    ///
    /// v1 handles TIME leaks inside inherent methods of named-field structs:
    /// it adds a `time: Arc<dyn navian_dst::Time>` field (defaulted to the real
    /// production clock in every constructor) and rewrites the leak call sites.
    /// Everything it cannot map cleanly is left untouched and reported.
    Migrate {
        /// File or directory to migrate. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Print a unified diff and write nothing.
        #[arg(long)]
        dry_run: bool,

        /// Trait families to migrate. v1 supports only `time`.
        #[arg(long, default_value = "time", value_delimiter = ',')]
        traits: Vec<String>,

        /// Also gate on `cargo test --doc` after the `cargo check --all-targets`
        /// gate passes, rolling the whole run back if any doctest fails.
        ///
        /// OFF by default: doctests can only be verified by RUNNING them (cargo
        /// rejects `--no-run` for `--doc`), which executes user code and is slow.
        /// When off, migrate warns that doctests were not verified.
        #[arg(long)]
        check_doctests: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Scan {
            path,
            json,
            deny,
            deny_level,
        } => {
            // Resolve the deny threshold. `--deny-level` implies `--deny` and
            // sets the bar; bare `--deny` fails on `high` only.
            let threshold = match deny_level.as_deref() {
                Some(s) => match Confidence::parse(s) {
                    Some(c) => Some(c),
                    None => {
                        eprintln!(
                            "error: invalid --deny-level `{s}` (expected high|medium|advisory)"
                        );
                        return ExitCode::from(EXIT_USAGE);
                    }
                },
                None => deny.then_some(Confidence::High),
            };

            // A path that does not exist is a usage error (exit 2), not a
            // "clean" scan — never let a typo look like a passing gate.
            if !path.exists() {
                eprintln!("error: path does not exist: {}", path.display());
                return ExitCode::from(EXIT_USAGE);
            }

            let report = scan_path(&path);
            if json {
                emit_json(&report);
            } else {
                emit_human(&report, threshold);
            }

            // Exit-code contract:
            //   0 — clean, or findings present without a deny gate;
            //   1 — one or more findings at/above the deny threshold under a gate;
            //   2 — tool/usage error, OR (under a gate) files that could not be
            //       read/parsed: the gate cannot certify a tree it never saw.
            match threshold {
                Some(_) if report.uncertifiable() => {
                    eprintln!(
                        "error: cannot certify — {} file(s) could not be read or parsed:",
                        report.parse_failures.len()
                    );
                    for f in &report.parse_failures {
                        eprintln!("  {f}");
                    }
                    ExitCode::from(EXIT_USAGE)
                }
                Some(t) if report.any_at_or_above(t) => ExitCode::FAILURE,
                _ => ExitCode::SUCCESS,
            }
        }
        Commands::Migrate {
            path,
            dry_run,
            traits,
            check_doctests,
        } => {
            // v1 only supports `time`. Reject anything else explicitly rather
            // than silently ignoring it.
            let mut families = Vec::new();
            for t in &traits {
                match t.as_str() {
                    "time" => families.push(TraitFamily::Time),
                    other => {
                        eprintln!(
                            "error: unsupported trait family `{other}`. v1 supports only `time`."
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            if families.is_empty() {
                families.push(TraitFamily::Time);
            }

            let opts = MigrateOptions {
                dry_run,
                traits: families,
                check_doctests,
            };
            let result = migrate_path(&path, &opts);
            emit_migrate(&result, dry_run)
        }
    }
}

fn emit_migrate(result: &MigrateResult, dry_run: bool) -> ExitCode {
    if dry_run {
        for c in &result.changes {
            print!("{}", c.diff);
        }
        if result.changes.is_empty() {
            println!("(no changes — nothing to migrate)");
        } else {
            println!(
                "\nwarning: --dry-run shows the planned rewrite but does NOT run `cargo check`; \
                 applying it (without --dry-run) may still fail to compile and be reverted."
            );
        }
    }

    println!("\n== Migrate summary ==");
    println!(
        "  structs migrated : {} {}",
        result.structs_migrated.len(),
        if result.structs_migrated.is_empty() {
            String::new()
        } else {
            format!(
                "({})",
                result
                    .structs_migrated
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    println!("  leaks rewritten  : {}", result.leaks_rewritten);
    println!("  leaks skipped    : {}", result.skips.len());
    if !result.skips.is_empty() {
        println!("\n  Skipped (manual / agent needed):");
        for s in &result.skips {
            println!(
                "    {}:{}:{}  {}  — {}",
                s.file, s.line, s.col, s.snippet, s.reason
            );
        }
    }
    if !result.parse_failures.is_empty() {
        println!("\n  Unparseable files (skipped):");
        for f in &result.parse_failures {
            println!("    {f}");
        }
    }

    // Report cargo-check outcome for the applied (non-dry-run) path.
    if !dry_run {
        match &result.check {
            CheckOutcome::Passed => {
                println!("\n  cargo check: PASSED — changes applied.");
                if result.doctests_checked {
                    println!("  cargo test --doc: PASSED.");
                } else if !result.structs_migrated.is_empty() {
                    // Never let the doctest gap ship silently.
                    println!(
                        "\n  Note: doctests are not verified (cargo cannot compile-check \
                         doctests without running them; --all-targets excludes them). If any \
                         migrated struct is constructed in a doctest, run `cargo test --doc` and \
                         update struct-literal construction to use the constructor / `with_time`. \
                         Re-run with --check-doctests to gate on doctests automatically."
                    );
                }
            }
            CheckOutcome::Skipped => {
                if result.no_op {
                    println!("\n  Nothing to migrate (idempotent no-op).");
                } else {
                    println!("\n  cargo check: skipped.");
                }
            }
            CheckOutcome::Failed(err) => {
                println!(
                    "\n  cargo check: FAILED (--all-targets) — a build target no longer \
                     compiles after the rewrite; ALL original files RESTORED (nothing applied).\n\
                     \x20 This can happen when a struct is built via a struct literal OUTSIDE \
                     its module (e.g. in tests/ or examples/), where the injected private \
                     `time` field is inaccessible — that struct needs a public constructor; \
                     migrate it manually / with an agent."
                );
                eprintln!("{err}");
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

fn emit_json(report: &ScanReport) {
    // The `--json` contract is exactly the leak array. Each item is a stable
    // object: {rule_id, confidence, category, file, line, col, function, snippet}.
    match serde_json::to_string_pretty(&report.leaks) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize leaks: {e}"),
    }
}

fn emit_human(report: &ScanReport, threshold: Option<Confidence>) {
    use std::collections::BTreeSet;

    println!("== Determinism leaks ==\n");

    if report.leaks.is_empty() {
        println!("No determinism leaks found.\n");
    }

    for &cat in Category::all() {
        let hits: Vec<_> = report.leaks.iter().filter(|l| l.category == cat).collect();
        if hits.is_empty() {
            continue;
        }
        println!("{} ({})", cat.label(), hits.len());
        for leak in hits {
            let loc = format!("{}:{}", leak.file, leak.line);
            let in_fn = match &leak.function {
                Some(name) => format!("   (in fn `{name}`)"),
                None => String::new(),
            };
            println!(
                "  {loc}  [{} {}]  {}  {}{in_fn}",
                leak.confidence.label(),
                leak.rule_id,
                leak.category.label(),
                leak.snippet
            );
        }
        println!();
    }

    // Rule legend: describe every rule that fired, from the catalog.
    if !report.leaks.is_empty() {
        let ids: BTreeSet<&str> = report.leaks.iter().map(|l| l.rule_id).collect();
        println!("Rules:");
        for id in ids {
            match rule_by_id(id) {
                Some(rule) => println!("  {id}  {}", rule.description),
                None => println!("  {id}"),
            }
        }
        println!();
    }

    // Confidence-tier tallies.
    let by_conf = |c: Confidence| report.leaks.iter().filter(|l| l.confidence == c).count();
    let files: BTreeSet<&str> = report.leaks.iter().map(|l| l.file.as_str()).collect();

    println!(
        "Scanned {} file(s): {} parsed, {} skipped (parse error).",
        report.files_scanned,
        report.files_parsed,
        report.parse_failures.len()
    );
    if !report.parse_failures.is_empty() {
        for f in &report.parse_failures {
            println!("  warning: could not parse {f} (skipped)");
        }
    }
    println!(
        "{} leak(s) across {} file(s) ({} high, {} medium, {} advisory).",
        report.leaks.len(),
        files.len(),
        by_conf(Confidence::High),
        by_conf(Confidence::Medium),
        by_conf(Confidence::Advisory),
    );
    match threshold {
        Some(t) if report.any_at_or_above(t) => println!(
            "GATE: FAIL — findings at or above `{}` (--deny threshold).",
            t.label()
        ),
        Some(t) => println!("GATE: pass — no findings at or above `{}`.", t.label()),
        None => {}
    }
}
