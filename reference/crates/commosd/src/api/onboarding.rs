//! `/v1/onboarding` — the admin setup wizard's brain, plus a self-contained wizard page.
//!
//! The goal is **rapid onboarding**: detect the network, discover phones, and propose good
//! defaults so the operator confirms rather than fills in forms. Everything here is comms
//! *management*, not SIP mechanics.

use axum::extract::{Query, State};
use axum::response::Html;
use axum::Json;
use serde::{Deserialize, Serialize};

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::problem::Problem;
use crate::control::onboarding::{
    self, Environment, EnvironmentProfile, MacBinding, OnboardingSuggestion,
};
use crate::state::AppState;
use crate::store::Tx;

/// `GET /v1/onboarding/environments` — the deployment kinds and their default profiles.
pub async fn list_environments(_tenant: TenantContext) -> Json<Vec<EnvironmentProfile>> {
    Json(onboarding::profiles())
}

#[derive(Deserialize)]
pub struct SuggestParams {
    /// office | hospitality | hospital | home
    pub environment: String,
    /// How many phones/devices will be deployed.
    #[serde(default)]
    pub devices: u32,
    /// SIP/DNS domain to use in provisioning records.
    pub domain: Option<String>,
    /// The HTTP port CommOS serves on (for the provisioning URL). Defaults to 8080.
    pub http_port: Option<u16>,
    /// The SIP UDP port (for the DNS SRV record). Defaults to 5060.
    pub sip_port: Option<u16>,
    /// Which host interface (by name, e.g. `eth0`) to align the phone plan on. When the host
    /// has more than one NIC the wizard offers this so the right subnet is picked. Omit to use
    /// the primary outbound interface.
    pub interface: Option<String>,
    /// Point phones at HTTPS/SIPS instead of plain HTTP/SIP-UDP. Defaults to `false` — a
    /// self-signed cert is rejected by most LAN phones, so SSL is opt-in (see the guide's advice).
    #[serde(default)]
    pub tls: bool,
}

/// `GET /v1/onboarding/suggest` — the full auto-detected suggestion for one round-trip setup.
pub async fn suggest(
    _tenant: TenantContext,
    Query(p): Query<SuggestParams>,
) -> Result<Json<OnboardingSuggestion>, Problem> {
    let env = Environment::parse(&p.environment)
        .ok_or_else(|| Problem::bad_request("unknown environment (office|hospitality|hospital|home)"))?;
    let domain = p.domain.unwrap_or_else(|| "commos.local".to_string());
    let http_port = p.http_port.unwrap_or(8080);
    let sip_port = p.sip_port.unwrap_or(5060);
    Ok(Json(onboarding::suggest(
        env,
        p.devices,
        &domain,
        http_port,
        sip_port,
        p.interface.as_deref(),
        p.tls,
    )))
}

/// Body for `POST /v1/onboarding/apply` — the operator's confirmed choice.
#[derive(Deserialize)]
pub struct ApplyBody {
    pub environment: String,
    #[serde(default)]
    pub devices: u32,
    /// The chosen starting series (defaults to the suggested one for the environment/fleet).
    pub series_start: Option<String>,
    /// SIP domain woven into each extension's route (`sip:<number>@<domain>`). Defaults to
    /// `commos.local`.
    pub domain: Option<String>,
    /// Explicit phone↔extension alignment the operator confirmed in the wizard: each entry pins a
    /// MAC to an extension number. When present these win over ARP-order guessing, so a phone
    /// lands on exactly the number the operator chose. Omit to keep the auto-bind behaviour.
    #[serde(default)]
    pub bindings: Vec<MacBinding>,
}

/// What `apply` created.
#[derive(Serialize)]
pub struct ApplyOutcome {
    pub users_created: usize,
    pub extensions_created: usize,
    pub devices_created: usize,
    pub routes_created: usize,
    pub extensions: Vec<String>,
}

