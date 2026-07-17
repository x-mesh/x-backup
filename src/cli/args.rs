//! clap derive 기반 CLI 인자 트리 — PRD §9 인터페이스 스케치에 1:1 대응.
//!
//! 서브커맨드: init / backup / restore / list / verify / prune / status.
//! 핸들러 자체는 후속 태스크가 구현하며, 여기서는 인자 표면(surface)만 정의한다.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// MongoDB 백업·복구 CLI.
#[derive(Debug, Parser)]
#[command(
    name = "x-backup",
    version,
    about = "MongoDB 백업·복구 CLI — 풀/증분(oplog), 로컬/원격(S3 호환), 암호화 중심",
    propagate_version = true
)]
pub struct Cli {
    /// config.toml 경로(미지정 시 `XB_CONFIG` 환경변수, 그다음 현재 디렉터리의
    /// `xbackup.toml`/`config.toml`/`.xbackup/config.toml`을 순서대로 자동 탐색).
    ///
    /// 어떤 config를 참조 중인지는 `status`/`doctor` 출력의 `config:` 줄에서 확인할 수 있다.
    /// 시크릿은 config에 평문 저장하지 않고 ENV 참조(uri_env 등)로 주입한다(FR-10).
    #[arg(long, global = true, value_name = "PATH", env = "XB_CONFIG")]
    pub config: Option<PathBuf>,

    /// 출력 설명 언어(en|ko). 라벨·기술용어는 항상 영문이며, 이 옵션은 설명·안내 문구에만
    /// 적용된다. 미지정 시 config `[output].language` → 기본 en.
    #[arg(long, global = true, value_enum, env = "XB_LANG")]
    pub lang: Option<crate::i18n::Lang>,

    /// 로그 상세도를 한 단계씩 올린다(-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// 실행할 서브커맨드.
    #[command(subcommand)]
    pub command: Command,
}

/// 백업 유형(full/incr) — PRD §FR-1/§FR-2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackupType {
    /// 풀 백업.
    Full,
    /// 증분 백업(oplog 기반).
    Incr,
}

/// 서브커맨드 트리 — PRD §9 스케치 그대로.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 대화형 마법사로 config.toml을 생성한다.
    Init(InitArgs),
    /// 백업을 실행한다(풀/증분).
    Backup(BackupArgs),
    /// 백업을 복구한다(풀/PITR/선택적).
    Restore(RestoreArgs),
    /// 가용 백업·증분 체인 카탈로그를 출력한다.
    List(ListArgs),
    /// 백업 무결성을 검증한다(구조/심층/체인).
    Verify(VerifyArgs),
    /// 보존 기준에 따라 오래된 백업을 안전 삭제한다.
    Prune(PruneArgs),
    /// 대상 서버 상태를 점검한다(읽기 전용·무부작용).
    Status(StatusArgs),
    /// config를 정적 점검한다(오프라인·전 프로파일·DB 연결 없음).
    Doctor(DoctorArgs),
    /// 데이터를 육안으로 확인한다 — 컬렉션별 문서 수 + 최신 문서(읽기 전용).
    Peek(PeekArgs),
    /// source→target으로 파일 없이 직접 마이그레이션한다(mongodump|mongorestore).
    Migrate(MigrateArgs),
    /// x-backup 자신을 최신 릴리스로 갱신한다(설치 소스 자동 감지).
    Update(UpdateArgs),
}

