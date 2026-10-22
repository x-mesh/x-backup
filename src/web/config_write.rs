//! `/config` 저장 이음매의 실제 구현 — [`crate::web::routes::config::ConfigStore`]의 프로덕션
//! 어댑터. t26은 검증까지 전부 만들고 [`crate::web::routes::config::PendingStore`]로 파일을
//! 쓰지 않은 채 남겨 뒀다(`ChangeReceipt::NotPersisted`). 이 파일이 그 자리에 실제 디스크
//! 쓰기를 붙인다.
//!
//! ## 이 파일이 지키는 것 — 왜 "그냥 쓰기"가 아닌가
//! 이 웹 콘솔은 사용자의 **운영 config 파일을 직접 고친다.** 그 파일이 반쪽으로 남으면
//! cron으로 도는 모든 백업이 그 순간부터 멈추거나(파싱 실패) 잘못된 설정으로 돈다. 그래서
//! [`FileConfigStore::apply`]는 다음 네 가지를 이 순서로 강제한다:
//!
//! 1. **저장 전 `doctor` 검증** — 새 config를 임시 파일에 써서 `x-backup doctor --json`으로
//!    점검하고, 통과하지 못하면(exit 2/3) **원본에 손도 대지 않는다.**
//! 2. **직전본 보관** — 저장 직전 원본을 세대별로 백업한다([`rotate_backups`]).
//! 3. **원자적 교체** — 임시 파일에 쓰고 `fsync`한 뒤 `rename`한다([`write_atomic`]).
//!    크래시가 어느 지점에서 나든 원본 config는 "완전한 구버전" 또는 "완전한 신버전" 중
//!    하나로만 관측된다 — 중간 상태가 없다.
//!
//! ## 값 해석·상속 펼침은 여기서 하지 않는다
//! [`crate::web::routes::config::ProfileChange`]는 이미 "프로파일 테이블에 직접 적힐 키"
//! (`set`)와 "지울 키"(`unset`)로 걸러진 상태로 온다 — 상속(`extends`/`[defaults]`)은 그
//! 계산에서 **의도적으로 빠져 있다**(t26 모듈 헤더 함정 2). 이 파일은 그 값을 원본 TOML
//! 테이블의 해당 프로파일 자리에 그대로 꽂거나 지울 뿐, `[defaults]`·`[base.*]`·다른
//! 프로파일에는 손대지 않는다([`apply_change_to_table`]). 그래서 "공용 정책 한 곳을 고치면
//! 전 프로파일에 반영된다"는 v2의 성질이 저장 후에도 유지된다.
//!
//! ## v1 문법 보존 — 그리고 `dest` 하나가 nested 4경로로 갈라지는 이유
//! v1은 중첩 구조([profiles.<name>.source] 등)라 flat 키를 그 자리에 꽂으려면 경로
//! 대응표([`crate::web::routes::config::V1_PATHS`], t26이 만들어 뒀다)가 필요하다. 그 표에서
//! **한 flat 키가 여러 nested 경로에 대응하는 경우가 정확히 하나** 있다 — `dest`다.
//!
//! 이유는 v1과 v2가 destination을 다른 층위에서 표현하기 때문이다. v2의 `dest`는 "어디에
//! 쓰는가"를 **한 문자열**로 적는다(`local:/srv/b`, `s3:bucket/prefix`). v1은 같은 정보를
//! 백엔드 종류(`destination.type`)와 그 종류에 딸린 위치 필드로 **쪼개서** 적고, 위치 필드가
//! 종류마다 다르다 — local이면 `destination.path`, s3면 `destination.s3.bucket`(+`.prefix`).
//! 그래서 compact 한 줄을 v1에 쓰려면 스킴을 먼저 읽어 **어느 경로 조합에 쓸지 고른** 다음,
//! 반대쪽 조합은 지워야 한다([`apply_dest_v1`]) — s3에서 local로 바꿨는데 옛 버킷이 남아
//! 있으면 파일을 읽는 사람이 둘 중 어느 것이 실제 위치인지 추측해야 한다. 그 4경로가
//! [`V1_DEST_PATHS`]이고, `V1_PATHS`에서 flat `dest`로 대응되는 경로 집합과 **같아야 한다**
//! (`v1_dest_paths_match_the_lookup_table` 테스트가 그 일치를 고정한다 — 한쪽만 늘어나면
//! 읽기와 쓰기가 어긋난다).
//!
//! t27은 이 압축 해제를 아직 구현하지 않은 채 `dest`가 낀 변경을 **조용히 버리지 않고
//! 거부**했다(422). 그 판단은 옳았고 — 조용히 버리면 운영자는 저장됐다고 믿는데 destination은
//! 그대로다 — 이제 그 자리에 실제 압축 해제가 들어왔다. `unset`도 같은 4경로를 전부 지운다.
//!
//! ## v1 → v2 전환은 **명시적 동작**이다 (저장의 부작용이 아니다)
//! [`plan_v1_to_v2`]·[`ConfigStore::convert_to_v2`]는 파일 **전체**를 v2 표면으로 다시 쓴다.
//! 값 하나를 저장하는 경로는 이 코드를 절대 부르지 않는다 — 운영자가 전환 화면에서 diff를
//! 보고 확인을 눌러야만 실행된다(`routes::config::convert_submit`).
//!
//! **전환은 문법 변환이지 정책 재구성이 아니다.** v1에는 상속 표면이 없으므로(모든 값이 각
//! 프로파일에 직접 적혀 있다) v2로 옮겨도 `[defaults]`·`[base.*]`·`extends`가 **생기지
//! 않는다** — 프로파일마다 자기 값을 그대로 들고 간다. 공통 정책을 한 곳으로 모으는 일은
//! 사람이 판단해야 하는 별개의 작업이고(어느 값이 공용 정책이고 어느 값이 그 프로파일만의
//! 예외인지는 파일이 알려주지 않는다), 이 코드가 추측하면 "합쳤는데 알고 보니 예외였던 값"이
//! 조용히 다른 프로파일로 번진다. 그래서 전환은 **값을 하나도 바꾸지 않는다**는 것을
//! 스스로 증명하고([`plan_v1_to_v2`]의 등가성 게이트), 화면이 그 사실을 말한다.
//!
//! ## 검증 자식은 t10 `JobRunner`를 쓰지 않는다
//! `JobRunner`는 서버 기동 시 확정된 **한 config 경로**로 자식을 띄우도록 배선되어 있다
//! (`JobSpec`에는 임시 경로를 넘길 자리가 없다 — `src/web/job/spec.rs` 확인). 여기서
//! 검증해야 하는 대상은 아직 디스크에 없는 **후보** config(임시 파일)이므로 그 배선에 맞지
//! 않는다. 대신 `routes::doctor`가 이미 쓰는 것과 같은 패턴 — 자기 자신을
//! [`std::env::current_exe`]로 다시 부르고, 상한 시간을 두고, 넘기지 못하면 죽인다 — 을
//! 이 파일에서 작고 독립적으로 반복한다([`run_with_timeout`]). 판정·파싱은 중복하지 않고
//! `routes::doctor`의 공개 함수(`doctor_exe`·`parse_report`·`Verdict`·`level_from_status`)를
//! 그대로 재사용한다.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use toml::value::{Table, Value};

use crate::config::file::Config;
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::web::audit::AuditReceipt;
use crate::web::routes::config::{ChangeOp, ChangeReceipt, ConfigStore, ProfileChange, V1_PATHS};
use crate::web::routes::doctor::{self, Verdict};
use crate::web::view::components::Level;
use crate::web::view::config::ConfigSyntax;

/// 저장 직전 원본을 몇 세대까지 보관하는가.
///
/// 1개만 두면 팀 지시서가 명시적으로 짚은 함정이 그대로 벌어진다 — 저장을 두 번 잘못
/// 누르면(예: 실수로 잘못된 값을 두 번 연속 저장) 진짜 원본이 사라진다. 반대로 무한정
/// 쌓으면 이 화면이 config 파일 하나를 가끔 고치는 용도인데도 디렉터리가 편집 세션 하나로
/// 지저분해진다. 5는 "실수로 여러 번 저장 버튼을 누르는 한 편집 세션" 동안 원본이 세대
/// 밖으로 밀려나지 않을 만한 여유이면서, 그 이상은 이 화면의 쓰임(운영자가 가끔 프로파일
/// 하나를 고침)에 비해 과하다고 판단한 절충값이다 — logrotate의 흔한 기본 보관 개수,
/// vim의 `.swp`/백업 관행과 같은 자리(수 개)를 택했다.
const BACKUP_GENERATIONS: usize = 5;

/// 저장 전 `doctor` 검증 자식의 실행 상한.
///
/// `routes::doctor`의 `DOCTOR_TIMEOUT`(20초)과 같은 근거를 그대로 쓴다 — `doctor`가 하는
/// 일은 config 파싱과 몇 번의 `stat()`뿐이라 정상 경로는 수십 밀리초 안에 끝나지만,
/// `recipient_file`이 멈춘 네트워크 마운트를 가리키면 `Path::exists()`가 실제로 오래
/// 매달릴 수 있다. 그 파일의 private 상수를 이 모듈에서 그대로 가져다 쓸 수는 없어(모듈
/// 경계) 값과 근거를 동일하게 복제한다.
const SAVE_DOCTOR_TIMEOUT: Duration = Duration::from_secs(20);

/// doctor 자식이 stdout에 아무것도 못 남기고 죽었을 때 stderr에서 보여줄 발췌 상한(문자 수).
const STDERR_EXCERPT_CHARS: usize = 400;

/// compact destination 표기를 싣는 flat 키.
const KEY_DEST: &str = "dest";

/// v1에서 **compact `dest` 하나가 갈라져 쓰이는 nested 경로 전부**(모듈 헤더 참조).
///
/// 이 목록은 [`V1_PATHS`]에서 flat `dest`로 대응되는 경로 집합과 같아야 한다 — 읽기(t26의
/// 출처 추적)와 쓰기(이 파일)가 같은 경로를 봐야 왕복이 성립한다.
const V1_DEST_PATHS: &[&str] = &[
    "destination.type",
    "destination.path",
    "destination.s3.bucket",
    "destination.s3.prefix",
];

// ---------------------------------------------------------------------------
// 순수 함수 — 파싱된 TOML 테이블에 변경을 적용한다(파일·자식 프로세스를 건드리지 않는다)
// ---------------------------------------------------------------------------

/// [`ProfileChange`]를 파싱된 config 루트 테이블에 적용한다. 문법별로 갈라지는 유일한
/// 지점이다 — 그 아래 함수들은 각자 자기 문법의 테이블 모양만 안다.
///
/// `pub(crate)`인 이유: `routes::config`의 라운드트립 테스트가 **저장이 실제로 쓰는 이 함수**로
/// 검증해야 한다. t26 시점에는 이 구현이 없어서 그 테스트가 자기 사본을 들고 있었고, 그러면
/// 사본과 실물이 갈라지는 순간(예: `dest` 압축 해제) 테스트는 통과하는데 저장은 실패한다.
pub(crate) fn apply_change_to_table(root: &mut Table, change: &ProfileChange) -> Result<()> {
    match change.syntax {
        ConfigSyntax::V1 => apply_change_v1(root, change),
        // 프로파일 섹션이 아예 없던 빈 config에 첫 프로파일을 만들 때는 v2로 시작한다 —
        // `x-backup init`이 새 config를 v2로 쓰는 것과 같은 관례다
        // (`src/cli/handlers/init.rs`의 `written.contains("[profile.prod]")` 단정).
        ConfigSyntax::V2 | ConfigSyntax::Empty => apply_change_v2(root, change),
    }
}

/// v2 — flat 키가 곧 TOML 키이므로 압축 해제가 필요 없다. `[profile.<name>]` 테이블
/// 하나에만 손댄다.
fn apply_change_v2(root: &mut Table, change: &ProfileChange) -> Result<()> {
    let profiles = root
        .entry("profile")
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| XBackupError::Config("'profile'이 테이블이 아닙니다".to_string()))?;

    if change.op == ChangeOp::Delete {
        profiles.remove(change.name.as_str());
        return Ok(());
    }
    check_existence_precondition(profiles, change)?;

    let entry = profiles
        .entry(change.name.as_str().to_string())
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            XBackupError::Config(format!(
                "[profile.{}]이 테이블이 아닙니다",
                change.name.as_str()
            ))
        })?;
    for key in &change.unset {
        entry.remove(key);
    }
    for (key, value) in &change.set {
        entry.insert(key.clone(), value.clone());
    }
    Ok(())
}

/// v1 — 중첩 구조라 flat 키를 [`V1_PATHS`]로 경로 변환해 써야 한다. `dest`만 예외적으로
/// 여러 경로로 갈라지므로 [`apply_dest_v1`]이 따로 다룬다(모듈 헤더 참조).
fn apply_change_v1(root: &mut Table, change: &ProfileChange) -> Result<()> {
    let profiles = root
        .entry("profiles")
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| XBackupError::Config("'profiles'가 테이블이 아닙니다".to_string()))?;

    if change.op == ChangeOp::Delete {
        profiles.remove(change.name.as_str());
        return Ok(());
    }
    check_existence_precondition(profiles, change)?;

    let entry = profiles
        .entry(change.name.as_str().to_string())
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            XBackupError::Config(format!(
                "[profiles.{}]이 테이블이 아닙니다",
                change.name.as_str()
            ))
        })?;

    for key in &change.unset {
        if key == KEY_DEST {
            // compact 한 줄을 지우는 것은 v1에서 **4경로를 다 지우는 것**이다. 하나라도
            // 남기면 "destination을 지웠다"고 믿는 운영자의 파일에 옛 위치가 살아 있게 된다.
            clear_dest_v1(entry);
            continue;
        }
        // v1_path_for가 None이면(=v1에 대응 없는 키) 조용히 건너뛴다. FIELDS의 나머지 키는
        // 전부 V1_PATHS에 1:1로 있다(방어적 no-op, 패닉 없음).
        if let Some(path) = v1_path_for(key) {
            remove_path(entry, path);
        }
    }
    for (key, value) in &change.set {
        if key == KEY_DEST {
            let compact = value.as_str().ok_or_else(|| {
                XBackupError::Usage(format!("'{KEY_DEST}' 값이 문자열이 아닙니다."))
            })?;
            apply_dest_v1(entry, compact)?;
            continue;
        }
        match v1_path_for(key) {
            Some(path) => set_path(entry, path, value.clone())?,
            None => {
                return Err(XBackupError::Config(format!(
                    "'{key}'는 v1 문법에서 쓸 자리가 없습니다."
                )))
            }
        }
    }
    Ok(())
}

