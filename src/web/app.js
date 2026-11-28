const tok=new URLSearchParams(location.search).get('token')||localStorage.getItem('twig_tok')||'';
if(tok)localStorage.setItem('twig_tok',tok);

let curSid='',curWt='',defaultWt='',defaultBranch='main',defaultDiff='',activeModel='default',selModel='',selEffort='',selRole='code',execMode='auto';
let filter='all',sbMode='threads',allRepos=false,diffOpen=false,scOpen=false,artOpen=false,artWide=false,termOpen=false,diffMode='unified',artTab='preview',artVp='desktop',ccShut=false;
let sessions=[],projects=[],schedules=[],models=[],skills=[],repoFiles=[],filesWt='',detail=null,autoData=null,lastHtml='',lastDiffSig='',forceTail=true,stickToBottom=true,progScroll=false,draftNew=false,draftReady=false,newWt=false;
let notifOn=localStorage.getItem('twig_notif')==='1',editor=localStorage.getItem('twig_ed')||'cursor',sendKeyMode=localStorage.getItem('twig_send_key')||'enter';
const LEGACY_TH={paper:'claude',obsidian:'obsidian',midnight:'studio',daylight:'studio'};
let palette=localStorage.getItem('twig_palette')||LEGACY_TH[localStorage.getItem('twig_theme_v2')]||'claude';
let colorMode=localStorage.getItem('twig_cmode')||'system';
const sysDarkMq=window.matchMedia('(prefers-color-scheme: dark)');
let pinned=new Set(JSON.parse(localStorage.getItem('twig_pins')||'[]'));
let drafts={};try{drafts=JSON.parse(localStorage.getItem('twig_drafts_v1')||'{}')||{};}catch(_){drafts={};}
let shutGroups={},images=[],reviewNotes=[],prevStates={},openLogs={},openTools={};
let acItems=[],acIdx=0,cmdkItems=[],cmdkIdx=0,recog=null,askResolve=null,askConfirmOnly=false,busyRpc=false;
let artStore=[],artMap=new Map(),curArt=null,curArtIdx=-1,presetScope='project';
let sseOk=false,liveText={},liveTool={},liveSteer={},userTurnTexts=[],histIdx=-1,histDraft='';
let searchHits=[],searchQ='',searchTm=null;

const ED_NAMES={cursor:'Cursor',vscode:'VS Code',windsurf:'Windsurf',zed:'Zed'};
const FAV_PAL=[['#122b26','#34d399'],['#17213a','#818cf8'],['#2a1b38','#c084fc'],['#332214','#fb923c'],['#311824','#f472b6']];
const SLASH_CMDS=[
  {k:'/goal ',d:'Autonomous multi-wave execution + blocker-gated /review --fix'},
  {k:'/review --fix',d:'Independent read-only diff review & auto-fix blockers'},
  {k:'/plan ',d:'Explore repository in read-only Plan mode'},
  {k:'/walkthrough',d:'Generate structured codebase architecture walkthrough'},
];

function resolvedAppearance(){return colorMode==='system'?(sysDarkMq.matches?'dark':'light'):colorMode;}
function applyAppearance(rerenderDiagrams=false){
  if(!['claude','codex','studio','obsidian'].includes(palette))palette='claude';
  if(!['system','dark','light'].includes(colorMode))colorMode='system';
  const eff=resolvedAppearance();
  document.documentElement.setAttribute('data-theme',`${palette}-${eff}`);
  localStorage.setItem('twig_palette',palette);
  localStorage.setItem('twig_cmode',colorMode);
  document.querySelectorAll('.th-dot').forEach(d=>d.classList.toggle('active',d.dataset.p===palette));
  const cb=document.getElementById('cmode-btn');
  if(cb){
    cb.textContent=colorMode==='system'?'◐':colorMode==='dark'?'☾':'☀';
    cb.title=`Appearance: ${colorMode==='system'?`System (auto · ${eff})`:colorMode} — click to cycle`;
  }
  if(rerenderDiagrams){lastHtml='';if(detail)render();if(artOpen)renderArtDock();if(document.getElementById('stg-modal')?.classList.contains('open'))renderStgBody();}
}
function setPalette(p){palette=p;applyAppearance(true);}
function setColorMode(m){colorMode=m;applyAppearance(true);}
function cycleColorMode(){
  const order=['system','dark','light'];
  setColorMode(order[(order.indexOf(colorMode)+1)%order.length]);
  toast(`Appearance: ${colorMode==='system'?`System (${resolvedAppearance()})`:colorMode}`);
}
sysDarkMq.addEventListener('change',()=>{if(colorMode==='system')applyAppearance(true);});
applyAppearance(false);
if(notifOn)document.getElementById('notif-btn').classList.add('on');

const hdrs=()=>tok?{'Content-Type':'application/json','Authorization':'Bearer '+tok}:{'Content-Type':'application/json'};
async function api(p,o={}){const r=await fetch(p,{...o,headers:hdrs()});return r.json();}
function esc(s){return String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function toast(m){const t=document.getElementById('toast');t.textContent=m;t.classList.add('show');clearTimeout(t._tm);t._tm=setTimeout(()=>t.classList.remove('show'),2400);}
function copyText(txt,msg='Copied',btnEl){
  navigator.clipboard?.writeText(txt||'');toast(msg);
  if(btnEl&&btnEl.tagName==='BUTTON'){
    const orig=btnEl.dataset.orig||btnEl.textContent;btnEl.dataset.orig=orig;
    btnEl.textContent='✓ Copied';btnEl.style.color='var(--ok)';
    clearTimeout(btnEl._ctm);btnEl._ctm=setTimeout(()=>{btnEl.textContent=orig;btnEl.style.color='';},1200);
  }
}
function openInputModal({title='Input',sub='',placeholder='',value='',okText='Confirm',confirmOnly=false}){
  return new Promise(res=>{
    askResolve=res;askConfirmOnly=!!confirmOnly;
    document.getElementById('ask-ttl').textContent=title;
    const subEl=document.getElementById('ask-sub');subEl.textContent=sub;subEl.style.display=sub?'block':'none';
    const inp=document.getElementById('ask-in');
    inp.style.display=confirmOnly?'none':'block';
    inp.placeholder=placeholder;inp.value=value||'';
    document.getElementById('ask-ok').textContent=okText;
    document.getElementById('ask-modal').classList.add('open');
    setTimeout(()=>{if(confirmOnly)document.getElementById('ask-ok').focus();else{inp.focus();inp.select();}},20);
  });
}
function resolveAskModal(val){
  document.getElementById('ask-modal').classList.remove('open');
  const fn=askResolve;askResolve=null;if(fn)fn(val);
}
function submitAskModal(){resolveAskModal(askConfirmOnly?true:document.getElementById('ask-in').value);}
function onAskModalKey(e){
  if(e.isComposing||e.keyCode===229)return;
  if(e.key==='Escape'){e.preventDefault();resolveAskModal(null);}
  else if(e.key==='Enter'){e.preventDefault();submitAskModal();}
}
function draftKey(sid){return sid||'__new__';}
function saveCurDraft(){
  if(!draftReady)return;
  const inp=document.getElementById('inp');if(!inp)return;
  const k=draftKey(curSid),v=inp.value;
  if(v&&v.trim())drafts[k]=v;else delete drafts[k];
  try{localStorage.setItem('twig_drafts_v1',JSON.stringify(drafts));}catch(_){}
}
function restoreCurDraft(){
  const inp=document.getElementById('inp');if(!inp)return;
  inp.value=drafts[draftKey(curSid)]||'';
  draftReady=true;
  histIdx=-1;histDraft='';autoGrow(inp);syncActionBtn();
}
function baseName(p){const s=String(p||'').replace(/\/+$/,'');return s.split('/').pop()||'workspace';}
function projFav(name){
  const clean=String(name||'tw');let h=0;for(let i=0;i<clean.length;i++)h=(h*31+clean.charCodeAt(i))>>>0;
  const[bg,fg]=FAV_PAL[h%FAV_PAL.length];
  const ab=clean.replace(/[^a-zA-Z0-9]/g,'').slice(0,2).toUpperCase()||'TW';
  return `<span class="proj-fav" style="background:${bg};color:${fg}">${esc(ab)}</span>`;
}
function shortAge(a){return String(a||'').replace(' ago','');}
function shortModel(m){const s=String(m||'default');return s.includes('/')?s.split('/').pop():s;}
function ideUri(absPath,line){const clean=String(absPath||'').replace(/^file:\/\//,'');if(!clean)return '#';const p=clean.startsWith('/')?clean:'/'+clean;return line?`${editor}://file${p}:${line}:1`:`${editor}://file${p}`;}

function togglePin(sid){
  if(pinned.has(sid))pinned.delete(sid);else pinned.add(sid);
  localStorage.setItem('twig_pins',JSON.stringify([...pinned]));
  renderList();
}

/* Panels & Responsive Shell */
function toggleSidebar(){
  const sb=document.getElementById('sidebar'),mask=document.getElementById('mob-mask');
  if(innerWidth<=760){const shut=sb.classList.toggle('mob-shut');mask.classList.toggle('show',!shut);}
  else sb.classList.toggle('collapsed');
}
function closeMobilePanels(){
  document.getElementById('sidebar').classList.add('mob-shut');
  document.getElementById('insp').classList.remove('mob-open');
  if(innerWidth<=1080){
    diffOpen=false;scOpen=false;artOpen=false;
    document.getElementById('diff-dock').classList.remove('open');
    document.getElementById('sidechat').classList.remove('open');
    document.getElementById('art-dock').classList.remove('open');
    document.getElementById('diff-toggle-btn').classList.remove('on');
    document.getElementById('sc-btn').classList.remove('on');
    document.getElementById('art-btn').classList.remove('on');
  }
  document.getElementById('mob-mask').classList.remove('show');
}
function closeSideDocks(except){
  if(except!=='diff'){diffOpen=false;document.getElementById('diff-dock').classList.remove('open');document.getElementById('diff-toggle-btn').classList.remove('on');}
  if(except!=='sc'){scOpen=false;document.getElementById('sidechat').classList.remove('open');document.getElementById('sc-btn').classList.remove('on');}
  if(except!=='art'){artOpen=false;document.getElementById('art-dock').classList.remove('open');document.getElementById('art-btn').classList.remove('on');}
}
function toggleInspector(){
  const el=document.getElementById('insp'),b=document.getElementById('insp-btn');
  if(innerWidth<=1080){
    const open=el.classList.toggle('mob-open');
    b.classList.toggle('on',open);
    document.getElementById('mob-mask').classList.toggle('show',open);
    return;
  }
  if((diffOpen||scOpen||artOpen)&&innerWidth<1640){
    closeSideDocks('');
    el.classList.remove('shut');b.classList.add('on');
    return;
  }
  const shut=el.classList.toggle('shut');
  b.classList.toggle('on',!shut);
}
function toggleDiffPanel(forceOpen){
  const dock=document.getElementById('diff-dock'),b=document.getElementById('diff-toggle-btn'),insp=document.getElementById('insp'),ib=document.getElementById('insp-btn');
  diffOpen=typeof forceOpen==='boolean'?forceOpen:!diffOpen;
  if(diffOpen)closeSideDocks('diff');
  dock.classList.toggle('open',diffOpen);
  b.classList.toggle('on',diffOpen);
  if(diffOpen){
    if(innerWidth<1640&&innerWidth>1080){insp.classList.add('shut');ib.classList.remove('on');}
    if(innerWidth<=1080){insp.classList.remove('mob-open');if(innerWidth<=760)document.getElementById('mob-mask').classList.add('show');}
    lastDiffSig='';renderDiffDock();
  }else if(innerWidth>1080&&insp.classList.contains('shut')){
    insp.classList.remove('shut');ib.classList.add('on');
  }
  syncArtPill();
}
function toggleSideChat(prefill){
  const sc=document.getElementById('sidechat'),b=document.getElementById('sc-btn'),insp=document.getElementById('insp'),ib=document.getElementById('insp-btn');
  scOpen=prefill?true:!scOpen;
  if(scOpen)closeSideDocks('sc');
  sc.classList.toggle('open',scOpen);
  b.classList.toggle('on',scOpen);
  if(scOpen){
    if(innerWidth<1640&&innerWidth>1080){insp.classList.add('shut');ib.classList.remove('on');}
    const inp=document.getElementById('sc-in');
    if(typeof prefill==='string'&&prefill)inp.value=prefill;
    setTimeout(()=>inp.focus(),40);
  }else if(innerWidth>1080&&insp.classList.contains('shut')){
    insp.classList.remove('shut');ib.classList.add('on');
  }
  syncArtPill();
}
function toggleArtDock(forceOpen){
  const dock=document.getElementById('art-dock'),b=document.getElementById('art-btn'),insp=document.getElementById('insp'),ib=document.getElementById('insp-btn');
  artOpen=typeof forceOpen==='boolean'?forceOpen:!artOpen;
  if(artOpen)closeSideDocks('art');
  dock.classList.toggle('open',artOpen);
  b.classList.toggle('on',artOpen);
  if(artOpen){
    if(innerWidth<1640&&innerWidth>1080){insp.classList.add('shut');ib.classList.remove('on');}
    renderArtDock();
  }else if(innerWidth>1080&&insp.classList.contains('shut')){
    insp.classList.remove('shut');ib.classList.add('on');
  }
  syncArtPill();
}
function toggleTerminal(){
  termOpen=!termOpen;
  document.getElementById('term-drawer').classList.toggle('open',termOpen);
  document.getElementById('term-btn').classList.toggle('on',termOpen);
  if(termOpen)setTimeout(()=>document.getElementById('td-in').focus(),40);
}
function setDiffMode(m){
  diffMode=m;lastDiffSig='';
  document.getElementById('dm-uni').classList.toggle('active',m==='unified');
  document.getElementById('dm-spl').classList.toggle('active',m==='split');
  renderDiffDock();
}

/* Popovers & Pickers */
function closeAllPops(){document.querySelectorAll('.pop.open').forEach(p=>p.classList.remove('open'));document.querySelectorAll('.pill-trig.open,.proj-sel-btn.open').forEach(b=>b.classList.remove('open'));}
function openPop(id,btn){
  const el=document.getElementById(id),was=el.classList.contains('open');
  closeAllPops();if(was)return;
  el.classList.add('open');btn.classList.add('open');
  const r=btn.getBoundingClientRect(),pw=Math.max(228,el.offsetWidth||228);
  const left=Math.max(8,Math.min(innerWidth-pw-8,r.left<300?r.left:r.right-pw));
  if(r.top>280){el.style.bottom=(innerHeight-r.top+6)+'px';el.style.top='auto';}
  else{el.style.top=(r.bottom+6)+'px';el.style.bottom='auto';}
  el.style.left=left+'px';
}
document.addEventListener('click',e=>{
  if(!e.target.closest('.pop')&&!e.target.closest('.pill-trig')&&!e.target.closest('.proj-sel-btn')&&!e.target.closest('.ctx-item')&&!e.target.closest('.split-ed'))closeAllPops();
  if(!e.target.closest('.ac-menu')&&e.target.id!=='inp')closeAc();
});

async function loadProjects(){
  const d=await api('/api/projects');
  if(Array.isArray(d.projects))projects=d.projects;
  syncProjBar();renderProjPop();
}
function syncProjBar(){
  const active=defaultWt||curWt||'';
  const found=projects.find(p=>p.path===active);
  const nm=found?found.name:baseName(active);
  const br=(found&&found.branch)||defaultBranch||'main';
  document.getElementById('sb-proj-ico').innerHTML=projFav(nm);
  document.getElementById('sb-proj-name').textContent=allRepos?'All Projects':nm;
  document.getElementById('sb-proj-br').textContent=allRepos?`${projects.length} repos`:br;
}
function renderProjPop(){
  const box=document.getElementById('proj-pop');if(!box)return;
  const active=defaultWt||curWt||'';
  box.innerHTML=`<div class="pop-lbl">Projects (Claude / Codex Workspaces)</div>`+
    projects.map(p=>{
      const on=!allRepos&&p.path===active;
      return `<button class="pop-opt ${on?'sel':''}" data-p="${esc(p.path)}" onclick="switchProject(this.dataset.p)">
        <span style="display:inline-flex;align-items:center;gap:7px;min-width:0">${projFav(p.name)}<span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(p.name)}</span>${p.has_agents_md?'<span class="draft-pill">AGENTS</span>':''}</span>
        <span style="font:11px/1 ui-monospace,monospace;color:var(--fg-muted)">${on?'✓':esc(p.branch||'')}</span>
      </button>`;
    }).join('')+
    `<div class="pop-sep"></div>
     <button class="pop-opt ${allRepos?'sel':''}" onclick="closeAllPops();if(!allRepos)toggleAllRepos()"><span>🌐 Show threads across all projects</span><span>${allRepos?'✓':''}</span></button>
     <button class="pop-opt" style="color:var(--accent)" onclick="closeAllPops();addLocalProject()"><span>+ Add local Git project…</span><span></span></button>`;
}
function openProjPop(btn){loadProjects();openPop('proj-pop',btn);}
async function switchProject(path){
  closeAllPops();
  if(!path)return;
  saveCurDraft();
  allRepos=false;
  const sb=document.getElementById('scope-btn');
  if(sb){sb.textContent='Repo';sb.classList.remove('active');}
  defaultWt=path;curWt=path;curSid='';filesWt='';draftNew=false;
  await loadMeta(path);
  await loadProjects();
  await loadList();
  if(sbMode==='auto')loadAutomations();
  toast('Project: '+baseName(path));
}
async function addLocalProject(){
  const p=await openInputModal({title:'Add Local Project Workspace',sub:'Enter absolute path or ~/… path to any local repository or folder',placeholder:'~/Dev/my-repo',okText:'Add Project'});
  if(!p||!p.trim())return;
  const r=await api('/api/projects',{method:'POST',body:JSON.stringify({action:'add',path:p.trim()})});
  if(r.error)return toast(r.error);
  await loadProjects();
  if(r.path)await switchProject(r.path);
}

function syncComposerPills(){
  const m=shortModel(selModel||activeModel);
  const e=selEffort?` · ${selEffort[0].toUpperCase()+selEffort.slice(1)}`:'';
  document.getElementById('model-lbl').textContent=m+e;
  const modeNames={auto:'⚡ Full Auto',review:'⛨ Review Edits',plan:'◈ Plan Only'};
  document.getElementById('role-lbl').textContent=modeNames[execMode]||'⚡ Full Auto';
  ['auto','review','plan'].forEach(k=>{
    const el=document.getElementById('ro-'+k);
    if(el){el.classList.toggle('sel',execMode===k);el.lastElementChild.textContent=execMode===k?'✓':'';}
  });
  document.querySelectorAll('#eff-seg button').forEach(b=>b.classList.toggle('sel',b.dataset.e===selEffort));
  document.getElementById('open-ed-lbl').textContent='Open in '+(ED_NAMES[editor]||'Editor');
  document.querySelectorAll('#ed-pop .pop-opt').forEach(b=>{const on=b.dataset.ed===editor;b.classList.toggle('sel',on);b.lastElementChild.textContent=on?'✓':'';});
}
function renderModelPop(){
  const box=document.getElementById('model-opts');
  const all=['',...models];
  box.innerHTML=all.map(m=>{
    const on=m===selModel;
    const lbl=m?shortModel(m):`Default (${shortModel(activeModel)})`;
    return `<button class="pop-opt ${on?'sel':''}" data-m="${esc(m)}" onclick="selModel=this.dataset.m;syncComposerPills();renderModelPop();closeAllPops()"><span>${esc(lbl)}</span><span>${on?'✓':''}</span></button>`;
  }).join('');
}
function pickEffort(e){selEffort=e;syncComposerPills();}
async function pickExecMode(mode,quiet=false){
  execMode=['auto','review','plan'].includes(mode)?mode:'auto';
  selRole=execMode==='plan'?'plan':'code';
  syncComposerPills();closeAllPops();
  if(!quiet){
    const lbl={auto:'Full Auto',review:'Review Edits (gates file writes)',plan:'Plan Only (read-only)'}[execMode];
    toast('Mode: '+lbl);
    if(curSid&&detail&&!detail.running){
      await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'mode.set',params:{plan:execMode==='plan',gate_edits:execMode==='review'}})});
    }
  }
}
function cycleExecMode(){
  const order=['auto','review','plan'];
  pickExecMode(order[(order.indexOf(execMode)+1)%order.length]);
}
function pickEditor(ed){editor=ed;localStorage.setItem('twig_ed',ed);syncComposerPills();renderInspector();closeAllPops();}

