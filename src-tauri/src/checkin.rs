//! 多账号每日签到引擎（移植 youdaocheckin/lobsterai_checkin.py）。
//!
//! 三步 HTTP 流程（服务端 https://lobsterai-server.youdao.com）：
//!   1. GET /api/client-activities/slot?placement=desktop_sidebar&clientVersion=…
//!      → activityCode + configRevision（code 51102 = 登录失效）
//!   2. GET /api/client-activities/{code}/context?configRevision=…
//!      → state.claimedToday 预检
//!   3. POST /api/client-activities/{code}/actions/check_in
//!      body {configRevision, idempotencyKey: uuid4, payload:{}}
//!      code 0 成功（creditsGranted/claimedDays/claimedCredits），51104 已签
//!
//! token 策略：签到只需要 accessToken，不依赖客户端运行。
//!   - 活跃账号：优先读 live sqlite（客户端可能刚续期过）；
//!     仅在 live 库不可读且 blob 临期时才自行续期写回（避免与客户端竞态）。
//!   - 非活跃账号：读账号库 auth_blob；JWT 临期（<60s）用 refreshToken 独立续期
//!     （POST /api/auth/refresh），成功后回写 blob + 快照 sqlite（客户端已退出时）。
//!
//! 调度：GUI 启动补签 + 每小时循环（lib.rs）；CLI checkin 子命令可挂计划任务。

use crate::store::{self, Account, AuthBlob, Paths};
use serde_json::{json, Value};
use std::sync::Mutex;

const PROD_BASE: &str = "https://lobsterai-server.youdao.com";
const TEST_BASE: &str = "https://lobsterai-server.inner.youdao.com";
const PLACEMENT: &str = "desktop_sidebar";
const CONTAINER_API_VERSION: i64 = 2;
const FALLBACK_CLIENT_VERSION: &str = "2026.9.4";
/// token 剩余寿命低于此值（秒）触发续期。
const REFRESH_LEAD_SECS: i64 = 60;

