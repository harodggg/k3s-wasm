#!/usr/bin/env node
// 守卫：禁止在视图里对「来自后端的 map 字段」直接 Object.entries。
//
// 事故背景：views-cluster.ts 里写过 Object.entries(r.nodeSelector)，而线上 k3s 内置的
// 那些 RuntimeClass 该字段是 null，于是自动刷新每 5 秒抛一次
// "Cannot convert undefined or null to object"，页面顶部一直挂着错误横幅 ——
// 看着像后端挂了，其实只是一个空 map。正确做法是用 dom.ts 的 pairsText()。
//
// 这个检查很朴素（grep 级别），但它把「一次性修复」变成了「不会复发」。
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';

const dir = fileURLToPath(new URL('../src/', import.meta.url));
const allow = new Set(['dom.ts', 'api.ts']); // 这两个文件里操作的是本地构造的对象
let bad = 0;

for (const f of readdirSync(dir).filter((n) => n.endsWith('.ts'))) {
  if (allow.has(f)) continue;
  readFileSync(join(dir, f), 'utf8')
    .split('\n')
    .forEach((line, i) => {
      if (/Object\.(entries|keys|values)\s*\(/.test(line)) {
        console.error(`${f}:${i + 1}: 不要直接对后端数据用 Object.entries —— 请用 dom.ts 的 pairsText()`);
        console.error(`    ${line.trim()}`);
        bad++;
      }
    });
}

if (bad > 0) {
  console.error(`\n发现 ${bad} 处。原因见本脚本头部注释（曾经导致自动刷新每 5 秒报错）。`);
  process.exit(1);
}
console.log('✓ 未发现对后端数据直接使用 Object.entries 的写法');