/* Lightweight Single-Pass Client Syntax Highlighter (Code + Diffs) */
const SH_RE=/(\/\/[^\n]*|#[^\n]*)|("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')|\b(fn|pub|let|mut|const|impl|struct|enum|trait|match|if|else|for|while|loop|return|use|mod|async|await|function|export|import|from|class|def|try|catch|None|Some|Ok|Err|true|false)\b|\b([A-Z][A-Za-z0-9_]{1,32})\b|\b(\d+(?:\.\d+)?)\b|[&<>"']/g;
function shCode(raw){
  return String(raw??'').replace(SH_RE,(m,cm,str,kw,ty,num)=>{
    if(cm)return `<span class="sh-cm">${esc(cm)}</span>`;
    if(str)return `<span class="sh-str">${esc(str)}</span>`;
    if(kw)return `<span class="sh-kw">${kw}</span>`;
    if(ty)return `<span class="sh-ty">${ty}</span>`;
    if(num)return `<span class="sh-num">${num}</span>`;
    return esc(m);
  });
}

/* Markdown + SVG + LaTeX + File Chips */
function svgDataUri(raw){try{const b=new TextEncoder().encode(raw);let s='';for(let i=0;i<b.length;i++)s+=String.fromCharCode(b[i]);return 'data:image/svg+xml;base64,'+btoa(s);}catch(_){return '';}}
const SYM={'\\alpha':'α','\\beta':'β','\\gamma':'γ','\\delta':'δ','\\theta':'θ','\\lambda':'λ','\\mu':'μ','\\pi':'π','\\sigma':'σ','\\omega':'ω','\\Delta':'Δ','\\Sigma':'∑','\\sum':'∑','\\prod':'∏','\\int':'∫','\\infty':'∞','\\pm':'±','\\times':'×','\\cdot':'·','\\leq':'≤','\\le':'≤','\\geq':'≥','\\ge':'≥','\\neq':'≠','\\ne':'≠','\\approx':'≈','\\to':'→','\\rightarrow':'→','\\Rightarrow':'⇒','\\in':'∈','\\forall':'∀','\\exists':'∃','\\partial':'∂','\\nabla':'∇'};
function fmtTex(s){
  let o=esc(s).replace(/\\frac\{([^{}]+)\}\{([^{}]+)\}/g,'($1)/($2)').replace(/\\sqrt\{([^{}]+)\}/g,'√($1)').replace(/\\text\{([^{}]+)\}/g,'$1');
  for(const[k,v]of Object.entries(SYM))o=o.split(k).join(v);
  return o.replace(/\^\{([^{}]+)\}/g,'<sup>$1</sup>').replace(/\^([0-9a-zA-Z])/g,'<sup>$1</sup>').replace(/_\{([^{}]+)\}/g,'<sub>$1</sub>').replace(/_([0-9a-zA-Z])/g,'<sub>$1</sub>');
}
function fmtFileLink(label,rawUrl){
  const clean=rawUrl.replace(/^file:\/\//,'');const[p,frag]=clean.split('#');
  const ln=frag?frag.replace(/^[Ll]/,'').split('-')[0]:'';
  return `<a class="file-chip" href="${esc(ideUri(p,ln))}">${esc(label)}</a>`;
}
function fmtIn(s){
  const codes=[];
  let o=s.replace(/`([^`\n]+)`/g,(_,c)=>{
    const m=c.match(/^([A-Za-z0-9_./-]+\.[a-z]{1,6})(?::(\d+)|#L(\d+))?$/);
    if(m&&(c.includes('/')||m[2]||m[3]||/^(rs|ts|tsx|js|py|go|md|toml|json|html|css)$/.test(c.split('.').pop()))){
      const base=(curWt||defaultWt||'').replace(/\/$/,'');const abs=m[1].startsWith('/')?m[1]:`${base}/${m[1]}`;
      codes.push(`<a class="file-chip" href="${esc(ideUri(abs,m[2]||m[3]||''))}"><code>${esc(c)}</code></a>`);
    }else codes.push(`<code>${esc(c)}</code>`);
    return `\x00C${codes.length-1}\x00`;
  });
  o=esc(o)
    .replace(/\\\((.+?)\\\)/g,(_,m)=>`<span class="math-in">${fmtTex(m)}</span>`)
    .replace(/(^|[^\\$])\$([^\s$][^$\n]*?[^\s$]|[^\s$])\$/g,(all,p,m)=>/^[0-9.,]+$/.test(m)?all:`${p}<span class="math-in">${fmtTex(m)}</span>`)
    .replace(/\*\*(.+?)\*\*/g,'<strong>$1</strong>')
    .replace(/\[([^\]]+)\]\((file:\/\/[^\s)"']+)\)/g,(_,l,u)=>fmtFileLink(l,u))
    .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)"']+)\)/g,(_,l,u)=>`<a href="${u}" target="_blank" rel="noopener noreferrer">${l}</a>`);
  return o.replace(/\x00C(\d+)\x00/g,(_,i)=>codes[+i]);
}
function syncArtPill(){
  const btn=document.getElementById('art-btn'),lbl=document.getElementById('art-lbl');
  if(!btn)return;
  const n=artStore.length;
  btn.style.display=(n>0||artOpen)?'inline-flex':'none';
  if(lbl)lbl.textContent=n>1?`Preview · ${n}`:'Preview';
}
function resetArtStore(){
  artStore=[];artMap.clear();curArt=null;curArtIdx=-1;
  if(artOpen)toggleArtDock(false);
  syncArtPill();
}
function registerArt(lang,raw){
  const key=lang+'::'+raw;
  if(artMap.has(key))return artMap.get(key);
  const id=artStore.length;
  artStore.push({lang,raw});
  if(artStore.length<500)artMap.set(key,id);
  syncArtPill();
  return id;
}
function renderMermaidSvg(raw){
  const lines=String(raw||'').split('\n').map(l=>l.trim()).filter(l=>l&&!l.startsWith('%%'));
  if(!lines.length)return '';
  const hdr=lines[0];
  const isLight=resolvedAppearance()==='light';
  const ac=palette==='claude'?(isLight?'#c96442':'#d97757'):palette==='codex'?'#10a37f':(isLight?'#4f46e5':'#6e7bf2');
  const nBg=isLight?'#ffffff':(palette==='claude'?'#22201d':palette==='codex'?'#212121':'#181b24');
  const nFg=isLight?'#18181b':'#eef0f6';
  const nSub=isLight?'#52525b':'#a1a1aa';
  const nBd=isLight?'#d4d4d8':'#3f3f46';
  const xmlEsc=s=>String(s||'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
  if(/^sequenceDiagram/i.test(hdr)){
    const parts=[],msgs=[];const pSet=new Map();
    const ensure=id=>{if(!pSet.has(id)){pSet.set(id,id);parts.push(id);}};
    for(const l of lines.slice(1)){
      const pm=l.match(/^participant\s+([^\s]+)(?:\s+as\s+(.+))?$/i);
      if(pm){pSet.set(pm[1],pm[2]||pm[1]);if(!parts.includes(pm[1]))parts.push(pm[1]);continue;}
      const mm=l.match(/^([^\s-]+?)\s*(->>|-->>|->|-->)\s*([^\s:]+)\s*:\s*(.+)$/);
      if(mm){ensure(mm[1]);ensure(mm[3]);msgs.push({a:mm[1],op:mm[2],b:mm[3],t:mm[4]});}
    }
    if(!parts.length)return '';
    const gap=156,w=Math.max(320,parts.length*gap+40),h=92+msgs.length*44;
    let g=`<defs><marker id="ma" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,1 L8,4 L0,7 z" fill="${ac}"/></marker></defs>`;
    parts.forEach((p,i)=>{
      const x=20+i*gap+gap/2,lbl=xmlEsc((pSet.get(p)||p).slice(0,22));
      g+=`<line x1="${x}" y1="44" x2="${x}" y2="${h-16}" stroke="${nBd}" stroke-dasharray="4 4"/>`;
      g+=`<rect x="${x-62}" y="12" width="124" height="30" rx="7" fill="${nBg}" stroke="${ac}"/><text x="${x}" y="31" text-anchor="middle" fill="${nFg}" font-family="system-ui,sans-serif" font-size="12" font-weight="600">${lbl}</text>`;
    });
    msgs.forEach((m,i)=>{
      const x1=20+parts.indexOf(m.a)*gap+gap/2,x2=20+parts.indexOf(m.b)*gap+gap/2,y=74+i*44;
      const dash=m.op.startsWith('--')?'stroke-dasharray="5 4"':'';
      g+=`<line x1="${x1}" y1="${y}" x2="${x2}" y2="${y}" stroke="${ac}" stroke-width="1.6" ${dash} marker-end="url(#ma)"/>`;
      g+=`<text x="${(x1+x2)/2}" y="${y-6}" text-anchor="middle" fill="${nSub}" font-family="ui-monospace,monospace" font-size="11">${xmlEsc(m.t.slice(0,38))}</text>`;
    });
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${w} ${h}">${g}</svg>`;
  }
  const lr=/\bLR\b/i.test(hdr);
  const nodes=new Map(),edges=[];
  const parseNode=rawTok=>{
    const m=rawTok.trim().match(/^([A-Za-z0-9_.-]+)(?:\s*[\[\(\{]+"?([^\]\)\}]+?)"?[\]\)\}]+)?$/);
    if(!m)return null;
    const id=m[1];if(!nodes.has(id))nodes.set(id,m[2]||id);else if(m[2])nodes.set(id,m[2]);
    return id;
  };
  for(const l of lines.slice(1)){
    if(/^(subgraph|end|style|classDef|class)\b/i.test(l))continue;
    const em=l.match(/^(.+?)\s*(?:-->|-\.->|==>|---)\s*(?:\|([^|]+)\|\s*)?(.+)$/);
    if(em){const a=parseNode(em[1]),b=parseNode(em[3]);if(a&&b)edges.push({a,b,lbl:em[2]||''});}
    else parseNode(l);
  }
  if(!nodes.size)return '';
  const rank=new Map();for(const k of nodes.keys())rank.set(k,0);
  for(let it=0;it<nodes.size;it++)for(const e of edges)rank.set(e.b,Math.max(rank.get(e.b)||0,Math.min(nodes.size,(rank.get(e.a)||0)+1)));
  const cols=new Map();
  for(const[k,r]of rank){if(!cols.has(r))cols.set(r,[]);cols.get(r).push(k);}
  const pos=new Map();let maxR=0,maxPer=1;
  for(const[r,arr]of cols){maxR=Math.max(maxR,r);maxPer=Math.max(maxPer,arr.length);arr.forEach((k,idx)=>pos.set(k,{r,idx,cnt:arr.length}));}
  const nw=152,nh=36,gx=lr?196:176,gy=lr?58:74;
  const w=lr?(maxR+1)*gx+32:Math.max(300,maxPer*gx+32),h=lr?Math.max(140,maxPer*gy+32):(maxR+1)*gy+28;
  const xy=k=>{const p=pos.get(k);return lr?{x:20+p.r*gx+nw/2,y:(h-(p.cnt-1)*gy)/2+p.idx*gy}:{x:(w-(p.cnt-1)*gx)/2+p.idx*gx,y:20+p.r*gy+nh/2};};
  let g=`<defs><marker id="fa" markerWidth="8" markerHeight="8" refX="7" refY="4" orient="auto"><path d="M0,1 L8,4 L0,7 z" fill="${ac}"/></marker></defs>`;
  for(const e of edges){
    const p1=xy(e.a),p2=xy(e.b);
    const x1=lr?p1.x+nw/2:p1.x,y1=lr?p1.y:p1.y+nh/2,x2=lr?p2.x-nw/2:p2.x,y2=lr?p2.y:p2.y-nh/2;
    g+=`<path d="M${x1},${y1} C${(x1+x2)/2},${y1} ${(x1+x2)/2},${y2} ${x2},${y2}" fill="none" stroke="${ac}" stroke-width="1.6" marker-end="url(#fa)"/>`;
    if(e.lbl)g+=`<text x="${(x1+x2)/2}" y="${(y1+y2)/2-4}" text-anchor="middle" fill="${nSub}" font-family="system-ui,sans-serif" font-size="10.5">${xmlEsc(e.lbl.slice(0,24))}</text>`;
  }
  for(const[k,lbl]of nodes){
    const c=xy(k);
    g+=`<rect x="${c.x-nw/2}" y="${c.y-nh/2}" width="${nw}" height="${nh}" rx="8" fill="${nBg}" stroke="${nBd}" stroke-width="1.3"/>`;
    g+=`<text x="${c.x}" y="${c.y+4}" text-anchor="middle" fill="${nFg}" font-family="system-ui,sans-serif" font-size="12" font-weight="500">${xmlEsc(lbl.slice(0,22))}</text>`;
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${w} ${h}">${g}</svg>`;
}
function openArtByIdx(id){
  const it=artStore[id];if(!it)return;
  curArt=it;curArtIdx=id;artTab='preview';
  toggleArtDock(true);
}
function setArtTab(t){artTab=t;renderArtDock();}
function toggleArtVp(){artVp=artVp==='desktop'?'mobile':'desktop';renderArtDock();}
function toggleArtWide(){
  artWide=!artWide;
  document.getElementById('art-dock').classList.toggle('wide',artWide);
  document.getElementById('av-wide')?.classList.toggle('active',artWide);
}
function renderArtDock(){
  if(!artOpen)return;
  document.getElementById('av-prev').classList.toggle('active',artTab==='preview');
  document.getElementById('av-src').classList.toggle('active',artTab==='source');
  const vpBtn=document.getElementById('av-vp');
  vpBtn.textContent=artVp==='mobile'?'Mobile 375':'Desktop';
  vpBtn.classList.toggle('active',artVp==='mobile');
  const selEl=document.getElementById('art-sel');
  if(selEl){
    selEl.style.display=artStore.length>1?'inline-block':'none';
    if(artStore.length>1)selEl.innerHTML=artStore.map((a,i)=>`<option value="${i}" ${i===curArtIdx?'selected':''}>v${i+1}/${artStore.length} · ${esc(a.lang.toUpperCase())}</option>`).join('');
  }
  const box=document.getElementById('art-canvas');
  if(!curArt){box.innerHTML='<div style="color:var(--fg-muted);font-size:13px">Click ▶ Preview on any HTML, SVG, or Mermaid code block to render it here.</div>';return;}
  document.getElementById('art-title').textContent=`${curArt.lang.toUpperCase()} Preview`;
  if(artTab==='source'){
    box.innerHTML=`<div class="code-box" style="width:100%;height:100%;margin:0;overflow:auto"><pre><code>${shCode(curArt.raw)}</code></pre></div>`;
    return;
  }
  if(curArt.lang==='image'){
    box.innerHTML=`<img src="${esc(curArt.raw)}" alt="preview" style="max-width:100%;max-height:100%;border-radius:10px;border:1px solid var(--border)">`;
    return;
  }
  if(curArt.lang==='svg'||curArt.lang==='mermaid'){
    const svg=curArt.lang==='mermaid'?renderMermaidSvg(curArt.raw):curArt.raw;
    const uri=svgDataUri(svg);
    box.innerHTML=uri?`<div class="svg-card" style="width:100%;margin:0"><img src="${uri}" alt="diagram" style="margin:0 auto"></div>`:`<pre>${esc(curArt.raw)}</pre>`;
    return;
  }
  box.innerHTML='';
  const fr=document.createElement('iframe');
  fr.className='art-iframe'+(artVp==='mobile'?' mob':'');
  fr.setAttribute('sandbox','allow-scripts');
  fr.srcdoc=curArt.raw;
  box.appendChild(fr);
}
function downloadArt(){
  if(!curArt)return toast('No active artifact');
  const ext=curArt.lang==='mermaid'?'svg':curArt.lang==='svg'?'svg':'html';
  const body=curArt.lang==='mermaid'?(renderMermaidSvg(curArt.raw)||curArt.raw):curArt.raw;
  const blob=new Blob([body],{type:ext==='svg'?'image/svg+xml':'text/html'});
  const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download=`artifact.${ext}`;a.click();
}

function fmtMd(src){
  const lines=(src||'').replace(/\r\n/g,'\n').split('\n');
  let out='',i=0;
  while(i<lines.length){
    const ln=lines[i],tr=ln.trim();
    if(tr.startsWith('```')){
      const lang=tr.slice(3).trim().toLowerCase();i++;let buf=[];
      while(i<lines.length&&!lines[i].trim().startsWith('```')){buf.push(lines[i]);i++;}
      i++;
      const raw=buf.join('\n');
      if(lang==='math'||lang==='latex'||lang==='tex')out+=`<div class="math-blk">${fmtTex(raw)}</div>`;
      else if(lang==='mermaid'){
        const aid=registerArt('mermaid',raw),svg=renderMermaidSvg(raw),uri=svg?svgDataUri(svg):'';
        out+=uri?`<div class="code-box"><div class="code-bar"><span>mermaid diagram</span><span style="display:inline-flex;gap:10px"><button class="meta-lnk" style="color:var(--accent)" data-aid="${aid}" onclick="openArtByIdx(+this.dataset.aid)">▶ Preview</button><button class="meta-lnk" data-aid="${aid}" onclick="copyText(artStore[+this.dataset.aid].raw,'Copied mermaid',this)">Copy</button></span></div><div style="padding:14px;background:var(--card)"><img src="${uri}" alt="mermaid diagram" style="max-width:100%;height:auto;display:block;margin:0 auto"></div></div>`:`<div class="code-box"><pre><code>${esc(raw)}</code></pre></div>`;
      }else if(lang==='svg'||raw.trim().startsWith('<svg')){
        const aid=registerArt('svg',raw),uri=svgDataUri(raw);
        out+=uri?`<div class="code-box"><div class="code-bar"><span>svg</span><span style="display:inline-flex;gap:10px"><button class="meta-lnk" style="color:var(--accent)" data-aid="${aid}" onclick="openArtByIdx(+this.dataset.aid)">▶ Preview</button><button class="meta-lnk" data-aid="${aid}" onclick="copyText(artStore[+this.dataset.aid].raw,'Copied svg',this)">Copy</button></span></div><div style="padding:14px;background:var(--card)"><img src="${uri}" alt="diagram" style="max-width:100%;height:auto;display:block"></div></div>`:`<div class="code-box"><pre><code>${esc(raw)}</code></pre></div>`;
      }else{
        const isHtml=lang==='html'||lang==='htm';
        const aid=isHtml?registerArt('html',raw):-1;
        const lnCnt=buf.length,longCls=lnCnt>32?' collapsed':'';
        const prevBtn=isHtml?`<button class="meta-lnk" style="color:var(--accent);font-weight:600" data-aid="${aid}" onclick="openArtByIdx(+this.dataset.aid)">▶ Preview</button>`:'';
        const expBar=lnCnt>32?`<button class="code-exp-bar" onclick="const b=this.closest('.code-box');b.classList.toggle('collapsed');this.textContent=b.classList.contains('collapsed')?'↕ Expand all ${lnCnt} lines':'↑ Collapse'">↕ Expand all ${lnCnt} lines</button>`:'';
        out+=`<div class="code-box${longCls}"><div class="code-bar"><span>${esc(lang||'code')} · ${lnCnt}L</span><span style="display:inline-flex;gap:10px">${prevBtn}<button class="meta-lnk" onclick="this.closest('.code-box').classList.toggle('wrap')">Wrap</button><button class="meta-lnk" onclick="copyText(this.closest('.code-bar').nextElementSibling.textContent,'Copied code',this)">Copy</button></span></div><pre><code>${shCode(raw)}</code></pre>${expBar}</div>`;
      }
      continue;
    }
    if(tr.startsWith('$$')||tr.startsWith('\\[')){
      const cl=tr.startsWith('$$')?'$$':'\\]';
      if(tr.length>2&&tr.endsWith(cl)){out+=`<div class="math-blk">${fmtTex(tr.slice(2,-2))}</div>`;i++;continue;}
      let buf=[tr.slice(2)];i++;
      while(i<lines.length&&!lines[i].trim().endsWith(cl)){buf.push(lines[i]);i++;}
      if(i<lines.length){buf.push(lines[i].trim().slice(0,-2));i++;}
      out+=`<div class="math-blk">${fmtTex(buf.join(' '))}</div>`;continue;
    }
    if(tr.includes('|')&&i+1<lines.length&&/^\|?[\s:-]+\|[\s|:-]*$/.test(lines[i+1].trim())){
      const cells=r=>r.trim().replace(/^\||\|$/g,'').split('|').map(c=>c.trim());
      const hdr=cells(ln);i+=2;let rows='';
      while(i<lines.length&&lines[i].trim().includes('|')){rows+=`<tr>${cells(lines[i]).map(c=>`<td>${fmtIn(c)}</td>`).join('')}</tr>`;i++;}
      out+=`<table><thead><tr>${hdr.map(c=>`<th>${fmtIn(c)}</th>`).join('')}</tr></thead><tbody>${rows}</tbody></table>`;continue;
    }
    if(!tr){i++;continue;}
    const hm=tr.match(/^(#{1,4})\s+(.+)$/);
    if(hm){const lv=hm[1].length+1;out+=`<h${lv}>${fmtIn(hm[2])}</h${lv}>`;i++;continue;}
    if(/^[-*]\s+/.test(tr)){
      let items='';while(i<lines.length&&/^[-*]\s+/.test(lines[i].trim())){items+=`<li>${fmtIn(lines[i].trim().replace(/^[-*]\s+/,''))}</li>`;i++;}
      out+=`<ul>${items}</ul>`;continue;
    }
    out+=`<p>${fmtIn(ln)}</p>`;i++;
  }
  return out;
}

/* Diff Parser + Granular Line Review Comments */
function intralineDiff(a,b){
  let p=0,la=a.length,lb=b.length,lim=Math.min(la,lb);
  while(p<lim&&a[p]===b[p])p++;
  let s=0,rem=Math.min(la-p,lb-p);
  while(s<rem&&a[la-1-s]===b[lb-1-s])s++;
  if(p+s<2)return null;
  return[
    `${esc(a.slice(0,p))}<mark class="d-int-del">${esc(a.slice(p,la-s))}</mark>${esc(a.slice(la-s))}`,
    `${esc(b.slice(0,p))}<mark class="d-int-add">${esc(b.slice(p,lb-s))}</mark>${esc(b.slice(lb-s))}`
  ];
}
function parseDiffFiles(rawDiff){
  if(!rawDiff||!rawDiff.trim())return[];
  return rawDiff.split(/(?=^diff --git )/m).filter(Boolean).map((c,idx)=>{
    const m=c.match(/^diff --git a\/(.+?) b\/(.+)$/m);
    const name=m?m[2]:`patch-${idx+1}`;
    let ins=0,del=0,oLn=0,nLn=0,rows=[];
    for(const l of c.split('\n')){
      if(l.startsWith('diff --git')||l.startsWith('index ')||l.startsWith('--- ')||l.startsWith('+++ '))continue;
      const hm=l.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/);
      if(hm){oLn=+hm[1];nLn=+hm[2];rows.push({t:'h',txt:l});continue;}
      if(l.startsWith('+')){ins++;rows.push({t:'a',o:'',n:nLn++,txt:l.slice(1)});}
      else if(l.startsWith('-')){del++;rows.push({t:'d',o:oLn++,n:'',txt:l.slice(1)});}
      else{rows.push({t:'c',o:oLn++,n:nLn++,txt:l.startsWith(' ')?l.slice(1):l});}
    }
    for(let i=0;i+1<rows.length;i++){
      if(rows[i].t==='d'&&rows[i+1].t==='a'){
        const pair=intralineDiff(rows[i].txt,rows[i+1].txt);
        if(pair){rows[i].hl=pair[0];rows[i+1].hl=pair[1];i++;}
      }
    }
    return{idx,name,ins,del,rows};
  });
}
async function addLineReview(file,ln){
  const note=await openInputModal({title:`Review comment · ${file}:${ln}`,sub:'Attached automatically to your next prompt',placeholder:'Describe what should change on this line…',okText:'Queue Comment'});
  if(!note||!note.trim())return;
  reviewNotes.push({loc:`${file}:${ln}`,note:note.trim()});
  renderAttachments();
  toast(`Queued comment on ${file}:${ln}`);
}
async function discardFileChanges(file){
  const ok=await openInputModal({title:`Discard changes in ${file}?`,sub:'Reverts uncommitted modifications in this single file back to HEAD.',okText:'Discard File',confirmOnly:true});
  if(!ok)return;
  const r=await api('/api/discard',{method:'POST',body:JSON.stringify({worktree:curWt||defaultWt,file})});
  if(r.error)return toast(r.error);
  toast(`Reverted ${file}`);lastDiffSig='';
  if(curSid)openSid(curSid);else await loadMeta();
  loadList();
}
function renderDiffDock(){
  if(!diffOpen)return;
  const raw=(detail&&detail.diff)||'';
  const sig=diffMode+'::'+raw;
  if(sig===lastDiffSig)return;
  lastDiffSig=sig;
  const files=parseDiffFiles(raw);
  const totA=files.reduce((s,f)=>s+f.ins,0),totD=files.reduce((s,f)=>s+f.del,0);
  document.getElementById('dd-stat').innerHTML=files.length?`<span class="c-add">+${totA}</span> <span class="c-del">-${totD}</span>`:'clean';
  document.getElementById('dd-files').innerHTML=files.map(f=>`<button class="df-pill" onclick="document.getElementById('dfb-${f.idx}')?.scrollIntoView({behavior:'smooth'})"><span>${esc(f.name.split('/').pop())}</span><span class="c-add">+${f.ins}</span><span class="c-del">-${f.del}</span></button>`).join('');
  if(!files.length){document.getElementById('dd-body').innerHTML='<div style="padding:24px;color:var(--fg-muted);font-size:13px">Working tree is clean.</div>';return;}
  document.getElementById('dd-body').innerHTML=files.map(f=>{
    const abs=`${(curWt||defaultWt||'').replace(/\/$/,'')}/${f.name}`;
    let trs='';
    for(const r of f.rows){
      const ln=r.n||r.o||1;
      const codeHtml=r.hl||shCode(r.txt);
      const cmtBtn=`<button class="d-cmt-btn" data-f="${esc(f.name)}" data-l="${ln}" onclick="event.stopPropagation();addLineReview(this.dataset.f,this.dataset.l)" title="Comment on line ${ln}">+</button>`;
      if(diffMode==='split'){
        if(r.t==='h')trs+=`<tr class="d-hunk-row"><td colspan="4">${esc(r.txt)}</td></tr>`;
        else if(r.t==='d')trs+=`<tr><td class="d-ln" style="background:var(--del-num);color:var(--err)">${cmtBtn}${r.o}</td><td style="background:var(--del-line)">${codeHtml}</td><td class="d-ln"></td><td></td></tr>`;
        else if(r.t==='a')trs+=`<tr><td class="d-ln"></td><td></td><td class="d-ln" style="background:var(--add-num);color:var(--ok)">${cmtBtn}${r.n}</td><td style="background:var(--add-line)">${codeHtml}</td></tr>`;
        else trs+=`<tr><td class="d-ln">${r.o}</td><td>${codeHtml}</td><td class="d-ln">${cmtBtn}${r.n}</td><td>${codeHtml}</td></tr>`;
      }else{
        if(r.t==='h')trs+=`<tr class="d-hunk-row"><td colspan="3">${esc(r.txt)}</td></tr>`;
        else{
          const cls=r.t==='a'?'d-add-row':r.t==='d'?'d-del-row':'';
          const pfx=r.t==='a'?'+':r.t==='d'?'-':' ';
          trs+=`<tr class="${cls}"><td class="d-ln">${r.o}</td><td class="d-ln">${cmtBtn}${r.n}</td><td>${pfx}${codeHtml}</td></tr>`;
        }
      }
    }
    const cols=diffMode==='split'?'<colgroup><col style="width:44px"><col><col style="width:44px"><col></colgroup>':'<colgroup><col style="width:44px"><col style="width:44px"><col></colgroup>';
    return `<div class="df-block" id="dfb-${f.idx}">
      <div class="df-hdr">
        <a class="file-chip" style="background:none;padding:0" href="${esc(ideUri(abs,''))}">${esc(f.name)}</a>
        <div style="display:flex;align-items:center;gap:10px">
          <button class="meta-lnk" style="color:var(--err)" data-f="${esc(f.name)}" onclick="discardFileChanges(this.dataset.f)" title="Revert uncommitted changes in this file">↺ Discard</button>
          <button class="meta-lnk" data-f="${esc(f.name)}" onclick="copyText(this.dataset.f,'Copied path',this)">Copy path</button>
          <span><span class="c-add">+${f.ins}</span> <span class="c-del">-${f.del}</span></span>
        </div>
      </div>
      <table class="d-table">${cols}<tbody>${trs}</tbody></table>
    </div>`;
  }).join('');
}

/* Metadata, Project-Grouped Sidebar & Dynamic Tab Title */
async function loadMeta(explicitWt){
  const qWt=explicitWt||curWt||defaultWt||'';
  const m=await api('/api/meta'+(qWt?'?wt='+encodeURIComponent(qWt):''));
  if(m.version)document.getElementById('ver').textContent='v'+m.version;
  if(m.default_repo){defaultWt=m.default_repo;if(!curWt)curWt=defaultWt;}
  if(typeof m.branch==='string')defaultBranch=m.branch||'main';
  if(typeof m.diff==='string'){defaultDiff=m.diff;if(!curSid&&detail){detail.diff=defaultDiff;lastDiffSig='';}}
  if(m.active_model)activeModel=m.active_model;
  if(m.effort&&!selEffort)selEffort=m.effort;
  if(Array.isArray(m.models))models=m.models;
  if(Array.isArray(m.skills))skills=m.skills;
  syncProjBar();syncComposerPills();renderModelPop();renderInspector();renderDiffDock();syncContextStrip();
  ensureRepoFiles();
}
async function ensureRepoFiles(){
  const wt=curWt||defaultWt;
  if(!wt||wt===filesWt)return;
  filesWt=wt;
  const r=await api('/api/files?wt='+encodeURIComponent(wt));
  if(Array.isArray(r.files))repoFiles=r.files;
}
function setFilter(f){
  filter=f;
  ['all','active','diff'].forEach(k=>document.getElementById('f-'+k.slice(0,3)).classList.toggle('active',f===k));
  renderList();
}
function toggleAllRepos(){
  allRepos=!allRepos;
  const b=document.getElementById('scope-btn');
  b.textContent=allRepos?'All':'Repo';
  b.classList.toggle('active',allRepos);
  syncProjBar();renderProjPop();
  loadList();
}
function updateDocTitle(){
  const waitCnt=sessions.filter(s=>s.status==='approval').length;
  const runCnt=sessions.filter(s=>s.status==='running').length;
  const cur=sessions.find(s=>s.sid===curSid);
  const name=(detail&&detail.title)||(cur&&cur.label)||'twig';
  if(waitCnt)document.title=`(${waitCnt}) ⛨ Approval · ${name}`;
  else if(runCnt)document.title=`● Working · ${name}`;
  else document.title=`${name} · twig`;
}
function renderList(){
  if(sbMode==='auto'){renderAutoSidebar();return;}
  const q=(document.getElementById('q').value||'').trim().toLowerCase();
  if(q.length>=2&&q!==searchQ){
    searchQ=q;clearTimeout(searchTm);
    searchTm=setTimeout(async()=>{
      const rq=allRepos?'&all=1':(defaultWt?'&repo='+encodeURIComponent(defaultWt):'');
      const r=await api('/api/search?q='+encodeURIComponent(q)+rq);
      searchHits=r.hits||[];renderList();
    },220);
  }else if(q.length<2&&searchQ){searchQ='';searchHits=[];}
  const byParent={};
  for(const s of sessions){if(s.parent_sid)(byParent[s.parent_sid]=byParent[s.parent_sid]||[]).push(s);}
  const roots=sessions.filter(s=>!s.parent_sid||!sessions.some(p=>p.sid===s.parent_sid));
  const match=s=>{
    if(filter==='active'&&s.status!=='running'&&s.status!=='approval')return false;
    if(filter==='diff'&&s.status!=='dirty')return false;
    if(q&&!`${s.label} ${s.branch||''} ${s.worktree||''} ${s.sid}`.toLowerCase().includes(q))return false;
    return true;
  };
  const card=(s,child=false)=>{
    const proj=baseName(s.worktree||defaultWt);
    const isPin=pinned.has(s.sid);
    const hasDraft=!!(drafts[s.sid]&&drafts[s.sid].trim());
    let stHtml=`<span class="t-status calm">${hasDraft?'<span class="draft-pill">draft</span> ':''}${esc(shortAge(s.age))}</span>`;
    if(s.status==='approval')stHtml=`<span class="t-status st-wait">⛨ Approval</span>`;
    else if(s.status==='running')stHtml=`<span class="t-status st-run shimmer">● Working</span>`;
    const diffHtml=(s.diff_add||s.diff_del)?`<span class="c-add">+${s.diff_add||0}</span><span class="c-del">-${s.diff_del||0}</span>`:`<span style="opacity:.6">t${s.turns||0}</span>`;
    return `<div class="t-card ${child?'child':''} ${s.status==='settled'?'settled':''} ${s.sid===curSid?'active':''}" data-sid="${esc(s.sid)}" onclick="openSid(this.dataset.sid)">
      <div class="t-row1">
        ${child?'<span>↳</span>':projFav(proj)}
        <span class="proj-name">${esc(child?(s.role||'subagent'):proj)}</span>
        ${stHtml}
        <span class="t-hover-acts">
          ${child?'':`<button class="t-act-ic ${isPin?'pinned':''}" data-sid="${esc(s.sid)}" title="Pin thread" onclick="event.stopPropagation();togglePin(this.dataset.sid)">★</button>`}
          <button class="t-act-ic" data-sid="${esc(s.sid)}" title="Archive thread" onclick="event.stopPropagation();archiveSid(this.dataset.sid)">×</button>
        </span>
      </div>
      <div class="t-title">${esc(s.label)}</div>
      <div class="t-row3"><span class="t-branch"><span class="ico sm"><svg viewBox="0 0 24 24"><line x1="6" y1="3" x2="6" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/></svg></span>${esc(s.branch||'main')}</span><span class="t-stat">${diffHtml}</span></div>
    </div>`;
  };

  const visibleRoots=roots.filter(r=>match(r)||(byParent[r.sid]||[]).some(match));
  const pinRoots=visibleRoots.filter(r=>pinned.has(r.sid));
  const restRoots=visibleRoots.filter(r=>!pinned.has(r.sid));

  let html='';
  if(pinRoots.length){
    html+=`<div class="grp-hdr"><span>★ Pinned</span><span>${pinRoots.length}</span></div>`;
    for(const r of pinRoots){html+=card(r,false);for(const k of(byParent[r.sid]||[]))if(match(k))html+=card(k,true);}
  }
  const byProj={};
  for(const r of restRoots){const p=baseName(r.worktree||defaultWt);(byProj[p]=byProj[p]||[]).push(r);}
  const projKeys=Object.keys(byProj);
  for(const p of projKeys){
    const shut=!!shutGroups[p];
    if(projKeys.length>1||pinRoots.length){
      html+=`<div class="grp-hdr" data-p="${esc(p)}" onclick="shutGroups[this.dataset.p]=!shutGroups[this.dataset.p];renderList()"><span>${shut?'▸':'▾'} ${esc(p)}</span><span>${byProj[p].length}</span></div>`;
    }
    if(!shut){
      for(const r of byProj[p]){html+=card(r,false);for(const k of(byParent[r.sid]||[]))if(match(k))html+=card(k,true);}
    }
  }
  if(q.length>=2&&searchHits.length){
    html+=`<div class="grp-hdr"><span>✦ Transcript Matches</span><span>${searchHits.length}</span></div>`;
    for(const h of searchHits){
      const snip=(h.matches&&h.matches[0])||'';
      html+=`<div class="t-card ${h.sid===curSid?'active':''}" data-sid="${esc(h.sid)}" onclick="openSid(this.dataset.sid)">
        <div class="t-row1"><span class="proj-name" style="color:var(--accent)">${esc(h.sid.slice(0,12))}</span></div>
        <div class="t-title" style="font-size:11.5px;white-space:normal;line-height:1.35">${esc(snip)}</div>
      </div>`;
    }
  }
  document.getElementById('slist').innerHTML=html||'<div style="padding:18px 12px;font-size:12.5px;color:var(--fg-muted)">No matching threads</div>';
  document.getElementById('sb-count').textContent=`${sessions.length} thread${sessions.length===1?'':'s'}`;
  updateDocTitle();
}

async function toggleNotif(){
  if(!('Notification'in window))return toast('Browser notifications unsupported');
  if(!notifOn){const p=await Notification.requestPermission();if(p!=='granted')return toast('Permission denied');notifOn=true;}
  else notifOn=false;
  localStorage.setItem('twig_notif',notifOn?'1':'0');
  document.getElementById('notif-btn').classList.toggle('on',notifOn);
  toast(notifOn?'Background alerts enabled':'Background alerts off');
}
function maybeNotify(list){
  if(!notifOn||!('Notification'in window)||Notification.permission!=='granted')return;
  for(const s of list){
    const prev=prevStates[s.sid];
    if(prev&&prev!==s.status&&document.hidden){
      if(s.status==='approval')new Notification('twig · Approval needed',{body:s.label});
      else if(prev==='running'&&s.status!=='running')new Notification('twig · Turn complete',{body:s.label});
    }
    prevStates[s.sid]=s.status;
  }
}
async function loadList(){
  const qs=allRepos?'?all=1':(defaultWt?'?repo='+encodeURIComponent(defaultWt):'');
  const d=await api('/api/sessions'+qs);
  if(!d.sessions)return;
  if(!defaultWt&&d.default_repo){defaultWt=d.default_repo;syncProjBar();}
  if(!curWt&&defaultWt)curWt=defaultWt;
  sessions=d.sessions;
  maybeNotify(sessions);
  renderList();
  if(!curSid&&!draftNew){
    if(sessions.length)openSid(sessions[0].sid);
    else newThread(false);
  }
}

/* Thread Navigation & Timeline */
function ensureThreadsMode(){
  if(sbMode!=='threads'){
    sbMode='threads';
    document.getElementById('m-thr').classList.add('active');
    document.getElementById('m-aut').classList.remove('active');
    document.getElementById('sb-thr-filters').style.display='flex';
  }
}
async function openSid(sid,resetScroll=false,fromPoll=false){
  if(!fromPoll)ensureThreadsMode();
  const changed=sid!==curSid||draftNew;
  if(!fromPoll&&changed)saveCurDraft();
  draftNew=false;newWt=false;
  if(changed){forceTail=true;stickToBottom=true;resetArtStore();}
  if(resetScroll){forceTail=true;stickToBottom=true;}
  curSid=sid;
  if(!fromPoll&&changed)restoreCurDraft();
  if(!fromPoll&&innerWidth<=760)closeMobilePanels();
  detail=await api('/api/session/'+sid);
  curWt=detail.worktree||curWt||defaultWt;
  if(!fromPoll&&detail){
    const nextMode=detail.plan?'plan':detail.gate_edits?'review':'auto';
    if(nextMode!==execMode)pickExecMode(nextMode,true);
  }
  ensureRepoFiles();syncWtUi();render();renderList();
}
function openParent(){if(detail&&detail.parent_sid)openSid(detail.parent_sid,true);}
function newThread(withWorktree=false){
  ensureThreadsMode();
  saveCurDraft();
  draftNew=true;curSid='';curWt=defaultWt;newWt=!!withWorktree;
  forceTail=true;stickToBottom=true;lastHtml='';resetArtStore();
  detail={messages:[],pending:[],todos:[],children:[],usage:{},diff:defaultDiff,branch:defaultBranch,worktree:defaultWt,running:false};
  restoreCurDraft();
  if(innerWidth<=760)closeMobilePanels();
  syncWtUi();render();renderList();
  document.getElementById('inp').focus();
}
function toggleNewWt(){
  if(curSid){newThread(true);toast('Started isolated worktree thread');return;}
  newWt=!newWt;syncWtUi();
}
function syncWtUi(){
  const b=document.getElementById('wt-toggle'),lbl=document.getElementById('wt-lbl'),inp=document.getElementById('wt-slug');
  b.classList.toggle('active',!curSid&&newWt);
  lbl.textContent=(!curSid&&newWt)?'New worktree':'Worktree';
  inp.classList.toggle('show',!curSid&&newWt);
}
function syncContextStrip(){
  const wt=(detail&&detail.worktree)||curWt||defaultWt||'';
  const proj=baseName(wt);
  const br=(detail&&detail.branch)||defaultBranch||'main';
  document.getElementById('ctx-repo').textContent=proj;
  document.getElementById('ctx-branch').textContent=br;
  document.getElementById('crumb-proj').innerHTML=`${projFav(proj)}<span>${esc(proj)}</span>`;
}
function prefillCmd(p){
  const inp=document.getElementById('inp');
  if(p==='/review --fix')inp.value=p;
  else inp.value=p+inp.value.replace(/^\/goal\s+/,'');
  autoGrow(inp);inp.focus();
}

function summarizeToolCall(name,rawArgs){
  let a={};try{a=typeof rawArgs==='string'?JSON.parse(rawArgs||'{}'):(rawArgs||{});}catch(_){}
  if(name==='read')return['Read',a.path||'file'];
  if(name==='edit')return['Edit',a.path||'file'];
  if(name==='write')return['Write',a.path||'file'];
  if(name==='apply_patch')return['Patch','multi-file patch'];
  if(name==='bash')return['Bash',a.command||'command'];
  if(name==='grep')return['Search',`${a.pattern||''} ${a.path||''}`.trim()];
  if(name==='glob')return['Glob',a.pattern||'*'];
  if(name==='subagent')return[`Subagent (${a.role||'scout'})`,a.task||'task'];
  if(name==='skill')return['Skill',a.name||a.query||'catalog'];
  if(name==='todo')return['Plan',`${(a.items||[]).length} step(s)`];
  return[name,JSON.stringify(a).slice(0,80)];
}
function groupTimeline(msgs,snaps){
  const toolOut={};
  for(const m of msgs){if(m.role==='tool'&&m.tool_call_id)toolOut[m.tool_call_id]=typeof m.content==='string'?m.content:JSON.stringify(m.content);}
  const items=[];let curWork=null,uCounter=0;userTurnTexts=[];
  const flushWork=()=>{if(curWork&&curWork.calls.length)items.push(curWork);curWork=null;};
  for(let i=0;i<msgs.length;i++){
    const m=msgs[i];
    if(m.role==='system'||m.role==='tool')continue;
    if(m.role==='user'){
      flushWork();
      let txt='',uImgs=[];
      if(typeof m.content==='string')txt=m.content;
      else if(Array.isArray(m.content)){
        txt=m.content.filter(x=>x.type==='text'||typeof x.text==='string').map(x=>x.text||'').join('\n');
        uImgs=m.content.filter(x=>x.type==='image_url'&&x.image_url?.url).map(x=>x.image_url.url);
      }else txt=JSON.stringify(m.content);
      const uIdx=uCounter++;
      userTurnTexts.push(txt);
      items.push({kind:'user',uIdx,txt,imgs:uImgs,hasSnap:!!(snaps&&snaps[uIdx])});
      continue;
    }
    if(m.role==='assistant'){
      const calls=Array.isArray(m.tool_calls)?m.tool_calls:[];
      const txt=typeof m.content==='string'?m.content.trim():'';
      if(calls.length){
        if(!curWork)curWork={kind:'work',id:`wl-${i}`,calls:[]};
        for(const c of calls){
          const cid=c.id||`c-${i}`;
          const fn=(c.function&&c.function.name)||'tool';
          const[verb,arg]=summarizeToolCall(fn,c.function&&c.function.arguments);
          curWork.calls.push({id:cid,fn,verb,arg,out:toolOut[cid]||''});
        }
      }
      if(txt){flushWork();items.push({kind:'assistant',txt});}
    }
  }
  flushWork();
  return items;
}
function toggleWorklog(id){openLogs[id]=!openLogs[id];document.getElementById(id)?.classList.toggle('open',!!openLogs[id]);}
function toggleToolItem(id){openTools[id]=!openTools[id];document.getElementById(id)?.classList.toggle('open',!!openTools[id]);}

function renderHero(){
  const proj=baseName(curWt||defaultWt);
  return `<div class="hero">
    <span class="hero-badge"><span style="color:var(--accent)">✻</span> Studio Cockpit · <kbd>⌘K</kbd> Command Palette · <kbd>Ctrl+;</kbd> Side Chat</span>
    <h1 class="hero-title">What should we build in <span class="hero-proj-pill">${projFav(proj)}<span>${esc(proj)}</span></span>?</h1>
    <p class="hero-sub">Run parallel Git worktree threads, inspect side-by-side diffs with inline line comments, or launch autonomous multi-wave goals.</p>
    <div class="hero-grid">
      <button class="starter" onclick="prefillCmd('/goal ')">
        <span class="starter-t"><span style="color:var(--accent)">✦</span> Autonomous /goal</span>
        <span class="starter-d">Execute end-to-end waves with todo tracking & blocker-gated review</span>
      </button>
      <button class="starter" onclick="newThread(true)">
        <span class="starter-t"><span style="color:var(--ok)">⑂</span> Isolated worktree feature</span>
        <span class="starter-d">Spin up a clean branch checkout without touching your main tree</span>
      </button>
      <button class="starter" onclick="setQuickPrompt('Audit uncommitted changes for bugs, security risks, and missing edge-case tests.')">
        <span class="starter-t"><span style="color:var(--warn)">⛨</span> Deep diff & security review</span>
        <span class="starter-d">Cross-check current changes for regressions and boundary bugs</span>
      </button>
      <button class="starter" onclick="setQuickPrompt('Map the architecture of this repository, module boundaries, and key entrypoints.')">
        <span class="starter-t"><span>◈</span> Architecture walkthrough</span>
        <span class="starter-d">Trace core dataflow, entrypoints, and invariants with file links</span>
      </button>
    </div>
  </div>`;
}
function setQuickPrompt(t){const inp=document.getElementById('inp');inp.value=t;autoGrow(inp);inp.focus();}

function renderInspector(){
  const wt=(detail&&detail.worktree)||curWt||defaultWt||'';
  const proj=baseName(wt);
  const br=(detail&&detail.branch)||defaultBranch||'main';
  document.getElementById('env-proj').textContent=proj;
  document.getElementById('env-wt').textContent=wt||'—';
  document.getElementById('env-branch').textContent=br;
  document.getElementById('open-ed').href=ideUri(wt,'');

  const pr=detail&&detail.pr;
  const prSlot=document.getElementById('env-pr-slot');
  if(pr&&pr.url){
    const num=pr.number||String(pr.url).split('/').pop()||'PR';
    prSlot.innerHTML=`<a class="pr-badge" href="${esc(pr.url)}" target="_blank" rel="noopener noreferrer">PR #${esc(num)} ↗</a>`;
  }else prSlot.innerHTML='';

  const files=parseDiffFiles((detail&&detail.diff)||'');
  const totA=files.reduce((s,f)=>s+f.ins,0),totD=files.reduce((s,f)=>s+f.del,0);
  document.getElementById('env-diff-stat').innerHTML=files.length?`<span class="c-add">+${totA}</span> <span class="c-del">-${totD}</span>`:'clean';
  document.getElementById('top-diff-stat').innerHTML=files.length?`Diff <span class="c-add">+${totA}</span> <span class="c-del">-${totD}</span>`:'Diff';

  const todos=(detail&&detail.todos)||[];
  const todoCard=document.getElementById('todo-card');
  todoCard.style.display=todos.length?'block':'none';
  if(todos.length){
    const done=todos.filter(t=>t.status==='completed').length;
    document.getElementById('todo-ttl').textContent=`Plan progress (${done}/${todos.length})`;
    document.getElementById('todo-list').innerHTML=todos.map(t=>{
      const g=t.status==='completed'?'✓':t.status==='in_progress'?'●':'○';
      const col=t.status==='completed'?'var(--ok)':t.status==='in_progress'?'var(--accent)':'var(--fg-muted)';
      return `<div class="todo-it ${t.status==='completed'?'done':''}"><span style="color:${col}">${g}</span><span>${esc(t.content)}</span></div>`;
    }).join('');
  }

  const kids=(detail&&detail.children)||[];
  const subCard=document.getElementById('sub-card');
  subCard.style.display=kids.length?'block':'none';
  if(kids.length){
    document.getElementById('sub-list').innerHTML=kids.map(k=>`<div class="t-card" style="padding:7px 8px;margin:0;border:1px solid var(--border)" data-sid="${esc(k.sid)}" onclick="openSid(this.dataset.sid)">
      <div class="t-row1"><span style="color:var(--ok);font-weight:600">${esc(k.role||'sub')}</span><span class="t-status">${k.tool_count||0} tools</span></div>
      <div class="t-title" style="font-size:12px">${esc(k.task||k.sid)}</div>
      <div style="display:flex;justify-content:flex-end;margin-top:4px"><button class="meta-lnk" style="color:var(--accent)" data-sid="${esc(k.sid)}" onclick="event.stopPropagation();applySubagentPatch(this.dataset.sid)">Apply patch</button></div>
    </div>`).join('');
  }

  const u=(detail&&detail.usage)||{};
  const inp=u.input_tokens||0,out=u.output_tokens||0,cached=u.cached_input_tokens||0;
  const pct=inp>0?Math.min(100,Math.round((cached*100)/inp)):0;
  const fmtK=n=>n>=1000?(n/1000).toFixed(1)+'k':String(n);
  document.getElementById('usage-kv').textContent=`${fmtK(inp)} in · ${fmtK(out)} out · cache ${pct}%`;
  document.getElementById('cache-bar').style.width=pct+'%';

  const ctxBytes=(detail&&detail.context_bytes)||0,ctxMax=184320;
  const ctxPct=Math.min(100,Math.round((ctxBytes*100)/ctxMax));
  const ctxBar=document.getElementById('ctx-bar');
  document.getElementById('ctx-kv').textContent=`Context: ${(ctxBytes/1024).toFixed(1)} KB / 180 KB (${ctxPct}%)`;
  ctxBar.style.width=ctxPct+'%';
  ctxBar.style.background=ctxPct>82?'var(--warn)':'var(--ok)';
}

function syncJumpPill(){
  const pill=document.getElementById('jump-latest');if(!pill)return;
  const show=sbMode==='threads'&&!stickToBottom&&!!(detail&&((detail.messages&&detail.messages.length)||detail.running));
  pill.classList.toggle('show',show);
  const live=!!(detail&&detail.running);
  pill.classList.toggle('live',live);
  pill.textContent=live?'↓ Live output · Jump to latest':'↓ Jump to bottom';
}
function jumpToBottom(){
  const pane=document.getElementById('pane');
  stickToBottom=true;progScroll=true;
  pane.scrollTo({top:pane.scrollHeight,behavior:'smooth'});
  setTimeout(()=>{progScroll=false;syncJumpPill();},260);
}

function syncActionBtn(){
  const actBtn=document.getElementById('act-btn'),actIco=document.getElementById('act-ico'),stopPill=document.getElementById('stop-pill-btn');
  const running=!!(detail&&detail.running);
  const hasTxt=!!(document.getElementById('inp')?.value.trim());
  actBtn.classList.remove('stop','steer');
  stopPill.classList.toggle('show',running&&hasTxt);
  if(running&&hasTxt){
    actBtn.classList.add('steer');actBtn.title='Steer active turn live (Enter)';
    actIco.innerHTML='<svg viewBox="0 0 24 24"><path d="M9 10l-5 5 5 5"/><path d="M20 4v7a4 4 0 0 1-4 4H4"/></svg>';
  }else if(running){
    actBtn.classList.add('stop');actBtn.title='Stop active turn';
    actIco.innerHTML='<svg viewBox="0 0 24 24"><rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor" stroke="none"/></svg>';
  }else{
    actBtn.title='Send (Enter)';
    actIco.innerHTML='<svg viewBox="0 0 24 24"><path d="M12 19V5M5 12l7-7 7 7"/></svg>';
  }
}

function render(){
  if(sbMode==='auto'){renderAutoStage();return;}
  if(!detail)return;
  document.querySelector('.composer-dock').style.display='';
  document.getElementById('parent-btn').style.display=detail.parent_sid?'inline-flex':'none';
  const sMeta=sessions.find(s=>s.sid===curSid);
  const stEl=document.getElementById('stitle');
  if(!stEl.querySelector('input'))stEl.textContent=detail.title||(sMeta&&sMeta.label)||(curSid?curSid:'New thread');
  syncContextStrip();renderInspector();renderDiffDock();updateDocTitle();syncActionBtn();

  const pane=document.getElementById('pane'),inner=document.getElementById('tl-inner');
  if(forceTail)stickToBottom=true;
  const grouped=groupTimeline(detail.messages||[],detail.snapshots||[]);
  const pending=detail.pending||[];
  const diffFiles=parseDiffFiles(detail.diff||'');
  const kids=detail.children||[];
  const streamTxt=(detail.running&&curSid&&liveText[curSid])||'';
  const activeTl=(detail.running&&curSid&&liveTool[curSid])||null;
  const steered=(detail.running&&curSid&&liveSteer[curSid])||'';

  let html='';
  if(!grouped.length&&!pending.length&&!detail.running){
    html=renderHero();
  }else{
    html+=grouped.map(it=>{
      if(it.kind==='user'){
        const imgRow=it.imgs&&it.imgs.length?`<div class="u-imgs">${it.imgs.map(u=>{const aid=registerArt('image',u);return `<img src="${esc(u)}" alt="attachment" data-aid="${aid}" onclick="openArtByIdx(+this.dataset.aid)">`;}).join('')}</div>`:'';
        const revCodeBtn=(!detail.running&&it.hasSnap)?`<button class="meta-lnk" style="color:var(--accent)" data-u="${it.uIdx}" onclick="rewindTurn(+this.dataset.u,true)" title="Restore worktree files & chat to before this turn">↺ Restore</button>`:'';
        const revChatBtn=!detail.running?`<button class="meta-lnk" data-u="${it.uIdx}" onclick="rewindTurn(+this.dataset.u,false)" title="Rewind conversation to this turn without reverting files">↶ Rewind chat</button>`:'';
        return `<div class="u-row">
          <div class="u-bubble">${imgRow}${esc(it.txt)}</div>
          <div class="msg-acts">
            ${revCodeBtn}${revChatBtn}
            <button class="meta-lnk" data-u="${it.uIdx}" onclick="copyText(userTurnTexts[+this.dataset.u]||'','Copied prompt',this)">Copy</button>
            <button class="meta-lnk" onclick="forkCurrent()">⑂ Fork</button>
          </div>
        </div>`;
      }
      if(it.kind==='work'){
        const counts={};for(const c of it.calls)counts[c.verb]=(counts[c.verb]||0)+1;
        const bd=Object.entries(counts).map(([k,v])=>`${v} ${k.toLowerCase()}`).join(' · ');
        const isOpen=!!openLogs[it.id];
        const rows=it.calls.map(c=>{
          const tId=`ti-${c.id}`;
          return `<div class="wl-item ${openTools[tId]?'open':''}" id="${esc(tId)}">
            <button class="wl-hdr" data-tid="${esc(tId)}" onclick="toggleToolItem(this.dataset.tid)">
              <span style="font-weight:600;color:var(--fg)">${esc(c.verb)}</span>
              <span class="wl-arg">${esc(c.arg)}</span>
            </button>
            <div class="wl-out">${esc((c.out||'(no output)').slice(0,2400))}</div>
          </div>`;
        }).join('');
        return `<div class="worklog ${isOpen?'open':''}" id="${esc(it.id)}">
          <button class="wl-sum" data-wid="${esc(it.id)}" onclick="toggleWorklog(this.dataset.wid)">
            <span>Worked · ${it.calls.length} tool${it.calls.length===1?'':'s'} (${esc(bd)})</span>
            <span class="wl-chev"><span class="ico sm"><svg viewBox="0 0 24 24"><path d="m9 18 6-6-6-6"/></svg></span></span>
          </button>
          <div class="wl-items">${rows}</div>
        </div>`;
      }
      return `<div class="a-row">${fmtMd(it.txt)}<div class="msg-acts" style="justify-content:flex-end;margin-top:4px">
        <button class="meta-lnk" onclick="copyText(${esc(JSON.stringify(it.txt))},'Copied reply',this)">Copy</button>
        <button class="meta-lnk" onclick="forkCurrent()">⑂ Fork thread</button>
        <button class="meta-lnk" onclick="toggleSideChat('Explain this part in more detail: ')">💬 Ask Side chat</button>
      </div></div>`;
    }).join('');

    if(kids.length){
      html+=`<div class="sub-swarm">${kids.map(k=>`<div class="sub-tile">
        <div style="display:flex;align-items:center;justify-content:space-between;gap:6px">
          <span class="f-pill" style="color:var(--ok);height:20px">↳ ${esc(k.role||'scout')}</span>
          <span style="font:11px/1 ui-monospace,monospace;color:var(--fg-muted)">${k.tool_count||0} tools · ${k.turns||0}t</span>
        </div>
        <div style="font-size:12.5px;color:var(--fg);white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${esc(k.task||k.sid)}</div>
        <div style="display:flex;align-items:center;justify-content:space-between;margin-top:2px">
          <button class="meta-lnk" data-sid="${esc(k.sid)}" onclick="openSid(this.dataset.sid)">Inspect thread →</button>
          <button class="meta-lnk" style="color:var(--accent)" data-sid="${esc(k.sid)}" onclick="applySubagentPatch(this.dataset.sid)">Apply patch</button>
        </div>
      </div>`).join('')}</div>`;
    }

    if(steered)html+=`<div class="wl-sum" style="color:var(--accent)">↳ Queued live steer: ${esc(steered)}</div>`;
    if(streamTxt)html+=`<div class="a-row">${fmtMd(streamTxt)}<span class="live-cursor"></span></div>`;
    if(detail.running){
      const actLbl=activeTl?`● ${activeTl[0]} · ${activeTl[1]}`:'● Working…';
      html+=`<div class="wl-sum shimmer">${esc(actLbl)}</div>`;
    }
    html+=pending.map(a=>`<div class="ap-card">
      <div class="ap-eyebrow">⛨ Approval needed · ${esc(a.name)}</div>
      <div class="ap-cmd">${esc(a.args&&a.args.command?a.args.command:JSON.stringify(a.args))}</div>
      <input class="ap-note-in" id="ap-note-${esc(a.id)}" placeholder="Optional feedback or alternative instruction if declining…">
      <div class="ap-btns">
        <button class="ap-allow" data-id="${esc(a.id)}" onclick="approve(this.dataset.id,true,false)">Allow & continue</button>
        <button class="ap-rem" data-id="${esc(a.id)}" onclick="approve(this.dataset.id,true,true)" title="Remember command pattern for this session">Allow & remember</button>
        <button class="ap-deny" data-id="${esc(a.id)}" onclick="approve(this.dataset.id,false,false)">Decline</button>
      </div>
    </div>`).join('');

    if(diffFiles.length){
      const totA=diffFiles.reduce((s,f)=>s+f.ins,0),totD=diffFiles.reduce((s,f)=>s+f.del,0);
      html+=`<div class="changes-card">
        <div class="cc-hdr">
          <div class="cc-left">
            <span>${diffFiles.length} changed file${diffFiles.length===1?'':'s'}</span>
            <span style="font-family:ui-monospace,monospace;font-size:12px"><span class="c-add">+${totA}</span> <span class="c-del">-${totD}</span></span>
          </div>
          <div class="cc-right">
            <button class="icon-btn" onclick="ccShut=!ccShut;lastHtml='';render()" title="Toggle file list"><span class="ico sm"><svg viewBox="0 0 24 24"><path d="m7 15 5 5 5-5M7 9l5-5 5 5"/></svg></span></button>
            <button class="btn-xs" onclick="toggleDiffPanel(true)">Open diff</button>
          </div>
        </div>
        <div class="cc-files ${ccShut?'shut':''}">
          ${diffFiles.map(f=>`<div class="cc-file" onclick="toggleDiffPanel(true);setTimeout(()=>document.getElementById('dfb-${f.idx}')?.scrollIntoView({behavior:'smooth'}),60)">
            <span>${esc(f.name)}</span>
            <span><span class="c-add">+${f.ins}</span> <span class="c-del">-${f.del}</span></span>
          </div>`).join('')}
        </div>
      </div>`;
    }
  }

  if(html!==lastHtml){
    const prevTop=pane.scrollTop;
    inner.innerHTML=html;
    lastHtml=html;
    if(stickToBottom){progScroll=true;pane.scrollTop=pane.scrollHeight;progScroll=false;}
    else pane.scrollTop=prevTop;
  }else if(forceTail){
    progScroll=true;pane.scrollTop=pane.scrollHeight;progScroll=false;
  }
  forceTail=false;
  syncJumpPill();
}

/* Inline Composer Autocomplete (/, @, $) */
function triggerToken(ch){
  const inp=document.getElementById('inp');
  const v=inp.value;
  inp.value=(v&&!v.endsWith(' ')?v+' ':v)+ch;
  inp.focus();updateAc();
}
function closeAc(){acItems=[];document.getElementById('ac-menu').classList.remove('open');}
function updateAc(){
  const inp=document.getElementById('inp'),pos=inp.selectionStart||0,before=inp.value.slice(0,pos);
  const m=before.match(/(?:^|\s)([/@$][A-Za-z0-9_./-]*)$/);
  if(!m)return closeAc();
  const tok=m[1],sig=tok[0],q=tok.slice(1).toLowerCase();
  let list=[];
  if(sig==='/'&&before.trim()===tok){
    list=SLASH_CMDS.filter(x=>x.k.toLowerCase().includes('/'+q)).map(x=>({ins:x.k,lbl:x.k.trim(),desc:x.d,full:true}));
  }else if(sig==='@'){
    const specials=[{ins:'@diff ',lbl:'@diff',desc:'Attach uncommitted git diff'},{ins:'@staged ',lbl:'@staged',desc:'Attach staged git diff'}];
    const fHits=repoFiles.filter(f=>f.toLowerCase().includes(q)).slice(0,12).map(f=>({ins:`@${f} `,lbl:`@${f}`,desc:'Repository file'}));
    list=[...specials.filter(x=>x.lbl.includes(q)),...fHits];
  }else if(sig==='$'){
    list=skills.filter(s=>s.name.toLowerCase().includes(q)).slice(0,12).map(s=>({ins:`$${s.name} `,lbl:`$${s.name}`,desc:`[${s.scope}] ${s.desc}`}));
  }
  if(!list.length)return closeAc();
  acItems=list;acIdx=0;renderAc();
}
function renderAc(){
  const menu=document.getElementById('ac-menu');
  menu.classList.add('open');
  menu.innerHTML=acItems.map((it,i)=>`<button class="ac-row ${i===acIdx?'sel':''}" onmousedown="event.preventDefault();applyAc(${i})"><span class="ac-key">${esc(it.lbl)}</span><span class="ac-desc">${esc(it.desc)}</span></button>`).join('');
}
function applyAc(i){
  const it=acItems[i];if(!it)return;
  const inp=document.getElementById('inp'),pos=inp.selectionStart||0,before=inp.value.slice(0,pos),after=inp.value.slice(pos);
  if(it.full){inp.value=it.ins;}
  else{
    const next=before.replace(/([/@$][A-Za-z0-9_./-]*)$/,it.ins);
    inp.value=next+after;
    inp.selectionStart=inp.selectionEnd=next.length;
  }
  closeAc();autoGrow(inp);inp.focus();
}

/* Global Command Palette (Ctrl+K / Cmd+K) */
function openCmdk(){
  document.getElementById('cmdk').classList.add('open');
  const inp=document.getElementById('cmdk-in');inp.value='';cmdkIdx=0;
  renderCmdk();setTimeout(()=>inp.focus(),20);
}
function closeCmdk(){document.getElementById('cmdk').classList.remove('open');}
function renderCmdk(){
  const q=(document.getElementById('cmdk-in').value||'').trim().toLowerCase();
  const base=[
    {t:'Open Settings & Appearance',h:'Ctrl+,',fn:()=>openSettings('general')},
    {t:'Settings · Customize Style & Workflow Packs',h:'Customize',fn:()=>openSettings('customize')},
    {t:'Settings · Per-Role Model Routing',h:'Roles',fn:()=>openSettings('roles')},
    {t:'Settings · Skills & Cold Vault Manager',h:'Skills',fn:()=>openSettings('skills')},
    {t:'Settings · MCP Servers & Memory',h:'MCP',fn:()=>openSettings('mcp')},
    {t:'Add Local Git Project Workspace…',h:'Project',fn:()=>addLocalProject()},
    {t:'New thread',h:'Action',fn:()=>newThread(false)},
    {t:'New isolated Git worktree thread',h:'Worktree',fn:()=>newThread(true)},
    {t:'Scheduled Routines & GitHub Automations',h:'Routines',fn:()=>setSbMode('auto')},
    {t:'Smart Commit active worktree',h:'Git',fn:()=>smartCommit(false)},
    {t:'Ship Pull Request (zero AI trailers)',h:'PR',fn:()=>shipPr()},
    {t:'Move active thread into isolated Worktree',h:'Branch',fn:()=>moveToWorktree()},
    {t:'Toggle Artifacts & Diagram Preview Dock',h:'Preview',fn:()=>toggleArtDock()},
    {t:'Toggle Diff Review Panel',h:'Ctrl+D',fn:()=>toggleDiffPanel()},
    {t:'Toggle Contextual Side Chat (/btw)',h:'Ctrl+;',fn:()=>toggleSideChat()},
    {t:'Toggle Quick Worktree Terminal',h:'Ctrl+`',fn:()=>toggleTerminal()},
    {t:'Fork active thread',h:'Branch',fn:()=>forkCurrent()},
    {t:'Export active thread as Markdown (.md)',h:'Export',fn:()=>exportThreadMd()},
    {t:'Compact session context',h:'Memory',fn:()=>compactCurrent()},
    {t:'Palette: Claude Organic (Warm Terracotta & Cream)',h:'Design',fn:()=>setPalette('claude')},
    {t:'Palette: Codex Neutral (Monochrome & Emerald)',h:'Design',fn:()=>setPalette('codex')},
    {t:'Palette: Twig Studio (Linear Indigo)',h:'Design',fn:()=>setPalette('studio')},
    {t:'Palette: Pure Obsidian (OLED Contrast)',h:'Design',fn:()=>setPalette('obsidian')},
    {t:'Appearance: Follow System Dark/Light',h:'Auto',fn:()=>setColorMode('system')},
    {t:'Appearance: Force Dark Mode',h:'Dark',fn:()=>setColorMode('dark')},
    {t:'Appearance: Force Light Mode',h:'Light',fn:()=>setColorMode('light')},
    ...projects.map(p=>({t:`Project · ${p.name} (${p.branch||'repo'})`,h:'Switch',fn:()=>switchProject(p.path)})),
    ...sessions.slice(0,20).map(s=>({t:`Thread · ${s.label}`,h:shortAge(s.age),fn:()=>openSid(s.sid)})),
  ];
  cmdkItems=base.filter(x=>!q||x.t.toLowerCase().includes(q));
  cmdkIdx=Math.min(cmdkIdx,Math.max(0,cmdkItems.length-1));
  document.getElementById('cmdk-list').innerHTML=cmdkItems.map((x,i)=>`<button class="cmdk-it ${i===cmdkIdx?'sel':''}" onclick="runCmdk(${i})"><span>${esc(x.t)}</span><kbd>${esc(x.h)}</kbd></button>`).join('');
}
function runCmdk(i){const it=cmdkItems[i];closeCmdk();if(it)it.fn();}
function onCmdkKey(e){
  if(e.isComposing||e.keyCode===229)return;
  if(e.key==='Escape'){e.preventDefault();closeCmdk();}
  else if(e.key==='ArrowDown'){e.preventDefault();cmdkIdx=(cmdkIdx+1)%Math.max(1,cmdkItems.length);renderCmdk();}
  else if(e.key==='ArrowUp'){e.preventDefault();cmdkIdx=(cmdkIdx-1+Math.max(1,cmdkItems.length))%Math.max(1,cmdkItems.length);renderCmdk();}
  else if(e.key==='Enter'){e.preventDefault();runCmdk(cmdkIdx);}
}
function switchAdjacentThread(delta){
  if(!sessions.length)return;
  const idx=sessions.findIndex(s=>s.sid===curSid);
  const next=idx<0?0:(idx+delta+sessions.length)%sessions.length;
  openSid(sessions[next].sid,true);
}
document.addEventListener('keydown',e=>{
  if(e.isComposing||e.keyCode===229)return;
  if(e.key==='Escape'){
    if(document.getElementById('ask-modal').classList.contains('open')){e.preventDefault();resolveAskModal(null);return;}
    if(document.getElementById('stg-modal').classList.contains('open')){e.preventDefault();closeSettings();return;}
    if(!document.getElementById('cmdk').classList.contains('open')&&!acItems.length){closeAllPops();closeMobilePanels();}
  }
  else if(e.shiftKey&&e.key==='Tab'){e.preventDefault();cycleExecMode();}
  else if(e.altKey&&e.key==='ArrowUp'){e.preventDefault();switchAdjacentThread(-1);}
  else if(e.altKey&&e.key==='ArrowDown'){e.preventDefault();switchAdjacentThread(1);}
  else if((e.metaKey||e.ctrlKey)&&e.key===','){e.preventDefault();openSettings('general');}
  else if((e.metaKey||e.ctrlKey)&&e.key.toLowerCase()==='k'){e.preventDefault();openCmdk();}
  else if((e.metaKey||e.ctrlKey)&&e.key===';'){e.preventDefault();toggleSideChat();}
  else if((e.metaKey||e.ctrlKey)&&e.key==='`'){e.preventDefault();toggleTerminal();}
  else if((e.metaKey||e.ctrlKey)&&e.key.toLowerCase()==='d'&&document.activeElement?.tagName!=='INPUT'){e.preventDefault();toggleDiffPanel();}
  else if(e.key==='/'&&!['INPUT','TEXTAREA','SELECT'].includes(document.activeElement?.tagName||'')){e.preventDefault();document.getElementById('inp')?.focus();}
});

/* Studio Settings Modal (6 Tabs: General, Customize & Packs, Roles, Runtime & Hooks, Skills & Vault, MCP & Memory) */
let stgTab='general',stgData=null,stgSkillQ='';
const STG_TITLES={general:'General & System Appearance',customize:'Customize Style & Workflow Packs',roles:'Models & Per-Role Routing',runtime:'Runtime Guardrails & Project Hooks',skills:'3-Tier Skills & Cold Vault',mcp:'MCP Servers & Persistent Memory'};
const STYLE_PRESETS=[
  {id:'ponytail',t:'Ponytail Minimal',d:'Shortest working solution, stdlib before dependencies, zero speculative abstractions (YAGNI).',txt:'- Favor the simplest, shortest working implementation (YAGNI).\n- Reach for standard library primitives before adding dependencies.\n- Keep answers concise with clickable file:line references.'},
  {id:'architect',t:'Architect Walkthrough',d:'Lead with module boundaries, invariants, dataflow tradeoffs, and explicit verification plans.',txt:'- Explain root causes, invariants, and module boundary tradeoffs clearly.\n- Cite exact file:line locations and verify edge cases before concluding.'},
  {id:'tdd',t:'Strict Test-First (TDD)',d:'Reproduce bugs or specify behavior with a failing test first, then write minimal code to pass.',txt:'- Always add or update a targeted test reproducing the requirement/bug before changing implementation.\n- Run the test suite to confirm green status before finishing.'},
  {id:'security',t:'Security & Boundary Audit',d:'Scrutinize auth, CSRF/Origin checks, path confinement, injection surfaces, and error handling.',txt:'- Audit every change for path confinement, auth/token leaks, injection surfaces, and resource bounds.\n- Never weaken sandbox, permission, or verification gates.'},
];

async function openSettings(tab='general'){
  stgTab=tab;
  document.getElementById('stg-modal').classList.add('open');
  syncStgNav();
  document.getElementById('stg-body').innerHTML='<div style="color:var(--fg-muted);font-size:13px">Loading configuration…</div>';
  stgData=await api('/api/settings?wt='+encodeURIComponent(curWt||defaultWt||''));
  renderStgBody();
}
function closeSettings(){document.getElementById('stg-modal').classList.remove('open');}
function switchStgTab(t){stgTab=t;syncStgNav();renderStgBody();}
function syncStgNav(){
  document.getElementById('stg-title').textContent=STG_TITLES[stgTab]||'Settings';
  document.querySelectorAll('.stg-tab').forEach(b=>b.classList.toggle('active',b.dataset.st===stgTab));
}
function setSendKey(m){sendKeyMode=m;localStorage.setItem('twig_send_key',m);renderStgBody();syncActionBtn();}
async function applyStylePreset(id){
  const p=STYLE_PRESETS.find(x=>x.id===id);if(!p||!stgData)return;
  const mem=stgData.memory||{};
  const cur=(presetScope==='project'?mem.project:mem.global)||'';
  const next=(cur.trim()?cur.trim()+'\n\n':'')+`## Style Preset: ${p.t}\n${p.txt}\n`;
  const body={section:'memory',worktree:curWt||defaultWt};
  if(presetScope==='project')body.project=next;else body.global=next;
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify(body)});
  if(r.error)return toast(r.error);
  toast(`Applied "${p.t}" to ${presetScope} preferences`);refreshStg();
}
async function activateWorkflowPack(pack){
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'bundle',worktree:curWt||defaultWt,pack})});
  if(r.error)return toast(r.error);
  toast(r.summary||'Activated workflow pack');refreshStg();
}
function renderStgBody(){
  const box=document.getElementById('stg-body');
  if(!stgData)return;
  if(stgTab==='general'){
    const pals=[
      {id:'claude',n:'Claude Organic',sub:'Pampas Cream & Terracotta',dk:'#1a1917',lt:'#faf9f5',a:'#d97757'},
      {id:'codex',n:'Codex Neutral',sub:'Graphite Monochrome & Emerald',dk:'#171717',lt:'#ffffff',a:'#10a37f'},
      {id:'studio',n:'Twig Studio',sub:'Linear Slate & Indigo',dk:'#0b0c10',lt:'#f6f7f9',a:'#6e7bf2'},
      {id:'obsidian',n:'Pure Obsidian',sub:'True OLED Black & Alabaster',dk:'#000000',lt:'#fafafa',a:'#6366f1'},
    ];
    const eff=resolvedAppearance();
    const pairUrl=location.origin+(tok?'/?token='+encodeURIComponent(tok):'');
    box.innerHTML=`
      <div class="stg-card">
        <div class="stg-row">
          <div><div class="stg-lbl">Appearance Mode</div><div class="stg-sub">Follows OS \`prefers-color-scheme\` live (currently resolved to <strong>${eff}</strong>)</div></div>
          <div style="display:flex;gap:4px">
            <button class="f-pill ${colorMode==='system'?'active':''}" onclick="setColorMode('system')">◐ System</button>
            <button class="f-pill ${colorMode==='dark'?'active':''}" onclick="setColorMode('dark')">☾ Dark</button>
            <button class="f-pill ${colorMode==='light'?'active':''}" onclick="setColorMode('light')">☀ Light</button>
          </div>
        </div>
        <div class="stg-lbl" style="margin-top:2px">Brand Design System</div>
        <div class="pal-grid">${pals.map(p=>`<button class="pal-card ${palette===p.id?'active':''}" onclick="setPalette('${p.id}')">
          <span style="display:flex;align-items:center;gap:6px">
            <span style="width:24px;height:16px;border-radius:5px;border:1px solid var(--border-hi);background:linear-gradient(135deg,${p.dk} 50%,${p.lt} 50%)"></span>
            <span style="width:10px;height:10px;border-radius:99px;background:${p.a}"></span>
          </span>
          <span style="font-size:12px;font-weight:600">${p.n}</span>
          <span style="font-size:10.5px;color:var(--fg-muted);line-height:1.25">${p.sub}</span>
        </button>`).join('')}</div>
      </div>
      <div class="stg-card">
        <div class="stg-row">
          <div><div class="stg-lbl">External IDE URI Scheme</div><div class="stg-sub">Used when clicking file chips, diff headers, or Open in Editor</div></div>
          <div style="display:flex;gap:4px">${Object.entries(ED_NAMES).map(([k,v])=>`<button class="f-pill ${editor===k?'active':''}" onclick="pickEditor('${k}');renderStgBody()">${v}</button>`).join('')}</div>
        </div>
        <div class="stg-row">
          <div><div class="stg-lbl">Composer Send Shortcut</div><div class="stg-sub">IME composition (\`isComposing\`) is always protected against premature send</div></div>
          <div style="display:flex;gap:4px">
            <button class="f-pill ${sendKeyMode==='enter'?'active':''}" onclick="setSendKey('enter')">Enter</button>
            <button class="f-pill ${sendKeyMode==='mod_enter'?'active':''}" onclick="setSendKey('mod_enter')">Ctrl/⌘ + Enter</button>
          </div>
        </div>
        <div class="stg-row">
          <div><div class="stg-lbl">Background Browser Notifications</div><div class="stg-sub">Alert when an unfocused thread finishes or pauses for approval</div></div>
          <button class="sw-btn ${notifOn?'on':''}" onclick="toggleNotif().then(()=>renderStgBody())"></button>
        </div>
      </div>
      <div class="stg-card">
        <div class="stg-row">
          <div><div class="stg-lbl">Mobile & LAN Companion URL</div><div class="stg-sub" style="font-family:ui-monospace,monospace">${esc(pairUrl)}</div></div>
          <button class="btn-xs" onclick="copyText(${esc(JSON.stringify(pairUrl))},'Copied companion URL',this)">Copy URL</button>
        </div>
      </div>
      <div class="stg-card">
        <div class="stg-lbl">Studio Keyboard Shortcuts</div>
        <div class="stg-row"><span class="stg-sub">Global Command Palette</span><kbd>Ctrl+K / ⌘K</kbd></div>
        <div class="stg-row"><span class="stg-sub">Cycle Execution Mode (Auto ⇄ Review Edits ⇄ Plan)</span><kbd>Shift+Tab</kbd></div>
        <div class="stg-row"><span class="stg-sub">Switch Previous / Next Thread</span><kbd>Alt+↑ / Alt+↓</kbd></div>
        <div class="stg-row"><span class="stg-sub">Settings & Configuration</span><kbd>Ctrl+, / ⌘,</kbd></div>
        <div class="stg-row"><span class="stg-sub">Contextual Side Chat (/btw)</span><kbd>Ctrl+;</kbd></div>
        <div class="stg-row"><span class="stg-sub">Worktree Quick Terminal</span><kbd>Ctrl+\`</kbd></div>
        <div class="stg-row"><span class="stg-sub">Diff Review Dock</span><kbd>Ctrl+D</kbd></div>
      </div>`;
    return;
  }
  if(stgTab==='customize'){
    const mem=stgData.memory||{};
    const agDoc=(mem.agents_md||'').trim();
    box.innerHTML=`
      <div class="stg-card">
        <div class="stg-row">
          <div><div class="stg-lbl">Coding & Response Style Presets</div><div class="stg-sub">1-click Claude-style behavioral profiles written cleanly to \`preferences.md\`</div></div>
          <div style="display:flex;gap:4px">
            <button class="f-pill ${presetScope==='project'?'active':''}" onclick="presetScope='project';renderStgBody()">Project</button>
            <button class="f-pill ${presetScope==='global'?'active':''}" onclick="presetScope='global';renderStgBody()">Global</button>
          </div>
        </div>
        <div class="preset-grid">${STYLE_PRESETS.map(p=>`<button class="preset-card" onclick="applyStylePreset('${p.id}')">
          <strong style="font-size:12.5px;color:var(--fg)">✧ ${esc(p.t)}</strong>
          <span class="stg-sub">${esc(p.d)}</span>
        </button>`).join('')}</div>
      </div>
      <div class="stg-card">
        <div><div class="stg-lbl">Curated Workflow & Plugin Packs</div><div class="stg-sub">Zero-token-bloat capability bundles combining hooks, multi-model routing, and MCP</div></div>
        <div class="stg-row" style="padding:8px 10px;border-radius:9px;background:var(--bg);border:1px solid var(--border)">
          <div style="flex:1"><strong style="font-size:12.5px">⛨ Auto Verification Gate</strong><div class="stg-sub">Auto-detect Cargo / npm / pytest / Go checks and bind \`.twig/project.json\` turn hooks</div></div>
          <button class="btn-xs" onclick="activateWorkflowPack('verify_hooks')">Activate</button>
        </div>
        <div class="stg-row" style="padding:8px 10px;border-radius:9px;background:var(--bg);border:1px solid var(--border)">
          <div style="flex:1"><strong style="font-size:12.5px">✻ Cross-Model Adversarial Reviewer</strong><div class="stg-sub">Route \`reviewer\` role to independent local \`cli:codex\` / \`cli:claude\` at high reasoning effort</div></div>
          <button class="btn-xs" onclick="activateWorkflowPack('cross_review')">Activate</button>
        </div>
        <div class="stg-row" style="padding:8px 10px;border-radius:9px;background:var(--bg);border:1px solid var(--border)">
          <div style="flex:1"><strong style="font-size:12.5px">◈ REA Architecture MCP Scaffold</strong><div class="stg-sub">Wire lazy stdio Repository Entropy & Architecture server in \`.twig/mcp.json\`</div></div>
          <button class="btn-xs" onclick="activateWorkflowPack('rea_pack')">Activate</button>
        </div>
      </div>
      <div class="stg-card">
        <div class="stg-row">
          <div><div class="stg-lbl">Project Instructions (\`AGENTS.md\` / \`CLAUDE.md\`)</div><div class="stg-sub">${agDoc?`Loaded automatically (${agDoc.length} chars)`:'No AGENTS.md or CLAUDE.md found in project root'}</div></div>
          <button class="btn-xs" onclick="switchStgTab('mcp')">Edit Memory →</button>
        </div>
        ${agDoc?`<pre style="max-height:120px;overflow:auto;padding:8px 10px;border-radius:8px;background:var(--code-bg);border:1px solid var(--border);font:11.5px/1.45 ui-monospace,monospace;color:var(--fg-dim);white-space:pre-wrap">${esc(agDoc.slice(0,900))}${agDoc.length>900?'\n…':''}</pre>`:''}
      </div>`;
    return;
  }
  if(stgTab==='roles'){
    const rows=stgData.roles||[];
    const allM=[...new Set([...models,...(stgData.cli_runners||[]),...rows.map(r=>r.model).filter(Boolean)])];
    box.innerHTML=`
      <div class="stg-card">
        <div class="stg-row">
          <div><div class="stg-lbl">Gateway Endpoint</div><div class="stg-sub" style="font-family:ui-monospace,monospace">${esc(stgData.gateway_base||'http://127.0.0.1:3425/v1')}</div></div>
          <span class="f-pill">CLI bridges: ${esc((stgData.cli_runners||[]).join(', ')||'none')}</span>
        </div>
      </div>
      <div class="stg-card">
        <div class="stg-row"><div class="stg-lbl">Per-Role Model & Reasoning Effort</div><button class="btn-xs" onclick="resetAllRoles()">Reset defaults</button></div>
        <div class="stg-sub">Route specialized roles across gateway models or local CLI agents (\`cli:codex\`, \`cli:claude\`, \`cli:agy\`, \`cli:gemini\`).</div>
        ${rows.map(r=>`<div class="stg-row" style="padding-top:6px;border-top:1px solid var(--border)">
          <div style="min-width:110px"><strong style="font-size:12.5px;font-family:ui-monospace,monospace">${esc(r.role)}</strong><div class="stg-sub">${esc(r.source)}</div></div>
          <div style="display:flex;gap:6px;align-items:center;flex-wrap:wrap">
            <input class="stg-in role-m-in" data-role="${esc(r.role)}" list="stg-m-list" value="${esc(r.model)}" style="width:210px;font-family:ui-monospace,monospace">
            <select class="stg-in role-e-in" data-role="${esc(r.role)}">
              ${['off','low','medium','high','xhigh'].map(e=>`<option value="${e}" ${r.effort===e?'selected':''}>effort: ${e}</option>`).join('')}
            </select>
          </div>
        </div>`).join('')}
        <datalist id="stg-m-list">${allM.map(m=>`<option value="${esc(m)}">`).join('')}</datalist>
        <div style="display:flex;justify-content:flex-end;margin-top:4px"><button class="btn-pri" onclick="saveRoleMatrix()">Save Role Routing</button></div>
      </div>`;
    return;
  }
  if(stgTab==='runtime'){
    const h=stgData.harness||{},hk=stgData.hooks||{};
    box.innerHTML=`
      <div class="stg-card">
        <div class="stg-lbl">OS Sandbox & Network (\`~/.config/twig/harness.json\`)</div>
        <div class="stg-row">
          <div><div class="stg-lbl">Linux Bubblewrap Isolation (\`isolation\`)</div><div class="stg-sub">Confine main-agent shell processes inside \`bwrap\` namespaces</div></div>
          <button class="sw-btn ${h.isolation?'on':''}" id="hz-iso" onclick="this.classList.toggle('on')"></button>
        </div>
        <div class="stg-row">
          <div><div class="stg-lbl">Sandbox Network Access (\`network\`)</div><div class="stg-sub">Permit outbound network when \`bwrap\` isolation is enabled</div></div>
          <button class="sw-btn ${h.network?'on':''}" id="hz-net" onclick="this.classList.toggle('on')"></button>
        </div>
      </div>
      <div class="stg-card">
        <div class="stg-lbl">Execution Budgets & Loop Guards</div>
        <div class="stg-row"><span class="stg-sub">Max turns per run (5–80)</span><input class="stg-in" type="number" id="hz-turns" min="5" max="80" value="${h.max_turns||40}" style="width:90px"></div>
        <div class="stg-row"><span class="stg-sub">Stall guard turns (2–20)</span><input class="stg-in" type="number" id="hz-stall" min="2" max="20" value="${h.stall_turns||6}" style="width:90px"></div>
        <div class="stg-row"><span class="stg-sub">Bash timeout seconds (5–600)</span><input class="stg-in" type="number" id="hz-timeout" min="5" max="600" value="${h.bash_timeout_secs||120}" style="width:90px"></div>
        <div class="stg-row"><span class="stg-sub">Parallel subagents (1–8)</span><input class="stg-in" type="number" id="hz-sub" min="1" max="8" value="${h.subagent_concurrency||3}" style="width:90px"></div>
        <div class="stg-row"><span class="stg-sub">Max lines per read (50–2000)</span><input class="stg-in" type="number" id="hz-lines" min="50" max="2000" value="${h.read_max_lines||200}" style="width:90px"></div>
      </div>
      <div class="stg-card">
        <div class="stg-lbl">Project Verification Hooks (\`.twig/project.json\`)</div>
        <div><div class="stg-sub" style="margin-bottom:4px">Turn Acceptance Check Command (\`check\`)</div><input class="stg-in" id="hk-check" value="${esc(hk.check||'')}" placeholder="e.g. cargo test --quiet" style="width:100%;font-family:ui-monospace,monospace"></div>
        <div><div class="stg-sub" style="margin-bottom:4px">Post-Edit Lint/Check Command (\`post_edit_check\`)</div><input class="stg-in" id="hk-post" value="${esc(hk.post_edit_check||'')}" placeholder="e.g. cargo check --quiet" style="width:100%;font-family:ui-monospace,monospace"></div>
        <div style="display:flex;justify-content:flex-end"><button class="btn-pri" onclick="saveRuntimeSettings()">Save Runtime & Hooks</button></div>
      </div>`;
    return;
  }
  if(stgTab==='skills'){
    const sk=stgData.skills||{items:[]};
    const items=(sk.items||[]).filter(x=>!stgSkillQ||`${x.name} ${x.desc} ${x.scope}`.toLowerCase().includes(stgSkillQ.toLowerCase()));
    box.innerHTML=`
      <div class="stg-card">
        <div class="stg-row">
          <div><div class="stg-lbl">${sk.active_count||0} Active · ${sk.vault_count||0} in Cold Vault</div><div class="stg-sub">Cold vault skills cost 0 system-prompt tokens until enabled or invoked via \`$skill\`</div></div>
          <span class="f-pill" style="color:${sk.healthy?'var(--ok)':'var(--warn)'}">${sk.healthy?'Doctor: OK':'Doctor: Check'}</span>
        </div>
        <input class="stg-in" placeholder="Filter skills…" value="${esc(stgSkillQ)}" oninput="stgSkillQ=this.value;renderStgBody()" style="width:100%">
        <div style="display:flex;flex-direction:column;gap:6px;max-height:330px;overflow-y:auto">
          ${items.map(it=>{
            const on=it.scope==='global'||it.scope==='project'||it.scope==='builtin';
            const locked=it.scope==='builtin';
            return `<div class="stg-row" style="padding:6px 8px;border-radius:8px;background:var(--bg);border:1px solid var(--border)">
              <div style="min-width:0;flex:1">
                <div style="display:flex;align-items:center;gap:6px"><strong style="font-size:12.5px;font-family:ui-monospace,monospace">$${esc(it.name)}</strong><span class="f-pill" style="height:18px;font-size:10px">${esc(it.scope)}</span></div>
                <div class="stg-sub" style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${esc(it.desc)}</div>
              </div>
              ${locked?'<span class="stg-sub">built-in</span>':`<button class="sw-btn ${on?'on':''}" data-sn="${esc(it.name)}" data-sc="${esc(it.scope)}" onclick="toggleSkillItem(this.dataset.sn,this.dataset.sc)"></button>`}
            </div>`;
          }).join('')}
        </div>
      </div>`;
    return;
  }
  if(stgTab==='mcp'){
    const mcps=stgData.mcp||[],mem=stgData.memory||{};
    box.innerHTML=`
      <div class="stg-card">
        <div class="stg-row"><div class="stg-lbl">MCP Servers (\`.twig/mcp.json\`)</div><button class="btn-xs" onclick="mcpInitRea()">+ Scaffold REA</button></div>
        ${mcps.length?mcps.map(m=>`<div class="stg-row" style="padding:6px 8px;border-radius:8px;background:var(--bg);border:1px solid var(--border)">
          <div style="min-width:0;flex:1">
            <strong style="font-size:12.5px;font-family:ui-monospace,monospace">${esc(m.name)}</strong> <span class="stg-sub">[${esc(m.scope)}]</span>
            <div class="stg-sub" style="font-family:ui-monospace,monospace">${esc(m.command)} ${esc((m.args||[]).join(' '))}</div>
          </div>
          <div style="display:flex;align-items:center;gap:6px">
            <button class="sw-btn ${m.disabled?'':'on'}" data-mn="${esc(m.name)}" data-dis="${m.disabled?'1':'0'}" onclick="toggleMcpSrv(this.dataset.mn,this.dataset.dis==='1')"></button>
            <button class="icon-btn" data-mn="${esc(m.name)}" onclick="removeMcpSrv(this.dataset.mn)" title="Remove">×</button>
          </div>
        </div>`).join(''):'<div class="stg-sub">No MCP servers configured yet.</div>'}
        <div style="display:flex;gap:6px;flex-wrap:wrap;padding-top:4px">
          <input class="stg-in" id="mcp-n" placeholder="name" style="width:110px;font-family:ui-monospace,monospace">
          <input class="stg-in" id="mcp-c" placeholder="command (e.g. npx)" style="width:140px;font-family:ui-monospace,monospace">
          <input class="stg-in" id="mcp-a" placeholder="args (space-separated)" style="flex:1;min-width:140px;font-family:ui-monospace,monospace">
          <button class="btn-xs" onclick="addMcpSrv()">Add Server</button>
        </div>
      </div>
      <div class="stg-card">
        <div class="stg-lbl">Persistent Memory & Instructions</div>
        <div><div class="stg-sub" style="margin-bottom:4px">Project Preferences (\`.twig/preferences.md\`)</div><textarea class="stg-ta" id="mem-proj" placeholder="Repo-specific conventions, test commands, architectural rules…">${esc(mem.project||'')}</textarea></div>
        <div><div class="stg-sub" style="margin-bottom:4px">Global Preferences (\`~/.config/twig/preferences.md\`)</div><textarea class="stg-ta" id="mem-glob" placeholder="Personal coding style and cross-project rules…">${esc(mem.global||'')}</textarea></div>
        <div style="display:flex;justify-content:flex-end"><button class="btn-pri" onclick="saveMemorySettings()">Save Memory</button></div>
      </div>`;
  }
}

