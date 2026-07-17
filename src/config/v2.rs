//! config **v2** — flat per-profile + 상속 표면(surface) 문법을 v1 nested 트리로 정규화한다.
//!
//! v2는 v1의 깊은 중첩(`[profiles.x.features.encryption]`)을 버리고, 프로파일당 하나의
//! `[profile.<name>]` 테이블 + flat 키로 표현한다. 공통 정책은 `[defaults]`(전체 적용)·
//! `[base.<name>]`(재사용 베이스)·`extends`로 한 번만 정의한다(중복 제거).
//!
//! v2는 **표면 문법일 뿐**이다 — [`normalize_v2`]가 파싱된 v2 `toml::Value`를 기존
//! 역직렬화기·ENV 오버라이드 파이프라인이 기대하는 정확한 `profiles.<name>.<...>` nested
//! 트리로 다시 쓴다. 따라서 내부 구조체와 모든 소비처는 무변경이고, v1 config는 그대로 통과한다.
//!
//! ## v1 ↔ v2 판별
//! 루트 테이블에 `profiles`(복수) 키가 있으면 v1, `profile`/`base`/`defaults`(단수)가 있으면 v2.
//! 둘 다 있으면 에러(반쪽 마이그레이션 방지). 둘 다 없으면 빈/ENV-only config로 그대로 통과.
//!
//! ## 키 매핑(요약)
//! - 접속: `uri`·`uri_env`·`prefer_secondary`·`connect_timeout_secs` → `source.*`
//! - 모드: `backup_type`·`output_mode`(→mode.output)·`precheck`·`engine` → `mode.*`
//! - 대상(단순): `dest = "local:/path"` | `"s3:bucket/prefix"` (+ `dest_name`·`s3_region`·
//!   `s3_endpoint`·`s3_prefix`·`s3_bucket`·`s3_creds`) → `destination.*`
//! - 대상(다중): `[[profile.x.dest]]` 테이블 배열 → `destinations[]`
//! - 압축: `compress = "zstd:6"`(또는 `compress_algorithm`/`compress_level`) → `features.compression.*`
//! - 암호화: `encrypt = "age:/path"` | `false` | `true`(또는 `encrypt_algorithm`/`recipient_file`)
//!   → `features.encryption.*`
//! - 증분: `incr_interval`·`incr_on_gap`·`pg_logical` → `features.incremental.*`
//! - 보존: `keep_full`·`keep_days`·`keep_last` → `retention.*`
//! - 상속: `extends = "name"` | `["a","b"]` (b가 a를 덮음)

use toml::value::{Table, Value};

use crate::error::{Result, XBackupError};

fn cfg_err(msg: impl Into<String>) -> XBackupError {
    XBackupError::Config(msg.into())
}

/// v2 표면을 v1 nested 트리로 정규화한다. v1/빈 config는 그대로 통과.
///
/// 이 함수는 **모든** raw-TOML→Config 진입점에서 호출되어야 한다(분기별 split-brain 방지).
/// 현재 호출점: [`crate::config::file::Config::from_toml_str`]와
/// [`crate::config::merged::ResolvedConfig::build_with`].
pub fn normalize_v2(root: Value) -> Result<Value> {
    let mut table = match root {
        Value::Table(t) => t,
        // 최상위가 테이블이 아니면 그대로 둔다(상위 역직렬화가 명확히 거부).
        other => return Ok(other),
    };

    let has_v1 = table.contains_key("profiles");
    let has_v2 = table.contains_key("profile")
        || table.contains_key("base")
        || table.contains_key("defaults");

    if has_v1 && has_v2 {
        return Err(cfg_err(
            "config에 v1(profiles)과 v2(profile/defaults/base)가 섞여 있습니다 — 한 형식만 쓰세요",
        ));
    }
    if !has_v2 {
        // v1 또는 빈 config — 무변경.
        return Ok(Value::Table(table));
    }

    // ── v2 → v1 정규화 ──
    let defaults = take_table(&mut table, "defaults")?;
    let bases = take_table(&mut table, "base")?;
    let profiles_in = take_table(&mut table, "profile")?;

    if profiles_in.is_empty() {
        return Err(cfg_err(
            "v2 config에 [profile.<name>] 프로파일이 하나도 없습니다",
        ));
    }

    let mut profiles_out = Table::new();
    for (name, prof_val) in &profiles_in {
        let prof = prof_val
            .as_table()
            .ok_or_else(|| cfg_err(format!("[profile.{name}]가 테이블이 아닙니다")))?;
        let flat = resolve_flat(
            name,
            prof,
            &defaults,
            &bases,
            &profiles_in,
            &mut vec![name.clone()],
        )?;
        let nested = expand_profile(name, &flat)?;
        profiles_out.insert(name.clone(), Value::Table(nested));
    }

    // default_profile·[output] 등 나머지 최상위 키는 보존된다(table에 그대로 남아 있음).
    table.insert("profiles".to_string(), Value::Table(profiles_out));
    Ok(Value::Table(table))
}

/// 테이블에서 키를 제거해 그 하위 테이블을 돌려준다(없으면 빈 테이블). 테이블이 아니면 에러.
fn take_table(t: &mut Table, key: &str) -> Result<Table> {
    match t.remove(key) {
        None => Ok(Table::new()),
        Some(Value::Table(tab)) => Ok(tab),
        Some(_) => Err(cfg_err(format!("'{key}'는 테이블이어야 합니다"))),
    }
}

/// 상속을 적용해 한 프로파일의 **실효 flat 키맵**을 만든다.
///
/// 우선순위(낮음→높음): `[defaults]` < `extends` 체인(왼→오, 뒤가 우선) < 프로파일 자신 키.
/// `extends`는 `[base.<name>]` 또는 다른 `[profile.<name>]`를 가리킨다. 사이클은 에러.
fn resolve_flat(
    name: &str,
    entity: &Table,
    defaults: &Table,
    bases: &Table,
    profiles: &Table,
    visiting: &mut Vec<String>,
) -> Result<Table> {
    let mut acc = Table::new();

    // 1) defaults(최하위).
    for (k, v) in defaults {
        if k != "extends" {
            acc.insert(k.clone(), v.clone());
        }
    }

    // 2) extends 체인.
    if let Some(ext) = entity.get("extends") {
        for base_name in extends_names(name, ext)? {
            if visiting.contains(&base_name) {
                return Err(cfg_err(format!(
                    "extends 순환 참조: {} → {base_name}",
                    visiting.join(" → ")
                )));
            }
            let base_entity = bases
                .get(&base_name)
                .or_else(|| profiles.get(&base_name))
                .and_then(Value::as_table)
                .ok_or_else(|| {
                    cfg_err(format!(
                        "[profile.{name}] extends 대상 '{base_name}'를 찾을 수 없습니다(base/profile)"
                    ))
                })?;
            visiting.push(base_name.clone());
            let base_flat = resolve_flat(&base_name, base_entity, defaults, bases, profiles, visiting)?;
            visiting.pop();
            for (k, v) in base_flat {
                acc.insert(k, v); // base가 defaults를 덮음
            }
        }
    }

    // 3) 자신 키(최상위 우선).
    for (k, v) in entity {
        if k != "extends" {
            acc.insert(k.clone(), v.clone());
        }
    }

    Ok(acc)
}

