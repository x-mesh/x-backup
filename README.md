<p align="center">
  <strong>English</strong> · <a href="README.ko.md">한국어</a>
</p>

# x-backup

> MongoDB & PostgreSQL backup and restore CLI — full and incremental (oplog) backups, PITR, local or S3-compatible storage, encrypted by default. A single Rust binary, **no external dump tools**.

x-backup backs up a running MongoDB (standalone or replica set) or PostgreSQL into an encrypted form you can verify and actually restore from. By default it talks to the database directly through the Rust driver — **no `mongodump`/`mongorestore` or `pg_dump`/`pg_restore` required** — and streams every stage, so memory stays flat no matter how large the dataset is. A 6 GiB backup peaks at 54.6 MiB RSS ([measured](docs/memory-profile.md)). The database is chosen automatically from the source URI scheme (`mongodb://` vs `postgresql://`).

## Features

- ✅ **Full backup** — driver-native streaming archive (data + indexes + collection options), no external tools; `mongodump --archive --oplog` available as an opt-in engine
- ✅ **Incremental backup** — captures the oplog directly; on a gap it promotes to a full backup automatically (exit 4)
- ✅ **PITR** — restore to a moment with `--at <RFC3339>|latest` (base restore + replay of MongoDB oplog or PostgreSQL logical-decoding changes, gated on chain verification)
- ✅ **Storage** — local disk or S3-compatible (MinIO, R2, OCI), streaming multipart upload with abort cleanup
- ✅ **Encrypted by default** — `age` (X25519; only the public key lives on the backup host) or AES-256-GCM, compressed with zstd before encryption
- ✅ **Integrity** — manifest + sha256, with `verify` (structural check, no key needed), `--deep`, and `--chain`
- ✅ **Operations** — `doctor` offline config check (all profiles, no DB connection), `status` preflight (connection, topology, privileges, version/FCV, clock skew, oplog window, data shape, **last backup age**, **destination writability + free space**), `--all` source-vs-target diff, `--watch` live monitor, chain-safe `prune`, concurrent-run locking, a defined exit-code contract (0–5)
- ✅ **PostgreSQL** — driver-native full backup/restore via the COPY protocol (data + tables + constraints + indexes + sequences), no `pg_dump`/`pg_restore`. Same pipeline (compress → encrypt → store), same `status`/`list`/`verify`/`restore`
- ✅ **Headless** — auto-quiet when not a TTY, `--json` output, built for cron and CI

