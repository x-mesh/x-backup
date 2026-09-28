//! 잡 명세 — "무엇을 실행할지"를 서술하는 타입.
//!
//! ## 이 파일이 보안 경계인 이유 — 플래그 이름은 절대 사용자 입력이 아니다
//! 옵션 주입을 막는 가장 강한 방법은 검증이 아니라 **어휘를 닫는 것**이다. 이 파일에서
//! argv의 플래그 이름은 전부 `&'static str` 리터럴이고, 그 리터럴을 고르는 것은
//! [`JobFlag`]·[`JobCount`]처럼 **닫힌 enum**뿐이다. 웹에서 들어온 문자열이 플래그
//! 이름 위치에 놓일 경로가 문법적으로 존재하지 않는다.
//!
//! 사용자 입력은 오직 **값 위치**에만 도달하고, 값 위치로 가는 모든 문은
//! [`super::args`]의 검증 함수를 지난다. 그래서 이 두 파일을 합치면 다음이 성립한다:
//!
//! ```text
//! 웹 입력 ──▶ args::validate_*  ──▶ Opt::Valued(리터럴 플래그, 검증된 값) ──▶ argv
//!                (거부 가능)              ▲
//!                                         └── 플래그 이름은 여기서만 나온다(리터럴)
//! ```
//!
//! `Opt`는 비공개다 — 이 모듈 밖에서는 임의의 플래그/값 쌍을 만들 방법이 없고, 오직
//! `with_*` 빌더를 거쳐야 한다.
//!
//! ## 웹은 접속 URI를 인자로 받지 않는다
//! CLI에는 `restore --target <URI>`·`migrate --target <URI>`·`backup --read-source <URI>`가
//! 있지만 이 어휘에는 **없다.** 두 가지 이유다:
//!
//! 1. argv는 같은 사용자 권한이면 `/proc/<pid>/cmdline`으로 읽힌다. 이 repo는 이미 그
//!    이유로 URI를 argv에 싣지 않는다 — mongodump에도 URI를 argv 대신 0600 임시 config
//!    파일로 넘긴다([`crate::engine`]의 dump 경로). 웹이 URI를 argv에 실으면 그 정책이
//!    웹 경로에서만 깨진다.
//! 2. URI에는 비밀번호가 들어간다. 감사 로그에는 argv가 남으므로(t8의 `args_masked`),
//!    URI를 인자로 받으면 마스킹이 유일한 방어선이 된다 — 마스킹은 2차 방어일 뿐이다
//!    ([`crate::web::mask`] 헤더).
//!
//! 대신 `--target-profile`(config에 이미 있는 프로파일 이름)로 대상을 지정한다. 새 대상을
//! 쓰려면 config에 프로파일을 추가하는 것이 정상 경로다 — 그러면 접속 정보는 config와
//! env 참조에 남고 argv에는 이름만 남는다.
//!
//! ## `--json`은 선택이 아니다
//! [`JobSpec::to_argv`]는 항상 마지막에 `--json`을 붙인다. 웹은 자식의 stdout을 파싱해
//! 진행률·결과를 중계하므로(t11), 사람용 출력이 섞이면 파싱이 깨진다. "붙이는 것을 잊을
//! 수 있는 플래그"로 두지 않고 argv 생성 자체에 박아 넣어 잊을 수 없게 한다.
//!
//! ## 파괴적 판정은 명령 단위다(플래그를 보지 않는다)
//! [`JobSpec::is_destructive`]는 `--dry-run`이 붙었는지 보지 **않는다.** restore/prune/
//! migrate는 플래그와 무관하게 파괴적으로 취급하고, 항상 감사 게이트를 요구한다. 근거:
//! 플래그 조합에 따라 안전 등급이 바뀌는 로직은 버그가 살기 가장 좋은 자리이고, 과다
//! 기록은 안전한 방향의 실패다("prod 복구를 누군가 시도했다"는 기록은 dry-run이었어도
//! 감사관이 보고 싶은 사실이다). 반대 방향 — 파괴적 작업이 기록 없이 실행되는 것 — 은
//! [`crate::web::audit`]가 존재하는 이유 자체를 무너뜨린다.

use crate::i18n::Lang;
use crate::web::job::args::{
    validate_backup_id, validate_collection_name, validate_db_name, validate_destination_ref,
    validate_namespace, validate_timestamp, ProfileName,
};
use crate::web::mask::SecretRegistry;

