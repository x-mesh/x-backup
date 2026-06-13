//! mongorestore 오케스트레이션 — stdin archive 스트리밍(스파이크 §3.2, pitfall 6).
//!
//! `mongorestore --archive=- [--nsInclude] [--drop]`를 스폰해 **stdin**으로 archive
//! 바이트를 흘려넣어 복구한다(백업의 [`super::dump`] 역방향). dump가 stdout으로
//! archive를 *내보냈다면*, restore는 stdin으로 archive를 *받아들인다*.
//!
//! ## dump와 동일한 안전 규칙(리서치 pitfalls §6, 태스크 지침 1)
//! - **URI는 argv 금지(보안, PRD §11):** dump와 동일하게 0600 임시 YAML config의
//!   `uri:` 필드로 전달한다([`super::uri_config`]). `ps`에 평문 노출되지 않는다.
//! - **stderr 독립 drain(데드락 방지, pitfall 6-2):** mongorestore도 진행 로그를
//!   stderr로 낸다. spawn 직후 독립 task로 끝까지 읽어 tracing으로 흘린다.
//! - **종료 판정은 exit code로만(pitfall 6-1):** stderr 내용으로 성공/실패를 판단하지
//!   않는다. exit status가 유일한 지표다.
//! - **kill+wait 쌍(zombie 방지, pitfall 6-4):** [`kill_on_drop(true)`]로 핸들 Drop 시
//!   SIGKILL, 명시적 취소/에러 경로에서도 kill 후 반드시 wait한다.
//!
//! ## 풀 복구 스코프(t5)
//! - `--archive=-`로 stdin 스트리밍. PITR(`--oplogReplay`/`--oplogLimit`)은 t9 소유이며
//!   풀 복구는 oplog replay 없이 base 스냅샷만 복원한다(archive에 oplog가 포함돼 있어도
//!   replay하지 않으면 dump 시점 스냅샷만 적용된다 — 스파이크 §3.2).
//! - `--drop`은 PRD 가드레일상 **기본 비활성**이다. 가드(force/대화형)는 상위
//!   ([`crate::pipeline::restore`])가 통과시킨 뒤에만 `drop: true`로 켠다.
//! - `--nsInclude`로 선택적 복구(`db.collection`)를 지원한다(`--only`).

use std::process::Stdio;

use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;

use crate::error::{Result, XBackupError};

/// restore 스폰에 필요한 옵션.
pub struct RestoreSpec {
    /// 실행할 실행파일 경로(보통 `mongorestore`; 테스트는 가짜 스크립트 주입).
    pub program: String,
    /// `--uri` 대신 `--config`로 전달할 URI config 파일 경로(argv 아님).
    pub uri_config_path: String,
    /// 선택적 복구 대상(`db.collection`). `Some`이면 `--nsInclude <ns>`를 부여한다.
    pub ns_include: Option<String>,
    /// 기존 컬렉션을 복원 전 drop할지 여부(PRD 가드레일상 기본 false).
    /// 상위 가드(force/대화형)를 통과한 경우에만 true로 설정한다.
    pub drop: bool,
}

/// 스폰된 mongorestore 핸들 — stdin 싱크 + stderr drain task + 자식 프로세스.
///
/// [`take_stdin`](Self::take_stdin)으로 archive 싱크를 꺼내 파이프라인 바이트를
/// 흘려넣고(`tokio::io::copy`), 닫은 뒤 [`wait`](Self::wait)로 종료 코드를 판정한다.
pub struct RestoreProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    /// stderr를 끝까지 읽는 독립 task(데드락 방지). join하면 마지막 stderr 몇 줄을 회수.
    stderr_drain: Option<JoinHandle<Vec<String>>>,
    program: String,
}

