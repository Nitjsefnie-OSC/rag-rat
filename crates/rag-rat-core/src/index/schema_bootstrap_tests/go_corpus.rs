//! The Go corpus (T8): deterministic end-to-end expectations over a REAL, buildable Go module.
//!
//! The fixture (`tests/fixtures/go-corpus`) is a two-package module — `cmd/app` imports
//! `corekit` — so cross-PACKAGE, cross-FILE calls genuinely exist. That is what makes this an
//! integration test rather than another extraction test: T6 already pins what `go_edges` emits
//! from a parse tree, and the language-registry tests already pin the shape of Go's
//! `ResolutionPolicy`. Neither of those runs the resolver. This file drives the whole pipeline —
//! discover → parse → extract → RESOLVE → SQLite — and asserts on the rows that come out the far
//! end, so a Go call edge that extracts perfectly but binds to nothing (or to the WRONG symbol)
//! fails here and nowhere else.
//!
//! Like the Swift corpus, this pins two different things:
//!
//! 1. **What the tree-sitter baseline gets right**: symbols and their kinds, the import edge, and
//!    the cross-file call that binds to a uniquely-named target.
//! 2. **What it honestly CANNOT know** — the method call it must leave unresolved rather than
//!    guess. A test that only asserted (1) would let a resolver that CONFIDENTLY GUESSES look like
//!    an improvement.

use super::*;

/// Index the corpus. The Go module puts the callee package under `corekit/` and the caller under
/// `cmd/`; both are bound so the cross-package call is inside the indexed set.
fn corpus() -> (ScratchRoot, IndexDatabase) {
    let root = fixture_temp_root("go-corpus");
    let config = source_config_dirs(root.clone(), Language::Go, &["cmd", "corekit"]);
    let db = IndexDatabase::rebuild(&config).unwrap();
    (root, db)
}

