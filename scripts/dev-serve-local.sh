#!/usr/bin/env bash
# dev-serve-local.sh —— 在笔记本上直接跑控制台后端（不需要 k3s、不需要容器）。
#
# 做的事：
#   1. 起一个假的 k8s API（scripts/dev-mock-k8s-api.py）
#   2. 用 wasmtime 的 wasi:http 宿主能力加载并运行 ui 后端 wasm 组件
#   3. 把 K8S_PROXY_URL 指向上面的 mock
#
# 用法：
#   ./scripts/dev-serve-local.sh                 # 默认 127.0.0.1:8080
#   PORT=9090 ./scripts/dev-serve-local.sh
#   ./scripts/dev-serve-local.sh path/to/k3s_wasm_ui.wasm
#   NO_MOCK=1 ./scripts/dev-serve-local.sh       # 已有真实 kubectl proxy 时
#
# 注意：这验证的是**组件逻辑 + wasi:http 契约**，不是 k3s 的 containerd shim 链路。
# shim 链路要用 scripts/verify-wasm-runtime.sh 在集群里验。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"

WASM="${1:-$REPO_DIR/ui/backend/target/wasm32-wasip2/release/k3s_wasm_ui.wasm}"
PORT="${PORT:-8080}"
PROXY_PORT="${PROXY_PORT:-8001}"
NO_MOCK="${NO_MOCK:-0}"

[ -f "$WASM" ] || die "找不到 $WASM
先构建：
    cd ui/backend && cargo build --release --target wasm32-wasip2"

# wasmtime 定位：环境变量 → PATH → 工作区里解压好的
if [ -z "${WASMTIME_BIN:-}" ]; then
    if have wasmtime; then
        WASMTIME_BIN="$(command -v wasmtime)"
    else
        WASMTIME_BIN="$(find "$REPO_DIR/.." -maxdepth 4 -type f -name wasmtime -perm -u+x 2>/dev/null | head -1 || true)"
    fi
fi
[ -n "${WASMTIME_BIN:-}" ] && [ -x "$WASMTIME_BIN" ] || die "找不到 wasmtime。
装一个（https://wasmtime.dev），或用 WASMTIME_BIN=/path/to/wasmtime 指定。"

MOCK_PID=""
cleanup() {
    if [ -n "$MOCK_PID" ] && kill -0 "$MOCK_PID" 2>/dev/null; then
        kill "$MOCK_PID" 2>/dev/null || true
        wait "$MOCK_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# wasmtime 默认把 JIT 缓存写 ~/Library/Caches，HOME 只读或受限时**启动就失败**：
#   Error: failed to create cache directory: .../Library/Caches/BytecodeAlliance.wasmtime
# 报错完全看不出跟权限之外的关系，所以缓存目录固定放仓库内。
export WASMTIME_CACHE_DIR="${WASMTIME_CACHE_DIR:-$REPO_DIR/.wasmtime-cache}"
mkdir -p "$WASMTIME_CACHE_DIR" 2>/dev/null || true

if [ "$NO_MOCK" = 1 ]; then
    export K8S_PROXY_URL="${K8S_PROXY_URL:-http://127.0.0.1:${PROXY_PORT}}"
    info "跳过 mock，K8S_PROXY_URL=$K8S_PROXY_URL"
else
    have python3 || die "需要 python3 来跑 mock k8s API"
    log "启动 mock k8s API（127.0.0.1:${PROXY_PORT}）"
    python3 "$SCRIPT_DIR/dev-mock-k8s-api.py" --port "$PROXY_PORT" &
    MOCK_PID=$!
    export K8S_PROXY_URL="http://127.0.0.1:${PROXY_PORT}"
    # 等端口起来
    for _ in $(seq 1 40); do
        if curl -fsS -o /dev/null "http://127.0.0.1:${PROXY_PORT}/version" 2>/dev/null; then
            break
        fi
        sleep 0.25
    done
    ok "mock 就绪"
fi

# 免密登录的会话密钥：**必须给**，否则门禁对所有 /api 返回 503（失败关闭）。
# http://127.0.0.1 是浏览器认可的「可信来源」，所以本机也能真正跑通 Touch ID 绑定/登录。
# 两个值都随机生成；注册码会打印出来，首次打开页面绑定时填它。
export K3S_WASM_SESSION_SECRET="${K3S_WASM_SESSION_SECRET:-$(python3 -c 'import base64,os;print(base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip("="))')}"
export K3S_WASM_REGISTRATION_CODE="${K3S_WASM_REGISTRATION_CODE:-$(python3 -c 'import os;print(os.urandom(8).hex())')}"
export K3S_WASM_RP_ID="${K3S_WASM_RP_ID:-127.0.0.1}"
export K3S_WASM_ORIGIN="${K3S_WASM_ORIGIN:-http://127.0.0.1:${PORT}}"

log "wasmtime serve $WASM"
info "组件 >  http://127.0.0.1:${PORT}/"
info "首次绑定用的一次性注册码：${K3S_WASM_REGISTRATION_CODE}"
info "API  >  http://127.0.0.1:${PORT}/api/summary"
info "代理 >  ${K8S_PROXY_URL}"
cat >&2 <<'EOF'
  ── 提示 ──────────────────────────────────────────────────────────────
  -S inherit-network=y  必须给：wasip2 下没有它，出站请求会以
                        PermissionDenied 的形式失败，看起来像「被墙」。
  -S http=y             提供 wasi:http 的 incoming/outgoing handler。
  -S inherit-env=y      让组件能读到 K8S_PROXY_URL。
  ─────────────────────────────────────────────────────────────────────
EOF

# -C cache=n：彻底关掉 JIT 缓存。只设 WASMTIME_CACHE_DIR 在 wasmtime 48 上**不生效**，
# 它仍会去建 ~/Library/Caches/BytecodeAlliance.wasmtime 并直接失败。
"$WASMTIME_BIN" serve \
    -C cache=n \
    -S http=y \
    -S inherit-network=y \
    -S inherit-env=y \
    --addr "127.0.0.1:${PORT}" \
    "$WASM"
