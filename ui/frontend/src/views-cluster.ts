// 集群侧的只读视图：概览 / 节点 / WASM 运行时 / 工作负载 / 日志。
//
// 这些都是只读的，所以 autoRefresh = true，轮询时整体重渲染没问题。

import type { ClusterEvent, PodInfo, RuntimeCategoryCounts, RuntimeInfo, Summary, Workload } from './api';
import {
  age,
  badge,
  button,
  card,
  clear,
  el,
  empty,
  errorText,
  input,
  pairsText,
  stat,
  table,
  toast,
} from './dom';
import {
  CATEGORY_LABEL,
  CATEGORY_ORDER,
  categoryBadge,
  categoryCountsText,
  categoryFilterSelect,
  categoryLabel,
  categoryRank,
  matchCategory,
  normalizeCategory,
} from './runtime-ui';
import type { CategoryFilter } from './runtime-ui';
import type { Ctx, ViewInstance } from './view-types';

/** 按类别统计一组 RuntimeClass（概览页的「运行时类别」摘要用） */
function runtimeCounts(runtimes: RuntimeInfo[]): RuntimeCategoryCounts {
  const c: RuntimeCategoryCounts = { wasm: 0, native: 0, gpu: 0, total: runtimes.length };
  for (const r of runtimes) c[normalizeCategory(r.category, r.isWasm)]++;
  return c;
}

// ── 概览 ────────────────────────────────────────────────────────────

export function overviewView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;

  const reload = async () => {
    if (!host) return;
    const s: Summary = await ctx.api.summary();
    render(s);
  };

  function render(s: Summary) {
    if (!host) return;
    clear(host);

    host.append(
      el(
        'div',
        { class: 'stats' },
        stat('节点', `${s.nodes.ready}/${s.nodes.total}`, `具备 wasm 运行时：${s.nodes.wasmCapable}`, s.nodes.ready === s.nodes.total ? 'ok' : 'warn'),
        stat(
          'Pod',
          `${s.pods.running}/${s.pods.total}`,
          s.runtimeCategories ? categoryCountsText(s.runtimeCategories) : `其中 wasm 工作负载：${s.pods.wasm}`,
          'info',
        ),
        stat('命名空间', s.namespaces, undefined, 'muted'),
        stat('运行时类别', s.runtimes.length, categoryCountsText(runtimeCounts(s.runtimes)), 'info'),
        stat('SpinApp', s.spinapps.installed === false ? '未安装 CRD' : s.spinapps.count, undefined, 'muted'),
      ),
    );

    // 需要人注意的配置问题集中放在最上面，而不是散落各处
    const warnings: Node[] = [];
    if (s.nodes.wasmCapable === 0) {
      warnings.push(
        hintBox(
          'warn',
          '没有任何节点具备 wasm 运行时',
          '在节点上执行 scripts/install-wasm-runtime.sh（它会装 shim、写 containerd 配置、打节点标签）。',
        ),
      );
    }
    // 「RuntimeClass 存在但没有节点装了对应 shim」对三个类别都成立，不再只盯 wasm：
    // GPU RuntimeClass 选不中带 nvidia.com/gpu 的节点，是同一类排障问题。
    const broken = s.runtimes.filter((r) => r.misconfigured);
    if (broken.length > 0) {
      const cats = new Set(broken.map((r) => normalizeCategory(r.category, r.isWasm)));
      const reasons: string[] = [];
      if (cats.has('wasm')) reasons.push('wasm：只在一部分节点装了 shim，却忘了给节点打标签（scripts/install-wasm-runtime.sh 会一并处理）');
      if (cats.has('gpu')) reasons.push('GPU：节点没有 nvidia.com/gpu 标签 / device plugin，或 RuntimeClass 的 nodeSelector 选不中它');
      if (cats.has('native')) reasons.push('原生：RuntimeClass 的 nodeSelector 与实际节点标签不一致');
      warnings.push(
        hintBox(
          'warn',
          `RuntimeClass ${broken.map((r) => r.name).join('、')} 存在，但没有节点满足它的 nodeSelector`,
          `通常意味着：${reasons.join('；')}。`,
        ),
      );
    }
    if (s.spinapps.installed === false) {
      warnings.push(
        hintBox(
          'info',
          '集群里没有 SpinApp CRD',
          '要用 SpinKube（SpinApp 面板）就执行 scripts/install-spinkube.sh；不用也完全没问题 —— 默认部署走的是 RuntimeClass wasmtime-wasip2 + Deployment。',
        ),
      );
    }
    if (s.config.proxyUrlSource === 'builtin-default') {
      warnings.push(
        hintBox(
          'info',
          `kube-api-proxy 地址用的是编译期默认值：${s.config.proxyUrl}`,
          '只有在改了命名空间/服务名时才需要覆盖（构建时用 K3S_WASM_PROXY_URL 烘焙进去）。',
        ),
      );
    }
    if (warnings.length > 0) host.append(el('div', { class: 'hints' }, ...warnings));

    host.append(
      el(
        'div',
        { class: 'grid-2' },
        card(
          '集群',
          el(
            'dl',
            { class: 'kv' },
            kv('Kubernetes', s.version?.gitVersion ?? '未知'),
            kv('平台', s.version?.platform ?? '-'),
            kv('kube-api-proxy', s.config.proxyUrl),
            kv('配置来源', s.config.proxyUrlSource),
            kv('默认命名空间', s.config.defaultNamespace),
          ),
        ),
        card(
          '运行时能力',
          s.runtimes.length === 0
            ? empty('没有 RuntimeClass', '先跑 scripts/install-wasm-runtime.sh')
            : table(
                ['名称', '类别', 'handler', '可调度节点', '状态'],
                s.runtimes
                  .slice()
                  .sort((a, b) => categoryRank(a.category, a.isWasm) - categoryRank(b.category, b.isWasm))
                  .map((r: RuntimeInfo) => [
                    r.name,
                    categoryBadge(r.category, r.isWasm),
                    r.handler,
                    String(r.capableNodes),
                    r.misconfigured ? badge('无可用节点', 'err') : badge('可调度', 'ok'),
                  ]),
              ),
        ),
      ),
    );
  }

  return {
    title: '概览',
    autoRefresh: true,
    mount(h, c) {
      host = h;
      ctx = c;
      return reload();
    },
    reload,
  };
}