/// `POST /v1/onboarding/apply` — mint the people, extensions, routes, and phones the operator
/// confirmed, in one transaction. Discovered phones are bound to extensions so they can
/// auto-provision (`/provision/{mac}.cfg`). Privileged: requires an admin (see [`AdminContext`]).
pub async fn apply(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(body): Json<ApplyBody>,
) -> Result<Json<ApplyOutcome>, Problem> {
    let env = Environment::parse(&body.environment)
        .ok_or_else(|| Problem::bad_request("unknown environment"))?;
    if body.devices == 0 || body.devices > 10_000 {
        return Err(Problem::bad_request("devices must be between 1 and 10000"));
    }
    let series = body
        .series_start
        .unwrap_or_else(|| onboarding::suggest_extension_plan(env, body.devices).recommended_series);
    let domain = body.domain.unwrap_or_else(|| "commos.local".to_string());

    let built =
        onboarding::build_entities(admin.tenant_id, body.devices, &series, &domain, &body.bindings);
    let outcome = ApplyOutcome {
        users_created: built.users.len(),
        extensions_created: built.extensions.len(),
        devices_created: built.devices.len(),
        routes_created: built.routes.len(),
        extensions: built.extensions.iter().map(|e| e.number.clone()).collect(),
    };
    let extension_numbers: Vec<String> = built.extensions.iter().map(|e| e.number.clone()).collect();
    st.store
        .commit(Tx {
            users: built.users,
            extensions: built.extensions,
            devices: built.devices,
            routes: built.routes,
            ..Default::default()
        })
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    // Mint each phone's SIP secret so they can authenticate and auto-provision.
    for number in &extension_numbers {
        st.provisioning
            .ensure_credential(admin.tenant_id, number)
            .await
            .map_err(|e| Problem::internal(e.to_string()))?;
    }
    Ok(Json(outcome))
}

/// `GET /onboarding` — a self-contained setup wizard page (unauthenticated, like the
/// dashboard). Pick an environment and fleet size; it auto-detects the rest.
pub async fn wizard() -> Html<&'static str> {
    Html(WIZARD_HTML)
}