/// `migrate` — source(프로파일) → target으로 파일 없이 직접 복사.
///
/// 백업이 아니라 복사다 — manifest·체크섬·암호화·PITR는 만들지 않는다. 정확한 시점
/// 일관성/검증 가능한 백업본이 필요하면 `backup` → `restore --target`을 쓴다.
#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// 사용할 프로파일 이름(source 접속 정보).
    #[arg(long, value_name = "NAME", env = "XB_PROFILE")]
    pub profile: String,
    /// 대상(target) MongoDB URI. `--target-profile`과 택일(둘 중 하나 필수).
    #[arg(long, value_name = "MONGO_URI", conflicts_with = "target_profile")]
    pub target: Option<String>,
    /// 대상을 다른 프로파일의 source 접속으로 지정한다(URI 직접 입력 대신).
    /// 같은 config.toml 안의 프로파일 이름.
    #[arg(long, value_name = "NAME")]
    pub target_profile: Option<String>,
    /// 선택적 마이그레이션 — 특정 DB만.
    #[arg(long, value_name = "DB")]
    pub db: Option<String>,
    /// 선택적 마이그레이션 — 특정 컬렉션만. `--db`를 함께 지정해야 한다(H9 — 컬렉션은 한 DB에
    /// 속하므로, `--db` 없이 주면 모든 DB의 동명 컬렉션이 대상이 된다).
    #[arg(long, value_name = "COLL", requires = "db")]
    pub collection: Option<String>,
    /// target의 기존 컬렉션을 복원 전 drop한다(기본 비활성).
    #[arg(long)]
    pub drop: bool,
    /// target에 기존 데이터가 있어도 덮어쓰기를 허용한다(가드레일 해제).
    #[arg(long)]
    pub force: bool,
    /// 실제 전송 없이 계획만 출력(연결·버전·충돌 네임스페이스).
    #[arg(long)]
    pub dry_run: bool,
    /// 진행 표시를 억제하고 요약·경고·에러만 출력(cron/CI).
    #[arg(long, conflicts_with = "progress")]
    pub quiet: bool,
    /// 비-TTY에서도 진행 표시를 강제한다.
    #[arg(long)]
    pub progress: bool,
    /// 진행/결과를 기계 판독 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
}

/// `update` — 자기 갱신. brew 설치는 brew upgrade로 위임, manual 설치는
/// 릴리스 자산 다운로드 + sha256 검증 + 원자적 교체(gk 컨벤션).
#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// 최신 버전 확인만 하고 설치하지 않는다.
    #[arg(long)]
    pub check: bool,
}

/// `init` — 마법사로 config.toml 생성(FR-10, R18).
#[derive(Debug, Args)]
pub struct InitArgs {
    /// 기존 config.toml을 덮어쓴다.
    #[arg(long)]
    pub force: bool,
}

/// `backup` — PRD §9 backup 플래그 전체.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// 사용할 프로파일 이름.
    #[arg(long, value_name = "NAME", env = "XB_PROFILE")]
    pub profile: String,
    /// 백업 유형(full/incr). 미지정 시 config의 기본값을 따른다.
    #[arg(long = "type", value_enum)]
    pub backup_type: Option<BackupType>,
    /// 특정 데이터베이스만 백업(선택적 백업 — --oplog와 병용 불가, FR-1).
    #[arg(long, value_name = "DB")]
    pub db: Option<String>,
    /// 특정 컬렉션만 백업(선택적 백업). `--db`를 함께 지정해야 한다(H9 — 컬렉션은 한 DB에
    /// 속하므로, `--db` 없이 주면 native 엔진이 모든 DB의 동명 컬렉션을 과다 수집한다).
    #[arg(long, value_name = "COLL", requires = "db")]
    pub collection: Option<String>,
    /// 암호화를 끄고 평문으로 백업(명시적 opt-out, FR-5).
    #[arg(long)]
    pub no_encrypt: bool,
    /// 압축 레벨(미지정 시 config 기본값).
    #[arg(long, value_name = "N")]
    pub compress_level: Option<i32>,
    /// 진행 표시를 억제하고 요약·경고·에러만 출력(cron/CI).
    #[arg(long, conflicts_with = "progress")]
    pub quiet: bool,
    /// 비-TTY에서도 진행 표시를 강제한다.
    #[arg(long)]
    pub progress: bool,
    /// 진행/결과를 기계 판독 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
    /// 백업 전 사전 점검(status)을 건너뛴다.
    #[arg(long)]
    pub skip_precheck: bool,
    /// 생명주기 훅(pre/post/on_error)을 이번 실행에서 비활성화한다(PRD-04).
    #[arg(long)]
    pub no_hooks: bool,
}

