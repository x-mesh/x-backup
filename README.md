<p align="center">
  <strong>English</strong> · <a href="README.ko.md">한국어</a>
</p>

# x-backup

> MongoDB, PostgreSQL & MySQL backup and restore CLI — full and incremental backups, PITR, local or S3-compatible storage, encrypted by default. A single Rust binary, **no external dump tools**.

x-backup backs up a running MongoDB (standalone or replica set), PostgreSQL, or MySQL into an encrypted form you can verify and actually restore from. By default it talks to the database directly through the Rust driver — **no `mongodump`/`mongorestore`, `pg_dump`/`pg_restore`, or `mysqldump`/`mysql` required** — and streams every stage, so memory stays flat no matter how large the dataset is. A 6 GiB backup peaks at 54.6 MiB RSS ([measured](docs/memory-profile.md)). The database is chosen automatically from the source URI scheme (`mongodb://`, `postgresql://`, or `mysql://`/`mariadb://`).

## Features

- ✅ **Full backup** — driver-native streaming archive (data + indexes + collection options), no external tools; `mongodump --archive --oplog` available as an opt-in engine
- ✅ **Incremental backup** — captures the oplog directly; on a gap it promotes to a full backup automatically (exit 4)
- ✅ **PITR** — restore to a moment with `--at <RFC3339>|latest` (base restore + replay of MongoDB oplog or PostgreSQL logical-decoding changes, gated on chain verification)
- ✅ **Storage** — local disk or S3-compatible (MinIO, R2, OCI), streaming multipart upload with abort cleanup
- ✅ **Encrypted by default** — `age` (X25519; only the public key lives on the backup host) or AES-256-GCM, compressed with zstd before encryption
- ✅ **Integrity** — manifest + sha256, with `verify` (structural check, no key needed), `--deep`, and `--chain`
- ✅ **Operations** — `doctor` offline config check (all profiles, no DB connection), `status` preflight (connection, topology, privileges, version/FCV, clock skew, oplog window, data shape, **last backup age**, **destination writability + free space**), `--all` source-vs-target diff, `--watch` live monitor, chain-safe `prune`, concurrent-run locking, a defined exit-code contract (0–5)
- ✅ **PostgreSQL** — driver-native full backup/restore via the COPY protocol (data + tables + constraints + indexes + sequences), no `pg_dump`/`pg_restore`. Same pipeline (compress → encrypt → store), same `status`/`list`/`verify`/`restore`
- ✅ **MySQL** — driver-native full backup/restore via `mysql_async` (data + DDL — tables, views, triggers, routines, events), no `mysqldump`/`mysql`. Same pipeline (compress → encrypt → store), same `status`/`list`/`verify`/`restore`. Incremental and PITR via binlog ROW streaming (opt-in).
- ✅ **Headless** — auto-quiet when not a TTY, `--json` output, built for cron and CI

