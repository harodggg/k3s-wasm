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

「入站 / 出站」指的是**谁在用这条隧道**，两者不互斥：

| 用途 | 谁在用 | 需要什么 |
|---|---|---|
| **出站** | 集群内的 Pod 把流量交给它出网 | ClusterIP 就够（默认） |
| **入站** | **你**从外部经「节点 IP」连进来当翻墙出口 | Service 要 `NodePort`/`LoadBalancer` **且** NetworkPolicy 放行来源；SOCKS5 认证必填 |

新建隧道时表单里选「用途」即可：

```
用途：○ 仅集群内（出站：集群里的 Pod 用它出网）
      ○ 允许外部经节点 IP（入站：我自己翻墙用）   ← 选它才会生成 NodePort
NodePort（可选）：留空自动分配  放行来源（外部用途时）：如 1.2.3.4/32，默认 0.0.0.0/0
```

创建后列表会显示能力位：`出站可用` + `入站可用（对外暴露）`/`仅集群内`，以及
`入站（外部经节点IP）` 一行（含 `socks5h://<节点IP>:<nodePort>` 与放行来源）。

⚠️ 两个实测坑（都已处理）：

1. **只把 Service 改成 NodePort 还不够**：我的 NetworkPolicy 默认只允许同命名空间 Pod 连接，
   外部经 NodePort 进来的流量会被 Cilium 丢掉，现象是「端口通、SOCKS5 没响应」。
   所以勾了入站用途时，NetworkPolicy 会额外放行一个来源 CIDR（`allowFrom`）。
2. **`allowFrom` 默认 `0.0.0.0/0` = 对全网开放**。请务必收窄到你的出口 IP，否则任何拿到凭据的人
   （或猜到弱密码的人）都能用你的出口。

### 客户端兼容性（选之前先看这条）

SOCKS5 **带用户名/密码**的认证，浏览器基本不支持（Firefox/Chrome 的代理设置里没有 SOCKS5 账密）。
所以：

| 你的客户端 | 能不能直连入站隧道 |
|---|---|
| `curl` / 大多数 CLI（含 `--proxy-user`） | ✅ 直接可用 |
| Telegram / 部分支持 SOCKS5 账密的应用 | ✅ |
| 浏览器 / 系统全局代理 | ❌ 需要本地再串一跳：`gost -L socks5://127.0.0.1:1080 -F socks5://user:pass@<节点IP>:<nodePort>` |
| xrayTun / 官方 Xray 客户端 | 它们吃 `vless://`，直接连你的 REALITY 服务端即可（不需要经这条隧道）|

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

- **并发：v0.2 起已不再是"一次一条连接"**（这条早期结论已过时，此处修正）。
  上游 v0.2 改为非阻塞并发（`MAX_CONCURRENT_CONNS = 64`），v0.3 进一步用 `wasi:io/poll`
  的 pollable 就绪通知替代轮询 —— README 的 A/B 实测：v0.1 顺序 accept 时长连接会把新请求
  卡到时超时（12s），v0.2+ 是 HTTP 200 / 1s；空闲 CPU 也从 ~0.78% 降到 ~0.05%。
  所以 `replicas=2` 是"分摊"而不是"必须"，长连接**不会**独占代理。
- **探针仍会真的建一条 TCP 连接**（客户端日志里能看到 SOCKS5 协商失败/early eof），
  由 `XT_HANDSHAKE_TIMEOUT` 丢弃；但不再会卡住后续请求。清单保持"不配 livenessProbe、
  readiness 放宽"的保守设置即可。
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

## 7. 面板新增的两个能力

| 能力 | 说明 |
|---|---|
| **按需展示 vless 链接** | 列表里点「显示 vless 链接」→ `GET /api/xray/tunnels/:ns/:name/vless`，从该隧道**自己的 Secret** 重建链接。不含 REALITY 私钥；链接里的地址优先取部署注解 `k3s-wasm/public-server`（对外地址），而隧道客户端实际连的是 Secret 里的 `XT_SERVER`（可能是 ClusterIP） |
| **生成参数分「入站/出站」** | `POST /api/xray/generate` 增加 `usage: cluster\|nodeport`，返回里除了服务端/客户端配置，还分别给出 `outbound`（集群内 Pod 怎么用：环境变量、curl 示例）与 `inbound`（外部怎么用：NodePort 端点、curl 示例、浏览器不支持的场景给 `gost` 本地中转示例） |

