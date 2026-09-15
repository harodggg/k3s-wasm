# 03 · Xray 面板：边界、契约与前置条件

## 1. 面板做了什么（已实现并实测）

`POST /api/xray/tunnels` 会下发三个对象：

| 对象 | 名字 | 内容 |
|---|---|---|
| ConfigMap | `<name>-config` | `tunnel.json`：server / uuid / publicKey / shortId / sni / listen |
| Deployment | `<name>` | 一个副本（可扩缩），`args: ["/xray-wasm-cli.wasm", "--config", "/etc/xray-wasm/tunnel.json"]`，把 ConfigMap 只读挂到 `/etc/xray-wasm` |
| Service | `<name>` | ClusterIP，把 SOCKS5 端口暴露给集群内其它 Pod |

三者都带 `app.kubernetes.io/part-of=xray-wasm` + `managed-by=k3s-wasm-ui` 标签，
UI 靠这两个标签认领自己创建的资源（**不会碰你手工创建的同名对象**）。
扩缩容走 `spec.replicas` 的 merge patch；删除是幂等的（对象不存在就跳过）。

界面上的 UUID 只显示「已配置 / 未配置」，不回显明文 —— 避免凭据进浏览器历史与日志。

## 2. 现在**还不能**真正建隧道（这是实话）

1. **`xt-wasm-cli` 的隧道层尚未接入**。`xray-wasm/crates/xt-wasm-cli/src/main.rs` 目前是桩：

   ```rust
   eprintln!("xt-wasm-cli: 隧道层尚未接入（等待 xt-wasm-tls / xt-wasm-vless 移植完成）");
   std::process::exit(2);
   ```

   所以面板创建出来的 Pod 会立刻以退出码 2 结束。等 M2/M3 落地后接上即可。

2. **wasmtime shim 默认不授予出站 TCP**。wasip2 的 socket 能力要宿主显式打开
   （本地 wasmtime 用的是 `-S tcp=y -S inherit-network=y`），而 runwasi 的 wasmtime shim
   目前把这部分固定下来，没有暴露等价的节点级开关。
   影响：**命令式 wasm 组件拿不到出站网络**，而 xray 隧道的本质就是出站 TCP。

   可选出路（按可行性排序）：
   - 等/推动 runwasi 暴露 wasi:sockets 配置，或用带该能力的自定义 shim；
   - 把「出站 TCP」下沉到宿主侧：让 wasm 只做协议处理，TCP 由宿主注入
     （`xray-wasm/PLAN.md` 的 §7 风险登记里也提到了这条：必要时改用内嵌 wasmtime 的宿主，自己控制 socket 注入）；
   - 若只是想验证隧道协议正确性，先在**主机上用 wasmtime** 跑通
     （`xray-wasm/scripts/run-local.sh` 已经封装了必需的 `-S` 开关）。

   在确认节点上的 shim 支持 wasi:sockets 之前，面板创建的隧道 Pod 会在「网络不可用」上失败。

## 3. 契约：CLI 需要接受的参数

面板按下面这个契约下发（与 `xray-wasm/scripts/run-local.sh` 文档里的 CLI 形态一致）：

```
xray-wasm-cli.wasm --config /etc/xray-wasm/tunnel.json
```

`tunnel.json` 的字段：

```json
{
  "server": "203.0.113.10:443",
  "uuid": "…",
  "publicKey": "…",        // REALITY 公钥（pbk）
  "shortId": "…",          // sid
  "sni": "www.amazon.com",
  "listen": "0.0.0.0:1080"
}
```

等价的纯命令行形式（若不实现 `--config`，把 Deployment 的 `args` 换成这一组即可）：

```
--server <ip:port> --pbk <公钥> --sid <shortId> --sni <域名> --uuid <uuid> --listen <addr>
```

**为什么默认用配置文件而不是命令行参数**：`args` 在 Pod spec 里对整个集群可见，
把 UUID 放在 args 里等于把它写进等等可读的对象；放进 ConfigMap 至少可以用 RBAC 单独收紧。

## 4. 想改用 Secret？

当前 ClusterRole **故意没有** `secrets` 权限（这样代理被摸到也读不到集群密钥），
所以隧道配置落在 ConfigMap 里。如果你不接受明文配置：

1. 给 `deploy/base/kube-api-proxy.yaml` 的 ClusterRole 加：

   ```yaml
   - apiGroups: [""]
     resources: ["secrets"]
     verbs: ["create", "update", "patch", "delete"]   # 建议再配 resourceNames 限制读
   ```

2. 把 `ui/backend/src/api.rs` 里 `xray_create` 的 ConfigMap 换成 Secret（
   `stringData` → `data` 的 base64 需要自己编码），并把 Deployment 的 volume 改成 `secret:`。

代价是「代理被摸到就能写密钥」—— 两害相权，默认选了 ConfigMap + NetworkPolicy 收紧。
更好的做法是上游支持 etcd 静态加密。

## 5. 面板里可调的东西

- `listen`：`0.0.0.0:1080` 形式，端口会同步到 Service 与 containerPort
- `image`：默认 `k3s-wasm/xray-wasm-cli:dev`，换成你构建的 xray-wasm 镜像
- `runtimeClassName`：默认 `wasmtime-wasip2`（命令式 wasm 只能跑在 wasmtime 运行时上）
- `replicas`：0 表示停掉