async function refreshStg(){stgData=await api('/api/settings?wt='+encodeURIComponent(curWt||defaultWt||''));await loadMeta();renderStgBody();}
async function saveRoleMatrix(){
  const items=[...document.querySelectorAll('.role-m-in')].map(el=>{
    const role=el.dataset.role;
    const eff=document.querySelector(`.role-e-in[data-role="${role}"]`)?.value||'';
    return{role,model:el.value.trim(),effort:eff};
  });
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'roles',worktree:curWt||defaultWt,items})});
  if(r.error)return toast(r.error);
  toast('Saved per-role model routing');refreshStg();
}
async function resetAllRoles(){
  await api('/api/settings',{method:'POST',body:JSON.stringify({section:'roles',action:'reset'})});
  toast('Reset roles to default');refreshStg();
}
async function saveRuntimeSettings(){
  const patch={
    isolation:document.getElementById('hz-iso').classList.contains('on'),
    network:document.getElementById('hz-net').classList.contains('on'),
    max_turns:+document.getElementById('hz-turns').value,
    stall_turns:+document.getElementById('hz-stall').value,
    bash_timeout_secs:+document.getElementById('hz-timeout').value,
    subagent_concurrency:+document.getElementById('hz-sub').value,
    read_max_lines:+document.getElementById('hz-lines').value,
  };
  const hooks={
    check:document.getElementById('hk-check').value,
    post_edit_check:document.getElementById('hk-post').value,
  };
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'harness',worktree:curWt||defaultWt,patch,hooks})});
  if(r.error)return toast(r.error);
  toast('Saved runtime guardrails & hooks');refreshStg();
}
async function toggleSkillItem(name,scope){
  const enable=scope==='vault';
  const project=scope==='project';
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'skill',worktree:curWt||defaultWt,name,enable,project})});
  if(r.error)return toast(r.error);
  toast(enable?`Enabled $${name}`:`Disabled $${name}`);refreshStg();
}
async function toggleMcpSrv(server,enable){
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'mcp',worktree:curWt||defaultWt,action:enable?'enable':'disable',server})});
  if(r.error)return toast(r.error);
  toast(r.summary||'Updated MCP server');refreshStg();
}
async function mcpInitRea(){
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'mcp',worktree:curWt||defaultWt,action:'init_rea'})});
  toast(r.summary||r.error||'Scaffolded REA');refreshStg();
}
async function addMcpSrv(){
  const server=document.getElementById('mcp-n').value.trim(),command=document.getElementById('mcp-c').value.trim(),args=document.getElementById('mcp-a').value.trim();
  if(!server||!command)return toast('Server name and command required');
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'mcp',worktree:curWt||defaultWt,action:'add',server,command,args})});
  if(r.error)return toast(r.error);
  toast('Added MCP server');refreshStg();
}
async function removeMcpSrv(server){
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'mcp',worktree:curWt||defaultWt,action:'remove',server})});
  if(r.error)return toast(r.error);
  toast('Removed MCP server');refreshStg();
}
async function saveMemorySettings(){
  const project=document.getElementById('mem-proj').value,global=document.getElementById('mem-glob').value;
  const r=await api('/api/settings',{method:'POST',body:JSON.stringify({section:'memory',worktree:curWt||defaultWt,project,global})});
  if(r.error)return toast(r.error);
  toast('Saved persistent memory');refreshStg();
}

