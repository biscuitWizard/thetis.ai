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

mod ablate;
mod corpus;
/// Routing at scale needs the lifted group table, which only exists when the
/// table could be extracted at this revision.
#[cfg(feature = "toolret")]
mod routescale;
mod embed;
mod metrics;
mod skillret;
// Gated with the same feature as `lifted::table`, which it imports. Declaring it
// unconditionally made every pre-routing revision fail to build with
// `unresolved import crate::lifted::table` -- 72 of 104 commits on main, all of
// them reported as "could not be measured" when they are in fact perfectly
// measurable for SkillRet.
#[cfg(feature = "toolret")]
mod toolret;

mod lifted;

// The lifted files refer to their siblings as `crate::skills` and
// `crate::skill_lint`, since in the orchestrator they are crate-level modules.
// These aliases make those paths resolve here without touching the copies.
pub use lifted::skill_lint;
pub use lifted::skills;

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

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
    /// Which corpus produced the SkillRet numbers: "pinned corpus/v1", or
    /// "live" when `--skills` pointed at a real tree. Recorded because the two
    /// are not comparable -- a live corpus differs between checkouts and grows
    /// over time -- so a chart must split the series here rather than draw one
    /// misleading line through both.
    corpus: String,
    skillret: skillret::SkillRet,
    #[cfg(feature = "toolret")]
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

