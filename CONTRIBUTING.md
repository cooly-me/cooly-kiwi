# Contributing to Kiwi

Thank you for helping. The principles in [docs/design.md](docs/design.md) (Markdown in
Git is the source of truth, compile instead of serve, tokens are the unit, governance
before retrieval, a microkernel with plugins, every answer pinned to a snapshot) decide
design arguments. For a larger change, open an issue first to agree on the approach.

## Development

Requirements: Rust 1.95 or newer (wasmtime 48 needs it), Git, and for the
example WebAssembly plugin the `wasm32-unknown-unknown` target.

```sh
cargo build                          # every plugin and backend compiled in
cargo build --no-default-features    # navigation only (map, read, exact lookups)
cargo test --all-features            # unit, integration, leak, federation and MCP tests
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --no-default-features -- -D warnings
cargo fmt --all --check
```

The `kiwi` binary's features: `bm25`, `vec` (with the `hash` embedder), `openai`
(embeddings from an OpenAI-compatible API), `fastembed` (local ONNX models for
embeddings and reranking; it links ONNX Runtime), `rerank` (cross-encoder reranking
over HTTP), `wasm`, `http`. The Linux release leaves out `fastembed`, whose prebuilt
ONNX Runtime needs glibc 2.38; check that set too:

```sh
cargo clippy --all-targets --no-default-features --features bm25,vec,openai,rerank,wasm,http -- -D warnings
cargo test --no-default-features --features bm25,vec,openai,rerank,wasm,http
```

The tests build the fixture corpus with the `hash` embedder and run the network
plugins (embeddings, rerank, audit sinks) against local mock servers, so they need no
model download and no network. The benchmark suite (`bench/run.sh`, described in
[docs/design.md](docs/design.md#results)) is not part of `cargo test`; CI runs it on
every release and nightly. `kiwi bench` with the default configuration downloads the
`multilingual-e5-small` ONNX model once into `~/.cache/kiwi` (override with
`KIWI_CACHE`).

Rebuild the example plugin after changing `crates/kiwi-wasm/wit/plugin.wit`:

```sh
cd plugins/example-lint
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/kiwi_example_lint.wasm example_lint.wasm
```

## Pull requests

- One topic per pull request; conventional commit messages (`feat(core): ...`,
  `fix(mcp): ...`, `docs: ...`).
- New behaviour comes with tests next to the code and, for anything a caller can
  observe, an integration test in `crates/kiwi-cli/tests`.
- Anything that touches retrieval, ranking or tool output must keep
  `crates/kiwi-cli/tests/leak.rs` green: hidden content may not change a single byte
  of what a principal receives.
- Tool output must stay byte-stable for a given snapshot and request (sorted lists,
  fixed field order, rounded scores).
- Public items carry rustdoc; library code does not `unwrap()` on input.
- Run the benchmark suite before and after changes to ranking, retrieval, builds or
  serving, and report both outputs: `cargo build --release && bench/run.sh` (a few
  minutes; `--models` adds the embedding-model and rerank runs). It fails when a result
  falls behind `bench/baselines/` by more than the tolerances written there; update a
  baseline in the same pull request as a change that moves it on purpose.

## Reporting bugs

Open an issue with the commit, the command and its output. Report security issues
(including any permission leak) privately as described in [SECURITY.md](SECURITY.md).

By contributing you agree that your contributions are licensed under the Apache
License 2.0.