function kv(k: string, v: string): Node {
  return el('div', { class: 'kv-row' }, el('dt', { text: k }), el('dd', { text: v }));
}

function hintBox(kind: 'info' | 'warn' | 'err', title: string, detail: string): HTMLElement {
  return el(
    'div',
    { class: `hint hint-${kind}` },
    el('div', { class: 'hint-title', text: title }),
    el('div', { class: 'hint-detail', text: detail }),
  );
}

// ── 节点 ────────────────────────────────────────────────────────────

export function nodesView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;

  const reload = async () => {
    if (!host) return;
    const nodes = await ctx.api.nodes();
    clear(host);
    if (nodes.length === 0) {
      host.append(empty('没有节点'));
      return;
    }
    host.append(
      table(
        ['名称', '状态', '架构 / kubelet', '容器运行时', '运行时类别', 'GPU 能力', '可调度', 'CPU / 内存'],
        nodes.map((n) => [
          el('span', { class: 'mono', text: n.name }),
          n.ready ? badge('Ready', 'ok') : badge('NotReady', 'err'),
          `${n.arch} · ${n.kubeletVersion ?? '-'}`,
          n.runtime ?? '-',
          el(
            'span',
            { class: 'badges' },
            n.wasm.spin || n.wasm.wasmtime ? categoryBadge('wasm') : null,
            n.wasm.spin ? badge('spin', 'muted') : null,
            n.wasm.wasmtime ? badge('wasmtime', 'muted') : null,
            !n.wasm.spin && !n.wasm.wasmtime ? badge('无', 'muted') : null,
          ),
          // GPU 存在性来自节点标签/容量（后端推导），present=false 时 count 必为 0
          n.gpu?.present
            ? el('span', { class: 'badges' }, categoryBadge('gpu'), badge(`×${n.gpu.count}`, 'muted'))
            : badge('无', 'muted'),
          n.unschedulable ? badge('已封锁', 'warn') : badge('正常', 'muted'),
          `${n.capacity.cpu} / ${n.capacity.memory}`,
        ]),
      ),
    );
  };

  return {
    title: '节点',
    autoRefresh: true,
    mount(h, c) {
      host = h;
      ctx = c;
      return reload();
    },
    reload,
  };
}

