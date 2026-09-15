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

### 端口选择：别用 80/443

k3s 自带的 Traefik 以 `LoadBalancer` Service 暴露，其 servicelb 声明的 **hostPort 80/443 由 Cilium 在 BPF 里实现 —— `ss` 看不到监听者**。
本仓库实测：把 REALITY 服务端放在 443 上「绑定成功」，但客户端拿到的却是 Traefik 的默认证书
（`CN=TRAEFIK DEFAULT CERT`）。所以测试服务端用 **8443**。
要在同一台机器上用 443，先让 Traefik 不再占用它（改 hostNetwork 并显式绑 80/443，或换 LB 方案）。

## 3. 已知限制（部署前必读）

- **一次只处理一条连接**：wasip2 没有线程，当前是顺序 accept。长连接客户端（HTTP/2、keep-alive）
  会独占一个 Pod。缓解：`replicas ≥ 2`。
- **探针会占用一次连接机会**：`tcpSocket` 探针会真的建连接（客户端日志里能看到协商失败），
  靠 `XT_HANDSHAKE_TIMEOUT` 丢弃。所以刻意**不配 livenessProbe**，readiness 也放得很宽。
- **不支持 UDP**：QUIC / HTTP3 不可用。
- **TLS 指纹不是浏览器形状**：未实现 uTLS Chrome 伪装，抗主动探测弱于官方客户端。
- **`XT_CLIENT_VER` 要对齐**服务端 `minClientVer/maxClientVer`（默认 `26.3.27`），
  设错的症状是「证书不是 Ed25519」。

## 4. 连接方式

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

## 5. 换到自己的服务端

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
