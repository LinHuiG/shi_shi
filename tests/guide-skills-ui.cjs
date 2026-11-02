const fs = require('fs'), path = require('path'), assert = require('assert');
const {withBrowser} = require('./ui-driver.cjs');
const out = fs.mkdtempSync(path.join(require('os').tmpdir(), 'shishi-guide-skills-'));
const skill = fs.readFileSync(path.join(__dirname, '../docs/skills/shishi-cli/SKILL.md'), 'utf8');
const fixture = {config:{enabled:false,auto_ai:false,leaders:[],daily_calls:100,interval_minutes:3,analysis_interval_minutes:30},tasks:[],messages:[],handling:[],activity:[],audit:[],reports:[],suggestions:[],chat_sources:[],schedules:[],usage:{}};
const mock = `window.fixture=${JSON.stringify(fixture)};window.skillText=${JSON.stringify(skill)};window.clipboardDenied=false;window.skillError=false;window.requests=[];Object.defineProperty(navigator,'clipboard',{value:{writeText:async text=>{if(clipboardDenied)throw Error('Denied');window.copied=text;}}});document.execCommand=()=>false;window.ipc={postMessage(raw){const {id,method}=JSON.parse(raw);requests.push(method);setTimeout(()=>{let data=method==='state'?fixture:true,error='';if(method==='cli_skill'){data={text:skillText};if(skillError)error='Synthetic skill read failed';}if(method==='plugin_development_guide')data={text:'Synthetic complete plugin contract'};receive({id,ok:!error,data,error});},5);}};`;
withBrowser(async(browser)=>{
    const page = await browser.newPage({viewport:{width:1360,height:900}}), errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.setContent(require('./ui-bundle.cjs')().replace('<script>','<script>'+mock));
    await page.locator('[data-page="guide"]').first().click();
    const cli = page.getByRole('button',{name:'复制 ShiShi CLI Skill',exact:true});
    const plugin = page.getByRole('button',{name:'复制插件 Skill',exact:true});
    await cli.click();
    await page.waitForFunction(()=>window.copied===skillText);
    assert((await page.locator('#guide-cli-copy-status').innerText()).includes('已复制'));
    assert(!(await cli.getAttribute('class')).includes('primary'));
    assert(!(await plugin.getAttribute('class')).includes('primary'));
    await plugin.click();
    await page.waitForFunction(()=>window.copied==='Synthetic complete plugin contract');
    await page.evaluate(()=>clipboardDenied=true);
    await cli.click();
    await page.locator('#guide-copy-text').waitFor();
    assert.equal(await page.locator('#guide-copy-text').inputValue(),skill);
    assert.equal(await page.locator('#guide-copy-text').evaluate(el=>el.selectionEnd-el.selectionStart),skill.length);
    await page.locator('[data-action="close"]').last().click();
    await page.locator('#overlay').waitFor({state:'hidden'});
    assert(!(await page.evaluate(()=>requests)).some(method=>['ai_job_start','analyze','memory_save','edit'].includes(method)));
    for(const width of [1360,760,360]){
      await page.setViewportSize({width,height:900});
      await cli.scrollIntoViewIfNeeded();
      assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      for(const button of [cli,plugin]){
        assert(await button.evaluate(el=>el.scrollWidth<=el.clientWidth));
      }
      if(width===1360)await page.screenshot({path:path.join(out,'skills.png')});
    }
    await page.evaluate(()=>skillError=true);
    await cli.click();
    await page.getByText('Synthetic skill read failed',{exact:true}).waitFor();
    assert(!(await cli.isDisabled()));
    assert.deepEqual(errors,[]);
    console.log('PASS guide CLI skill, independent plugin copy, manual fallback, failure recovery and compact responsive buttons. '+out);
  }).catch(error=>{console.error(error);process.exitCode=1;});
