const fs=require('fs'),path=require('path');
const build=require('../tests/ui-bundle.cjs');
module.exports={build};
if(require.main===module){const out=process.argv[2];if(!out)throw Error('Pass output HTML path');fs.mkdirSync(path.dirname(path.resolve(out)),{recursive:true});fs.writeFileSync(out,build());}