/// compact `dest` 한 줄을 v1 nested 경로로 펼쳐 쓴다.
///
/// 스킴이 **어느 경로 조합에 쓸지를 정한다**(모듈 헤더 참조). 쓰기 전에 [`clear_dest_v1`]으로
/// 4경로를 모두 비우는 것이 핵심이다 — 그러지 않으면 s3→local 전환에서 `destination.s3.bucket`이
/// 남아 파일에 위치가 두 개 적힌 것처럼 보인다(로더는 `type`을 보므로 동작은 하지만, 그 파일을
/// 읽는 사람은 어느 쪽이 사는 값인지 알 수 없다).
///
/// 표기 검증의 권위는 [`crate::web::routes::config`]의 `validate_dest`이고 폼 제출은 그것을
/// 먼저 통과한다. 여기서 다시 형태를 확인하는 것은 폼 밖에서 만든 요청에 대한 방어이며,
/// 판정 기준은 [`crate::config::v2`]의 `parse_dest_compact`와 같다(같지 않으면 저장은 됐는데
/// 로더가 거부하는 파일이 나온다).
fn apply_dest_v1(entry: &mut Table, compact: &str) -> Result<()> {
    let (scheme, rest) = compact.split_once(':').ok_or_else(|| {
        XBackupError::Usage(format!(
            "'{KEY_DEST}' 값 '{compact}'에 스킴이 없습니다 — \"local:/path\" 또는 \
             \"s3:bucket/prefix\" 형태여야 합니다."
        ))
    })?;
    clear_dest_v1(entry);
    match scheme {
        "local" => {
            if rest.is_empty() {
                return Err(XBackupError::Usage(format!(
                    "'{KEY_DEST}' 값 '{compact}': local 경로가 비었습니다."
                )));
            }
            set_path(
                entry,
                "destination.type",
                Value::String("local".to_string()),
            )?;
            set_path(entry, "destination.path", Value::String(rest.to_string()))?;
        }
        "s3" => {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((bucket, prefix)) => (bucket, Some(prefix)),
                None => (rest, None),
            };
            if bucket.is_empty() {
                return Err(XBackupError::Usage(format!(
                    "'{KEY_DEST}' 값 '{compact}': s3 버킷이 비었습니다."
                )));
            }
            set_path(entry, "destination.type", Value::String("s3".to_string()))?;
            set_path(
                entry,
                "destination.s3.bucket",
                Value::String(bucket.to_string()),
            )?;
            if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
                set_path(
                    entry,
                    "destination.s3.prefix",
                    Value::String(prefix.to_string()),
                )?;
            }
        }
        other => {
            return Err(XBackupError::Usage(format!(
                "'{KEY_DEST}' 값 '{compact}': 알 수 없는 스킴 '{other}'(local|s3)."
            )))
        }
    }
    prune_empty_dest_tables(entry);
    Ok(())
}

/// destination의 **위치 4경로**만 지운다. `destination.name`·`destination.s3.{region,endpoint,
/// credentials_env}`는 각자 독립된 flat 키(`dest_name`·`s3_region`·…)를 가지므로 건드리지
/// 않는다 — 운영자가 지우라고 하지 않은 키를 지우는 것도 조용한 손실이다.
fn clear_dest_v1(entry: &mut Table) {
    for path in V1_DEST_PATHS {
        remove_path(entry, path);
    }
    prune_empty_dest_tables(entry);
}

/// 값이 하나도 남지 않은 `[destination.s3]`·`[destination]` 테이블을 지운다.
///
/// serde 기준으로 **빈 테이블과 없는 테이블은 같은 뜻이다**(두 구조체의 모든 필드가
/// `#[serde(default)]`이므로 어느 쪽이든 전부 `None`으로 역직렬화된다). 그래서 이 정리는
/// 의미를 바꾸지 않고, 파일에 `[profiles.p.destination.s3]`만 남은 유령 섹션이 쌓이는 것을
/// 막는다. 반대로 **비어 있지 않으면 절대 건드리지 않는다** — region만 남은 s3 테이블은
/// 운영자가 적어 둔 값이다.
fn prune_empty_dest_tables(entry: &mut Table) {
    let Some(destination) = entry.get_mut("destination").and_then(Value::as_table_mut) else {
        return;
    };
    if destination
        .get("s3")
        .and_then(Value::as_table)
        .is_some_and(Table::is_empty)
    {
        destination.remove("s3");
    }
    if destination.is_empty() {
        entry.remove("destination");
    }
}

/// 동시 편집 경합에 대한 마지막 방어선.
///
/// 라우트(`routes::config::save`/`delete_submit`)가 이미 존재 여부를 확인하지만, 그 확인과
/// 이 저장 사이에는 파일을 다시 읽고 doctor 자식을 기다리는 시간(TOCTOU 창)이 있다. `Create`
/// 인데 그 사이 다른 요청이 같은 이름을 먼저 만들었다면 `or_insert_with`는 조용히 기존
/// 테이블에 병합해 버린다 — 그러면 두 요청의 입력이 뒤섞인 프로파일이 생긴다. `Update`인데
/// 그 사이 다른 요청이 먼저 지웠다면 `or_insert_with`는 조용히 되살려 버린다. 둘 다 "저장
/// 결과가 화면에 없던 값을 갖는다"는 조용한 실패이므로, 여기서 다시 한 번 보고 명확히
/// 거부한다.
fn check_existence_precondition(profiles: &Table, change: &ProfileChange) -> Result<()> {
    let exists = profiles.contains_key(change.name.as_str());
    match (change.op, exists) {
        (ChangeOp::Create, true) => Err(XBackupError::Config(format!(
            "프로파일 '{}'이 이미 있습니다 — 다른 요청이 먼저 만들었을 수 있습니다. \
             화면을 새로고침한 뒤 다시 시도하세요.",
            change.name.as_str()
        ))),
        (ChangeOp::Update, false) => Err(XBackupError::Config(format!(
            "프로파일 '{}'을 찾을 수 없습니다 — 다른 요청이 먼저 지웠을 수 있습니다.",
            change.name.as_str()
        ))),
        _ => Ok(()),
    }
}

/// flat 키 하나가 쓰일 v1 중첩 경로. `dest`는 여러 경로로 갈라지므로(모듈 헤더 참조) 항상
/// `None`이다 — 호출부가 그 키를 [`apply_dest_v1`]로 먼저 가로챈다.
fn v1_path_for(key: &str) -> Option<&'static str> {
    if key == KEY_DEST {
        return None;
    }
    V1_PATHS
        .iter()
        .find(|(_, flat)| *flat == key)
        .map(|(path, _)| *path)
}

/// 점으로 구분된 경로를 따라가며 중간 테이블을 만들고 리프에 값을 쓴다.
///
/// 중간 노드가 테이블이 아니면 오류로 접는다(패닉하지 않는다) — 이 경로는 우리 자신의
/// 정적 상수([`V1_PATHS`])에서만 오므로 정상 상황에서는 항상 성공하지만, 저장 도중 파일이
/// 외부에서 바뀌는 경합(TOCTOU)까지 방어한다.
fn set_path(table: &mut Table, path: &str, value: Value) -> Result<()> {
    let segments: Vec<&str> = path.split('.').collect();
    let (last, parents) = segments
        .split_last()
        .expect("V1_PATHS의 경로는 항상 비어 있지 않은 상수 문자열이다");
    let mut current = table;
    for segment in parents {
        let next = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Table(Table::new()));
        current = next.as_table_mut().ok_or_else(|| {
            XBackupError::Config(format!(
                "config의 '{segment}'가 테이블이 아니어서 그 안에 값을 쓸 수 없습니다(파일이 \
                 저장 도중 바뀌었을 수 있습니다) — 다시 시도하세요."
            ))
        })?;
    }
    current.insert((*last).to_string(), value);
    Ok(())
}

/// 점 경로를 따라가며 리프를 지운다. 중간 노드가 없거나 테이블이 아니면 지울 것이 없다는
/// 뜻이므로 조용히 반환한다(오류가 아니다 — "이미 없는 키를 지워라"는 정상 요청이다).
fn remove_path(table: &mut Table, path: &str) {
    let segments: Vec<&str> = path.split('.').collect();
    let (last, parents) = segments
        .split_last()
        .expect("V1_PATHS의 경로는 항상 비어 있지 않은 상수 문자열이다");
    let mut current = table;
    for segment in parents {
        match current.get_mut(*segment).and_then(Value::as_table_mut) {
            Some(next) => current = next,
            None => return,
        }
    }
    current.remove(*last);
}

/// 점 경로가 가리키는 값(없으면 `None`). [`set_path`]·[`remove_path`]와 같은 경로 어휘를
/// 쓰는 읽기 쪽 짝이다.
///
/// `routes::config`에도 같은 모양의 `lookup_path`가 있다(출처 추적용). 한쪽으로 모으지 않은
/// 이유는 이 파일이 TOML 경로 조작(set/remove/get)을 한 묶음으로 들고 있어야 v1 쓰기 로직을
/// 한 화면에서 읽을 수 있기 때문이다 — 8줄짜리 순회를 공유하려고 모듈 경계를 넓히는 것보다
/// 낫다고 판단했다.
fn value_at<'a>(table: &'a Table, path: &str) -> Option<&'a Value> {
    let mut current = table;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let value = current.get(segment)?;
        if segments.peek().is_none() {
            return Some(value);
        }
        current = value.as_table()?;
    }
    None
}

// ---------------------------------------------------------------------------
// v1 → v2 전환 계획 (순수 함수 — 파일도, 자식 프로세스도 건드리지 않는다)
// ---------------------------------------------------------------------------

/// 전환 계획 — **미리보기와 실행이 같은 계산 결과를 쓴다.**
///
/// 필드가 비공개이고 생성자가 [`plan_v1_to_v2`] 하나뿐인 이유는 [`AuditReceipt`]와 같다:
/// 이 값의 [`new_text`](Self::new_text)는 곧 **운영자 config 파일을 통째로 덮어쓸 내용**이다.
/// 아무 문자열로 계획을 조립할 수 있으면 "전환"이라는 이름으로 임의의 파일을 쓰는 경로가
/// 생긴다. 유일한 생성자가 등가성까지 증명하므로, 이 타입을 손에 든 코드는 "값을 바꾸지
/// 않는 문법 변환"만 실행할 수 있다.
#[derive(Debug, Clone)]
pub struct ConversionPlan {
    /// 계획을 만든 **원본 텍스트**의 sha256(hex). 실행 시점에 파일을 다시 읽어 이 값과
    /// 비교한다 — 미리보기와 실제 파일이 같은 것임을 증명하는 유일한 근거다.
    digest: String,
    /// 원본 바이트 수(화면 요약용).
    original_bytes: usize,
    /// 쓰일 v2 텍스트.
    new_text: String,
    /// 프로파일별로 어느 nested 경로가 어느 flat 키로 옮겨지는가(미리보기 표).
    profiles: Vec<PlannedProfile>,
}

impl ConversionPlan {
    /// 원본 텍스트의 지문. 확인 폼이 이 값을 그대로 되돌려 보내야 실행된다.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// 쓰일 v2 텍스트(**원본 값 그대로** — 화면에 실으려면 호출부가 가려야 한다).
    pub fn new_text(&self) -> &str {
        &self.new_text
    }

    /// 프로파일별 이동 계획.
    pub fn profiles(&self) -> &[PlannedProfile] {
        &self.profiles
    }

    /// 원본 바이트 수.
    pub fn original_bytes(&self) -> usize {
        self.original_bytes
    }
}

/// 프로파일 하나에서 옮겨지는 키들.
#[derive(Debug, Clone)]
pub struct PlannedProfile {
    /// 프로파일 이름(config에 적힌 그대로).
    pub name: String,
    /// 이 프로파일의 이동 목록.
    pub moves: Vec<KeyMove>,
}

/// v1 nested 경로가 v2 flat 키로 옮겨진 한 건.
///
/// `value`는 **원본 값**이다 — 평문 `uri`처럼 자격증명이 섞여 있을 수 있으므로 화면에 실기
/// 전에 호출부가 가려야 한다(`routes::config`가 [`crate::web::view::config::RedactedUri`]와
/// 시크릿 레지스트리를 통과시킨다). 화면용 타입과 이 타입이 갈라져 있는 것이 요점이다 —
/// 마스킹을 건너뛴 값은 화면 모델에 들어갈 자리가 없다.
#[derive(Debug, Clone)]
pub struct KeyMove {
    /// v1 nested 경로(compact로 접히는 경우 여러 개를 쉼표로 잇는다).
    pub from: String,
    /// v2 flat 키.
    pub to: String,
    /// 옮겨지는 값(원본).
    pub value: Value,
}

/// 다중 destination 배열 항목의 v1 경로 → v2 키 대응.
///
/// 단일 destination과 달리 **compact로 접지 않는다** — v2의 `[[profile.x.dest]]` 항목은
/// `type`/`path`/`s3_*` 명시 키를 그대로 받으므로(`crate::config::v2`의 `expand_dest_array`)
/// 접을 이유가 없고, 접지 않는 쪽이 무손실이다.
const DEST_ARRAY_KEYS: &[(&str, &str)] = &[
    ("name", "name"),
    ("type", "type"),
    ("path", "path"),
    ("s3.bucket", "s3_bucket"),
    ("s3.prefix", "s3_prefix"),
    ("s3.region", "s3_region"),
    ("s3.endpoint", "s3_endpoint"),
    ("s3.credentials_env", "s3_creds"),
];

/// v1 텍스트를 v2 표면으로 옮기는 계획을 만든다 — **순수 함수.**
///
/// ## 왜 등가성을 스스로 증명하는가
/// 전환은 파일 전체를 다시 쓴다. 그 과정에서 키 하나가 조용히 사라지면 운영자는 "문법만
/// 바꿨다"고 믿는데 백업이 다른 곳에 쓰이거나 암호화가 꺼진다. 그래서 이 함수는 변환 결과를
/// **다시 로더에 먹여** 원본과 같은 [`Config`]가 되는지 비교하고, 다르면 거부한다. 이 게이트가
/// 있기 때문에 미래에 `config::file`에 필드가 추가되고 [`V1_PATHS`]가 갱신되지 않아도
/// "조용히 잃는" 대신 "전환을 거부"한다(fail-closed).
///
/// 비교는 JSON으로 한다 — TOML 직렬화는 `None`을 아예 생략해 "키가 사라진 것"과 "원래 없는
/// 것"을 구분하지 못하지만, JSON은 `null`로 남기므로 누락이 그대로 드러난다.
pub fn plan_v1_to_v2(original: &str) -> Result<ConversionPlan> {
    // 1) 값 해석과 모든 거부 판정(v1/v2 혼합·extends 순환·없는 base·파싱 오류)은 로더에
    //    맡긴다 — 웹이 자체 판정을 두지 않는다는 `routes::config` 헤더의 규약과 같다.
    let before = Config::from_toml_str(original)?;

    let root: Value = toml::from_str(original)
        .map_err(|e| XBackupError::Config(format!("config.toml 파싱 실패: {e}")))?;
    let table = root
        .as_table()
        .ok_or_else(|| XBackupError::Config("config 최상위가 테이블이 아닙니다".to_string()))?;

    let (new_root, profiles) = convert_root_to_v2(table)?;
    let new_text = toml::to_string(&Value::Table(new_root))
        .map_err(|e| XBackupError::Config(format!("전환 결과를 TOML로 쓰지 못했습니다: {e}")))?;

    // 2) 결과가 v2로 다시 읽히는지(미지 키 거부까지) 로더에게 물어본다.
    let after = Config::from_toml_str(&new_text)?;

    // 3) 등가성 게이트 — 값이 하나라도 달라지면 저장 경로에 넘기지 않는다.
    if let Some(path) = first_difference(&canonical(&before)?, &canonical(&after)?, "") {
        return Err(XBackupError::Config(format!(
            "전환이 값을 그대로 보존하지 못해 중단했습니다(달라지는 자리: {path}). \
             원본은 손대지 않았습니다 — 이 프로파일은 config 파일에서 직접 옮기세요."
        )));
    }

    Ok(ConversionPlan {
        digest: digest_of(original),
        original_bytes: original.len(),
        new_text,
        profiles,
    })
}

