#!/usr/bin/env bash
# verify-wasm-runtime.sh —— 在真集群上验证 wasm32-wasip2 工作负载真的能跑。
#
# 这是**唯一**能证明 containerd → shim → wasmtime 这条链通了的测试：
# 笔记本上能验证组件的逻辑（scripts/e2e-local-test.sh），
# 但 shim 链路只能在节点上验。
#
# 模式：
#   --mode http      跑控制台镜像（标准 wasi:http/proxy 组件，RuntimeClass wasmtime-wasip2，:8080）
#   --mode command   现场编 examples/hello-wasip2 成镜像，作为命令式 wasm 跑（同运行时，跑完即退）
#   --mode spinapp   创建一个 SpinApp（需要先跑 install-spinkube.sh）
#
# 用法：
#   ./scripts/verify-wasm-runtime.sh
#   ./scripts/verify-wasm-runtime.sh --mode command
#   IMAGE=k3s-wasm/k3s-wasm-ui:dev ./scripts/verify-wasm-runtime.sh --mode http
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"

MODE="${MODE:-auto}"
NS="${NS:-k3s-wasm-verify}"
UI_IMAGE="${IMAGE:-docker.io/k3s-wasm/k3s-wasm-ui:dev}"
CMD_IMAGE="${CMD_IMAGE:-docker.io/k3s-wasm/hello-wasip2:dev}"
SPINAPP_IMAGE="${SPINAPP_IMAGE:-docker.io/k3s-wasm/k3s-wasm-ui:dev}"
TIMEOUT="${TIMEOUT:-120}"
KEEP="${KEEP:-0}"
BUILD_IMAGE="${BUILD_IMAGE:-1}"

RC_HTTP="wasmtime-wasip2"
RC_SPIN="wasmtime-spin-v2"

usage() {
    sed -n '2,19p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --mode)    MODE="${2:?}"; shift 2 ;;
        --image)   UI_IMAGE="${2:?}"; shift 2 ;;
        --ns)      NS="${2:?}"; shift 2 ;;
        --keep)    KEEP=1; shift ;;
        --timeout) TIMEOUT="${2:?}"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) die "未知参数：$1" ;;
    esac
done

have kubectl || have k3s || die "需要 kubectl 或 k3s"

