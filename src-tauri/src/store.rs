//! LobsterAI 多账号切换核心。
//!
//! 与家族前作（RaccoonPlus 整文件换 auth.json、TraePlus storage.json 键级交换）
//! 不同：LobsterAI 的对话库（cowork_*）没有账号字段，登录态/对话/Cookies/
//! openclaw 状态全部混在一个数据目录里。因此本作采用「manifest 清单式目录交换」：
//! 切换 = 杀客户端 → live manifest 路径移入源账号快照（sync_back 保鲜）
//!        → 目标账号快照 manifest 路径拷回 live → 读回校验 → 可选重启。
//!
//! 红线：
//! - live lobsterai.sqlite 只读（读登录态/身份），绝不直接写；
//! - 切换必过读回校验（live kv.auth_user.id == 目标账号 id），失败自动回滚；
//! - 首次切换前把 live manifest 子集全量备份一次。

use crate::guard;
use crate::i18n::{tr, trf};
use crate::kvdb;
use crate::lockfile::ProxyLock;
use crate::manifest;
use crate::zcrypto;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_STALE: Duration = Duration::from_secs(180);
/// 切换涉及上百 MB 目录拷贝，锁陈旧阈值放宽。

pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn detect() -> Paths {
        let home = pick_home(
            std::env::var("LOBSTER_PLUS_HOME").ok().map(PathBuf::from),
            std::env::var("USERPROFILE").ok().map(PathBuf::from),
            std::env::var("HOME").ok().map(PathBuf::from),
        );
        Paths { home }
    }

    pub fn store_dir(&self) -> PathBuf { self.home.join(".lobster-plus") }
    pub fn accounts_dir(&self) -> PathBuf { self.store_dir().join("accounts") }
    pub fn backups_dir(&self) -> PathBuf { self.store_dir().join("backups") }
    pub fn settings_file(&self) -> PathBuf { self.store_dir().join("settings.json") }
    pub fn proxy_dir(&self) -> PathBuf { self.store_dir().join("proxy") }
    pub fn proxy_script(&self) -> PathBuf { self.proxy_dir().join("lobster_proxy.py") }
    pub fn proxy_config(&self) -> PathBuf { self.proxy_dir().join("config.json") }
    pub fn proxy_log(&self) -> PathBuf { self.proxy_dir().join("proxy.log") }
    /// 某账号的专属目录（account.json + snapshot/）。
    pub fn account_dir(&self, id: &str) -> PathBuf { self.accounts_dir().join(id) }
    pub fn account_file(&self, id: &str) -> PathBuf { self.account_dir(id).join("account.json") }

    /// LobsterAI live 数据目录（%APPDATA%\LobsterAI）。
    pub fn live_dir(&self) -> PathBuf {
        #[cfg(windows)]
        let roaming = std::env::var("APPDATA")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join("AppData").join("Roaming"));
        #[cfg(not(windows))]
        let roaming = self.home.join(".config");
        roaming.join("LobsterAI")
    }

    pub fn live_db(&self) -> PathBuf { self.live_dir().join("lobsterai.sqlite") }

    pub fn ensure_dirs(&self) -> Result<(), String> {
        fs::create_dir_all(self.accounts_dir())
            .map_err(|e| trf("err.store.mk_accounts_dir", &[("e", &e.to_string())]))?;
        Ok(())
    }

    pub fn secret(&self) -> String {
        zcrypto::default_secret(&self.home)
    }
}

pub fn pick_home(custom: Option<PathBuf>, userprofile: Option<PathBuf>, home_env: Option<PathBuf>) -> PathBuf {
    custom.or(userprofile).or(home_env).unwrap_or_else(|| PathBuf::from("."))
}

pub fn in_sandbox() -> bool {
    std::env::var("LOBSTER_PLUS_HOME").is_ok()
}

// ---------- 账号模型 ----------