/* Voice Dictation (Web Speech API) */
function toggleDictation(){
  const SR=window.SpeechRecognition||window.webkitSpeechRecognition;
  if(!SR)return toast('Speech recognition is not supported in this browser');
  const btn=document.getElementById('mic-btn');
  if(recog){recog.stop();recog=null;btn.classList.remove('rec');return;}
  recog=new SR();recog.continuous=false;recog.interimResults=false;
  btn.classList.add('rec');toast('Listening…');
  recog.onresult=e=>{
    const tr=Array.from(e.results).map(r=>r[0].transcript).join(' ');
    const inp=document.getElementById('inp');
    inp.value=(inp.value?inp.value+' ':'')+tr;
    autoGrow(inp);inp.focus();
  };
  recog.onend=()=>{btn.classList.remove('rec');recog=null;};
  recog.onerror=()=>{btn.classList.remove('rec');recog=null;toast('Voice capture ended');};
  recog.start();
}

/* Attachments, RPC Actions, Quick Shell & Side Chat */
function renderAttachments(){
  const row=document.getElementById('att-row');
  row.classList.toggle('has',images.length>0||reviewNotes.length>0);
  const imgHtml=images.map((im,i)=>`<span class="att-chip"><img src="${im.url}" alt=""><span>${esc(im.label)}</span><button class="meta-lnk" onclick="images.splice(${i},1);renderAttachments()">×</button></span>`).join('');
  const revHtml=reviewNotes.map((rn,i)=>`<span class="att-chip rev"><span>${esc(rn.loc)}: ${esc(rn.note.slice(0,36))}</span><button class="meta-lnk" onclick="reviewNotes.splice(${i},1);renderAttachments()">×</button></span>`).join('');
  row.innerHTML=revHtml+imgHtml;
}
function addImageFile(f){
  if(!f||!f.type.startsWith('image/')||f.size>8*1024*1024)return;
  const rd=new FileReader();
  rd.onload=()=>{images.push({label:f.name||'image.png',url:rd.result});renderAttachments();};
  rd.readAsDataURL(f);
}
function autoGrow(el){el.style.height='auto';el.style.height=Math.min(180,Math.max(46,el.scrollHeight))+'px';}
function primaryAction(){
  if(detail&&detail.running){
    if(document.getElementById('inp').value.trim())steerActiveTurn();
    else cancelRun();
  }else sendPrompt();
}