/// 웹 콘솔이 자식으로 띄울 수 있는 서브커맨드.
///
/// `init`(대화형 마법사)·`update`(자기 갱신)·`serve`(이 서버 자신)는 의도적으로 없다.
/// init은 stdin 대화가 필요하고(자식 stdin은 `/dev/null`이다 — [`super::runner`] 헤더),
/// update는 실행 중인 바이너리를 교체하므로 상주 서버가 자기 발밑을 바꾸게 되고,
/// serve는 서버가 서버를 낳는 재귀다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCommand {
    /// `backup` — 백업 실행(풀/증분).
    Backup,
    /// `restore` — 복구(풀/PITR/선택적). **파괴적.**
    Restore,
    /// `verify` — 무결성 검증(읽기 전용).
    Verify,
    /// `prune` — 보존 기준에 따른 삭제. **파괴적.**
    Prune,
    /// `migrate` — source→target 직접 복사. **파괴적**(target에 쓴다).
    Migrate,
    /// `status` — 대상 서버 상태 점검(읽기 전용).
    Status,
    /// `list` — 백업 카탈로그(읽기 전용).
    List,
    /// `peek` — 데이터 육안 확인(읽기 전용).
    Peek,
    /// `doctor` — config 정적 점검(오프라인·읽기 전용).
    Doctor,
}

impl JobCommand {
    /// argv 첫 토큰(서브커맨드 이름).
    pub fn verb(self) -> &'static str {
        match self {
            Self::Backup => "backup",
            Self::Restore => "restore",
            Self::Verify => "verify",
            Self::Prune => "prune",
            Self::Migrate => "migrate",
            Self::Status => "status",
            Self::List => "list",
            Self::Peek => "peek",
            Self::Doctor => "doctor",
        }
    }

    /// 데이터를 되돌릴 수 없게 바꾸는 명령인지.
    ///
    /// - `restore`: 대상 DB의 기존 데이터를 덮어쓴다.
    /// - `prune`: 백업 산출물을 지운다 — 잘못 지우면 복구 자체가 불가능해진다.
    /// - `migrate`: target DB에 쓴다(`--drop`이면 기존 컬렉션을 지운다).
    ///
    /// `backup`은 파괴적이 아니다 — 새 산출물을 만들 뿐 기존 것을 바꾸지 않는다. 다만
    /// 락을 잡고 소스에 부하를 주므로, 감사 기록 자체는 [`super::runner`]가 모든 잡에
    /// 대해 남긴다(게이트를 요구하는 것과 기록을 남기는 것은 다른 문제다).
    pub fn is_destructive(self) -> bool {
        matches!(self, Self::Restore | Self::Prune | Self::Migrate)
    }

    /// 감사 로그 `action` 필드 값(t8 [`crate::web::audit::AuditEvent::action`]).
    ///
    /// `<verb>.run` 고정 어휘를 쓴다 — 감사 로그는 사람이 아니라 `grep`/스크립트가 훑는
    /// 것을 전제하므로(t8 헤더), 명령마다 표기가 흔들리지 않게 한다.
    pub fn audit_action(self) -> &'static str {
        match self {
            Self::Backup => "backup.run",
            Self::Restore => "restore.run",
            Self::Verify => "verify.run",
            Self::Prune => "prune.run",
            Self::Migrate => "migrate.run",
            Self::Status => "status.run",
            Self::List => "list.run",
            Self::Peek => "peek.run",
            Self::Doctor => "doctor.run",
        }
    }
}

/// 값을 받지 않는 플래그의 닫힌 어휘.
///
/// 모든 플래그가 모든 명령에 유효하지는 않다(`--deep`은 verify만). 조합의 유효성은
/// **CLI의 clap이 판정한다** — 여기서 다시 검사하면 CLI 인자 표면을 이중 구현하게 되고,
/// 두 구현이 갈라지는 순간 웹이 CLI와 다르게 동작한다([`crate::web`] 최상위 불변식).
/// 잘못된 조합은 자식이 exit 2로 끊고, [`super::runner::JobOutcome::Rejected`]로
/// 분류된다 — 조용히 다르게 실행되는 일은 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobFlag {
    /// `--dry-run` — 실제 변경 없이 계획만.
    DryRun,
    /// `--force` — 가드레일 해제(restore 덮어쓰기, prune 비대화형 승인, migrate 덮어쓰기).
    Force,
    /// `--deep` — verify 심층 검증(복호화·압축해제).
    Deep,
    /// `--chain` — verify 체인 연속성 검증.
    Chain,
    /// `--all` — status 전 프로파일 점검.
    All,
    /// `--skip-precheck` — 사전 점검 생략.
    SkipPrecheck,
    /// `--no-hooks` — 생명주기 훅 비활성.
    NoHooks,
    /// `--no-encrypt` — 암호화 없이 백업(명시적 opt-out).
    NoEncrypt,
    /// `--drop` — migrate 시 target 기존 컬렉션 drop.
    Drop,
    /// `--ns-detail` — status에 ns별 문서 수 포함.
    NsDetail,
    /// `--asc` — list 오름차순.
    Asc,
    /// `--watch` — status 라이브 모드(끝나지 않는 스트림).
    Watch,
}

