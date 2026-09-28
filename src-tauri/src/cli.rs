use crate::guard;
use crate::store::*;
use serde_json::{json, Value};

fn flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn parse_bool(v: &str) -> bool {
    matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn ok(v: Value) -> String {
    let mut m = v.as_object().cloned().unwrap_or_default();
    m.insert("ok".into(), Value::Bool(true));
    serde_json::to_string_pretty(&Value::Object(m)).unwrap()
}

fn err(e: &str) -> String {
    serde_json::to_string_pretty(&json!({ "ok": false, "error": e })).unwrap()
}

pub fn run(args: &[String]) -> (String, i32) {
    let mut lang_override: Option<crate::i18n::Lang> = None;
    let args: Vec<String> = {
        let mut out = vec![];
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--lang" {
                if let Some(l) = it.next().and_then(|v| crate::i18n::Lang::parse(v)) {
                    lang_override = Some(l);
                }
            } else {
                out.push(a.clone());
            }
        }
        out
    };
    let paths = Paths::detect();

    if let Some(l) = lang_override {
        crate::i18n::set(l);
    } else {
        crate::i18n::init_from_settings(&load_settings(&paths));
    }

    let Some(cmd) = args.first().cloned() else {
        return (err(&crate::i18n::tr("cli.missing_cmd")), 2);
    };
    let rest = &args[1..];

    let out = match cmd.as_str() {
        "state" => match get_state(&paths) {
            Ok(st) => ok(serde_json::to_value(st).unwrap_or(Value::Null)),
            Err(e) => return (err(&e), 1),
        },
        "list" => match list_accounts(&paths) {
            Ok(a) => ok(json!({
                "accounts": a.iter().map(account_public).collect::<Vec<_>>()
            })),
            Err(e) => return (err(&e), 1),
        },
        "manifest" => ok(crate::manifest::manifest_json()),
        "capture" => {
            let name = flag(rest, "--name");
            match capture_current(&paths, name) {
                Ok(out) => {
                    if has_flag(rest, "--restart") {
                        let (p, ok_path) = guard::effective_lobster_path(&paths);
                        if ok_path {
                            let _ = guard::launch_lobster(&p);
                        }
                    }
                    ok(json!({
                        "account": account_public(&out.account),
                        "warnings": out.warnings,
                        "was_running": out.was_running,
                    }))
                }
                Err(e) => return (err(&e), 1),
            }
        }
        "rename" => {
            let (Some(id), Some(name)) = (flag(rest, "--id"), flag(rest, "--name")) else {
                return (err(&crate::i18n::tr("cli.usage.rename")), 2);
            };
            match rename_account(&paths, &id, &name) {
                Ok(a) => ok(account_public(&a)),
                Err(e) => return (err(&e), 1),
            }
        }
        "delete" => {
            let Some(id) = flag(rest, "--id") else {
                return (err(&crate::i18n::tr("cli.usage.delete")), 2);
            };
            match delete_account(&paths, &id) {
                Ok(()) => ok(json!({ "deleted": id })),
                Err(e) => return (err(&e), 1),
            }
        }
        "switch" => {
            let Some(id) = flag(rest, "--id") else {
                return (err(&crate::i18n::tr("cli.usage.switch")), 2);
            };
            if has_flag(rest, "--dry-run") {
                let live = paths.live_dir();
                return (ok(json!({
                    "live_dir": live.to_string_lossy(),
                    "manifest_existing": crate::manifest::live_manifest_paths(&live),
                })), 0);
            }
            let force = has_flag(rest, "--force");
            let settings = load_settings(&paths);
            let restart = if has_flag(rest, "--restart") {
                true
            } else if has_flag(rest, "--no-restart") {
                false
            } else {
                settings.launch_after_switch()
            };
            match switch_to(&paths, &id, force, restart) {
                Ok(r) => ok(serde_json::to_value(&r).unwrap_or(Value::Null)),
                Err(e) => return (err(&e), 1),
            }
        }
        "checkin" => match crate::checkin::cli_run(&paths, flag(rest, "--id")) {
            Ok(v) => ok(v),
            Err(e) => return (err(&e), 1),
        },
        "proxy" => {
            let Some(sub) = rest.first().cloned() else {
                return (err(&crate::i18n::tr("cli.usage.proxy")), 2);
            };
            match sub.as_str() {
                "start" => match crate::proxy::start(&paths) {
                    Ok(st) => ok(serde_json::to_value(&st).unwrap_or(Value::Null)),
                    Err(e) => return (err(&e), 1),
                },
                "stop" => match crate::proxy::stop(&paths) {
                    Ok(true) => ok(json!({ "stopped": true })),
                    Ok(false) => return (err("代理停止超时"), 1),
                    Err(e) => return (err(&e), 1),
                },
                "status" => match crate::proxy::health(&paths) {
                    Ok(st) => ok(serde_json::to_value(&st).unwrap_or(Value::Null)),
                    Err(e) => return (err(&e), 1),
                },
                "log" => ok(json!({ "tail": crate::proxy::log_tail(&paths, 80) })),
                other => return (err(&format!("未知 proxy 子命令：{other}")), 2),
            }
        }
        "register" => match crate::register::register(&paths, has_flag(rest, "--switch")) {
            Ok(v) => ok(v),
            Err(e) => return (err(&e), 1),
        },
        "schedule" => {
            let Some(sub) = rest.first().cloned() else {
                return (err(&crate::i18n::tr("cli.usage.schedule")), 2);
            };
            match sub.as_str() {
                "install" => match install_schedule(&paths) {
                    Ok(v) => ok(v),
                    Err(e) => return (err(&e), 1),
                },
                "uninstall" => match uninstall_schedule() {
                    Ok(v) => ok(v),
                    Err(e) => return (err(&e), 1),
                },
                "status" => ok(schedule_status()),
                other => return (err(&format!("未知 schedule 子命令：{other}")), 2),
            }
        }
        "doctor" => ok(doctor(&paths)),
        "launch" => {
            let (p, ok_path) = guard::effective_lobster_path(&paths);
            if !ok_path {
                return (err(&crate::i18n::trf("err.lobster.missing", &[("path", &p)])), 1);
            }
            match guard::launch_lobster(&p) {
                Ok(()) => ok(json!({ "launched": p })),
                Err(e) => return (err(&e), 1),
            }
        }
        "kill" => match guard::kill_lobster(&paths) {
            Ok(true) => ok(json!({ "killed": true })),
            Ok(false) => return (err(&crate::i18n::tr("err.lobster.kill_timeout")), 1),
            Err(e) => return (err(&e), 1),
        },
        "setpath" => {
            let Some(p) = flag(rest, "--path") else {
                return (err("用法：--cli setpath --path <LobsterAI.exe 完整路径>"), 2);
            };
            let mut s = load_settings(&paths);
            s.lobster_path = Some(p);
            match save_settings(&paths, &s) {
                Ok(()) => ok(json!({})),
                Err(e) => return (err(&e), 1),
            }
        }
        "behavior" => {
            let las = flag(rest, "--launch-after-switch");
            let lang = flag(rest, "--language");
            let mut s = load_settings(&paths);
            if let Some(v) = las {
                s.launch_after_switch = Some(parse_bool(&v));
            }
            if let Some(v) = lang {
                if crate::i18n::Lang::parse(&v).is_some() {
                    s.language = Some(v);
                }
            }
            match save_settings(&paths, &s) {
                Ok(()) => ok(json!({
                    "launch_after_switch": s.launch_after_switch(),
                    "language": s.language,
                })),
                Err(e) => return (err(&e), 1),
            }
        }
        other => return (err(&crate::i18n::trf("cli.unknown_cmd", &[("cmd", other)])), 2),
    };
    (out, 0)
}

