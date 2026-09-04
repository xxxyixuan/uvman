# 更新日志（Changelog）

uvman 所有显著变更记录于此。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)；每个版本对应一个 GitHub Release，详见各版本链接。

## [v0.1.7](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.7) — 2026-09-04

裸命令打印帮助，hint 显式标注可运行命令。

### 新增特性

- 裸 `uvman`（不带任何子命令）向 stdout 打印完整帮助并以 0 退出，不再静默结束（[#18](https://github.com/xxxyixuan/uvman/pull/18)）

### 优化与改进

- hint 输出：建议命令渲染为 `you can run "<cmd>"` 的可运行行，用户一眼可知该命令可直接复制执行；同步精简 self-update、use、plugin 等命令 hint 消息中冗余的 run: 措辞（[#17](https://github.com/xxxyixuan/uvman/pull/17)）

## [v0.1.6](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.6) — 2026-08-30

新增 `uninstall` 与 `doctor` 命令，`install` 的「重建已装版本」改为原子化（失败可回滚），并完成一轮零成本抽象重构。

### 新增特性

- `uvman uninstall <tool[@version]>`：移除单个已装版本或整个工具；移除触及当前激活版本时自动回滚激活记录；支持部分版本与别名定位（[#15](https://github.com/xxxyixuan/uvman/pull/15)）
- `uvman doctor`：环境自检，覆盖 `UVMAN_HOME` 布局、全局配置可解析性、插件目录完整性、Shell 激活状态；支持 `--json`，任一检查失败退出码为 1（[#15](https://github.com/xxxyixuan/uvman/pull/15)）

### 优化与改进

- install：重建已装版本改为原子化替换（`ReplaceGuard`），先移开旧版本、成功后提交、失败时回滚（[#15](https://github.com/xxxyixuan/uvman/pull/15)）
- 零成本抽象重构：消除 `expect`/`unwrap`/索引 panic 路径（HTTP 客户端优雅降级）、迭代器优先重写并削减分配、新增在构造时校验与归一化的 `HexDigest` 类型（[#16](https://github.com/xxxyixuan/uvman/pull/16)）

## [v0.1.5](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.5) — 2026-08-30

`list` 命令基于 ratatui 重写为交互式 TUI 查看器。

### 新增特性

- 重构 `list` 命令输出，统一为基于 ratatui 的交互式界面；长 `--remote` 列表分页浏览，支持前缀匹配搜索与左右翻页，退出查看器时回显当前停留页面（[#14](https://github.com/xxxyixuan/uvman/pull/14)）

## [v0.1.4](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.4) — 2026-08-29

新增 `activate` / `env` 激活脚本与 `use` 版本切换，工具版本自动同步到当前 shell 环境。

### 新增特性

- `uvman activate`：按 shell（bash / zsh / fish / pwsh）生成激活脚本并注册提示符钩子，基于状态文件 mtime 快速路径检测变更，`uvman use` 在下一次提示符自动生效（mise 风格）（[#12](https://github.com/xxxyixuan/uvman/pull/12)）
- 隐藏命令 `uvman env`：激活脚本的后端，仅输出 shell 语句供钩子求值；清理过期 `UVMAN_*` 变量，按需导出工具版本/安装目录变量，并将各工具 `bin/` 前置到 PATH（[#12](https://github.com/xxxyixuan/uvman/pull/12)）
- `uvman use <tool>@<version>`：在已安装版本间切换，支持部分版本与别名解析；切换写入激活状态表，未激活会话给出一次性求值提示（[#12](https://github.com/xxxyixuan/uvman/pull/12)）

## [v0.1.3](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.3) — 2026-08-29

新增 `uvman self-update` 自升级命令。

### 新增特性

- `uvman self-update`：核对 GitHub 最新版本 → 交互确认 → 下载资产并校验 SHA-256 → 替换正在运行的二进制；支持 `--check` / `--yes` / `--prerelease` / `--json`（[#10](https://github.com/xxxyixuan/uvman/pull/10)）
- Windows 采用 rename-to-delete 模式替换运行中的 exe（旧二进制重命名为 `uvman.exe.old`，残留文件下次启动自动清理）；Unix 通过原子 rename 替换并保留可执行权限位；`UVMAN_BIN` 环境变量可重定向安装目标（[#10](https://github.com/xxxyixuan/uvman/pull/10)）

### 优化与改进

- `uvman version` 升级提示改为 hint 样式并建议 `uvman self-update`；版本核对逻辑从 `cli/version.rs` 下沉到 `core/upgrade.rs`，消除重复代码（[#10](https://github.com/xxxyixuan/uvman/pull/10)）

## [v0.1.2](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.2) — 2026-08-29

插件系统重构：支持从本地路径安装自定义插件，精简插件子命令。

### 新增特性

- `uvman plugin install <tool> --path <file>`：从本地 TOML 文件安装自定义插件（[#8](https://github.com/xxxyixuan/uvman/pull/8)）

### 优化与改进

- 精简插件子命令，简化插件错误类型，代理失败提示改为引导在 config/uvman.toml 中配置（[#8](https://github.com/xxxyixuan/uvman/pull/8)）
- 配置 `.github/release.yml`，按 PR label 自动分类 Release Notes（[#7](https://github.com/xxxyixuan/uvman/pull/7)）

### 破坏性变更

- 移除 `plugin upgrade` / `sync` / `create` 子命令；代理无法再通过 `sync --proxy` 指定，请改在 config/uvman.toml 中配置

## [v0.1.1](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.1) — 2026-08-26

`version` 命令体验优化与代码注释规范化。

### 优化与改进

- `version` 命令输出增加启动横幅与升级提示，便于查看最新可用版本；缩短升级检查超时（[#5](https://github.com/xxxyixuan/uvman/pull/5)）
- 清理 version 相关冗余配置，移除测试配置文件 `test/config/uvman.toml`；更新 cli / core / toolset / ui 等模块代码注释（[#5](https://github.com/xxxyixuan/uvman/pull/5)）

## [v0.1.0](https://github.com/xxxyixuan/uvman/releases/tag/v0.1.0) — 2026-08-17

首个正式版本：基于 TOML 插件系统的通用开发工具版本管理器 CLI，新增工具无需修改代码。

### 新增特性

- 核心框架：`UError` 错误处理域与 hint 修复建议、支持代理/重试/原子写入/进度条的 HTTP 客户端、`UVMAN_HOME` 目录布局与全局配置自举（[#1](https://github.com/xxxyixuan/uvman/pull/1)、[#3](https://github.com/xxxyixuan/uvman/pull/3)）
- 插件系统：基于 TOML 的 `ToolPlugin` 数据模型与模板渲染；`uvman plugin` 子命令（install / uninstall / list / info 等）（[#2](https://github.com/xxxyixuan/uvman/pull/2)）
- 工具管理：`install`（下载 → SHA-256 校验 → 解压 → 部署，TTL 缓存）、`list`（本地/远端，`--json`）、`use` 切换、`env` / `activate` shell 集成（bash / zsh / fish / pwsh / cmd）；版本解析支持具体版本、部分版本（`node@22`）与别名（`node@lts` / `node@latest`）（[#3](https://github.com/xxxyixuan/uvman/pull/3)）
- UI 与发布：语义化颜色与统一诊断输出，`--verbose` / `--quiet` / `NO_COLOR`；GitHub Actions 打 tag 自动构建多平台 Release（[#1](https://github.com/xxxyixuan/uvman/pull/1)）
