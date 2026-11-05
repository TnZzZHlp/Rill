# Rill Agent Guidance

## Project

Rill is a Linux HTTP/HTTPS reverse proxy and static file server written in Rust. It provides multi-domain routing, KDL v2 configuration, profile/snippet reuse, TLS/SNI, WebSocket and SSE proxying, secure static file serving, Prometheus metrics, optional access logs and OpenTelemetry tracing, and transactional reload through a local Unix control socket.

- Rust edition 2024; declared MSRV is 1.95. The recorded working toolchain is 1.99.1; do not claim a 1.95 build without actually running it.
- Runtime stack: Pingora 0.9.0, OpenSSL, Tokio, KDL 6.7.1, Clap 4.6.
- Single binary named `rill`; the release binary is dynamically linked, not fully static.
- Explicit non-goals: ACME, HTTP/3, GUI, WAF, Lua/WASM plugins, complex load balancing, service discovery, Ingress controller, distributed configuration, and full Nginx configuration compatibility.

Primary documentation:

- `README.md`: build, quick start, CLI exit codes, examples.
- `docs/configuration.md`: authoritative KDL and CLI contract.
- `docs/architecture.md`: module boundaries, snapshot/reload semantics, security limits, error codes, observability boundaries.
- `docs/plans/rill-web-server/`: implementation plan and subplans; `docs/validation/`: historical acceptance evidence. These are records, not substitutes for running checks.
- `deploy/systemd/` and `deploy/podman/`: deployment constraints and unit files.
- `scripts/bench/`: Podman-based benchmark harness; not part of the normal Rust test suite.

## Layout

- `src/main.rs`: only delegates to `rill::cli::run`.
- `src/cli/`: argument parsing and command orchestration (`run`, `check`, `reload`). Keep it thin.
- `src/config/`: loading, imports, KDL parsing, profile/snippet normalization, validation, preparation, and source-spanned `ConfigError`.
- `src/routing/`: immutable Host and path matching.
- `src/runtime/`: candidate preparation, single-point publication, generation tracking, reload, and watch lifecycle.
- `src/proxy/`: Pingora adaptation, upstream selection, DNS and address policy, headers, streaming, WebSocket.
- `src/static_files/`: constrained root opens, representations, ranges, conditional responses, SPA, error pages, directory listing, cache.
- `src/tls/`: certificate preparation and SNI resolution.
- `src/control/`: Unix socket server/client, peer authorization, bounded JSON frames.
- `src/observability/`: structured logs, access events, metrics, optional tracing.
- `tests/<feature>.rs` plus `tests/<feature>/`: integration tests, grouped by feature with shared `support` modules.
- `examples/`: runnable KDL examples used by `check` and tests.
- `Cargo.toml` and `Cargo.lock`: dependencies are exactly pinned and the lock file is committed. Use `--locked`; do not add dependencies or run `cargo update` without explicit approval.
- In this checkout, `target` is a symlink to an external shared build cache. Do not delete or replace it.

## Build and Checks

Prerequisites are documented as Rust Stable, a C toolchain, `make`, Perl, CMake, and `pkg-config`; the locked dependency set builds vendored `libz-ng`/OpenSSL via CMake.

```sh
cargo build --release --locked
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo test --locked --offline --no-fail-fast -q -- --test-threads=2
```

- Add `--offline` only when the dependency cache is already complete. Network fetching of crates or other artifacts requires prior approval.
- The validation records run the same commands plus `--include-ignored --nocapture` for the full test suite and `cargo build --release --locked --offline -q` for release. Only include ignored tests when the isolated DNS fixture exists.
- `Cargo.toml` denies `unsafe_code` and promotes clippy style, complexity, performance, suspicious, pedantic, nursery, and cargo lints to `deny`. Code must stay warning-free under `-D warnings`.
- Configuration checks can be run directly:

```sh
cargo run --locked -- check --config examples/config.kdl
cargo run --locked -- run --config examples/config.kdl
rill reload --socket /run/rill/control.sock --timeout 35s
```

