# Agents.md

Keep this file current as the project develops.

Gnosis Core is the source of truth for Gnosis storage and shared domain
behavior. It serves the `gnosis` Emacs frontend, command-line, and future web
clients. The separate `gnosis` package is the Emacs frontend, not a separate
product named emacs-gnosis. Its established UI and behavior are the
compatibility contract for the migration. Coordinated frontend changes are
expected when a core operation lands.

The deployed daemon exclusively owns
`~/.local/share/gnosis/gnosis.sqlite`. The Emacs frontend connects through
`$XDG_RUNTIME_DIR/gnosis/gnosis.sock`; it must never open the core database
directly.

## Architecture

- `src/domain` is the functional core. It contains no SQLite, Tokio, Serde JSON,
  sockets, filesystem, or orgize types.
- `src/application` validates typed commands and builds projections before a
  write can enter the database queue.
- `src/parser` is the concrete Org adapter. Do not add a parser trait until a
  second implementation creates a real substitution boundary.
- `src/store` is the only SQLite adapter. One daemon-owned thread holds the
  connection and handles both reads and atomic writes through a bounded queue.
  SQLite files are mode `0600`.
- `src/migration.rs` owns the offline schema-v9 snapshot path. It must never
  modify or replace the source database, and it publishes only a fully checked
  target.
- Legacy themata and scheduling state are exposed through a read-only adapter.
  Keep review mutations out until their compatibility contract is explicit.
- `src/protocol` owns the versioned wire DTOs, framing, Unix-socket server, and
  client. Public fields use opaque string IDs, never SQLite row IDs.
- `src/bin/gnosisd.rs` is the only shipped database owner.
  `src/bin/gnosis.rs` uses only protocol methods.

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

Emacs integration and the migration of complete frontend operations are active
work. Review writes remain unavailable until their scheduling, immutable event,
and atomic state-transition contract is explicit. Remote transport,
authentication, and synchronization remain out of scope.

## Commands

The ambient host may not have Cargo on `PATH`. Enter the pinned development
environment when needed:

```sh
nix develop
```

Focused checks:

```sh
cargo test domain
cargo test --lib parser::org_tests
cargo test --test org_parser
cargo test --lib store::sqlite::tests
cargo test --test sqlite_store
cargo test --test legacy_migration
cargo test application
cargo test protocol
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
