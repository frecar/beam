import { existsSync, readdirSync, statSync } from 'node:fs';
import { extname, join } from 'node:path';

const assetsDir = new URL('../dist/assets/', import.meta.url).pathname;
const budgets = {
  js: 700 * 1024,
  css: 180 * 1024,
  total: 900 * 1024,
};

function walk(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    return entry.isDirectory() ? walk(path) : [path];
  });
}

if (!existsSync(assetsDir)) {
  console.error('Missing dist/assets. Run npm run build before checking bundle budget.');
  process.exit(1);
}

const totals = { js: 0, css: 0, total: 0 };
for (const file of walk(assetsDir)) {
  const size = statSync(file).size;
  const ext = extname(file);
  if (ext === '.js') totals.js += size;
  if (ext === '.css') totals.css += size;
  if (ext === '.js' || ext === '.css') totals.total += size;
}

const formatKiB = (bytes) => `${(bytes / 1024).toFixed(1)} KiB`;
const failures = Object.entries(budgets).filter(([key, budget]) => totals[key] > budget);

console.log(
  `Bundle budget: js=${formatKiB(totals.js)} / ${formatKiB(budgets.js)}, ` +
    `css=${formatKiB(totals.css)} / ${formatKiB(budgets.css)}, ` +
    `total=${formatKiB(totals.total)} / ${formatKiB(budgets.total)}`
);

if (failures.length > 0) {
  for (const [key, budget] of failures) {
    console.error(
      `${key} bundle budget exceeded: ${formatKiB(totals[key])} > ${formatKiB(budget)}`
    );
  }
  process.exit(1);
}
