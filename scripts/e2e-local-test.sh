#!/usr/bin/env bash
# e2e-local-test.sh —— 在笔记本上对控制台后端做端到端测试。
#
# 它会被测试的**不是** mock，而是真实的 wasm 组件跑在真实的 wasm 宿主里：
#   宿主        ：优先 spin（SpinKube 用的就是 Spin 运行时），否则 wasmtime serve
#   被访问的 API：scripts/dev-mock-k8s-api.py
#
# 覆盖：静态资源、JSON 信封、k8s 聚合、SpinApp/隧道的增删改、日志、错误路径。
#
# 用法：
#   ./scripts/e2e-local-test.sh                 # 自动挑宿主
#   HOST=spin ./scripts/e2e-local-test.sh
#   KEEP_LOG=1 ./scripts/e2e-local-test.sh      # 失败时保留宿主日志
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib/common.sh"

PORT="${PORT:-8181}"
PROXY_PORT="${PROXY_PORT:-8399}"
HOST_KIND="${HOST:-auto}"
WASM="$REPO_DIR/ui/backend/target/wasm32-wasip2/release/k3s_wasm_ui.wasm"
BASE="http://127.0.0.1:${PORT}"

PASS=0; FAIL=0
ok_case()  { PASS=$((PASS+1)); printf '  %s✓%s %s\n' "$C_GRN" "$C_RST" "$1"; }
bad_case() { FAIL=$((FAIL+1)); printf '  %s✗%s %s\n' "$C_RED" "$C_RST" "$1"; printf '      %s\n' "$2"; }

[ -f "$WASM" ] || die "找不到 ${WASM}，先构建：cd ui/backend && cargo build --release --target wasm32-wasip2"

# ── 选宿主 ──────────────────────────────────────────────────────────
SPIN_BIN="$(command -v spin 2>/dev/null || true)"
if [ -z "$SPIN_BIN" ]; then
    SPIN_BIN="$(find "$REPO_DIR/.." -maxdepth 4 -type f -name spin -perm -u+x 2>/dev/null | head -1 || true)"
fi
if [ -z "${WASMTIME_BIN:-}" ]; then
    WASMTIME_BIN="$(command -v wasmtime 2>/dev/null || true)"
    [ -n "$WASMTIME_BIN" ] || WASMTIME_BIN="$(find "$REPO_DIR/.." -maxdepth 4 -type f -name wasmtime -perm -u+x 2>/dev/null | head -1 || true)"
fi

if [ "$HOST_KIND" = auto ]; then
    if [ -n "$SPIN_BIN" ]; then HOST_KIND=spin; else HOST_KIND=wasmtime; fi
fi

case "$HOST_KIND" in
    spin)     [ -n "$SPIN_BIN" ] || die "找不到 spin 二进制（装 spin 或设 HOST=wasmtime）" ;;
    wasmtime) [ -n "$WASMTIME_BIN" ] || die "找不到 wasmtime 二进制（装 wasmtime 或设 HOST=spin）" ;;
    *) die "HOST 只能是 spin / wasmtime / auto" ;;
esac

# ── 起 mock + 宿主 ──────────────────────────────────────────────────
# 预检端口：如果端口已被占用，说明有上一次的残留进程，
# 否则测试会打到旧实例上 —— 那种「改了代码却还是旧行为」的假象非常难查。
for p in "$PORT" "$PROXY_PORT"; do
    if command -v lsof >/dev/null 2>&1 && lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
        die "端口 $p 已被占用（可能是上次测试残留）：lsof -nP -iTCP:$p -sTCP:LISTEN
     清理：pkill -f 'spin up'（或 pkill -f wasmtime）"
    fi
done

