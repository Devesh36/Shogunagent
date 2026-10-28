import {createServer} from 'node:http';
import {readFile,stat} from 'node:fs/promises';
import {resolve,extname,sep} from 'node:path';
import {fileURLToPath} from 'node:url';
const root=fileURLToPath(new URL('../dist/',import.meta.url));
const types={'.html':'text/html; charset=utf-8','.css':'text/css; charset=utf-8','.js':'text/javascript; charset=utf-8','.mjs':'text/javascript; charset=utf-8','.svg':'image/svg+xml','.woff2':'font/woff2','.txt':'text/plain; charset=utf-8','.md':'text/markdown; charset=utf-8','.xml':'application/xml; charset=utf-8','.png':'image/png','.webp':'image/webp'};
function previewOrigin(req) {
  const fallback=`http://127.0.0.1:${req.socket.localPort}`;
  try {
    const url=new URL(`http://${req.headers.host}`);
    return ['localhost','127.0.0.1'].includes(url.hostname)&&url.port===String(req.socket.localPort)?url.origin:fallback;
  } catch { return fallback; }
}
createServer(async(req,res)=>{
  try {
    if(!['GET','HEAD'].includes(req.method)){res.writeHead(405,{Allow:'GET, HEAD'});res.end();return}
    const u=new URL(req.url,'http://localhost');
    let pathname=decodeURIComponent(u.pathname);
    if(pathname==='/'&&(u.searchParams.get('agent')==='1'||req.headers.accept?.includes('text/markdown')))pathname='/index.md';
    let p=resolve(root,'.'+pathname);
    if(p!==resolve(root)&&!p.startsWith(resolve(root)+sep)){res.writeHead(403);res.end('Forbidden');return}
    try {
      if((await stat(p)).isDirectory())p=resolve(p,'index.html');
      let data=await readFile(p);
      if(pathname==='/setup-prompt.txt')data=Buffer.from(data.toString().replaceAll('https://shogunagent.com',previewOrigin(req)));
      if(['/llms.txt','/llms-full.txt','/index.md'].includes(pathname))data=Buffer.from(data.toString().replaceAll('https://shogunagent.com/docs/',previewOrigin(req)+'/docs/'));
      res.writeHead(200,{'content-type':types[extname(p)]||'application/octet-stream','x-content-type-options':'nosniff','cache-control':'no-cache','referrer-policy':'strict-origin-when-cross-origin'});
      res.end(req.method==='HEAD'?undefined:data);
    } catch {
      res.writeHead(404,{'content-type':'text/html; charset=utf-8'});
      res.end(await readFile(resolve(root,'404.html')));
    }
  } catch {res.writeHead(400);res.end('Bad request')}
}).listen(Number(process.env.PORT||3001),'127.0.0.1',()=>console.log(`ShogunAgent website: http://127.0.0.1:${process.env.PORT||3001}`));