/// `extends` 값(문자열 또는 문자열 배열)을 이름 목록으로 변환한다.
fn extends_names(profile: &str, ext: &Value) -> Result<Vec<String>> {
    match ext {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(arr) => arr
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    cfg_err(format!("[profile.{profile}] extends 배열 항목은 문자열이어야 합니다"))
                })
            })
            .collect(),
        _ => Err(cfg_err(format!(
            "[profile.{profile}] extends는 문자열 또는 문자열 배열이어야 합니다"
        ))),
    }
}

/// 실효 flat 키맵을 v1 nested 프로파일 테이블로 펼친다. 알 수 없는 키는 거부(오타 보호).
fn expand_profile(name: &str, flat: &Table) -> Result<Table> {
    let mut source = Table::new();
    let mut mode = Table::new();
    let mut destination = Table::new();
    let mut s3 = Table::new();
    let mut compression = Table::new();
    let mut encryption = Table::new();
    let mut incremental = Table::new();
    let mut retention = Table::new();
    let mut hooks = Table::new();
    let mut destinations: Option<Value> = None;

    for (k, v) in flat {
        match k.as_str() {
            // ── source ──
            "uri" => insert_into(&mut source, "uri", v),
            "uri_env" => insert_into(&mut source, "uri_env", v),
            "read_uri" => insert_into(&mut source, "read_uri", v),
            "read_uri_env" => insert_into(&mut source, "read_uri_env", v),
            "prefer_secondary" => insert_into(&mut source, "prefer_secondary", v),
            "connect_timeout_secs" => insert_into(&mut source, "connect_timeout_secs", v),
            // ── mode ── (output_mode → mode.output: 루트 [output]와 혼동 방지)
            "backup_type" => insert_into(&mut mode, "backup_type", v),
            "output_mode" => insert_into(&mut mode, "output", v),
            "precheck" => insert_into(&mut mode, "precheck", v),
            "engine" => insert_into(&mut mode, "engine", v),
            // ── destination ── (단순 compact 문자열 또는 다중 테이블 배열)
            "dest" => match v {
                Value::String(s) => parse_dest_compact(name, s, &mut destination, &mut s3)?,
                Value::Array(arr) => destinations = Some(expand_dest_array(name, arr)?),
                _ => {
                    return Err(cfg_err(format!(
                        "[profile.{name}] dest는 문자열(\"local:/path\") 또는 [[profile.{name}.dest]] 배열이어야 합니다"
                    )))
                }
            },
            "dest_name" => insert_into(&mut destination, "name", v),
            "s3_bucket" => insert_into(&mut s3, "bucket", v),
            "s3_prefix" => insert_into(&mut s3, "prefix", v),
            "s3_region" => insert_into(&mut s3, "region", v),
            "s3_endpoint" => insert_into(&mut s3, "endpoint", v),
            "s3_creds" => insert_into(&mut s3, "credentials_env", v),
            // ── compression ──
            "compress" => parse_compress(name, v, &mut compression)?,
            "compress_algorithm" => insert_into(&mut compression, "algorithm", v),
            "compress_level" => insert_into(&mut compression, "level", v),
            // ── encryption ──
            "encrypt" => parse_encrypt(name, v, &mut encryption)?,
            "encrypt_algorithm" => insert_into(&mut encryption, "algorithm", v),
            "recipient_file" => insert_into(&mut encryption, "recipient_file", v),
            // ── incremental ──
            "incr_interval" => insert_into(&mut incremental, "interval", v),
            "incr_on_gap" => insert_into(&mut incremental, "on_gap", v),
            "pg_logical" => insert_into(&mut incremental, "pg_logical", v),
            "mysql_binlog" => insert_into(&mut incremental, "mysql_binlog", v),
            // ── retention ──
            "keep_full" => insert_into(&mut retention, "keep_full", v),
            "keep_days" => insert_into(&mut retention, "keep_days", v),
            "keep_last" => insert_into(&mut retention, "keep_last", v),
            "recovery_window_days" => insert_into(&mut retention, "recovery_window_days", v),
            "min_redundancy" => insert_into(&mut retention, "min_redundancy", v),
            // ── hooks ── (flat `hook_*` → hooks.*)
            "hook_pre_backup" => insert_into(&mut hooks, "pre_backup", v),
            "hook_post_backup" => insert_into(&mut hooks, "post_backup", v),
            "hook_pre_restore" => insert_into(&mut hooks, "pre_restore", v),
            "hook_post_restore" => insert_into(&mut hooks, "post_restore", v),
            "hook_pre_prune" => insert_into(&mut hooks, "pre_prune", v),
            "hook_post_prune" => insert_into(&mut hooks, "post_prune", v),
            "hook_on_error" => insert_into(&mut hooks, "on_error", v),
            "hook_timeout_secs" => insert_into(&mut hooks, "hook_timeout_secs", v),
            "extends" => {} // 이미 소비됨
            other => {
                return Err(cfg_err(format!(
                    "[profile.{name}] 알 수 없는 키 '{other}'"
                )))
            }
        }
    }

    // s3 하위테이블을 destination에 합친다(단일 dest 경로).
    if !s3.is_empty() {
        destination.insert("s3".to_string(), Value::Table(s3));
    }

    let mut profile = Table::new();
    if !source.is_empty() {
        profile.insert("source".to_string(), Value::Table(source));
    }
    if !mode.is_empty() {
        profile.insert("mode".to_string(), Value::Table(mode));
    }
    // destinations(다중)가 있으면 그것이 우선(effective_destinations 규칙), 아니면 단일 destination.
    if let Some(d) = destinations {
        profile.insert("destinations".to_string(), d);
    } else if !destination.is_empty() {
        profile.insert("destination".to_string(), Value::Table(destination));
    }
    let mut features = Table::new();
    if !compression.is_empty() {
        features.insert("compression".to_string(), Value::Table(compression));
    }
    if !encryption.is_empty() {
        features.insert("encryption".to_string(), Value::Table(encryption));
    }
    if !incremental.is_empty() {
        features.insert("incremental".to_string(), Value::Table(incremental));
    }
    if !features.is_empty() {
        profile.insert("features".to_string(), Value::Table(features));
    }
    if !retention.is_empty() {
        profile.insert("retention".to_string(), Value::Table(retention));
    }
    if !hooks.is_empty() {
        profile.insert("hooks".to_string(), Value::Table(hooks));
    }

    Ok(profile)
}

