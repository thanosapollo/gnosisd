# Agents.md

Keep this file current as the project develops.

Gnosis Core is the source of truth for Gnosis storage and shared domain
behavior. It serves the `gnosis` Emacs frontend, command-line, and future web
clients. The separate `gnosis` package is the Emacs frontend, not a separate
product named emacs-gnosis. Its established UI and behavior are the
compatibility contract for the migration. Coordinated frontend changes are
expected when a core operation lands.

The deployed daemon exclusively owns the configured SQLite database. Ordinary
clients use authenticated HTTP `/v1`; the Unix socket is the privileged local
trust, administration, recovery, and temporary compatibility boundary. No
client may open the core database directly.

## Architecture

- `src/domain` is the functional core. It contains no SQLite, Tokio, Serde JSON,
  sockets, filesystem, or orgize types.
- `src/application` validates typed commands and builds projections before a
  write can enter the database queue.
- `src/parser` is the concrete Org adapter. Do not add a parser trait until a
  second implementation creates a real substitution boundary.
- `src/store` is the only SQLite adapter. SQLite files are mode `0600`.
- `src/service` is the concrete imperative shell for server imports, backups,
  blobs, and persistence coordination. One daemon-owned thread holds the
  canonical connection and handles reads and atomic writes through a bounded
  queue. The concrete client replica store owns a separate local connection.
- `src/migration.rs` owns the offline schema-v9 snapshot path. It must never
  modify or replace the source database, and it publishes only a fully checked
  target.
- Legacy themata remain exposed through a read-only adapter. Normalized review
  writes use immutable attempts and events plus atomic scheduling transitions.
- `src/protocol` owns the versioned wire DTOs, HTTP API, and privileged Unix
  framing. Public fields use opaque string IDs, never SQLite row IDs.
  `src/protocol/http_v1.rs` is the client-facing HTTP contract kernel. Generate
  OpenAPI schemas from these DTOs instead of maintaining parallel JSON models.
- Local online review DTOs remain in `src/protocol`. Pure validation and
  transition construction remain in `src/application`, concrete server
  orchestration remains in `src/service`, and persistence remains in
  `src/store`. The daemon owns grading, scheduling transitions, session
  progress, suspension, daily totals, and immutable review events. Do not move
  that authority into a frontend.
- Review attempt envelope IDs are disposable correlation IDs. Desired-state
  mutations carry a separate durable operation ID and expected attempt
  revision. Operation identity is scoped to the authenticated device or local
  Unix actor. Exact retries return the retained result; competing revisions
  and stale captures remain explicit. Legacy toggle methods are compatibility
  adapters and advance the same monotonic attempt revision.
- Pending HTTP review attempts expose only a server-materialized question and
  typed response prompt. Raw answer-bearing keimenon and captured answers stay
  server-side until grading. Keep concealment and response cardinality in the
  pure application projection instead of duplicating them in clients.
- Offline review HTTP uses a typed v1 operation envelope and typed v2 pack and
  reconciliation responses. Pack, session, source, status, and conflict
  discriminants are closed enums. Keep the runtime response types and OpenAPI
  schemas identical instead of returning generic JSON values.
- Offline document reconciliation uses typed v1 operation and result envelopes.
  Mutation, base, status, and conflict variants are closed, and accepted versus
  conflict results are disjoint wire states.
- Frozen direct note routes retain their original v1 status and response shapes.
  Retry-safe direct document writes and lifecycle changes use the additive
  document-mutation routes, device-scoped immutable receipts, exact request
  fingerprints, retained typed results, and durable replica cursors. A changed
  operation key must fail before blob I/O; rejected and no-op mutations must not
  publish unreferenced blobs or advance document revisions and cursors.
- Replica bootstrap uses a device-bound immutable snapshot manifest captured at
  one content cursor. Cursorless retries reuse a resumable manifest. Clients
  must install every page atomically before promoting the terminal delta cursor.
- Delta cursors bind the stable server identity, numeric position, and unique
  feed token so restored branches cannot reuse a cursor silently. Every feed
  row resolves to an exact immutable revision. SSE is only a wakeup.
- `src/client_replica.rs` is the concrete offline client foundation. Partial
  snapshots stay in staging tables, terminal pages atomically replace active
  projections, and snapshot recovery never deletes review packs, content or
  review operations, results, or device sequence state. Reconciliation cursors
  are hints and must never fast-forward the replica feed cursor.
- `src/client_sync.rs` is the concrete HTTP replica coordinator. It observes
  server identity, resumes or recovers snapshots, pulls durable deltas, fetches
  required blobs, and drains the shared outbox strictly by device sequence.
  Unknown mutation delivery stops synchronization with the operation identity
  intact; the next explicit run replays that exact immutable operation.
