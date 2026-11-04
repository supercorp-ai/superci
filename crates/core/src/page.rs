//! HTML for the dashboard `superci` serves on the user's machine (the online control plane has no pages).
use crate::io::Response;

pub fn esc(s: &str) -> String {
    s.chars().map(|c| match c { '&' => "&amp;".into(), '<' => "&lt;".into(), '>' => "&gt;".into(), '"' => "&quot;".into(), '\'' => "&#39;".into(), c => c.to_string() }).collect()
}

/// The look of supercov's reports (shared with supercov.com): warm paper, white cards, tight ink type, mono labels,
/// orange for work left, blue for what you can act on.
const STYLE: &str = r#":root{color-scheme:light;--bg:#f2f1ed;--bg-deep:#e9e8e3;--card:#fff;--card-2:#f6f5f2;--ink:#10141c;--muted:#59627a;--faint:#8d95a8;
--rule:rgba(16,20,28,.12);--rule-soft:rgba(16,20,28,.07);--accent:#0071e3;--accent-hover:#0062c4;--accent-soft:#e6f0fd;--open:#e0680a;--open-ink:#b44f00;--open-soft:#fbead9;
--good:#1f8a4c;--good-fill:#2fb35f;--good-soft:#e3f5ea;--bad:#c8321f;--bad-fill:#e5442f;--bad-soft:#fde9e6;--pick-last:#eaf2fd;--sk:#ecebe6;--sk-hi:rgba(255,255,255,.65);
--shadow:0 1px 2px rgba(16,20,28,.05),0 14px 34px rgba(16,20,28,.07);
--sans:-apple-system,BlinkMacSystemFont,"Helvetica Neue",Helvetica,Arial,sans-serif;
--mono:ui-monospace,SFMono-Regular,"SF Mono",Menlo,Monaco,Consolas,"Liberation Mono","Courier New",monospace}
:root[data-theme=dark]{color-scheme:dark;--bg:#0b0b0c;--bg-deep:#050506;--card:#141415;--card-2:#1c1c1e;--ink:#f5f5f7;--muted:#a1a1a6;--faint:#8e8e93;
--rule:rgba(255,255,255,.15);--rule-soft:rgba(255,255,255,.09);--accent:#2997ff;--accent-hover:#52abff;--accent-soft:rgba(41,151,255,.16);--open:#ff9f0a;--open-ink:#ffb340;--open-soft:rgba(255,159,10,.15);
--good:#32d74b;--good-fill:#30d158;--good-soft:rgba(50,215,75,.15);--bad:#ff6961;--bad-fill:#ff453a;--bad-soft:rgba(255,69,58,.16);--pick-last:#0f1d2e;--sk:#1c1c1e;--sk-hi:rgba(255,255,255,.06);--shadow:0 1px 2px rgba(0,0,0,.5),0 14px 34px rgba(0,0,0,.45)}
*,*::before,*::after{box-sizing:border-box}html{min-width:320px;background:var(--bg);-webkit-text-size-adjust:100%}
body{margin:0;color:var(--ink);background:var(--bg);font:400 14px/1.5 var(--sans);letter-spacing:-.006em;-webkit-font-smoothing:antialiased}
button,input,select{font:inherit;color:inherit;letter-spacing:inherit}button{border:0;background:none;padding:0;text-align:left;cursor:pointer}
a{color:var(--accent);text-decoration:none}h1,h2,h3{margin:0;font-weight:600;letter-spacing:-.035em;line-height:1.05}
::selection{background:#ffd9b0;color:#10141c}:focus-visible{outline:2px solid var(--accent);outline-offset:3px;border-radius:4px}
code{font:12px var(--mono);overflow-wrap:anywhere}form{display:inline}p{margin:0}
.rail{display:flex;align-items:center;gap:9px;margin:26px 8px 10px;font:600 11px/1 var(--mono);letter-spacing:.14em;text-transform:uppercase;color:var(--faint)}
.rail::before{content:"";width:16px;height:1px;background:currentColor;opacity:.6}
.pill{display:inline-flex;align-items:center;gap:6px;height:24px;padding:0 10px;border-radius:999px;font-size:12px;font-weight:600;white-space:nowrap;color:var(--muted);background:var(--card-2)}
.pill.good{color:var(--good);background:var(--good-soft)}.pill.open{color:var(--open-ink);background:var(--open-soft)}.pill.bad{color:var(--bad);background:var(--bad-soft)}.pill.accent{color:var(--accent);background:var(--accent-soft)}
.pill i{width:6px;height:6px;border-radius:50%;background:currentColor}
.button{display:inline-flex;align-items:center;justify-content:center;gap:8px;height:36px;padding:0 16px;border-radius:10px;color:#fff;background:var(--ink);font-size:13px;font-weight:550;box-shadow:0 1px 2px rgba(16,20,28,.16);transition:transform .18s cubic-bezier(.2,.7,.3,1),background .18s;white-space:nowrap}
.button:hover{background:#000;transform:translateY(-1px)}:root[data-theme=dark] .button{color:#10141c;background:#fff}
.button.primary,:root[data-theme=dark] .button.primary{color:#fff;background:var(--accent)}.button.primary:hover{background:var(--accent-hover)}
.button.secondary,:root[data-theme=dark] .button.secondary{color:var(--ink);background:var(--card);border:1px solid var(--rule);box-shadow:0 1px 2px rgba(16,20,28,.05)}.button.secondary:hover{background:var(--card-2)}
.chip{display:inline-flex;align-items:center;justify-content:center;gap:6px;min-height:30px;padding:5px 12px;border:1px solid var(--rule);border-radius:999px;background:var(--card);color:var(--ink);font-size:12px;font-weight:500;box-shadow:0 1px 2px rgba(0,0,0,.03);white-space:nowrap}
.chip:hover{background:var(--card-2);border-color:var(--faint)}
input[type=text],input[type=password],select{height:36px;padding:0 12px;border:1px solid var(--rule);border-radius:10px;background:var(--card);color:var(--ink)}
.layout{min-height:100vh;display:grid;grid-template-columns:264px minmax(0,1fr)}
.side{position:sticky;top:0;height:100vh;display:flex;flex-direction:column;padding:20px 14px 16px;border-right:1px solid var(--rule-soft);background:var(--bg-deep);overflow-y:auto}
.brand{display:flex;align-items:center;gap:8px;padding:0 8px;font-size:15px;font-weight:620;letter-spacing:-.035em}.brand>span{flex:1}.mark{width:22px;height:22px}
.toggle{width:28px;height:28px;display:grid;place-items:center;border-radius:8px;color:var(--faint)}.toggle:hover{color:var(--ink);background:var(--card)}
.toggle svg{width:16px;height:16px;fill:none;stroke:currentColor;stroke-width:1.75;stroke-linecap:round;stroke-linejoin:round}
.item{width:100%;display:grid;gap:4px;padding:11px 12px;border:1px solid transparent;border-radius:12px;color:var(--muted)}
a.item:hover{color:var(--ink);background:var(--card)}.item[aria-current=true]{color:var(--ink);background:var(--card);border-color:var(--rule-soft);box-shadow:var(--shadow)}
.item-top{display:flex;align-items:center;gap:8px;font-size:13px;font-weight:600;color:var(--ink);letter-spacing:-.015em}.item-top>span:not(.dot):not(.pill){flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.dot{flex:0 0 7px}
.item-sub{font:500 10.5px/1.3 var(--mono);color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.side-foot{margin-top:auto;padding:16px 8px 0;display:grid;gap:10px}.side-foot .signed{font:500 10.5px/1.4 var(--mono);color:var(--faint)}
.nav{display:grid;gap:2px;margin-top:26px}.nav a{display:flex;align-items:center;gap:10px;padding:9px 10px;border-radius:10px;color:var(--muted);font-size:13.5px;font-weight:550;letter-spacing:-.015em}
.nav a:hover{color:var(--ink);background:var(--card)}.nav a[aria-current=true]{color:var(--ink);background:var(--card);box-shadow:var(--shadow)}
.nav svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round;flex:0 0 17px}
.nav .count{margin-left:auto;font:500 10.5px var(--mono);color:var(--faint)}
.page-title{font-size:26px;letter-spacing:-.035em}
.main{width:100%;max-width:1180px;min-width:0;margin:0 auto;padding:0 clamp(24px,4.5vw,64px) 96px}
.bar{display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;gap:10px 16px;padding:16px 0 14px}
.tabs{display:flex;gap:2px;padding:3px;border-radius:999px;background:var(--bg-deep);border:1px solid var(--rule-soft)}
.tab{padding:7px 14px;border-radius:999px;color:var(--muted);font-size:12.5px;font-weight:600;white-space:nowrap}.tab:hover{color:var(--ink)}
.tab[aria-selected=true]{color:var(--ink);background:var(--card);box-shadow:0 1px 2px rgba(16,20,28,.08),0 3px 10px rgba(16,20,28,.06)}
.context{display:flex;flex-wrap:wrap;align-items:center;gap:8px 0;margin:6px 0 22px;color:var(--muted);font-size:12.5px}.context>*+*::before{content:"·";margin:0 10px;color:var(--faint)}
.context code{color:var(--ink)}.cards{display:grid;gap:20px}
.card{display:flex;flex-direction:column;gap:16px;min-width:0;padding:24px;border:1px solid var(--rule-soft);border-radius:16px;background:var(--card);box-shadow:var(--shadow)}
.card.missing{background:transparent;border:1px dashed var(--rule);box-shadow:none}
.card-head{display:flex;align-items:center;justify-content:space-between;gap:12px;min-height:30px}.card-head h2{font-size:15px;letter-spacing:-.015em}
.figures{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:24px}
.figure-number{font-size:40px;font-weight:600;line-height:.95;letter-spacing:-.05em;font-variant-numeric:tabular-nums}.figure-number.open{color:var(--open-ink)}.figure-number.none{color:var(--faint)}
.figure-unit{margin-top:8px;color:var(--muted);font-size:13px}.figure-unit .compare{display:block;margin-top:4px;color:var(--faint);font-size:12px}
.rows{display:grid}.row{display:grid;grid-template-columns:8px minmax(0,1fr) auto;gap:12px;align-items:center;padding:11px 12px;min-height:48px;border-radius:9px}
.row:hover{background:var(--card-2)}.row-name{display:block;overflow:hidden;font-size:13px;font-weight:600;letter-spacing:-.015em;text-overflow:ellipsis;white-space:nowrap}
.row-sub{display:block;margin-top:2px;color:var(--faint);font-size:11.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.row-end{display:flex;align-items:center;gap:10px;flex-wrap:wrap;justify-content:flex-end}.mono{font:500 11px/1 var(--mono);color:var(--muted);font-variant-numeric:tabular-nums;white-space:nowrap}
.dot{width:7px;height:7px;border-radius:50%;background:var(--faint)}.dot.good{background:var(--good-fill)}.dot.open{background:var(--open)}.dot.bad{background:var(--bad-fill)}.dot.accent{background:var(--accent)}
.note{color:var(--muted);font-size:12.5px}.empty{padding:28px 12px;text-align:center;color:var(--muted)}
.plain{max-width:640px;margin:0 auto;padding:64px 24px}.plain h1{font-size:26px;margin-bottom:12px}
.wait{display:grid;place-items:center;min-height:100vh;text-align:center;padding:24px}.wait .mark{width:34px;height:34px;margin:0 auto 18px}.wait h1{font-size:22px;margin-bottom:8px}.wait p{color:var(--muted)}
/* A command's last page in the browser: what was done, its mark with a check drawn on, nothing to press. (The page's own: a set-up step that is done is called the same.) */
body>.done{display:grid;place-items:center;min-height:100vh;padding:24px;text-align:center}body>.done>div{display:grid;justify-items:center;max-width:420px}
.done-mark{position:relative;display:grid;place-items:center;width:84px;height:84px;margin-bottom:26px;border-radius:24px;background:var(--card);border:1px solid var(--rule);box-shadow:var(--shadow);animation:done-in .5s cubic-bezier(.2,.9,.3,1.2) both}
.done-mark>.logo{border-radius:12px}.done-mark>.mark{width:40px;height:40px}
.done-check{position:absolute;right:-9px;bottom:-9px;display:grid;place-items:center;width:30px;height:30px;border-radius:50%;background:var(--good-fill);color:#fff;border:3px solid var(--bg);animation:done-in .4s .25s cubic-bezier(.2,.9,.3,1.4) both}
.done-check svg{width:15px;height:15px;fill:none;stroke:currentColor;stroke-width:3.2;stroke-linecap:round;stroke-linejoin:round;stroke-dasharray:22;stroke-dashoffset:22;animation:done-draw .35s .45s ease-out forwards}
body>.done h1{font-size:28px;letter-spacing:-.035em;margin-bottom:10px}body>.done p{max-width:32ch;color:var(--muted);font-size:15.5px;line-height:1.5}body>.done .done-foot{margin-top:30px;color:var(--faint);font:500 12px/1.4 var(--mono)}.done-go{display:grid;gap:10px;justify-items:center;margin-top:26px}
@keyframes done-in{from{opacity:0;transform:scale(.8)}}@keyframes done-draw{to{stroke-dashoffset:0}}
@media (prefers-reduced-motion:reduce){.done-mark,.done-check{animation:none}.done-check svg{animation:none;stroke-dashoffset:0}}
.spinner{width:26px;height:26px;margin:22px auto 0;border:3px solid var(--rule);border-top-color:var(--accent);border-radius:50%;animation:spin .8s linear infinite}@keyframes spin{to{transform:rotate(360deg)}}@media (prefers-reduced-motion:reduce){.spinner{animation:none}}
.sk{height:14px;margin:10px 0;border-radius:7px;background:var(--sk) linear-gradient(90deg,transparent 30%,var(--sk-hi) 50%,transparent 70%) no-repeat;background-size:200% 100%;background-position:150% 0;animation:shimmer 1.8s ease-in-out infinite}.sk.w30{width:30%}.sk.w40{width:40%;height:18px}.sk.w70{width:70%}
@keyframes shimmer{from{background-position:150% 0}to{background-position:-50% 0}}@media (prefers-reduced-motion:reduce){.sk{animation:none}}
table.list{display:table;width:100%;border-collapse:separate;border-spacing:0;font-size:13px}table.list th{padding:0 12px 8px;color:var(--muted);font-size:11.5px;font-weight:600;text-align:left}
table.list td{padding:14px 12px;border-top:1px solid var(--rule-soft);vertical-align:middle}table.list td:last-child{text-align:right;white-space:nowrap}table.list tr:hover td{background:var(--card-2)}
@media (max-width:680px){table.list thead{display:none}table.list td{display:block;border:0;padding:6px 12px}table.list td:last-child{text-align:left}table.list tr{display:block;border-top:1px solid var(--rule-soft);padding:8px 0}}
.logo{display:inline-grid;place-items:center;flex:0 0 auto;border-radius:9px;vertical-align:middle}.logo svg{display:block}
.ic{width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round;flex:0 0 18px}
.prov{display:flex;align-items:center;gap:12px;font-weight:600;font-size:13.5px;white-space:nowrap}.logos{display:inline-flex;gap:6px;vertical-align:middle}
.empty-state{display:grid;justify-items:center;text-align:center;gap:10px;padding:34px 16px}.empty-state .empty-state .tile .ic{width:24px;height:24px;flex-basis:24px}.empty-state h3{font-size:16px;letter-spacing:-.02em}.empty-state p{max-width:460px;color:var(--muted)}.empty-state .actions{display:flex;gap:10px;flex-wrap:wrap;justify-content:center;align-items:center;margin-top:6px}.empty-state .actions form{display:flex}.empty-state .more{margin-top:4px;font-size:13px}
.steps{display:grid}.step{display:grid;grid-template-columns:32px minmax(0,1fr);gap:14px;padding:18px 0;border-top:1px solid var(--rule-soft)}.step:first-child{border-top:0;padding-top:4px}
.num{display:grid;place-items:center;width:28px;height:28px;border-radius:50%;background:var(--accent-soft);color:var(--accent);font:600 12.5px var(--mono)}.step.done .num{background:var(--good-soft);color:var(--good)}.step.locked .num{background:var(--card-2);color:var(--faint)}
.step h3{font-size:15px;letter-spacing:-.02em;display:flex;align-items:center;gap:10px;flex-wrap:wrap}.step p{color:var(--muted);margin-top:6px;max-width:640px}.step .do{margin-top:12px;display:flex;gap:10px;flex-wrap:wrap;align-items:center}
.step.locked h3,.step.locked p{color:var(--faint)}.locked-note{display:inline-flex;align-items:center;gap:6px;color:var(--faint);font-size:12.5px;margin-top:10px}.locked-note .ic{width:15px;height:15px;flex-basis:15px}
.preview{margin-top:14px;display:grid;grid-template-columns:repeat(auto-fit,minmax(240px,1fr));gap:12px}.preview>div{padding:14px 16px;border-radius:12px;background:var(--card-2)}.preview h4{margin:0 0 8px;font-size:11px;font:600 11px var(--mono);letter-spacing:.12em;text-transform:uppercase;color:var(--faint)}
pre.snippet{margin:0;font:12.5px/1.6 var(--mono);white-space:pre;color:var(--ink);overflow-x:auto}pre.snippet .hl{color:var(--accent);font-weight:600}.preview ol{margin:0;padding-left:18px;color:var(--muted);font-size:13px;display:grid;gap:4px}
tr.soon td{color:var(--faint)}tr.soon .logo{filter:grayscale(1);opacity:.55}
.empty-state{display:grid;justify-items:center;text-align:center;gap:10px;padding:34px 16px}.empty-state .tile{display:flex;align-items:center;justify-content:center;gap:8px;min-width:56px;padding:0 10px;height:56px;border-radius:16px;background:var(--card-2);color:var(--muted)}
.empty-state .tile .ic{width:24px;height:24px;flex-basis:24px}.empty-state h3{font-size:16px;letter-spacing:-.02em}.empty-state p{max-width:460px;color:var(--muted)}.empty-state .actions{display:flex;gap:10px;flex-wrap:wrap;justify-content:center;align-items:center;margin-top:6px}.empty-state .actions form{display:flex}.empty-state .more{margin-top:4px;font-size:13px}
.gate{position:relative;min-height:calc(100vh - 24px)}.gate-bg{padding-top:18px;filter:blur(3px);opacity:.45;pointer-events:none;user-select:none}.gate-bg .page-title{margin:2px 0 26px}.gate-bg .sk{animation:none}
.gate-front{position:absolute;inset:0;display:grid;justify-items:center;align-content:start;padding:clamp(24px,9vh,96px) 0 48px}
.gate-panel{width:min(456px,100%);display:grid;gap:16px;padding:26px 24px 20px;border:1px solid var(--rule-soft);border-radius:20px;background:var(--card);box-shadow:0 1px 2px rgba(16,20,28,.06),0 28px 70px rgba(16,20,28,.16)}
:root[data-theme=dark] .gate-panel{box-shadow:0 1px 2px rgba(0,0,0,.4),0 28px 70px rgba(0,0,0,.55)}
.gate-panel h2{font-size:22px;letter-spacing:-.035em}.gate-panel .lede{margin-top:7px;color:var(--muted);font-size:13.5px}
.gate-back{justify-self:start;font-size:13px;font-weight:550;margin-bottom:-4px}.gate-foot{padding-top:14px;border-top:1px solid var(--rule-soft);color:var(--faint);font-size:12.5px}
.gated .side .nav a:not([href*="p=settings"]){opacity:.4;pointer-events:none}.listing .side>.nav{visibility:hidden}
/* The name at the top of the sidebar is the switcher between control planes. */
.brand-menu{display:block;padding:0}.brand-menu>summary{display:flex;align-items:center;gap:8px;padding:6px 8px;border-radius:10px}.brand-menu>summary>span{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.brand-menu.has>summary:hover,.brand-menu[open]>summary{background:var(--card)}.brand-menu:not(.has)>summary{pointer-events:none}.brand-menu:not(.has) .chev{display:none}
.brand-menu>summary .mark{width:22px;height:22px;fill:currentColor;stroke:none}.brand-menu>summary img.mark{border-radius:6px;object-fit:cover}.brand-menu>summary svg.mark[hidden]{display:none}.brand-menu>summary .chev{width:16px;height:16px;color:var(--faint)}
.brand-menu .menu-pop{left:0;right:auto;min-width:248px;font-weight:500;letter-spacing:-.015em}
.menu-pop a{display:grid;gap:1px;padding:8px 11px;border-radius:8px;font-size:13.5px;color:var(--ink);text-decoration:none}.menu-pop a small{color:var(--faint);font-size:12px;font-weight:400}.menu-pop a:hover,.menu-pop a[aria-current=true]{background:var(--card-2)}.menu-pop a.space{grid-template-columns:28px minmax(0,1fr);column-gap:10px;align-items:center}.menu-pop a.space>.logo{grid-row:1/3;border-radius:7px}.menu-pop a.space>span:not(.logo),.menu-pop a.space>small{grid-column:2;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.menu-pop a.space[aria-current=true]>span:not(.logo)::after{content:"✓";float:right;color:var(--accent);font-weight:600}
img.logo{object-fit:cover;background:var(--card-2)}.dlg .limit-do{display:flex;gap:8px;justify-content:flex-end}.dlg .limit-do input:not([type=hidden]){flex:1;min-width:0;height:36px}.dlg .limit-do .button{height:36px;padding:0 16px}.secret-form{display:grid;gap:12px}.secret-form label{display:grid;gap:5px;font-size:12.5px;color:var(--muted)}.secret-form input,.secret-form textarea{height:36px;border:1px solid var(--rule);border-radius:8px;background:var(--card);color:var(--ink);padding:0 10px;font:inherit;font-size:13px}.secret-form textarea{height:auto;padding:8px 10px;resize:vertical;font-family:var(--mono,ui-monospace,monospace)}.secret-form details{display:grid;gap:10px}.secret-form details[open]>summary{margin-bottom:10px}.secret-form details label+label{margin-top:10px}.secret-form summary{cursor:pointer;font-size:12.5px;color:var(--muted)}.secret-form .button{justify-self:end;height:36px;padding:0 16px}
.acct{border:1px solid var(--rule);border-radius:14px;background:var(--card)}.acct-do{display:flex;align-items:center;gap:10px}.acct-head{display:grid;grid-template-columns:36px minmax(0,1fr) auto;gap:14px;align-items:center;padding:14px 16px 14px 14px}.acct-head strong{display:block;font-size:14.5px;font-weight:600;letter-spacing:-.02em}.acct-head small{display:block;margin-top:2px;color:var(--muted);font-size:12.5px;line-height:1.45}
.acct-facts{margin:0;border-top:1px solid var(--rule-soft)}.acct-facts>div{display:grid;grid-template-columns:170px minmax(0,1fr) auto;gap:16px;align-items:center;padding:12px 16px;border-top:1px solid var(--rule-soft)}.acct-facts>div:first-child{border-top:0}.acct-facts dt{color:var(--muted);font-size:13px}.acct-facts dd{margin:0;font-size:13.5px;font-weight:500}.acct-facts dd small{display:block;margin-top:2px;color:var(--muted);font-size:12.5px;font-weight:400;line-height:1.45}
.acct-regions{list-style:none;margin:0;padding:0;display:grid;gap:8px}.acct-regions li{display:grid;grid-template-columns:150px minmax(0,1fr) auto auto;gap:12px;align-items:center;min-height:30px}.acct-facts dd:has(.acct-regions){grid-column:2/-1}.acct-facts dd small a{color:inherit;text-decoration:underline}.acct-regions .ar-name small{display:inline;margin:0 0 0 6px}.acct-regions .ar-use{color:var(--muted);font-weight:400}.acct-regions .ar-cpu{color:var(--muted);font-weight:400;font-variant-numeric:tabular-nums;white-space:nowrap}
@media (max-width:680px){.acct-facts>div{grid-template-columns:1fr;gap:4px}.acct-facts dd:has(.acct-regions){grid-column:auto}.acct-regions li{grid-template-columns:1fr auto;gap:2px 12px}.acct-regions .ar-use{grid-column:1/-1;grid-row:2}}
.logo.initial{background:var(--card-2);color:var(--muted);font-weight:600;letter-spacing:0;line-height:1}.menu-pop a.space[aria-current=true]>.logo.initial,.menu-pop a.space:hover>.logo.initial{background:var(--card)}
.spaces{display:grid;place-items:center;min-height:100vh;padding:24px}.spaces>div{display:grid;gap:14px;width:min(460px,100%)}.spaces .mark{width:30px;height:30px}.spaces h1{font-size:26px;letter-spacing:-.035em;margin-top:6px}.spaces p{color:var(--muted);font-size:14.5px;line-height:1.5;margin-bottom:6px}.spaces .picks{display:grid;gap:8px}.spaces .button{justify-self:start;margin-top:6px}.menu-pop hr{width:100%;border:0;border-top:1px solid var(--rule-soft);margin:4px 0}
.side-foot{display:grid;gap:12px;margin-top:auto;padding-top:20px}.side-foot .nav{margin-top:0}#side-update:empty{display:none}
.side-update{display:grid;gap:6px;padding:12px;border:1px solid var(--rule-soft);border-radius:12px;background:var(--card);box-shadow:var(--shadow)}
.side-update strong{display:flex;align-items:center;gap:8px;font-size:12.5px;letter-spacing:-.01em}.side-update small{color:var(--muted);font-size:11.5px;line-height:1.45}
.side-update small a{color:var(--accent);font-weight:550}.side-update form{margin-top:4px}
.release{display:grid;gap:8px;padding:4px 0}.release+.release{padding-top:14px;border-top:1px solid var(--rule-soft)}.release-head{display:flex;align-items:center;gap:10px;flex-wrap:wrap}.release-head strong{font-size:14px}
.release ul{margin:0;padding-left:18px;display:grid;gap:5px;color:var(--ink);font-size:13px;line-height:1.5}.release li::marker{color:var(--faint)}.release code{font:12px var(--mono)}.side-update .button{width:100%;justify-content:center}.side-update .button:disabled{opacity:.7;cursor:progress}
.picks{display:grid;gap:8px}
.pick{display:grid;grid-template-columns:36px minmax(0,1fr) auto;align-items:center;gap:14px;padding:12px 14px;border:1px solid var(--rule);border-radius:14px;background-color:var(--card);color:var(--ink);transition:border-color .15s,box-shadow .15s,transform .18s cubic-bezier(.2,.7,.3,1)}
a.pick:hover,summary.pick:hover{border-color:var(--faint);box-shadow:0 1px 2px rgba(16,20,28,.05),0 10px 24px rgba(16,20,28,.08);transform:translateY(-1px)}
.pick strong{display:flex;align-items:center;gap:8px;font-size:14.5px;font-weight:600;letter-spacing:-.02em}.pick small{display:block;margin-top:2px;color:var(--muted);font-size:12.5px;line-height:1.35}
.pick-end{display:flex;align-items:center}.pick-end .ic{width:16px;height:16px;flex-basis:16px;stroke:var(--faint);transition:transform .2s}
.pick .last{padding:2px 8px;border:1px solid var(--accent);border-radius:999px;color:var(--accent);font-size:11px;font-weight:600;letter-spacing:0;white-space:nowrap}
.pick.last-used{border-color:var(--accent);background-color:var(--pick-last)}.pick.last-used .pick-end .ic{stroke:var(--accent)}
details.more summary{list-style:none;cursor:pointer}details.more summary::-webkit-details-marker{display:none}details.more[open] summary{margin-bottom:8px}details.more[open] .chev{transform:rotate(180deg)}
.stack{display:grid;grid-template-columns:repeat(2,16px);gap:3px;place-content:center;width:36px;height:36px;border-radius:10px;background:var(--card-2)}.stack .logo{border-radius:4px}
.pick.soon{border-style:dashed;background-color:transparent}.pick.soon strong{color:var(--muted)}.pick.soon small{color:var(--faint)}.pick.soon .logo{filter:grayscale(.6);opacity:.7}
.soon-tag{padding:2px 9px;border:1px solid var(--rule);border-radius:999px;color:var(--faint);font-size:11px;font-weight:600}
.button.sm{height:32px;padding:0 12px;font-size:12.5px}.pick-end form{display:flex;gap:8px;align-items:center}.pick-end select{height:32px;padding:0 8px;font-size:12.5px}.pick-end{gap:8px}.picks.page{max-width:760px}
.first{display:flex;align-items:center;justify-content:space-between;gap:16px;padding:16px 2px 2px;border-top:1px solid var(--rule-soft)}.first strong{display:block;font-size:14px;font-weight:600;letter-spacing:-.02em}.first small{display:block;margin-top:2px;color:var(--muted);font-size:12.5px;line-height:1.4}
.then{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:18px;padding:18px 0 4px;border-top:1px solid var(--rule-soft)}.then-step{display:grid;grid-template-columns:28px minmax(0,1fr);gap:12px;align-items:start}
.then-step .num{background:var(--card-2);color:var(--faint)}.then-step h3{font-size:14px;letter-spacing:-.02em;line-height:1.3;color:var(--muted)}.then-step p{margin-top:3px;color:var(--faint);font-size:13px}
.deploy-opt-head{display:flex;align-items:center;gap:12px}.deploy-opt-head>span:last-child{display:grid;min-width:0}.deploy-opt-head strong{font-size:15px;font-weight:600;letter-spacing:-.02em}
.deploy-opt-head small{color:var(--muted);font-size:12.5px;overflow-wrap:anywhere}.deploy-opt .eyebrow{margin:0 0 8px}.deploy-opt .adds{font-size:13.5px}
.deploy{max-width:760px;gap:20px}.deploy-head{display:flex;gap:16px;align-items:center}.deploy-head h2{font-size:19px;letter-spacing:-.03em}.deploy-head .note{margin-top:4px;overflow-wrap:anywhere}.deploy.stopped .deploy-head h2{color:var(--bad)}
.kpi.is-empty .kpi-num{color:var(--faint)}.empty-snippet{text-align:left;padding:12px 16px;border-radius:10px;background:var(--card-2)}
.eyebrow{margin:3px 0 9px;font:600 10.5px/1 var(--mono);letter-spacing:.14em;text-transform:uppercase;color:var(--faint)}
.adds{list-style:none;margin:0;padding:0;display:grid;gap:6px}.adds li::before{content:"+";display:inline-block;width:16px;color:var(--good);font-weight:700}
.radios{display:flex;flex-wrap:wrap;gap:6px}.radios label{position:relative;display:inline-flex}.radios input{position:absolute;opacity:0;pointer-events:none}
.radios span{display:inline-flex;align-items:center;height:32px;padding:0 12px;border:1px solid var(--rule);border-radius:9px;background:var(--card);cursor:pointer}.radios input:checked+span{border-color:var(--accent);background:var(--accent-soft)}.radios input:focus-visible+span{outline:2px solid var(--accent);outline-offset:2px}
.deploy-do{display:flex;align-items:center;gap:14px;flex-wrap:wrap}.step .deploy-alt{margin-top:14px;font-size:13px}
.progress{list-style:none;margin:0;padding:18px 0 0;border-top:1px solid var(--rule-soft);display:grid;gap:13px}.progress li{display:flex;align-items:center;gap:12px;color:var(--faint);font-size:13.5px}.progress li.done,.progress li.now,.progress li.failed{color:var(--ink)}
.tick{display:grid;place-items:center;flex:0 0 22px;width:22px;height:22px;border:1.5px solid var(--rule);border-radius:50%;font-size:12px;font-weight:700}.tick .ic{width:13px;height:13px;flex-basis:13px}
.progress li.done .tick{border:0;background:var(--good-soft);color:var(--good)}.progress li.now .tick{border:2px solid var(--rule);border-top-color:var(--accent);animation:spin .8s linear infinite}.progress li.failed .tick{border:0;background:var(--bad-soft);color:var(--bad)}

[data-refresh]{display:none}
.figures .sk{display:block}.sk.fig{display:block;width:56%;height:34px;margin:0 0 10px;border-radius:9px}.sk.thin{height:10px;margin:6px 0 0}.sk.pillish{width:64px;height:22px;margin:0;border-radius:999px}
.row.sk-row .sk{display:block}.row.sk-row .sk.w40{margin:2px 0 0}.row.sk-row:hover{background:none}
.loading-note{display:inline-flex;align-items:center;gap:8px;color:var(--faint);font-size:12.5px;opacity:0;animation:appear .3s ease 3s forwards}@keyframes appear{to{opacity:1}}
.waiting-for{display:inline-flex;align-items:center;gap:8px;color:var(--muted);font-size:13px}.waiting-for .mini-spin{flex:0 0 auto}
.mini-spin{flex:0 0 auto;box-sizing:border-box;width:12px;height:12px;border:2px solid var(--rule);border-top-color:var(--accent);border-radius:50%;animation:spin .8s linear infinite}
@media (prefers-reduced-motion:reduce){.mini-spin{animation:none}.loading-note{animation:none;opacity:1}}@media (prefers-reduced-motion:reduce){.progress li.now .tick{animation:none;border-top-color:var(--rule)}}
@media (max-width:760px){.then{grid-template-columns:minmax(0,1fr);gap:12px}}
@media (max-width:560px){.gate-panel{padding:22px 18px 18px}.first{flex-direction:column;align-items:stretch}.pick.act{grid-template-columns:36px minmax(0,1fr)}.pick.act .pick-end{grid-column:2;flex-wrap:wrap}}
details summary{cursor:pointer;color:var(--muted);font-size:12.5px}details[open] summary{margin-bottom:10px}
table{border-collapse:collapse;font-size:12.5px}td{padding:4px 12px 4px 0}
/* Runners: the places in order, the default machine, labels. */
input[type=number]{height:32px;width:78px;padding:0 8px;border:1px solid var(--rule);border-radius:9px;background:var(--card);color:var(--ink);font-variant-numeric:tabular-nums}
.waiting{display:flex;flex-wrap:wrap;align-items:center;gap:8px;padding:10px 12px;border-radius:10px;background:var(--open-soft);color:var(--open-ink);font-size:12.5px}.waiting .pill{background:var(--card)}
.check{display:flex;align-items:center;gap:8px;height:36px;font-size:12.5px;color:var(--ink)}.check input{width:16px;height:16px;accent-color:var(--accent)}
.note.warn{color:var(--open-ink)}
.field{display:grid;gap:6px;font-size:12px;color:var(--muted);font-weight:500}.field-more{font-size:12px}.field-more summary{cursor:pointer;color:var(--muted);font-weight:500;margin-bottom:6px}.field-more textarea{width:100%;box-sizing:border-box;font:12px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;padding:8px 10px;border:1px solid var(--rule);border-radius:10px;background:var(--card);color:var(--ink);resize:vertical}.field select{min-width:110px}.field small{font-weight:400;font-size:11.5px;color:var(--faint);line-height:1.45}
/* Runner providers: compact rows, dragged into order; each with a settings dialog. */
.pools{list-style:none;margin:0;padding:0;display:grid;border:1px solid var(--rule-soft);border-radius:12px;overflow:hidden}
.pool{display:grid;grid-template-columns:16px 32px minmax(0,1fr) auto 32px;gap:12px;align-items:center;padding:10px 10px 10px 8px;background:var(--card);cursor:grab}.pool+.pool{border-top:1px solid var(--rule-soft)}
.pool:hover .grip{color:var(--muted)}.pool.dragging{opacity:.55;background:var(--card-2)}.order-note{margin-left:auto;display:inline-flex;align-items:center;gap:7px;font-size:12.5px;color:var(--muted)}.order-note.saved{color:var(--good)}.order-note.bad{color:var(--bad)}.order-note svg{width:15px;height:15px;fill:none;stroke:currentColor;stroke-width:2.4;stroke-linecap:round;stroke-linejoin:round}.pool{transition:box-shadow .3s}
.pool.still{cursor:default;grid-template-columns:32px minmax(0,1fr) auto 32px;padding-left:12px}.pool.moved{box-shadow:inset 3px 0 0 var(--accent)}
.grip{display:grid;place-items:center;color:var(--faint)}.grip svg{width:16px;height:16px;fill:currentColor}
.pool-main{display:grid;gap:2px;min-width:0}.pool-main strong{font-size:13.5px;letter-spacing:-.015em}.pool-main small{color:var(--muted);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.tag{margin-left:6px;padding:1px 6px;border-radius:6px;background:var(--card-2);color:var(--muted);font:500 10.5px/1.5 var(--mono);vertical-align:1px}
.pool-now{display:flex;align-items:center;gap:8px;flex-wrap:wrap;justify-content:flex-end}.pool-now .pill{height:22px;font-size:11.5px;padding:0 9px}
.lim{padding:2px 8px;border:1px solid var(--rule);border-radius:999px;color:var(--muted);font-size:11.5px;white-space:nowrap;font-variant-numeric:tabular-nums}
.icon-btn{display:grid;place-items:center;width:32px;height:32px;border-radius:9px;color:var(--muted);cursor:pointer}.icon-btn:hover{background:var(--card-2);color:var(--ink)}
.icon-btn svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}
.dlg{width:min(440px,calc(100vw - 32px));padding:0;border:1px solid var(--rule-soft);border-radius:16px;background:var(--card);color:var(--ink);box-shadow:0 24px 60px rgba(16,20,28,.25)}.dlg::backdrop{background:rgba(16,20,28,.35)}
.dlg-body{display:grid;gap:16px;padding:20px}.dlg-head{display:grid;grid-template-columns:36px minmax(0,1fr) 32px;gap:12px;align-items:center}.dlg-head span{display:grid;gap:2px;min-width:0}.dlg-head strong{font-size:15px}.dlg-head small{color:var(--muted);font-size:12px}
.dlg input[type=number]{width:140px;height:36px}.money{display:flex;align-items:center;gap:6px;color:var(--ink);font-size:13px}.dlg-foot{display:flex;justify-content:flex-end;gap:8px}
.dlg-danger{display:flex;align-items:center;justify-content:space-between;gap:12px;padding:14px 20px;border-top:1px solid var(--rule-soft);background:var(--card-2)}.dlg-danger span{display:grid;gap:2px}.dlg-danger strong{font-size:13px}.dlg-danger small{color:var(--muted);font-size:11.5px}
/* supercov's prompt dialog (Workflows → Switch workflows), as it is there. */
dialog.report-modal{--ink:#f7f7fa;--muted:#bdc2ce;--faint:#abb2c1;--rule-soft:rgba(255,255,255,.12);--card:#242830;--card-2:rgba(255,255,255,.065);color-scheme:dark;margin:auto;color:var(--ink)}
dialog.prompt-modal::backdrop{background:rgba(8,10,14,.5);-webkit-backdrop-filter:blur(3px);backdrop-filter:blur(3px)}
.prompt-modal .sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap;border:0}
.head-actions{display:flex;align-items:center;gap:10px;flex-wrap:wrap;justify-content:flex-end}
.jmark{display:inline-grid;place-items:center}
.subtle-button{cursor:pointer;font-family:inherit;gap:6px}
.subtle-button { display: inline-flex; align-items: center; justify-content: center; min-height: 30px; padding: 5px 12px; border: 1px solid var(--rule); border-radius: 999px; background: var(--card); color: var(--ink); font-size: 12px; font-weight: 500; box-shadow: 0 1px 2px rgba(0,0,0,.03); }
.subtle-button:hover { color: var(--ink); background: var(--card-2); border-color: var(--faint); transform: none; }
.improve-button { gap: 6px; padding-inline: 10px; white-space: nowrap; }
.agent-marks { display: inline-flex; align-items: center; isolation: isolate; }
.agent-marks .jmark { width: 18px; height: 18px; padding: 1px; border-radius: 50%; background: var(--card); }
.agent-marks .jmark + .jmark { margin-left: -5px; }
.agent-marks svg { display: block; width: 16px; height: 16px; }
.agent-marks .agent-codex svg { fill: currentColor; }
.improve-button:hover .agent-marks .jmark { background: var(--card-2); }
.improve-prompt { max-height: 45vh; overflow-y: auto; }
.report-modal.prompt-modal{width:min(100%,430px);max-height:calc(100dvh - 36px);padding:20px;position:relative;overflow-y:auto;overscroll-behavior:contain;border:1px solid rgba(255,255,255,.24);border-radius:34px;background:linear-gradient(145deg,rgba(255,255,255,.09),transparent 32%),radial-gradient(circle at 8% 12%,rgba(255,176,92,.13),transparent 31%),radial-gradient(circle at 92% 18%,rgba(105,151,255,.17),transparent 33%),radial-gradient(circle at 78% 92%,rgba(74,204,188,.12),transparent 31%),rgba(12,14,19,.82);box-shadow:0 36px 100px rgba(0,0,0,.42),inset 0 1px 0 rgba(255,255,255,.2),inset 0 -1px 0 rgba(255,255,255,.06);-webkit-backdrop-filter:blur(28px) saturate(180%) brightness(.78);backdrop-filter:blur(28px) saturate(180%) brightness(.78);color:#f7f7fa;opacity:1;transition:opacity .2s ease,transform .26s cubic-bezier(.2,.78,.25,1);scrollbar-width:thin;scrollbar-color:rgba(255,255,255,.2) transparent}
.prompt-modal-close{appearance:none;width:36px;height:36px;padding:0;position:absolute;right:14px;top:14px;z-index:2;display:grid;place-items:center;border:1px solid rgba(255,255,255,.14);border-radius:50%;background:rgba(255,255,255,.09);box-shadow:inset 0 1px 0 rgba(255,255,255,.1);color:#fff;cursor:pointer;transition:background .18s ease,transform .18s ease}
.prompt-modal-close:hover{background:rgba(255,255,255,.16)}
.prompt-modal-close:active{transform:scale(.94)}
.prompt-modal-close svg{width:18px;height:18px;fill:none;stroke:currentColor;stroke-width:1.7;stroke-linecap:round}
.prompt-modal-step{margin-top:24px}
.prompt-modal-step-first{margin-top:4px}
.prompt-modal-step-head{min-height:32px;padding-right:45px;display:grid;grid-template-columns:32px minmax(0,1fr);align-items:center;gap:12px}
.prompt-modal-step-head>span{width:30px;height:30px;display:grid;place-items:center;border:1px solid rgba(255,255,255,.16);border-radius:50%;background:rgba(255,255,255,.08);color:rgba(255,255,255,.83);font-size:.76rem;font-weight:520}
.prompt-modal-step-head h2{margin:0;color:#fff;font-size:1rem;font-weight:610;line-height:1.2;letter-spacing:-.025em}
.prompt-modal-choices{margin:12px 0 0 44px;padding:0;overflow:hidden;border:1px solid rgba(255,255,255,.12);border-radius:21px;background:rgba(255,255,255,.055);box-shadow:inset 0 1px 0 rgba(255,255,255,.045)}
.prompt-modal-choice{appearance:none;width:100%;min-height:76px;padding:14px 16px;display:grid;grid-template-columns:18px minmax(0,1fr);align-items:center;gap:12px;border:0;background:transparent;color:inherit;text-align:left;cursor:pointer;transition:background .18s ease}
.prompt-modal-choice+.prompt-modal-choice{border-top:1px solid rgba(255,255,255,.1)}
.prompt-modal-choice:hover{background:rgba(255,255,255,.045)}
.prompt-modal-choice.is-selected{background:rgba(255,255,255,.095)}
.prompt-modal-choice>input{appearance:none;width:16px;height:16px;margin:0;border:1.5px solid rgba(255,255,255,.42);border-radius:50%;background:transparent}
.prompt-modal-choice>input:checked{background:radial-gradient(circle,#fff 0 4px,transparent 4.5px)}
.prompt-modal-choice>input:focus-visible{outline:2px solid #2997ff;outline-offset:3px}
.prompt-modal-choice>span{display:grid;gap:4px}
.prompt-modal-choice strong{color:#fff;font-size:.91rem;font-weight:590;line-height:1.2}
.prompt-modal-choice small{color:rgba(235,235,241,.68);font-size:.73rem;line-height:1.35}
.prompt-modal-copy-card{margin:12px 0 0 44px;padding:15px;border:1px solid rgba(255,255,255,.12);border-radius:21px;background:rgba(255,255,255,.055);box-shadow:inset 0 1px 0 rgba(255,255,255,.045)}
.prompt-modal-preview{margin:0 0 13px;color:rgba(247,247,250,.9);font-size:.79rem;line-height:1.46;letter-spacing:-.012em}
.prompt-modal-preview code{border:0;background:linear-gradient(100deg,#ff8a39 3%,#e84474 30%,#8366ee 56%,#168bd8 78%,#09a982);color:transparent;font:inherit;font-weight:650;-webkit-background-clip:text;background-clip:text}
.prompt-modal-copy{appearance:none;width:100%;min-height:42px;padding:0 16px;display:flex;align-items:center;justify-content:center;gap:8px;border:0;border-radius:999px;background:#0a84ff;box-shadow:0 10px 24px rgba(0,104,225,.24),inset 0 1px 0 rgba(255,255,255,.28);color:#fff;font:inherit;font-size:.78rem;font-weight:620;cursor:pointer;transition:background .18s ease,transform .18s ease}
.prompt-modal-copy:hover{background:#188cff}
.prompt-modal-copy:active{transform:scale(.985)}
.prompt-modal-copy.is-copied{background:#22a868}
.prompt-modal-copy svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:1.7;stroke-linecap:round;stroke-linejoin:round}
.prompt-modal-instruction{margin:10px 0 0 44px;color:rgba(235,235,241,.68);font-size:.73rem;line-height:1.45}
.prompt-modal-agents{margin:14px 0 0 44px;display:grid;grid-template-columns:repeat(4,minmax(0,1fr));align-items:start;gap:12px}
.prompt-modal-agents>span{min-width:0;display:grid;justify-items:center;gap:7px;color:rgba(245,245,248,.7)}
.prompt-modal-agents img{width:28px;height:28px;object-fit:contain}
.prompt-modal-agents>span:nth-child(2) img,.prompt-modal-agents>span:nth-child(3) img{filter:brightness(0) invert(1)}
.prompt-modal-agents small{max-width:100%;font-size:.59rem;line-height:1.2;text-align:center;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.prompt-modal :focus-visible{outline:2px solid rgba(96,174,255,.92);outline-offset:3px}
.perm-needs{list-style:none;margin:0;padding:0;display:grid;gap:13px}
.perm-needs li{display:grid;gap:4px}
.perm-needs strong{display:flex;align-items:baseline;justify-content:space-between;gap:10px;color:#fff;font-size:.86rem;font-weight:590;line-height:1.25}
.perm-needs em{flex:none;padding:2px 8px;border-radius:999px;background:rgba(255,255,255,.1);color:rgba(235,235,241,.78);font-size:.62rem;font-style:normal;font-weight:560}
.perm-needs small{color:rgba(235,235,241,.68);font-size:.73rem;line-height:1.42}
.perm-needs small span{color:rgba(235,235,241,.42)}
.perm-action{margin:12px 0 0 44px;display:grid;gap:8px}
.perm-field{margin:12px 0 0 44px;display:grid;gap:7px}
.perm-field[hidden]{display:none}
.prompt-modal .perm-field input{width:100%;box-sizing:border-box;height:44px;padding:0 15px;border:1px solid rgba(255,255,255,.16);border-radius:15px;background:rgba(255,255,255,.07);box-shadow:inset 0 1px 0 rgba(255,255,255,.05);color:#fff;font:inherit;font-size:.86rem;letter-spacing:-.01em}
.prompt-modal .perm-field input::placeholder{color:rgba(235,235,241,.38)}
.prompt-modal .perm-field input:focus{outline:2px solid rgba(96,174,255,.92);outline-offset:1px;border-color:transparent}
.perm-field small{color:rgba(235,235,241,.6);font-size:.7rem;line-height:1.4}
.perm-modal form{margin:0}
.perm-action form{margin:0}
a.prompt-modal-copy{box-sizing:border-box;text-decoration:none}
.prompt-modal-copy.is-quiet{background:rgba(255,255,255,.12);box-shadow:inset 0 1px 0 rgba(255,255,255,.12)}
.prompt-modal-copy.is-quiet:hover{background:rgba(255,255,255,.18)}
@media (max-width:520px){
.report-modal.prompt-modal{max-height:calc(100dvh - 20px);padding:17px 16px 18px;border-radius:28px}
.prompt-modal-close{right:12px;top:12px;width:34px;height:34px}
.prompt-modal-step{margin-top:21px}
.prompt-modal-step-first{margin-top:3px}
.prompt-modal-step-head{grid-template-columns:30px minmax(0,1fr);gap:10px}
.prompt-modal-step-head>span{width:28px;height:28px}
.prompt-modal-choices,.prompt-modal-copy-card,.prompt-modal-instruction,.prompt-modal-agents{margin-left:40px}
.prompt-modal-choice{min-height:70px;padding:12px 13px}
.prompt-modal-choice small{font-size:.69rem}
.prompt-modal-agents{gap:7px}
.prompt-modal-agents img{width:25px;height:25px}}
.report-modal.prompt-modal { font-size: 16px; }
.prompt-modal > .report-modal-head { margin: 0; }
.prompt-modal .report-modal-head h2, .prompt-modal .sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; border: 0; }
.prompt-modal .prompt-modal-step-head h2 { font-size: 16px; }
.prompt-modal .prompt-modal-close { display: grid; flex: none; }
.prompt-modal .prompt-modal-close .jmark { display: contents; }
.prompt-modal .improve-prompt { max-height: none; overflow: visible; white-space: pre-wrap; overflow-wrap: anywhere; user-select: text; }
.prompt-modal .prompt-modal-copy-card .prompt-modal-copy { margin-top: 0; }
.prompt-copy-error { margin-top: 10px; color: #ffb16b; font-size: 12px; }
/* Its prompts are long: the preview scrolls in its card, so Copy stays in view. */
.prompt-modal .improve-prompt{max-height:min(300px,38vh);overflow-y:auto;overscroll-behavior:contain;padding-right:6px;scrollbar-width:thin;scrollbar-color:rgba(255,255,255,.2) transparent}
.dlg-signin{display:flex;align-items:center;justify-content:space-between;gap:12px;flex-wrap:wrap;margin:14px 0 2px;padding:12px 14px;border-radius:10px;background:var(--card-2)}
.button.danger,:root[data-theme=dark] .button.danger{color:var(--bad);background:var(--card);border:1px solid var(--rule)}.button.danger:hover{background:var(--bad-soft)}
input[type=number]{height:32px;padding:0 10px;border:1px solid var(--rule);border-radius:9px;background:var(--card);color:var(--ink);font-variant-numeric:tabular-nums}
/* Machines: pick one, get its label. */
.default-machine{display:grid;grid-template-columns:36px minmax(0,1fr) auto auto;gap:14px;align-items:center;padding:14px 14px 14px 12px;border:1px solid var(--rule-soft);border-radius:12px}
.dm-main{display:grid;gap:2px;min-width:0}.dm-main strong{font-size:13.5px;letter-spacing:-.015em}.dm-main small{color:var(--muted);font-size:12px}.dm-spec{display:flex;flex-wrap:wrap;gap:6px;justify-content:flex-end}
.picker-head{display:grid;gap:4px;padding-top:6px}.picker-head h3{font-size:13.5px;letter-spacing:-.01em}
.default-machine>form{display:contents}.default-machine>form>.button{grid-column:4}.default-machine>.button{grid-column:4}
@media (max-width:680px){.default-machine{grid-template-columns:36px minmax(0,1fr)}.dm-spec{grid-column:1/-1;justify-content:flex-start}.default-machine .button,.default-machine>form>.button{grid-column:1/-1;justify-self:start}}
.picks.compact .pick{padding:10px 12px}.picks.compact .logo{width:30px!important;height:30px!important}.steps .picks.compact{margin-top:10px}
details.connect{display:block}details.connect>summary{margin:0}details.connect>summary .button{pointer-events:none}details.connect[open]>summary{border-bottom-left-radius:0;border-bottom-right-radius:0}details.connect .gl-form{padding:16px 18px 18px;border:1px solid var(--rule-soft);border-top:0;border-radius:0 0 12px 12px;background:var(--card)}
.two-snippets{display:grid;grid-template-columns:repeat(auto-fit,minmax(260px,1fr));gap:12px}.snippet-head{display:flex;align-items:center;gap:8px;margin-bottom:8px;font-size:12.5px;font-weight:600}.snippet-head .logo{border-radius:5px}
/* Overview, after supercov's report: a quiet line at the top, big card titles, rings, a table to look at. */
.sep{color:var(--faint)}
.live{display:inline-flex;align-items:center;gap:6px;color:var(--good);font-weight:550}.live i{width:7px;height:7px;border-radius:50%;background:currentColor}.live.bad{color:var(--bad)}
.card.big>.card-head h2{font-size:22px;letter-spacing:-.025em}
.card-head .loading-note{margin-left:auto}
.status-line{display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-top:-6px;color:var(--muted);font-size:13.5px}
.status{display:inline-flex;align-items:center;gap:7px;font-weight:550;color:var(--muted)}.status svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:2.2;stroke-linecap:round;stroke-linejoin:round}.status.good{color:var(--good)}.status.open{color:var(--open-ink)}
.metrics{display:grid;grid-template-columns:repeat(auto-fit,minmax(230px,1fr));gap:20px 28px;padding:6px 0 4px}
.tile{display:grid;align-content:start;gap:4px;min-width:0;padding-right:8px}.tile .metric-cap{margin-top:2px}.tile .metric-sub{margin-top:6px}.tile .sk{display:block}.sk.block.small{height:40px;margin:8px 0}
.axis{display:flex;justify-content:space-between;color:var(--faint);font-size:11px}
.vs{display:grid;gap:6px;margin-top:10px}.vs>div{display:grid;grid-template-columns:52px minmax(0,1fr) 62px;gap:8px;align-items:center;font-size:12px;color:var(--muted)}.vs .mono{text-align:right}
.vs-bar{display:block;height:8px;border-radius:4px;background:var(--card-2);overflow:hidden}.vs-bar i{display:block;height:100%;border-radius:4px}.vs-bar .you{background:var(--accent)}.vs-bar .them{background:var(--faint)}
.seg{display:inline-flex;padding:3px;border-radius:999px;background:var(--card-2)}.seg input{position:absolute;opacity:0;pointer-events:none}.seg label{display:inline-flex;align-items:center;gap:6px;padding:6px 12px;border-radius:999px;font-size:12.5px;font-weight:600;color:var(--muted);cursor:pointer}.seg label .logo{border-radius:4px}
.seg input:checked+label{background:var(--card);color:var(--ink);box-shadow:0 1px 2px rgba(16,20,28,.08)}.seg input:focus-visible+label{outline:2px solid var(--accent)}
.seg-links{margin-left:auto}.seg-links a{display:inline-flex;align-items:center;padding:5px 11px;border-radius:999px;font-size:12px;font-weight:600;color:var(--muted);text-decoration:none}.seg-links a:hover{color:var(--ink)}.seg-links a.on{background:var(--card);color:var(--ink);box-shadow:0 1px 2px rgba(16,20,28,.08)}
.kpis.four{grid-template-columns:repeat(4,minmax(0,1fr))}@media (max-width:1100px){.kpis.four{grid-template-columns:repeat(2,minmax(0,1fr))}}@media (max-width:560px){.kpis.four{grid-template-columns:minmax(0,1fr)}}.good-text{color:var(--good);font-weight:600}
.use-steps{list-style:none;margin:0;padding:0;display:none;gap:14px}.card:has(#use-gh:checked) .use-steps.gh,.card:has(#use-gl:checked) .use-steps.gl,.use-steps.only{display:grid}
.use-steps li{display:grid;grid-template-columns:24px minmax(0,1fr);gap:12px;align-items:start;font-size:13.5px}.use-steps li .tick{margin-top:1px}.use-steps li.done .tick{background:var(--good-fill);border-color:var(--good-fill);color:#fff}.use-steps li.done{color:var(--muted)}
.use-steps pre.snippet{margin-top:8px}.use-steps a{color:var(--accent)}
.metric{display:flex;align-items:center;gap:16px;min-width:0}.metric>div{flex:1;min-width:0}.metric-big{font-size:30px;font-weight:650;letter-spacing:-.04em;line-height:1;font-variant-numeric:tabular-nums}
.metric-cap{margin-top:5px;font-size:13px;color:var(--ink)}.metric-sub{margin-top:3px;font-size:12px;color:var(--muted)}
.ring{flex:0 0 56px;width:56px;height:56px}.ring circle{fill:none;stroke-width:6}.ring .track{stroke:var(--card-2)}.ring .on{stroke:var(--faint);stroke-linecap:round}.ring.good .on{stroke:var(--good-fill)}.ring.open .on{stroke:var(--open)}.ring.accent .on{stroke:var(--accent)}
.metric-icon{flex:0 0 56px;display:grid;place-items:center;width:56px;height:56px;border-radius:50%;background:var(--card-2);color:var(--muted)}.metric-icon svg{width:22px;height:22px;fill:none;stroke:currentColor;stroke-width:2}
.sub-block{display:grid;gap:8px;padding-top:14px;border-top:1px solid var(--rule-soft)}.sub-block h3{font-size:14px;letter-spacing:-.01em}
.jt{display:grid}.jt-head,.jt-row{display:grid;grid-template-columns:minmax(0,1.5fr) minmax(0,1fr) 56px 64px 90px 64px;gap:16px;align-items:center;padding:11px 8px}
.jt-head{padding:2px 8px 8px;color:var(--faint);font-size:12px;font-weight:550;border-bottom:1px solid var(--rule-soft)}.jt-row{color:inherit;text-decoration:none;border-bottom:1px solid var(--rule-soft)}.jt-row:last-child{border-bottom:0}.jt-row:hover{background:var(--card-2)}
.jt-job{display:grid;gap:3px;min-width:0}.jt-job strong{font-size:13.5px;font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.jt-job small{display:flex;align-items:center;gap:6px;color:var(--faint);font-size:12px;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.jt-job small .logo{border-radius:4px}.jt-job small.jt-why{display:block;color:var(--bad)}.jt-job small.jt-why.open{color:var(--open-ink)}.jt-job small.jt-why.held{color:var(--faint)}.jt-runner.count{color:var(--muted);font-size:13px}a.tab{text-decoration:none}.vs .mono{font-family:var(--sans);font-variant-numeric:tabular-nums}.split{height:6px}
.jt-runner{display:flex;align-items:center;gap:8px;min-width:0;font-size:13px;color:var(--muted);white-space:nowrap;overflow:hidden}.jt-runner .logo{border-radius:5px}.jt-runner span{overflow:hidden;text-overflow:ellipsis}
.jt-num{text-align:right;font-size:13px;color:var(--muted);font-variant-numeric:tabular-nums;white-space:nowrap}.jt-when{text-align:right;font-size:12.5px;color:var(--faint);white-space:nowrap}.faint{color:var(--faint)}
.jt-state{display:inline-flex;align-items:center;gap:7px;font-size:13px;font-weight:550;color:var(--muted)}.jt-state i{width:7px;height:7px;border-radius:50%;background:var(--faint)}.jt-state.good{color:var(--good)}.jt-state.good i{background:var(--good-fill)}.jt-state.bad{color:var(--bad)}.jt-state.bad i{background:var(--bad-fill)}.jt-state.open{color:var(--open-ink)}.jt-state.open i{background:var(--open)}.jt-state.accent{color:var(--accent)}.jt-state.accent i{background:var(--accent)}
.issues{display:grid;gap:8px}.issue{display:grid;grid-template-columns:32px minmax(0,1fr) 16px;gap:12px;align-items:center;padding:10px 12px;border-radius:11px;background:var(--card-2);color:inherit;text-decoration:none}.issue:hover{background:var(--bg-deep)}
.issue-mark{display:grid;place-items:center;width:32px;height:32px;border-radius:9px}.issue-mark svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}.issue-mark.bad{background:var(--bad-soft);color:var(--bad)}.issue-mark.open{background:var(--open-soft);color:var(--open-ink)}
.issue-main{display:grid;gap:2px;min-width:0}.issue-main strong{font-size:13.5px;font-weight:600}.issue-main small{color:var(--muted);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.issue .chev{width:16px;height:16px;fill:none;stroke:var(--faint);stroke-width:2}
.split{display:flex;gap:3px;height:8px;border-radius:4px;overflow:hidden}.split i{display:block;height:100%}.split-legend{display:flex;flex-wrap:wrap;gap:8px 22px;font-size:13px}.split-item{display:inline-flex;align-items:center;gap:7px}.split-item>i{width:8px;height:8px;border-radius:2px}.split-item .faint{font-variant-numeric:tabular-nums}
.split .c0,.split-item .c0{background:var(--accent)}.split .c1,.split-item .c1{background:var(--good-fill)}.split .c2,.split-item .c2{background:var(--open)}.split .c3,.split-item .c3{background:var(--muted)}.split .c4,.split-item .c4{background:var(--faint)}
.more-link{color:var(--accent)}
@media (max-width:680px){.jt-head{display:none}.jt-row{grid-template-columns:minmax(0,1fr) auto;gap:6px 12px}.jt-runner{grid-column:1}.jt-num{display:none}.jt-state{grid-row:1;grid-column:2}.jt-when{grid-column:2;grid-row:2}}
.card-foot{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:8px;padding-top:12px;border-top:1px solid var(--rule-soft)}.more-link{font-size:13px;font-weight:550}
/* Skeletons in the shape of what comes. */
.sk.logo-sk{display:block;width:36px;height:36px;margin:0;border-radius:10px}.sk.ringish{display:block;flex:0 0 56px;width:56px;height:56px;margin:0;border-radius:50%}.sk.w60{width:60%}.sk.block{display:block;height:84px;margin:0;border-radius:10px}.sk.field-sk{display:block;width:120px;height:36px;margin:0;border-radius:10px}
.sk-pick .sk,.sk-pool .sk,.default-machine .sk,.metric .sk{display:block}.sk-pick .sk.w30,.sk-pool .sk.w30{margin:0 0 8px}.sk-pick .sk.thin,.sk-pool .sk.thin{margin:0}.sk-pick:hover{background:var(--card)}.sk-pool{cursor:default}
.gh-mark{position:relative;display:inline-flex;align-items:center;gap:6px;padding:3px 9px 3px 6px;border-radius:999px;background:var(--card-2);color:var(--muted);font-size:12.5px;font-weight:550;cursor:default;outline:none}.gh-mark .logo{border-radius:4px}.gh-mark:hover,.gh-mark:focus{color:var(--ink)}
.gh-pop{position:absolute;z-index:5;top:calc(100% + 8px);right:0;width:280px;display:none;gap:10px;padding:14px;border-radius:12px;background:var(--card);border:1px solid var(--rule);box-shadow:var(--shadow);color:var(--ink);font-weight:400;white-space:normal}.gh-mark:hover .gh-pop,.gh-mark:focus .gh-pop{display:grid}.gh-pop strong{font-size:13px}.gh-pop small{color:var(--muted);font-size:12px;line-height:1.45}
.first-jobs{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:12px 20px;padding:14px 16px;border-radius:12px;background:var(--card-2)}.first-jobs strong{font-size:14px}.first-jobs .note{margin:3px 0 0}
.status-line{display:flex;flex-wrap:wrap;align-items:center;gap:6px 8px}.status{white-space:nowrap}
.slim{display:block;height:4px;margin-top:7px;border-radius:2px;background:var(--rule-soft);overflow:hidden;max-width:320px}.slim i{display:block;height:100%;background:var(--accent);border-radius:2px;transition:width .4s}.bad-text{color:var(--bad)}
.move-what{display:grid;gap:8px;margin:4px 0 6px}.move-what>.note{font-size:12px;font-weight:600;letter-spacing:.04em;text-transform:uppercase;color:var(--faint);margin-top:6px}
ul.checks{list-style:none;margin:0;padding:0;display:grid;gap:7px}ul.checks li{display:flex;align-items:flex-start;gap:9px;font-size:13.5px}ul.checks li svg{flex:0 0 16px;width:16px;height:16px;margin-top:1px;fill:none;stroke:var(--good);stroke-width:2.4;stroke-linecap:round;stroke-linejoin:round}ul.checks.stays li{color:var(--muted)}
.stale-bar{position:sticky;top:12px;z-index:20;display:flex;align-items:center;gap:10px;margin:0 0 18px;padding:10px 14px;border:1px solid var(--rule);border-radius:12px;background:var(--card);box-shadow:var(--shadow);color:var(--muted);font-size:13px}.stale-bar strong{color:var(--ink);font-weight:600}.stale-bar code{font:500 12px var(--mono);color:var(--ink)}.stale-bar .dot{flex:0 0 auto;width:8px;height:8px;border-radius:50%;background:var(--open-ink)}.stale-bar .mini-spin{flex:0 0 auto}
body.stale #live>:not(.stale-bar){opacity:.45;filter:grayscale(.5);pointer-events:none;user-select:none;transition:opacity .3s,filter .3s}
.notice{display:flex;align-items:center;gap:10px;margin:0 0 16px;padding:11px 14px;border-radius:11px;background:var(--good-soft);color:var(--good);font-size:13.5px}.notice.warn{background:var(--open-soft);color:var(--open-ink)}.notice a{color:inherit;text-decoration:underline}.notice form{margin-left:auto;flex:none}.notice svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:2.4;stroke-linecap:round;stroke-linejoin:round}
.warn-mark{width:36px;height:36px;background:var(--bad-soft);color:var(--bad)}.warn-mark svg{width:19px;height:19px;fill:none;stroke:currentColor;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}
.pick.current{border-color:color-mix(in srgb,var(--accent) 45%,transparent);box-shadow:0 0 0 3px var(--accent-soft)}.pick.idle strong{color:var(--muted)}
details.menu{position:relative}details.menu>summary{list-style:none;cursor:pointer}details.menu[open]>summary{margin-bottom:0}details.menu>summary::-webkit-details-marker{display:none}details.menu>summary svg{width:18px;height:18px;fill:none;stroke:currentColor}details.menu>summary.button{display:inline-flex;align-items:center;gap:4px}details.menu>summary.button svg.chev{width:14px;height:14px;stroke-width:2;margin-right:-4px}details.menu[open]>summary.button svg.chev{transform:rotate(180deg)}
.menu-pop{position:absolute;z-index:20;right:0;top:calc(100% + 6px);min-width:220px;display:grid;padding:6px;border-radius:12px;background:var(--card);border:1px solid var(--rule);box-shadow:var(--shadow)}.menu-pop button{display:block;width:100%;text-align:left;padding:9px 11px;border:0;border-radius:8px;background:none;font:inherit;font-size:13.5px;color:var(--ink);cursor:pointer}.menu-pop button:hover{background:var(--card-2)}.menu-pop button.danger-item{color:var(--bad)}.menu-pop form{display:contents}
.upd{display:grid;gap:7px;min-width:190px}.upd-step{display:flex;align-items:flex-start;gap:8px;font-size:12.5px;line-height:1.45;color:var(--muted)}.upd-step .mini-spin{margin-top:3px}.upd .slim{margin-top:0}.upd form{margin-top:4px}
body.busy::before{content:"";position:fixed;z-index:50;top:0;left:0;height:2px;width:35%;background:var(--accent);animation:busy 1.1s ease-in-out infinite}@keyframes busy{0%{left:-35%}100%{left:100%}}@media (prefers-reduced-motion:reduce){body.busy::before{animation:none;width:100%;opacity:.5}}
.step-do{margin-top:14px}.step-note{margin:8px 0 0}.dm-card .default-machine{border:0;padding:0;background:none;box-shadow:none}.kept-labels{display:grid;border-top:1px solid var(--rule-soft)}.kept-label{display:flex;align-items:center;justify-content:space-between;gap:12px;padding:10px 0;border-bottom:1px solid var(--rule-soft);font-size:13px}.kept-label:last-child{border-bottom:0;padding-bottom:0}.kept-label small{margin-left:6px;font-size:12px}#labels .limit-do input,#name .limit-do input{flex:1;min-width:0}#labels .limit-do,#name .limit-do{display:flex;gap:8px}
.dm-card .default-machine+.default-machine{margin-top:16px;padding-top:16px;border-top:1px solid var(--rule-soft);border-radius:0}.dlg.wide{width:min(760px,calc(100vw - 32px));max-width:none}.dlg.wide .picker{display:grid;gap:14px}
.rg-list{list-style:none;margin:0;padding:0;display:grid;border:1px solid var(--rule-soft);border-radius:11px;overflow:hidden}.rg{display:grid;grid-template-columns:18px minmax(0,1fr) 32px;gap:10px;align-items:center;padding:8px 8px 8px 10px;background:var(--card);border-bottom:1px solid var(--rule-soft);cursor:grab}.rg:last-child{border-bottom:0}.rg.dragging{opacity:.55;background:var(--card-2)}.rg-main{display:grid;gap:1px;min-width:0}.rg-main strong{font-size:13.5px;font-weight:550}.rg-main small{color:var(--faint);font-size:12px}.rg-list .rg:only-child .icon-btn{visibility:hidden}.rg-add{margin-top:8px;width:100%}
.button.working{display:inline-flex;align-items:center;gap:8px;opacity:.85;cursor:progress}.button.working .mini-spin{border-color:rgba(255,255,255,.4);border-top-color:#fff}.button.secondary.working .mini-spin{border-color:var(--rule);border-top-color:var(--accent)}
.pick-main{display:grid;gap:2px;min-width:0}.pick-main small{display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.pick-main small .slim{max-width:none}
.card-head .card-sub{font-size:12.5px;color:var(--faint);margin-right:auto;margin-left:2px}.jobs-card .card-head{justify-content:flex-start;gap:10px}.jobs-card .card-head .loading-note{margin-left:auto}
.kpis{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:22px 32px;padding:6px 0 2px}.kpi{display:grid;align-content:start;min-width:0}.kpi .sk.fig{width:90px;height:30px;margin:0 0 4px}.kpi .sk.spark-sk{height:36px;margin:12px 0 4px;border-radius:6px}
.spark{display:block;width:100%;height:36px;margin:14px 0 2px;overflow:visible}.spark .base{stroke:var(--rule);stroke-width:1}.spark .a{fill:var(--accent);opacity:.8}.spark .b{fill:var(--bad-fill);opacity:.75}.spark .hit{fill:transparent}.spark g:hover .a{opacity:1}
.jobs-card .card-head{align-items:baseline}.kpi-num{font-size:30px;font-weight:650;letter-spacing:-.04em;line-height:1.05;font-variant-numeric:tabular-nums}.kpi-cap{margin-top:4px;font-size:13.5px;color:var(--muted)}.kpi-sub{margin-top:8px;min-height:24px;display:flex;align-items:center;flex-wrap:wrap;gap:6px;font-size:12.5px;color:var(--muted)}
.list-title{font-size:15px;font-weight:600;margin:28px 0 10px}.needs{list-style:none;margin:4px 0 0;padding:0;display:grid;gap:12px}.needs li{display:grid;gap:3px}.need-what{display:flex;align-items:center;gap:8px;justify-content:space-between}.needs small{color:var(--muted);font-size:12.5px;line-height:1.45}.side-update .asks{display:block;margin-top:4px;color:var(--muted)}.asks-list{border-bottom:1px solid var(--rule-soft);padding-bottom:14px;margin-bottom:6px}.limits{display:grid;gap:14px}.limits.out{margin-top:14px;padding:12px 12px 12px 14px;border:1px dashed var(--rule-soft);border-radius:12px}
.log-card .card-head{flex-wrap:wrap}.log-tools{margin-left:auto;display:flex;gap:6px;align-items:center}.log-search{height:28px;width:160px;padding:0 10px;border:1px solid var(--rule-soft);border-radius:999px;background:var(--card-2);color:var(--ink);font-size:12.5px}.log-tools .chip.on{background:var(--ink);color:var(--card)}
.log-steps{margin-top:10px;border:1px solid var(--rule-soft);border-radius:12px;overflow:hidden}.lstep+.lstep{border-top:1px solid var(--rule-soft)}.lstep>summary{display:flex;align-items:center;gap:10px;padding:9px 14px;cursor:pointer;list-style:none;font-size:13px}.lstep>summary::-webkit-details-marker{display:none}.lstep>summary:hover{background:var(--card-2)}.lstep[open]>summary{background:var(--card-2)}
.lsn{flex:0 1 auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lsb{flex:1;display:flex;align-items:center;gap:6px;min-width:0;overflow:hidden}
.lsb .tag{font:600 10px/1 var(--mono);letter-spacing:.06em;text-transform:uppercase;padding:3px 7px;border:1px solid var(--rule);border-radius:999px;color:var(--faint);white-space:nowrap}
.lsb .lst{color:var(--faint);font-size:12px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.lstep>summary::before{content:"";flex:none;width:5px;height:5px;margin:0 3px 0 1px;border-right:1.5px solid var(--faint);border-bottom:1.5px solid var(--faint);transform:rotate(-45deg);transition:transform .12s}
.lstep[open]>summary::before{transform:rotate(45deg)}
.lstep.lchild>summary{padding-left:36px}
.lstep.lhid{display:none}
.lstep.lgroup>.lines{display:none}
.lstep.lgroup[open]>summary{margin-bottom:0}.lsm{color:var(--faint);font:12px var(--mono)}
.ls{width:15px;height:15px;flex:none;border-radius:50%;display:inline-grid;place-items:center;font-style:normal;font-size:10px;font-weight:700;color:#fff}.lstep.success .ls{background:var(--good)}.lstep.success .ls::before{content:"✓"}.lstep.failure .ls{background:var(--bad)}.lstep.failure .ls::before{content:"✕"}.lstep.skipped .ls,.lstep.cancelled .ls{background:var(--faint)}.lstep.skipped .ls::before,.lstep.cancelled .ls::before{content:"–"}.lstep.pending .ls{border:1.5px solid var(--faint)}.lstep.running .ls{border:2px solid var(--accent);border-right-color:transparent;animation:spin .8s linear infinite}
.lines{counter-reset:ln;font:12px/1.6 var(--mono);padding:6px 0 10px;background:var(--card);overflow-x:auto}.lines>.note{padding:6px 14px;margin:0}/* 200 lines each, drawn only when on screen. */
.lc{content-visibility:auto;contain-intrinsic-size:auto 3840px}
.ll{display:flex;gap:12px;padding:0 14px 0 0;white-space:pre-wrap;word-break:break-word}.ln{flex:none;width:48px;text-align:right;color:var(--faint);user-select:none}.ll-more{margin:8px 14px 2px 62px}.ll:hover{background:var(--card-2)}
.lt{display:none;flex:none;color:var(--faint);user-select:none}.log-card.times .lt{display:inline}.ll.kg .lx{font-weight:650}.ll.ke{background:color-mix(in srgb,var(--bad) 10%,transparent)}.ll.ke .lx{color:var(--bad)}.ll.kw{background:color-mix(in srgb,#d29922 12%,transparent)}.ll.kc .lx{color:var(--accent)}
.log-card.finding .ll:not(.hit){display:none}.log-card.finding .lstep.nohit{display:none}.ll.hit .lx{background:color-mix(in srgb,var(--accent) 14%,transparent)}
.ab{font-weight:700}.a30,.a90{color:#6e7781}.a31,.a91{color:#cf222e}.a32,.a92{color:#1a7f37}.a33,.a93{color:#9a6700}.a34,.a94{color:#0969da}.a35,.a95{color:#8250df}.a36,.a96{color:#1b7c83}.a37,.a97{color:var(--muted)}
:root[data-theme=dark] .a31,:root[data-theme=dark] .a91{color:#ff7b72}:root[data-theme=dark] .a32,:root[data-theme=dark] .a92{color:#3fb950}:root[data-theme=dark] .a33,:root[data-theme=dark] .a93{color:#d29922}:root[data-theme=dark] .a34,:root[data-theme=dark] .a94{color:#58a6ff}:root[data-theme=dark] .a35,:root[data-theme=dark] .a95{color:#bc8cff}:root[data-theme=dark] .a36,:root[data-theme=dark] .a96{color:#39c5cf}
pre.log{margin:0;font:12px/1.55 var(--mono);white-space:pre-wrap;word-break:break-word;color:var(--ink);background:var(--card-2);border-radius:10px;padding:12px 14px;max-height:70vh;overflow:auto}
ul.steps{margin:0;padding-left:18px;display:grid;gap:4px;font:12.5px/1.5 var(--mono);color:var(--muted)}
p.note{margin:0;color:var(--muted)}.dlg-search{width:100%;height:34px;margin:2px 0 10px;border:1px solid var(--rule);border-radius:8px;background:var(--card);color:var(--ink);padding:0 10px;font:inherit;font-size:13px}.chk-list{display:grid;gap:2px;max-height:min(52vh,420px);overflow:auto;margin:0 -6px;padding:0 6px}.chk{display:flex;align-items:center;gap:10px;padding:7px 8px;border-radius:8px;font-size:13.5px;cursor:pointer}.chk:hover{background:var(--card-2)}.chk input{width:16px;height:16px;accent-color:var(--accent);margin:0}.chk .faint{color:var(--faint)}.limit{display:flex;align-items:center;justify-content:space-between;gap:12px 20px;flex-wrap:wrap}.limit-text{display:grid;gap:2px;min-width:0}.limit-text small{color:var(--muted);font-size:12.5px}.limit-do{display:flex;align-items:center;gap:8px}.limit-do input{height:32px;border:1px solid var(--rule);border-radius:8px;background:var(--card);color:var(--ink);padding:0 10px;font:inherit;font-size:13px}.limit-do input[type=number]{width:72px}.limit-do input[name=repo]{width:200px}.tag-chips{display:flex;flex-wrap:wrap;gap:6px}.tag-chip{display:inline-flex;align-items:center;gap:4px;padding:2px 4px 2px 10px;border:1px solid var(--rule);border-radius:999px;font-size:12.5px}.tag-chip code{background:none;padding:0}.icon-btn.sm{width:22px;height:22px}.icon-btn.sm svg{width:12px;height:12px}
.jobs-card .sub-block{border-top:0;padding-top:14px}.card-foot.end{justify-content:flex-end}.jobs-card .card-foot.end{border-top:0;padding-top:2px}.kpi-sub .sep{color:var(--faint)}
.jt-head{border-bottom:0}.jt-head span,.jt-head .jt-num,.jt-head .jt-when{font-size:12px;font-weight:500;color:var(--faint)}.jt-row{border-bottom:0;border-radius:10px}
@media (max-width:760px){.kpis{grid-template-columns:minmax(0,1fr)}}
.region-list{display:grid;grid-template-columns:1fr;gap:6px}.region-list .note{font-size:12px;margin:0}.aws-add{display:flex;align-items:center;gap:10px;flex-wrap:wrap;justify-content:flex-end}details.change summary{cursor:pointer;list-style:none}details.change summary::-webkit-details-marker{display:none}details.change[open] summary{display:none}details.change select{max-width:260px}.gl-form{display:grid;gap:16px}.gl-form .field input{width:100%}.gl-form .field small{color:var(--muted)}.gl-form .field small a{color:var(--accent)}
.gl-then{display:grid;gap:10px;padding-top:18px;border-top:1px solid var(--rule-soft)}.gl-then h3{font-size:13px}.or-gitlab{margin-top:10px}.or-gitlab a{color:var(--accent);font-weight:550}
.picker{display:grid;gap:16px}.picker-fields{display:flex;flex-wrap:wrap;gap:12px 14px;align-items:flex-end}
.label-out{display:flex;align-items:center;gap:10px;min-width:0;padding:10px 10px 10px 16px;border-radius:12px;background:var(--card-2)}.label-out code{flex:1;min-width:0;overflow-x:auto;white-space:nowrap;font:13.5px/1.6 var(--mono)}.label-out .k{color:var(--muted)}.label-out .hl{color:var(--accent);font-weight:600}
.fits{display:flex;flex-wrap:wrap;align-items:center;gap:8px}.fit{display:inline-flex;align-items:center;gap:6px;padding:3px 10px 3px 4px;border:1px solid var(--rule);border-radius:999px;font-size:12px;font-weight:550}.fit .logo{border-radius:999px}
.fit small{color:var(--muted);font:500 11px/1 var(--mono);font-variant-numeric:tabular-nums}.fit small:not(:empty)::before{content:"·";margin-right:6px;color:var(--faint)}
.fit.no{opacity:.45;text-decoration:line-through;text-decoration-color:var(--faint)}
.picker-foot{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:10px;padding-top:14px;border-top:1px solid var(--rule-soft)}.picker-foot .button:disabled{opacity:.5;cursor:default}
@media (max-width:680px){.pool{grid-template-columns:16px 32px minmax(0,1fr) 32px}.pool.still{grid-template-columns:32px minmax(0,1fr) 32px}.pool.still .pool-now{grid-column:2}.pool.still .icon-btn{grid-column:3}.pool-now{grid-column:3;grid-row:2;justify-content:flex-start}.pool .icon-btn{grid-column:4;grid-row:1}}
@media (max-width:980px){.layout{display:block}.side{position:relative;height:auto;padding:12px 16px}.side-foot{margin-top:2px;padding-top:0;gap:2px}.side-update{margin:8px 0}}
@media (max-width:680px){.main{padding:0 16px 56px}.bar{padding:12px 0}.row{grid-template-columns:8px minmax(0,1fr)}.row-end{grid-column:2;justify-content:flex-start}}
:root .dlg{color-scheme:dark;--bg:#0b0b0c;--bg-deep:#050506;--card:#242830;--card-2:rgba(255,255,255,.065);--ink:#f7f7fa;--muted:#bdc2ce;--faint:#abb2c1;--rule:rgba(255,255,255,.16);--rule-soft:rgba(255,255,255,.12);--accent:#2997ff;--accent-hover:#52abff;--accent-soft:rgba(41,151,255,.16);--open:#ff9f0a;--open-ink:#ffb340;--open-soft:rgba(255,159,10,.15);--good:#32d74b;--good-fill:#30d158;--good-soft:rgba(50,215,75,.15);--bad:#ff6961;--bad-fill:#ff453a;--bad-soft:rgba(255,69,58,.16);--pick-last:#0f1d2e;--sk:rgba(255,255,255,.07);--sk-hi:rgba(255,255,255,.06);--shadow:0 1px 2px rgba(0,0,0,.5),0 14px 34px rgba(0,0,0,.45);
max-height:calc(100dvh - 36px);overflow-y:auto;overscroll-behavior:contain;border:1px solid rgba(255,255,255,.24);border-radius:30px;background:linear-gradient(145deg,rgba(255,255,255,.09),transparent 32%),radial-gradient(circle at 8% 12%,rgba(255,176,92,.13),transparent 31%),radial-gradient(circle at 92% 18%,rgba(105,151,255,.17),transparent 33%),radial-gradient(circle at 78% 92%,rgba(74,204,188,.12),transparent 31%),rgba(12,14,19,.82);box-shadow:0 36px 100px rgba(0,0,0,.42),inset 0 1px 0 rgba(255,255,255,.2),inset 0 -1px 0 rgba(255,255,255,.06);-webkit-backdrop-filter:blur(28px) saturate(180%) brightness(.78);backdrop-filter:blur(28px) saturate(180%) brightness(.78);color:var(--ink);scrollbar-width:thin;scrollbar-color:rgba(255,255,255,.2) transparent}
:root .dlg:focus{outline:none}
:root .dlg::backdrop{background:rgba(8,10,14,.5);-webkit-backdrop-filter:blur(3px);backdrop-filter:blur(3px)}
:root .dlg .dlg-body{gap:18px;padding:20px}
:root .dlg .dlg-head{grid-template-columns:36px minmax(0,1fr) 36px;gap:13px}
:root .dlg .dlg-head strong{color:#fff;font-size:1rem;font-weight:610;letter-spacing:-.025em;line-height:1.2}
:root .dlg .dlg-head small{color:rgba(235,235,241,.68);font-size:.78rem;line-height:1.4}
:root .dlg .dlg-head>.logo{border-radius:50%}
:root .dlg .dlg-head>.logo[style*="accent-soft"]{border:1px solid rgba(255,255,255,.16);background:rgba(255,255,255,.08)!important;color:rgba(255,255,255,.86)!important}
:root .dlg .icon-btn{width:36px;height:36px;border:1px solid rgba(255,255,255,.14);border-radius:50%;background:rgba(255,255,255,.09);box-shadow:inset 0 1px 0 rgba(255,255,255,.1);color:#fff;transition:background .18s ease,transform .18s ease}
:root .dlg .icon-btn:hover{background:rgba(255,255,255,.16)}
:root .dlg .icon-btn:active{transform:scale(.94)}
:root .dlg .button{height:40px;padding:0 18px;border:1px solid rgba(255,255,255,.14);border-radius:999px;background:rgba(255,255,255,.09);box-shadow:inset 0 1px 0 rgba(255,255,255,.1);color:#fff;font-size:.8rem;font-weight:620;transition:background .18s ease,transform .18s ease}
:root .dlg .button:hover{background:rgba(255,255,255,.16);transform:none}
:root .dlg .button:active{transform:scale(.985)}
:root .dlg .button.primary{border-color:transparent;background:#0a84ff;box-shadow:0 10px 24px rgba(0,104,225,.24),inset 0 1px 0 rgba(255,255,255,.28)}
:root .dlg .button.primary:hover{background:#188cff}
:root .dlg .button.danger{border-color:rgba(255,105,97,.32);background:rgba(255,69,58,.14);color:#ff8f88}
:root .dlg .button.danger:hover{background:rgba(255,69,58,.24)}
:root .dlg .button.sm{height:36px;padding:0 16px}
:root .dlg .button[disabled]{opacity:.6}
:root .dlg input:not([type=checkbox]):not([type=radio]):not([type=hidden]):not([type=range]),:root .dlg select{height:42px;padding:0 14px;border:1px solid rgba(255,255,255,.16);border-radius:14px;background:rgba(255,255,255,.07);box-shadow:inset 0 1px 0 rgba(255,255,255,.05);color:#fff;font-size:.86rem;letter-spacing:-.01em}
:root .dlg input::placeholder{color:rgba(235,235,241,.38)}
:root .dlg input:focus,:root .dlg select:focus{outline:2px solid rgba(96,174,255,.92);outline-offset:1px;border-color:transparent}
:root .dlg .limit-do .button{height:42px}
:root .dlg .dlg-search{margin:0}
:root .dlg .dlg-foot{gap:10px}
:root .dlg .dlg-danger{border-top:1px solid rgba(255,255,255,.1);border-radius:0 0 30px 30px;background:rgba(255,255,255,.045)}
:root .dlg .dlg-signin{border:1px solid rgba(255,255,255,.1);border-radius:18px;background:rgba(255,255,255,.055)}
:root .dlg :focus-visible{outline:2px solid rgba(96,174,255,.92);outline-offset:3px}"#;

/// Follows the system's light or dark, also when it changes while the page is open.
const THEME: &str = r#"<script>(function(){var m=matchMedia('(prefers-color-scheme: dark)');function f(){document.documentElement.dataset.theme=m.matches?'dark':'light'}f();if(m.addEventListener)m.addEventListener('change',f)})();</script>"#;

/// A full page with the shared look; `body` is placed as is.
/// SuperCI's mark, as a path in a box of 24 by 24.
#[macro_export]
macro_rules! mark_path { () => { "M4.31 12.77L4.58 12.25L4.56 11.66L4.40 11.34L3.58 10.29L3.10 9.39L2.72 8.33L2.51 7.12L2.51 5.99L2.69 4.85L3.16 3.56L3.52 3.18L4.53 2.79L5.54 2.56L6.65 2.48L7.69 2.58L8.65 2.81L9.52 3.15L10.48 3.71L11.42 4.45L11.97 4.61L12.56 4.46L13.62 3.64L14.61 3.10L15.56 2.75L16.65 2.53L17.89 2.50L19.04 2.67L20.42 3.15L20.82 3.52L21.21 4.53L21.44 5.54L21.52 6.65L21.42 7.69L21.19 8.65L20.85 9.52L20.29 10.48L19.55 11.42L19.39 11.97L19.54 12.56L20.36 13.62L20.90 14.61L21.28 15.65L21.47 16.65L21.51 17.81L21.33 19.04L20.85 20.42L20.48 20.82L19.47 21.21L18.46 21.44L17.35 21.52L16.31 21.42L15.35 21.19L14.50 20.86L13.60 20.34L12.58 19.55L11.99 19.39L11.42 19.55L10.40 20.34L9.50 20.86L8.57 21.21L7.69 21.42L6.65 21.52L5.54 21.44L4.53 21.21L3.56 20.84L3.16 20.44L2.69 19.15L2.51 18.01L2.52 16.79L2.72 15.67L3.10 14.61L3.64 13.62L4.31 12.77Z" } }

/// The icon a browser shows on a page's tab: SuperCI's mark, white on a dark tile (as superci.dev has it). The
/// dashboard puts the picture of the workspace's organization in its place when it has one.
const ICON: &str = concat!(r#"<link rel="icon" id="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='8' fill='%2310141c'/%3E%3Cpath fill='%23fff' transform='translate(4 4)' d='"#, mark_path!(), r#"'/%3E%3C/svg%3E">"#);

pub fn document(status: u16, title: &str, body: &str, refresh_secs: Option<u32>) -> Response {
    let refresh = refresh_secs.map(|s| format!(r#"<meta http-equiv="refresh" content="{s}">"#)).unwrap_or_default();
    let page = format!(r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">{refresh}<title>{}</title>{ICON}{THEME}<style>{STYLE}</style></head><body>{body}</body></html>"#, esc(title));
    Response::new(status, "text/html; charset=utf-8", page).with_header("x-frame-options", "DENY").with_header("referrer-policy", "no-referrer")
}

pub fn html(status: u16, title: &str, body: &str) -> Response {
    html_refreshing(status, title, body, None)
}

pub fn html_refreshing(status: u16, title: &str, body: &str, refresh_secs: Option<u32>) -> Response {
    document(status, title, &format!(r#"<main class="plain">{body}</main>"#), refresh_secs)
}

pub fn message(status: u16, heading: &str, text: &str) -> Response {
    html(status, "superci", &format!(r#"<h1>{}</h1><p class="note">{text}</p><p style="margin-top:18px"><a class="chip" href="/">Back</a></p>"#, esc(heading)))
}

/// The page that posts the manifest to GitHub (GitHub requires a form POST from the browser).
pub fn manifest_form(target: &str, manifest: &serde_json::Value) -> Response {
    html(200, "Creating the GitHub App", &format!(r#"<h1>Creating your GitHub App…</h1><p class="note">GitHub will ask you to confirm its name, then create it.</p>
<form id="f" method="post" action="{}" style="display:block;margin-top:18px"><input type="hidden" name="manifest" value="{}"><button class="button">Continue to GitHub</button></form>
<script>document.getElementById('f').submit()</script>"#, esc(target), esc(&manifest.to_string())))
}

/// A region the setup page offers for AWS: its id, its name as AWS's console writes it, its short name, and the
/// regions to fall back to from it, nearest first (when it has no spot capacity left).
pub struct Region { pub id: &'static str, pub name: &'static str, pub short: &'static str, pub nearby: &'static [&'static str] }

pub const AWS_REGIONS: [Region; 12] = [
    Region { id: "us-east-1", name: "US East (N. Virginia)", short: "N. Virginia", nearby: &["us-east-2", "us-west-2"] },
    Region { id: "us-east-2", name: "US East (Ohio)", short: "Ohio", nearby: &["us-east-1", "us-west-2"] },
    Region { id: "us-west-2", name: "US West (Oregon)", short: "Oregon", nearby: &["us-east-2", "us-east-1"] },
    Region { id: "ca-central-1", name: "Canada (Central)", short: "Canada", nearby: &["us-east-1", "us-east-2"] },
    Region { id: "eu-west-1", name: "Europe (Ireland)", short: "Ireland", nearby: &["eu-west-2", "eu-central-1"] },
    Region { id: "eu-west-2", name: "Europe (London)", short: "London", nearby: &["eu-west-1", "eu-central-1"] },
    Region { id: "eu-central-1", name: "Europe (Frankfurt)", short: "Frankfurt", nearby: &["eu-west-1", "eu-north-1"] },
    Region { id: "eu-north-1", name: "Europe (Stockholm)", short: "Stockholm", nearby: &["eu-central-1", "eu-west-1"] },
    Region { id: "ap-south-1", name: "Asia Pacific (Mumbai)", short: "Mumbai", nearby: &["ap-southeast-1"] },
    Region { id: "ap-northeast-1", name: "Asia Pacific (Tokyo)", short: "Tokyo", nearby: &["ap-southeast-1"] },
    Region { id: "ap-southeast-1", name: "Asia Pacific (Singapore)", short: "Singapore", nearby: &["ap-northeast-1", "ap-southeast-2"] },
    Region { id: "ap-southeast-2", name: "Asia Pacific (Sydney)", short: "Sydney", nearby: &["ap-southeast-1"] },
];

/// Regions the setup page offers for AWS, by id.
pub const REGIONS: [&str; 12] = {
    let (mut ids, mut i) = ([""; 12], 0);
    while i < ids.len() { ids[i] = AWS_REGIONS[i].id; i += 1 }
    ids
};

fn aws_region(id: &str) -> Option<&'static Region> { AWS_REGIONS.iter().find(|r| r.id == id) }

/// A region's name as AWS's console writes it (one not offered: nothing).
pub fn region_name(region: &str) -> &'static str { aws_region(region).map_or("", |r| r.name) }

/// A region's short name: "N. Virginia" for us-east-1 (one not offered: its id).
pub fn region_short(region: &str) -> String { aws_region(region).map_or(region, |r| r.short).to_string() }

/// The regions to fall back to from one, nearest first (when it has no spot capacity left).
pub fn nearby_regions(region: &str) -> &'static [&'static str] { aws_region(region).map_or(&[], |r| r.nearby) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(esc(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }

    #[test]
    fn every_region_offered_has_its_names_and_falls_back_only_to_regions_offered() {
        assert_eq!(REGIONS.len(), AWS_REGIONS.len());
        for (id, r) in REGIONS.iter().zip(&AWS_REGIONS) {
            assert_eq!((*id, region_name(id), region_short(id), nearby_regions(id)), (r.id, r.name, r.short.to_string(), r.nearby));
            assert!(r.name.contains(r.short) || r.short == "Canada", "{id}");
            assert!(!r.nearby.is_empty() && r.nearby.iter().all(|n| REGIONS.contains(n) && n != id), "{id}");
        }
        assert_eq!((region_name("us-east-1"), region_short("eu-north-1"), nearby_regions("ap-south-1")), ("US East (N. Virginia)", "Stockholm".to_string(), &["ap-southeast-1"][..]));
        assert_eq!((region_name("mars-1"), region_short("mars-1"), nearby_regions("mars-1")), ("", "mars-1".to_string(), &[][..]));
    }
}
