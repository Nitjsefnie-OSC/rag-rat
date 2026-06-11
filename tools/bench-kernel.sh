#!/usr/bin/env bash
# Headline "indexes the Linux kernel in X seconds" benchmark.
#
# Clones a pinned Linux kernel tag, indexes its C/H sources once with the *release* rag-rat binary
# (dogfooding the shipped `index` command), and writes a Bencher Metric Format (BMF) JSON file with
# three measures: latency (ns), throughput (files/s), and peak memory (bytes). The release workflow
# (.github/workflows/bench_release.yml) feeds that file to `bencher run --adapter json`.
#
# Single-shot on purpose: a full kernel index is ~tens of minutes, so criterion's 10-sample loop
# isn't viable — this is one cold rebuild, the number a user actually sees. Runs only on release
# (or manual dispatch), so the long runtime is acceptable.
#
# Env knobs:
#   KERNEL_TAG               kernel tag to index            (default: v7.0)
#   RAG_RAT_BIN              path to the release binary     (default: target/release/rag-rat)
#   RAG_RAT_KERNEL_SUBDIRS   space-separated subtrees to    (default: ".", the whole tree)
#                            index — set e.g. "kernel mm fs net lib" to bound scope/memory
#   BMF_OUT                  output BMF JSON path           (default: kernel_bmf.json)
#   KERNEL_WORK              working dir                    (default: a fresh mktemp dir)
set -euo pipefail

KERNEL_TAG="${KERNEL_TAG:-v7.0}"
RAG_RAT_BIN="${RAG_RAT_BIN:-target/release/rag-rat}"
RAG_RAT_KERNEL_SUBDIRS="${RAG_RAT_KERNEL_SUBDIRS:-.}"
BMF_OUT="${BMF_OUT:-kernel_bmf.json}"
WORK="${KERNEL_WORK:-$(mktemp -d)}"

command -v "$RAG_RAT_BIN" >/dev/null 2>&1 || [ -x "$RAG_RAT_BIN" ] || {
  echo "bench-kernel: rag-rat binary not found at '$RAG_RAT_BIN' (build with: cargo build --release --no-default-features --bin rag-rat)" >&2
  exit 1
}

echo "bench-kernel: cloning Linux kernel ${KERNEL_TAG} (shallow) into ${WORK}/linux" >&2
git clone --quiet --depth 1 --branch "$KERNEL_TAG" https://github.com/torvalds/linux.git "$WORK/linux"
KERNEL_SHA="$(git -C "$WORK/linux" rev-parse HEAD)"
echo "bench-kernel: ${KERNEL_TAG} = ${KERNEL_SHA}" >&2

# Render a C-language config over the requested subtree(s). `c = [...]` maps to **/*.c + **/*.h.
subdirs_toml="$(printf '"%s", ' $RAG_RAT_KERNEL_SUBDIRS)"
cat > "$WORK/rag-rat.toml" <<EOF
[index]
root = "$WORK/linux"
database = "$WORK/kernel-index.sqlite"

[target_bindings]
c = [${subdirs_toml%, }]
EOF

# Single cold full index, measured. Wall clock + peak RSS come from python's resource.getrusage on
# the child process — portable, no /usr/bin/time dependency (Arch/minimal boxes don't ship it). Use
# the release binary built --no-default-features (hash embedder; no model download, no network) so
# the number is the indexing machinery, reproducible across runs.
echo "bench-kernel: indexing (this takes a while)…" >&2
python3 - "$RAG_RAT_BIN" "$WORK/rag-rat.toml" > "$WORK/measure.txt" <<'PY'
import resource, subprocess, sys, time
rag_rat, cfg = sys.argv[1], sys.argv[2]
start = time.monotonic()
subprocess.run([rag_rat, "--config", cfg, "index", "--full"], stdout=subprocess.DEVNULL, check=True)
seconds = time.monotonic() - start
# ru_maxrss is the child's peak resident set size in kilobytes on Linux.
rss_kb = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
print(f"{seconds} {rss_kb}")
PY
read -r seconds rss_kb < "$WORK/measure.txt"

# True indexed-file count (what landed in the DB), for throughput.
files="$(python3 - "$WORK/kernel-index.sqlite" <<'PY'
import sqlite3, sys
print(sqlite3.connect(sys.argv[1]).execute("select count(*) from files").fetchone()[0])
PY
)"

python3 - "$seconds" "$rss_kb" "$files" "$BMF_OUT" "$KERNEL_TAG" "$KERNEL_SHA" <<'PY'
import json, sys
seconds = float(sys.argv[1])
rss_kb = int(sys.argv[2])
files = int(sys.argv[3])
out, tag, sha = sys.argv[4], sys.argv[5], sys.argv[6]

bmf = {
    # Benchmark name carries the tag so re-pinning to a newer kernel starts a distinct series.
    f"linux-kernel-{tag}/full-index": {
        "latency": {"value": seconds * 1e9},      # nanoseconds — shares Bencher's built-in Latency measure
        "throughput": {"value": files / seconds}, # files per second
        "memory": {"value": rss_kb * 1024},       # peak RSS bytes
    }
}
with open(out, "w") as f:
    json.dump(bmf, f, indent=2)

print(
    f"bench-kernel: indexed {files} files of Linux {tag} in {seconds:.1f}s "
    f"({files/seconds:.1f} files/s, peak {rss_kb/1024:.0f} MiB) → {out}",
    file=sys.stderr,
)
PY