/// 账号 = 元数据（account.json，明文）+ 目录快照（snapshot/，manifest 清单）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    /// 用户 id（kv.auth_user.id，身份识别主键）
    #[serde(default)]
    pub user_id: String,
    /// 昵称（kv.auth_user.nickname）
    #[serde(default)]
    pub nickname: Option<String>,
    /// yid（有道账号 id）
    #[serde(default)]
    pub yid: Option<String>,
    /// 登录态 blob（enc:v1 加密的 {accessToken, refreshToken, keyfrom, uuid, userId, version}）
    /// 签到引擎独立续期用，与快照目录里的 sqlite 冗余双保险。
    #[serde(default)]
    pub auth_blob: Option<String>,
    /// refreshToken 的 SHA-256（识别同一登录的兜底依据）
    #[serde(default)]
    pub auth_ref: String,
    /// access token JWT exp（毫秒，健康度依据）
    #[serde(default)]
    pub token_exp_ms: Option<i64>,
    /// 最近一次签到结果
    #[serde(default)]
    pub last_checkin: Option<Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    pub lobster_path: Option<String>,
    pub launch_after_switch: Option<bool>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub proxy_autostart: Option<bool>,
}

impl Settings {
    pub fn launch_after_switch(&self) -> bool { self.launch_after_switch.unwrap_or(true) }
    pub fn proxy_autostart(&self) -> bool { self.proxy_autostart.unwrap_or(false) }
}

#[derive(Serialize, Clone, Debug)]
pub struct AccountSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    pub is_active: bool,
    pub user_id: String,
    pub nickname: Option<String>,
    pub snapshot_ok: bool,
    pub snapshot_size: u64,
    pub token_days_left: Option<f64>,
    pub token_exp_text: Option<String>,
    pub health: &'static str,
    pub last_checkin: Option<Value>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AppState {
    pub lobster_running: bool,
    pub lobster_path: String,
    pub lobster_path_ok: bool,
    pub live_dir: String,
    pub live_db_exists: bool,
    pub live_logged_in: bool,
    pub live_name: Option<String>,
    pub active_account_id: Option<String>,
    pub accounts: Vec<AccountSummary>,
    pub store_dir: String,
    pub launch_after_switch: bool,
    pub proxy: Value,
    pub language: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct SwitchResult {
    pub switched: bool,
    pub already_active: bool,
    pub name: String,
    pub preserved_as: Option<String>,
    pub killed: bool,
    pub launched: bool,
    pub warnings: Vec<String>,
    /// 交换的 manifest 路径数（保鲜/恢复）
    pub preserved_count: usize,
    pub restored_count: usize,
}

pub fn now_ts() -> String {
    Local::now().format("%Y-%m-%d %H:%M").to_string()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn atomic_write(path: &Path, data: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
    fs::write(&tmp, data)
        .map_err(|e| trf("err.write_file", &[("path", &path.display().to_string()), ("e", &e.to_string())]))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(trf("err.rename_fail", &[("path", &path.display().to_string()), ("e", &e.to_string())]));
    }
    Ok(())
}

// ---------- 身份与健康度 ----------

/// 登录 blob 明文结构（auth_blob 解密后）。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AuthBlob {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub first_keyfrom: Option<String>,
    #[serde(default)]
    pub latest_keyfrom: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

pub fn encode_auth_blob(blob: &AuthBlob, secret: &str) -> Result<String, String> {
    zcrypto::encrypt_with_secret(&serde_json::to_string(blob).unwrap_or_default(), secret)
}

pub fn decode_auth_blob(enc: &str, secret: &str) -> Result<AuthBlob, String> {
    let plain = if zcrypto::is_encrypted(enc) {
        zcrypto::decrypt_with_secret(enc, secret)?
    } else {
        enc.to_string()
    };
    serde_json::from_str(&plain).map_err(|e| trf("err.store.corrupt", &[("e", &e.to_string())]))
}

/// 解析 JWT exp（毫秒）。失败返回 None。
pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(|e| e.as_i64()).map(|s| s * 1000)
}

