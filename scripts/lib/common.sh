#!/usr/bin/env bash
# 公共函数库：日志、架构探测、k3s 探测、下载。
# 由其他脚本 source，不单独执行。
# shellcheck shell=bash

set -euo pipefail

# ── 日志 ────────────────────────────────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_YLW=$'\033[33m'
    C_BLU=$'\033[34m'; C_DIM=$'\033[2m';  C_RST=$'\033[0m'
else
    C_RED=; C_GRN=; C_YLW=; C_BLU=; C_DIM=; C_RST=
fi

log()   { printf '%s==>%s %s\n' "$C_BLU" "$C_RST" "$*" >&2; }
info()  { printf '%s  -%s %s\n' "$C_DIM" "$C_RST" "$*" >&2; }
ok()    { printf '%s  ✓%s %s\n' "$C_GRN" "$C_RST" "$*" >&2; }
warn()  { printf '%swarn:%s %s\n' "$C_YLW" "$C_RST" "$*" >&2; }
err()   { printf '%serr:%s %s\n'  "$C_RED" "$C_RST" "$*" >&2; }
die()   { err "$*"; exit 1; }

# 命令存在性
have() { command -v "$1" >/dev/null 2>&1; }

# ── 全局开关（由调用方设置）──────────────────────────────────────────
DRY_RUN="${DRY_RUN:-0}"
ASSUME_YES="${ASSUME_YES:-0}"

run() {
    if [ "$DRY_RUN" = 1 ]; then
        printf '%s  [dry-run]%s %s\n' "$C_DIM" "$C_RST" "$*" >&2
        return 0
    fi
    "$@"
}

# 写文件（支持 dry-run）；用法：write_file <路径> <<'EOF' ... EOF
write_file() {
    local path="$1" mode="${2:-0644}"
    local tmp; tmp="$(mktemp)"
    cat >"$tmp"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] 将写入 $path ($mode):"
        sed 's/^/      | /' "$tmp" >&2
        rm -f "$tmp"
        return 0
    fi
    install -D -m "$mode" "$tmp" "$path"
    rm -f "$tmp"
}

confirm() {
    [ "$ASSUME_YES" = 1 ] && return 0
    local reply
    printf '%s [y/N] ' "$1" >&2
    read -r reply </dev/tty || return 1
    case "$reply" in [yY]*) return 0 ;; *) return 1 ;; esac
}

# ── 环境探测 ────────────────────────────────────────────────────────
require_root() {
    [ "$(id -u)" = 0 ] || die "需要 root 权限（会写 containerd 配置并重启 k3s）。请用 sudo 重跑。"
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   echo amd64 ;;
        aarch64|arm64)  echo arm64 ;;
        armv7l|armhf)   echo arm ;;
        *) die "不支持的架构：$(uname -m)" ;;
    esac
}

# k3s 是否以 systemd 服务运行
k3s_service_name() {
    if systemctl list-unit-files 2>/dev/null | grep -q '^k3s\.service'; then
        echo k3s
    elif systemctl list-unit-files 2>/dev/null | grep -q '^k3s-agent\.service'; then
        echo k3s-agent
    else
        echo ""
    fi
}

require_k3s() {
    have k3s || die "找不到 k3s，请先安装 k3s：https://docs.k3s.io/installation"
    local svc; svc="$(k3s_service_name)"
    [ -n "$svc" ] || warn "k3s 似乎不是由 systemd 管理；安装后需自行重启 k3s 进程。"
    echo "$svc"
}

k3s_kubectl() {
    if have kubectl; then kubectl "$@"; else k3s kubectl "$@"; fi
}

# ── 下载 ────────────────────────────────────────────────────────────
download() {
    local url="$1" dest="$2"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] 下载 $url -> $dest"
        return 0
    fi
    if have curl; then
        curl -fsSL --retry 3 --retry-delay 2 -o "$dest" "$url"
    elif have wget; then
        wget -q -O "$dest" "$url"
    else
        die "需要 curl 或 wget 来下载 $url"
    fi
}

# 校验 sha256；$2 为期望值（空则跳过并返回 2）
verify_sha256() {
    local file="$1" expected="$2"
    [ -n "$expected" ] || return 2
    local actual
    if have sha256sum; then
        actual="$(sha256sum "$file" | awk '{print $1}')"
    elif have shasum; then
        actual="$(shasum -a 256 "$file" | awk '{print $1}')"
    else
        warn "没有 sha256sum/shasum，跳过校验"
        return 2
    fi
    if [ "$actual" = "$expected" ]; then
        return 0
    fi
    err "sha256 不匹配：期望 ${expected}，实际 $actual"
    return 1
}

# 从 GitHub release 的 checksums 文件里取某个文件的摘要
fetch_expected_sha() {
    local sums_url="$1" pattern="$2" tmp
    tmp="$(mktemp)"
    if download "$sums_url" "$tmp" 2>/dev/null; then
        grep -E "[[:space:]]\*?${pattern}\$" "$tmp" 2>/dev/null | awk '{print $1}' | head -1 || true
    fi
    rm -f "$tmp"
}

# 后端探测 GitHub latest tag（离线/被限流时返回空，由调用方 fallback 到 pin 的版本）
github_latest_tag() {
    local repo="$1"
    have curl || return 1
    curl -fsSL --max-time 10 "https://api.github.com/repos/${repo}/releases/latest" 2>/dev/null \
        | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name":[[:space:]]*"([^"]+)".*/\1/'
}