async function sendPrompt(overridePrompt){
  const inp=document.getElementById('inp');
  let text=(typeof overridePrompt==='string'?overridePrompt:inp.value).trim();
  if(!text&&!images.length&&!reviewNotes.length)return;
  if(reviewNotes.length){
    const notes=reviewNotes.splice(0,reviewNotes.length).map(n=>`- \`${n.loc}\`: ${n.note}`).join('\n');
    text=`Address these inline diff review comments:\n${notes}${text?'\n\n'+text:''}`;
  }
  if(typeof overridePrompt!=='string'){inp.value='';saveCurDraft();}
  histIdx=-1;histDraft='';autoGrow(inp);closeAc();forceTail=true;stickToBottom=true;syncActionBtn();
  const slug=(!curSid&&newWt)?(document.getElementById('wt-slug').value.trim()||text.toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/^-|-$/g,'').slice(0,24)||'task'):'';
  const imgs=images.splice(0,images.length);renderAttachments();
  const r=await api('/api/send',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,new_worktree_slug:slug,role:selRole,gate_edits:execMode==='review',model:selModel,effort:selEffort,prompt:text,images:imgs})});
  if(r.error)return toast(r.error);
  if(r.sid){draftNew=false;newWt=false;curSid=r.sid;liveText[curSid]='';liveTool[curSid]=null;liveSteer[curSid]='';if(r.worktree)curWt=r.worktree;syncWtUi();setTimeout(()=>openSid(curSid,true),160);}
}
async function steerActiveTurn(){
  if(!curSid)return;
  const inp=document.getElementById('inp'),text=inp.value.trim();
  if(!text)return;
  inp.value='';saveCurDraft();autoGrow(inp);closeAc();syncActionBtn();
  const r=await api('/api/steer',{method:'POST',body:JSON.stringify({sid:curSid,input:text})});
  if(r.error)return toast(r.error);
  if(r.idle){inp.value=text;return sendPrompt();}
  liveSteer[curSid]=text;lastHtml='';render();
  toast('↳ Queued live steer for next boundary');
}
async function rewindTurn(uIdx,restoreCode){
  if(!curSid||!detail)return;
  const totalU=userTurnTexts.length;
  const turns=totalU-uIdx;
  if(turns<=0)return;
  const origTxt=userTurnTexts[uIdx]||'';
  const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'session.rewind',params:{turns,restore_code:!!restoreCode}})});
  if(r.error)return toast(r.error);
  toast(restoreCode?'↺ Restored worktree & rewound chat':`↶ Rewound ${r.rewound_turns||turns} turn(s)`);
  const inp=document.getElementById('inp');
  lastHtml='';await openSid(curSid,true);loadList();
  if(origTxt&&!inp.value.trim()){inp.value=origTxt;saveCurDraft();autoGrow(inp);inp.focus();}
}
async function cancelRun(){if(!curSid)return;const r=await api('/api/cancel',{method:'POST',body:JSON.stringify({sid:curSid})});toast(r.stopped?'Interrupted active turn':'No active process');setTimeout(()=>openSid(curSid),220);}
async function archiveSid(sid){const r=await api('/api/archive',{method:'POST',body:JSON.stringify({sid})});if(r.error)return toast(r.error);delete drafts[sid];toast('Archived thread');if(curSid===sid)newThread(false);loadList();}
async function approve(id,allow,remember=false){
  const noteEl=document.getElementById('ap-note-'+id);
  const note=noteEl?noteEl.value.trim():'';
  const wasSingle=(detail&&detail.pending&&detail.pending.length===1);
  const r=await api('/api/approve',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,id,allow,remember,note:note||undefined})});
  if(r.error)return toast(r.error);
  toast(allow?(remember?'Allowed & remembered pattern':'Allowed'):(note?'Declined with guidance':'Declined'));
  if(r.resumed_live){
    if(detail)detail.pending=(detail.pending||[]).filter(p=>p.id!==id);
    lastHtml='';render();
    return;
  }
  await openSid(curSid);
  if(wasSingle&&(allow||note))sendPrompt('Continue from the resolved tool approval.');
}
function exportThreadMd(){
  if(!detail||!(detail.messages&&detail.messages.length))return toast('Open a thread with messages to export');
  const title=detail.title||(sessions.find(s=>s.sid===curSid)?.label)||'twig session export';
  const wt=detail.worktree||curWt||defaultWt||'';
  const br=detail.branch||defaultBranch||'main';
  const mdl=detail.model||activeModel||'default';
  const grouped=groupTimeline(detail.messages||[],detail.snapshots||[]);
  let md=`# ${title}\n\n- **Session:** \`${curSid||'draft'}\`\n- **Workspace:** \`${wt}\` (\`${br}\`)\n- **Model:** \`${mdl}\`\n\n---\n\n`;
  for(const it of grouped){
    if(it.kind==='user')md+=`## User\n\n${(it.txt||'').trim()}\n\n`;
    else if(it.kind==='work'){
      md+=`<details>\n<summary>Worked · ${it.calls.length} tool(s)</summary>\n\n`;
      for(const c of it.calls)md+=`- **${c.verb}** \`${String(c.arg||'').replace(/`/g,"'")}\`\n`;
      md+=`\n</details>\n\n`;
    }else if(it.kind==='assistant')md+=`## Assistant\n\n${(it.txt||'').trim()}\n\n`;
  }
  const slug=title.toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/^-|-$/g,'').slice(0,32)||'session';
  const blob=new Blob([md],{type:'text/markdown;charset=utf-8'});
  const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download=`${slug}-${(curSid||'export').slice(0,8)}.md`;a.click();
  toast('Exported Markdown transcript');
}
async function smartCommit(withHint){
  if(!curSid)return toast('Open a thread with changes first');
  if(busyRpc)return;
  let hint=undefined;
  if(withHint){
    const h=await openInputModal({title:'Smart commit hint',sub:'Leave blank to auto-generate a repo-style commit message from the diff',placeholder:'e.g. feat(web): refine streaming scroll lock',okText:'Commit'});
    if(h===null)return;
    hint=h.trim()||undefined;
  }
  busyRpc=true;toast('✦ Generating smart commit…');
  try{
    const r=await api('/api/commit',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,hint})});
    toast(r.summary||r.error||'Committed changes');openSid(curSid);loadList();
  }finally{busyRpc=false;}
}
async function shipPr(){
  if(!curSid)return toast('Open an active thread first');
  if(busyRpc)return;
  busyRpc=true;toast('⇪ Pushing branch & creating zero-trailer Pull Request…');
  try{
    const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'git.pr',params:{}})});
    if(r.error)return toast(r.error);
    toast(r.url?`Shipped PR #${r.number||''}`:(r.summary||'Pull Request created'));
    openSid(curSid);loadList();
  }finally{busyRpc=false;}
}
async function moveToWorktree(){
  if(!curSid)return toast('Open an active thread first');
  if(busyRpc)return;
  const def=(document.getElementById('stitle').textContent||'feature').toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/^-|-$/g,'').slice(0,24)||'feature';
  const slug=await openInputModal({title:'Move thread to isolated Worktree',sub:'Creates a new Git branch worktree and carries over all uncommitted changes',placeholder:'branch-slug',value:def,okText:'Move Thread'});
  if(slug===null)return;
  busyRpc=true;toast('⑂ Provisioning worktree & carrying uncommitted state…');
  try{
    const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'git.worktree',params:{arg:slug.trim()||def}})});
    if(r.error)return toast(r.error);
    if(r.worktree)curWt=r.worktree;
    toast(r.branch?`Switched to ${r.branch}`:'Moved to isolated worktree');
    openSid(curSid);loadList();
  }finally{busyRpc=false;}
}
async function applySubagentPatch(childSid){
  if(!curSid||!childSid)return;
  const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'session.apply',params:{sid:childSid}})});
  if(r.error)return toast(r.error);
  toast(r.summary||'Applied subagent patch');openSid(curSid);loadList();
}
function renameCurrent(){
  if(!curSid||sbMode==='auto')return;
  const st=document.getElementById('stitle');
  if(st.querySelector('input'))return;
  const cur=st.textContent.trim();
  const inp=document.createElement('input');
  inp.className='inline-ttl-in';inp.value=cur;
  st.textContent='';st.appendChild(inp);
  inp.focus();inp.select();
  let done=false;
  const finish=async save=>{
    if(done)return;done=true;
    const next=inp.value.trim();
    st.textContent=save&&next?next:cur;
    if(save&&next&&next!==cur){
      const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'session.rename',params:{title:next}})});
      if(r.error){st.textContent=cur;return toast(r.error);}
      toast('Renamed thread');openSid(curSid);loadList();
    }
  };
  inp.addEventListener('keydown',e=>{
    if(e.isComposing||e.keyCode===229)return;
    if(e.key==='Enter'){e.preventDefault();finish(true);}
    else if(e.key==='Escape'){e.preventDefault();finish(false);}
  });
  inp.addEventListener('blur',()=>finish(true));
}
async function forkCurrent(){
  if(!curSid)return toast('Open a thread first to fork');
  const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'session.fork',params:{}})});
  if(r.error)return toast(r.error);
  toast('Forked branch thread');await loadList();if(r.sid)openSid(r.sid,true);
}
async function compactCurrent(){
  if(!curSid)return toast('Open a thread first');
  toast('Compacting session context…');
  const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'session.compact',params:{}})});
  if(r.error)return toast(r.error);
  toast(r.compacted?'Context compacted':'Context already compact');openSid(curSid);
}
async function askSideChat(){
  if(!curSid)return toast('Open a thread first to ask contextual Side Chat');
  const inp=document.getElementById('sc-in'),q=inp.value.trim();if(!q)return;
  inp.value='';
  const box=document.getElementById('sc-body');
  box.insertAdjacentHTML('beforeend',`<div class="sc-q">${esc(q)}</div><div class="sc-a shimmer">Thinking…</div>`);
  box.scrollTop=box.scrollHeight;
  const r=await api('/api/rpc',{method:'POST',body:JSON.stringify({sid:curSid,worktree:curWt,method:'session.btw',params:{question:q}})});
  box.lastElementChild.remove();
  box.insertAdjacentHTML('beforeend',`<div class="sc-a">${r.error?esc(r.error):fmtMd(r.answer||'')}</div>`);
  box.scrollTop=box.scrollHeight;
}
async function runQuickCmd(cmd){
  const c=(cmd||'').trim();if(!c)return;
  const out=document.getElementById('td-out');
  out.textContent=`$ ${c}\nRunning…`;
  const r=await api('/api/shell',{method:'POST',body:JSON.stringify({worktree:curWt||defaultWt,command:c})});
  if(r.error){out.textContent=`$ ${c}\n[error] ${r.error}`;return;}
  out.textContent=`$ ${c}  (exit ${r.exit_code})\n${(r.stdout||'')+(r.stderr?'\n'+r.stderr:'')}`.trim();
  if(curSid)openSid(curSid);
}

