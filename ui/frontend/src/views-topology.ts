// 网络拓扑视图：一次拉整张图，用内联 SVG 画分层布局 —— 不引任何图形库。
//
// 三个关键设计：
//   1. revision 去重：后端保证同一 revision 的节点/边完全一致，所以内容没变时
//      直接跳过重排 —— 否则每 5 秒整图重建一次，正在看的人会晕。
//   2. 位置平滑过渡：节点/边元素跨轮次复用，只改 transform / path，让 CSS transition
//      把重排显示成「移动」而不是「闪一下」。
//   3. 变化高亮：新增节点绿色、消失的节点保留一轮显示成红色幽灵；
//      ready/pods/restarts 等计数变化的节点脉冲一次。轮询时肉眼能直接看到哪里动了。
//
// 过滤全部在客户端做（不发额外请求）；只有「命名空间」和「显示 Pod」会重新请求，
// 因为它们本来就对应后端查询参数。

import { ApiError } from './api';
import type { TopologyEdge, TopologyGraph, TopologyNode } from './api';
import { badge, clear, el, empty, errorText, pairsText } from './dom';
import {
  categoryBadge,
  categoryFilterSelect,
  categoryLabel,
  matchCategory,
  normalizeCategory,
} from './runtime-ui';
import type { CategoryFilter } from './runtime-ui';
import { LANE_TITLE, layoutTopology } from './topology-layout';
import type { LayoutEdge, LayoutNode } from './topology-layout';
import type { Ctx, ViewInstance } from './view-types';

const SVG_NS = 'http://www.w3.org/2000/svg';
/** 与 main.ts 的 POLL_MS 对齐；写在界面上是为了让人知道不用手点刷新 */
const LIVE_HINT = '实时 · 每 5 秒';

function svgEl<K extends keyof SVGElementTagNameMap>(
  tag: K,
  attrs: [string, string][] = [],
  text?: string,
): SVGElementTagNameMap[K] {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of attrs) node.setAttribute(k, v);
  if (text !== undefined) node.textContent = text;
  return node;
}

function truncate(s: string, max: number): string {
  return s.length <= max ? s : `${s.slice(0, max - 1)}…`;
}

/** dom.ts 的 clear() 只接受 HTMLElement；SVG 的 <g> 是 SVGGElement，单独包一个。 */
function clearSvg(node: SVGElement): void {
  node.replaceChildren();
}

/** 详情面板只展示已知字段：meta 的键随 kind 变化，不能对后端数据做 Object.entries。 */
const META_FIELDS: Record<string, [string, string][]> = {
  node: [
    ['ready', '就绪'], ['wasmtime', 'wasmtime'], ['spin', 'spin'], ['gpu', 'GPU'],
    ['gpuCount', 'GPU 数'], ['pods', 'Pod 数'], ['cpu', 'CPU'], ['memory', '内存'],
    ['kubeletVersion', 'kubelet'],
  ],
  namespace: [['workloads', '工作负载'], ['pods', 'Pod 数']],
  workload: [
    ['namespace', '命名空间'], ['workloadKind', '类型'], ['runtimeClass', 'RuntimeClass'],
    ['desired', '期望副本'], ['ready', '就绪副本'], ['pods', 'Pod 数'],
    ['nodeSpread', '节点分布'], ['image', '镜像'], ['createdAt', '创建时间'],
  ],
  service: [
    ['namespace', '命名空间'], ['type', '类型'], ['clusterIP', 'ClusterIP'],
    ['ports', '端口'], ['endpoints', '端点'], ['external', '外部地址'],
    ['matchedWorkloads', '匹配的工作负载'],
  ],
  ingress: [
    ['namespace', '命名空间'], ['className', 'IngressClass'], ['tls', 'TLS'],
    ['rules', '规则'], ['serviceCount', '后端 Service 数'],
  ],
  networkPolicy: [
    ['namespace', '命名空间'], ['policyTypes', '策略类型'],
    ['ingressRules', '入站规则数'], ['egressRules', '出站规则数'],
    ['matchedWorkloads', '匹配的工作负载'],
  ],
  pod: [
    ['namespace', '命名空间'], ['node', '节点'], ['phase', '阶段'], ['ready', '就绪'],
    ['restarts', '重启'], ['workload', '所属工作负载'], ['podIP', 'Pod IP'],
  ],
};

