//! Retrieval benchmarks for Thetis: SkillRet and ToolRet.
//!
//! Emits one JSON datapoint per run so that changes to skill cards, the ranker,
//! the group table or the tag lists can be measured rather than argued about,
//! and plotted over time.
//!
//! The design decision that matters: the real ranking source is *lifted*, never
//! restated. `skills.rs`, `skill_index.rs` and `skill_lint.rs` are copied
//! verbatim into a worktree by the runner and `include!`d below; the group table
//! is extracted from `groups.rs` by extract.py. So the benchmark cannot pass
//! while the shipping ranker is broken, which a hand-written mock could. The
//! files chosen depend only on anyhow, toml, tracing and std -- no wasmtime, no
//! redb, no host -- so this compiles in seconds where the orchestrator takes
//! minutes, which is what makes running it over a dozen historical revisions
//! practical.
//!
//! Usage:
//!   retrieval-bench                     measure the working tree
//!   retrieval-bench --json out.json     write the datapoint to a file
//!   retrieval-bench --verbose           per-case detail, for diagnosing a drop
//!
//! To measure past revisions, use ./run.sh --rev <sha> instead: this binary only
//! ever measures the tree it was compiled against.

mod embed;
mod metrics;
mod skillret;
mod toolret;

mod lifted;

// The lifted files refer to their siblings as `crate::skills` and
// `crate::skill_lint`, since in the orchestrator they are crate-level modules.
// These aliases make those paths resolve here without touching the copies.
pub use lifted::skill_lint;
pub use lifted::skills;

use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
struct Datapoint {
    /// Schema version, so a plot script can refuse a file it cannot read rather
    /// than silently charting a renamed field as zero.
    schema: u32,
    commit: String,
    commit_date: String,
    subject: String,
    measured_at: String,
    /// True when the harness was injected into a worktree of an older revision,
    /// which means the gold sets and metric code are newer than the code under
    /// test. Points with this set are comparable to each other and to HEAD, but
    /// they were not produced by the tree as it stood.
    backfilled: bool,
    skillret: skillret::SkillRet,
    #[serde(skip_serializing_if = "Option::is_none")]
    toolret: Option<toolret::ToolRet>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("retrieval-bench failed: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let json_out = flag(&args, "--json").map(PathBuf::from);

    let root = flag(&args, "--root")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let skills_dir = flag(&args, "--skills")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("skills"));
    let gold_dir = flag(&args, "--gold")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gold"));
    let cache_dir = flag(&args, "--cache")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/thetis/workspace/zero-retrieval-bench/cache"));

    // Held at 4 to match the shipped skills.retrieve_limit, so the benchmark
    // measures the pressure the agent is actually under. Raising it makes every
    // number look better without anything having improved.
    let limit: usize = flag(&args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);

    let mut notes: Vec<String> = Vec::new();

    let mut embedder = if args.iter().any(|a| a == "--lexical") {
        notes.push("dense ranking skipped: --lexical requested".into());
        None
    } else {
        let e = embed::Embedder::new(
            &cache_dir,
            "openai/text-embedding-3-small",
            1536,
        );
        if e.is_none() {
            notes.push(
                "dense ranking unavailable: no OPENROUTER_API_KEY or OPENAI_API_KEY in the \
                 environment; measured the BM25 fallback instead"
                    .into(),
            );
        }
        e
    };

    let skillret = skillret::run(
        &skills_dir,
        &gold_dir.join("skillret.json"),
        limit,
        embedder.as_mut(),
        verbose,
    )?;

    #[cfg(feature = "toolret")]
    let toolret = Some(toolret::run(
        &skills_dir,
        &gold_dir.join("toolret.json"),
        verbose,
    )?);
    #[cfg(not(feature = "toolret"))]
    let toolret = {
        notes.push(
            "ToolRet skipped: no tool-group table could be lifted from this revision".into(),
        );
        None
    };

    let (commit, commit_date, subject) = git_facts(&root);

