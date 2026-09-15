// WASM 相关的两类「可写」视图：SpinKube 的 SpinApp、以及 xray-wasm 隧道。
//
// 这两个视图里有表单，所以 autoRefresh = false：
// 全局轮询只会调用 reload()，而 reload 只重画列表，不动表单 ——
// 否则用户正在输入时会被轮询清空。

import type {
  CreatedWalljump,
  NodeInfo,
  SpinApp,
  SpinAppExecutorInfo,
  TunnelMode,
  XrayModeInfo,
  XrayTunnel,
} from './api';
import { age, badge, button, card, clear, copyText, el, empty, errorText, field, input, toast } from './dom';
import type { Ctx, ViewInstance } from './view-types';

// ── SpinKube：SpinApp ───────────────────────────────────────────────

export function spinAppsView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;
  let apiVersion = 'core.spinkube.dev/v1alpha1';
  let executors: SpinAppExecutorInfo[] = [];
  const list = el('div', { class: 'stack' });

  const reload = async () => {
    if (!host) return;
    try {
      const res = await ctx.api.spinapps(ctx.currentNamespace, apiVersion);
      apiVersion = res.apiVersion || apiVersion;
      if (!res.installed) {
        list.replaceChildren(
          empty('集群里没有 SpinApp CRD', res.hint ?? '用 scripts/install-spinkube.sh 安装 SpinKube。'),
        );
        return;
      }
      list.replaceChildren(renderList(res.items));
    } catch (e) {
      list.replaceChildren(empty('读取 SpinApp 失败', errorText(e)));
    }
  };

  function renderList(items: SpinApp[]): HTMLElement {
    if (items.length === 0) return empty('还没有 SpinApp', '用左边的表单创建一个');
    return el(
      'div',
      { class: 'stack' },
      ...items.map((app) =>
        card(
          app.name,
          el(
            'div',
            { class: 'stack' },
            el(
              'dl',
              { class: 'kv' },
              kv('命名空间', app.namespace),
              kv('镜像', app.image),
              kv('executor', app.executor ?? '（未设置）'),
              kv('副本', app.replicas === 0 ? '0（已停）' : `${app.readyReplicas}/${app.replicas}`),
              kv('创建', age(app.createdAt)),
            ),
            el(
              'div',
              { class: 'row-actions' },
              ...[1, 2, 3, 0].map((n) =>
                button(`副本 ${n}`, async () => {
                  try {
                    await ctx.api.scaleSpinApp(app.namespace, app.name, n, apiVersion);
                    toast(`${app.name} 副本已设为 ${n}`);
                    await reload();
                  } catch (e) {
                    toast(`扩缩失败：${errorText(e)}`, 'err');
                  }
                }),
              ),
              button(
                '删除',
                async () => {
                  if (!confirm(`删除 SpinApp ${app.namespace}/${app.name}？`)) return;
                  try {
                    await ctx.api.deleteSpinApp(app.namespace, app.name, apiVersion);
                    toast('已删除');
                    await reload();
                  } catch (e) {
                    toast(`删除失败：${errorText(e)}`, 'err');
                  }
                },
                'danger',
              ),
            ),
          ),
          badge('SpinApp', 'ok'),
        ),
      ),
    );
  }

  return {
    title: 'Spin 应用',
    autoRefresh: false,
    async mount(h, c) {
      host = h;
      ctx = c;
      clear(host);

      // 表单字段
      const name = input('hello-spin');
      const image = input('registry.k8s.io/… 或 ghcr.io/…');
      image.setAttribute('list', 'spin-image-options');
      const spinImageList = el('datalist', { attrs: { id: 'spin-image-options' } });
      const replicas = input('1', '1', 'number') as HTMLInputElement;
      const variableKey = input('例如 greeting');
      const variableValue = input('例如 hi');
      const executorSelect = el('select', { class: 'input' }) as HTMLSelectElement;

      // executor 决定 runtimeClassName，必须来自集群里真实存在的 SpinAppExecutor
      try {
        const res = await ctx.api.executors(ctx.currentNamespace);
        executors = res.installed ? res.items : [];
      } catch {
        executors = [];
      }
      if (executors.length === 0) {
        executorSelect.append(el('option', { text: '（没有可用 executor）' }) as HTMLOptionElement);
      } else {
        for (const e of executors) {
          const opt = el('option', {
            text: `${e.name}${e.runtimeClassName ? ` → ${e.runtimeClassName}` : ''}`,
          }) as HTMLOptionElement;
          opt.value = e.name;
          executorSelect.append(opt);
        }
      }

      const create = button('创建 SpinApp', async () => {
        const body = {
          name: name.value.trim(),
          namespace: ctx.currentNamespace === '_all' ? ctx.defaultNamespace : ctx.currentNamespace,
          image: image.value.trim(),
          replicas: Number(replicas.value || '1'),
          executor: executorSelect.value || 'containerd-shim-spin',
          apiVersion,
        } as Parameters<typeof ctx.api.createSpinApp>[0] & { apiVersion: string };
        const k = variableKey.value.trim();
        if (k) body.variables = { [k]: variableValue.value };
        try {
          await ctx.api.createSpinApp(body);
          toast(`已创建 ${body.name}`);
          name.value = '';
          image.value = '';
          await reload();
        } catch (e) {
          toast(`创建失败：${errorText(e)}`, 'err');
        }
      }, 'primary');

      // 下拉候选：集群里正在跑的镜像（kubelet 会拉，选本地已存在的镜像最稳）
      try {
        const imgs = await ctx.api.images(false);
        const values = [
          ...imgs.items.map((i) => i.image),
          ...imgs.defaults.filter((d) => !imgs.items.some((i) => i.image === d)),
        ];
        spinImageList.replaceChildren(
          ...values.map((v) => el('option', { attrs: { value: v } })),
        );
      } catch {
        /* 拉不到候选不影响手输 */
      }

      host.append(
        el(
          'div',
          { class: 'grid-form' },
          card(
            '新建 SpinApp',
            el(
              'div',
              { class: 'stack' },
              field('名称', name, 'DNS-1123：小写字母数字和 -'),
              field('镜像', image, '下拉里是集群里正在跑的镜像（选它一定已在节点上）；也可以手输'),
              spinImageList,
              field('副本数', replicas),
              field('executor', executorSelect, 'SpinApp 的 runtimeClassName 来自 executor，不是自己写'),
              el('div', { class: 'grid-2-tight' }, field('变量名（可选）', variableKey), field('变量值', variableValue)),
              el(
                'div',
                { class: 'hints' },
                el(
                  'div',
                  { class: 'hint hint-info' },
                  el('div', { class: 'hint-title', text: 'SpinApp 跑在 Spin shim 上' }),
                  el('div', {
                    class: 'hint-detail',
                    text: 'SpinApp 只承载 Spin 应用（spin.toml / Spin SDK 构建）。标准 wasi:http/proxy 组件请用 RuntimeClass wasmtime-wasip2 + Deployment。',
                  }),
                ),
              ),
              create,
            ),
          ),
          el('div', { class: 'stack' }, el('h3', { text: '现有 SpinApp' }), list),
        ),
      );
      await reload();
    },
    reload,
  };
}