MOCK_PID=""; HOST_PID=""
cleanup() {
    if [ -n "$HOST_PID" ]; then
        pkill -P "$HOST_PID" 2>/dev/null
        kill "$HOST_PID" 2>/dev/null
    fi
    [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT INT TERM

log "启动 mock k8s API :$PROXY_PORT"
python3 "$SCRIPT_DIR/dev-mock-k8s-api.py" --port "$PROXY_PORT" >/tmp/k3s-wasm-e2e-mock.log 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 40); do
    curl -fsS -o /dev/null "http://127.0.0.1:${PROXY_PORT}/version" 2>/dev/null && break
    sleep 0.25
done

log "启动 wasm 宿主：${HOST_KIND}（端口 ${PORT}）"
export K8S_PROXY_URL="http://127.0.0.1:${PROXY_PORT}"
export DISABLE_WASMTIME_CACHE=1     # 否则 wasmtime 会去写 ~/Library/Caches 并失败
export WASMTIME_CACHE_DIR="$REPO_DIR/.wasmtime-cache"

# 免密登录：门禁默认开启（失败关闭），所以 e2e 必须给出会话密钥，并自己签一个合法的
# 会话 cookie。格式与 ui/backend/src/auth.rs 一致：
#   b64url(JSON) + "." + b64url(HMAC-SHA256(secret, b64url(JSON)))
# 签名对不上只会得到「未登录」，看不出是格式问题 —— 所以这段刻意与实现一一对应。
export K3S_WASM_SESSION_SECRET="$(python3 -c 'import base64,os;print(base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip("="))')"
export K3S_WASM_REGISTRATION_CODE="e2e-registration-code"
export K3S_WASM_RP_ID="127.0.0.1"
export K3S_WASM_ORIGIN="http://127.0.0.1:${PORT}"
COOKIE="$(python3 - "$K3S_WASM_SESSION_SECRET" <<'PYCOOKIE'
import base64, hashlib, hmac, json, sys, time
secret_b64 = sys.argv[1]
secret = base64.urlsafe_b64decode(secret_b64 + "=" * (-len(secret_b64) % 4))
def b64(b): return base64.urlsafe_b64encode(b).decode().rstrip("=")
payload = b64(json.dumps({"kind": "session", "sub": "owner", "exp": int(time.time()) + 600}).encode())
sig = b64(hmac.new(secret, payload.encode(), hashlib.sha256).digest())
print("k3s_wasm_session=" + payload + "." + sig)
PYCOOKIE
)"
mkdir -p "$WASMTIME_CACHE_DIR" 2>/dev/null || true

if [ "$HOST_KIND" = spin ]; then
    # 必须先 cd 再后台起进程：写成 ( cd ... && spin ... ) & 的话 $! 是子 shell 的 pid，
    # kill 它杀不到 spin，spin 会变孤儿占着端口，下一次测试就打到旧实例上。
    cd "$REPO_DIR/ui/backend"
    "$SPIN_BIN" up -e "K8S_PROXY_URL=$K8S_PROXY_URL" \
        -e "K3S_WASM_SESSION_SECRET=$K3S_WASM_SESSION_SECRET" \
        -e "K3S_WASM_REGISTRATION_CODE=$K3S_WASM_REGISTRATION_CODE" \
        --listen "127.0.0.1:${PORT}" >/tmp/k3s-wasm-e2e-host.log 2>&1 &
    HOST_PID=$!
    cd "$REPO_DIR"
else
    "$WASMTIME_BIN" serve -C cache=n -S http=y -S inherit-network=y -S inherit-env=y \
        --addr "127.0.0.1:${PORT}" "$WASM" >/tmp/k3s-wasm-e2e-host.log 2>&1 &
    HOST_PID=$!
fi

for _ in $(seq 1 60); do
    curl -fsS -o /dev/null "$BASE/api/health" 2>/dev/null && break
    sleep 0.5
done
if ! curl -fsS -o /dev/null "$BASE/api/health" 2>/dev/null; then
    err "宿主没起来。日志："
    tail -25 /tmp/k3s-wasm-e2e-host.log | sed 's/^/    /'
    exit 2
fi
ok "宿主就绪（${HOST_KIND}）"

# ── 断言工具 ────────────────────────────────────────────────────────
# get_json <路径> -> 打印 body
# 带会话 cookie：除 /api/health 与 /api/auth/* 外，所有 /api 都要求已登录
get_json() { curl -sS -m 10 -H "Cookie: $COOKIE" "$BASE$1"; }
jq_ok()    { jq -e "$2" >/dev/null 2>&1; }

check_json() {
    local desc="$1" path="$2" expr="$3"
    local body; body="$(get_json "$path")"
    if printf '%s' "$body" | jq_ok - "$expr"; then
        ok_case "$desc"
    else
        bad_case "$desc" "GET $path → $(printf '%s' "$body" | head -c 300)（期望 jq: ${expr}）"
    fi
}

