# 在 k3s 上部署 xray-wasm（真·wasm 工作负载）

把 [`xray-wasm`](https://github.com/harodggg/xray-wasm) 发布的 `xt-wasm-cli.wasm`
直接跑在 k3s 的 **wasmtime shim** 上（RuntimeClass `wasmtime-wasip2`），
作为集群内的 SOCKS5 出口代理；容器里**不含** wasmtime，运行时就是 containerd 的 shim。

> 上游也提供了把 wasmtime 打包进容器的镜像 `ghcr.io/harodggg/xray-wasm:latest`，
> 那种跑法不需要本仓库的 shim 链路。这里给的是「纯 wasm 工作负载」那条路。

## 前置

```bash
# 1) 节点侧：装 wasm 运行时（含 wasmtime shim）
sudo ./scripts/install-wasm-runtime.sh -y

# 2) 取发布包并校验（务必校验）
gh release download v0.1.0 -R harodggg/xray-wasm -p 'xt-wasm-cli.wasm*' -D /tmp
cd /tmp && shasum -a 256 -c xt-wasm-cli.wasm.sha256

# 3) 打成 OCI 镜像并导入节点（不需要 docker、不需要镜像仓库）
cd -   # 回到本仓库
python3 scripts/make-wasm-image.py --wasm /tmp/xt-wasm-cli.wasm \
    --tag docker.io/k3s-wasm/xray-wasm-cli:v0.1.0 --out /tmp/xray.tar
scp /tmp/xray.tar root@<node>:/tmp/ && ssh root@<node> \
    'k3s ctr -n k8s.io images import /tmp/xray.tar'
```

## 部署

```bash
# 凭据放 Secret（绝不要写进 args —— kubectl describe pod 会把 args 原样打印）
kubectl create ns xray
kubectl -n xray create secret generic xray-wasm \
  --from-literal=XT_SERVER='<服务端 ip:port>' \
  --from-literal=XT_UUID='<uuid>' \
  --from-literal=XT_PBK='<REALITY 公钥 base64url>' \
  --from-literal=XT_SID='<shortId hex>' \
  --from-literal=XT_SNI='<伪装域名>' \
  --from-literal=XT_SOCKS_USER='<代理用户名>' \
  --from-literal=XT_SOCKS_PASS='<代理密码>'

kubectl apply -f deployment.yaml -f service.yaml
kubectl -n xray rollout status deploy/xray-wasm
```

## 安全要求（和上游 `deploy/k8s/README.md` 一致，别跳）

1. **必须开认证**。`XT_LISTEN=0.0.0.0:1080` 且不设 `XT_SOCKS_USER/PASS` 就是**开放代理**。
2. **不要用 NodePort / LoadBalancer**。清单刻意是 `ClusterIP`；暴露到集群外等于公开你的出口。
3. **叠加 NetworkPolicy**：`networkpolicy.yaml` 只放行指定来源，并把 egress 收紧到服务端 IP。
4. **凭据走 Secret**（本目录的 Secret 由你创建；控制台面板也支持「引用已存在的 Secret」）。
5. `XT_CLIENT_VER` 要与服务端 `minClientVer/maxClientVer` 对齐，默认 `26.3.27`。

## 已知限制（上游文档，部署前必读）

- **一次只处理一条连接**：wasip2 没有线程，当前是顺序 accept。长连接客户端（HTTP/2、keep-alive）
  会独占一个 Pod。缓解：`replicas ≥ 2`，或用 sidecar 模式（本仓库没做 sidecar，但同理）。
- **探针会占用一次连接机会**：`tcpSocket` 探针会真的建连接，靠协商超时丢弃。
  所以清单里没有 `livenessProbe`，`readinessProbe` 也放得很宽。
- **不支持 UDP**：QUIC / HTTP3 经此代理不可用。
- **TLS 指纹不是浏览器形状**：未实现 uTLS Chrome 伪装，抗主动探测弱于官方客户端。

## 验证（本仓库在真机上跑过）

```bash
# 只做 REALITY 握手，最快确认「shim 给了 guest 真 TCP」
kubectl -n xray run self-test --rm --restart=Never \
  --image=docker.io/k3s-wasm/xray-wasm-cli:v0.1.0 \
  --overrides='{"spec":{"runtimeClassName":"wasmtime-wasip2","containers":[{"name":"cli","image":"docker.io/k3s-wasm/xray-wasm-cli:v0.1.0","args":["--self-test"],"envFrom":[{"secretRef":{"name":"xray-wasm"}}]}]}}'

# 经隧道出网（客户端务必 socks5h，让服务端解析域名）
kubectl -n xray run curl --rm -i --restart=Never --image=curlimages/curl:8.10.1 -- \
  curl -sS --proxy-user "$U:$P" --proxy socks5h://xray-wasm:1080 https://api.ipify.org
```