// ---------- Windows 计划任务（每日签到） ----------

const TASK_NAME: &str = "LobsterPlus 每日签到";

fn current_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn schtasks(args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("schtasks 调用失败：{e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let err_text = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(format!("schtasks 失败：{err_text}"));
    }
    Ok(text)
}

fn install_schedule(_paths: &Paths) -> Result<Value, String> {
    let exe = current_exe();
    if exe.is_empty() {
        return Err("无法定位当前 exe".into());
    }
    let cmd = format!("\"{exe}\" --cli checkin");
    schtasks(&[
        "/Create", "/TN", TASK_NAME, "/SC", "DAILY", "/ST", "09:00",
        "/TR", &cmd, "/F",
    ])?;
    Ok(json!({ "installed": true, "task": TASK_NAME, "daily_at": "09:00", "command": cmd }))
}

fn uninstall_schedule() -> Result<Value, String> {
    schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
    Ok(json!({ "uninstalled": true, "task": TASK_NAME }))
}

fn schedule_status() -> Value {
    match schtasks(&["/Query", "/TN", TASK_NAME]) {
        Ok(_) => json!({ "installed": true, "task": TASK_NAME }),
        Err(_) => json!({ "installed": false, "task": TASK_NAME }),
    }
}

// ---------- doctor：一键诊断 ----------

