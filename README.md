# k3s-wasm

[![ci](https://github.com/harodggg/k3s-wasm/actions/workflows/ci.yml/badge.svg)](https://github.com/harodggg/k3s-wasm/actions/workflows/ci.yml)

在 **k3s** 上跑 **wasm32-wasip2** 工作负载，外加一个**自己也是 wasm** 的 k3s 控制台。

两件交付物：

1. **运行时落地**：把 containerd 的 wasm shim 装进 k3s 节点并正确配置（脚本 + 清单 + 验证脚本）。
2. **控制台 UI**：后端是编译到 `wasm32-wasip2` 的 `wasi:http/proxy` 组件，前端是 36 KB 的无框架 SPA，
   产物在**编译期**嵌进 wasm —— 用它来管理集群里的 wasm 工作负载。

---

## 已实测到什么程度（先看这张表）

| 环节 | 验证方式 | 结论 |
|---|---|---|
| wasm 组件逻辑（路由 / JSON / 参数校验 / 错误处理） | 29 个原生单测 | ✅ 全绿 |
| 组件在真实 WASI p2 宿主上跑起来 + 访问 HTTP + 读写 env + 静态资源 | **真实 Spin 4.1 宿主** + mock Kubernetes API，31 项断言 | ✅ 全绿 |
| 组件导出的接口是否是标准 wasi:http | `wasm-tools component wit` | ✅ 只导出 `wasi:http/incoming-handler@0.2.12`，**零 `spin:*` 导入** |
| 示例组件（`examples/hello-http`） | 真实 Spin 宿主 + curl | ✅ 200 / 404 行为正确 |
| 部署清单能否渲染 | `kubectl kustomize`（两个 overlay） | ✅ |
| **k3s 节点上的 containerd → shim → 运行链路** | 需要真节点 | ⚠️ **未在本环境验证**，请用 `scripts/verify-wasm-runtime.sh` 在你的集群上验 |

> 最后一行是这份交付物唯一的空白。本环境没有 k3s 节点，`containerd → shim` 这段只能在节点上验。
> 验证脚本会把常见的失败原因（镜像没导进节点、shim 没装、nodeSelector 不匹配、镜像里没有 wasm 模块）
> 逐项检查并打印出来。

---

## 快速开始

```bash
# ── 0. 前置（本机）─────────────────────────────────────────────
rustup target add wasm32-wasip2
# 注意：如果系统里同时有 Homebrew 的 rustc，它没有 wasm32-wasip2 sysroot，
# 用它编 wasm 会报 "can't find crate for core"。本仓库的脚本会把 rustup 的
# cargo 放到 PATH 最前面（见 scripts/build-ui.sh）。

# ── 1. 节点侧：装 wasm 运行时（每个要跑 wasm 的节点都要做）──────
scp -r . node:/tmp/k3s-wasm && ssh node 'cd /tmp/k3s-wasm && sudo ./scripts/install-wasm-runtime.sh'
# 它会：装 containerd-shim-spin-v2 与 containerd-shim-wasmtime-v1 到 /usr/local/bin
#       → 写 containerd 模板(config-v3.toml.tmpl)并打开 SystemdCgroup
#       → 重启 k3s → 给节点打 wasm.sh/* 标签 → 创建 RuntimeClass
# 只想看看会做什么：sudo ./scripts/install-wasm-runtime.sh --dry-run

# ── 2. 构建并部署控制台 ────────────────────────────────────────
./scripts/build-ui.sh --import          # 构建 wasm + 前端，打镜像并导入 k3s containerd
kubectl apply -k deploy/overlays/shim-only
kubectl -n k3s-wasm port-forward svc/k3s-wasm-ui 8080:80
# 浏览器打开 http://127.0.0.1:8080

# ── 3. 验证（强烈建议）─────────────────────────────────────────
./scripts/verify-wasm-runtime.sh --mode http     # 真跑一个 wasm 工作负载并打 /api/health
```

想在**不碰集群**的情况下先把控制台跑起来看界面：

```bash
./scripts/dev-serve-local.sh      # mock k8s API + wasmtime 宿主，浏览器开 :8080
# 或
make e2e                          # 31 项端到端断言（真实 wasm 宿主）
```

---

## 两条部署路径

| | shim-only（默认） | spinkube |
|---|---|---|
| 运行时 | runwasi `containerd-shim-wasmtime-v1` | SpinKube 的 `containerd-shim-spin-v2` |
| RuntimeClass | `wasmtime-wasip2`（handler `wasmtime`） | `wasmtime-spin-v2`（handler `spin` / RCM 下是 `spin-v2`） |
| 工作负载形态 | `Deployment` | `SpinApp`（CRD，operator 生成 Deployment+Service） |
| 监听端口 | **8080** | **80** |
| 需要什么 | 节点上有 shim | shim + cert-manager + operator + CRD |
| 入口 | `deploy/overlays/shim-only` | `deploy/overlays/spinkube` |

**为什么默认是 shim-only**：这个控制台组件没有任何 `spin:*` 导入，是标准
`wasi:http/proxy` 组件。`containerd-shim-wasmtime` 会按**导出名**自动识别这类组件
（`wasi:http/incoming-handler*`）并在容器内 8080 起 HTTP 服务，不需要额外配置。
而 spin shim 加载的是 Spin 应用（LockedApp + Spin 触发器），官方并未文档化它能承载
「非 Spin 的裸 proxy 组件」—— 我们在真实 Spin 4.1 宿主上试过它可以，但把它当默认路径
不严谨。两条路的取舍与细节见 `docs/01-runtime.md`。

---

## 控制台功能

- **概览**：节点/Pod/wasm 工作负载统计；主动暴露「RuntimeClass 存在但没有节点装了 shim」这类配置错误
- **节点**：wasm 能力标签（`wasm.sh/spin` / `wasm.sh/wasmtime`）一目了然
- **WASM 运行时**：RuntimeClass ↔ 可调度节点数 ↔ 是否错配
- **工作负载**：Deployment 列表，可按命名空间过滤、只看 wasm
- **Spin 应用**：创建 / 扩缩 / 删除 SpinApp（含 executor 选择，因为 runtimeClassName 来自 executor）
- **Xray 隧道**：把 `xray-wasm` 作为 wasm 工作负载下发（ConfigMap + Deployment + Service）—— **见下方说明**
- **日志 / 事件**：Pod 日志跟随刷新、命名空间事件（排障用）

### 关于 Xray 面板（说实话）

`xray-wasm` 仓库里 `xt-wasm-cli` 的隧道层**还没接入**（`main.rs` 目前直接 `exit 2`），
所以面板创建出来的 Pod 会立刻退出。面板本身（配置下发、扩缩容、Service 暴露）
是完整可用且已实测的；等 M2/M3 落地后接上即可。另外 wasmtime shim 默认不授予出站 TCP，
而建隧道必须有出站能力 —— 细节和绕过方式见 `docs/03-xray-wasm.md`。

---

## 目录结构

```
k3s-wasm/
├── scripts/
│   ├── install-wasm-runtime.sh    # [节点] 装 shim + containerd 配置 + 标签 + RuntimeClass
│   ├── install-spinkube.sh        # [集群] cert-manager + RCM + spin-operator + CRD
│   ├── build-ui.sh                # [本机] 前端 + wasm + 出包（--import/--push/--spin-push）
│   ├── verify-wasm-runtime.sh     # [集群] 端到端验证（http / command / spinapp 三种模式）
│   ├── e2e-local-test.sh          # [本机] 31 项断言，真实 wasm 宿主 + mock k8s API
│   ├── dev-serve-local.sh         # [本机] 一键起本地控制台（带 mock API）
│   ├── dev-mock-k8s-api.py        # 假 Kubernetes API（支持 chunked 请求体）
│   └── lib/common.sh
├── deploy/
│   ├── base/                      # namespace / RuntimeClass / RBAC / kube-api-proxy / Service
│   └── overlays/{shim-only,spinkube}/
├── ui/
│   ├── backend/                   # Rust → wasm32-wasip2（wasi:http/proxy 组件）
│   └── frontend/                  # 无框架 TS SPA（Vite，36 KB）
├── examples/
│   ├── hello-http/                # 最小 wasi:http 组件（两种宿主都能跑）
│   └── hello-wasip2/              # 命令式 wasm 组件（wasmtime shim 的 Command 模式）
└── docs/
    ├── 01-runtime.md              # 运行时选型、k3s 自动探测、wasi-http 版本对齐
    ├── 02-ui.md                   # UI 架构、API、配置方式、kubectl proxy 的取舍
    ├── 03-xray-wasm.md            # Xray 面板的边界与前置条件
    └── 04-troubleshooting.md      # 报错 → 原因 → 处置
```

---

## 两个值得记住的坑（本仓库里已经踩过并修好）

1. **wasi-http 的 0.2.x 补丁版本互不兼容**。组件模型里每个 `0.2.N` 都是独立包版本，
   宿主只实现其中几个。用 spin-sdk 5.2 经 wit-bindgen 生成的是 `@0.2.9`，而 wasmtime 48
   只实现 `@0.2.12`，直接报
   `component imports instance 'wasi:cli/environment@0.2.9', but a matching implementation was not found`。
   所以后端改用底层 `wasi`(wasip2) 绑定，版本与宿主对齐（`+wasi-0.2.12`）。
2. **`OutgoingRequest` 的 header 必须在构造时用 `Fields` 传入**，事后 `req.headers()` 是只读视图，
   `set` 会失败；而且 **body 要在 `handle()` 发起之后再写**。这两点错了的表现是
   「GET 正常、POST 全空 body」，很容易误判成代理或网络问题。

更多报错对照见 `docs/04-troubleshooting.md`。
