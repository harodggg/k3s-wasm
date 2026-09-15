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
| `containerd: failed to unmarshal TOML: toml: table spin already exists`（k3s 只显示「重启失败」） | 你在 containerd 模板里声明了 k3s 会自动探测的 runtime，生成了两个同名 TOML 表 | 删掉模板里重复的 runtime 段（或删模板让 auto-detect 生效）。见 `docs/05-k3s-cilium.md` §3.3 |
| Traefik/`LoadBalancer` Service 有 EXTERNAL-IP，但节点 80/443 没监听 | k3s servicelb 靠 hostPort，而 kube-proxy 被 Cilium 替换后 hostPort 不生效 | 用 NodePort（Cilium 在 BPF 实现）。见 `docs/05-k3s-cilium.md` §3.1 |
| 下载 shim 404 | 两个仓库的资产命名风格不同：spin 是 `containerd-shim-spin-v2-linux-x86_64.tar.gz`，runwasi 是 `containerd-shim-wasmtime-x86_64-linux-musl.tar.gz` | 见 `scripts/install-wasm-runtime.sh` 里的 `spin_asset()` / `wasmtime_asset()` |
| Pod 指标不准 | runwasi 默认 cgroupfs，与 systemd cgroup driver 不一致 | 配置里加 `SystemdCgroup = true`（安装脚本已做） |

## C. 组件运行时（wasm 内部）

| 现象 | 原因 | 处置 |
|---|---|---|
| GET 正常，POST body 为空（k8s 回 `resource name may not be empty`，或 mock 回 422） | `OutgoingRequest` 的 header/body 用法不对：header 必须在 `OutgoingRequest::new(Fields::from_list(...))` 时传入；body 必须在 `handle()` **之后**写 | 见 `ui/backend/src/k8s.rs` 里 `send()` 的注释与顺序 |
| `设置请求头 content-type 失败` | 用 `req.headers().set(...)` 改已构造请求的 header —— 那是只读视图 | 同上，构造时传入 |
| `访问 kube-api-proxy 失败：... PermissionDenied` / `outbound host not allowed` | 出站被宿主拒绝：wasmtime 缺 `-S inherit-network=y`，或 Spin 的 `allowed_outbound_hosts` 没放行 | wasmtime 加 `-S http=y -S inherit-network=y`；Spin 在 `spin.toml` 里放行对应 host |
| 出站请求**永久挂住**（无报错，只是超时），用 IP 直连却正常 | wasmtime shim 不向 guest 提供域名解析（`wasi:sockets/ip-name-lookup` 未开） | 用 IP 字面量：把代理 Service 的 ClusterIP 固定下来（本仓库固定为 10.43.0.53） |
| `pull access denied ... repository does not exist` 但镜像明明在本地 | 三个常见原因：① 导入到了默认命名空间而不是 `k8s.io` ② 导入用短名、Pod 用 `docker.io/` 全名（或反之），CRI 名字对不上 ③ `imagePullPolicy: Always` | `k3s ctr -n k8s.io images import <tar>`，且镜像名统一用完全限定名 `docker.io/...` |
| `ctr: wrong diff id "sha256:..." calculated on extraction "sha256:..."` | 自己构造 OCI 镜像时把 gzip 层的 `diff_ids` 写成了压缩后字节的摘要 | 层描述符 digest 用**压缩后**、`rootfs.diff_ids` 用**解压后** tar 的摘要（`scripts/make-wasm-image.py` 里有注释） |
| 刚建完命名空间，组件出站请求挂住，几秒后自己好了 | Cilium 的 BPF 策略/conntrack 对新建的 NetworkPolicy 与 Pod IP 需要几秒铺开 | 重试即可；`verify-wasm-runtime.sh` 已内置最多 60s 的重试 |
| 改了组件代码、重新导入了镜像，但行为没变 | 镜像 tag 是可变 tag（`:dev`）且 `imagePullPolicy: IfNotPresent`，Pod spec 没变就不会重建 | `kubectl -n k3s-wasm rollout restart deploy/k3s-wasm-ui`；或每次构建用唯一 tag |
| 自己起的服务在 443/80 上「绑定成功」却收不到外部流量；TLS 探测拿到的是 `CN=TRAEFIK DEFAULT CERT` | k3s servicelb 的 hostPort 由 **Cilium 在 BPF 里**实现，**`ss` 看不到监听者**（所以我们误判 443 空闲）。Traefik 的 LB Service 占着 80/443 | 换端口（实测 8443 可用），或先确认 80/443 真的没人用：`kubectl -n kube-system get svc traefik` 看它的 LB 端口；不要只信 `ss` |
| `Kubernetes API 返回 403（Forbidden）` | ClusterRole 权限不足 | 报错里带着 k8s 原文（哪个资源、哪个动词），照它加 `deploy/base/kube-api-proxy.yaml` 里的 rules |
| UI 显示「kube-api-proxy 地址用的是编译期默认值」 | 没有设置 `K8S_PROXY_URL`，正在用默认的 in-cluster 地址 | 标准部署无需处理；非标准命名空间设 env 或构建时烘焙 |
| 页面横幅「自动刷新失败：Cannot convert undefined or null to object」 | 视图里对**后端返回的 map 字段**直接用了 `Object.entries`，而该字段是 `null`（例如 k3s 内置 RuntimeClass 的 `nodeSelector`）。自动刷新每 5 秒重放一次，于是横幅常驻 | 用 `dom.ts` 的 `pairsText()` 渲染后端来的 map；后端也保证这类字段发 `{}` 而不是 `null`。CI 里有 `check-no-raw-object-entries.mjs` 守卫防止复发 |
| 前端白屏，页面显示「前端资源未构建」 | 构建 wasm 时 `ui/frontend/dist` 不存在（build.rs 写入了占位页） | `cd ui/frontend && npm ci && npm run build`，再重新 `cargo build --release --target wasm32-wasip2`；或用 `make ui` |
| 前端 fetch 报 `Unexpected token '<'` | 请求打到了静态资源回退（返回 HTML） | 未知 `/api/*` 现在会返回 JSON 404；若仍出现，确认路径拼写与 `lib.rs` 路由表 |
| WebAuthn 接口回 **400「请求没有 Host 头」**，但浏览器明明发了 Host | **wasi:http 把 Host 放在 request 的 `authority` 上，不保证出现在 `headers()` 里**（wasmtime 就不放）。凡是靠 Host 推 RP ID / origin / 对外地址的代码都会拿不到值 | `http_io::read_request` 现在用 `ensure_host_header()` 从 `authority` 补一条 `host` 头；另外 `effective_rp` 在 `K3S_WASM_RP_ID/K3S_WASM_ORIGIN` 都配好时不再依赖 Host |
| `/api/*` 全返回 **503** 并提示 `K3S_WASM_SESSION_SECRET` | 免密登录开启后门禁是**失败关闭**：没有会话密钥就拒绝服务（而不是放行） | 建 `k3s-wasm-ui-auth-env` Secret 并挂进 Deployment，见 `docs/06-auth.md` §2 |
| 点「用 Touch ID 登录」没反应 / `navigator.credentials` 是 undefined | 不是安全上下文：明文 HTTP、或 https 但证书不被信任 | 用 `https://<域名>` 打开；自签证书要导入系统钥匙串。控制台已不再暴露明文 NodePort |