    let point = Datapoint {
        schema: 1,
        commit,
        commit_date,
        subject,
        measured_at: now(),
        // Checked for a non-empty value, not merely for being set: run.sh
        // always exports it and passes "" for the working tree, so `is_ok()`
        // would mark every live run as backfilled.
        backfilled: std::env::var("RETRIEVAL_BENCH_BACKFILL")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false),
        skillret,
        toolret,
        notes,
    };

    report(&point, verbose);

    if let Some(path) = json_out {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, serde_json::to_string_pretty(&point)?)?;
        eprintln!("\nwrote {}", path.display());
    }

    Ok(())
}

fn report(p: &Datapoint, verbose: bool) {
    let s = &p.skillret;
    println!("retrieval-bench  commit {}  {}", short(&p.commit), p.commit_date);
    if p.backfilled {
        println!("  (backfilled: harness newer than the code under test)");
    }
    println!();
    println!(
        "SkillRet   mode={}  corpus={}  limit={}  cases={}",
        s.mode, s.corpus_size, s.limit, s.cases
    );
    println!("  nDCG@{:<3}      {:.3}", s.limit, s.ndcg_at_k);
    println!("  recall@{:<3}    {:.3}", s.limit, s.recall_at_k);
    println!("  hit@1         {:.3}", s.hit_at_1);
    println!("  MRR           {:.3}", s.mrr);
    if !s.unknown_gold_ids.is_empty() {
        println!(
            "  WARNING {} gold id(s) not in the corpus (cases unwinnable): {}",
            s.unknown_gold_ids.len(),
            s.unknown_gold_ids.join(", ")
        );
    }

    if let Some(t) = &p.toolret {
        println!();
        println!(
            "ToolRet    groups={}  threshold={}  cases={}",
            t.groups, t.threshold, t.cases
        );
        println!("  F1            {:.3}", t.f1);
        println!("  recall        {:.3}", t.recall);
        println!("  specificity   {:.3}", t.specificity);
        println!(
            "  tags only     F1 {:.3}  recall {:.3}   (skill edge adds {:+.3} F1)",
            t.f1_tags_only,
            t.recall_tags_only,
            t.f1 - t.f1_tags_only
        );
        if !t.unknown_ids.is_empty() {
            println!("  WARNING unknown ids: {}", t.unknown_ids.join(", "));
        }
    }

    for note in &p.notes {
        println!("\nnote: {note}");
    }

    if !verbose {
        return;
    }

    if let Some(cases) = &s.per_case {
        println!("\n--- SkillRet, worst first ---");
        let mut sorted: Vec<_> = cases.iter().collect();
        sorted.sort_by(|a, b| a.ndcg.partial_cmp(&b.ndcg).unwrap());
        for c in sorted {
            println!("\n  nDCG {:.3}  hit1 {:.0}  {:?}", c.ndcg, c.hit1, c.query);
            println!("    returned: {}", c.returned.join(", "));
            if !c.missed.is_empty() {
                println!("    MISSED:   {}", c.missed.join(", "));
            }
        }
    }

    if let Some(t) = &p.toolret {
        if let Some(cases) = &t.per_case {
            println!("\n--- ToolRet, worst first ---");
            let mut sorted: Vec<_> = cases.iter().collect();
            sorted.sort_by(|a, b| a.f1.partial_cmp(&b.f1).unwrap());
            for c in sorted {
                if c.f1 >= 1.0 {
                    continue;
                }
                println!("\n  F1 {:.3}  {:?}", c.f1, c.query);
                println!("    attached: {}", c.attached.join(", "));
                if !c.missing.is_empty() {
                    println!("    MISSING:  {}", c.missing.join(", "));
                }
                if !c.spurious.is_empty() {
                    println!("    SPURIOUS: {}", c.spurious.join(", "));
                }
            }
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Commit identity of the tree being measured, taken from git rather than passed
/// in, so a datapoint cannot be mislabelled by a runner bug.
fn git_facts(root: &std::path::Path) -> (String, String, String) {
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    (
        run(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into()),
        run(&["log", "-1", "--format=%cI"]).unwrap_or_else(|| "unknown".into()),
        run(&["log", "-1", "--format=%s"]).unwrap_or_else(|| "unknown".into()),
    )
}

/// UTC timestamp without pulling in a date crate.
fn now() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