Scope: MongoDB replica sets get full and incremental backups, standalone gets full only, and sharded clusters are detected and refused. PostgreSQL gets full backup, restore, migrate, status/peek/watch, plus incremental backup and PITR via logical decoding (opt-in). See [PostgreSQL](#postgresql). MySQL gets the same command set (full/restore/status/peek/migrate), plus incremental backup and PITR via binlog ROW streaming (opt-in, requires `log_bin=ROW` on the server). See [MySQL](#mysql).

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

- **source** — the database you back up (your prod). `uri_env` (an env var name) for anything
  with credentials; `uri` (a literal) is fine for local/no-secret connections.
- **destination** — where backup *files* go. This is storage (`local` / `s3`), **not** a database.
- **restore target** — the database you restore *into*. Not in config; passed at restore time
  with `restore --target <uri>`.

x-backup reads **two config formats** and auto-detects which one a file uses:

- **v2 (recommended)** — one flat `[profile.<name>]` table per profile, with shared policy
  factored out into `[defaults]` (applied to every profile) and `[base.<name>]` + `extends`
  (reusable, opt-in). Compact one-liners for the common cases (`dest`, `compress`, `encrypt`).
- **v1 (still supported)** — the original deeply-nested layout
  (`[profiles.<name>.features.encryption]`, …). Existing v1 configs keep loading unchanged.

**Detection:** a singular `[profile]`/`[defaults]`/`[base]` table means v2; a plural
`[profiles]` table means v1. **Mixing the two in one file is an error** — pick one.

#### v2 (recommended)

The whole prod profile above, in v2 — shared compression/encryption live once in `[defaults]`:

```toml
default_profile = "prod"

[output]
language = "ko"                  # description/help language (en | ko); labels stay English

[defaults]                       # applied to every profile (lowest precedence)
compress = "zstd:10"             # algorithm[:level]
encrypt  = "age:/etc/x-backup/age.pub"   # "age:<public-key-path>" | true | false | "off"

[profile.prod]
uri_env = "MONGO_URI"            # prod: env reference (config can leak — keep secrets out)
# uri = "mongodb://localhost:27017/?replicaSet=rs0"   # dev/no-secret: literal is fine
                                 # if both set, uri_env (when its env is present) wins
prefer_secondary     = true      # back up from a secondary when possible
connect_timeout_secs = 5         # connect/server-selection timeout (default 5s)
dest      = "s3:db-backups/mongo/prod"   # "local:/path" | "s3:bucket/prefix"
dest_name = "central-s3"
s3_region = "ap-northeast-2"
s3_endpoint = "https://s3.example.com"
s3_creds  = "S3_CREDS"           # env var NAME holding "ACCESS_KEY:SECRET_KEY"
keep_last = 100                  # retention: keep the newest 100 backups (per chain)
keep_days = 30                   # ...and anything from the last 30 days

[profile.pg]
uri_env    = "PG_URI"            # postgresql:// → PostgreSQL engine auto-selected
dest       = "local:/var/backups/pg"
pg_logical = true                # PostgreSQL incremental/PITR (needs server wal_level=logical)
```

`[profile.pg]` inherits `compress`/`encrypt` from `[defaults]`; it does **not** repeat them.

#### v2 — inheritance (`[defaults]`, `[base.<name>]`, `extends`)

Factor shared policy out once and override per profile. Precedence, low → high:

```
[defaults]  <  extends chain (left→right, later wins)  <  the profile's own keys
```

`extends` references a `[base.<name>]` (reusable, not selectable as a profile) or another
`[profile.<name>]`; it takes a string or an array (`extends = ["a", "b"]`, where `b` overrides
`a`). For endpoint-only target profiles, **emit no `dest`** (they stay restore/migrate targets)
and override the inherited encryption with `encrypt = false`:

```toml
default_profile = "mongo"

[defaults]
compress = "zstd:6"
encrypt  = "age:/etc/x-backup/age.pub"

[base.s3-central]                # reusable destination policy, not a profile itself
dest      = "s3:db-backups"
s3_region = "ap-northeast-2"
s3_creds  = "S3_CREDS"

[profile.mongo]                  # backup job: source + dest + inherited policy
uri  = "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true"
extends   = "s3-central"
s3_prefix = "mongo"

[profile.mongo-target]           # endpoint-only: source only, no dest, encryption off
uri     = "mongodb://localhost:27117/?replicaSet=rs0&directConnection=true"
encrypt = false

[profile.pg]
uri        = "postgres://xbackup:xbackup-dev@localhost:5432/app"
dest       = "local:/var/backups/pg"
pg_logical = true

[profile.pg-target]              # endpoint-only
uri     = "postgres://xbackup:xbackup-dev@localhost:5433/app_restore"
encrypt = false
```

#### v2 — full key reference

Every key below is a **flat** key inside `[profile.<name>]` (or `[defaults]` / `[base.<name>]`).
The normalizer rewrites them into the v1 nested tree, so behaviour is identical.

| Flat key(s) | Maps to | Notes |
|-------------|---------|-------|
| `uri`, `uri_env` | `source.uri` / `source.uri_env` | `uri_env` holds an env var **name** (secret) |
| `prefer_secondary`, `connect_timeout_secs` | `source.*` | |
| `backup_type` | `mode.backup_type` | `full` (default) \| `incr` |
| `output_mode` | `mode.output` | `progress` (default) \| `quiet` |
| `precheck` | `mode.precheck` | default `true` |
| `engine` | `mode.engine` | `native` (default) \| `mongodump` |
| `dest = "local:/path"` \| `"s3:bucket/prefix"` | `destination.{type,path}` or `destination.s3.{bucket,prefix}` | compact single destination |
| `dest_name` | `destination.name` | |
| `s3_bucket`, `s3_prefix`, `s3_region`, `s3_endpoint`, `s3_creds` | `destination.s3.{bucket,prefix,region,endpoint,credentials_env}` | `s3_creds` is an env var **name** |
| `[[profile.x.dest]]` (array of tables) | `destinations[]` | multi-destination; see below |
| `compress = "zstd:6"` | `features.compression.{algorithm,level}` | or `compress_algorithm` + `compress_level` |
| `encrypt = "age:/path"` \| `true` \| `false` \| `"off"` | `features.encryption.{enabled,algorithm,recipient_file}` | or `encrypt_algorithm` + `recipient_file`; `recipient_file` is a public-key **path** |
| `incr_interval`, `incr_on_gap`, `pg_logical`, `mysql_binlog` | `features.incremental.{interval,on_gap,pg_logical,mysql_binlog}` | |
| `keep_full`, `keep_days`, `keep_last` | `retention.{keep_full,keep_days,keep_last}` | |
| `extends = "name"` \| `["a","b"]` | (inheritance) | references a base or profile; later wins |

Omittable defaults (emitters can leave them out): `backup_type=full`, `output_mode=progress`,
`precheck=true`, `engine=native`, compression `zstd`/level `10`, encryption `enabled=true`+`age`,
incremental `interval=15m`/`on_gap=promote_full`/`pg_logical=false`, `prefer_secondary=false`.

A richer v2 example — S3 + retention + per-profile overrides:

```toml
default_profile = "prod"

[defaults]
compress = "zstd:10"
encrypt  = "age:/etc/x-backup/age.pub"

[base.s3-central]
dest      = "s3:db-backups"
s3_region = "ap-northeast-2"
s3_endpoint = "https://s3.ap-northeast-2.amazonaws.com"
s3_creds  = "S3_CREDS"

[profile.prod]
extends     = "s3-central"
uri_env     = "MONGO_PROD_URI"
prefer_secondary = true
s3_prefix   = "mongo/prod"
incr_interval = "15m"
incr_on_gap = "promote_full"
keep_last   = 30
keep_days   = 14

[profile.pg-prod]
extends     = "s3-central"
uri_env     = "PG_PROD_URI"
s3_prefix   = "pg/prod"
pg_logical  = true
keep_last   = 30
keep_days   = 14
```

#### v1 (still supported)

The original nested layout still loads unchanged — no migration required:

```toml
default_profile = "prod"

[profiles.prod.source]
uri_env = "MONGO_URI"            # prod: env reference (config can leak — keep secrets out)
# uri = "mongodb://localhost:27017/?replicaSet=rs0"   # dev/no-secret: literal is fine
                                 # if both set, uri_env (when its env is present) wins
connect_timeout_secs = 5         # connect/server-selection timeout (default 5s)
                                 # a serverSelectionTimeoutMS in the URI wins (warns if it differs)

[profiles.prod.destination]      # where backup FILES go — storage, not a database
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

Any value can be overridden by an `XB_`-prefixed environment variable
(`XB_DESTINATION__S3__BUCKET=...`). **ENV overrides always use the v1 nested dot-path**,
regardless of which format the file uses — they target the normalized tree, not the v2 flat
keys. Precedence is `CLI > ENV > config.toml > built-in default`.

### Multiple destinations

Back up to several places at once with an array of destination tables. The backup runs
once; the artifact is then replicated **byte-for-byte** to each destination, so every copy
has the same checksum and the same backup id — `verify`/`restore` work against any of them.

In v2, use `[[profile.<name>.dest]]` (each entry takes the compact `dest = "..."` form or
explicit `type`/`path`/`name`/`s3_*` keys):

```toml
[profile.prod]
uri_env = "MONGO_URI"

[[profile.prod.dest]]               # first entry = primary (required)
name = "local"
dest = "local:/var/backups/mongo"

[[profile.prod.dest]]               # secondary (best-effort)
name = "offsite"
dest = "s3:db-backups"
s3_endpoint = "https://s3.example.com"
s3_creds    = "S3_CREDS"
```

The v1 equivalent uses `[[profiles.<name>.destinations]]`:

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
[profile.prod]
engine = "native"     # native (default) | mongodump     (v2 flat key → mode.engine)

# v1 equivalent:
# [profiles.prod.mode]
# engine = "native"
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
[profile.pg]                     # v2: one flat table
uri_env    = "PG_URI"            # e.g. postgresql://user:pass@host:5432/mydb
dest       = "local:/var/backups/pg"
pg_logical = true                # opt in to incremental/PITR via logical decoding
                                 # (requires server wal_level=logical)
keep_last  = 100
keep_days  = 30
```

<details><summary>v1 equivalent</summary>

```toml
[profiles.pg.source]
uri_env = "PG_URI"
[profiles.pg.destination]
type = "local"
path = "/var/backups/pg"

[profiles.pg.features.incremental]
pg_logical = true

[profiles.pg.retention]
keep_last = 100
keep_days = 30
```

</details>

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
server's `wal_level=logical` plus `pg_logical = true` (v2: a flat key on `[profile.<name>]`;
v1: `[profiles.<name>.features.incremental] pg_logical = true`). A full
backup then creates a replication slot, `backup --type incr` captures the changes since, and
`restore --at <RFC3339>|latest` replays them up to the target time (`latest` replays everything).