/// The ablation sweep over a large external corpus.
///
/// Reported as one row per arm with a delta against the baseline, plus the
/// vector count each arm needed — quality and cost side by side, because the
/// question being answered is whether a mechanism earns what it costs.
/// Tool-group routing over the dataset-labelled corpus, swept across thresholds.
#[cfg(feature = "toolret")]
fn run_routescale(args: &[String]) -> Result<()> {
    let corpus_path = flag(args, "--corpus")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/toolret.json")
        });
    let json_out = flag(args, "--json").map(PathBuf::from);
    let keep_worst: usize = flag(args, "--worst")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    if !corpus_path.exists() {
        anyhow::bail!(
            "no corpus at {}\n\nBuild it first:\n  ./fetch-toolret.py --out {}",
            corpus_path.display(),
            corpus_path.display()
        );
    }

    // Read the file without the ranking-side transformation: routing needs the
    // raw group membership, not a SkillTree.
    let raw = std::fs::read_to_string(&corpus_path)
        .with_context(|| format!("reading {}", corpus_path.display()))?;
    let file: corpus::ToolRetFile =
        serde_json::from_str(&raw).context("parsing the ToolRet corpus")?;

    // Dense strategies need vectors. Without a key the tag baseline still runs
    // and the strategy comparison is simply absent, rather than the whole
    // measurement failing.
    let lexical_only = args.iter().any(|a| a == "--lexical");
    let cache_dir = flag(args, "--cache")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/thetis/workspace/zero-retrieval-bench/cache"));
    let mut embedder = if lexical_only {
        None
    } else {
        embed::Embedder::new(&cache_dir, "openai/text-embedding-3-small", 1536)
    };
    if embedder.is_none() && !lexical_only {
        eprintln!(
            "note: no embeddings key, so only the tag baseline is measured; set OPENROUTER_API_KEY"
        );
    }

    // `embed_all` persists the cache itself, so there is nothing to flush here.
    let out = routescale::run(&file, keep_worst, embedder.as_mut())?;

    println!("corpus  {}", corpus_path.display());
    println!(
        "routing {} queries over {} groups drawn from {} tools",
        out.queries, out.groups, out.tools
    );
    if out.unlabelled > 0 {
        println!(
            "        {} queries unlabelled (no gold tool fell in any bucket)",
            out.unlabelled
        );
    }
    println!();
    println!("  thresh   recall   spec     F1       groups/q  unrouted");
    for p in &out.sweep {
        let mark = if (p.threshold - out.shipped_threshold).abs() < 1e-9 {
            " <- shipped"
        } else if (p.threshold - out.best_threshold).abs() < 1e-9 {
            " <- best F1"
        } else {
            ""
        };
        println!(
            "  {:<8.2} {:<8.4} {:<8.4} {:<8.4} {:<9.2} {:<8}{}",
            p.threshold, p.recall, p.specificity, p.f1, p.mean_attached, p.unrouted, mark
        );
    }
    println!();
    if (out.best_threshold - out.shipped_threshold).abs() > 1e-9 {
        println!(
            "The shipped threshold {:.2} scores F1 {:.4}; the best swept value {:.2} scores {:.4}.",
            out.shipped_threshold, out.at_shipped.f1, out.best_threshold, out.best_f1
        );
        println!(
            "Treat that as a hint, not a setting: the buckets are synthesised, so the"
        );
        println!("optimum is fitted to my bucketing as much as to the queries.");
    } else {
        println!(
            "The shipped threshold {:.2} is also the best swept value.",
            out.shipped_threshold
        );
    }

    if !out.strategies.is_empty() {
        println!();
        println!("Routing strategies, at the shipped threshold where applicable:");
        println!(
            "  {:<26} {:<8} {:<8} {:<8} {:<9} {:<9} {}",
            "strategy", "recall", "spec", "F1", "groups/q", "unrouted", "cost"
        );
        for s in &out.strategies {
            println!(
                "  {:<26} {:<8.4} {:<8.4} {:<8.4} {:<9.2} {:<9} {}",
                s.strategy, s.recall, s.specificity, s.f1, s.mean_attached, s.unrouted, s.cost
            );
        }
    }

    if let Some(worst) = &out.worst_cases {
        println!();
        println!("Worst cases at the shipped threshold:");
        for c in worst.iter().take(5) {
            println!("  F1 {:.2}  {}", c.f1, truncate(&c.query, 68));
            if !c.missed.is_empty() {
                println!("           missed: {}", c.missed.join(", "));
            }
            if !c.spurious.is_empty() {
                println!("           spurious: {}", c.spurious.join(", "));
            }
        }
    }

    if let Some(path) = json_out {
        let (commit, commit_date, subject) = git_facts(Path::new("."));
        let doc = serde_json::json!({
            "schema": 1,
            "kind": "routescale",
            "commit": commit,
            "commit_date": commit_date,
            "subject": subject,
            "measured_at": now(),
            "corpus": corpus_path.display().to_string(),
            "source": file.source,
            "labels": "derived: dataset per-tool relevance mapped through synthesised buckets",
            "result": out,
        });
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&path, serde_json::to_string_pretty(&doc)?)
            .with_context(|| format!("writing {}", path.display()))?;
        println!();
        println!("wrote {}", path.display());
    }

    Ok(())
}

