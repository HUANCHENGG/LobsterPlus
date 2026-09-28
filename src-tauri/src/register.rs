//! CC-Switch provider 注册（rusqlite 移植 youdaoAPI/add_provider.py）。
//!
//! 把 "LobsterPlus(本地)" 写进 %USERPROFILE%\.cc-switch\cc-switch.db：
//!   - apiFormat = "anthropic"（native：LobsterPlus 代理直接说 Anthropic 协议）
//!   - ANTHROPIC_BASE_URL = http://127.0.0.1:<port>（固定端口，永不随轮换漂移）
//!   - ANTHROPIC_AUTH_TOKEN = 代理 api_key（config.json 里配；空则用占位符）
//! 非破坏性：写前备份 db；写后还原写入前的当前 provider（--switch 才切换）。

use crate::proxy;
use crate::store::Paths;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

const PROVIDER_NAME: &str = "LobsterPlus(本地)";
/// 占位 token：代理未配置 api_key 时（不校验鉴权）用这个。
const PLACEHOLDER_TOKEN: &str = "PROXY_MANAGED";

fn cc_switch_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(not(windows))]
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cc-switch")
}

fn cc_switch_db() -> PathBuf {
    cc_switch_dir().join("cc-switch.db")
}

fn cc_switch_settings() -> PathBuf {
    cc_switch_dir().join("settings.json")
}