fn doctor(paths: &Paths) -> Value {
    let mut items: Vec<(String, bool, String)> = vec![];

    // 数据目录
    let live = paths.live_dir();
    let db = paths.live_db();
    items.push(("live_data_dir".into(), live.is_dir(), live.to_string_lossy().to_string()));
    items.push(("live_sqlite".into(), db.is_file(), db.to_string_lossy().to_string()));

    // 安装路径
    let (exe, exe_ok) = guard::effective_lobster_path(paths);
    items.push(("lobster_exe".into(), exe_ok, exe));

    // 进程
    let running = guard::lobster_running(paths);
    items.push(("lobster_running".into(), true, running.to_string()));

    // 登录态
    match read_live_login(paths) {
        Ok(Some(l)) => items.push((
            "live_login".into(),
            true,
            format!("user_id={} nickname={}", l.user_id, l.nickname.unwrap_or_default()),
        )),
        Ok(None) => items.push(("live_login".into(), false, "未登录".into())),
        Err(e) => items.push(("live_login".into(), false, e)),
    }

    // 账号库
    let accounts = list_accounts(paths).unwrap_or_default();
    items.push(("accounts".into(), true, format!("{} 个账号", accounts.len())));

    // 上游探测（LobsterAI 本地模型代理）
    let upstream = probe_upstream_summary();
    items.push(("upstream_model_proxy".into(), upstream.1, upstream.2.clone()));
    let _ = upstream;

    // 我们的代理
    match crate::proxy::health(paths) {
        Ok(st) => items.push((
            "lobsterplus_proxy".into(),
            st.running,
            format!("running={} port={} upstream_alive={:?}", st.running, st.port, st.upstream_alive),
        )),
        Err(e) => items.push(("lobsterplus_proxy".into(), false, e)),
    }

    // Python
    let py = crate::proxy::find_python();
    items.push((
        "python".into(),
        py.is_some(),
        py.map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "未找到".into()),
    ));

    // CC-Switch
    let ccs = cc_switch_db_path();
    let ccs_ok = ccs.is_file();
    items.push(("cc_switch_db".into(), ccs_ok, ccs.to_string_lossy().to_string()));

    // 计划任务
    let sched = schedule_status();
    items.push((
        "daily_checkin_task".into(),
        true,
        if sched["installed"].as_bool().unwrap_or(false) { "已安装（每日 09:00）".into() } else { "未安装".into() },
    ));

    let checks: Vec<Value> = items
        .into_iter()
        .map(|(k, ok, detail)| json!({ "item": k, "ok": ok, "detail": detail }))
        .collect();
    let all_ok = checks.iter().filter(|c| c["ok"].as_bool().unwrap_or(false)).count();
    json!({ "checks": checks, "ok_count": all_ok, "total": checks.len() })
}

fn probe_upstream_summary() -> (String, bool, String) {
    // 复用 Python 代理的探测逻辑路径：读两个候选文件
    let live = Paths::detect().live_dir();
    let mut cands = vec![];
    let mf = live.join("openclaw").join("state").join("agents").join("main").join("agent").join("models.json");
    if let Ok(s) = std::fs::read_to_string(&mf) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if let Some(provs) = v.get("providers").and_then(|p| p.as_object()) {
                for (_pid, p) in provs {
                    let base = p.get("baseUrl").and_then(|x| x.as_str()).unwrap_or("");
                    if !base.is_empty() && base.contains("127.0.0.1") {
                        cands.push(base.to_string());
                    }
                }
            }
        }
    }
    let oc = live.join("openclaw").join("state").join("openclaw.json");
    if let Ok(s) = std::fs::read_to_string(&oc) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if let Some(provs) = v.pointer("/models/providers").and_then(|p| p.as_object()) {
                for (_pid, p) in provs {
                    let base = p.get("baseUrl").and_then(|x| x.as_str()).unwrap_or("");
                    if !base.is_empty() && base.contains("127.0.0.1") {
                        cands.push(base.to_string());
                    }
                }
            }
        }
    }
    cands.dedup();
    let desc = if cands.is_empty() {
        "未发现候选（LobsterAI 从未运行？）".to_string()
    } else {
        cands.join(" / ")
    };
    ("upstream".into(), !cands.is_empty(), desc)
}

fn cc_switch_db_path() -> std::path::PathBuf {
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    #[cfg(not(windows))]
    let home = std::env::var("HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    home.join(".cc-switch").join("cc-switch.db")
}

#[cfg(not(windows))]
fn schtasks(_args: &[&str]) -> Result<String, String> {
    Err("计划任务仅支持 Windows".into())
}
