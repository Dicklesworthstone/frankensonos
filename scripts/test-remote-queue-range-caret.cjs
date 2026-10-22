// Run with node --test scripts/test-remote-queue-range-caret.cjs.
// Drives the shipped queue panel over a modeled DOM and proves the caret in the
// First/Last range inputs survives the redraw every keystroke causes: typing
// "2" then "4" must leave "24", not "42". The negative re-runs the same script
// with the caret restoration narrowed to the search input (the pre-9ff8fab
// shape) and must leave "42".
'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const SOURCE = fs.readFileSync(path.join(__dirname, '../crates/fsonos-api/src/remote/queue.js'), 'utf8');

function load(source, document) {
  return vm.runInContext(`${source}\nQueueRemote;`, vm.createContext({ console, document }));
}

function makeDom() {
  const matches = (node, selector) => {
    if (selector.startsWith('[data-focus="')) return node.getAttribute('data-focus') === selector.slice(13, -2);
    if (selector.startsWith('[data-') ) return node.getAttribute(selector.slice(1, -1)) !== null;
    if (selector.startsWith('.')) return String(node.className || '').split(/\s+/).includes(selector.slice(1));
    return false;
  };
  const walk = (node, out) => { for (const child of node.childNodes) { out.push(child); walk(child, out); } return out; };
  function createElement(tag) {
    let attrs = {};
    const node = {
      tagName: String(tag).toUpperCase(), childNodes: [], listeners: {}, value: '',
      disabled: false, open: false, hidden: false, scrollTop: 0, className: '',
      get textContent() { return this._text || ''; },
      set textContent(v) { this._text = String(v); },
      setAttribute(name, value) { attrs[name] = String(value); },
      getAttribute(name) { return Object.prototype.hasOwnProperty.call(attrs, name) ? attrs[name] : null; },
      removeAttribute(name) { delete attrs[name]; },
      append(...kids) {
        for (const raw of kids) {
          const kid = typeof raw === 'object' && raw !== null ? raw : { tagName: '#TEXT', textContent: String(raw), childNodes: [], getAttribute: () => null };
          kid.parentNode = node; node.childNodes.push(kid);
        }
      },
      replaceChildren(...kids) { node.childNodes = []; node.append(...kids); },
      get firstChild() { return node.childNodes[0] || null; },
      appendChild(kid) { node.append(kid); return kid; },
      addEventListener(type, fn) { (node.listeners[type] ||= []).push(fn); },
      removeEventListener(type, fn) { node.listeners[type] = (node.listeners[type] || []).filter(f => f !== fn); },
      dispatch(type, event = {}) { for (const fn of node.listeners[type] || []) fn({ target: node, isComposing: false, preventDefault() {}, ...event }); },
      focus() { document.activeElement = node; if (node.selectionStart === null) node.selectionStart = node.selectionEnd = 0; },
      setSelectionRange(start, end) { node.selectionStart = start; node.selectionEnd = end; },
      contains(other) { return other === node || walk(node, []).includes(other); },
      querySelector(selector) { return walk(node, []).find(child => matches(child, selector)) || null; },
      querySelectorAll(selector) { return walk(node, []).filter(child => matches(child, selector)); },
      closest(selector) {
        for (let at = node; at; at = at.parentNode) if (matches(at, selector)) return at;
        return null;
      },
      scrollIntoView() {},
      click() { node.dispatch('click'); },
    };
    // Inputs start with no selection until one is set, mirroring a real caret.
    if (String(tag).toLowerCase() === 'input') { node.selectionStart = null; node.selectionEnd = null; }
    return node;
  }
  const document = {
    activeElement: null, hidden: false, listeners: {},
    createElement,
    addEventListener(type, fn) { (document.listeners[type] ||= []).push(fn); },
    removeEventListener() {},
  };
  return { document, createElement };
}