impl RestoreProcess {
    /// `mongorestore`를 `--archive=-`(stdin) 모드로 스폰한다.
    ///
    /// spawn 직후 stderr를 독립 task로 drain하기 시작한다.
    pub fn spawn(spec: &RestoreSpec) -> Result<Self> {
        let mut cmd = Command::new(&spec.program);

        // archive를 stdin으로(--archive=-), URI는 config 파일로(argv 금지).
        cmd.arg("--archive=-")
            .arg("--config")
            .arg(&spec.uri_config_path);

        // 선택적 복구: 네임스페이스 필터. (PITR --oplogReplay와는 병용 불가 — pitfall 1-4,
        //   t9가 PITR 경로에서 차단; 풀 복구 t5는 --oplogReplay를 쓰지 않는다.)
        if let Some(ns) = &spec.ns_include {
            cmd.arg("--nsInclude").arg(ns);
        }
        // 파괴적 옵션은 가드 통과 시에만 켠다(PRD 가드레일, 기본 비활성).
        if spec.drop {
            cmd.arg("--drop");
        }

        // -vv 진단용: 실행 인자(시크릿 제외 — URI는 config 파일로 전달).
        tracing::debug!(
            program = %spec.program,
            drop = spec.drop,
            ns_include = spec.ns_include.as_deref().unwrap_or("(전체)"),
            "mongorestore 스폰(--archive=- --config <임시>)"
        );

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // 핸들 Drop 시 자식에 SIGKILL — 패닉·조기 반환 시 좀비 방지(pitfall 6-4).
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            XBackupError::Failure(format!("'{}' 실행 실패(설치/PATH 확인): {e}", spec.program))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| XBackupError::Failure("mongorestore stdin 파이프 획득 실패".into()))?;

        // stderr를 spawn 직후 독립 task로 끝까지 읽는다(pitfall 6-2 데드락 방지).
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| XBackupError::Failure("mongorestore stderr 파이프 획득 실패".into()))?;
        let stderr_drain = tokio::spawn(drain_stderr(stderr));

        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr_drain: Some(stderr_drain),
            program: spec.program.clone(),
        })
    }

    /// archive stdin 싱크를 꺼낸다(파이프라인 데이터를 여기로 흘린다). 한 번만 호출 가능.
    ///
    /// 호출자는 바이트를 다 쓴 뒤 이 싱크를 **반드시 Drop(또는 shutdown)** 해야 한다 —
    /// stdin EOF가 mongorestore에 입력 종료를 알리는 신호다(닫지 않으면 hang).
    pub fn take_stdin(&mut self) -> Result<ChildStdin> {
        self.stdin
            .take()
            .ok_or_else(|| XBackupError::Failure("mongorestore stdin이 이미 소비됨".into()))
    }

    /// 프로세스 종료를 기다리고 **exit code로만** 성공/실패를 판정한다.
    ///
    /// stdin을 닫은(Drop한) 뒤 호출해야 한다(EOF = 입력 종료 신호). stderr drain
    /// task도 함께 join해 마지막 로그를 회수하고 좀비를 남기지 않는다(pitfall 6-1/6-4).
    pub async fn wait(mut self) -> Result<()> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| XBackupError::Failure(format!("mongorestore wait 실패: {e}")))?;

        // stderr drain을 join — 마지막 로그 회수 + task 정리.
        let tail = self.join_stderr().await;

        if status.success() {
            tracing::info!(program = %self.program, "mongorestore 정상 종료(exit 0)");
            Ok(())
        } else {
            // 종료 판정은 exit code로만. stderr는 진단 메시지에만 첨부.
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            let detail = if tail.is_empty() {
                String::new()
            } else {
                format!(" — 마지막 stderr: {}", tail.join(" | "))
            };
            Err(XBackupError::Failure(format!(
                "mongorestore 비정상 종료(exit {code}){detail}"
            )))
        }
    }

    /// 에러·취소 경로 정리: kill 후 wait(좀비 방지, pitfall 6-4).
    ///
    /// 이미 종료했으면 무해하다. drain task도 정리한다.
    pub async fn abort(mut self) {
        // stdin이 아직 살아 있으면 먼저 Drop해 입력을 닫는다(미소비 핸들 정리).
        self.stdin.take();
        // best-effort kill — 이미 종료했으면 에러는 무시.
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        let _ = self.join_stderr().await;
    }

    /// stderr drain task를 join해 마지막 로그 줄들을 회수한다.
    async fn join_stderr(&mut self) -> Vec<String> {
        match self.stderr_drain.take() {
            Some(handle) => handle.await.unwrap_or_default(),
            None => Vec::new(),
        }
    }
}

