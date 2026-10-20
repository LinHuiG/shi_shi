const assert=require('assert'),fs=require('fs'),{chromium}=require('playwright');
const bundle=require('./ui-bundle.cjs');
const rgb=s=>s.match(/[\d.]+/g).map(Number);
const over=(a,b)=>a.slice(0,3).map((c,i)=>c*(a[3]??1)+b[i]*(1-(a[3]??1)));
const luminance=c=>c.map(v=>{v/=255;return v<=.04045?v/12.92:((v+.055)/1.055)**2.4}).reduce((s,v,i)=>s+v*[.2126,.7152,.0722][i],0);
const contrast=(a,b)=>(Math.max(luminance(a),luminance(b))+.05)/(Math.min(luminance(a),luminance(b))+.05);
(async()=>{
 const browser=await chromium.launch({headless:true,executablePath:['C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe','C:/Program Files/Microsoft/Edge/Application/msedge.exe'].find(fs.existsSync)});
 try{
  const page=await browser.newPage();
  // Test the actual bundled CSS in a minimal shell: no user data or model IPC.
  const css=bundle().match(/<style>([\s\S]*?)<\/style>/)[1];
  await page.setContent(`<style>${css}</style><div class="app"><aside class="sidebar">Navigation</aside><main class="main">Content</main></div>`);
  const colors=()=>page.evaluate(()=>Object.fromEntries(['html','body','.sidebar','.main'].map(s=>[s,getComputedStyle(document.querySelector(s)).backgroundColor])));
  assert.equal((await colors()).body,'rgb(32, 32, 32)');
  for(const theme of ['light','dark']){
   await page.emulateMedia({colorScheme:theme});
   await page.evaluate(()=>document.documentElement.dataset.backdrop='mica');
   for(const width of [900,1240,1800]){
    await page.setViewportSize({width,height:820});
    const c=await colors();
    // Worst-case white native surface, not only the expected dark Mica surface.
    const navigation=over(rgb(c.html),[255,255,255]);
    const content=over(rgb(c['.main']),navigation);
    assert(navigation.every(v=>v<60),'white native underlay remains dark');
    assert(content.every(v=>v<45),'content remains dark');
    const muted=await page.evaluate(()=>getComputedStyle(document.documentElement).getPropertyValue('--text-muted').trim());
    const text=muted.match(/^#(..)(..)(..)$/).slice(1).map(v=>parseInt(v,16));
    assert(contrast(text,navigation)>=4.5,'muted navigation text contrast');
    assert(contrast(text,content)>=4.5,'muted content text contrast');
   }
   await page.evaluate(()=>delete document.documentElement.dataset.backdrop);
   assert.equal((await colors()).body,'rgb(32, 32, 32)','complete opaque fallback');
  }
  await page.evaluate(()=>document.documentElement.dataset.backdrop='mica');
  await page.emulateMedia({forcedColors:'active'});
  const forced=await colors();
  assert(!forced.body.startsWith('rgba('),'forced colors keep an opaque system canvas');
  console.log('PASS backdrop: white-underlay contrast, light/dark preferences, resize, opaque fallback, forced colors');
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
