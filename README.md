<p align="center">
  <strong>English</strong> · <a href="README.ko.md">한국어</a>
</p>

# x-backup

> MongoDB backup and restore CLI — full and incremental (oplog) backups, PITR, local or S3-compatible storage, encrypted by default. A single Rust binary.

x-backup backs up a running MongoDB (standalone or replica set) into an encrypted form you can verify and actually restore from. It drives `mongodump` and `mongorestore` and streams every stage, so memory stays flat no matter how large the dataset is. A 6 GiB backup peaks at 54.6 MiB RSS ([measured](docs/memory-profile.md)).

## Features

- ✅ **Full backup** — streamed `mongodump --archive --oplog`, point-in-time consistent
- ✅ **Incremental backup** — captures the oplog directly; on a gap it promotes to a full backup automatically (exit 4)
- ✅ **PITR** — restore to a moment with `--at <RFC3339>` (base restore + oplog replay, gated on chain verification)
- ✅ **Storage** — local disk or S3-compatible (MinIO, R2, OCI), streaming multipart upload with abort cleanup
- ✅ **Encrypted by default** — `age` (X25519; only the public key lives on the backup host) or AES-256-GCM, compressed with zstd before encryption
- ✅ **Integrity** — manifest + sha256, with `verify` (structural check, no key needed), `--deep`, and `--chain`
- ✅ **Operations** — `status` preflight (traffic-light summary), chain-safe `prune`, concurrent-run locking, a defined exit-code contract (0–5)
- ✅ **Headless** — auto-quiet when not a TTY, `--json` output, built for cron and CI

Scope: replica sets get full and incremental backups, standalone gets full only, and sharded clusters are detected and refused (out of scope).

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

You need `mongodump` and `mongorestore` (MongoDB Database Tools 100.x) on your PATH. `make tools` installs them locally under `.tools/` with sha256 verification.

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
x-backup status  --profile prod                 # is the server ready to back up?
x-backup backup  --profile prod                 # full backup → compress → encrypt → store
x-backup backup  --profile prod --type incr     # oplog increment
x-backup list    --profile prod                 # catalog, with chain status
x-backup verify  --id <backup-id>               # structural check, no key required
x-backup restore --profile prod --target mongodb://staging --dry-run
x-backup restore --profile prod --target mongodb://staging --force
x-backup restore --profile prod --at 2026-06-01T00:00:00Z --force   # PITR
x-backup prune   --profile prod --keep-full 7 --dry-run
x-backup migrate --profile prod --target mongodb://newcluster --force # direct copy, no file
```

### Migrate (direct copy, no file)

`migrate` copies one MongoDB straight into another — `mongodump | mongorestore` streamed
directly, no intermediate file. Use it for one-off moves where you don't need a stored,
verifiable backup.

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
  a no-`--drop` copy would be a half-merge (mongorestore inserts; same-`_id` docs are kept,
  stale docs remain) — almost never what a migration wants. With `--drop`, each collection
  **present in the source** is dropped and recreated; other collections in the target are
  left alone. The target database/instance is never fully dropped.
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
```

Any value can be overridden by an `XB_`-prefixed environment variable (`XB_DESTINATION__S3__BUCKET=...`). Precedence is `CLI > ENV > config.toml > built-in default`.

### Multiple destinations

Back up to several places at once with `[[...destinations]]` (an array). mongodump runs
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
- `restore --at <time>` is PITR: it restores the base, then replays incremental oplog up to that time (the largest ts at or before it). It requires `verify --chain` to pass, and it cannot be combined with `--only` (selective restore), a `mongorestore` limitation.
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
make postgres-up        # PostgreSQL, for the upcoming adapter
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

PostgreSQL adapter (next), GFS retention, Prometheus metrics, KMS/HSM key integration,
live migration (oplog-tailing, near-zero-downtime cutover) — see [PRD §12](docs/PRD.md).