Scope: MongoDB replica sets get full and incremental backups, standalone gets full only, and sharded clusters are detected and refused. PostgreSQL gets full backup, restore, migrate, status/peek/watch, plus incremental backup and PITR via logical decoding (opt-in). See [PostgreSQL](#postgresql).

## Install

### Homebrew

```bash
brew install x-mesh/tap/x-backup
```

While the repository is private, downloading release assets needs a GitHub token:

```bash
export HOMEBREW_GITHUB_API_TOKEN=$(gh auth token)
brew install x-mesh/tap/x-backup
```

### curl (install.sh)

```bash
# Once the repository is public:
curl -fsSL https://raw.githubusercontent.com/x-mesh/x-backup/main/install.sh | sh

# While private (reuses your gh auth):
gh api repos/x-mesh/x-backup/contents/install.sh --jq '.content' | base64 -d | sh
```

This installs to `~/.local/bin/x-backup`. Override with `XB_VERSION` or `XB_INSTALL_DIR`.

### From source

```bash
git clone git@github.com:x-mesh/x-backup.git && cd x-backup
make build        # → target/release/x-backup
```

The default `native` engine needs no external tools. `mongodump`/`mongorestore` (MongoDB Database Tools 100.x) are only required if you opt into the `mongodump` engine (see [Backup engine](#backup-engine)); `make tools` installs them locally under `.tools/` with sha256 verification.

## Update

```bash
x-backup update           # detects how it was installed
x-backup update --check   # check only, no install
```

- **Homebrew install** → delegates to `brew upgrade x-mesh/tap/x-backup`
- **install.sh install** → downloads the latest release, verifies sha256, and replaces itself atomically
- **cargo install** → prints the upgrade command instead of overwriting

While the repository is private, this needs `GITHUB_TOKEN` (or a prior `gh auth login`).

## Quick Start

```bash
x-backup init                                   # interactive wizard → config.toml
x-backup doctor                                 # offline config check: all profiles, no DB connection
x-backup status  --profile prod                 # is the server ready to back up?
x-backup status  --all                          # side-by-side diff of every profile (source vs target)
x-backup status  --profile prod --watch         # live monitor: per-namespace doc/size deltas (Ctrl-C)
x-backup peek    --profile prod                 # eyeball data: per-collection counts + latest doc
x-backup backup  --profile prod                 # full backup → compress → encrypt → store
x-backup backup  --profile prod --type incr     # oplog increment
x-backup list    --profile prod                 # catalog, with chain status
x-backup verify  --id <backup-id>               # structural check, no key required
x-backup restore --profile prod --target mongodb://staging --dry-run
x-backup restore --profile prod --target mongodb://staging --force
x-backup restore --profile prod --at 2026-06-01T00:00:00Z --force   # PITR
x-backup prune   --profile prod --keep-last 100 --dry-run
x-backup migrate --profile prod --target mongodb://newcluster --force # direct copy, no file
```

Every command prints a one-line context to stderr — the active profile and DB engine,
e.g. `▸ 프로파일 prod · DB postgresql` — so you always know what you're touching in a
multi-DB config (skipped under `--json`). `--profile` can also come from the `XB_PROFILE`
env var, and `--config` from `XB_CONFIG`.

### Migrate (direct copy, no file)

`migrate` copies one MongoDB straight into another — streamed directly, no intermediate
file. Use it for one-off moves where you don't need a stored, verifiable backup. Like
`backup`/`restore`, it honours the profile's [engine](#backup-engine): the default `native`
engine streams driver-to-driver with **no external tools**; the `mongodump` engine pipes
`mongodump | mongorestore` instead. Either way it copies data, indexes, and collection
options.

```bash
x-backup migrate --profile prod --target mongodb://newcluster --dry-run
x-backup migrate --profile prod --target mongodb://newcluster --drop --force
x-backup migrate --profile prod --target-profile staging --drop --force   # target from a profile
```

The target can be a literal URI (`--target`) or another profile's source
(`--target-profile <name>`, resolved from the same config) — so you can keep both
endpoints in `config.toml` instead of pasting URIs.

**Target rules** (migrate means *replace*, and it never wipes the whole target):
- **Empty target** → just copies, no flags needed.
- **Target with data** → `--drop` is **required**. Without it, migrate is refused (exit 2):
  a no-`--drop` copy would be a half-merge (inserts without dropping; same-`_id` docs are
  kept, stale docs remain) — almost never what a migration wants. With `--drop`, each
  collection **present in the source** is dropped and recreated; other collections in the
  target are left alone. The target database/instance is never fully dropped.
- `--drop` is destructive, so it also needs `--force` (or an interactive confirm). A clean
  replace of the migrated collections is therefore `--drop --force`.

There is no incremental migration — `migrate` is a one-shot copy. For a point-in-time move
or an ongoing chain, use the file path (`backup` → `restore --at`). Live migration
(oplog-tailing with near-zero-downtime cutover) is a roadmap item, not implemented.

It is a copy, not a backup: no manifest, checksum, encryption-at-rest, or PITR. For a
point-in-time-consistent move of a busy replica set, or to keep a verifiable artifact,
use the file path instead (`backup` → `restore --target`, which supports `--oplog`/PITR).

### config.toml

A config has three distinct axes, easy to conflate:

- **source** — the MongoDB you back up (your prod). `uri_env` (an env var name) for anything
  with credentials; `uri` (a literal) is fine for local/no-secret connections.
- **destination** — where backup *files* go. This is storage (`local` / `s3`), **not** a MongoDB.
- **restore target** — the MongoDB you restore *into*. Not in config; passed at restore time
  with `restore --target <uri>`.

```toml
default_profile = "prod"

[profiles.prod.source]
uri_env = "MONGO_URI"            # prod: env reference (config can leak — keep secrets out)
# uri = "mongodb://localhost:27017/?replicaSet=rs0"   # dev/no-secret: literal is fine
                                 # if both set, uri_env (when its env is present) wins
connect_timeout_secs = 5         # MongoDB connect/server-selection timeout (default 5s)
                                 # a serverSelectionTimeoutMS in the URI wins (warns if it differs)

[profiles.prod.destination]      # where backup FILES go — storage, not a MongoDB
type = "s3"                      # local | s3

[profiles.prod.destination.s3]
endpoint        = "https://s3.example.com"
bucket          = "db-backups"
prefix          = "mongo/prod"
region          = "ap-northeast-2"
credentials_env = "S3_CREDS"     # value format: "ACCESS_KEY:SECRET_KEY"

[profiles.prod.features.compression]
algorithm = "zstd"
level     = 10

[profiles.prod.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "/etc/x-backup/age.pub"   # public key only; keep the private key on the restore host

[profiles.prod.retention]        # prune defaults (CLI flags override these)
keep_last = 100                  # keep the newest 100 backups (per chain)
keep_days = 30                   # ...and anything from the last 30 days

# [profiles.prod.features.incremental]    # PostgreSQL only — opt in to logical-decoding incr/PITR
# pg_logical = true                       # needs server wal_level=logical (see PostgreSQL below)
```

Any value can be overridden by an `XB_`-prefixed environment variable (`XB_DESTINATION__S3__BUCKET=...`). Precedence is `CLI > ENV > config.toml > built-in default`.

### Multiple destinations

Back up to several places at once with `[[...destinations]]` (an array). The backup runs
once; the artifact is then replicated **byte-for-byte** to each destination, so every copy
has the same checksum and the same backup id — `verify`/`restore` work against any of them.

```toml
[[profiles.prod.destinations]]      # first entry = primary (required)
name = "local"
type = "local"
path = "/var/backups/mongo"

[[profiles.prod.destinations]]      # secondary (best-effort)
name = "offsite"
type = "s3"
[profiles.prod.destinations.s3]
endpoint        = "https://s3.example.com"
bucket          = "db-backups"
credentials_env = "S3_CREDS"
```

When `destinations` is non-empty it takes precedence over the single `destination`. The
policy is **primary required, the rest are warnings**: if the primary fails the backup
fails; if a secondary fails the backup still succeeds with exit 4 (warning) naming the
failed destination. Restore reads from the primary by default; `restore --from <name>`
picks a specific replica.

### Backup engine

Each profile picks how it reads and writes MongoDB with `mode.engine`. The default is
`native` — no external binaries needed.

```toml
[profiles.prod.mode]
engine = "native"     # native (default) | mongodump
```

| Engine | External tools | Archive format | What it captures | Use when |
|--------|---------------|----------------|------------------|----------|
| `native` (default) | none | `xb-native-v1` | data + indexes + collection options (capped, validator, collation, …) | the default — zero dependencies, single binary |
| `mongodump` | `mongodump` / `mongorestore` on PATH | mongodump `--archive` | whatever mongodump emits, plus consistent in-archive `--oplog` | you specifically want mongodump's archive or its in-dump oplog snapshot |

Both engines stream through the same compress → encrypt pipeline and record oplog
timestamps for chaining, so incremental/PITR work the same way. The engine that produced a
backup is recorded in the manifest (`tool_versions.archive_format`), and `restore` dispatches
automatically — a `native` archive is restored through the driver, a mongodump archive
through `mongorestore`. You can restore an old mongodump backup even after switching the
profile to `native`.

### PostgreSQL

Point a profile's `source.uri` at `postgresql://…` (or `postgres://…`) and x-backup uses its
PostgreSQL engine automatically — **no `pg_dump`/`pg_restore`**. It backs up through the
driver's COPY protocol (the same path those tools use internally), so it stays a single
self-contained binary.

```toml
[profiles.pg.source]
uri_env = "PG_URI"               # e.g. postgresql://user:pass@host:5432/mydb
[profiles.pg.destination]
type = "local"
path = "/var/backups/pg"

[profiles.pg.features.incremental]
pg_logical = true                # opt in to incremental/PITR via logical decoding
                                 # (requires server wal_level=logical)

[profiles.pg.retention]
keep_last = 100
keep_days = 30
```

All the DB-agnostic commands work the same as MongoDB:

```bash
x-backup backup  --profile pg                    # COPY-based full backup → compress → encrypt → store
x-backup backup  --profile pg --type incr        # logical-decoding increment (needs pg_logical = true)
x-backup restore --profile pg --target postgresql://host:5432/restored --force
x-backup restore --profile pg --target postgresql://host/restored --at latest --force   # PITR (latest = all)
x-backup status  --profile pg [--all] [--watch]  # version, db size, table/row counts, last backup; live Δ
x-backup peek    --profile pg [--ns schema.table]# eyeball data: per-table counts + latest rows
x-backup migrate --profile pg --target postgresql://host/other --drop --force   # driver COPY, PG → PG
x-backup list/verify/prune ...                    # manifest-based (DB-agnostic)
```

What it captures: **table data** (text COPY, exact values) and a broad slice of the schema —
**multiple schemas**, columns (incl. **`GENERATED … AS IDENTITY`** / **`… STORED`**),
**constraints** (PK/UNIQUE/FK/CHECK), **indexes**, **sequences** (serial + identity, full
parameters + value, reset to the data's max), **extensions**, **user-defined types**
(enum/domain/composite), **functions/procedures**, **triggers**, **views + materialized views**
(matviews populated `WITH DATA`), and **declarative partitioning** (parent `PARTITION BY` +
children `PARTITION OF`, multi-level; new rows route correctly after restore). Restore replays
pre-objects (extensions, types, functions) with dependency retry, recreates schemas + tables,
bulk-loads via COPY (generated columns recompute), applies constraints/indexes, resets sequences,
then replays views/matviews/triggers (also retry-ordered).

Connections use rustls TLS with `sslmode` negotiation — the default (`prefer`) tries TLS and
falls back to plaintext for servers without it, while `sslmode=require`/`verify-full` enforce
TLS. Data moves as text COPY (the portable format pg_dump uses), so restoring across
PostgreSQL major versions is safe; a major-version mismatch is logged as a warning.

Incremental backup and PITR work via **logical decoding** (not WAL archiving): opt in with the
server's `wal_level=logical` plus `[profiles.<name>.features.incremental] pg_logical = true`. A full
backup then creates a replication slot, `backup --type incr` captures the changes since, and
`restore --at <RFC3339>|latest` replays them up to the target time (`latest` replays everything).

Not yet covered (roadmap): ownership/grants, comments, aggregate/window functions, and user-defined
base/range types. Restoring into a non-empty database should use `--force` (drops and recreates each
backed-up table); an empty target needs no flag. `migrate` is PG → PG only (no cross-engine).

### Live monitor (`status --watch`)

`status --watch` turns the read-only check into a live dashboard, refreshing on an interval
(default 1s, `--interval <secs>`) and showing the **change since the last tick** (Δ) — green
for growth, red for shrink. It re-uses the driver connection between ticks and queries only
cheap metadata (`estimatedDocumentCount`, `dbStats`), so it's light on the server.

```bash
x-backup status --profile prod --watch                 # per-namespace doc counts + Δ, total size + Δ
x-backup status --all --watch --interval 2             # one row per profile, refreshed every 2s
x-backup status --profile prod --watch --count 5       # take 5 samples then exit (scripts/CI)
```

Pair it with `scripts/xb churn` to watch increments land in real time. `Ctrl-C` exits cleanly.

### Listing backups (`list`)

`list` shows the catalog for a profile, **newest-first** by default. The first line is the
store location it's reading from, and each backup carries a **DB** column (`postgresql`/`mongodb`)
alongside its type and chain status. Filter and sort it:

```bash
x-backup list --profile prod                     # newest-first catalog, with store location + DB column
x-backup list --profile prod --type incr         # only increments (full | incr | orphan)
x-backup list --profile pg   --engine pg         # only PostgreSQL backups (pg/mongo aliases ok)
x-backup list --profile prod --sort size         # biggest first (created | size; created is default)
x-backup list --profile prod --asc --limit 10    # oldest 10 (default order is descending)
x-backup list --profile prod --json              # machine-readable; includes a `store` field
```

`--sort` is `created` (default) or `size`; the default order is descending (newest/biggest first),
and `--asc` flips it. `--limit N` caps the rows after sorting/filtering.

### Pruning (`prune`)

`prune` deletes old backups by retention rule, always chain-safe — it works per chain, so a live
increment's base full backup is never deleted out from under it. Rules:

```bash
x-backup prune --profile prod --keep-last 100 --dry-run   # keep the newest 100 backups
x-backup prune --profile prod --keep-full 7 --force       # keep the newest 7 full chains
x-backup prune --profile prod --keep-days 30 --force      # keep anything from the last 30 days
```

`--keep-last N`, `--keep-full N`, and `--keep-days D` can be combined. When a CLI flag is absent,
`prune` falls back to the profile's `[profiles.<name>.retention]` (`keep_last` / `keep_full` /
`keep_days`) as the default; CLI flags override config. With no rule from either source, `prune`
deletes nothing and reports the error.

### Exit codes

| Code | Meaning |
|:---:|------|
| 0 | success |
| 1 | failure (work did not complete) |
| 2 | usage or config error |
| 3 | preflight failed (work never started) |
| 4 | success with a warning (e.g. gap → promoted to full) |
| 5 | lock conflict (another instance is running) |

To treat 4 as success in cron: `x-backup backup ...; rc=$?; [ $rc -eq 4 ] && rc=0; exit $rc`

## Restore semantics

- `restore` (no `--at`) restores the **base full backup snapshot only**.
- `restore --at <time>|latest` is PITR: it restores the base, then replays increments up to that time — MongoDB oplog (the largest ts at or before it) or PostgreSQL logical-decoding changes; `latest` replays everything. It requires `verify --chain` to pass, and it cannot be combined with `--only` (selective restore).
- `verify --deep` runs only on a host that holds the private key (key isolation, PRD §8.5). The backup host carries only the public key, so a compromised backup host still cannot decrypt past backups.

## Development

```bash
make help               # list all targets
make build              # release build
make build-debug        # debug build
make lint               # fmt --check + clippy -D warnings
make test               # unit + exit-code E2E, no DB needed
make mongodb-up         # two test replica sets (source :27017 + target :27117)
make test-integration   # Docker replica set integration tests
make test-s3            # MinIO S3 integration tests
make scenario           # E2E scenario (full → incr → verify → restore → PITR, 22 assertions)
make postgres-up        # PostgreSQL test server
make xbenv-pg           # isolated PostgreSQL test workspace (then: source <dir>/activate)
make xbenv-mongo        # isolated MongoDB test workspace
make xbenv-clean        # remove the isolated workspaces
```

### Manual testing against the containers

`scripts/xb` wraps the binary for hands-on testing against `make mongodb-up`. On first
run it creates `.devenv/` with a `config.toml` and an age keypair, then injects the right
env (`XB_CONFIG`, `MONGO_URI`, `XB_AGE_IDENTITY_FILE`), tools PATH, and `--profile`
automatically — so you don't wire any of that up by hand.

```bash
make mongodb-up           # start the containers
scripts/xb setup          # prepare .devenv + run a status check
scripts/xb seed           # deterministic baseline (drops, then inserts)
scripts/xb backup         # full backup (compressed + encrypted)
scripts/xb list
scripts/xb verify-latest  # structural + deep verify of the newest backup
scripts/xb restore-target # restore to the target (:27117) and print the doc counts
make devenv-down          # tear down containers + remove .devenv
```

To exercise **incremental** backups you need writes between backups, so the wrapper
has `churn`, which adds random documents to the source without dropping anything:

```bash
scripts/xb churn 100            # add 100 random docs (generates oplog)
scripts/xb backup --type incr   # captures them as an increment
scripts/xb list                 # full ← incr chain

scripts/xb incr-demo            # all of the above in one shot:
                                # seed → full → (churn → incr) ×2 → list
```

Any real x-backup subcommand passes straight through (`scripts/xb backup --type incr`,
`scripts/xb status --json`). To export the env and call `x-backup` directly instead:
`eval "$(scripts/xb env)"`. Plaintext backups: `XB_NO_ENCRYPT=1 scripts/xb setup`.

`scripts/xb` rebuilds automatically when sources changed (mtime check; no rebuild when
nothing changed), so you don't need `make build` after edits. Override with
`XB_BIN=<path>` or skip the check with `XB_NO_BUILD=1`.

The generated config has two profiles — `demo` (source `:27017`) and `target` (`:27117`) —
so you can check either side. `--profile` works in any position (the wrapper places it
correctly): `scripts/xb --profile target status` or `scripts/xb status --profile target`.

On a near-empty oplog (a fresh container), an increment may promote itself to a full
backup — that is the gap guard working, not an error. Churning data in first keeps the
oplog window healthy.

For isolated, throwaway test setups (a Python-venv-style model), `scripts/xbenv` creates a
self-contained workspace you `source <dir>/activate` into — it sets `XB_PROFILE`/`XB_CONFIG`
for that shell. `make xbenv-pg` / `make xbenv-mongo` spin one up per engine and `make
xbenv-clean` removes them (details in [docs/postgres.md](docs/postgres.md)).

## Docs

The docs are written in Korean.

| Document | Contents |
|------|------|
| [docs/PRD.md](docs/PRD.md) | Product requirements (FR-1–12, incremental design, encryption design) |
| [docs/test-scenario.md](docs/test-scenario.md) | E2E scenario definition |
| [docs/acceptance-report.md](docs/acceptance-report.md) | Acceptance criteria 10/10, with measured evidence |
| [docs/memory-profile.md](docs/memory-profile.md) | Memory ceiling measurement (constant RSS) |
| [docs/spike-oplog-archive.md](docs/spike-oplog-archive.md) | archive/oplog path spike, measured |
| [docs/ci.md](docs/ci.md) | CI setup and operations notes |

## Roadmap

GFS retention, Prometheus metrics, KMS/HSM key integration, live migration (oplog-tailing,
near-zero-downtime cutover) — see [PRD §12](docs/PRD.md).
