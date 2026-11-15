/* CommOS admin console — vanilla JS, no build step. One screen per thing an office manager knows:
   People, Groups, Numbers, Hours, History, System. Talks to the same /v1 API as everything else. */
(function () {
'use strict';

/* ---------- tiny helpers ---------- */
const DEV_TENANT = '01920000-0000-7000-8000-000000000001';
const DAYS = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];
const DAY_NAMES = { mon: 'Monday', tue: 'Tuesday', wed: 'Wednesday', thu: 'Thursday', fri: 'Friday', sat: 'Saturday', sun: 'Sunday' };
let uid = 0;
const nid = () => 'f' + (++uid);

function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const k in (props || {})) {
    const v = props[k];
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'value') el.value = v;
    else if (k === 'checked' || k === 'disabled' || k === 'selected' || k === 'readOnly') el[k] = !!v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  const add = (c) => {
    if (c == null || c === false) return;
    if (Array.isArray(c)) c.forEach(add);
    else el.appendChild(c.nodeType ? c : document.createTextNode(String(c)));
  };
  kids.forEach(add);
  return el;
}
const $ = (s, r) => (r || document).querySelector(s);
const meta = (n) => { const m = document.querySelector('meta[name="' + n + '"]'); return m ? m.content : ''; };
const store = {
  get(k) { try { return sessionStorage.getItem(k); } catch (e) { return null; } },
  set(k, v) { try { if (v == null) sessionStorage.removeItem(k); else sessionStorage.setItem(k, v); } catch (e) { /* storage unavailable */ } },
};

function toast(msg, bad) {
  const t = h('div', { class: 'toast' + (bad ? ' bad' : '') }, msg);
  $('#toasts').appendChild(t);
  setTimeout(() => t.remove(), bad ? 6000 : 3000);
}

class ApiError extends Error {
  constructor(status, msg) { super(msg); this.status = status; }
}

/* ---------- auth + api ---------- */
const S = { token: null, dev: false, hasSchedules: false, didPatch: null, schedule: null };

function bearer() { return S.token ? 'admin:' + S.token : 'tenant:' + DEV_TENANT; }

async function api(method, path, body, o) {
  o = o || {};
  const init = { method, headers: { Authorization: 'Bearer ' + bearer() } };
  if (body !== undefined) {
    if (o.raw) { init.headers['Content-Type'] = o.type; init.body = body; }
    else { init.headers['Content-Type'] = 'application/json'; init.body = JSON.stringify(body); }
  }
  let r;
  try { r = await fetch(path, init); }
  catch (e) { throw new ApiError(0, 'Can’t reach the phone system. Check your connection and try again.'); }
  if (r.status === 401 && !o.quiet401) { showLogin(); throw new ApiError(401, 'Please sign in again.'); }
  if (!r.ok) {
    let d = '';
    try { const j = await r.json(); d = j.detail || j.title || ''; } catch (e) { /* not json */ }
    throw new ApiError(r.status, d || 'Something went wrong (' + r.status + ').');
  }
  if (o.blob) return r.blob();
  if (o.text) return r.text();
  if (r.status === 204) return null;
  const ct = r.headers.get('content-type') || '';
  return ct.includes('json') ? r.json() : null;
}

async function all(path) {
  let out = [], cursor = null;
  for (let i = 0; i < 25; i++) {
    const r = await api('GET', path + (path.includes('?') ? '&' : '?') + 'limit=200' + (cursor ? '&cursor=' + encodeURIComponent(cursor) : ''));
    out = out.concat(r.items || []);
    cursor = r.next_cursor;
    if (!cursor) break;
  }
  return out;
}

/* ---------- formatting ---------- */
const pad = (n) => String(n).padStart(2, '0');
function fmtTime(iso) {
  const d = new Date(iso); if (isNaN(d)) return '';
  const now = new Date(), t = pad(d.getHours()) + ':' + pad(d.getMinutes());
  const day = (x) => x.getFullYear() * 400 + x.getMonth() * 32 + x.getDate();
  if (day(d) === day(now)) return 'Today, ' + t;
  if (day(d) === day(new Date(now - 864e5))) return 'Yesterday, ' + t;
  return d.toLocaleDateString(undefined, { day: 'numeric', month: 'short' }) + ', ' + t;
}
function fmtDur(ms) {
  if (!ms) return '—';
  const s = Math.round(ms / 1000);
  return Math.floor(s / 60) + ':' + pad(s % 60);
}
const userPart = (ref) => {
  let s = String(ref || '').replace(/^(sip|sips|tel|ext|external):/i, '');
  const at = s.indexOf('@'); if (at >= 0) s = s.slice(0, at);
  return s;
};
const isMac = (s) => /^[0-9a-f]{12}$/i.test(String(s || '').replace(/[:.-]/g, ''));
const plural = (n, w) => n + ' ' + w + (n === 1 ? '' : 's');

/* ---------- data model ---------- */
const D = {};

async function loadCore() {
  const [users, exts, routes, regs, fwds, groups, devices] = await Promise.all([
    all('/v1/users'), all('/v1/extensions'), all('/v1/routes'),
    api('GET', '/v1/registrations').then((r) => r.items || []).catch(() => []),
    all('/v1/forwardings'), all('/v1/ring-groups'), all('/v1/devices').catch(() => []),
  ]);
  Object.assign(D, { users, exts, routes, regs, fwds, groups, devices });
  derive();
}

function derive() {
  const routeById = new Map(D.routes.map((r) => [r.id, r]));
  const userById = new Map(D.users.map((u) => [u.id, u]));
  const live = (u) => u && u.state !== 'DEACTIVATED';
  const byExt = new Map(), byMac = new Map(), byName = new Map();
  for (const u of D.users) {
    if (!live(u)) continue;
    byName.set(u.display_name, u);
    for (const c of u.capabilities || []) if (c.startsWith('ext:')) byExt.set(c.slice(4), u);
  }
  for (const d of D.devices) {
    if (d.mac && d.assigned_user_id && live(userById.get(d.assigned_user_id))) byMac.set(String(d.mac).toLowerCase().replace(/[:.-]/g, ''), userById.get(d.assigned_user_id));
  }
  const now = Date.now();
  const online = new Set(D.regs.filter((r) => !r.expires_at || Date.parse(r.expires_at) > now).map((r) => userPart(r.aor)));
  const groupExt = new Map();
  D.people = []; D.groupList = []; D.domain = 'commos.local';
  D.usedNumbers = new Set(D.exts.map((e) => e.number));
  for (const ext of D.exts) {
    const dest = (routeById.get(ext.route_id) || {}).destination_ref || '';
    const dm = dest.match(/^sips?:[^@]+@(.+)$/); if (dm) D.domain = dm[1];
    if (dest.startsWith('ringgroup:')) { groupExt.set(dest.slice(10), ext); continue; }
    if (/^(ivr|queue|voicemail|external)/.test(dest)) continue;
    const mac = String(ext.label || '').toLowerCase().replace(/[:.-]/g, '');
    const user = byExt.get(ext.number) || (isMac(ext.label) && byMac.get(mac)) || byName.get('Extension ' + ext.number) || null;
    const caps = (user && user.capabilities) || [];
    const lbl = ext.label && !isMac(ext.label) ? ext.label : '';
    D.people.push({
      ext, user, number: ext.number, route: routeById.get(ext.route_id),
      name: (user && user.display_name) || lbl || 'Extension ' + ext.number,
      email: (user && user.email) || '',
      // First-class `ext.voicemail` (default on); legacy `voicemail:off` capability still reads as off.
      vmOff: ext.voicemail === false || caps.includes('voicemail:off'),
      dnd: ext.dnd === true,
      fwd: D.fwds.find((f) => f.number === ext.number) || null,
      online: online.has(ext.number),
    });
  }
  D.people.sort((a, b) => a.number.localeCompare(b.number, undefined, { numeric: true }));
  for (const g of D.groups) {
    const ext = groupExt.get(g.id);
    D.groupList.push({ group: g, ext, number: ext ? ext.number : '', name: g.label || (ext && ext.label) || 'Group', route: null });
  }
  D.groupList.sort((a, b) => a.number.localeCompare(b.number, undefined, { numeric: true }));
  D.personByNumber = new Map(D.people.map((p) => [p.number, p]));
}

