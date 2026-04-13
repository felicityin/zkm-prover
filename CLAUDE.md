# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`zkm-prover` is a parallel proving service for [ZKM](https://github.com/ProjectZKM/zkm). It is a Cargo workspace consisting of a stage server and prover-node binaries that communicate over gRPC. A user submits proof generation requests to the stage, which fans tasks out to prover nodes.

Toolchain is pinned in `rust-toolchain.toml` (currently `nightly-2025-04-10`). `protoc` is required at build time (the `Dockerfile` installs `protoc 26.0`).

## Workspace layout

- `proof-service/` — the binary crate (`proof-service`). Contains both the stage server (`--stage`) and the prover node (default), plus the gRPC service implementations, MySQL/SQLx layer, Prometheus metrics, and config loading.
- `prover/` — v1 prover library wrapping `zkm-prover`/`zkm-emulator`/`zkm-recursion` (plonky2-based). Exposes `pipeline::Pipeline` plus per-stage provers (`root`, `agg`, `snark`) and `executor::SplitContext`. Has a `gpu` feature.
- `prover_v2/` — v2 prover library wrapping the [Ziren](https://github.com/ProjectZKM/Ziren) toolchain and Plonky3. Provides its own `Pipeline`, contexts, and a `single_node_prover`. Has `gpu` (uses `ziren-gpu`) and `debug` features. Holds global LRU caches for proving keys and programs (`KEY_CACHE`, `PROGRAM_CACHE`) and a `OnceLock`-backed `ZKMProver`.
- `common/` — shared helpers: filesystem abstraction with S3 support (`file`), and TLS config (`tls`).
- `proto/` — protobuf definitions under `src/proto/{include,prover,stage}/v1/`. Compiled by `proof-service/build.rs` via `tonic-build`.
- `proof-service/migrations/` — SQLx migrations for the MySQL schema (stage tasks, prove tasks, users).
- `proof-service/examples/stage.rs` — end-to-end example client; see `proof-service/examples/README.md` for run recipes (hello-world, minigeth, revme).
- `proof-service/config/` — sample TOMLs and `gen_config.sh` for generating multi-prover configurations.

`proof-service` depends on `prover` and/or `prover_v2` only via Cargo features — see "Features" below. Without one of those features the binary builds but cannot actually prove.

## Common commands

```bash
# Build everything (no prover backend; service compiles but cannot prove)
cargo build --release

# Build with a specific prover backend
cargo build --release --features=prover            # v1 (plonky2)
cargo build --release --features=prover_v2         # v2 (Ziren)
cargo build --release --features=gpu               # v1 + GPU
cargo build --release --features=prover_v2_gpu     # v2 + GPU

# Lint / format / test (mirrors CI in .github/workflows/ci.yml)
make clippy                                        # cargo check + fmt --check + clippy -D warnings
cargo fmt --all -- --check
cargo clippy --features=prover     --all-targets -- -D warnings
cargo clippy --features=prover_v2  --all-targets -- -D warnings
cargo test --release --features=prover
cargo test --release --features=prover_v2

# Run a single test
cargo test --release --features=prover_v2 -p proof-service <test_name> -- --nocapture

# Run the example client against a running stage
RUST_LOG=info ELF_PATH=... OUTPUT_DIR=... ENDPOINT=http://127.0.0.1:50000 \
  cargo run --release --example stage
```

The same binary serves both roles; pass `--stage` to run as the stage server, omit it to run as a prover node:

```bash
RUST_LOG=info ./target/release/proof-service --config ./proof-service/config/stage.toml --stage
RUST_LOG=info ./target/release/proof-service --config ./proof-service/config/config.toml
```

A Prometheus scrape endpoint is exposed on `metrics_addr`. `RUST_LOGGER=forest|flat` switches between tracing-forest and the flat subscriber (default `flat`).

## Cargo features (important)

`proof-service` has mutually-exclusive prover backends gated by features:

- `prover` → links the v1 `prover` crate (plonky2). Provides `AggContext`, `ProveContext`, `SnarkContext`, `SplitContext`, `Pipeline`.
- `prover_v2` → links the v2 `prover_v2` crate (Ziren). Provides the same names plus `SingleNodeContext`.
- `gpu` → enables GPU on the v1 backend (also pulls in `plonky2` directly so `proof-service` can call `plonky2::create_ctx` / `init_globalmem` / `destroy_ctx` at startup/shutdown in `bin/proof-service.rs`).
- `prover_v2_gpu` → enables GPU on the v2 backend.

Many modules in `proof-service/src` are wrapped in `#[cfg(feature = "prover")]` / `#[cfg(feature = "prover_v2")]`. When editing code paths that use `AggContext`, `ProveContext`, `SplitContext`, etc., be aware that the *type definitions differ between the two backends* — check both `prover/src/contexts/` and `prover_v2/src/contexts.rs`. CI builds and lints both feature sets, so changes must compile under both.

The `prover_v2` GPU build currently requires the `pinned-pages` feature on `zkm-gpu-core` (set in `prover_v2/Cargo.toml`).

## High-level architecture

### Stage workflow

The stage runs a fixed pipeline of task types. Each task transitions through `INITIAL → UNPROCESSED → PROCESSING → SUCCESS|FAILED` (constants in `proof-service/src/stage/tasks/mod.rs`):

```
Init (GenerateTask) → Split → Prove → Agg → Snark → End
                                  │
                                  └─ if composite_proof, skip Agg/Snark
```

| Stage | Task type      | Lives in            |
|-------|----------------|---------------------|
| Init  | `GenerateTask` | memory              |
| Split | `SplitTask`    | disk (NFS / S3)     |
| Prove | `ProveTask`    | memory              |
| Agg   | `AggTask`      | memory              |
| Snark | `SnarkTask`    | memory              |

`Stage` (`proof-service/src/stage/stage.rs`) owns one `GenerateTask`, one `SplitTask`, a `Vec<ProveTask>`, a `Vec<AggTask>`, and one `SnarkTask`. Task generation happens lazily: split → produces N prove tasks → produces a binary tree of agg tasks → snark task. `is_tasks_gen_done` flips once the full graph exists. `ProveTask` failures are retried up to 3 times before the whole stage is marked errored (`on_prove_task!` macro).

`stage_worker` is the background loop that drives task transitions and calls into `prover_client` to dispatch ready tasks; `stage_service` exposes the public gRPC surface (`generate_proof`, `get_status`); persistence goes through `database.rs` (SQLx + MySQL, schema in `migrations/`).

### Stage ↔ Prover-node split

Two binary roles, one executable:

- **Stage** (`--stage`): runs `StageServiceSVC`, the worker loop, the file server for the example client, and a `prover_node` registry seeded from `runtime_config.prover_addrs`. It calls out to prover nodes via `prover_client`.
- **Prover node** (default): runs `ProverServiceSVC` (`proof-service/src/prover_service.rs`) which exposes RPCs `split_elf`, `prove`, `aggregate`, `snark_proof`, `single_node`, `get_status`, `get_task_result`. Each RPC builds a backend `*Context` and runs it through the backend `Pipeline` on `spawn_blocking`, with panics caught and surfaced as task failures (`run_back_task`).

A prover node can serve any of the RPCs, but because `snark_proof` is CPU-heavy and the others are GPU-heavy, deployments typically schedule different node instances on different machines for hardware affinity.

### Shared filesystem requirement

`split_elf` reads ELFs that the stage's `GenerateTask` wrote, so all stage and prover nodes must share `base_dir`. Supported backends are NFS or S3 (`s3://bucket/object`). `common::file` is the abstraction; for S3, the standard AWS env vars apply (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_DEFAULT_REGION`, `AWS_ENDPOINT_URL`). Removing this filesystem dependency in favor of pure gRPC streaming is a long-term TODO noted in `README.md`.

### Configuration

`RuntimeConfig` (`proof-service/src/config.rs`) is loaded from a TOML file via `--config`. Important fields: `addr`, `metrics_addr`, `database_url` (MySQL, stage only), `prover_addrs` (stage only), `base_dir`, `proving_key_paths` (indexed by `ProverVersion` enum from `proto/include/v1/includes.proto` — index 0 is `Zkm` v1, index 1 is `Zkm2` v2), `max_concurrent_tasks`, and optional TLS (`ca_cert_path`, `cert_path`, `key_path`).

Templates live in `proof-service/config/*.toml*`; `gen_config.sh` produces multi-prover layouts.

## Things to know when editing

- Changes that touch `prover/src/contexts/` or `prover_v2/src/contexts.rs` typically need a matching change on the `proof-service` side — and the dispatch code is feature-gated, so verify both `--features=prover` and `--features=prover_v2` still build.
- `proof-service/build.rs` regenerates protobuf bindings from `proto/src/proto/**/*.proto`; rerun `cargo build` after editing `.proto` files.
- The GPU code path in `bin/proof-service.rs` calls `plonky2::create_ctx(13, 13)` and `init_globalmem(128 MiB)` before serving and `destroy_ctx()` on shutdown — only under `feature = "gpu"`, only for prover-node mode.
- `prover/src/lib.rs::init_stark_op_stream_simple` reads hard-coded paths under `/mnt_zkm/app/mytest_get_opstreams/`; missing files are silently ignored. This is GPU-only.
- CI is Postgres-flavored via `DATABASE_URL` and `SQLX_OFFLINE=1` even though production uses MySQL — keep SQLx queries offline-compatible.