fn ref_of_token(token: &str) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(token.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn days_left(exp_ms: Option<i64>) -> Option<f64> {
    let exp = exp_ms?;
    Some((exp - now_ms()) as f64 / 86_400_000.0)
}

pub fn health_of(exp_ms: Option<i64>) -> &'static str {
    match days_left(exp_ms) {
        None => "unknown",
        Some(d) if d <= 0.0 => "expired",
        Some(d) if d <= 3.0 => "warn",
        Some(_) => "ok",
    }
}

pub fn fmt_time_ms(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

// ---------- 账号库 CRUD ----------

pub fn list_accounts(paths: &Paths) -> Result<Vec<Account>, String> {
    let dir = paths.accounts_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    // 账号库结构：accounts\<uuid>\account.json（两层，uuid 目录还装 snapshot/）
    for entry in fs::read_dir(&dir).map_err(|e| trf("err.store.list_fail", &[("e", &e.to_string())]))? {
        let sub = entry.map_err(|e| trf("err.store.list_fail", &[("e", &e.to_string())]))?.path();
        let path = sub.join("account.json");
        if !path.is_file() {
            continue;
        }
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(a) = serde_json::from_str::<Account>(&raw) {
                out.push(a);
            }
        }
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.name.cmp(&b.name)));
    Ok(out)
}

pub fn save_account(paths: &Paths, acc: &Account) -> Result<(), String> {
    paths.ensure_dirs()?;
    fs::create_dir_all(paths.account_dir(&acc.id))
        .map_err(|e| trf("err.mkdir", &[("e", &e.to_string())]))?;
    let path = paths.account_file(&acc.id);
    let body = serde_json::to_string_pretty(acc).unwrap_or_default() + "\n";
    atomic_write(&path, &body)
}

pub fn load_account(paths: &Paths, id: &str) -> Result<Account, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(tr("err.store.bad_id"));
    }
    let path = paths.account_file(id);
    let raw = fs::read_to_string(&path)
        .map_err(|_| trf("err.store.no_account_id", &[("id", id)]))?;
    serde_json::from_str::<Account>(&raw).map_err(|e| trf("err.store.corrupt", &[("e", &e.to_string())]))
}

fn name_exists(accounts: &[Account], name: &str) -> bool {
    accounts.iter().any(|a| a.name.eq_ignore_ascii_case(name))
}

pub fn unique_name(accounts: &[Account], base: &str) -> String {
    if !name_exists(accounts, base) {
        return base.to_string();
    }
    for n in 2..1000 {
        let cand = format!("{base} {n}");
        if !name_exists(accounts, &cand) {
            return cand;
        }
    }
    format!("{base} {}", Uuid::new_v4().simple())
}

// ---------- live 登录态 ----------

pub struct LiveLogin {
    pub user_id: String,
    pub nickname: Option<String>,
    pub yid: Option<String>,
    pub access: String,
    pub refresh: String,
}

/// 读 live 登录态（只读 sqlite）。未登录返回 None。
pub fn read_live_login(paths: &Paths) -> Result<Option<LiveLogin>, String> {
    let db = paths.live_db();
    if !db.is_file() {
        return Ok(None);
    }
    let Some((access, refresh)) = kvdb::read_auth_tokens(&db)? else {
        return Ok(None);
    };
    let user_v = kvdb::read_kv_json(&db, kvdb::KEY_AUTH_USER)?;
    // id 可能为字符串或数字（实测两种都出现过），统一转字符串
    let user_id = match user_v.as_ref().and_then(|u| u.get("id")) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let nickname = user_v
        .as_ref()
        .and_then(|u| u.get("nickname").and_then(|v| v.as_str()))
        .map(String::from);
    let yid = user_v
        .as_ref()
        .and_then(|u| u.get("yid").and_then(|v| v.as_str()))
        .map(String::from);
    Ok(Some(LiveLogin { user_id, nickname, yid, access, refresh }))
}

