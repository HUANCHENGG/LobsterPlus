# CLAUDE.md

LobsterPlus：有道 LobsterAI 多账号切换器（Tauri 2 + Rust + 原生 JS 前端）。
架构蓝本：RaccoonPlus / TraePlus（家族同构）。详细设计见 `docs/architecture.md`。

## 构建红线（必读）

1. **本机必须走 GNU 工具链**（MSVC 链接环境残缺）：release 构建一律用 `scripts\build-release.cmd`（内部前置 `PATH=D:\mingw64\bin` 并带 `--features tauri/custom-protocol` 嵌入前端）。直接 `cargo build --release` 不带 custom-protocol 会产出连 127.0.0.1:5177 的废 exe。
2. **`[lib]` 只留 `rlib`**：cdylib 会被 MinGW 链成 DLL 且 ureq/rustls 导出符号超 ld 65535 上限（家族教训）。
3. 前端先 `npm install`（package-lock 未提交前）。开发：`npm run tauri dev`。
4. 单测：`cargo test --manifest-path src-tauri\Cargo.toml`。

## 模块地图

| 模块 | 职责 | 关键点 |
|---|---|---|
| `store.rs` | 账号库 + 切换编排 | 读回校验失败自动回滚；same_login 以 `auth_user.id` 为主键 |
| `manifest.rs` | 目录快照交换引擎 | MANIFEST_PATHS / EXCLUDED_PATHS 两张清单；串号防线=restore 时清 live 残留 |
| `kvdb.rs` | lobsterai.sqlite kv 读写 | live 库只读红线；write 仅限客户端退出后的续期回写 |
| `checkin.rs` | 签到三步 HTTP + token 续期 | 51102=登录失效；51104=已签（幂等）；1h 节流 |
| `guard.rs` | LobsterAI.exe 进程管理 | 优雅关闭=taskkill 不带 /F（WAL 落盘）；Electron 多进程全退才继续 |
| `proxy.rs` | Python 代理生命周期 | include_str! 内嵌脚本；.released-tag 版本标记自动更新；config.json 只在缺失时写 |
| `register.rs` | CC-Switch 注册 | 写前备份 db、写后还原激活项；apiFormat=anthropic |
| `zcrypto.rs` | enc:v1 AES-256-GCM | AAD=`lobster-plus:v1`（注意与家族前作不同） |
| `lockfile.rs` | ProxyLock 跨进程锁 | 锁超时 30s/陈旧 180s（目录拷贝耗时长） |
| `cli.rs` | CLI 镜像 + doctor + schtasks | `--cli` 前缀分流（main.rs） |

## 数据契约

- live 数据目录：`%APPDATA%\LobsterAI`；自有库：`~\.lobster-plus\`（沙箱 `LOBSTER_PLUS_HOME` 重定向）。
- 账号 = `accounts\<uuid>\account.json`（元数据+加密 auth_blob）+ `accounts\<uuid>\snapshot\`（manifest 目录快照）。
- 凭据加密密钥：环境变量 `LOBSTER_PLUS_CREDENTIAL_SECRET`，缺省机器指纹派生（跨机迁移必须带环境变量）。
- 代理固定端口 19260（`proxy\config.json` 可改）；上游候选来自 `openclaw\state\...\models.json` 与 `openclaw\state\openclaw.json`。

## 修改守则

1. **绝不在客户端运行时写 live lobsterai.sqlite**（WAL 竞态会损坏数据）。切换/收编前必须 `guard::close_lobster_graceful`。
2. **manifest 改动必须跑 `manifest_conflicts` 单测**（manifest 与 EXCLUDED 不得重叠）并补 roundtrip 测试。
3. 切换流程任何失败路径都要考虑回滚（读回校验失败 → rollback_from_backup）。
4. CLI 新子命令：i18n.rs 补 usage 文案（zh/en 两份）；JSON 输出不得含 token/apiKey。
5. lobster_proxy.py 是纯标准库脚本（Python 3.8+），禁止引第三方依赖；改完跑 `--selftest`。
6. 前端无框架（原生 ESM），文案进 `src/locales/{zh,en}.js`，图标进 `src/icons.js`。

## 已知上游事实（逆向结论，勿凭印象改）

- 签到服务端：`https://lobsterai-server.youdao.com`（testMode 走 inner）；
- 登录失效码 51102、已签幂等码 51104、成功码 0；
- LobsterAI 本地模型代理端口随重启轮换（历史 3248/3858/2880/1316），两处 openclaw 配置可能互相过期；
- **上游对 `stream:false` 也强制返回 SSE**（`content-type: text/event-stream`）——代理的非流式路径必须按 SSE 聚合（`_aggregate_sse`），上游也没有 `/v1/models` 端点（404），探活用最小 chat 请求；
- `auth_user.id` 可能是数字型（实测 81754），`installation_uuid` 可能是裸字符串——kvdb 读取均已兼容；
- **SKILL 会派生独立 node/python 服务**（命令行含 `\LobsterAI\SKILLs\...`），持有 SKILLs 目录句柄——guard 杀进程必须覆盖（`is_lobster_child_cmdline`），否则目录交换在 SKILLs 上失败。

## 实机踩坑记录（Windows Server 17763 本机）

1. **libsqlite3-sys bundled 编译失败**：mingw64 的 time.h 引缺失的 `pthread_time.h`。build.rs 给 cc 传 `CFLAGS=-D_STRICT_STDC` 绕过（时间函数仍可用），构建脚本已内置。
2. **tauri 2.12 系依赖树**（webview2-com-sys 0.39）测试二进制冷启动报 `STATUS_ENTRYPOINT_NOT_FOUND`——Cargo.lock 全套对齐 TraePlus 验证过的版本树（tauri 2.11.6 + webview2-com-sys 0.38.2）。
3. **GUI 层 feature-gated**（`gui` feature 默认开）：单测跑 `cargo test --lib --no-default-features`，只编译纯逻辑模块，避开 webview2 链接；测试与 CLI 共用同一 lib。
4. **spawn 子进程必须显式重定向 stdio**：proxy start / launch 不置 null 会继承父进程管道，CLI 调用方永不返回（python subprocess 挂起）。
5. **构建脚本 ASCII-only**：cmd 的 GBK 代码页下 UTF-8 中文注释会被当命令执行。

## 实机验证状态（2026-09-28）

| 功能 | 状态 |
|---|---|
| doctor 一键诊断 | ✅ 11 项全绿 |
| 收编（capture） | ✅ 快照 15 项 + 加密 blob |
| 全库签到（checkin） | ✅ 服务端返回"今日已签（累计 30 天）" |
| API 代理 OpenAI 非流式/流式 | ✅ SSE 聚合正确（含 reasoning_content） |
| API 代理 Anthropic /v1/messages | ✅ 协议转换 + 流式事件序列正确 |
| proxy start/stop | ✅ 不再挂起 |
| CC-Switch 注册（register） | ✅ apiFormat=anthropic、固定端口、备份+还原激活项 |
| 切换（switch --force） | ✅ 杀进程 → 15 项保鲜 → 15 项恢复 → 读回校验通过（27s） |
| A↔B 双账号往返 | ⏳ 待第二账号收编后验证 |
