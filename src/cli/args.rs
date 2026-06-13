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
    /// config.toml 경로(미지정 시 기본 탐색 경로 사용).
    ///
    /// 시크릿은 config에 평문 저장하지 않고 ENV 참조(uri_env 등)로 주입한다(FR-10).
    #[arg(long, global = true, value_name = "PATH", env = "XB_CONFIG")]
    pub config: Option<PathBuf>,

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
    /// x-backup 자신을 최신 릴리스로 갱신한다(설치 소스 자동 감지).
    Update(UpdateArgs),
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
    #[arg(long, value_name = "NAME")]
    pub profile: String,
    /// 백업 유형(full/incr). 미지정 시 config의 기본값을 따른다.
    #[arg(long = "type", value_enum)]
    pub backup_type: Option<BackupType>,
    /// 특정 데이터베이스만 백업(선택적 백업 — --oplog와 병용 불가, FR-1).
    #[arg(long, value_name = "DB")]
    pub db: Option<String>,
    /// 특정 컬렉션만 백업(선택적 백업).
    #[arg(long, value_name = "COLL")]
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
}

/// `restore` — PRD §9 restore 플래그 전체.
#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// 사용할 프로파일 이름.
    #[arg(long, value_name = "NAME")]
    pub profile: String,
    /// 복구할 백업 ID. 미지정 시 최신 풀 백업을 자동 선택한다(FR-3).
    #[arg(long, value_name = "BACKUP_ID")]
    pub id: Option<String>,
    /// 복구 대상 MongoDB URI(미지정 시 프로파일 source). 타깃 분리 복구용.
    #[arg(long, value_name = "MONGO_URI")]
    pub target: Option<String>,
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
}

/// `list` — 카탈로그 출력(FR-7, R22).
#[derive(Debug, Args)]
pub struct ListArgs {
    /// 대상 프로파일(미지정 시 default_profile).
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
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
    #[arg(long, value_name = "NAME")]
    pub profile: String,
    /// 최근 풀백업 N개를 보존한다.
    #[arg(long, value_name = "N")]
    pub keep_full: Option<u32>,
    /// 최근 D일치 백업을 보존한다.
    #[arg(long, value_name = "D")]
    pub keep_days: Option<u32>,
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
    /// 사용할 프로파일 이름.
    #[arg(long, value_name = "NAME")]
    pub profile: String,
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
}
