# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-10-29

### Added

- Backup compression accepts `compress = "gzip:N"` (level 1 to 9) and `compress = "none"`.
  `backup --no-compress` turns compression off for one run. Restore reads gzip backups.
- Full backups of MariaDB/MySQL, PostgreSQL and MongoDB (native engine) record the row
  count of each table or collection in the manifest (`table_rows`). `list` shows a `ROWS`
  column, and `list --json` has `total_rows`.
- `verify --against <profile|uri>` compares the row counts in a backup with a target
  database. It reports `MISMATCH`, `MISSING` and `EXTRA` and exits with code 1 on a
  difference. `--tables` limits the comparison. For MongoDB, `--estimate` uses
  `estimatedDocumentCount`.
- `verify --against --content` compares content digests. A MariaDB/MySQL full backup
  records a digest for each table by default (`--no-content-digest` turns it off). A
  MongoDB native full backup records a digest for each collection with
  `backup --content-digest`. A table with the same row count and different values is
  reported as `CONTENT`.
- `restore` compares the row counts of the backup with the target database after the
  restore. A difference gives a warning and exit code 4. `--no-verify-rows` turns the
  check off.
- The backup progress line and the summary show rows, tables (collections), the largest
  tables and the elapsed time. The `--json` output has `rows_total`, `table_rows` and
  `elapsed_secs`.
- A full backup compares its row counts with the previous full backup in the same
  destination. If a table disappears or loses rows above `--warn-drop-percent` (default
  50), the backup gives a warning and exit code 4. `--json` has `rows_vs_previous`.

### Fixed

- A PostgreSQL backup sets `DateStyle = ISO`, `extra_float_digits = 3` and
  `IntervalStyle = postgres` for its session. Before this change, server defaults such as
  `DateStyle = SQL, DMY` or a low `extra_float_digits` changed dates and rounded floats in
  the dump, and the restore completed without an error.
- `install.sh` and `x-backup update` read the latest release tag from the
  `releases/latest` redirect. Without a token, they no longer use the GitHub API, so the
  API rate limit no longer stops an install or an update.
- Control characters in a table name are replaced with `?` in the backup progress line
  and the summary.

## [0.6.1] - 2026-10-27

### Fixed

- `verify` failed with `destination.type is not set (local is required)` for a profile
  with several destinations (`[[profile.x.dest]]`). It now reads the primary destination,
  and `verify --from <NAME>` selects another one.

## [0.6.0] - 2026-10-26

### Added

- `x-backup config`: a terminal editor for the profiles in `config.toml`. Use the arrow
  keys to select a profile, edit its fields, review the changes, run a connection test and
  save. Each field opens with its current value. The editor uses the same field rules and
  save path as the web `/config` page: a save runs the `doctor` checks, keeps `.bak.N`
  copies of the previous file and replaces the file atomically. The connection test
  checks the draft and does not write the file.
- `list --from <NAME>` and `prune --from <NAME>` select one destination of a profile that
  has several destinations. Without `--from`, they use the primary destination.
- `doctor` and the config editor warn when two or more profiles use the same destination.
  The backup path and the manifest do not record the profile, so the incremental base,
  `prune` and the backup list of these profiles mix.

### Changed

- The web `/config` page shows one row per profile with its `doctor` result. A profile
  name opens a detail page at `/config/profile/{name}`. `/doctor` redirects to `/config`,
  and the Setup menu no longer has a Doctor entry.
- A plain `uri` or `read_uri` can contain `user:pass@` credentials. The web pages, the
  audit log and the editor show the value masked.

### Fixed

- `x-backup serve` stops on Ctrl+C or SIGTERM when a browser holds an SSE stream open.
  Other connections get up to 10 seconds, and a second signal stops the server at once.
- `list` and `prune` failed with `destination.type is not set (local is required)` for a
  profile with several destinations (`[[profile.x.dest]]`). They now read the primary
  destination.

## [0.5.0] - 2026-10-22

### Added

- A file backup engine. A `file://` source is packed as a tar stream and goes through the
  same compress, encrypt and store pipeline as a database backup.
- Incremental file backups. A snapshot index records each run; the next run stores the
  changed files plus a tombstone list for deleted paths. `restore --id <incremental>`
  replays the chain in order and needs `verify --chain` to pass. If the head index is
  lost, the run is promoted to a full backup and exits with code 4.
- `x-backup daemon`: a foreground scheduler that runs `backup` for each profile that sets
  `schedule` (five-field cron, local time zone). `--dry-run` (with `--json`) prints the next
  fire times, and `--print-systemd` prints a unit file. A failed run is logged and
  reported, and the loop continues. A run missed while the daemon was down is not made up.
- Webhook notifications. `notify.webhook_url_env` names the environment variable that
  holds the URL, so the URL is never stored in the config.

### Changed

- PITR replays the MongoDB oplog through the driver (`applyOps`). `restore --at` no longer
  needs `mongorestore`.