/* Scheduled Routines & GitHub Automations Mode (Threads | Routines) */
async function setSbMode(m){
  sbMode=m;
  document.getElementById('m-thr').classList.toggle('active',m==='threads');
  document.getElementById('m-aut').classList.toggle('active',m==='auto');
  document.getElementById('sb-thr-filters').style.display=m==='auto'?'none':'flex';
  if(m==='auto'){await loadAutomations();}
  else{document.querySelector('.composer-dock').style.display='';renderList();lastHtml='';render();}
}
async function loadAutomations(){
  const[ad,sd]=await Promise.all([
    api('/api/automations?wt='+encodeURIComponent(curWt||defaultWt||'')),
    api('/api/schedules'),
  ]);
  autoData=ad;
  schedules=Array.isArray(sd.schedules)?sd.schedules:[];
  if(sbMode==='auto'){renderAutoSidebar();renderAutoStage();}
}
function renderAutoSidebar(){
  const items=(autoData&&autoData.items)||[];
  const q=(document.getElementById('q').value||'').trim().toLowerCase();
  const filtSch=schedules.filter(s=>!q||`${s.name} ${s.prompt} ${s.worktree}`.toLowerCase().includes(q));
  const filtIt=items.filter(it=>!q||`${it.title} #${it.number} ${it.state}`.toLowerCase().includes(q));
  document.getElementById('sb-count').textContent=`${schedules.length} routine${schedules.length===1?'':'s'} · ${items.length} queued`;
  let html='';
  if(filtSch.length){
    html+=`<div class="grp-hdr"><span>⟳ Scheduled</span><span>${filtSch.length}</span></div>`;
    html+=filtSch.map(s=>`<div class="t-card" data-sid="${esc(s.last_sid||'')}" onclick="if(this.dataset.sid)openSid(this.dataset.sid);else renderAutoStage()">
      <div class="t-row1"><span class="proj-name">${esc(baseName(s.worktree))}</span><span class="t-status" style="color:${s.enabled?'var(--ok)':'var(--fg-muted)'}">${s.enabled?`every ${s.interval_mins}m`:'paused'}</span></div>
      <div class="t-title">${esc(s.name)}</div>
      <div class="t-row3"><span class="t-branch">${esc(s.role)}</span><span class="t-stat">${s.last_sid?'last run →':'ready'}</span></div>
    </div>`).join('');
  }
  if(filtIt.length){
    html+=`<div class="grp-hdr"><span>⇪ GitHub Queue</span><span>${filtIt.length}</span></div>`;
    html+=filtIt.map(it=>{
      const col=it.state==='done'?'var(--ok)':it.state==='failed'?'var(--err)':it.state==='needs_attention'?'var(--warn)':'var(--accent)';
      return `<div class="t-card" data-sid="${esc(it.last_sid||'')}" onclick="if(this.dataset.sid)openSid(this.dataset.sid,true);else renderAutoStage()">
        <div class="t-row1"><span class="proj-name">#${it.number} · ${esc(it.kind)}</span><span class="t-status" style="color:${col}">${esc(it.state)}</span></div>
        <div class="t-title">${esc(it.title)}</div>
        <div class="t-row3"><span class="t-branch">${esc(it.repository||'')}</span><span class="t-stat">${it.last_sid?'inspect →':''}</span></div>
      </div>`;
    }).join('');
  }
  document.getElementById('slist').innerHTML=html||'<div style="padding:18px 12px;font-size:12.5px;color:var(--fg-muted)">No active routines or queued tasks yet. Create one on the right.</div>';
}
function prefillSchedule(name,mins,role,prompt){
  const nEl=document.getElementById('sch-name'),mEl=document.getElementById('sch-mins'),rEl=document.getElementById('sch-role'),pEl=document.getElementById('sch-prompt');
  if(nEl)nEl.value=name;if(mEl)mEl.value=String(mins);if(rEl)rEl.value=role;if(pEl){pEl.value=prompt;pEl.focus();}
}
async function addScheduleRoutine(){
  const name=document.getElementById('sch-name')?.value.trim();
  const interval_mins=+(document.getElementById('sch-mins')?.value||60);
  const role=document.getElementById('sch-role')?.value||'plan';
  const prompt=document.getElementById('sch-prompt')?.value.trim();
  if(!name||!prompt)return toast('Routine name and prompt required');
  const r=await api('/api/schedules',{method:'POST',body:JSON.stringify({action:'add',name,interval_mins,role,prompt,worktree:curWt||defaultWt})});
  if(r.error)return toast(r.error);
  toast(`Scheduled "${name}" every ${interval_mins}m`);loadAutomations();
}
async function toggleSchedule(id){
  const r=await api('/api/schedules',{method:'POST',body:JSON.stringify({action:'toggle',id})});
  if(r.error)return toast(r.error);
  loadAutomations();
}
async function runScheduleNow(id){
  const r=await api('/api/schedules',{method:'POST',body:JSON.stringify({action:'run_now',id})});
  if(r.error)return toast(r.error);
  toast('Dispatched routine turn');
  await loadList();
  if(r.sid)openSid(r.sid,true);else loadAutomations();
}
async function removeSchedule(id){
  const r=await api('/api/schedules',{method:'POST',body:JSON.stringify({action:'remove',id})});
  if(r.error)return toast(r.error);
  toast('Removed routine');loadAutomations();
}
function renderAutoStage(){
  document.querySelector('.composer-dock').style.display='none';
  document.getElementById('stitle').textContent='Scheduled Routines & Automations';
  const d=autoData||{items:[]},cfg=d.config||{},items=d.items||[];
  const repo=cfg.repository||d.suggested_repo||'owner/repo';
  const html=`<div class="auto-wrap">
    <div class="stg-card">
      <div class="stg-row">
        <div><div class="stg-lbl">Scheduled Recurring Routines (\`~/.config/twig/schedules.json\`)</div><div class="stg-sub">Local background routines (Claude Scheduled + Codex Automations) dispatched automatically in \`${esc(baseName(curWt||defaultWt))}\`.</div></div>
        <div style="display:flex;gap:5px;flex-wrap:wrap">
          <button class="td-chip" onclick="prefillSchedule('Hourly Test & Lint Sweep',60,'plan','Run tests and static checks, and report any regressions or flaky failures.')">+ Test Sweep</button>
          <button class="td-chip" onclick="prefillSchedule('Daily Security & Debt Audit',1440,'plan','Audit recent commits and dependencies for security risks, dead code, or boundary violations.')">+ Daily Audit</button>
        </div>
      </div>
      <div style="display:grid;grid-template-columns:1.3fr .8fr .8fr;gap:8px">
        <input class="stg-in" id="sch-name" placeholder="Routine name (e.g. Nightly CI Health)">
        <select class="stg-in" id="sch-mins">
          <option value="15">Every 15 min</option>
          <option value="30">Every 30 min</option>
          <option value="60" selected>Hourly (60m)</option>
          <option value="360">Every 6 hours</option>
          <option value="1440">Daily (24h)</option>
        </select>
        <select class="stg-in" id="sch-role">
          <option value="plan" selected>◈ Plan / Audit (RO)</option>
          <option value="code">⚡ Full Auto (Code)</option>
        </select>
      </div>
      <div style="display:flex;gap:8px">
        <input class="stg-in" id="sch-prompt" placeholder="Prompt or $skill instruction to run on schedule…" style="flex:1">
        <button class="btn-pri" onclick="addScheduleRoutine()">+ Schedule</button>
      </div>
      ${schedules.length?`<div style="display:flex;flex-direction:column;gap:6px;padding-top:4px">${schedules.map(s=>`<div class="stg-row" style="padding:8px 10px;border-radius:9px;background:var(--bg);border:1px solid var(--border)">
        <div style="min-width:0;flex:1">
          <div style="display:flex;align-items:center;gap:7px"><strong style="font-size:13px">${esc(s.name)}</strong><span class="f-pill" style="height:20px">every ${s.interval_mins}m · ${esc(s.role)}</span><span class="stg-sub">${esc(baseName(s.worktree))}</span></div>
          <div class="stg-sub" style="white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${esc(s.prompt)}</div>
        </div>
        <div style="display:flex;align-items:center;gap:6px">
          ${s.last_sid?`<button class="btn-xs" data-sid="${esc(s.last_sid)}" onclick="openSid(this.dataset.sid,true)">Last thread →</button>`:''}
          <button class="btn-xs" data-id="${esc(s.id)}" onclick="runScheduleNow(this.dataset.id)">▶ Run now</button>
          <button class="sw-btn ${s.enabled?'on':''}" data-id="${esc(s.id)}" onclick="toggleSchedule(this.dataset.id)" title="Enable / pause schedule"></button>
          <button class="icon-btn" data-id="${esc(s.id)}" onclick="removeSchedule(this.dataset.id)" title="Delete routine">×</button>
        </div>
      </div>`).join('')}</div>`:'<div class="stg-sub">No recurring schedules configured yet.</div>'}
    </div>
    <div class="stg-card">
      <div class="stg-row">
        <div><div class="stg-lbl">Durable GitHub & Worktree Automation (\`.twig/automation.json\`)</div><div class="stg-sub">Isolates each queued Issue/PR in its own worktree, runs acceptance checks + reviewer gate, and pauses before shipping.</div></div>
        <div style="display:flex;gap:6px">
          ${d.configured?`<button class="btn-xs" onclick="triggerAutoRun('run_local')">${d.running?'● Running…':'▶ Run Queued'}</button><button class="btn-pri" onclick="triggerAutoRun('poll_once')">↻ Sync GitHub & Run</button>`:''}
        </div>
      </div>
      <div style="display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:8px;padding-top:4px">
        <div><div class="stg-sub">GitHub owner/repo</div><input class="stg-in" id="au-repo" value="${esc(repo)}" style="width:100%;font-family:ui-monospace,monospace"></div>
        <div><div class="stg-sub">Label filter</div><input class="stg-in" id="au-lbl" value="${esc(cfg.label||'twig')}" style="width:100%;font-family:ui-monospace,monospace"></div>
        <div><div class="stg-sub">Verification check command</div><input class="stg-in" id="au-chk" value="${esc(cfg.check_command||'cargo test --quiet')}" style="width:100%;font-family:ui-monospace,monospace"></div>
      </div>
      <div class="stg-row" style="padding-top:4px">
        <label style="display:inline-flex;align-items:center;gap:6px;font-size:12px;color:var(--fg-dim);cursor:pointer"><input type="checkbox" id="au-prs" ${cfg.include_pull_requests?'checked':''}> Include open PRs</label>
        <label style="display:inline-flex;align-items:center;gap:6px;font-size:12px;color:var(--fg-dim);cursor:pointer"><input type="checkbox" id="au-ship" ${cfg.create_pr?'checked':''}> Permit push + PR creation</label>
        <button class="btn-xs" onclick="saveAutoCfg()">Save Automation Config</button>
      </div>
    </div>
    ${d.configured?`<div class="stg-card">
      <div class="stg-lbl">Enqueue Task Manually</div>
      <div style="display:flex;gap:6px;flex-wrap:wrap">
        <input class="stg-in" id="au-t-ttl" placeholder="Issue / maintenance task title…" style="flex:1;min-width:200px">
        <input class="stg-in" id="au-t-body" placeholder="Acceptance criteria / reproduction notes…" style="flex:1;min-width:200px">
        <button class="btn-pri" onclick="enqueueAutoTask()">+ Enqueue</button>
      </div>
    </div>`:''}
    <div class="stg-card">
      <div class="stg-lbl">Queue Ledger (${items.length})</div>
      ${items.length?items.map(it=>`<div class="stg-row" style="padding:8px 10px;border-radius:9px;background:var(--bg);border:1px solid var(--border)">
        <div style="min-width:0;flex:1">
          <div style="display:flex;align-items:center;gap:7px"><strong style="font-size:13px">#${it.number} · ${esc(it.title)}</strong><span class="f-pill" style="height:20px">${esc(it.state)}</span></div>
          <div class="stg-sub" style="font-family:ui-monospace,monospace">${esc(it.id)}${it.worktree?' · '+esc(it.worktree):''}</div>
        </div>
        <div style="display:flex;align-items:center;gap:6px">
          ${it.last_sid?`<button class="btn-xs" data-sid="${esc(it.last_sid)}" onclick="openSid(this.dataset.sid,true)">Inspect thread →</button>`:''}
          ${it.url?`<a class="btn-xs" href="${esc(it.url)}" target="_blank" rel="noopener noreferrer" style="text-decoration:none">GitHub ↗</a>`:''}
          ${it.state==='needs_attention'?`<button class="btn-xs" data-id="${esc(it.id)}" data-act="${esc(it.pending_action||'create_pr')}" onclick="resolveAutoShip(this.dataset.id,this.dataset.act,true)">Confirm Shipped</button>`:''}
        </div>
      </div>`).join(''):'<div class="stg-sub">No work items queued yet.</div>'}
    </div>
  </div>`;
  document.getElementById('tl-inner').innerHTML=html;
  lastHtml='';
}
async function saveAutoCfg(){
  const body={
    action:'save_config',worktree:curWt||defaultWt,
    repository:document.getElementById('au-repo').value.trim(),
    label:document.getElementById('au-lbl').value.trim(),
    check_command:document.getElementById('au-chk').value.trim(),
    include_pull_requests:document.getElementById('au-prs').checked,
    create_pr:document.getElementById('au-ship').checked,
  };
  const r=await api('/api/automations',{method:'POST',body:JSON.stringify(body)});
  if(r.error)return toast(r.error);
  toast('Saved .twig/automation.json');loadAutomations();
}
async function enqueueAutoTask(){
  const title=document.getElementById('au-t-ttl').value.trim(),body=document.getElementById('au-t-body').value.trim();
  if(!title)return toast('Task title required');
  const r=await api('/api/automations',{method:'POST',body:JSON.stringify({action:'enqueue',worktree:curWt||defaultWt,title,body})});
  if(r.error)return toast(r.error);
  document.getElementById('au-t-ttl').value='';document.getElementById('au-t-body').value='';
  toast('Enqueued automation task');loadAutomations();
}
async function triggerAutoRun(action){
  const r=await api('/api/automations',{method:'POST',body:JSON.stringify({action,worktree:curWt||defaultWt})});
  if(r.error)return toast(r.error);
  toast(action==='poll_once'?'Polling GitHub & executing queue…':'Executing queued items in isolated worktrees…');
  loadAutomations();
}
async function resolveAutoShip(id,shipping_action,completed){
  const r=await api('/api/automations',{method:'POST',body:JSON.stringify({action:'resolve_shipping',worktree:curWt||defaultWt,id,shipping_action,completed})});
  if(r.error)return toast(r.error);
  toast('Resolved shipping state');loadAutomations();
}

