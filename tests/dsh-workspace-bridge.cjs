const fs=require('node:fs/promises'),path=require('node:path'),os=require('node:os'),assert=require('node:assert/strict'),{pathToFileURL}=require('node:url'),{spawnSync}=require('node:child_process');
(async()=>{
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'ss-dsh-bridge-'));
 try{
  const plugin=await import(pathToFileURL(path.resolve(__dirname,'../examples/dsh-workspace-bridge/index.js')));
  const workspace=path.join(root,'workspace');await fs.mkdir(workspace);
  const attached=[],created=[],ctx={sessionPersistence:{async stat(id){return id==='pending'?undefined:{header:{cwd:workspace}};}},workspaceRegistry:{async create(cwd){created.push(cwd);return {async attachSession(id){attached.push(id)}};}}};
  for(const id of ['ready','pending'])await fs.writeFile(path.join(root,id+'.json'),JSON.stringify({version:1,session_id:id}));
  const done=new Set();await plugin.reconcile(ctx,root,done);await plugin.reconcile(ctx,root,done);
  assert.deepEqual(attached,['ready']);assert.equal(created.length,1);assert.equal(created[0],await fs.realpath(workspace));assert(!done.has('pending.json'));
  await fs.writeFile(path.join(root,'invalid.json'),JSON.stringify({version:1,session_id:'../unsafe'}));
  await assert.rejects(plugin.reconcile(ctx,root,done),/Invalid SS session/);
  const hook=path.resolve(__dirname,'../providers/acp/assets/stdio-console.cjs');
  const child=spawnSync(process.execPath,['--require',hook,'-e','console.info("[provider] synthetic model listing");process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:1,result:{}})+"\\n")'],{encoding:'utf8'});
  assert.equal(child.status,0);assert.deepEqual(JSON.parse(child.stdout.trim()),{jsonrpc:'2.0',id:1,result:{}});assert(child.stderr.includes('[provider] synthetic model listing'));
  const inherited=spawnSync(process.execPath,['-e','const child=require("node:child_process").spawnSync(process.execPath,["-e","console.log(123)"],{encoding:"utf8"});process.stdout.write(child.stdout)'],{encoding:'utf8',env:{...process.env,NODE_OPTIONS:'--require="'+hook.replace(/\\/g,'/')+'"',SHISHI_DSH_ORIGINAL_NODE_OPTIONS:''}});
  assert.equal(inherited.status,0);assert.equal(inherited.stdout.trim(),'123');assert.equal(inherited.stderr,'');
  console.log('PASS native workspace attachment, idempotence, pending persistence, path validation, console/protocol isolation');
 }finally{await fs.rm(root,{recursive:true,force:true});}
})().catch(error=>{console.error(error);process.exitCode=1});
