#!/usr/bin/env node
// 拓扑布局的确定性检查。
//
// 布局是纯函数（src/topology-layout.ts 不 import 任何运行时模块），所以可以直接在
// Node 里 import 进来断言坐标 —— 不用起浏览器、不用起后端。这里只覆盖最容易写错的几件事：
//   1. 分层列顺序（ingress → service → workload → pod → node，namespace/networkPolicy 旁带）
//   2. 输入顺序不影响结果（轮询时后端数组顺序可能变，位置不能跟着抖）
//   3. 单列限高后换子列（不出现一根无限长的面条）
//   4. Pod 泳道里同一工作负载的 Pod 不重叠
//   5. 边只画两端都存在的，且 count 原样带出
//
// 运行：node scripts/check-topology-layout.mjs
import { layoutTopology } from '../src/topology-layout.ts';

let failed = 0;
function ok(cond, what) {
  if (cond) {
    console.log(`  ✓ ${what}`);
  } else {
    console.error(`  ✗ ${what}`);
    failed++;
  }
}

const node = (id, kind, label, category = 'native') => ({
  id, kind, label, sublabel: `${kind} 说明`, group: kind, category, meta: {},
});

function fixture() {
  const nodes = [
    node('ingress:ns/web', 'ingress', 'web.example.test'),
    node('service:ns/web', 'service', 'web'),
    node('ns:ns', 'namespace', 'ns'),
    node('netpol:ns/lock', 'networkPolicy', 'lock'),
    node('node:n1', 'node', 'n1'),
    node('node:n2', 'node', 'n2'),
  ];
  // 20 个工作负载：单列容量 8，必然换子列
  for (let i = 0; i < 20; i++) nodes.push(node(`workload:ns/w${i}`, 'workload', `w${i}`, 'wasm'));
  for (let i = 0; i < 3; i++) nodes.push(node(`pod:ns/web-${i}`, 'pod', `web-${i}`, 'wasm'));
  const edges = [
    { from: 'ingress:ns/web', to: 'service:ns/web', kind: 'routes', count: 1 },
    { from: 'service:ns/web', to: 'workload:ns/w0', kind: 'exposes', count: 1 },
    { from: 'workload:ns/w0', to: 'node:n1', kind: 'runs-on', count: 1 },
    { from: 'workload:ns/w0', to: 'ns:ns', kind: 'in', count: 1 },
    { from: 'netpol:ns/lock', to: 'workload:ns/w1', kind: 'allows', count: 2 },
    { from: 'pod:ns/web-0', to: 'workload:ns/w0', kind: 'belongs-to', count: 1 },
    { from: 'pod:ns/web-1', to: 'workload:ns/w0', kind: 'belongs-to', count: 1 },
    { from: 'pod:ns/web-2', to: 'workload:ns/w0', kind: 'belongs-to', count: 1 },
    // 悬空边：目标不在图里，必须被丢掉
    { from: 'service:ns/web', to: 'workload:ns/missing', kind: 'exposes', count: 1 },
  ];
  return { nodes, edges };
}

const byId = (l) => new Map(l.nodes.map((n) => [n.id, n]));

console.log('拓扑布局检查');

{
  const { nodes, edges } = fixture();
  const layout = layoutTopology(nodes, edges);
  const pos = byId(layout);

  console.log('列顺序与分层：');
  const x = (id) => pos.get(id).x;
  ok(x('ns:ns') < x('ingress:ns/web'), '命名空间旁带在最左');
  ok(x('netpol:ns/lock') < x('ingress:ns/web'), 'NetworkPolicy 在旁带');
  ok(x('ingress:ns/web') < x('service:ns/web'), 'ingress → service');
  ok(x('service:ns/web') < x('workload:ns/w0'), 'service → workload');
  ok(x('workload:ns/w0') < x('pod:ns/web-0'), 'workload → Pod 泳道');
  ok(x('pod:ns/web-0') < x('node:n1'), 'Pod 泳道 → node');
  ok(x('node:n1') === x('node:n2'), '同列节点共享 x');
  ok(pos.get('node:n1').y !== pos.get('node:n2').y, '同列节点纵向排开');

  console.log('边：');
  ok(layout.edges.length === edges.length - 1, `悬空边被丢弃（${layout.edges.length}/${edges.length}）`);
  const double = layout.edges.find((e) => e.kind === 'allows');
  ok(double && double.count === 2, 'count 原样带出（用于线宽/标签）');
  ok(layout.edges.every((e) => e.d.startsWith('M ') && e.d.includes('C ')), '边是三次贝塞尔路径');

  console.log('换子列（限高）：');
  const workX = new Set(
    layout.nodes.filter((n) => n.kind === 'workload').map((n) => n.x),
  );
  ok(workX.size === 3, `20 个工作负载 → 3 个子列（实际 ${workX.size}）`);
  const maxY = Math.max(...layout.nodes.filter((n) => n.kind === 'workload').map((n) => n.y));
  ok(maxY < 640, `单列高度受限（最大 y=${maxY} < 640）`);

  console.log('Pod 堆叠：');
  const pods = layout.nodes.filter((n) => n.kind === 'pod').sort((a, b) => a.y - b.y);
  ok(pods.length === 3, '3 个 Pod 都有坐标');
  ok(new Set(pods.map((p) => p.x)).size === 1, 'Pod 在同一泳道');
  for (let i = 1; i < pods.length; i++) {
    ok(pods[i].y >= pods[i - 1].y + pods[i - 1].h, `Pod ${i} 与上一个不重叠`);
  }
  const parent = pos.get('workload:ns/w0');
  const first = pods[0];
  ok(Math.abs(first.y + first.h / 2 - (parent.y + parent.h / 2)) <= 24, '第一个 Pod 贴近父工作负载');

  console.log('确定性：');
  const shuffled = { nodes: [...nodes].reverse(), edges: [...edges].reverse() };
  const again = layoutTopology(shuffled.nodes, shuffled.edges);
  const same = JSON.stringify(layout) === JSON.stringify(again);
  ok(same, '打乱输入后坐标完全一致');

  console.log('坐标有限且非负：');
  ok(
    layout.nodes.every((n) => Number.isFinite(n.x) && Number.isFinite(n.y) && n.x >= 0 && n.y >= 0),
    '所有坐标都是有限非负数',
  );
  ok(layout.width > 0 && layout.height > 0, `画布尺寸有效（${layout.width}×${layout.height}）`);
}

{
  console.log('空图：');
  const empty = layoutTopology([], []);
  ok(empty.nodes.length === 0 && empty.edges.length === 0, '空输入返回空布局');
}

if (failed > 0) {
  console.error(`\n${failed} 项失败`);
  process.exit(1);
}
console.log('\n✓ 全部通过');