impl JobFlag {
    /// argv에 들어가는 플래그 문자열.
    fn as_arg(self) -> &'static str {
        match self {
            Self::DryRun => "--dry-run",
            Self::Force => "--force",
            Self::Deep => "--deep",
            Self::Chain => "--chain",
            Self::All => "--all",
            Self::SkipPrecheck => "--skip-precheck",
            Self::NoHooks => "--no-hooks",
            Self::NoEncrypt => "--no-encrypt",
            Self::Drop => "--drop",
            Self::NsDetail => "--ns-detail",
            Self::Asc => "--asc",
            Self::Watch => "--watch",
        }
    }
}

/// 정수 값을 받는 옵션의 닫힌 어휘.
///
/// 값이 [`u32`]이므로 주입 표면이 아예 없다 — 문자열이 아니라 숫자를 받고, argv 문자열은
/// 우리가 [`u32::to_string`]으로 만든다. `--compress-level`(음수 zstd 레벨 가능)은 이
/// 어휘에 넣지 않았다: 값이 `-5`처럼 `-`로 시작해 옵션 주입 방어([`super::args`] 헤더)와
/// 정면으로 부딪히고, 압축 레벨은 config가 정하는 것이 정상 경로다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobCount {
    /// `--keep-full <N>`
    KeepFull,
    /// `--keep-days <D>`
    KeepDays,
    /// `--keep-last <N>`
    KeepLast,
    /// `--recovery-window-days <N>`
    RecoveryWindowDays,
    /// `--min-redundancy <M>`
    MinRedundancy,
    /// `--limit <N>` — list 출력 개수, peek 문서 수.
    Limit,
    /// `--interval <SECS>` — `status --watch` 갱신 주기(초).
    ///
    /// 정수만 받는다. CLI는 소수도 받지만(`--interval 0.5`) 웹 콘솔이 초 단위 미만으로
    /// 프로덕션을 두드릴 이유가 없고, `JobCount`의 어휘를 정수로 유지하는 편이
    /// "웹이 만들 수 있는 argv" 표면을 좁게 유지한다.
    Interval,
    /// `--count <N>` — `status --watch`를 N회 갱신 후 종료(0=무한).
    Count,
}

impl JobCount {
    /// argv에 들어가는 플래그 문자열.
    fn as_arg(self) -> &'static str {
        match self {
            Self::KeepFull => "--keep-full",
            Self::KeepDays => "--keep-days",
            Self::KeepLast => "--keep-last",
            Self::RecoveryWindowDays => "--recovery-window-days",
            Self::MinRedundancy => "--min-redundancy",
            Self::Limit => "--limit",
            Self::Interval => "--interval",
            Self::Count => "--count",
        }
    }
}

/// argv 한 조각. **비공개** — 임의의 플래그를 만들 경로를 막는다(모듈 헤더 참조).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Opt {
    /// 값 없는 플래그.
    Flag(&'static str),
    /// `<플래그> <값>` 쌍. 값은 이미 검증을 통과했다.
    Valued(&'static str, String),
}

impl Opt {
    /// 이 조각의 플래그 이름(중복 판정용).
    fn arg(&self) -> &'static str {
        match self {
            Self::Flag(a) | Self::Valued(a, _) => a,
        }
    }
}

/// 실행할 잡 하나의 완전한 명세.
///
/// 빌더는 전부 `self`를 소비해 돌려주므로 체인으로 쓴다:
///
/// ```
/// use x_backup::i18n::Lang;
/// use x_backup::web::job::{JobCommand, JobFlag, JobSpec, ProfileName};
///
/// // `lang`은 검증이 거부할 때의 **문구**만 정한다 — argv는 이 값과 무관하다.
/// let spec = JobSpec::new(JobCommand::Backup, Lang::En)
///     .with_profile(ProfileName::parse("prod", Lang::En).unwrap())
///     .with_flag(JobFlag::SkipPrecheck);
/// assert_eq!(
///     spec.to_argv(),
///     vec!["backup", "--profile", "prod", "--skip-precheck", "--json"]
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    command: JobCommand,
    profile: Option<ProfileName>,
    opts: Vec<Opt>,
    /// 검증이 거부할 때 쓸 설명 언어.
    ///
    /// **argv에는 영향을 주지 않는다** — 자식에게 넘길 인자는 이 값과 무관하다. 여기 있는
    /// 이유는 `with_*`가 사용자 입력을 거부할 때 그 문장이 화면에 그대로 렌더되기 때문이다
    /// (`args` 모듈 헤더 "오류 문구는 ko/en 둘 다 낸다"). 빌더마다 언어를 되풀이해 넘기는
    /// 대신 명세를 여는 자리에서 한 번만 정한다 — 한 명세 안에서 언어가 갈릴 이유가 없다.
    lang: Lang,
}

