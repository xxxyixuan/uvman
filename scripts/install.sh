#!/usr/bin/env bash
set -euo pipefail

# uvman — Linux/macOS 安装脚本
#
# 提供两种安装方式，由用户交互选择：
#   1) 自动下载二进制：从 GitHub Releases 下载当前平台最新版的 uvman，
#      安装到 ~/.local/bin（可按需修改），并提示写入 shell 配置；
#   2) 手动下载：打印 release 下载地址与放置说明，由用户自行下载并
#      放入 PATH 中的任一目录（如 /usr/local/bin）。
#
# 数据目录说明：
#   Linux/macOS 上 uvman 的数据根固定为 ~/.uvman，独立于二进制位置，
#   遵循类 Unix 约定，脚本不写入也无需设置 UVMAN_HOME。
#
# 用法：
#   bash install.sh                 # 交互式选择安装方式
#   bash install.sh --auto          # 跳过选择，直接自动下载安装
#   bash install.sh --version X.Y.Z # 指定版本号（默认最新 release）
#
# 环境变量：
#   UVMAN_BIN  安装目录（默认 ~/.local/bin）

REPO="xxxyixuan/uvman"
RELEASES_URL="https://github.com/${REPO}/releases"
API_LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"

# ---- 基础工具检测 ---------------------------------------------------------
say()  { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '\033[32m%s\033[0m\n' "$*"; }
warn() { printf '\033[33m%s\033[0m\n' "$*"; }
err()  { printf '\033[31m%s\033[0m\n' "$*" >&2; }

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "需要安装 '$1'，请先安装后再运行本脚本。"
        exit 1
    fi
}

# ---- 平台探测 -------------------------------------------------------------
detect_platform() {
    local os arch
    case "$(uname -s)" in
        Linux)  os="linux" ;;
        Darwin) os="darwin" ;;
        *) err "暂不支持的系统: $(uname -s)"; exit 1 ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64)  arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) err "暂不支持的架构: $(uname -m)"; exit 1 ;;
    esac
    printf '%s\n' "$arch-${os}"
}
# 注意：资产命名以实际发布为准，详见下方 pick_asset

# ---- 解析版本（最新 release 或用户指定）-----------------------------------
resolve_version() {
    if [[ -n "${UVMAN_VER:-}" ]]; then
        printf '%s\n' "$UVMAN_VER"
        return
    fi
    need_cmd curl
    # 解析 latest release 的 tag（如 v0.1.0 → 0.1.0）
    local tag
    tag="$(curl -fsSL "$API_LATEST_URL" | sed -n 's/.*"tag_name": *"\(v\?[^"]*\)".*/\1/p' | head -n1)"
    [[ -n "$tag" ]] || { err "无法获取最新版本号，请用 --version 指定。"; exit 1; }
    printf '%s\n' "${tag#v}"
}

# ---- 自动下载安装 ---------------------------------------------------------
install_auto() {
    local version="$1"
    local platform="$2"
    local bin_dir="${UVMAN_BIN:-$HOME/.local/bin}"

    need_cmd curl
    need_cmd unzip

    # 资产命名约定：uvman-<ver>-<plat>.zip（按需调整）
    local asset="uvman-${version}-${platform}.zip"
    local url="${RELEASES_URL}/download/v${version}/${asset}"
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    say "下载 $url"
    curl -fL --proto '=https' --tlsv1.2 -o "$tmp/$asset" "$url" ||
        { err "下载失败：可能是发布资产命名与约定不一致，请改用手动方式。"; exit 1; }

    say "解压到 $bin_dir"
    mkdir -p "$bin_dir"
    unzip -o -q "$tmp/$asset" -d "$tmp/extracted"
    if [[ -f "$tmp/extracted/uvman" ]]; then
        install -m 0755 "$tmp/extracted/uvman" "$bin_dir/uvman"
    elif [[ -f "$tmp/extracted/bin/uvman" ]]; then
        install -m 0755 "$tmp/extracted/bin/uvman" "$bin_dir/uvman"
    else
        err "压缩包内未找到 uvman 可执行文件。"
        exit 1
    fi

    ok "uvman ${version} 已安装到 ${bin_dir}"
    add_to_shell "$bin_dir"
    ok "运行 'uvman version' 验证安装。"
}

add_to_shell() {
    local bin_dir="$1"
    local rc
    # 已在 PATH 中的目录无需写入配置
    case ":$PATH:" in
        *":$bin_dir:"*) return ;;
    esac
    case "$(basename "${SHELL:-/bin/sh}")" in
        bash) rc="$HOME/.bashrc" ;;
        zsh)  rc="$HOME/.zshrc" ;;
        fish)
            if ! command grep -q "fish_add_path $bin_dir" "$HOME/.config/fish/config.fish" 2>/dev/null; then
                command mkdir -p "$HOME/.config/fish"
                printf 'fish_add_path %s\n' "$bin_dir" >> "$HOME/.config/fish/config.fish"
                ok "已追加 fish_add_path 到 $HOME/.config/fish/config.fish"
            fi
            return ;;
        *) return ;;
    esac
    if ! command grep -q "export PATH=\"$bin_dir:\$PATH\"" "$rc" 2>/dev/null; then
        printf '\nexport PATH="%s:$PATH"\n' "$bin_dir" >> "$rc"
        ok "已追加 PATH 到 $rc（重新打开终端生效）"
    fi
}

# ---- 手动下载提示 ---------------------------------------------------------
install_manual() {
    local version="$1"
    local platform="$2"
    ok "手动安装步骤："
    printf '  1) 打开 %s\n' "$RELEASES_URL"
    printf '  2) 下载资产: uvman-%s-%s.zip\n' "$version" "$platform"
    printf '  3) 解压，将 uvman 放入 PATH 中的任一目录，例如:\n'
    printf '     sudo install -m 0755 uvman /usr/local/bin/uvman\n'
    printf '  4) 运行 uvman version 验证。\n'
}

# ---- 入口 -----------------------------------------------------------------
main() {
    local mode="interactive"
    while (($#)); do
        case "$1" in
            --auto)    mode="auto" ;;
            --version) shift; UVMAN_VER="$1" ;;
            *) warn "忽略未知参数: $1" ;;
        esac
        shift
    done

    local platform
    platform="$(detect_platform)"
    local version
    version="$(resolve_version)"

    say "uvman 安装器 | 版本: $version | 平台: $platform"

    if [[ "$mode" == "auto" ]]; then
        install_auto "$version" "$platform"
        return
    fi

    # 交互选择
    printf '\n请选择安装方式：\n'
    printf '  1) 自动下载二进制到 ~/.local/bin （推荐）\n'
    printf '  2) 手动下载 release，自行放入 bin 目录\n'
    printf '  输入 1 或 2 [默认 1]: '
    read -r choice
    case "${choice:-1}" in
        1) install_auto "$version" "$platform" ;;
        2) install_manual "$version" "$platform" ;;
        *) warn "无效输入，默认自动安装。"; install_auto "$version" "$platform" ;;
    esac
}

main "$@"