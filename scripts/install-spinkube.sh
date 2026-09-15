#!/usr/bin/env bash
# install-spinkube.sh —— 装 SpinKube（SpinApp CRD + operator + 运行时管理器）。
#
# 命令按官方当前文档（OCI chart，没有 helm repo 可 add）+ 上游 release 资产写成，
# 版本都钉住了，便于复现。
#
# 两条路线二选一，**不要混用**（同名 RuntimeClass wasmtime-spin-v2 会被互相覆盖）：
#   A) 节点侧手工装 shim（scripts/install-wasm-runtime.sh）→ RuntimeClass handler = spin
#   B) 用 Runtime Class Manager（本脚本默认）        → RuntimeClass handler = spin-v2
#
# 用法：
#   sudo ./scripts/install-spinkube.sh                # 装 cert-manager + RCM + operator
#   sudo ./scripts/install-spinkube.sh --no-runtime-class-manager
#                                                    # 已用 install-wasm-runtime.sh 装过 shim 时
#   sudo ./scripts/install-spinkube.sh --dry-run
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"

CERT_MANAGER_VERSION="${CERT_MANAGER_VERSION:-v1.20.0}"
SPIN_OPERATOR_VERSION="${SPIN_OPERATOR_VERSION:-0.6.1}"
RCM_VERSION="${RCM_VERSION:-0.2.0}"
SPIN_SHIM_VERSION="${SPIN_SHIM_VERSION:-v0.25.1}"
SHIM_NODE_LABEL="${SHIM_NODE_LABEL:-spin}"

WITH_RCM=1
DRY_RUN="${DRY_RUN:-0}"
ASSUME_YES="${ASSUME_YES:-0}"

usage() {
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    cat >&2 <<'EOF'
选项：
  --no-runtime-class-manager   跳过 RCM（你已经用 install-wasm-runtime.sh 装过 shim）
  --shim-label KEY             节点标签键（默认 spin，对应 RCM Shim 的 nodeSelector）
  --dry-run / -y / -h
EOF
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --no-runtime-class-manager) WITH_RCM=0; shift ;;
        --shim-label) SHIM_NODE_LABEL="${2:?}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -y|--yes) ASSUME_YES=1; shift ;;
        -h|--help) usage 0 ;;
        *) die "未知参数：$1" ;;
    esac
done

have helm || die "需要 helm（装 SpinKube 用 OCI chart）"
have kubectl || have k3s || die "需要 kubectl 或 k3s"

run_kubectl() { k3s_kubectl "$@"; }

log "SpinKube 安装：operator=$SPIN_OPERATOR_VERSION rcm=$RCM_VERSION cert-manager=$CERT_MANAGER_VERSION"
if ! confirm "将对集群执行 helm/kubectl 变更，继续？"; then die "已取消"; fi

apply_url() {
    local url="$1" what="$2"
    log "apply $what"
    info "$url"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] kubectl apply -f $url"
        return 0
    fi
    run_kubectl apply -f "$url" >/dev/null
    ok "$what 已应用"
}

# ── 1. cert-manager（spin-operator 的 webhook 证书依赖它）─────────────
log "① cert-manager $CERT_MANAGER_VERSION"
apply_url "https://github.com/cert-manager/cert-manager/releases/download/${CERT_MANAGER_VERSION}/cert-manager.yaml" "cert-manager"
if [ "$DRY_RUN" != 1 ]; then
    info "等 cert-manager webhook 就绪…"
    run_kubectl wait --for=condition=available --timeout=300s \
        deployment/cert-manager-webhook -n cert-manager >/dev/null 2>&1 \
        && ok "cert-manager webhook 就绪" || warn "cert-manager webhook 还没就绪，稍后重试"
fi

# ── 2. Runtime Class Manager（可选）──────────────────────────────────
if [ "$WITH_RCM" = 1 ]; then
    log "② Runtime Class Manager $RCM_VERSION"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] helm upgrade --install runtime-class-manager -n runtime-class-manager --create-namespace --version $RCM_VERSION oci://ghcr.io/spinframework/charts/runtime-class-manager"
    else
        helm upgrade --install runtime-class-manager \
            --namespace runtime-class-manager --create-namespace \
            --version "$RCM_VERSION" \
            oci://ghcr.io/spinframework/charts/runtime-class-manager >/dev/null
        ok "RCM 已安装"
    fi

    # Shim CR：RCM 会据此在匹配的节点上跑特权 Job 装 shim、改 containerd 配置并重启 k3s
    apply_url "https://github.com/spinframework/containerd-shim-spin/releases/download/${SPIN_SHIM_VERSION}/runtime-class-manager-shim-v1alpha1-${SPIN_SHIM_VERSION}.yaml" "Shim CR（containerd-shim-spin ${SPIN_SHIM_VERSION}）"

    log "给节点打标签 ${SHIM_NODE_LABEL}=true（必须与 Shim.spec.nodeSelector 一致）"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] kubectl label node --all ${SHIM_NODE_LABEL}=true"
    else
        run_kubectl label node --all "${SHIM_NODE_LABEL}=true" --overwrite >/dev/null 2>&1 \
            && ok "节点已打标签" || warn "打标签失败，请手动执行：kubectl label node --all ${SHIM_NODE_LABEL}=true"
    fi
    info "RCM 会创建 RuntimeClass wasmtime-spin-v2（handler spin-v2）"
else
    warn "跳过 RCM：请确认你已跑过 scripts/install-wasm-runtime.sh"
    info "那种情况下 RuntimeClass wasmtime-spin-v2 的 handler 是 spin（不是 spin-v2）"
fi

# ── 3. SpinApp CRD + 默认 executor ──────────────────────────────────
log "③ SpinApp CRD 与默认 SpinAppExecutor"
apply_url "https://github.com/spinframework/spin-operator/releases/download/v${SPIN_OPERATOR_VERSION}/spin-operator.crds.yaml" "SpinApp/SpinAppExecutor CRD"
apply_url "https://github.com/spinframework/spin-operator/releases/download/v${SPIN_OPERATOR_VERSION}/spin-operator.shim-executor.yaml" "默认 SpinAppExecutor（containerd-shim-spin）"

# ── 4. spin-operator ────────────────────────────────────────────────
log "④ spin-operator $SPIN_OPERATOR_VERSION"
if [ "$DRY_RUN" = 1 ]; then
    info "[dry-run] helm upgrade --install spin-operator -n spin-operator --create-namespace --version $SPIN_OPERATOR_VERSION --wait oci://ghcr.io/spinframework/charts/spin-operator"
else
    helm upgrade --install spin-operator \
        --namespace spin-operator --create-namespace \
        --version "$SPIN_OPERATOR_VERSION" --wait \
        oci://ghcr.io/spinframework/charts/spin-operator >/dev/null
    ok "spin-operator 已安装"
fi

log "完成。检查："
cat >&2 <<EOF
    kubectl get crd | grep spinkube
    kubectl get spinappexecutor -A
    kubectl get runtimeclass
    # 之后：
    ./scripts/build-ui.sh --push <你的仓库>/k3s-wasm-ui:dev
    kubectl apply -k deploy/overlays/spinkube
EOF
info "注意：SpinAppExecutor 是命名空间级的，SpinApp 必须和它在同一个命名空间（默认 k3s-wasm）。"
