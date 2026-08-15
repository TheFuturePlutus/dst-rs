//! `check` — the run-twice-diff determinism gesture.
//!
//! Runs a command several times under one fixed seed and reports whether its
//! output is IDENTICAL (deterministic) or DIVERGES (nondeterminism escaped). A
//! correctly-seeded workload reproduces byte-for-byte; anything that varies
//! run-to-run — an unseeded clock, real RNG, `HashMap` iteration order, thread
//! interleaving — surfaces here as a divergence even though the static `scan`
//! never sees it. This is the honest, self-contained "DST" gesture (à la
//! madsim's same-seed divergence check).
//!
//! ## What the seed does and does NOT control
//!
//! The `navian-dst` library takes its seed through its **API**
//! (`SimulatedRandom::from_seed`, `FaultSchedule::new(seed)`, the `SimScheduler`);
//! it reads **no** seed from the environment. `check` therefore cannot force a
//! seed onto library code — it exports [`SEED_ENV_PRIMARY`] and [`SEED_ENV_COMPAT`]
//! for the command to consult IF it chooses, and otherwise just runs the command
//! as given and diffs its output. So `check` catches gross / escaped
//! nondeterminism; it does not itself make a program deterministic.
//!
//! ## Hangs
//!
//! Without `--timeout`, a command that hangs (e.g. a deadlock from
//! nondeterministic thread interleaving) hangs `check` too — a hang is not
//! auto-surfaced as a divergence. `--timeout <secs>` kills a run that exceeds
//! the budget and reports the timeout, turning an indefinite hang into an
//! actionable failure.

use std::io::Read as _;
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use regex::bytes::Regex;

/// The forward-looking seed env var `check` exports to the child process.
pub const SEED_ENV_PRIMARY: &str = "NAVIAN_DST_SEED";
/// A compatibility alias also exported (matches the `SEED` convention used by
/// some navian-dst test harnesses). Both carry the same value.
pub const SEED_ENV_COMPAT: &str = "SEED";

/// Upper bound on how many run results we pre-reserve. `--runs` is an unbounded
/// `u32`; reserving `runs` eagerly would let `--runs 4000000000` abort on a
/// tens-of-GB allocation. The loop still honors the real run count — this only
/// caps the *initial* reservation.
const RESERVE_CAP: usize = 64;

/// The captured result of one run of the command.
struct RunResult {
    /// stdout after the `--ignore` line filters have been applied. Kept as raw
    /// bytes so a byte-exact comparison never masks a divergence that only
    /// appears in non-UTF-8 output (e.g. a raw-bytes replay hash).
    filtered_stdout: Vec<u8>,
    /// Process exit status (code, or signal on unix; a signal kill for a
    /// timed-out run).
    status: ExitStatus,
    /// stderr, kept for the divergence report (NOT part of the comparison key).
    stderr: String,
    /// Whether this run was killed for exceeding `--timeout`.
    timed_out: bool,
}

impl RunResult {
    /// Two runs are considered identical when their timeout outcome, filtered
    /// stdout, AND exit status match. stderr is reported but deliberately
    /// excluded from the key, per the documented comparison semantics.
    fn matches(&self, other: &RunResult) -> bool {
        self.timed_out == other.timed_out
            && self.filtered_stdout == other.filtered_stdout
            && self.status == other.status
    }
}