fn insert_into(t: &mut Table, key: &str, v: &Value) {
    t.insert(key.to_string(), v.clone());
}

/// `dest = "local:/path"` | `"s3:bucket/prefix"` 를 destination/s3 테이블로 파싱.
fn parse_dest_compact(name: &str, s: &str, dest: &mut Table, s3: &mut Table) -> Result<()> {
    let (scheme, rest) = s.split_once(':').ok_or_else(|| {
        cfg_err(format!(
            "[profile.{name}] dest \"{s}\": \"local:/path\" 또는 \"s3:bucket/prefix\" 형식이어야 합니다"
        ))
    })?;
    match scheme {
        "local" => {
            if rest.is_empty() {
                return Err(cfg_err(format!(
                    "[profile.{name}] dest \"{s}\": local 경로가 비었습니다"
                )));
            }
            dest.insert("type".to_string(), Value::String("local".to_string()));
            dest.insert("path".to_string(), Value::String(rest.to_string()));
        }
        "s3" => {
            dest.insert("type".to_string(), Value::String("s3".to_string()));
            let (bucket, prefix) = match rest.split_once('/') {
                Some((b, p)) => (b, Some(p)),
                None => (rest, None),
            };
            if bucket.is_empty() {
                return Err(cfg_err(format!(
                    "[profile.{name}] dest \"{s}\": s3 버킷이 비었습니다"
                )));
            }
            s3.insert("bucket".to_string(), Value::String(bucket.to_string()));
            if let Some(p) = prefix.filter(|p| !p.is_empty()) {
                s3.insert("prefix".to_string(), Value::String(p.to_string()));
            }
        }
        other => {
            return Err(cfg_err(format!(
                "[profile.{name}] dest \"{s}\": 알 수 없는 스킴 '{other}'(local|s3)"
            )))
        }
    }
    Ok(())
}

/// `[[profile.x.dest]]` 테이블 배열 → v1 `destinations[]` 배열. 각 항목은 compact `dest="..."`
/// 또는 명시 키(type/path/name/s3_*)를 쓴다.
fn expand_dest_array(name: &str, arr: &[Value]) -> Result<Value> {
    let mut out = Vec::with_capacity(arr.len());
    for (i, elem) in arr.iter().enumerate() {
        let t = elem.as_table().ok_or_else(|| {
            cfg_err(format!("[[profile.{name}.dest]] {i}번 항목이 테이블이 아닙니다"))
        })?;
        let mut d = Table::new();
        let mut s3 = Table::new();
        for (k, v) in t {
            match k.as_str() {
                "dest" => {
                    let s = v.as_str().ok_or_else(|| {
                        cfg_err(format!("[[profile.{name}.dest]] {i}: dest는 문자열이어야 합니다"))
                    })?;
                    parse_dest_compact(name, s, &mut d, &mut s3)?;
                }
                "type" => insert_into(&mut d, "type", v),
                "path" => insert_into(&mut d, "path", v),
                "name" => insert_into(&mut d, "name", v),
                "s3_bucket" => insert_into(&mut s3, "bucket", v),
                "s3_prefix" => insert_into(&mut s3, "prefix", v),
                "s3_region" => insert_into(&mut s3, "region", v),
                "s3_endpoint" => insert_into(&mut s3, "endpoint", v),
                "s3_creds" => insert_into(&mut s3, "credentials_env", v),
                other => {
                    return Err(cfg_err(format!(
                        "[[profile.{name}.dest]] {i}: 알 수 없는 키 '{other}'"
                    )))
                }
            }
        }
        if !s3.is_empty() {
            d.insert("s3".to_string(), Value::Table(s3));
        }
        out.push(Value::Table(d));
    }
    Ok(Value::Array(out))
}

/// `compress = "zstd:6"` | `"zstd"` 를 compression 테이블로 파싱.
fn parse_compress(name: &str, v: &Value, comp: &mut Table) -> Result<()> {
    let s = v.as_str().ok_or_else(|| {
        cfg_err(format!(
            "[profile.{name}] compress는 \"zstd:6\" 같은 문자열이어야 합니다"
        ))
    })?;
    let (algo, level) = match s.split_once(':') {
        Some((a, l)) => (a, Some(l)),
        None => (s, None),
    };
    if algo.is_empty() {
        return Err(cfg_err(format!(
            "[profile.{name}] compress \"{s}\": 알고리즘이 비었습니다"
        )));
    }
    comp.insert("algorithm".to_string(), Value::String(algo.to_string()));
    if let Some(l) = level {
        let lvl: i64 = l.parse().map_err(|_| {
            cfg_err(format!(
                "[profile.{name}] compress 레벨 '{l}'은 정수여야 합니다"
            ))
        })?;
        comp.insert("level".to_string(), Value::Integer(lvl));
    }
    Ok(())
}

/// `encrypt = "age:/path"` | `true`/`false` | `"off"` 를 encryption 테이블로 파싱.
fn parse_encrypt(name: &str, v: &Value, enc: &mut Table) -> Result<()> {
    match v {
        Value::Boolean(b) => {
            enc.insert("enabled".to_string(), Value::Boolean(*b));
        }
        Value::String(s) if s == "off" || s == "false" => {
            enc.insert("enabled".to_string(), Value::Boolean(false));
        }
        Value::String(s) => {
            let (algo, recipient) = match s.split_once(':') {
                Some((a, r)) => (a, Some(r)),
                None => (s.as_str(), None),
            };
            if algo.is_empty() {
                return Err(cfg_err(format!(
                    "[profile.{name}] encrypt \"{s}\": 알고리즘이 비었습니다"
                )));
            }
            enc.insert("enabled".to_string(), Value::Boolean(true));
            enc.insert("algorithm".to_string(), Value::String(algo.to_string()));
            if let Some(r) = recipient.filter(|r| !r.is_empty()) {
                enc.insert("recipient_file".to_string(), Value::String(r.to_string()));
            }
        }
        _ => {
            return Err(cfg_err(format!(
                "[profile.{name}] encrypt는 bool 또는 \"age:/path\"/\"off\" 문자열이어야 합니다"
            )))
        }
    }
    Ok(())
}