// ── WASM 运行时 ─────────────────────────────────────────────────────

export function runtimesView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;
  let filter: CategoryFilter = 'all';

  const reload = async () => {
    if (!host) return;
    const rs = await ctx.api.runtimes();
    render(rs);
  };

  function render(rs: RuntimeInfo[]) {
    if (!host) return;
    clear(host);

    host.append(
      hintBox(
        'info',
        '三个运行时类别',
        'WASM：wasmtime-spin-v2（handler spin）跑 Spin 应用，wasmtime-wasip2（handler wasmtime）跑标准 wasi:http/proxy 组件；' +
          '原生：默认 runc 容器；GPU：需要节点装好 GPU 设备插件、RuntimeClass 的 nodeSelector 选得中它。',
      ),
    );

    // 配置问题独立于类别：任何「有 RuntimeClass 但没有可用节点」都要显式提示
    const broken = rs.filter((r) => r.misconfigured);
    if (broken.length > 0) {
      const cats = new Set(broken.map((r) => normalizeCategory(r.category, r.isWasm)));
      const reasons: string[] = [];
      if (cats.has('wasm')) reasons.push('wasm：只在一部分节点装了 shim，漏打节点标签');
      if (cats.has('gpu')) reasons.push('GPU：节点没有 nvidia.com/gpu 标签 / device plugin，或 nodeSelector 选不中');
      if (cats.has('native')) reasons.push('原生：RuntimeClass 的 nodeSelector 与实际节点标签不一致');
      host.append(
        hintBox(
          'warn',
          `RuntimeClass ${broken.map((r) => r.name).join('、')} 存在，但没有节点满足它的 nodeSelector`,
          `通常意味着：${reasons.join('；')}。`,
        ),
      );
    }

    if (rs.length === 0) {
      host.append(empty('集群里没有 RuntimeClass', '在有 shim 的节点上执行 scripts/install-wasm-runtime.sh'));
      return;
    }

    const select = categoryFilterSelect(filter, (v) => {
      filter = v;
      render(rs);
    });
    host.append(
      el(
        'div',
        { class: 'toolbar' },
        el('label', { class: 'inline-field' }, el('span', { text: '类别' }), select),
        el('span', { class: 'muted', text: categoryCountsText(runtimeCounts(rs)) }),
      ),
    );

    // 按类别分组展示，而不是只分「wasm / 非 wasm」—— GPU 与原生各有各的排障方式
    const groups = CATEGORY_ORDER.map((c) => ({
      c,
      items: rs.filter((r) => normalizeCategory(r.category, r.isWasm) === c),
    })).filter((g) => g.items.length > 0 && (filter === 'all' || filter === g.c));

    if (groups.length === 0) {
      host.append(empty(`没有「${categoryLabel(filter)}」类别的 RuntimeClass`));
      return;
    }

    for (const g of groups) {
      host.append(el('h3', { class: 'section-title', text: `${CATEGORY_LABEL[g.c]}（${g.items.length}）` }));
      host.append(
        table(
          ['名称', 'handler', 'nodeSelector', '可调度节点'],
          g.items.map((r) => [
            el('span', { class: 'mono', text: r.name }),
            el('span', { class: 'mono', text: r.handler }),
            // 不要在这里直接 Object.entries：nodeSelector 可能是 null/空
            el('span', { class: 'mono', text: pairsText(r.nodeSelector, '（无 → 无法判定节点）') }),
            r.capableNodes === null
              ? badge('无法判定', 'muted')
              : r.misconfigured
                ? el('span', {}, badge('0', 'err'), el('span', { class: 'muted', text: ' 没有节点装了对应 shim' }))
                : badge(String(r.capableNodes), 'ok'),
          ]),
        ),
      );
    }
  }

  return {
    title: 'WASM 运行时',
    autoRefresh: true,
    mount(h, c) {
      host = h;
      ctx = c;
      return reload();
    },
    reload,
  };
}

