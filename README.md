# uvman

UVMAN — 一款用 Rust 编写的**通用开发工具版本管理器** CLI。

`uvman` 帮助你安装、切换和管理多种开发工具（Node、Python、Go……）的多个版本。所有工具通过**插件系统**驱动，插件用 TOML 描述工具的下载来源与安装方式，新增工具无需改代码。

```bash
uvman install node@22.19.0   # 安装指定版本
uvman use node@22            # 切换到已安装版本
eval "$(uvman env)"          # 把当前版本注入 shell（写入 ~/.bashrc）
```

## 特性

- **多工具管理**：任意开发工具，通过 TOML 插件扩展
- **版本解析**：支持具体版本 `node@20.11.0`、部分版本 `node@22`、代号 `node@lts` / `node@latest`
- **shell 集成**：`uvman env` 输出 export 语句，`uvman activate` 实现 mtime 快速路径的提示钩子自动刷新
- **跨平台**：Windows (cmd/pwsh)、Linux/macOS (bash/zsh/fish)
- **缓存与校验**：下载缓存（TTL 控制）、SHA-256 校验、原子写入、HTTP 重试
- **更新检查**：`uvman version` 可输出运行程序版本（含最新版本检查预留）

## 安装

### Linux / macOS — 一键脚本

```bash
curl -fsSL https://raw.githubusercontent.com/xxxyixuan/uvman/main/scripts/install.sh | bash
```

脚本提供两种安装方式供选择：

1. **自动下载二进制**：下载当前平台的最新 release 到 `~/.local/bin`（并在交互中提示写入 shell 配置）；
2. **手动下载**：脚本打印 release 下载地址与放置说明，由你自行下载并放入 PATH 中任一目录（如 `/usr/local/bin`）。

### Windows — PowerShell 脚本

```powershell
powershell -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/xxxyixuan/uvman/main/scripts/install.ps1 | iex"
```

脚本会引导你选择安装目录（默认 `%USERPROFILE%\.uvman\bin`），下载 release 并自动将该目录加入用户环境变量 `PATH`。

### 手动安装

到 [Releases](https://github.com/xxxyixuan/uvman/releases) 下载对应平台/架构的压缩包，解压后将 `uvman`（Windows 为 `uvman.exe`）放入 PATH 中的任意目录即可，无需额外依赖。

> uvman 仅通过 GitHub Release 分发，不发布到 crates.io。

### 从源码构建

```bash
git clone https://github.com/xxxyixuan/uvman.git
cd uvman
cargo build --release
cp target/release/uvman ~/.local/bin/   # 或放入你 PATH 中的任意目录
```

## 快速上手

```bash
# 1. 安装 Node（默认版本）
uvman install node

# 2. 查看已安装 / 远端版本
uvman list                 # 本地已安装的全部工具与版本
uvman list node --remote   # 远端可用的 node 版本（最新在最后）

# 3. 切换并使用
uvman use node@22          # 写入当前版本状态
eval "$(uvman env)"        # 注入 UVMAN_NODE_HOME / PATH

# 4. 让每次打开终端自动生效（bash/zsh，写入 ~/.bashrc 或 ~/.zshrc）
eval "$(uvman env)"
```

## 命令参考

| 命令 | 说明 |
| --- | --- |
| `uvman install <tool>@<version>` | 安装工具（别名 `i`），`--force` 强制重装 |
| `uvman list [tool]` | 列出本地已安装版本；`--remote` 列出远端可用版本，`--json` 输出 JSON |
| `uvman use <tool>@<version>` | 切换当前使用版本（别名 `u`），需已安装 |
| `uvman env [tool]` | 输出 shell export 语句，注入版本相关环境变量与 PATH |
| `uvman activate` | 输出激活脚本，通过提示钩子自动刷新（bash/zsh/fish/pwsh） |
| `uvman plugin <cmd>` | 插件管理：`install/uninstall/list/upgrade/info/sync/create` |
| `uvman version [--json]` | 显示 uvman 版本信息（别名 `v`，也支持 `-V`） |

## Shell 集成

- **bash / zsh**：`eval "$(uvman env)"`
- **fish**：`uvman env | source`
- **pwsh**：`uvman env | iex`
- **cmd**：`for /f "delims=" %i in ('uvman env --shell cmd') do %i`

`uvman env` 会先清理已失效的 `UVMAN_*` 变量，再输出当前激活工具的环境变量与 PATH 前缀。

### 自动刷新（激活模式）

在 shell 配置中追加（放在最后，避免覆盖自定义提示函数）：

```bash
eval "$(uvman activate)"   # bash/zsh
uvman activate | source    # fish
uvman activate | Out-String | Invoke-Expression  # pwsh
```

激活后，`uvman use` 的改动会在下一个提示符自动生效，无需手动刷新。`cmd` 无提示钩子，需使用 AutoRun 启动注入。

## 数据目录

uvman 的数据根目录按平台约定：

- **Windows**：数据与二进制同目录（便携式布局），`uvman.exe` 放哪，`tools/cache/config` 就建在哪，整体移动即可迁移。可用环境变量 `UVMAN_HOME` 覆盖为其他位置。
- **Linux/macOS**：固定为 `~/.uvman`，独立于二进制位置，遵循类 Unix 约定。

```
~/.uvman/
├── config/          # 配置文件（setting.toml、tool_current.toml）
├── tools/<tool>/<version>/   # 已安装的工具版本
├── plugins/         # TOML 插件
├── cache/           # 下载缓存与版本缓存
└── logs/
```

## 开发

```bash
cargo test        # 运行单元测试
cargo clippy      # 代码质量检查
```

## License

[MIT](LICENSE)