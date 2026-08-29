//! Failing tests for issues observed during the lemmalog session.
//!
//! These tests document concrete bugs in the current codebase. Each test
//! name describes the defect; the assertions encode the expected (correct)
//! behavior that the current implementation violates.

use lemmalog::{AgentMemory, MockExtractor};

fn mem(extra: &str) -> AgentMemory<MockExtractor> {
    AgentMemory::new(MockExtractor::new(0.9), extra).unwrap()
}

// =====================================================================
// Issue 1: Dynamically installed rule batches are not persisted.
//
// `AgentMemory::save` writes `extra_rules` (the rules passed to `new`),
// but rule batches installed later via `install_rules` / the MCP
// `lemmalog_install_rules` tool go through `engine.install_program`
// directly and never update `extra_rules`. On reload, the batch and all
// its derivations are gone.
//
// Repro: save a memory with a mid-session rule install, load it, query
// the derived predicate — it returns nothing.
// =====================================================================
#[test]
fn save_persists_dynamically_installed_rule_batches() {
    let dir = std::env::temp_dir().join("lemmalog-test-dynrules");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dyn.snapshot");
    let path = path.to_str().unwrap();

    let mut m = mem("");
    m.observe_at("alice --manager--> bob", 100);
    m.maintain(100);
    assert!(!m.ask("current(\"alice\", \"manager\", O)").unwrap().is_empty());

    // Install a rule batch AFTER construction (mid-session).
    m.install_rules("reports_to(X,Y) :- current(X,\"manager\",Y).")
        .unwrap();
    m.maintain(100);
    assert_eq!(
        m.ask("reports_to(\"alice\", Y)").unwrap(),
        vec!["Y=bob".to_string()],
    );

    m.save(path).unwrap();
    let m2 = AgentMemory::load(MockExtractor::new(0.9), path).unwrap();

    // The batch should survive save/load.
    let batches = m2.rule_batches();
    assert!(
        batches.iter().any(|(_id, src)| src.contains("reports_to")),
        "dynamically installed batch must survive save/load; got {batches:?}"
    );

    // The derivation should be rebuilt on load.
    assert_eq!(
        m2.ask("reports_to(\"alice\", Y)").unwrap(),
        vec!["Y=bob".to_string()],
        "derived relation must be rebuilt after loading a snapshot with a mid-session batch"
    );

    let _ = std::fs::remove_file(path);
}

// Regression: uninstalling a dynamic batch after canonicalization (which
// appends engine-level batches behind it) must drop the batch from the
// persisted set AND its stale derived rows must not leak into the
// snapshot as base facts.
#[test]
fn uninstall_after_engine_batches_does_not_resurrect_or_leak() {
    let dir = std::env::temp_dir().join("lemmalog-test-skew");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("skew.snapshot");
    let path = path.to_str().unwrap();

    let mut m = mem("");
    m.observe_at("alice --manager--> bob", 100);
    m.maintain(100);
    let b1 = m
        .install_rules("reports_to(X,Y) :- current(X,\"manager\",Y).")
        .unwrap();
    m.maintain(100);
    // Canonicalization appends batches directly on the engine, behind the
    // dynamic one — the scenario that broke position-based tracking.
    lemmalog::canonical::assert_alias(&mut m.engine, "al", "alice", 0.9);
    lemmalog::canonical::install_canonicalization(&mut m.engine, &["current"]).unwrap();
    m.maintain(100);

    assert!(m.uninstall_rules(&b1));
    // Save immediately, without a maintain: derived rows are still
    // materialized and must be recognized as derived, not persisted.
    m.save(path).unwrap();
    let snap = std::fs::read_to_string(path).unwrap();
    assert!(
        !snap.contains("reports_to"),
        "uninstalled batch must be gone from the snapshot entirely:\n{snap}"
    );

    let m2 = AgentMemory::load(MockExtractor::new(0.9), path).unwrap();
    assert!(m2.ask("reports_to(\"alice\", Y)").unwrap().is_empty());
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(&dir);
}