Not yet covered (roadmap): ownership/grants, comments, aggregate/window functions, and user-defined
base/range types. Restoring into a non-empty database should use `--force` (drops and recreates each
backed-up table); an empty target needs no flag. `migrate` is PG → PG only (no cross-engine).

### MySQL

Point a profile's `source.uri` at `mysql://…` (or `mariadb://…`) and x-backup uses its
MySQL engine automatically — **no `mysqldump`/`mysql`**. It backs up through `mysql_async`,
a pure-Rust driver, so it stays a single self-contained binary. The database name is required
in the URI (`mysql://user:pass@host:3306/dbname`).

```toml
[profile.mysql]                  # v2: one flat table
uri_env      = "MYSQL_URI"       # e.g. mysql://root:pass@127.0.0.1:3306/mydb
dest         = "local:/var/backups/mysql"
mysql_binlog = true              # opt in to incremental/PITR via binlog ROW streaming
                                 # (requires log_bin=ON, binlog_format=ROW, binlog_row_image=FULL on server)
keep_last    = 100
keep_days    = 30
```

<details><summary>v1 equivalent</summary>

```toml
[profiles.mysql.source]
uri_env = "MYSQL_URI"
[profiles.mysql.destination]
type = "local"
path = "/var/backups/mysql"

[profiles.mysql.features.incremental]
mysql_binlog = true

[profiles.mysql.retention]
keep_last = 100
keep_days = 30
```

