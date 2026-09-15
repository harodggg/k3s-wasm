# 05 · 一键 k3s + Cilium + Hubble UI

`scripts/install-k3s-cilium.sh` 把一台干净的 Linux 机器变成「k3s + Cilium（替换 flannel/kube-proxy）+ Hubble UI」，
可选顺带装上 wasm32-wasip2 运行时。本文记录它做了什么、真实跑出来的结果，以及踩到的三个交互坑。

## 1. 用法

```bash
sudo ./scripts/install-k3s-cilium.sh                  # 全默认
sudo ./scripts/install-k3s-cilium.sh --dry-run        # 只看计划
sudo ./scripts/install-k3s-cilium.sh --connectivity-test
sudo ./scripts/install-k3s-cilium.sh --no-cilium      # 只要 k3s（默认 flannel）
sudo ./scripts/install-k3s-cilium.sh --uninstall
```

它会（幂等，重复执行只做增量）：

1. 前置检查：root / 架构 / cgroup v2 / bpffs / 外网 / swap / 探测节点 IP
2. 装 k3s（`--flannel-backend=none --disable-network-policy --disable-kube-proxy`，因为这三件事交给 Cilium）
3. 装 helm（若缺），用 Helm 装 Cilium（Hubble + relay + UI），values 落到 `/etc/k3s-wasm/cilium-values.yaml`
4. 等 Cilium / operator / hubble-relay / hubble-ui rollout，等节点 Ready
5. 暴露 Hubble UI：patch `hubble-ui` 为 NodePort + **额外建一个独立的 `hubble-ui-nodeport` Service**
6. 装 `cilium` / `hubble` CLI
7. `cilium status --wait`，可选连通性测试
8. 若仓库在旁边，自动跑 `install-wasm-runtime.sh`（装 spin + wasmtime shim）

参数留档：`/etc/k3s-wasm/k3s-exec-flags`、`/etc/k3s-wasm/cilium-values.yaml`（事后审计「当初装了什么」）。

## 2. 实测结果（Hetzner VM · Ubuntu 26.04 · x86_64 · 8 vCPU/15G）

| 项 | 结果 |
|---|---|
| k3s | `v1.36.4+k3s1`，单节点 control-plane，Ready |
| Cilium | `1.20.1`，`cilium status` 全绿；kube-proxy 替换开启；Pod IP 来自 cluster-pool（10.42.x） |
| Hubble UI | `http://<node-ip>:30080/` → **HTTP 200**（页面标题 `Hubble UI`） |
| 控制台 | `http://<node-ip>:30081/` → **HTTP 200**，`/api/summary` 返回真实集群数据 |
| wasm 运行时 | 装完两个 shim 后节点出现 `wasmtime` / `spin` runtime，RuntimeClass `wasmtime-wasip2` / `wasmtime-spin-v2` 各有 1 个可用节点 |

浏览器直接开这两个 NodePort 即可。

## 3. 三个坑（按重要性）

### 3.1 k3s 的 servicelb 在 Cilium kube-proxy 替换下是「假入口」

k3s 的 servicelb（klipper-lb）用 **hostPort** 实现 `type: LoadBalancer`：

```
$ kubectl -n kube-system get svc traefik
NAME      TYPE           CLUSTER-IP   EXTERNAL-IP   PORT(S)
traefik   LoadBalancer   10.43.7.135  2.29.44.63    80:32122/TCP,443:32581/TCP
$ ss -tlnp | grep -E ':80 |:443 '      # 节点上
（什么都没有）
```

`EXTERNAL-IP` 有值，但节点 **80/443 根本没在监听** —— kube-proxy 被替换后 hostPort 不再由它处理。
给 Cilium 打开 `hostPort.enabled=true` 在本次环境里也没救活它（且 `helm upgrade` 会把
手动 patch 的 Service 重置回 ClusterIP）。

**处置**：用 **NodePort**（Cilium 在 BPF 里实现，实测正常）。本仓库的 Hubble UI 和控制台都走 NodePort。
如果确实要 Ingress：把 Traefik 改成 hostNetwork 并让它直接监听 80/443（注意它会和 svclb 抢 hostPort，
需要先让 servicelb 不再管理 Traefik），或者换一个不依赖 hostPort 的 ingress controller。

### 3.2 wasmtime shim 不给 wasm guest 域名解析 —— 而且失败方式很坏

控制台组件访问 `http://kube-api-proxy.k3s-wasm.svc.cluster.local:8001` 时：

```
$ time curl -m 60 .../api/summary      # 用 DNS 名
curl: (28) Operation timed out after 60001 milliseconds with 0 bytes received
$ # 改用 ClusterIP 直连
real 0m0.741s                           # 立刻返回真实集群数据
```

对照实验证明集群 DNS 本身没问题（busybox Pod 能正常解析该名字）。
所以限制在 **guest 侧**：shim 没给 wasm 打开 `wasi:sockets/ip-name-lookup`，
而「解析不了」的表现不是报错，而是**请求永久挂住**。

**处置**：组件用 **IP 字面量**。为此把 `kube-api-proxy` 的 ClusterIP **固定**为 `10.43.0.53`
（在 k3s 默认 service CIDR 10.43.0.0/16 内），组件默认值与 Deployment 的 env 都用这个 IP。
换 service CIDR 的集群要同步改这两处（见 `deploy/base/kube-api-proxy.yaml` 的注释）。
`verify-wasm-runtime.sh` 里加了一道守卫：发现 `K8S_PROXY_URL` 是域名就明确告警。

### 3.3 不要给 k3s 写 containerd runtime 模板（会和 auto-detect 撞表）

早期版本的 `install-wasm-runtime.sh` 会写一个 `config-v3.toml.tmpl` 来补 `SystemdCgroup`。
实际后果是把节点搞成 containerd 起不来：

```
containerd: failed to unmarshal TOML: toml: table spin already exists
→ k3s 只会告诉你 "重启失败"
```

原因是 **k3s 的 auto-detect 本身就已经写全了**：

```toml
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.'spin']
  runtime_type = "io.containerd.spin.v2"
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.'spin'.options]
  BinaryName = "/usr/local/bin/containerd-shim-spin-v2"
  SystemdCgroup = true          ← k3s 自己就写了
```

**处置**：默认不写模板（只把 shim 放到 `/usr/local/bin` 交给 auto-detect）。
只有 shim 不在 k3s 搜索路径里（`--bin-dir /opt/...`）时才用 `--template`，
而且脚本会在「会撞表」时拒绝执行。另外脚本现在会用 k3s 自带的 containerd
`config dump` 校验生成的配置 —— 这类错一秒就能发现。

## 4. 与本仓库其余部分的衔接

```bash
# 一键：k3s + Cilium + Hubble UI + wasm 运行时
sudo ./scripts/install-k3s-cilium.sh -y

# 验证 wasm 真的能跑（需先把镜像导进节点）
./scripts/verify-wasm-runtime.sh --mode command
./scripts/verify-wasm-runtime.sh --mode http

# 部署控制台（NodePort 30081）
kubectl apply -k deploy/overlays/shim-only
```

没有镜像仓库时，用 `scripts/make-wasm-image.py` 本地造 OCI 镜像（不需要 docker），
再 `k3s ctr -n k8s.io images import`。**注意两点**（都实测踩过）：

- 必须导入到 `k8s.io` 命名空间，否则 kubelet 看不到；
- 镜像名用**完全限定名** `docker.io/...`：短名会让 CRI 找不到镜像，进而去真拉取并报
  `pull access denied`（即使镜像就在本地）。