/// Shorten a query for a fixed-width report without splitting a character.
fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        return s;
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn run_ablation(args: &[String]) -> Result<()> {
    let corpus_path = flag(args, "--corpus")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/toolret.json")
        });
    let cache_dir = flag(args, "--cache")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/thetis/workspace/zero-retrieval-bench/cache"));
    let limit: usize = flag(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let group_tree = args.iter().any(|a| a == "--group-tree");
    let lexical_only = args.iter().any(|a| a == "--lexical");
    let json_out = flag(args, "--json").map(PathBuf::from);
    // A cap for a quick look; the committed default is the whole corpus so the
    // published number is not a subsample somebody has to caveat.
    let max_cases: Option<usize> = flag(args, "--cases").and_then(|v| v.parse().ok());

    // `--skills DIR` runs the sweep over a real Thetis skills tree plus the
    // hand-written SkillRet gold set, instead of the external ToolRet corpus.
    //
    // Both surfaces matter and they answer different questions. ToolRet gives
    // statistical power (thousands of queries) on documents that are not ours;
    // the skills tree is the corpus the ranker actually serves, but it is small.
    // A mechanism worth shipping should win on both, and a claim made about one
    // should not be assumed to hold on the other.
    let skills_dir = flag(args, "--skills").map(PathBuf::from);

    let (file, tree, mut cases) = match &skills_dir {
        Some(dir) => {
            let gold_path = flag(args, "--gold").map(PathBuf::from).unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gold/skillret.json")
            });
            corpus::load_skills(dir, &gold_path)?
        }
        None => {
            if !corpus_path.exists() {
                anyhow::bail!(
                    "no corpus at {}\n\nBuild it first:\n  ./fetch-toolret.py --out {}\n\nOr sweep the real skills tree instead:\n  --skills ../../skills",
                    corpus_path.display(),
                    corpus_path.display()
                );
            }
            corpus::load(&corpus_path, group_tree)?
        }
    };
    if let Some(n) = max_cases {
        cases.truncate(n);
    }

    let skills: Vec<crate::lifted::skills::Skill> = {
        let mut v: Vec<_> = tree.skills.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id)); // determinism: HashMap order is not stable
        v
    };

    println!(
        "corpus  {}",
        match &skills_dir {
            Some(d) => d.display().to_string(),
            None => corpus_path.display().to_string(),
        }
    );
    println!("source  {}", file.source);
    println!(
        "docs    {}  ({} groups, tree={})",
        skills.len(),
        file.groups.len(),
        if group_tree { "nested" } else { "flat" }
    );
    println!("queries {}", cases.len());
    println!("limit   {limit}");

    let bad = corpus::unresolvable(&tree, &cases);
    if !bad.is_empty() {
        println!(
            "WARNING {} gold ids missing from the corpus (cases affected score 0)",
            bad.len()
        );
        for id in bad.iter().take(5) {
            println!("          {id}");
        }
    }

    // The ranker returns the whole corpus unranked when limit >= corpus size,
    // scoring a perfect 1.0 while measuring nothing at all.
    if skills.len() <= limit {
        anyhow::bail!(
            "corpus of {} is not larger than limit {}: rank() short-circuits and \
             returns everything, measuring nothing",
            skills.len(),
            limit
        );
    }

    let mut embedder = if lexical_only {
        None
    } else {
        embed::Embedder::new(&cache_dir, "openai/text-embedding-3-small", 1536)
    };
    if embedder.is_none() && !lexical_only {
        println!(
            "\nnote: no embeddings key (OPENROUTER_API_KEY / OPENAI_API_KEY);\n      \
             running the lexical arms only, so dense and fusion are absent."
        );
    }

    let (doc_vecs, query_vecs, mode) =
        ablate::embed_corpus(embedder.as_mut(), &skills, &cases)?;
    let dense_ok = !doc_vecs.is_empty() && !query_vecs.is_empty();
    println!("mode    {}", mode.label());

    let plan = ablate::Plan::default_sweep(limit, dense_ok);
    println!("\nrunning {} arms...\n", plan.arms.len());

    let results = ablate::run_plan(&plan, &tree, &skills, &doc_vecs, &cases, &query_vecs);

    println!(
        "{:<28} {:>7} {:>7} {:>7} {:>9} {:>18} {:>8}",
        "arm", "nDCG", "hit@1", "MRR", "vs base", "95% CI (p)", "vectors"
    );
    println!("{}", "-".repeat(88));
    for r in &results {
        let (delta, stat) = if r.arm == plan.baseline {
            ("  baseline".to_string(), String::new())
        } else {
            let d = r.vs_baseline.unwrap_or(0.0);
            let s = match (r.ci95, r.p_value) {
                (Some((lo, hi)), Some(p)) => {
                    // A CI straddling zero is the finding, so mark it: it means
                    // "no detectable difference", not "a small difference".
                    let flag = if lo <= 0.0 && hi >= 0.0 { " ns" } else { "" };
                    format!("[{lo:+.3},{hi:+.3}] p={p:.3}{flag}")
                }
                _ => String::new(),
            };
            (format!("{d:+.4}"), s)
        };
        println!(
            "{:<28} {:>7.4} {:>7.4} {:>7.4} {:>9} {:>18} {:>8}",
            r.arm, r.ndcg_at_k, r.hit_at_1, r.mrr, delta, stat, r.vectors_used
        );
    }
    println!(
        "\n  ns = 95% CI includes zero: no difference detected at this sample size."
    );

    if let Some(path) = json_out {
        let payload = serde_json::json!({
            "schema": 1,
            "kind": "ablation",
            // Report the corpus that was actually graded. `--skills` ignores
            // corpus_path entirely, so printing it there labels the run with a
            // file it never opened -- which is exactly the kind of mislabelling
            // that makes a published chart untrustworthy.
            "corpus": match &skills_dir {
                Some(dir) => dir.display().to_string(),
                None => corpus_path.display().to_string(),
            },
            "surface": if skills_dir.is_some() { "skills" } else { "toolret" },
            "source": file.source,
            "caveat": file.note,
            "docs": skills.len(),
            "queries": cases.len(),
            "groups": file.groups.len(),
            "tree": if skills_dir.is_some() {
                "nested (real skill hierarchy)"
            } else if group_tree {
                "nested"
            } else {
                "flat"
            },
            "limit": limit,
            "mode": mode.label(),
            "commit": git_facts(std::path::Path::new(".")).0,
            "measured_at": now(),
            "baseline": plan.baseline,
            "arms": results,
        });
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&path, serde_json::to_string_pretty(&payload)?)?;
        println!("\nwrote {}", path.display());
    }

    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `ablate` is the primary measurement and takes over the process. It answers
    // "what does each mechanism buy", which the per-commit datapoint below
    // cannot: the ranker has exactly one distinct version in the whole history,
    // so a time series over it is flat by construction.
    if args.first().map(|a| a.as_str()) == Some("ablate") {
        return run_ablation(&args[1..]);
    }

    // `routescale` measures tool-group routing over ~1,600 dataset-labelled
    // queries instead of 34 hand-written ones, and sweeps the threshold. Same
    // reason as `ablate`: at n=34 one case is worth 3 points of recall, which is
    // too coarse to see a regression in.
    #[cfg(feature = "toolret")]
    if args.first().map(|a| a.as_str()) == Some("routescale") {
        return run_routescale(&args[1..]);
    }

    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let json_out = flag(&args, "--json").map(PathBuf::from);

    let root = flag(&args, "--root")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    // The pinned corpus is the default, not the live `skills/` tree. Thetis
    // pushes selectively, so checkouts legitimately carry different corpora:
    // the same commit scored corpus=127/hit@1 0.750 here and corpus=61/0.583 in
    // CI. Grading every revision against a committed fixture means a movement
    // in the number has one cause, the code. `--skills <dir>` opts back into a
    // live tree for a one-off look at the real product.
    let pinned = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/v1");
    let (skills_dir, corpus_label) = match flag(&args, "--skills") {
        Some(d) => (PathBuf::from(d), "live".to_string()),
        None if pinned.is_dir() => (pinned, "pinned corpus/v1".to_string()),
        // Before the fixture existed, or if it is deleted: fall back rather
        // than refuse, and say so in the datapoint.
        None => (root.join("skills"), "live (no pinned fixture)".to_string()),
    };
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
    notes.push("ToolRet skipped: no tool-group table could be lifted from this revision".into());

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
        corpus: corpus_label,
        skillret,
        #[cfg(feature = "toolret")]
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
        "SkillRet   mode={}  corpus={} [{}]  limit={}  cases={}",
        s.mode, s.corpus_size, p.corpus, s.limit, s.cases
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

    #[cfg(feature = "toolret")]
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

    #[cfg(feature = "toolret")]
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