</details>

All the DB-agnostic commands work the same as MongoDB and PostgreSQL:

```bash
x-backup backup  --profile mysql                   # SELECT-streaming full backup → compress → encrypt → store
x-backup backup  --profile mysql --type incr       # binlog ROW increment (needs mysql_binlog = true)
x-backup restore --profile mysql --target mysql://host:3306/restored --force
x-backup restore --profile mysql --target mysql://host/restored --at latest --force   # PITR (latest = all)
x-backup status  --profile mysql [--all] [--watch] # version, db size, table/row counts, replica status
x-backup peek    --profile mysql                   # eyeball data: per-table counts + latest rows
x-backup migrate --profile mysql --target mysql://host/other --drop --force   # driver streaming, MySQL → MySQL
x-backup list/verify/prune ...                     # manifest-based (DB-agnostic)
```

What it captures: **table data** (SELECT streaming, exact values) and a broad slice of the schema —
**tables** (`SHOW CREATE TABLE`), **views**, **triggers**, **stored procedures**, **functions**, and
**events** (`SHOW CREATE …`). Byte columns are encoded as `0x` hex; JSON columns are preserved as-is.
`STORED` generated columns are excluded from INSERT (the server recomputes them). Invisible columns
are included. Tables are dumped in FK dependency order with `FOREIGN_KEY_CHECKS=0`. `AUTO_INCREMENT`
values are preserved. Views, triggers, routines, and events are applied after data; `DEFINER` clauses
are stripped for portability. The restore session opens with `FOREIGN_KEY_CHECKS=0`,
`UNIQUE_CHECKS=0`, `SQL_MODE='NO_AUTO_VALUE_ON_ZERO'`, `time_zone='+00:00'`, `NAMES utf8mb4`.

