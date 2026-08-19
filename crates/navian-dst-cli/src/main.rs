//! `navian-dst` — command-line tools for the navian-dst deterministic-simulation-testing
//! substrate.
//!
//! Currently ships one subcommand, [`scan`](Commands::Scan): a static
//! determinism-leak detector that finds calls into wall-clock time, RNG,
//! network, and unstructured concurrency APIs — the sources of non-determinism
//! that break replay-based testing.

#![warn(missing_docs)]

mod baseline;
mod check;
mod migrate;
mod presence;
mod scanner;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use baseline::Baseline;
use migrate::{migrate_path, CheckOutcome, MigrateOptions, MigrateResult, TraitFamily};
use presence::{check_path, PresenceReport, Status};
use scanner::{fingerprint, fix_by_id, rule_by_id, scan_path, Category, Confidence, ScanReport};

/// Exit code for a tool/usage error (bad args, unreadable path).
const EXIT_USAGE: u8 = 2;

/// Landing page for the tool, embedded in SARIF output and scaffolding.
const INFORMATION_URI: &str = "https://github.com/TheFuturePlutus/navian-dst";

/// Output format for `scan`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Human-readable report grouped by category (default).
    Human,
    /// The stable leak array as JSON (same as the legacy `--json` flag).
    Json,
    /// SARIF 2.1.0 — for GitHub code scanning and other SARIF consumers.
    Sarif,
    /// GitHub Actions workflow commands (`::error`/`::warning`/`::notice`).
    Github,
}

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

        /// Output format: `human` (default), `json`, `sarif`, or `github`.
        #[arg(long, value_enum, value_name = "human|json|sarif|github")]
        format: Option<Format>,

        /// Emit machine-readable JSON instead of a human report. Back-compat
        /// alias for `--format json`; `--format` wins if both are given.
        #[arg(long)]
        json: bool,

        /// Suppress findings recorded in a baseline file (see the `baseline`
        /// subcommand). A suppressed finding is dropped from output AND from the
        /// `--deny` decision, so the gate fails only on NEW findings. A missing
        /// or invalid baseline is a usage error (exit 2).
        #[arg(long, value_name = "FILE")]
        baseline: Option<PathBuf>,

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

    /// Record the current findings to a baseline file so a later
    /// `scan --baseline <file>` suppresses them.
    ///
    /// This is what lets a team turn on the `--deny` gate on a codebase that is
    /// not yet clean: baseline the existing findings once, commit the file, and
    /// thereafter the gate fails only on NEW findings. Each entry stores the
    /// stable, line-independent fingerprint (shared with SARIF), plus the rule
    /// id, file, and function for readability.
    Baseline {
        /// Directory (or file) to scan. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Baseline file to write. Defaults to `navian-dst-baseline.json`.
        #[arg(long, default_value = "navian-dst-baseline.json")]
        out: PathBuf,
    },

    /// Explain a rule: why it is nondeterministic, the injectable seam to route
    /// it through, a before → after snippet, and whether `migrate` auto-fixes it.
    ///
    /// This is the "hand it to your AI agent" recipe for a single finding. Run
    /// `explain <RULE-ID>` for the full recipe, `explain --list` (or bare
    /// `explain`) to list every rule, and add `--format json` for the structured
    /// form an agent can consume.
    Explain {
        /// Rule id to explain (e.g. `DST-TIME-001`). Omit — or pass `--list` — to
        /// list every rule id with its one-line description.
        #[arg(value_name = "RULE-ID")]
        rule_id: Option<String>,

        /// List every rule id + one-line description instead of explaining one.
        #[arg(long)]
        list: bool,

        /// Output format: `human` (default) or `json`. `json` emits the fix
        /// recipe as a structured object for agents. (`sarif`/`github` are not
        /// meaningful here and fall back to `human`.)
        #[arg(long, value_enum, value_name = "human|json")]
        format: Option<Format>,
    },

    /// Scaffold navian-dst adoption in the current repo (never overwrites).
    ///
    /// Writes a GitHub Actions workflow that runs `scan --deny` as a gate and a
    /// commented config template. Existing files are left untouched and
    /// reported as skipped.
    Init,

    /// Run a command several times under one fixed seed and report whether its
    /// output is IDENTICAL (deterministic) or DIVERGES (nondeterminism escaped).
    ///
    /// This is the dynamic "run twice, diff" determinism gesture: a correctly
    /// seeded run reproduces byte-for-byte, so anything that varies run-to-run —
    /// an unseeded clock, real RNG, `HashMap` iteration order, thread
    /// interleaving — shows up here even though the static `scan` never sees it.
    ///
    /// HANGS: without `--timeout`, a command that HANGS will hang `check` — a
    /// hang is NOT auto-surfaced as a divergence. A deadlock (a classic
    /// thread-interleaving symptom) manifests as an indefinite hang, not a
    /// reported difference. Pass `--timeout <secs>` to kill and REPORT an
    /// overrunning run instead of blocking forever.
    ///
    /// SEEDING, HONESTLY: the navian-dst LIBRARY takes its seed through its API
    /// (`SimulatedRandom::from_seed`, `FaultSchedule::new`, the `SimScheduler`),
    /// NOT from the environment. `check` cannot force a seed onto library code —
    /// it exports `NAVIAN_DST_SEED` and `SEED` for the command to consult IF it
    /// chooses, and otherwise just runs the command and diffs the output. So this
    /// catches gross / escaped nondeterminism; it does not itself make a program
    /// deterministic.
    ///
    /// The command's stdout should be deterministic under a fixed seed — point it
    /// at a test/binary that prints a STATE SNAPSHOT or a REPLAY HASH, not at raw
    /// `cargo test` (its "finished in 0.03s" timing always differs and would cry
    /// wolf). Use `--ignore <regex>` (repeatable) to drop matching lines — e.g.
    /// timestamps or timings — before comparison.
    ///
    /// SEMANTICS: `check` measures DIVERGENCE, not pass/fail. A command that FAILS
    /// identically every run is "deterministic" (exit 0); only differing
    /// (filtered stdout, exit status) is a divergence (exit 1). Only stdout and
    /// exit status are compared — stderr is captured and shown but not compared.
    ///
    /// Exit codes: 0 = identical across all runs; 1 = a run diverged; 2 = usage
    /// error (bad `--runs`/`--ignore`, or the command could not be spawned).
    Check {
        /// Seed exported to every run (as `NAVIAN_DST_SEED` and `SEED`) so the
        /// runs are directly comparable. Default 0.
        #[arg(long, default_value_t = 0)]
        seed: u64,

        /// How many times to run the command. Must be >= 2. Default 2.
        #[arg(long, default_value_t = 2)]
        runs: u32,

        /// Kill any run that exceeds this many seconds and REPORT it as a
        /// timeout (exit 1). Without this, a hanging/deadlocked command hangs
        /// `check` indefinitely. Must be >= 1.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,

        /// Drop stdout lines matching this regex before comparing (repeatable).
        /// Use it to strip timestamps/timings that legitimately vary.
        #[arg(long = "ignore", value_name = "REGEX")]
        ignore: Vec<String>,

        /// The command to run, then its arguments. Put it after `--` so its own
        /// flags are not parsed by `check` (e.g. `check -- cargo run --quiet`).
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            required = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },

    /// Check that DST simulations actually assert something — that invariants are
    /// PRESENT, not that they are correct.
    ///
    /// Seeded replay proves a run is reproducible; it says nothing about whether
    /// the run checks anything. This flags files that exercise the simulation
    /// surface (`SimScheduler`, `FaultSchedule`, `SimulatedRandom`, …) but register
    /// no invariant and make no assertion. It certifies that assertions EXIST; it
    /// can never certify they are RIGHT — that is domain policy the author owns.
    ///
    /// Tiers, per simulation site (file):
    ///   MISSING  — asserts nothing (no `Invariant`/live `InvariantEngine`, no
    ///              `.check()`, no `assert!` family). `InvariantEngine::new(vec![])`
    ///              with an empty list counts as MISSING. `--deny` fails on these.
    ///   RAW-ONLY — only raw `assert!`-family macros, no declared invariant.
    ///              Advisory; fails the gate only under `--deny-raw`.
    ///   OK       — a real invariant is constructed or evaluated.
    ///
    /// A NAME-BASED heuristic like `scan`: it errs toward NOT reporting MISSING on
    /// a file that plausibly asserts (a false OK is the safe direction for a gate),
    /// and it only sees files it can read and parse — an unreadable file makes a
    /// `--deny` gate exit 2 rather than pass green.
    ///
    /// SCOPE: analysis is per FILE. A file with several tests is one site, so a
    /// vacuous simulation in one test can be masked by an assertion in another test
    /// in the same file (a false OK — the safe direction). Per-test attribution is
    /// a planned refinement.
    ///
    /// Exit codes: 0 = clean (or findings without a gate); 1 = a gated tier fired;
    /// 2 = usage error, or (under a gate) a file could not be read/parsed.
    Invariants {
        /// Directory (or file) to check. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Output format: `human` (default) or `json`. `sarif`/`github` are
        /// rejected (exit 2) — they are not meaningful for this command.
        #[arg(long, value_enum, value_name = "human|json")]
        format: Option<Format>,

        /// Turn this into a CI gate: exit non-zero if any simulation site is
        /// MISSING (asserts nothing).
        #[arg(long)]
        deny: bool,

        /// Also fail the gate on RAW-ONLY sites (raw `assert!` macros but no
        /// declared invariant). Implies `--deny`.
        #[arg(long)]
        deny_raw: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Scan {
            path,
            format,
            json,
            baseline,
            deny,
            deny_level,
        } => {
            // Resolve the output format. `--format` wins; the legacy `--json`
            // flag is the back-compat alias for `--format json`.
            let format = format.unwrap_or(if json { Format::Json } else { Format::Human });

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

            // Load the baseline BEFORE scanning: a missing/invalid baseline is a
            // usage error (exit 2), unconditionally — never a silent no-op that
            // would let the gate pass on a tree it did not really filter.
            let mut suppress = match &baseline {
                Some(bp) => match Baseline::load(bp) {
                    Ok(b) => b.fingerprint_counts(),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(EXIT_USAGE);
                    }
                },
                None => std::collections::HashMap::new(),
            };

            // A path that does not exist is a usage error (exit 2), not a
            // "clean" scan — never let a typo look like a passing gate.
            if !path.exists() {
                eprintln!("error: path does not exist: {}", path.display());
                return ExitCode::from(EXIT_USAGE);
            }

            let mut report = scan_path(&path);

            // Apply baseline suppression: drop findings whose fingerprint is in
            // the baseline, from BOTH the output and the gate decision below.
            // Count-bounded: at most `count` occurrences of each fingerprint are
            // suppressed, so an ADDED identical leak (same rule/function/snippet
            // in another file or a repeated call) exceeds the baseline count and
            // survives to the gate as a NEW finding.
            if let Some(bp) = &baseline {
                let before = report.leaks.len();
                report.leaks.retain(|l| {
                    if let Some(remaining) = suppress.get_mut(&fingerprint(l)) {
                        if *remaining > 0 {
                            *remaining -= 1;
                            return false; // within baseline budget → suppress
                        }
                    }
                    true // no budget left → a NEW finding, keep it
                });
                let suppressed = before - report.leaks.len();
                eprintln!(
                    "suppressed {suppressed} finding(s) via baseline {}",
                    bp.display()
                );
            }

            match format {
                Format::Human => emit_human(&report, threshold),
                Format::Json => emit_json(&report),
                Format::Sarif => emit_sarif(&report),
                Format::Github => emit_github(&report),
            }

            // In a machine format WITHOUT a gate, skipped files would otherwise
            // be invisible — the human report lists them and the gate exits 2 on
            // them, but plain `scan --format sarif .` must not look "clean" for
            // files it never analyzed. Warn on stderr so stdout stays valid.
            if threshold.is_none()
                && !matches!(format, Format::Human)
                && !report.parse_failures.is_empty()
            {
                eprintln!(
                    "warning: {} file(s) could not be read or parsed and were NOT analyzed:",
                    report.parse_failures.len()
                );
                for f in &report.parse_failures {
                    eprintln!("  {f}");
                }
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
        Commands::Baseline { path, out } => {
            // Same "missing path is a usage error" rule as scan.
            if !path.exists() {
                eprintln!("error: path does not exist: {}", path.display());
                return ExitCode::from(EXIT_USAGE);
            }
            let report = scan_path(&path);
            let baseline = Baseline::from_report(&report);
            match std::fs::write(&out, baseline.to_pretty_json()) {
                Ok(()) => {
                    println!(
                        "Baselined {} finding(s) → {}",
                        baseline.findings.len(),
                        out.display()
                    );
                    if !report.parse_failures.is_empty() {
                        println!(
                            "  note: {} file(s) could not be parsed and were not baselined:",
                            report.parse_failures.len()
                        );
                        for f in &report.parse_failures {
                            println!("    {f}");
                        }
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: could not write baseline {}: {e}", out.display());
                    ExitCode::from(EXIT_USAGE)
                }
            }
        }
        Commands::Explain {
            rule_id,
            list,
            format,
        } => {
            let as_json = matches!(format, Some(Format::Json));
            // Bare `explain` or `explain --list` → list every rule.
            match rule_id {
                None => {
                    emit_explain_list(as_json);
                    ExitCode::SUCCESS
                }
                Some(_) if list => {
                    // `--list` is a list request; a stray id alongside it is
                    // ignored in favor of the list (no silent surprise).
                    emit_explain_list(as_json);
                    ExitCode::SUCCESS
                }
                Some(id) => match (rule_by_id(&id), fix_by_id(&id)) {
                    (Some(rule), Some(fix)) => {
                        emit_explain_one(rule, &fix, as_json);
                        ExitCode::SUCCESS
                    }
                    // Unknown id (or, defensively, a catalog rule missing fix
                    // content) → usage error with a pointer to `--list`.
                    _ => {
                        eprintln!(
                            "error: unknown rule id `{id}`. Run `navian-dst explain --list` \
                             to see every rule id."
                        );
                        ExitCode::from(EXIT_USAGE)
                    }
                },
            }
        }
        Commands::Init => init_scaffold(),
        Commands::Check {
            seed,
            runs,
            timeout,
            ignore,
            command,
        } => check::run_check(seed, runs, timeout, &ignore, &command),
        Commands::Invariants {
            path,
            format,
            deny,
            deny_raw,
        } => {
            // `--deny-raw` implies `--deny`; `deny` is the base gate.
            let gated = deny || deny_raw;

            // Only human/json are meaningful here; reject sarif/github explicitly
            // rather than silently treating them as human (the help says
            // human|json, so accepting more would be a lie).
            if matches!(format, Some(Format::Sarif) | Some(Format::Github)) {
                eprintln!("error: `invariants` supports only --format human|json");
                return ExitCode::from(EXIT_USAGE);
            }

            // A path that does not exist is a usage error (exit 2), not a "clean"
            // check — never let a typo look like a passing gate.
            if !path.exists() {
                eprintln!("error: path does not exist: {}", path.display());
                return ExitCode::from(EXIT_USAGE);
            }

            let report = check_path(&path);

            // `--format json` (and the legacy `--json` is not offered here) emits
            // the site array; everything else is the human report.
            match format {
                Some(Format::Json) => emit_invariants_json(&report),
                _ => emit_invariants_human(&report, gated, deny_raw),
            }

            // Exit-code contract mirrors `scan`:
            //   2 — under a gate, a file could not be read/parsed (cannot certify);
            //   1 — a gated tier fired (MISSING always; RAW-ONLY under --deny-raw);
            //   0 — otherwise.
            if gated && report.uncertifiable() {
                eprintln!(
                    "error: cannot certify — {} file(s) could not be read or parsed:",
                    report.parse_failures.len()
                );
                for f in &report.parse_failures {
                    eprintln!("  {f}");
                }
                return ExitCode::from(EXIT_USAGE);
            }
            let gate_fails =
                (gated && report.any_missing()) || (deny_raw && report.any_raw_only());
            if gate_fails {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

/// The GitHub Actions workflow written by `init`.
const WORKFLOW_YML: &str = r#"# navian-dst — determinism-leak CI gate.
# Generated by `navian-dst init`. Safe to edit.
name: navian-dst

on:
  push:
  pull_request:

jobs:
  determinism-gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Install navian-dst
        run: cargo install navian-dst-cli
      - name: Determinism-leak gate
        # Fails the build on high-confidence findings. Add `--deny-level medium`
        # to tighten, or `--baseline navian-dst-baseline.json` to gate only on
        # NEW findings while adopting on a dirty tree (see `navian-dst baseline`).
        run: navian-dst scan --deny .

      # --- Optional: upload results to GitHub code scanning instead of (or in
      # --- addition to) failing inline. Uncomment to enable.
      # - name: Determinism scan (SARIF)
      #   if: always()
      #   run: navian-dst scan --format sarif . > navian-dst.sarif
      # - name: Upload SARIF
      #   if: always()
      #   uses: github/codeql-action/upload-sarif@v3
      #   with:
      #     sarif_file: navian-dst.sarif
"#;

/// The config template written by `init`.
const CONFIG_TOML: &str = r#"# navian-dst.toml — TEMPLATE / stub.
#
# NOTE: This file is a TEMPLATE. The navian-dst CLI does NOT read it yet — the
# keys below simply document the command-line flags you pass today. Editing
# values here has NO effect on a scan; wiring this config into `scan` is planned
# but not implemented. Use CLI flags for real behavior.

# Confidence threshold the `--deny` gate fails on: "high" | "medium" | "advisory".
# CLI equivalent today: navian-dst scan --deny-level high
deny_level = "high"

# Paths to exclude from scanning (example). NOT honored by the CLI yet.
# exclude = ["target", "vendor", "third_party"]
"#;

/// Scaffold CI + config for adopting navian-dst. Conservative: never overwrites
/// an existing file — it prints whether each path was created or skipped, and
/// creates only what actually works today (the config is a labeled template).
fn init_scaffold() -> ExitCode {
    let files: [(&str, &str); 2] = [
        (".github/workflows/navian-dst.yml", WORKFLOW_YML),
        ("navian-dst.toml", CONFIG_TOML),
    ];
    let mut any_err = false;
    for (rel, contents) in files {
        let path = Path::new(rel);
        if path.exists() {
            println!("skip    {rel} (already exists — left untouched)");
            continue;
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("error: could not create {}: {e}", parent.display());
                    any_err = true;
                    continue;
                }
            }
        }
        match std::fs::write(path, contents) {
            Ok(()) => println!("create  {rel}"),
            Err(e) => {
                eprintln!("error: could not write {rel}: {e}");
                any_err = true;
            }
        }
    }
    if any_err {
        ExitCode::from(EXIT_USAGE)
    } else {
        ExitCode::SUCCESS
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

/// Human report for `invariants`: one line per simulation site, grouped so the
/// gate-failing MISSING sites are impossible to miss, then a summary + gate line.
fn emit_invariants_human(report: &PresenceReport, gated: bool, deny_raw: bool) {
    println!("== Invariant presence ==\n");

    if report.sites.is_empty() {
        println!("No simulation sites found (no use of SimScheduler / FaultSchedule / Simulated*).\n");
    }

    // MISSING first (the defect), then RAW-ONLY (advisory), then OK.
    for (status, heading) in [
        (Status::Missing, "MISSING — simulates but asserts nothing"),
        (Status::RawOnly, "RAW-ONLY — only raw assert! macros, no declared invariant"),
        (Status::Ok, "OK — a real invariant is registered or checked"),
    ] {
        let sites: Vec<_> = report.sites.iter().filter(|s| s.status == status).collect();
        if sites.is_empty() {
            continue;
        }
        println!("{} ({})", heading, sites.len());
        for s in sites {
            let markers = s.sim_markers.join(", ");
            let detail = match s.status {
                Status::Missing if s.empty_engine => "  — empty InvariantEngine::new([]) registers nothing".to_string(),
                Status::RawOnly => format!("  — {} raw assert(s)", s.assert_macros),
                _ => String::new(),
            };
            // A waived site is reported but does not fail the gate — flag it so
            // the exemption is visible, not silent.
            let waiver = if s.waived { "  (WAIVED — invariants elsewhere)" } else { "" };
            println!("  {}:{}  [{}]  uses {}{}{}", s.file, s.sim_line, s.status.label(), markers, detail, waiver);
        }
        println!();
    }

    println!(
        "Scanned {} file(s): {} parsed, {} skipped (read/parse error).",
        report.files_scanned,
        report.files_parsed,
        report.parse_failures.len()
    );
    if !report.parse_failures.is_empty() {
        for f in &report.parse_failures {
            println!("  warning: could not parse {f} (skipped)");
        }
    }
    let waived = report.sites.iter().filter(|s| s.waived).count();
    println!(
        "{} simulation site(s): {} ok, {} raw-only, {} missing{}.",
        report.sites.len(),
        report.tally(Status::Ok),
        report.tally(Status::RawOnly),
        report.tally(Status::Missing),
        if waived > 0 { format!(" ({waived} waived)") } else { String::new() },
    );

    // Gate line: describe the decision the exit code will encode.
    if gated {
        if report.uncertifiable() {
            println!(
                "GATE: cannot certify — {} file(s) could not be read or parsed.",
                report.parse_failures.len()
            );
        } else if report.any_missing() {
            println!("GATE: FAIL — simulation site(s) assert nothing (MISSING).");
        } else if deny_raw && report.any_raw_only() {
            println!("GATE: FAIL — RAW-ONLY site(s) under --deny-raw.");
        } else {
            let waived = report.sites.iter().filter(|s| s.waived).count();
            if waived > 0 {
                println!(
                    "GATE: pass — every non-waived simulation site asserts something ({waived} waived)."
                );
            } else {
                println!("GATE: pass — every simulation site asserts something.");
            }
        }
    }

    // Honesty footer: a clean result is not a correctness claim.
    println!(
        "\nNote: this certifies invariants are PRESENT, not that they are correct — \
         that stays with the author, written and reviewed as domain policy."
    );
}

/// Machine-readable form of `invariants`: a full report object. Emits the sites
/// AND the scan counts / parse failures / uncertifiable flag, so a machine
/// consumer under a gate can see that the tree was not fully certifiable without
/// having to parse stderr and the exit code.
fn emit_invariants_json(report: &PresenceReport) {
    let doc = serde_json::json!({
        "sites": report.sites,
        "files_scanned": report.files_scanned,
        "files_parsed": report.files_parsed,
        "parse_failures": report.parse_failures,
        "uncertifiable": report.uncertifiable(),
    });
    match serde_json::to_string_pretty(&doc) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize report: {e}"),
    }
}

/// A short, actionable next step for a finding, keyed off whether `migrate` can
/// rewrite it. Used as the `fix_hint` field in `scan --json`.
fn fix_hint(rule_id: &str, autofixable: bool) -> String {
    if autofixable {
        format!("navian-dst migrate can rewrite this (run: navian-dst migrate); details: navian-dst explain {rule_id}")
    } else {
        format!("run: navian-dst explain {rule_id}")
    }
}

fn emit_json(report: &ScanReport) {
    use serde_json::json;

    // Back-compat: the top-level shape stays the stable leak ARRAY, and every
    // item keeps its existing keys in place ({rule_id, confidence, category,
    // file, line, col, function, snippet}). Each item gains three ADDITIVE
    // agent-worklist keys: `suggested_seam`, `autofixable`, `fix_hint`. We do
    // NOT wrap the array in a top-level object (that would break consumers that
    // read the array directly), so no top-level `summary` is emitted here.
    let enriched: Vec<serde_json::Value> = report
        .leaks
        .iter()
        .map(|l| {
            // `Leak` is a plain `#[derive(Serialize)]` of owned scalars/strings,
            // so serialization is infallible; `expect` guards the stable contract
            // (a finding must never silently lose its 8 keys).
            let mut v = serde_json::to_value(l).expect("Leak serializes");
            let (seam, autofixable) = match fix_by_id(l.rule_id) {
                Some(f) => (f.suggested_seam.to_string(), f.autofixable),
                // Defensive: every catalog rule has fix content (enforced by a
                // test), so this branch is unreachable for real findings.
                None => (String::new(), false),
            };
            if let serde_json::Value::Object(map) = &mut v {
                map.insert("suggested_seam".into(), json!(seam));
                map.insert("autofixable".into(), json!(autofixable));
                map.insert("fix_hint".into(), json!(fix_hint(l.rule_id, autofixable)));
            }
            v
        })
        .collect();

    match serde_json::to_string_pretty(&enriched) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize leaks: {e}"),
    }
}

/// List every rule id + one-line description, as human text or a JSON array.
fn emit_explain_list(as_json: bool) {
    use scanner::rules::CATALOG;

    if as_json {
        let arr: Vec<serde_json::Value> = CATALOG
            .iter()
            .map(|r| {
                serde_json::json!({
                    "rule_id": r.id,
                    "category": r.category,
                    "confidence": r.confidence,
                    "description": r.description,
                    "autofixable": fix_by_id(r.id).map(|f| f.autofixable).unwrap_or(false),
                })
            })
            .collect();
        match serde_json::to_string_pretty(&arr) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("failed to serialize rule list: {e}"),
        }
        return;
    }

    println!("Determinism rules ({}):\n", CATALOG.len());
    for r in CATALOG {
        println!(
            "  {:<15} [{:<8}] {:<12} {}",
            r.id,
            r.confidence.label(),
            r.category.label(),
            r.description
        );
    }
    println!("\nRun `navian-dst explain <RULE-ID>` for the full fix recipe.");
}

/// Explain one rule, as a human recipe or a structured JSON object.
fn emit_explain_one(rule: &scanner::Rule, fix: &scanner::RuleFix, as_json: bool) {
    if as_json {
        // The documented agent schema:
        // {rule_id, category, confidence, why, suggested_seam, autofixable, before, after}.
        let doc = serde_json::json!({
            "rule_id": rule.id,
            "category": rule.category,
            "confidence": rule.confidence,
            "why": fix.why,
            "suggested_seam": fix.suggested_seam,
            "autofixable": fix.autofixable,
            "before": fix.before,
            "after": fix.after,
        });
        match serde_json::to_string_pretty(&doc) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("failed to serialize explain: {e}"),
        }
        return;
    }

    println!(
        "{}  [{}]  {}",
        rule.id,
        rule.confidence.label(),
        rule.category.label()
    );
    println!("{}\n", rule.description);

    println!("Why it is nondeterministic:");
    println!("  {}\n", fix.why);

    println!("Injectable seam:");
    println!("  {}\n", fix.suggested_seam);

    println!("Before → after:");
    println!("  - {}", fix.before);
    println!("  + {}\n", fix.after);

    if fix.autofixable {
        println!("Auto-fixable: YES — `navian-dst migrate` can rewrite this. {MIGRATE_SCOPE}");
    } else {
        println!(
            "Auto-fixable: NO — `navian-dst migrate` will not fully auto-rewrite this rule \
             today. {MIGRATE_SCOPE} Apply the seam above by hand, or hand this recipe to your \
             coding agent."
        );
    }
}

/// The accurate, generic description of what `navian-dst migrate` handles today.
/// Reused everywhere migrate's scope is enumerated so no message over- or
/// under-claims. migrate rewrites a *conservative subset* of the Time family —
/// the `SystemTime::now()…as_millis()` idiom (→ `now_ms()`), `Instant::now()`
/// (→ `instant_now()`), and `tokio::time::sleep()` (→ `Time::sleep()`) — inside
/// inherent methods of named-field structs; it skips everything else (including
/// `tokio::time::sleep_until`/`interval`/`timeout`).
const MIGRATE_SCOPE: &str = "migrate handles a conservative subset of the Time \
family — the SystemTime millis idiom, Instant::now, and tokio::time::sleep — \
verifying each rewrite with `cargo check --all-targets` and rolling back anything \
it can't safely change.";

/// A short, human-readable SARIF rule `name` derived from the description: the
/// text before the em-dash (`SystemTime::now`), else the whole description.
fn sarif_rule_name(description: &str) -> &str {
    description.split(" — ").next().unwrap_or(description).trim()
}

/// Emit a SARIF 2.1.0 document.
///
/// One `reportingDescriptor` per catalog rule (the driver's rule set) and one
/// `result` per finding. Each result carries `partialFingerprints`
/// (`dstFingerprintV1`) so SARIF consumers can track a finding across moves —
/// the same fingerprint the `baseline` store uses.
fn emit_sarif(report: &ScanReport) {
    use serde_json::json;

    let rules: Vec<serde_json::Value> = scanner::rules::CATALOG
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "name": sarif_rule_name(r.description),
                "shortDescription": { "text": r.description },
                "defaultConfiguration": { "level": r.confidence.sarif_level() },
            })
        })
        .collect();

    let results: Vec<serde_json::Value> = report
        .leaks
        .iter()
        .map(|l| {
            let desc = rule_by_id(l.rule_id).map(|r| r.description).unwrap_or(l.rule_id);
            json!({
                "ruleId": l.rule_id,
                "level": l.confidence.sarif_level(),
                "message": { "text": format!("{desc}: {}", l.snippet) },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": l.file },
                        "region": { "startLine": l.line, "startColumn": l.col },
                    }
                }],
                "partialFingerprints": { "dstFingerprintV1": fingerprint(l) },
            })
        })
        .collect();

    let doc = json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "navian-dst",
                    "informationUri": INFORMATION_URI,
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules,
                }
            },
            "results": results,
        }],
    });

    match serde_json::to_string_pretty(&doc) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("failed to serialize SARIF: {e}"),
    }
}

