#!/usr/bin/env python3
"""Print a `kiwi bench --json` report as Markdown tables (bench/run.sh)."""

import json
import sys


def main():
    with open(sys.argv[1], encoding="utf-8") as f:
        report = json.load(f)
    print(f"{report['atoms']:,} atoms, {report['questions']} answerable questions, snapshot {report['snapshot']}")
    print()
    print("| Mode | recall@5 | recall@10 | MRR | tokens / question | p50 ms | p95 ms | p99 ms | leaks |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for m in report["modes"]:
        print(
            f"| {m['mode']} | {m['recall_at_5']:.3f} | {m['recall_at_10']:.3f} | {m['mrr']:.3f} "
            f"| {m['mean_tokens']:,.0f} | {m['p50_ms']:.3f} | {m['p95_ms']:.3f} | {m['p99_ms']:.3f} | {m['leaks']} |"
        )
    print()
    print(
        f"build: full {report['full_build_ms']:,.0f} ms, one file changed {report['incremental_build_ms']:,.0f} ms "
        f"({report['incremental_atoms']} atoms), no change {report['noop_build_ms']:,.0f} ms; "
        f"pack {report['pack_bytes'] / 1e6:.2f} MB; reload {report['reload_ms']:.2f} ms; "
        f"cold start {report['cold_start_ms']:.2f} ms; "
        f"leak checks {report['leak_checks']}, violations {len(report['leak_violations'])}"
    )
    scale = report.get("scale")
    if scale:
        print()
        print(
            f"scale: {scale['atoms']:,} atoms, pack {scale['pack_bytes'] / 1e6:.1f} MB, "
            f"build {scale['build_ms']:,.0f} ms, one-file rebuild {scale['incremental_build_ms']:,.0f} ms, "
            f"cold start {scale['cold_start_ms']:.2f} ms"
        )
        print()
        print("| Query | p50 ms | p95 ms | p99 ms |")
        print("| --- | ---: | ---: | ---: |")
        for key, label in [
            ("exact_ms", "exact lookup by id"),
            ("structural_ms", "structural filter"),
            ("bm25_retriever_ms", "BM25 retriever"),
            ("bm25_ms", "BM25 search"),
            ("vec_scan_ms", "Hamming scan"),
            ("vec_ms", "vector search (hash)"),
        ]:
            p50, p95, p99 = scale[key]
            print(f"| {label} | {p50:.3f} | {p95:.3f} | {p99:.3f} |")


if __name__ == "__main__":
    main()
