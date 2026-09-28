fn main() {
    // 本机 mingw64 工具链的 time.h 在默认宏组合下会 #include <pthread_time.h>，
    // 而该头文件在本机缺失（winpthread 开发文件不完整）。_STRICT_STDC 宏
    // 可绕过该 include，且 time/localtime 等 sqlite3.c 用到的函数全部可用
    // （已实测）。cc-rs 读 CFLAGS 环境变量，libsqlite3-sys 的 build script
    // 由 cargo 继承本进程环境，直接 set_var 即可生效。
    #[cfg(target_env = "gnu")]
    {
        let flags = std::env::var("CFLAGS").unwrap_or_default();
        if !flags.contains("_STRICT_STDC") {
            let patched = if flags.is_empty() {
                "-D_STRICT_STDC".to_string()
            } else {
                format!("{flags} -D_STRICT_STDC")
            };
            std::env::set_var("CFLAGS", &patched);
            println!("cargo:rerun-if-env-changed=CFLAGS");
        }
    }
    // tauri_build 只在 GUI feature 下需要（无 GUI 时不必解析 tauri.conf.json /
    // 嵌入图标，纯逻辑测试编译更快）
    #[cfg(feature = "gui")]
    {
        // 图标文件变化必须触发资源段重编（embed-resource 在 build script 里
        // 执行；只改 icons/*.ico 而不动 conf 时 cargo 不会重跑本脚本）
        println!("cargo:rerun-if-changed=icons");
        tauri_build::build()
    }
}