/// 当前 live 登录是否就是该账号（user_id 主键 + auth_ref 兜底）。
fn same_login(live: Option<&LiveLogin>, acc: &Account) -> bool {
    let Some(live) = live else { return false };
    if !live.user_id.is_empty() && !acc.user_id.is_empty() {
        return live.user_id == acc.user_id;
    }
    !acc.auth_ref.is_empty() && ref_of_token(&live.refresh) == acc.auth_ref
}

// ---------- 收编 ----------

pub struct CaptureOutcome {
    pub account: Account,
    pub warnings: Vec<String>,
    pub was_running: bool,
}

/// 收编当前登录为新账号：优雅关闭客户端（WAL 落盘）→ 全量快照 manifest 路径
/// → 解析身份 + 加密登录 blob。失败时尽量代为重启客户端。
pub fn capture_current(paths: &Paths, name: Option<String>) -> Result<CaptureOutcome, String> {
    let mut warnings: Vec<String> = vec![];
    let was_running = guard::lobster_running(paths);
    if was_running {
        guard::close_lobster_graceful(paths)?;
        warnings.push(tr("state.capture_closed"));
    }
    match capture_from_disk(paths, name, &mut warnings) {
        Ok(account) => Ok(CaptureOutcome { account, warnings, was_running }),
        Err(e) => {
            if was_running {
                let (p, ok) = guard::effective_lobster_path(paths);
                if ok {
                    let _ = guard::launch_lobster(&p);
                }
            }
            Err(e)
        }
    }
}

fn capture_from_disk(paths: &Paths, name: Option<String>, warnings: &mut Vec<String>) -> Result<Account, String> {
    let live_dir = paths.live_dir();
    let db = paths.live_db();
    if !db.is_file() {
        return Err(trf("err.capture.no_db", &[("path", &db.display().to_string())]));
    }
    let Some(login) = read_live_login(paths)? else {
        return Err(tr("err.capture.not_logged_in"));
    };
    if login.user_id.is_empty() {
        return Err(tr("err.capture.no_user_id"));
    }
    let accounts = list_accounts(paths)?;
    if accounts.iter().any(|a| a.user_id == login.user_id) {
        let dup = accounts.iter().find(|a| a.user_id == login.user_id).unwrap();
        return Err(trf("err.live.dup_saved", &[("name", &dup.name)]));
    }

    // 全量快照：live manifest 路径 → snapshot/
    let id = Uuid::new_v4().to_string();
    let acc_dir = paths.account_dir(&id);
    manifest::save_live_to_snapshot(&live_dir, &acc_dir)
        .map_err(|e| trf("err.capture.snapshot", &[("e", &e)]))?;
    if !manifest::snapshot_root(&acc_dir).join("lobsterai.sqlite").exists() {
        let _ = fs::remove_dir_all(&acc_dir);
        return Err(tr("err.capture.snapshot_no_db"));
    }

    // 登录 blob（签到独立续期用）
    let client_version = kvdb::read_client_version(&db, "2026.9.4").unwrap_or_else(|_| "2026.9.4".into());
    let rp = kvdb::read_refresh_payload(&db, &client_version);
    let blob = AuthBlob {
        access_token: login.access.clone(),
        refresh_token: login.refresh.clone(),
        first_keyfrom: rp.first_keyfrom,
        latest_keyfrom: rp.latest_keyfrom,
        uuid: rp.uuid,
        user_id: Some(login.user_id.clone()),
        version: Some(client_version),
    };
    let secret = paths.secret();
    let auth_blob = encode_auth_blob(&blob, &secret)
        .map_err(|e| trf("err.store.corrupt", &[("e", &e)]))?;

    let base = match name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => login
            .nickname
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("账号 {}", Local::now().format("%m-%d %H%M"))),
    };
    let ts = now_ts();
    let acc = Account {
        id,
        name: unique_name(&accounts, &base),
        created_at: ts.clone(),
        updated_at: ts,
        user_id: login.user_id,
        nickname: login.nickname,
        yid: login.yid,
        auth_blob: Some(auth_blob),
        auth_ref: ref_of_token(&login.refresh),
        token_exp_ms: jwt_exp_ms(&login.access),
        last_checkin: None,
    };
    save_account(paths, &acc)?;
    if manifest::snapshot_size(&acc_dir) == 0 {
        warnings.push(tr("state.capture_empty_snapshot"));
    }
    Ok(acc)
}

