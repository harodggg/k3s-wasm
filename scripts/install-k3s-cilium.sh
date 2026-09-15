#!/usr/bin/env bash
# install-k3s-cilium.sh —— 一键：k3s + Cilium（替换 flannel/kube-proxy）+ Hubble UI，可选 wasm 运行时与控制台。
#
# 设计取向：这是给「一台干净的 Linux 机器」用的引导脚本，所以
#   · 幂等：重复执行只做增量的部分（k3s 已装就不重装，Cilium 走 helm upgrade）
#   · 每一步都能单独跳过，便于在已有集群上只补 Cilium 或只补 Hubble UI
#   · 关键动作前后都做校验（节点是否 Ready、Cilium 是否 rollout 完、UI 端口是否真的通）
#   · 所有生成物落在 /etc/k3s-wasm/，便于事后审计「当初装了什么参数」
#
# ── 为什么 k3s 要关掉 flannel / kube-proxy / 内置 network-policy ──────
#   Cilium 要接管这三件事，否则两套实现会打架（典型症状是 Pod 之间通、Service 不通，
#   或者 NodePort 时通时不通）。k3s 的对应开关：
#     --flannel-backend=none      关掉 flannel（k3s 默认 CNI）
#     --disable-network-policy    关掉 kube-router 的 NetworkPolicy 控制器
#     --disable-kube-proxy        关掉 kube-proxy，交给 Cilium 的 kube-proxy 替换
#   代价：在 Cilium 装好之前节点是 NotReady（没有 CNI），所以脚本不会在这里死等 Ready。
#
# ── 一个实测到的交互坑（写在这里免得你重踩）──────────────────────────
#   k3s 的 servicelb（klipper-lb）用 hostPort 实现 LoadBalancer，而 kube-proxy 被 Cilium
#   替换后 hostPort 不再由 kube-proxy 处理。结果是 Traefik 的 Service 显示 EXTERNAL-IP：
#   <节点IP>，但 `ss -tlnp | grep :80` 什么都没有 —— 看起来「有入口」，实际不通。
#   所以本脚本用 NodePort 暴露 Hubble UI（Cilium 在 BPF 里实现，实测正常）。
#   想用 Ingress 的话：把 Traefik 改成 hostNetwork 并让它直接监听 80/443，
#   或者换一个不依赖 hostPort 的 ingress controller。见 docs/05-k3s-cilium.md。
#
# 用法（在节点上以 root 执行）：
#   ./scripts/install-k3s-cilium.sh                      # 全默认
#   ./scripts/install-k3s-cilium.sh --connectivity-test  # 装完再跑 Cilium 连通性测试
#   ./scripts/install-k3s-cilium.sh --no-cilium          # 只要 k3s（默认 flannel）
#   ./scripts/install-k3s-cilium.sh --with-wasm-runtime  # 顺带装 wasm32-wasip2 运行时
#   ./scripts/install-k3s-cilium.sh --uninstall
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/lib/common.sh" ]; then
    # shellcheck source=lib/common.sh
    . "$SCRIPT_DIR/lib/common.sh"
else
    # 允许单独把这个脚本拷到节点上跑（此时没有 lib/）
    C_GRN=; C_YLW=; C_RED=; C_BLU=; C_DIM=; C_RST=
    log()  { printf '==> %s\n' "$*" >&2; }
    info() { printf '  - %s\n' "$*" >&2; }
    ok()   { printf '  ✓ %s\n' "$*" >&2; }
    warn() { printf 'warn: %s\n' "$*" >&2; }
    err()  { printf 'err: %s\n' "$*" >&2; }
    die()  { err "$*"; exit 1; }
    have() { command -v "$1" >/dev/null 2>&1; }
    DRY_RUN=0
    run() {
        if [ "$DRY_RUN" = 1 ]; then printf '  [dry-run] %s\n' "$*" >&2; return 0; fi
        "$@"
    }
fi

