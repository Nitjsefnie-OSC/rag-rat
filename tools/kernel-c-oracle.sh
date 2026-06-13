#!/usr/bin/env bash
# Present C edge resolution on the Linux kernel via the scip-clang oracle (#71), repeatably.
#
# Companion to tools/bench-kernel.sh: same pinned kernel + C-target conventions, but it additionally
# BUILDS the kernel to produce a compile_commands.json (scip-clang's input), runs
# `oracle run --tool scip-clang`, and emits the heuristic-vs-compiler resolution delta as a BMF for
# Bencher (benchmark `linux-kernel-<tag>/c-oracle`).
#
# COVERAGE CAVEAT (load-bearing): scip-clang only resolves translation units present in
# compile_commands.json, which is exactly the set the chosen KERNEL_CONFIG compiles — `defconfig`
# is a few thousand TUs, `allmodconfig` is most of the tree. Every resolution metric below is over
# that COMPILED SUBSET, not the whole-kernel 62k/67.4% headline that bench-kernel.sh reports.
#
# Env:
#   KERNEL_TAG / KERNEL_SHA   pinned kernel (default v7.0 / 028ef9c9…, matches bench-kernel.sh)
#   KERNEL_CONFIG             make config target for the compdb (default: defconfig)
#   RAG_RAT_BIN               release binary (default: target/release/rag-rat)
#   KERNEL_WORK               working dir (default: a fresh mktemp dir)
#   BMF_OUT                   Bencher Metric Format output path (default: kernel_c_oracle_bmf.json)
set -euo pipefail

KERNEL_TAG="${KERNEL_TAG:-v7.0}"
KERNEL_SHA="${KERNEL_SHA:-028ef9c96e96197026887c0f092424679298aae8}"
KERNEL_CONFIG="${KERNEL_CONFIG:-defconfig}"
RAG_RAT_BIN="${RAG_RAT_BIN:-target/release/rag-rat}"
WORK="${KERNEL_WORK:-$(mktemp -d)}"
BMF_OUT="${BMF_OUT:-kernel_c_oracle_bmf.json}"
# Resolve to an absolute path before any cd, and before WORK might be removed.
RAG_RAT_BIN="$(command -v "$RAG_RAT_BIN" || readlink -f "$RAG_RAT_BIN")"
BMF_OUT="$(readlink -f "$BMF_OUT" 2>/dev/null || echo "$PWD/$BMF_OUT")"
mkdir -p "$WORK"
DB="$WORK/kernel-index.sqlite"
KDIR="$WORK/linux"

[ -x "$RAG_RAT_BIN" ] || { echo "kernel-c-oracle: rag-rat not found at '$RAG_RAT_BIN'" >&2; exit 1; }
command -v scip-clang >/dev/null 2>&1 || {
  echo "kernel-c-oracle: scip-clang not on PATH (install from github.com/sourcegraph/scip-clang)" >&2
  exit 1
}

echo "kernel-c-oracle: fetching Linux ${KERNEL_TAG} (${KERNEL_SHA}, shallow)" >&2
git init -q "$KDIR"
git -C "$KDIR" remote add origin https://github.com/torvalds/linux.git
git -C "$KDIR" -c protocol.version=2 fetch -q --depth 1 origin "$KERNEL_SHA"
git -C "$KDIR" checkout -q "$KERNEL_SHA"

# Build the kernel so its compile_commands.json target can read the per-object .cmd files. Quiet
# build; failures in a few TUs don't abort (|| true) — a partial compdb still demonstrates the join.
echo "kernel-c-oracle: building ${KERNEL_CONFIG} + compile_commands.json" >&2
make -C "$KDIR" -s "$KERNEL_CONFIG"
make -C "$KDIR" -s -j"$(nproc)" 2>/dev/null || true
make -C "$KDIR" -s compile_commands.json
TUS="$(python3 -c "import json,sys; print(len(json.load(open('$KDIR/compile_commands.json'))))")"
echo "kernel-c-oracle: compile_commands.json covers $TUS translation units" >&2

cat > "$KDIR/rag-rat.toml" <<EOF
[index]
root = "$KDIR"
database = "$DB"

[target_bindings]
c = ["."]
EOF

echo "kernel-c-oracle: rag-rat index --full" >&2
( cd "$KDIR" && "$RAG_RAT_BIN" index --full >/dev/null )

# The scip-clang oracle pass over the compiled subset (stdout = clean JSON report).
echo "kernel-c-oracle: oracle run --tool scip-clang" >&2
( cd "$KDIR" && "$RAG_RAT_BIN" oracle run --tool scip-clang ) > "$WORK/oracle-report.json"

python3 - "$DB" "$WORK/oracle-report.json" "$TUS" "$BMF_OUT" "$KERNEL_TAG" <<'PY'
import json, sqlite3, sys
db, report_path, tus, bmf_out, tag = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5]
report = json.load(open(report_path)).get("report", {})
conn = sqlite3.connect(db)
q = lambda s: conn.execute(s).fetchone()[0]

# Whole-index heuristic baseline (rag-rat indexes the full tree; the oracle only covers the
# compiled subset, so these two populations differ — reported side by side, honestly).
total_calls = q("SELECT COUNT(*) FROM edges WHERE edge_kind='calls_name' AND callee_start_byte IS NOT NULL")
heur_resolved = q("SELECT COUNT(*) FROM edges WHERE edge_kind='calls_name' AND callee_start_byte IS NOT NULL AND to_symbol_id IS NOT NULL")
heur_rate = 100.0 * heur_resolved / total_calls if total_calls else 0.0

confirmed = report.get("confirmed", 0)
contradicted = report.get("contradicted", 0)
upgraded = report.get("upgraded", 0)
resolved_external = report.get("resolved_external", 0)
covered = report.get("covered_calls", 0)
oracle_only = report.get("oracle_only_calls", 0)
judged = confirmed + contradicted
precision = 100.0 * confirmed / judged if judged else 0.0   # compiler-confirmed fraction of resolved
recall = 100.0 * covered / (covered + oracle_only) if (covered + oracle_only) else 0.0

print(f"\n=== C edge resolution on Linux {tag} (compiled subset: {tus} TUs) ===")
print(f"whole-index heuristic calls_name resolved: {heur_resolved}/{total_calls} ({heur_rate:.1f}%)")
print(f"oracle (compiled subset): confirmed={confirmed} contradicted={contradicted} "
      f"upgraded={upgraded} resolved_external={resolved_external}")
print(f"compiler-confirmed precision of heuristic-resolved edges: {precision:.1f}%  "
      f"(confirm/(confirm+contradict))")
print(f"call recall (oracle-seen calls a calls_name edge covered): {recall:.1f}%")

bmf = {f"linux-kernel-{tag}/c-oracle": {
    "compiled_tus": {"value": tus},
    "heuristic_resolved_rate": {"value": heur_rate},
    "compiler_precision": {"value": precision},
    "call_recall": {"value": recall},
    "confirmed": {"value": confirmed},
    "contradicted": {"value": contradicted},
    "upgraded": {"value": upgraded},
    "resolved_external": {"value": resolved_external},
}}
json.dump(bmf, open(bmf_out, "w"), indent=2)
print(f"wrote BMF -> {bmf_out}")
PY

# Free the multi-GB kernel checkout + index DB (they accumulate per run on the self-hosted box);
# keep the small oracle-report.json in WORK for artifact upload and the BMF at $BMF_OUT.
rm -rf "$KDIR" "$DB" "$DB"-wal "$DB"-shm
echo "kernel-c-oracle: done (report: $WORK/oracle-report.json, BMF: $BMF_OUT)" >&2