/// 루트 테이블을 v2 표면으로 옮긴다. `profiles` 외의 최상위 키(`default_profile`·`[output]`)는
/// **그대로 남긴다** — 전환의 대상은 프로파일 표현이지 파일 전체의 의미가 아니다.
fn convert_root_to_v2(root: &Table) -> Result<(Table, Vec<PlannedProfile>)> {
    if root.contains_key("profile") || root.contains_key("base") || root.contains_key("defaults") {
        return Err(XBackupError::Config(
            "이 config는 이미 v2 표면(profile/base/defaults)을 씁니다 — 전환할 것이 없습니다."
                .to_string(),
        ));
    }
    let profiles = root
        .get("profiles")
        .and_then(Value::as_table)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            XBackupError::Config(
                "이 config에는 [profiles.<name>] 프로파일이 없습니다 — 전환할 것이 없습니다."
                    .to_string(),
            )
        })?;

    let mut out = root.clone();
    out.remove("profiles");
    let mut converted = Table::new();
    let mut planned = Vec::with_capacity(profiles.len());
    for (name, value) in profiles {
        let nested = value.as_table().ok_or_else(|| {
            XBackupError::Config(format!("[profiles.{name}]이 테이블이 아닙니다"))
        })?;
        let (flat, moves) = flatten_profile(name, nested)?;
        converted.insert(name.clone(), Value::Table(flat));
        planned.push(PlannedProfile {
            name: name.clone(),
            moves,
        });
    }
    out.insert("profile".to_string(), Value::Table(converted));
    Ok((out, planned))
}

/// v1 프로파일 테이블 하나를 v2 flat 테이블로 접는다.
fn flatten_profile(name: &str, nested: &Table) -> Result<(Table, Vec<KeyMove>)> {
    reject_unmapped_paths(name, nested, &mapped_profile_paths(), "destinations")?;

    let mut flat = Table::new();
    let mut moves = Vec::new();

    // 1) 1:1 대응 키 — 표 순서대로 옮긴다(화면의 이동 표도 이 순서로 읽힌다).
    for (path, key) in V1_PATHS {
        if *key == KEY_DEST {
            continue; // compact로 접어야 하므로 아래에서 따로.
        }
        if let Some(value) = value_at(nested, path) {
            flat.insert((*key).to_string(), value.clone());
            moves.push(KeyMove {
                from: (*path).to_string(),
                to: (*key).to_string(),
                value: value.clone(),
            });
        }
    }

    // 2) destination — 단일(compact)이거나 배열이다. **둘 다 있으면 v2로 옮길 수 없다**:
    //    v2는 두 경우를 같은 `dest` 키 하나로 구분하므로(문자열이면 단일, 배열이면 다중)
    //    한 프로파일에서 둘을 동시에 표현할 문법이 없다.
    let single_has_values = nested
        .get("destination")
        .and_then(Value::as_table)
        .is_some_and(table_has_leaf);
    match nested.get("destinations").and_then(Value::as_array) {
        Some(items) if !items.is_empty() => {
            if single_has_values {
                return Err(XBackupError::Config(format!(
                    "[profiles.{name}]에 destination(단일)과 destinations(복수)가 함께 있습니다 \
                     — v2는 dest 키 하나로 둘을 구분하므로 동시에 표현할 수 없습니다. 하나로 \
                     정리한 뒤 다시 시도하세요."
                )));
            }
            let array = flatten_dest_array(name, items)?;
            let value = Value::Array(array);
            flat.insert(KEY_DEST.to_string(), value.clone());
            moves.push(KeyMove {
                from: "destinations[]".to_string(),
                to: "dest[]".to_string(),
                value,
            });
        }
        _ => fold_single_dest(name, nested, &mut flat, &mut moves)?,
    }

    Ok((flat, moves))
}

/// 단일 destination을 compact `dest` 한 줄로 접는다.
///
/// 접히지 않는 모양은 **거부한다** — v2에는 `destination.type`을 compact 밖에서 적을 문법이
/// 없어서, 접히지 않으면 백엔드 종류를 표현할 방법이 아예 없다. 조용히 버리면 백업 위치를
/// 잃은 config가 저장된다.
fn fold_single_dest(
    name: &str,
    nested: &Table,
    flat: &mut Table,
    moves: &mut Vec<KeyMove>,
) -> Result<()> {
    let kind = value_at(nested, "destination.type").and_then(Value::as_str);
    let path = value_at(nested, "destination.path").and_then(Value::as_str);
    let bucket = value_at(nested, "destination.s3.bucket").and_then(Value::as_str);
    let prefix = value_at(nested, "destination.s3.prefix").and_then(Value::as_str);

    let unsupported = |detail: String| {
        Err(XBackupError::Config(format!(
            "[profiles.{name}] destination을 v2 compact 표기로 옮길 수 없습니다 — {detail} \
             이 프로파일은 config 파일에서 직접 옮기세요(원본은 손대지 않았습니다)."
        )))
    };

    let compact = match kind {
        // type이 없으면 v1 로더도 이 destination을 쓰지 않는다(endpoint 전용 프로파일).
        // 그런데 path/bucket이 적혀 있으면 v2에는 그 값을 담을 자리가 없다 — 거부한다.
        None => {
            if path.is_some() || bucket.is_some() || prefix.is_some() {
                return unsupported(
                    "destination.type이 없는데 위치(path/s3.bucket)가 적혀 있습니다.".to_string(),
                );
            }
            return Ok(());
        }
        Some("local") => match path.filter(|p| !p.is_empty()) {
            Some(path) => format!("local:{path}"),
            None => return unsupported("type이 local인데 path가 없거나 비었습니다.".to_string()),
        },
        Some("s3") => match bucket.filter(|b| !b.is_empty()) {
            // 버킷 이름에 `/`가 들어가면 compact 표기에서 prefix와 구분되지 않는다(S3 버킷
            // 이름 규칙상 나올 수 없는 값이지만, 그렇다고 조용히 잘라 쓸 수는 없다).
            Some(bucket) if bucket.contains('/') => {
                return unsupported(format!("s3 버킷 이름에 '/'가 있습니다: {bucket}"))
            }
            Some(bucket) => match prefix.filter(|p| !p.is_empty()) {
                Some(prefix) => format!("s3:{bucket}/{prefix}"),
                None => format!("s3:{bucket}"),
            },
            None => return unsupported("type이 s3인데 bucket이 없거나 비었습니다.".to_string()),
        },
        Some(other) => {
            return unsupported(format!(
                "v2 compact 표기는 local|s3만 표현합니다 — type이 '{other}'입니다."
            ))
        }
    };

    let mut from: Vec<&str> = V1_DEST_PATHS
        .iter()
        .copied()
        .filter(|p| value_at(nested, p).is_some())
        .collect();
    // 빈 문자열 prefix는 compact에 담기지 않는다(`parse_dest_compact`가 빈 prefix를 무시한다)
    // — 명시 키로 따로 내보내야 값이 그대로 남는다.
    if prefix == Some("") {
        flat.insert("s3_prefix".to_string(), Value::String(String::new()));
        moves.push(KeyMove {
            from: "destination.s3.prefix".to_string(),
            to: "s3_prefix".to_string(),
            value: Value::String(String::new()),
        });
        from.retain(|p| *p != "destination.s3.prefix");
    }

    let value = Value::String(compact);
    flat.insert(KEY_DEST.to_string(), value.clone());
    moves.push(KeyMove {
        from: from.join(", "),
        to: KEY_DEST.to_string(),
        value,
    });
    Ok(())
}

/// v1 `destinations[]` 항목들을 v2 `[[profile.<name>.dest]]` 테이블 배열로 옮긴다.
fn flatten_dest_array(name: &str, items: &[Value]) -> Result<Vec<Value>> {
    let mapped: BTreeSet<String> = DEST_ARRAY_KEYS
        .iter()
        .map(|(path, _)| (*path).to_string())
        .collect();
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let nested = item.as_table().ok_or_else(|| {
            XBackupError::Config(format!(
                "[[profiles.{name}.destinations]] {index}번 항목이 테이블이 아닙니다"
            ))
        })?;
        reject_unmapped_paths(
            &format!("{name}.destinations[{index}]"),
            nested,
            &mapped,
            "",
        )?;
        let mut flat = Table::new();
        for (path, key) in DEST_ARRAY_KEYS {
            if let Some(value) = value_at(nested, path) {
                flat.insert((*key).to_string(), value.clone());
            }
        }
        out.push(Value::Table(flat));
    }
    Ok(out)
}

/// [`V1_PATHS`]가 아는 nested 경로 전부.
fn mapped_profile_paths() -> BTreeSet<String> {
    V1_PATHS
        .iter()
        .map(|(path, _)| (*path).to_string())
        .collect()
}

/// 대응표에 없는 경로가 있으면 거부한다.
///
/// **조용히 버리지 않는 것이 요점이다.** v1 로더는 모르는 키를 무시하지만(serde 기본 동작)
/// v2 로더는 거부한다. 그 차이 때문에, 대응되지 않는 키를 그냥 빼고 전환하면 "파일에 적어 둔
/// 줄이 사라졌는데 아무도 말해주지 않는" 상태가 된다 — 오타든 미래의 필드든, 운영자가 알아야
/// 고칠 수 있다.
fn reject_unmapped_paths(
    label: &str,
    nested: &Table,
    mapped: &BTreeSet<String>,
    extra_allowed: &str,
) -> Result<()> {
    let mut leaves = Vec::new();
    collect_leaf_paths(nested, "", &mut leaves);
    let unknown: Vec<String> = leaves
        .into_iter()
        .filter(|path| !mapped.contains(path) && path != extra_allowed)
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(XBackupError::Config(format!(
        "[profiles.{label}]의 키를 v2로 옮길 자리가 없습니다: {} — 웹 콘솔이 아는 키가 \
         아니어서(오타이거나 이 버전이 모르는 필드) 조용히 버리지 않고 전환을 중단했습니다. \
         원본은 손대지 않았습니다.",
        unknown.join(", ")
    )))
}

/// 테이블의 모든 리프(테이블이 아닌 값) 경로를 점 표기로 모은다. 빈 테이블은 리프를 만들지
/// 않는다 — 빈 `[destination]`은 "없는 것"과 같은 뜻이므로 옮길 것도 없다.
fn collect_leaf_paths(table: &Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(inner) => collect_leaf_paths(inner, &path, out),
            _ => out.push(path),
        }
    }
}

/// 테이블 안에 값(리프)이 하나라도 있는지.
fn table_has_leaf(table: &Table) -> bool {
    let mut leaves = Vec::new();
    collect_leaf_paths(table, "", &mut leaves);
    !leaves.is_empty()
}

/// [`Config`]를 비교 가능한 정규형(JSON)으로 접는다.
fn canonical(config: &Config) -> Result<serde_json::Value> {
    serde_json::to_value(config).map_err(|e| {
        XBackupError::Config(format!("전환 전후 비교를 위한 직렬화에 실패했습니다: {e}"))
    })
}

/// 두 정규형에서 **처음 달라지는 자리의 경로**를 찾는다(없으면 `None`).
///
/// 값 자체는 절대 담지 않는다 — 평문 `uri`가 달라진 경우 그 값이 오류 화면과 로그로 새어
/// 나가기 때문이다. 운영자에게 필요한 것은 "어디가 달라지는가"이고, 그건 경로가 말해준다.
fn first_difference(a: &serde_json::Value, b: &serde_json::Value, path: &str) -> Option<String> {
    let here = |segment: &str| {
        if path.is_empty() {
            segment.to_string()
        } else {
            format!("{path}.{segment}")
        }
    };
    match (a, b) {
        (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                match (x.get(key), y.get(key)) {
                    (Some(left), Some(right)) => {
                        if let Some(found) = first_difference(left, right, &here(key)) {
                            return Some(found);
                        }
                    }
                    _ => return Some(here(key)),
                }
            }
            None
        }
        (serde_json::Value::Array(x), serde_json::Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path}[] (개수 {} → {})", x.len(), y.len()));
            }
            x.iter()
                .zip(y.iter())
                .enumerate()
                .find_map(|(index, (left, right))| {
                    first_difference(left, right, &here(&index.to_string()))
                })
        }
        _ => {
            if a == b {
                None
            } else {
                Some(if path.is_empty() {
                    "(최상위)".to_string()
                } else {
                    path.to_string()
                })
            }
        }
    }
}