- `check` validates configuration, TLS material, static roots, and literal upstream policy, but does not resolve DNS, bind ports, connect upstream, or verify upstream TLS trust.
- CLI exit codes are 0 for success, 1 for configuration/preparation or explicit server failure, 2 for invalid arguments, and 3 for management communication failure, timeout, or unknown result. Exit 3 does not prove the reload failed or did not happen; inspect logs and service state instead of blindly retrying. Reload is a control-socket operation, not SIGHUP, and startup-only settings cannot be changed through it.
- Permission-denial tests require an unprivileged UID and a test binary/paths readable by that UID. Running the suite as root in a root-only workspace can fail these tests by design.
- `tests/reload/timeout.rs` contains an ignored test that requires an isolated resolver at `127.0.0.1:53`. Do not enable ignored tests unless that fixture exists.

## Coding Conventions

- Keep entry points thin; put business logic in the owning module. Do not add routing, TLS, or configuration rules to `main` or CLI code.
- `check`, `run`, reload, and watch must reuse the same pipeline: load, parse, normalize, validate, prepare resources, then publish one snapshot. Do not create a second validation path.
- Runtime state is a single immutable `RuntimeSnapshot` behind `ArcSwap`. Prepare the complete candidate before publishing; never publish routes, certificates, or policies piecemeal. A failed candidate leaves the old generation serving.
- Startup settings such as listen addresses, control socket path, watch enablement, static cache budget, metrics address, and OTLP endpoint require a restart. Reload must reject changes to these with `RESTART_REQUIRED`; do not silently apply them.
- Keep configuration strict: KDL v2 booleans are `#true`/`#false`, unknown fields are errors, relative paths resolve from the file that declares them, and profile/snippet expansion must preserve origin spans. Add syntax only with an approved contract change.
- Return structured, redacted errors. Recoverable external input must not be `unwrap`ped. Do not print private keys, credentials, raw management frames, or sensitive configuration values.
- Preserve the security boundaries: constrained `openat2` root traversal, control socket directory mode 0700 and socket mode 0600 with peer credential checks, upstream address allow lists, loopback-only metrics/OTLP, and bounded buffers, queues, timeouts, and retries.
- Never commit real private keys, certificates with private material, tokens, or generated validation artifacts containing secrets. Examples may use placeholders only.
- Comments are sparse in this codebase. Do not add explanatory comment blocks, documentation, tests, fixtures, or tooling unless explicitly requested.
- `README.md` and `docs/` are written in Chinese. Match their language and style when updating user-facing documentation.

## Testing

- Tests are end-to-end/integration oriented. They frequently launch the real `CARGO_BIN_EXE_rill` binary, create temporary directories, bind loopback ports, start real backend threads, and assert HTTP behavior. Do not replace these with mocks or a new test framework.
- Shared helpers live in `tests/<feature>/support.rs`; reuse them instead of duplicating process, port, or fixture setup.
- Default to no new tests. Add a minimal regression test for a reproduced bug, or a small test for high-risk logic (money, permissions, security, data consistency, state transitions, complex boundaries) when the project already has the harness.
- Do not test UI text, simple getters/setters, forwarding code, configuration values, framework behavior, generated output, or one-off scripts.
- Do not add mocks, fixtures, helpers, test tools, test configuration, CI, or dependencies without explicit approval.
- Benchmark scripts under `scripts/bench/` require Podman, prepared certificates, cpusets, private writable directories, and specific network fixtures. Do not run them casually; read `scripts/bench/README.md` first.

## Git and Change Scope

- The repository uses one completion commit per implementation subplan. The established message form is `feat(rill-web-server): complete subplan NNN - phase-name`; use the repository convention if a different change is requested.
- Stage only paths belonging to the intended change. Do not mix unrelated subplans, pre-staged edits, or user changes. Do not push, rewrite history, create empty commits, or bypass hooks.
- Before changing behavior, read `docs/architecture.md` and `docs/configuration.md`, then update the relevant plan/validation status only when the work is actually done and verified.
- Preserve unrelated user edits in the working tree. Do not reformat or rewrite files outside the requested scope.
