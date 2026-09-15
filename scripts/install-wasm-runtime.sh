#!/usr/bin/env bash
# install-wasm-runtime.sh —— 让 k3s 节点能跑 wasm32-wasip2 工作负载。
#
# 做四件事：
#   1. 下载 containerd wasm shim 二进制到 /usr/local/bin（默认：spin + wasmtime 两个）
#   2. 写 k3s 的 containerd 配置模板，注册 runtime 并打开 SystemdCgroup
#   3. 给节点打标签（RuntimeClass.scheduling 用它把 wasm Pod 钉在有 shim 的节点上）
#   4. 重启 k3s、建 RuntimeClass、等待节点 Ready
#
# ── 关于 k3s 的一个重要事实（决定了本脚本为什么这么写）────────────────
#   k3s 启动时会用 exec.LookPath 在服务 PATH 里找这些二进制：
#     containerd-shim-spin-v2      → runtime 名 spin     / runtime_type io.containerd.spin.v2
#     containerd-shim-wasmtime-v1  → runtime 名 wasmtime / runtime_type io.containerd.wasmtime.v1
#   找到就**自动**写进 containerd 配置，并自动建 RuntimeClass `spin` / `wasmtime`。
#   所以 /usr/local/bin 是正确位置（在 k3s 文档列的搜索路径里），
#   而 /var/lib/rancher/k3s/data/current/bin 不是 —— 那是 k3s 自己的版本化载荷目录，
#   不在搜索路径里，升级就变。
#
#   那为什么还要写模板？为了 `SystemdCgroup = true`：
#   runwasi 系 shim 默认走 cgroupfs，与 k3s 的 systemd cgroup driver 不一致时
#   Pod 指标会不准。auto-detect 生成的 stanza 不带这个选项，所以要显式写。
#   不想动模板可以加 --no-template（只靠 auto-detect）。
#
#   ⚠️ containerd 2.x 的插件域是 `io.containerd.cri.v1.runtime`，模板文件名是
#      config-v3.toml.tmpl；1.7 及更早才是 config.toml.tmpl + io.containerd.grpc.v1.cri。
#      k3s v1.31.6+/v1.32.2+ 起内置 containerd 2.0，现代 k3s 都走 v3。
#      脚本会自动探测；写错文件名的后果是「改了没生效」且没有任何报错。
#
# 用法：
#   sudo ./scripts/install-wasm-runtime.sh                    # spin + wasmtime
#   sudo ./scripts/install-wasm-runtime.sh --shims wasmtime    # 只装 wasmtime
#   sudo ./scripts/install-wasm-runtime.sh --dry-run
#   sudo ./scripts/install-wasm-runtime.sh --uninstall
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
. "$SCRIPT_DIR/lib/common.sh"

# ── 固定版本（可用参数覆盖）──────────────────────────────────────────
SPIN_SHIM_REPO="${SPIN_SHIM_REPO:-spinframework/containerd-shim-spin}"
SPIN_SHIM_VERSION="${SPIN_SHIM_VERSION:-v0.25.1}"
# runwasi 的 release tag 带 crate 前缀，URL 里就是带斜杠的
WASMTIME_SHIM_TAG="${WASMTIME_SHIM_TAG:-containerd-shim-wasmtime/v0.6.1}"
WASMTIME_SHIM_REPO="${WASMTIME_SHIM_REPO:-containerd/runwasi}"

# 默认版本的 sha256（取自上游 release 资产）
declare -A PINNED_SHA=(
    ["containerd-shim-spin-v2-linux-x86_64.tar.gz"]="1755fbeb2dec7d026faf8c37031d8a025975f5e194b782da10188206a386d6e4"
    ["containerd-shim-spin-v2-linux-aarch64.tar.gz"]="5ca9c9207a146a182dc9c870eaaad1afbf64e1d9a4781228dbf6e7ef90ff2e1f"
    ["containerd-shim-wasmtime-x86_64-linux-musl.tar.gz"]="a9b1215ee670f11414c8fd8e970b52679085929c8ec73a0fe6ed9cf215cf9ba1"
    ["containerd-shim-wasmtime-aarch64-linux-musl.tar.gz"]="0ae922715dd484a923825fd4fa25052fe00f8769d29c236704f1f5638f735ae0"
)