# ── 默认参数 ────────────────────────────────────────────────────────
K3S_CHANNEL="${K3S_CHANNEL:-stable}"
CILIUM_VERSION="${CILIUM_VERSION:-1.20.1}"
CILIUM_CLI_VERSION="${CILIUM_CLI_VERSION:-v0.20.0}"
HUBBLE_CLI_VERSION="${HUBBLE_CLI_VERSION:-v1.19.4}"
HUBBLE_NODEPORT="${HUBBLE_NODEPORT:-30080}"
NODE_IP="${NODE_IP:-}"
CONF_DIR="/etc/k3s-wasm"
KUBECONFIG_PATH="/etc/rancher/k3s/k3s.yaml"

WITH_CILIUM=1
KUBE_PROXY_REPLACEMENT=1
WITH_CLI=1
WITH_INGRESS=1
CONNECTIVITY_TEST=0
WITH_WASM_RUNTIME=""
UNINSTALL=0
FORCE_K3S=0
DRY_RUN=0

usage() {
    sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    cat >&2 <<'EOF'
选项：
  --node-ip IP              节点的对外 IP（默认自动探测）
  --k3s-channel C           稳定通道或具体版本（默认 stable，形如 v1.36.4+k3s1）
  --cilium-version V        Cilium chart 版本（默认 1.20.1）
  --hubble-nodeport N       Hubble UI 的 NodePort（默认 30080）
  --no-cilium               不装 Cilium（k3s 用默认 flannel + kube-proxy）
  --keep-kube-proxy         装 Cilium 但保留 kube-proxy（kubeProxyReplacement=false）
  --no-ingress              不建 Traefik Ingress（只留 NodePort 与 port-forward）
  --no-cli                  不装 cilium / hubble CLI
  --connectivity-test       装完跑 cilium connectivity test（数分钟）
  --with-wasm-runtime       顺带跑 scripts/install-wasm-runtime.sh
  --force-k3s               即使已装 k3s 也重跑安装脚本
  --uninstall               卸载 k3s 与 Cilium
  --dry-run / -y / -h
EOF
    exit "${1:-0}"
}

ASSUME_YES=0
while [ $# -gt 0 ]; do
    case "$1" in
        --node-ip)           NODE_IP="${2:?}"; shift 2 ;;
        --k3s-channel)       K3S_CHANNEL="${2:?}"; shift 2 ;;
        --cilium-version)    CILIUM_VERSION="${2:?}"; shift 2 ;;
        --hubble-nodeport)   HUBBLE_NODEPORT="${2:?}"; shift 2 ;;
        --no-cilium)         WITH_CILIUM=0; shift ;;
        --keep-kube-proxy)   KUBE_PROXY_REPLACEMENT=0; shift ;;
        --no-ingress)        WITH_INGRESS=0; shift ;;
        --no-cli)            WITH_CLI=0; shift ;;
        --connectivity-test) CONNECTIVITY_TEST=1; shift ;;
        --with-wasm-runtime) WITH_WASM_RUNTIME=1; shift ;;
        --force-k3s)         FORCE_K3S=1; shift ;;
        --uninstall)         UNINSTALL=1; shift ;;
        --dry-run)           DRY_RUN=1; shift ;;
        -y|--yes)            ASSUME_YES=1; shift ;;
        -h|--help)           usage 0 ;;
        *) die "未知参数：$1（--help 看用法）" ;;
    esac
done

confirm() {
    [ "$ASSUME_YES" = 1 ] && return 0
    # dry-run 什么都不改，不该拦一道确认
    [ "$DRY_RUN" = 1 ] && return 0
    local reply
    if ! [ -e /dev/tty ]; then
        die "当前没有可用终端（非交互执行）。请加 -y 明确同意。"
    fi
    printf '%s [y/N] ' "$1" >&2
    read -r reply </dev/tty || return 1
    case "$reply" in [yY]*) return 0 ;; *) return 1 ;; esac
}

k() { k3s kubectl "$@"; }

# helm / cilium CLI 不认 k3s 自带的 kubeconfig 路径，必须显式导出。
# 漏了它的表现是 helm 报 "Kubernetes cluster unreachable: Get http://localhost:8080/version" ——
# 看起来像集群没起来，其实是客户端找错了配置。
setup_kubeconfig() {
    [ -r "$KUBECONFIG_PATH" ] || return 0
    export KUBECONFIG="${KUBECONFIG:-$KUBECONFIG_PATH}"
    mkdir -p /root/.kube
    ln -sf "$KUBECONFIG_PATH" /root/.kube/config
    info "KUBECONFIG=$KUBECONFIG（并软链到 /root/.kube/config，root 下直接用 kubectl/helm 即可）"
}

