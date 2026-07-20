//! `/dashboard` — a live operations dashboard for the single binary (non-normative).
//!
//! This is the first taste of CommOS's UX: a self-contained HTML page (no external CDNs,
//! fonts, or images — everything inline so it works offline on a Raspberry Pi) that shows
//! the platform running. It polls the unauthenticated operational signals (`/info`,
//! `/_introspect/events`) and, with an operator-supplied bearer token, the versioned
//! `/v1/*` workload resources (calls, video rooms, presence, registrations, messaging).
//!
//! It is mounted unauthenticated at `GET /dashboard`; the token the operator pastes is
//! used only client-side as the `Authorization` header on `/v1` fetches. Nothing here is
//! part of the frozen contract.

use axum::response::Html;

/// `GET /dashboard` — the self-contained operations dashboard page.
pub async fn dashboard() -> Html<String> {
    Html(PAGE.to_string())
}

/// The complete page. Inline `<style>` + `<script>`, zero external requests.
const PAGE: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>CommOS — Operations</title>
<style>
  :root {
    --accent: #4f46e5;
    --accent-weak: #6366f1;
    --bg: #f6f7fb;
    --panel: #ffffff;
    --panel-2: #fbfcfe;
    --border: #e6e8ef;
    --text: #1a1d29;
    --muted: #6b7280;
    --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
    --shadow: 0 1px 2px rgba(16,24,40,.06), 0 1px 3px rgba(16,24,40,.10);
    --radius: 14px;
    --ok: #16a34a;
    --warn: #d97706;
    --grey: #64748b;
    --err: #dc2626;
    --info: #2563eb;
    --purple: #7c3aed;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --accent: #818cf8;
      --accent-weak: #a5b4fc;
      --bg: #0b0d14;
      --panel: #151824;
      --panel-2: #1a1e2d;
      --border: #262b3d;
      --text: #e7e9f0;
      --muted: #9aa2b4;
      --shadow: 0 1px 2px rgba(0,0,0,.4), 0 2px 8px rgba(0,0,0,.35);
      --ok: #4ade80;
      --warn: #fbbf24;
      --grey: #94a3b8;
      --err: #f87171;
      --info: #60a5fa;
      --purple: #c4b5fd;
    }
  }
  * { box-sizing: border-box; }
  html, body { margin: 0; padding: 0; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    background: var(--bg);
    color: var(--text);
    line-height: 1.45;
    -webkit-font-smoothing: antialiased;
    overflow-x: hidden;
  }
  a { color: var(--accent); }
  .wrap { max-width: 1200px; margin: 0 auto; padding: 20px 16px 64px; }

  header.top {
    display: flex; flex-wrap: wrap; align-items: center; gap: 14px;
    padding: 4px 2px 18px;
  }
  .logo {
    display: flex; align-items: center; gap: 12px; min-width: 0;
  }
  .mark {
    width: 40px; height: 40px; border-radius: 11px; flex: none;
    background: linear-gradient(135deg, var(--accent), var(--accent-weak));
    display: grid; place-items: center; color: #fff; font-weight: 800; font-size: 20px;
    box-shadow: var(--shadow);
  }
  .brand h1 { margin: 0; font-size: 20px; letter-spacing: -.01em; }
  .brand p { margin: 1px 0 0; color: var(--muted); font-size: 12.5px; }
  .spacer { flex: 1 1 auto; }
  .status {
    display: inline-flex; align-items: center; gap: 7px;
    font-size: 12.5px; color: var(--muted);
    background: var(--panel); border: 1px solid var(--border);
    padding: 7px 11px; border-radius: 999px; box-shadow: var(--shadow);
  }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--grey); flex: none; }
  .dot.live { background: var(--ok); box-shadow: 0 0 0 3px color-mix(in srgb, var(--ok) 22%, transparent); }
  .dot.down { background: var(--err); box-shadow: 0 0 0 3px color-mix(in srgb, var(--err) 22%, transparent); }

  /* info strip */
  .infobar {
    display: grid; grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 1px; background: var(--border); border: 1px solid var(--border);
    border-radius: var(--radius); overflow: hidden; box-shadow: var(--shadow);
    margin-bottom: 20px;
  }
  .infocell { background: var(--panel); padding: 12px 14px; }
  .infocell .k { font-size: 10.5px; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); }
  .infocell .v { margin-top: 3px; font-size: 14px; font-weight: 600; }
  .infocell .v.mono { font-family: var(--mono); font-weight: 500; font-size: 12.5px; }

  /* controls */
  .controls {
    display: flex; flex-wrap: wrap; align-items: flex-end; gap: 12px;
    background: var(--panel); border: 1px solid var(--border);
    border-radius: var(--radius); padding: 14px 16px; margin-bottom: 20px; box-shadow: var(--shadow);
  }
  .field { display: flex; flex-direction: column; gap: 5px; flex: 1 1 340px; min-width: 0; }
  .field label { font-size: 11px; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); }
  .field input {
    font-family: var(--mono); font-size: 13px; color: var(--text);
    background: var(--panel-2); border: 1px solid var(--border); border-radius: 9px;
    padding: 9px 11px; width: 100%; outline: none;
  }
  .field input:focus { border-color: var(--accent); box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 20%, transparent); }
  .btns { display: flex; flex-wrap: wrap; gap: 9px; }
  button {
    font: inherit; font-size: 13px; font-weight: 600; cursor: pointer;
    border: 1px solid var(--border); background: var(--panel-2); color: var(--text);
    padding: 9px 14px; border-radius: 9px; transition: transform .04s ease, background .15s ease;
    white-space: nowrap;
  }
  button:hover { background: color-mix(in srgb, var(--accent) 8%, var(--panel-2)); }
  button:active { transform: translateY(1px); }
  button.primary { background: var(--accent); border-color: transparent; color: #fff; }
  button.primary:hover { background: var(--accent-weak); }
  button:disabled { opacity: .5; cursor: not-allowed; }

  /* grid of cards */
  .grid {
    display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
    gap: 16px; align-items: start;
  }
  .card {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: var(--radius); box-shadow: var(--shadow); overflow: hidden;
    display: flex; flex-direction: column;
  }
  .card.wide { grid-column: 1 / -1; }
  .card h2 {
    margin: 0; padding: 13px 16px; font-size: 13.5px; letter-spacing: -.01em;
    display: flex; align-items: center; gap: 9px; border-bottom: 1px solid var(--border);
    background: var(--panel-2);
  }
  .card h2 .count {
    margin-left: auto; font-family: var(--mono); font-size: 12px; font-weight: 700;
    color: var(--accent); background: color-mix(in srgb, var(--accent) 12%, transparent);
    padding: 2px 9px; border-radius: 999px;
  }
  .card h2 .glyph { font-size: 15px; }
  .list { list-style: none; margin: 0; padding: 6px; display: flex; flex-direction: column; gap: 2px; max-height: 340px; overflow-y: auto; }
  .row {
    display: flex; align-items: center; gap: 9px; padding: 8px 10px; border-radius: 9px;
    font-size: 13px;
  }
  .row:hover { background: var(--panel-2); }
  .row .id { font-family: var(--mono); font-size: 11.5px; color: var(--muted); flex: none; }
  .row .main { min-width: 0; flex: 1 1 auto; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .row .main .arrow { color: var(--muted); padding: 0 4px; }
  .row time { font-family: var(--mono); font-size: 11px; color: var(--muted); flex: none; }
  .badge {
    font-size: 10.5px; font-weight: 700; letter-spacing: .03em; text-transform: uppercase;
    padding: 2px 8px; border-radius: 999px; flex: none; white-space: nowrap;
    border: 1px solid transparent;
  }
  .empty { padding: 22px 16px; text-align: center; color: var(--muted); font-size: 12.5px; }
  .empty.err { color: var(--err); }

  /* event feed */
  .feed { max-height: 460px; }
  .evt { display: flex; align-items: center; gap: 10px; padding: 8px 10px; border-radius: 9px; font-size: 13px; }
  .evt:hover { background: var(--panel-2); }
  .evt .etype { font-weight: 700; font-size: 11px; letter-spacing: .02em; }
  .evt .subj { font-family: var(--mono); font-size: 11.5px; color: var(--muted); flex: 1 1 auto; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .evt time { font-family: var(--mono); font-size: 11px; color: var(--muted); flex: none; }

  .toast {
    position: fixed; left: 50%; bottom: 22px; transform: translateX(-50%) translateY(20px);
    background: var(--text); color: var(--bg); font-size: 13px; font-weight: 600;
    padding: 10px 16px; border-radius: 10px; box-shadow: var(--shadow);
    opacity: 0; pointer-events: none; transition: opacity .2s ease, transform .2s ease; z-index: 50;
    max-width: 90vw;
  }
  .toast.show { opacity: 1; transform: translateX(-50%) translateY(0); }
  .toast.bad { background: var(--err); color: #fff; }
  .foot { margin-top: 26px; text-align: center; color: var(--muted); font-size: 11.5px; }

  @media (max-width: 560px) {
    .field { flex-basis: 100%; }
    .row .id { display: none; }
  }
</style>
</head>
<body>
<div class="wrap">
  <header class="top">
    <div class="logo">
      <div class="mark">C</div>
      <div class="brand">
        <h1>CommOS</h1>
        <p>A communications OS — voice is one workload.</p>
      </div>
    </div>
    <div class="spacer"></div>
    <div class="status" id="status" title="server connectivity">
      <span class="dot" id="statusDot"></span>
      <span id="statusText">connecting…</span>
    </div>
  </header>

  <div class="infobar" id="infobar">
    <div class="infocell"><div class="k">Version</div><div class="v mono" id="i_version">—</div></div>
    <div class="infocell"><div class="k">Spec</div><div class="v mono" id="i_spec">—</div></div>
    <div class="infocell"><div class="k">Topology</div><div class="v" id="i_topo">—</div></div>
    <div class="infocell"><div class="k">Arch / OS</div><div class="v mono" id="i_arch">—</div></div>
    <div class="infocell"><div class="k">Uptime</div><div class="v mono" id="i_uptime">—</div></div>
  </div>

  <div class="controls">
    <div class="field">
      <label for="token">Bearer token (tenant:&lt;uuidv7&gt;)</label>
      <input id="token" spellcheck="false" autocomplete="off"
             value="tenant:01920000-0000-7000-8000-000000000001">
    </div>
    <div class="btns">
      <button class="primary" id="btnCall">Originate test call</button>
      <button id="btnMsg">Send test message</button>
    </div>
  </div>

  <div class="grid">
    <section class="card">
      <h2><span class="glyph">📞</span> Calls <span class="count" id="c_calls">0</span></h2>
      <ul class="list" id="l_calls"></ul>
    </section>
    <section class="card">
      <h2><span class="glyph">🎥</span> Video rooms <span class="count" id="c_video">0</span></h2>
      <ul class="list" id="l_video"></ul>
    </section>
    <section class="card">
      <h2><span class="glyph">🟢</span> Presence <span class="count" id="c_presence">0</span></h2>
      <ul class="list" id="l_presence"></ul>
    </section>
    <section class="card">
      <h2><span class="glyph">📇</span> Registrations <span class="count" id="c_reg">0</span></h2>
      <ul class="list" id="l_reg"></ul>
    </section>
    <section class="card">
      <h2><span class="glyph">💬</span> Channels <span class="count" id="c_channels">0</span></h2>
      <ul class="list" id="l_channels"></ul>
    </section>
    <section class="card">
      <h2><span class="glyph">✉️</span> Messages <span class="count" id="c_messages">0</span></h2>
      <ul class="list" id="l_messages"></ul>
    </section>

    <section class="card wide">
      <h2><span class="glyph">⚡</span> Event stream <span class="count" id="c_events">0</span></h2>
      <ul class="list feed" id="l_events"></ul>
    </section>
  </div>

  <div class="foot">Transactional-outbox event stream · newest first · auto-refresh ~2s · self-contained, works offline</div>
</div>
<div class="toast" id="toast"></div>

<script>
(function () {
  "use strict";

  var POLL_MS = 2000;
  var $ = function (id) { return document.getElementById(id); };
  var tokenEl = $("token");

  // ---- tiny DOM helpers (textContent only — never innerHTML for data) ----
  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  }
  function shortId(v) {
    if (!v) return "—";
    var s = String(v);
    return s.length > 8 ? s.slice(0, 8) : s;
  }
  function fmtTime(v) {
    if (!v) return "";
    var d = new Date(v);
    if (isNaN(d.getTime())) return "";
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  }

  // colored state badges
  var STATE_COLOR = {
    ANSWERED: "ok", ACTIVE: "ok", AVAILABLE: "ok", DELIVERED: "ok", READ: "ok", REGISTERED: "ok", ONLINE: "ok",
    RINGING: "warn", INITIATED: "info", SENT: "info", ON_CALL: "info", ONCALL: "info",
    HELD: "purple",
    AWAY: "warn",
    ENDED: "grey", ARCHIVED: "grey", OFFLINE: "grey", EXPIRED: "grey",
    FAILED: "err", BUSY: "err", REJECTED: "err", NO_ANSWER: "err", DND: "err", UNREGISTERED: "err"
  };
  function colorFor(state) {
    var c = STATE_COLOR[String(state || "").toUpperCase()];
    return c || "grey";
  }
  function badge(state) {
    var b = el("span", "badge", state == null ? "?" : String(state));
    var c = colorFor(state);
    var col = "var(--" + c + ")";
    b.style.color = col;
    b.style.borderColor = "color-mix(in srgb, " + col + " 45%, transparent)";
    b.style.background = "color-mix(in srgb, " + col + " 14%, transparent)";
    return b;
  }

  function renderList(ulId, countId, items, err, rowFn) {
    var ul = $(ulId);
    ul.textContent = "";
    if (countId) $(countId).textContent = err ? "—" : (items ? items.length : 0);
    if (err) {
      ul.appendChild(el("li", "empty err", err));
      return;
    }
    if (!items || items.length === 0) {
      ul.appendChild(el("li", "empty", "None yet"));
      return;
    }
    items.forEach(function (it) {
      try { ul.appendChild(rowFn(it)); } catch (e) { /* skip malformed */ }
    });
  }

  // ---- fetch wrappers ----
  function bearer() {
    return { "Authorization": "Bearer " + (tokenEl.value || "").trim() };
  }
  function getOp(path) { // unauthenticated operational endpoints
    return fetch(path, { headers: { "Accept": "application/json" } });
  }
  function getV1(path) {
    return fetch(path, { headers: Object.assign({ "Accept": "application/json" }, bearer()) });
  }
  function itemsOf(json) {
    if (!json) return [];
    if (Array.isArray(json)) return json;
    if (Array.isArray(json.items)) return json.items;
    return [];
  }

  // Load one workload resource; degrade quietly on 404/empty/unreachable.
  function loadResource(path, ulId, countId, rowFn) {
    return getV1(path).then(function (r) {
      if (r.status === 404) { renderList(ulId, countId, [], null, rowFn); return; }
      if (r.status === 401 || r.status === 403) { renderList(ulId, countId, null, "unauthorized — check token", rowFn); return; }
      if (!r.ok) { renderList(ulId, countId, null, "error " + r.status, rowFn); return; }
      return r.json().then(function (j) { renderList(ulId, countId, itemsOf(j), null, rowFn); });
    }).catch(function () {
      renderList(ulId, countId, null, "unreachable", rowFn);
    });
  }

  // ---- row renderers ----
  function callRow(c) {
    var li = el("li", "row");
    li.appendChild(el("span", "id", shortId(c.id)));
    var main = el("div", "main");
    main.appendChild(el("span", null, (c.direction || "?")));
    var pair = el("span", null, " " + (c.from_ref || "?"));
    main.appendChild(pair);
    main.appendChild(el("span", "arrow", "→"));
    main.appendChild(el("span", null, (c.to_ref || "?")));
    li.appendChild(main);
    li.appendChild(badge(c.state));
    return li;
  }
  function videoRow(v) {
    var li = el("li", "row");
    li.appendChild(el("span", "id", shortId(v.id)));
    var main = el("div", "main");
    var n = v.name || ("room " + shortId(v.id));
    var parts = Array.isArray(v.participants) ? v.participants.length : 0;
    main.textContent = n + (v.mode ? " · " + v.mode : "") + " · " + parts + " participant" + (parts === 1 ? "" : "s");
    li.appendChild(main);
    li.appendChild(badge(v.state));
    return li;
  }
  function presenceRow(p) {
    var li = el("li", "row");
    li.appendChild(el("span", "id", shortId(p.user_id || p.id)));
    var main = el("div", "main");
    main.textContent = "user " + shortId(p.user_id || p.id) + (p.since ? " · since " + fmtTime(p.since) : "");
    li.appendChild(main);
    li.appendChild(badge(p.status));
    return li;
  }
  function regRow(x) {
    var li = el("li", "row");
    li.appendChild(el("span", "id", shortId(x.id)));
    var main = el("div", "main");
    main.textContent = x.aor || x.contact || x.user_ref || x.device_id || shortId(x.id);
    li.appendChild(main);
    li.appendChild(badge(x.state || x.status || "REGISTERED"));
    return li;
  }
  function channelRow(ch) {
    var li = el("li", "row");
    li.appendChild(el("span", "id", shortId(ch.id)));
    var main = el("div", "main");
    var members = Array.isArray(ch.members) ? ch.members.length : 0;
    main.textContent = (ch.name || "channel") + (ch.kind ? " · " + ch.kind : "") + " · " + members + " member" + (members === 1 ? "" : "s");
    li.appendChild(main);
    li.appendChild(badge(ch.state));
    return li;
  }
  function messageRow(m) {
    var li = el("li", "row");
    li.appendChild(el("span", "id", shortId(m.id)));
    var main = el("div", "main");
    var body = m.body != null ? m.body : "(no body)";
    main.textContent = (m.sender_ref || "?") + ": " + body;
    li.appendChild(main);
    li.appendChild(badge(m.state));
    return li;
  }

  // ---- event feed ----
  function eventRow(ev) {
    var li = el("li", "evt");
    var type = ev.type || ev.event_type || "Event";
    var t = el("span", "etype", type);
    // color the type by a coarse family
    var fam = /Failed|Rejected|Error|Busy/.test(type) ? "err"
            : /Ended|Archived|Expired/.test(type) ? "grey"
            : /Answered|Delivered|Created|Started|Registered|Sent/.test(type) ? "ok"
            : "info";
    t.style.color = "var(--" + fam + ")";
    li.appendChild(t);
    li.appendChild(el("span", "subj", shortId(ev.subject)));
    var when = el("time", null, fmtTime(ev.time));
    li.appendChild(when);
    return li;
  }
  function loadEvents() {
    return getOp("/_introspect/events").then(function (r) {
      if (!r.ok) throw new Error("status " + r.status);
      return r.json();
    }).then(function (arr) {
      var evs = Array.isArray(arr) ? arr.slice() : [];
      evs.reverse(); // ring is newest-last; show newest first
      $("c_events").textContent = evs.length;
      var ul = $("l_events");
      ul.textContent = "";
      if (evs.length === 0) { ul.appendChild(el("li", "empty", "No events yet — try a test action")); return; }
      evs.forEach(function (ev) { try { ul.appendChild(eventRow(ev)); } catch (e) {} });
    }).catch(function () {
      $("c_events").textContent = "—";
      var ul = $("l_events"); ul.textContent = "";
      ul.appendChild(el("li", "empty err", "event stream unreachable"));
    });
  }

  // ---- info / connectivity ----
  var connected = false;
  function setStatus(ok) {
    connected = ok;
    var dot = $("statusDot"), txt = $("statusText");
    dot.className = "dot " + (ok ? "live" : "down");
    txt.textContent = ok ? "connected" : "disconnected";
  }
  function humanUptime(ms) {
    if (ms < 0 || isNaN(ms)) return "—";
    var s = Math.floor(ms / 1000);
    var d = Math.floor(s / 86400); s -= d * 86400;
    var h = Math.floor(s / 3600); s -= h * 3600;
    var m = Math.floor(s / 60); s -= m * 60;
    var out = [];
    if (d) out.push(d + "d");
    if (h || d) out.push(h + "h");
    out.push(m + "m");
    out.push(s + "s");
    return out.join(" ");
  }
  var startedAt = null;
  function loadInfo() {
    return getOp("/info").then(function (r) {
      if (!r.ok) throw new Error("status " + r.status);
      return r.json();
    }).then(function (i) {
      setStatus(true);
      $("i_version").textContent = i.version || "—";
      $("i_spec").textContent = i.spec_version || "—";
      $("i_topo").textContent = i.topology || "—";
      $("i_arch").textContent = (i.arch || "?") + " / " + (i.os || "?");
      if (i.started_at) { var d = new Date(i.started_at); if (!isNaN(d.getTime())) startedAt = d.getTime(); }
      tickUptime();
    }).catch(function () {
      setStatus(false);
    });
  }
  function tickUptime() {
    if (startedAt) $("i_uptime").textContent = humanUptime(Date.now() - startedAt);
  }

  // ---- toast ----
  var toastTimer = null;
  function toast(msg, bad) {
    var t = $("toast");
    t.textContent = msg;
    t.className = "toast show" + (bad ? " bad" : "");
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { t.className = "toast"; }, 3200);
  }

  // ---- interactive actions ----
  function newUuidV7() {
    // RFC 9562 UUIDv7: 48-bit ms timestamp + version/variant + random.
    var ms = Date.now();
    var bytes = new Uint8Array(16);
    if (window.crypto && crypto.getRandomValues) crypto.getRandomValues(bytes);
    else for (var i = 0; i < 16; i++) bytes[i] = Math.floor(Math.random() * 256);
    bytes[0] = (ms / Math.pow(2, 40)) & 0xff;
    bytes[1] = (ms / Math.pow(2, 32)) & 0xff;
    bytes[2] = (ms / Math.pow(2, 24)) & 0xff;
    bytes[3] = (ms / Math.pow(2, 16)) & 0xff;
    bytes[4] = (ms / Math.pow(2, 8)) & 0xff;
    bytes[5] = ms & 0xff;
    bytes[6] = 0x70 | (bytes[6] & 0x0f);         // version 7
    bytes[8] = 0x80 | (bytes[8] & 0x3f);         // variant
    var hex = [];
    for (var j = 0; j < 16; j++) hex.push((bytes[j] + 0x100).toString(16).slice(1));
    return hex.slice(0, 4).join("") + "-" + hex.slice(4, 6).join("") + "-" +
           hex.slice(6, 8).join("") + "-" + hex.slice(8, 10).join("") + "-" + hex.slice(10, 16).join("");
  }

  function originateCall() {
    var btn = $("btnCall"); btn.disabled = true;
    var suffix = String(1000 + Math.floor(Math.random() * 9000));
    var body = { direction: "OUTBOUND", from_ref: "sip:agent-" + suffix, to_ref: "+1415555" + suffix };
    fetch("/v1/calls", {
      method: "POST",
      headers: Object.assign({ "Content-Type": "application/json" }, bearer()),
      body: JSON.stringify(body)
    }).then(function (r) {
      if (r.ok) { toast("Call originated → " + body.to_ref); refresh(); }
      else if (r.status === 401 || r.status === 403) toast("Unauthorized — check the token", true);
      else toast("Call failed (" + r.status + ")", true);
    }).catch(function () { toast("Could not reach server", true); })
      .finally(function () { btn.disabled = false; });
  }

  // Send a message: reuse an existing channel or create one, then POST the message.
  function sendMessage() {
    var btn = $("btnMsg"); btn.disabled = true;
    getV1("/v1/channels?limit=1").then(function (r) {
      if (r.status === 401 || r.status === 403) throw new Error("unauth");
      if (!r.ok) return [];
      return r.json().then(itemsOf);
    }).then(function (chs) {
      if (chs && chs.length && chs[0].id) return chs[0].id;
      // create one
      return fetch("/v1/channels", {
        method: "POST",
        headers: Object.assign({ "Content-Type": "application/json" }, bearer()),
        body: JSON.stringify({ kind: "CHAT", name: "dashboard-test", members: ["dashboard"] })
      }).then(function (r) {
        if (!r.ok) throw new Error("channel " + r.status);
        return r.json().then(function (c) { return c.id; });
      });
    }).then(function (channelId) {
      return fetch("/v1/messages", {
        method: "POST",
        headers: Object.assign({ "Content-Type": "application/json" }, bearer()),
        body: JSON.stringify({
          channel_id: channelId,
          sender_ref: "dashboard",
          body: "Hello from the CommOS dashboard at " + new Date().toLocaleTimeString()
        })
      });
    }).then(function (r) {
      if (r.ok) { toast("Message sent"); refresh(); }
      else toast("Message failed (" + r.status + ")", true);
    }).catch(function (e) {
      toast(e && e.message === "unauth" ? "Unauthorized — check the token" : "Could not send message", true);
    }).finally(function () { btn.disabled = false; });
  }

  // ---- refresh cycle ----
  var inflight = false;
  function refresh() {
    if (inflight) return;
    inflight = true;
    Promise.allSettled([
      loadInfo(),
      loadEvents(),
      loadResource("/v1/calls", "l_calls", "c_calls", callRow),
      loadResource("/v1/video-rooms", "l_video", "c_video", videoRow),
      loadResource("/v1/presence", "l_presence", "c_presence", presenceRow),
      loadResource("/v1/registrations", "l_reg", "c_reg", regRow),
      loadResource("/v1/channels", "l_channels", "c_channels", channelRow),
      loadResource("/v1/messages", "l_messages", "c_messages", messageRow)
    ]).finally(function () { inflight = false; });
  }

  // uptime ticks every second even between polls
  setInterval(tickUptime, 1000);

  // poll on interval; pause while the tab is hidden
  setInterval(function () { if (!document.hidden) refresh(); }, POLL_MS);
  document.addEventListener("visibilitychange", function () { if (!document.hidden) refresh(); });

  $("btnCall").addEventListener("click", originateCall);
  $("btnMsg").addEventListener("click", sendMessage);

  refresh();
})();
</script>
</body>
</html>
"####;
