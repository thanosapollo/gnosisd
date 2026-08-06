# Agents.md

`gnosisd` owns canonical documents, roam-like nodes/links, themata, review state,
grading, FSRS, persistence, blobs, sync. Clients: typed authenticated HTTP `/v1` only.
Never open canonical DB or compute scheduling client-side.

Unix socket = privileged local operator (system info, device bootstrap/revocation,
backup, recovery, diagnostics). Data/content/review stay HTTP.

## Architecture

- `src/domain`: functional core; no SQLite, Tokio, Serde JSON, sockets, FS, orgize
- `src/application`: validate typed commands; projections before persistence queue
- `src/parser`: concrete Org adapter; no parser trait until second impl
- `src/store`: sole SQLite adapter; DB files mode `0600`
- `src/service`: imports, backups, blobs, persistence; one daemon thread owns canonical
  connection; bounded-queue reads/atomic writes
- `src/protocol`: versioned wire DTOs, HTTP routes, OpenAPI, privileged Unix framing;
  opaque public IDs, never SQLite row IDs
- `src/protocol/http_v1.rs`: client HTTP contract kernel; runtime DTOs == OpenAPI
  schemas; no parallel JSON models
- `src/bin/gnosisd.rs`: only shipped canonical DB owner
- `src/bin/gnosisd-admin.rs`: privileged Unix operator client

## Data and review invariants

Keep separate: textual link occurrences, unique semantic node edges, `ThemaConcept`
associations. Node titles local; title paths = indexed ancestors only; source spans =
zero-based half-open UTF-8 byte ranges over complete Org links. Daemon owns grading,
FSRS transitions, session progress, suspension, daily totals, revisions, conflicts,
immutable review events. Pending HTTP attempts: server-materialized questions + typed
response prompts only; answers server-side until grading. Desired-state mutations:
durable device-scoped op IDs + expected revisions; exact retries return retained
results; stale/competing revisions explicit. Review + content ops share one contiguous
per-device sequence; accepted/conflict results immutable; invalid ops change no
canonical state. Document revisions bind content digests; numeric revision alone never
authorizes offline write. Canonical Org writes reference active content-addressed assets
only; verify path, digest, size, state before publication/use. Replica bootstrap: atomic
complete snapshot install before durable cursor advance; reconciliation cursors never
fast-forward replica feed. Unknown mutation delivery stops sync with op identity intact;
next explicit sync replays that exact immutable op. Review pack acquisition explicit;
persist typed packs before download/asset verify; periodic replica sync never creates
pack.

## Safety

No transactions, clocks, randomness, socket I/O, FS, or async types in `src/domain`. No
`Arc<Mutex<Connection>>`. Accepted writes finish after client disconnect; no blocking
guard/transaction across `.await`. Bound bodies, responses, queues, parsing; CPU-heavy
parse/projection off Tokio workers. HTTP mutations: exact scopes + statuses; no
redirects, implicit proxy trust, auto mutation retries, or hidden delivery uncertainty.
Socket startup: atomic `.startup-lock` dir ownership for stale cleanup/bind; SIGINT/SIGTERM graceful
shutdown. Never expose secrets, tokens, private topology, operator paths, or live data
in source, tests, logs, docs, commits, handoffs.

## Verification

Org parity (`GNOSIS_ELISP_DIR`) optional; not monorepo handoff gate. `nix flake check` =
package + `cargo test` only (not fmt/clippy/doc). Required:

```sh
nix flake check
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo tree --depth 1
```

## Git

Coherent signed commits; behavior + matching tests together; subjects `module: Change`.
Preserve unrelated work; never commit plans, agent artifacts, generated metadata,
secrets, private operational details. Never amend, rebase, push, deploy, or activate
without explicit permission.