// =====================================================================
// Issue 2: Aggregate temp relations (`__agg:*`) leak into snapshots.
//
// `save` skips predicates that appear as clause heads, but aggregate
// temp relations (`__agg:{head_pred}:{clause_index}`) are not clause
// heads — they are lowered at evaluation time. The `ever_derived` set
// tracks them, but `save` does not check it. So `__agg:*` rows are
// written as base facts and re-asserted on load, violating the claim
// that "derived relations are NOT persisted."
//
// Repro: install an aggregate rule, save, read the snapshot file,
// observe `__agg:` lines in the FACT section.
// =====================================================================
#[test]
fn save_does_not_persist_aggregate_temp_relations() {
    let dir = std::env::temp_dir().join("lemmalog-test-aggtemp");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agg.snapshot");
    let path = path.to_str().unwrap();

    let mut m = mem("kit_count(P, count(K)) :- edge(P, \"bought\", K, _, _, _).");
    m.observe_at("alice --bought--> spitfire", 100);
    m.observe_at("alice --bought--> tiger", 100);
    m.maintain(100);
    assert_eq!(
        m.ask("kit_count(\"alice\", N)").unwrap(),
        vec!["N=2".to_string()],
    );

    // Aggregate temp rows must never be written as base facts.
    m.save(path).unwrap();
    let content = std::fs::read_to_string(path).unwrap();
    assert!(
        !content.contains("__agg:"),
        "aggregate temp relations must not be persisted; found __agg: in snapshot:\n{content}"
    );

    // And the aggregate must be rebuilt on load.
    let m2 = AgentMemory::load(MockExtractor::new(0.9), path).unwrap();
    assert_eq!(
        m2.ask("kit_count(\"alice\", N)").unwrap(),
        vec!["N=2".to_string()],
        "aggregate must be recomputed after load, not restored from persisted temp rows"
    );
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(&dir);
}

// =====================================================================
// Issue 3: Automatic save in MCP swallows errors.
//
// `tool_call` in lemmalog-mcp.rs line 437 does `let _ = state.memory.save(p);`
// — if the save fails (disk full, permissions, path invalid), the tool
// reports success to the caller. The user loses state silently.
//
// This test exercises the same pattern at the library level: the MCP
// auto-save path is simulated by calling save and ignoring the error.
// =====================================================================
#[test]
fn mcp_autosave_does_not_swallow_errors() {
    // The MCP server used to auto-save with `let _ = state.memory.save(p);`,
    // silently dropping io::Error. It now surfaces the failure as a WARNING
    // appended to the tool result (src/bin/lemmalog-mcp.rs, tool_call).
    // This test locks the precondition: save() can fail, so the swallow
    // was real, and a good path must succeed.
    let mut m = mem("");
    m.observe_at("alice --works_at--> acme", 100);
    m.maintain(100);

    let bad_path = "/nonexistent/dir/that/cannot/be/written/snap.file";
    let result = m.save(bad_path);
    assert!(
        result.is_err(),
        "save to a nonexistent directory must return an error"
    );

    let dir = std::env::temp_dir().join("lemmalog-test-autosave");
    std::fs::create_dir_all(&dir).unwrap();
    let good = dir.join("snap.file");
    m.save(good.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&good);
    let _ = std::fs::remove_dir(&dir);
}

// =====================================================================
// Issue 4: Multi-valued non-exclusive relations generate spurious
// escalations ("conflicts") on every assertion after the first.
//
// The update policy treats any different-object assertion on a
// non-exclusive predicate as an escalation. This is by design for
// truly exclusive predicates like `works_at`, but for set-valued
// relations like `recommendation`, `evidence`, `includes`, etc., every
// second value produces a conflict report. During the session, dozens
// of escalations were generated for ordinary multi-valued data.
//
// Repro: observe two different values for the same (subject, predicate)
// pair on a non-exclusive predicate and check that the escalation
// list grows.
// =====================================================================
#[test]
fn multi_valued_non_exclusive_relations_should_not_escalate() {
    let mut m = mem("");
    m.observe_at("incident --recommendation--> rec_a", 100);
    let r = m.observe_at("incident --recommendation--> rec_b", 200);

    // These are different recommendations, not conflicting updates.
    // The escalation is spurious — this is a design limitation, not a
    // correct conflict.
    assert_eq!(
        r.escalations.len(),
        0,
        "two different values for a non-exclusive multi-valued predicate should not escalate; got: {}",
        r.escalations.join("; ")
    );
}