/// Drop every line matching any `--ignore` pattern, then rejoin. Splits on `\n`
/// and strips a trailing `\r`, mirroring `str::lines()` so a trailing-newline or
/// `\r\n` difference never masquerades as a divergence. Operates on raw bytes to
/// keep the comparison byte-exact.
fn filter_stdout(raw: &[u8], ignores: &[Regex]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = raw;
    while !rest.is_empty() {
        let (line, tail) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        // Strip a trailing `\r` so `\r\n` and `\n` line endings compare equal.
        let line = match line.last() {
            Some(&b'\r') => &line[..line.len() - 1],
            _ => line,
        };
        if !ignores.iter().any(|re| re.is_match(line)) {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
        rest = tail;
    }
    out
}

/// A compact unified-diff of two multi-line strings (LCS over lines). No `@@`
/// hunk ranges — the whole body is emitted with ` `/`-`/`+` prefixes, which is
/// enough to read a divergence at a glance. Guards against a pathological
/// `O(n*m)` DP allocation on very large outputs by falling back to a head dump.
fn unified_diff(a: &str, b: &str, a_label: &str, b_label: &str) -> String {
    use std::fmt::Write as _;

    let a_lines: Vec<&str> = a.lines().collect();
    let b_lines: Vec<&str> = b.lines().collect();
    let n = a_lines.len();
    let m = b_lines.len();

    let mut out = String::new();
    let _ = writeln!(out, "--- {a_label}");
    let _ = writeln!(out, "+++ {b_label}");

    // Cap BOTH the DP cell count (n*m) AND the per-row allocation count
    // (max(n,m) — the DP table is `n+1` nested Vecs). A one-large/one-empty
    // divergence (a run that crashes and prints nothing) has a tiny product but
    // a huge dimension, and would otherwise allocate millions of Vecs, so the
    // dimension guard is load-bearing, not belt-and-suspenders.
    if n.saturating_mul(m) > 4_000_000 || n.max(m) > 20_000 {
        let _ = writeln!(out, "(outputs too large for a full diff — showing the first difference)");
        match a_lines.iter().zip(b_lines.iter()).position(|(x, y)| x != y) {
            Some(k) => {
                let _ = writeln!(out, "-{}", a_lines[k]);
                let _ = writeln!(out, "+{}", b_lines[k]);
            }
            // Every shared-index line matched → one output is a prefix of the
            // other; the divergence is purely a length/truncation difference.
            None => {
                let _ = writeln!(out, "(one output is a prefix of the other)");
            }
        }
        if n != m {
            let _ = writeln!(out, "(run 1 has {n} line(s); the other has {m})");
        }
        return out;
    }

    // dp[i][j] = LCS length of a_lines[i..] and b_lines[j..].
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a_lines[i] == b_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a_lines[i] == b_lines[j] {
            let _ = writeln!(out, " {}", a_lines[i]);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            let _ = writeln!(out, "-{}", a_lines[i]);
            i += 1;
        } else {
            let _ = writeln!(out, "+{}", b_lines[j]);
            j += 1;
        }
    }
    while i < n {
        let _ = writeln!(out, "-{}", a_lines[i]);
        i += 1;
    }
    while j < m {
        let _ = writeln!(out, "+{}", b_lines[j]);
        j += 1;
    }
    out
}

/// Human-friendly rendering of an exit status for the report.
fn status_str(status: &ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("exit code {c}"),
        None => format!("{status}"), // signal death (unix) has no code
    }
}

/// Shell-quote a single argument: single-quote it if it is empty or contains
/// whitespace/quotes, escaping embedded single quotes as `'\''` so the result is
/// copy-paste runnable.
fn shell_quote(arg: &str) -> String {
    if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '\'') {
        format!("'{}'", arg.replace('\'', "'\\''"))
    } else {
        arg.to_string()
    }
}

