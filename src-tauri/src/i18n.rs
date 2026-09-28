use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }
    pub fn parse(s: &str) -> Option<Lang> {
        match s.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh_cn" | "zh-hans" => Some(Lang::Zh),
            "en" | "en-us" | "en-gb" => Some(Lang::En),
            _ => None,
        }
    }
}

static LANG: RwLock<Lang> = RwLock::new(Lang::Zh);

fn lang_cell() -> std::sync::RwLockReadGuard<'static, Lang> {
    LANG.read().unwrap_or_else(|e| e.into_inner())
}

pub fn current() -> Lang {
    *lang_cell()
}

pub fn set(lang: Lang) {
    *LANG.write().unwrap_or_else(|e| e.into_inner()) = lang;
}

pub fn resolve(explicit: Option<&str>) -> Lang {
    if let Some(l) = explicit.and_then(Lang::parse) {
        return l;
    }
    if let Some(os) = sys_locale::get_locale() {
        if !os.to_ascii_lowercase().starts_with("zh") {
            return Lang::En;
        }
    }
    Lang::Zh
}

pub fn init_from_settings(s: &crate::store::Settings) {
    set(resolve(s.language.as_deref()));
}

pub fn tr(key: &str) -> String {
    let lang = current();
    lookup(lang, key).unwrap_or_else(|| lookup(Lang::Zh, key).unwrap_or_else(|| key.to_string()))
}