// ---------- 切换 ----------

/// sync_back：把 live 当前 manifest 状态移回它所属账号的快照（对话/token 全量保鲜）。
/// live 登录不属于任何已知账号时收编为新账号（auto_preserve 语义合并在此）。
fn sync_back(paths: &Paths, accounts: &[Account], warnings: &mut Vec<String>) -> Result<Option<String>, String> {
    let live_dir = paths.live_dir();
    let Some(login) = read_live_login(paths)? else {
        return Ok(None);
    };
    // 未入库 → 自动收编（含全量快照）
    if !accounts.iter().any(|a| same_login(Some(&login), a)) {
        if login.user_id.is_empty() {
            return Ok(None);
        }
        let captured = capture_from_disk(paths, None, warnings)?;
        return Ok(Some(captured.name));
    }
    let acc = accounts.iter().find(|a| same_login(Some(&login), a)).unwrap();
    let acc_dir = paths.account_dir(&acc.id);
    manifest::save_live_to_snapshot(&live_dir, &acc_dir)
        .map_err(|e| trf("err.switch.sync_back", &[("e", &e)]))?;
    // 顺带刷新元数据（token 可能被客户端续期过）
    let mut acc = acc.clone();
    acc.auth_ref = ref_of_token(&login.refresh);
    acc.token_exp_ms = jwt_exp_ms(&login.access).or(acc.token_exp_ms);
    if acc.nickname.is_none() {
        acc.nickname = login.nickname.clone();
    }
    acc.updated_at = now_ts();
    save_account(paths, &acc)?;
    Ok(None)
}

/// 首次切换前备份 live manifest 子集（backups 目录已有内容则跳过）。
fn first_backup(paths: &Paths) -> Result<Option<String>, String> {
    let bd = paths.backups_dir();
    let has_backup = bd
        .read_dir()
        .map(|mut rd| rd.any(|e| e.map(|x| x.path().exists()).unwrap_or(false)))
        .unwrap_or(false);
    if has_backup {
        return Ok(None);
    }
    let live_dir = paths.live_dir();
    if !live_dir.is_dir() {
        return Ok(None);
    }
    let dest = bd.join(format!("pre-first-switch-{}", Local::now().format("%Y%m%d-%H%M%S")));
    manifest::backup_live(&live_dir, &dest).map_err(|e| e.to_string())?;
    Ok(Some(dest.to_string_lossy().to_string()))
}

/// 从备份目录回滚 live（读回校验失败时的安全网）。
fn rollback_from_backup(paths: &Paths) -> Result<(), String> {
    let bd = paths.backups_dir();
    let Some(entry) = fs::read_dir(&bd)
        .ok()
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).max())
        .flatten()
    else {
        return Err(tr("err.switch.no_backup"));
    };
    // 备份目录是「快照形态」（含 manifest 相对结构），用 restore 语义恢复
    let live_dir = paths.live_dir();
    // 清掉 live 当前（校验失败的）manifest 内容，再从备份恢复
    for m in manifest::MANIFEST_PATHS {
        let p = live_dir.join(m);
        if p.exists() {
            manifest::remove_path(&p)?;
        }
    }
    manifest::restore_backup_to_live(&entry, &live_dir)
        .map(|_| ())
        .map_err(|e| trf("err.switch.rollback", &[("e", &e)]))
}

