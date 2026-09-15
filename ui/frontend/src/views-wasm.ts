// WASM 相关的两类「可写」视图：SpinKube 的 SpinApp、以及 xray-wasm 隧道。
//
// 这两个视图里有表单，所以 autoRefresh = false：
// 全局轮询只会调用 reload()，而 reload 只重画列表，不动表单 ——
// 否则用户正在输入时会被轮询清空。

import type { SpinApp, SpinAppExecutorInfo, XrayTunnel } from './api';
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
              field('镜像', image, 'kubelet 会去拉这个引用，别写 localhost'),
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

// ── xray-wasm 隧道 ──────────────────────────────────────────────────

// ── xray-wasm 隧道 ──────────────────────────────────────────────────

export function xrayView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;
  const list = el('div', { class: 'stack' });

  const reload = async () => {
    if (!host) return;
    try {
      const res = await ctx.api.tunnels(ctx.currentNamespace);
      if (res.items.length === 0) {
        list.replaceChildren(
          empty('还没有隧道', `创建出来的工作负载会跑在 RuntimeClass ${res.shim.runtimeClass} 上`),
        );
        return;
      }
      list.replaceChildren(...res.items.map(renderTunnel));
    } catch (e) {
      list.replaceChildren(empty('读取隧道失败', errorText(e)));
    }
  };

  function renderTunnel(t: XrayTunnel): HTMLElement {
    const port = (t.tunnel.listen ?? '0.0.0.0:1080').split(':').pop() ?? '1080';
    return card(
      t.name,
      el(
        'div',
        { class: 'stack' },
        el(
          'dl',
          { class: 'kv' },
          // 方向：这条面板目前只能创建「出站」隧道（客户端把集群内流量送出去）；
          // 「入站」在这里的含义是「谁能连进它的监听端口」，由 Service 类型决定。
          kv('方向', t.directionLabel ?? '出站'),
          kv('出站链路', `${t.egress?.via ?? t.tunnel.server ?? '-'} · ${t.egress?.protocol ?? 'SOCKS5 → VLESS+REALITY'}`),
          kv('入站入口', t.ingress?.endpoint ?? `${t.name}.${t.namespace}.svc.cluster.local:${port}`),
          kv(
            '入站暴露',
            `${t.ingress?.exposure?.reach ?? '未知'}${
              t.ingress?.exposure?.serviceType ? ` · Service 类型 ${t.ingress.exposure.serviceType}` : ''
            }`,
          ),
          kv('命名空间', t.namespace),
          kv('SNI', t.tunnel.sni ?? '-'),
          kv('shortId', t.tunnel.shortId ?? '-'),
          kv('UUID', t.tunnel.hasUuid ? '已配置（不回显）' : '未配置'),
          kv('监听', t.tunnel.listen ?? '-'),
          kv('副本', t.replicas === 0 ? '0（已停）' : `${t.readyReplicas}/${t.replicas}`),
          kv('运行时', t.runtimeClass),
          kv('镜像', t.image ?? '-'),
        ),
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
          button(
            '删除',
            async () => {
              if (!confirm(`删除隧道 ${t.namespace}/${t.name}（Deployment + Service + ConfigMap）？`)) return;
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
      el(
        'span',
        { class: 'badges' },
        badge('出站', 'ok'),
        t.ingress?.exposure?.public === true
          ? badge('公网可达', 'err')
          : t.ingress?.exposure?.public === false
            ? badge('仅集群内', 'muted')
            : badge('暴露未知', 'warn'),
        badge(t.runtimeClass, 'info'),
      ),
    );
  }

  return {
    title: 'Xray 隧道',
    autoRefresh: false,
    async mount(h, c) {
      host = h;
      ctx = c;
      clear(host);

      const name = input('tokyo');
      const vlessLink = input('可选：直接粘贴 vless:// 链接，下面几项会自动补全');
      const server = input('203.0.113.10:443');
      const uuid = input('xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx');
      const publicKey = input('REALITY 公钥（xray x25519 的 Password/公钥）');
      const shortId = input('9f1c2a3b');
      const sni = input('www.amazon.com');
      const socksUser = input('xrayuser', 'xrayuser');
      const socksPass = input('必填：否则 0.0.0.0 上的无认证 SOCKS5 就是开放代理');
      const secretName = input('可选：引用已存在的 Secret（填了就不由控制台创建）');
      const listen = input('0.0.0.0:1080', '0.0.0.0:1080');
      const replicas = input('2', '2', 'number') as HTMLInputElement;
      const image = input('docker.io/k3s-wasm/xray-wasm-cli:v0.1.0');

      // vless:// 解析放在后端做（前端只把原文传过去），避免两处实现漂移
      vlessLink.addEventListener('change', () => {
        const m = /^vless:\/\/([^@]+)@([^?]+)\?(.*)$/.exec(vlessLink.value.trim());
        if (!m) return;
        uuid.value = m[1] ?? '';
        server.value = m[2] ?? '';
        for (const kv of (m[3] ?? '').split('&')) {
          const [k, v] = kv.split('=');
          if (k === 'pbk') publicKey.value = decodeURIComponent(v ?? '');
          if (k === 'sid') shortId.value = decodeURIComponent(v ?? '');
          if (k === 'sni') sni.value = decodeURIComponent(v ?? '');
        }
      });

      const generated = el('div', { class: 'stack' });
      const name2 = name; // 生成时也用它当链接里的标签

      // ① 自动生成：一次给全套参数（REALITY 密钥对 + UUID + shortId + SOCKS 凭据），
      //    并把服务端 config.json 与 vless:// 链接一起显示出来，省掉手工拆字段。
      const generate = button('① 自动生成参数', async () => {
        try {
          const g = await ctx.api.generateTunnel({
            server: server.value.trim() || undefined,
            sni: sni.value.trim() || undefined,
            name: name2.value.trim() || undefined,
          });
          uuid.value = g.uuid;
          publicKey.value = g.publicKey;
          shortId.value = g.shortId;
          sni.value = g.sni;
          socksUser.value = g.socksUser;
          socksPass.value = g.socksPass;
          if (!server.value.trim()) server.value = g.server;
          toast('已生成并填入；请把服务端配置粘到你的服务器上');

          const block = (label: string, text: string, rows: number) => {
            const ta = el('textarea', { class: 'input mono', attrs: { readonly: '', rows: String(rows) } }) as HTMLTextAreaElement;
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
          };

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
            block('vless:// 分享链接（导入官方客户端 / xrayTun）', g.vlessLink, 3),
            block('服务端 config.json（粘到服务器）', JSON.stringify(g.serverConfig, null, 2), 14),
            block('客户端 config.json（用官方 Xray 先验证服务端）', JSON.stringify(g.clientConfig, null, 2), 12),
            el(
              'ul',
              { class: 'notes' },
              ...g.notes.map((n) => el('li', { text: n })),
            ),
          );
        } catch (e) {
          toast(`生成失败：${errorText(e)}`, 'err');
        }
      }, 'primary');

      const create = button('② 创建隧道', async () => {
        try {
          await ctx.api.createTunnel({
            name: name.value.trim(),
            namespace: ctx.currentNamespace === '_all' ? ctx.defaultNamespace : ctx.currentNamespace,
            vlessLink: vlessLink.value.trim() || undefined,
            server: server.value.trim(),
            uuid: uuid.value.trim(),
            publicKey: publicKey.value.trim(),
            shortId: shortId.value.trim(),
            sni: sni.value.trim(),
            socksUser: socksUser.value.trim(),
            socksPass: socksPass.value,
            secretName: secretName.value.trim() || undefined,
            listen: listen.value.trim(),
            replicas: Number(replicas.value || '2'),
            image: image.value.trim(),
          });
          toast(`已创建 ${name.value}`);
          name.value = '';
          await reload();
        } catch (e) {
          toast(`创建失败：${errorText(e)}`, 'err');
        }
      }, 'primary');

      host.append(
        el(
          'div',
          { class: 'hints' },
          el(
            'div',
            { class: 'hint hint-warn' },
            el('div', { class: 'hint-title', text: 'xray-wasm 目前还跑不起来 —— 这是真话，不是保守说法' }),
            el('div', {
              class: 'hint-detail',
              text:
                'xray-wasm 仓库里 xt-wasm-cli 的隧道层尚未接入（main.rs 现在直接 exit 2），' +
                '所以这个面板创建出来的 Pod 会立刻退出。面板本身、配置下发、扩缩容、Service 暴露都是可用且已实现的；' +
                '等 xt-wasm-cli 的 M2/M3 落地后把它们接上即可。另外 wasmtime shim 默认不授予出站 TCP，' +
                '需要出站能力才能建隧道，这一点见 docs/03-xray-wasm.md。',
            }),
          ),
        ),
        el(
          'div',
          { class: 'grid-form' },
          card(
            '新建隧道',
            el(
              'div',
              { class: 'stack' },
              el('div', { class: 'grid-2-tight' }, field('名称', name), field('副本（≥2 缓解单连接限制）', replicas)),
              generate,
              generated,
              field('vless:// 链接（可选）', vlessLink, '粘贴后自动填下面几项；也可以手工填'),
              field('服务端地址', server, 'VLESS + REALITY 的 ip:port'),
              field('UUID', uuid),
              field('REALITY 公钥', publicKey),
              el('div', { class: 'grid-2-tight' }, field('shortId', shortId), field('SNI', sni)),
              el('div', { class: 'grid-2-tight' }, field('SOCKS5 用户名', socksUser), field('SOCKS5 密码', socksPass)),
              el('div', { class: 'grid-2-tight' }, field('已有 Secret（可选）', secretName), field('SOCKS5 监听', listen)),
              field('镜像', image),
              create,
            ),
          ),
          el('div', { class: 'stack' }, el('h3', { text: '隧道列表' }), list),
        ),
      );
      await reload();
    },
    reload,
  };
}
