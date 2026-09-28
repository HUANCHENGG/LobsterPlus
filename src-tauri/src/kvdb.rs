//! lobsterai.sqlite kv 表读写（只读为主，写仅限「客户端已退出 + token 续期回写」场景）。
//!
//! kv 表结构：`(key TEXT PRIMARY KEY, value TEXT)`，value 是 JSON 字符串。
//! 关键 key：auth_tokens（accessToken/refreshToken）、auth_user（id/yid/nickname）、
//! app_config、keyfrom.attribution.v1、installation_uuid、client_sidebar_banner.*。
//!
//! lobsterai.sqlite 有活跃的 WAL（客户端运行时 4MB+），所以：
//! - 读走 rusqlite 只读连接（不触发 checkpoint）；
//! - 写必须在客户端进程已退出后进行（wal 模式下写会自动恢复 wal 内容）；
//! - live 库是切换引擎 manifest 的一部分，整体随快照交换。

use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub const KEY_AUTH_TOKENS: &str = "auth_tokens";
pub const KEY_AUTH_USER: &str = "auth_user";
pub const KEY_APP_CONFIG: &str = "app_config";
pub const KEY_KEYFROM: &str = "keyfrom.attribution.v1";
pub const KEY_INSTALL_UUID: &str = "installation_uuid";
pub const KEY_SIDEBAR_BANNER_V2: &str = "client_sidebar_banner.schedule.desktop_sidebar.v2";
pub const KEY_SIDEBAR_BANNER_V1: &str = "client_sidebar_banner.schedule.desktop_sidebar.v1";

fn conn_read(db: &Path) -> Result<Connection, String> {
    if !db.is_file() {
        return Err(format!("数据库不存在：{}", db.display()));
    }
    let c = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开数据库失败 {}: {e}", db.display()))?;
    c.busy_timeout(Duration::from_secs(5)).ok();
    Ok(c)
}

/// 读 kv 表某 key 的原始字符串值（不存在返回 None）。
pub fn read_kv_raw(db: &Path, key: &str) -> Result<Option<String>, String> {
    let c = conn_read(db)?;
    read_kv_raw_conn(&c, key)
}

fn read_kv_raw_conn(c: &Connection, key: &str) -> Result<Option<String>, String> {
    let mut stmt = c
        .prepare("SELECT value FROM kv WHERE key = ?1")
        .map_err(|e| format!("查询失败：{e}"))?;
    let mut rows = stmt.query([key]).map_err(|e| format!("查询失败：{e}"))?;
    match rows.next().map_err(|e| format!("查询失败：{e}"))? {
        Some(row) => Ok(Some(row.get::<_, String>(0).map_err(|e| format!("读取失败：{e}"))?)),
        None => Ok(None),
    }
}

/// 读 kv 表某 key 并解析为 JSON 值（不存在/非 JSON 返回 None）。
pub fn read_kv_json(db: &Path, key: &str) -> Result<Option<Value>, String> {
    let raw = read_kv_raw(db, key)?;
    Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
}

/// 读 auth_tokens。返回 (accessToken, refreshToken)，未登录返回 None。
pub fn read_auth_tokens(db: &Path) -> Result<Option<(String, String)>, String> {
    let v = read_kv_json(db, KEY_AUTH_TOKENS)?;
    let Some(v) = v else { return Ok(None) };
    let access = v.get("accessToken").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let refresh = v.get("refreshToken").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if access.is_empty() {
        return Ok(None);
    }
    Ok(Some((access, refresh)))
}

/// 读 auth_user 的用户 id（登录身份识别主键）。id 可能是字符串或数字。
pub fn read_user_id(db: &Path) -> Result<Option<String>, String> {
    let v = read_kv_json(db, KEY_AUTH_USER)?;
    Ok(v.and_then(|u| json_value_to_id(u.get("id"))))
}

/// 读 auth_user 的昵称（账号卡片显示名）。
pub fn read_nickname(db: &Path) -> Result<Option<String>, String> {
    let v = read_kv_json(db, KEY_AUTH_USER)?;
    Ok(v.and_then(|u| {
        u.get("nickname")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    }))
}

/// id 字段兼容：字符串直接用，数字转字符串（实测 auth_user.id 可能为数字型）。
fn json_value_to_id(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// 读客户端记录的当前版本号（签到接口 clientVersion 参数）。
pub fn read_client_version(db: &Path, fallback: &str) -> Result<String, String> {
    for key in [KEY_SIDEBAR_BANNER_V2, KEY_SIDEBAR_BANNER_V1] {
        if let Some(v) = read_kv_json(db, key)? {
            if let Some(cv) = v.get("clientVersion").and_then(|x| x.as_str()) {
                if !cv.is_empty() {
                    return Ok(cv.to_string());
                }
            }
        }
    }
    if let Ok(v) = std::env::var("LOBSTERAI_CLIENT_VERSION") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    Ok(fallback.to_string())
}

/// 解析服务端 base url（app_config.app.testMode → 测试环境）。
pub fn read_server_base(db: &Path, prod: &str, test: &str) -> Result<String, String> {
    let cfg = read_kv_json(db, KEY_APP_CONFIG)?;
    let test_mode = cfg
        .as_ref()
        .and_then(|c| c.get("app"))
        .and_then(|a| a.get("testMode"))
        .and_then(|t| t.as_bool())
        .unwrap_or(false);
    Ok(if test_mode { test.to_string() } else { prod.to_string() })
}

/// 签到 token 续期请求的辅助字段（firstKeyfrom/latestKeyfrom/uuid/userId/version）。
pub fn read_refresh_payload(db: &Path, client_version: &str) -> RefreshPayload {
    let mut p = RefreshPayload::default();
    if let Ok(Some(v)) = read_kv_json(db, KEY_KEYFROM) {
        p.first_keyfrom = v.get("firstKeyfrom").and_then(|x| x.as_str()).map(String::from);
        p.latest_keyfrom = v.get("latestKeyfrom").and_then(|x| x.as_str()).map(String::from);
    }
    p.uuid = read_kv_value_as_string(db, KEY_INSTALL_UUID);
    if let Ok(Some(v)) = read_kv_json(db, KEY_AUTH_USER) {
        p.user_id = v.get("id").and_then(|x| x.as_str()).map(String::from);
    }
    p.version = client_version.to_string();
    p
}

/// 兼容读：kv 值可能是 JSON 字符串（带引号）也可能是裸字符串。
fn read_kv_value_as_string(db: &Path, key: &str) -> Option<String> {
    let raw = read_kv_raw(db, key).ok().flatten()?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => v.as_str().map(String::from),
        Err(_) => Some(raw), // 裸字符串（如 installation_uuid 可能存裸值）
    }
}