// ── 工作负载 ────────────────────────────────────────────────────────

export function workloadsView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;
  // 默认看全部（和旧版「只看 WASM」关闭时的默认一致），避免打开页面就少一半数据
  let filter: CategoryFilter = 'all';

  const reload = async () => {
    if (!host) return;
    const items = await ctx.api.workloads(ctx.currentNamespace);
    const shown: Workload[] = items
      .filter((i) => matchCategory(filter, i.runtimeCategory, i.isWasm))
      .sort(
        (a, b) =>
          categoryRank(a.runtimeCategory, a.isWasm) - categoryRank(b.runtimeCategory, b.isWasm) ||
          (a.name < b.name ? -1 : a.name > b.name ? 1 : 0),
      );
    list.replaceChildren(renderList(shown, items.length));
  };

  function renderList(shown: Workload[], total: number): HTMLElement {
    if (total === 0) {
      return empty(`命名空间 ${ctx.currentNamespace} 下没有 Deployment`);
    }
    if (shown.length === 0) {
      return empty(
        `有 ${total} 个 Deployment，但没有「${categoryLabel(filter)}」类别的`,
        '判定依据是 Pod 模板里的 RuntimeClassName',
      );
    }
    return table(
      ['名称', '命名空间', '副本', '镜像', '运行时', '创建'],
      shown.map((w) => [
        el('span', { class: 'mono', text: w.name }),
        w.namespace,
        w.replicas === 0
          ? badge('0（已停）', 'warn')
          : `${w.readyReplicas}/${w.replicas}`,
        el('span', { class: 'mono truncate', text: w.image ?? '-', title: w.image ?? '' }),
        el(
          'span',
          { class: 'badges' },
          categoryBadge(w.runtimeCategory, w.isWasm),
          el('span', { class: 'mono muted', text: w.runtimeClass || (w.isWasm ? 'wasm' : 'runc') }),
        ),
        age(w.createdAt),
      ]),
    );
  }

  const list = el('div', {});

  return {
    title: '工作负载',
    autoRefresh: true,
    mount(h, c) {
      host = h;
      ctx = c;
      clear(host);

      const nsSelect = el('select', { class: 'input' }) as HTMLSelectElement;
      for (const ns of ['_all', ...ctx.namespaces]) {
        const opt = el('option', { text: ns === '_all' ? '全部命名空间' : ns }) as HTMLOptionElement;
        opt.value = ns;
        nsSelect.append(opt);
      }
      nsSelect.value = ctx.currentNamespace;
      nsSelect.addEventListener('change', () => {
        ctx.setNamespace(nsSelect.value);
        void reload();
      });

      // 三分类过滤器：取代旧的「只看 WASM」二选一（GPU/原生 也需要单独看）
      const catSelect = categoryFilterSelect(filter, (v) => {
        filter = v;
        void reload();
      });

      host.append(
        el(
          'div',
          { class: 'toolbar' },
          el('label', { class: 'inline-field' }, el('span', { text: '命名空间' }), nsSelect),
          el('label', { class: 'inline-field' }, el('span', { text: '运行时类别' }), catSelect),
        ),
        list,
      );
      return reload();
    },
    reload,
  };
}

// ── 日志 ────────────────────────────────────────────────────────────

