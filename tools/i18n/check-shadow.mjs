// check-shadow.mjs - finds the real shadowing bug in static/js/*.js: a CALL t(...) whose `t`
// resolves (by scope) to a local binding instead of the module-level import of t().
// A local named `t` that is never called as t(...) inside its scope is fine and is not listed.
// Read-only. Exit 1 if any such call is found.
//
// Usage: node tools/i18n/check-shadow.mjs [--all] [dir-or-files...]   (default: static/js)
//   --all   list every local binding named `t`, called or not (the full list)
//
// Each hit prints file:lines of the calls, the binding, the first line of its scope, and a
// suggested rename for the local.

import { parse } from './vendor/acorn.mjs';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const argv = process.argv.slice(2);
const ALL = argv.includes('--all');
const targets = argv.filter((a) => a !== '--all');
if (!targets.length) targets.push(join(root, 'static/js'));
const files = [];
for (const a of targets) {
  if (statSync(a).isDirectory()) {
    for (const f of readdirSync(a).sort()) if (f.endsWith('.js')) files.push(join(a, f));
  } else files.push(a);
}

const NAME = 't';
const isFn = (n) => n.type === 'FunctionDeclaration' || n.type === 'FunctionExpression' || n.type === 'ArrowFunctionExpression';
const kids = (n, f) => {
  for (const k of Object.keys(n)) {
    if (k === 'loc') continue;
    const v = n[k];
    if (Array.isArray(v)) for (const c of v) f(c);
    else if (v && typeof v.type === 'string') f(v);
  }
};

// Identifier nodes bound by a pattern.
function names(p, out) {
  if (!p) return out;
  switch (p.type) {
    case 'Identifier': out.push(p); break;
    case 'ObjectPattern': for (const x of p.properties) names(x.type === 'RestElement' ? x.argument : x.value, out); break;
    case 'ArrayPattern': for (const x of p.elements) names(x, out); break;
    case 'RestElement': names(p.argument, out); break;
    case 'AssignmentPattern': names(p.left, out); break;
  }
  return out;
}

// var and function declarations hoisted to the enclosing function (not from nested functions).
function hoisted(node, out) {
  const visit = (n, top) => {
    if (!n || typeof n.type !== 'string') return;
    if (isFn(n) && !top) return;
    if (n.type === 'VariableDeclaration' && n.kind === 'var') for (const d of n.declarations) for (const id of names(d.id, [])) out.push({ id, kind: 'var' });
    kids(n, (c) => visit(c, false));
  };
  visit(node, true);
}

// let/const/class/function declared directly in a statement list.
function lexical(list, out) {
  for (const s of list) {
    const d = s.type === 'ExportNamedDeclaration' && s.declaration ? s.declaration : s;
    if (d.type === 'VariableDeclaration' && d.kind !== 'var') for (const x of d.declarations) for (const id of names(x.id, [])) out.push({ id, kind: d.kind });
    else if (d.type === 'ClassDeclaration' && d.id) out.push({ id: d.id, kind: 'class' });
    else if (d.type === 'FunctionDeclaration' && d.id) out.push({ id: d.id, kind: 'function' });
  }
}

// Rename hints from how the local is used.
const SUGG = [
  [/^(dataset|classList|closest|querySelector|querySelectorAll|style|onclick|remove|focus|contains|parentElement|offsetParent|disabled|hidden|setAttribute|getAttribute|el)$/, 'el'],
  [/^(trim|toLowerCase|toUpperCase|split|charAt|slice|replace|startsWith|endsWith|includes|indexOf|length|padStart|match)$/, 'text'],
];