fn load_json_file(p: &std::path::Path) -> Value {
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

fn save_json_file(p: &std::path::Path, v: &Value) -> Result<(), String> {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    let body = serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    fs::write(p, body).map_err(|e| format!("写 {e} 失败：{}", p.display()))
}

fn get_current_provider_id() -> Option<String> {
    let v = load_json_file(&cc_switch_settings());
    v.get("currentProviderClaude").and_then(|x| x.as_str()).map(String::from)
}

fn set_current_provider_id(pid: &str) -> Result<(), String> {
    let mut v = load_json_file(&cc_switch_settings());
    if !v.is_object() {
        v = json!({});
    }
    v.as_object_mut().unwrap().insert("currentProviderClaude".into(), json!(pid));
    save_json_file(&cc_switch_settings(), &v)
}

/// 注册结果摘要。
pub fn register(paths: &Paths, make_current: bool) -> Result<Value, String> {
    // 代理得在运行（要确定端口与健康）
    let st = proxy::health(paths)?;
    if !st.running {
        return Err(crate::i18n::tr("err.register.port_unavailable"));
    }
    let port = st.port;
    let base_url = format!("http://127.0.0.1:{port}");
    let token = proxy::api_key(paths).unwrap_or_else(|| PLACEHOLDER_TOKEN.to_string());
    let model = default_upstream_model().unwrap_or_else(|| "deepseek-flash".into());

    let db = cc_switch_db();
    if !db.is_file() {
        return Err(crate::i18n::trf("err.register.no_db", &[("path", &db.display().to_string())]));
    }

    // 记住写入前的当前 provider（写后还原，避免破坏现有使用）
    let prev_current = get_current_provider_id();

    // 备份
    let backup_dir = cc_switch_dir().join("backups");
    fs::create_dir_all(&backup_dir).map_err(|e| format!("创建备份目录失败：{e}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let backup = backup_dir.join(format!("cc-switch_pre_lobsterplus_{stamp}.db"));
    fs::copy(&db, &backup).map_err(|e| format!("备份失败：{e}"))?;

    let env = build_env(&base_url, &token, &model);
    let settings_config = json!({ "env": env });
    let meta = json!({
        "commonConfigEnabled": true,
        "endpointAutoSelect": true,
        "apiFormat": "anthropic",
        "isFullUrl": true,
    });
    let now_ms = chrono::Utc::now().timestamp_millis();

    let pid = {
        let c = Connection::open(&db).map_err(|e| format!("打开 cc-switch.db 失败：{e}"))?;
        let existing: Option<String> = c
            .query_row(
                "SELECT id FROM providers WHERE app_type='claude' AND name=?1",
                [PROVIDER_NAME],
                |r| r.get(0),
            )
            .ok();
        let pid = match existing {
            Some(pid) => {
                c.execute(
                    "UPDATE providers SET settings_config=?1, meta=?2, notes=?3, created_at=?4 WHERE id=?5",
                    rusqlite::params![
                        serde_json::to_string(&settings_config).unwrap(),
                        serde_json::to_string(&meta).unwrap(),
                        model,
                        now_ms,
                        pid
                    ],
                )
                .map_err(|e| format!("更新 provider 失败：{e}"))?;
                c.execute("DELETE FROM provider_endpoints WHERE provider_id=?1", [&pid])
                    .map_err(|e| format!("更新端点失败：{e}"))?;
                pid
            }
            None => {
                let pid = uuid::Uuid::new_v4().to_string();
                c.execute(
                    "INSERT INTO providers (id,app_type,name,settings_config,website_url,category,created_at,sort_index,notes,icon,icon_color,meta,is_current,in_failover_queue,cost_multiplier,limit_daily_usd,limit_monthly_usd,provider_type)
                     VALUES (?1,'claude',?2,?3,NULL,NULL,?4,NULL,?5,NULL,NULL,?6,0,0,'1.0',NULL,NULL,NULL)",
                    rusqlite::params![
                        pid,
                        PROVIDER_NAME,
                        serde_json::to_string(&settings_config).unwrap(),
                        now_ms,
                        model,
                        serde_json::to_string(&meta).unwrap(),
                    ],
                )
                .map_err(|e| format!("插入 provider 失败：{e}"))?;
                pid
            }
        };
        c.execute(
            "INSERT INTO provider_endpoints (provider_id,app_type,url,added_at) VALUES (?1,'claude',?2,?3)",
            rusqlite::params![pid, base_url, now_ms],
        )
        .map_err(|e| format!("插入端点失败：{e}"))?;
        pid
    };

    // 切换策略：--switch 设为当前；否则还原写入前的当前 provider
    let mut switched = false;
    if make_current {
        set_db_current(&pid)?;
        let _ = set_current_provider_id(&pid);
        switched = true;
    } else if let Some(prev) = &prev_current {
        set_db_current(prev)?;
        let _ = set_current_provider_id(prev);
    }

    Ok(json!({
        "provider": PROVIDER_NAME,
        "id": pid,
        "base_url": base_url,
        "model": model,
        "backup": backup.to_string_lossy(),
        "switched": switched,
        "restored_previous": !make_current && prev_current.is_some(),
    }))
}

fn set_db_current(pid: &str) -> Result<(), String> {
    let c = Connection::open(cc_switch_db()).map_err(|e| format!("打开 cc-switch.db 失败：{e}"))?;
    c.execute("UPDATE providers SET is_current=0 WHERE app_type='claude'", [])
        .map_err(|e| format!("清除当前标记失败：{e}"))?;
    c.execute(
        "UPDATE providers SET is_current=1 WHERE id=?1 AND app_type='claude'",
        [pid],
    )
    .map_err(|e| format!("设置当前标记失败：{e}"))?;
    Ok(())
}

fn build_env(base_url: &str, token: &str, model: &str) -> Value {
    json!({
        "ANTHROPIC_AUTH_TOKEN": token,
        "ANTHROPIC_BASE_URL": base_url,
        "ANTHROPIC_MODEL": model,
        "ANTHROPIC_DEFAULT_OPUS_MODEL": model,
        "ANTHROPIC_DEFAULT_SONNET_MODEL": model,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": model,
        "ANTHROPIC_DEFAULT_FABLE_MODEL": model,
        "CLAUDE_CODE_SUBAGENT_MODEL": model,
    })
}

/// 从代理 /health 的模型清单挑默认模型。
/// 优先真实上游 ID（deepseek-flash 系），避免把 claude-* 映射名写进 CC-Switch
/// （CC-Switch 里显示映射名会让用户误以为直接可用）。
fn default_upstream_model() -> Option<String> {
    let port = proxy::listen_port(&crate::store::Paths::detect());
    let url = format!("http://127.0.0.1:{port}/health");
    let resp = ureq::get(&url).timeout(std::time::Duration::from_millis(1500)).call().ok()?;
    let body = resp.into_string().ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let models: Vec<String> = v
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str()).map(String::from).collect())
        .unwrap_or_default();
    models
        .iter()
        .find(|m| m.as_str() == "deepseek-flash")
        .or_else(|| models.iter().find(|m| !m.starts_with("claude-")))
        .or_else(|| models.first())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_shape() {
        let env = build_env("http://127.0.0.1:19260", "k", "deepseek-flash");
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:19260");
        assert_eq!(env["ANTHROPIC_MODEL"], "deepseek-flash");
    }
}