# ════════════════════════════════════════════════════════════════════
# 0. 前置检查
# ════════════════════════════════════════════════════════════════════
preflight() {
    [ "$(id -u)" = 0 ] || die "需要 root"
    [ "$(uname -s)" = Linux ] || die "这个脚本只支持 Linux"

    case "$(uname -m)" in
        x86_64)  ARCH=amd64 ;;
        aarch64) ARCH=arm64 ;;
        *) die "不支持的架构：$(uname -m)" ;;
    esac
    info "架构：$ARCH"

    # cgroup v2 + bpffs 是 Cilium 的硬需求（新内核都有，但容器化环境里常缺）
    if [ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" != cgroup2fs ]; then
        warn "cgroup 不是 v2（Cilium 1.20 建议 cgroup v2）"
    else
        ok "cgroup v2"
    fi
    if ! mount | grep -q ' /sys/fs/bpf '; then
        info "挂载 /sys/fs/bpf"
        run mount bpffs /sys/fs/bpf -t bpf 2>/dev/null || warn "bpffs 挂载失败（Cilium 通常也能自己处理）"
    fi

    for c in curl tar; do have "$c" || die "缺少 $c"; done

    # 外网（k3s 安装脚本、helm chart、镜像都从网上来）
    local code
    code="$(curl -sS -m 12 -o /dev/null -w '%{http_code}' https://get.k3s.io 2>/dev/null || echo 000)"
    [ "$code" = 200 ] || warn "get.k3s.io 返回 $code（网络受限？）"
    code="$(curl -sS -m 12 -o /dev/null -w '%{http_code}' https://helm.cilium.io/index.yaml 2>/dev/null || echo 000)"
    [ "$code" = 200 ] || warn "helm.cilium.io 返回 $code（Cilium chart 可能拉不到）"

    if swapon --show 2>/dev/null | grep -q .; then
        warn "检测到 swap，k3s 官方建议关闭：swapoff -a && sed -i '/ swap / s/^/#/' /etc/fstab"
    fi

    if [ -z "$NODE_IP" ]; then
        NODE_IP="$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}' | head -1)"
        [ -n "$NODE_IP" ] || NODE_IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
    fi
    [ -n "$NODE_IP" ] || die "探测不到节点 IP，请用 --node-ip 指定"
    ok "节点 IP：$NODE_IP"

    if have k3s; then
        info "已安装 k3s：$(k3s --version | head -1)"
        setup_kubeconfig
    else
        info "未安装 k3s"
    fi
}

# ════════════════════════════════════════════════════════════════════
# 1. k3s
# ════════════════════════════════════════════════════════════════════
k3s_exec_flags() {
    local flags="server --write-kubeconfig-mode 600 --node-ip ${NODE_IP} --advertise-address ${NODE_IP} --tls-san ${NODE_IP}"
    if [ "$WITH_CILIUM" = 1 ] && [ "$KUBE_PROXY_REPLACEMENT" = 1 ]; then
        # 这三个开关交给 Cilium 接管；缺一个都会出现「一半流量不通」的怪现象
        flags="$flags --flannel-backend=none --disable-network-policy --disable-kube-proxy"
    fi
    printf '%s' "$flags"
}