cleanup() {
    if [ "$KEEP" = 1 ]; then
        warn "保留命名空间 ${NS}（排查完：kubectl delete ns ${NS}）"
        return 0
    fi
    k3s_kubectl delete ns "${NS}" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ════════════════════════════════════════════════════════════════════
# 前置检查：这几项能覆盖大部分「wasm Pod 起不来」的原因
# ════════════════════════════════════════════════════════════════════
log "① 集群可达性"
k3s_kubectl get --raw /version >/dev/null 2>&1 || die "连不上集群，检查 KUBECONFIG"
ok "集群可达"

log "② 本机 containerd 配置（只有本脚本跑在节点上时才有意义）"
CFG="/var/lib/rancher/k3s/agent/etc/containerd/config.toml"
if [ -r "$CFG" ]; then
    for h in spin wasmtime; do
        if grep -qE "runtimes\.'?${h}'?\]" "$CFG" 2>/dev/null; then
            ok "已注册 runtime：${h}"
        else
            warn "未注册 runtime：${h}（这台节点没跑过 install-wasm-runtime.sh？）"
        fi
    done
    if grep -q 'SystemdCgroup' "$CFG" 2>/dev/null; then
        ok "看到 SystemdCgroup（Pod 指标会准）"
    else
        warn "没有 SystemdCgroup —— 若 Pod 指标异常，多半是这个原因"
    fi
else
    info "读不到 ${CFG}（不在节点上或权限不足），跳过"
fi

log "③ RuntimeClass 与节点标签"
k3s_kubectl get runtimeclass 2>/dev/null | sed 's/^/    /' || warn "没有 RuntimeClass"

HAS_HTTP=0
k3s_kubectl get runtimeclass "$RC_HTTP" >/dev/null 2>&1 && HAS_HTTP=1
HAS_SPIN=0
k3s_kubectl get runtimeclass "$RC_SPIN" >/dev/null 2>&1 && HAS_SPIN=1
[ "$HAS_HTTP" = 1 ] || warn "没有 RuntimeClass ${RC_HTTP}（http/command 模式需要它）"

nodes_json="$(k3s_kubectl get nodes -o json 2>/dev/null || echo '{"items":[]}')"

# RuntimeClass 的 nodeSelector 是否有节点满足 —— 这是最隐蔽的一类失败：
# Pod 会一直 Pending，或者 shim 报 "no runtime for ... is configured"。
check_selector() {
    local rc="$1"
    local sel
    sel="$(k3s_kubectl get runtimeclass "$rc" -o json 2>/dev/null | python3 -c '
import json,sys
try:
    rc = json.load(sys.stdin)
except Exception:
    sys.exit(0)
sel = ((rc.get("scheduling") or {}).get("nodeSelector")) or {}
print("\t".join(f"{k}={v}" for k, v in sel.items()))
' 2>/dev/null || true)"
    if [ -z "$sel" ]; then
        info "RuntimeClass ${rc} 没有 nodeSelector（任何节点都可能被调度到）"
        return 0
    fi
    local n
    n="$(printf '%s' "$nodes_json" | SEL="$sel" python3 -c '
import json,sys,os
sel = dict(kv.split("=",1) for kv in os.environ["SEL"].split("\t") if "=" in kv)
nodes = json.load(sys.stdin)["items"]
print(sum(1 for nd in nodes
          if all((nd["metadata"].get("labels") or {}).get(k)==v for k,v in sel.items())))
' 2>/dev/null || echo 0)"
    if [ "$n" = 0 ]; then
        warn "RuntimeClass ${rc} 的 nodeSelector（${sel}）匹配 0 个节点"
        info "修：在节点上跑 install-wasm-runtime.sh，或 kubectl label node <节点> ${sel}"
    else
        ok "RuntimeClass ${rc} 有 ${n} 个可用节点"
    fi
}

[ "$HAS_HTTP" = 1 ] && check_selector "$RC_HTTP"
[ "$HAS_SPIN" = 1 ] && check_selector "$RC_SPIN"

if [ "$MODE" = auto ]; then
    if [ "$HAS_HTTP" = 1 ]; then MODE=http; else MODE=spinapp; fi
    info "自动选择模式：${MODE}"
fi

k3s_kubectl create ns "${NS}" --dry-run=client -o yaml | k3s_kubectl apply -f - >/dev/null

# ════════════════════════════════════════════════════════════════════
case "$MODE" in
    http)
        log "④ 跑标准 wasi:http/proxy 组件（${UI_IMAGE}）"
        k3s_kubectl -n "${NS}" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: verify-http
  labels: { app: verify-http }
spec:
  runtimeClassName: ${RC_HTTP}
  restartPolicy: Never
  containers:
    - name: wasm
      image: ${UI_IMAGE}
      imagePullPolicy: IfNotPresent
      ports: [{ name: http, containerPort: 8080 }]
      env:
        - name: K8S_PROXY_URL
          value: http://kube-api-proxy.k3s-wasm.svc.cluster.local:8001
      readinessProbe:
        httpGet: { path: /api/health, port: 8080 }
        initialDelaySeconds: 2
        periodSeconds: 3