function nextFree(start, existing) {
  let n = start;
  while (D.usedNumbers.has(String(n))) n++;
  return String(n);
}
function nextPersonNumber() {
  const nums = D.people.map((p) => parseInt(p.number, 10)).filter((n) => n >= 100 && n < 9000);
  return nextFree(nums.length ? Math.max.apply(null, nums) + 1 : 101);
}
function nextGroupNumber() {
  const nums = D.groupList.map((g) => parseInt(g.number, 10)).filter((n) => n >= 100 && n < 9999);
  return nextFree(nums.length ? Math.max.apply(null, nums) + 1 : 600);
}

function whoIs(ref) {
  const n = userPart(ref);
  if (!n) return '';
  const p = D.personByNumber && D.personByNumber.get(n);
  return p ? p.name + ' (' + n + ')' : n;
}

/* ---------- dialogs + forms ---------- */
function modal(title, body, buttons, o) {
  o = o || {};
  const dlg = h('dialog', { 'aria-labelledby': 'dlgT' });
  const close = () => { dlg.close(); dlg.remove(); };
  dlg.addEventListener('cancel', (e) => { e.preventDefault(); close(); });
  const foot = h('div', { class: 'dlg-f' });
  const form = h('form', { method: 'dialog', novalidate: true, onsubmit: (e) => e.preventDefault() },
    h('div', { class: 'dlg-h' }, h('h3', { id: 'dlgT' }, title)),
    h('div', { class: 'dlg-b' }, body), foot);
  (buttons || []).forEach((b) => {
    const btn = h('button', { type: b.primary ? 'submit' : 'button', class: 'btn' + (b.primary ? ' primary' : '') + (b.danger ? ' danger' : '') + (b.solid ? ' solid' : '') + (b.left ? ' left' : '') }, b.label);
    btn.addEventListener('click', async (e) => {
      e.preventDefault();
      if (b.close) return close();
      btn.disabled = true;
      try { const keep = await b.run(close, btn); if (keep === false) btn.disabled = false; }
      catch (err) { btn.disabled = false; toast(err.message || String(err), true); }
    });
    if (b.left) btn.style.marginRight = 'auto';
    foot.appendChild(btn);
  });
  dlg.appendChild(form);
  document.body.appendChild(dlg);
  dlg.showModal();
  const first = o.focus || dlg.querySelector('input:not([readonly]):not([type=checkbox]),select');
  if (first) first.focus();
  return close;
}

function confirmBox(title, text, label, run) {
  modal(title, h('p', null, text), [
    { label: 'Cancel', close: true },
    { label, danger: true, solid: true, run: async (close) => { await run(); close(); } },
  ]);
}

function field(label, input, hint, id) {
  id = id || nid();
  input.id = id;
  const err = h('div', { class: 'err', role: 'alert', hidden: true });
  const f = h('div', { class: 'field' }, h('label', { for: id }, label), input, hint ? h('div', { class: 'hint' }, hint) : null, err);
  f.setError = (msg) => {
    err.textContent = msg || ''; err.hidden = !msg;
    input.classList.toggle('invalid', !!msg);
    input.setAttribute('aria-invalid', msg ? 'true' : 'false');
  };
  input.addEventListener('input', () => f.setError(''));
  return f;
}
function check(label, sub, checked) {
  const input = h('input', { type: 'checkbox', checked });
  return { el: h('label', { class: 'check' }, input, h('span', null, label, sub ? h('small', null, sub) : null)), input };
}
function opt(value, text, selected) { return h('option', { value, selected }, text); }

/* ---------- shell ---------- */
const TABS = [['people', 'People'], ['groups', 'Groups'], ['numbers', 'Numbers'], ['hours', 'Hours'], ['history', 'History'], ['system', 'System']];

function shell(active) {
  const app = $('#app');
  app.textContent = '';
  const tabs = TABS.filter((t) => t[0] !== 'hours' || S.hasSchedules);
  app.appendChild(h('header', { class: 'top' }, h('div', { class: 'top-in' },
    h('div', { class: 'brand' }, h('i'), 'CommOS'),
    h('nav', { class: 'tabs', 'aria-label': 'Sections' }, tabs.map((t) =>
      h('a', { href: '#/' + t[0], 'aria-current': t[0] === active ? 'page' : null }, t[1]))),
    h('button', { class: 'linkbtn', onclick: signOut }, 'Sign out'))));
  const main = h('main', { id: 'main' });
  app.appendChild(main);
  return main;
}

function pageHead(title, sub, actions) {
  return h('div', { class: 'page-head' }, h('div', null, h('h2', null, title), sub ? h('p', null, sub) : null), h('div', { class: 'actions' }, actions));
}
function empty(title, hint, btn) {
  return h('div', { class: 'empty' }, h('b', null, title), hint, btn ? h('div', null, btn) : null);
}
const spin = () => h('div', { class: 'spin' }, 'Loading…');

/* ---------- login / boot ---------- */
function showLogin(msg) {
  S.token = null; store.set('commos.admin', null);
  const app = $('#app'); app.textContent = '';
  const pw = h('input', { type: 'password', autocomplete: 'current-password', required: true });
  const f = field('Admin password', pw);
  const go = h('button', { class: 'btn primary', type: 'submit' }, 'Sign in');
  app.appendChild(h('form', { class: 'card login', onsubmit: async (e) => {
    e.preventDefault(); f.setError(''); go.disabled = true;
    try {
      const r = await fetch('/admin/login', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ password: pw.value }) });
      if (r.ok) { const j = await r.json(); S.token = j.token; store.set('commos.admin', j.token); await boot(); return; }
      const j = await r.json().catch(() => ({}));
      f.setError(r.status === 401 ? 'That password isn’t right.' : (j.detail || 'Could not sign in.'));
    } catch (err) { f.setError('Can’t reach the phone system.'); }
    go.disabled = false;
  } },
  h('h1', null, 'CommOS admin'), h('p', null, msg || 'Sign in to manage your phones.'), f, go));
  pw.focus();
}

async function signOut() {
  try { if (S.token) await api('POST', '/admin/logout', {}, { quiet401: true }); } catch (e) { /* already gone */ }
  showLogin();
}

async function boot() {
  const embedded = meta('commos-session');
  if (!S.token) S.token = embedded || store.get('commos.admin') || null;
  S.dev = meta('commos-dev') === '1';
  try {
    await api('GET', '/admin/whoami', undefined, { quiet401: true });
  } catch (e) {
    if (e.status === 401) return showLogin(S.dev ? 'Admin access is limited to your local network until a password is set.' : null);
    $('#app').textContent = '';
    $('#app').appendChild(h('p', { class: 'boot' }, e.message));
    return;
  }
  if (S.token) store.set('commos.admin', S.token);
  // Optional features: only show what this server can do.
  try { const s = await api('GET', '/v1/schedules', undefined, { quiet401: true }); S.hasSchedules = true; S.schedule = (s.items || [])[0] || null; }
  catch (e) { S.hasSchedules = false; }
  await route();
}

