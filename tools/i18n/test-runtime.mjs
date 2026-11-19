// Quick test of static/js/i18n.js outside a browser.
globalThis.localStorage = { getItem: () => process.env.LANG_PICK || null, setItem() {}, removeItem() {} };
Object.defineProperty(globalThis, 'navigator', { value: { languages: ['en-US'] }, configurable: true });
globalThis.document = { documentElement: {} };
globalThis.location = { reload() {} };
globalThis.fetch = async (u) => ({ ok: true, json: async () => globalThis.__pack });
globalThis.__pack = null;
const m = await import('../../static/js/i18n.js');
const ok = (a, b) => { if (a !== b) { console.error('FAIL', JSON.stringify(a), '!==', JSON.stringify(b)); process.exitCode = 1; } };
ok(m.t('a', 'Hello {who}', { who: 'Ann' }), 'Hello Ann');
const P1 = '{n, plural, =0 {No events} one {# event} other {# events}}';
ok(m.t('b', P1, { n: 0 }), 'No events'); ok(m.t('b', P1, { n: 1 }), '1 event'); ok(m.t('b', P1, { n: 1234 }), '1,234 events');
ok(m.t('c', '{k, select, car {A car} person {A person} other {Something}} at {cam}', { k: 'car', cam: 'Gate' }), 'A car at Gate');
ok(m.t('d', "It''s {n, plural, one {# day} other {# days}} left", { n: 2 }), "It's 2 days left");
ok(m.t('e', "Use '{'braces'}'"), 'Use {braces}');
ok(m.safeAgainst('<b>Hallo</b> Welt', '<b>Hello</b> world'), true);
ok(m.safeAgainst('<b>Hallo</b><img src=x onerror=1>', '<b>Hello</b>'), false);
ok(m.safeAgainst('Hallo & Welt > Mehr', 'Hello world'), true);   // bare & and > are text
ok(m.safeAgainst('a" onmouseover="x', 'a'), false);              // no straight quote the English lacks
ok(m.safeAgainst('a<script', 'a'), false);
// a translation shows only the values its English shows (plural branch words are not arguments)
ok(m.safeAgainst('Hallo {secret}', 'Hello {name}'), false);
ok(m.safeAgainst('Hallo {name}', 'Hello {name}'), true);
ok(m.safeAgainst('{n, plural, one {Wiederherstellungscode} other {# Codes}}', '{n, plural, one {recovery code} other {# codes}}'), true);
ok(m.safeAgainst('{n, plural, one {x} other {{token}}}', '{n, plural, one {x} other {y}}'), false);
ok(m.safeAgainst("Befehl '{image}'", "Command '{image}'"), true);
// stray apostrophes cannot hide an argument from the check (the formatter's own parse is used)
ok(m.safeAgainst("Hallo '{secret}", 'Hello'), false);
ok(m.safeAgainst("Hallo ''{secret}", 'Hello'), false);
ok(m.safeAgainst("{n, plural, a'{ x} other {{secret}}} '", '{n, plural, other {# items}}'), false);
// a value the English only uses to pick a branch is never shown raw
ok(m.safeAgainst('Das ist {kind}', '{kind, select, car {A car} other {Something}}'), false);
ok(m.safeAgainst('{n} Ereignisse', '{n, plural, one {# event} other {# events}}'), false);
ok(m.safeAgainst('{n, plural, other {# Ereignisse}}', '{n, plural, one {# event} other {# events}}'), true);
ok(m.safeAgainst('Hallo&nbsp;Welt', 'Hello&nbsp;world'), true);
console.log(process.exitCode ? 'runtime tests FAILED' : 'runtime tests ok');