SHIM_BIN_DIR="${SHIM_BIN_DIR:-/usr/local/bin}"
K3S_AGENT_DIR="${K3S_AGENT_DIR:-/var/lib/rancher/k3s/agent/etc/containerd}"
MARK_BEGIN="# >>> k3s-wasm managed block >>>"
MARK_END="# <<< k3s-wasm managed block <<<"

SHIMS="spin,wasmtime"
ACTION="install"
NO_RESTART=0
WRITE_TEMPLATE=1
APPLY_RUNTIMECLASSES=1
FORCE_TMPL=0
NODE_LABEL="${K3S_WASM_LABEL:-}"

usage() {
    sed -n '2,44p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    cat >&2 <<'EOF'
选项：
  --shims spin,wasmtime      要安装的运行时（默认两个都装）
  --spin-version vX.Y.Z      containerd-shim-spin 版本（tag 形如 v0.25.1）
  --wasmtime-tag TAG         runwasi tag（形如 containerd-shim-wasmtime/v0.6.1）
  --bin-dir DIR              shim 安装目录（默认 /usr/local/bin，必须在 k3s 的 PATH 里）
  --no-template              不写 containerd 模板，只靠 k3s 自动探测（会丢掉 SystemdCgroup）
  --no-restart               不重启 k3s
  --no-runtimeclass          不创建 RuntimeClass
  --label KEY=VALUE[,KEY=V]  附加到本节点的标签（默认按 shim 自动生成）
  --force-template           模板里没有 {{ template "base" . }} 时也继续追加
  --dry-run                  只打印动作
  -y, --yes                  跳过确认
  --uninstall                卸载 shim 与托管配置块
  -h, --help
EOF
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --shims)            SHIMS="${2:?}"; shift 2 ;;
        --spin-version)     SPIN_SHIM_VERSION="${2:?}"; shift 2 ;;
        --wasmtime-tag)     WASMTIME_SHIM_TAG="${2:?}"; shift 2 ;;
        --bin-dir)          SHIM_BIN_DIR="${2:?}"; shift 2 ;;
        --no-template)      WRITE_TEMPLATE=0; shift ;;
        --no-restart)       NO_RESTART=1; shift ;;
        --no-runtimeclass)  APPLY_RUNTIMECLASSES=0; shift ;;
        --label)            NODE_LABEL="${2:?}"; shift 2 ;;
        --force-template)   FORCE_TMPL=1; shift ;;
        --dry-run)          DRY_RUN=1; shift ;;
        -y|--yes)           ASSUME_YES=1; shift ;;
        --uninstall)        ACTION="uninstall"; shift ;;
        -h|--help)          usage 0 ;;
        *) die "未知参数：$1（--help 看用法）" ;;
    esac
done

ARCH="$(detect_arch)"
K3S_SERVICE="$(require_k3s)"
require_root
[ -n "$K3S_SERVICE" ] || K3S_SERVICE="k3s"

has_shim() { case ",$SHIMS," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

# ════════════════════════════════════════════════════════════════════
# 上游资产名（两个仓库的命名风格不同，这是最容易写出 404 的地方）
# ════════════════════════════════════════════════════════════════════
spin_asset() {
    case "$ARCH" in
        amd64) echo "containerd-shim-spin-v2-linux-x86_64.tar.gz" ;;
        arm64) echo "containerd-shim-spin-v2-linux-aarch64.tar.gz" ;;
        *) die "spin shim 只提供 x86_64 / aarch64 资产，当前架构：$ARCH" ;;
    esac
}

