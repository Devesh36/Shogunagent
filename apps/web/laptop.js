(()=>{
  const root=document.querySelector('.local-laptop-demo');
  if(!root)return;
  const $=selector=>root.querySelector(selector);
  const scene=$('.laptop-scene');
  const steps=[...root.querySelectorAll('[data-laptop-step]')];
  const motion=matchMedia('(prefers-reduced-motion: reduce)');
  const duration=7600;
  const frames=[
    {speaker:'You · a useful detail',window:'Launch notes',command:'Maya will review the launch copy on Thursday. Wait for her approval before publishing.',output:'Saved with the original note',detail:'Ready when you need it again.',card:'The detail stays with you.',badge:'Remembered',copy:'Maya’s review comes before publishing. Thursday is the next step to keep in mind.',source:'Source: your launch note',status:'Your work, kept in context',description:'Share a useful detail once. ShogunAgent keeps the context and the original source together.'},
    {speaker:'You · picking up the work',window:'Your assistant',command:'What still needs to happen before we publish?',output:'Found in your launch note',detail:'The answer includes the source.',card:'Maya’s review is still pending.',badge:'Your answer',copy:'Maya needs to review the launch copy on Thursday. We’re waiting for her approval before publishing.',source:'“Wait for her approval before publishing.”',status:'Answer connected to its source',description:'Ask in everyday language. Your agent can use the saved context to explain what is still open.'},
    {speaker:'You · switching agents',window:'Your writing agent',command:'Help my writing agent draft a quick launch update.',output:'The relevant context travels',detail:'No need to explain the review again.',card:'A draft with the right context.',badge:'Continue the work',copy:'“The launch copy is ready for Maya’s Thursday review. We’ll publish once she approves.”',source:'Based on your launch note · sample draft',status:'Planning agent → writing agent',description:'Switch agents without starting the story over. The next agent receives the task and the evidence it needs.'},
    {speaker:'You · ready to share',window:'Your assistant',command:'Can you send that update to the team?',output:'Your approval comes first',detail:'Nothing has been sent.',card:'You keep the final say.',badge:'Waiting for you',copy:'Review the draft before it goes out. Sharing context with an agent does not give it permission to send.',source:'Sample approval request · nothing sent',status:'Draft ready · waiting for human review',description:'Your agent prepares the work. You review and approve external actions before anything is sent.'}
  ];
  let index=0,elapsed=0,last=null,raf=null,visible=false,playing=!motion.matches,finished=false;
  const display=$('.laptop-display'),desktop=$('.laptop-desktop');
  function resize(){desktop.style.transform=`scale(${display.clientWidth/760})`;}
  new ResizeObserver(resize).observe(display);resize();
  function updateButton(){const b=$('#laptop-play');b.textContent=finished?'↻ Replay':playing?'Ⅱ Pause':'▷ Play';b.setAttribute('aria-label',finished?'Replay local workflow animation':playing?'Pause local workflow animation':'Play local workflow animation');scene.style.setProperty('--play-state',playing?'running':'paused');}
  function setFrame(n){index=n;elapsed=0;scene.dataset.step=String(n);const f=frames[n];$('#laptop-speaker').textContent=f.speaker;$('.laptop-titlebar>span:nth-child(2)').textContent=f.window;$('#laptop-output-title').textContent=f.output;$('#laptop-output-detail').textContent=f.detail;$('#laptop-card-title').textContent=f.card;$('#laptop-card-badge').textContent=f.badge;$('#laptop-card-copy').textContent=f.copy;$('#laptop-card-source').textContent=f.source;$('#laptop-terminal-status').textContent=f.status;$('#laptop-description').textContent=f.description;steps.forEach((b,i)=>b.setAttribute('aria-pressed',String(i===n)));render();}
  function render(){const f=frames[index],staticView=motion.matches;const chars=staticView?f.command.length:Math.max(0,Math.floor((elapsed-200)/22));$('#laptop-command').textContent=f.command.slice(0,chars);scene.dataset.phase=staticView||elapsed>=4100?'complete':elapsed>=3200?'transfer':elapsed>=2350?'result':'typing';}
  function tick(time){raf=null;if(!playing||!visible||document.hidden||finished){last=null;return}if(last!==null)elapsed+=Math.min(time-last,100);last=time;if(elapsed>=duration){if(index===frames.length-1){finished=true;playing=false;elapsed=duration;render();updateButton();last=null;return}setFrame(index+1)}render();raf=requestAnimationFrame(tick);}
  function schedule(){if(raf!==null){cancelAnimationFrame(raf);raf=null}last=null;if(playing&&visible&&!document.hidden&&!finished)raf=requestAnimationFrame(tick);}
  steps.forEach((b,i)=>b.addEventListener('click',()=>{playing=false;finished=false;setFrame(i);elapsed=duration;render();updateButton();schedule()}));
  $('#laptop-play').addEventListener('click',()=>{if(finished){finished=false;setFrame(0);playing=true}else{playing=!playing;if(playing&&elapsed>=duration)setFrame(index)}updateButton();schedule()});
  new IntersectionObserver(entries=>{visible=entries[0].isIntersecting;schedule()},{threshold:.2}).observe(root);
  document.addEventListener('visibilitychange',schedule);
  motion.addEventListener('change',()=>{if(motion.matches){playing=false;elapsed=duration;render();updateButton();schedule()}});
  setFrame(0);updateButton();
})();
