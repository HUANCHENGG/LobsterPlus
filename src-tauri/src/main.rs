#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
fn ensure_cli_console() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        let cur = GetStdHandle(STD_OUTPUT_HANDLE);
        if cur != 0 && cur != INVALID_HANDLE_VALUE {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        // 重新打开 CONOUT$ 并挂到 stdout/stderr，CLI 输出才能落到父控制台
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
            let h = f.as_raw_handle() as isize;
            SetStdHandle(STD_OUTPUT_HANDLE, h);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--cli") {
        let cli_args: Vec<String> = args.iter().skip(pos + 1).cloned().collect();
        #[cfg(windows)]
        ensure_cli_console();
        let (out, code) = lobster_plus_lib::cli::run(&cli_args);
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
        std::process::exit(code);
    }
    #[cfg(feature = "gui")]
    lobster_plus_lib::run();
    #[cfg(not(feature = "gui"))]
    {
        eprintln!("此构建未启用 GUI（--no-default-features）。请使用 --cli 子命令。");
        std::process::exit(2);
    }
}
