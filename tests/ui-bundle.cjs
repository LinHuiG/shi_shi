// Assemble assets/ui exactly like src/ui_bundle.rs (manifest order, one <style>,
// one <script>). UI tests and the preview page use this instead of the app.
const fs=require('fs'),path=require('path');
const dir=path.resolve(__dirname,'../assets/ui');
const read=f=>fs.readFileSync(path.join(dir,f),'utf8').replace(/\r\n/g,'\n');
module.exports=function bundle(){
 let styles='',scripts='';
 for(const raw of read('manifest.txt').split('\n')){
  const line=raw.trim();if(!line||line.startsWith('#'))continue;
  const [kind,file]=[line.slice(0,line.indexOf(' ')),line.slice(line.indexOf(' ')+1).trim()];
  const text=read(file),chunk=text.endsWith('\n')?text:text+'\n';
  if(kind==='style')styles+=chunk;else if(kind==='script')scripts+=chunk;else throw Error('unknown manifest kind: '+kind);
 }
 return read('index.html').replace('<!--STYLES-->',()=>'<style>\n'+styles+'</style>').replace('/*SCRIPTS*/',()=>scripts.trimEnd());
};