check_post() {
    local desc="$1" path="$2" payload="$3" expr="$4"
    local body; body="$(curl -sS -m 10 -X POST -H "Cookie: $COOKIE" -H 'content-type: application/json' -d "$payload" "$BASE$path")"
    if printf '%s' "$body" | jq_ok - "$expr"; then
        ok_case "$desc"
    else
        bad_case "$desc" "POST $path → $(printf '%s' "$body" | head -c 300)（期望 jq: ${expr}）"
    fi
}

check_delete() {
    local desc="$1" path="$2" expr="$3"
    local body; body="$(curl -sS -m 10 -X DELETE -H "Cookie: $COOKIE" "$BASE$path")"
    if printf '%s' "$body" | jq_ok - "$expr"; then
        ok_case "$desc"
    else
        bad_case "$desc" "DELETE $path → $(printf '%s' "$body" | head -c 300)（期望 jq: ${expr}）"
    fi
}

echo
log "① 自身状态与静态资源"
check_json "health 报告 wasm32-wasip2 与 wasi:http 接口" /api/health '.data.target=="wasm32-wasip2" and (.data.interface|test("incoming-handler"))'
check_json "health 的 proxyUrl 来自环境变量" /api/health '.data.proxyUrlSource=="env"'
check_json "health 报告免密登录已配置（含 rpId/origin）" /api/health '.data.auth.mode=="webauthn-passkey" and .data.auth.configured==true'

# ── 门禁：匿名 401 / 带会话 200 / 认证状态匿名可读 ──
ANON_CODE="$(curl -sS -m 10 -o /dev/null -w '%{http_code}' "$BASE/api/nodes")"
if [ "$ANON_CODE" = 401 ]; then
    ok_case "匿名访问 /api/nodes → 401（门禁生效）"
else
    bad_case "匿名访问 /api/nodes 应为 401" "拿到 HTTP ${ANON_CODE}"
fi
check_json "带会话 cookie 访问 /api/nodes → 200" /api/nodes '.ok==true'
AUTH_STATUS="$(curl -sS -m 10 "$BASE/api/auth/status")"
if printf '%s' "$AUTH_STATUS" | jq_ok - '.data.configured==true and .data.authenticated==false and .data.registered==false'; then
    ok_case "/api/auth/status 匿名可读：已配置/未登录/未绑定"
else
    bad_case "/api/auth/status 状态正确" "拿到：$(printf '%s' "$AUTH_STATUS" | head -c 200)"
fi

SPA="$(get_json /)"
if printf '%s' "$SPA" | grep -q 'id="app"'; then
    ok_case "GET / 返回真实 SPA（不是占位页）"
else
    bad_case "GET / 返回真实 SPA" "拿到：$(printf '%s' "$SPA" | head -c 200)"
fi

JS_PATH="$(printf '%s' "$SPA" | grep -oE 'assets/index\.[A-Za-z0-9_-]+\.js' | head -1)"
if [ -n "$JS_PATH" ]; then
    HDR="$(curl -sS -m 10 -D - -o /dev/null "$BASE/$JS_PATH")"
    if printf '%s' "$HDR" | grep -qi 'content-type: text/javascript' && printf '%s' "$HDR" | grep -qi 'max-age=31536000'; then
        ok_case "带 hash 的 JS 资源：mime 正确 + 长缓存"
    else
        bad_case "静态资源响应头" "$(printf '%s' "$HDR" | tr -d '\r' | head -6)"
    fi
else
    bad_case "在 index.html 里找到 assets/*.js" "index.html：$(printf '%s' "$SPA" | head -c 200)"
fi

echo
log "② 集群聚合"
check_json "summary 汇总节点（mock 有 2 个，1 个具备 wasm 能力）" /api/summary '.data.nodes.total==2 and .data.nodes.wasmCapable==1'
check_json "summary 统计 wasm Pod" /api/summary '.data.pods.wasm==2'
check_json "summary 报告 SpinApp CRD 已安装" /api/summary '.data.spinapps.installed==true'
check_json "summary 带出 k8s 版本" /api/summary '.data.version.gitVersion=="v1.33.1+k3s1"'
check_json "runtimes 列出两个 wasm 运行时且都未错配" /api/runtimes '[.data[]|select(.isWasm)]|length==2 and (map(.misconfigured)|any|not)'
check_json "nodes 读出 wasm 能力标签" /api/nodes '[.data[]|select(.wasm.spin)]|length==1'
check_json "pods 按命名空间过滤（只返回该命名空间，且确实是 3 个）" "/api/pods?namespace=k3s-wasm" '(.data|length)==3 and (.data|map(.namespace)|unique==["k3s-wasm"])'
check_json "pods 不带 namespace 时返回全部" "/api/pods" '.data|length==3'
check_json "pod 日志（text/plain 也要能取回）" /api/pods/k3s-wasm/k3s-wasm-ui-7d9f8b6c5-abcde/logs?tail=10 '.data.log|test("xt-wasm-cli")'
check_json "events 接口" "/api/events?namespace=k3s-wasm" '.data|length==2'
check_json "namespaces 接口" /api/namespaces '.data|length==3'