读 Secret 的权限是**命名空间级 Role**（`k3s-wasm-tunnel-secrets`，只 `get`），不是集群级
`get secrets` —— 后者等于能读全集群密钥。在别的命名空间建隧道时，把这份 Role/RoleBinding 复制过去。

## 8. 两种模式：翻墙（reality → socket）vs 隧道（socket → reality）—— 都是 wasm

| 模式 | 入站 | 出站 | 实现 | 链接里的 `flow` |
|---|---|---|---|---|
| **翻墙** | **reality**（NodePort，pod 内 8443） | **socket 直连**（走所选节点自己的网络，无第二跳） | 同一个 wasm 组件，`XT_MODE=server` | **必须不带** |
| **隧道** | **socket / SOCKS5**（ClusterIP / NodePort，1080） | **reality**（连远端服务端） | 同一个 wasm 组件，默认（客户端） | 上游是 stock Xray 时可带 `xtls-rprx-vision` |

### 8.1 为什么两者能共用一个模块

`xray-wasm` **v0.4.0** 起，一个 `xt-wasm-cli.wasm` 同时是客户端与服务端（**v0.5.0** 起客户端支持 `--no-flow`，用于连本工程的服务端）：

```sh
# 客户端（隧道模式）：本地 SOCKS5 → REALITY 出
xt-wasm-cli --server <远端 ip:port> --pbk <公钥> --sid <shortId> --sni <域名> --uuid <uuid> [--listen 0.0.0.0:1080]

# 服务端（翻墙模式）：REALITY 入 → 直连目标；未认证流量原样转发到 dest
xt-wasm-cli server --private-key <base64url> --short-ids <hex> --server-names <域名> \
                   --dest <真实 TLS 站点:443> --users <uuid> [--listen 0.0.0.0:8443]
```

服务端所有参数都有对应**环境变量**（`XT_MODE=server` / `XT_PRIVATE_KEY` / `XT_SHORT_IDS` /
`XT_SERVER_NAMES` / `XT_DEST` / `XT_USERS` / `XT_SERVER_LISTEN`），所以控制台把凭据整包放进
Secret、用 `envFrom` 注入，**args 里不放任何凭据**（`kubectl describe pod` 会打印 args）。

### 8.2 三条实测约束（决定面板怎么生成链接）

1. **翻墙链接不带 `flow`**：xray-wasm 服务端**尚未实现** XTLS-Vision 流控，带非空 `flow`
   的客户端会被**明确拒绝**（服务端日志：`Not supported: 服务端尚未实现 Vision 流控（客户端请求了
   flow=xtls-rprx-vision）`），不是静默降级。面板据此生成无 flow 的链接与客户端配置。
2. **入口用 NodePort，不用 hostPort**：本集群命名空间带 PodSecurity `baseline` 强制，
   `hostPort` 会被准入直接拒掉（实测 `violates PodSecurity "baseline:latest": hostPort`）。
3. **调度需要 wasm 节点**：翻墙模式跑的是 wasm 组件，而 `wasmtime-wasip2` 这个 RuntimeClass
   自带 `nodeSelector: wasm.sh/wasmtime=true`；选到没有该标签的节点会一直 Pending，
   所以控制台在创建前就校验并给出可读的 400。

### 8.3 真机验证（k3s v1.36.4 + Cilium 1.20.1）