for (const file of files) {
  const src = readFileSync(file, 'utf8');
  const lines = src.split('\n');
  const rel = file.startsWith(root + '/') ? file.slice(root.length + 1) : file;
  let ast;
  try {
    ast = parse(src, { ecmaVersion: 'latest', sourceType: 'module', locations: true, allowHashBang: true });
  } catch (e) {
    console.error(`${rel}: parse error: ${e.message}`);
    global.__bad = true;
    continue;
  }

  const mk = (node, parent) => ({ vars: new Map(), node, parent });
  const declare = (sc, id, kind) => { if (!sc.vars.has(id.name)) sc.vars.set(id.name, { id, kind, calls: [], scope: sc }); };
  const lookup = (sc, name) => { for (let s = sc; s; s = s.parent) if (s.vars.has(name)) return s.vars.get(name); return null; };
  const bindings = [];
  const note = (sc) => { const b = sc.vars.get(NAME); if (b && b.kind !== 'import') bindings.push(b); };

  const mod = mk(ast, null);
  for (const s of ast.body) if (s.type === 'ImportDeclaration') for (const sp of s.specifiers) declare(mod, sp.local, 'import');
  { const l = []; lexical(ast.body, l); for (const x of l) declare(mod, x.id, x.kind); }
  { const h = []; hoisted(ast, h); for (const x of h) declare(mod, x.id, x.kind); }
  note(mod);

  const walk = (n, sc) => {
    if (!n || typeof n.type !== 'string') return;
    let inner = sc;
    if (isFn(n)) {
      inner = mk(n, sc);
      if (n.type === 'FunctionExpression' && n.id) declare(inner, n.id, 'function name');
      for (const p of n.params) for (const id of names(p, [])) declare(inner, id, 'parameter');
      if (n.body.type === 'BlockStatement') {
        const h = []; hoisted(n.body, h); for (const x of h) declare(inner, x.id, x.kind);
        const l = []; lexical(n.body.body, l); for (const x of l) declare(inner, x.id, x.kind);
      }
      note(inner);
      for (const p of n.params) walk(p, inner);
      if (n.body.type === 'BlockStatement') for (const s of n.body.body) walk(s, inner);
      else walk(n.body, inner);
      return;
    }
    if (n.type === 'BlockStatement' || n.type === 'StaticBlock') {
      inner = mk(n, sc); const l = []; lexical(n.body, l); for (const x of l) declare(inner, x.id, x.kind); note(inner);
    } else if (n.type === 'ForStatement' || n.type === 'ForInStatement' || n.type === 'ForOfStatement') {
      inner = mk(n, sc);
      const d = n.type === 'ForStatement' ? n.init : n.left;
      if (d && d.type === 'VariableDeclaration' && d.kind !== 'var') for (const x of d.declarations) for (const id of names(x.id, [])) declare(inner, id, d.kind);
      note(inner);
    } else if (n.type === 'SwitchStatement') {
      inner = mk(n, sc); const l = []; for (const c of n.cases) lexical(c.consequent, l); for (const x of l) declare(inner, x.id, x.kind); note(inner);
    } else if (n.type === 'CatchClause') {
      inner = mk(n, sc); for (const id of names(n.param, [])) declare(inner, id, 'catch parameter'); note(inner);
    } else if ((n.type === 'ClassExpression' || n.type === 'ClassDeclaration') && n.id && n.type === 'ClassExpression') {
      inner = mk(n, sc); declare(inner, n.id, 'class'); note(inner);
    }
    if (n.type === 'CallExpression' && n.callee.type === 'Identifier' && n.callee.name === NAME) {
      const b = lookup(inner, NAME);
      if (b && b.kind !== 'import') b.calls.push(n);
    }
    kids(n, (c) => walk(c, inner));
  };
  walk(ast, mod);

  const suggest = (b) => {
    const node = b.scope.node;
    const props = new Set();
    let arith = false;
    const seen = (n) => {
      if (!n || typeof n.type !== 'string') return;
      if (n.type === 'MemberExpression' && n.object.type === 'Identifier' && n.object.name === NAME && !n.computed && n.property.type === 'Identifier') props.add(n.property.name);
      if (n.type === 'BinaryExpression' && ['*', '/', '-', '<', '>', '<=', '>='].includes(n.operator) && [n.left, n.right].some((x) => x.type === 'Identifier' && x.name === NAME)) arith = true;
      kids(n, seen);
    };
    seen(node);
    let pick = null;
    for (const [re, name] of SUGG) if ([...props].some((p) => re.test(p))) { pick = name; break; }
    if (!pick && arith) pick = 'ts';
    if (!pick) pick = props.size ? 'item' : 'tx';
    const text = src.slice(node.start, node.end);
    let cand = pick;
    for (let i = 2; new RegExp(`\\b${cand}\\b`).test(text); i++) cand = pick + i;
    return cand;
  };
  const ctx = (sc) => (sc.node === ast ? '(module level)' : lines[sc.node.loc.start.line - 1].trim());

  const out = [];
  for (const b of bindings) {
    if (ALL) out.push({ line: b.id.loc.start.line, text: `${rel}:${b.id.loc.start.line}: ${b.kind} \`t\`, ${b.calls.length} call(s) | ${ctx(b.scope)}` });
    else if (b.calls.length) {
      const lns = [...new Set(b.calls.map((c) => c.loc.start.line))];
      out.push({ line: lns[0], text: `${rel}:${lns.join(',')}: t(...) calls the local ${b.kind} \`t\` declared on line ${b.id.loc.start.line} | ${ctx(b.scope)}\n    -> rename the local to \`${suggest(b)}\`` });
    }
  }
  out.sort((a, b) => a.line - b.line);
  for (const o of out) console.log(o.text);
  global.__found = (global.__found || 0) + out.length;
}
const found = global.__found || 0;
console.log(found
  ? `\n${found} ${ALL ? 'local binding(s) named `t`' : 'call(s) of t() that resolve to a local `t`'} found.`
  : ALL ? `No local binding named \`t\` in ${files.length} files.` : `No t() call resolves to a local \`t\` in ${files.length} files.`);
process.exit(found || global.__bad ? 1 : 0);