wasmtime_asset() {
    case "$ARCH" in
        amd64) echo "containerd-shim-wasmtime-x86_64-linux-musl.tar.gz" ;;
        arm64) echo "containerd-shim-wasmtime-aarch64-linux-musl.tar.gz" ;;
        *) die "wasmtime shim 只提供 x86_64 / aarch64 资产，当前架构：$ARCH" ;;
    esac
}

# ════════════════════════════════════════════════════════════════════
# 安装 / 卸载二进制
# ════════════════════════════════════════════════════════════════════

# install_shim <标签> <URL> <资产名> <期望sha> <要装的二进制名>
install_shim() {
    local label="$1" url="$2" asset="$3" want_sha="$4" bin_name="$5"
    local tmpdir; tmpdir="$(mktemp -d)"

    log "安装 $label"
    info "$url"
    if ! download "$url" "$tmpdir/$asset"; then
        rm -rf "$tmpdir"
        die "下载失败：$url
     404 说明版本号或资产名不对。资产名规律：
       spin shim    : containerd-shim-spin-v2-linux-{x86_64,aarch64}.tar.gz
       wasmtime shim: containerd-shim-wasmtime-{x86_64,aarch64}-linux-musl.tar.gz
     两者风格**不一样**（前者架构在后、后者在前还带 musl），照抄另一个必然 404。"
    fi

    if [ -n "$want_sha" ]; then
        verify_sha256 "$tmpdir/$asset" "$want_sha" || { rm -rf "$tmpdir"; die "sha256 校验失败，已中止"; }
        ok "sha256 校验通过"
    else
        warn "自定义版本没有内置 sha256，跳过校验（建议自己核对上游 checksums）"
    fi

    tar -xzf "$tmpdir/$asset" -C "$tmpdir"
    local bin
    bin="$(find "$tmpdir" -type f -name "$bin_name" | head -1)"
    [ -n "$bin" ] || { rm -rf "$tmpdir"; die "压缩包里找不到 ${bin_name}（资产结构可能变了）"; }

    run install -D -m 0755 "$bin" "$SHIM_BIN_DIR/$bin_name"
    ok "$bin_name → $SHIM_BIN_DIR/$bin_name"
    rm -rf "$tmpdir"
}

install_spin_shim() {
    local asset url sha
    asset="$(spin_asset)"
    url="https://github.com/${SPIN_SHIM_REPO}/releases/download/${SPIN_SHIM_VERSION}/${asset}"
    sha=""
    [ "$SPIN_SHIM_VERSION" = "v0.25.1" ] && sha="${PINNED_SHA[$asset]:-}"
    install_shim "containerd-shim-spin-v2 $SPIN_SHIM_VERSION" "$url" "$asset" "$sha" containerd-shim-spin-v2
}

install_wasmtime_shim() {
    local asset url sha
    asset="$(wasmtime_asset)"
    url="https://github.com/${WASMTIME_SHIM_REPO}/releases/download/${WASMTIME_SHIM_TAG}/${asset}"
    sha=""
    [ "$WASMTIME_SHIM_TAG" = "containerd-shim-wasmtime/v0.6.1" ] && sha="${PINNED_SHA[$asset]:-}"
    install_shim "containerd-shim-wasmtime-v1 $WASMTIME_SHIM_TAG" "$url" "$asset" "$sha" containerd-shim-wasmtime-v1
}

remove_bin() {
    local f="$1"
    if [ -e "$f" ] || [ -L "$f" ]; then
        log "删除 $f"
        run rm -f "$f"
        return 0
    fi
    return 1
}