/** 兜底字段：kind 认不出来时至少把通用字段列出来 */
const META_FALLBACK: [string, string][] = [
  ['namespace', '命名空间'], ['node', '节点'], ['phase', '阶段'],
  ['ready', '就绪'], ['restarts', '重启'], ['podIP', 'Pod IP'],
];

function metaText(v: unknown): string {
  if (v === null || v === undefined) return '-';
  if (typeof v === 'boolean') return v ? '是' : '否';
  if (typeof v === 'object') {
    // 数组（ports/rules）直接序列化；map（nodeSpread）交给 pairsText 拍平成 k=v
    return Array.isArray(v) ? JSON.stringify(v) : pairsText(v);
  }
  return String(v);
}

/** 用于「计数变化」判断的指纹：只取会随负载变化的键 */
function nodeSignature(n: TopologyNode): string {
  const m = n.meta;
  return [
    String(m['ready'] ?? ''),
    String(m['desired'] ?? ''),
    String(m['pods'] ?? ''),
    String(m['restarts'] ?? ''),
    String(m['endpoints'] ?? ''),
    String(m['phase'] ?? ''),
  ].join('|');
}

const EXTERNAL_EDGE_KINDS = new Set(['routes', 'exposes', 'runs-on', 'belongs-to']);

function pushAdj(map: Map<string, string[]>, key: string, value: string): void {
  const list = map.get(key);
  if (list) list.push(value);
  else map.set(key, [value]);
}

/** 「有外部入口」= 从任意 Ingress 出发、沿 routes/exposes/runs-on/belongs-to 可达（无向）。 */
function externalReachable(nodes: TopologyNode[], edges: TopologyEdge[]): Set<string> {
  const adj = new Map<string, string[]>();
  for (const e of edges) {
    if (!EXTERNAL_EDGE_KINDS.has(e.kind)) continue;
    pushAdj(adj, e.from, e.to);
    pushAdj(adj, e.to, e.from);
  }
  const seen = new Set<string>();
  const queue = nodes.filter((n) => n.kind === 'ingress').map((n) => n.id);
  while (queue.length > 0) {
    const id = queue.pop()!;
    if (seen.has(id)) continue;
    seen.add(id);
    for (const next of adj.get(id) ?? []) if (!seen.has(next)) queue.push(next);
  }
  return seen;
}

/** 类别过滤：留下匹配类别的工作负载/Pod，再保留被它们引用到的结构节点（多轮收敛）。 */
function filterByCategory(
  nodes: TopologyNode[],
  edges: TopologyEdge[],
  category: CategoryFilter,
): { nodes: TopologyNode[]; edges: TopologyEdge[] } {
  if (category === 'all') return { nodes, edges };
  const keep = new Set<string>();
  for (const n of nodes) {
    if ((n.kind === 'workload' || n.kind === 'pod') && matchCategory(category, n.category)) {
      keep.add(n.id);
    }
  }
  let changed = true;
  while (changed) {
    changed = false;
    for (const e of edges) {
      const add =
        ((e.kind === 'exposes' || e.kind === 'routes' || e.kind === 'allows') && keep.has(e.to) && !keep.has(e.from)) ||
        ((e.kind === 'in' || e.kind === 'runs-on' || e.kind === 'belongs-to') && keep.has(e.from) && !keep.has(e.to));
      if (!add) continue;
      keep.add(e.kind === 'exposes' || e.kind === 'routes' || e.kind === 'allows' ? e.from : e.to);
      changed = true;
    }
  }
  return {
    nodes: nodes.filter((n) => keep.has(n.id)),
    edges: edges.filter((e) => keep.has(e.from) && keep.has(e.to)),
  };
}

interface NodeSlot {
  el: SVGGElement;
  sig: string;
}