install_k3s() {
    if have k3s && [ "$FORCE_K3S" != 1 ]; then
        ok "k3s 已安装，跳过（要重装加 --force-k3s）"
        return 0
    fi
    log "① 安装 k3s（channel=$K3S_CHANNEL）"
    local flags; flags="$(k3s_exec_flags)"
    info "INSTALL_K3S_EXEC=\"$flags\""
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] curl -sfL https://get.k3s.io | INSTALL_K3S_CHANNEL=$K3S_CHANNEL INSTALL_K3S_EXEC=\"$flags\" sh -"
        return 0
    fi
    curl -sfL https://get.k3s.io | \
        INSTALL_K3S_CHANNEL="$K3S_CHANNEL" \
        INSTALL_K3S_EXEC="$flags" \
        sh -s - || die "k3s 安装失败，看 journalctl -u k3s -n 100"
    ok "k3s 已安装：$(k3s --version | head -1)"

    info "等 kubeconfig 与 API 就绪…"
    local i=0
    while [ $i -lt 60 ]; do
        if k version >/dev/null 2>&1; then break; fi
        sleep 2; i=$((i+1))
    done
    k version >/dev/null 2>&1 || die "API 起不来：journalctl -u k3s -n 100"
    ok "API 已就绪"
    setup_kubeconfig

    # k3s 自己的 RuntimeClass 清单是 addon，会异步出现；这里不强求
    info "记下安装参数到 $CONF_DIR/k3s-exec-flags"
    mkdir -p "$CONF_DIR"
    printf '%s\n' "$flags" > "$CONF_DIR/k3s-exec-flags"
}

wait_node_ready() {
    local timeout="${1:-300}"
    log "等节点 Ready（需要 CNI 就绪）"
    if [ "$DRY_RUN" = 1 ]; then info "[dry-run] wait node Ready"; return 0; fi
    local i=0
    while [ $i -lt "$timeout" ]; do
        if k get nodes 2>/dev/null | grep -q ' Ready'; then
            ok "节点 Ready"
            return 0
        fi
        sleep 3; i=$((i+3))
    done
    warn "节点在 ${timeout}s 内未 Ready，现状："
    k get nodes 2>/dev/null | sed 's/^/    /'
    k get pods -n kube-system 2>/dev/null | sed 's/^/    /'
    return 1
}

# ════════════════════════════════════════════════════════════════════
# 2. Cilium（+ Hubble + Hubble UI）
# ════════════════════════════════════════════════════════════════════
ensure_helm() {
    if have helm; then ok "helm 已存在：$(helm version --short)"; return 0; fi
    log "安装 helm"
    if [ "$DRY_RUN" = 1 ]; then info "[dry-run] 安装 helm"; return 0; fi
    curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash >/dev/null
    have helm || die "helm 安装失败"
    ok "helm：$(helm version --short)"
}

# 选 IPAM：k3s 在关掉 flannel 后仍可能给节点分配 podCIDR。
#  - 有 podCIDR → 用 kubernetes 模式（跟着 k3s 的 CIDR 走，最省心）
#  - 没有       → 用 cluster-pool，并显式指定 10.42.0.0/16（与 k3s 默认 cluster-cidr 一致，
#                 避免撞上云厂商常用的 10.0.0.0/8）
detect_ipam() {
    local cidr
    cidr="$(k get nodes -o jsonpath='{.items[0].spec.podCIDR}' 2>/dev/null || true)"
    if [ -n "$cidr" ]; then
        echo "kubernetes|${cidr}"
    else
        echo "cluster-pool|10.42.0.0/16"
    fi
}