pub fn switch_to(paths: &Paths, id: &str, force: bool, restart: bool) -> Result<SwitchResult, String> {
    let target = load_account(paths, id)?;
    let live_dir = paths.live_dir();
    if !live_dir.is_dir() {
        return Err(trf("err.capture.no_db", &[("path", &live_dir.display().to_string())]));
    }
    let mut warnings: Vec<String> = vec![];

    // 已是当前账号：只做 sync_back 保鲜
    if let Some(login) = read_live_login(paths)? {
        if same_login(Some(&login), &target) {
            let accounts = list_accounts(paths)?;
            sync_back(paths, &accounts, &mut warnings)?;
            let mut target = target;
            target.updated_at = now_ts();
            save_account(paths, &target)?;
            return Ok(SwitchResult {
                switched: false,
                already_active: true,
                name: target.name,
                preserved_as: None,
                killed: false,
                launched: false,
                warnings,
                preserved_count: 0,
                restored_count: 0,
            });
        }
    }

    // 客户端运行中：先关闭（内存态/WAL 会覆写磁盘）
    let mut killed = false;
    if guard::lobster_running(paths) {
        if !force {
            return Err(tr("err.switch.running"));
        }
        if !guard::close_lobster_graceful(paths)? {
            return Err(tr("err.switch.kill_timeout"));
        }
        killed = true;
    }

    // 跨进程锁（签到续期回写 / 代理共用）
    let _lock = ProxyLock::acquire(&live_dir, LOCK_TIMEOUT, LOCK_STALE)
        .map_err(|e| trf("err.switch.lock", &[("e", &e)]))?;

    // 1) 首次切换前备份
    match first_backup(paths) {
        Ok(Some(dest)) => warnings.push(trf("state.backed_up", &[("dest", &dest)])),
        Ok(None) => {}
        Err(e) => warnings.push(trf("err.backup.storage", &[("e", &e)])),
    }

    // 2) live 全量保鲜回源账号（未入库登录自动收编）
    let accounts = list_accounts(paths)?;
    let preserved_as = sync_back(paths, &accounts, &mut warnings)?;
    if preserved_as.is_some() {
        warnings.push(tr("state.auto_preserved"));
    }

    // 3) 恢复目标账号快照到 live
    let acc_dir = paths.account_dir(&target.id);
    if !manifest::snapshot_root(&acc_dir).is_dir() {
        return Err(trf("err.switch.no_snapshot", &[("name", &target.name)]));
    }
    let (preserved_count, restored_count) = {
        let report = manifest::restore_snapshot_to_live(&acc_dir, &live_dir)
            .map_err(|e| trf("err.switch.restore", &[("e", &e)]))?;
        (report.restored.len(), report.restored.len())
    };

    // 4) 读回校验：live kv 的 user_id 必须等于目标账号
    let verified = match read_live_login(paths) {
        Ok(Some(login)) => !target.user_id.is_empty() && login.user_id == target.user_id,
        _ => false,
    };
    if !verified {
        let e = tr("err.switch.verify");
        // 自动回滚安全网
        match rollback_from_backup(paths) {
            Ok(()) => warnings.push(tr("state.rolled_back")),
            Err(re) => warnings.push(trf("err.switch.rollback", &[("e", &re)])),
        }
        return Err(trf("err.switch.verify_rolled", &[("e", &e)]));
    }

    let mut target = target;
    target.updated_at = now_ts();
    save_account(paths, &target)?;
    drop(_lock);

    // 5) 可选重启
    let mut launched = false;
    if restart {
        let (p, ok) = guard::effective_lobster_path(paths);
        if ok && guard::launch_lobster(&p).is_ok() {
            launched = true;
        } else if !ok {
            warnings.push(trf("err.lobster.missing", &[("path", &p)]));
        }
    }

    Ok(SwitchResult {
        switched: true,
        already_active: false,
        name: target.name,
        preserved_as,
        killed,
        launched,
        warnings,
        preserved_count,
        restored_count,
    })
}

// ---------- 其他操作 ----------

pub fn rename_account(paths: &Paths, id: &str, new_name: &str) -> Result<Account, String> {
    let name = new_name.trim();
    if name.is_empty() {
        return Err(tr("err.name.empty"));
    }
    if name.chars().count() > 40 {
        return Err(tr("err.name.too_long"));
    }
    let mut acc = load_account(paths, id)?;
    let accounts = list_accounts(paths)?;
    if let Some(other) = accounts.iter().find(|a| a.id != id && a.name.eq_ignore_ascii_case(name)) {
        return Err(trf("err.name.taken", &[("name", name), ("other", &other.name)]));
    }
    acc.name = name.to_string();
    acc.updated_at = now_ts();
    save_account(paths, &acc)?;
    Ok(acc)
}

