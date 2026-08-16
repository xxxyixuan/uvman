<#
Windows 版 uvman 安装脚本
用法（以管理员权限运行，或将网购命令放入普通终端）：
  powershell -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/xxxyixuan/uvman/main/scripts/install.ps1 | iex"

行为：
  - 引导用户选择安装目录（默认 %USERPROFILE%\.uvman\bin）
  - 从 GitHub Releases 下载当前平台最新版 uvman 压缩包并解压到该目录
  - 将该目录加入【用户】PATH（无需管理员权限的当前用户环境变量）
  - 提示重启终端后生效

数据目录说明：
  Windows 上 uvman 采用便携式布局——数据根即 uvman.exe 所在目录。
  因此「安装目录」既是二进制所在处，也是 tools/cache/config 的存放处；
  如需指定不同数据位置，可另行设置 UVMAN_HOME 环境变量。
  脚本不写入 UVMAN_HOME，力求开箱即用。

参数：
  -InstallDir <path>   自定义安装目录
  -Version <x.y.z>     指定版本号（默认最新 release）
  -Arch <x64|arm64>    指定架构（默认自动探测）
#>
[CmdletBinding()]
param(
    [string]$InstallDir = "",
    [string]$Version = "",
    [string]$Arch = ""
)

$ErrorActionPreference = "Stop"

$Repo      = "xxxyixuan/uvman"
$Releases  = "https://github.com/${Repo}/releases"
$ApiLatest = "https://api.github.com/repos/${Repo}/releases/latest"

function Write-Info  { Write-Host $args[0] -ForegroundColor Cyan }
function Write-Ok    { Write-Host $args[0] -ForegroundColor Green }
function Write-Warn  { Write-Host $args[0] -ForegroundColor Yellow }
function Write-Fail  { Write-Host $args[0] -ForegroundColor Red }

# ---- 前置：当前用户 PATH（避免覆盖其他路径）--------------------------------
function Get-UserPath {
    $regPath = "HKCU:\Environment"
    $val = (Get-ItemProperty -Path $regPath -Name Path -ErrorAction SilentlyContinue).Path
    if (-not $val) { return "" }
    return $val.TrimEnd(';')
}

function Set-UserPath {
    param([string]$NewPath)
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    # 当前会话同步（方便连续使用）
    $env:Path = "$NewPath;$env:Path"
}

function Add-ToUserPath {
    param([string]$Dir)
    $userPath = Get-UserPath
    if ($userPath -split ';' | Where-Object { $_.TrimEnd('\') -eq $Dir.TrimEnd('\') }) {
        Write-Ok "  $Dir 已在 PATH 中，跳过。"
        return
    }
    $updated = if ($userPath) { "$Dir;$userPath" } else { $Dir }
    Set-UserPath -NewPath $updated
    Write-Ok "  已将 $Dir 加入用户 PATH。"
}

# ---- 探测架构 -------------------------------------------------------------
function Get-Arch {
    if ($Arch) { return $Arch }
    if ([Environment]::Is64BitOperatingSystem) { return "x64" } else { return "x86" }
}

# ---- 解析版本 -------------------------------------------------------------
function Resolve-Version {
    if ($Version) { return $Version }
    Write-Info "正在获取最新版本号..."
    $latest = Invoke-RestMethod -Uri $ApiLatest -Headers @{ "User-Agent" = "toolkit-invoke" }
    $tag = [string]$latest.tag_name
    return $tag.TrimStart('v')
}

# ---- 下载并解压 -----------------------------------------------------------
function Install-Binary {
    param([string]$Ver, [string]$Platform, [string]$Dir)
    # 资产命名约定：uvman-<ver>-<plat>.zip（按需调整）
    $asset = "uvman-${Ver}-${Platform}.zip"
    $url   = "${Releases}/download/v${Ver}/${asset}"
    $tmp   = Join-Path $env:TEMP ("uvman-" + [guid]::NewGuid().ToString("N"))

    Write-Info "下载 $url"
    Invoke-WebRequest -Uri $url -OutFile "$tmp.zip"

    Write-Info "解压到 $Dir"
    Expand-Archive -Path "$tmp.zip" -DestinationPath $tmp -Force
    $exe = Get-ChildItem -Path $tmp -Recurse -Filter "uvman.exe" | Select-Object -First 1
    if (-not $exe) {
        throw "压缩包内未找到 uvman.exe"
    }
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    Copy-Item -Path $exe.FullName -Destination (Join-Path $Dir "uvman.exe") -Force

    Remove-Item -Recurse -Force $tmp, "$tmp.zip"
}

# ---- 入口 -----------------------------------------------------------------
Write-Info "uvman Windows 安装脚本"

$ver = Resolve-Version
$plat = "windows-$(Get-Arch)"

$finalDir = $InstallDir
if (-not $finalDir) {
    $default = Join-Path $env:USERPROFILE ".uvman\bin"
    $input = Read-Host "安装目录 [回车用默认: $default]"
    $finalDir = if ($input) { $input } else { $default }
}
# 展开 ~；确保绝对路径
$finalDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($finalDir)

try {
    Install-Binary -Ver $ver -Platform $plat -Dir $finalDir
    Add-ToUserPath -Dir $finalDir
    Write-Ok "uvman $ver 已安装到 $finalDir"
    Write-Warn "请关闭并重新打开终端，或运行: uvman version 验证。"
}
catch {
    Write-Fail "安装失败: $($_.Exception.Message)"
    Write-Warn "请检查资产命名是否与约定 uvman-<ver>-<plat>.zip 一致，或改用手动方式下载：$Releases"
    exit 1
}