| 项目 | 结果 |
|---|---|
| 发布产物 sha256 校验 | ✅ `xt-wasm-cli.wasm` v0.4.0 |
| `server` 子命令在**发布产物**里 | ✅ `server --help` 参数完整 |
| stock Xray 客户端（flow 空）→ wasm 服务端 → 直连出 | ✅ 出口 = 节点 IP（`wasmtime run` 与 k3s shim 两种宿主都通过） |
| flow 非空 | ✅ 明确拒绝（见 8.2 第 1 条） |
| 未认证探测 | ✅ 集群内与公网都看到 `dest` 的真实证书（回落行为，抗主动探测） |
| k3s 交付 | ✅ `wasmtime-wasip2` 下 1/1 Running；guest stdout 进 `kubectl logs`；NodePort 公网可达 |
| **shim 内 DNS** | ✅ 目标用域名也能通（这一条曾是未知项：`wasi.rs` 里写明域名解析需要宿主开 `allow-ip-name-lookup`，实测 shim 提供了） |

### 8.4 与官方核心的关系

官方 Xray 核心（Go）编不到 wasip2（Go 只支持 `wasip1`，而 wasip1 标准库层面发不出站 TCP），
所以「REALITY 服务端也是 wasm」这件事只能由 xray-wasm 自己实现 —— 也就是 v0.4.0 做的。
节点上那套 systemd + 官方二进制（`/opt/xray-test`）现在只是历史遗留，不再是翻墙模式的唯一路径。

## 9. 镜像现在是下拉可选

「Xray 隧道」与「Spin 应用」两个表单的**镜像**字段都改成了「下拉建议 + 可手输」（HTML `datalist`）。
候选来自 `GET /api/images?wasmOnly=1|0`，即**集群里正在运行的 Pod 所用镜像**：

```
?wasmOnly=1 → docker.io/k3s-wasm/xray-wasm-cli:v0.1.0  (wasm 次数=6, wasmtime-wasip2)
              docker.io/k3s-wasm/k3s-wasm-ui:dev        (wasm 次数=2, wasmtime-wasip2)
?wasmOnly=0 → 再加系统组件镜像（cilium / klipper-lb / hubble-relay …，共 14 个）
```

为什么用「在跑的镜像」而不是 containerd 的完整镜像列表：CRI 没有把镜像列表暴露成 k8s API，
组件只能通过 kube-api-proxy 说话；而**已经在跑的镜像一定已被节点拉取或导入**，
选它不会遇到 `pull access denied`/`ImagePullBackOff` —— 这正是之前踩过的坑。

拉不到候选时不影响手输；`defaults` 里也放了两条常见镜像兜底。

## 10. 镜像下拉：按版本 tag 选

「Xray 隧道」表单的镜像字段是一个 `datalist`，候选按三个来源合并、**版本优先**：

| 来源 | 内容 | 为什么可信 |
|---|---|---|
| ① `GET /api/image-tags?repo=harodggg/xray-wasm` | `docker.io/k3s-wasm/xray-wasm-cli:v0.3.0 / v0.2.0 / v0.1.0…` | tag 取自 **GitHub releases**，发新版本下拉自动多一项 |
| ② `GET /api/images?wasmOnly=1` | 集群里正在跑的 wasm 镜像 | 一定已在节点上，选了不会 pull 失败（标「集群已有」） |
| ③ `defaults` | 兜底两条 | 集群里暂时没有 wasm 工作负载时下拉不为空 |

实测：`tags: ['v0.2.0', 'v0.1.0']`；两个版本的镜像都已导入节点，
且 **v0.2.0 的模块在 wasmtime shim 下实测可用**（起隧道 → 出网 HTTP 200）。

⚠️ 踩坑记录：**GitHub API 对没有 `User-Agent` 的请求直接返回 403**。
组件最初不带 UA，表现是"取 tag 失败：GitHub API 返回 403"，很容易误判成限流或 HTTPS 不通；
节点上 `curl`（自带 UA）就是 200。已在 `k8s::fetch_absolute` 里固定带上 UA 与 `accept` 头。

另外注意：`ghcr.io/harodggg/xray-wasm:<tag>` 是**容器镜像**（里面自带 wasmtime），
不能配 wasmtime shim 用；shim 需要的是纯 wasm 模块镜像（`docker.io/k3s-wasm/xray-wasm-cli:<tag>`）。

## 11. 换到自己的服务端

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