/// 删除账号：account.json + snapshot/ 一起删（backups 保留）。
/// 若是当前活跃账号则拒绝（先切走再删）。
pub fn delete_account(paths: &Paths, id: &str) -> Result<(), String> {
    let acc = load_account(paths, id)?;
    if let Ok(Some(login)) = read_live_login(paths) {
        if same_login(Some(&login), &acc) {
            return Err(tr("err.delete.active"));
        }
    }
    let acc_dir = paths.account_dir(id);
    if acc_dir.is_dir() {
        manifest::remove_path(&acc_dir)?;
    }
    Ok(())
}

// ---------- 状态 ----------

pub fn load_settings(paths: &Paths) -> Settings {
    match fs::read_to_string(paths.settings_file()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("settings.json 损坏，已回退默认值：{e}");
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(paths: &Paths, s: &Settings) -> Result<(), String> {
    paths.ensure_dirs()?;
    let body = serde_json::to_string_pretty(s).unwrap_or_default() + "\n";
    atomic_write(&paths.settings_file(), &body)
}

pub fn get_state(paths: &Paths) -> Result<AppState, String> {
    let accounts = list_accounts(paths)?;
    let live_login = read_live_login(paths).ok().flatten();
    let live_logged_in = live_login.is_some();

    let active_account_id = if live_logged_in {
        accounts
            .iter()
            .find(|a| same_login(live_login.as_ref(), a))
            .map(|a| a.id.clone())
    } else {
        None
    };

    let summaries = accounts
        .iter()
        .map(|a| {
            let acc_dir = paths.account_dir(&a.id);
            AccountSummary {
                id: a.id.clone(),
                name: a.name.clone(),
                created_at: a.created_at.clone(),
                updated_at: a.updated_at.clone(),
                is_active: same_login(live_login.as_ref(), a),
                user_id: a.user_id.clone(),
                nickname: a.nickname.clone(),
                snapshot_ok: manifest::snapshot_root(&acc_dir).join("lobsterai.sqlite").is_file(),
                snapshot_size: manifest::snapshot_size(&acc_dir),
                token_days_left: days_left(a.token_exp_ms),
                token_exp_text: a.token_exp_ms.map(fmt_time_ms),
                health: health_of(a.token_exp_ms),
                last_checkin: a.last_checkin.clone(),
            }
        })
        .collect();

    let (lobster_path, lobster_path_ok) = guard::effective_lobster_path(paths);
    let settings = load_settings(paths);
    let proxy = match crate::proxy::health(paths) {
        Ok(v) => serde_json::to_value(&v).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };

    Ok(AppState {
        lobster_running: guard::lobster_running(paths),
        lobster_path,
        lobster_path_ok,
        live_dir: paths.live_dir().to_string_lossy().to_string(),
        live_db_exists: paths.live_db().is_file(),
        live_logged_in,
        live_name: live_login.as_ref().and_then(|l| l.nickname.clone()),
        active_account_id,
        accounts: summaries,
        store_dir: paths.store_dir().to_string_lossy().to_string(),
        launch_after_switch: settings.launch_after_switch(),
        proxy,
        language: crate::i18n::current().as_str().to_string(),
    })
}

/// 把账号敏感字段（auth_blob 加密值）挡在 CLI JSON 输出之外。
pub fn account_public(a: &Account) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "created_at": a.created_at,
        "updated_at": a.updated_at,
        "user_id": a.user_id,
        "nickname": a.nickname,
        "yid": a.yid,
        "token_exp_ms": a.token_exp_ms,
        "health": health_of(a.token_exp_ms),
        "has_blob": a.auth_blob.is_some(),
        "last_checkin": a.last_checkin,
    })
}
