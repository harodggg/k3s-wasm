# 01 · 运行时：k3s 上怎么跑 wasm32-wasip2

## 1. 两个 shim，职责不同

| | `containerd-shim-wasmtime-v1`（runwasi） | `containerd-shim-spin-v2`（SpinKube） |
|---|---|---|
| 它能跑什么 | 标准 WASI 组件：导出 `wasi:http/incoming-handler` 的 **HTTP proxy 组件**，或 `wasi:cli/run` 的**命令行组件** | **Spin 应用**（LockedApp + Spin 触发器） |
| HTTP 入口 | 按导出名自动识别，容器内监听 **8080**（`WASMTIME_HTTP_PROXY_SOCKET_ADDR` 可改） | Spin 的 HTTP 触发器，监听 **80**（`SPIN_HTTP_LISTEN_ADDR` 可改） |
| 额外能力 | 无（就是 WASI） | Spin KV / variables / SQLite / 组件路由 / 多触发器 |
| 需要 shim 之外的东西 | 不需要 | 想用 SpinApp CRD 才需要 operator |

**最常见的误解**：以为 spin shim 能跑任意 `wasi:http/proxy` 组件。它加载的是 Spin 应用 ——
`containerd-shim-spin` 内部走 `spin_loader` + `spin_trigger_http`，单文件镜像那条路也是先
**合成一份带 `[[trigger.http]]` 的 Spin 清单**再交给 Spin 运行时。

本仓库的控制台后端没有任何 `spin:*` 导入（`wasm-tools component wit` 可查），
所以默认把它交给 **wasmtime shim**；SpinApp 那条路留作可选 overlay，
并在真实 Spin 4.1 宿主上实测可用（见下方验证矩阵）。

## 2. k3s 会自动探测 shim（所以不用手写 containerd 配置）

k3s 启动时用 `exec.LookPath` 在服务 `PATH` 里找：

```
containerd-shim-spin-v2      → runtime 名 spin     / runtime_type io.containerd.spin.v2
containerd-shim-wasmtime-v1  → runtime 名 wasmtime / runtime_type io.containerd.wasmtime.v1
```

找到就自动写进 containerd 配置，并自动创建 RuntimeClass `spin` / `wasmtime`。
k3s 文档列出的搜索路径包含 `/usr/local/bin` —— 这就是 shim 该放的位置。
（`/var/lib/rancher/k3s/data/current/bin` 不是搜索路径，而且那是 k3s 自己的版本化载荷目录，升级会变。）

**那为什么 `install-wasm-runtime.sh` 还要写模板？** 为了 `SystemdCgroup = true`：
runwasi 系 shim 默认走 cgroupfs，与 k3s 的 systemd cgroup driver 不一致时 Pod 指标会不准，
而 auto-detect 生成的 stanza 不带这个选项。

模板文件名/插件域必须按 containerd 世代选，写错的表现是「改了没生效」且毫无报错：

| containerd | 模板文件 | 插件域 |
|---|---|---|
| 2.x（k3s v1.31.6+/v1.32.2+） | `config-v3.toml.tmpl` | `plugins.'io.containerd.cri.v1.runtime'` |
| 1.7 及更早 | `config.toml.tmpl` | `plugins."io.containerd.grpc.v1.cri"` |

脚本会自动探测并在重启后**回读** `config.toml` 确认 runtime 真的注册进去了。

## 3. RuntimeClass 命名：为什么不是 `wasmtime`

k3s 会自己创建 RuntimeClass `wasmtime`（handler `wasmtime`，**没有 nodeSelector**）。
如果我们也叫 `wasmtime` 并在里面写 nodeSelector，可能被 k3s 的 addon 覆盖，
于是 Pod 被调度到没装 shim 的节点上，报错是：

```
failed to create shim task: no runtime for io.containerd.wasmtime.v1 is configured
```

所以本仓库用独立名字：

| RuntimeClass | handler | 用途 |
|---|---|---|
| `wasmtime-wasip2` | `wasmtime` | 标准 wasi:http/proxy 组件、命令式 wasm（**本仓库默认**） |
| `wasmtime-spin-v2` | `spin`（RCM 路线是 `spin-v2`） | Spin 应用 / SpinApp（与 spin-operator 默认 executor 一致） |

`handler` 必须等于 containerd 配置里的 **runtime 名**（`.runtimes.<NAME>`），不是 `runtime_type`。

⚠️ 手工装 shim（本仓库脚本）与用 Runtime Class Manager 装，会创建**同名但 handler 不同**的
`wasmtime-spin-v2`，两条路线不要同时用。

## 4. wasi-http 的版本对齐（本项目踩过的最深的坑）

组件模型里 `wasi:http@0.2.x` 的每个**补丁版本都是独立包版本**，宿主只实现其中某几个。
实测：

```
# spin-sdk 5.2（经 wit-bindgen 生成 @0.2.9）→ wasmtime 48（只实现 @0.2.12）
Error: component imports instance `wasi:cli/environment@0.2.9`,
       but a matching implementation was not found in the linker
  Caused by: instance export `get-environment` has the wrong type
```

而且 proxy world 自带 `wasi:cli/*` 导入，所以**任何**用标准 proxy world 的组件都会带上它们 ——
不是"你没用 std::env 就没事"。

处置：后端改用底层 `wasi`(wasip2) 绑定，其版本号后缀直接标明生成的接口版本：

| crate | 生成接口版本 |
|---|---|
| `wasi = "0.14"`（wasip2 `1.0.4+wasi-0.2.12`） | `@0.2.12` |
| `wasi = "0.13"`（wasip2 `…+wasi-0.2.0`） | `@0.2.0` |

选哪个取决于你的宿主实现了哪个版本。查组件当前版本：

```bash
wasm-tools component wit target/wasm32-wasip2/release/k3s_wasm_ui.wasm | grep import
```

部署到节点前建议先用 `scripts/verify-wasm-runtime.sh` 确认宿主能吃下这个版本。

## 5. 验证矩阵（哪些验过、哪些没验）

| 项 | 手段 | 结果 |
|---|---|---|
| 组件接口 | `wasm-tools component wit` | 导出 `wasi:http/incoming-handler@0.2.12`；导入只有 `wasi:*`，无 `spin:*` |
| 组件行为 | 真实 Spin 4.1 宿主 + mock k8s API，31 项断言 | 全绿（含出站 HTTP、静态资源、增删改、错误路径） |
| 示例 | `examples/hello-http` 在真实 Spin 宿主 | 200 / 404 正确 |
| wasmtime 宿主 | `wasmtime serve` 48 | ❌ 因 `wasi:cli/environment@0.2.12` 无匹配实现而无法加载（宿主侧限制，非组件问题） |
| k3s shim 链路 | 需真节点 | 未验证，用 `scripts/verify-wasm-runtime.sh` 在节点上验 |

**推论**：Spin 宿主这条路已经实证可用（shim v0.25.1 内嵌 Spin 4.0.1，与实测的 4.1 同族）；
wasmtime shim 那条路在接口层面成立（导出名会被它的启发式识别），但需要你在节点上确认
它内嵌的 wasmtime 实现的 wasi-http 版本与组件一致。
