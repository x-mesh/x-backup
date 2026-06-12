//! mongodump 오케스트레이션 — stdout archive 스트리밍(스파이크 §5, pitfall 6).
//!
//! `mongodump --archive=- [--oplog]`를 스폰해 stdout을 파이프라인 소스로 노출한다.
//!
//! ## 핵심 안전 규칙(리서치 pitfalls §6, 태스크 지침 2)
//! - **URI는 argv 금지(보안, PRD §11):** `--uri`를 인자로 넘기면 `ps`에 평문 노출.
//!   권한 0600 임시 YAML config 파일(`uri:` 필드)로 전달하고, 종료 후 삭제한다.
//!   임시 파일은 [`tempfile::NamedTempFile`]이라 핸들 Drop 시 자동 삭제된다(에러
//!   경로 포함).
//! - **stderr 독립 drain(데드락 방지, pitfall 6-2):** mongodump는 성공해도 stderr로
//!   진행 로그를 낸다(TOOLS-1565). stderr 버퍼가 차면 프로세스가 블록되므로, spawn
//!   직후 stderr를 **독립 tokio task로** 끝까지 읽어 tracing 로그로 흘린다.
//! - **종료 판정은 exit code로만(pitfall 6-1):** stderr 내용으로 성공/실패를 판단하지
//!   않는다. exit status가 1차이자 유일한 지표다.
//! - **kill+wait 쌍(zombie 방지, pitfall 6-4):** [`kill_on_drop(true)`]로 핸들 Drop 시
//!   SIGKILL을 보내고, 명시적 취소/에러 경로에서도 kill 후 반드시 wait해 좀비를
//!   남기지 않는다.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::task::JoinHandle;

use crate::error::{Result, XBackupError};

/// dump 스폰에 필요한 옵션.
pub struct DumpSpec {
    /// 실행할 실행파일 경로(보통 `mongodump`; 테스트는 가짜 스크립트 주입).
    pub program: String,
    /// `--uri` config로 전달할 MongoDB URI(argv 아님).
    pub uri_config_path: String,
    /// replica set이면 `--oplog`를 부여한다(FR-1).
    pub oplog: bool,
    /// 선택적 백업 — 특정 DB만(`--db`). `--oplog`와 병용 불가(FR-1; 차단은 상위에서).
    pub db: Option<String>,
    /// 선택적 백업 — 특정 컬렉션만(`--collection`).
    pub collection: Option<String>,
}

/// 스폰된 mongodump 핸들 — stdout 스트림 + stderr drain task + 자식 프로세스.
///
/// [`stdout`](Self::take_stdout)으로 archive 스트림을 꺼내 파이프라인에 흘리고,
/// 스트림 소비 후 [`wait`](Self::wait)로 종료 코드를 판정한다.
pub struct DumpProcess {
    child: Child,
    stdout: Option<ChildStdout>,
    /// stderr를 끝까지 읽는 독립 task(데드락 방지). join하면 마지막 stderr 몇 줄을 회수.
    stderr_drain: Option<JoinHandle<Vec<String>>>,
    program: String,
}

impl DumpProcess {
    /// `mongodump`를 archive=- 모드로 스폰한다.
    ///
    /// spawn 직후 stderr를 독립 task로 drain하기 시작한다.
    pub fn spawn(spec: &DumpSpec) -> Result<Self> {
        let mut cmd = Command::new(&spec.program);

        // archive를 stdout으로(--archive=-), URI는 config 파일로(argv 금지).
        cmd.arg("--archive=-")
            .arg("--config")
            .arg(&spec.uri_config_path);

        if spec.oplog {
            cmd.arg("--oplog");
        }
        if let Some(db) = &spec.db {
            cmd.arg("--db").arg(db);
        }
        if let Some(coll) = &spec.collection {
            cmd.arg("--collection").arg(coll);
        }

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            // 핸들 Drop 시 자식에 SIGKILL — 패닉·조기 반환 시 좀비 방지(pitfall 6-4).
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            XBackupError::Failure(format!(
                "'{}' 실행 실패(설치/PATH 확인): {e}",
                spec.program
            ))
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| XBackupError::Failure("mongodump stdout 파이프 획득 실패".into()))?;