pub fn trf(key: &str, params: &[(&str, &str)]) -> String {
    let mut s = tr(key);
    for (k, v) in params {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

fn lookup(lang: Lang, key: &str) -> Option<String> {
    let table: &[(&str, &str)] = match lang {
        Lang::Zh => ZH,
        Lang::En => EN,
    };
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

const ZH: &[(&str, &str)] = &[
    // 通用
    ("err.write", "写入失败：{e}"),
    ("err.write_file", "写入失败 {path}: {e}"),
    ("err.rename_fail", "落盘失败 {path}: {e}"),
    ("err.read", "无法读取 {path}: {e}"),
    ("err.bad_json", "{path} 不是有效的 JSON：{e}"),
    ("err.mkdir", "无法创建目录：{e}"),
    ("err.store.mk_accounts_dir", "无法创建账号库目录：{e}"),
    ("err.store.list_fail", "读取账号库失败：{e}"),
    ("err.store.bad_id", "非法的账号 id"),
    ("err.store.no_account_id", "账号不存在：{id}"),
    ("err.store.corrupt", "账号存档损坏：{e}"),
    ("err.store.decrypt_fail", "账号快照解密失败（密钥不匹配或数据损坏）"),
    ("err.name.empty", "名称不能为空"),
    ("err.name.too_long", "名称过长（最多 40 字符）"),
    ("err.name.taken", "名称「{name}」已被账号「{other}」占用"),
    // 收编
    ("err.capture.no_db", "未找到 LobsterAI 数据目录（{path}）。请先安装并登录客户端"),
    ("err.capture.not_logged_in", "LobsterAI 当前未登录"),
    ("err.capture.no_user_id", "登录态缺用户 id（auth_user 异常），请重新登录后收编"),
    ("err.capture.snapshot", "目录快照失败：{e}"),
    ("err.capture.snapshot_no_db", "快照里没有 lobsterai.sqlite（数据目录异常），已回滚"),
    ("err.live.dup_saved", "当前登录已作为账号「{name}」保存在库中（如需刷新请切换到此账号做一次同步）"),
    ("state.capture_closed", "已优雅关闭 LobsterAI 以读取最新数据（WAL 已落盘）"),
    ("state.capture_empty_snapshot", "快照为空（数据目录可能有异常），请检查"),
    // 切换
    ("err.switch.running", "LobsterAI 正在运行，请先关闭后再切换（或选择强制关闭）"),
    ("err.switch.kill_timeout", "关闭 LobsterAI 超时，请手动关闭后重试"),
    ("err.switch.lock", "获取数据目录锁失败（签到/代理可能正忙）：{e}"),
    ("err.switch.sync_back", "当前账号数据保鲜失败：{e}"),
    ("err.switch.no_snapshot", "账号「{name}」没有目录快照，无法切换（请重新收编）"),
    ("err.switch.restore", "恢复目标账号数据失败：{e}"),
    ("err.switch.verify", "切换后读回校验未通过"),
    ("err.switch.verify_rolled", "切换校验未通过，已尝试从备份回滚：{e}"),
    ("err.switch.no_backup", "没有可用备份，无法自动回滚"),
    ("err.switch.rollback", "回滚失败：{e}"),
    ("state.rolled_back", "已自动从备份回滚 live 数据目录"),
    ("err.lobster.missing", "找不到 LobsterAI 主程序：{path}"),
    ("err.lobster.launch", "启动 LobsterAI 失败：{e}"),
    ("err.lobster.kill_timeout", "关闭 LobsterAI 超时"),
    // 备份
    ("err.backup.storage", "切换前备份失败（不阻断切换）：{e}"),
    ("state.backed_up", "首次切换前已备份到 {dest}"),
    // 切换结果附注
    ("state.auto_preserved", "发现未入库的登录，已自动收编为新账号"),
    ("err.delete.active", "该账号当前正活跃，请先切换到其他账号再删除"),
    // 代理
    ("err.proxy.sandbox", "沙箱模式下不可操作代理"),
    ("err.proxy.no_python", "找不到 Python（PATH → LobsterAI 自带运行时 → py 启动器均失败）"),
    ("err.proxy.launch", "启动代理失败：{e}"),
    ("err.proxy.start_timeout", "代理启动超时（/health 未就绪）"),
    ("err.proxy.log_tail", "代理日志尾部：\n{log}"),
    // CC-Switch 注册
    ("err.register.no_db", "未找到 CC-Switch 数据库：{path}（请先安装并运行一次 CC-Switch）"),
    ("err.register.fail", "写入 CC-Switch 失败：{e}"),
    ("err.register.port_unavailable", "代理未在运行，无法确定注册端口（请先启动代理）"),
    // 对话框
    ("dialog.exe", "可执行文件"),
    // CLI
    ("cli.missing_cmd", "缺少子命令。可用：state/list/capture/rename/delete/switch/checkin/proxy/register/schedule/doctor/manifest"),
    ("cli.usage.capture", "用法：--cli capture [--name <名称>]"),
    ("cli.usage.switch", "用法：--cli switch --id <账号id> [--force] [--restart|--no-restart]"),
    ("cli.usage.rename", "用法：--cli rename --id <账号id> --name <新名称>"),
    ("cli.usage.delete", "用法：--cli delete --id <账号id>"),
    ("cli.usage.checkin", "用法：--cli checkin [--id <账号id>]（缺省对全库账号签到）"),
    ("cli.usage.proxy", "用法：--cli proxy start|stop|status"),
    ("cli.usage.register", "用法：--cli register [--switch]（把 LobsterPlus 注册进 CC-Switch）"),
    ("cli.usage.schedule", "用法：--cli schedule install|uninstall（每日 09:00 计划任务跑 checkin --all）"),
    ("cli.unknown_cmd", "未知子命令：{cmd}"),
    ("cli.read_fail", "读取文件失败：{e}"),
    ("cli.json_fail", "JSON 解析失败：{e}"),
];

const EN: &[(&str, &str)] = &[
    ("err.write", "Write failed: {e}"),
    ("err.write_file", "Write failed {path}: {e}"),
    ("err.rename_fail", "Persist failed {path}: {e}"),
    ("err.read", "Cannot read {path}: {e}"),
    ("err.bad_json", "{path} is not valid JSON: {e}"),
    ("err.mkdir", "Cannot create directory: {e}"),
    ("err.store.mk_accounts_dir", "Cannot create accounts dir: {e}"),
    ("err.store.list_fail", "Failed to list accounts: {e}"),
    ("err.store.bad_id", "Invalid account id"),
    ("err.store.no_account_id", "Account not found: {id}"),
    ("err.store.corrupt", "Account snapshot corrupted: {e}"),
    ("err.store.decrypt_fail", "Snapshot decrypt failed (key mismatch or corrupted)"),
    ("err.name.empty", "Name cannot be empty"),
    ("err.name.too_long", "Name too long (max 40 chars)"),
    ("err.name.taken", "Name \"{name}\" is already taken by \"{other}\""),
    ("err.capture.no_db", "LobsterAI data directory not found ({path}). Install and log in first"),
    ("err.capture.not_logged_in", "LobsterAI is not logged in"),
    ("err.capture.no_user_id", "Login state has no user id (auth_user broken); log in again and re-capture"),
    ("err.capture.snapshot", "Directory snapshot failed: {e}"),
    ("err.capture.snapshot_no_db", "Snapshot has no lobsterai.sqlite (data dir broken); rolled back"),
    ("err.live.dup_saved", "Current login already saved as \"{name}\" (switch to it and sync to refresh)"),
    ("state.capture_closed", "LobsterAI closed gracefully so the latest data is on disk (WAL flushed)"),
    ("state.capture_empty_snapshot", "Snapshot is empty (data dir may be broken); please check"),
    ("err.switch.running", "LobsterAI is running. Close it first (or force close)"),
    ("err.switch.kill_timeout", "Timed out closing LobsterAI; close it manually and retry"),
    ("err.switch.lock", "Failed to acquire the data-dir lock (check-in/proxy busy?): {e}"),
    ("err.switch.sync_back", "Failed to preserve current account data: {e}"),
    ("err.switch.no_snapshot", "Account \"{name}\" has no directory snapshot; re-capture it first"),
    ("err.switch.restore", "Failed to restore target account data: {e}"),
    ("err.switch.verify", "Post-switch verification failed"),
    ("err.switch.verify_rolled", "Verification failed; rollback from backup attempted: {e}"),
    ("err.switch.no_backup", "No backup available; cannot auto-rollback"),
    ("err.switch.rollback", "Rollback failed: {e}"),
    ("state.rolled_back", "Live data directory restored from backup automatically"),
    ("err.lobster.missing", "LobsterAI executable not found: {path}"),
    ("err.lobster.launch", "Failed to launch LobsterAI: {e}"),
    ("err.lobster.kill_timeout", "Timed out killing LobsterAI"),
    ("err.backup.storage", "Pre-switch backup failed (switch continues): {e}"),
    ("state.backed_up", "Backed up to {dest} before first switch"),
    ("state.auto_preserved", "Unknown login detected; captured as a new account"),
    ("err.delete.active", "This account is currently active; switch away before deleting"),
    ("err.proxy.sandbox", "Proxy operations are disabled in sandbox mode"),
    ("err.proxy.no_python", "Python not found (PATH → LobsterAI runtime → py launcher all failed)"),
    ("err.proxy.launch", "Failed to launch proxy: {e}"),
    ("err.proxy.start_timeout", "Proxy start timed out (/health not ready)"),
    ("err.proxy.log_tail", "Proxy log tail:\n{log}"),
    ("err.register.no_db", "CC-Switch database not found: {path} (install and run CC-Switch once)"),
    ("err.register.fail", "Failed to write CC-Switch: {e}"),
    ("err.register.port_unavailable", "Proxy not running; cannot determine the port (start the proxy first)"),
    ("dialog.exe", "Executable"),
    ("cli.missing_cmd", "Missing subcommand. Available: state/list/capture/rename/delete/switch/checkin/proxy/register/schedule/doctor/manifest"),
    ("cli.usage.capture", "Usage: --cli capture [--name <name>]"),
    ("cli.usage.switch", "Usage: --cli switch --id <account-id> [--force] [--restart|--no-restart]"),
    ("cli.usage.rename", "Usage: --cli rename --id <account-id> --name <new-name>"),
    ("cli.usage.delete", "Usage: --cli delete --id <account-id>"),
    ("cli.usage.checkin", "Usage: --cli checkin [--id <account-id>] (all accounts when omitted)"),
    ("cli.usage.proxy", "Usage: --cli proxy start|stop|status"),
    ("cli.usage.register", "Usage: --cli register [--switch] (register LobsterPlus into CC-Switch)"),
    ("cli.usage.schedule", "Usage: --cli schedule install|uninstall (daily 09:00 task running checkin --all)"),
    ("cli.unknown_cmd", "Unknown subcommand: {cmd}"),
    ("cli.read_fail", "Failed to read file: {e}"),
    ("cli.json_fail", "Failed to parse JSON: {e}"),
];
