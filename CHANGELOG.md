# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.2.0]: https://github.com/x-mesh/x-backup/compare/v0.1.0...v0.2.0
