# 更新日志（Changelog）

uvman 所有显著变更记录于此。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)；每个版本对应一个 GitHub Release，详见各版本链接。

## [v0.3.6](https://github.com/xxxyixuan/uvman/releases/tag/v0.3.6) — 2026-09-20

shims 改为全转发架构：删除「复制 + 自引用重写」机制，每个命令只生成一个 `.exe` 转发器，运行时按调用终端类型选择部署目录中的具体脚本形态执行。这修掉了 0.3.5 仍未覆盖的一整类脚本入口失效（`$basedir` 等 npm 生态自引用写法）。

### 破坏性变更

- 脚本类 shim 从同名脚本（`npm.cmd`/`npm.ps1`）改为统一的 `<命令>.exe` 转发器：`rehash` 后 `shims/` 下每个命令只有一个入口（命令调用方式不变）；迁移旧生成物过程：`shims status` / `doctor` 会把旧名报告为过期，`uvman shims rehash` 一次性清理重建

### Bug 修复

- 修复 npm 生态脚本入口失效（0.3.5 重写规则未覆盖）：`npm install pnpm -g` 生成的 `pnpm.ps1` 等使用 `$basedir=Split-Path $MyInvocation.MyCommand.Definition` 的自引用写法，复制到 `shims/` 后 `$basedir` 指向 `shims/`，`pnpm -v` 报 `E:\devtools\uvman\shims\node_modules\pnpm\pnpm.exe` 找不到。全转发后脚本在部署目录内执行，`$basedir`/`%~dp0`/`$PSScriptRoot` 天然正确
- 删除「复制 + 自引用重写」机制（含 BOM/UTF-8 修补），消除整类「重写规则永远差一种写法」的维护隐患

### 优化与改进

- `shims` 每命令单入口：`npm`/`npm.cmd`/`npm.ps1`/裸名 shebang 脚本归并为 `npm.exe`，cmd/PowerShell/git-bash 均能命中；无扩展名裸命令（shebang 脚本）也纳入命令集
- 转发器按终端类型选择部署入口：git-bash/MSYS（`OSTYPE`/`MSYSTEM`/`SHELL`）优先裸名脚本经 `sh` 运行、PowerShell（`PSModulePath`）优先 `.ps1`、cmd/GUI 按 `.exe`→`.cmd` 顺序；纯启发式，README 记录局限
- `rehash` 每轮必装 helper（脚本不再免 helper）；一致性校验统一为「命令任意形态存在即健康」

## [v0.3.5](https://github.com/xxxyixuan/uvman/releases/tag/v0.3.5) — 2026-09-20

修复 0.3.4 引入的 shims 脚本入口失效问题：`.cmd`/`.bat`/`.ps1` 复制到 `shims/` 后，脚本内的 `%~dp0` / `$PSScriptRoot` 会解析到 `shims/` 而非真实部署目录，导致 `npm --version` 等命令报 `Cannot find module ... npm-prefix.js`。现在生成脚本入口时会把这类自引用重写到部署目录的绝对路径。

### Bug 修复