function kv(k: string, v: string): Node {
  return el('div', { class: 'kv-row' }, el('dt', { text: k }), el('dd', { text: v }));
}

// ── xray-wasm：翻墙 / 隧道 ──────────────────────────────────────────
//
// 同一个 wasm 组件两个方向（v0.4.0 起）：
//   walljump 翻墙 = XT_MODE=server：入站 REALITY（NodePort）→ 出站直连
//   tunnel  隧道 = 默认客户端：入站 SOCKS5（ClusterIP/NodePort）→ 经 REALITY 出网
//
// 两种模式的语义文案（label/entry/who/flowNote）由后端 GET /api/xray/tunnels 的
// modes[] 下发，面板直接渲染 —— 前端不再维护第二份，否则后端改了语义这里就开始说谎。

/** 只读文本块 + 一键复制。生成结果与创建结果共用，省掉三份重复实现。 */
function copyBlock(label: string, text: string, rows: number): HTMLElement {
  const ta = el('textarea', {
    class: 'input mono',
    attrs: { readonly: '', rows: String(rows) },
  }) as HTMLTextAreaElement;
  ta.value = text;
  return el(
    'div',
    { class: 'stack' },
    el('div', { class: 'field-label', text: label }),
    ta,
    button('复制', async () => {
      const ok = await copyText(text);
      toast(ok ? `已复制${label}` : '复制失败：请手动选中后复制', ok ? 'ok' : 'err');
    }),
  );
}

/** 节点下拉里展示的地址：与后端选节点时用的口径一致，优先 InternalIP。 */
function nodeIP(n: NodeInfo): string {
  return n.addresses?.find((a) => a.type === 'InternalIP')?.address ?? '';
}

