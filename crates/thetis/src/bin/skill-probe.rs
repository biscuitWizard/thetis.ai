//! Exercises the skill pipeline against the real corpus and the real provider.
//!
//! Run with no arguments to lint and inspect the tree without touching the
//! network. Pass queries to embed them and rank:
//!
//! ```text
//! cargo run -p thetis --bin skill-probe
//! cargo run -p thetis --bin skill-probe -- "how do I undo a bad edit"
//! ```

use anyhow::Result;
use thetis::{config::Config, embeddings::Embedder, skill_index, skills, store::Store};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let queries: Vec<String> = std::env::args().skip(1).collect();

    let cfg = Arc::new(Config::load()?);
    let tree = skills::discover(&cfg.paths.skills)?;

    println!("corpus: {} skills from {}", tree.len(), cfg.paths.skills.display());
    for s in tree.all() {
        let kind = if s.universal { "universal" } else { "         " };
        let kids = tree.children(&s.id).len();
        println!(
            "  {kind}  {:<40} depth {}  {} child(ren)  {} resource(s)",
            s.id,
            s.depth,
            kids,
            s.resources.len()
        );
    }

    let diags = skills::lint_all(&tree);
    if diags.is_empty() {
        println!("\nlint: clean");
    } else {
        println!("\nlint: {} finding(s)", diags.len());
        for d in &diags {
            let who = if d.id.is_empty() { "(tree)" } else { &d.id };
            println!("  [{}] {who}: {}", d.severity.label(), d.message);
        }
    }

    println!("\nL0 block as the agent would see it:");
    println!("{}", indent(&skills::l0_block(&tree.universal())));

    if queries.is_empty() {
        println!("(pass queries as arguments to test retrieval)");
        return Ok(());
    }

    let db = Arc::new(Store::open(&std::env::temp_dir().join("skill-probe.redb"))?);
    let embedder = Embedder::new(cfg.clone(), thetis::persist::Persist::Local(db))?;

    println!(
        "embedding: model={} dims={} available={}",
        cfg.skills.embedding_model, cfg.skills.embedding_dimensions, embedder.available()
    );

    let all = tree.all();
    let (vectors, stats) = embedder.vectors_for(&all).await;
    println!(
        "corpus vectors: {} cached, {} fetched, {} missing",
        stats.hits, stats.fetched, stats.missing
    );

    let corpus: Vec<skill_index::Indexed<'_>> = all
        .iter()
        .zip(&vectors)
        .map(|(skill, v)| skill_index::Indexed {
            skill,
            vector: v.as_deref(),
        })
        .collect();

    for query in &queries {
        println!("\n--- {query:?}");
        let qv = match embedder.embed_query(query).await {
            Ok(v) => Some(v),
            Err(e) => {
                println!("  (query embedding failed: {e}; falling back to lexical)");
                None
            }
        };

        // SKILL_PROBE_LIMIT forces a limit below the corpus size, which is
        // the only way to exercise the ranker on a corpus this small.
        let limit = std::env::var("SKILL_PROBE_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(cfg.skills.retrieve_limit);

        let ranked = skill_index::rank(&tree, &corpus, query, qv.as_deref(), limit);

        if ranked.is_empty() {
            println!("  no matches");
        }
        for (i, r) in ranked.iter().enumerate() {
            println!("  {}. {:<40} {:.4}  {}", i + 1, r.id, r.score, r.how.label());
        }

        if std::env::var("SKILL_PROBE_CARDS").is_ok() {
            let cards: Vec<String> = ranked
                .iter()
                .filter_map(|r| tree.get(&r.id))
                .map(|s| skills::l1_card(s, &tree.children(&s.id)))
                .collect();
            println!("\n  L1 block the agent would receive:");
            println!("{}", indent(&skills::l1_block(&cards)));
        }
    }

    Ok(())
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
