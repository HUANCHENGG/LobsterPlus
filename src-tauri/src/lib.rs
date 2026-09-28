//! LobsterPlus 库。GUI（tauri 命令层）在 `gui` feature（默认开）下编译；
//! `--no-default-features` 时只剩纯逻辑模块，供单元测试/CLI 无 GUI运行。

pub mod checkin;
pub mod cli;
pub mod guard;
pub mod i18n;
pub mod kvdb;
pub mod manifest;
pub mod proxy;
pub mod register;
pub mod store;
pub mod zcrypto;
mod lockfile;

#[cfg(feature = "gui")]
mod app;

#[cfg(feature = "gui")]
pub use app::run;