// ── v2 직렬화(normalize_v2의 역연산) ──
//
// `Config`(v1 nested 구조체)를 **v2 표면 TOML 텍스트**로 다시 쓴다. 핵심 규칙:
//  - 프로파일당 `[profile.<name>]` 하나 + flat 키.
//  - 문서화된 기본값과 같은 값은 **생략**(출력 최소화) — emitter가 omit하면 normalize_v2의
//    역직렬화기가 같은 기본값을 다시 채운다(왕복 보존).
//  - compact 폼: `dest="local:/path"`/`"s3:bucket/prefix"`, `compress="zstd:N"`,
//    `encrypt="age:/path"`(또는 `encrypt=false`).
//  - endpoint 전용(destination 없음) 프로파일은 dest를 내보내지 않는다.
//  - serde Serialize(중첩 구조체)는 v1을 만들므로 쓰지 않고 **TOML 문자열을 직접 조립**한다.
//
// [defaults]/extends는 손-작성 편의 기능이라 단일 Config에서 내보내지 않는다 — 각 프로파일을
// (기본값 생략한 채) 완전하게 내보낸다.

use crate::config::file::{Config, DestinationConfig, Profile};

/// TOML 기본 문자열(basic string)로 한 줄 이스케이프한다.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `key = "value"` 한 줄을 버퍼에 추가한다.
fn line_str(buf: &mut String, key: &str, val: &str) {
    buf.push_str(key);
    buf.push_str(" = ");
    buf.push_str(&esc(val));
    buf.push('\n');
}

/// `key = <raw>` 한 줄(불리언/정수 등 비-문자열)을 추가한다.
fn line_raw(buf: &mut String, key: &str, raw: &str) {
    buf.push_str(key);
    buf.push_str(" = ");
    buf.push_str(raw);
    buf.push('\n');
}

/// destination(단일/배열 항목)을 compact `dest` 문자열로 만든다(local|s3). s3 추가 키가
/// compact로 표현 불가하면(region/endpoint/creds, 혹은 name) `None`을 반환해 호출자가
/// 명시 키로 폴백하게 한다.
fn dest_compact(d: &DestinationConfig) -> Option<String> {
    match d.r#type.as_deref()? {
        "local" => d.path.as_deref().map(|p| format!("local:{p}")),
        "s3" => {
            let s3 = d.s3.as_ref()?;
            let bucket = s3.bucket.as_deref()?;
            // compact 폼은 bucket(/prefix)만 담는다 — 나머지는 별도 키로.
            match s3.prefix.as_deref().filter(|p| !p.is_empty()) {
                Some(p) => Some(format!("s3:{bucket}/{p}")),
                None => Some(format!("s3:{bucket}")),
            }
        }
        _ => None,
    }
}

/// 단일 destination을 flat 키들로 내보낸다([profile.x] 본문 내, dest_name/s3_* 포함).
fn emit_single_dest(buf: &mut String, d: &DestinationConfig) {
    if let Some(name) = &d.name {
        line_str(buf, "dest_name", name);
    }
    match d.r#type.as_deref() {
        Some("local") => {
            if let Some(p) = &d.path {
                line_str(buf, "dest", &format!("local:{p}"));
            }
        }
        Some("s3") => {
            if let Some(s3) = &d.s3 {
                // bucket(+prefix)은 compact dest로, 나머지는 별도 s3_* 키로.
                if let Some(b) = &s3.bucket {
                    match s3.prefix.as_deref().filter(|p| !p.is_empty()) {
                        Some(p) => line_str(buf, "dest", &format!("s3:{b}/{p}")),
                        None => line_str(buf, "dest", &format!("s3:{b}")),
                    }
                } else {
                    // bucket이 없으면 compact 불가 — type만이라도 명시.
                    line_str(buf, "dest", "s3:");
                }
                if let Some(r) = &s3.region {
                    line_str(buf, "s3_region", r);
                }
                if let Some(e) = &s3.endpoint {
                    line_str(buf, "s3_endpoint", e);
                }
                if let Some(c) = &s3.credentials_env {
                    line_str(buf, "s3_creds", c);
                }
            }
        }
        _ => {}
    }
}