impl JobSpec {
    /// 명령만 정한 빈 명세. `lang`은 검증 실패 문구에만 쓰인다([`JobSpec::lang`] 필드 문서).
    #[must_use]
    pub fn new(command: JobCommand, lang: Lang) -> Self {
        Self {
            command,
            profile: None,
            opts: Vec::new(),
            lang,
        }
    }

    /// 이 명세의 명령.
    pub fn command(&self) -> JobCommand {
        self.command
    }

    /// 이 명세의 프로파일(지정된 경우).
    pub fn profile(&self) -> Option<&ProfileName> {
        self.profile.as_ref()
    }

    /// 파괴적 명령인지 — 모듈 헤더 "파괴적 판정은 명령 단위다" 참조.
    pub fn is_destructive(&self) -> bool {
        self.command.is_destructive()
    }

    /// 이 명세가 `--dry-run`을 들고 있는가.
    ///
    /// 파괴적 명령이라도 `--dry-run`이 붙으면 자식은 **아무것도 바꾸지 않고 계획만 낸다**.
    /// 그 사실을 확인할 수 있어야 [`crate::web::job::JobRunner::spawn_preview`]가 "계획을
    /// 보여주려고 감사 게이트를 통과해야 하는" 모순을 피할 수 있다(그 함수 doc).
    ///
    /// argv를 직접 훑는다 — 플래그를 따로 기록해 두면 `with_flag`가 쌓는 argv와 그 기록이
    /// 갈릴 수 있고, 그러면 이 판정이 **실제로 실행될 argv와 다른 것**을 보게 된다.
    /// 여기서는 그 어긋남이 곧 "안 지운다고 믿고 지우는" 사고다.
    pub fn has_dry_run(&self) -> bool {
        let needle = JobFlag::DryRun.as_arg();
        self.opts.iter().any(|opt| match opt {
            Opt::Flag(name) => *name == needle,
            Opt::Valued(..) => false,
        })
    }

    /// `--profile <NAME>`. 이미 검증된 [`ProfileName`]만 받는다.
    #[must_use]
    pub fn with_profile(mut self, profile: ProfileName) -> Self {
        self.push(Opt::Valued("--profile", profile.as_str().to_string()));
        self.profile = Some(profile);
        self
    }

    /// `--target-profile <NAME>` — 복구/마이그레이션 대상을 config의 다른 프로파일로.
    /// 원시 URI를 받지 않는 이유는 모듈 헤더 참조.
    #[must_use]
    pub fn with_target_profile(mut self, profile: ProfileName) -> Self {
        self.push(Opt::Valued(
            "--target-profile",
            profile.as_str().to_string(),
        ));
        self
    }

    /// 값 없는 플래그를 켠다. 같은 플래그를 두 번 켜도 argv에는 한 번만 나간다
    /// (clap은 같은 boolean 플래그의 중복 등장을 사용법 오류로 볼 수 있어, 웹의 실수가
    /// exit 2로 번지지 않게 여기서 접는다).
    #[must_use]
    pub fn with_flag(mut self, flag: JobFlag) -> Self {
        self.push(Opt::Flag(flag.as_arg()));
        self
    }

    /// 정수 옵션. 같은 옵션을 다시 주면 마지막 값이 남는다.
    #[must_use]
    pub fn with_count(mut self, opt: JobCount, value: u32) -> Self {
        self.push(Opt::Valued(opt.as_arg(), value.to_string()));
        self
    }

    /// `--type full|incr` — 백업 유형. [`crate::cli::args::BackupType`]를 그대로 받아
    /// CLI가 아는 어휘와 갈라지지 않게 한다.
    #[must_use]
    pub fn with_backup_type(mut self, kind: crate::cli::args::BackupType) -> Self {
        let value = match kind {
            crate::cli::args::BackupType::Full => "full",
            crate::cli::args::BackupType::Incr => "incr",
        };
        self.push(Opt::Valued("--type", value.to_string()));
        self
    }

    /// `--sort created|size` — list 정렬 기준.
    #[must_use]
    pub fn with_sort(mut self, sort: crate::cli::args::ListSort) -> Self {
        let value = match sort {
            crate::cli::args::ListSort::Created => "created",
            crate::cli::args::ListSort::Size => "size",
        };
        self.push(Opt::Valued("--sort", value.to_string()));
        self
    }

    /// `--id <BACKUP_ID>`. 형태만 검증한다([`validate_backup_id`]).
    pub fn with_backup_id(mut self, raw: &str) -> crate::error::Result<Self> {
        let value = validate_backup_id(raw, self.lang)?;
        self.push(Opt::Valued("--id", value));
        Ok(self)
    }