/// 单账号签到节流：每账号每小时最多一次真实 API 调用（防重试风暴）。
const PER_ACCOUNT_THROTTLE_MS: i64 = 60 * 60_000;
static LAST_ATTEMPT: Mutex<Option<(String, i64)>> = Mutex::new(None);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn local_date(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

// ---------- HTTP ----------

fn request(method: &str, url: &str, token: Option<&str>, body: Option<&Value>) -> Result<(u16, Value), String> {
    let req = match method {
        "GET" => ureq::get(url),
        _ => ureq::post(url),
    }
    .set("Accept", "application/json")
    .set("Cache-Control", "no-store")
    .timeout(std::time::Duration::from_secs(25));
    let req = if let Some(t) = token {
        req.set("Authorization", &format!("Bearer {t}"))
    } else {
        req
    };
    let resp = match body {
        Some(b) => {
            let s = serde_json::to_string(b).unwrap_or_default();
            req.set("Content-Type", "application/json").send_string(&s)
        }
        None => req.call(),
    };
    match resp {
        Ok(r) => {
            let code = r.status();
            let text = r.into_string().unwrap_or_default();
            let v = serde_json::from_str(&text).unwrap_or_else(|_| json!({"code": code, "message": text}));
            Ok((code, v))
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            let v = serde_json::from_str(&text)
                .unwrap_or_else(|_| json!({"code": code, "message": text}));
            Ok((code, v))
        }
        Err(e) => Err(format!("网络错误：{e}")),
    }
}

// ---------- token 管理 ----------

/// 解析账号可用 token。返回 (access_token, server_base)。
/// 活跃账号优先 live sqlite；blob 临期自动续期。
fn get_account_token(paths: &Paths, acc: &Account, is_active: bool) -> Result<(String, String), String> {
    let live_db = paths.live_db();
    let secret = paths.secret();

    // 活跃账号：live sqlite 的 token 最新鲜（客户端自己会续期）
    if is_active && live_db.is_file() {
        if let Ok(Some((access, refresh))) = crate::kvdb::read_auth_tokens(&live_db) {
            let fresh = store::jwt_exp_ms(&access)
                .map(|exp| (exp - now_ms()) / 1000 > REFRESH_LEAD_SECS)
                .unwrap_or(true);
            if fresh && !access.is_empty() {
                let base = resolve_server_base(&live_db);
                // 顺带把 live 新 token 回写 blob（客户端续期过的最新值）
                if let Some(blob) = acc.auth_blob.as_ref() {
                    if let Ok(mut b) = store::decode_auth_blob(blob, &secret) {
                        if b.access_token != access || b.refresh_token != refresh {
                            b.access_token = access.clone();
                            b.refresh_token = refresh;
                            if let Ok(enc) = store::encode_auth_blob(&b, &secret) {
                                if let Ok(mut a) = store::load_account(paths, &acc.id) {
                                    a.auth_blob = Some(enc);
                                    a.token_exp_ms = store::jwt_exp_ms(&access);
                                    let _ = store::save_account(paths, &a);
                                }
                            }
                        }
                    }
                }
                return Ok((access, base));
            }
        }
    }

    // blob 路径（非活跃账号 / live 不可读）
    let blob = acc
        .auth_blob
        .as_ref()
        .map(|enc| store::decode_auth_blob(enc, &secret))
        .transpose()
        .map_err(|e| e.to_string())?
        .ok_or("账号缺登录 blob（请重新收编）")?;

    let exp_ok = store::jwt_exp_ms(&blob.access_token)
        .map(|exp| (exp - now_ms()) / 1000 > REFRESH_LEAD_SECS)
        .unwrap_or(true);
    if exp_ok && !blob.access_token.is_empty() {
        let base = blob_server_base(&blob);
        return Ok((blob.access_token, base));
    }

    // 续期
    let new_access = refresh_tokens(paths, acc, &blob, is_active)?;
    let base = blob_server_base(&blob);
    Ok((new_access, base))
}

fn resolve_server_base(db: &std::path::Path) -> String {
    crate::kvdb::read_server_base(db, PROD_BASE, TEST_BASE).unwrap_or_else(|_| PROD_BASE.to_string())
}

fn blob_server_base(_blob: &AuthBlob) -> String {
    // blob 不含 testMode 信息；测试环境属于极少数场景，走生产
    PROD_BASE.to_string()
}

/// 用 refreshToken 续期。成功后回写账号 blob（+ 快照 sqlite，客户端退出时）。
fn refresh_tokens(paths: &Paths, acc: &Account, blob: &AuthBlob, is_active: bool) -> Result<String, String> {
    if blob.refresh_token.is_empty() {
        return Err("登录态缺 refreshToken，请重新登录客户端后收编".into());
    }
    let mut body = json!({ "refreshToken": blob.refresh_token });
    let obj = body.as_object_mut().unwrap();
    if let Some(v) = &blob.first_keyfrom {
        obj.insert("firstKeyfrom".into(), json!(v));
    }
    if let Some(v) = &blob.latest_keyfrom {
        obj.insert("latestKeyfrom".into(), json!(v));
    }
    if let Some(v) = &blob.uuid {
        obj.insert("uuid".into(), json!(v));
    }
    if let Some(v) = &blob.user_id {
        obj.insert("userId".into(), json!(v));
    }
    let cv = blob.version.clone().unwrap_or_else(|| FALLBACK_CLIENT_VERSION.to_string());
    obj.insert("version".into(), json!(cv));

    let base = blob_server_base(blob);
    let (_status, resp) = request("POST", &format!("{base}/api/auth/refresh"), None, Some(&body))?;
    let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    let data = resp.get("data").cloned().unwrap_or(Value::Null);
    let new_access = data.get("accessToken").and_then(|t| t.as_str()).unwrap_or("").to_string();
    if code != 0 || new_access.is_empty() {
        let msg = resp.get("message").and_then(|m| m.as_str()).unwrap_or("未知错误");
        return Err(format!("令牌续期失败（code={code}）：{msg}。请切回该账号在客户端重新登录"));
    }
    let new_refresh = data
        .get("refreshToken")
        .and_then(|t| t.as_str())
        .map(String::from)
        .unwrap_or_else(|| blob.refresh_token.clone());

    // 回写账号 blob
    let secret = paths.secret();
    let mut nb = blob.clone();
    nb.access_token = new_access.clone();
    nb.refresh_token = new_refresh.clone();
    let enc = store::encode_auth_blob(&nb, &secret)?;
    if let Ok(mut a) = store::load_account(paths, &acc.id) {
        a.auth_blob = Some(enc);
        a.auth_ref = {
            use sha2::Digest;
            let d = sha2::Sha256::digest(new_refresh.as_bytes());
            d.iter().map(|b| format!("{b:02x}")).collect()
        };
        a.token_exp_ms = store::jwt_exp_ms(&new_access);
        a.updated_at = store::now_ts();
        let _ = store::save_account(paths, &a);
    }
    // 活跃账号且客户端不在运行：回写快照 sqlite，客户端下次启动直接用新 token
    if is_active && !crate::guard::lobster_running(paths) {
        let snap_db = crate::manifest::snapshot_root(&paths.account_dir(&acc.id)).join("lobsterai.sqlite");
        if snap_db.is_file() {
            let _ = crate::kvdb::write_auth_tokens(&snap_db, &new_access, &new_refresh);
        }
    }
    Ok(new_access)
}

// ---------- 签到 ----------

/// 单账号签到。返回 (状态, 说明, 附加数据)。
/// 状态：ok=签到成功 / done=今天已签 / no_activity=无活动 / skip=节流跳过 / error。
pub fn checkin_one(paths: &Paths, acc: &Account, is_active: bool) -> (String, String, Value) {
    // 节流：同账号 1 小时内已尝试过则跳过（失败也计）
    {
        let mut last = LAST_ATTEMPT.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((id, t)) = last.as_ref() {
            if *id == acc.id && now_ms() - *t < PER_ACCOUNT_THROTTLE_MS {
                return ("skip".into(), "1 小时内已尝试过，跳过（每小时自动补签会重试）".into(), Value::Null);
            }
        }
        *last = Some((acc.id.clone(), now_ms()));
    }

    let (token, base) = match get_account_token(paths, acc, is_active) {
        Ok(v) => v,
        Err(e) => return ("error".into(), e, Value::Null),
    };
    if token.is_empty() {
        return ("error".into(), "登录态无 token（可能已登出，请重新收编）".into(), Value::Null);
    }

    let client_version = acc
        .auth_blob
        .as_ref()
        .and_then(|enc| store::decode_auth_blob(enc, &paths.secret()).ok())
        .and_then(|b| b.version.filter(|v| !v.is_empty()))
        .unwrap_or_else(|| FALLBACK_CLIENT_VERSION.to_string());
    let platform = if cfg!(windows) { "win32" } else if cfg!(target_os = "macos") { "darwin" } else { "linux" };

    // 1) 活动位
    let slot_url = format!(
        "{base}/api/client-activities/slot?placement={PLACEMENT}&clientVersion={client_version}&containerApiVersion={CONTAINER_API_VERSION}&platform={platform}"
    );
    let (_s, resp) = match request("GET", &slot_url, Some(&token), None) {
        Ok(v) => v,
        Err(e) => return ("error".into(), format!("活动位查询失败：{e}"), Value::Null),
    };
    let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code == 51102 {
        return ("error".into(), "登录已失效（51102），请切回该账号在客户端重新登录".into(), Value::Null);
    }
    if code != 0 {
        let msg = resp.get("message").and_then(|m| m.as_str()).unwrap_or("未知错误");
        return ("error".into(), format!("活动位查询失败（code={code}）：{msg}"), Value::Null);
    }
    let activity = resp
        .pointer("/data/activity")
        .cloned()
        .unwrap_or(Value::Null);
    let activity_code = activity.get("activityCode").and_then(|c| c.as_str()).unwrap_or("");
    let revision = activity.get("configRevision").and_then(|c| c.as_i64()).unwrap_or(0);
    if activity_code.is_empty() {
        return ("no_activity".into(), "当前没有可参与的签到活动".into(), Value::Null);
    }

    // 2) 活动上下文（claimedToday 预检）
    let ctx_url = format!("{base}/api/client-activities/{activity_code}/context?configRevision={revision}");
    let (_s, resp) = match request("GET", &ctx_url, Some(&token), None) {
        Ok(v) => v,
        Err(e) => return ("error".into(), format!("活动状态查询失败：{e}"), Value::Null),
    };
    let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = resp.get("message").and_then(|m| m.as_str()).unwrap_or("未知错误");
        return ("error".into(), format!("活动状态查询失败（code={code}）：{msg}"), Value::Null);
    }
    let state = resp.pointer("/data/state").cloned().unwrap_or(Value::Null);
    if state.get("claimedToday").and_then(|c| c.as_bool()).unwrap_or(false) {
        let days = state.get("claimedDays").cloned().unwrap_or(Value::Null);
        let credits = state.get("claimedCredits").cloned().unwrap_or(Value::Null);
        return (
            "done".into(),
            format!(
                "今日已签到（累计 {} 天，积分 {}）",
                days.as_i64().unwrap_or(0),
                credits.as_i64().unwrap_or(0)
            ),
            json!({ "claimedDays": days, "claimedCredits": credits }),
        );
    }

    // 3) 执行签到
    let idem = uuid::Uuid::new_v4().to_string();
    let body = json!({ "configRevision": revision, "idempotencyKey": idem, "payload": {} });
    let claim_url = format!("{base}/api/client-activities/{activity_code}/actions/check_in");
    let (_s, resp) = match request("POST", &claim_url, Some(&token), Some(&body)) {
        Ok(v) => v,
        Err(e) => return ("error".into(), format!("签到请求失败：{e}"), Value::Null),
    };
    let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code == 51104 {
        return ("done".into(), "今日已签到".into(), Value::Null);
    }
    if code != 0 {
        let msg = resp.get("message").and_then(|m| m.as_str()).unwrap_or("未知错误");
        return ("error".into(), format!("签到失败（code={code}）：{msg}"), Value::Null);
    }
    let granted = resp.pointer("/data/result/creditsGranted").and_then(|c| c.as_i64()).unwrap_or(0);
    let days = resp.pointer("/data/context/state/claimedDays").and_then(|c| c.as_i64()).unwrap_or(0);
    let credits = resp.pointer("/data/context/state/claimedCredits").and_then(|c| c.as_i64()).unwrap_or(0);
    (
        "ok".into(),
        format!("签到成功！+{granted} 积分（累计 {days} 天 / 共 {credits} 积分）"),
        json!({ "creditsGranted": granted, "claimedDays": days, "claimedCredits": credits }),
    )
}