- `src/bin/gnosisd.rs` is the only shipped database owner.
  `src/bin/gnosis.rs` routes review attempt recovery and revisioned desired-state
  operations through `HttpClient`. Operator commands and remaining compatibility
  methods still use the Unix socket; ordinary commands must migrate before the
  corresponding Unix methods are removed.

Keep textual link occurrences, unique semantic node edges, and `ThemaConcept`
learning associations separate. Node titles are local values; title paths
contain only indexed ancestors. Source spans are zero-based, half-open UTF-8
byte ranges covering the complete Org link.

Do not put transactions, clocks, randomness, socket I/O, filesystem access, or
async types in the domain. Do not use `Arc<Mutex<Connection>>`. Accepted writes
must finish even if their client disconnects, and no blocking guard or
transaction may cross `.await`.

Socket startup uses an atomic `.startup-lock` directory to serialize stale
cleanup and bind. Do not weaken it to a stat-then-unlink sequence. SIGINT and
SIGTERM both enter graceful shutdown.

Online reviews and device-bound offline review packs reconcile through
immutable operations. The daemon remains authoritative, conflicts stay
explicit, and review or content state must never use last-write-wins.
Offline review v1 submissions remain accepted; v2 pack and result envelopes
return durable replica cursors.
Review and content operations share one contiguous per-device sequence. Offline
document puts retain their complete submitted Org revision. Renames and
tombstones retain their captured base. Every mutation receives an immutable
accepted or conflict result. Invalid, stale, or expired-cursor mutations must
leave the canonical document head and index unchanged.
Lifecycle operations may be queued only after the replica installs the
server-bound content capability. Keep that negotiation separate from the
strict v1 sync-state response so old clients remain compatible. Replica path
lookup overlays the active server's accepted and pending content chain, and
supplies stable document IDs for frontends whose Org files have no file ID.
Reject a locally known path collision before enqueue so optimistic lookup never
turns an expected conflict into apparent replica corruption.
Queued document mutations bind every revision to its captured content digest.
Downloaded bases must predate the captured cursor; predicted bases require an
accepted predecessor operation. A numeric revision alone must never authorize
an offline write. A queued or accepted tombstone closes its local operation
chain until a newer active canonical projection is downloaded.
Canonical Org writes may reference only active content-addressed assets. Store
current and historical document asset associations, rebuild them from retained
Org blobs before daemon startup completes, and reject asset tombstones while
active documents or themata still reference them. If an active legacy document
has no source blob, asset deletion fails closed because its references cannot be
verified. Offline review packs reject external assets. Replica asset lookup must
distinguish missing, tombstoned, and available state and verify the downloaded
bytes against the canonical path, digest, and recorded size.
Offline asset creation uses a separate strict capability and wire protocol but
shares the contiguous device operation sequence. A replica must store the bytes
and immutable operation in one local transaction. It uploads the blob before
the operation; missing server bytes must not create a journal result or consume
the sequence. Server reconciliation atomically commits the asset head, revision,
feed row, result, and sequence. Asset rename, deletion, and reactivation remain
online-only until their offline reference-rewrite and resurrection contracts are
explicit.

## Commands

The ambient host may not have Cargo on `PATH`. Enter the pinned development
environment when needed:

```sh
nix develop
```

Focused checks:

```sh
cargo test --lib domain::
cargo test --lib parser::org_tests
cargo test --test org_parser
cargo test --lib store::sqlite::tests
cargo test --test sqlite_store
cargo test --test legacy_migration
cargo test --lib application::
cargo test --lib protocol::
cargo test --test daemon_protocol
```

Required handoff gates:

```sh
nix flake check
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo tree --depth 1
```

Org parity checks run the live Elisp code read-only:

```sh
export GNOSIS_ELISP_DIR=/path/to/gnosis
tools/check-org-parity.sh tests/fixtures/org/basic.org
tools/check-org-parity.sh tests/fixtures/org/no-file-id.org
tools/check-org-parity.sh tests/fixtures/org/duplicate-links.org
tools/check-org-parity.sh tests/fixtures/org/unicode.org
tools/check-org-parity.sh tests/fixtures/org/compatibility.org
```

Parity checks must not run build targets or write compiled files into the Emacs
frontend checkout.

## Git

- Commit completed work without asking for separate permission.
- Split work into coherent logical commits. Keep a feature and its tests in the
  same commit.
- Use `module: Change` subjects. Add a minimal body only when the reason or an
  important constraint is not clear from the subject and diff.
- Preserve unrelated user changes and never commit planning notes, local
  assistant configuration, generated metadata, or other unrelated files.
- Never amend, rebase, or push without explicit permission.
