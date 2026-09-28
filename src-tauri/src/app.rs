//! Tauri GUI 命令层（`gui` feature）。CLI 与单测不带此模块。

use crate::checkin;
use crate::cli;
use crate::guard;
use crate::i18n;
use crate::proxy;
use crate::register;
use crate::store::{self, Paths, Settings};
use serde_json::{json, Value};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

static STORE_LOCK: Mutex<()> = Mutex::new(());

fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    match STORE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn emit_changed(app: &AppHandle) {
    let _ = app.emit("state-changed", ());
}

#[tauri::command]
fn get_state() -> Result<store::AppState, String> {
    let paths = Paths::detect();
    store::get_state(&paths)
}

#[tauri::command]
fn capture_current(app: AppHandle, name: Option<String>) -> Result<Value, String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let out = store::capture_current(&paths, name)?;
    emit_changed(&app);
    Ok(json!({
        "account": store::account_public(&out.account),
        "warnings": out.warnings,
        "was_running": out.was_running,
    }))
}

#[tauri::command]
fn rename_account(id: String, name: String) -> Result<Value, String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let acc = store::rename_account(&paths, &id, &name)?;
    Ok(store::account_public(&acc))
}

#[tauri::command]
fn delete_account(id: String) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    store::delete_account(&paths, &id)
}

#[tauri::command]
fn switch_to(id: String, force: bool, restart: Option<bool>) -> Result<store::SwitchResult, String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let settings = store::load_settings(&paths);
    let restart = restart.unwrap_or_else(|| settings.launch_after_switch());
    store::switch_to(&paths, &id, force, restart)
}

#[tauri::command]
fn checkin(id: Option<String>) -> Result<Value, String> {
    // 不持 STORE_LOCK：签到只写各账号自己的 JSON，不碰 live 数据目录
    let paths = Paths::detect();
    checkin::cli_run(&paths, id)
}

#[tauri::command]
fn kill_lobster() -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    guard::close_lobster_graceful(&paths).map(|_| ())
}

#[tauri::command]
fn launch_lobster() -> Result<(), String> {
    let paths = Paths::detect();
    let (p, ok) = guard::effective_lobster_path(&paths);
    if !ok {
        return Err(i18n::trf("err.lobster.missing", &[("path", &p)]));
    }
    guard::launch_lobster(&p)
}

#[tauri::command]
async fn pick_lobster_path(app: AppHandle) -> Result<Value, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.exe"), &["exe"])
        .blocking_pick_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp
        .into_path()
        .map_err(|e| format!("{e}"))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy() }))
}

// ---------- 代理 ----------

#[tauri::command]
fn proxy_status() -> Result<Value, String> {
    let paths = Paths::detect();
    proxy::health(&paths).map(|st| serde_json::to_value(st).unwrap_or(Value::Null))
}

#[tauri::command]
fn proxy_start() -> Result<Value, String> {
    let paths = Paths::detect();
    proxy::start(&paths).map(|st| serde_json::to_value(st).unwrap_or(Value::Null))
}

#[tauri::command]
fn proxy_stop() -> Result<Value, String> {
    let paths = Paths::detect();
    proxy::stop(&paths).map(|stopped| json!({ "stopped": stopped }))
}

#[tauri::command]
fn proxy_log() -> Result<Value, String> {
    let paths = Paths::detect();
    Ok(json!({ "tail": proxy::log_tail(&paths, 80) }))
}

#[tauri::command]
fn register_ccswitch(switch_to_it: Option<bool>) -> Result<Value, String> {
    let paths = Paths::detect();
    register::register(&paths, switch_to_it.unwrap_or(false))
}

// ---------- 设置 ----------

#[tauri::command]
fn set_settings(
    app: AppHandle,
    lobster_path: Option<String>,
    launch_after_switch: Option<bool>,
    language: Option<String>,
    proxy_autostart: Option<bool>,
) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = store::load_settings(&paths);
    if let Some(p) = lobster_path {
        let t = p.trim();
        s.lobster_path = (!t.is_empty()).then(|| t.to_string());
    }
    if let Some(b) = launch_after_switch {
        s.launch_after_switch = Some(b);
    }
    if let Some(l) = language {
        if i18n::Lang::parse(&l).is_some() {
            s.language = Some(l);
            i18n::init_from_settings(&s);
        }
    }
    if let Some(b) = proxy_autostart {
        s.proxy_autostart = Some(b);
    }
    let r = store::save_settings(&paths, &s);
    emit_changed(&app);
    r
}

/// 补签一次（不持 STORE_LOCK：签到不碰 live 数据目录）。
fn catchup_once(paths: &Paths) -> Result<(), String> {
    let r = checkin::catchup(paths)?;
    if let Some(results) = r.get("results").and_then(|v| v.as_array()) {
        for item in results {
            eprintln!(
                "checkin: {} {} - {}",
                item.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                item.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                item.get("msg").and_then(|v| v.as_str()).unwrap_or(""),
            );
        }
    }
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_state,
            capture_current,
            rename_account,
            delete_account,
            switch_to,
            checkin,
            kill_lobster,
            launch_lobster,
            pick_lobster_path,
            proxy_status,
            proxy_start,
            proxy_stop,
            proxy_log,
            register_ccswitch,
            set_settings,
        ])
        .setup(|app| {
            i18n::init_from_settings(&store::load_settings(&Paths::detect()));
            // 补签：启动时静默对「今天未签」的账号签到；之后每小时检查一次。
            // 失败只记 stderr，不打扰用户（手动启动模式下靠这个保证不漏签）。
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let paths = Paths::detect();
                let settings = store::load_settings(&paths);
                if settings.proxy_autostart() {
                    if let Err(e) = proxy::start(&paths) {
                        eprintln!("proxy autostart: {e}");
                    }
                    let _ = app_handle.emit("state-changed", ());
                }
                if let Err(e) = catchup_once(&paths) {
                    eprintln!("startup checkin: {e}");
                }
                let _ = app_handle.emit("state-changed", ());
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                    let paths = Paths::detect();
                    if let Err(e) = catchup_once(&paths) {
                        eprintln!("hourly checkin: {e}");
                    }
                    let _ = app_handle.emit("state-changed", ());
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// 保持 cli 模块在 GUI feature 下被引用（doctor 等 CLI 入口在无 GUI 也可用）
#[allow(dead_code)]
fn _cli_marker() {
    let _ = cli::run(&[]);
}