fn record_checkin(paths: &Paths, acc: &Account, status: &str, msg: &str, data: Value) {
    let entry = json!({
        "date": local_date(now_ms()),
        "status": status,
        "msg": msg,
        "data": data,
        "at_ms": now_ms(),
    });
    if let Ok(mut a) = store::load_account(paths, &acc.id) {
        a.last_checkin = Some(entry);
        a.updated_at = store::now_ts();
        let _ = store::save_account(paths, &a);
    }
}

/// 全库签到（或指定账号）。结果写回 last_checkin。
pub fn checkin_all(paths: &Paths, only_id: Option<&str>) -> Result<Value, String> {
    let accounts = store::list_accounts(paths)?;
    let live_login = store::read_live_login(paths).ok().flatten();
    let mut results = vec![];
    for acc in accounts.iter() {
        if let Some(id) = only_id {
            if acc.id != id {
                continue;
            }
        }
        let is_active = live_login.as_ref().map(|l| !acc.user_id.is_empty() && l.user_id == acc.user_id).unwrap_or(false);
        let (status, msg, data) = checkin_one(paths, acc, is_active);
        if status == "ok" || status == "done" {
            record_checkin(paths, acc, &status, &msg, data.clone());
        }
        results.push(json!({
            "id": acc.id,
            "name": acc.name,
            "nickname": acc.nickname,
            "status": status,
            "msg": msg,
        }));
    }
    if let Some(id) = only_id {
        if !results.iter().any(|r| r.get("id").and_then(|v| v.as_str()) == Some(id)) {
            return Err(format!("账号不存在：{id}"));
        }
    }
    Ok(json!({
        "date": local_date(now_ms()),
        "results": results,
        "ok_count": results.iter().filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("ok")).count(),
        "done_count": results.iter().filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("done")).count(),
        "error_count": results.iter().filter(|r| r.get("status").and_then(|v| v.as_str()) == Some("error")).count(),
    }))
}