YAML
        if ! k3s_kubectl -n "${NS}" wait --for=condition=Ready pod/verify-http --timeout="${TIMEOUT}s"; then
            err "Pod 未就绪，状态与事件："
            k3s_kubectl -n "${NS}" describe pod verify-http | tail -30 | sed 's/^/    /'
            k3s_kubectl -n "${NS}" get events --sort-by=.lastTimestamp 2>/dev/null | tail -10 | sed 's/^/    /'
            die "http 模式失败。常见原因：镜像没导进节点 / shim 没装 / nodeSelector 不匹配 / 镜像里没有 wasm 模块"
        fi
        ok "Pod Ready —— 8080 上的 wasi:http 服务真的起来了"

        # 守卫：wasmtime shim 没有域名解析能力，用 DNS 名的出站会挂住。
        # 这个坑非常隐蔽（不报错、只是超时），所以在验证脚本里直接指出来。
        proxy_url="$(k3s_kubectl -n "${NS}" get pod verify-http -o jsonpath='{.spec.containers[0].env[?(@.name=="K8S_PROXY_URL")].value}' 2>/dev/null || true)"
        if [ -n "$proxy_url" ]; then
            host="$(printf '%s' "$proxy_url" | sed -E 's#^https?://##; s#[:/].*$##')"
            if ! printf '%s' "$host" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$'; then
                warn "K8S_PROXY_URL 用的是域名（$host）—— wasmtime shim 不给 guest 解析域名，"
                warn "出站请求会永久挂住且没有报错。请改成 IP（或固定 ClusterIP 的 Service）。"
            fi
        fi

        # 出站就绪要等一会：新建命名空间 + NetworkPolicy 后，Cilium 的 BPF 策略与
        # conntrack 需要几秒才铺完，此窗口内组件到代理的请求会挂住。
        # 这里显式重试，免得把「瞬态」误判成「组件有问题」。
        log "等组件的出站（到 kube-api-proxy）真的可用"
        k3s_kubectl -n "${NS}" port-forward pod/verify-http 18080:8080 >/tmp/k3s-wasm-pf.log 2>&1 &
        PF=$!
        for _ in $(seq 1 20); do
            curl -fsS -o /dev/null http://127.0.0.1:18080/api/health 2>/dev/null && break
            sleep 0.5
        done
        outbound_ok=0
        for attempt in $(seq 1 12); do
            if curl -fsS -m 8 -o /dev/null http://127.0.0.1:18080/api/nodes 2>/dev/null; then
                outbound_ok=1
                break
            fi
            sleep 5
        done
        if [ "$outbound_ok" = 1 ]; then
            ok "组件出站可用（第 ${attempt} 次尝试成功）"
        else
            kill $PF 2>/dev/null || true
            warn "组件的出站请求在 60s 内一直没成功。逐项查："
            warn "  1) K8S_PROXY_URL 必须是 IP（wasmtime shim 不给 guest 解析域名）"
            warn "  2) kube-api-proxy Pod 是否 Ready；NetworkPolicy 是否允许本 Pod"
            warn "  3) 再等几秒重试：刚建命名空间时 Cilium 策略还没铺完"
            die "出站不可用"
        fi
        kill $PF 2>/dev/null || true

        log "打一下 /api/health"
        k3s_kubectl -n "${NS}" port-forward pod/verify-http 18081:8080 >/tmp/k3s-wasm-pf2.log 2>&1 &
        PF2=$!
        for _ in $(seq 1 20); do
            curl -fsS -o /dev/null http://127.0.0.1:18081/api/health 2>/dev/null && break
            sleep 0.5
        done
        body="$(curl -sS -m 10 http://127.0.0.1:18081/api/health || true)"
        kill $PF2 2>/dev/null || true
        if printf '%s' "$body" | grep -q '"status":"ok"'; then
            ok "组件响应：$(printf '%s' "$body" | head -c 160)"
        else
            warn "拿到的响应：$(printf '%s' "$body" | head -c 200)"
            info "如果是 JSON 但内容不对，检查 kube-api-proxy 是否就绪"
        fi
        ;;

    command)
        log "④ 命令式 wasm：跑 examples/hello-wasip2"
        if [ "$BUILD_IMAGE" = 1 ]; then
            # 本机有 cargo + docker 时，全自动；否则用 BUILD_IMAGE=0 + 预先导入的镜像
            have cargo || die "需要 cargo（或用 BUILD_IMAGE=0 并预先导入 ${CMD_IMAGE}）"
            ( cd "$REPO_DIR/examples/hello-wasip2" && cargo build --release --target wasm32-wasip2 >/dev/null )
            WASM="$REPO_DIR/examples/hello-wasip2/target/wasm32-wasip2/release/hello-wasip2.wasm"
            [ -f "$WASM" ] || die "构建产物不存在：${WASM}"
            ok "wasm 产物 $(du -h "$WASM" | awk '{print $1}')"

            if have docker && docker info >/dev/null 2>&1; then
                CTX="$(mktemp -d)"
                cp "$WASM" "$CTX/hello.wasm"
                cat >"$CTX/Dockerfile" <<'EOF'