/// 한 프로파일 본문(flat 키들)을 버퍼에 내보낸다(헤더는 호출자가 출력).
fn emit_profile_body(buf: &mut String, p: &Profile) {
    // ── source ──
    if let Some(uri) = &p.source.uri {
        line_str(buf, "uri", uri);
    }
    if let Some(env) = &p.source.uri_env {
        line_str(buf, "uri_env", env);
    }
    if p.source.prefer_secondary {
        line_raw(buf, "prefer_secondary", "true");
    }
    if let Some(t) = p.source.connect_timeout_secs {
        line_raw(buf, "connect_timeout_secs", &t.to_string());
    }

    // ── mode(기본값 생략) ──
    if p.mode.backup_type != "full" {
        line_str(buf, "backup_type", &p.mode.backup_type);
    }
    if p.mode.output != "progress" {
        line_str(buf, "output_mode", &p.mode.output);
    }
    if !p.mode.precheck {
        line_raw(buf, "precheck", "false");
    }
    if p.mode.engine != "native" {
        line_str(buf, "engine", &p.mode.engine);
    }

    // ── destination(들) ── endpoint 전용이면 아무것도 내보내지 않는다.
    if !p.destinations.is_empty() {
        // 다중 destination — [[profile.<name>.dest]] 배열. (헤더는 호출자가 추가.)
        // 여기서는 표식만 남기지 않고, 호출자에서 별도 처리하므로 단일 경로만 다룬다.
        // (emit_profile은 다중을 직접 처리한다.)
    } else if !p.is_endpoint_only() {
        emit_single_dest(buf, &p.destination);
    }

    // ── compression(기본 zstd/10 생략) ──
    let c = &p.features.compression;
    if c.algorithm != "zstd" || c.level != 10 {
        if c.algorithm == "zstd" {
            // level만 다른 흔한 경우도 compact 폼으로.
            line_str(buf, "compress", &format!("zstd:{}", c.level));
        } else {
            line_str(buf, "compress", &format!("{}:{}", c.algorithm, c.level));
        }
    }

    // ── encryption(기본 enabled=true+age 생략; recipient_file 있으면 내보냄) ──
    let e = &p.features.encryption;
    if !e.enabled {
        line_raw(buf, "encrypt", "false");
    } else if e.algorithm == "age" {
        // age + recipient 있으면 "age:<path>", 없으면 기본값과 동일 → 생략.
        if let Some(rf) = &e.recipient_file {
            line_str(buf, "encrypt", &format!("age:{rf}"));
        }
    } else {
        // 비-age 알고리즘 — 명시 키로.
        line_str(buf, "encrypt_algorithm", &e.algorithm);
        if let Some(rf) = &e.recipient_file {
            line_str(buf, "recipient_file", rf);
        }
    }

    // ── incremental(기본 15m/promote_full/false 생략) ──
    let i = &p.features.incremental;
    if i.interval != "15m" {
        line_str(buf, "incr_interval", &i.interval);
    }
    if i.on_gap != "promote_full" {
        line_str(buf, "incr_on_gap", &i.on_gap);
    }
    if i.pg_logical {
        line_raw(buf, "pg_logical", "true");
    }
    if i.mysql_binlog {
        line_raw(buf, "mysql_binlog", "true");
    }

    // ── retention(모두 선택) ──
    if let Some(k) = p.retention.keep_full {
        line_raw(buf, "keep_full", &k.to_string());
    }
    if let Some(k) = p.retention.keep_days {
        line_raw(buf, "keep_days", &k.to_string());
    }
    if let Some(k) = p.retention.keep_last {
        line_raw(buf, "keep_last", &k.to_string());
    }
}

/// 다중 destination을 `[[profile.<name>.dest]]` 테이블 배열로 내보낸다.
fn emit_multi_dest(buf: &mut String, name: &str, dests: &[DestinationConfig]) {
    for d in dests {
        buf.push_str(&format!("[[profile.{name}.dest]]\n"));
        if let Some(n) = &d.name {
            line_str(buf, "name", n);
        }
        match dest_compact(d) {
            Some(compact) => {
                line_str(buf, "dest", &compact);
                // s3의 compact 불가 키는 명시 키로 보강.
                if d.r#type.as_deref() == Some("s3") {
                    if let Some(s3) = &d.s3 {
                        if let Some(r) = &s3.region {
                            line_str(buf, "s3_region", r);
                        }
                        if let Some(e) = &s3.endpoint {
                            line_str(buf, "s3_endpoint", e);
                        }
                        if let Some(c) = &s3.credentials_env {
                            line_str(buf, "s3_creds", c);
                        }
                    }
                }
            }
            None => {
                // compact 불가 — type/path/s3_* 명시 키로.
                if let Some(t) = &d.r#type {
                    line_str(buf, "type", t);
                }
                if let Some(p) = &d.path {
                    line_str(buf, "path", p);
                }
                if let Some(s3) = &d.s3 {
                    if let Some(b) = &s3.bucket {
                        line_str(buf, "s3_bucket", b);
                    }
                    if let Some(p) = &s3.prefix {
                        line_str(buf, "s3_prefix", p);
                    }
                    if let Some(r) = &s3.region {
                        line_str(buf, "s3_region", r);
                    }
                    if let Some(e) = &s3.endpoint {
                        line_str(buf, "s3_endpoint", e);
                    }
                    if let Some(c) = &s3.credentials_env {
                        line_str(buf, "s3_creds", c);
                    }
                }
            }
        }
        buf.push('\n');
    }
}