- 修复 0.3.4 起脚本类 shim 全面失效的问题：`npm --version` 报 `Cannot find module '<shims>\node_modules\npm\bin\npm-prefix.js'`、`Could not determine Node.js install directory`。根因是脚本入口按「逐字节复制」生成，但其 `%~dp0` / `$PSScriptRoot` 是相对于脚本自身位置的，复制到 `shims/` 后全部指向错误目录
- 脚本入口改为「复制 + 自引用重写」：`rehash` 生成 `.cmd`/`.bat`/`.ps1` 入口时，把自引用 token 替换为部署目录绝对路径（`%~dp0\node_modules\...` → `E:\...\tools\node\<ver>\node_modules\...`），其余字节（CRLF、编码、注释）原样保留
- 重写规则按「实际会解析的相对引用」精确匹配，避免误伤：批处理只重写后接路径片段或 `%VAR%` 跳转的 `%~dp0`；PowerShell 只重写直接拼路径的裸 `$PSScriptRoot`（`"$PSScriptRoot\node"`），保留 `Join-Path $PSScriptRoot` / `$PSScriptRoot + '\'` / `$env:PSScriptRoot` 等显式全局前缀用法
- 分隔符按脚本原样保留：`%~dp0/x` 保持 `/`（npm 的 `npm.ps1` 依赖此形式），`%~dp0\x` 保持 `\`，不产生混用分隔符的路径

### 优化与改进

- 一致性校验升级：脚本入口的「漂移」判定从「与部署源逐字节一致」改为「与当前重写结果一致」，因此旧版本（0.3.4）生成的失效副本会被 `uvman shims status` / `doctor` 识别为过期并提示 `uvman shims rehash`
- `uvman-shim` 转发器补全解释器间接调用（满足「转发 cmd / ps1 / bash 等脚本命令」的契约）：Windows 下 `.cmd`/`.bat` 走 `cmd.exe /c`、`.ps1` 走 `pwsh`（回退 `powershell.exe`）`-NoProfile -ExecutionPolicy Bypass -File`；Unix 下 `.sh`/`.zsh`/`.fish` 等内核无法直接启动的脚本走对应解释器，裸名与 shebang 脚本仍由内核直接执行
- 新增 `UVMAN_SHIM_AS` 环境变量，可覆盖转发器自身识别到的命令名（仅供测试与手工调试，正式布局不使用）

## [v0.3.4](https://github.com/xxxyixuan/uvman/releases/tag/v0.3.4) — 2026-09-20

`shims/` 入口按源文件类型分两种生成方式：可执行程序生成转发器，脚本文件（Windows `.ps1`/`.cmd`/`.bat`）逐字节复制原文，脚本相对路径依赖在 shims 内原位解析；一致性校验同时覆盖两类入口。

### 新增特性

- shims 按文件类型分类生成入口：可执行程序（Windows `.exe`、Unix 无扩展名二进制）生成 `uvman-shim` 转发器（透传参数与退出码，与 `which` 同源解析）；脚本文件（`.ps1`/`.cmd`/`.bat`）逐字节复制工具自带原始脚本到 `shims/`，不经转发（[b322236](https://github.com/xxxyixuan/uvman/commit/b322236)）

### 优化与改进

- 脚本入口必须「复制原文」而非包一层转发：脚本普遍用 `%~dp0` / `$PSScriptRoot` 定位同级依赖（如 `npm.cmd` 紧邻 `node.exe` 与 `node_modules/`），转发会改变脚本所在目录致相对路径失效
- 一致性校验覆盖两类入口：`rehash` / `status` / `doctor` 检查转发器「是否仍能解析到激活版本真实二进制」、脚本入口「与部署源是否逐字节一致」，任一漂移提示 `uvman shims rehash`（[b322236](https://github.com/xxxyixuan/uvman/commit/b322236)）

## [v0.3.3](https://github.com/xxxyixuan/uvman/releases/tag/v0.3.3) — 2026-09-16

uvman-shim 转发逻辑内联为无依赖实现、构建更稳定；release 构建针对二进制体积优化。

### 新增特性

- `uvman-shim` 自带 std-only 转发查找副本，不再依赖 core 模块：`shims/` 下每个转发器保持零依赖，转发逻辑与 `which`/`doctor` 的 core 解析保持同步（通过 shim 内测试守约）（[585c18b](https://github.com/xxxyixuan/uvman/commit/585c18b)）

### 优化与改进

- release 构建优化：`opt-level = "z"` + 全程序 LTO + `strip`，主二进制与每个 shim 副本体积显著缩小（shim 每命令一份拷贝，效果随管理的命令数放大）（[1077b4b](https://github.com/xxxyixuan/uvman/commit/1077b4b)）

## [v0.3.2](https://github.com/xxxyixuan/uvman/releases/tag/v0.3.2) — 2026-09-13

`uvman list <tool> --remote` 支持按插件的 `display_pattern` 干净显示远程版本；插件文件统一迁移到插件仓库的 `plugins/` 子目录。

### 新增特性

- `uvman list <tool> --remote`：api 插件可配置 `display_pattern`，仅用于列表展示的版本串转换（如 Azul 的 `26.32.203-ca-jdk26.0.2.1` 显示为 `26.0.2.1`），安装/切换仍使用完整原始版本（[22ce698](https://github.com/xxxyixuan/uvman/commit/22ce698)）

### 优化与改进

- 插件仓库目录结构化：插件 `.toml` 文件统一存放于仓库 `plugins/` 子目录，`plugin install` 与远程插件名索引均指向该子目录（[f8e7f0b](https://github.com/xxxyixuan/uvman/commit/f8e7f0b)）

## [v0.3.1](https://github.com/xxxyixuan/uvman/releases/tag/v0.3.1) — 2026-09-13

`install` 新增 `add` 别名，修复 Windows 注册表 PATH 写入的 UTF-16 编码问题，附带依赖更新与帮助文本精简。

### 新增特性

- `uvman add <tool>@<version>`：`install` 新增可见别名 `add`（与既有 `i` 别名并列），习惯用 `add` 安装的用户可直接使用（[3a786f2](https://github.com/xxxyixuan/uvman/commit/3a786f2)）

### 优化与改进

- 精简各命令与子命令的帮助文本，`--help` 输出更紧凑易读（[8d067d5](https://github.com/xxxyixuan/uvman/commit/8d067d5)）
- 更新依赖版本（[b6554b2](https://github.com/xxxyixuan/uvman/commit/b6554b2)）

### Bug 修复

- 修复 Windows 下写入注册表 PATH 时值字节未显式按 UTF-16 转换的问题，避免编码错误导致 PATH 值异常（[a3d9b02](https://github.com/xxxyixuan/uvman/commit/a3d9b02)）

## [v0.3.0](https://github.com/xxxyixuan/uvman/releases/tag/v0.3.0) — 2026-09-13

Shims 与 GUI/IDE 场景：GUI 进程（IDEA、VS Code 等继承 Explorer 环境、感知不到 `activate` 的进程）经 shims 转发目录解析 uvman 管理的工具，`use` 切版本零 PATH 变更，两个场景看到同一激活状态。

### 新增特性

- `uvman-shim` 第二编译目标：按命令名生成的转发器（`<UVMAN_HOME>\shims\<cmd>`），GUI/IDE 进程无需 `activate` 即可解析 uvman 管理的工具；转发与 `which` 同源（共享 `core::resolve` 与可执行定位规则），未激活/目标缺失给出可操作错误
- `uvman shims <enable|disable|status|rehash>`：管理用户 PATH 中的 shims 目录。`enable` 仅 Windows 写入 `HKCU\Environment\Path`（备份先行、`REG_EXPAND_SZ` 类型保真、广播 `WM_SETTINGCHANGE`，幂等），Unix 降级为 shell profile 手动提示；`disable` 仅移除 uvman 自有条目；`status` 报告目录/一致性/系统工具遮蔽；`rehash` 幂等重建（manifest 驱动清理，不删用户手放文件）
- `install` / `uninstall` / `use` 成功后自动 rehash，保持 shim 与激活状态同步（静默尽力而为，不阻断命令）
- `doctor` 新增 shims 检查项：目录与生成一致性、用户 PATH 接线（Windows）、可复制修复命令

### 优化与改进

- crate 新增 `[lib]` 目标，主二进制瘦身为薄入口，`uvman-shim` 仅共享 core（不引入 clap/网络/UI）

## [v0.2.0](https://github.com/xxxyixuan/uvman/releases/tag/v0.2.0) — 2026-09-06

查询命令定稿：新增只读查询命令 `current` / `which`，版本解析收敛为 `core` 单一入口，为 0.5.0 项目级作用域预埋接口。

### 新增特性

- `uvman current [tool] [-J/--json]`：只读输出当前激活版本，无参数列出全部工具；人类可读输出 `node  22.19.0 (global)`，`--json` 输出 `{ "node": { "version": "22.19.0", "scope": "global" } }`；无激活版本输出 `none` 并以 0 退出，JSON 为空对象（[7eb14da](https://github.com/xxxyixuan/uvman/commit/7eb14da)）
- `uvman which <tool>`：输出当前激活版本可执行文件的绝对路径（供脚本定位实际二进制）；解析链为激活版本 → `tools/<tool>/<version>/`（含 `bin/`），Windows 依次尝试 `.exe/.cmd/.bat/.ps1`、Unix 尝试裸文件名；无法定位时明确报错并提示 `uvman install <tool>@<version>` 重新部署（[8e74837](https://github.com/xxxyixuan/uvman/commit/8e74837)）

### 优化与改进

- 版本解析收敛为 `core::resolve` 单一入口（返回 `(version, scope)`，scope 本版本恒为 `global`），`current` / `which` / `env` 三个查询命令均经该入口；0.5.0 项目级作用域落地时只需替换入口的查找实现，命令无需改动（[#20](https://github.com/xxxyixuan/uvman/pull/20)）
- 激活版本目录被手工删除时，只读命令一致按「无此版本」处理且不修复状态：`current` 跳过并输出 `none`（`--json` 为空对象）、`which` 报错提示重新部署（[#21](https://github.com/xxxyixuan/uvman/pull/21)）

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
