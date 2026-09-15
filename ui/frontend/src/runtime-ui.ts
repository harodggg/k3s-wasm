// 运行时类别（wasm / 原生 / GPU）的统一呈现。
//
// 为什么单独一个模块：同一个类别会出现在概览、节点、RuntimeClass、工作负载、
// Pod 列表和网络拓扑里。如果各处自己写徽章文案和颜色，过两周就会出现
// 「WASM」「wasm」「WebAssembly」三种叫法、三种颜色 —— 排查时反而更难对齐。
// 这里集中定义文案、颜色、排序与过滤，所有视图都从这里取。
//
// 另外：后端字段名并不统一（RuntimeClass 上是 `category`，Pod/工作负载/拓扑节点上是
// `runtimeCategory`），而且这个接口是并行开发的 —— 所以取值一律走
// normalizeCategory()，缺字段时退回 isWasm，不让界面出现空白徽章。

import type { RuntimeCategoryCounts } from './api';
import { badge } from './dom';
import type { BadgeKind } from './dom';

export type RuntimeCategory = 'wasm' | 'native' | 'gpu';

/** 类别过滤器的取值：'all' 表示不过滤 */
export type CategoryFilter = 'all' | RuntimeCategory;

export const CATEGORY_LABEL: Record<string, string> = {
  wasm: 'WASM',
  native: '原生',
  gpu: 'GPU',
};

/** 颜色映射只在这里定义一次：wasm=绿（现有 wasm 徽章的颜色）、原生=灰、GPU=蓝。 */
const CATEGORY_KIND: Record<RuntimeCategory, BadgeKind> = {
  wasm: 'ok',
  native: 'muted',
  gpu: 'info',
};

/** 展示与排序顺序：先 wasm，再 GPU，最后原生（原生数量最多，放最后不抢注意力） */
export const CATEGORY_ORDER: RuntimeCategory[] = ['wasm', 'gpu', 'native'];

export const CATEGORY_FILTERS: { value: CategoryFilter; label: string }[] = [
  { value: 'all', label: '全部类别' },
  ...CATEGORY_ORDER.map((c) => ({ value: c as CategoryFilter, label: CATEGORY_LABEL[c]! })),
];

/** 把后端可能缺失/大小写不一致的类别收敛成三个已知值。
 *
 *  fallbackIsWasm 用于过渡期：老后端没有 category 字段时，至少 isWasm 还能给出正确的一半。
 */
export function normalizeCategory(value: unknown, fallbackIsWasm = false): RuntimeCategory {
  if (value === 'wasm' || value === 'native' || value === 'gpu') return value;
  return fallbackIsWasm ? 'wasm' : 'native';
}

export function categoryLabel(value: unknown, fallbackIsWasm = false): string {
  return CATEGORY_LABEL[normalizeCategory(value, fallbackIsWasm)] ?? '未知';
}

/** 全站统一的类别徽章：同样的文案、同样的颜色。 */
export function categoryBadge(value: unknown, fallbackIsWasm = false): HTMLElement {
  const c = normalizeCategory(value, fallbackIsWasm);
  return badge(CATEGORY_LABEL[c]!, CATEGORY_KIND[c]);
}

export function categoryRank(value: unknown, fallbackIsWasm = false): number {
  const i = CATEGORY_ORDER.indexOf(normalizeCategory(value, fallbackIsWasm));
  return i < 0 ? CATEGORY_ORDER.length : i;
}

export function matchCategory(filter: CategoryFilter, value: unknown, fallbackIsWasm = false): boolean {
  return filter === 'all' || normalizeCategory(value, fallbackIsWasm) === filter;
}

/** 下拉式类别过滤器：工作负载页与拓扑页共用，保证三处选项一致。 */
export function categoryFilterSelect(
  current: CategoryFilter,
  onChange: (value: CategoryFilter) => void,
): HTMLSelectElement {
  const select = document.createElement('select');
  select.className = 'input';
  for (const f of CATEGORY_FILTERS) {
    const opt = document.createElement('option');
    opt.text = f.label;
    opt.value = f.value;
    select.append(opt);
  }
  select.value = current;
  select.addEventListener('change', () => onChange(select.value as CategoryFilter));
  return select;
}

/** 概览页用的类别计数摘要：`WASM 4 · 原生 7 · GPU 1`。 */
export function categoryCountsText(c: RuntimeCategoryCounts | undefined | null): string {
  if (!c) return '类别统计不可用';
  return CATEGORY_ORDER.map((k) => `${CATEGORY_LABEL[k]!} ${c[k] ?? 0}`).join(' · ');
}