/* Real-Time SSE Stream (/api/events) with Graceful Fallback */
let rafPend=false;
function scheduleLiveRender(){
  if(rafPend)return;rafPend=true;
  requestAnimationFrame(()=>{rafPend=false;if(sbMode==='threads'&&curSid)render();});
}
function connectSse(){
  try{
    const es=new EventSource('/api/events'+(tok?'?token='+encodeURIComponent(tok):''));
    es.onopen=()=>{sseOk=true;document.getElementById('sse-dot')?.classList.add('live');};
    es.onerror=()=>{sseOk=false;document.getElementById('sse-dot')?.classList.remove('live');};
    es.onmessage=e=>{
      let d;try{d=JSON.parse(e.data);}catch(_){return;}
      const sid=d.sid||'';
      if(d.ev==='automation.updated'){if(sbMode==='auto')loadAutomations();if(d.error)toast('Automation: '+d.error);return;}
      if(d.ev==='schedule.updated'){if(sbMode==='auto')loadAutomations();loadList();return;}
      if(d.ev==='session.started'){loadList();if(sid===curSid&&detail){detail.running=true;scheduleLiveRender();}return;}
      if(d.ev==='model.delta'&&sid){
        liveText[sid]=(liveText[sid]||'')+(d.text||'');
        if(detail&&sid===curSid)detail.running=true;
        if(sid===curSid)scheduleLiveRender();
        return;
      }
      if(d.ev==='tool.started'&&sid){
        liveTool[sid]=summarizeToolCall(d.tool,d.args);
        if(sid===curSid)scheduleLiveRender();
        return;
      }
      if(d.ev==='steering.queued'&&sid){
        liveSteer[sid]=d.text||'';
        if(sid===curSid)scheduleLiveRender();
        return;
      }
      if(d.ev==='tool.completed'||d.ev==='approval.requested'||d.ev==='turn.completed'||d.ev==='session.idle'||d.ev==='session.settled'){
        if(d.ev==='turn.completed'||d.ev==='session.settled'){liveText[sid]='';liveTool[sid]=null;if(d.ev==='session.settled')liveSteer[sid]='';}
        loadList();
        if(sid===curSid&&sbMode==='threads')openSid(curSid,false,true);
      }
    };
  }catch(_){}
}