uninstall_runtime() {
    log "卸载 wasm 运行时"
    local removed=0
    if has_shim spin; then remove_bin "$SHIM_BIN_DIR/containerd-shim-spin-v2" && removed=1 || true; fi
    if has_shim wasmtime; then remove_bin "$SHIM_BIN_DIR/containerd-shim-wasmtime-v1" && removed=1 || true; fi

    local tmpl
    for tmpl in "$K3S_AGENT_DIR/config-v3.toml.tmpl" "$K3S_AGENT_DIR/config.toml.tmpl"; do
        [ -f "$tmpl" ] || continue
        grep -qF "$MARK_BEGIN" "$tmpl" || continue
        log "从 $tmpl 移除托管配置块"
        if [ "$DRY_RUN" = 1 ]; then
            info "[dry-run] 删除 $tmpl 中的托管块"
        else
            cp -a "$tmpl" "$tmpl.bak.$(date +%Y%m%d%H%M%S)"
            awk -v b="$MARK_BEGIN" -v e="$MARK_END" '
                $0==b {skip=1}
                skip==0 {print}
                $0==e {skip=0}
            ' "$tmpl" >"$tmpl.new"
            mv "$tmpl.new" "$tmpl"
        fi
        removed=1
    done

    if [ "$removed" = 1 ]; then
        restart_k3s
        log "RuntimeClass 不会自动删除，需要的话手动删："
        info "kubectl delete runtimeclass wasmtime-wasip2 wasmtime-spin-v2"
    else
        info "没有发现本脚本安装的东西"
    fi
}

# ════════════════════════════════════════════════════════════════════
# containerd 配置模板
# ════════════════════════════════════════════════════════════════════

# 返回 v3 或 v2：决定模板文件名与插件域。
detect_containerd_config_version() {
    local cfg="$K3S_AGENT_DIR/config.toml"
    if [ -f "$cfg" ] && grep -qE '^[[:space:]]*version[[:space:]]*=[[:space:]]*3' "$cfg"; then
        echo v3; return
    fi
    if [ -f "$K3S_AGENT_DIR/config-v3.toml.tmpl" ]; then
        echo v3; return
    fi
    local kv
    kv="$(k3s --version 2>/dev/null | sed -n 's/.*k3s version \(v[0-9.]*\).*/\1/p' | head -1)"
    case "$kv" in
        v1.31.6*|v1.31.[7-9]*|v1.3[2-9]*|v2*) echo v3 ;;
        *) echo v2 ;;
    esac
}

managed_block() {
    local plugin_domain="$1"
    cat <<EOF
${MARK_BEGIN}
# 由 k3s-wasm/scripts/install-wasm-runtime.sh 生成，请勿手改这一段。
# runtime 名（右侧的键）必须与 deploy/base/runtimeclass-*.yaml 的 handler 一致：
#   spin     → RuntimeClass wasmtime-spin-v2（SpinKube 默认 executor 认这个名字）
#   wasmtime → RuntimeClass wasmtime-wasip2（裸 wasi:http / 命令式 wasm 组件用）
EOF
    if has_shim spin; then
        cat <<EOF

[plugins.${plugin_domain}.containerd.runtimes.spin]
  runtime_type = "io.containerd.spin.v2"
  [plugins.${plugin_domain}.containerd.runtimes.spin.options]
    SystemdCgroup = true
EOF
    fi
    if has_shim wasmtime; then
        cat <<EOF

[plugins.${plugin_domain}.containerd.runtimes.wasmtime]
  runtime_type = "io.containerd.wasmtime.v1"
  [plugins.${plugin_domain}.containerd.runtimes.wasmtime.options]
    SystemdCgroup = true
EOF
    fi
    cat <<EOF

${MARK_END}
EOF
}

