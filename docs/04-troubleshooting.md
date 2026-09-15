# 04 · 报错 → 原因 → 处置

按「你在哪一步看到它」分组。每条都是本仓库实际遇到或针对实际链路写下的。

## A. 构建阶段（本机）

| 报错 | 原因 | 处置 |
|---|---|---|
| `error[E0463]: can't find crate for 'core'` / `the wasm32-wasip2 target may not be installed` | 用的是 Homebrew 的 rustc，它没有 wasm32-wasip2 sysroot（系统里 rustup 的 shim 才是对的，但 Homebrew 在 PATH 里更靠前） | `export PATH="$HOME/.cargo/bin:$PATH"`，或直接用 `scripts/build-ui.sh`（它已经处理） |
| `failed to write cache ... Operation not permitted`（cargo） | `~` 不可写 | 把 `CARGO_HOME` 指到工作区内（`export CARGO_HOME=$PWD/../.cargo`） |
| `Error: component imports instance 'wasi:cli/environment@0.2.x', but a matching implementation was not found in the linker` | **wasi-http 补丁版本不匹配**：组件按某个 `0.2.N` 生成，宿主只实现了别的版本 | 换 `wasi` crate 版本让两边对齐（`0.14`→0.2.12、`0.13`→0.2.0）；用 `wasm-tools component wit <wasm> \| grep import` 看组件版本 |
| `Error: failed to create cache directory: .../Library/Caches/BytecodeAlliance.wasmtime` | wasmtime / Spin 的 JIT 缓存目录不可写 | `wasmtime serve -C cache=n`；或 `export DISABLE_WASMTIME_CACHE=1`（Spin） |

## B. 节点侧（containerd / shim）

| 报错/现象 | 原因 | 处置 |
|---|---|---|
| `failed to create shim task: no runtime for io.containerd.spin.v2 is configured` | 该节点没装 shim，或 containerd 配置没生效，或 Pod 被调度到别的节点 | `grep runtimes /var/lib/rancher/k3s/agent/etc/containerd/config.toml`；确认 shim 在 `/usr/local/bin`；`kubectl get runtimeclass <名> -o jsonpath='{.scheduling}'` 看 nodeSelector 是否匹配节点标签 |
| 改了模板但「什么都没发生」 | 模板文件名/插件域写错（containerd 2.x 要 `config-v3.toml.tmpl` + `'io.containerd.cri.v1.runtime'`） | `scripts/install-wasm-runtime.sh` 会自动探测并回读确认；手工改的话见 `docs/01-runtime.md` §2 |
| `Pod 一直 Pending`（事件里 nodeSelector 不匹配） | RuntimeClass 的 `scheduling.nodeSelector` 没有节点满足 | `kubectl label node <节点> wasm.sh/wasmtime=true`（或 `wasm.sh/spin=true`） |
| 装完 shim 没有任何变化 | k3s 只在**启动时**探测 shim | `sudo systemctl restart k3s`（worker 节点是 `k3s-agent`） |
| 下载 shim 404 | 两个仓库的资产命名风格不同：spin 是 `containerd-shim-spin-v2-linux-x86_64.tar.gz`，runwasi 是 `containerd-shim-wasmtime-x86_64-linux-musl.tar.gz` | 见 `scripts/install-wasm-runtime.sh` 里的 `spin_asset()` / `wasmtime_asset()` |
| Pod 指标不准 | runwasi 默认 cgroupfs，与 systemd cgroup driver 不一致 | 配置里加 `SystemdCgroup = true`（安装脚本已做） |

## C. 组件运行时（wasm 内部）

| 现象 | 原因 | 处置 |
|---|---|---|
| GET 正常，POST body 为空（k8s 回 `resource name may not be empty`，或 mock 回 422） | `OutgoingRequest` 的 header/body 用法不对：header 必须在 `OutgoingRequest::new(Fields::from_list(...))` 时传入；body 必须在 `handle()` **之后**写 | 见 `ui/backend/src/k8s.rs` 里 `send()` 的注释与顺序 |
| `设置请求头 content-type 失败` | 用 `req.headers().set(...)` 改已构造请求的 header —— 那是只读视图 | 同上，构造时传入 |
| `访问 kube-api-proxy 失败：... PermissionDenied` / `outbound host not allowed` | 出站被宿主拒绝：wasmtime 缺 `-S inherit-network=y`，或 Spin 的 `allowed_outbound_hosts` 没放行 | wasmtime 加 `-S http=y -S inherit-network=y`；Spin 在 `spin.toml` 里放行对应 host |
| `Kubernetes API 返回 403（Forbidden）` | ClusterRole 权限不足 | 报错里带着 k8s 原文（哪个资源、哪个动词），照它加 `deploy/base/kube-api-proxy.yaml` 里的 rules |
| UI 显示「kube-api-proxy 地址用的是编译期默认值」 | 没有设置 `K8S_PROXY_URL`，正在用默认的 in-cluster 地址 | 标准部署无需处理；非标准命名空间设 env 或构建时烘焙 |
| 前端白屏，页面显示「前端资源未构建」 | 构建 wasm 时 `ui/frontend/dist` 不存在（build.rs 写入了占位页） | `cd ui/frontend && npm ci && npm run build`，再重新 `cargo build --release --target wasm32-wasip2`；或用 `make ui` |
| 前端 fetch 报 `Unexpected token '<'` | 请求打到了静态资源回退（返回 HTML） | 未知 `/api/*` 现在会返回 JSON 404；若仍出现，确认路径拼写与 `lib.rs` 路由表 |

## D. 本地端到端测试（`make e2e`）

| 现象 | 原因 | 处置 |
|---|---|---|
| `端口 xxxx 已被占用` | 上次测试残留的宿主进程（`spin up` / `wasmtime` 是子进程，父 shell 被 kill 不会带走它） | `pkill -f 'spin up'`、`pkill -f dev-mock-k8s-api.py`；脚本已加端口预检 |
| POST 的 body 在 mock 侧是 0 字节 | mock 不认 **chunked** 请求体（wasi:http 出站常用 chunked，不带 content-length） | mock 已支持 chunked；自己写 mock 时要记住这条 |
| 改了代码但行为没变 | 测试打到了上一次残留的宿主进程（旧 wasm 已加载进内存） | 同上，先清理端口再跑 |
| `KeyError: 'name'` 之类 mock 崩溃 | mock 假设了字段存在 | mock 崩掉会把响应截断，客户端只看到 `HttpProtocolError`，很容易误判成组件问题 —— mock 已统一用安全访问器 |

## E. 一条通用排障顺序

```bash
# 1. wasm 组件本身有没有问题？（不需要集群）
make test && make e2e

# 2. 节点上的运行时注册对不对？
grep -A3 runtimes /var/lib/rancher/k3s/agent/etc/containerd/config.toml

# 3. RuntimeClass 与节点标签是否一致？
kubectl get runtimeclass
kubectl get nodes --show-labels | grep wasm.sh

# 4. 真跑一个最小 wasm 工作负载（最有信息量的一步）
./scripts/verify-wasm-runtime.sh --mode command --keep
kubectl -n k3s-wasm-verify describe pod verify-command
```