/* ---------- router ---------- */
async function route() {
  const parts = (location.hash.replace(/^#\/?/, '') || 'people').split('/');
  let name = parts[0];
  if (!TABS.some((t) => t[0] === name) || (name === 'hours' && !S.hasSchedules)) name = 'people';
  const main = shell(name);
  main.appendChild(spin());
  const seq = ++route.seq;
  try {
    if (!['system', 'hours'].includes(name)) await loadCore();
    if (seq !== route.seq) return;
    main.textContent = '';
    await VIEWS[name](main, parts[1]);
  } catch (e) {
    if (e.status === 401) return;
    main.textContent = '';
    main.appendChild(h('div', { class: 'card' }, empty('Something went wrong', e.message, h('button', { class: 'btn', onclick: route }, 'Try again'))));
  }
}
route.seq = 0;
window.addEventListener('hashchange', () => { if (S.ready) route(); });
const refresh = () => route();

/* ====================================================================
   PEOPLE
   ==================================================================== */
const FWD_MODES = [['', 'Don’t forward'], ['ALWAYS', 'Always forward'], ['BUSY', 'Forward when busy'], ['NO_ANSWER', 'Forward when no answer']];

function viewPeople(main) {
  main.appendChild(pageHead('People', 'Everyone with a phone number. Click a person to change their details.',
    h('button', { class: 'btn primary', onclick: () => personForm(null) }, '+ Add person')));
  if (!D.people.length) {
    main.appendChild(h('div', { class: 'card' }, empty('No one here yet', 'Add the first person and give them a number.',
      h('button', { class: 'btn primary', onclick: () => personForm(null) }, '+ Add person'))));
    return;
  }
  const list = h('ul', { class: 'rows people' },
    h('li', { class: 'head' }, h('span', null, 'Name'), h('span', null, 'Number'), h('span', { class: 'hide-s' }, 'Email'), h('span', { class: 'hide-s' }, 'Status'), h('span')));
  for (const p of D.people) {
    const badges = h('span', { class: 'chips hide-s' },
      p.dnd ? h('span', { class: 'badge warn' }, 'Do not disturb') : null,
      p.fwd && p.fwd.enabled ? h('span', { class: 'badge info' }, 'Forwarding') : null,
      p.vmOff ? h('span', { class: 'badge' }, 'No voicemail') : null,
      !p.dnd && !(p.fwd && p.fwd.enabled) && !p.vmOff ? h('span', { class: 'sub' }, p.online ? 'Phone online' : 'Phone offline') : null);
    list.appendChild(h('li', { class: 'clickable', onclick: () => personForm(p) },
      h('span', { class: 'cell-main' }, h('span', { class: 'dot' + (p.online ? ' on' : ''), title: p.online ? 'Phone is online' : 'Phone is offline', role: 'img', 'aria-label': p.online ? 'Online' : 'Offline' }), h('span', { class: 't' }, p.name)),
      h('span', { class: 'mono' }, p.number),
      h('span', { class: 'sub hide-s' }, p.email || '—'),
      badges,
      h('span', { class: 'end' }, h('button', { class: 'btn small', onclick: (e) => { e.stopPropagation(); personForm(p); } }, 'Edit'))));
  }
  main.appendChild(h('div', { class: 'card' }, list));
}

function validNumberFor(v) { return /^\+?[0-9*#]{2,20}$/.test(String(v).replace(/[\s\-().]/g, '')); }
const cleanNumber = (v) => String(v).replace(/[\s\-().]/g, '');

function personForm(p) {
  const edit = !!p;
  const name = field('Name', h('input', { type: 'text', value: p ? p.name : '', autocomplete: 'off', placeholder: 'e.g. Sam Rivera' }));
  const num = field('Extension number', h('input', { type: 'text', inputmode: 'numeric', value: p ? p.number : nextPersonNumber(), readOnly: edit }),
    edit ? 'The number can’t be changed. To use a different one, add a new person.' : 'Others dial this to reach them. We picked the next free one.');
  const email = field('Email (optional)', h('input', { type: 'email', value: p ? p.email : '', autocomplete: 'off', placeholder: 'name@example.com' }), 'Used for voicemail messages and notifications.');
  const vm = check('Voicemail', 'Callers can leave a message when it isn’t answered.', p ? !p.vmOff : true);
  const dnd = check('Do not disturb', 'Calls skip this phone.', p ? p.dnd : false);
  const mode = h('select', null, FWD_MODES.map((m) => opt(m[0], m[1], p && p.fwd && p.fwd.enabled && p.fwd.mode === m[0])));
  const modeF = field('Call forwarding', mode);
  const to = field('Forward to', h('input', { type: 'tel', value: p && p.fwd ? (p.fwd.targets || [])[0] || '' : '', placeholder: 'Number, e.g. 07700 900123', autocomplete: 'off' }));
  const syncFwd = () => { to.hidden = !mode.value; };
  mode.addEventListener('change', syncFwd); syncFwd();

  const body = h('div', null, name, num, email, vm.el, h('div', { style: 'height:12px' }), dnd.el, h('div', { style: 'height:14px' }), modeF, to);
  if (edit) {
    body.appendChild(h('div', { class: 'actions', style: 'margin:6px 0 10px' },
      h('button', { type: 'button', class: 'btn small', onclick: () => resetPassword(p) }, 'Reset phone password'),
      h('button', { type: 'button', class: 'btn small', onclick: () => rebootPhone(p) }, 'Restart phone')));
  }

  const validate = () => {
    let ok = true;
    if (!name.querySelector('input').value.trim()) { name.setError('Please enter a name.'); ok = false; }
    const n = num.querySelector('input').value.trim();
    if (!edit) {
      if (!/^[0-9]{2,6}$/.test(n)) { num.setError('Use 2 to 6 digits.'); ok = false; }
      else if (D.usedNumbers.has(n)) { num.setError('That number is already used.'); ok = false; }
    }
    const em = email.querySelector('input').value.trim();
    if (em && !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(em)) { email.setError('That doesn’t look like an email address.'); ok = false; }
    if (mode.value && !validNumberFor(to.querySelector('input').value)) { to.setError('Enter the number to forward to.'); ok = false; }
    return ok;
  };

  const buttons = [{ label: 'Cancel', close: true }];
  if (edit) buttons.unshift({ label: 'Remove', danger: true, left: true, run: (close) => { close(); confirmRemovePerson(p); return false; } });
  buttons.push({ label: edit ? 'Save' : 'Add person', primary: true, run: async (close) => {
    if (!validate()) return false;
    const v = {
      name: name.querySelector('input').value.trim(), number: num.querySelector('input').value.trim(),
      email: email.querySelector('input').value.trim(), voicemail: vm.input.checked, dnd: dnd.input.checked,
      fwdMode: mode.value, fwdTo: cleanNumber(to.querySelector('input').value),
    };
    if (edit) { await savePerson(p, v); close(); toast('Saved'); refresh(); }
    else { const r = await createPerson(v); close(); await refreshQuiet(); showCreated(v, r.password); refresh(); }
  } });
  modal(edit ? p.name : 'Add person', body, buttons);
}

async function refreshQuiet() { try { await loadCore(); } catch (e) { /* the page refresh will report it */ } }

async function createPerson(v) {
  const caps = ['ext:' + v.number];
  const user = await api('POST', '/v1/users', { display_name: v.name, email: v.email || undefined, capabilities: caps });
  let ext;
  try {
    ext = await api('POST', '/v1/extensions', { number: v.number, destination_ref: 'sip:' + v.number + '@' + D.domain, label: v.name });
  } catch (e) {
    try { await api('DELETE', '/v1/users/' + user.id); } catch (_) { /* best effort */ }
    throw e;
  }
  let password = null;
  try { password = (await api('POST', '/v1/extensions/' + ext.id + '/sip-password')).password; } catch (_) { /* optional */ }
  if (v.fwdMode) await api('POST', '/v1/forwardings', { number: v.number, enabled: true, mode: v.fwdMode, targets: [v.fwdTo] });
  if (!v.voicemail) { try { await api('PATCH', '/v1/extensions/' + ext.id, { voicemail: false }); } catch (_) { toast('Turning voicemail off isn’t available on this version yet.', true); } }
  if (v.dnd) { try { await api('PATCH', '/v1/extensions/' + ext.id, { dnd: true }); } catch (_) { toast('Do not disturb isn’t available on this version yet.', true); } }
  return { ext, user, password };
}

async function savePerson(p, v) {
  let caps = ((p.user && p.user.capabilities) || []).filter((c) => c !== 'voicemail:off' && !c.startsWith('ext:'));
  caps.push('ext:' + p.number);
  const userBody = { display_name: v.name, email: v.email, capabilities: caps };
  if (p.user) {
    const same = p.user.display_name === v.name && (p.user.email || '') === v.email && JSON.stringify(p.user.capabilities || []) === JSON.stringify(caps);
    if (!same) await api('PATCH', '/v1/users/' + p.user.id, userBody);
  } else {
    await api('POST', '/v1/users', { display_name: v.name, email: v.email || undefined, capabilities: caps });
  }
  const patch = {};
  if (!p.ext.label || p.ext.label === p.name) { if (p.ext.label !== v.name) patch.label = v.name; }
  if (v.dnd !== p.dnd) patch.dnd = v.dnd;
  // Also migrates a legacy `voicemail:off` capability (dropped from caps above) onto the extension.
  if (v.voicemail !== (p.ext.voicemail !== false)) patch.voicemail = v.voicemail;
  if (Object.keys(patch).length) {
    const r = await api('PATCH', '/v1/extensions/' + p.ext.id, patch);
    if ('dnd' in patch && r && r.dnd !== undefined && r.dnd !== v.dnd) toast('Do not disturb didn’t stick.', true);
    if ('dnd' in patch && r && r.dnd === undefined && v.dnd) toast('Do not disturb isn’t available on this version yet.', true);
  }
  const f = p.fwd;
  if (!v.fwdMode) { if (f) await api('DELETE', '/v1/forwardings/' + f.id); }
  else {
    const body = { number: p.number, enabled: true, mode: v.fwdMode, targets: [v.fwdTo], ring_seconds: f ? f.ring_seconds : undefined };
    if (f) await api('PATCH', '/v1/forwardings/' + f.id, body); else await api('POST', '/v1/forwardings', body);
  }
}

function showCreated(v, password) {
  const body = h('div', null,
    h('p', null, h('b', null, v.name), ' can now be called on ', h('b', null, v.number), '.'));
  const buttons = [{ label: 'Done', primary: true, close: true }];
  if (password) {
    body.appendChild(h('p', { class: 'sub' }, 'To set up a phone by hand, use extension ', h('b', null, v.number), ' with this password. It is shown only once.'));
    body.appendChild(h('div', { class: 'secret', tabindex: 0 }, password));
    buttons.unshift({ label: 'Copy password', run: async (c, btn) => { await copy(password); btn.textContent = 'Copied'; return false; } });
  }
  modal('Added ' + v.name, body, buttons);
}

async function copy(text) {
  try { await navigator.clipboard.writeText(text); toast('Copied'); }
  catch (e) { toast('Select the password and copy it.', true); }
}

function resetPassword(p) {
  confirmBox('Reset phone password?', 'The phone for ' + p.name + ' (' + p.number + ') will stop working until it is given the new password. Phones set up by auto-provisioning pick it up when they restart.', 'Reset password', async () => {
    const r = await api('POST', '/v1/extensions/' + p.ext.id + '/sip-password');
    modal('New password for ' + p.number, h('div', null, h('p', { class: 'sub' }, 'Shown only once. Enter it on the phone, or restart the phone to fetch it automatically.'), h('div', { class: 'secret', tabindex: 0 }, r.password)),
      [{ label: 'Copy password', run: async (c, btn) => { await copy(r.password); btn.textContent = 'Copied'; return false; } }, { label: 'Done', primary: true, close: true }]);
  });
}

async function rebootPhone(p) {
  try {
    await api('POST', '/v1/onboarding/reboot-extension', { number: p.number });
    toast('Restarting ' + p.name + '’s phone…');
  } catch (e) { toast(e.message, true); }
}

function confirmRemovePerson(p) {
  confirmBox('Remove ' + p.name + '?', 'Extension ' + p.number + ' will stop working and be taken out of any groups. Voicemail and call history stay.', 'Remove', async () => {
    for (const g of D.groupList) {
      if ((g.group.members || []).includes(p.number)) await patchGroup(g, { members: g.group.members.filter((m) => m !== p.number) });
    }
    if (p.fwd) await api('DELETE', '/v1/forwardings/' + p.fwd.id);
    await api('DELETE', '/v1/extensions/' + p.ext.id);
    if (p.route) { try { await api('DELETE', '/v1/routes/' + p.route.id); } catch (_) { /* leftover route is harmless */ } }
    if (p.user) { try { await api('DELETE', '/v1/users/' + p.user.id); } catch (_) { /* already inactive */ } }
    toast(p.name + ' removed');
    refresh();
  });
}

/* ====================================================================
   GROUPS
   ==================================================================== */
const STRATS = { RING_ALL: 'Ring everyone at once', SEQUENTIAL: 'One after another, in order', ROUND_ROBIN: 'Take turns', RANDOM: 'Random order' };

function viewGroups(main) {
  main.appendChild(pageHead('Groups', 'A number that rings several people, like Sales or Front desk.',
    h('button', { class: 'btn primary', onclick: () => groupForm(null) }, '+ Add group')));
  if (!D.groupList.length) {
    main.appendChild(h('div', { class: 'card' }, empty('No groups yet', 'Make a group so one number can ring a whole team.',
      h('button', { class: 'btn primary', onclick: () => groupForm(null) }, '+ Add group'))));
    return;
  }
  const list = h('ul', { class: 'rows groups' },
    h('li', { class: 'head' }, h('span', null, 'Name'), h('span', null, 'Number'), h('span', { class: 'hide-s' }, 'Members'), h('span', { class: 'hide-s' }, 'Rings'), h('span')));
  for (const g of D.groupList) {
    const names = (g.group.members || []).map((m) => { const p = D.personByNumber.get(userPart(m)); return p ? p.name : m; });
    list.appendChild(h('li', { class: 'clickable', onclick: () => groupForm(g) },
      h('span', { class: 'cell-main' }, h('span', { class: 't' }, g.name)),
      h('span', { class: 'mono' }, g.number || '—'),
      h('span', { class: 'chips hide-s' }, names.length ? names.map((n) => h('span', { class: 'badge' }, n)) : h('span', { class: 'sub' }, 'No members yet')),
      h('span', { class: 'sub hide-s' }, g.group.strategy === 'RING_ALL' ? 'All at once' : g.group.strategy === 'SEQUENTIAL' ? 'In order' : STRATS[g.group.strategy] || ''),
      h('span', { class: 'end' }, h('button', { class: 'btn small', onclick: (e) => { e.stopPropagation(); groupForm(g); } }, 'Edit'))));
  }
  main.appendChild(h('div', { class: 'card' }, list));
}

function patchGroup(g, changes) {
  const x = g.group;
  return api('PATCH', '/v1/ring-groups/' + x.id, Object.assign({
    strategy: x.strategy, members: x.members || [], ring_seconds: x.ring_seconds, no_answer_ref: x.no_answer_ref, label: x.label,
  }, changes));
}

function groupForm(g) {
  const edit = !!g;
  const name = field('Group name', h('input', { type: 'text', value: g ? g.name : '', placeholder: 'e.g. Sales', autocomplete: 'off' }));
  const num = field('Number', h('input', { type: 'text', inputmode: 'numeric', value: g ? g.number : nextGroupNumber(), readOnly: edit }),
    edit ? 'The number can’t be changed.' : 'Dial this number to ring the group.');
  const strat = h('select', null, ['RING_ALL', 'SEQUENTIAL'].concat(g && !['RING_ALL', 'SEQUENTIAL'].includes(g.group.strategy) ? [g.group.strategy] : []).map((k) => opt(k, STRATS[k], g ? g.group.strategy === k : k === 'RING_ALL')));
  const stratF = field('How it rings', strat);
  const have = new Set(g ? (g.group.members || []).map(userPart) : []);
  const boxes = D.people.map((p) => ({ p, c: check(p.name + ' · ' + p.number, null, have.has(p.number)) }));
  const pick = h('div', { class: 'pick' }, boxes.length ? boxes.map((b) => b.c.el) : h('span', { class: 'sub' }, 'Add some people first.'));
  const membersF = h('div', { class: 'field' }, h('div', { class: 'lbl' }, 'Who is in this group'), pick, h('div', { class: 'err', role: 'alert', hidden: true }));
  const validate = () => {
    let ok = true;
    if (!name.querySelector('input').value.trim()) { name.setError('Please enter a name.'); ok = false; }
    const n = num.querySelector('input').value.trim();
    if (!edit) {
      if (!/^[0-9]{2,6}$/.test(n)) { num.setError('Use 2 to 6 digits.'); ok = false; }
      else if (D.usedNumbers.has(n)) { num.setError('That number is already used.'); ok = false; }
    }
    const err = membersF.querySelector('.err');
    const any = boxes.some((b) => b.c.input.checked);
    err.hidden = any; err.textContent = any ? '' : 'Pick at least one person.';
    return ok && any;
  };
  const buttons = [{ label: 'Cancel', close: true }];
  if (edit) buttons.unshift({ label: 'Delete', danger: true, left: true, run: (close) => { close(); confirmDeleteGroup(g); return false; } });
  buttons.push({ label: edit ? 'Save' : 'Add group', primary: true, run: async (close) => {
    if (!validate()) return false;
    const label = name.querySelector('input').value.trim();
    const members = boxes.filter((b) => b.c.input.checked).map((b) => b.p.number);
    if (edit) {
      await patchGroup(g, { label, members, strategy: strat.value });
      if (g.ext && g.ext.label !== label) await api('PATCH', '/v1/extensions/' + g.ext.id, { label });
    } else {
      const number = num.querySelector('input').value.trim();
      const grp = await api('POST', '/v1/ring-groups', { strategy: strat.value, members, label });
      try { await api('POST', '/v1/extensions', { number, destination_ref: 'ringgroup:' + grp.id, label }); }
      catch (e) { try { await api('DELETE', '/v1/ring-groups/' + grp.id); } catch (_) { /* best effort */ } throw e; }
    }
    close(); toast('Saved'); refresh();
  } });
  modal(edit ? g.name : 'Add group', h('div', null, name, num, membersF, stratF), buttons);
}

function confirmDeleteGroup(g) {
  const n = (D.dids || []).filter((d) => d.destination_ref === 'ringgroup:' + g.group.id).length;
  confirmBox('Delete ' + g.name + '?', 'The number ' + (g.number || '') + ' will stop ringing.' + (n ? ' ' + plural(n, 'outside number') + ' currently ring this group.' : ''), 'Delete', async () => {
    if (g.ext) {
      await api('DELETE', '/v1/extensions/' + g.ext.id);
      try { await api('DELETE', '/v1/routes/' + g.ext.route_id); } catch (_) { /* harmless */ }
    }
    await api('DELETE', '/v1/ring-groups/' + g.group.id);
    toast('Group deleted'); refresh();
  });
}

/* ====================================================================
   NUMBERS
   ==================================================================== */
async function viewNumbers(main) {
  const [dids, ivrs] = await Promise.all([all('/v1/dids'), all('/v1/ivrs').catch(() => [])]);
  D.dids = dids; D.ivrs = ivrs;
  if (S.hasSchedules) { try { S.schedule = ((await api('GET', '/v1/schedules')).items || [])[0] || null; } catch (e) { /* keep */ } }
  main.appendChild(pageHead('Numbers', 'Your outside phone numbers and where each one rings.'));
  if (!dids.length) {
    main.appendChild(h('div', { class: 'card' }, empty('No outside numbers yet', 'Numbers from your phone provider show up here once they are connected.')));
    return;
  }
  const showClosed = S.hasSchedules && S.didPatch !== false;
  const list = h('ul', { class: 'rows numbers' },
    h('li', { class: 'head' }, h('span', null, 'Number'), h('span', null, showClosed ? 'During open hours' : 'Rings'), h('span', { class: 'hide-s' }, showClosed ? 'When closed' : ''), h('span')));
  for (const d of dids) {
    list.appendChild(h('li', { class: 'clickable', onclick: () => numberForm(d) },
      h('span', { class: 'cell-main mono' }, h('span', { class: 't' }, d.e164)),
      h('span', null, destLabel(d.destination_ref)),
      h('span', { class: 'hide-s' }, showClosed ? (d.closed_destination_ref ? destLabel(d.closed_destination_ref) : h('span', { class: 'sub' }, 'Same as open')) : ''),
      h('span', { class: 'end' }, h('button', { class: 'btn small', onclick: (e) => { e.stopPropagation(); numberForm(d); } }, 'Change'))));
  }
  main.appendChild(h('div', { class: 'card' }, list));
}

function destLabel(ref) {
  if (!ref) return '—';
  if (ref === 'voicemail') return 'Voicemail';
  if (ref.startsWith('voicemail:')) return 'Voicemail for ' + whoIs(ref.slice(10));
  if (ref.startsWith('ringgroup:')) { const g = D.groupList.find((x) => x.group.id === ref.slice(10)); return g ? g.name + ' (group)' : 'A deleted group'; }
  if (ref.startsWith('ivr:')) { const i = (D.ivrs || []).findIndex((x) => x.id === ref.slice(4)); return i >= 0 ? 'Phone menu ' + (i + 1) : 'A phone menu'; }
  if (/^sips?:/.test(ref)) return whoIs(ref);
  return ref;
}

function destSelect(current, allowSame) {
  const sel = h('select');
  if (allowSame) sel.appendChild(opt('', 'Same as during open hours', !current));
  const g1 = h('optgroup', { label: 'People' });
  D.people.forEach((p) => g1.appendChild(opt('sip:' + p.number + '@' + D.domain, p.name + ' (' + p.number + ')', current && userPart(current) === p.number && /^sips?:/.test(current))));
  if (g1.children.length) sel.appendChild(g1);
  const g2 = h('optgroup', { label: 'Groups' });
  D.groupList.forEach((g) => g2.appendChild(opt('ringgroup:' + g.group.id, g.name, current === 'ringgroup:' + g.group.id)));
  if (g2.children.length) sel.appendChild(g2);
  if ((D.ivrs || []).length) {
    const g3 = h('optgroup', { label: 'Phone menus' });
    D.ivrs.forEach((i, n) => g3.appendChild(opt('ivr:' + i.id, 'Phone menu ' + (n + 1), current === 'ivr:' + i.id)));
    sel.appendChild(g3);
  }
  sel.appendChild(opt('voicemail', 'Voicemail (take a message)', current === 'voicemail'));
  if (current && ![...sel.options].some((o) => o.value === current)) sel.appendChild(opt(current, destLabel(current), true));
  return sel;
}

function numberForm(d) {
  const open = destSelect(d.destination_ref, false);
  const openF = field(S.hasSchedules && S.didPatch !== false ? 'During open hours, ring' : 'Ring', open);
  const body = h('div', null, h('p', { class: 'sub', style: 'margin-top:0' }, d.e164), openF);
  let closed = null;
  if (S.hasSchedules && S.didPatch !== false) {
    closed = destSelect(d.closed_destination_ref, true);
    const closedF = field('When closed, ring', closed, S.schedule ? 'Uses your office hours (see the Hours tab).' : null);
    if (!S.schedule) { closed.disabled = true; closedF.querySelector('.hint') || closedF.insertBefore(h('div', { class: 'hint' }, 'Set your opening hours first, then you can choose what happens when you’re closed.'), closedF.querySelector('.err')); }
    body.appendChild(closedF);
  }
  modal('Change where ' + d.e164 + ' rings', body, [
    { label: 'Cancel', close: true },
    { label: 'Save', primary: true, run: async (close) => {
      const patch = { destination_ref: open.value };
      const wantClosed = closed && !closed.disabled ? closed.value : '';
      const closedChanged = closed && !closed.disabled && wantClosed !== (d.closed_destination_ref || '');
      if (closed && !closed.disabled) { patch.closed_destination_ref = wantClosed || null; patch.schedule_id = wantClosed && S.schedule ? S.schedule.id : null; }
      try {
        const r = await api('PATCH', '/v1/dids/' + d.id, patch);
        if (S.didPatch == null) S.didPatch = true;
        if (closedChanged && wantClosed && r && !('closed_destination_ref' in r)) { toast('Closed-hours routing isn’t supported by this version yet.', true); S.didPatch = false; }
      } catch (e) {
        if (e.status !== 404 && e.status !== 405) throw e;
        S.didPatch = false;
        if (closedChanged && wantClosed) toast('Closed-hours routing isn’t available on this version.', true);
        if (open.value !== d.destination_ref) {
          // This server can't edit a number in place: replace it with the same number and a new destination.
          await api('DELETE', '/v1/dids/' + d.id);
          await api('POST', '/v1/dids', { e164: d.e164, carrier_id: d.carrier_id, destination_ref: open.value });
        }
      }
      close(); toast('Saved'); refresh();
    } },
  ]);
}

/* ====================================================================
   HOURS
   ==================================================================== */
function toDayModel(sch) {
  const m = {};
  DAYS.forEach((d) => { m[d] = { open: false, start: '09:00', end: '17:00', extra: [] }; });
  const seen = {};
  ((sch && sch.weekly) || []).forEach((w) => (w.days || []).forEach((d) => {
    if (!m[d]) return;
    if (!seen[d]) { seen[d] = true; m[d].open = true; m[d].start = w.start; m[d].end = w.end; }
    else m[d].extra.push({ start: w.start, end: w.end });
  }));
  return m;
}
function fromDayModel(m) {
  const groups = new Map(), extras = [];
  DAYS.forEach((d) => {
    if (!m[d].open) return;
    const k = m[d].start + '|' + m[d].end;
    if (!groups.has(k)) groups.set(k, []);
    groups.get(k).push(d);
    m[d].extra.forEach((x) => extras.push({ days: [d], start: x.start, end: x.end }));
  });
  const out = [];
  groups.forEach((days, k) => { const [start, end] = k.split('|'); out.push({ days, start, end }); });
  return out.concat(extras);
}

async function viewHours(main) {
  const list = (await api('GET', '/v1/schedules')).items || [];
  const sch = list[0] || null;
  S.schedule = sch;
  const model = toDayModel(sch || { weekly: [{ days: ['mon', 'tue', 'wed', 'thu', 'fri'], start: '09:00', end: '17:00' }] });
  let holidays = ((sch && sch.holidays) || []).map((x) => ({ date: x.date, name: x.name || '' }));
  const tzDefault = (sch && sch.timezone) || (() => { try { return Intl.DateTimeFormat().resolvedOptions().timeZone; } catch (e) { return 'UTC'; } })();

  const status = h('span', { class: 'badge' }, '');
  main.appendChild(pageHead('Opening hours', 'When your office is open. Outside these times, calls follow your “when closed” settings on the Numbers tab.', status));
  if (sch) {
    api('GET', '/v1/schedules/' + sch.id + '/status').then((s) => {
      status.textContent = s.open ? 'Open right now' : 'Closed right now';
      status.className = 'badge ' + (s.open ? 'ok' : 'warn');
    }).catch(() => status.remove());
  } else status.remove();

  const weekEl = h('div');
  DAYS.forEach((d) => {
    const m = model[d];
    const cb = h('input', { type: 'checkbox', checked: m.open, id: 'o-' + d });
    const st = h('input', { type: 'time', value: m.start, 'aria-label': DAY_NAMES[d] + ' opens' });
    const en = h('input', { type: 'time', value: m.end, 'aria-label': DAY_NAMES[d] + ' closes' });
    const err = h('span', { class: 'err', hidden: true, role: 'alert' });
    const times = h('div', { class: 'times' }, st, h('span', null, 'to'), en, m.extra.map((x) => h('span', { class: 'sub' }, '+ ' + x.start + '–' + x.end)), err);
    const closedTxt = h('span', { class: 'sub' }, 'Closed');
    const sync = () => { m.open = cb.checked; times.hidden = !m.open; closedTxt.hidden = m.open; };
    cb.addEventListener('change', sync);
    st.addEventListener('input', () => { m.start = st.value; err.hidden = true; st.classList.remove('invalid'); en.classList.remove('invalid'); });
    en.addEventListener('input', () => { m.end = en.value; err.hidden = true; st.classList.remove('invalid'); en.classList.remove('invalid'); });
    m._err = (msg) => { err.textContent = msg; err.hidden = !msg; st.classList.toggle('invalid', !!msg); en.classList.toggle('invalid', !!msg); };
    sync();
    weekEl.appendChild(h('div', { class: 'day' }, h('label', { class: 'check', for: 'o-' + d }, cb, h('span', null, DAY_NAMES[d])), h('div', null, times, closedTxt)));
  });

  const tz = h('input', { type: 'text', value: tzDefault, list: 'tzlist', autocomplete: 'off' });
  const tzList = h('datalist', { id: 'tzlist' });
  try { (Intl.supportedValuesOf ? Intl.supportedValuesOf('timeZone') : []).forEach((z) => tzList.appendChild(h('option', { value: z }))); } catch (e) { /* optional */ }
  const tzF = field('Time zone', tz, 'Hours are read in this time zone.');

  const holEl = h('div');
  const renderHol = () => {
    holEl.textContent = '';
    if (!holidays.length) holEl.appendChild(h('p', { class: 'sub', style: 'margin:0 0 8px' }, 'No days off yet. Add bank holidays or closures and the office counts as closed on those dates.'));
    holidays.forEach((x, i) => {
      const dt = h('input', { type: 'date', value: x.date, 'aria-label': 'Holiday date', oninput: (e) => { x.date = e.target.value; } });
      const nm = h('input', { type: 'text', value: x.name, placeholder: 'Name, e.g. Christmas', 'aria-label': 'Holiday name', oninput: (e) => { x.name = e.target.value; } });
      holEl.appendChild(h('div', { class: 'hol' }, dt, nm, h('button', { class: 'linkbtn danger', type: 'button', onclick: () => { holidays.splice(i, 1); renderHol(); } }, 'Remove')));
    });
    holEl.appendChild(h('button', { class: 'btn small', type: 'button', onclick: () => { holidays.push({ date: '', name: '' }); renderHol(); const ins = holEl.querySelectorAll('input[type=date]'); ins[ins.length - 1].focus(); } }, '+ Add a day off'));
  };
  renderHol();

  const save = h('button', { class: 'btn primary', type: 'button', onclick: async () => {
    let ok = true;
    DAYS.forEach((d) => { if (model[d].open && !(model[d].start && model[d].end && model[d].end > model[d].start)) { model[d]._err('Closing time must be after opening time.'); ok = false; } });
    if (holidays.some((x) => !x.date)) { toast('Pick a date for each day off (or remove it).', true); ok = false; }
    if (!tz.value.trim()) { tzF.setError('Please enter a time zone.'); ok = false; }
    if (!ok) return;
    save.disabled = true;
    const body = { name: (sch && sch.name) || 'Office hours', timezone: tz.value.trim(), weekly: fromDayModel(model), holidays: holidays.filter((x) => x.date).map((x) => (x.name.trim() ? { date: x.date, name: x.name.trim() } : { date: x.date })) };
    try {
      if (sch) await api('PATCH', '/v1/schedules/' + sch.id, body); else await api('POST', '/v1/schedules', body);
      toast('Hours saved'); refresh();
    } catch (e) { toast(e.message, true); save.disabled = false; }
  } }, 'Save hours');

  main.appendChild(h('div', { class: 'stack' },
    h('div', { class: 'card pad' }, h('h3', { style: 'margin-bottom:6px' }, 'Weekly hours'), weekEl),
    h('div', { class: 'card pad' }, tzF, tzList),
    h('div', { class: 'card pad' }, h('h3', { style: 'margin-bottom:10px' }, 'Days off'), holEl),
    h('div', { class: 'actions' }, save)));
}

/* ====================================================================
   HISTORY
   ==================================================================== */
function ulawToWav(buf) {
  const u = new Uint8Array(buf);
  if (u.length > 12 && String.fromCharCode(u[0], u[1], u[2], u[3]) === 'RIFF') return new Blob([buf], { type: 'audio/wav' });
  const n = u.length, out = new ArrayBuffer(44 + n * 2), dv = new DataView(out);
  const w = (o, s) => { for (let i = 0; i < s.length; i++) dv.setUint8(o + i, s.charCodeAt(i)); };
  w(0, 'RIFF'); dv.setUint32(4, 36 + n * 2, true); w(8, 'WAVEfmt '); dv.setUint32(16, 16, true);
  dv.setUint16(20, 1, true); dv.setUint16(22, 1, true); dv.setUint32(24, 8000, true); dv.setUint32(28, 16000, true);
  dv.setUint16(32, 2, true); dv.setUint16(34, 16, true); w(36, 'data'); dv.setUint32(40, n * 2, true);
  for (let i = 0; i < n; i++) {
    const b = ~u[i] & 0xff, exp = (b >> 4) & 7, man = b & 15;
    let s = (((man << 3) + 0x84) << exp) - 0x84;
    if (b & 0x80) s = -s;
    dv.setInt16(44 + i * 2, s, true);
  }
  return new Blob([out], { type: 'audio/wav' });
}

async function playInto(slot, path, onPlayed) {
  slot.textContent = 'Loading…';
  try {
    const blob = await api('GET', path, undefined, { blob: true });
    const wav = ulawToWav(await blob.arrayBuffer());
    const au = h('audio', { controls: true, src: URL.createObjectURL(wav), 'aria-label': 'Audio' });
    slot.textContent = ''; slot.appendChild(au);
    au.play().catch(() => { /* needs a click; controls are visible */ });
    if (onPlayed) onPlayed();
  } catch (e) { slot.textContent = ''; toast(e.message, true); }
}

async function viewHistory(main, sub) {
  sub = ['voicemail', 'recordings'].includes(sub) ? sub : 'calls';
  const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'History type' },
    [['calls', 'Calls'], ['voicemail', 'Voicemail'], ['recordings', 'Recordings']].map((t) =>
      h('button', { type: 'button', 'aria-pressed': t[0] === sub ? 'true' : 'false', onclick: () => { location.hash = '#/history/' + t[0]; } }, t[1])));
  main.appendChild(pageHead('History', 'Recent calls, messages and recordings.', seg));
  const box = h('div'); main.appendChild(box); box.appendChild(spin());
  if (sub === 'calls') await histCalls(box); else if (sub === 'voicemail') await histVoicemail(box); else await histRecordings(box);
}

async function callsIndex() {
  const [calls, cdrs] = await Promise.all([all('/v1/calls').catch(() => []), all('/v1/cdrs').catch(() => [])]);
  const cdrByCall = new Map(cdrs.map((c) => [c.call_id, c]));
  const rows = calls.map((c) => ({ call: c, cdr: cdrByCall.get(c.id), at: c.created_at }));
  const known = new Set(calls.map((c) => c.id));
  cdrs.filter((c) => !known.has(c.call_id)).forEach((c) => rows.push({ call: null, cdr: c, at: c.created_at }));
  rows.sort((a, b) => (a.at < b.at ? 1 : -1));
  return { rows, callById: new Map(calls.map((c) => [c.id, c])) };
}

function callStatus(r) {
  const c = r.call;
  if (!c) return r.cdr && r.cdr.duration_ms > 0 ? 'answered' : 'missed';
  if (c.answered_at || c.state === 'ANSWERED' || c.state === 'HELD') return 'answered';
  if (['INITIATED', 'RINGING'].includes(c.state)) return 'live';
  return 'missed';
}

async function histCalls(box) {
  const { rows } = await callsIndex();
  box.textContent = '';
  if (!rows.length) { box.appendChild(h('div', { class: 'card' }, empty('No calls yet', 'Calls will show up here as soon as someone dials.'))); return; }
  const q = h('input', { type: 'search', placeholder: 'Search a name or number', 'aria-label': 'Search calls' });
  const stat = h('select', { 'aria-label': 'Filter by result' }, opt('', 'All calls'), opt('answered', 'Answered'), opt('missed', 'Missed'));
  const when = h('select', { 'aria-label': 'Filter by date' }, opt('', 'Any time'), opt('today', 'Today'), opt('week', 'Last 7 days'));
  const out = h('div'); let shown = 50;
  const draw = () => {
    const needle = q.value.trim().toLowerCase();
    const now = new Date(), start = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    const f = rows.filter((r) => {
      const s = callStatus(r);
      if (stat.value && s !== stat.value) return false;
      const t = Date.parse(r.at);
      if (when.value === 'today' && t < start) return false;
      if (when.value === 'week' && t < start - 6 * 864e5) return false;
      if (needle) {
        const c = r.call, hay = [c && c.from_ref, c && c.to_ref, c && whoIs(c.from_ref), c && whoIs(c.to_ref), r.cdr && r.cdr.extension, r.cdr && r.cdr.did].join(' ').toLowerCase();
        if (!hay.includes(needle)) return false;
      }
      return true;
    });
    out.textContent = '';
    if (!f.length) { out.appendChild(h('div', { class: 'card' }, empty('No calls match', 'Try a different search or filter.'))); return; }
    const list = h('ul', { class: 'rows calls' }, h('li', { class: 'head' }, h('span', null, 'When'), h('span', null, 'From'), h('span', null, 'To'), h('span', null, 'Length'), h('span', null, 'Result')));
    f.slice(0, shown).forEach((r) => {
      const c = r.call, s = callStatus(r);
      list.appendChild(h('li', null,
        h('span', { class: 'sub' }, fmtTime(r.at)),
        h('span', null, c ? whoIs(c.from_ref) : (r.cdr.extension || '—')),
        h('span', null, c ? whoIs(c.to_ref) : (r.cdr.did || '—')),
        h('span', { class: 'mono' }, fmtDur(r.cdr ? r.cdr.duration_ms : 0)),
        h('span', null, h('span', { class: 'badge ' + (s === 'answered' ? 'ok' : s === 'live' ? 'info' : 'warn') }, s === 'answered' ? 'Answered' : s === 'live' ? 'In progress' : 'Missed'))));
    });
    out.appendChild(h('div', { class: 'card' }, list));
    if (f.length > shown) out.appendChild(h('div', { class: 'actions', style: 'margin-top:12px;justify-content:center' }, h('button', { class: 'btn', onclick: () => { shown += 50; draw(); } }, 'Show more (' + (f.length - shown) + ')')));
  };
  [q, stat, when].forEach((el) => el.addEventListener('input', () => { shown = 50; draw(); }));
  box.appendChild(h('div', { class: 'toolbar' }, q, stat, when));
  box.appendChild(out); draw();
}

async function histVoicemail(box) {
  const vms = (await all('/v1/voicemails')).sort((a, b) => (a.created_at < b.created_at ? 1 : -1));
  box.textContent = '';
  if (!vms.length) { box.appendChild(h('div', { class: 'card' }, empty('No voicemail', 'Messages left for your team will appear here.'))); return; }
  const userName = (id) => { const u = D.users.find((x) => x.id === id); return u ? u.display_name : 'Unassigned mailbox'; };
  const list = h('ul', { class: 'rows vms' }, h('li', { class: 'head' }, h('span', null, 'When'), h('span', null, 'For'), h('span', null, 'Length'), h('span', null, 'Listen')));
  vms.forEach((vm) => {
    const dot = h('span', { class: 'dot' + (vm.read ? '' : ' unread'), title: vm.read ? 'Heard' : 'New', role: 'img', 'aria-label': vm.read ? 'Heard' : 'New' });
    const slot = h('span');
    const play = h('button', { class: 'btn small', onclick: () => { play.hidden = true; playInto(slot, '/v1/voicemails/' + vm.id + '/content', async () => {
      if (!vm.read) { try { await api('POST', '/v1/voicemails/' + vm.id + '/read', {}); vm.read = true; dot.classList.remove('unread'); dot.title = 'Heard'; } catch (e) { /* not critical */ } }
    }); } }, 'Play');
    list.appendChild(h('li', null, h('span', { class: 'cell-main' }, dot, h('span', { class: 't' }, fmtTime(vm.created_at))), h('span', null, userName(vm.user_id)), h('span', { class: 'mono' }, fmtDur(vm.duration_ms)), h('span', null, play, slot)));
  });
  box.appendChild(h('div', { class: 'card' }, list));
}

async function histRecordings(box) {
  const [recs, { callById }] = await Promise.all([all('/v1/recordings'), callsIndex()]);
  recs.sort((a, b) => (a.created_at < b.created_at ? 1 : -1));
  box.textContent = '';
  if (!recs.length) { box.appendChild(h('div', { class: 'card' }, empty('No recordings', 'Call recording is off, or no recorded calls yet.'))); return; }
  const list = h('ul', { class: 'rows vms' }, h('li', { class: 'head' }, h('span', null, 'When'), h('span', null, 'Call'), h('span', null, 'Length'), h('span', null, 'Listen')));
  recs.forEach((r) => {
    const c = callById.get(r.call_id), slot = h('span');
    const play = h('button', { class: 'btn small', onclick: () => { play.hidden = true; playInto(slot, '/v1/recordings/' + r.id + '/content'); } }, 'Play');
    list.appendChild(h('li', null, h('span', { class: 'sub' }, fmtTime(r.created_at)), h('span', null, c ? whoIs(c.from_ref) + ' → ' + whoIs(c.to_ref) : 'Call'), h('span', { class: 'mono' }, fmtDur(r.duration_ms)), h('span', null, play, slot)));
  });
  box.appendChild(h('div', { class: 'card' }, list));
}

/* ====================================================================
   SYSTEM
   ==================================================================== */
function viewSystem(main) {
  main.appendChild(pageHead('System', 'Version, password, and backups.'));
  const ver = meta('commos-version') || 'unknown';
  const stack = h('div', { class: 'stack' });

  stack.appendChild(h('div', { class: 'card pad sys' }, h('h3', { style: 'margin-bottom:10px' }, 'About'),
    h('dl', null, h('dt', null, 'Version'), h('dd', null, ver),
      h('dt', null, 'Live view'), h('dd', null, h('a', { href: '/dashboard' }, 'Open the live dashboard')),
      h('dt', null, 'Phone setup'), h('dd', null, h('a', { href: '/onboarding' }, 'Open the setup wizard')))));

  // password
  const cur = field('Current password', h('input', { type: 'password', autocomplete: 'current-password' }));
  const nw = field('New password', h('input', { type: 'password', autocomplete: 'new-password' }), 'At least 8 characters.');
  const nw2 = field('New password again', h('input', { type: 'password', autocomplete: 'new-password' }));
  const pwBtn = h('button', { class: 'btn primary', type: 'submit' }, 'Change password');
  const pwBox = h('form', { class: 'card pad', onsubmit: async (e) => {
    e.preventDefault();
    const a = cur.querySelector('input'), b = nw.querySelector('input'), c = nw2.querySelector('input');
    let ok = true;
    if (!a.value) { cur.setError('Enter your current password.'); ok = false; }
    if (b.value.length < 8) { nw.setError('Use at least 8 characters.'); ok = false; }
    if (b.value !== c.value) { nw2.setError('The two passwords don’t match.'); ok = false; }
    if (!ok) return;
    pwBtn.disabled = true;
    try {
      await api('POST', '/admin/password', { current: a.value, new: b.value });
      toast('Password changed'); a.value = b.value = c.value = '';
    } catch (err) {
      if (err.status === 401) cur.setError('That isn’t your current password.');
      else toast(err.message, true);
    }
    pwBtn.disabled = false;
  } }, h('h3', { style: 'margin-bottom:10px' }, 'Admin password'), cur, nw, nw2, pwBtn);
  if (S.dev) { pwBox.textContent = ''; pwBox.appendChild(h('h3', { style: 'margin-bottom:6px' }, 'Admin password')); pwBox.appendChild(h('p', { class: 'sub', style: 'margin:0' }, 'No admin password is set. This console is only reachable from your local network. Set admin_password in pbx.yaml (or reinstall with a password) to turn on sign-in.')); }
  stack.appendChild(pwBox);

  // backup / restore
  const file = h('input', { type: 'file', accept: '.yaml,.yml,.json,text/yaml,application/json', hidden: true, id: 'restoreFile' });
  file.addEventListener('change', () => {
    const f = file.files[0]; if (!f) return;
    f.text().then((txt) => {
      confirmBox('Restore from ' + f.name + '?', 'Everything in this file will be added to your phone system. Existing people and groups with the same numbers may end up duplicated. Download a backup first if you’re unsure.', 'Restore', async () => {
        const r = await api('POST', '/v1/config', txt, { raw: true, type: 'text/yaml' });
        const total = r ? Object.values(r).filter((n) => typeof n === 'number').reduce((a, b) => a + b, 0) : 0;
        toast('Restored' + (total ? ' (' + plural(total, 'item') + ' added)' : ''));
      });
    }).catch(() => toast('Could not read that file.', true));
    file.value = '';
  });
  stack.appendChild(h('div', { class: 'card pad' }, h('h3', { style: 'margin-bottom:4px' }, 'Backup and restore'),
    h('p', { class: 'sub', style: 'margin:0 0 12px' }, 'A backup is a single file with your people, groups, numbers and settings. Passwords are never included.'),
    h('div', { class: 'actions' },
      h('button', { class: 'btn', onclick: async (e) => {
        const b = e.currentTarget; b.disabled = true;
        try {
          const txt = await api('GET', '/v1/config', undefined, { text: true });
          const a = h('a', { href: URL.createObjectURL(new Blob([txt], { type: 'text/yaml' })), download: 'commos-backup-' + new Date().toISOString().slice(0, 10) + '.yaml' });
          document.body.appendChild(a); a.click(); a.remove();
          toast('Backup downloaded');
        } catch (err) { toast(err.message, true); }
        b.disabled = false;
      } }, 'Download backup'),
      h('button', { class: 'btn', onclick: () => file.click() }, 'Restore from file…'), file)));
  main.appendChild(stack);
}

const VIEWS = { people: viewPeople, groups: viewGroups, numbers: viewNumbers, hours: viewHours, history: viewHistory, system: viewSystem };

S.ready = false;
boot().then(() => { S.ready = true; });
})();
