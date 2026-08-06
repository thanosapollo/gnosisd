# Agents.md

`gnosisd` owns canonical documents, roam-like nodes/links, themata, review
state, grading, FSRS, persistence, blobs, sync. Clients: typed authenticated
HTTP `/v1` only. Never open canonical DB or compute scheduling client-side.

Unix socket = privileged local operator boundary (system info, device
bootstrap/revocation, backup, recovery, diagnostics). Ordinary data/content/
review stay HTTP.

## Architecture

- `src/domain`: functional core. No SQLite, Tokio, Serde JSON, sockets, FS,
  orgize types.
- `src/application`: validate typed commands; build projections before
  persistence queue.
- `src/parser`: concrete Org adapter. No parser trait until second impl needs
  substitution boundary.
- `src/store`: sole SQLite adapter. DB files mode `0600`.
- `src/service`: imports, backups, blobs, persistence. One daemon thread holds
  canonical connection; reads/atomic writes via bounded queues.
- `src/protocol`: versioned wire DTOs, HTTP routes, OpenAPI, privileged Unix
  framing. Public IDs opaque strings, never SQLite row IDs.
- `src/protocol/http_v1.rs`: client HTTP contract kernel. Runtime DTOs and
  OpenAPI schemas identical; no parallel JSON models.
- `src/bin/gnosisd.rs`: only shipped canonical DB owner.
- `src/bin/gnosisd-admin.rs`: minimal privileged Unix operator client.

## Data and review invariants

- Keep textual link occurrences, unique semantic node edges, `ThemaConcept`
  learning associations separate.
- Node titles local. Title paths = indexed ancestors only. Source spans
  zero-based half-open UTF-8 byte ranges over complete Org links.
- Daemon owns grading, FSRS transitions, session progress, suspension, daily
  totals, revisions, conflicts, immutable review events.
- Pending HTTP attempts: server-materialized questions + typed response prompts
  only. Answers server-side until grading.
- Desired-state mutations: durable device-scoped op IDs + expected revisions.
  Exact retries return retained results; stale/competing revisions stay
  explicit.
- Review + content ops share one contiguous per-device sequence. Accepted and
  conflict results immutable; invalid ops change no canonical state.
- Document revisions bind content digests. Numeric revision alone never
  authorizes offline write.
- Canonical Org writes reference active content-addressed assets only. Verify
  path, digest, size, state before publication/use.
- Replica bootstrap: atomic complete snapshot install before durable cursor
  advance. Reconciliation cursors never fast-forward replica feed.
- Unknown mutation delivery stops sync with op identity intact. Next explicit
  sync replays that exact immutable operation.
- Review pack acquisition explicit. Persist typed packs before download/asset
  verify. Periodic replica sync never creates pack.

## Safety

- No transactions, clocks, randomness, socket I/O, FS, or async types in
  `src/domain`.
- No `Arc<Mutex<Connection>>`.
- Accepted writes finish after client disconnect. No blocking guard/transaction
  crosses `.await`.
- Bound bodies, responses, queues, parsing. CPU-heavy parsing/projection prep
  off Tokio runtime workers.
- HTTP mutations: exact scopes + statuses. No redirects, implicit proxy trust,
  automatic mutation retries, or hidden delivery uncertainty.
- Socket startup: atomic `.startup-lock` dir ownership for stale cleanup/bind.
  SIGINT/SIGTERM graceful shutdown.
- Never expose secrets, tokens, private topology, operator paths, or live data
  in source, tests, logs, docs, commits, handoffs.

## Verification

```sh
nix develop   # when Cargo unavailable
```

Focused:

```sh
cargo test --lib domain::
cargo test --lib parser::org_tests
cargo test --lib store::sqlite::tests
cargo test --lib application::
cargo test --lib protocol::
cargo test --test daemon_protocol
```

Optional Org parity: monorepo `clients/emacs` ships no `lisp/gnosis-org.el`.
`tools/check-org-parity.sh` needs external tree via `GNOSIS_ELISP_DIR`; not a
monorepo handoff gate.

Required handoff gates. `nix flake check` builds default package; checkPhase
runs `cargo test` via cargoCheckHook only (not fmt/clippy/doc). Keep cargo
gates below for those:

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

- Coherent signed commits. Behavior + matching tests together.
- Subjects: `module: Change`.
- Preserve unrelated work. Never commit plans, agent artifacts, generated
  metadata, secrets, private operational details.
- Never amend, rebase, push, deploy, or activate without explicit permission.
