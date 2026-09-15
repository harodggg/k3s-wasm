#!/usr/bin/env bash
# build-ui.sh —— 构建控制台（前端 + wasm 后端），并按需打成容器镜像。
#
# 三种出包方式，用途不同：
#   --import              打成本地镜像并直接 `k3s ctr images import`（无需镜像仓库，
#                         单机 k3s 最省事；镜像标签保持 k3s-wasm/k3s-wasm-ui:dev）
#   --push REPO:TAG       构建并 docker push（多节点/有仓库时用）
#   --spin-push REPO:TAG  用 `spin registry push` 产出 Spin 应用镜像（SpinKube/SpinApp 路径）
#
# 用法：
#   ./scripts/build-ui.sh                    # 只构建，产出 wasm 与 dist
#   ./scripts/build-ui.sh --import
#   ./scripts/build-ui.sh --push registry.example.com/k3s-wasm-ui:1.0.0
#   K3S_WASM_PROXY_URL=http://proxy.k3s-wasm.svc:8001 ./scripts/build-ui.sh --import
#
# 关于 wasm 镜像格式（说实话的部分）：
#   wasmtime shim 期望镜像里能拿到 .wasm 模块（runwasi 认的是 wasm 层 media type，
#   或容器文件系统里的路径）。本脚本用最朴素的 `FROM scratch` + COPY + ENTRYPOINT，
#   这在多数 k3s 上可用；如果你的节点上报
#     "failed to load wasm module" / "no wasm layer found"
#   那就改用 Docker 的 wasm 平台构建：
#     docker buildx build --platform wasi/wasm -t $IMG .
#   或者走 SpinApp 路径（--spin-push），那条路的镜像格式由 spin 自己保证。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

MODE="build"
IMAGE="${IMAGE:-k3s-wasm/k3s-wasm-ui:dev}"
SKIP_FRONTEND=0

usage() {
    sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --import)    MODE="import"; shift ;;
        --push)      MODE="push"; IMAGE="${2:?}"; shift 2 ;;
        --spin-push) MODE="spin-push"; IMAGE="${2:?}"; shift 2 ;;
        --skip-frontend) SKIP_FRONTEND=1; shift ;;
        -h|--help)   usage 0 ;;
        *) die "未知参数：$1" ;;
    esac
done

# ── 工具链：把 rustup 的 shim 放到 PATH 最前面 ───────────────────────
# 系统里常同时有 Homebrew 的 rustc 与 rustup 的 rustc，而 Homebrew 在前。
# 用 Homebrew 的 rustc 编 wasm 会报「can't find crate for core」，
# 报错完全看不出是工具链选错了。
if [ -d "$HOME/.cargo/bin" ]; then
    case ":$PATH:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) PATH="$HOME/.cargo/bin:$PATH" ;;
    esac
    export PATH
fi
# 依赖缓存/产物目录放工作区，避免 ~ 不可写（受限环境常见）
export CARGO_HOME="${CARGO_HOME:-$REPO_DIR/../.cargo}"
mkdir -p "$CARGO_HOME" 2>/dev/null || true

have cargo || die "需要 cargo（rustup 安装的，且已 rustup target add wasm32-wasip2）"
have npm || die "需要 npm"

# ── 1. 前端 ─────────────────────────────────────────────────────────
if [ "$SKIP_FRONTEND" = 0 ]; then
    log "① 构建前端"
    export npm_config_cache="${npm_config_cache:-$REPO_DIR/../.npm-cache}"
    cd "$REPO_DIR/ui/frontend"
    if [ -f package-lock.json ]; then
        npm ci --no-audit --no-fund >/dev/null 2>&1 || npm install --no-audit --no-fund >/dev/null
    else
        npm install --no-audit --no-fund >/dev/null
    fi
    npm run build >/dev/null
    ok "前端产物 $(du -sh dist | awk '{print $1}')（会被编译期嵌进 wasm）"
else
    warn "按 --skip-frontend 跳过前端构建"
fi

# ── 2. wasm 后端 ────────────────────────────────────────────────────
log "② 构建 wasm 后端（wasm32-wasip2）"
cd "$REPO_DIR/ui/backend"
cargo build --release --target wasm32-wasip2 2>&1 | grep -E '^(error|warning: unused)' || true
WASM="$REPO_DIR/ui/backend/target/wasm32-wasip2/release/k3s_wasm_ui.wasm"
[ -f "$WASM" ] || die "构建产物不存在：$WASM"
ok "wasm 产物 $(du -h "$WASM" | awk '{print $1}') → $WASM"

if [ -n "${K3S_WASM_PROXY_URL:-}" ]; then
    info "构建期烘焙的 kube-api-proxy 地址：$K3S_WASM_PROXY_URL"
else
    info "使用编译期默认代理地址（标准部署不需要改）"
fi

if [ "$MODE" = build ]; then
    log "完成（只构建）。要出包用 --import / --push / --spin-push"
    exit 0
fi

# ── 3. 出包 ─────────────────────────────────────────────────────────
case "$MODE" in
    import|push)
        have docker || die "需要 docker（或用 --spin-push 完全绕开 docker）"
        log "③ 构建容器镜像 $IMAGE"
        CTX="$(mktemp -d)"
        cp "$WASM" "$CTX/ui.wasm"
        cat >"$CTX/Dockerfile" <<'EOF'
# 极简 wasm 镜像：只有一个模块文件。
# ENTRYPOINT 指向它，既给 wasmtime shim 一个可用的模块路径，
# 也让 Command 型组件（本仓库 examples/hello-wasip2）能直接跑。
FROM scratch
COPY ui.wasm /ui.wasm
ENTRYPOINT ["/ui.wasm"]
EOF
        docker build -q -t "$IMAGE" "$CTX" >/dev/null
        rm -rf "$CTX"
        ok "镜像已构建：$IMAGE"

        if [ "$MODE" = import ]; then
            have k3s || die "本机没有 k3s，无法 --import；请用 --push 推到仓库"
            log "导入 k3s 的 containerd（不经过镜像仓库）"
            docker save "$IMAGE" | k3s ctr images import - >/dev/null
            ok "已导入。记得 Pod 里写 imagePullPolicy: Never 或 IfNotPresent"
            info "部署：kubectl apply -k deploy/overlays/shim-only"
        else
            log "推送 $IMAGE"
            docker push "$IMAGE" >/dev/null
            ok "已推送"
        fi
        ;;

    spin-push)
        SPIN_BIN="$(command -v spin 2>/dev/null || true)"
        [ -n "$SPIN_BIN" ] || die "需要 spin CLI（https://spinframework.dev）"
        log "③ spin registry push $IMAGE"
        info "说明：这条路径产出的是 Spin 应用镜像（带 spin.toml 与 wasm），供 SpinKube 的 SpinApp 使用。"
        cd "$REPO_DIR/ui/backend"
        "$SPIN_BIN" registry push --build "$IMAGE" >/dev/null
        ok "已推送 Spin 应用镜像：$IMAGE"
        info "部署：把 deploy/overlays/spinkube/spinapp.yaml 里的 image 改成 $IMAGE，然后 kubectl apply -k deploy/overlays/spinkube"
        ;;
esac