/// `restore` — PRD §9 restore 플래그 전체.
#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// 사용할 프로파일 이름.
    #[arg(long, value_name = "NAME", env = "XB_PROFILE")]
    pub profile: String,
    /// 복구할 백업 ID. 미지정 시 최신 풀 백업을 자동 선택한다(FR-3).
    #[arg(long, value_name = "BACKUP_ID")]
    pub id: Option<String>,
    /// 복구 대상 DB URI(미지정 시 프로파일 source). 타깃 분리 복구용. `--target-profile`과 택일.
    #[arg(long, value_name = "DB_URI", conflicts_with = "target_profile")]
    pub target: Option<String>,
    /// 복구 대상을 다른 프로파일의 source 접속으로 지정한다(URI 직접 입력 대신).
    /// 같은 config.toml 안의 프로파일 이름. `--target`과 택일. 예: `--profile mongo --target-profile dr`.
    #[arg(long, value_name = "NAME")]
    pub target_profile: Option<String>,
    /// 어느 destination에서 읽을지(멀티 destination일 때). 이름 또는 `type#idx`.
    /// 미지정 시 primary(첫 destination).
    #[arg(long, value_name = "NAME")]
    pub from: Option<String>,
    /// PITR 목표 시점(RFC 3339 UTC wall-clock). 이하 최대 oplog ts로 내림 매핑(FR-3).
    #[arg(long, value_name = "TIMESTAMP")]
    pub at: Option<String>,
    /// 선택적 복구 대상(`db.collection`).
    #[arg(long, value_name = "DB.COLLECTION")]
    pub only: Option<String>,
    /// 기존 데이터 덮어쓰기를 허용(프로덕션 가드레일 해제, FR-3).
    #[arg(long)]
    pub force: bool,
    /// 실제 복원 없이 복구 계획만 출력(체인·대상·예상 크기·충돌).
    #[arg(long)]
    pub dry_run: bool,
    /// 복구 사전 점검을 건너뛴다.
    #[arg(long)]
    pub skip_precheck: bool,
    /// 진행 표시를 억제한다(cron/CI).
    #[arg(long, conflicts_with = "progress")]
    pub quiet: bool,
    /// 비-TTY에서도 진행 표시를 강제한다.
    #[arg(long)]
    pub progress: bool,
    /// 진행/결과를 기계 판독 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
    /// DB 서버로 복원하는 대신, 백업을 mongodump 레이아웃(`<db>/<coll>.bson` +
    /// `.metadata.json`)으로 이 디렉터리에 추출한다. 서버 없이 `mongorestore <DIR>`로 쓸 수
    /// 있다. native(기본) 풀 백업만 지원. `--target`/`--target-profile`/`--at`과 택일.
    #[arg(
        long = "to-dir",
        value_name = "DIR",
        conflicts_with_all = ["target", "target_profile", "at"]
    )]
    pub to_dir: Option<PathBuf>,
}

/// list 정렬 기준.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ListSort {
    /// 생성 시각(=id, UUID v7). 기본 — 최신이 위.
    #[default]
    Created,
    /// 저장 크기.
    Size,
}

/// `list` — 카탈로그 출력(FR-7, R22).
#[derive(Debug, Args)]
pub struct ListArgs {
    /// 대상 프로파일(미지정 시 default_profile).
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// 정렬 기준(created|size). 기본 created.
    #[arg(long, value_enum, default_value_t = ListSort::Created)]
    pub sort: ListSort,
    /// 오름차순으로 정렬(기본은 내림차순 — 최신/큰 것이 위).
    #[arg(long)]
    pub asc: bool,
    /// 유형 필터(full|incr|orphan).
    #[arg(long = "type", value_name = "T")]
    pub type_filter: Option<String>,
    /// DB 엔진 필터(postgresql|mongodb; pg/mongo 약어 허용).
    #[arg(long, value_name = "E")]
    pub engine: Option<String>,
    /// 출력 개수 제한(정렬·필터 후 상위 N개).
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// 결과를 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
}

/// `verify` — 무결성 검증(FR-7, R12/R13).
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// 검증할 백업 ID.
    #[arg(long, value_name = "BACKUP_ID")]
    pub id: String,
    /// 심층 검증 — 복호화·압축해제 디코드 확인(개인키 필요, §8.5).
    #[arg(long)]
    pub deep: bool,
    /// 체인 검증 — base+증분 체인 연속성(gap 없음) 확인(PITR 전제).
    #[arg(long)]
    pub chain: bool,
    /// 결과를 기계 판독 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
}

