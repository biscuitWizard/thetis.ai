//! Checks the group table and the router against the real source in
//! `agents/agent-core/src/groups.rs`, which `extract.py` copies into
//! `table.rs`. The point is the invariants that would otherwise fail silently:
//! a tool in no group, a group naming a tool that does not exist, and a
//! plausible query routing to the wrong set.

mod table;
use table::*;

fn tok(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// What routing would admit for a query: always-on plus anything over
/// threshold. Mirrors `route_once` minus the skill edges and the KV pin, which
/// need a host.
fn route(query: &str, threshold: f64) -> Vec<String> {
    let q = tok(query);
    let mut active: Vec<String> = all()
        .iter()
        .filter(|g| g.always_on)
        .map(|g| g.id.to_string())
        .collect();
    for g in all() {
        if !active.iter().any(|a| a == g.id) && score(g, &q) >= threshold {
            active.push(g.id.to_string());
        }
    }
    in_table_order(&active)
}

fn main() {
    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "PASS" } else { "FAIL" }, if detail.is_empty() { String::new() } else { format!(" — {detail}") });
        if !ok {
            failures += 1;
        }
    };

    // --- table integrity ---------------------------------------------------

    let ids: Vec<&str> = all().iter().map(|g| g.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    let before = sorted.len();
    sorted.dedup();
    check("group ids are unique", sorted.len() == before, format!("{} groups", ids.len()));

    // A tool in two groups makes `group_of` order-dependent and `builtin_active`
    // a coin flip.
    let mut members: Vec<&str> = all().iter().flat_map(|g| g.members.iter().copied()).collect();
    let n = members.len();
    members.sort();
    members.dedup();
    check("no tool is in two groups", members.len() == n, format!("{n} memberships"));

    // The escape hatch must be in an always-on group or it can be scoped away.
    let search_group = all().iter().find(|g| g.members.contains(&"tool_search")).map(|g| (g.id, g.always_on));
    check(
        "tool_search is in an always-on group",
        matches!(search_group, Some((_, true))),
        format!("{search_group:?}"),
    );

    // Every prefix rule must name a group that exists.
    let bad: Vec<&str> = PREFIX_RULES.iter().filter(|(_, id)| find(id).is_none()).map(|(p, _)| *p).collect();
    check("prefix rules name real groups", bad.is_empty(), format!("{bad:?}"));

    // The fallback group must exist and be always-on: it is what catches an
    // untabled tool.
    let fallback = find(UNGROUPED);
    check(
        "fallback group exists and is always-on",
        fallback.map(|g| g.always_on).unwrap_or(false),
        format!("{UNGROUPED}"),
    );

    // --- coverage against the live tool list -------------------------------

    // The real built-in names, as of this session's tool surface.
    let builtins: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
    let (ungrouped, phantom) = coverage_gaps(&builtins);
    check("every built-in is in a group", ungrouped.is_empty(), format!("{ungrouped:?}"));
    check("no group names a missing tool", phantom.is_empty(), format!("{phantom:?}"));

    // --- component classification ------------------------------------------

    let cases: &[(&str, &[&str], &str)] = &[
        ("bq-query", &[], "bigquery"),
        ("notion-search", &[], "notion"),
        ("web-search", &[], "web"),
        ("git-commit", &[], "github"),
        ("config-probe", &[], "extra"),
        // An explicit declaration beats the naming convention.
        ("bq-query", &["group:web"], "web"),
        // A declaration naming a group that does not exist falls back, loudly.
        ("bq-query", &["group:nonsense"], "extra"),
        ("read-only-thing", &["read-only"], "extra"),
    ];
    for (name, caps, want) in cases {
        let caps: Vec<String> = caps.iter().map(|s| s.to_string()).collect();
        let got = component_group(name, &caps);
        check(
            &format!("component_group({name}, {caps:?})"),
            got == *want,
            format!("want {want}, got {got}"),
        );
    }

    // --- scoring ------------------------------------------------------------

    let files = find("files").unwrap();
    // m/(m+1) on distinct matching tags: one match is 0.5, two 0.667, three
    // 0.75. So a single tag hit clears the 0.15 threshold comfortably — the
    // threshold's job is to reject zero, not to demand several.
    check(
        "one distinct tag match scores 0.50",
        (score(files, &tok("open the directory")) - 0.5).abs() < 1e-9,
        format!("{:.4}", score(files, &tok("open the directory"))),
    );
    check(
        "score rises with more matches, sub-linearly, never reaching 1",
        {
            let one = score(files, &tok("open the directory"));
            let two = score(files, &tok("read the directory"));
            let three = score(files, &tok("read and edit the directory"));
            one < two && two < three && three < 1.0
        },
        format!(
            "{:.4} < {:.4} < {:.4} < 1",
            score(files, &tok("open the directory")),
            score(files, &tok("read the directory")),
            score(files, &tok("read and edit the directory"))
        ),
    );
    check(
        "a repeated tag does not inflate the score",
        (score(files, &tok("read read read read")) - score(files, &tok("read"))).abs() < 1e-9,
        format!("{:.4}", score(files, &tok("read read read read"))),
    );
    check(
        "zero matches scores zero",
        score(files, &tok("what is the weather in oslo")) == 0.0,
        String::new(),
    );
    check(
        "a tagless group never routes on tags",
        score(find("core").unwrap(), &tok("core memory remember recall")) == 0.0,
        String::new(),
    );

    // --- routing ------------------------------------------------------------

    let t = 0.15;
    let scenarios: &[(&str, &[&str], &[&str])] = &[
        // (query, must include, must exclude)
        (
            "Refactor the parser in src/lib.rs and run the tests",
            &["files", "shell"],
            &["notion", "bigquery", "ssh"],
        ),
        (
            "How many rows are in the events table? Query BigQuery.",
            &["bigquery"],
            &["notion", "ssh", "selfmod"],
        ),
        (
            "Update the status of the launch page in Notion",
            &["notion"],
            &["bigquery", "ssh"],
        ),
        (
            "Add a new tool to your own loop and rebuild it",
            &["selfmod"],
            &["notion", "bigquery"],
        ),
        (
            "Is there arxiv research on tool retrieval?",
            &["web"],
            &["notion", "ssh"],
        ),
        (
            "Merge trunk into this branch and resolve the conflicts",
            &["branch"],
            &["notion", "bigquery"],
        ),
        (
            "Open a shell on the build-box host over ssh",
            &["ssh", "shell"],
            &["notion", "bigquery"],
        ),
        // The degenerate case: nothing recognisable. Must still be usable.
        ("hi", &["core", "skills", "files", "extra"], &["notion", "bigquery"]),
    ];

    for (query, must, mustnt) in scenarios {
        let active = route(query, t);
        let missing: Vec<&str> = must.iter().copied().filter(|m| !active.iter().any(|a| a == m)).collect();
        let leaked: Vec<&str> = mustnt.iter().copied().filter(|m| active.iter().any(|a| a == m)).collect();
        check(
            &format!("route({:?})", &query[..query.len().min(44)]),
            missing.is_empty() && leaked.is_empty(),
            format!("active={active:?} missing={missing:?} leaked={leaked:?}"),
        );
    }

    // Always-on groups are in every route, whatever the query.
    let always: Vec<&str> = all().iter().filter(|g| g.always_on).map(|g| g.id).collect();
    let mut ok = true;
    for (query, _, _) in scenarios {
        let active = route(query, t);
        if !always.iter().all(|a| active.iter().any(|x| x == a)) {
            ok = false;
        }
    }
    check("always-on groups survive every route", ok, format!("{always:?}"));

    // Table order is stable regardless of the order evidence arrived in.
    let a = in_table_order(&["web".into(), "core".into(), "shell".into()]);
    let b = in_table_order(&["shell".into(), "web".into(), "core".into()]);
    check("in_table_order is order-independent", a == b, format!("{a:?}"));

    // --- how much is actually withheld -------------------------------------

    println!("\n-- surface size by scenario (built-ins only) --");
    let total = BUILTINS.len();
    for (query, _, _) in scenarios {
        let active = route(query, t);
        let n = BUILTINS.iter().filter(|b| builtin_active(b, &active)).count();
        println!(
            "  {:3}/{} built-ins  groups={:2}  {:?}",
            n,
            total,
            active.len(),
            &query[..query.len().min(46)]
        );
    }

    println!(
        "\n{}",
        if failures == 0 {
            "all checks passed".to_string()
        } else {
            format!("{failures} FAILURES")
        }
    );
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

/// The built-in names the agent offers, taken from `tools.rs`.
const BUILTINS: &[&str] = &[
    "remember", "recall", "ask_user", "tool_search",
    "skill_fetch", "skill_search", "skill_write", "skill_delete", "skill_lint",
    // Offered only when a sandbox is configured, but always classifiable —
    // `all_builtins` adds them back when it is not.
    "exec", "write_file", "read_file",
    "read_path", "edit_path", "search_files", "find_files", "write_path", "list_path", "delete_path",
    "terminal_open", "terminal_run", "terminal_read", "terminal_send", "terminal_signal",
    "terminal_close", "terminal_list", "git_clone",
    "ssh_host_list", "ssh_host_get", "ssh_host_set", "ssh_host_remove", "ssh_host_rename",
    "new_tool", "write_code", "patch_code", "add_dependency", "remove_dependency",
    "list_dependencies", "read_code", "list_code",
    "branch_status", "branch_log", "update_from_trunk", "reset_branch", "complete_merge", "abort_merge",
    "list_config", "read_config", "set_config",
    "restart_orchestrator",
];