/// 텍스트의 sha256(hex). 미리보기와 실행이 같은 파일을 가리키는지 확인하는 데만 쓴다.
fn digest_of(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

// ---------------------------------------------------------------------------
// 저장 전 doctor 검증
// ---------------------------------------------------------------------------

/// doctor 검증이 저장을 막지 않는다고 판단했을 때 함께 실어 나르는 정보.
#[derive(Debug)]
struct DoctorOutcome {
    /// exit 4의 WARN 항목 메시지("<profile>/<label>: <message>" 형태). exit 0이면 비어
    /// 있다. 저장 후 화면([`ChangeReceipt::PersistedWithWarnings`])에 그대로 보여준다.
    warnings: Vec<String>,
}

/// 후보 config 텍스트를 doctor로 검증한다. 성공(exit 0/4)이면 경고 목록을, 실패(exit 2/3)나
/// 자식 실행 자체가 안 되면 `Err`를 돌려준다 — 호출부(`FileConfigStore::apply`)는 `Err`를
/// 받으면 저장을 진행하지 않는다.
fn validate_with_doctor(new_text: &str, doctor_exe: &Path) -> Result<DoctorOutcome> {
    let mut candidate = tempfile::Builder::new()
        .prefix("x-backup-config-validate-")
        .suffix(".toml")
        .tempfile()
        .map_err(XBackupError::Io)?;
    candidate
        .write_all(new_text.as_bytes())
        .map_err(XBackupError::Io)?;
    candidate.flush().map_err(XBackupError::Io)?;
    // candidate(NamedTempFile)는 이 함수가 끝날 때까지 살아 있어야 한다 — 자식이 그 경로를
    // 읽는 동안 파일이 존재해야 하기 때문이다. 함수가 반환하며 Drop이 지운다.

    let mut cmd = Command::new(doctor_exe);
    cmd.arg("doctor")
        .arg("--config")
        .arg(candidate.path())
        .arg("--json");
    // 자식이 어떤 이유로든 자기 환경을 찍어도(디버그 모드·크래시 리포트) 콘솔 세션 토큰이
    // 새지 않게 한다 — `routes::doctor::doctor_command`와 같은 방어.
    cmd.env_remove(crate::web::auth::ENV_WEB_TOKEN);
    cmd.env_remove(crate::web::auth::ENV_WEB_TOKEN_FILE);

    let output = run_with_timeout(cmd, SAVE_DOCTOR_TIMEOUT)?;
    map_doctor_output(&output)
}

/// 자식 종료 코드·stdout을 저장 판정으로 접는다 — **순수 함수**(테스트가 실제 자식 없이
/// 모든 분기를 확인할 수 있다).
fn map_doctor_output(output: &Output) -> Result<DoctorOutcome> {
    match Verdict::from_exit(output.status.code()) {
        Verdict::Clean => Ok(DoctorOutcome {
            warnings: Vec::new(),
        }),
        // exit 4는 경고 동반 **성공**이다 — 저장을 막지 않는다(`routes::doctor`의 같은
        // 판단, `Verdict::Warned`의 doc 참조). 저장은 진행하되 경고를 모아 화면에 남긴다.
        Verdict::Warned => {
            let report = doctor::parse_report(&output.stdout).map_err(|e| {
                XBackupError::Failure(format!(
                    "doctor가 경고(exit 4)로 끝났지만 출력을 해석하지 못해 저장을 \
                     보류합니다: {}",
                    e.explain(Lang::En)
                ))
            })?;
            Ok(DoctorOutcome {
                warnings: collect_by_level(&report, Level::Warn),
            })
        }
        Verdict::Blocked => Err(XBackupError::PrecheckFailed(format!(
            "저장하려는 값이 doctor 점검을 통과하지 못했습니다(exit 3): {}",
            describe_failure(output)
        ))),
        Verdict::Misconfigured => Err(XBackupError::Config(format!(
            "저장하려는 config를 doctor가 해석하지 못했습니다(exit 2): {}",
            describe_failure(output)
        ))),
        Verdict::Unexpected(code) => Err(XBackupError::Failure(format!(
            "doctor 검증이 예상치 못한 종료 상태({code:?})로 끝나 저장을 보류합니다: {}",
            describe_failure(output)
        ))),
    }
}

/// 보고서에서 특정 레벨(WARN 등)의 항목만 "<profile>/<label>: <message>"로 모은다.
fn collect_by_level(report: &doctor::Report, level: Level) -> Vec<String> {
    report
        .profiles
        .iter()
        .flat_map(|p| p.items.iter().map(move |item| (p.profile.as_str(), item)))
        .filter(|(_, item)| doctor::level_from_status(&item.status) == level)
        .map(|(profile, item)| format!("{profile}/{}: {}", item.label, item.message))
        .collect()
}

/// 실패(exit 2/3/기타) 상황을 사람이 읽을 문장으로 접는다. stdout을 보고서로 읽을 수
/// 있으면 OK가 아닌 항목을 모으고, 못 읽으면(자식이 JSON을 찍기 전에 죽은 경우 등) stderr
/// 발췌로 대신한다.
fn describe_failure(output: &Output) -> String {
    if let Ok(report) = doctor::parse_report(&output.stdout) {
        let bad: Vec<String> = report
            .profiles
            .iter()
            .flat_map(|p| p.items.iter().map(move |item| (p.profile.as_str(), item)))
            .filter(|(_, item)| doctor::level_from_status(&item.status) != Level::Ok)
            .map(|(profile, item)| format!("{profile}/{}: {}", item.label, item.message))
            .collect();
        if !bad.is_empty() {
            return bad.join("; ");
        }
    }
    excerpt(&String::from_utf8_lossy(&output.stderr))
}

/// 문자열을 [`STDERR_EXCERPT_CHARS`]로 자른다(문자 경계 — 멀티바이트를 깨지 않는다).
fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= STDERR_EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(STDERR_EXCERPT_CHARS).collect();
    format!("{head}…")
}