export function topologyView(): ViewInstance {
  let host: HTMLElement | null = null;
  let ctx: Ctx;

  let graph: TopologyGraph | null = null;
  let revision = '';
  let categoryFilter: CategoryFilter = 'all';
  let externalOnly = false;
  let showPods = false;
  let selectedId: string | null = null;

  const slots = new Map<string, NodeSlot>();
  const edgeEls = new Map<string, SVGPathElement>();
  const edgeLabels = new Map<string, SVGTextElement>();
  let ghosts = new Set<string>();
  let edgeGhosts = new Set<string>();

  // DOM 骨架（mount 时建一次；之后只动 SVG 内容与提示文案）
  const canvas = el('div', { class: 'topo-canvas' });
  const scopeBox = el('div', { class: 'topo-scope' });
  const details = el('aside', { class: 'topo-details' });
  const note = el('div', { class: 'topo-note hidden' });
  const svg = svgEl('svg', [['class', 'topo-svg'], ['width', '100'], ['height', '100']]);
  const gHeaders = svgEl('g', [['class', 'topo-layer-headers']]);
  const gEdges = svgEl('g', [['class', 'topo-layer-edges']]);
  const gNodes = svgEl('g', [['class', 'topo-layer-nodes']]);

  const updatedLabel = el('span', { class: 'muted', text: '' });

  /** 空态/错误态用 note 覆盖，而不是清空画布 —— 一旦 <svg> 被移出 DOM 再插回来，
   *  正在进行的 CSS transition 会中断，重排就变成了闪烁。 */
  function setNote(content: Node | null): void {
    clear(note);
    if (content) {
      note.append(content);
      note.classList.remove('hidden');
      svg.classList.add('hidden');
    } else {
      note.classList.add('hidden');
      svg.classList.remove('hidden');
    }
  }

  function initSvg(): void {
    const defs = svgEl('defs');
    const marker = svgEl('marker', [
      ['id', 'topo-arrow'],
      ['viewBox', '0 0 10 10'],
      ['refX', '9'],
      ['refY', '5'],
      ['markerWidth', '7'],
      ['markerHeight', '7'],
      ['orient', 'auto-start-reverse'],
    ]);
    marker.append(svgEl('path', [['d', 'M 0 0 L 10 5 L 0 10 z'], ['class', 'topo-arrow-head']]));
    defs.append(marker);
    svg.append(defs, gHeaders, gEdges, gNodes);
  }

  function visible(): { nodes: TopologyNode[]; edges: TopologyEdge[] } {
    if (!graph) return { nodes: [], edges: [] };
    let nodes = graph.nodes;
    let edges = graph.edges;
    const byCat = filterByCategory(nodes, edges, categoryFilter);
    nodes = byCat.nodes;
    edges = byCat.edges;
    if (externalOnly) {
      const reach = externalReachable(nodes, edges);
      nodes = nodes.filter((n) => reach.has(n.id));
      edges = edges.filter((e) => reach.has(e.from) && reach.has(e.to));
    }
    return { nodes, edges };
  }

  function clearHighlights(): void {
    for (const slot of slots.values()) slot.el.classList.remove('added', 'pulse', 'ghost');
    for (const p of edgeEls.values()) p.classList.remove('added', 'ghost');
    ghosts = new Set();
    edgeGhosts = new Set();
  }

  function makeNodeEl(n: TopologyNode, p: LayoutNode): SVGGElement {
    const cat = normalizeCategory(n.category);
    const g = svgEl('g', [
      ['class', `topo-node kind-${n.kind} cat-${cat}`],
      ['data-id', n.id],
    ]);
    g.append(
      svgEl('rect', [
        ['class', 'topo-box'],
        ['width', String(p.w)],
        ['height', String(p.h)],
        ['rx', '10'],
      ]),
      svgEl('text', [['class', 'topo-label'], ['x', '12'], ['y', '23']], truncate(n.label, 24)),
      svgEl('text', [['class', 'topo-sublabel'], ['x', '12'], ['y', '42']], truncate(n.sublabel, 26)),
      svgEl(
        'text',
        [['class', `topo-cat cat-${cat}`], ['x', String(p.w - 10)], ['y', '20'], ['text-anchor', 'end']],
        categoryLabel(n.category),
      ),
      svgEl('title', [], `${n.id}\n${n.label} · ${n.kind} · ${categoryLabel(n.category)}`),
    );
    g.addEventListener('click', (ev) => {
      ev.stopPropagation();
      select(n.id);
    });
    return g;
  }

  function select(id: string | null): void {
    selectedId = id;
    for (const [nid, slot] of slots) slot.el.classList.toggle('selected', nid === id);
    renderDetails();
  }

  function renderDetails(): void {
    clear(details);
    const n = graph?.nodes.find((x) => x.id === selectedId);
    if (!n) {
      details.append(el('div', { class: 'topo-details-empty muted', text: '点击图中的节点查看详情' }));
      return;
    }
    const fields = META_FIELDS[n.kind] ?? META_FALLBACK;
    const rows: Node[] = [];
    for (const [key, label] of fields) {
      const value = n.meta[key];
      if (value === undefined || value === null) continue;
      rows.push(
        el(
          'div',
          { class: 'kv-row' },
          el('dt', { text: label }),
          el('dd', { text: metaText(value) }),
        ),
      );
    }
    details.append(
      el(
        'div',
        { class: 'stack' },
        el(
          'div',
          { class: 'topo-details-head' },
          el('strong', { text: n.label }),
          categoryBadge(n.category),
          badge(n.kind, 'muted'),
        ),
        el('div', { class: 'mono muted', text: n.id }),
        el('dl', { class: 'kv' }, ...rows),
        el('div', {
          class: 'field-hint',
          text: 'meta 字段按对象类型列出；未列出的键不在界面展示（后端字段随版本变化）。',
        }),
      ),
    );
  }

  function updateSvgSize(layout: { width: number; height: number }): void {
    const w = Math.max(320, layout.width);
    const h = Math.max(200, layout.height);
    svg.setAttribute('viewBox', `0 0 ${w} ${h}`);
    svg.setAttribute('width', String(w));
    svg.setAttribute('height', String(h));
  }

  function renderHeaders(layout: ReturnType<typeof layoutTopology>): void {
    clearSvg(gHeaders);
    for (const h of layout.headers) {
      gHeaders.append(
        svgEl(
          'text',
          [['class', 'topo-header'], ['x', String(h.x)], ['y', String(h.y)]],
          `${LANE_TITLE[h.kind] ?? h.kind} · ${h.count}`,
        ),
      );
    }
  }

  function renderEdge(e: LayoutEdge, animate: boolean): SVGPathElement {
    let p = edgeEls.get(e.key);
    const isNew = !p;
    if (!p) {
      p = svgEl('path', [['class', `topo-edge kind-${e.kind}`], ['marker-end', 'url(#topo-arrow)']]);
      gEdges.append(p);
      edgeEls.set(e.key, p);
    }
    const path = p;
    path.setAttribute('d', e.d);
    path.setAttribute('class', `topo-edge kind-${e.kind}`);
    // 线宽随 count 增长，不写 inline style 会被 CSS 的 stroke-width 盖掉
    path.style.strokeWidth = e.count > 1 ? String(Math.min(4, 1.2 + (e.count - 1) * 0.6)) : '';
    if (isNew && animate) path.classList.add('added');

    // count>1 时在旁边标数字（同一对节点之间的多条关系会被后端合并计数）
    let label = edgeLabels.get(e.key);
    if (e.count > 1) {
      if (!label) {
        label = svgEl('text', [['class', 'topo-edge-label'], ['text-anchor', 'middle']]);
        gEdges.append(label);
        edgeLabels.set(e.key, label);
      }
      label.setAttribute('x', String(e.labelX));
      label.setAttribute('y', String(e.labelY));
      label.textContent = String(e.count);
    } else if (label) {
      label.remove();
      edgeLabels.delete(e.key);
    }
    return path;
  }

  /** 把当前过滤结果同步到 DOM。
   *  animate=true（数据变了）：新增绿、消失变红幽灵、计数变化脉冲。
   *  animate=false（只改了过滤条件）：不产生「变化」高亮，直接增删。 */
  function sync(animate: boolean, nodes: TopologyNode[], edges: TopologyEdge[]): void {
    const layout = layoutTopology(nodes, edges);
    updateSvgSize(layout);
    renderHeaders(layout);

    const alive = new Set(layout.nodes.map((n) => n.id));

    // 上一轮留下的幽灵：这一轮仍然不在图里就真正删掉
    for (const id of ghosts) {
      if (alive.has(id)) continue;
      slots.get(id)?.el.remove();
      slots.delete(id);
    }
    const nextGhosts = new Set<string>();

    const byId = new Map(nodes.map((n) => [n.id, n] as const));
    for (const p of layout.nodes) {
      const n = byId.get(p.id);
      if (!n) continue;
      let slot = slots.get(p.id);
      if (!slot) {
        const nodeEl = makeNodeEl(n, p);
        // 新节点先无动画落位，避免从 SVG 原点飞进来
        nodeEl.style.transition = 'none';
        nodeEl.style.transform = `translate(${p.x}px, ${p.y}px)`;
        gNodes.append(nodeEl);
        nodeEl.getBoundingClientRect();
        nodeEl.style.transition = '';
        slot = { el: nodeEl, sig: nodeSignature(n) };
        slots.set(p.id, slot);
        if (animate) nodeEl.classList.add('added');
      } else {
        slot.el.style.transform = `translate(${p.x}px, ${p.y}px)`;
        // 整体重建 class：自动清掉上一轮的 added/pulse/ghost，同时保住选中态
        slot.el.setAttribute(
          'class',
          `topo-node kind-${n.kind} cat-${normalizeCategory(n.category)}${selectedId === p.id ? ' selected' : ''}`,
        );
        const sig = nodeSignature(n);
        if (animate && sig !== slot.sig) {
          slot.el.getBoundingClientRect(); // 强制回流，让动画能重新触发
          slot.el.classList.add('pulse');
        }
        slot.sig = sig;
      }
    }

    for (const [id, slot] of slots) {
      if (alive.has(id)) continue;
      if (animate) {
        slot.el.classList.add('ghost');
        nextGhosts.add(id);
      } else {
        slot.el.remove();
        slots.delete(id);
      }
    }
    ghosts = nextGhosts;

    const aliveEdges = new Set(layout.edges.map((e) => e.key));
    // 与节点同样：上一轮的幽灵边只留一轮，仍然不在图里就删掉
    for (const key of edgeGhosts) {
      if (aliveEdges.has(key)) continue;
      edgeEls.get(key)?.remove();
      edgeEls.delete(key);
      edgeLabels.get(key)?.remove();
      edgeLabels.delete(key);
    }
    const nextEdgeGhosts = new Set<string>();
    for (const [key, p] of edgeEls) {
      if (aliveEdges.has(key)) continue;
      if (animate) {
        p.classList.add('ghost');
        nextEdgeGhosts.add(key);
      } else {
        p.remove();
        edgeEls.delete(key);
        edgeLabels.get(key)?.remove();
        edgeLabels.delete(key);
      }
    }
    edgeGhosts = nextEdgeGhosts;
    for (const e of layout.edges) renderEdge(e, animate);
  }

  function renderScopeHint(): void {
    clear(scopeBox);
    if (!graph) return;
    const c = graph.counts;
    scopeBox.append(
      el('div', {
        class: 'muted',
        text:
          `节点 ${c.nodes} · 工作负载 ${c.workloads} · 服务 ${c.services} · Ingress ${c.ingresses} · ` +
          `NetworkPolicy ${c.networkPolicies} · Pod ${c.pods}（WASM ${c.wasm} · 原生 ${c.native} · GPU ${c.gpu}）`,
      }),
    );
    if (graph.scope.truncated) {
      scopeBox.append(
        el(
          'div',
          { class: 'hint hint-warn' },
          el('div', { class: 'hint-title', text: '对象数量超过后端上限，部分未显示' }),
          el('div', {
            class: 'hint-detail',
            text: '后端对 Pod（200）与工作负载（60）分别设了上限；切到具体命名空间能看得更全。',
          }),
        ),
      );
    }
    // 可选资源（Service/Ingress/NetworkPolicy/Namespace）读取失败时后端只给原因、不整图报错。
    // 必须显示出来，否则「图里没有 Service」会被误读成「集群里没有 Service」。
    if (graph.notes && graph.notes.length > 0) {
      scopeBox.append(
        el(
          'div',
          { class: 'hint hint-info' },
          el('div', { class: 'hint-title', text: '部分可选资源未纳入图' }),
          el('div', { class: 'hint-detail', text: graph.notes.join('；') }),
        ),
      );
    }
  }

  /** 清空 SVG 图层与跨轮次的状态（空图/过滤到空时用） */
  function resetLayers(): void {
    clearSvg(gHeaders);
    clearSvg(gEdges);
    clearSvg(gNodes);
    slots.clear();
    edgeEls.clear();
    edgeLabels.clear();
    ghosts = new Set();
    edgeGhosts = new Set();
  }

  function renderGraph(animate: boolean): void {
    if (!graph) return;
    if (graph.nodes.length === 0) {
      resetLayers();
      setNote(empty('拓扑里没有可展示的对象', '换个命名空间，或先部署工作负载/服务。'));
      renderScopeHint();
      renderDetails();
      return;
    }
    const vis = visible();
    if (vis.nodes.length === 0) {
      // 过滤器把整张图筛没了：给明确提示，而不是一块空白画布
      resetLayers();
      setNote(
        empty('当前过滤条件下没有可展示的对象', '把「运行时类别」调回全部，或关掉「只看有外部入口」。'),
      );
      renderScopeHint();
      renderDetails();
      return;
    }
    setNote(null);
    if (!animate) clearHighlights();
    sync(animate, vis.nodes, vis.edges);
    renderScopeHint();
    renderDetails();
  }

  function renderError(message: string): void {
    // 清掉 revision，让后端恢复后即使内容没变也会重画一次（否则画布会一直停在错误态）
    revision = '';
    setNote(empty('读取拓扑失败', message));
    clear(scopeBox);
    clear(details);
    details.append(el('div', { class: 'muted', text: '点击图中的节点查看详情' }));
  }

  async function reload(): Promise<void> {
    if (!host) return;
    let next: TopologyGraph;
    try {
      next = await ctx.api.topology(ctx.currentNamespace, showPods);
    } catch (e) {
      // 401 交给 main.ts 切回登录页；其余错误就地展示，并保留上一张图
      if (e instanceof ApiError && e.status === 401) throw e;
      renderError(errorText(e));
      return;
    }
    updatedLabel.textContent = `更新于 ${new Date().toLocaleTimeString()}`;
    const unchanged = revision !== '' && next.revision === revision && graph !== null;
    graph = next;
    if (unchanged) {
      // 内容没变：不重排、不动 DOM，只刷新计数与提示
      renderScopeHint();
      return;
    }
    revision = next.revision;
    renderGraph(true);
  }

  return {
    title: '网络拓扑',
    autoRefresh: true,
    mount(h, c) {
      host = h;
      ctx = c;
      clear(host);
      initSvg();

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

      const catSelect = categoryFilterSelect(categoryFilter, (v) => {
        categoryFilter = v;
        renderGraph(false);
      });

      const podToggle = el('input', { attrs: { type: 'checkbox' } }) as HTMLInputElement;
      podToggle.checked = showPods;
      podToggle.addEventListener('change', () => {
        showPods = podToggle.checked;
        void reload();
      });

      const extToggle = el('input', { attrs: { type: 'checkbox' } }) as HTMLInputElement;
      extToggle.checked = externalOnly;
      extToggle.addEventListener('change', () => {
        externalOnly = extToggle.checked;
        renderGraph(false);
      });

      host.append(
        el(
          'div',
          { class: 'toolbar topo-toolbar' },
          el('label', { class: 'inline-field' }, el('span', { text: '命名空间' }), nsSelect),
          el('label', { class: 'inline-field' }, el('span', { text: '运行时类别' }), catSelect),
          el('label', { class: 'checkbox' }, podToggle, el('span', { text: '显示 Pod' })),
          el('label', { class: 'checkbox' }, extToggle, el('span', { text: '只看有外部入口' })),
          el('span', { class: 'badges topo-legend' }, categoryBadge('wasm'), categoryBadge('native'), categoryBadge('gpu')),
          el('span', { class: 'topo-live' }, badge(LIVE_HINT, 'info'), updatedLabel),
        ),
        scopeBox,
        el('div', { class: 'topo-wrap' }, canvas, details),
      );
      svg.addEventListener('click', () => select(null));
      // svg 与 note 只挂一次，后续 render 只改内容（保住 CSS transition）
      canvas.append(svg, note);
      renderDetails();
      return reload();
    },
    reload,
  };
}