async function open(source) {
  const { document, createElement } = makeDom();
  const Queue = load(source, document);
  const room = 'Room@S2';
  const call = async (route, args) => {
    if (route !== 'queue') throw new Error(`unexpected write ${route}`);
    const total = 60, offset = args.query.offset, limit = args.query.limit;
    return { total, revision: 'q:1', playing: 3, items: Array.from({ length: Math.min(limit, total - offset) },
      (_, n) => ({ position: offset + n + 1, title: `Entry ${offset + n + 1}`, uri: `renderer:${offset + n}` })) };
  };
  if (process.env.DUMP) { const c=createElement; globalThis.__n=0; }
  const el = (tag, attrs = {}, ...kids) => {
    const built = createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
      if (value === null || value === undefined || value === false) continue;
      if (key === 'text') built.textContent = value;
      else if (key === 'class') built.className = value;
      else if (key === 'value') built.value = String(value);
      else if (key === 'disabled') built.disabled = Boolean(value);
      else if (key.startsWith('on')) built.addEventListener(key.slice(2), value);
      else built.setAttribute(key, value === true ? '' : String(value));
    }
    built.append(...kids.filter(kid => kid !== null && kid !== undefined));
    return built;
  };
  const host = document.createElement('div');
  const panel = Queue.panel({ zone: room, el, call, current: () => true, played() {}, execute: work => work() });
  if (process.env.DUMP) console.log('kids right after panel:', panel.node.childNodes.length);
  host.append(panel.node);
  // Open the disclosure the way a reader does; the panel loads on toggle.
  panel.node.open = true;
  panel.node.dispatch('toggle');
  for (let n = 0; n < 6; n += 1) await new Promise(resolve => setTimeout(resolve, 0));
  if (process.env.DUMP) {
    const show = (n, depth = 0) => ['  '.repeat(depth) + n.tagName + (n.getAttribute('data-focus') ? `[${n.getAttribute('data-focus')}]` : '') +
      (n.value ? `=${n.value}` : '') + (n.disabled ? ' disabled' : ''), ...n.childNodes.map(c => show(c, depth + 1))].join('\n');
    console.log(show(host));
  }
  return { panel, document, host, restore: () => {} };
}

function type(document, input, text) {
  // Insert at the live caret, exactly like a browser would.
  const at = typeof input.selectionStart === 'number' ? input.selectionStart : input.value.length;
  input.value = input.value.slice(0, at) + text + input.value.slice(at);
  input.selectionStart = input.selectionEnd = at + text.length;
  input.dispatch('input');
}

async function scenario(source, which) {
  const live = await open(source);
  try {
    const first = live.host.querySelector('[data-focus="range-first"]');
    assert.ok(first, 'the First range input rendered');
    first.focus();
    first.setSelectionRange(0, 0);
    type(live.document, first, '2');
    await new Promise(resolve => setTimeout(resolve, 0));
    const again = live.host.querySelector('[data-focus="range-first"]');
    assert.equal(again.value, '2');
    // The redraw must have kept the caret after the digit.
    assert.equal(live.document.activeElement, again, 'focus stays in the First input');
    type(live.document, again, '4');
    await new Promise(resolve => setTimeout(resolve, 0));
    const final = live.host.querySelector('[data-focus="range-first"]');
    assert.equal(final.value, which === 'fixed' ? '24' : '42',
      `${which}: typed digits keep their order`);
    if (which === 'fixed') {
      assert.equal(final.selectionStart, 2, 'the caret stays after the typed digits');
      assert.equal(live.host.querySelector('[data-focus="range-first"]'), final);
    }
    // Caret other than end-of-text survives too: put it at 0 and type.
    const three = which === 'fixed' ? '24' : '42';
    final.focus();
    final.value = three;
    final.setSelectionRange(0, 0);
    final.dispatch('input');
    await new Promise(resolve => setTimeout(resolve, 0));
    const after = live.host.querySelector('[data-focus="range-first"]');
    assert.equal(after.value, three);
    assert.equal(after.selectionStart, 0, 'a caret before the text is preserved');
    type(live.document, after, '9');
    await new Promise(resolve => setTimeout(resolve, 0));
    assert.equal(live.host.querySelector('[data-focus="range-first"]').value, `9${three}`,
      'typing at a restored caret inserts in place');
    // The Last input gets the same contract.
    const last = live.host.querySelector('[data-focus="range-last"]');
    assert.ok(last, 'the Last range input rendered');
    last.focus();
    last.setSelectionRange(0, 0);
    type(live.document, last, '1');
    await new Promise(resolve => setTimeout(resolve, 0));
    type(live.document, live.host.querySelector('[data-focus="range-last"]'), '2');
    await new Promise(resolve => setTimeout(resolve, 0));
    assert.equal(live.host.querySelector('[data-focus="range-last"]').value,
      which === 'fixed' ? '12' : '21', `${which}: the Last input keeps its order`);
  } finally {
    live.panel?.dispose?.();
    live.restore();
  }
}

test('typing into the First/Last range inputs keeps the caret and the digit order', async () => {
  await scenario(SOURCE, 'fixed');
});

test('the pre-9ff8fab search-only caret condition reverses the digits', async () => {
  const narrowed = SOURCE.replace(
    'if (caret && key === focus && typeof target.setSelectionRange === \'function\') {',
    'if (caret && key === \'queue-search\' && typeof target.setSelectionRange === \'function\') {');
  assert.notEqual(narrowed, SOURCE, 'the caret condition was found to narrow');
  await scenario(narrowed, 'old');
});