- The `mongodump` engine is now behind the `legacy-mongodump` Cargo feature. A default
  build has no code that starts `mongodump` or `mongorestore`, `engine = "mongodump"` is
  a configuration error, and a backup in the mongodump archive format restores only with a
  build that enables the feature.
- `x-backup update` on a Homebrew install prints the `brew upgrade` command and does not
  run it. The token lookup reads `GITHUB_TOKEN` and `GH_TOKEN` only; it no longer calls
  `gh auth token`.
- The CI no-subprocess guard allows process spawning in the web console and the lifecycle
  hooks, which start processes by design. It still blocks new spawn points elsewhere.
- Engine detection is now one function (`DbKind::from_archive_format`), and the full
  backup paths share one storage core.

## [0.4.0] - 2026-09-28

### Added

- `x-backup serve`: a web console for running backups, restores and the rest from a
  browser. It has no backup logic of its own — every action runs the CLI as
  `x-backup <command> --json` and relays the result to the page and over SSE, so the
  console and the terminal cannot disagree. Screens: Dashboard, Monitor, Catalog, Backup,
  Verify, Peek, Restore, Prune, Migrate, Schedule, Jobs, Lock, Config and Doctor.
  It binds to loopback by default, refuses to start without an access token
  (`XB_WEB_TOKEN_FILE` with mode 0600, or `XB_WEB_TOKEN`), keeps secrets out of pages and
  logs, and puts restore, prune and migrate behind an audit log and a confirmation step.
  Operations guide: `docs/web-console.ko.md`.
- A backup history heatmap at the top of the Dashboard: one row per profile, one cell per
  UTC day (as many days as fit the panel, up to 63), the worst outcome of the day in each
  cell. Successes come from
  the destination's manifests; failures come from the console's own job history, so a
  backup that failed under external cron shows as "no record", not as a failure.
- `status --watch --json`, which emits one NDJSON frame per refresh. The console's
  Monitor screen shares a single such child across all viewers.
- `LICENSE` (MIT). `Cargo.toml` has declared `license = "MIT"` since its first commit,
  but the file itself was never there.
