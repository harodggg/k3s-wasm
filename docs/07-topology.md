# 07 · 运行时三分类与实时网络拓扑

两个能力共用同一套「运行时分类」口径，所以写在一篇里。

---

## 1. 运行时三分类：wasm / 原生 / GPU

### 为什么需要它

控制台最早只有「是不是 wasm」这一个判断，于是：

* GPU 工作负载（`runtimeClassName: nvidia`）被算进「原生」，和普通 Go 容器混在一起，
  而它们要调度到带 GPU 的节点、要装 device plugin —— 排查时完全是两回事；
* 每个视图各写一套判断（概览按 `runtimeClassName.contains("wasm")`、运行时列表按
  `handler`），迟早漂移成两个口径；
* 「这个节点能跑什么」只答了一半（wasm 能力），GPU 能力没地方看。

### 唯一判据与唯一函数

后端 `api.rs::runtime_category()` 是整个控制台**唯一**的分类函数，输入是
`runtimeClassName` 或 RuntimeClass 的 `handler`，输出三选一：

| category | 判据（大小写不敏感） | 典型值 |
|---|---|---|
| `wasm` | 含 `wasm` 或 `spin` | `wasmtime-wasip2` / `wasmtime-spin-v2` / `containerd-shim-spin` |
| `gpu` | 含 `nvidia` / `gpu` / `cuda` / `mig-` | `nvidia` / `nvidia-container-runtime` |
| `native` | 其余（包括**没有** `runtimeClassName` 的普通 Pod） | 空串 / `runc` / `kata` |

刻意不看镜像名：`containerd-shim-*` 这类名字什么都说明不了，而同一个命名空间里
既有 wasm 也有原生容器。

### 字段落在哪

| 接口 | 字段 |
|---|---|
| `GET /api/runtimes` | 每项 `category`（同时保留旧的 `isWasm` 与 `misconfigured`） |
| `GET /api/pods` | 每项 `runtimeCategory` |
| `GET /api/workloads` | 每项 `runtimeCategory` |
| `GET /api/summary` | `runtimeCategories: {wasm, gpu, native, total}`（按 Pod 计） |
| `GET /api/nodes` | `gpu: {present, count}`（与已有的 `wasm: {spin, wasmtime}` 并列） |
| `GET /api/topology` | 每个节点对象的 `category`，以及 `counts.{wasm,native,gpu}` |

节点 GPU 能力有三个来源，取并集：device plugin 报的 `capacity`/`allocatable`
（`nvidia.com/gpu`、`amd.com/gpu`、`gpu.intel.com/i915`，最可信）、NVIDIA 的节点标签
（`nvidia.com/gpu.present` / `nvidia.com/gpu.product`）、以及 PCI 设备标签
（`feature.node.kubernetes.io/pci-10de.present`，还没装 device plugin 时也能看出来）。

前端 `runtime-ui.ts` 是唯一的徽章/过滤实现，所有视图都调它，徽章文案与配色三处一致：
**WASM / 原生 / GPU**。

---

## 2. 实时网络拓扑（`#/topology`）

### 它是什么（以及刻意不是什么）

`GET /api/topology?namespace=<ns|_all>&pods=0|1` 把集群对象压成「节点 + 边」一张图。
这里的边都是**声明出来的关系**，不是抓包得到的流量：

| kind | 含义 | 来源 |
|---|---|---|
| `routes` | Ingress → Service | Ingress 规则里写的 service 名 |
| `exposes` | Service → 工作负载 | Service 的 `spec.selector` 命中工作负载模板标签 |
| `runs-on` | 工作负载 / Pod → Node | Pod 的 `spec.nodeName` |
| `in` | 对象 → Namespace | 对象的 namespace |
| `allows` | NetworkPolicy → 工作负载 | 策略的 `podSelector.matchLabels` |
| `belongs-to` | Pod → 工作负载 | Pod 的 `ownerReferences`（ReplicaSet 名去掉 hash 尾巴） |

**它不声称知道真实数据流向。** 要画实际流量得接 Hubble（Cilium 已经有 Hubble UI，
本仓库的安装脚本会把它装上）。这里回答的是另一个更适合排障的问题：
「谁**被允许**连谁、谁**暴露**在哪儿、谁被**调度**到哪个节点」。

### 为什么在后端建图

组件是「一个请求一个实例」、没有内存状态，前端每 5 秒只拿到一份快照；把
「快照 → 图」做成纯函数（`topology.rs::build_topology`）就能在笔记本上 `cargo test`
（6 个单测：节点/边、分类、Pod 节点、命名空间过滤、指纹变化、SpinApp）。
图的结构只在一处定义，前端只负责画。

### 实时是怎么做的

1. 前端复用控制台既有的 5 秒轮询（`ViewInstance.autoRefresh` + `main.ts` 的 `POLL_MS`），
   `reload()` 重新拉一次 `/api/topology`；
2. 后端给每次响应一个 **`revision`**（内容指纹：id 集合 + 关键计数 + 边集合的哈希）。
   `revision` 不变就不重绘 —— 省掉每 5 秒一次无意义的 DOM 抖动；
3. `revision` 变了才重排，位置变化走 **CSS transition**，所以是「移动」而不是「闪一下」；
4. 前端对比上一轮的 id 集合：新增的短暂高亮绿色，消失的以红色 ghost 保留一轮；
   Pod/副本数变化的对象做一次脉冲。

### 规模上限与降级

| 场景 | 行为 |
|---|---|
| 全命名空间视图 | 工作负载上限 `MAX_WORKLOADS = 60`，超出截断并在 `scope.truncated` 里如实标注 |
| 打开「显示 Pod」 | Pod 节点上限 `MAX_POD_NODES = 200`，超出同样标注 |
| 没权限 / 没有 Service、Ingress、NetworkPolicy、Namespace、SpinApp | 这些是**可选**资源：读不到就空表 + 把原因写进响应的 `notes`（前端显示出来），而不是整张图报错。Pod/Deployment/Node 是主干，读不到才失败 |
| 一个 Pod 的工作负载在图上没有对象（例如 StatefulSet、裸 Pod） | 从 `ownerReferences` 现造一个工作负载节点，避免这些 Pod 在拓扑里凭空消失 |

### 读图

从左到右四列：**Ingress → Service → 工作负载 → 节点**，命名空间与 NetworkPolicy
单独一条侧带。线的粗细/标签带 `count`（例如一个 Service 后面有几个 Pod）。
点任意节点看详情（含命名空间、镜像、端口、nodeSpread 等原始 `meta`）。