The full backup records the binlog coordinates at snapshot time (`file:pos` + `gtid_executed`) in
the manifest, so incremental chains can anchor to it.

Incremental backup and PITR work via **binlog ROW streaming** (not server-side logical decoding):
opt in with `mysql_binlog = true` (v2: a flat key on `[profile.<name>]`; v1:
`[profiles.<name>.features.incremental] mysql_binlog = true`). A full backup then records the
binlog position, `backup --type incr` captures ROW events (WriteRows/UpdateRows/DeleteRows) since,
and `restore --at <RFC3339>|latest` replays them up to the target time (`latest` replays everything).
PITR timestamp granularity is **1 second** (binlog event header resolution) — for an exact cut,
prefer binlog file:position or GTID over a timestamp. Gap detection: if the base binlog file has
been purged from the server, the increment is refused and promoted to a full backup (exit 4).

Server requirements for incremental: `log_bin=ON`, `binlog_format=ROW`, `binlog_row_image=FULL`,
`binlog_row_metadata=FULL` (MySQL 8.0.1+), `gtid_mode=ON` (recommended), a unique `server_id`,
and a MySQL account with `REPLICATION SLAVE` + `REPLICATION CLIENT` privileges. Full
backup/restore/status/peek work on any MySQL 8.0+ or MariaDB 10.x without these requirements.

Known limitations: FLOAT/DOUBLE use the server's text representation (mysqldump parity — not
guaranteed 10-decimal lossless); the consistent snapshot does not isolate concurrent DDL (InnoDB
only); per-object `sql_mode` for views/triggers is not reproduced. MySQL 8.4 renames
`SHOW MASTER STATUS` → `SHOW BINARY LOG STATUS`; the engine handles both.
`migrate` is MySQL → MySQL only (no cross-engine). See [docs/mysql.md](docs/mysql.md).

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
`prune` falls back to the profile's retention defaults (`keep_last` / `keep_full` / `keep_days`
— v2 flat keys, or `[profiles.<name>.retention]` in v1); CLI flags override config. With no rule from either source, `prune`
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
make mysql-up           # MySQL test servers (source :3306 + target :3307)
make test-mysql         # MySQL engine unit tests (no DB needed)
make scenario-mysql     # MySQL E2E (full → incr → restore → PITR)
make xbenv-mysql        # isolated MySQL test workspace (then: source <dir>/activate)
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
| [docs/mysql.md](docs/mysql.md) | MySQL engine deep-dive (schema fidelity, binlog internals, PITR, dev/CI) |
| [docs/PRD.md](docs/PRD.md) | Product requirements (FR-1–12, incremental design, encryption design) |
| [docs/test-scenario.md](docs/test-scenario.md) | E2E scenario definition |
| [docs/acceptance-report.md](docs/acceptance-report.md) | Acceptance criteria 10/10, with measured evidence |
| [docs/memory-profile.md](docs/memory-profile.md) | Memory ceiling measurement (constant RSS) |
| [docs/spike-oplog-archive.md](docs/spike-oplog-archive.md) | archive/oplog path spike, measured |
| [docs/ci.md](docs/ci.md) | CI setup and operations notes |

## Roadmap

GFS retention, Prometheus metrics, KMS/HSM key integration, live migration (oplog-tailing,
near-zero-downtime cutover) — see [PRD §12](docs/PRD.md).