/// [`Config`]를 **v2 표면 TOML 텍스트**로 직렬화한다(normalize_v2가 다시 읽을 수 있는 형식).
///
/// 문서화된 기본값과 같은 값은 생략해 출력을 최소화한다. 다중 destination은
/// `[[profile.<name>.dest]]` 배열로, 단일은 compact `dest=...`로 내보낸다.
pub fn to_v2_string(config: &Config) -> Result<String> {
    let mut buf = String::new();

    // 최상위: default_profile, [output] language.
    if let Some(dp) = &config.default_profile {
        line_str(&mut buf, "default_profile", dp);
    }
    if let Some(out) = &config.output {
        if let Some(lang) = &out.language {
            buf.push_str("\n[output]\n");
            line_str(&mut buf, "language", lang);
        }
    }

    // 각 프로파일 — [profile.<name>] + flat 키, 그리고 다중 dest는 별도 테이블 배열.
    for (name, p) in &config.profiles {
        buf.push_str(&format!("\n[profile.{name}]\n"));
        emit_profile_body(&mut buf, p);
        if !p.destinations.is_empty() {
            buf.push('\n');
            emit_multi_dest(&mut buf, name, &p.destinations);
        }
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> Result<Value> {
        normalize_v2(toml::from_str::<Value>(s).unwrap())
    }

    /// v1(profiles 복수)은 그대로 통과한다.
    #[test]
    fn v1_passthrough_unchanged() {
        let src = "default_profile = \"p\"\n[profiles.p.source]\nuri = \"mongodb://h/db\"\n";
        let before = toml::from_str::<Value>(src).unwrap();
        let after = normalize_v2(before.clone()).unwrap();
        assert_eq!(before, after);
    }

    /// 빈/ENV-only config도 그대로 통과한다.
    #[test]
    fn empty_passthrough() {
        let after = norm("").unwrap();
        assert_eq!(after, Value::Table(Table::new()));
    }

    /// v1과 v2가 섞이면 에러.
    #[test]
    fn mixed_v1_v2_is_error() {
        let err = norm("[profiles.a]\n[profile.b]\n").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// 기본 v2 단일 프로파일 → nested로 펼쳐진다(compact dest/compress/encrypt 포함).
    #[test]
    fn basic_v2_expands_to_nested() {
        let v = norm(
            "default_profile = \"mongo\"\n\
             [profile.mongo]\n\
             uri = \"mongodb://localhost:27017/db\"\n\
             dest = \"local:/srv/store/mongo\"\n\
             compress = \"zstd:6\"\n\
             encrypt = \"age:/keys/age.pub\"\n\
             pg_logical = true\n",
        )
        .unwrap();
        // v1 Config로 역직렬화되어야 한다.
        let cfg: crate::config::file::Config = v.try_into().unwrap();
        assert_eq!(cfg.default_profile.as_deref(), Some("mongo"));
        let p = cfg.profile("mongo").unwrap();
        assert_eq!(p.source.uri.as_deref(), Some("mongodb://localhost:27017/db"));
        assert_eq!(p.destination.r#type.as_deref(), Some("local"));
        assert_eq!(p.destination.path.as_deref(), Some("/srv/store/mongo"));
        assert_eq!(p.features.compression.algorithm, "zstd");
        assert_eq!(p.features.compression.level, 6);
        assert!(p.features.encryption.enabled);
        assert_eq!(p.features.encryption.algorithm, "age");
        assert_eq!(
            p.features.encryption.recipient_file.as_deref(),
            Some("/keys/age.pub")
        );
        assert!(p.features.incremental.pg_logical);
    }

    /// compact s3 dest → destination.s3.{bucket,prefix} + 별도 s3_* 키 병합.
    #[test]
    fn compact_s3_dest_and_extra_keys() {
        let v = norm(
            "[profile.p]\n\
             uri_env = \"U\"\n\
             dest = \"s3:db-backups/mongo/prod\"\n\
             s3_region = \"ap-northeast-2\"\n\
             s3_creds = \"S3_CREDS\"\n",
        )
        .unwrap();
        let cfg: crate::config::file::Config = v.try_into().unwrap();
        let p = cfg.profile("p").unwrap();
        let s3 = p.destination.s3.as_ref().unwrap();
        assert_eq!(p.destination.r#type.as_deref(), Some("s3"));
        assert_eq!(s3.bucket.as_deref(), Some("db-backups"));
        assert_eq!(s3.prefix.as_deref(), Some("mongo/prod"));
        assert_eq!(s3.region.as_deref(), Some("ap-northeast-2"));
        assert_eq!(s3.credentials_env.as_deref(), Some("S3_CREDS"));
    }

    /// [defaults] 상속 + 프로파일 오버라이드(encrypt=false).
    #[test]
    fn defaults_inheritance_and_override() {
        let v = norm(
            "default_profile = \"mongo\"\n\
             [defaults]\n\
             compress = \"zstd:6\"\n\
             encrypt = \"age:/keys/age.pub\"\n\
             [profile.mongo]\n\
             uri = \"mongodb://h:27017/db\"\n\
             dest = \"local:/s/mongo\"\n\
             [profile.target]\n\
             uri = \"mongodb://h:27117/db\"\n\
             encrypt = false\n",
        )
        .unwrap();
        let cfg: crate::config::file::Config = v.try_into().unwrap();
        // mongo: defaults 상속 → 암호화 on + recipient.
        let m = cfg.profile("mongo").unwrap();
        assert!(m.features.encryption.enabled);
        assert_eq!(
            m.features.encryption.recipient_file.as_deref(),
            Some("/keys/age.pub")
        );
        assert_eq!(m.features.compression.level, 6);
        // target: encrypt=false 오버라이드 + destination 없음(endpoint 전용).
        let t = cfg.profile("target").unwrap();
        assert!(!t.features.encryption.enabled);
        assert!(t.is_endpoint_only(), "target은 dest가 없어 endpoint 전용이어야 함");
    }

    /// extends 체인(base) — base가 defaults를 덮고, 프로파일이 base를 덮는다.
    #[test]
    fn extends_base_chain() {
        let v = norm(
            "[defaults]\n\
             compress = \"zstd:3\"\n\
             [base.s3prod]\n\
             dest = \"s3:bucket/p\"\n\
             s3_region = \"ap-northeast-2\"\n\
             compress = \"zstd:9\"\n\
             [profile.prod]\n\
             extends = \"s3prod\"\n\
             uri_env = \"U\"\n",
        )
        .unwrap();
        let cfg: crate::config::file::Config = v.try_into().unwrap();
        let p = cfg.profile("prod").unwrap();
        assert_eq!(p.destination.r#type.as_deref(), Some("s3"));
        assert_eq!(p.features.compression.level, 9, "base가 defaults(3)를 덮어 9");
        assert_eq!(p.source.uri_env.as_deref(), Some("U"));
    }

    /// extends 순환 참조는 에러.
    #[test]
    fn extends_cycle_is_error() {
        let err = norm(
            "[profile.a]\nextends = \"b\"\nuri=\"x\"\n[profile.b]\nextends = \"a\"\nuri=\"y\"\n",
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// 알 수 없는 키는 거부(오타 보호).
    #[test]
    fn unknown_key_is_error() {
        let err = norm("[profile.p]\nuri=\"x\"\nnope = 1\n").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("nope"));
    }

    /// 다중 destination — [[profile.x.dest]] 배열 → destinations[].
    #[test]
    fn multi_destination_array() {
        let v = norm(
            "[profile.p]\n\
             uri = \"mongodb://h/db\"\n\
             [[profile.p.dest]]\n\
             name = \"primary\"\n\
             dest = \"s3:bucket/p\"\n\
             s3_region = \"ap-northeast-2\"\n\
             [[profile.p.dest]]\n\
             name = \"offsite\"\n\
             dest = \"local:/mnt/off\"\n",
        )
        .unwrap();
        let cfg: crate::config::file::Config = v.try_into().unwrap();
        let p = cfg.profile("p").unwrap();
        let dests = p.effective_destinations();
        assert_eq!(dests.len(), 2);
        assert_eq!(dests[0].name.as_deref(), Some("primary"));
        assert_eq!(dests[0].r#type.as_deref(), Some("s3"));
        assert_eq!(dests[0].s3.as_ref().unwrap().bucket.as_deref(), Some("bucket"));
        assert_eq!(dests[1].name.as_deref(), Some("offsite"));
        assert_eq!(dests[1].path.as_deref(), Some("/mnt/off"));
    }

    /// encrypt = true (recipient 없이) → enabled만 true.
    #[test]
    fn encrypt_bool_true() {
        let v = norm("[profile.p]\nuri=\"x\"\nencrypt = true\n").unwrap();
        let cfg: crate::config::file::Config = v.try_into().unwrap();
        assert!(cfg.profile("p").unwrap().features.encryption.enabled);
    }

    // ── to_v2_string 왕복 테스트 ──

    use crate::config::file::{
        CompressionConfig, EncryptionConfig, FeaturesConfig, IncrementalConfig, ModeConfig,
        RetentionConfig, S3Config, SourceConfig,
    };
    // Config, DestinationConfig, Profile은 super(모듈 본문)에서 이미 가져온다.

    /// 두 프로파일을 의미 있는 모든 필드에서 동등 비교한다(serde 구조체에 PartialEq가 없으므로
    /// 필드별 단언).
    fn assert_profile_eq(a: &Profile, b: &Profile, ctx: &str) {
        assert_eq!(a.source.uri, b.source.uri, "{ctx}: source.uri");
        assert_eq!(a.source.uri_env, b.source.uri_env, "{ctx}: source.uri_env");
        assert_eq!(
            a.source.prefer_secondary, b.source.prefer_secondary,
            "{ctx}: prefer_secondary"
        );
        assert_eq!(
            a.source.connect_timeout_secs, b.source.connect_timeout_secs,
            "{ctx}: connect_timeout_secs"
        );
        assert_eq!(a.mode.backup_type, b.mode.backup_type, "{ctx}: backup_type");
        assert_eq!(a.mode.output, b.mode.output, "{ctx}: output");
        assert_eq!(a.mode.precheck, b.mode.precheck, "{ctx}: precheck");
        assert_eq!(a.mode.engine, b.mode.engine, "{ctx}: engine");
        // destination(들): effective_destinations로 정규화 비교.
        let ad = a.effective_destinations();
        let bd = b.effective_destinations();
        assert_eq!(ad.len(), bd.len(), "{ctx}: destinations len");
        for (i, (x, y)) in ad.iter().zip(bd.iter()).enumerate() {
            assert_dest_eq(x, y, &format!("{ctx}: dest[{i}]"));
        }
        assert_eq!(
            a.features.compression.algorithm, b.features.compression.algorithm,
            "{ctx}: compress.algorithm"
        );
        assert_eq!(
            a.features.compression.level, b.features.compression.level,
            "{ctx}: compress.level"
        );
        assert_eq!(
            a.features.encryption.enabled, b.features.encryption.enabled,
            "{ctx}: encrypt.enabled"
        );
        assert_eq!(
            a.features.encryption.algorithm, b.features.encryption.algorithm,
            "{ctx}: encrypt.algorithm"
        );
        assert_eq!(
            a.features.encryption.recipient_file, b.features.encryption.recipient_file,
            "{ctx}: encrypt.recipient_file"
        );
        assert_eq!(
            a.features.incremental.interval, b.features.incremental.interval,
            "{ctx}: incr.interval"
        );
        assert_eq!(
            a.features.incremental.on_gap, b.features.incremental.on_gap,
            "{ctx}: incr.on_gap"
        );
        assert_eq!(
            a.features.incremental.pg_logical, b.features.incremental.pg_logical,
            "{ctx}: incr.pg_logical"
        );
        assert_eq!(
            a.retention.keep_full, b.retention.keep_full,
            "{ctx}: keep_full"
        );
        assert_eq!(
            a.retention.keep_days, b.retention.keep_days,
            "{ctx}: keep_days"
        );
        assert_eq!(
            a.retention.keep_last, b.retention.keep_last,
            "{ctx}: keep_last"
        );
    }

    fn assert_dest_eq(a: &DestinationConfig, b: &DestinationConfig, ctx: &str) {
        assert_eq!(a.name, b.name, "{ctx}: name");
        assert_eq!(a.r#type, b.r#type, "{ctx}: type");
        assert_eq!(a.path, b.path, "{ctx}: path");
        match (&a.s3, &b.s3) {
            (None, None) => {}
            (Some(x), Some(y)) => {
                assert_eq!(x.bucket, y.bucket, "{ctx}: s3.bucket");
                assert_eq!(x.prefix, y.prefix, "{ctx}: s3.prefix");
                assert_eq!(x.region, y.region, "{ctx}: s3.region");
                assert_eq!(x.endpoint, y.endpoint, "{ctx}: s3.endpoint");
                assert_eq!(x.credentials_env, y.credentials_env, "{ctx}: s3.creds");
            }
            _ => panic!("{ctx}: s3 presence mismatch"),
        }
    }

    /// to_v2_string → from_toml_str 왕복 후 모든 프로파일이 동등해야 한다.
    fn roundtrip(cfg: &Config) -> Config {
        let text = to_v2_string(cfg).unwrap();
        // 생성된 텍스트는 v2로 인식되어야 한다(profile 단수).
        assert!(
            text.contains("[profile."),
            "v2 출력에 [profile.<name>]가 있어야 함:\n{text}"
        );
        let parsed = Config::from_toml_str(&text)
            .unwrap_or_else(|e| panic!("v2 출력 재파싱 실패: {e}\n--- text ---\n{text}"));
        for (name, p) in &cfg.profiles {
            let q = parsed
                .profile(name)
                .unwrap_or_else(|_| panic!("프로파일 {name} 누락\n{text}"));
            assert_profile_eq(p, q, name);
        }
        assert_eq!(cfg.default_profile, parsed.default_profile, "default_profile");
        parsed
    }

    #[test]
    fn roundtrip_local_age_mongo() {
        let mut cfg = Config {
            default_profile: Some("mongo".to_string()),
            ..Config::default()
        };
        cfg.profiles.insert(
            "mongo".to_string(),
            Profile {
                source: SourceConfig {
                    uri: Some("mongodb://localhost:27017/?replicaSet=rs0".to_string()),
                    prefer_secondary: true,
                    ..SourceConfig::default()
                },
                destination: DestinationConfig {
                    r#type: Some("local".to_string()),
                    path: Some("/srv/store/mongo".to_string()),
                    ..DestinationConfig::default()
                },
                features: FeaturesConfig {
                    compression: CompressionConfig {
                        algorithm: "zstd".to_string(),
                        level: 6,
                    },
                    encryption: EncryptionConfig {
                        enabled: true,
                        algorithm: "age".to_string(),
                        recipient_file: Some("/srv/keys/age.pub".to_string()),
                    },
                    incremental: IncrementalConfig::default(),
                },
                ..Profile::default()
            },
        );
        roundtrip(&cfg);
    }

    #[test]
    fn roundtrip_s3_multi_dest() {
        let mut cfg = Config::default();
        cfg.profiles.insert(
            "prod".to_string(),
            Profile {
                source: SourceConfig {
                    uri_env: Some("MONGO_URI".to_string()),
                    ..SourceConfig::default()
                },
                destinations: vec![
                    DestinationConfig {
                        name: Some("primary".to_string()),
                        r#type: Some("s3".to_string()),
                        s3: Some(S3Config {
                            bucket: Some("db-backups".to_string()),
                            prefix: Some("mongo/prod".to_string()),
                            region: Some("ap-northeast-2".to_string()),
                            credentials_env: Some("S3_CREDS".to_string()),
                            endpoint: None,
                        }),
                        ..DestinationConfig::default()
                    },
                    DestinationConfig {
                        name: Some("offsite".to_string()),
                        r#type: Some("local".to_string()),
                        path: Some("/mnt/offsite".to_string()),
                        ..DestinationConfig::default()
                    },
                ],
                ..Profile::default()
            },
        );
        roundtrip(&cfg);
    }

    #[test]
    fn roundtrip_pg_pg_logical() {
        let mut cfg = Config {
            default_profile: Some("pg".to_string()),
            ..Config::default()
        };
        cfg.profiles.insert(
            "pg".to_string(),
            Profile {
                source: SourceConfig {
                    uri: Some("postgres://xbackup@localhost:5432/db".to_string()),
                    ..SourceConfig::default()
                },
                destination: DestinationConfig {
                    r#type: Some("local".to_string()),
                    path: Some("/srv/store/pg".to_string()),
                    ..DestinationConfig::default()
                },
                mode: ModeConfig {
                    engine: "native".to_string(),
                    ..ModeConfig::default()
                },
                features: FeaturesConfig {
                    incremental: IncrementalConfig {
                        interval: "15m".to_string(),
                        on_gap: "promote_full".to_string(),
                        pg_logical: true,
                        mysql_binlog: false,
                    },
                    ..FeaturesConfig::default()
                },
                retention: RetentionConfig {
                    keep_full: Some(3),
                    keep_days: Some(14),
                    keep_last: None,
                    ..Default::default()
                },
                ..Profile::default()
            },
        );
        roundtrip(&cfg);
    }

    #[test]
    fn roundtrip_endpoint_only() {
        // destination이 없는 endpoint 전용 프로파일 — dest를 내보내면 안 된다.
        let mut cfg = Config::default();
        cfg.profiles.insert(
            "target".to_string(),
            Profile {
                source: SourceConfig {
                    uri: Some("mongodb://localhost:27117/db".to_string()),
                    ..SourceConfig::default()
                },
                features: FeaturesConfig {
                    encryption: EncryptionConfig {
                        enabled: false,
                        ..EncryptionConfig::default()
                    },
                    ..FeaturesConfig::default()
                },
                ..Profile::default()
            },
        );
        let parsed = roundtrip(&cfg);
        assert!(
            parsed.profile("target").unwrap().is_endpoint_only(),
            "endpoint 전용이 유지되어야 함"
        );
        // 출력에 dest 키가 없어야 한다.
        let text = to_v2_string(&cfg).unwrap();
        assert!(!text.contains("dest"), "endpoint 전용엔 dest가 없어야 함:\n{text}");
    }

    #[test]
    fn roundtrip_combined_workspace() {
        // mongo + mongo-target + pg + pg-target, 공통 encrypt/compress는 각 프로파일에 풀어서.
        let mk_target = |uri: &str| Profile {
            source: SourceConfig {
                uri: Some(uri.to_string()),
                ..SourceConfig::default()
            },
            features: FeaturesConfig {
                encryption: EncryptionConfig {
                    enabled: false,
                    ..EncryptionConfig::default()
                },
                ..FeaturesConfig::default()
            },
            ..Profile::default()
        };
        let enc = EncryptionConfig {
            enabled: true,
            algorithm: "age".to_string(),
            recipient_file: Some("/ws/keys/age.pub".to_string()),
        };
        let comp = CompressionConfig {
            algorithm: "zstd".to_string(),
            level: 6,
        };
        let mut cfg = Config {
            default_profile: Some("mongo".to_string()),
            ..Config::default()
        };
        cfg.profiles.insert(
            "mongo".to_string(),
            Profile {
                source: SourceConfig {
                    uri: Some("mongodb://localhost:27017/?replicaSet=rs0".to_string()),
                    ..SourceConfig::default()
                },
                destination: DestinationConfig {
                    r#type: Some("local".to_string()),
                    path: Some("/ws/store/mongo".to_string()),
                    ..DestinationConfig::default()
                },
                features: FeaturesConfig {
                    compression: comp.clone(),
                    encryption: enc.clone(),
                    ..FeaturesConfig::default()
                },
                ..Profile::default()
            },
        );
        cfg.profiles.insert(
            "mongo-target".to_string(),
            mk_target("mongodb://localhost:27117/?replicaSet=rs0"),
        );
        cfg.profiles.insert(
            "pg".to_string(),
            Profile {
                source: SourceConfig {
                    uri: Some("postgres://xbackup@localhost:5432/db".to_string()),
                    ..SourceConfig::default()
                },
                destination: DestinationConfig {
                    r#type: Some("local".to_string()),
                    path: Some("/ws/store/pg".to_string()),
                    ..DestinationConfig::default()
                },
                features: FeaturesConfig {
                    compression: comp.clone(),
                    encryption: enc.clone(),
                    incremental: IncrementalConfig {
                        pg_logical: true,
                        ..IncrementalConfig::default()
                    },
                },
                ..Profile::default()
            },
        );
        cfg.profiles.insert(
            "pg-target".to_string(),
            mk_target("postgres://xbackup@localhost:5433/db_restore"),
        );
        roundtrip(&cfg);
    }

    /// v2 flat `hook_*` 키가 hooks.* nested로 정규화되어 파싱된다(PRD-04). 미지의 키로
    /// 거부되지 않아야 한다.
    #[test]
    fn v2_flat_hooks_map_to_nested() {
        let toml = "[profile.p]\nuri = \"mongodb://h/db\"\n\
                    hook_pre_backup = \"quiesce.sh\"\nhook_post_backup = \"notify.sh\"\n\
                    hook_on_error = \"pager.sh\"\nhook_timeout_secs = 15\n";
        let cfg = crate::config::file::Config::from_toml_str(toml).unwrap();
        let p = cfg.profile("p").unwrap();
        assert_eq!(p.hooks.pre_backup.as_deref(), Some("quiesce.sh"));
        assert_eq!(p.hooks.post_backup.as_deref(), Some("notify.sh"));
        assert_eq!(p.hooks.on_error.as_deref(), Some("pager.sh"));
        assert_eq!(p.hooks.hook_timeout_secs, Some(15));
    }
}