// =====================================================================
// Issue 5: `observe` (via `observe_extracted`) does not accept the
// `located(Entity, "ref")` predicate syntax from the SKILL.md schema.
//
// The SKILL.md instructs agents to anchor evidence with
// `located(Entity, "file:line")`. But `observe_extracted` feeds input
// through `parse_protocol_reported`, which only accepts `S --rel--> O`
// line protocol. Predicate-style lines like `located(...)` are dropped
// with the reason "no `--rel-->` structure."
//
// Repro: call observe_extracted with a `located(...)` line.
// =====================================================================
#[test]
fn observe_accepts_located_predicate_from_skill_schema() {
    let mut m = mem("");
    let (report, dropped) = m.observe_extracted(
        "alice --works_at--> acme\nlocated(alice, \"src/main.go:42\")",
        100,
    );
    assert_eq!(
        report.added, 2,
        "both the edge fact and the located() anchor should be asserted; dropped: {dropped:?}"
    );
    m.maintain(100);
    // The located fact should be queryable.
    assert!(
        !m.ask("current(\"alice\", \"located\", O)").unwrap().is_empty()
    );
}

// =====================================================================
// Issue 6: Entity names with decimal points or other punctuation are
// silently dropped, even when quoted in the line protocol.
//
// The line protocol `S --rel[conf]--> O` parses S and O as raw trimmed
// strings, but `entity_token_problem` rejects any string containing
// characters outside `[a-zA-Z0-9_'\' -]`. This means values like
// `"24.7GiB"`, `"86.3"`, `"2026-08-28"` are dropped — the agent cannot
// assert numeric or date-valued facts even with quoting.
//
// Repro: observe a fact with a quoted decimal value.
// =====================================================================
#[test]
fn observe_accepts_quoted_values_with_punctuation() {
    let mut m = mem("");
    let (report, dropped) = m.observe_extracted(
        "incident --peak_rss--> \"24.7GiB\"\nincident --peak_cpu--> \"86.3\"",
        100,
    );
    assert_eq!(
        report.added, 2,
        "quoted values with punctuation should be accepted; dropped: {dropped:?}"
    );
    m.maintain(100);
    assert!(
        !m.ask("current(\"incident\", \"peak_rss\", O)").unwrap().is_empty()
    );
}

// =====================================================================
// Issue 7: MCP `parse_fact_atom` splits on every comma, so entity
// names containing commas cannot be parsed by `lemmalog_why`.
//
// `parse_fact_atom` (lemmalog-mcp.rs line 146) splits arguments on
// every comma with `.split(',')`. If a quoted entity name contains a
// comma (e.g. `"Doe, John"`), it is split into two arguments and the
// parse fails or produces wrong results.
//
// Repro: call parse_fact_atom with a comma-containing entity.
// =====================================================================
#[test]
fn mcp_parse_fact_atom_handles_commas_in_quoted_entities() {
    // parse_fact_atom (src/bin/lemmalog-mcp.rs, cfg `mcp`) delegates to
    // the shared splitter: commas inside quoted entity names must not
    // split arguments, and the surrounding quotes come off.
    let args = lemmalog::agent::split_unquoted("\"Doe, John\", Carol");
    assert_eq!(
        args.len(),
        2,
        "comma in quoted entity should not split the arg; got {args:?}"
    );
    let args: Vec<String> = args.iter().map(|a| a.trim_matches('"').to_string()).collect();
    assert_eq!(args[0], "Doe, John");
    assert_eq!(args[1], "Carol");
}
