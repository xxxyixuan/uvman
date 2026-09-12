# uvman

> A unified CLI for installing, switching, and managing multiple versions of development tools.

[![Version](https://img.shields.io/github/v/release/xxxyixuan/uvman?style=flat-square)](https://github.com/xxxyixuan/uvman/releases)
[![License: MIT](https://img.shields.io/github/license/xxxyixuan/uvman?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?style=flat-square)](https://www.rust-lang.org)

---

## 简介

`uvman` 是一个轻量级、插件化的开发工具版本管理器。它统一了 Node.js、Python、Go、Rust
等主流开发工具的安装与切换流程，让你用同一套命令管理所有工具的多个版本——下载、解压、路径注入、版本切换，一行命令搞定。

```bash
# 1. 安装 Node.js plugin
uvman plugin install node
# 2. 安装使用 Node.js 22.19.0
uvman use node@22
```

不再需要在不同版本管理器之间来回切换，不再需要手动配置 PATH。

## 特性

- **统一命令**：一套 `install` / `use` / `list` 管理所有开发工具
- **插件驱动**：通过 TOML 声明每个工具的下载源、版本接口、平台映射，新增工具只需写配置，无需修改核心代码
- **自动激活**：Shell 提示符钩子自动注入版本环境，切换后下一条命令即刻生效
- **GUI/IDE 场景**：`shims` 转发目录让 IDEA、VS Code 等图形化进程也能解析 uvman 管理的工具，`use` 切版本零 PATH 变更
- **安全安装**：下载校验 SHA-256，强制重装先备份再操作，失败自动回滚
- **便携部署**：Windows 下数据与二进制同目录，整体移动即迁移
- **Rust 编写**：高性能、低内存占用，启动速度毫秒级

## 演示

在同一台机器上安装并切换多个 Node 版本：

```
$ uvman install node@22.19.0
Installing node@22.19.0 ...
Installed node@22.19.0 to ~/.uvman/tools/node/22.19.0

$ uvman use node@20
Switched node 22.19.0 → 20.19.2

$ node -v
v20.19.2

$ uvman use node@22
Switched node 20.19.2 → 22.19.0

$ node -v
v22.19.0
```

同时管理 Python、Go 等多种工具：

```
$ uvman install python@3.12
$ uvman install go@1.23
$ uvman list
go:
 - 1.23.4
node:
 - 20.19.2
 - 22.19.0
python:
 - 3.12.8
```

## 快速上手

### 安装

**Linux / macOS（一键脚本）：**

```bash
curl -fsSL https://raw.githubusercontent.com/xxxyixuan/uvman/main/scripts/install.sh | bash
```

**Windows（PowerShell）：**

```bash
powershell -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/xxxyixuan/uvman/main/scripts/install.ps1 | iex"
```

**手动安装：**

1. 前往 [Releases](https://github.com/xxxyixuan/uvman/releases) 下载对应平台的二进制文件
2. 或将源码克隆后自行编译：`cargo build --release`
3. 将生成的 `uvman` 放入 PATH 即可

### 接入 Shell

在 Shell 配置末尾追加激活语句（自动刷新当前版本）：

- bash

```bash
echo 'eval "$(uvman activate)"' >> ~/.bashrc
```

- zsh

```bash
echo 'eval "$(uvman activate)"' >> ~/.zshrc
```

- fish

```bash
echo 'uvman activate | source' >> ~/.config/fish/config.fish
```

- PowerShell

```bash
echo 'uvman activate | Out-String | Invoke-Expression' >> $PROFILE
```

- cmd

```bash
# 使用 AutoRun 启动注入，或手动刷新
```

启用后，每次 `uvman use` 切换都会在下一条提示符自动生效，无需手动 `eval` 或重开终端。

### GUI / IDE 场景（可选）

Shell 激活只作用于会话内；IDEA、VS Code 等图形化进程继承 Explorer（注册表）环境，感知不到 `activate` 注入。如需让它们使用 uvman 管理的版本，启用 shims：

```bash
$ uvman install node@lts        # 先安装工具
$ uvman use node@lts            # 激活版本（`use` 永不触碰注册表）
$ uvman shims enable            # 把 <UVMAN_HOME>\shims 写入用户 PATH（仅 Windows 自动写入）
```

启用后**重启已运行的程序（含 IDEA）**生效；之后 `use` 切换版本会自动重建转发器，GUI 与 shell 看到同一激活状态。

- `uvman shims status` 查看转发器与激活版本是否一致、系统工具遮蔽等
- `uvman shims rehash` 手动重建；`uvman shims disable` 移除 PATH 中的 shims 条目
- Linux / macOS 上 `uvman shims enable` 只打印 shell profile 提示（不自动改 profile），或继续使用 `activate`

### 使用

```bash
$ uvman install node@22          # 安装指定版本
$ uvman install node@latest      # 安装最新版
$ uvman install node@lts         # 安装 LTS 版
$ uvman use node@22              # 切换版本
$ uvman uninstall node@20        # 卸载指定版本
$ uvman uninstall node           # 卸载整个工具
$ uvman doctor                   # 环境自检
$ uvman list                     # 查看所有已安装工具与版本
$ uvman list node --remote       # 查看远端可用版本
$ uvman shims enable             # 让 GUI/IDE 使用 uvman 管理版本（Windows）
$ uvman shims status             # 查看 shims 一致性
```

> `uvman use` 与 `uvman install` 一次操作一个工具，如需管理多个工具逐个执行即可。

## 命令参考

| 命令                               | 说明                                              |
|----------------------------------|-------------------------------------------------|
| `uvman install <tool>@<version>` | 安装工具（别名 `i`），`--force` 强制重装、失败自动回滚              |
| `uvman uninstall <tool>[@<version>]` | 卸载单个版本（支持 `22` / `latest` 等写法）或整个工具；卸载当前激活版本时自动移除激活记录 |
| `uvman list [tool]`              | 列出本地已安装版本；`--remote` 列远端版本、`--json` 输出 JSON     |
| `uvman use <tool>@<version>`     | 切换当前使用版本（别名 `u`），需已安装                           |
| `uvman current [tool]`           | 查询当前激活版本（只读）：无参数列出全部，`--json` 输出结构化结果 |
| `uvman which <tool>`             | 输出当前激活版本可执行文件的绝对路径（供脚本定位实际二进制） |
| `uvman env`                      | （内部命令）`activate` 的后台求值器，不在 `help` 中显示，`--shell` 指定语法 |
| `uvman activate`                 | 输出激活脚本，通过提示符钩子自动刷新（支持 bash / zsh / fish / pwsh） |
| `uvman shims <enable\|disable\|status\|rehash>` | 管理 GUI/IDE 场景的转发器：`enable` 把 `shims/` 目录写入用户 PATH（Windows 注册表，备份+类型保真+广播；Unix 打印 profile 提示），`disable` 移除自有条目，`status` 报告一致性，`rehash` 幂等重建转发器 |
| `uvman doctor`                   | 环境自检：`UVMAN_HOME` 布局、配置可解析、插件完整性、shell 激活状态、shims 一致性；`--json` 输出结构化报告，有检查失败时退出码为 1 |
| `uvman plugin <cmd>`             | 插件管理：`install` / `uninstall` / `list` / `info`  |
| `uvman version`                  | 显示版本信息（别名 `v`，`-V` 可用），`--json` 输出结构化信息         |

## 工作原理

### 插件即配置

每个工具对应一份 `<tool>.toml` 插件文件，声明元信息、镜像源、版本接口（API 拉取或静态列表）、平台映射与安装方式。

```
uvman plugin install node   # 从插件仓库拉取
uvman plugin install mytool --path ./mytool.toml   # 安装本地自定义插件
```

### 安装流程

1. **定位版本**：通过插件声明的版本接口获取下载地址
2. **下载校验**：下载文件并校验 SHA-256 摘要
3. **解压部署**：解压到 `tools/<tool>/<version>/` 目录
4. **安全回滚**：`--force` 重装时先备份旧目录，失败自动恢复

### 切换与生效

1. `uvman use` 写入 `config/tool_current.toml` 记录当前激活状态
2. `uvman env` 读取该文件并输出 `UVMAN_<TOOL>_HOME` + PATH 前插语句
3. `uvman activate` 将环境注入挂在 Shell 提示符钩子上，自动保持最新

### GUI 场景：shims

图形化进程继承 Explorer（注册表）环境，看不到 `activate` 的会话注入。解法是转发器（shim）：用户 PATH 中只放**一个稳定目录** `shims/`，目录内是按命令名生成的转发器（`node.exe` / `npm.cmd` …）。调用时经统一解析入口（`core::resolve`）定位激活版本的真实二进制并透传参数与退出码——与 `which` 完全同源。

```text
GUI 进程 → shims\node.exe → 解析入口（全局激活）→ tools\node\22.19.0\node.exe
```

- `install` / `uninstall` / `use` 后自动重建（rehash）；`shims/` 内的 manifest 只清理 uvman 自己生成的文件
- `use` 切换版本**永不**触碰注册表；注册表写入只发生在 `shims enable` / `disable`（HKCU，备份先行、类型保真、广播 `WM_SETTINGCHANGE`）
- 边界：shim 转发不注入 `[env]` 自定义环境变量（GUI 进程继承 Explorer 环境，属已知边界）；`activate` 的会话刷新只剥离 `tools/` 前缀条目，不影响 `shims/` 条目

## 数据布局

数据根目录按平台约定不同：

- **Windows**：数据与二进制同目录（便携布局），整体移动即迁移；也可通过 `UVMAN_HOME` 环境变量覆盖
- **Linux / macOS**：固定为 `~/.uvman`，独立于二进制位置

```
~/.uvman/
── config/
│   ├── uvman.toml        # 全局配置（插件仓库 / 镜像 / 网络 / 缓存 TTL）
│   ── tool_current.toml # 当前激活版本状态
── tools/<tool>/<version>/   # 已安装的工具版本
── shims/                # 按命令名生成的转发器（GUI/IDE 场景，PATH 稳定目录）
│   ── .uvman/           # 内部 helper 与 manifest（用户无需关心）
── plugins/              # 已安装的 TOML 插件
── cache/                # 下载缓存与远端版本缓存（TTL 控制）
── backup/               # shims enable/disable 写入前的用户 PATH 备份
── logs/
```

首次运行自动生成 `config/uvman.toml`，其中 `[plugin] repo` 指向插件仓库，可按需配置网络代理、镜像源与缓存 TTL。

## 开发

### 环境要求

- Rust 1.85+
- Cargo

### 构建与测试

```bash
git clone https://github.com/xxxyixuan/uvman.git
cd uvman

cargo build              # 编译
cargo test               # 运行单元测试
cargo clippy             # 代码质量检查
cargo fmt                # 代码格式化
```

### 项目结构

| 路径             | 说明                             |
|----------------|--------------------------------|
| `src/cli/`     | 子命令定义与入口（含 `shims` 命令集）          |
| `src/core/`    | 配置、路径、插件、平台、解析、Shell 渲染、shims（转发/rehash）、pathstore（用户 PATH）、HTTP、错误处理 |
| `src/toolset/` | 安装计划与执行、版本解析                   |
| `src/ui/`      | 颜色输出与报告                        |
| `src/bin/uvman-shim.rs` | 转发器二进制（GUI/IDE 场景，仅共享 core）    |

### 贡献指南

欢迎提交 Issue 和 Pull Request！

1. **Fork** 本仓库
2. 在特性分支上开发：`git checkout -b feat/my-feature`
3. 提交变更：`git commit -m "feat: add xxx"`
4. 推送分支：`git push origin feat/my-feature`
5. 提交 **Pull Request**（目标分支为 `dev`）

维护者会将 PR squash 合并到 `dev` 分支，定期发布到 `main`。

## 许可证

[MIT](LICENSE) © yixuan