    /// `--at <RFC3339>` — PITR 목표 시점. 형태만 검증하고 의미는 CLI가 판정한다.
    pub fn with_at(mut self, raw: &str) -> crate::error::Result<Self> {
        let value = validate_timestamp(raw, self.lang)?;
        self.push(Opt::Valued("--at", value));
        Ok(self)
    }

    /// `--only <db.collection>` — 선택적 복구 대상.
    pub fn with_only(mut self, raw: &str) -> crate::error::Result<Self> {
        let value = validate_namespace(raw, self.lang)?;
        self.push(Opt::Valued("--only", value));
        Ok(self)
    }

    /// `--ns <db.collection>` — peek 대상 네임스페이스.
    pub fn with_ns(mut self, raw: &str) -> crate::error::Result<Self> {
        let value = validate_namespace(raw, self.lang)?;
        self.push(Opt::Valued("--ns", value));
        Ok(self)
    }

    /// `--db <DB>` — 선택적 백업/복구/마이그레이션 대상 DB.
    pub fn with_db(mut self, raw: &str) -> crate::error::Result<Self> {
        let value = validate_db_name(raw, self.lang)?;
        self.push(Opt::Valued("--db", value));
        Ok(self)
    }

    /// `--collection <COLL>` — 선택적 대상 컬렉션(CLI가 `--db`를 함께 요구한다).
    pub fn with_collection(mut self, raw: &str) -> crate::error::Result<Self> {
        let value = validate_collection_name(raw, self.lang)?;
        self.push(Opt::Valued("--collection", value));
        Ok(self)
    }

    /// `--from <NAME|type#idx>` — 어느 destination에서 읽을지.
    pub fn with_from(mut self, raw: &str) -> crate::error::Result<Self> {
        let value = validate_destination_ref(raw, self.lang)?;
        self.push(Opt::Valued("--from", value));
        Ok(self)
    }