export function xrayView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;
  const list = el('div', { class: 'stack' });
  // 后端下发的模式语义；请求失败时退化成只有 id/label 的骨架，界面其余部分照常可用
  let modes: XrayModeInfo[] = [];

  const reload = async () => {
    if (!host) return;
    try {
      const res = await ctx.api.tunnels(ctx.currentNamespace);
      if (res.modes && res.modes.length > 0) modes = res.modes;
      if (res.items.length === 0) {
        list.replaceChildren(
          empty(
            '还没有翻墙入口或隧道',
            `创建出来的工作负载会跑在 RuntimeClass ${res.shim.runtimeClass} 上（节点需带标签 ${res.shim.requiredNodeLabel}）`,
          ),
        );
        return;
      }
      list.replaceChildren(...res.items.map(renderTunnel));
    } catch (e) {
      list.replaceChildren(empty('读取翻墙 / 隧道失败', errorText(e)));
    }
  };

  function renderTunnel(t: XrayTunnel): HTMLElement {
    const isWalljump = t.mode === 'walljump';
    const port = (t.tunnel.listen ?? '0.0.0.0:1080').split(':').pop() ?? '1080';
    const vlessBox = el('div', { class: 'stack' });

    const rows: Node[] = [
      kv('实现', t.impl ?? (isWalljump ? 'xray-wasm（REALITY 服务端）' : 'xray-wasm（REALITY 客户端）')),
    ];
    if (isWalljump) {
      // 翻墙：入口就是 REALITY 的 NodePort，出口走这台节点自己的网络（没有第二跳）
      rows.push(
        kv('方向', t.directionLabel || '入站（REALITY 入 → 直连出）'),
        kv('入口', t.ingress?.endpoint || t.tunnel.server || '-'),
        kv('入口节点', t.node ?? '-'),
        kv('出站链路', `${t.egress?.via ?? '直连（该节点自己的网络）'} · ${t.egress?.protocol ?? 'VLESS+REALITY → 直连目标'}`),
      );
    } else {
      // 隧道：入站是 SOCKS5（谁能连由 Service 类型决定），出站去远端 REALITY 服务端
      rows.push(
        kv('方向', t.directionLabel ?? '出站（集群内 → 经 REALITY 出网）'),
        kv('出站链路', `${t.egress?.via ?? t.tunnel.server ?? '-'} · ${t.egress?.protocol ?? 'SOCKS5 → VLESS+REALITY'}`),
        kv('入站入口', t.ingress?.endpoint ?? `${t.name}.${t.namespace}.svc.cluster.local:${port}`),
        kv(
          '入站（外部经节点IP）',
          t.usage?.inbound
            ? `可用 · ${t.ingress?.exposure?.reach ?? ''}${
                t.ingress?.exposure?.nodePort ? ` · socks5h://<节点IP>:${t.ingress.exposure.nodePort}` : ''
              }${t.usage?.allowFrom ? ` · 放行来源 ${t.usage.allowFrom}` : ''}`
            : `不可用（${t.ingress?.exposure?.reach ?? '未知'}）—— 改用途为 nodeport 才可外部使用`,
        ),
      );
    }
    rows.push(
      kv('命名空间', t.namespace),
      kv('SNI', t.tunnel.sni ?? '-'),
      kv('shortId', t.tunnel.shortId ?? '-'),
      kv('UUID', t.tunnel.hasUuid ? '已配置（不回显）' : '未配置'),
      kv('监听', t.tunnel.listen ?? '-'),
      kv('副本', t.replicas === 0 ? '0（已停）' : `${t.readyReplicas}/${t.replicas}`),
      kv('运行时', t.runtimeClass),
      kv('镜像', t.image ?? '-'),
      kv('创建', age(t.createdAt)),
    );

    // 方向徽章按模式区分：翻墙是「入站 REALITY / 直连出 / 服务端」，隧道是「出站 / 经 REALITY 出」
    const badges: Node[] = isWalljump
      ? [badge('入站 REALITY', 'ok'), badge('直连出', 'info'), badge('xray-wasm 服务端', 'muted')]
      : [badge('出站', 'ok'), badge('经 REALITY 出', 'info')];
    badges.push(
      t.ingress?.exposure?.public === true
        ? badge('公网可达', 'err')
        : t.ingress?.exposure?.public === false
          ? badge('仅集群内', 'muted')
          : badge('暴露未知', 'warn'),
      badge(t.runtimeClass, 'info'),
    );

    return card(
      t.name,
      el(
        'div',
        { class: 'stack' },
        el('dl', { class: 'kv' }, ...rows),
        vlessBox,
        el(
          'div',
          { class: 'row-actions' },
          ...[1, 2, 0].map((n) =>
            button(`副本 ${n}`, async () => {
              try {
                await ctx.api.scaleTunnel(t.namespace, t.name, n);
                toast(`${t.name} 副本已设为 ${n}`);
                await reload();
              } catch (e) {
                toast(`扩缩失败：${errorText(e)}`, 'err');
              }
            }),
          ),
          button('显示 vless 链接', async () => {
            vlessBox.replaceChildren(el('div', { class: 'field-label', text: '读取中…' }));
            try {
              const v = await ctx.api.tunnelVless(t.namespace, t.name);
              const ta = el('textarea', {
                class: 'input mono',
                attrs: { readonly: '', rows: '3' },
              }) as HTMLTextAreaElement;
              ta.value = v.vlessLink;

              const actions: Node[] = [
                button('复制链接', async () => {
                  const ok = await copyText(v.vlessLink);
                  toast(ok ? '已复制 vless 链接' : '复制失败，请手动复制', ok ? 'ok' : 'err');
                }),
              ];
              // 翻墙没有 SOCKS5 入口（后端这两个字段给空串），不能给出会误导人的用法命令
              if (v.socksEndpoint) {
                actions.push(
                  button('复制 SOCKS5 用法', async () => {
                    const line = `curl --proxy-user '${v.socksUser}:<密码>' --proxy socks5h://${v.socksEndpoint} https://api.ipify.org`;
                    const ok = await copyText(line);
                    toast(ok ? '已复制（密码用你创建时设置的）' : '复制失败', ok ? 'ok' : 'err');
                  }),
                );
              }
              // 有完整客户端配置时给一条更省事的路径：导入 config.json，不用手抄字段
              if (v.clientConfig) {
                actions.push(
                  button('复制客户端 config.json', async () => {
                    const ok = await copyText(JSON.stringify(v.clientConfig, null, 2));
                    toast(ok ? '已复制客户端 config.json' : '复制失败', ok ? 'ok' : 'err');
                  }),
                );
              }

              vlessBox.replaceChildren(
                el('div', { class: 'field-label', text: 'vless:// 链接（含客户端凭据，别公开）' }),
                ta,
                el('div', { class: 'row-actions' }, ...actions),
                // note 由后端生成：翻墙模式里它写明了「链接为什么不带 flow」
                el('div', { class: 'field-hint', text: v.note }),
              );
            } catch (e) {
              vlessBox.replaceChildren(el('div', { class: 'field-hint', text: `读取失败：${errorText(e)}` }));
            }
          }),
          button(
            '删除',
            async () => {
              if (
                !confirm(
                  `删除${isWalljump ? '翻墙入口' : '隧道'} ${t.namespace}/${t.name}（Deployment + Service + Secret + NetworkPolicy）？`,
                )
              )
                return;
              try {
                await ctx.api.deleteTunnel(t.namespace, t.name);
                toast('已删除');
                await reload();
              } catch (e) {
                toast(`删除失败：${errorText(e)}`, 'err');
              }
            },
            'danger',
          ),
        ),
      ),
      el('span', { class: 'badges' }, ...badges),
    );
  }

  return {
    title: 'Xray 翻墙 / 隧道',
    autoRefresh: false,
    async mount(h, c) {
      host = h;
      ctx = c;
      clear(host);

      // 节点列表给翻墙模式的节点下拉用；modes 由 reload() 从列表接口一并带回来。
      let nodes: NodeInfo[] = [];
      const loadNodes = async () => {
        try {
          nodes = await ctx.api.nodes();
        } catch {
          nodes = [];
        }
      };
      await Promise.all([reload(), loadNodes()]);

      const name = input('tokyo');

      // 模式选择器：选项文案就是后端 modes[].label，默认翻墙
      const modeSel = el('select', { class: 'input' }) as HTMLSelectElement;
      const modeList: XrayModeInfo[] =
        modes.length > 0
          ? modes
          : [
              { id: 'walljump', label: '翻墙' },
              { id: 'tunnel', label: '隧道' },
            ];
      for (const m of modeList) {
        const o = el('option', { text: m.label }) as HTMLOptionElement;
        o.value = m.id;
        modeSel.append(o);
      }
      modeSel.value = modeList.some((m) => m.id === 'walljump') ? 'walljump' : (modeList[0]?.id ?? '');
      const currentMode = (): TunnelMode => (modeSel.value === 'walljump' ? 'walljump' : 'tunnel');

      // 模式说明整块来自后端 modes[]：前端不复制这套文案
      const modeInfo = el('div', { class: 'hint hint-info' });
      const renderModeInfo = () => {
        const m = modeList.find((x) => x.id === currentMode());
        if (!m) {
          modeInfo.replaceChildren();
          modeInfo.classList.add('hidden');
          return;
        }
        modeInfo.classList.remove('hidden');
        const out: Node[] = [el('div', { class: 'hint-title', text: m.label })];
        const detail = (label: string, text: string | undefined) => {
          if (text) out.push(el('div', { class: 'hint-detail', text: `${label}${text}` }));
        };
        detail('实现：', m.impl);
        detail('入口：', m.entry);
        detail('谁在用：', m.who);
        detail('节点要求：', m.nodeRequirement);
        detail('flow：', m.flowNote);
        modeInfo.replaceChildren(...out);
      };

      // ── 翻墙模式字段 ──
      const wjNode = el('select', { class: 'input' }) as HTMLSelectElement;
      const wasmNodes = nodes.filter((n) => n.wasm.wasmtime);
      if (nodes.length === 0) {
        wjNode.append(
          el('option', { text: '（读不到节点列表：留空交给后端选第一个 Ready 节点）' }) as HTMLOptionElement,
        );
      } else {
        for (const n of nodes) {
          const ip = nodeIP(n);
          const capable = n.wasm.wasmtime;
          const o = el('option', {
            text: `${n.name}${ip ? ` · ${ip}` : ''} · ${
              capable ? 'wasm 运行时 ✓' : '无 wasm 运行时（调度上去会一直 Pending）'
            }${n.ready ? '' : ' · NotReady'}`,
          }) as HTMLOptionElement;
          o.value = n.name;
          // wasmtime-wasip2 的 nodeSelector 是 wasm.sh/wasmtime=true：没有该标签的节点选不中，
          // 直接禁止选中，省掉一轮「Pod 一直 Pending」的排查。
          o.disabled = !capable;
          wjNode.append(o);
        }
        const pick = wasmNodes.find((n) => n.ready) ?? wasmNodes[0];
        if (pick) wjNode.value = pick.name;
      }
      const hasWasmNode = wasmNodes.length > 0;

      const wjNodePort = input('30543', '30543', 'number') as HTMLInputElement;
      const wjSni = input('www.cloudflare.com', 'www.cloudflare.com');
      const wjDest = input('默认 <SNI>:443', 'www.cloudflare.com:443');
      const wjPublicHost = input('留空 = 面板访问地址 / 节点 IP');
      const wjReplicas = input('1', '1', 'number') as HTMLInputElement;
      const wjImage = input('docker.io/k3s-wasm/xray-wasm-cli:v0.4.0');
      const wjAllowFrom = input('0.0.0.0/0 = 对全网开放，建议收窄到你自己的出口 IP', '0.0.0.0/0');

      // dest 默认跟着 SNI 走：探测者会看到这个站点真实的 TLS 证书。
      // 只在用户没改过 dest（还是上一个 SNI 推导出来的值）时才自动跟随，避免覆盖手填值。
      let lastSni = wjSni.value.trim();
      wjSni.addEventListener('input', () => {
        const s = wjSni.value.trim();
        if (!wjDest.value.trim() || wjDest.value.trim() === `${lastSni}:443`) {
          wjDest.value = s ? `${s}:443` : '';
        }
        lastSni = s;
      });

      const wjCreated = el('div', { class: 'stack' });
      const createWj = button('创建翻墙入口', async () => {
        try {
          const res: CreatedWalljump = await ctx.api.createWalljumpTunnel({
            mode: 'walljump',
            name: name.value.trim(),
            namespace: ctx.currentNamespace === '_all' ? ctx.defaultNamespace : ctx.currentNamespace,
            node: wjNode.value || undefined,
            nodePort: Number(wjNodePort.value.trim()) || undefined,
            sni: wjSni.value.trim() || undefined,
            dest: wjDest.value.trim() || undefined,
            publicHost: wjPublicHost.value.trim() || undefined,
            replicas: Number(wjReplicas.value || '1'),
            image: wjImage.value.trim() || undefined,
            allowFrom: wjAllowFrom.value.trim() || undefined,
          });
          const out: Node[] = [
            el(
              'div',
              { class: 'hint hint-info' },
              el('div', { class: 'hint-title', text: `已创建翻墙入口 ${res.name}（入站 REALITY → 直连出）` }),
              // note 里已经写清「链接为什么故意不带 flow」，直接显示，不再另写一份
              el('div', { class: 'hint-detail', text: res.note ?? '' }),
            ),
            copyBlock('vless:// 链接（导入官方客户端；链接不带 flow）', res.vlessLink, 3),
            copyBlock('入口', res.entry, 2),
            copyBlock('客户端 config.json', JSON.stringify(res.clientConfig, null, 2), 12),
          ];
          if (res.warning) {
            out.push(
              el(
                'div',
                { class: 'hint hint-warn' },
                el('div', { class: 'hint-title', text: '部分附属资源没建成' }),
                el('div', { class: 'hint-detail', text: res.warning }),
              ),
            );
          }
          wjCreated.replaceChildren(...out);
          toast(`已创建翻墙入口 ${res.name}`);
          await reload();
        } catch (e) {
          toast(`创建失败：${errorText(e)}`, 'err');
        }
      }, 'primary');

      // ── 隧道模式字段（原有字段全部保留）──
      const tnVless = input('可选：直接粘贴 vless:// 链接，下面几项会自动补全');
      const tnServer = input('203.0.113.10:443');
      const tnUuid = input('xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx');
      const tnPublicKey = input('REALITY 公钥（xray x25519 的 Password/公钥）');
      const tnShortId = input('9f1c2a3b');
      const tnSni = input('www.amazon.com');
      const tnSocksUser = input('xrayuser', 'xrayuser');
      const tnSocksPass = input('必填：否则 0.0.0.0 上的无认证 SOCKS5 就是开放代理');
      const tnSecretName = input('可选：引用已存在的 Secret（填了就不由控制台创建）');
      const tnListen = input('0.0.0.0:1080', '0.0.0.0:1080');
      const tnReplicas = input('2', '2', 'number') as HTMLInputElement;
      // 固定仓库 + 实时版本：版本列表从 GitHub releases 取，镜像 = 仓库 + 所选版本
      const TN_IMAGE_REPO = 'docker.io/k3s-wasm/xray-wasm-cli';
      const tnImage = input(`${TN_IMAGE_REPO}:v0.4.0`);
      const tnImageTagSel = el('select', { class: 'input' }) as HTMLSelectElement;
      tnImageTagSel.append(el('option', { text: '版本加载中…' }) as HTMLOptionElement);
      tnImageTagSel.addEventListener('change', () => {
        if (tnImageTagSel.value === '__custom__') {
          tnImage.focus();
          return;
        }
        if (tnImageTagSel.value) tnImage.value = `${TN_IMAGE_REPO}:${tnImageTagSel.value}`;
      });

      // 用途：直接决定 Service 是 ClusterIP 还是 NodePort，以及 NetworkPolicy 是否放行外部来源
      const exposeSel = el('select', { class: 'input' }) as HTMLSelectElement;
      for (const [v, label] of [
        ['cluster', '仅集群内（出站：集群里的 Pod 用它出网）'],
        ['nodeport', '允许外部经节点 IP（入站：给本机/外部客户端连）'],
      ] as [string, string][]) {
        const o = el('option', { text: label }) as HTMLOptionElement;
        o.value = v;
        exposeSel.append(o);
      }
      const tnNodePort = input('留空自动分配，如 31080');
      const tnAllowFrom = input('如 1.2.3.4/32；默认 0.0.0.0/0 = 对全网开放');
      const tnCreated = el('div', { class: 'stack' });

      // vless:// 解析放在后端做（前端只把原文传过去），避免两处实现漂移
      tnVless.addEventListener('change', () => {
        const m = /^vless:\/\/([^@]+)@([^?]+)\?(.*)$/.exec(tnVless.value.trim());
        if (!m) return;
        tnUuid.value = m[1] ?? '';
        tnServer.value = m[2] ?? '';
        for (const pair of (m[3] ?? '').split('&')) {
          const [k, v] = pair.split('=');
          if (k === 'pbk') tnPublicKey.value = decodeURIComponent(v ?? '');
          if (k === 'sid') tnShortId.value = decodeURIComponent(v ?? '');
          if (k === 'sni') tnSni.value = decodeURIComponent(v ?? '');
        }
      });

      const generated = el('div', { class: 'stack' });

      // ① 自动生成：一次给全套参数（REALITY 密钥对 + UUID + shortId + SOCKS 凭据），
      //    并把服务端 config.json 与 vless:// 链接一起显示出来，省掉手工拆字段。
      const genUsage = el('select', { class: 'input' }) as HTMLSelectElement;
      for (const [v, label] of [
        ['cluster', '只给集群内用（出站）'],
        ['nodeport', '还要给外部/本机用（入站 + 出站）'],
      ] as [string, string][]) {
        const o = el('option', { text: label }) as HTMLOptionElement;
        o.value = v;
        genUsage.append(o);
      }

      const generate = button('① 自动生成参数', async () => {
        try {
          const g = await ctx.api.generateTunnel({
            server: tnServer.value.trim() || undefined,
            sni: tnSni.value.trim() || undefined,
            name: name.value.trim() || undefined,
            usage: genUsage.value === 'nodeport' ? 'nodeport' : 'cluster',
          });
          // 生成的用途也同步到创建表单，避免两处不一致
          exposeSel.value = g.usage === 'nodeport' ? 'nodeport' : 'cluster';
          tnUuid.value = g.uuid;
          tnPublicKey.value = g.publicKey;
          tnShortId.value = g.shortId;
          tnSni.value = g.sni;
          tnSocksUser.value = g.socksUser;
          tnSocksPass.value = g.socksPass;
          if (!tnServer.value.trim()) tnServer.value = g.server;
          toast('已生成并填入；请把服务端配置粘到你的服务器上');

          generated.replaceChildren(
            el(
              'div',
              { class: 'hint hint-warn' },
              el('div', { class: 'hint-title', text: '私钥只显示这一次（控制台不保存）' }),
              el('div', {
                class: 'hint-detail',
                text:
                  '下面的服务端配置里含 REALITY 私钥：请立刻粘到服务器的 config.json 并重启 xray，' +
                  '然后点 ② 创建隧道。私钥不要提交到 git、不要留在聊天记录里。',
              }),
            ),
            copyBlock('vless:// 分享链接（导入官方客户端 / xrayTun）', g.vlessLink, 3),
            copyBlock('服务端 config.json（粘到服务器）', JSON.stringify(g.serverConfig, null, 2), 14),
            copyBlock('客户端 config.json（用官方 Xray 先验证服务端）', JSON.stringify(g.clientConfig, null, 2), 12),
            copyBlock('出站：集群内怎么用（环境变量 / curl）', JSON.stringify(g.outbound, null, 2), 9),
            copyBlock('入站：外部/本机怎么用', JSON.stringify(g.inbound, null, 2), 9),
            el('ul', { class: 'notes' }, ...g.notes.map((n) => el('li', { text: n }))),
          );
        } catch (e) {
          toast(`生成失败：${errorText(e)}`, 'err');
        }
      }, 'primary');

      const createTn = button('② 创建隧道', async () => {
        try {
          const res = await ctx.api.createTunnel({
            mode: 'tunnel',
            name: name.value.trim(),
            namespace: ctx.currentNamespace === '_all' ? ctx.defaultNamespace : ctx.currentNamespace,
            vlessLink: tnVless.value.trim() || undefined,
            server: tnServer.value.trim(),
            uuid: tnUuid.value.trim(),
            publicKey: tnPublicKey.value.trim(),
            shortId: tnShortId.value.trim(),
            sni: tnSni.value.trim(),
            socksUser: tnSocksUser.value.trim(),
            socksPass: tnSocksPass.value,
            secretName: tnSecretName.value.trim() || undefined,
            listen: tnListen.value.trim(),
            replicas: Number(tnReplicas.value || '2'),
            image: tnImage.value.trim(),
            expose: exposeSel.value === 'nodeport' ? 'nodeport' : 'cluster',
            nodePort: Number(tnNodePort.value.trim()) || undefined,
            allowFrom: tnAllowFrom.value.trim() || undefined,
          });

          // 把两种用途的连接方式直接摊开，省得用户自己拼地址；
          // NetworkPolicy 建失败时后端会给 warning 且没有 portForward，所以按字段是否存在来渲染。
          const u = tnSocksUser.value.trim();
          const p2 = tnSocksPass.value;
          const out: Node[] = [
            el(
              'div',
              { class: 'hint hint-info' },
              el('div', {
                class: 'hint-title',
                text: `已创建 ${res.name}（${res.expose === 'nodeport' ? '入站+出站' : '仅出站'}）`,
              }),
              el('div', { class: 'hint-detail', text: res.note ?? '' }),
            ),
            copyBlock(
              '出站（集群内 Pod 用）',
              `curl --proxy-user '${u}:${p2}' --proxy socks5h://${res.socksEndpoint} https://api.ipify.org`,
              2,
            ),
          ];
          if (res.externalEndpoint) {
            out.push(
              copyBlock(
                '入站（你本机翻墙用）',
                `curl --proxy-user '${u}:${p2}' --proxy ${res.externalEndpoint} https://api.ipify.org`,
                2,
              ),
            );
          }
          if (res.portForward) out.push(copyBlock('或者 port-forward（不用暴露端口）', res.portForward, 2));
          if (res.warning) {
            out.push(
              el(
                'div',
                { class: 'hint hint-warn' },
                el('div', { class: 'hint-title', text: '部分附属资源没建成' }),
                el('div', { class: 'hint-detail', text: res.warning }),
              ),
            );
          }
          tnCreated.replaceChildren(...out);
          toast(`已创建 ${res.name}`);
          await reload();
        } catch (e) {
          toast(`创建失败：${errorText(e)}`, 'err');
        }
      }, 'primary');

      // 版本候选（实时）：① /api/image-tags（GitHub releases）② 集群里在跑的镜像反推版本（标「集群已有」）
      // 两个来源都拉不到就留可编辑输入框，不阻塞使用。
      try {
        const [tags, imgs] = await Promise.all([
          ctx.api.imageTags('harodggg/xray-wasm').catch(() => null),
          ctx.api.images(true).catch(() => null),
        ]);
        const running = new Set((imgs?.items ?? []).map((i) => i.image));
        const versions: string[] = [];
        for (const t of tags?.tags ?? []) if (t.tag) versions.push(t.tag);
        for (const img of running) {
          if (img.startsWith(`${TN_IMAGE_REPO}:`)) {
            const v = img.slice(TN_IMAGE_REPO.length + 1);
            if (v && !versions.includes(v)) versions.push(v);
          }
        }
        // 双方向（含服务端形态）从 v0.4.0 起才有：默认就选它，别让下拉把人带回旧版本
        if (!versions.includes('v0.4.0')) versions.unshift('v0.4.0');
        tnImageTagSel.replaceChildren(
          ...versions.map((v) => {
            const o = el('option', {
              text: running.has(`${TN_IMAGE_REPO}:${v}`) ? `${v}（集群已有）` : v,
            }) as HTMLOptionElement;
            o.value = v;
            return o;
          }),
          (() => {
            const o = el('option', { text: '自定义（在下面手输完整镜像）' }) as HTMLOptionElement;
            o.value = '__custom__';
            return o;
          })(),
        );
        tnImageTagSel.value = 'v0.4.0';
        tnImage.value = `${TN_IMAGE_REPO}:v0.4.0`;
      } catch {
        /* 拉不到版本就手输 */
      }

      const wjGroup = el(
        'div',
        { class: 'stack' },
        field('节点', wjNode, '只有带 wasm.sh/wasmtime=true 的节点才能跑 wasm 组件；没有该标签的节点已置灰'),
        hasWasmNode
          ? null
          : el(
              'div',
              { class: 'hint hint-warn' },
              el('div', { class: 'hint-title', text: '没有可用的 wasm 节点' }),
              el('div', {
                class: 'hint-detail',
                text: '集群里没有带 wasm.sh/wasmtime=true 的节点，翻墙 Pod 会一直 Pending。先在节点上跑 scripts/install-wasm-runtime.sh。',
              }),
            ),
        el(
          'div',
          { class: 'grid-2-tight' },
          field('入口 NodePort', wjNodePort, '30000-32767；默认 30543。hostPort 被本集群 PodSecurity baseline 禁止'),
          field('副本', wjReplicas, '1-5'),
        ),
        el(
          'div',
          { class: 'grid-2-tight' },
          field('伪装站点 SNI', wjSni, '未认证的探测者会看到这个站点的真实证书'),
          field('回落目标 dest', wjDest, '默认跟随 SNI 变成 <sni>:443'),
        ),
        field('对外地址（可留空）', wjPublicHost, '缺省用面板访问地址，其次节点 IP；也可以手填域名/公网 IP'),
        field('镜像', wjImage, '默认 docker.io/k3s-wasm/xray-wasm-cli:v0.4.0（同一个 wasm 组件的服务端形态）'),
        field('放行来源 allowFrom', wjAllowFrom, 'REALITY 入口默认对公网开放；能收窄就收窄'),
        createWj,
        wjCreated,
      );

      const tnGroup = el(
        'div',
        { class: 'stack' },
        field('生成用途', genUsage, '决定生成的配置里给不给「外部经节点IP」那一套'),
        generate,
        generated,
        field('vless:// 链接（可选）', tnVless, '粘贴后自动填下面几项；也可以手工填'),
        field('服务端地址', tnServer, 'VLESS + REALITY 的 ip:port'),
        field('UUID', tnUuid),
        field('REALITY 公钥', tnPublicKey),
        el('div', { class: 'grid-2-tight' }, field('shortId', tnShortId), field('SNI', tnSni)),
        el('div', { class: 'grid-2-tight' }, field('SOCKS5 用户名', tnSocksUser), field('SOCKS5 密码', tnSocksPass)),
        el('div', { class: 'grid-2-tight' }, field('已有 Secret（可选）', tnSecretName), field('SOCKS5 监听', tnListen)),
        field('用途', exposeSel, '入站=外部经节点IP连进来用；出站=集群内 Pod 用它出网。两者可同时具备'),
        el('div', { class: 'grid-2-tight' }, field('NodePort（可选）', tnNodePort), field('放行来源（外部用途时）', tnAllowFrom)),
        field('副本', tnReplicas, '≥2 缓解单连接限制'),
        field('镜像版本', tnImageTagSel, '版本列表实时取自 GitHub releases；标「集群已有」的可直接用'),
        field('镜像', tnImage, '固定仓库 docker.io/k3s-wasm/xray-wasm-cli；选上面的版本会自动填这里，也可手输别的'),
        createTn,
        tnCreated,
      );

      // 两种模式各有自己的 input 节点，切换只切显隐 —— 已填的值留在各自的框里，不会被清掉。
      const applyMode = () => {
        const m = currentMode();
        wjGroup.classList.toggle('hidden', m !== 'walljump');
        tnGroup.classList.toggle('hidden', m !== 'tunnel');
        renderModeInfo();
      };
      modeSel.addEventListener('change', applyMode);

      host.append(
        el(
          'div',
          { class: 'hints' },
          el(
            'div',
            { class: 'hint hint-info' },
            el('div', { class: 'hint-title', text: '一个 wasm 模块，两个方向' }),
            el('div', {
              class: 'hint-detail',
              text:
                'xray-wasm v0.4.0 起同一个 wasm 组件既能当 REALITY 服务端（翻墙：入站 REALITY → 直连出），' +
                '也能当客户端（隧道：SOCKS5 入站 → 经 REALITY 出网）。约束：它作为服务端未实现 XTLS-Vision 流控，' +
                '所以翻墙链接不带 flow；隧道指向这类服务端时 flow 也必须留空。',
            }),
          ),
        ),
        el(
          'div',
          { class: 'grid-form' },
          card(
            '新建翻墙入口 / 隧道',
            el(
              'div',
              { class: 'stack' },
              field('名称', name, 'DNS-1123：小写字母数字和 -；两种模式共用'),
              field('模式', modeSel, '翻墙=国内直连公网入口，出口走该节点网络；隧道=集群内流量经远端 REALITY 出网'),
              modeInfo,
              wjGroup,
              tnGroup,
            ),
          ),
          el('div', { class: 'stack' }, el('h3', { text: '翻墙入口 / 隧道列表' }), list),
        ),
      );
      applyMode();
    },
    reload,
  };
}