install_cilium() {
    [ "$WITH_CILIUM" = 1 ] || { warn "按 --no-cilium 跳过 Cilium"; return 0; }
    log "② 安装 Cilium $CILIUM_VERSION（Hubble + Hubble UI）"
    ensure_helm

    mkdir -p "$CONF_DIR"
    local ipam_mode pod_cidr
    IFS='|' read -r ipam_mode pod_cidr <<<"$(detect_ipam)"
    info "IPAM 模式：$ipam_mode（节点 podCIDR：${pod_cidr}）"

    local values="$CONF_DIR/cilium-values.yaml"
    {
        echo "# 由 scripts/install-k3s-cilium.sh 生成"
        echo "cluster:"
        echo "  name: default"
        echo "kubeProxyReplacement: $([ "$KUBE_PROXY_REPLACEMENT" = 1 ] && echo true || echo false)"
        if [ "$KUBE_PROXY_REPLACEMENT" = 1 ]; then
            # 关掉 kube-proxy 后，Cilium 必须知道去哪找 API Server（此时还没有 Service 转发）
            echo "k8sServiceHost: ${NODE_IP}"
            echo "k8sServicePort: 6443"
        fi
        echo "ipam:"
        echo "  mode: ${ipam_mode}"
        if [ "$ipam_mode" = cluster-pool ]; then
            echo "  operator:"
            echo "    clusterPoolIPv4PodCIDRList:"
            echo "      - ${pod_cidr}"
            echo "    clusterPoolIPv4MaskSize: 24"
        fi
        # k3s 用 systemd cgroup driver；显式告诉 Cilium 不要自己去挂 cgroup
        echo "cgroup:"
        echo "  autoMount:"
        echo "    enabled: false"
        echo "  hostRoot: /sys/fs/cgroup"
        echo "operator:"
        echo "  replicas: 1"
        echo "hubble:"
        echo "  enabled: true"
        echo "  relay:"
        echo "    enabled: true"
        echo "  ui:"
        echo "    enabled: true"
    } > "$values"
    ok "values 写入 $values"

    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] helm upgrade --install cilium cilium/cilium -n kube-system --version $CILIUM_VERSION -f $values"
        info "[dry-run] values 预览："; sed 's/^/      | /' "$values" >&2
        return 0
    fi

    if ! helm repo list 2>/dev/null | grep -q '^cilium'; then
        helm repo add cilium https://helm.cilium.io/ >/dev/null
    fi
    helm repo update cilium >/dev/null 2>&1 || warn "helm repo update 失败，用本地缓存继续"

    helm upgrade --install cilium cilium/cilium \
        --namespace kube-system \
        --version "$CILIUM_VERSION" \
        --values "$values" \
        --wait --timeout 10m >/dev/null || {
            err "Cilium 安装失败。看：kubectl -n kube-system get pods; kubectl -n kube-system logs ds/cilium --tail=50"
            die "helm upgrade 返回非零"
        }
    ok "Cilium 已安装（helm release：cilium）"

    log "等 Cilium 各组件 rollout"
    for d in ds/cilium deploy/cilium-operator deploy/hubble-relay deploy/hubble-ui; do
        k -n kube-system rollout status "$d" --timeout=300s >/dev/null 2>&1 \
            && ok "$d 就绪" || warn "$d 未就绪（继续，稍后可 kubectl -n kube-system get pods 查）"
    done
}

expose_hubble_ui() {
    [ "$WITH_CILIUM" = 1 ] || return 0
    log "③ 暴露 Hubble UI"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] patch svc/hubble-ui 为 NodePort ${HUBBLE_NODEPORT}"
        return 0
    fi

    if ! k -n kube-system get svc hubble-ui >/dev/null 2>&1; then
        warn "没有 hubble-ui Service（Hubble UI 没装成功？）"
        return 0
    fi

    # 已经是 NodePort 就沿用它的端口，避免每次重跑都漂移
    local current_type current_port
    current_type="$(k -n kube-system get svc hubble-ui -o jsonpath='{.spec.type}')"
    current_port="$(k -n kube-system get svc hubble-ui -o jsonpath='{.spec.ports[0].nodePort}')"
    if [ "$current_type" = NodePort ] && [ -n "$current_port" ]; then
        HUBBLE_NODEPORT="$current_port"
        ok "已是 NodePort ${HUBBLE_NODEPORT}"
    else
        k -n kube-system patch svc hubble-ui --type merge \
            -p "{\"spec\":{\"type\":\"NodePort\",\"ports\":[{\"name\":\"http\",\"port\":80,\"targetPort\":8081,\"nodePort\":${HUBBLE_NODEPORT}}]}}" >/dev/null \
            && ok "已改为 NodePort ${HUBBLE_NODEPORT}" \
            || { warn "patch 失败（端口 ${HUBBLE_NODEPORT} 可能被占用）"; k -n kube-system get svc hubble-ui | sed 's/^/    /'; }
    fi

    # 另外建一个**独立的** NodePort Service。
    #
    # 为什么不直接 patch hubble-ui 那个 Service：它由 helm 管理，
    # 任何 `helm upgrade cilium` 都会把它重置回 ClusterIP（我们实测被还原过）。
    # 为什么不用 Traefik Ingress：k3s 的 servicelb 靠 hostPort 实现 LoadBalancer，
    # 而 kube-proxy 被 Cilium 替换后 hostPort 不再生效 —— 现象是 Traefik 的 Service
    # 拿到了 EXTERNAL-IP，但节点 80/443 根本没在监听（EXTERNAL-IP 是个假象）。
    # NodePort 由 Cilium 在 BPF 里实现，实测可用。
    if [ "$WITH_INGRESS" = 1 ]; then
        k apply -f - >/dev/null <<YAML || warn "创建 hubble-ui-nodeport 失败"
