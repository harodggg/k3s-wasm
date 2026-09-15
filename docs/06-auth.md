# 06 · 控制台登录：免密 Touch ID / passkey（WebAuthn）

控制台手里是 **kube-api-proxy 的权限**（能建删工作负载、读 Secret），所以它必须
①放在 HTTPS 后面 ②有鉴权。本项目选的是**纯免密**方案：WebAuthn（Mac 上就是
Touch ID，iPhone/iPad 是面容/指纹，其它平台是系统 passkey）。

没有口令库、没有撞库、没有明文口令过网 —— 代价是「首个凭据怎么来」需要一次
一次性注册码。

---

## 1. 两条硬前提（不满足就别往下看）

| 前提 | 为什么 |
|---|---|
| **必须 HTTPS + 浏览器信任的证书** | WebAuthn 只在**安全上下文**可用。明文 HTTP 上浏览器连 `navigator.credentials` 都不给，点击「用 Touch ID 登录」不会有任何反应。自签证书需导入钥匙串，否则也不算安全上下文。 |
| **RP ID / origin 必须与地址栏一致** | 校验的是 `rpIdHash = SHA256(rpId)` 与 `clientDataJSON.origin` 的**精确匹配**。用 `https://console.2.29.44.63.nip.io` 打开却把 `K3S_WASM_RP_ID` 配成别的域名，表现是「找不到凭据」或 401。 |

TLS 由 Traefik 的 Let's Encrypt 自动签发，配置见
`deploy/optional/traefik-acme.yaml`（用 **TLS-ALPN 还是 HTTP-01** 的坑见该文件注释）
与控制台入口 `deploy/base/ui-ingress.yaml`。

---

## 2. 部署（三步）

```bash
# ① 让 Traefik 能签发证书（acme.json 落 PVC，避免重启后重复签发撞 LE 限额）
kubectl apply -f deploy/optional/traefik-acme.yaml

# ② 会话密钥 + 一次性注册码
SESSION=$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')
CODE=$(openssl rand -hex 16)
kubectl -n k3s-wasm create secret generic k3s-wasm-ui-auth-env \
  --from-literal=sessionSecret="$SESSION" \
  --from-literal=registrationCode="$CODE"
echo "一次性注册码：$CODE"

# ③ 部署控制台（含 HTTPS 入口；不再暴露明文 NodePort）
kubectl apply -k deploy/overlays/shim-only
```

节点侧生成的注册码也存了一份在 `/root/k3s-wasm-ui-registration-code.txt`（600 权限），
免得只出现在聊天记录/终端滚动里。

---

## 3. 绑定与登录

1. 浏览器打开 `https://console.<你的域名>/`（**不是** IP 或 http）。
2. 首次：页面显示「绑定」界面 → 填一次性注册码 → 点「用 Touch ID 绑定」→
   系统弹 Touch ID → 完成。此后该注册码失效（再调注册接口直接 409）。
3. 之后：点「用 Touch ID 登录」→ 弹指纹 → 进入控制台。会话 12 小时。

---

## 4. 实现要点（为什么这么做）

### 4.1 组件无状态 → 状态全在签名 cookie 里

runwasi 是「一个请求一个实例」，组件里也不能依赖内存或文件系统。所以：

| 状态 | 存放 | 说明 |
|---|---|---|
| 会话 | `k3s_wasm_session` cookie | `base64url(JSON) + "." + HMAC-SHA256(JSON)`，12 小时过期 |
| 挑战 | `k3s_wasm_chal` cookie | 同上，5 分钟过期；`kind` 区分注册/登录，**不能混用** |
| 公钥凭据 | k8s Secret（`K3S_WASM_AUTH_SECRET`，默认 `k3s-wasm-ui-auth`） | 只存 credentialId + COSE 公钥 + 签名计数器 |

用 HMAC 而不是「服务端 session 表」：组件本来就没地方放表。签名密钥来自
`K3S_WASM_SESSION_SECRET`（必须 ≥16 字节，base64url 编码）。

### 4.2 失败关闭（fail closed）

`K3S_WASM_SESSION_SECRET` 没配好时，`/api/*` 一律 **503 并说明要配什么**，
而不是「配置不全就放行」。这个判断是刻意的：一个带着集群写权限的组件，
配置失误时唯一安全的默认是拒绝服务。`/api/health` 会如实报告认证配置状态。

### 4.3 验签清单（WebAuthn 最容易做漏的六处）

代码在 `ui/backend/src/auth.rs`，每条都有对应单测：

1. `clientDataJSON.type` 必须是 `webauthn.create` / `webauthn.get`（拿注册的响应来登录会被拒）。
2. `challenge` 必须等于我们签发的那个（防重放）。
3. `origin` 精确匹配。
4. `rpIdHash == SHA256(rpId)`（防「凭据被搬到另一个站点重放」）。
5. **必须同时置位 UP（用户在场）与 UV（用户验证）** —— 只查签名等于允许「碰一下」就登录。
6. 签名对象是 `authenticatorData || SHA256(clientDataJSON)`，且 ES256 签名是
   **ASN.1 DER** 而不是裸 `r||s`（直接按 64 字节切会验签失败）；签名计数器必须单调递增。

只接受 `attestation: none`：其它格式意味着要校验厂商证明链，本控制台不做
「看起来通过了但其实没验」的事，遇到直接拒绝并说明。

### 4.4 前端

登录页与守卫在 `ui/frontend/src/views-auth.ts` + `main.ts`：启动先问
`/api/auth/status`，未登录只渲染登录页；任何请求收到 401（会话过期）会退回登录页。

---

## 5. 环境变量一览

| 变量 | 必填 | 作用 |
|---|---|---|
| `K3S_WASM_SESSION_SECRET` | ✅ | 会话/挑战 cookie 的 HMAC 密钥（base64url，≥16 字节）；缺了就是 503 |
| `K3S_WASM_REGISTRATION_CODE` | 首次绑定需要 | 一次性注册码；不配则注册接口 503 |
| `K3S_WASM_RP_ID` | 建议 | WebAuthn RP ID（域名，不带端口）；不配则从 `Host` 推 |
| `K3S_WASM_ORIGIN` | 建议 | 允许的 origin（带 scheme）；不配则按 `https://<Host>` 推 |
| `K3S_WASM_AUTH_SECRET` | 可选 | 存公钥凭据的 Secret 名，默认 `k3s-wasm-ui-auth` |

---

## 6. 排障

| 现象 | 原因 | 处理 |
|---|---|---|
| 点按钮没反应 | 不是安全上下文 | 用 `https://` 打开；证书必须被信任（自签要导入钥匙串） |
| 401 / 「找不到凭据」 | RP ID 与地址栏域名不一致 | 对齐 `K3S_WASM_RP_ID` 与访问域名 |
| 注册时 409 | 已经绑定过了 | 删掉 `k3s-wasm-ui-auth` Secret 里的 `credential` 键再绑 |
| 注册时 400 `UV 标志未置位` | 认证器没做真的指纹/面容验证 | 换用平台认证器（不要用「仅存在性」的认证器） |
| 登录时 401 `签名计数器未递增` | 疑似凭据被克隆 | 已拒绝；确认设备安全后重新绑定 |
| `/api/*` 全都 503 | 没配 `K3S_WASM_SESSION_SECRET` | 按上面第 2 步建 Secret |
| 证书签发失败 | HTTP-01 被跳转截胡 / 端口 80 不可达 | 见 `deploy/optional/traefik-acme.yaml` 注释；确认 80 能被外网访问 |
