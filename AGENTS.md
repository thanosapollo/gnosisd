# Agents.md

Keep this file current and concise.

Gnosis Core is a standalone daemon/server. `gnosisd` owns canonical documents,
parsed roam-like nodes and links, themata linked to nodes, review state, grading,
FSRS scheduling, persistence, blobs, and synchronization. Clients consume typed
authenticated HTTP `/v1`; they never open the canonical database or calculate
scheduling.

The Unix socket is a privileged local operator boundary for system information,
device bootstrap and revocation, backup, recovery, and diagnostics. Ordinary
data, content, and review operations use HTTP only.

## Architecture

- `src/domain` is the functional core. It contains no SQLite, Tokio, Serde JSON,
  sockets, filesystem, or orgize types.
- `src/application` validates typed commands and builds projections before a
  write enters the persistence queue.
- `src/parser` is the concrete Org adapter. Add no parser trait until a second
  implementation creates a real substitution boundary.
- `src/store` is the only SQLite adapter. SQLite files are mode `0600`.
- `src/service` owns imports, backups, blobs, and persistence coordination. One
  daemon-owned thread holds the canonical connection and handles reads and
  atomic writes through bounded queues.
- `src/protocol` owns versioned wire DTOs, HTTP routes, OpenAPI, and privileged
  Unix framing. Public IDs are opaque strings, never SQLite row IDs.
- `src/protocol/http_v1.rs` is the client HTTP contract kernel. Runtime DTOs and
  OpenAPI schemas must remain identical; do not maintain parallel JSON models.
- `src/bin/gnosisd.rs` is the only shipped canonical database owner.
- `src/bin/gnosisd-admin.rs` is the minimal privileged Unix operator client.
  Ordinary data, content, and review operations remain HTTP client concerns.

## Data and review invariants

- Keep textual link occurrences, unique semantic node edges, and
  `ThemaConcept` learning associations separate.
- Node titles are local values. Title paths contain indexed ancestors only.
  Source spans are zero-based, half-open UTF-8 byte ranges covering complete
  Org links.
- The daemon owns grading, FSRS transitions, session progress, suspension,
  daily totals, revisions, conflicts, and immutable review events.
- Pending HTTP attempts expose only server-materialized questions and typed
  response prompts. Keep answers server-side until grading.
- Desired-state mutations use durable device-scoped operation IDs and expected
  revisions. Exact retries return retained results; stale or competing
  revisions remain explicit.
- Review and content operations share one contiguous per-device sequence.
  Accepted and conflict results are immutable; invalid operations change no
  canonical state.
- Document revisions bind to captured content digests. Numeric revision alone
  never authorizes an offline write.
- Canonical Org writes reference active content-addressed assets only. Verify
  asset path, digest, size, and state before publication or use.
- Replica bootstrap installs complete snapshots atomically before advancing its
  durable cursor. Reconciliation cursors never fast-forward the replica feed.
- Unknown mutation delivery stops synchronization with operation identity
  intact. Next explicit sync replays that exact immutable operation.
- Review pack acquisition is explicit. Persist typed packs before downloading
  and verifying assets. Periodic replica sync never creates a pack.

## Safety

- Keep transactions, clocks, randomness, socket I/O, filesystem access, and
  async types out of `src/domain`.
- Do not use `Arc<Mutex<Connection>>`.
- Accepted writes finish after client disconnect. No blocking guard or
  transaction crosses `.await`.
- Bound request bodies, responses, queues, and parsing work. CPU-heavy parsing
  and projection preparation must not run on Tokio runtime workers.
- HTTP mutations require exact scopes and statuses. Do not follow redirects,
  trust proxies implicitly, retry mutations automatically, or hide delivery
  uncertainty.
- Socket startup uses atomic `.startup-lock` directory ownership for stale
  cleanup and bind. SIGINT and SIGTERM enter graceful shutdown.
- Never expose secrets, tokens, private topology, operator paths, or live data
  in source, tests, logs, docs, commits, or handoffs.

## Verification

Enter pinned environment when Cargo is unavailable:

```sh
nix develop
```

Focused checks:

```sh
cargo test --lib domain::
cargo test --lib parser::org_tests
cargo test --lib store::sqlite::tests
cargo test --lib application::
cargo test --lib protocol::
cargo test --test daemon_protocol
```

Optional Org parity: monorepo `clients/emacs` does not ship
`lisp/gnosis-org.el`. `tools/check-org-parity.sh` needs an external
tree via `GNOSIS_ELISP_DIR` and is not a monorepo handoff gate.

Required handoff gates:

`nix flake check` builds the default package. That package's checkPhase
runs `cargo test` via cargoCheckHook. It does not run `cargo fmt`,
`clippy`, or `cargo doc`. Keep the cargo gates below for those.

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

- Commit completed work in coherent signed commits. Keep behavior and matching
  tests together.
- Use `module: Change` subjects.
- Preserve unrelated work. Never commit plans, agent artifacts, generated
  metadata, secrets, or private operational details.
- Never amend, rebase, push, deploy, or activate without explicit permission.