    /// argv를 만든다 — `<verb> <옵션…> --json`.
    ///
    /// `--json`이 항상 마지막에 붙는 이유는 모듈 헤더 "`--json`은 선택이 아니다" 참조.
    /// 전역 플래그(`--config`/`--lang`)는 여기 없다 — 서브커맨드 앞에 놓아야 하고 그
    /// 값은 서버 설정에서 오므로 [`super::runner::JobRunner`]가 붙인다.
    pub fn to_argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.opts.len() * 2 + 2);
        argv.push(self.command.verb().to_string());
        for opt in &self.opts {
            match opt {
                Opt::Flag(flag) => argv.push((*flag).to_string()),
                Opt::Valued(flag, value) => {
                    argv.push((*flag).to_string());
                    // 값은 **독립된 argv 원소**로 넣는다. `--flag=value`로 합치지 않는
                    // 이유: 합치면 값 안의 `=`이 경계를 모호하게 만들고, 셸을 거치지
                    // 않는다는 성질을 눈으로 확인하기 어려워진다.
                    argv.push(value.clone());
                }
            }
        }
        argv.push("--json".to_string());
        argv
    }

    /// 감사 로그 `target` 필드 값 — 프로파일명, 없으면 `-`.
    ///
    /// 빈 문자열이 아니라 `-`를 쓰는 이유: NDJSON을 `grep`으로 훑을 때 "값이 없음"과
    /// "필드가 비었음"이 구분되어야 한다.
    pub fn audit_target(&self) -> &str {
        self.profile.as_ref().map_or("-", ProfileName::as_str)
    }

    /// 감사 로그 `action` 필드 값.
    pub fn audit_action(&self) -> &'static str {
        self.command.audit_action()
    }

    /// 감사 로그에 넣을 마스킹된 인자 목록(t8의 `args_masked` 계약).
    ///
    /// 이 어휘에는 시크릿이 들어갈 자리가 없다(모듈 헤더 "웹은 접속 URI를 인자로 받지
    /// 않는다"). 그런데도 [`SecretRegistry`]를 통과시키는 이유는 방어 심층화다 — 나중에
    /// 누군가 시크릿을 담을 수 있는 옵션을 이 어휘에 추가해도, 감사 로그 경로는 이미
    /// 마스킹을 지나고 있다. 1차 방어(어휘에 자리를 두지 않음)를 대신하는 것이 아니라
    /// 뒤에 서는 것이다([`crate::web::mask`] 헤더의 1차/2차 방어 관계와 같다).
    pub fn masked_args(&self, secrets: &SecretRegistry) -> Vec<String> {
        self.to_argv().iter().map(|a| secrets.mask(a)).collect()
    }

    /// 옵션을 넣는다 — 같은 플래그가 이미 있으면 교체한다.
    ///
    /// 교체(마지막 승리)로 정한 이유: 웹 폼은 같은 필드를 여러 번 보낼 수 있고
    /// (브라우저·프록시 동작), 그때 argv에 같은 플래그가 두 번 나가면 clap이 사용법
    /// 오류를 낸다. 여기서 접으면 "폼을 두 번 눌렀는데 exit 2"가 생기지 않는다.
    fn push(&mut self, opt: Opt) {
        match self.opts.iter_mut().find(|o| o.arg() == opt.arg()) {
            Some(existing) => *existing = opt,
            None => self.opts.push(opt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{BackupType, ListSort};

    /// 최소 명세는 `<verb> --json`이다 — `--json`이 항상 붙는다.
    #[test]
    fn minimal_spec_always_appends_json() {
        for command in [
            JobCommand::Backup,
            JobCommand::Restore,
            JobCommand::Verify,
            JobCommand::Prune,
            JobCommand::Migrate,
            JobCommand::Status,
            JobCommand::List,
            JobCommand::Peek,
            JobCommand::Doctor,
        ] {
            let argv = JobSpec::new(command, Lang::En).to_argv();
            assert_eq!(argv.first().unwrap(), command.verb());
            assert_eq!(
                argv.last().unwrap(),
                "--json",
                "{}에 --json이 붙지 않았다",
                command.verb()
            );
            assert_eq!(
                argv.iter().filter(|a| *a == "--json").count(),
                1,
                "--json이 중복됐다"
            );
        }
    }

    /// 서브커맨드 이름이 CLI의 clap 정의와 일치한다 — 오타 하나가 전 잡을 exit 2로
    /// 만드는 회귀를 컴파일 대신 테스트로 잡는다.
    #[test]
    fn verbs_match_cli_subcommand_names() {
        use clap::CommandFactory;
        let mut cmd = crate::cli::Cli::command();
        cmd.build();
        let names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
        for command in [
            JobCommand::Backup,
            JobCommand::Restore,
            JobCommand::Verify,
            JobCommand::Prune,
            JobCommand::Migrate,
            JobCommand::Status,
            JobCommand::List,
            JobCommand::Peek,
            JobCommand::Doctor,
        ] {
            assert!(
                names.contains(&command.verb()),
                "'{}'는 CLI 서브커맨드가 아니다(어휘가 갈라졌다)",
                command.verb()
            );
        }
    }

    /// 모든 플래그·정수 옵션이 실제 CLI 인자 표면에 존재한다.
    ///
    /// 이 테스트가 이 파일의 어휘와 `src/cli/args.rs`를 묶어 둔다 — CLI에서 플래그
    /// 이름이 바뀌면 여기서 먼저 깨진다(자식이 exit 2를 내는 런타임 실패보다 낫다).
    #[test]
    fn flag_vocabulary_exists_in_cli_surface() {
        use clap::CommandFactory;
        let mut cmd = crate::cli::Cli::command();
        cmd.build();
        // 전 서브커맨드의 long 플래그 집합을 모은다(플래그가 어느 명령에 속하는지는
        // clap이 판정하므로 여기서는 "어딘가에 존재한다"만 확인한다).
        let mut longs: Vec<String> = Vec::new();
        for sub in cmd.get_subcommands() {
            for arg in sub.get_arguments() {
                if let Some(long) = arg.get_long() {
                    longs.push(format!("--{long}"));
                }
            }
        }
        let vocabulary = [
            JobFlag::DryRun.as_arg(),
            JobFlag::Force.as_arg(),
            JobFlag::Deep.as_arg(),
            JobFlag::Chain.as_arg(),
            JobFlag::All.as_arg(),
            JobFlag::SkipPrecheck.as_arg(),
            JobFlag::NoHooks.as_arg(),
            JobFlag::NoEncrypt.as_arg(),
            JobFlag::Drop.as_arg(),
            JobFlag::NsDetail.as_arg(),
            JobFlag::Asc.as_arg(),
            JobCount::KeepFull.as_arg(),
            JobCount::KeepDays.as_arg(),
            JobCount::KeepLast.as_arg(),
            JobCount::RecoveryWindowDays.as_arg(),
            JobCount::MinRedundancy.as_arg(),
            JobCount::Limit.as_arg(),
            "--profile",
            "--target-profile",
            "--id",
            "--at",
            "--only",
            "--ns",
            "--db",
            "--collection",
            "--from",
            "--type",
            "--sort",
            "--json",
        ];
        for flag in vocabulary {
            assert!(
                longs.iter().any(|l| l == flag),
                "'{flag}'가 CLI 인자 표면에 없다 — 어휘가 갈라졌다"
            );
        }
    }

    /// 값은 플래그와 **분리된** argv 원소로 나간다(`--flag=value`로 합치지 않는다).
    #[test]
    fn values_are_separate_argv_elements() {
        let spec = JobSpec::new(JobCommand::Restore, Lang::En)
            .with_profile(ProfileName::parse("prod", Lang::En).unwrap())
            .with_at("2026-07-25T13:00:00Z")
            .unwrap();
        let argv = spec.to_argv();
        let at_idx = argv.iter().position(|a| a == "--at").unwrap();
        assert_eq!(argv[at_idx + 1], "2026-07-25T13:00:00Z");
        assert!(
            !argv.iter().any(|a| a.contains('=')),
            "`--flag=value` 형태가 섞였다: {argv:?}"
        );
    }

    /// 같은 플래그를 두 번 켜도 argv에는 한 번만 나간다.
    #[test]
    fn repeated_flag_is_collapsed() {
        let argv = JobSpec::new(JobCommand::Prune, Lang::En)
            .with_flag(JobFlag::Force)
            .with_flag(JobFlag::Force)
            .with_flag(JobFlag::DryRun)
            .to_argv();
        assert_eq!(argv.iter().filter(|a| *a == "--force").count(), 1);
        assert_eq!(argv.iter().filter(|a| *a == "--dry-run").count(), 1);
    }

    /// 같은 값 옵션을 두 번 주면 마지막 값이 남는다(플래그는 한 번만 나간다).
    #[test]
    fn repeated_valued_option_keeps_last_value() {
        let spec = JobSpec::new(JobCommand::List, Lang::En)
            .with_count(JobCount::Limit, 10)
            .with_count(JobCount::Limit, 25);
        let argv = spec.to_argv();
        assert_eq!(argv.iter().filter(|a| *a == "--limit").count(), 1);
        let idx = argv.iter().position(|a| a == "--limit").unwrap();
        assert_eq!(argv[idx + 1], "25");
    }

    /// 프로파일을 바꿔 지정하면 argv와 감사 target이 함께 갱신된다(둘이 어긋나면
    /// 감사 로그가 실행된 것과 다른 프로파일을 가리킨다).
    #[test]
    fn profile_replacement_updates_argv_and_audit_target() {
        let spec = JobSpec::new(JobCommand::Backup, Lang::En)
            .with_profile(ProfileName::parse("staging", Lang::En).unwrap())
            .with_profile(ProfileName::parse("prod", Lang::En).unwrap());
        assert_eq!(spec.audit_target(), "prod");
        let argv = spec.to_argv();
        assert_eq!(argv.iter().filter(|a| *a == "--profile").count(), 1);
        let idx = argv.iter().position(|a| a == "--profile").unwrap();
        assert_eq!(argv[idx + 1], "prod");
    }

    /// 프로파일이 없으면 감사 target은 `-`다(빈 문자열이 아니다).
    #[test]
    fn audit_target_is_dash_without_profile() {
        assert_eq!(
            JobSpec::new(JobCommand::Doctor, Lang::En).audit_target(),
            "-"
        );
    }

    /// 파괴적 명령은 restore/prune/migrate뿐이다.
    #[test]
    fn destructive_set_is_exactly_restore_prune_migrate() {
        let destructive = [JobCommand::Restore, JobCommand::Prune, JobCommand::Migrate];
        let safe = [
            JobCommand::Backup,
            JobCommand::Verify,
            JobCommand::Status,
            JobCommand::List,
            JobCommand::Peek,
            JobCommand::Doctor,
        ];
        for c in destructive {
            assert!(c.is_destructive(), "{}는 파괴적이어야 함", c.verb());
            assert!(JobSpec::new(c, Lang::En).is_destructive());
        }
        for c in safe {
            assert!(!c.is_destructive(), "{}는 파괴적이 아니어야 함", c.verb());
        }
    }

    /// `--dry-run`을 붙여도 파괴적 판정은 바뀌지 않는다 — 모듈 헤더 "파괴적 판정은
    /// 명령 단위다" 참조.
    #[test]
    fn dry_run_does_not_downgrade_destructive_classification() {
        let spec = JobSpec::new(JobCommand::Prune, Lang::En).with_flag(JobFlag::DryRun);
        assert!(
            spec.is_destructive(),
            "--dry-run이 감사 게이트를 우회하면 안 됨"
        );
    }

    /// 감사 action은 명령마다 서로 다르다(고정 어휘 — grep 가능).
    #[test]
    fn audit_actions_are_distinct_and_suffixed() {
        let commands = [
            JobCommand::Backup,
            JobCommand::Restore,
            JobCommand::Verify,
            JobCommand::Prune,
            JobCommand::Migrate,
            JobCommand::Status,
            JobCommand::List,
            JobCommand::Peek,
            JobCommand::Doctor,
        ];
        let mut actions: Vec<&str> = commands.iter().map(|c| c.audit_action()).collect();
        actions.sort_unstable();
        let count = actions.len();
        actions.dedup();
        assert_eq!(actions.len(), count, "감사 action이 중복됐다");
        for c in commands {
            assert_eq!(c.audit_action(), format!("{}.run", c.verb()));
        }
    }

    /// enum으로만 만드는 값(백업 유형·정렬)은 CLI가 아는 어휘로 정확히 접힌다.
    #[test]
    fn enum_valued_options_render_cli_vocabulary() {
        let argv = JobSpec::new(JobCommand::Backup, Lang::En)
            .with_backup_type(BackupType::Incr)
            .to_argv();
        let idx = argv.iter().position(|a| a == "--type").unwrap();
        assert_eq!(argv[idx + 1], "incr");

        let argv = JobSpec::new(JobCommand::List, Lang::En)
            .with_sort(ListSort::Size)
            .to_argv();
        let idx = argv.iter().position(|a| a == "--sort").unwrap();
        assert_eq!(argv[idx + 1], "size");
    }

    /// 정수 옵션은 우리가 포맷하므로 숫자만 나간다(주입 표면 없음).
    #[test]
    fn count_options_render_digits_only() {
        let argv = JobSpec::new(JobCommand::Prune, Lang::En)
            .with_count(JobCount::KeepFull, 7)
            .with_count(JobCount::MinRedundancy, 2)
            .to_argv();
        for (flag, expected) in [("--keep-full", "7"), ("--min-redundancy", "2")] {
            let idx = argv.iter().position(|a| a == flag).unwrap();
            assert_eq!(argv[idx + 1], expected);
            assert!(argv[idx + 1].chars().all(|c| c.is_ascii_digit()));
        }
    }

    /// 검증에 걸리는 값은 명세 자체가 만들어지지 않는다 — 잘못된 값이 argv까지
    /// 도달할 경로가 없다.
    #[test]
    fn hostile_values_never_reach_argv() {
        let base = JobSpec::new(JobCommand::Restore, Lang::En);
        assert!(base.clone().with_backup_id("--force").is_err());
        assert!(base.clone().with_at("$(whoami)").is_err());
        assert!(base.clone().with_only("../etc/passwd").is_err());
        assert!(base.clone().with_db("a;rm -rf /").is_err());
        assert!(base.clone().with_collection("a\0b").is_err());
        assert!(base.with_from("-f").is_err());
        assert!(ProfileName::parse("../../etc", Lang::En).is_err());
    }

    /// argv 전체에 셸 메타문자가 하나도 없다 — 검증을 통과한 명세가 만드는 argv는
    /// 항상 이 성질을 만족한다.
    #[test]
    fn generated_argv_contains_no_shell_metacharacters() {
        let spec = JobSpec::new(JobCommand::Restore, Lang::En)
            .with_profile(ProfileName::parse("prod", Lang::En).unwrap())
            .with_target_profile(ProfileName::parse("dr", Lang::En).unwrap())
            .with_backup_id(&uuid::Uuid::now_v7().to_string())
            .unwrap()
            .with_at("2026-07-25T13:00:00Z")
            .unwrap()
            .with_flag(JobFlag::Force);
        for arg in spec.to_argv() {
            assert!(
                !arg.chars()
                    .any(|c| ";|&$`(){}[]<>*?~!'\"\\\n\r\0 ".contains(c)),
                "argv 원소에 셸 메타문자가 있다: {arg:?}"
            );
        }
    }

    /// 마스킹 경로는 등록된 시크릿을 지운다 — 어휘에 시크릿 자리가 없어도 이 경로가
    /// 살아 있어야 한다(방어 심층화).
    #[test]
    fn masked_args_redacts_registered_secret() {
        // 프로파일명 어휘로 표현 가능한 값을 시크릿으로 등록해(현실에는 없는 조합)
        // 마스킹이 실제로 argv 원소에 적용되는지 확인한다.
        let mut registry = SecretRegistry::new();
        registry.register("supersecretprofile");
        let spec = JobSpec::new(JobCommand::Backup, Lang::En)
            .with_profile(ProfileName::parse("supersecretprofile", Lang::En).unwrap());
        let masked = spec.masked_args(&registry);
        assert!(
            !masked.iter().any(|a| a == "supersecretprofile"),
            "마스킹이 적용되지 않았다: {masked:?}"
        );
        assert!(masked
            .iter()
            .any(|a| a == crate::web::mask::REDACTED_PLACEHOLDER));
        // 마스킹은 argv 길이를 바꾸지 않는다(감사 로그와 실행 argv의 원소 수가 같아야
        // 나중에 두 기록을 나란히 놓고 읽을 수 있다).
        assert_eq!(masked.len(), spec.to_argv().len());
    }
}