## C2. 证书签发（Traefik + Let's Encrypt）

| 现象 | 原因 | 处置 |
|---|---|---|
| Traefik 日志 `"HTTP challenge is not enabled"` + `Router uses a nonexistent certificate resolver le` | 在 **Traefik v3** 写了 `--certificatesresolvers.le.acme.httpchallenge=true` 这个 **v2 时代的布尔量**，导致 httpChallenge 段落解析失败、resolver 被跳过 | 只留 `--certificatesresolvers.le.acme.httpchallenge.entrypoint=web`（存在即启用） |
| `unable to get ACME account: open /acme/acme.json: no such file or directory` | chart values 的层级写错了：`additionalVolumes` 在 `deployment:` 下，但 **`additionalVolumeMounts` 是顶层键**；写错只被静默忽略（volume 建了、没挂） | 见 `deploy/optional/traefik-acme.yaml` 注释里的层级 |
| LE 回 `403 ... Invalid response from https://<域名>/.well-known/acme-challenge/...`（返回的是控制台 HTML） | 控制台 HTTP 入口带「301 跳 https」，而 Traefik 内部 ACME 路由优先级低于 `Host+PathPrefix` 用户路由，挑战被跳转截胡 | 给明文入口那条 Ingress 加 `traefik.ingress.kubernetes.io/router.priority: "1"`，让内部挑战路由赢（见 `deploy/base/ui-ingress.yaml`）。注意 Ingress **不支持** `router.rule` 注解，别用 `!PathPrefix` 排除（会把整条注解解析搞挂） |
| TLS-ALPN 挑战回 `remote error: tls: unrecognized name` | 该 Traefik 组合下 ALPN 挑战路径没接住 | 用 HTTP-01 + 上面的优先级方案；确认 80 端口能被外网访问到 |

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