/// 是否需要补签：账号 last_checkin 的 date 不是今天（ok/done 状态）。
pub fn needs_checkin(acc: &Account) -> bool {
    match acc.last_checkin.as_ref() {
        Some(c) => {
            let today_ok = c.get("date").and_then(|d| d.as_str()) == Some(local_date(now_ms()).as_str())
                && matches!(
                    c.get("status").and_then(|s| s.as_str()),
                    Some("ok") | Some("done") | Some("no_activity")
                );
            !today_ok
        }
        None => true,
    }
}

/// 启动/定时补签：对所有「今天未签」的账号执行签到。
pub fn catchup(paths: &Paths) -> Result<Value, String> {
    let accounts = store::list_accounts(paths)?;
    let todo: Vec<&Account> = accounts.iter().filter(|a| needs_checkin(a)).collect();
    if todo.is_empty() {
        return Ok(json!({ "date": local_date(now_ms()), "results": [], "skipped": true }));
    }
    let live_login = store::read_live_login(paths).ok().flatten();
    let mut results = vec![];
    for acc in todo {
        let is_active = live_login.as_ref().map(|l| !acc.user_id.is_empty() && l.user_id == acc.user_id).unwrap_or(false);
        let (status, msg, data) = checkin_one(paths, acc, is_active);
        if status == "ok" || status == "done" || status == "no_activity" {
            record_checkin(paths, acc, &status, &msg, data.clone());
        }
        results.push(json!({ "id": acc.id, "name": acc.name, "status": status, "msg": msg }));
    }
    Ok(json!({ "date": local_date(now_ms()), "results": results, "skipped": false }))
}

/// CLI / Tauri 命令入口。
pub fn cli_run(paths: &Paths, id: Option<String>) -> Result<Value, String> {
    checkin_all(paths, id.as_deref())
}