/// stderr를 끝까지 읽어 tracing 로그로 흘리고, 마지막 몇 줄을 반환한다.
///
/// 종료 판정에 쓰지 않는다(pitfall 6-1) — 진단·관측용. 마지막 N줄만 회수해
/// 비정상 종료 시 에러 메시지에 첨부한다.
async fn drain_stderr(stderr: tokio::process::ChildStderr) -> Vec<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    const TAIL_LINES: usize = 5;
    let mut reader = BufReader::new(stderr).lines();
    let mut tail: Vec<String> = Vec::new();

    while let Ok(Some(line)) = reader.next_line().await {
        tracing::debug!(target: "mongorestore", "{line}");
        tail.push(line);
        if tail.len() > TAIL_LINES {
            tail.remove(0);
        }
    }
    tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// 가짜 mongorestore 셸 스크립트를 만든다. stdin을 파일로 받아 적고(검증용),
    /// stderr로 노이즈를 낸 뒤 주어진 exit code로 종료한다.
    fn fake_mongorestore(
        stdin_sink: &str,
        stderr_noise: &str,
        exit_code: i32,
    ) -> tempfile::TempPath {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let mut f = tempfile::Builder::new()
            .prefix("fake-mongorestore-")
            .suffix(".sh")
            .tempfile()
            .unwrap();
        // stdin을 sink 파일로 복사(cat) + stderr 노이즈 + 종료 코드.
        // "$@"(argv)도 sink 옆 .argv 파일에 기록해 argv 검증에 쓴다.
        writeln!(
            f,
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > {sink:?}.argv\n>&2 printf '%s\\n' {noise:?}\ncat > {sink:?}\nexit {code}",
            sink = stdin_sink,
            noise = stderr_noise,
            code = exit_code
        )
        .unwrap();
        let path = f.into_temp_path();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn spec(program: &str, drop: bool, ns: Option<String>) -> RestoreSpec {
        RestoreSpec {
            program: program.to_string(),
            uri_config_path: "/dev/null".to_string(),
            ns_include: ns,
            drop,
        }
    }

    /// 정상 종료(exit 0): stdin으로 흘려넣은 바이트가 자식에 그대로 전달되고
    /// stderr 노이즈가 있어도 exit code로 성공을 판정한다.
    #[tokio::test]
    async fn successful_restore_consumes_stdin_and_drains_stderr() {
        let sink = tempfile::NamedTempFile::new().unwrap();
        let sink_path = sink.path().to_str().unwrap().to_string();
        let fake = fake_mongorestore(&sink_path, "preparing collections to restore", 0);

        let mut proc = RestoreProcess::spawn(&spec(fake.to_str().unwrap(), false, None)).unwrap();
        let mut stdin = proc.take_stdin().unwrap();
        stdin.write_all(b"ARCHIVE_STREAM_BYTES").await.unwrap();
        stdin.shutdown().await.unwrap();
        drop(stdin); // EOF.

        proc.wait().await.unwrap();

        // 자식이 stdin으로 받은 바이트가 sink에 기록됐는지.
        let received = std::fs::read(&sink_path).unwrap();
        assert_eq!(received, b"ARCHIVE_STREAM_BYTES");
    }

    /// 비정상 종료(exit 1)는 exit code로 실패를 판정하고 마지막 stderr를 첨부한다.
    #[tokio::test]
    async fn nonzero_exit_is_detected_via_exit_code() {
        let sink = tempfile::NamedTempFile::new().unwrap();
        let sink_path = sink.path().to_str().unwrap().to_string();
        let fake = fake_mongorestore(&sink_path, "Failed: target unreachable", 1);

        let mut proc = RestoreProcess::spawn(&spec(fake.to_str().unwrap(), false, None)).unwrap();
        let mut stdin = proc.take_stdin().unwrap();
        let _ = stdin.write_all(b"bytes").await;
        drop(stdin);

        let err = proc.wait().await.unwrap_err();
        assert_eq!(err.exit_code(), 1);
        let msg = err.to_string();
        assert!(msg.contains("비정상 종료"), "메시지: {msg}");
        assert!(
            msg.contains("target unreachable"),
            "stderr 첨부 누락: {msg}"
        );
    }

    /// argv 검증: --archive=- / --config 는 항상, --drop 은 drop=true일 때만,
    /// --nsInclude 는 ns_include가 Some일 때만, 그리고 URI 평문은 절대 없어야 한다.
    #[tokio::test]
    async fn argv_includes_flags_and_never_uri() {
        let sink = tempfile::NamedTempFile::new().unwrap();
        let sink_path = sink.path().to_str().unwrap().to_string();
        let fake = fake_mongorestore(&sink_path, "noise", 0);

        // 시크릿 URI를 0600 config로 전달.
        let secret = crate::config::secret::Secret::new(
            "mongodb://user:SUPERSECRET@host:27017/?replicaSet=rs0",
        );
        let uri_config = super::super::uri_config::UriConfigFile::create(&secret).unwrap();

        let restore_spec = RestoreSpec {
            program: fake.to_str().unwrap().to_string(),
            uri_config_path: uri_config.path().to_string(),
            ns_include: Some("testdb.items".to_string()),
            drop: true,
        };
        let mut proc = RestoreProcess::spawn(&restore_spec).unwrap();
        let mut stdin = proc.take_stdin().unwrap();
        stdin.write_all(b"X").await.unwrap();
        drop(stdin);
        proc.wait().await.unwrap();

        let recorded = std::fs::read_to_string(format!("{sink_path}.argv")).unwrap();
        assert!(recorded.contains("--archive=-"), "argv: {recorded}");
        assert!(recorded.contains("--config"), "argv: {recorded}");
        assert!(recorded.contains("--drop"), "argv: {recorded}");
        assert!(recorded.contains("--nsInclude"), "argv: {recorded}");
        assert!(recorded.contains("testdb.items"), "argv: {recorded}");
        assert!(
            !recorded.contains("mongodb://") && !recorded.contains("SUPERSECRET"),
            "자식 argv에 URI/시크릿 노출: {recorded}"
        );
    }

    /// drop=false면 argv에 --drop이 없어야 한다(PRD 가드레일 기본 비활성).
    #[tokio::test]
    async fn drop_disabled_by_default() {
        let sink = tempfile::NamedTempFile::new().unwrap();
        let sink_path = sink.path().to_str().unwrap().to_string();
        let fake = fake_mongorestore(&sink_path, "noise", 0);

        let mut proc = RestoreProcess::spawn(&spec(fake.to_str().unwrap(), false, None)).unwrap();
        let mut stdin = proc.take_stdin().unwrap();
        stdin.write_all(b"Y").await.unwrap();
        drop(stdin);
        proc.wait().await.unwrap();

        let recorded = std::fs::read_to_string(format!("{sink_path}.argv")).unwrap();
        assert!(
            !recorded.contains("--drop"),
            "drop 비활성인데 --drop 노출: {recorded}"
        );
    }

    /// 존재하지 않는 실행파일은 즉시 실패한다(설치/PATH 안내).
    #[tokio::test]
    async fn missing_executable_fails_to_spawn() {
        match RestoreProcess::spawn(&spec("/nonexistent/x-fake-mongorestore", false, None)) {
            Ok(_) => panic!("존재하지 않는 실행파일인데 spawn 성공"),
            Err(err) => assert_eq!(err.exit_code(), 1),
        }
    }
}