/// Shell-safe rendering of the command, only for the "to reproduce" hint.
fn command_display(command: &[String]) -> String {
    command
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `command` once with the seed env exported, capturing stdout/stderr/status.
///
/// stdout and stderr are drained on dedicated threads so the child never blocks
/// on a full pipe while we wait — the deadlock trap for any timeout scheme. With
/// `timeout` set, a run that overruns is killed and reaped (no zombie) and
/// flagged `timed_out`. Returns `Err` only when the process could not be spawned.
fn run_once(
    seed: u64,
    command: &[String],
    ignores: &[Regex],
    timeout: Option<Duration>,
) -> std::io::Result<RunResult> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .env(SEED_ENV_PRIMARY, seed.to_string())
        .env(SEED_ENV_COMPAT, seed.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Drain both pipes concurrently so a chatty child can't wedge us.
    let mut child_out = child.stdout.take().expect("child stdout is piped");
    let mut child_err = child.stderr.take().expect("child stderr is piped");
    // The readers send their buffer over a channel and are DETACHED (we never
    // join them). Detaching is what lets `--timeout` be a true wall-clock bound:
    // `kill()` reaps only the DIRECT child, so a grandchild that inherited the
    // pipe (a backgrounded process, a deadlocked worker) keeps the write end
    // open and `read_to_end` would block forever — a `join()` here would hang.
    // With channels we instead WAIT on the receiver with a deadline and walk
    // away from a reader still blocked in the kernel; it ends at process exit.
    let (tx_out, rx_out) = mpsc::channel();
    let (tx_err, rx_err) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_out.read_to_end(&mut buf);
        let _ = tx_out.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_err.read_to_end(&mut buf);
        let _ = tx_err.send(buf);
    });

    // Build a timed-out result (readers detached; a timed-out run's output is
    // unused because `run_check` surfaces the timeout before any diff).
    let timed_out = |status: ExitStatus| RunResult {
        filtered_stdout: Vec::new(),
        status,
        stderr: String::new(),
        timed_out: true,
    };

    // Without a timeout, wait unbounded (a hang hangs `check` — documented).
    let dur = match timeout {
        None => {
            let status = child.wait()?;
            let raw_stdout = rx_out.recv().unwrap_or_default();
            let raw_stderr = rx_err.recv().unwrap_or_default();
            return Ok(RunResult {
                filtered_stdout: filter_stdout(&raw_stdout, ignores),
                status,
                stderr: String::from_utf8_lossy(&raw_stderr).into_owned(),
                timed_out: false,
            });
        }
        Some(dur) => dur,
    };

    // Bounded path. ONE deadline governs BOTH the child's exit AND the output
    // drain. `start.elapsed()` (never `Instant::now() + dur`) so an absurd
    // `--timeout` can't overflow-panic.
    let start = Instant::now();

    // Phase 1 — wait for the direct child to exit within the deadline.
    let status = loop {
        match child.try_wait()? {
            Some(st) => break st,
            None => {
                if start.elapsed() >= dur {
                    child.kill().ok();
                    let st = child.wait()?; // reap the killed child (no zombie)
                    return Ok(timed_out(st));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };

    // Phase 2 — the child exited in time, but a grandchild may still hold the
    // pipe open. Bound the drain by the REMAINING budget so it can't hang; if it
    // does not arrive in time, the run has not completed → report it as a
    // timeout rather than block.
    let raw_stdout = match rx_out.recv_timeout(dur.saturating_sub(start.elapsed())) {
        Ok(buf) => buf,
        Err(_) => return Ok(timed_out(status)),
    };
    let raw_stderr = match rx_err.recv_timeout(dur.saturating_sub(start.elapsed())) {
        Ok(buf) => buf,
        Err(_) => return Ok(timed_out(status)),
    };

    Ok(RunResult {
        filtered_stdout: filter_stdout(&raw_stdout, ignores),
        status,
        stderr: String::from_utf8_lossy(&raw_stderr).into_owned(),
        timed_out: false,
    })
}

/// Entry point for the `check` subcommand.
///
/// Exit-code contract (mirrors the tool's 0/1/2 convention):
/// - `0` — every run's (filtered stdout, exit status) is identical (deterministic;
///   this includes a command that FAILS identically every run — `check` measures
///   divergence, not pass/fail);
/// - `1` — at least one run diverged, or a run exceeded `--timeout`;
/// - `2` — tool/usage error (bad `--runs`/`--timeout`, bad `--ignore` regex, or
///   the command could not be spawned).
pub fn run_check(
    seed: u64,
    runs: u32,
    timeout_secs: Option<u64>,
    ignore: &[String],
    command: &[String],
) -> ExitCode {
    // A single run can never diverge from itself; the gesture needs at least two.
    if runs < 2 {
        eprintln!("error: --runs must be >= 2 (the check needs at least two runs to compare)");
        return ExitCode::from(crate::EXIT_USAGE);
    }
    // A zero-second timeout would kill every run instantly — reject it.
    let timeout = match timeout_secs {
        Some(0) => {
            eprintln!("error: --timeout must be >= 1 second");
            return ExitCode::from(crate::EXIT_USAGE);
        }
        Some(s) => Some(Duration::from_secs(s)),
        None => None,
    };
    // clap enforces `required = true`, but never trust the caller.
    if command.is_empty() {
        eprintln!("error: a command to run is required (e.g. `navian-dst check -- my-test`)");
        return ExitCode::from(crate::EXIT_USAGE);
    }

    // Compile the ignore patterns up front; a bad regex is a usage error.
    let mut ignores = Vec::with_capacity(ignore.len());
    for pat in ignore {
        match Regex::new(pat) {
            Ok(re) => ignores.push(re),
            Err(e) => {
                eprintln!("error: invalid --ignore regex `{pat}`: {e}");
                return ExitCode::from(crate::EXIT_USAGE);
            }
        }
    }

    // Execute the command `runs` times. Reserve conservatively — `runs` is an
    // unbounded u32 (see RESERVE_CAP).
    let mut results = Vec::with_capacity((runs as usize).min(RESERVE_CAP));
    for n in 0..runs {
        match run_once(seed, command, &ignores, timeout) {
            Ok(r) => results.push(r),
            Err(e) => {
                // Not-found / not-executable / etc. — the command never ran, so
                // this is a tool/usage error, not a divergence.
                eprintln!(
                    "error: could not run command `{}` (run {}): {e}",
                    command[0],
                    n + 1
                );
                return ExitCode::from(crate::EXIT_USAGE);
            }
        }
    }

    // Reproduce hints must carry the SAME `--ignore` (and `--timeout`) or the
    // re-run would filter differently and report a different divergence.
    let ignore_flags: String = ignore
        .iter()
        .map(|p| format!(" --ignore {}", shell_quote(p)))
        .collect();
    let timeout_flag = match timeout_secs {
        Some(s) => format!(" --timeout {s}"),
        None => String::new(),
    };

    // A hang killed by --timeout is surfaced first: it is not a byte divergence
    // (the run never finished) but it is exactly the flaky-test symptom we must
    // not swallow as an infinite hang.
    if let Some(idx) = results.iter().position(|r| r.timed_out) {
        let secs = timeout_secs.unwrap_or(0);
        println!(
            "TIMEOUT under seed {seed}: run {} did not complete within the --timeout {secs}s budget.",
            idx + 1
        );
        println!(
            "A command that hangs (e.g. a deadlock from nondeterministic thread interleaving) \
             does not complete under a fixed seed."
        );
        println!();
        println!("To reproduce a single run under this seed:");
        println!(
            "  {SEED_ENV_PRIMARY}={seed} {SEED_ENV_COMPAT}={seed} {}",
            command_display(command)
        );
        return ExitCode::FAILURE;
    }

    // Compare every run against the first; report the first that diverges.
    let baseline = &results[0];
    if let Some(idx) = results.iter().position(|r| !baseline.matches(r)) {
        let other = &results[idx];
        println!(
            "DIVERGENCE under seed {seed}: run 1 and run {} produced different output.",
            idx + 1
        );
        println!("  run 1      {}", status_str(&baseline.status));
        println!("  run {}      {}", idx + 1, status_str(&other.status));

        if baseline.filtered_stdout != other.filtered_stdout {
            // The comparison above is byte-exact; the diff is a text rendering.
            let a = String::from_utf8_lossy(&baseline.filtered_stdout);
            let b = String::from_utf8_lossy(&other.filtered_stdout);
            println!();
            if a == b {
                // Bytes differ but both lossy-decode identically → the divergence
                // is purely in non-UTF-8 bytes a text diff can't show.
                println!("  (filtered stdout differs only in non-UTF-8 bytes; not shown)");
            } else {
                print!(
                    "{}",
                    unified_diff(
                        &a,
                        &b,
                        "run 1 (stdout, filtered)",
                        &format!("run {} (stdout, filtered)", idx + 1),
                    )
                );
            }
        } else {
            println!("  (filtered stdout is identical; the exit status differs)");
        }

        // Surface stderr only when it actually differs, so the report stays
        // focused; it is not part of the comparison key.
        if baseline.stderr != other.stderr {
            println!("  note: stderr also differs between the two runs (not compared).");
        }

        println!();
        println!("To reproduce a single run under this seed:");
        println!(
            "  {SEED_ENV_PRIMARY}={seed} {SEED_ENV_COMPAT}={seed} {}",
            command_display(command)
        );
        println!(
            "Then diff the outputs, or re-run: navian-dst check --seed {seed} --runs {runs}{timeout_flag}{ignore_flags} -- {}",
            command_display(command)
        );
        return ExitCode::FAILURE;
    }

    println!(
        "deterministic: {runs} runs identical under seed {seed} ({}).",
        status_str(&baseline.status)
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(p: &str) -> Regex {
        Regex::new(p).unwrap()
    }

    #[test]
    fn filter_drops_matching_lines() {
        let raw = b"keep 1\ntimestamp: 12:00\nkeep 2\n";
        let out = filter_stdout(raw, &[re(r"^timestamp:")]);
        assert_eq!(out, b"keep 1\nkeep 2\n");
    }

    #[test]
    fn filter_no_patterns_normalizes_newline() {
        // No ignore patterns → identity up to a normalized trailing newline.
        assert_eq!(filter_stdout(b"a\nb", &[]), b"a\nb\n");
        // A trailing newline does NOT create a spurious empty final line.
        assert_eq!(filter_stdout(b"a\nb\n", &[]), b"a\nb\n");
    }

    #[test]
    fn filter_strips_carriage_returns() {
        // `\r\n` and `\n` line endings compare equal after filtering.
        assert_eq!(filter_stdout(b"a\r\nb\r\n", &[]), b"a\nb\n");
    }

    #[test]
    fn filter_compares_raw_non_utf8_bytes() {
        // Two DIFFERENT invalid-UTF-8 lines must NOT collapse to equal.
        let a = filter_stdout(&[0xff, b'\n'], &[]);
        let b = filter_stdout(&[0xfe, b'\n'], &[]);
        assert_ne!(a, b, "distinct non-UTF-8 output must stay distinct");
    }

    #[test]
    fn unified_diff_marks_changed_line() {
        let d = unified_diff("a\nb\nc\n", "a\nX\nc\n", "one", "two");
        assert!(d.contains("--- one"), "{d}");
        assert!(d.contains("+++ two"), "{d}");
        assert!(d.contains("-b"), "{d}");
        assert!(d.contains("+X"), "{d}");
        assert!(d.contains(" a") && d.contains(" c"), "{d}");
    }

    #[test]
    fn command_display_escapes_single_quotes() {
        let d = command_display(&["it's".to_string(), "plain".to_string()]);
        // The embedded quote is escaped as '\'' and `plain` is left bare.
        assert_eq!(d, r"'it'\''s' plain");
    }
}