/// Every `(kind, scope_path)` for a symbol name. Go contributes no ancestor-derived scope
/// segments (`scope_segment` returns `None` for every node), so a Go symbol's `scope_path` is its
/// own name — receiver-qualified for methods.
fn symbols_named(db: &IndexDatabase, name: &str) -> Vec<(String, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare("SELECT kind, scope_path FROM symbols WHERE name = ?1 ORDER BY scope_path")
        .unwrap();
    let rows = stmt
        .query_map([name], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// The `(confidence, resolution)` of every `calls_name` edge with this callee, from the function
/// named `from`. Edge `from_name` is the QUALIFIED name (`path/to/file.go::Fn`), so the caller is
/// matched on its trailing segment.
fn call_states(db: &IndexDatabase, from: &str, to: &str) -> Vec<(String, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT edges.confidence, edges.resolution
             FROM edges
             WHERE edges.edge_kind = 'calls_name'
               AND edges.to_name = ?2
               AND COALESCE(edges.from_name, '') LIKE '%::' || ?1",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![from, to], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// The file paths that a resolved `calls_name` edge for `to` actually BOUND to, via the edge's
/// `to_symbol_id`. This is the assertion that separates "an edge exists" from "the edge points at
/// the right declaration" — the whole point of resolving.
fn resolved_call_targets(db: &IndexDatabase, to: &str) -> Vec<(String, String)> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT files.path, symbols.name
             FROM edges
             JOIN symbols ON symbols.id = edges.to_symbol_id
             JOIN files ON files.id = symbols.file_id
             WHERE edges.edge_kind = 'calls_name' AND edges.to_name = ?1
             ORDER BY files.path",
        )
        .unwrap();
    let rows = stmt
        .query_map([to], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// Every `imports` target recorded for the file whose path ends with `path_suffix`.
fn imports_from(db: &IndexDatabase, path_suffix: &str) -> Vec<String> {
    let conn = db.storage.connection();
    let mut stmt = conn
        .prepare(
            "SELECT to_name FROM edges
             WHERE edge_kind = 'imports' AND COALESCE(from_name, '') LIKE '%' || ?1
             ORDER BY to_name",
        )
        .unwrap();
    let rows = stmt.query_map([path_suffix], |row| row.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

/// Go's declaration kinds reach the index, with methods carrying their receiver type.
#[test]
fn go_corpus_extracts_declarations_and_receiver_qualified_methods() {
    let (root, db) = corpus();

    // Plain functions are indexed under their bare name.
    assert_eq!(symbols_named(&db, "Compute"), vec![("function".into(), "Compute".into())]);
    assert_eq!(symbols_named(&db, "Run"), vec![("function".into(), "Run".into())]);

    // A `type X struct` is a `struct`, distinct from the `type` catch-all.
    assert_eq!(symbols_named(&db, "Counter"), vec![("struct".into(), "Counter".into())]);

    // The POINTER-receiver method carries its receiver type in the symbol name, which is what
    // keeps two same-named methods on different types apart. Go contributes no ancestor scope, so
    // `scope_path` equals that same receiver-qualified name.
    assert_eq!(symbols_named(&db, "Counter.Increment"), vec![(
        "method".into(),
        "Counter.Increment".into()
    )]);

    // The VALUE-receiver method is qualified identically — the receiver unwrap is indifferent to
    // the pointer.
    assert_eq!(symbols_named(&db, "Counter.Total"), vec![(
        "method".into(),
        "Counter.Total".into()
    )]);

    // The bare method name is NOT indexed on its own; only the qualified form exists.
    assert!(symbols_named(&db, "Increment").is_empty(), "method is indexed receiver-qualified");

    let _ = fs::remove_dir_all(&root);
}

/// Go import edges reach the graph, for both the first-party package path and the stdlib one.
#[test]
fn go_corpus_records_import_edges_for_first_party_and_stdlib_packages() {
    let (root, db) = corpus();

    // The caller's grouped `import ( … )` yields one edge per spec, with the path UNQUOTED.
    assert_eq!(imports_from(&db, "cmd/app/main.go"), vec![
        "example.com/gocorpus/corekit".to_string(),
        "fmt".to_string(),
    ]);

    // The single-spec `import "fmt"` form produces the same shape of edge.
    assert_eq!(imports_from(&db, "corekit/compute.go"), vec!["fmt".to_string()]);

    let _ = fs::remove_dir_all(&root);
}

/// THE test this task exists for: a Go call that crosses a file AND package boundary resolves,
/// through the real pipeline, to the correct declaration in the other file.
#[test]
fn go_corpus_resolves_a_cross_package_call_to_the_declaration_in_the_other_file() {
    let (root, db) = corpus();

    // `Run` calls `corekit.Compute(seed)`. The extractor keys the edge on the selector's trailing
    // field (`Compute`) and keeps `corekit` only as a receiver hint, so binding happens by bare
    // name — and `Compute` is unique corpus-wide, so there is exactly one right answer.
    assert_eq!(call_states(&db, "Run", "Compute"), vec![(
        "Syntactic".into(),
        "target_name_fallback".into()
    )]);

    // The load-bearing half: the edge points at the declaration in the OTHER file, not at
    // something in the caller's own file. Without this, an edge that resolved to the wrong
    // symbol would still pass the assertion above.
    assert_eq!(resolved_call_targets(&db, "Compute"), vec![(
        "corekit/compute.go".to_string(),
        "Compute".to_string()
    )]);

    // A second, independent cross-package call resolves the same way, so a single lucky bind
    // cannot carry the result.
    assert_eq!(call_states(&db, "Summarize", "Describe"), vec![(
        "Syntactic".into(),
        "target_name_fallback".into()
    )]);

    let _ = fs::remove_dir_all(&root);
}

/// What the baseline must REFUSE to resolve. A method call carries only the bare field name at
/// the call site while the declaration is stored receiver-qualified, and tree-sitter has no types
/// with which to recover the receiver — so the honest answer is "unresolved", not a guess.
#[test]
fn go_corpus_leaves_a_method_call_unresolved_rather_than_guessing() {
    let (root, db) = corpus();

    // `counter.Increment(3)` — the only `Increment` declaration is named `Counter.Increment`, so
    // the bare-name lookup finds nothing and the edge stays unresolved rather than binding to
    // some same-named function elsewhere.
    let increments = call_states(&db, "UseCounter", "Increment");
    assert_eq!(increments.len(), 1, "the method call site is recorded: {increments:?}");
    assert!(
        increments
            .iter()
            .all(|(confidence, resolution)| confidence == "NameOnly" && resolution == "unresolved"),
        "a receiver-typed method call cannot be resolved by name: {increments:?}"
    );

    // Recorded-but-unresolved means exactly that: no `to_symbol_id` was invented for it.
    assert!(
        resolved_call_targets(&db, "Increment").is_empty(),
        "an unresolved method call must not be bound to any symbol"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The fixture really is a valid Go module, not just text that happens to parse.
///
/// tree-sitter is error-tolerant: it produces a tree for input the Go compiler would reject, so a
/// corpus that silently drifted into invalid Go would keep passing every assertion above while no
/// longer representing real code. Gated behind `RAG_RAT_GO_BUILD=1` because CI images do not all
/// carry a Go toolchain — but when the flag is set, a MISSING toolchain is a failure rather than
/// a silent skip, so the check can never be "passing" without having actually run.
#[test]
fn go_corpus_builds_with_the_go_toolchain() {
    if std::env::var("RAG_RAT_GO_BUILD").as_deref() != Ok("1") {
        return;
    }
    let go = Command::new("go").arg("version").output();
    let version = match go {
        Ok(output) if output.status.success() =>
            String::from_utf8_lossy(&output.stdout).to_string(),
        _ => panic!(
            "RAG_RAT_GO_BUILD=1 but no working `go` on PATH — refusing to report a Go build as \
             passing without a toolchain. Install one (go.dev/dl) or unset the flag."
        ),
    };
    eprintln!("go toolchain provenance: {}", version.lines().next().unwrap_or("unknown"));

    let root = fixture_temp_root("go-corpus");
    // `go vet` rather than plain `go build`: it compiles every package AND rejects the suspicious
    // constructs that still compile, so a corpus edit that quietly breaks the fixture's meaning
    // is caught as well as one that breaks its syntax.
    let build =
        Command::new("go").args(["vet", "./..."]).current_dir(&root).output().expect("run go vet");
    assert!(
        build.status.success(),
        "the corpus must build with the Go toolchain:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );

    let _ = fs::remove_dir_all(&root);
}