#[derive(Debug, Default, Clone)]
pub struct RefreshPayload {
    pub first_keyfrom: Option<String>,
    pub latest_keyfrom: Option<String>,
    pub uuid: Option<String>,
    pub user_id: Option<String>,
    pub version: String,
}

impl RefreshPayload {
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        if let Some(v) = &self.first_keyfrom {
            m.insert("firstKeyfrom".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.latest_keyfrom {
            m.insert("latestKeyfrom".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.uuid {
            m.insert("uuid".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.user_id {
            m.insert("userId".into(), Value::String(v.clone()));
        }
        if !self.version.is_empty() {
            m.insert("version".into(), Value::String(self.version.clone()));
        }
        Value::Object(m)
    }
}

/// 写回 kv 表（upsert）。红线：仅在客户端进程已退出后调用
/// （切换流程内或续期回写，且调用方负责校验进程状态）。
pub fn write_kv_json(db: &Path, key: &str, value: &Value) -> Result<(), String> {
    let c = Connection::open(db).map_err(|e| format!("打开数据库失败：{e}"))?;
    c.busy_timeout(Duration::from_secs(10)).ok();
    write_kv_json_conn(&c, key, value)
}

fn write_kv_json_conn(c: &Connection, key: &str, value: &Value) -> Result<(), String> {
    let payload = serde_json::to_string(value).map_err(|e| format!("序列化失败：{e}"))?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    // kv 表有 updated_at 列（youdaocheckin write_kv 同款 upsert）
    c.execute(
        "INSERT INTO kv(key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
        rusqlite::params![key, payload, now_ms],
    )
    .map_err(|e| format!("写入 kv 失败：{e}"))?;
    Ok(())
}

/// 对指定 sqlite 写入 auth_tokens（token 续期回写专用）。
pub fn write_auth_tokens(db: &Path, access: &str, refresh: &str) -> Result<(), String> {
    write_kv_json(
        db,
        KEY_AUTH_TOKENS,
        &serde_json::json!({ "accessToken": access, "refreshToken": refresh }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lp-kvdb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("lobsterai.sqlite");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
             INSERT INTO kv(key, value, updated_at) VALUES
               ('auth_tokens', '{\"accessToken\":\"at-1\",\"refreshToken\":\"rt-1\"}', 1),
               ('auth_user', '{\"id\":\"uid-9\",\"nickname\":\"测试虾\",\"yid\":\"y1\"}', 1),
               ('app_config', '{\"app\":{\"testMode\":false}}', 1),
               ('keyfrom.attribution.v1', '{\"firstKeyfrom\":\"k1\",\"latestKeyfrom\":\"k2\"}', 1),
               ('installation_uuid', 'inst-uuid', 1);",
        )
        .unwrap();
        db
    }

    #[test]
    fn reads_and_identity() {
        let db = make_db();
        let (at, rt) = read_auth_tokens(&db).unwrap().unwrap();
        assert_eq!((at.as_str(), rt.as_str()), ("at-1", "rt-1"));
        assert_eq!(read_user_id(&db).unwrap().as_deref(), Some("uid-9"));
        assert_eq!(read_nickname(&db).unwrap().as_deref(), Some("测试虾"));
        assert_eq!(
            read_server_base(&db, "https://p", "https://t").unwrap(),
            "https://p"
        );
        let p = read_refresh_payload(&db, "2026.9.4");
        assert_eq!(p.first_keyfrom.as_deref(), Some("k1"));
        assert_eq!(p.uuid.as_deref(), Some("inst-uuid"));
        assert_eq!(p.user_id.as_deref(), Some("uid-9"));
        assert_eq!(p.version, "2026.9.4");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn write_back_roundtrip() {
        let db = make_db();
        write_auth_tokens(&db, "at-2", "rt-2").unwrap();
        let (at, rt) = read_auth_tokens(&db).unwrap().unwrap();
        assert_eq!((at.as_str(), rt.as_str()), ("at-2", "rt-2"));
        // 其他 key 不受影响
        assert_eq!(read_user_id(&db).unwrap().as_deref(), Some("uid-9"));
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn missing_db_errors() {
        assert!(read_kv_raw(Path::new("Z:/nope/no.db"), "k").is_err());
    }
}