echo
log "③ SpinApp 全生命周期"
check_post "创建 SpinApp" /api/spinapps \
    '{"name":"e2e-spin","namespace":"k3s-wasm","image":"example.com/e2e:1","replicas":1,"executor":"containerd-shim-spin"}' \
    '.data.created==true'
check_json "列表里能看到它" "/api/spinapps?namespace=k3s-wasm" '[.data.items[]|select(.name=="e2e-spin")]|length==1'
check_post "扩容到 3" /api/spinapps/k3s-wasm/e2e-spin/scale '{"replicas":3}' '.data.replicas==3'
check_json "扩容结果已生效" "/api/spinapps?namespace=k3s-wasm" '[.data.items[]|select(.name=="e2e-spin" and .replicas==3)]|length==1'
check_delete "删除 SpinApp" /api/spinapps/k3s-wasm/e2e-spin '.data.deleted==true'
check_json "executor 列表接口" "/api/spinapp-executors?namespace=k3s-wasm" '.ok==true'

echo
log "④ xray 隧道"
check_json "隧道列表（mock 里有一条 tokyo）" "/api/xray/tunnels?namespace=k3s-wasm" '.data.items|length==1'
check_json "隧道披露 SOCKS5 入口与运行时" "/api/xray/tunnels?namespace=k3s-wasm" '.data.shim.runtimeClass=="wasmtime-wasip2"'
check_post "创建隧道（会下发 ConfigMap + Deployment + Service）" /api/xray/tunnels \
    '{"name":"e2e-tokyo","namespace":"k3s-wasm","server":"203.0.113.9:443","uuid":"11111111-2222-3333-4444-555555555555","publicKey":"PUBKEY","shortId":"abcd1234","sni":"www.example.com","listen":"0.0.0.0:1080","replicas":1,"socksUser":"e2e","socksPass":"e2e-pass"}' \
    '.data.created==true and (.data.socksEndpoint|test(":1080"))'
check_post "隧道扩容" /api/xray/tunnels/k3s-wasm/e2e-tokyo/scale '{"replicas":2}' '.ok==true'
check_delete "删除隧道（Deployment+Service+NetworkPolicy）" /api/xray/tunnels/k3s-wasm/e2e-tokyo '.data.deleted|index("deployment")!=null and index("service")!=null'

echo
log "⑤ 参数校验与错误路径（这些才是线上最容易出问题的地方）"
check_post "创建 SpinApp 缺 image → 400" /api/spinapps '{"name":"x","namespace":"k3s-wasm"}' '.ok==false and .error.status==400'
check_post "非法名称 → 400" /api/spinapps '{"name":"Bad_Name","namespace":"k3s-wasm","image":"a"}' '.error.status==400'
check_post "非法 listen → 400" /api/xray/tunnels \
    '{"name":"t","namespace":"k3s-wasm","server":"a:1","uuid":"u","publicKey":"p","listen":"nonsense"}' '.error.status==400'
check_json "未知接口 → JSON 404（不是 HTML）" /api/nope '.ok==false and .error.status==404'
check_delete "删除不存在的 SpinApp 是幂等的" /api/spinapps/k3s-wasm/never-existed '.ok==true'

echo
if [ "$FAIL" -eq 0 ]; then
    printf '%s全部通过：%d 项%s\n' "$C_GRN" "$PASS" "$C_RST"
    exit 0
fi
printf '%s失败 %d 项 / 通过 %d 项%s\n' "$C_RED" "$FAIL" "$PASS" "$C_RST"
if [ "${KEEP_LOG:-0}" = 1 ]; then
    echo "--- 宿主日志 ---"; tail -40 /tmp/k3s-wasm-e2e-host.log
    echo "--- mock 日志 ---"; tail -20 /tmp/k3s-wasm-e2e-mock.log
fi
exit 1