apiVersion: v1
kind: Service
metadata:
  name: hubble-ui-nodeport
  namespace: kube-system
  labels:
    app.kubernetes.io/part-of: k3s-wasm
spec:
  type: NodePort
  selector:
    k8s-app: hubble-ui
  ports:
    - name: http
      port: 80
      targetPort: 8081
      nodePort: ${HUBBLE_NODEPORT}
YAML
        info "说明：\"--no-ingress\" 会跳过这个独立 NodePort Service（HUBBLE 端口仍会被 patch 到 hubble-ui 上）"
    fi

    # 不管上面成不成，NodePort 这条必须验一下
    local code
    code="$(curl -sS -m 15 -o /dev/null -w '%{http_code}' "http://${NODE_IP}:${HUBBLE_NODEPORT}/" 2>/dev/null || echo 000)"
    if [ "$code" = 200 ]; then
        ok "NodePort 可用：http://${NODE_IP}:${HUBBLE_NODEPORT}/"
    else
        warn "NodePort 返回 $code（若刚 patch 完可稍等几秒再试）"
    fi
}

install_clis() {
    [ "$WITH_CLI" = 1 ] || return 0
    log "④ 安装 cilium / hubble CLI（排障用）"
    if [ "$DRY_RUN" = 1 ]; then info "[dry-run] 安装 cilium/hubble CLI"; return 0; fi
    local tmp; tmp="$(mktemp -d)"
    if ! have cilium; then
        if curl -fsSL -o "$tmp/cilium.tgz" \
            "https://github.com/cilium/cilium-cli/releases/download/${CILIUM_CLI_VERSION}/cilium-linux-${ARCH}.tar.gz"; then
            tar -xzf "$tmp/cilium.tgz" -C "$tmp" && install -m 0755 "$tmp/cilium" /usr/local/bin/cilium && ok "cilium CLI 已安装"
        else
            warn "cilium CLI 下载失败（不影响集群运行）"
        fi
    else
        ok "cilium CLI 已存在"
    fi
    if ! have hubble; then
        if curl -fsSL -o "$tmp/hubble.tgz" \
            "https://github.com/cilium/hubble/releases/download/${HUBBLE_CLI_VERSION}/hubble-linux-${ARCH}.tar.gz"; then
            tar -xzf "$tmp/hubble.tgz" -C "$tmp" && install -m 0755 "$tmp/hubble" /usr/local/bin/hubble && ok "hubble CLI 已安装"
        else
            warn "hubble CLI 下载失败（不影响集群运行）"
        fi
    else
        ok "hubble CLI 已存在"
    fi
    rm -rf "$tmp"
}

cilium_status() {
    [ "$WITH_CILIUM" = 1 ] || return 0
    log "⑤ 状态确认"
    if have cilium; then
        cilium status --wait --wait-duration 3m 2>&1 | sed 's/^/    /' || warn "cilium status 未通过，看上面的输出"
    else
        k -n kube-system get pods -l k8s-app=cilium -o wide 2>/dev/null | sed 's/^/    /'
    fi
    if [ "$CONNECTIVITY_TEST" = 1 ]; then
        log "跑 Cilium 连通性测试（会创建 cilium-test 命名空间，数分钟）"
        have cilium || die "需要 cilium CLI 才能跑连通性测试"
        cilium connectivity test --request-timeout 30s 2>&1 | tail -25 | sed 's/^/    /' \
            || warn "连通性测试有失败项，看上面输出"
    fi
}