const inpEl=document.getElementById('inp'),compCard=document.querySelector('.composer-card'),paneEl=document.getElementById('pane');
paneEl.addEventListener('scroll',()=>{
  if(progScroll)return;
  stickToBottom=(paneEl.scrollHeight-paneEl.scrollTop-paneEl.clientHeight)<52;
  syncJumpPill();
},{passive:true});
inpEl.addEventListener('input',()=>{autoGrow(inpEl);updateAc();syncActionBtn();saveCurDraft();});
inpEl.addEventListener('keydown',e=>{
  if(e.isComposing||e.keyCode===229)return;
  if(acItems.length){
    if(e.key==='ArrowDown'){e.preventDefault();acIdx=(acIdx+1)%acItems.length;renderAc();return;}
    if(e.key==='ArrowUp'){e.preventDefault();acIdx=(acIdx-1+acItems.length)%acItems.length;renderAc();return;}
    if(e.key==='Tab'||(e.key==='Enter'&&!e.shiftKey)){e.preventDefault();applyAc(acIdx);return;}
    if(e.key==='Escape'){e.preventDefault();closeAc();return;}
  }
  if(e.key==='ArrowUp'&&userTurnTexts.length&&!inpEl.value.slice(0,inpEl.selectionStart||0).includes('\n')&&(!inpEl.value.trim()||histIdx>=0)){
    e.preventDefault();
    if(histIdx<0){histDraft=inpEl.value;histIdx=userTurnTexts.length-1;}
    else if(histIdx>0)histIdx--;
    inpEl.value=userTurnTexts[histIdx]||'';autoGrow(inpEl);syncActionBtn();return;
  }
  if(e.key==='ArrowDown'&&histIdx>=0&&!inpEl.value.slice(inpEl.selectionEnd||0).includes('\n')){
    e.preventDefault();
    if(histIdx<userTurnTexts.length-1){histIdx++;inpEl.value=userTurnTexts[histIdx]||'';}
    else{histIdx=-1;inpEl.value=histDraft;}
    autoGrow(inpEl);syncActionBtn();return;
  }
  const wantSend=sendKeyMode==='mod_enter'?(e.key==='Enter'&&(e.metaKey||e.ctrlKey)):(e.key==='Enter'&&!e.shiftKey);
  if(wantSend){e.preventDefault();primaryAction();}
});
document.addEventListener('paste',e=>{
  const items=[...(e.clipboardData?.items||[])].filter(it=>it.type.startsWith('image/'));
  if(!items.length)return;
  e.preventDefault();
  for(const it of items)addImageFile(it.getAsFile());
  toast('Attached clipboard image');
});
document.body.addEventListener('dragover',e=>{e.preventDefault();compCard?.classList.add('drop-active');});
document.body.addEventListener('dragleave',e=>{if(!e.relatedTarget)compCard?.classList.remove('drop-active');});
document.body.addEventListener('drop',e=>{e.preventDefault();compCard?.classList.remove('drop-active');for(const f of(e.dataTransfer?.files||[]))addImageFile(f);});

loadMeta().then(()=>loadProjects());loadList();connectSse();
setInterval(()=>{
  if(sbMode==='auto')return;
  loadList();
  if(curSid&&!sseOk)openSid(curSid,false,true);
},3500);