export function logsView(params: URLSearchParams): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;
  let pods: PodInfo[] = [];
  /** 格式 "namespace/name"，这样切命名空间时也能正确回选 */
  let selected = params.get('pod') ?? '';
  let tail = 200;
  let follow = true;

  const reload = async () => {
    if (!host) return;
    pods = await ctx.api.pods(ctx.currentNamespace);
    if (!selected && pods.length > 0) {
      selected = `${pods[0]!.namespace}/${pods[0]!.name}`;
    }
    renderPickers();
    await loadLog();
  };

  async function loadLog() {
    const pre = host?.querySelector('pre.log');
    if (!pre) return;
    if (!selected) {
      pre.textContent = '（没有可选中的 Pod）';
      return;
    }
    const [ns, name] = selected.split('/');
    if (!ns || !name) return;
    try {
      const res = await ctx.api.logs(ns, name, tail);
      pre.textContent = res.log.trim() === '' ? '（日志为空）' : res.log;
      pre.scrollTop = pre.scrollHeight;
    } catch (e) {
      pre.textContent = `读取日志失败：${errorText(e)}`;
    }
  }

  function renderPickers() {
    const box = host?.querySelector('.log-pickers');
    if (!box) return;
    const select = el('select', { class: 'input' }) as HTMLSelectElement;
    for (const p of pods) {
      const value = `${p.namespace}/${p.name}`;
      const opt = el('option', {
        text: `${p.name} · ${categoryLabel(p.runtimeCategory, p.isWasm)} · ${p.phase}${p.restarts > 0 ? ` · 重启${p.restarts}` : ''}`,
      }) as HTMLOptionElement;
      opt.value = value;
      select.append(opt);
    }
    select.value = selected;
    select.addEventListener('change', () => {
      selected = select.value;
      void loadLog();
    });
    box.replaceChildren(select);
  }

  return {
    title: '日志',
    autoRefresh: true,
    async mount(h, c) {
      host = h;
      ctx = c;
      clear(host);

      const tailSelect = el('select', { class: 'input' }) as HTMLSelectElement;
      for (const n of [100, 200, 500, 1000]) {
        const opt = el('option', { text: `最近 ${n} 行` }) as HTMLOptionElement;
        opt.value = String(n);
        tailSelect.append(opt);
      }
      tailSelect.value = String(tail);
      tailSelect.addEventListener('change', () => {
        tail = Number(tailSelect.value);
        void loadLog();
      });

      const followBox = input('', '', 'checkbox') as HTMLInputElement;
      followBox.checked = follow;
      followBox.addEventListener('change', () => {
        follow = followBox.checked;
      });

      host.append(
        el(
          'div',
          { class: 'toolbar' },
          el('div', { class: 'log-pickers' }),
          tailSelect,
          el('label', { class: 'checkbox' }, followBox, el('span', { text: '跟随刷新' })),
          button('立刻拉取', () => loadLog()),
        ),
        el('pre', { class: 'log', text: '加载中…' }),
      );
      await reload();
    },
    async reload() {
      if (follow) await loadLog();
    },
  };
}

// ── 事件（排查用） ──────────────────────────────────────────────────

export function eventsView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;

  const reload = async () => {
    if (!host) return;
    let items: ClusterEvent[] = [];
    let err = '';
    try {
      items = await ctx.api.events(ctx.currentNamespace);
    } catch (e) {
      err = errorText(e);
    }
    clear(host);
    if (err) {
      host.append(empty('读取事件失败', err));
      return;
    }
    if (items.length === 0) {
      host.append(empty('没有事件'));
      return;
    }
    host.append(
      table(
        ['类型', '原因', '对象', '次数', '最后出现', '消息'],
        items.map((e) => [
          e.type === 'Warning' ? badge('Warning', 'warn') : badge(e.type || '-', 'muted'),
          e.reason ?? '-',
          el('span', { class: 'mono', text: e.object }),
          String(e.count ?? 1),
          age(e.lastSeen),
          el('span', { class: 'wrap', text: e.message ?? '' }),
        ]),
      ),
    );
  };

  return {
    title: '事件',
    autoRefresh: true,
    mount(h, c) {
      host = h;
      ctx = c;
      return reload();
    },
    reload,
  };
}

/** 供其他视图复用的错误提示 */
export function showError(prefix: string, e: unknown): void {
  toast(`${prefix}：${errorText(e)}`, 'err');
}