/// `prune` — 보존 관리 안전 삭제(FR-11, R19).
#[derive(Debug, Args)]
pub struct PruneArgs {
    /// 사용할 프로파일 이름.
    #[arg(long, value_name = "NAME", env = "XB_PROFILE")]
    pub profile: String,
    /// 최근 풀백업 N개를 보존한다.
    #[arg(long, value_name = "N")]
    pub keep_full: Option<u32>,
    /// 최근 D일치 백업을 보존한다.
    #[arg(long, value_name = "D")]
    pub keep_days: Option<u32>,
    /// 최신 백업 N벌을 보존한다(체인 단위 누적). 미지정 시 config retention.keep_last.
    #[arg(long, value_name = "N")]
    pub keep_last: Option<u32>,
    /// 복구 보장 윈도우(일) — 지난 N일 임의 시점 복구를 보장한다(경계 base까지 보존, PRD-02).
    #[arg(long, value_name = "N")]
    pub recovery_window_days: Option<u32>,
    /// 최소 이중화 — 어떤 규칙이든 최소 M개 풀 체인은 남긴다(PRD-02).
    #[arg(long, value_name = "M")]
    pub min_redundancy: Option<u32>,
    /// 실제 삭제 없이 삭제 대상 목록만 출력한다.
    #[arg(long)]
    pub dry_run: bool,
    /// 대화형 확인 없이 삭제를 진행한다.
    #[arg(long)]
    pub force: bool,
}

/// `status` — 대상 서버 상태 점검(FR-8, R14).
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// 사용할 프로파일 이름. 생략하면 config의 `default_profile`로 폴백한다(`--all`이면 전체).
    /// `--profile`/`XB_PROFILE`/`default_profile`이 모두 없을 때만 오류다(list/backup과 일관).
    #[arg(long, value_name = "NAME", env = "XB_PROFILE")]
    pub profile: Option<String>,
    /// config의 모든 프로파일을 한 번에 점검한다(한 줄 요약 + 최악 exit code).
    /// `--profile`/`XB_PROFILE`보다 우선한다(둘 다 있으면 --all로 동작 — 워크스페이스
    /// 활성(XB_PROFILE) 중에도 `status --all`이 충돌 없이 동작하도록).
    #[arg(long)]
    pub all: bool,
    /// 결과를 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
    /// 라이브 모드 — 주기적으로 갱신하며 변경량(Δ)을 추적한다(Ctrl-C 종료).
    #[arg(long)]
    pub watch: bool,
    /// watch 갱신 주기(초). 기본 1.0.
    #[arg(long, value_name = "SECS", default_value_t = 1.0, requires = "watch")]
    pub interval: f64,
    /// watch를 N회 갱신 후 종료(0=무한, 기본 0). 스크립트·테스트용.
    #[arg(long, value_name = "N", default_value_t = 0, requires = "watch")]
    pub count: u64,
    /// ns별(컬렉션/테이블) 문서 수를 함께 출력한다(읽기 전용 — 신호등 점검과 별개 섹션).
    /// mongo는 사용자 컬렉션, PG는 사용자 테이블을 센다. 연결 실패 프로파일은 건너뛴다.
    #[arg(long = "ns-detail")]
    pub ns_detail: bool,
}

/// `doctor` — config 정적 점검(오프라인). DB 연결 없이 모든 프로파일의 설정 문제를 찾는다.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// 특정 프로파일만 점검(미지정 시 config의 모든 프로파일).
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// 결과를 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
}

/// `peek` — 데이터 육안 확인(읽기 전용). 컬렉션별 문서 수 + 최신 문서.
#[derive(Debug, Args)]
pub struct PeekArgs {
    /// 사용할 프로파일 이름.
    #[arg(long, value_name = "NAME", env = "XB_PROFILE")]
    pub profile: String,
    /// 특정 네임스페이스만(`db.collection`). 지정 시 그 컬렉션의 최신 N건을 보여준다.
    #[arg(long, value_name = "DB.COLLECTION")]
    pub ns: Option<String>,
    /// 보여줄 최신 문서 수(`--ns` 지정 시 적용, 기본 1).
    #[arg(short = 'n', long, value_name = "N", default_value_t = 1)]
    pub limit: i64,
    /// 결과를 JSON으로 출력한다.
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap 정의가 내부적으로 일관되는지(중복 플래그·잘못된 구성 없음) 검증한다.
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// 7개 서브커맨드가 모두 등록되어 있는지 확인한다.
    #[test]
    fn all_subcommands_present() {
        let cmd = Cli::command();
        let names: Vec<_> = cmd.get_subcommands().map(|c| c.get_name()).collect();
        for expected in [
            "init", "backup", "restore", "list", "verify", "prune", "status",
        ] {
            assert!(names.contains(&expected), "서브커맨드 누락: {expected}");
        }
    }

