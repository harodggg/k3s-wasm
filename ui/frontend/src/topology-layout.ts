// 拓扑图的纯布局计算：不碰 DOM、不 import 任何运行时模块（只用 import type），
// 所以可以直接在 Node 里 import 进来做确定性断言（见 scripts/check-topology-layout.mjs）。
//
// 布局决定（为什么这样摆）：
//   1. 分层：ingress → service → workload →（Pod 泳道）→ node，namespace/networkPolicy
//      放在最左侧的旁带。边基本都从左往右，符合「流量怎么进来」的阅读方向；
//      workload→namespace 这种反向边会画成向左的回弯，用虚线区分，不至于看乱。
//   2. 单列限高：一列最多 MAX_LANE_H 高，超出的同类节点另起一个子列（列会向右推）。
//      这比「无限向下长」强 —— 后者在有几十个 Pod 时会变成一根面条，浏览器里也没法一眼看全。
//      高度本身仍可能超过视口，视图层用可滚动画布兜底。
//   3. Pod 紧跟父工作负载：同一工作负载的 Pod 在同一泳道里纵向堆叠，并按「不与上一个
//      Pod 堆叠重叠」顺延，避免两个相邻工作负载的 Pod 叠在一起。

import type { TopologyEdge, TopologyNode } from './api';

const PAD = 24;
const GAP_X = 64;
const GAP_Y = 16;
const LANE_TOP = 54; // 顶部留给列标题
const NODE_W = 190;
const NODE_H = 56;
const POD_W = 150;
const POD_H = 38;
const POD_GAP = 8;
/** 单列最大高度：超过就换子列 */
const MAX_LANE_H = 620;

/** 横向顺序。namespace / networkPolicy 是旁带，pod 泳道在 workload 与 node 之间。 */
const LANE_ORDER = ['namespace', 'networkPolicy', 'ingress', 'service', 'workload', 'pod', 'node'] as const;

const POD_KIND = 'pod';
const WORKLOAD_KIND = 'workload';