- `Cargo.toml` metadata: `repository`, `homepage`, `readme`, `keywords`, `categories`.
- Terminal demos in the README, recorded with [VHS](https://github.com/charmbracelet/vhs):
  a full backup and the increment that follows it, the `status` preflight, and
  `verify --deep --chain`. The tapes and their setup script live in `docs/assets/`, so
  the recordings can be reproduced rather than only replaced.

### Fixed

- Linux-only failures that never showed up on macOS: `CARGO_BIN_EXE_*` handling in tests,
  an `ETXTBSY` race when spawning a freshly written binary, and platform differences in
  `statvfs` and `c_char`. The MSRV CI job now really builds with Rust 1.88.
- `scripts/release.sh` builds the linux musl targets on Apple Silicon hosts: it pins the
  cross images to an Ubuntu 24.04 base, runs them under amd64 emulation, and stops early
  when the linux toolchain that cross mounts is missing.
- Output no longer mixes languages under `[output].language = "en"`. Two things leaked
  Korean regardless of the setting: the error-kind prefix, which was baked into each
  variant's `#[error("작업 실패: {0}")]` and therefore fixed at compile time, and fifteen
  `tracing` log lines written as Korean literals. The prefix moved out of `Display` into
  `XBackupError::kind_label(lang)`, which the printer in `main` applies, and the log lines
  now go through a `tr!` macro that formats only the language in use. Error message
  *bodies* are still Korean — that is the whole error surface and has not been touched.

### Changed

- prune checks chains with `ChainVerifier`, measured 2,109 times faster than before on a
  catalog of 8,000 backups.
- The docs no longer assume a private repository. `README.md`, `README.ko.md`,
  `install.sh`, the release skill, and the `x-backup update` doc comments all opened by
  telling the reader to set a GitHub token, which no longer buys anything. `update`
  still picks up a token when one is set, but only to dodge the rate limit on
  unauthenticated GitHub API calls.
- `install.sh` downloads straight from the public release URL. The `gh` and API-token
  paths existed to reach assets in a private repo, so they are gone and the script is
  half its former length.
- CI pins `dtolnay/rust-toolchain` to a commit SHA rather than `@master`, which moves.
- The README doc tables now link `docs/postgres.md` and `docs/prd/`. Both files were
  already in the repo; nothing pointed at them.

### Removed

- `install`, a byte-identical copy of `install.sh` that nothing referenced. Use
  `install.sh`.

## [0.3.0] - 2026-09-02

### Added

- **MySQL / MariaDB engine**: full backup (`SHOW CREATE` + `SELECT`), binlog ROW-based
  incremental backup, and PITR replay through the native `mysql_async` driver — no
  external CLI tools. Wired into `backup`, `restore`, `peek`, `status`, `list`,
  `migrate`, and `doctor`, with a `mysql_binlog` feature flag and manifest field.
- **Lifecycle hooks** (`pre_backup` / `post_backup` / `on_error`): run shell commands
  around a backup. A non-zero `pre_*` exit stops the operation; `post_*` and `on_error`
  warn only. Hooks receive `XB_*` context environment variables, run under a timeout
  with process-group kill, and get secret environment variables removed plus
  credential URIs masked. Configure with `[profiles.*.hooks]` (v1) or `hook_*` (v2);
  `--no-hooks` disables them.
- **Recovery-window retention**: `recovery_window_days` keeps every chain inside the
  window plus the boundary base backup, and `min_redundancy` keeps the last N complete
  chains. Both combine with `keep_full` / `keep_days` / `keep_last` as a union.
  CLI: `--recovery-window-days`, `--min-redundancy`.
- **Standby reads**: `source.read_uri` / `read_uri_env` (v1) and `read_uri` (v2) move
  backup reads to a replica and reduce primary load. Precedence: `--read-source`,
  then config `read_uri`, then the primary source.
- **`status` recovery point**: reports `recoverable_until` and the RPO gap from the
  catalog. `--json` exposes it as `items[key=recoverable]`.
- **v2 flat config schema** with automatic detection, inheritance precedence, and a
  v1 migration mapping (`docs/`).
- **i18n**: `--lang en|ko` global flag selects the output description language.
- **`restore --to-dir`**: extract a native full backup into a `mongodump` layout
  without a target server. Supports `--only`, dry-run, and JSON output.
- **`restore --target-profile`** selects the restore target by profile.
- **`init`**: auto-generate an age keypair when the recipient file is missing (0600
  private key), and a connection wizard that accepts environment variable names or
  direct URIs.
- **`status --ns-detail`** shows per-namespace document counts.
- Makefile targets for install/uninstall, version bump, tag, and release. `xbenv`
  MySQL workspaces, a Docker PostgreSQL target instance, and an `ensure-db` command
  that recreates a missing database.

### Fixed

- MySQL restore: report a missing target database instead of a generic driver error.
- Hook secret scrub covers `read_uri_env` (replica credentials).
- The `status` recovery point reads `Complete` backups only. An incomplete latest
  backup no longer over-reports the recoverable time.
- A read-source URI that does not resolve degrades to a warning. It no longer fails
  `status`, `list`, `restore`, and `prune` together.
- Secret masking handles an unencoded `/` inside a password.
- A hook timeout kills the whole process group, not the direct child only (unix).
- PostgreSQL connection errors chain their source, so details such as "database does
  not exist" stay visible.
- CI: restore the formatting and doctest checks.

### Changed

- `status` no longer requires `--profile`. It falls back to `default_profile`, like
  `list` and `backup`.
- Config auto-discovery also finds `config.toml`, the default output of `init`.
- `list` and the backup picker mark gap-promoted full backups (`promoted_from_gap`).
- The `status` version check treats `mongodump` as optional for native engines.

## [0.2.0] - 2026-06-20

### Added

- **PostgreSQL engine**: full and incremental backup plus PITR via logical decoding
  (`pgoutput` decoder, replication slots, `restore --at`).
- **Native driver engines** for MongoDB and PostgreSQL — backup/restore/status run
  through the drivers with no external CLI tools (`mongodump`/`pg_dump`) required.
- **Multi-destination fan-out backup** (sequential), with `restore --from <destination>`
  and an `restore --id` fuzzy backup picker (dialoguer).
- **`status`**: source↔target diff view (`--all`), `--watch` live change-delta monitor,
  destination write probe, and data-shape/last-backup/FCV/clock checks.
- **`list` / `prune`**: sorting, filter, `--limit`, store-location column, multi-DB
  awareness, and config-driven retention (`keep_last`).
- **`migrate`**: native engine, direct source→target migration (no intermediate file),
  `--target-profile`, and a dry-run detail table.
- **Crypto**: age and AES-GCM encryption.
- **PostgreSQL object coverage**: declarative partitioning (parent/child, multi-level),
  functions/procedures + triggers, extensions, user-defined types, views/matviews,
  and sequence parameters.
- **`doctor`** diagnostics and dev workspace tooling (`xbenv` isolated test workspaces,
  `scripts/xb` wrapper, source-change auto-rebuild).
- PostgreSQL integration tests and CI jobs (logical-decoding E2E).

### Fixed

- PostgreSQL incremental apply correctness: `OVERRIDING SYSTEM VALUE`, `$n::text` cast
  generalization, column-name mapping, and sequence re-sync.
- Skip + warn (instead of silent data loss) on keyless UPDATE/DELETE during incremental.
- `migrate`: require `--drop` for a non-empty target (merge footgun).
- Numerous adversarial-review fixes across integrity, security, recovery-failure paths,
  TLS, portability, and sequence handling.

### Changed

- README defaults to English; Korean documentation split into `README.ko.md`.

[0.5.0]: https://github.com/x-mesh/x-backup/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/x-mesh/x-backup/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/x-mesh/x-backup/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/x-mesh/x-backup/compare/v0.1.0...v0.2.0
