//! 에러 모델과 종료 코드(exit code) 매핑.
//!
//! PRD §9 종료 코드 규약을 단일 진실 공급원(single source of truth)으로 구현한다.
//! `main`은 [`XBackupError::exit_code`]를 통해서만 프로세스 종료 코드를 결정한다.
//!
//! | 코드 | 의미 |
//! |:---:|------|
//! | 0 | 성공 (에러 아님 — 정상 경로) |
//! | 1 | 실패 — 작업 미완료(파이프라인·업로드·복구·IO·드라이버 오류) |
//! | 2 | 사용법·설정 오류(잘못된 플래그, config 결함) |
//! | 3 | 사전 점검 실패 — 작업 미시작(`status` 핵심 항목 실패) |
//! | 4 | 경고 동반 성공(gap 감지로 증분→풀 승격, verify 경고 등) |
//! | 5 | 잠금 충돌 — 동일 프로파일의 다른 인스턴스 실행 중(FR-12) |

use std::process::ExitCode;

use crate::i18n::Lang;

/// 종료 코드 상수. 매직 넘버를 코드 전반에 흩뿌리지 않기 위해 한곳에 모은다.
pub mod exit_codes {
    /// 성공.
    pub const SUCCESS: u8 = 0;
    /// 실패 — 작업 미완료.
    pub const FAILURE: u8 = 1;
    /// 사용법·설정 오류.
    pub const USAGE: u8 = 2;
    /// 사전 점검 실패 — 작업 미시작.
    pub const PRECHECK: u8 = 3;
    /// 경고 동반 성공.
    pub const WARNING: u8 = 4;
    /// 잠금 충돌.
    pub const LOCK_CONFLICT: u8 = 5;
}

/// x-backup의 모든 실패 경로를 표현하는 최상위 에러 타입.
///
/// 각 variant는 PRD §9의 종료 코드 클래스 중 하나로 매핑된다([`XBackupError::exit_code`]).
/// 도메인별 세부 에러(스토리지·암호화·드라이버 등)는 후속 태스크에서
/// 각 variant의 내부로 합성하거나 새 variant로 추가한다.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum XBackupError {
    /// 작업 미완료 — 파이프라인/업로드/복구/IO/드라이버 등 일반 실행 실패. → exit 1
    #[error("{0}")]
    Failure(String),

    /// 입출력 오류. → exit 1
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// 스토리지 업로드(put) 실패 — 산출물 저장 중 오류. → exit 1
    ///
    /// 부분 산출물은 호출 경로(RAII 가드·멀티파트 abort)에서 정리한다(PRD §11 신뢰성).
    #[error("{0}")]
    StorageUpload(String),

    /// 스토리지 다운로드(get)·조회(list)·삭제(delete) 실패 — 산출물 읽기/관리 중 오류. → exit 1
    #[error("{0}")]
    StorageDownload(String),

    /// 사용법·플래그 오류(잘못된 인자 조합 등). → exit 2
    #[error("{0}")]
    Usage(String),

    /// 설정 결함(config 파싱 실패, 필수 키 누락, 알 수 없는 프로파일 등). → exit 2
    #[error("{0}")]
    Config(String),

    /// 사전 점검 실패 — 작업을 시작하지 않음(`status` 핵심 항목 실패). → exit 3
    #[error("{0}")]
    PrecheckFailed(String),

    /// oplog gap 감지로 증분→풀 승격 등, 경고를 동반한 성공. → exit 4
    #[error("{0}")]
    Warning(String),

    /// 무결성 검증에서 경고가 발생함(verify 경고). → exit 4
    #[error("{0}")]
    VerifyWarning(String),

    /// 동일 프로파일의 다른 인스턴스가 실행 중 — 잠금 충돌(FR-12). → exit 5
    #[error("{0}")]
    LockConflict(String),
}

impl XBackupError {
    /// 에러 종류를 가리키는 짧은 라벨.
    ///
    /// 예전에는 이 라벨이 각 variant의 `#[error("작업 실패: {0}")]`에 박혀 있었다.
    /// 그 포맷 문자열은 컴파일 시점에 고정되므로 `[output].language = "en"`으로 돌려도
    /// 한국어 라벨이 영어 본문 앞에 붙었다. 라벨을 Display에서 떼어 내고 여기서 고르면
    /// 출력 지점이 언어를 정할 수 있다 — 본문은 각 호출부가 이미 언어에 맞게 만든다.
    pub fn kind_label(&self, lang: Lang) -> &'static str {
        match self {
            Self::Failure(_) => lang.sel("failed", "작업 실패"),
            Self::Io(_) => lang.sel("I/O error", "IO 오류"),
            Self::StorageUpload(_) => lang.sel("storage upload failed", "스토리지 업로드 실패"),
            Self::StorageDownload(_) => lang.sel("storage read failed", "스토리지 읽기 실패"),
            Self::Usage(_) => lang.sel("usage error", "사용법 오류"),
            Self::Config(_) => lang.sel("config error", "설정 오류"),
            Self::PrecheckFailed(_) => lang.sel("precheck failed", "사전 점검 실패"),
            Self::Warning(_) => lang.sel("warning", "경고"),
            Self::VerifyWarning(_) => lang.sel("verify warning", "검증 경고"),
            Self::LockConflict(_) => lang.sel("lock conflict", "잠금 충돌"),
        }
    }

    /// PRD §9 규약에 따른 프로세스 종료 코드(`u8`)를 반환한다.
    ///
    /// 이 매핑이 종료 코드의 단일 진실 공급원이다 — `main`은 이 값만 신뢰한다.
    pub fn exit_code(&self) -> u8 {
        use exit_codes::*;
        match self {
            Self::Failure(_) | Self::Io(_) | Self::StorageUpload(_) | Self::StorageDownload(_) => {
                FAILURE
            }
            Self::Usage(_) | Self::Config(_) => USAGE,
            Self::PrecheckFailed(_) => PRECHECK,
            Self::Warning(_) | Self::VerifyWarning(_) => WARNING,
            Self::LockConflict(_) => LOCK_CONFLICT,
        }
    }

    /// 표준 라이브러리의 [`ExitCode`]로 변환한다(`main` 반환용).
    pub fn exit(&self) -> ExitCode {
        ExitCode::from(self.exit_code())
    }

    /// 경고 동반 성공(exit 4)인지 — `main`이 ERROR가 아닌 WARN 레벨로 출력하기 위해 쓴다.
    ///
    /// gap→풀 승격, verify 경고처럼 작업은 성공했으나 주의가 필요한 경우다. 빨간 ERROR로
    /// 찍으면 실패처럼 보이므로(exit 4 ≠ 실패) 호출부에서 표시 레벨을 분기한다.
    pub fn is_warning(&self) -> bool {
        self.exit_code() == exit_codes::WARNING
    }
}