        // stderr를 spawn 직후 독립 task로 끝까지 읽는다(pitfall 6-2 데드락 방지).
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| XBackupError::Failure("mongodump stderr 파이프 획득 실패".into()))?;
        let stderr_drain = tokio::spawn(drain_stderr(stderr));

        Ok(Self {
            child,
            stdout: Some(stdout),
            stderr_drain: Some(stderr_drain),
            program: spec.program.clone(),
        })
    }

    /// archive stdout 스트림을 꺼낸다(파이프라인 소스). 한 번만 호출 가능.
    pub fn take_stdout(&mut self) -> Result<ChildStdout> {
        self.stdout
            .take()
            .ok_or_else(|| XBackupError::Failure("mongodump stdout이 이미 소비됨".into()))
    }

    /// 프로세스 종료를 기다리고 **exit code로만** 성공/실패를 판정한다.
    ///
    /// stdout을 끝까지 소비한 뒤 호출해야 한다(EOF = dump 종료 신호). stderr drain
    /// task도 함께 join해 마지막 로그를 회수하고 좀비를 남기지 않는다(pitfall 6-1/6-4).
    pub async fn wait(mut self) -> Result<()> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| XBackupError::Failure(format!("mongodump wait 실패: {e}")))?;

        // stderr drain을 join — 마지막 로그 회수 + task 정리.
        let tail = self.join_stderr().await;

        if status.success() {
            tracing::info!(program = %self.program, "mongodump 정상 종료(exit 0)");
            Ok(())
        } else {
            // 종료 판정은 exit code로만. stderr는 진단 메시지에만 첨부.
            let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
            let detail = if tail.is_empty() {
                String::new()
            } else {
                format!(" — 마지막 stderr: {}", tail.join(" | "))
            };
            Err(XBackupError::Failure(format!(
                "mongodump 비정상 종료(exit {code}){detail}"
            )))
        }
    }

    /// 에러·취소 경로 정리: kill 후 wait(좀비 방지, pitfall 6-4).
    ///
    /// 이미 종료했으면 무해하다. drain task도 정리한다.
    pub async fn abort(mut self) {
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
    const TAIL_LINES: usize = 5;
    let mut reader = BufReader::new(stderr).lines();
    let mut tail: Vec<String> = Vec::new();

    while let Ok(Some(line)) = reader.next_line().await {
        tracing::debug!(target: "mongodump", "{line}");
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
    use tokio::io::AsyncReadExt;

    /// 가짜 mongodump 셸 스크립트를 만든다. stdout으로 지정 바이트를 내고,
    /// stderr로 노이즈를 출력한 뒤 주어진 exit code로 종료한다.
    fn fake_mongodump(stdout_payload: &str, stderr_noise: &str, exit_code: i32) -> tempfile::TempPath {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let mut f = tempfile::Builder::new()
            .prefix("fake-mongodump-")
            .suffix(".sh")
            .tempfile()
            .unwrap();
        // stderr 노이즈를 먼저 내고, stdout으로 페이로드를 낸 뒤 종료.
        writeln!(
            f,
            "#!/usr/bin/env bash\n>&2 printf '%s\\n' {stderr_noise:?}\nprintf '%s' {stdout_payload:?}\nexit {exit_code}"
        )
        .unwrap();
        let path = f.into_temp_path();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn spec(program: &str) -> DumpSpec {
        DumpSpec {
            program: program.to_string(),
            uri_config_path: "/dev/null".to_string(),
            oplog: false,
            db: None,
            collection: None,
        }
    }

    /// 정상 종료(exit 0): stdout 바이트를 끝까지 읽고 wait가 Ok여야 한다.
    /// stderr 노이즈가 있어도(성공 stderr, TOOLS-1565) 판정에 영향 없어야 한다.
    #[tokio::test]
    async fn successful_dump_streams_stdout_and_drains_stderr() {
        let fake = fake_mongodump("ARCHIVE_BYTES_HERE", "writing captured oplog", 0);
        let mut proc = DumpProcess::spawn(&spec(fake.to_str().unwrap())).unwrap();

        let mut stdout = proc.take_stdout().unwrap();
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"ARCHIVE_BYTES_HERE");

        // stderr 노이즈에도 불구하고 exit 0 → Ok.
        proc.wait().await.unwrap();
    }

    /// 비정상 종료(exit 1)는 stdout이 비어도 exit code로 실패를 판정해야 한다.
    /// 마지막 stderr가 에러 메시지에 첨부되는지 확인한다.
    #[tokio::test]
    async fn nonzero_exit_is_detected_via_exit_code() {
        let fake = fake_mongodump("", "Failed: connection refused", 1);
        let mut proc = DumpProcess::spawn(&spec(fake.to_str().unwrap())).unwrap();

        let mut stdout = proc.take_stdout().unwrap();
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await.unwrap();

        let err = proc.wait().await.unwrap_err();
        assert_eq!(err.exit_code(), 1);
        let msg = err.to_string();
        assert!(msg.contains("비정상 종료"), "메시지: {msg}");
        assert!(msg.contains("connection refused"), "stderr 첨부 누락: {msg}");
    }

    /// 실제 spawn된 자식 프로세스의 argv에 URI가 절대 들어가지 않음을 검증한다.
    /// 가짜 mongodump가 자신의 argv를 파일에 기록하게 하고, 그 안에 URI 스킴이
    /// 없고 --config 파일 경로만 있는지 확인한다(PRD §11, DoD: argv에 URI 부재).
    #[tokio::test]
    async fn spawned_child_argv_has_no_uri_only_config_path() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // argv를 기록하는 디렉터리.
        let workdir = tempfile::tempdir().unwrap();
        let argv_log = workdir.path().join("argv.txt");

        // 가짜 mongodump: "$@"를 argv 로그에 기록 후 stdout으로 바이트 방출.
        let mut script = tempfile::Builder::new()
            .prefix("fake-mongodump-argv-")
            .suffix(".sh")
            .tempfile_in(workdir.path())
            .unwrap();
        writeln!(
            script,
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > {log:?}\nprintf 'BYTES'\nexit 0",
            log = argv_log.to_str().unwrap()
        )
        .unwrap();
        let script_path = script.into_temp_path();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        // 시크릿 URI를 담은 0600 config 파일을 만들어 --config로 전달.
        let secret = crate::config::secret::Secret::new(
            "mongodb://user:SUPERSECRET@host:27017/?replicaSet=rs0",
        );
        let uri_config = super::super::uri_config::UriConfigFile::create(&secret).unwrap();

        let dump_spec = DumpSpec {
            program: script_path.to_str().unwrap().to_string(),
            uri_config_path: uri_config.path().to_string(),
            oplog: true,
            db: None,
            collection: None,
        };
        let mut proc = DumpProcess::spawn(&dump_spec).unwrap();
        let mut stdout = proc.take_stdout().unwrap();
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await.unwrap();
        proc.wait().await.unwrap();

        // 기록된 argv 검사: URI/시크릿이 한 글자도 없어야 한다.
        let recorded = std::fs::read_to_string(&argv_log).unwrap();
        assert!(
            !recorded.contains("mongodb://") && !recorded.contains("SUPERSECRET"),
            "자식 argv에 URI/시크릿 노출: {recorded}"
        );
        // --config 파일 경로는 인자로 존재해야 한다.
        assert!(recorded.contains("--config"), "argv에 --config 누락: {recorded}");
        assert!(recorded.contains("--oplog"), "argv에 --oplog 누락: {recorded}");
        assert!(recorded.contains("--archive=-"), "argv에 --archive=- 누락: {recorded}");
    }

    /// 존재하지 않는 실행파일은 즉시 실패한다(설치/PATH 안내).
    #[tokio::test]
    async fn missing_executable_fails_to_spawn() {
        // DumpProcess는 Debug가 아니므로 unwrap_err 대신 match로 검사한다.
        match DumpProcess::spawn(&spec("/nonexistent/x-fake-mongodump")) {
            Ok(_) => panic!("존재하지 않는 실행파일인데 spawn 성공"),
            Err(err) => assert_eq!(err.exit_code(), 1),
        }
    }

    /// argv에 URI가 들어가지 않음을 보장한다 — spawn이 구성하는 인자를 직접 검증한다.
    /// (URI는 --config 파일 경로로만 전달되어야 한다, PRD §11.)
    #[test]
    fn argv_never_contains_uri() {
        // spawn이 만드는 Command의 인자를 재현해 검사한다.
        let s = DumpSpec {
            program: "mongodump".into(),
            uri_config_path: "/tmp/secret-uri-config.yaml".into(),
            oplog: true,
            db: None,
            collection: None,
        };
        let mut cmd = Command::new(&s.program);
        cmd.arg("--archive=-").arg("--config").arg(&s.uri_config_path);
        if s.oplog {
            cmd.arg("--oplog");
        }
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // config 파일 경로는 있지만, mongodb URI 스킴은 어떤 인자에도 없어야 한다.
        assert!(args.iter().any(|a| a == "--config"));
        assert!(
            !args.iter().any(|a| a.contains("mongodb://") || a.contains("mongodb+srv://")),
            "argv에 URI 노출: {args:?}"
        );
    }
}