    /// backup 서브커맨드가 핵심 플래그를 파싱하는지 검증한다.
    #[test]
    fn backup_parses_core_flags() {
        let cli = Cli::try_parse_from([
            "x-backup",
            "backup",
            "--profile",
            "prod",
            "--type",
            "incr",
            "--no-encrypt",
            "--quiet",
        ])
        .expect("파싱 실패");
        match cli.command {
            Command::Backup(args) => {
                assert_eq!(args.profile, "prod");
                assert_eq!(args.backup_type, Some(BackupType::Incr));
                assert!(args.no_encrypt);
                assert!(args.quiet);
            }
            other => panic!("backup이 아님: {other:?}"),
        }
    }

    /// --quiet와 --progress는 상호 배타임을 확인한다.
    #[test]
    fn quiet_and_progress_conflict() {
        let result = Cli::try_parse_from([
            "x-backup",
            "backup",
            "--profile",
            "p",
            "--quiet",
            "--progress",
        ]);
        assert!(result.is_err(), "quiet+progress는 충돌해야 함");
    }

    /// H9 — backup `--collection`은 `--db` 없이 쓸 수 없다(컬렉션은 한 DB에 속하므로
    /// `--db` 없으면 native 엔진이 모든 DB의 동명 컬렉션을 과다 수집함).
    #[test]
    fn backup_collection_requires_db() {
        // --collection만 → 거부(usage 오류, exit 2).
        assert!(
            Cli::try_parse_from([
                "x-backup",
                "backup",
                "--profile",
                "p",
                "--collection",
                "users"
            ])
            .is_err(),
            "--collection은 --db 없이 거부되어야 함"
        );
        // --db + --collection → 통과.
        assert!(
            Cli::try_parse_from([
                "x-backup",
                "backup",
                "--profile",
                "p",
                "--db",
                "app",
                "--collection",
                "users",
            ])
            .is_ok(),
            "--db와 함께면 통과해야 함"
        );
        // --db 단독 → 통과(DB만 선택적 백업은 정상).
        assert!(
            Cli::try_parse_from(["x-backup", "backup", "--profile", "p", "--db", "app"]).is_ok(),
            "--db 단독은 통과해야 함"
        );
    }

    /// H9 — migrate `--collection`도 동일하게 `--db`를 요구한다.
    #[test]
    fn migrate_collection_requires_db() {
        assert!(
            Cli::try_parse_from([
                "x-backup",
                "migrate",
                "--profile",
                "p",
                "--target",
                "mongodb://t/db",
                "--collection",
                "users",
            ])
            .is_err(),
            "migrate --collection은 --db 없이 거부되어야 함"
        );
        assert!(
            Cli::try_parse_from([
                "x-backup",
                "migrate",
                "--profile",
                "p",
                "--target",
                "mongodb://t/db",
                "--db",
                "app",
                "--collection",
                "users",
            ])
            .is_ok(),
            "migrate --db와 함께면 통과해야 함"
        );
    }

    /// restore는 `--target`(URI)와 `--target-profile`(프로파일 이름)을 동시에 줄 수 없다(택일).
    #[test]
    fn restore_target_and_target_profile_conflict() {
        // 둘 다 → 거부.
        assert!(
            Cli::try_parse_from([
                "x-backup",
                "restore",
                "--profile",
                "mongo",
                "--target",
                "mongodb://t/db",
                "--target-profile",
                "dr",
            ])
            .is_err(),
            "--target과 --target-profile 동시 지정은 거부되어야 함"
        );
        // --target-profile 단독 → 통과.
        assert!(
            Cli::try_parse_from([
                "x-backup",
                "restore",
                "--profile",
                "mongo",
                "--target-profile",
                "dr",
            ])
            .is_ok(),
            "--target-profile 단독은 통과해야 함"
        );
    }
}
