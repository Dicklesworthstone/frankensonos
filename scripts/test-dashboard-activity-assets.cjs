'use strict';
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.join(__dirname, '..');
const read = (p) => fs.readFileSync(path.join(root, p), 'utf8');
const asset = (name) => read(`crates/fsonos-api/src/remote/${name}`);
const rust = read('crates/fsonos-api/src/http/remote/dashboard.rs');
const preview = read('scripts/dashboard-preview.py');
const definitions = new Map([...rust.matchAll(/const (\w+): &str = include_str!\("\.\.\/\.\.\/remote\/([^"]+)"\);/g)].map(m => [m[1], m[2]]));
const shipped = (first) => [...rust.match(new RegExp(`"(\\{${first}\\}[^";]+)"`))[1].matchAll(/\{(\w+)\}/g)].map(m => definitions.get(m[1]));
const previews = (name) => [...preview.match(new RegExp(`${name} = \\[([\\s\\S]*?)\\]`))[1].matchAll(/"([^"]+)"/g)].map(m => m[1]);

test('the served and preview scripts share the complete ordered asset list', () => {
  const scripts = shipped('EVENTS'); assert.deepEqual(scripts, previews('SCRIPTS'));
  assert.equal(new Set(scripts).size, scripts.length);
  const order = ['dashboard-core.js','dashboard-activity.js','dashboard-activity-panel.js','dashboard-workspace.js','dashboard.js'];
  assert.deepEqual(scripts.filter(s => order.includes(s)), order);
});
test('both delivery paths include the responsive activity stylesheet without changing browser guards', () => {
  assert.deepEqual(shipped('STYLE'), previews('STYLES'));
  assert.ok(shipped('STYLE').includes('dashboard-activity.css'));
  assert.match(rust, /asset\(content_type, text\)/);
  assert.match(preview, /connect-src 'self'/); assert.match(preview, /The preview cannot control speakers/);
});
test('production activity, panel and workspace coexist in the same browser scope', () => {
  const script=['dashboard-core.js','dashboard-activity.js','dashboard-activity-panel.js','dashboard-workspace.js'].map(asset).join('\n');
  assert.doesNotThrow(()=>new vm.Script(script));
});
test('workspace mounts activity and forwards observation, invalidation and lifecycle events', () => {
  const source=asset('dashboard-workspace.js');
  assert.match(source,/DashboardActivityPanel.create\(\{ call, controller, session \}\)/);
  for(const method of ['observe','invalidate','suspend','resume']) assert.ok(source.includes(`activity?.${method}()`));
  assert.match(asset('dashboard-activity-panel.js'),/document.querySelector\('\.page-footer'\)\?\.before\(root\)/);
});
test('metadata uses text nodes only and activity adds no executable or remote dependencies', () => {
  for(const name of ['dashboard-activity.js','dashboard-activity-panel.js']) {
    assert.doesNotMatch(asset(name),/innerHTML|outerHTML|insertAdjacentHTML|eval\(|new Function|https?:\/\//);
  }
  assert.doesNotMatch(asset('dashboard-activity.css'),/@import|https?:\/\//);
  assert.match(asset('dashboard-activity-panel.js'),/node.textContent = text/);
});
test('Linux CI retains existing gates and executes both new Node suites', () => {
  const workflow=read('.github/workflows/linux-speech.yml');
  for(const name of ['test-dashboard-activity.cjs','test-dashboard-activity-assets.cjs']) assert.ok(workflow.includes(`"scripts/${name}"`));
  assert.match(workflow,/node --test scripts\/test-dashboard-activity.cjs scripts\/test-dashboard-activity-assets.cjs/);
  assert.match(workflow,/cargo test --locked --workspace --no-fail-fast/);
});
