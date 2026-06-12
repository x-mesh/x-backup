//! MongoDB 서버 메타 질의 — 드라이버로 토폴로지·버전·oplog ts 조회.
//!
//! dump 자체는 `mongodump` 서브프로세스가 수행하므로, 드라이버는 **메타데이터 질의**
//! 에만 쓴다(태스크 지침 1):
//! - `hello` → `setName`으로 replica set 감지(→ `--oplog` 자동 부여 판단).
//! - `buildInfo` → 서버 버전(manifest server_version, 복구 호환 점검).
//! - `local.oplog.rs` 최신 ts → dump 전후로 기록해 oplog 구간(FR-7) 산정.
//!
//! oplog `ts`는 BSON Timestamp{t,i}로 다룬다(DateTime 변환 금지, pitfall 2-2).

use bson::{doc, Timestamp};
use mongodb::options::{ClientOptions, FindOneOptions};
use mongodb::Client;

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::Topology;

/// 드라이버로 조회한 서버 메타데이터.
#[derive(Debug, Clone)]
pub struct ServerMeta {
    /// replica set이면 set 이름(예: `rs0`), standalone이면 None.
    pub repl_set_name: Option<String>,
    /// 서버 버전 문자열(예: `7.0.35`).
    pub server_version: String,
}

impl ServerMeta {
    /// 토폴로지로 환산한다(set 이름 유무 기준).
    pub fn topology(&self) -> Topology {
        if self.repl_set_name.is_some() {
            Topology::ReplicaSet
        } else {
            Topology::Standalone
        }
    }

    /// replica set이면 `--oplog`를 부여해야 한다(FR-1).
    pub fn supports_oplog(&self) -> bool {
        self.repl_set_name.is_some()
    }
}

/// 메타 질의용 드라이버 클라이언트 래퍼.
///
/// URI는 [`Secret`]으로 받아 argv·로그에 노출하지 않는다. dump 서브프로세스에도
/// argv가 아닌 임시 config 파일로 전달한다([`super::dump`]).
pub struct MongoMeta {
    client: Client,
}

impl MongoMeta {
    /// URI 시크릿으로 클라이언트를 연결한다(SRV lookup 포함).
    pub async fn connect(uri: &Secret) -> Result<Self> {
        let options = ClientOptions::parse(uri.expose())
            .await
            .map_err(|e| XBackupError::Failure(format!("MongoDB URI 파싱/연결 실패: {e}")))?;
        let client = Client::with_options(options)
            .map_err(|e| XBackupError::Failure(format!("MongoDB 클라이언트 생성 실패: {e}")))?;
        Ok(Self { client })
    }

    /// `hello` + `buildInfo`로 토폴로지·서버 버전을 조회한다.
    pub async fn server_meta(&self) -> Result<ServerMeta> {
        let admin = self.client.database("admin");

        let hello = admin
            .run_command(doc! { "hello": 1 })
            .await
            .map_err(|e| XBackupError::Failure(format!("hello 명령 실패: {e}")))?;
        // setName이 있으면 replica set. mongos(sharded)는 msg="isdbgrid"로 구분되나,
        // 샤딩 거부는 status(t14) 소유 — 여기서는 oplog 가능 여부만 본다.
        let repl_set_name = hello.get_str("setName").ok().map(|s| s.to_string());

        let build_info = admin
            .run_command(doc! { "buildInfo": 1 })
            .await
            .map_err(|e| XBackupError::Failure(format!("buildInfo 명령 실패: {e}")))?;
        let server_version = build_info
            .get_str("version")
            .map_err(|e| XBackupError::Failure(format!("서버 버전 조회 실패: {e}")))?
            .to_string();

        Ok(ServerMeta {
            repl_set_name,
            server_version,
        })
    }

    /// 사용자 네임스페이스(`db.collection`) 목록을 조회한다(복구 사전 점검·충돌 감지용).
    ///
    /// 시스템 DB(`admin`/`config`/`local`)는 제외한다 — 복구가 덮어쓸 *사용자 데이터*만
    /// "기존 데이터"로 본다(PRD §FR-3 가드레일·dry-run 충돌 목록). 결과는 정렬된 `db.coll`
    /// 문자열 벡터다. `ns_filter`가 `Some(db.coll)`이면 그 네임스페이스로 한정해 본다
    /// (선택적 복구 `--only` 시 충돌 범위를 좁힌다).
    pub async fn user_namespaces(&self, ns_filter: Option<&str>) -> Result<Vec<String>> {
        const SYSTEM_DBS: [&str; 3] = ["admin", "config", "local"];

        // 충돌 범위를 특정 db로 좁힐 수 있으면(--only db.coll) 그 db만 본다.
        let only_db = ns_filter.and_then(|ns| ns.split('.').next());

        let db_names = self.client.list_database_names().await.map_err(|e| {
            XBackupError::Failure(format!("데이터베이스 목록 조회 실패: {e}"))
        })?;

        let mut namespaces = Vec::new();
        for db_name in db_names {
            if SYSTEM_DBS.contains(&db_name.as_str()) {
                continue;
            }
            if let Some(want) = only_db {
                if db_name != want {
                    continue;
                }
            }
            let db = self.client.database(&db_name);
            let colls = db.list_collection_names().await.map_err(|e| {
                XBackupError::Failure(format!("컬렉션 목록 조회 실패({db_name}): {e}"))
            })?;
            for coll in colls {
                // system.* 컬렉션은 사용자 데이터가 아니므로 제외.
                if coll.starts_with("system.") {
                    continue;
                }
                namespaces.push(format!("{db_name}.{coll}"));
            }
        }
        namespaces.sort();

        // 특정 네임스페이스 필터(db.collection 전체 지정)면 정확히 일치하는 것만.
        if let Some(ns) = ns_filter {
            if ns.contains('.') && ns.split('.').count() == 2 && !ns.ends_with('.') {
                namespaces.retain(|existing| existing == ns);
            }
        }

        Ok(namespaces)
    }

    /// `local.oplog.rs`의 최신 엔트리 ts를 반환한다(natural order 내림차순 1건).
    ///
    /// standalone 등 oplog 부재 시 `None`. dump 전후로 호출해 oplog 구간을 산정한다.
    pub async fn latest_oplog_ts(&self) -> Result<Option<Timestamp>> {
        let oplog = self
            .client
            .database("local")
            .collection::<bson::Document>("oplog.rs");

        // $natural:-1 정렬로 가장 최근 엔트리 1건.
        let options = FindOneOptions::builder()
            .sort(doc! { "$natural": -1 })
            .build();

        let latest = oplog
            .find_one(doc! {})
            .with_options(options)
            .await
            .map_err(|e| XBackupError::Failure(format!("oplog 최신 ts 조회 실패: {e}")))?;

        match latest {
            Some(entry) => match entry.get_timestamp("ts") {
                Ok(ts) => Ok(Some(ts)),
                // ts가 Timestamp가 아니면(이론상 없음) 조용히 None — gap 판단은 상위에서.
                Err(_) => Ok(None),
            },
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_from_set_name() {
        let rs = ServerMeta {
            repl_set_name: Some("rs0".to_string()),
            server_version: "7.0.35".to_string(),
        };
        assert_eq!(rs.topology(), Topology::ReplicaSet);
        assert!(rs.supports_oplog());

        let standalone = ServerMeta {
            repl_set_name: None,
            server_version: "7.0.35".to_string(),
        };
        assert_eq!(standalone.topology(), Topology::Standalone);
        assert!(!standalone.supports_oplog());
    }
}