install_containerd_config() {
    [ "$WRITE_TEMPLATE" = 1 ] || { warn "按 --no-template 跳过模板；SystemdCgroup 不会设置"; return 0; }

    local ver plugin_domain tmpl
    ver="$(detect_containerd_config_version)"
    if [ "$ver" = v3 ]; then
        tmpl="$K3S_AGENT_DIR/config-v3.toml.tmpl"
        plugin_domain="'io.containerd.cri.v1.runtime'"
    else
        tmpl="$K3S_AGENT_DIR/config.toml.tmpl"
        plugin_domain='"io.containerd.grpc.v1.cri"'
    fi
    info "探测到 containerd 配置世代：${ver}（模板 ${tmpl}）"

    local block; block="$(managed_block "$plugin_domain")"

    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] 将确保 $tmpl 含以下内容："
        printf '%s\n' "$block" | sed 's/^/      | /' >&2
        return 0
    fi

    install -d -m 0755 "$K3S_AGENT_DIR"

    if [ ! -f "$tmpl" ]; then
        # 必须调用 {{ template "base" . }}，否则 k3s 的默认 CRI 配置会全部丢失。
        # k3s 官方建议「基于 base 模板扩展」，而不是把 config.toml 拷成模板再改。
        {
            printf '# 由 k3s-wasm 生成。\n'
            printf '{{ template "base" . }}\n\n'
            printf '%s\n' "$block"
        } >"$tmpl"
        ok "模板已创建：$tmpl"
        return 0
    fi

    if ! grep -qF '{{ template "base" . }}' "$tmpl"; then
        if [ "$FORCE_TMPL" != 1 ]; then
            die "$tmpl 已存在但没有 {{ template \"base\" . }}。
     直接追加会丢掉 k3s 默认配置。请把这一行放到文件首行，或加 --force-template 强制继续。"
        fi
        warn "模板缺少 base 调用，按 --force-template 继续"
    fi

    cp -a "$tmpl" "$tmpl.bak.$(date +%Y%m%d%H%M%S)"
    if grep -qF "$MARK_BEGIN" "$tmpl"; then
        awk -v b="$MARK_BEGIN" -v e="$MARK_END" -v block="$block" '
            $0==b {print block; skip=1}
            skip==0 {print}
            $0==e {skip=0}
        ' "$tmpl" >"$tmpl.new"
        mv "$tmpl.new" "$tmpl"
        ok "托管块已更新"
    else
        printf '\n%s\n' "$block" >>"$tmpl"
        ok "托管块已追加"
    fi
}

# ════════════════════════════════════════════════════════════════════
# 标签 / RuntimeClass / 重启
# ════════════════════════════════════════════════════════════════════
label_node() {
    local label="$NODE_LABEL"
    if [ -z "$label" ]; then
        if has_shim spin && has_shim wasmtime; then
            label="wasm.sh/spin=true,wasm.sh/wasmtime=true"
        elif has_shim spin; then
            label="wasm.sh/spin=true"
        else
            label="wasm.sh/wasmtime=true"
        fi
    fi
    [ -n "$label" ] || return 0

    local node="${K3S_NODE_NAME:-$(hostname | tr '[:upper:]' '[:lower:]')}"
    log "给节点 $node 打标签 $label"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] kubectl label node $node $label --overwrite"
        return 0
    fi
    # shellcheck disable=SC2086  # 逗号分隔的多个 k=v 需要词分割
    if k3s_kubectl label node "$node" ${label//,/ } --overwrite >/dev/null 2>&1; then
        ok "标签已生效（RuntimeClass.scheduling 依赖它）"
    else
        warn "打标签失败（节点名可能不是 $(hostname)）。请手动执行："
        warn "  kubectl label node <节点名> ${label//,/ } --overwrite"
    fi
}

runtimeclass_yaml() {
    local name="$1" handler="$2" selector_key="$3"
    cat <<EOF
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: ${name}
  labels:
    app.kubernetes.io/part-of: k3s-wasm
handler: ${handler}
scheduling:
  nodeSelector:
    ${selector_key}: "true"
EOF
}

