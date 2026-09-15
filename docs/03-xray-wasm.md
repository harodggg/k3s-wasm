# 03 · 在 k3s 上部署 xray-wasm（纯 wasm SOCKS5 出口代理）

`xray-wasm` 的 `xt-wasm-cli.wasm` 是一个 **命令式 wasm32-wasip2 组件**
（导出 `wasi:cli/run`，导入 `wasi:sockets/tcp@0.2.9`），所以它可以直接跑在
runwasi 的 **wasmtime shim** 上 —— 容器里不需要 wasmtime，运行时就是 containerd 的 shim。

本文记录实测结论、部署形状与连接方式。上游的部署须知在
[`deploy/k8s/README.md`](https://github.com/harodggg/xray-wasm/blob/main/deploy/k8s/README.md)。

## 1. 实测结论（k3s v1.36.4+k3s1 + containerd-shim-wasmtime-v1 v0.6.1）

| 项 | 结果 |
|---|---|
| 组件能否在 shim 上实例化 | ✅ 全部 `wasi:*@0.2.9` 导入被满足（含 `wasi:sockets/tcp`） |
| shim 是否给 guest **真 TCP** | ✅ `--self-test` 显示 `TCP 已连接` + `REALITY 握手完成，用时 11.75ms` |
| 完整隧道 | ✅ 集群内 curl 经 SOCKS5 出网，出口 IP 正确，HTTP 200（往返 0.18–0.25s） |
| SOCKS5 认证 | ✅ 无认证请求被拒绝（`curl: (97) No authentication method was acceptable`） |
| 加固项是否兼容 shim | ✅ `runAsUser: 10001` + `readOnlyRootFilesystem: true` + drop ALL 都能跑 |

> 顺带修正一个容易误推的结论：本仓库另一处记录的「wasmtime shim 不给域名解析」，
> 指的是 **`wasi:http` 出站**那条路（宿主侧连接）。**guest 裸 socket** 是另一套能力，
> 这个组件就导入了 `wasi:sockets/ip-name-lookup`。不过 `XT_SERVER` 本来就是 `ip:port`，用不上。

## 2. 部署形状（四条硬约束都来自上游须知）

1. **认证必填**：`XT_LISTEN=0.0.0.0:1080` 且不设 `XT_SOCKS_USER/PASS` = 开放代理。
2. **不用 NodePort / LoadBalancer**：Service 刻意 `ClusterIP`。
3. **叠加 NetworkPolicy**：认证管「谁能用」，它管「谁能连」；egress 收紧到服务端 IP。
4. **凭据走 Secret**：不要写进 `args`（`kubectl describe pod` 会原样打印）。全量选项都可用环境变量给：
   `XT_SERVER` `XT_UUID` `XT_PBK` `XT_SID` `XT_SNI` `XT_LISTEN` `XT_SOCKS_USER` `XT_SOCKS_PASS`
   `XT_CLIENT_VER` `XT_HANDSHAKE_TIMEOUT` `XT_SELF_TEST`。

清单见 `deploy/xray-wasm/`；控制台的「Xray 隧道」面板会生成同一套对象（含 Secret 与 NetworkPolicy）。

### 端口选择：非 443 容易被拦，但 443 要先从 Traefik 手里拿回来

k3s 自带的 Traefik 以 `LoadBalancer` Service 暴露，其 servicelb 声明的 **hostPort 80/443 由 Cilium 在 BPF 里实现 —— `ss` 看不到监听者**。
所以会出现这种诡异现象：把 REALITY 服务端绑到 443 上「成功」、`ss` 也显示它在听，
但客户端拿到的却是 Traefik 的默认证书（`CN=TRAEFIK DEFAULT CERT`）——
BPF 的 hostPort DNAT 抢在本地 socket 之前。

而 8443 这类非 443 端口虽然能用，在真实网络里更容易被针对（上游 README 也提醒过）。
**推荐做法：把 443 真正腾出来**（本仓库实测有效）：

```bash
# Traefik 的 LB 我们其实用不到（UI 都走 NodePort），改成 ClusterIP 后 svclb Pod 消失，
# BPF hostPort 规则随之移除，443 才真正空闲
kubectl -n kube-system patch svc traefik --type merge -p '{"spec":{"type":"ClusterIP"}}'
kubectl -n kube-system get pods | grep svclb            # 应为空
# 然后让 REALITY 服务端绑 443，并从公网侧验证：
#   openssl s_client -connect <ip>:443 -servername <伪装域名> | grep subject=
# 未认证客户端应看到**伪装站点的真实证书**（REALITY 回落），而不是 Traefik 的默认证书
```

想保留 Traefik 的 80/443 LB 又想自己用 443，就得换一台机器/换一个 IP 了。

## 3. 面板上的「自动生成参数」

新建隧道前不用手工凑参数——点 **① 自动生成参数**（等价于 `POST /api/xray/generate`），
它会一次给出配套的一整套：

| 产物 | 用途 |
|---|---|
| REALITY **X25519 密钥对** | 私钥给服务端 `privateKey`，公钥 `pbk` 给客户端 |
| UUID / shortId / SNI | 两端共用 |
| SOCKS5 用户名+密码 | 只用于**集群内**这条隧道的入口认证 |
| 服务端 `config.json` | 可直接粘到服务器（含 `dest` 回落站点、`flow=xtls-rprx-vision`） |
| 客户端 `config.json` | 官方 Xray 的等价配置，用来先验证服务端 |
| `vless://` 链接 | 导入官方客户端 / xrayTun |

实现要点：

- 随机数走宿主的 `wasi:random`（不引 getrandom/rand），密钥对用 `x25519-dalek` 在 wasm 里算
- **私钥只在响应里出现一次，控制台不保存、不写集群**；UI 上明确标红提示"别提交进 git"
- `b64url`、UUIDv4 位、`生成→解析` 自洽性、服务端配置内容都有单测覆盖

实测（真集群）：生成 → 用生成的**私钥**起一个 REALITY 服务端 → 用生成的**公钥**参数经面板建隧道
→ 经它出网 HTTP 200。也就是说生成的参数是配套可用的，不是"看着对"。

> 顺带修了一个真 bug：`parse_vless_link` 之前没剥 `#fragment`，导致最后一个 query 参数被污染
> （`flow=xtls-rprx-vision#Xray`）。是新增的「生成→解析」往返单测把它抓出来的 —— 之前那条
> 只断言了 uuid/pbk/sid/sni，恰好漏过 flow。

## 4. 列表里的「出站 / 入站」

隧道列表现在把两个方向分开显示，因为它们的含义完全不同：

| 方向 | 含义 | 谁决定 |
|---|---|---|
| **出站** | 流量从**集群内**出发 → 经 REALITY 服务端 → 目标。这是 `xray-wasm` 唯一能做的方向（它是**客户端**） | 架构决定，恒为出站 |
| **入站** | 这里是「**谁能连进**这条隧道的监听端口」 | 后端**读实际的 Service 类型**，不猜：`ClusterIP` → 仅集群内；`NodePort`/`LoadBalancer` → 列表标红「公网可达」 |

后端的 `ingress.exposure` 是**从集群真实读取**的，实测对照：

```
ClusterIP  → direction=egress public=false  reach=仅集群内可达
NodePort   → direction=egress public=true   reach=公网可达（NodePort 31080）   ← 会自动标红
```

> ⚠️ **真正的"入站隧道"（外部 → 集群内服务）这个面板做不到**，这是架构边界不是配置问题：
> `xray-wasm` 是出站客户端（SOCKS5 服务端 + REALITY 客户端），它无法接受来自公网的连接再转发进集群。
> 要做反向方向，需要服务端侧配合（例如服务端再跑一个反向代理/relay，或换成 frp/rathole 这类内网穿透），
> 那是另一套软件。所以列表里的「入站」只描述**暴露面**，不表示存在入站转发能力。

## 5. 已知限制（部署前必读）

- **一次只处理一条连接**：wasip2 没有线程，当前是顺序 accept。长连接客户端（HTTP/2、keep-alive）
  会独占一个 Pod。缓解：`replicas ≥ 2`。
- **探针会占用一次连接机会**：`tcpSocket` 探针会真的建连接（客户端日志里能看到协商失败），
  靠 `XT_HANDSHAKE_TIMEOUT` 丢弃。所以刻意**不配 livenessProbe**，readiness 也放得很宽。
- **不支持 UDP**：QUIC / HTTP3 不可用。
- **TLS 指纹不是浏览器形状**：未实现 uTLS Chrome 伪装，抗主动探测弱于官方客户端。
- **`XT_CLIENT_VER` 要对齐**服务端 `minClientVer/maxClientVer`（默认 `26.3.27`），
  设错的症状是「证书不是 Ed25519」。

## 6. 连接方式

部署后（假设 Service 名 `xray-wasm`、命名空间 `xray`、端口 1080）：

```bash
# ① 集群内 Pod 直接用（客户端务必用 socks5h，让服务端解析域名）
curl --proxy-user "$U:$P" --proxy socks5h://xray-wasm.xray.svc.cluster.local:1080 https://example.com

# ② 从你的机器用：kubectl port-forward（推荐，不暴露端口）
kubectl -n xray port-forward svc/xray-wasm 1080:1080
curl --proxy-user "$U:$P" --proxy socks5h://127.0.0.1:1080 https://api.ipify.org

# ③ 或者 SSH 隧道（不用 kubectl）
ssh -N -L 1080:$(kubectl -n xray get svc xray-wasm -o jsonpath='{.spec.clusterIP}'):1080 root@<node>

# ④ 浏览器 / 应用里填 SOCKS5 代理：127.0.0.1:1080 + 用户名/密码（选 SOCKS5，不要选 HTTP 代理）
```

> 为什么不做成 NodePort/LoadBalancer：那等于把你的出口代理公开给全网。
> 需要长期外部访问的话，用 WireGuard/Tailscale 之类把节点网络接通，再走 ② 或 ③。

## 7. 换到自己的服务端

```bash
kubectl -n xray create secret generic xray-wasm \
  --from-literal=XT_SERVER='<ip:port>' --from-literal=XT_UUID='<uuid>' \
  --from-literal=XT_PBK='<REALITY 公钥 base64url>' --from-literal=XT_SID='<shortId hex>' \
  --from-literal=XT_SNI='<伪装域名>' \
  --from-literal=XT_SOCKS_USER='<用户名>' --from-literal=XT_SOCKS_PASS='<密码>' \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl -n xray rollout restart deploy/xray-wasm
```

嫌手工拆字段麻烦：控制台面板支持直接粘贴 `vless://<uuid>@<ip:port>?pbk=...&sid=...&sni=...`，
后端会解析（`parse_vless_link`，带单测）。