const WIZARD_HTML: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>CommOS — Setup</title>
<style>
:root{--bg:#0b1020;--card:#151b31;--fg:#e7ecf5;--muted:#9aa6c2;--acc:#5b8cff;--ok:#37d39b;--warn:#f5a623;--mono:ui-monospace,SFMono-Regular,Menlo,monospace}
@media (prefers-color-scheme:light){:root{--bg:#f4f6fb;--card:#fff;--fg:#161c2e;--muted:#5a6785;--acc:#2f6bff}}
*{box-sizing:border-box}body{margin:0;font:15px/1.5 system-ui,Segoe UI,Roboto,sans-serif;background:var(--bg);color:var(--fg)}
.wrap{max-width:960px;margin:0 auto;padding:28px 20px 60px}
h1{font-size:22px;margin:0 0 2px}.sub{color:var(--muted);margin:0 0 22px}
.card{background:var(--card);border-radius:14px;padding:18px 20px;margin:14px 0;box-shadow:0 1px 0 rgba(0,0,0,.15)}
label{display:block;font-weight:600;margin:10px 0 6px}
.envs{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:10px}
.env{border:2px solid transparent;border-radius:12px;padding:12px;background:rgba(127,127,127,.08);cursor:pointer}
.env.sel{border-color:var(--acc)}.env b{display:block}.env small{color:var(--muted)}
input,button{font:inherit;color:inherit}
input[type=number]{background:rgba(127,127,127,.1);border:1px solid rgba(127,127,127,.3);border-radius:8px;padding:8px 10px;width:120px}
button{background:var(--acc);color:#fff;border:0;border-radius:10px;padding:10px 18px;font-weight:600;cursor:pointer;margin-top:12px}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(240px,1fr));gap:14px}
.k{color:var(--muted);font-size:13px}.v{font-weight:600}
.pill{display:inline-block;background:rgba(91,140,255,.15);color:var(--acc);border-radius:999px;padding:2px 10px;font-size:12px;font-weight:600}
pre{background:rgba(127,127,127,.12);border-radius:10px;padding:12px;overflow:auto;font-family:var(--mono);font-size:13px;margin:6px 0}
select{background:rgba(127,127,127,.1);border:1px solid rgba(127,127,127,.3);border-radius:8px;padding:8px 10px;color:inherit}
.warn{color:var(--warn);font-weight:600}.ok{color:var(--ok)}.mono{font-family:var(--mono)}
table{width:100%;border-collapse:collapse;font-size:13px}td,th{text-align:left;padding:6px 8px;border-bottom:1px solid rgba(127,127,127,.15)}
.hide{display:none}
</style></head><body><div class="wrap">
<h1>Welcome to CommOS</h1>
<p class="sub">Two questions, and we'll set up the rest. Everything below is auto-detected or a sensible default you can change.</p>

<div class="card">
  <label>1 · Where is this being deployed?</label>
  <div class="envs" id="envs"></div>
  <label>2 · How many phones/devices?</label>
  <input type="number" id="devices" value="20" min="1" max="100000">
  <div><button id="go">Detect &amp; suggest →</button></div>
  <p class="k" id="tok">Uses the dev token for setup; configure real auth later.</p>
</div>

<div id="out"></div>
</div>
<script>
const TOKEN="tenant:01920000-0000-7000-8000-000000000001";
// Admin session token, set after a successful /admin/login (configured deployments only).
let ADMIN=null;
const el=(t,c,x)=>{const e=document.createElement(t);if(c)e.className=c;if(x!=null)e.textContent=x;return e};
// The bearer to use for a privileged call: an admin session if we have one, else the dev
// token (which acts as admin when no admin password is configured).
const adminBearer=()=>ADMIN?('admin:'+ADMIN):TOKEN;
// Exchange an admin password for a session token; returns true on success.
async function adminLogin(password){
  try{
    const r=await fetch('/admin/login',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({password})});
    if(!r.ok)return false;
    const o=await r.json();ADMIN=o.token;return true;
  }catch(e){return false}
}
let env="office";
// Which host interface to align on (null → primary). Persisted across re-renders.
let iface=null;
// Whether to point phones at HTTPS/SIPS. Default OFF — self-signed certs are rejected on a LAN.
let useTls=false;
// Explicit phone↔extension alignment: MAC (as displayed) → extension number. Survives re-render.
const macNumber={};
// Manual MAC↔number rows for phones not yet on the wire. Survives re-render.
const manualBindings=[];
async function loadEnvs(){
  const box=document.getElementById('envs');
  try{
    const r=await fetch('/v1/onboarding/environments',{headers:{Authorization:'Bearer '+TOKEN}});
    const list=await r.json();
    box.innerHTML='';
    list.forEach((p,i)=>{
      const d=el('div','env'+(p.environment===env?' sel':''));
      d.appendChild(el('b',null,p.title));
      d.appendChild(el('small',null,p.description));
      d.onclick=()=>{env=p.environment;[...box.children].forEach(c=>c.classList.remove('sel'));d.classList.add('sel')};
      box.appendChild(d);
    });
  }catch(e){box.textContent='Could not load environments.'}
}
function row(k,v){const d=el('div');d.appendChild(el('div','k',k));d.appendChild(el('div','v',v));return d}
async function suggest(){
  const devices=document.getElementById('devices').value||20;
  const qp={environment:env,devices,http_port:location.port||8080,tls:useTls};
  if(iface)qp.interface=iface;
  const q=new URLSearchParams(qp);
  const out=document.getElementById('out');out.innerHTML='';
  let s;try{
    const r=await fetch('/v1/onboarding/suggest?'+q,{headers:{Authorization:'Bearer '+TOKEN}});
    if(!r.ok){out.textContent='Error: '+r.status;return}
    s=await r.json();
  }catch(e){out.textContent='Request failed.';return}
  if(iface==null&&s.selected_interface)iface=s.selected_interface;

  // Extensions
  const ext=el('div','card');ext.appendChild(el('h3',null,'Extensions'));
  const eg=el('div','grid');
  const series=el('div');series.appendChild(el('div','k','Starting series (suggested)'));
  const sel=el('select');s.extension_plan.series_options.forEach(o=>{const op=el('option',null,o);op.value=o;if(o===s.extension_plan.recommended_series)op.selected=true;sel.appendChild(op)});
  series.appendChild(sel);eg.appendChild(series);
  eg.appendChild(row('Digits',s.extension_plan.digits));
  eg.appendChild(row('Reception / help desk',s.extension_plan.reception_extension));
  eg.appendChild(row('Example extensions',s.extension_plan.example_extensions.join(', ')));
  ext.appendChild(eg);
  const fc=el('div');fc.appendChild(el('div','k','Feature codes (defaults)'));
  const tbl=el('table');s.extension_plan.feature_codes.forEach(f=>{const tr=el('tr');const a=el('td','mono',f.code);const b=el('td',null,f.purpose);tr.append(a,b);tbl.appendChild(tr)});
  fc.appendChild(tbl);ext.appendChild(fc);out.appendChild(ext);

  // IP plan
  const ip=el('div','card');ip.appendChild(el('h3',null,'Network'));
  // Interface picker — only meaningful when the host has more than one usable NIC. Choosing the
  // wrong one aligns the whole plan on the wrong subnet, so ask which interface carries the phones.
  if(s.interfaces&&s.interfaces.length>1){
    const isel=el('div');isel.appendChild(el('div','k','This host has several interfaces — which one carries the phones?'));
    const ims=el('select');
    s.interfaces.forEach(it=>{
      const label=it.name+' · '+it.ipv4+' ('+it.cidr+')'+(it.is_primary?' · default':'');
      const op=el('option',null,label);op.value=it.name;if(it.name===s.selected_interface)op.selected=true;ims.appendChild(op);
    });
    ims.onchange=()=>{iface=ims.value;suggest();};
    isel.appendChild(ims);ip.appendChild(isel);
  }else if(s.interfaces&&s.interfaces.length===1){
    ip.appendChild(el('p','k','Aligning on '+s.interfaces[0].name+' ('+s.interfaces[0].cidr+') — the only LAN interface.'));
  }
  const ig=el('div','grid');
  ig.appendChild(row('Detected host IP',s.ip_plan.detected_host_ip||'—'));
  ig.appendChild(row('LAN subnet',s.ip_plan.detected_subnet||'—'));
  ig.appendChild(row('Suggested phone pool',(s.ip_plan.phone_pool_start||'—')+' – '+(s.ip_plan.phone_pool_end||'—')));
  ig.appendChild(row('Pool capacity',s.ip_plan.phone_pool_capacity));
  ip.appendChild(ig);
  const fit=el('p',s.ip_plan.fits?'ok':'warn',s.ip_plan.fits?'✓ The fleet fits this subnet.':('⚠ '+(s.ip_plan.recommendation||'')));
  ip.appendChild(fit);out.appendChild(ip);

  // Discovered devices — with an editable "Extension #" so the operator lines up each handset
  // (by MAC) with the number it should own. Likely phones are pre-filled with a sequential
  // extension from the chosen series; every field is editable and blank means "don't bind".
  const dv=el('div','card');dv.appendChild(el('h3',null,'Align phones to extensions'));
  dv.appendChild(el('p','k','Each phone found on the network, with the extension it will become. Edit any number; clear it to skip. Phones bind by MAC, so they keep their number even if their IP changes.'));
  const base=parseInt(sel.value||s.extension_plan.recommended_series||'100')||100;
  let seq=0; // running offset for pre-filling likely phones
  if(s.discovered_devices.length){
    const t=el('table');const hr=el('tr');['IP','MAC','Vendor','Phone?','Extension #'].forEach(h=>hr.appendChild(el('th',null,h)));t.appendChild(hr);
    s.discovered_devices.forEach(d=>{
      const tr=el('tr');
      tr.appendChild(el('td','mono',d.ip));
      tr.appendChild(el('td','mono',d.mac));
      tr.appendChild(el('td',null,d.vendor||'—'));
      tr.appendChild(el('td',null,d.likely_phone?'yes':''));
      // Pre-fill an extension for likely phones the first time we see them this session.
      if(!(d.mac in macNumber)&&d.likely_phone){macNumber[d.mac]=String(base+(seq++));}
      const inp=el('input');inp.type='number';inp.min='0';inp.style.width='110px';
      inp.value=macNumber[d.mac]||'';inp.placeholder='(skip)';
      inp.oninput=()=>{macNumber[d.mac]=inp.value;};
      const td=el('td');td.appendChild(inp);tr.appendChild(td);
      t.appendChild(tr);
    });
    dv.appendChild(t);
  }else dv.appendChild(el('p','k','No phones on the wire yet — power them on and re-run, or add them by MAC below. (ARP table.)'));

  // Manual alignment — add a phone that hasn't appeared on the network yet, by MAC.
  const man=el('div');man.appendChild(el('div','k','Add a phone by MAC (for handsets not yet powered on)'));
  const mtbl=el('table');
  const renderManual=()=>{
    mtbl.innerHTML='';
    const hr=el('tr');['MAC','Extension #',''].forEach(h=>hr.appendChild(el('th',null,h)));mtbl.appendChild(hr);
    manualBindings.forEach((b,i)=>{
      const tr=el('tr');
      const mi=el('input');mi.type='text';mi.placeholder='00:15:65:aa:bb:cc';mi.value=b.mac;mi.oninput=()=>{b.mac=mi.value;};
      const ni=el('input');ni.type='number';ni.min='0';ni.placeholder='101';ni.style.width='110px';ni.value=b.number;ni.oninput=()=>{b.number=ni.value;};
      const rm=el('button',null,'Remove');rm.style.margin='0';rm.style.background='transparent';rm.style.color='var(--warn)';rm.onclick=()=>{manualBindings.splice(i,1);renderManual();};
      [mi,ni,rm].forEach(x=>{const td=el('td');td.appendChild(x);tr.appendChild(td);});
      mtbl.appendChild(tr);
    });
  };
  renderManual();man.appendChild(mtbl);
  const addBtn=el('button',null,'+ Add phone');addBtn.style.background='transparent';addBtn.style.color='var(--acc)';addBtn.style.border='1px solid var(--acc)';
  addBtn.onclick=()=>{manualBindings.push({mac:'',number:''});renderManual();};
  man.appendChild(addBtn);dv.appendChild(man);
  out.appendChild(dv);

  // Provisioning
  const pv=el('div','card');pv.appendChild(el('h3',null,'Auto-provisioning — paste these into your DNS / DHCP'));
  // SSL toggle — optional, and off by default. A self-signed cert on a LAN is rejected by most
  // phones, so plain HTTP/UDP is the pragmatic default; media is still SRTP-encrypted regardless.
  const tl=el('label');tl.style.fontWeight='600';
  const tc=el('input');tc.type='checkbox';tc.checked=useTls;tc.style.marginRight='8px';
  tc.onchange=()=>{useTls=tc.checked;suggest();};
  tl.appendChild(tc);tl.appendChild(document.createTextNode('Use SSL/TLS (HTTPS + SIPS) — only with a CA-signed certificate'));
  pv.appendChild(tl);
  pv.appendChild(el('p',s.provisioning.tls?'warn':'k',s.provisioning.tls_advice));
  pv.appendChild(el('div','k','DHCP (dnsmasq)'));
  pv.appendChild(Object.assign(el('pre'),{textContent:s.provisioning.dhcp_dnsmasq.join('\n')}));
  pv.appendChild(el('div','k','DNS (BIND zone)'));
  pv.appendChild(Object.assign(el('pre'),{textContent:s.provisioning.dns_bind_zone.join('\n')}));
  pv.appendChild(el('p','k',s.provisioning.note));
  out.appendChild(pv);

  // Apply — one click to create the people + extensions (+ bind discovered phones).
  const ap=el('div','card');ap.appendChild(el('h3',null,'Create it'));
  ap.appendChild(el('p','k','Creates '+document.getElementById('devices').value+' extensions and people, using the selected series. Phones aligned above are bound to their number by MAC so they auto-provision.'));
  const btn=el('button',null,'Create extensions & people →');
  const res=el('p','k');
  // Collect the operator's phone↔number alignment (discovered + manual), dropping blanks.
  const collectBindings=()=>{
    const out=[];
    Object.keys(macNumber).forEach(mac=>{const n=(macNumber[mac]||'').trim();if(n)out.push({mac,number:n});});
    manualBindings.forEach(b=>{const m=(b.mac||'').trim(),n=(b.number||'').trim();if(m&&n)out.push({mac:m,number:n});});
    return out;
  };
  const doApply=async()=>{
    const body=JSON.stringify({environment:env,devices:parseInt(document.getElementById('devices').value||'0'),series_start:sel.value,bindings:collectBindings()});
    return fetch('/v1/onboarding/apply',{method:'POST',headers:{Authorization:'Bearer '+adminBearer(),'content-type':'application/json'},body});
  };
  btn.onclick=async()=>{
    btn.disabled=true;res.className='k';res.textContent='Applying…';
    try{
      let r=await doApply();
      // A configured deployment rejects the dev token: prompt for the admin password, log in,
      // and retry once with the admin session.
      if(r.status===401&&!ADMIN){
        const pw=prompt('Admin password required to apply setup:');
        if(pw&&await adminLogin(pw)){r=await doApply();}
        else{res.className='warn';res.textContent='Admin login required.';btn.disabled=false;return;}
      }
      const o=await r.json();
      if(!r.ok){res.className='warn';res.textContent='Error: '+(o.detail||r.status);}
      else{res.className='ok';res.textContent='✓ Created '+o.extensions_created+' extensions ('+o.extensions.slice(0,5).join(', ')+(o.extensions.length>5?'…':'')+'), '+o.users_created+' people, '+o.routes_created+' routes, '+o.devices_created+' phones bound.';}
    }catch(e){res.className='warn';res.textContent='Request failed.';}
    btn.disabled=false;
  };
  ap.appendChild(btn);ap.appendChild(res);out.appendChild(ap);
}
document.getElementById('go').onclick=suggest;
loadEnvs();
</script></body></html>"##;