/// 커맨드를 상한 시간 안에 끝까지 돌린다(동기, tokio 없이).
///
/// [`ConfigStore::apply`]가 동기 트레이트 메서드라(t26이 그렇게 뒀고, 이 크레이트의
/// `Storage`/`ConfigStore`처럼 dyn으로 쓰는 트레이트를 async로 바꾸려면 `async_trait`
/// boxing이 필요해 파급이 커진다) 여기서도 tokio 없이 표준 라이브러리 `Command`로 처리한다.
/// 상한을 걸기 위해 별도 스레드에서 `wait_with_output`(파이프 드레인까지 표준 라이브러리가
/// 안전하게 처리한다 — 직접 `try_wait`를 폴링하면 자식이 파이프 버퍼를 채울 때 교착될 수
/// 있다)을 돌리고, 그 결과를 채널로 받는다. `recv_timeout`이 만료되면 pid로 직접
/// `SIGKILL`을 보낸다(`libc::kill` — `src/lock/file_lock.rs`가 stale lock 판정에 쓰는 것과
/// 같은 방식). 죽인 뒤에도 백그라운드 스레드는 `wait()`가 끝나며 자연히 정리된다(좀비 없음)
/// — 이 함수는 그 결과를 기다리지 않고 이미 반환한 뒤이므로 채널 송신은 조용히 버려진다.
fn run_with_timeout(mut cmd: Command, limit: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| XBackupError::Failure(format!("doctor 검증 자식을 띄우지 못했습니다: {e}")))?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(limit) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(XBackupError::Failure(format!(
            "doctor 검증 출력을 읽는 중 오류: {e}"
        ))),
        Err(_elapsed) => {
            #[cfg(unix)]
            // SAFETY: pid는 방금 spawn()이 돌려준, 아직 우리가 소유(reap 전)한 자식의
            // pid다. 값 자체를 신뢰할 수 없는 외부 입력이 아니라 std가 보증하는 값이라
            // SIGKILL 전송은 안전하다.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            #[cfg(not(unix))]
            let _ = pid; // 이 프로젝트는 darwin/linux만 배포 대상이다(release 스킬 참조).
            Err(XBackupError::Failure(format!(
                "doctor 검증이 {}초 안에 끝나지 않아 종료시켰습니다 — 저장을 보류합니다.",
                limit.as_secs()
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// 직전본 보관
// ---------------------------------------------------------------------------

/// `<config>.bak.<generation>` 경로. 숫자가 작을수록 최근 것이다(logrotate 관례와 같은
/// 방향) — `.bak.1`이 바로 직전 저장 전 상태, `.bak.5`가 가장 오래된 것.
fn backup_path(config_path: &Path, generation: usize) -> PathBuf {
    let mut name = config_path.as_os_str().to_os_string();
    name.push(format!(".bak.{generation}"));
    PathBuf::from(name)
}

/// 저장 직전 원본을 세대 1로 밀어 넣는다 — 기존 1..N을 2..N+1로 회전하고 가장 오래된
/// 세대(N)는 버린다. **저장이 실제로 일어날 때만 호출한다** — doctor 검증에 실패해 저장을
/// 포기하는 경로에서는 부르지 않는다(불필요한 회전으로 세대를 낭비하지 않기 위함).
fn rotate_backups(config_path: &Path) -> Result<()> {
    // 가장 오래된 것부터 지운다 — 반대 순서(새것부터 rename)로 하면 회전 도중 실패했을 때
    // 두 파일이 같은 이름을 다투는 상태가 남을 수 있다.
    let oldest = backup_path(config_path, BACKUP_GENERATIONS);
    if oldest.exists() {
        std::fs::remove_file(&oldest)?;
    }
    for generation in (1..BACKUP_GENERATIONS).rev() {
        let from = backup_path(config_path, generation);
        if from.exists() {
            std::fs::rename(&from, backup_path(config_path, generation + 1))?;
        }
    }
    // 원본을 세대 1로 **복사**한다(이동이 아니다) — 원본은 바로 뒤이어 [`write_atomic`]이
    // 새 내용으로 교체할 것이고, 이 함수는 그 전에 호출된다. `copy`는 원본 파일의 권한
    // 비트를 그대로 물려받으므로(`std::fs::copy` 문서) 백업 파일도 원본과 같은 접근 권한을
    // 갖는다.
    std::fs::copy(config_path, backup_path(config_path, 1))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 원자적 쓰기
// ---------------------------------------------------------------------------

/// 임시 파일에 쓰고 `fsync`한 뒤 `rename`하고, 마지막으로 디렉터리도 `fsync`한다.
///
/// ## 순서와 각 단계의 근거
/// 1. **원본과 같은 디렉터리에 임시 파일** — `rename(2)`은 같은 파일시스템 안에서만
///    원자적이다(POSIX). 다른 파일시스템(예: `/tmp`가 별도 마운트)에 쓰고 rename하면
///    커널·libc가 내부적으로 copy+delete로 대체할 수 있고, 그 사이 크래시가 나면 목적지가
///    반쯤 쓰인 채로 남는다. `tempfile::Builder::tempfile_in(부모 디렉터리)`로 이 전제를
///    강제한다.
/// 2. **내용을 fsync** — `rename(2)` 자체는 "디렉터리 엔트리 교체"만 원자적으로 보장하고,
///    그 이름이 가리키는 내용이 실제로 디스크에 있는지는 보장하지 않는다. 많은
///    파일시스템(ext4 등)에서 쓰기는 먼저 페이지 캐시에만 반영되므로, fsync 없이 rename만
///    하면 정전 시 "이름은 새 파일을 가리키지만 내용이 옛 캐시 상태(0바이트 또는 일부)"로
///    남을 수 있다. **이 fsync를 빠뜨리면 원자적 rename이라는 성질이 무의미해진다** —
///    이름 교체는 원자적이어도 그 이름이 가리키는 내용이 완전하다는 보장이 없기 때문이다.
/// 3. **rename** — 이제서야 이름을 바꾼다. 이 시점부터 원본 경로를 읽는 어떤 프로세스도
///    "완전한 구버전" 또는 "완전한 신버전" 중 하나만 본다(중간 상태 없음).
/// 4. **디렉터리 fsync** — rename이 바꾸는 것은 디렉터리 엔트리이므로, 그 디렉터리
///    자체의 메타데이터도 fsync해야 정전 시 "파일 내용은 안전한데 디렉터리가 옛 이름을
///    계속 가리키는" 상태를 막는다(주로 ext4류에서 관측되는 실패 모드 — 디렉터리 엔트리
///    변경은 파일 데이터 fsync와 별개의 디스크 쓰기이기 때문이다). 이 단계는 **최선
///    노력**이다 — 실패해도 파일 내용은 이미 fsync됐으므로 저장 자체를 막을 이유가 없고
///    경고만 남긴다. `#[cfg(unix)]`인 이유: 디렉터리를 `File::open`으로 열어 fsync하는
///    것은 POSIX 관용이고, 이 프로젝트의 배포 대상은 darwin/linux뿐이다.
fn write_atomic(target: &Path, contents: &[u8]) -> Result<()> {
    let dir = target.parent().ok_or_else(|| {
        XBackupError::Config(format!(
            "config 경로에 상위 디렉터리가 없습니다: {}",
            target.display()
        ))
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".x-backup-config-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(XBackupError::Io)?;
    tmp.write_all(contents).map_err(XBackupError::Io)?;
    tmp.as_file().sync_all().map_err(XBackupError::Io)?;

    // 원본이 있으면 권한을 물려받는다 — tempfile의 기본 권한(0600)을 강제로 얹지 않는다.
    // 실패해도 저장을 막지 않는다(권한을 못 맞추는 것이 내용을 위험하게 만들지는 않고,
    // 운영자가 나중에 chmod로 고칠 수 있다).
    if let Ok(meta) = std::fs::metadata(target) {
        let _ = std::fs::set_permissions(tmp.path(), meta.permissions());
    }

    tmp.persist(target).map_err(|e| XBackupError::Io(e.error))?;

    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(dir).and_then(|f| f.sync_all()) {
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "config 디렉터리 fsync 실패 — 파일 내용은 이미 안전하다(최선 노력 단계)"
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 저장소
// ---------------------------------------------------------------------------

/// [`ConfigStore`]의 프로덕션 구현. `routes::config::apply_and_render`가 요청마다
/// `ctx.config_path`로 새로 만들어 쓴다(`JobRunner`처럼 기동 시점에 한 번 만들어 캐싱하지
/// 않는다 — 이 값은 요청 하나의 저장 한 번에만 쓰이고, `routes::doctor`가 매 요청마다
/// `doctor_exe()`를 새로 묻는 것과 같은 결로 유지한다).
pub struct FileConfigStore {
    config_path: PathBuf,
    /// 검증에 실행할 바이너리 경로. `None`이면 매 호출마다 [`doctor::doctor_exe`]
    /// (=현재 실행 중인 이 바이너리 자신)를 다시 묻는다.
    doctor_exe_override: Option<PathBuf>,
}

impl FileConfigStore {
    /// 실제 저장소를 만든다.
    pub fn new(config_path: PathBuf) -> Self {
        Self {
            config_path,
            doctor_exe_override: None,
        }
    }

    /// 검증에 쓸 바이너리를 직접 지정한다 — **테스트 전용**
    /// ([`crate::web::job::runner::JobRunner::with_exe`]와 같은 이유: `current_exe()`는
    /// 테스트에서 테스트 하니스 자신을 가리키므로, 실제 `x-backup` 바이너리로 doctor를
    /// 검증하려면 그 경로를 직접 주입해야 한다).
    #[cfg(test)]
    pub(crate) fn with_doctor_exe(config_path: PathBuf, doctor_exe: PathBuf) -> Self {
        Self {
            config_path,
            doctor_exe_override: Some(doctor_exe),
        }
    }

    fn resolve_doctor_exe(&self) -> Result<PathBuf> {
        match &self.doctor_exe_override {
            Some(path) => Ok(path.clone()),
            None => doctor::doctor_exe().map_err(|e| XBackupError::Config(e.explain(Lang::En))),
        }
    }
}

impl ConfigStore for FileConfigStore {
    /// `audit`를 값으로 받아 **소비만 한다** — 읽을 것이 없다(필드가 없는 증표 타입이다).
    /// 이 파라미터의 존재가 곧 "이 호출 직전에 감사 로그 append가 성공했다"는 전제조건이고,
    /// 그 전제를 문법 수준에서 강제하는 것이 목적이다([`ConfigStore`] doc). 이 함수 안에서는
    /// 감사 로그를 다시 만지지 않는다 — 기록의 주체는 요청 핸들러 하나여야 한다.
    fn apply(&self, change: &ProfileChange, audit: AuditReceipt) -> Result<ChangeReceipt> {
        let _audit: AuditReceipt = audit;
        if change.is_noop() {
            // 바뀌는 것이 없다 — 파일도, 백업도, doctor 자식도 건드리지 않는다. 파일은
            // 이미 요청받은 상태와 일치하므로 Persisted를 돌려준다(그러지 않으면 "무변경
            // 저장"을 눌렀을 때 화면이 "config 파일은 아직 쓰이지 않았습니다"라는 혼란스러운
            // 경고를 낸다 — 사실 쓸 것이 없을 뿐 검증 미배선 상태가 아니다).
            return Ok(ChangeReceipt::Persisted);
        }

        let original = std::fs::read_to_string(&self.config_path)?;
        let mut root: Value = toml::from_str(&original).map_err(|e| {
            XBackupError::Config(format!(
                "config를 다시 읽는 중 실패했습니다(그 사이 파일이 바뀌었을 수 있습니다): {e}"
            ))
        })?;
        {
            let table = root.as_table_mut().ok_or_else(|| {
                XBackupError::Config("config 최상위가 테이블이 아닙니다".to_string())
            })?;
            apply_change_to_table(table, change)?;
        }
        let new_text = toml::to_string(&root).map_err(|e| {
            XBackupError::Config(format!("변경을 TOML로 직렬화하지 못했습니다: {e}"))
        })?;

        let doctor_exe = self.resolve_doctor_exe()?;
        let outcome = validate_with_doctor(&new_text, &doctor_exe)?;

        rotate_backups(&self.config_path)?;
        write_atomic(&self.config_path, new_text.as_bytes())?;

        Ok(if outcome.warnings.is_empty() {
            ChangeReceipt::Persisted
        } else {
            ChangeReceipt::PersistedWithWarnings(outcome.warnings)
        })
    }

    /// v1 → v2 전환을 적용한다 — **파일 전체를 다시 쓴다.**
    ///
    /// 프로파일 하나를 고치는 [`apply`](Self::apply)보다 파괴적이므로 같은 세 겹(직전본 회전 →
    /// doctor 검증 → 원자적 교체)을 그대로 지나고, 그 앞에 **지문 확인**이 하나 더 붙는다:
    /// 계획을 만든 원본과 지금 디스크에 있는 파일이 같아야 한다. 그러지 않으면 운영자가 화면에서
    /// 본 diff와 실제로 덮어쓰는 대상이 다른 파일이 된다(미리보기와 확인 사이에 다른 요청이
    /// 저장을 마쳤거나, 누군가 편집기로 파일을 고친 경우 — 이 화면의 TOCTOU 창이다).
    fn convert_to_v2(&self, plan: &ConversionPlan, audit: AuditReceipt) -> Result<ChangeReceipt> {
        let _audit: AuditReceipt = audit;
        let current = std::fs::read_to_string(&self.config_path)?;
        if digest_of(&current) != plan.digest {
            return Err(XBackupError::Config(
                "미리보기를 만든 뒤 config 파일이 바뀌었습니다 — 전환하지 않았습니다. \
                 전환 화면을 새로 열어 바뀐 내용으로 다시 확인하세요."
                    .to_string(),
            ));
        }

        let doctor_exe = self.resolve_doctor_exe()?;
        let outcome = validate_with_doctor(&plan.new_text, &doctor_exe)?;

        rotate_backups(&self.config_path)?;
        write_atomic(&self.config_path, plan.new_text.as_bytes())?;

        Ok(if outcome.warnings.is_empty() {
            ChangeReceipt::Persisted
        } else {
            ChangeReceipt::PersistedWithWarnings(outcome.warnings)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::job::args::ProfileName;
    use crate::web::routes::config::ProfileOrigins;
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::process::ExitStatusExt;

    fn output_with_code(code: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            // `code << 8`은 유닉스 `wait()` 상태 워드의 관례(하위 바이트는 시그널 정보)다 —
            // `ExitStatusExt::from_raw`가 그 워드를 받는다.
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    /// `ConfigStore::apply`에 넘길 감사 영수증을 만든다.
    ///
    /// [`AuditReceipt`]는 [`crate::web::audit::AuditLog::gate`]가 append에 성공했을 때만
    /// 만들어지고 그 모듈 밖에서는 생성할 방법이 없다 — 그게 이 시그니처의 요점이다
    /// ([`ConfigStore`] doc). 그래서 테스트도 우회하지 않고 실제로 임시 디렉터리에 감사
    /// 로그를 열어 게이트를 통과한다. `gate`가 async이고 이 테스트들은 동기이므로 최소
    /// 런타임을 하나 세워 그 안에서만 기다린다(`#[tokio::test]`로 바꾸면 저장 경로가
    /// 블로킹 I/O를 하는 이 테스트들이 런타임 위에서 돌게 되어 성질이 달라진다).
    fn test_receipt() -> AuditReceipt {
        let dir = tempfile::tempdir().expect("임시 디렉터리 생성 실패");
        let log = crate::web::audit::AuditLog::open(dir.path()).expect("감사 로그 열기 실패");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("런타임 생성 실패")
            .block_on(log.gate("test", "config.save", "p", &[]))
            .expect("정상 경로에서 게이트는 통과해야 함")
    }

    fn change(op: ChangeOp, syntax: ConfigSyntax) -> ProfileChange {
        ProfileChange {
            op,
            name: ProfileName::parse("p", Lang::En).unwrap(),
            syntax,
            set: BTreeMap::new(),
            unset: BTreeSet::new(),
            broke_inheritance: Vec::new(),
            origins: ProfileOrigins::default(),
        }
    }

    fn set_change(syntax: ConfigSyntax, pairs: &[(&str, Value)]) -> ProfileChange {
        let mut c = change(ChangeOp::Update, syntax);
        for (k, v) in pairs {
            c.set.insert((*k).to_string(), v.clone());
        }
        c
    }

    // -- map_doctor_output: 순수 판정 분기 -----------------------------------

    const OK_JSON: &str = r#"{"schema":1,"overall":"ok","profiles":[
        {"profile":"p","db":"mongodb","items":[{"label":"source","status":"ok","message":"resolved"}]}]}"#;
    const WARN_JSON: &str = r#"{"schema":1,"overall":"warn","profiles":[
        {"profile":"p","db":"mongodb","items":[
            {"label":"source","status":"ok","message":"resolved"},
            {"label":"encryption","status":"warn","message":"disabled — plaintext"}]}]}"#;
    const FAIL_JSON: &str = r#"{"schema":1,"overall":"fail","profiles":[
        {"profile":"p","db":"mongodb","items":[
            {"label":"encryption","status":"fail","message":"recipient_file missing"}]}]}"#;

    /// exit 0 — 경고 없이 통과.
    #[test]
    fn exit_0_is_clean_with_no_warnings() {
        let out = output_with_code(0, OK_JSON.as_bytes(), b"");
        let outcome = map_doctor_output(&out).expect("exit 0은 저장을 막지 않아야 함");
        assert!(outcome.warnings.is_empty());
    }

    /// **exit 4는 저장을 막지 않는다** — 경고 목록을 실어 돌려준다. 이것이 "exit 4에서
    /// 저장을 허용했음을 어떻게 고정했는지"의 단위 테스트 층이다(통합 계층은 아래
    /// `real_doctor_*` 테스트가 맡는다).
    #[test]
    fn exit_4_persists_and_carries_warning_text() {
        let out = output_with_code(4, WARN_JSON.as_bytes(), b"");
        let outcome = map_doctor_output(&out).expect("exit 4는 경고 동반 성공이라 막으면 안 됨");
        assert_eq!(outcome.warnings.len(), 1);
        assert!(
            outcome.warnings[0].contains("plaintext"),
            "{:?}",
            outcome.warnings
        );
    }

    /// exit 3은 저장을 막고, 실패 사유에 FAIL 항목 메시지가 들어간다.
    #[test]
    fn exit_3_blocks_with_fail_detail() {
        let out = output_with_code(3, FAIL_JSON.as_bytes(), b"");
        let err = map_doctor_output(&out).expect_err("exit 3은 막아야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::PRECHECK);
        assert!(err.to_string().contains("recipient_file missing"));
    }

    /// exit 2는 저장을 막는다(설정 오류로 분류).
    #[test]
    fn exit_2_blocks_as_config_error() {
        let out = output_with_code(2, b"", b"usage error on stderr");
        let err = map_doctor_output(&out).expect_err("exit 2는 막아야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
    }

    /// 시그널 종료(코드 없음)·미지 코드도 저장을 막는다(fail-closed) — 조용히 통과시키지
    /// 않는다.
    #[test]
    fn signal_and_unknown_exit_are_blocked_fail_closed() {
        assert!(map_doctor_output(&output_with_code(9, b"", b"killed")).is_err());
        let signaled = Output {
            status: std::process::ExitStatus::from_raw(9), // 시그널로 죽음(코드 없음)
            stdout: Vec::new(),
            stderr: b"segv".to_vec(),
        };
        assert!(map_doctor_output(&signaled).is_err());
    }

    /// exit 4인데 stdout이 깨진 JSON이면(있어서는 안 되지만) 저장을 막는다 — 경고 목록을
    /// 신뢰할 수 없는 상태로 저장을 진행하지 않는다.
    #[test]
    fn exit_4_with_unparseable_stdout_blocks() {
        let out = output_with_code(4, b"not json", b"");
        assert!(map_doctor_output(&out).is_err());
    }

    // -- run_with_timeout: 실제 프로세스 메커니즘 ----------------------------

    /// 존재하지 않는 실행 파일은 명확한 실패로 접힌다(패닉·무한 대기 없음).
    #[test]
    fn run_with_timeout_reports_spawn_failure() {
        let cmd = Command::new("/nonexistent/x-backup-config-write-9f21");
        let err =
            run_with_timeout(cmd, Duration::from_secs(5)).expect_err("없는 파일이 성공하면 안 됨");
        assert!(err.to_string().contains("띄우지 못했습니다"));
    }

    /// 상한을 넘기면 자식을 죽이고 명확한 타임아웃 오류로 끝난다 — 무한 대기하지 않는다.
    #[test]
    fn run_with_timeout_kills_and_reports_on_expiry() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 5");
        let limit = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let err = run_with_timeout(cmd, limit).expect_err("타임아웃이 걸리지 않았다");
        assert!(err.to_string().contains("종료시켰습니다"));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "상한을 기다리지 않고 자식이 끝날 때까지 매달렸다: {:?}",
            started.elapsed()
        );
    }

    // -- 순수 TOML 편집: v2 -----------------------------------------------

    /// v2 update — 손댄 키만 바뀌고, `[defaults]`/`[base.*]`/다른 프로파일은 그대로다.
    #[test]
    fn v2_update_touches_only_the_named_profile() {
        let text = r#"
[defaults]
compress = "zstd:3"

[base.s3prod]
dest = "s3:bucket/prefix"

[profile.p]
extends = "s3prod"
uri_env = "U"
keep_full = 3

[profile.q]
uri = "mongodb://h/q"
"#;
        let mut root: Value = toml::from_str(text).unwrap();
        let change = set_change(ConfigSyntax::V2, &[("keep_full", Value::Integer(9))]);
        apply_change_to_table(root.as_table_mut().unwrap(), &{
            let mut c = change;
            c.name = ProfileName::parse("p", Lang::En).unwrap();
            c
        })
        .unwrap();
        let out = toml::to_string(&root).unwrap();
        assert!(out.contains("keep_full = 9"), "{out}");
        assert!(
            out.contains("[defaults]") && out.contains("compress"),
            "{out}"
        );
        assert!(out.contains("[base.s3prod]"), "{out}");
        assert!(out.contains("extends"), "extends가 사라졌다:\n{out}");
        assert!(
            out.contains("[profile.q]"),
            "남의 프로파일이 사라졌다:\n{out}"
        );
    }

    /// v2 create — 새 `[profile.<name>]`이 생기고 기존 섹션은 그대로다.
    #[test]
    fn v2_create_adds_a_new_profile_section() {
        let text = "[profile.p]\nuri = \"mongodb://h/p\"\n";
        let mut root: Value = toml::from_str(text).unwrap();
        let mut c = change(ChangeOp::Create, ConfigSyntax::V2);
        c.name = ProfileName::parse("fresh", Lang::En).unwrap();
        c.set
            .insert("uri_env".to_string(), Value::String("F".to_string()));
        apply_change_to_table(root.as_table_mut().unwrap(), &c).unwrap();
        let out = toml::to_string(&root).unwrap();
        assert!(out.contains("[profile.fresh]"), "{out}");
        assert!(out.contains("[profile.p]"), "{out}");
    }

    /// v2 create가 이미 있는 이름과 부딪히면(동시 편집 경합 시뮬레이션) 조용히 병합하지
    /// 않고 거부한다.
    #[test]
    fn v2_create_rejects_when_name_already_exists_concurrently() {
        let text = "[profile.p]\nuri = \"mongodb://h/p\"\n";
        let mut root: Value = toml::from_str(text).unwrap();
        let mut c = change(ChangeOp::Create, ConfigSyntax::V2);
        c.name = ProfileName::parse("p", Lang::En).unwrap();
        c.set
            .insert("uri_env".to_string(), Value::String("X".to_string()));
        let err = apply_change_to_table(root.as_table_mut().unwrap(), &c)
            .expect_err("이미 있는 이름의 Create는 거부되어야 함");
        assert!(err.to_string().contains("이미 있습니다"));
    }

    /// v2 update가 이미 지워진 프로파일을 겨냥하면(동시 삭제 경합) 되살리지 않고 거부한다.
    #[test]
    fn v2_update_rejects_when_profile_already_deleted_concurrently() {
        let text = "[profile.q]\nuri = \"mongodb://h/q\"\n";
        let mut root: Value = toml::from_str(text).unwrap();
        let c = set_change(ConfigSyntax::V2, &[("keep_full", Value::Integer(1))]);
        let err = apply_change_to_table(root.as_table_mut().unwrap(), &c)
            .expect_err("없는 프로파일의 Update는 거부되어야 함");
        assert!(err.to_string().contains("찾을 수 없습니다"));
    }

    /// v2 delete — 대상만 사라지고 나머지는 그대로다.
    #[test]
    fn v2_delete_removes_only_the_named_profile() {
        let text = "[profile.p]\nuri = \"mongodb://h/p\"\n[profile.q]\nuri = \"mongodb://h/q\"\n";
        let mut root: Value = toml::from_str(text).unwrap();
        let c = change(ChangeOp::Delete, ConfigSyntax::V2);
        apply_change_to_table(root.as_table_mut().unwrap(), &c).unwrap();
        let out = toml::to_string(&root).unwrap();
        assert!(!out.contains("[profile.p]"), "{out}");
        assert!(out.contains("[profile.q]"), "{out}");
    }

    // -- 순수 TOML 편집: v1 -------------------------------------------------

    /// v1 update — flat 키가 중첩 경로에 정확히 꽂히고, 다른 값은 그대로다.
    #[test]
    fn v1_update_writes_the_nested_path_and_preserves_the_rest() {
        let text = r#"
[profiles.p.source]
uri_env = "U"

[profiles.p.retention]
keep_full = 3
"#;
        let mut root: Value = toml::from_str(text).unwrap();
        let c = set_change(
            ConfigSyntax::V1,
            &[
                ("keep_days", Value::Integer(14)),
                ("compress_level", Value::Integer(6)),
            ],
        );
        apply_change_to_table(root.as_table_mut().unwrap(), &c).unwrap();
        let doc =
            crate::web::routes::config::ConfigDocument::parse(&toml::to_string(&root).unwrap())
                .expect("결과가 다시 파싱되어야 함");
        assert_eq!(doc.syntax, ConfigSyntax::V1);
        let p = doc.config.profile("p").unwrap();
        assert_eq!(p.retention.keep_days, Some(14));
        assert_eq!(p.retention.keep_full, Some(3), "손대지 않은 값이 사라졌다");
        assert_eq!(p.features.compression.level, 6);
        assert_eq!(p.source.uri_env.as_deref(), Some("U"));
    }

    /// v1 unset — 지운 키만 사라지고 나머지는 그대로다.
    #[test]
    fn v1_unset_removes_only_the_named_key() {
        let text = "[profiles.p.source]\nuri_env = \"U\"\n[profiles.p.retention]\nkeep_full = 3\nkeep_days = 7\n";
        let mut root: Value = toml::from_str(text).unwrap();
        let mut c = change(ChangeOp::Update, ConfigSyntax::V1);
        c.unset.insert("keep_full".to_string());
        apply_change_to_table(root.as_table_mut().unwrap(), &c).unwrap();
        let out = toml::to_string(&root).unwrap();
        assert!(!out.contains("keep_full"), "{out}");
        assert!(out.contains("keep_days"), "{out}");
    }

    /// 변경을 적용한 뒤 결과를 다시 문서로 읽는다(테스트 편의).
    fn applied(text: &str, change: &ProfileChange) -> crate::web::routes::config::ConfigDocument {
        let mut root = Value::Table(toml::from_str(text).unwrap_or_default());
        apply_change_to_table(root.as_table_mut().unwrap(), change)
            .unwrap_or_else(|e| panic!("적용 실패: {e}"));
        let out = toml::to_string(&root).unwrap();
        crate::web::routes::config::ConfigDocument::parse(&out)
            .unwrap_or_else(|e| panic!("결과가 다시 파싱되어야 함: {e:?}\n--- text ---\n{out}"))
    }

    fn dest_change(compact: &str) -> ProfileChange {
        set_change(
            ConfigSyntax::V1,
            &[(KEY_DEST, Value::String(compact.to_string()))],
        )
    }

    /// **v1 compact `dest`는 이제 거부되지 않고 nested 경로로 펼쳐진다**(t27은 이 변경을
    /// 422로 막았다 — 모듈 헤더 참조). local과 s3이 서로 다른 경로 조합에 쓰인다.
    #[test]
    fn v1_dest_expands_into_the_nested_paths() {
        let text = "[profiles.p.source]\nuri_env = \"U\"\n";

        let local = applied(text, &dest_change("local:/srv/b"));
        assert_eq!(local.syntax, ConfigSyntax::V1, "v1이 v2로 새어 나갔다");
        let p = local.config.profile("p").unwrap();
        assert_eq!(p.destination.r#type.as_deref(), Some("local"));
        assert_eq!(p.destination.path.as_deref(), Some("/srv/b"));
        assert!(p.destination.s3.is_none(), "local에 s3 테이블이 생겼다");

        let s3 = applied(text, &dest_change("s3:bucket/mongo/prod"));
        let p = s3.config.profile("p").unwrap();
        assert_eq!(p.destination.r#type.as_deref(), Some("s3"));
        assert_eq!(p.destination.path, None);
        let s3cfg = p.destination.s3.as_ref().expect("s3 테이블이 있어야 함");
        assert_eq!(s3cfg.bucket.as_deref(), Some("bucket"));
        assert_eq!(
            s3cfg.prefix.as_deref(),
            Some("mongo/prod"),
            "첫 '/' 뒤 전체가 prefix여야 한다"
        );

        // prefix 없는 s3는 bucket만 쓴다(빈 prefix를 남기지 않는다).
        let bare = applied(text, &dest_change("s3:bucket"));
        let s3cfg = bare
            .config
            .profile("p")
            .unwrap()
            .destination
            .s3
            .as_ref()
            .unwrap()
            .clone();
        assert_eq!(s3cfg.bucket.as_deref(), Some("bucket"));
        assert_eq!(s3cfg.prefix, None);
    }

    /// **스킴을 바꾸면 반대쪽 위치 키가 남지 않는다.** 남으면 파일에 위치가 두 개 적힌 것처럼
    /// 보이고, 읽는 사람이 어느 쪽이 사는 값인지 추측해야 한다.
    #[test]
    fn v1_dest_switching_scheme_clears_the_other_location() {
        let s3_original = "\
[profiles.p.source]
uri_env = \"U\"

[profiles.p.destination]
type = \"s3\"

[profiles.p.destination.s3]
bucket = \"old-bucket\"
prefix = \"old/prefix\"
region = \"ap-northeast-2\"
";
        let doc = applied(s3_original, &dest_change("local:/srv/new"));
        let p = doc.config.profile("p").unwrap();
        assert_eq!(p.destination.r#type.as_deref(), Some("local"));
        assert_eq!(p.destination.path.as_deref(), Some("/srv/new"));
        let s3 = p.destination.s3.as_ref().expect("region은 남아야 한다");
        assert_eq!(s3.bucket, None, "옛 버킷이 남았다");
        assert_eq!(s3.prefix, None, "옛 prefix가 남았다");
        // region은 별도 flat 키(`s3_region`)를 가지므로 이 변경의 대상이 아니다 — 지우라고
        // 하지 않은 키를 지우는 것도 조용한 손실이다.
        assert_eq!(s3.region.as_deref(), Some("ap-northeast-2"));

        // 반대 방향 — local에서 s3로 바꾸면 path가 사라진다.
        let local_original = "\
[profiles.p.destination]
type = \"local\"
path = \"/srv/old\"
";
        let doc = applied(local_original, &dest_change("s3:b/p"));
        let p = doc.config.profile("p").unwrap();
        assert_eq!(p.destination.r#type.as_deref(), Some("s3"));
        assert_eq!(p.destination.path, None, "옛 local 경로가 남았다");
    }

    /// **`dest`를 지우면 nested 4경로가 전부 사라지고, 로더는 그 파일을 여전히 읽는다.**
    ///
    /// 남은 destination 테이블이 비면 지운다 — serde 기준으로 빈 테이블과 없는 테이블이 같은
    /// 뜻이라 의미는 그대로이고(프로파일이 endpoint 전용이 된다) 유령 섹션만 없어진다.
    #[test]
    fn v1_dest_unset_removes_every_nested_path_and_the_file_still_loads() {
        let text = "\
[profiles.p.source]
uri_env = \"U\"

[profiles.p.destination]
type = \"s3\"

[profiles.p.destination.s3]
bucket = \"b\"
prefix = \"p\"
";
        let mut c = change(ChangeOp::Update, ConfigSyntax::V1);
        c.unset.insert(KEY_DEST.to_string());
        let doc = applied(text, &c);
        let p = doc.config.profile("p").unwrap();
        assert_eq!(p.destination.r#type, None);
        assert_eq!(p.destination.path, None);
        assert!(p.destination.s3.is_none());
        assert!(
            p.is_endpoint_only(),
            "destination을 지웠으면 endpoint 전용이 된다"
        );
        assert_eq!(
            p.source.uri_env.as_deref(),
            Some("U"),
            "손대지 않은 값이 사라졌다"
        );

        // region이 남아 있으면 s3 테이블은 지우지 않는다(운영자가 적어 둔 값이다).
        let with_region = format!("{text}region = \"ap-northeast-2\"\n");
        let doc = applied(&with_region, &c);
        let p = doc.config.profile("p").unwrap();
        assert!(p.is_endpoint_only(), "위치가 없으면 endpoint 전용이다");
        assert_eq!(
            p.destination
                .s3
                .as_ref()
                .and_then(|s3| s3.region.as_deref()),
            Some("ap-northeast-2")
        );
    }

    /// s3 부속 키(region·endpoint·creds)가 각자의 v1 nested 경로에 쓰인다 — compact `dest`와
    /// 섞이지 않는다.
    #[test]
    fn v1_s3_side_keys_write_their_own_nested_paths() {
        let doc = applied(
            "[profiles.p.source]\nuri_env = \"U\"\n",
            &set_change(
                ConfigSyntax::V1,
                &[
                    (KEY_DEST, Value::String("s3:bucket/prefix".to_string())),
                    ("s3_region", Value::String("ap-northeast-2".to_string())),
                    (
                        "s3_endpoint",
                        Value::String("https://s3.example.com".to_string()),
                    ),
                    ("s3_creds", Value::String("S3_CREDS".to_string())),
                    ("dest_name", Value::String("primary".to_string())),
                ],
            ),
        );
        let dest = &doc.config.profile("p").unwrap().destination;
        assert_eq!(dest.name.as_deref(), Some("primary"));
        let s3 = dest.s3.as_ref().unwrap();
        assert_eq!(s3.bucket.as_deref(), Some("bucket"));
        assert_eq!(s3.prefix.as_deref(), Some("prefix"));
        assert_eq!(s3.region.as_deref(), Some("ap-northeast-2"));
        assert_eq!(s3.endpoint.as_deref(), Some("https://s3.example.com"));
        assert_eq!(s3.credentials_env.as_deref(), Some("S3_CREDS"));
    }

    /// 폼 밖에서 만든 잘못된 compact 표기는 **조용히 통과하지 않는다**(422에 해당).
    #[test]
    fn v1_malformed_dest_is_refused() {
        for bad in ["nocolon", "local:", "s3:", "ftp://host/p"] {
            let mut root: Value = toml::from_str("[profiles.p.source]\nuri_env=\"U\"\n").unwrap();
            let err = apply_change_to_table(root.as_table_mut().unwrap(), &dest_change(bad))
                .expect_err("잘못된 compact 표기는 거부되어야 함");
            assert_eq!(
                err.exit_code(),
                crate::error::exit_codes::USAGE,
                "'{bad}': {err}"
            );
        }
    }

    /// **[`V1_DEST_PATHS`]와 [`V1_PATHS`]가 어긋나지 않는다.** 한쪽만 늘어나면 읽기(출처
    /// 추적)와 쓰기(이 파일)가 다른 경로를 보게 되고, 그 순간 왕복이 깨진다.
    #[test]
    fn v1_dest_paths_match_the_lookup_table() {
        let from_table: BTreeSet<&str> = V1_PATHS
            .iter()
            .filter(|(_, flat)| *flat == KEY_DEST)
            .map(|(path, _)| *path)
            .collect();
        let ours: BTreeSet<&str> = V1_DEST_PATHS.iter().copied().collect();
        assert_eq!(
            ours, from_table,
            "compact dest가 갈라지는 경로 집합이 대응표와 다르다"
        );
    }

    /// v1 create — `[profiles.<name>]`이 새로 생긴다.
    #[test]
    fn v1_create_adds_a_new_nested_profile() {
        let mut root: Value = toml::from_str("").unwrap();
        let mut c = change(ChangeOp::Create, ConfigSyntax::V1);
        c.set
            .insert("uri_env".to_string(), Value::String("U".to_string()));
        apply_change_to_table(root.as_table_mut().unwrap(), &c).unwrap();
        let out = toml::to_string(&root).unwrap();
        assert!(out.contains("[profiles.p"), "{out}");
        assert!(!out.contains("[profile.p]"), "v2로 새어 나갔다:\n{out}");
    }

    /// 빈 config(Empty)에 첫 프로파일을 만들면 v2로 시작한다 — `init`과 같은 관례.
    #[test]
    fn empty_syntax_create_starts_as_v2() {
        let mut root: Value = toml::from_str("").unwrap();
        let mut c = change(ChangeOp::Create, ConfigSyntax::Empty);
        c.set
            .insert("uri_env".to_string(), Value::String("U".to_string()));
        apply_change_to_table(root.as_table_mut().unwrap(), &c).unwrap();
        let out = toml::to_string(&root).unwrap();
        assert!(out.contains("[profile.p]"), "{out}");
    }

    // -- v1 → v2 전환 --------------------------------------------------------

    /// v1 스키마를 **넓게** 덮는 표본 — 전환이 무엇 하나 잃지 않는지 보려면 흔한 필드만으로는
    /// 부족하다. 읽기 전용 소스(`read_uri*`)·훅·`recovery_window_days`/`min_redundancy`처럼
    /// 폼에 칸이 없거나 뒤늦게 추가된 필드가 특히 위험하다(대응표에서 빠지기 쉽다).
    const RICH_V1: &str = r#"
default_profile = "prod"

[output]
language = "ko"

[profiles.prod.mode]
backup_type = "incr"
output      = "quiet"
precheck    = false
engine      = "mongodump"

[profiles.prod.source]
uri              = "mongodb://db.internal:27017/app"
uri_env          = "PROD_URI"
read_uri         = "mongodb://replica.internal:27017/app"
read_uri_env     = "PROD_READ_URI"
prefer_secondary = true
connect_timeout_secs = 9

[profiles.prod.destination]
name = "primary"
type = "s3"

[profiles.prod.destination.s3]
bucket          = "db-backups"
prefix          = "mongo/prod"
region          = "ap-northeast-2"
endpoint        = "https://s3.example.com"
credentials_env = "S3_CREDS"

[profiles.prod.features.compression]
algorithm = "zstd"
level     = 6

[profiles.prod.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "/keys/age.pub"

[profiles.prod.features.incremental]
interval      = "30m"
on_gap        = "promote_full"
pg_logical    = true
mysql_binlog  = true

[profiles.prod.retention]
keep_full            = 3
keep_days            = 14
keep_last            = 100
recovery_window_days = 7
min_redundancy       = 2

[profiles.prod.hooks]
pre_backup        = "quiesce.sh"
post_backup       = "notify.sh"
pre_restore       = "stop-app.sh"
post_restore      = "start-app.sh"
pre_prune         = "check.sh"
post_prune        = "report.sh"
on_error          = "pager.sh"
hook_timeout_secs = 45

[profiles.dr.source]
uri = "mongodb://dr.internal:27117/app"

[profiles.dr.features.encryption]
enabled = false
"#;

    /// **전환은 값을 하나도 바꾸지 않는다.** 이 단정이 전환의 전부다 — 등가성은
    /// [`plan_v1_to_v2`]가 스스로 확인하지만(그래서 통과했다는 사실 자체가 증거다), 여기서
    /// 결과를 다시 읽어 문법이 v2로 바뀌었고 값은 그대로임을 눈으로 고정한다.
    #[test]
    fn conversion_moves_every_value_and_changes_only_the_syntax() {
        let plan = plan_v1_to_v2(RICH_V1).expect("풍부한 v1 표본은 전환되어야 함");
        let doc = crate::web::routes::config::ConfigDocument::parse(plan.new_text())
            .expect("전환 결과가 다시 파싱되어야 함");
        assert_eq!(doc.syntax, ConfigSyntax::V2, "{}", plan.new_text());

        // 값 등가성 — 원본과 결과를 같은 로더로 읽어 정규형을 비교한다(계획 안의 게이트와
        // 같은 판정이지만, 그 게이트가 사라지면 이 테스트가 잡는다).
        let before = Config::from_toml_str(RICH_V1).unwrap();
        assert_eq!(
            first_difference(
                &canonical(&before).unwrap(),
                &canonical(&doc.config).unwrap(),
                ""
            ),
            None,
            "전환이 값을 바꿨다:\n{}",
            plan.new_text()
        );

        // 표면이 실제로 v2다 — flat 키가 [profile.<name>] 하나에 모인다.
        let text = plan.new_text();
        assert!(text.contains("[profile.prod]"), "{text}");
        assert!(!text.contains("[profiles."), "v1 중첩이 남았다:\n{text}");
        assert!(
            text.contains("dest = \"s3:db-backups/mongo/prod\""),
            "compact dest로 접히지 않았다:\n{text}"
        );
        assert!(
            text.contains("hook_pre_backup"),
            "훅이 flat 키로 옮겨지지 않았다:\n{text}"
        );

        // 최상위 섹션은 그대로 남는다 — 전환의 대상은 프로파일 표현이다.
        assert_eq!(doc.config.default_profile.as_deref(), Some("prod"));
        assert_eq!(
            doc.config.output.as_ref().and_then(|o| o.language.clone()),
            Some("ko".to_string())
        );
    }

    /// **전환은 상속을 만들지 않는다.** v1에는 상속 표면이 없으므로 각 프로파일이 자기 값을
    /// 그대로 들고 간다 — 화면이 그렇게 말하고, 코드도 그렇게 동작해야 한다.
    #[test]
    fn conversion_creates_no_inheritance_surface() {
        let plan = plan_v1_to_v2(RICH_V1).unwrap();
        let text = plan.new_text();
        assert!(!text.contains("[defaults]"), "{text}");
        assert!(!text.contains("[base."), "{text}");
        assert!(!text.contains("extends"), "{text}");

        // 두 프로파일이 공유하는 값(encrypt)도 각자 적힌 채 남는다 — 합치는 것은 사람의 일이다.
        let doc = crate::web::routes::config::ConfigDocument::parse(text).unwrap();
        assert_eq!(
            doc.origins_of("dr")
                .origin_of(&["encrypt", "encrypt_algorithm"]),
            crate::web::view::config::Origin::Direct,
            "전환이 상속을 만들어 냈다"
        );
    }

    /// 다중 destination은 `[[profile.<name>.dest]]` 배열로 옮겨진다 — compact로 접지 않으므로
    /// 무손실이다.
    #[test]
    fn conversion_moves_multiple_destinations_as_an_array() {
        let text = r#"
[profiles.p.source]
uri_env = "U"

[[profiles.p.destinations]]
name = "primary"
type = "s3"

[profiles.p.destinations.s3]
bucket = "db-backups"
prefix = "mongo"
region = "ap-northeast-2"

[[profiles.p.destinations]]
name = "offsite"
type = "local"
path = "/mnt/offsite"
"#;
        let plan = plan_v1_to_v2(text).expect("다중 destination도 전환되어야 함");
        let doc = crate::web::routes::config::ConfigDocument::parse(plan.new_text()).unwrap();
        let dests = doc.config.profile("p").unwrap().effective_destinations();
        assert_eq!(dests.len(), 2, "{}", plan.new_text());
        assert_eq!(dests[0].name.as_deref(), Some("primary"));
        assert_eq!(
            dests[0].s3.as_ref().unwrap().region.as_deref(),
            Some("ap-northeast-2")
        );
        assert_eq!(dests[1].path.as_deref(), Some("/mnt/offsite"));
    }

    /// v2로 옮길 자리가 없는 모양은 **조용히 버리지 않고 전환을 중단한다.**
    #[test]
    fn conversion_refuses_what_it_cannot_express() {
        let cases: &[(&str, &str)] = &[
            // v2 compact 표기는 local|s3만 안다.
            (
                "[profiles.p.destination]\ntype = \"gcs\"\npath = \"/x\"\n",
                "local|s3",
            ),
            // type이 s3인데 bucket이 없다 — compact를 만들 수 없다.
            ("[profiles.p.destination]\ntype = \"s3\"\n", "bucket"),
            // type이 local인데 path가 없다.
            ("[profiles.p.destination]\ntype = \"local\"\n", "path"),
            // type 없이 위치만 적혀 있다.
            ("[profiles.p.destination]\npath = \"/x\"\n", "type"),
            // 버킷 이름에 '/'가 있으면 prefix와 구분되지 않는다.
            (
                "[profiles.p.destination]\ntype = \"s3\"\n[profiles.p.destination.s3]\nbucket = \"a/b\"\n",
                "'/'",
            ),
            // 단일과 복수가 함께 있으면 v2의 dest 키 하나로 둘을 표현할 수 없다.
            (
                "[profiles.p.destination]\ntype = \"local\"\npath = \"/a\"\n\
                 [[profiles.p.destinations]]\ntype = \"local\"\npath = \"/b\"\n",
                "동시에 표현할 수 없습니다",
            ),
            // 대응표에 없는 키 — 오타이거나 이 버전이 모르는 필드다.
            ("[profiles.p.source]\nuri_env = \"U\"\nnope = 1\n", "nope"),
        ];
        for (text, needle) in cases {
            let err = plan_v1_to_v2(text)
                .err()
                .unwrap_or_else(|| panic!("전환되어서는 안 됨:\n{text}"));
            assert!(
                err.to_string().contains(needle),
                "사유에 '{needle}'이 없다: {err}"
            );
        }
    }

    /// 빈 문자열 prefix는 compact에 담기지 않으므로 명시 키로 따로 나간다 — 값이 그대로 남는다.
    #[test]
    fn conversion_keeps_an_empty_prefix_as_an_explicit_key() {
        let text = "[profiles.p.destination]\ntype = \"s3\"\n\
                    [profiles.p.destination.s3]\nbucket = \"b\"\nprefix = \"\"\n";
        let plan = plan_v1_to_v2(text).expect("빈 prefix도 보존되어야 함");
        assert!(
            plan.new_text().contains("s3_prefix = \"\""),
            "{}",
            plan.new_text()
        );
        let doc = crate::web::routes::config::ConfigDocument::parse(plan.new_text()).unwrap();
        assert_eq!(
            doc.config
                .profile("p")
                .unwrap()
                .destination
                .s3
                .as_ref()
                .unwrap()
                .prefix
                .as_deref(),
            Some("")
        );
    }

    /// v1이 아닌 파일·전환할 프로파일이 없는 파일은 계획 단계에서 거부된다.
    #[test]
    fn conversion_requires_a_v1_file_with_profiles() {
        let already_v2 = plan_v1_to_v2("[profile.p]\nuri_env = \"U\"\n").expect_err("v2는 거부");
        assert!(already_v2.to_string().contains("이미 v2"), "{already_v2}");

        let empty = plan_v1_to_v2("[output]\nlanguage = \"ko\"\n").expect_err("빈 config는 거부");
        assert!(empty.to_string().contains("프로파일이 없습니다"), "{empty}");

        // 로더가 먼저 거부하는 것들(v1/v2 혼합)은 그 메시지가 그대로 올라온다.
        let mixed = plan_v1_to_v2("[profiles.a.source]\nuri=\"x\"\n[profile.b]\nuri=\"y\"\n")
            .expect_err("혼합 문법은 거부");
        assert!(mixed.to_string().contains("섞여 있"), "{mixed}");
        assert_eq!(mixed.exit_code(), crate::error::exit_codes::USAGE);
    }

    /// 등가성 게이트가 실제로 값의 차이를 잡는지 — 게이트 자체의 카나리.
    ///
    /// 이 테스트가 없으면 [`first_difference`]가 항상 `None`을 돌려주도록 망가져도(비교가 죽어도)
    /// 전환은 계속 "통과"한다. 그 상태가 이 화면에서 가장 위험한 실패다.
    #[test]
    fn equivalence_gate_notices_a_missing_value() {
        let full = Config::from_toml_str(
            "[profiles.p.source]\nuri_env = \"U\"\n[profiles.p.retention]\nkeep_full = 3\n",
        )
        .unwrap();
        let missing = Config::from_toml_str("[profiles.p.source]\nuri_env = \"U\"\n").unwrap();
        let found = first_difference(
            &canonical(&full).unwrap(),
            &canonical(&missing).unwrap(),
            "",
        )
        .expect("빠진 값을 잡아야 한다");
        assert!(found.contains("keep_full"), "{found}");
        // 같은 값끼리는 차이가 없다.
        assert_eq!(
            first_difference(&canonical(&full).unwrap(), &canonical(&full).unwrap(), ""),
            None
        );
    }

    /// **차이 보고에 값이 실리지 않는다** — 평문 URI가 달라진 경우 그 값이 오류 화면·로그로
    /// 새어 나가면 안 된다(경로만 말한다).
    #[test]
    fn difference_report_names_the_path_but_never_the_value() {
        const FAKE: &str = "NOT-A-REAL-SECRET-t28-diff-4b91";
        let a = Config::from_toml_str(&format!(
            "[profiles.p.source]\nuri = \"mongodb://u:{FAKE}@h/db\"\n"
        ))
        .unwrap();
        let b = Config::from_toml_str("[profiles.p.source]\nuri = \"mongodb://h/db\"\n").unwrap();
        let found = first_difference(&canonical(&a).unwrap(), &canonical(&b).unwrap(), "").unwrap();
        assert!(
            !found.contains(FAKE),
            "차이 보고에 시크릿이 실렸다: {found}"
        );
        assert!(found.contains("uri"), "{found}");
    }

    /// 지문은 원본 텍스트에 묶인다 — 그 사이 파일이 바뀌면 저장소가 전환을 거부한다(TOCTOU).
    #[test]
    fn conversion_refuses_when_the_file_changed_after_the_preview() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "[profiles.p.source]\nuri_env = \"U\"\n").unwrap();
        let plan = plan_v1_to_v2(&std::fs::read_to_string(&config).unwrap()).unwrap();

        // 미리보기 뒤 누군가 파일을 고쳤다.
        std::fs::write(&config, "[profiles.p.source]\nuri_env = \"OTHER\"\n").unwrap();
        let before = std::fs::read(&config).unwrap();

        let store = FileConfigStore::new(config.clone());
        let err = store
            .convert_to_v2(&plan, test_receipt())
            .expect_err("지문이 다르면 거부해야 함");
        assert!(err.to_string().contains("바뀌었습니다"), "{err}");
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "거부됐는데 파일이 바뀌었다"
        );
        assert!(
            !backup_path(&config, 1).exists(),
            "쓰지도 않았는데 백업이 생겼다"
        );
    }

    /// **실제 doctor로 전환이 끝까지 도는 경로** — 파일이 v2로 바뀌고 직전본이 남는다.
    #[test]
    fn real_doctor_conversion_writes_v2_and_keeps_a_backup() {
        let Some(bin) = skip_if_binary_missing() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let config_path = dir.path().join("config.toml");
        let original = format!(
            r#"
[profiles.p.source]
uri_env = "XB_CONFIG_CONVERT_TEST_URI_UNUSED"

[profiles.p.destination]
type = "local"
path = "{}"

[profiles.p.features.encryption]
enabled = false

[profiles.p.retention]
keep_full = 3
"#,
            dest.display()
        );
        std::fs::write(&config_path, &original).unwrap();

        let plan = plan_v1_to_v2(&original).expect("전환 계획이 서야 함");
        let store = FileConfigStore::with_doctor_exe(config_path.clone(), bin);
        let receipt = store
            .convert_to_v2(&plan, test_receipt())
            .expect("정상 config의 전환은 진행되어야 함");
        assert!(receipt.persisted());

        let saved = std::fs::read_to_string(&config_path).unwrap();
        let doc = crate::web::routes::config::ConfigDocument::parse(&saved).unwrap();
        assert_eq!(doc.syntax, ConfigSyntax::V2, "{saved}");
        assert_eq!(
            doc.config.profile("p").unwrap().retention.keep_full,
            Some(3)
        );
        assert_eq!(
            doc.config.profile("p").unwrap().destination.path.as_deref(),
            Some(dest.to_str().unwrap())
        );
        // 직전본(v1 원본)이 그대로 남아 있다 — 전환을 되돌릴 수 있는 유일한 근거다.
        assert_eq!(
            std::fs::read_to_string(backup_path(&config_path, 1)).unwrap(),
            original
        );
    }

    // -- 백업 회전 -----------------------------------------------------------

    /// 첫 저장은 세대 1만 만든다.
    #[test]
    fn rotate_backups_creates_generation_one_on_first_save() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "v1-content").unwrap();
        rotate_backups(&config).unwrap();
        assert_eq!(
            std::fs::read_to_string(backup_path(&config, 1)).unwrap(),
            "v1-content"
        );
        assert!(!backup_path(&config, 2).exists());
    }

    /// 세대 수 상한을 넘는 저장은 가장 오래된 세대를 버리고, 원본은 절대 사라지지 않는다
    /// (매 회전 직후 `.bak.1`에는 항상 "회전 직전의 원본"이 있어야 한다).
    #[test]
    fn rotate_backups_caps_generations_and_never_loses_the_latest_prior_version() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        for i in 0..(BACKUP_GENERATIONS + 3) {
            std::fs::write(&config, format!("content-{i}")).unwrap();
            rotate_backups(&config).unwrap();
            assert_eq!(
                std::fs::read_to_string(backup_path(&config, 1)).unwrap(),
                format!("content-{i}"),
                "회전 직후 .bak.1이 직전 원본이 아니다(반복 {i})"
            );
        }
        assert!(
            !backup_path(&config, BACKUP_GENERATIONS + 1).exists(),
            "세대 상한을 넘겨 파일이 쌓였다"
        );
        for gen in 1..=BACKUP_GENERATIONS {
            assert!(backup_path(&config, gen).exists(), "세대 {gen}이 없다");
        }
    }

    /// **저장을 두 번 잘못해도**(연속 회전) 원본이 완전히 사라지지 않는다 — 세대 하나만
    /// 두는 설계였다면 이 테스트가 실패했을 것이다(지시서가 명시한 함정).
    #[test]
    fn two_consecutive_bad_saves_do_not_erase_all_history() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "good-original").unwrap();
        rotate_backups(&config).unwrap(); // 첫 저장(실수라고 하자).
        std::fs::write(&config, "bad-edit-1").unwrap();
        rotate_backups(&config).unwrap(); // 두 번째 저장(또 실수).
        std::fs::write(&config, "bad-edit-2").unwrap();

        // 원본(good-original)이 세대 어딘가에 여전히 존재한다.
        let generations: Vec<String> = (1..=BACKUP_GENERATIONS)
            .filter_map(|g| std::fs::read_to_string(backup_path(&config, g)).ok())
            .collect();
        assert!(
            generations.iter().any(|c| c == "good-original"),
            "원본이 두 번의 저장 만에 사라졌다: {generations:?}"
        );
    }

    // -- 원자적 쓰기 ---------------------------------------------------------

    /// 정상 경로 — 내용이 완전히 교체되고, 임시 파일이 디렉터리에 남지 않는다.
    #[test]
    fn write_atomic_replaces_content_and_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "old").unwrap();
        write_atomic(&config, b"new-content").unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "new-content");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".x-backup-config-")
            })
            .collect();
        assert!(leftovers.is_empty(), "임시 파일이 남았다: {leftovers:?}");
    }

    /// **"크래시" 시뮬레이션** — 원자적 쓰기의 마지막 단계(rename)에 도달하기 전에
    /// 멈춰도 원본은 절대 바뀌지 않는다.
    ///
    /// 진짜 프로세스 kill -9를 syscall 중간에 주입할 수는 없다(rename(2)은 커널의 단일
    /// syscall이라 우리 코드가 그것을 "부분 실행"시킬 방법이 없다 — 그것이 바로 원자성의
    /// 정의다). 우리가 보장해야 하는 성질은 "그 앞 단계에서 어떤 이유로든 멈춰도 원본을
    /// 절대 건드리지 않는다"이고, 이 테스트는 [`write_atomic`]과 똑같은 절차를 밟되
    /// `persist`(rename)를 호출하지 않고 버림으로써 정확히 그 성질을 확인한다.
    #[test]
    fn interrupted_write_never_touches_the_original_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "original-content").unwrap();

        let mut tmp = tempfile::Builder::new()
            .prefix(".x-backup-config-")
            .suffix(".tmp")
            .tempfile_in(dir.path())
            .unwrap();
        tmp.write_all(b"new-content-that-never-lands").unwrap();
        tmp.as_file().sync_all().unwrap();
        drop(tmp); // persist(rename)를 부르지 않고 버린다 — Drop이 임시 파일을 지운다.

        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "original-content",
            "rename 전에 멈췄는데 원본이 바뀌었다"
        );
    }

    /// 쓰기 권한이 없는 디렉터리에서는 패닉 없이 명확한 오류로 끝난다.
    #[test]
    fn write_atomic_fails_cleanly_without_write_permission() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "old").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = write_atomic(&config, b"new");

        // 정리(다음 테스트나 tempdir Drop이 지울 수 있게 권한을 되돌린다).
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err(), "쓰기 불가 디렉터리에서 성공하면 안 됨");
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "old",
            "실패했는데 원본이 바뀌었다"
        );
    }

    use std::os::unix::fs::PermissionsExt;

    // -- FileConfigStore::apply — 실제 doctor 자식 e2e ----------------------
    //
    // 아래 테스트들은 실제로 빌드된 `x-backup` 바이너리를 자식으로 띄운다
    // (`src/web/job/runner.rs`의 `real_child_doctor_runs_and_classifies_exit_code`와 같은
    // 패턴). `cargo test --lib`만 돌리면 bin 타깃이 아직 없을 수 있으므로 그 경우 건너뛴다
    // — 라이브러리 단위 테스트가 bin 빌드 여부에 묶이지 않게 하기 위함이다.

    /// 빌드된 `x-backup` 바이너리 경로 — 없으면 `None`(테스트를 건너뛴다).
    ///
    /// ## `assert_cmd::cargo::cargo_bin`을 쓰지 않는 이유 — 그 함수가 패닉한다
    /// `CARGO_BIN_EXE_<name>`은 카고가 **통합 테스트**(`tests/`)에만 넣어 주는 변수다.
    /// 라이브러리 단위 테스트에는 없고, `cargo_bin`은 그 변수가 없으면 경로를 돌려주는
    /// 대신 **패닉한다**. 그래서 "없으면 건너뛴다"는 이 함수가 오히려 테스트를 죽였다:
    /// `cargo test --lib`(CI가 도는 명령)에서 6개가 실패하고, `cargo test`로 전 타깃을
    /// 돌리면 bin이 함께 빌드되어 변수가 채워지므로 **통과한다** — 그래서 로컬에서는
    /// 보이지 않았다.
    ///
    /// 두 경로를 모두 본다: 변수가 있으면 그것(가장 정확하다), 없으면 테스트 바이너리
    /// 위치에서 프로파일 디렉터리를 거슬러 올라가 추정한다
    /// (`target/<profile>/deps/<test>-<hash>` → `target/<profile>/x-backup`).
    fn skip_if_binary_missing() -> Option<PathBuf> {
        let bin = match option_env!("CARGO_BIN_EXE_x-backup") {
            Some(path) => PathBuf::from(path),
            None => {
                let exe = std::env::current_exe().ok()?;
                // deps/<test-bin> → deps → <profile>
                let profile_dir = exe.parent()?.parent()?;
                profile_dir.join("x-backup")
            }
        };
        if bin.exists() {
            Some(bin)
        } else {
            eprintln!(
                "건너뜀: {} 바이너리가 아직 빌드되지 않았습니다",
                bin.display()
            );
            None
        }
    }

    /// 최소 유효 config 본문을 만든다(`destination`은 로컬 tempdir). `warn`/`fail`을
    /// 결정적으로 유도하려면 `encrypt_recipient`를 지정한다. `uri_env`는 호출부가 정한다 —
    /// 테스트마다 다른 이름을 써서 "env가 설정돼 있는가"를 서로 간섭 없이 통제하기
    /// 위함이다(병렬로 도는 다른 테스트가 같은 이름을 건드릴 수 없다).
    fn minimal_v2_config(
        dest_dir: &std::path::Path,
        uri_env: &str,
        recipient: Option<&str>,
    ) -> String {
        let mut body = format!(
            r#"
[profile.p]
uri_env = "{uri_env}"
dest = "local:{}"
"#,
            dest_dir.display()
        );
        if let Some(r) = recipient {
            body.push_str(&format!("encrypt = \"age:{r}\"\n"));
        } else {
            body.push_str("encrypt = false\n");
        }
        body
    }

    /// **exit 0(정상) 경로 — 실제 doctor로 저장이 진행되고 파일이 바뀐다.**
    ///
    /// 진짜로 경고가 0개인 프로파일을 만들려면 두 가지가 필요하다:
    /// 1. `uri_env`가 실제로 설정돼 있어야 한다(비어 있으면 WARN — 아래
    ///    `real_doctor_warning_still_saves_and_surfaces_the_warning`이 그 경로다).
    /// 2. 암호화가 **켜져 있어야** 한다 — `encrypt = false`조차 doctor에게는 WARN이다
    ///    (`routes::doctor`의 SAMPLE_JSON: `"disabled — backups are stored in plaintext"`).
    ///    그래서 실제로 존재하는 recipient 파일을 가리키는 `encrypt = "age:<path>"`를 쓴다.
    ///
    /// `ENV_GUARD`로 다른 테스트의 프로세스 env 조작과 직렬화한다(`web::auth`의 같은 패턴,
    /// `routes::config`의 `empty_env_counts_as_unset`도 동일). **락을 쥔 채로 실패할 수
    /// 있는 단정문을 두지 않는다** — panic이 나면 그 지점에서 `Mutex`가 poison되어 이
    /// 락을 공유하는 다른 테스트(`web::server`의 auth 테스트 등)까지 연쇄로 실패한다. 그래서
    /// env 정리가 끝나는 즉시 `drop(_guard)`로 명시적으로 놓은 뒤에만 단정한다.
    #[test]
    fn real_doctor_clean_config_is_saved() {
        let Some(bin) = skip_if_binary_missing() else {
            return;
        };
        let _guard = crate::web::auth::ENV_GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        const URI_ENV: &str = "XB_CONFIG_WRITE_TEST_CLEAN_URI";
        // SAFETY: ENV_GUARD가 프로세스 env 접근을 이 테스트 모듈 전체에서 직렬화한다.
        unsafe {
            std::env::set_var(URI_ENV, "mongodb://fake-host.invalid:27017/test");
        }

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let recipient = dir.path().join("recipient.pub");
        std::fs::write(&recipient, "age1dummy-recipient-for-test-only").unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            minimal_v2_config(&dest, URI_ENV, Some(recipient.to_str().unwrap())),
        )
        .unwrap();

        let store = FileConfigStore::with_doctor_exe(config_path.clone(), bin);
        let mut change = set_change(ConfigSyntax::V2, &[("keep_full", Value::Integer(3))]);
        change.name = ProfileName::parse("p", Lang::En).unwrap();

        let receipt = store.apply(&change, test_receipt());

        // SAFETY: 위와 같다.
        unsafe {
            std::env::remove_var(URI_ENV);
        }
        drop(_guard); // 아래 단정이 실패해도 락이 poison되지 않게 먼저 놓는다.

        let receipt = receipt.expect("정상 config는 저장되어야 함");
        assert!(receipt.persisted());
        assert!(
            receipt.warnings().is_empty(),
            "경고 없이 통과해야 하는데: {:?}",
            receipt.warnings()
        );
        let saved = std::fs::read_to_string(&config_path).unwrap();
        assert!(saved.contains("keep_full = 3"), "{saved}");
        // 직전본이 남았다.
        assert!(backup_path(&config_path, 1).exists());
    }

    /// **exit 3(차단) 경로 — 422에 해당하는 오류로 막히고 파일이 바이트 단위로 그대로다.**
    #[test]
    fn real_doctor_blocking_failure_prevents_the_write() {
        let Some(bin) = skip_if_binary_missing() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let config_path = dir.path().join("config.toml");
        // encrypt algorithm이 age인데 recipient_file이 존재하지 않는 경로 — doctor의 결정적
        // FAIL 경로(`routes::doctor`의 SAMPLE_JSON과 같은 항목). uri_env는 비워 둬도 무방하다
        // — FAIL이 WARN보다 심각도가 높아 최종 판정을 덮는다(`routes::doctor::severity`와
        // 같은 우선순위).
        let original = minimal_v2_config(
            &dest,
            "XB_CONFIG_WRITE_TEST_FAIL_URI_UNUSED",
            Some("/nonexistent/x-backup-recipient.pub"),
        );
        std::fs::write(&config_path, &original).unwrap();
        let original_bytes = std::fs::read(&config_path).unwrap();

        let store = FileConfigStore::with_doctor_exe(config_path.clone(), bin);
        let mut change = set_change(ConfigSyntax::V2, &[("keep_full", Value::Integer(3))]);
        change.name = ProfileName::parse("p", Lang::En).unwrap();

        let err = store
            .apply(&change, test_receipt())
            .expect_err("차단성 설정은 저장을 막아야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::PRECHECK);

        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            original_bytes,
            "저장이 막혔는데 파일이 바이트 단위로 바뀌었다"
        );
        assert!(
            !backup_path(&config_path, 1).exists(),
            "저장하지 않았는데 백업이 생겼다"
        );
    }

    /// **exit 4(경고) 경로 — 저장은 진행되고, 경고 메시지가 receipt에 실린다.**
    ///
    /// 이 테스트가 "exit 4에서 저장을 허용했음을 어떻게 고정했는지"의 통합 계층이다(단위
    /// 계층은 위 `exit_4_persists_and_carries_warning_text`).
    #[test]
    fn real_doctor_warning_still_saves_and_surfaces_the_warning() {
        let Some(bin) = skip_if_binary_missing() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let config_path = dir.path().join("config.toml");
        // uri_env를 지정했지만 그 env가 실제로 설정돼 있지 않다 — doctor의 결정적 WARN
        // 경로(`src/web/job/runner.rs`의 e2e 테스트와 같은 유발 방식). 이 이름은 다른
        // 테스트가 절대 설정하지 않는 이름이라(위 `real_doctor_clean_config_is_saved`와
        // 다른 상수) `ENV_GUARD` 없이도 안전하다.
        std::fs::write(
            &config_path,
            minimal_v2_config(&dest, "XB_CONFIG_WRITE_TEST_WARN_URI_UNUSED", None),
        )
        .unwrap();

        let store = FileConfigStore::with_doctor_exe(config_path.clone(), bin);
        let mut change = set_change(ConfigSyntax::V2, &[("keep_full", Value::Integer(7))]);
        change.name = ProfileName::parse("p", Lang::En).unwrap();

        let receipt = store
            .apply(&change, test_receipt())
            .expect("exit 4는 저장을 막으면 안 됨");
        assert!(receipt.persisted(), "경고가 저장을 막았다");
        assert!(
            !receipt.warnings().is_empty(),
            "경고가 있어야 하는데 비어 있다(doctor 판정이 바뀌었다면 이 표본을 갱신할 것)"
        );
        let saved = std::fs::read_to_string(&config_path).unwrap();
        assert!(saved.contains("keep_full = 7"), "{saved}");
    }

    /// v1 config를 실제 doctor로 저장해도 문법이 v1로 남는다(로드→저장→재로드 라운드트립).
    #[test]
    fn real_doctor_v1_save_roundtrip_stays_v1() {
        let Some(bin) = skip_if_binary_missing() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
[profiles.p.source]
uri_env = "XB_CONFIG_WRITE_TEST_URI_UNUSED"

[profiles.p.destination]
type = "local"
path = "{}"

[profiles.p.features.encryption]
enabled = false

[profiles.p.retention]
keep_full = 3
"#,
                dest.display()
            ),
        )
        .unwrap();

        let store = FileConfigStore::with_doctor_exe(config_path.clone(), bin);
        let mut change = set_change(ConfigSyntax::V1, &[("keep_days", Value::Integer(21))]);
        change.name = ProfileName::parse("p", Lang::En).unwrap();

        let receipt = store
            .apply(&change, test_receipt())
            .expect("v1 정상 config는 저장되어야 함");
        assert!(receipt.persisted());
        let saved = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            saved.contains("[profiles.p") && !saved.contains("[profile.p]"),
            "v1이 v2로 다시 써졌다:\n{saved}"
        );
        let doc = crate::web::routes::config::ConfigDocument::parse(&saved).unwrap();
        assert_eq!(doc.syntax, ConfigSyntax::V1);
        assert_eq!(
            doc.config.profile("p").unwrap().retention.keep_days,
            Some(21)
        );
        assert_eq!(
            doc.config.profile("p").unwrap().retention.keep_full,
            Some(3),
            "손대지 않은 값이 사라졌다"
        );
    }
}
