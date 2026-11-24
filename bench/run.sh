#!/usr/bin/env bash
# The Kiwi benchmark suite: search quality, latency percentiles, build and reload
# times, pack sizes, at three scale points. Separate from `cargo test`; a few minutes.
#
#   bench/run.sh [--kiwi <binary>] [--out <dir>] [--models]
#
# Suites (each a `kiwi bench` run, its JSON in <out>/<suite>.json, its table on stdout):
#
#   acme           the example corpus (146 atoms, 48 questions, 7 principals), BM25 and
#                  vectors from the `hash` embedder, plus a synthetic scale point of
#                  100,000 atoms; checked against bench/baselines/acme.toml
#   catalogue      10,000 one-record documents with planted near-duplicates, generated
#                  by bench/catalogue.py (seed 7, 200 questions); checked against
#                  bench/baselines/catalogue.toml
#
# With --models (a kiwi built with `fastembed`; downloads the models once into
# ~/.cache/kiwi) it adds the published figures' model runs, which have no baseline:
#
#   acme-models    vectors from multilingual-e5-small, and the local cross-encoder
#                  jina-reranker-v2-base-multilingual over the top 10, then the top 20
#   catalogue-models  vectors from multilingual-e5-small
#
# Exits non-zero when a suite leaks or falls behind its baseline by more than the
# tolerances in its baseline file.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
kiwi=${KIWI:-$root/target/release/kiwi}
out=$root/target/bench
models=0
while [ $# -gt 0 ]; do
  case $1 in
    --kiwi) kiwi=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --models) models=1; shift ;;
    *) echo "usage: bench/run.sh [--kiwi <binary>] [--out <dir>] [--models]" >&2; exit 2 ;;
  esac
done
[ -x "$kiwi" ] || { echo "no kiwi binary at $kiwi (cargo build --release)" >&2; exit 2; }
mkdir -p "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
status=0

# suite <name> <kiwi bench arguments…>: one run, its JSON kept, its table printed.
suite() {
  local name=$1
  shift
  echo "== $name"
  if ! "$kiwi" bench "$@" --json > "$out/$name.json"; then
    status=1
  fi
  if [ -s "$out/$name.json" ]; then
    python3 "$root/bench/table.py" "$out/$name.json"
  fi
  echo
}

cd "$root"
"$kiwi" info
suite acme fixtures/acme-kb --embedder hash --synthetic 100000 --baseline bench/baselines/acme.toml

python3 bench/catalogue.py "$work/catalogue" --items 10000 --questions 200 --seed 7 > /dev/null
suite catalogue "$work/catalogue" --embedder hash \
  --questions "$work/catalogue/bench/questions.yaml" \
  --principals "$work/catalogue/bench/principals.yaml" \
  --baseline bench/baselines/catalogue.toml

if [ "$models" = 1 ]; then
  suite acme-models fixtures/acme-kb --rerank jina-reranker-v2-base-multilingual
  suite acme-rerank-20 fixtures/acme-kb --rerank jina-reranker-v2-base-multilingual --rerank-top-k 20 --repeat 1
  suite catalogue-models "$work/catalogue" \
    --questions "$work/catalogue/bench/questions.yaml" \
    --principals "$work/catalogue/bench/principals.yaml"
fi
exit $status