/// 크레이트 전역에서 사용하는 결과 별칭.
pub type Result<T> = std::result::Result<T, XBackupError>;

#[cfg(test)]
mod tests {
    use super::exit_codes::*;
    use super::*;

    /// 각 variant가 PRD §9 표의 코드(0~5)로 정확히 매핑되는지 검증한다.
    #[test]
    fn exit_code_mapping_matches_prd_table() {
        assert_eq!(XBackupError::Failure("x".into()).exit_code(), FAILURE);
        assert_eq!(
            XBackupError::Io(std::io::Error::other("disk")).exit_code(),
            FAILURE
        );
        assert_eq!(XBackupError::StorageUpload("x".into()).exit_code(), FAILURE);
        assert_eq!(
            XBackupError::StorageDownload("x".into()).exit_code(),
            FAILURE
        );
        assert_eq!(XBackupError::Usage("x".into()).exit_code(), USAGE);
        assert_eq!(XBackupError::Config("x".into()).exit_code(), USAGE);
        assert_eq!(
            XBackupError::PrecheckFailed("x".into()).exit_code(),
            PRECHECK
        );
        assert_eq!(XBackupError::Warning("x".into()).exit_code(), WARNING);
        assert_eq!(XBackupError::VerifyWarning("x".into()).exit_code(), WARNING);
        assert_eq!(
            XBackupError::LockConflict("x".into()).exit_code(),
            LOCK_CONFLICT
        );
    }

    /// 종료 코드는 PRD가 정의한 0~5 범위를 벗어나지 않는다.
    #[test]
    fn all_exit_codes_in_valid_range() {
        let samples = [
            XBackupError::Failure("a".into()),
            XBackupError::StorageUpload("a".into()),
            XBackupError::StorageDownload("a".into()),
            XBackupError::Usage("a".into()),
            XBackupError::Config("a".into()),
            XBackupError::PrecheckFailed("a".into()),
            XBackupError::Warning("a".into()),
            XBackupError::VerifyWarning("a".into()),
            XBackupError::LockConflict("a".into()),
        ];
        for err in samples {
            let code = err.exit_code();
            assert!(
                (SUCCESS..=LOCK_CONFLICT).contains(&code),
                "코드 {code} 범위 밖"
            );
        }
    }

    /// 종류 라벨은 언어를 따른다 — 예전에는 `#[error(...)]`에 한국어로 박혀 있어서
    /// `language = "en"`으로도 바뀌지 않았다. 그 회귀를 막는다.
    #[test]
    fn kind_label_follows_language() {
        let err = XBackupError::Warning("x".into());
        assert_eq!(err.kind_label(Lang::En), "warning");
        assert_eq!(err.kind_label(Lang::Ko), "경고");

        let err = XBackupError::Usage("x".into());
        assert_eq!(err.kind_label(Lang::En), "usage error");
        assert_eq!(err.kind_label(Lang::Ko), "사용법 오류");
    }

    /// Display에는 종류 라벨이 들어가지 않는다 — 라벨은 출력 지점이 붙인다.
    /// 라벨이 Display에 남아 있으면 중첩 에러에 접두사가 두 번 끼거나, 언어가
    /// 뒤섞인 한 줄이 만들어진다.
    #[test]
    fn display_carries_only_the_message() {
        assert_eq!(XBackupError::Failure("boom".into()).to_string(), "boom");
        assert_eq!(
            XBackupError::Warning("heads up".into()).to_string(),
            "heads up"
        );
    }

    /// 상수 값이 PRD §9 표의 숫자와 일치하는지 고정한다(회귀 방지).
    #[test]
    fn exit_code_constants_are_stable() {
        assert_eq!(SUCCESS, 0);
        assert_eq!(FAILURE, 1);
        assert_eq!(USAGE, 2);
        assert_eq!(PRECHECK, 3);
        assert_eq!(WARNING, 4);
        assert_eq!(LOCK_CONFLICT, 5);
    }
}