FROM scratch
COPY hello.wasm /hello.wasm
ENTRYPOINT ["/hello.wasm"]
EOF
                docker build -q -t "$CMD_IMAGE" "$CTX" >/dev/null
                rm -rf "$CTX"
                ok "镜像已构建：${CMD_IMAGE}"
                if have k3s; then
                        docker save "$CMD_IMAGE" | k3s ctr -n k8s.io images import - >/dev/null
                    ok "已导入节点 containerd"
                else
                    warn "本机没有 k3s，请把 ${CMD_IMAGE} 推到节点能拉到的仓库"
                fi
            else
                # 没有 docker 也能造镜像：make-wasm-image.py 直接写 OCI layout
                have python3 || die "需要 python3 来构造 OCI 镜像"
                TAR="$(mktemp -d)/hello.tar"
                python3 "$SCRIPT_DIR/make-wasm-image.py" --wasm "$WASM" --tag "$CMD_IMAGE" --out "$TAR" >/dev/null
                ok "已用 make-wasm-image.py 构造镜像 tar（无需 docker）"
                if have k3s; then
                    k3s ctr -n k8s.io images import "$TAR" >/dev/null && ok "已导入节点 containerd（k8s.io 命名空间）"
                else
                    warn "本机没有 k3s：请把 $TAR 拷到节点并 k3s ctr images import"
                fi
                rm -f "$TAR"
            fi
        else
            info "BUILD_IMAGE=0：假定 ${CMD_IMAGE} 已在节点 containerd 里"
        fi

        k3s_kubectl -n "${NS}" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: verify-command
spec:
  runtimeClassName: ${RC_HTTP}
  restartPolicy: Never
  containers:
    - name: wasm
      image: ${CMD_IMAGE}
      imagePullPolicy: IfNotPresent
YAML
        k3s_kubectl -n "${NS}" wait --for=jsonpath='{.status.phase}'=Succeeded pod/verify-command --timeout="${TIMEOUT}s" \
            || { k3s_kubectl -n "${NS}" describe pod verify-command | tail -25 | sed 's/^/    /'; die "命令式 wasm 没跑成功"; }
        echo "    ── wasm 输出 ─────────────────────────"
        k3s_kubectl -n "${NS}" logs verify-command | sed 's/^/    /'
        echo "    ──────────────────────────────────────"
        ok "命令式 wasm32-wasip2 在 k3s 上跑通"
        ;;

    spinapp)
        log "④ SpinApp 模式"
        [ "$HAS_SPIN" = 1 ] || die "没有 RuntimeClass ${RC_SPIN} —— 先跑 scripts/install-spinkube.sh 或 install-wasm-runtime.sh"
        k3s_kubectl get crd spinapps.core.spinkube.dev >/dev/null 2>&1 || die "没有 SpinApp CRD —— 先跑 scripts/install-spinkube.sh"

        # executor 是命名空间级的，验证命名空间里也要有一个
        if ! k3s_kubectl -n "${NS}" get spinappexecutor containerd-shim-spin >/dev/null 2>&1; then
            log "在 ${NS} 里创建 executor containerd-shim-spin"
            k3s_kubectl -n "${NS}" apply -f - >/dev/null <<YAML
apiVersion: core.spinkube.dev/v1alpha1
kind: SpinAppExecutor
metadata:
  name: containerd-shim-spin
  namespace: ${NS}
spec:
  createDeployment: true
  deploymentConfig:
    runtimeClassName: ${RC_SPIN}
    installDefaultCACerts: true
YAML
        fi

        k3s_kubectl -n "${NS}" apply -f - >/dev/null <<YAML
apiVersion: core.spinkube.dev/v1alpha1
kind: SpinApp
metadata:
  name: verify-spinapp
  namespace: ${NS}
spec:
  image: ${SPINAPP_IMAGE}
  executor: containerd-shim-spin
  replicas: 1
YAML
        if ! k3s_kubectl -n "${NS}" wait --for=condition=Ready spinapp/verify-spinapp --timeout="${TIMEOUT}s" 2>/dev/null; then
            warn "SpinApp 未在 ${TIMEOUT}s 内 Ready，现状："
            k3s_kubectl -n "${NS}" get spinapp verify-spinapp -o yaml 2>/dev/null | tail -25 | sed 's/^/    /'
            k3s_kubectl -n "${NS}" get pods -o wide 2>/dev/null | tail -10 | sed 's/^/    /'
            die "spinapp 模式失败。检查镜像是不是 spin registry push 产出的（普通 docker 镜像不行）"
        fi
        ok "SpinApp Ready"
        info "operator 会创建同名 Service（80 → http-app）：kubectl -n ${NS} port-forward svc/verify-spinapp 8083:80"
        ;;

    *) die "未知模式：${MODE}（可用：http / command / spinapp）" ;;
esac

log "验证结束"