/// Escape the message portion of a GitHub workflow command (`%`, CR, LF). `%`
/// MUST be escaped first so the introduced `%XX` sequences aren't re-escaped.
fn gh_escape_data(s: &str) -> String {
    s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

/// Escape a property value (e.g. `file=`), which additionally may not contain
/// the `,`/`:` delimiters GitHub uses between properties.
fn gh_escape_prop(s: &str) -> String {
    gh_escape_data(s).replace(',', "%2C").replace(':', "%3A")
}

/// Emit GitHub Actions workflow commands — one annotation per finding. High →
/// `::error`, Medium → `::warning`, Advisory → `::notice`. Exit codes are
/// unchanged; the gate is still governed by `--deny`.
fn emit_github(report: &ScanReport) {
    for l in &report.leaks {
        let cmd = match l.confidence {
            Confidence::High => "error",
            Confidence::Medium => "warning",
            Confidence::Advisory => "notice",
        };
        let desc = rule_by_id(l.rule_id).map(|r| r.description).unwrap_or(l.rule_id);
        let file = gh_escape_prop(&l.file);
        let title = gh_escape_prop(l.rule_id);
        let message = gh_escape_data(&format!("{desc}: {}", l.snippet));
        println!(
            "::{cmd} file={file},line={},col={},title={title}::{message}",
            l.line, l.col
        );
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
        Some(_) if report.uncertifiable() => println!(
            "GATE: cannot certify — {} file(s) could not be read or parsed.",
            report.parse_failures.len()
        ),
        Some(t) if report.any_at_or_above(t) => println!(
            "GATE: FAIL — findings at or above `{}` (--deny threshold).",
            t.label()
        ),
        Some(t) => println!("GATE: pass — no findings at or above `{}`.", t.label()),
        None => {}
    }
}
