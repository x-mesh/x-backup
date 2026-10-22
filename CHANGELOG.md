# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[0.4.0]: https://github.com/x-mesh/x-backup/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/x-mesh/x-backup/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/x-mesh/x-backup/compare/v0.1.0...v0.2.0