# ════════════════════════════════════════════════════════════════════
# 可选：wasm 运行时（本仓库的另一半交付物）
# ════════════════════════════════════════════════════════════════════
maybe_wasm_runtime() {
    local auto=0
    [ -n "$WITH_WASM_RUNTIME" ] && auto=1
    if [ "$auto" = 0 ] && [ -x "$SCRIPT_DIR/install-wasm-runtime.sh" ]; then
        # 仓库在旁边时默认也装上（这就是「一键」的意义）
        auto=1
        info "检测到 scripts/install-wasm-runtime.sh，顺带安装 wasm32-wasip2 运行时"
    fi
    [ "$auto" = 1 ] || return 0
    [ -x "$SCRIPT_DIR/install-wasm-runtime.sh" ] || { warn "找不到 install-wasm-runtime.sh，跳过"; return 0; }

    log "⑥ 安装 wasm32-wasip2 运行时"
    if [ "$DRY_RUN" = 1 ]; then info "[dry-run] install-wasm-runtime.sh -y"; return 0; fi
    "$SCRIPT_DIR/install-wasm-runtime.sh" -y 2>&1 | sed 's/^/    /' || warn "wasm 运行时安装有问题，看上面输出"
}

# ════════════════════════════════════════════════════════════════════
# 卸载
# ════════════════════════════════════════════════════════════════════
do_uninstall() {
    log "卸载 k3s 与 Cilium"
    confirm "确认卸载这台机器上的 k3s（含数据）？" || die "已取消"
    if have helm && have k3s; then
        helm uninstall cilium -n kube-system >/dev/null 2>&1 || true
    fi
    if [ -x /usr/local/bin/k3s-uninstall.sh ]; then
        /usr/local/bin/k3s-uninstall.sh || warn "k3s-uninstall.sh 返回非零"
    fi
    rm -rf /etc/rancher "$CONF_DIR"
    ok "已卸载"
}

# ════════════════════════════════════════════════════════════════════
summary() {
    log "完成。访问信息"
    cat >&2 <<EOF

  Hubble UI（Cilium 的界面）
    · NodePort    : http://${NODE_IP}:${HUBBLE_NODEPORT}/
    · Ingress     : http://hubble.${NODE_IP}.nip.io/        （依赖 nip.io 解析，仅默认开启 Ingress 时）
    · 最稳的兜底  : kubectl -n kube-system port-forward svc/hubble-ui 12000:80
                   然后开 http://127.0.0.1:12000

  命令行
    · cilium status --wait
    · cilium hubble port-forward &   →  hubble observe --follow
    · hubble status

  kubeconfig
    · 节点上 : export KUBECONFIG=${KUBECONFIG_PATH}
    · 本机用 : ssh root@${NODE_IP} cat ${KUBECONFIG_PATH} > k3s.yaml
              然后把 server 改成 https://${NODE_IP}:6443

  参数留档
    · ${CONF_DIR}/k3s-exec-flags
    · ${CONF_DIR}/cilium-values.yaml
EOF
}

# ════════════════════════════════════════════════════════════════════
main() {
    preflight
    if [ "$UNINSTALL" = 1 ]; then do_uninstall; return 0; fi

    log "目标：k3s(${K3S_CHANNEL}) + $([ "$WITH_CILIUM" = 1 ] && echo "Cilium ${CILIUM_VERSION} + Hubble UI" || echo "默认 flannel")"
    info "节点 IP ${NODE_IP}，kube-proxy 替换：$([ "$KUBE_PROXY_REPLACEMENT" = 1 ] && echo on || echo off)"
    [ "$DRY_RUN" = 1 ] && warn "dry-run：不会真正改动系统"
    confirm "将在这台机器上安装/变更 k3s 与 Cilium，继续？" || die "已取消"

    install_k3s
    if [ "$WITH_CILIUM" = 1 ] && [ "$KUBE_PROXY_REPLACEMENT" = 1 ]; then
        # 关了 kube-proxy 时，节点要等 Cilium 才会 Ready
        install_cilium
        wait_node_ready 300 || warn "节点仍未 Ready —— 常见原因是 Cilium 没起来"
    else
        wait_node_ready 300 || true
        install_cilium
    fi
    expose_hubble_ui
    install_clis
    cilium_status
    maybe_wasm_runtime
    summary
}

main "$@"