export interface LayoutNode {
  id: string;
  kind: string;
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface LayoutEdge {
  key: string;
  from: string;
  to: string;
  kind: string;
  count: number;
  /** SVG path 的 d */
  d: string;
  /** count>1 时计数的落点（贝塞尔中点） */
  labelX: number;
  labelY: number;
}

export interface LayoutHeader {
  kind: string;
  x: number;
  y: number;
  count: number;
}

export interface TopologyLayout {
  width: number;
  height: number;
  nodes: LayoutNode[];
  edges: LayoutEdge[];
  headers: LayoutHeader[];
}

function bySort(a: TopologyNode, b: TopologyNode): number {
  if (a.label !== b.label) return a.label < b.label ? -1 : 1;
  return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
}

function maxPerColumn(): number {
  // +GAP_Y 是为了让最后一行的下边距也算进去
  return Math.max(1, Math.floor((MAX_LANE_H - LANE_TOP + GAP_Y) / (NODE_H + GAP_Y)));
}

function edgePath(
  sx: number,
  sy: number,
  ex: number,
  ey: number,
): { d: string; mx: number; my: number } {
  if (Math.abs(ex - sx) < 1) {
    // 同列/回环：从右侧绕出去再回来，避免路径退化成一个点
    const bulge = 46;
    const c1x = sx + bulge;
    const c2x = ex + bulge;
    const d = `M ${sx} ${sy} C ${c1x} ${sy}, ${c2x} ${ey}, ${ex} ${ey}`;
    return { d, mx: (sx + 3 * c1x + 3 * c2x + ex) / 8, my: (sy + ey) / 2 };
  }
  const rightward = ex > sx;
  const dx = Math.max(40, Math.abs(ex - sx) * 0.45);
  const c1x = rightward ? sx + dx : sx - dx;
  const c2x = rightward ? ex - dx : ex + dx;
  const d = `M ${sx} ${sy} C ${c1x} ${sy}, ${c2x} ${ey}, ${ex} ${ey}`;
  const mx = (sx + 3 * c1x + 3 * c2x + ex) / 8;
  return { d, mx, my: (sy + ey) / 2 };
}

/** 计算整张图的坐标。输入顺序不影响结果（内部会排序），便于测试与轮询时比对。 */
export function layoutTopology(nodes: TopologyNode[], edges: TopologyEdge[]): TopologyLayout {
  const grouped = new Map<string, TopologyNode[]>();
  for (const n of nodes) {
    const list = grouped.get(n.kind);
    if (list) list.push(n);
    else grouped.set(n.kind, [n]);
  }
  for (const list of grouped.values()) list.sort(bySort);

  const podLane = grouped.get(POD_KIND) ?? [];
  const hasPods = podLane.length > 0;
  const perColumn = maxPerColumn();

  // ── 横向：逐列分配 x；同类节点超过单列容量就向右再开一个子列 ──
  const pos = new Map<string, LayoutNode>();
  const headers: LayoutHeader[] = [];
  let cursor = PAD;

  for (const kind of LANE_ORDER) {
    if (kind === POD_KIND) {
      if (hasPods) {
        headers.push({ kind, x: cursor, y: LANE_TOP - 26, count: podLane.length });
        cursor += POD_W + GAP_X;
      }
      continue;
    }
    const items = grouped.get(kind);
    if (!items || items.length === 0) continue;
    const subCount = Math.ceil(items.length / perColumn);
    headers.push({ kind, x: cursor, y: LANE_TOP - 26, count: items.length });
    for (let sub = 0; sub < subCount; sub++) {
      const x = cursor;
      const slice = items.slice(sub * perColumn, (sub + 1) * perColumn);
      slice.forEach((n, i) => {
        pos.set(n.id, { id: n.id, kind, x, y: LANE_TOP + i * (NODE_H + GAP_Y), w: NODE_W, h: NODE_H });
      });
      cursor += NODE_W + GAP_X;
    }
  }

  // ── Pod：挂到父工作负载右侧，纵向顺延 ──
  if (hasPods) {
    const podX = headers.find((h) => h.kind === POD_KIND)!.x;
    const podById = new Map(podLane.map((p) => [p.id, p] as const));
    const podsByParent = new Map<string, TopologyNode[]>();
    const linked = new Set<string>();
    for (const e of edges) {
      if (e.kind !== 'belongs-to') continue;
      const pod = podById.get(e.from);
      if (!pod) continue;
      linked.add(pod.id);
      const list = podsByParent.get(e.to);
      if (list) list.push(pod);
      else podsByParent.set(e.to, [pod]);
    }
    const orphans = podLane.filter((p) => !linked.has(p.id));

    let laneCursor = LANE_TOP;
    for (const w of grouped.get(WORKLOAD_KIND) ?? []) {
      const wp = pos.get(w.id);
      if (!wp) continue;
      const list = (podsByParent.get(w.id) ?? []).sort(bySort);
      if (list.length === 0) continue;
      let y = Math.max(wp.y + (NODE_H - POD_H) / 2, laneCursor);
      for (const p of list) {
        pos.set(p.id, { id: p.id, kind: POD_KIND, x: podX, y, w: POD_W, h: POD_H });
        y += POD_H + POD_GAP;
      }
      laneCursor = y + GAP_Y - POD_GAP;
    }
    for (const p of orphans) {
      pos.set(p.id, { id: p.id, kind: POD_KIND, x: podX, y: laneCursor, w: POD_W, h: POD_H });
      laneCursor += POD_H + POD_GAP;
    }
  }

  // ── 边：两端都在图里才画 ──
  const layoutEdges: LayoutEdge[] = [];
  for (const e of edges) {
    const s = pos.get(e.from);
    const t = pos.get(e.to);
    if (!s || !t) continue;
    const forward = t.x >= s.x;
    const sx = forward ? s.x + s.w : s.x;
    const ex = forward ? t.x : t.x + t.w;
    const sy = s.y + s.h / 2;
    const ey = t.y + t.h / 2;
    const { d, mx, my } = edgePath(sx, sy, ex, ey);
    layoutEdges.push({
      key: `${e.from}|${e.to}|${e.kind}`,
      from: e.from,
      to: e.to,
      kind: e.kind,
      count: e.count,
      d,
      labelX: mx,
      labelY: my - 4,
    });
  }
  layoutEdges.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));

  let maxX = 0;
  let maxY = 0;
  for (const n of pos.values()) {
    maxX = Math.max(maxX, n.x + n.w);
    maxY = Math.max(maxY, n.y + n.h);
  }
  return {
    width: maxX + PAD,
    height: maxY + PAD,
    nodes: [...pos.values()].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)),
    edges: layoutEdges,
    headers,
  };
}

/** 列标题文案（视图层渲染用，放在这里保证与 LANE_ORDER 同步） */
export const LANE_TITLE: Record<string, string> = {
  namespace: '命名空间',
  networkPolicy: 'NetworkPolicy',
  ingress: 'Ingress',
  service: 'Service',
  workload: '工作负载',
  pod: 'Pod',
  node: '节点',
};