apply_runtimeclasses() {
    [ "$APPLY_RUNTIMECLASSES" = 1 ] || return 0
    log "创建 RuntimeClass"
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] kubectl apply RuntimeClass wasmtime-spin-v2 / wasmtime-wasip2"
        return 0
    fi

    if has_shim spin; then
        if runtimeclass_yaml wasmtime-spin-v2 spin wasm.sh/spin | k3s_kubectl apply -f - >/dev/null 2>&1; then
            ok "RuntimeClass wasmtime-spin-v2（handler spin）"
        else
            warn "创建 wasmtime-spin-v2 失败（集群还没就绪？稍后可重跑本脚本）"
        fi
    fi
    if has_shim wasmtime; then
        if runtimeclass_yaml wasmtime-wasip2 wasmtime wasm.sh/wasmtime | k3s_kubectl apply -f - >/dev/null 2>&1; then
            ok "RuntimeClass wasmtime-wasip2（handler wasmtime）"
        else
            warn "创建 wasmtime-wasip2 失败"
        fi
    fi
}

restart_k3s() {
    [ "$NO_RESTART" = 1 ] && { warn "按 --no-restart 跳过重启，请自行重启 $K3S_SERVICE"; return 0; }
    if [ "$DRY_RUN" = 1 ]; then
        info "[dry-run] systemctl restart $K3S_SERVICE"
        return 0
    fi
    log "重启 ${K3S_SERVICE}（auto-detect 与模板渲染都发生在启动时）"
    systemctl restart "$K3S_SERVICE" || die "重启失败，看 journalctl -u $K3S_SERVICE -n 100"
    info "等待节点 Ready…"
    local i=0
    while [ $i -lt 60 ]; do
        if k3s_kubectl get nodes 2>/dev/null | grep -q ' Ready'; then
            ok "节点已 Ready"
            verify_containerd_config
            return 0
        fi
        sleep 2; i=$((i + 1))
    done
    warn "等了 120s 仍未 Ready，检查：journalctl -u $K3S_SERVICE -n 200"
}

# 重启后回读 k3s 生成的 config.toml，确认 runtime 真的注册进去了。
# 这一步很值：模板文件名/插件域写错时的现象就是「静默不生效」。
verify_containerd_config() {
    local cfg="$K3S_AGENT_DIR/config.toml"
    [ -f "$cfg" ] || return 0
    local missing=0
    if has_shim spin && ! grep -q 'runtimes\.spin' "$cfg"; then
        warn "config.toml 里没有 spin runtime —— 模板可能写错了文件名/插件域"; missing=1
    fi
    if has_shim wasmtime && ! grep -q 'runtimes\.wasmtime' "$cfg"; then
        warn "config.toml 里没有 wasmtime runtime —— 同上"; missing=1
    fi
    if [ "$missing" = 0 ]; then
        ok "containerd 配置已确认包含所需 runtime"
    else
        info "排查：grep -A3 runtimes $cfg"
    fi
}

# ════════════════════════════════════════════════════════════════════
main() {
    if [ "$ACTION" = "uninstall" ]; then
        uninstall_runtime
        return 0
    fi

    log "k3s wasm 运行时安装：shims=$SHIMS arch=$ARCH service=$K3S_SERVICE"
    [ "$DRY_RUN" = 1 ] && warn "dry-run：不会真正改动系统"

    if ! confirm "将安装 shim 到 $SHIM_BIN_DIR 并重启 ${K3S_SERVICE}，继续？"; then
        die "已取消"
    fi

    # 先装二进制：k3s 的 auto-detect 依赖它们在 PATH 里
    has_shim spin && install_spin_shim
    has_shim wasmtime && install_wasmtime_shim

    install_containerd_config
    restart_k3s
    label_node
    apply_runtimeclasses

    log "完成。下一步："
    cat >&2 <<EOF
    kubectl get runtimeclass
    ./scripts/verify-wasm-runtime.sh          # 端到端验证（会真跑一个 wasm 工作负载）
EOF
    info "只有装了 shim 的节点才能跑 wasm Pod；RuntimeClass.scheduling 已把 Pod 钉到对应标签的节点。"
    info "SpinKube 路线（SpinApp CRD + operator）见 scripts/install-spinkube.sh —— 它走 RCM，handler 是 spin-v2，别再手建同名 RuntimeClass。"
}

main "$@"
