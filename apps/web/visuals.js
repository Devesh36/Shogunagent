(()=>{
  const graph=document.querySelector('.connection-card');
  if(!graph)return;
  const motion=matchMedia('(prefers-reduced-motion: reduce)');
  const motionButton=graph.querySelector('.graph-motion');
  function setMotion(on){graph.dataset.visualMotion=on?'on':'off';motionButton.textContent=on?'Pause motion':'Play motion';motionButton.setAttribute('aria-pressed',String(on));}
  setMotion(!motion.matches);
  motionButton.addEventListener('click',()=>setMotion(graph.dataset.visualMotion!=='on'));
  motion.addEventListener('change',()=>setMotion(!motion.matches));
  new IntersectionObserver(entries=>graph.classList.toggle('is-in-view',entries[0].isIntersecting),{threshold:.15}).observe(graph);
  const details={
    person:['Person','Who is involved?','Maya is reviewing the launch copy. Your agent can connect her to this project and the review she committed to.','“Maya will review the launch copy on Thursday.”'],
    project:['Project','What are we working on?','The website launch brings the note, the reviewer, and the next step together. The context belongs to this piece of work.','“Maya will review the launch copy on Thursday.”'],
    promise:['Commitment','What happens next?','The review is planned for Thursday. Your agent can carry this commitment into a handoff without asking you to repeat it.','“Maya will review the launch copy on Thursday.”'],
    loop:['Open loop','What is still waiting?','The copy needs Maya’s approval before publishing. Your next agent knows to keep this task open.','“Wait for her approval before publishing.”']
  };
  graph.querySelectorAll('[data-context-node]').forEach(button=>button.addEventListener('click',()=>{
    graph.querySelectorAll('[data-context-node]').forEach(node=>node.setAttribute('aria-pressed',String(node===button)));
    const detail=details[button.dataset.contextNode];
    ['graph-kind','graph-title','graph-description','graph-quote'].forEach((id,i)=>document.getElementById(id).textContent=detail[i]);
  }));
  const timeline=document.querySelector('.timeline-card');
  const update=document.getElementById('timeline-update'),reset=document.getElementById('timeline-reset');
  function setTimeline(approved){
    timeline.dataset.timeline=approved?'approved':'pending';
    const text=approved?{
      'timeline-state':'Ready for the next step','review-state':'Approved','publish-state':'Up next',
      'event-title':'Maya · new update','event-copy':'“The copy looks good. Approved!”','event-tag':'New',
      'timeline-answer-title':'The review is complete.','timeline-answer-copy':'Maya approved the copy. Publishing is next, with your approval before any external action.'
    }:{
      'timeline-state':'Waiting for review','review-state':'Pending','publish-state':'Waiting',
      'event-title':'Maya · launch note','event-copy':'“I’ll review the copy on Thursday.”','event-tag':'Saved',
      'timeline-answer-title':'The review is still open.','timeline-answer-copy':'Wait for Maya’s approval before preparing to publish.'
    };
    Object.entries(text).forEach(([id,value])=>document.getElementById(id).textContent=value);
    timeline.querySelector('.work-step.review>span').textContent=approved?'✓':'2';
    update.disabled=approved;update.innerHTML=approved?'Context updated <span aria-hidden="true">✓</span>':'Try it: Maya approves <span aria-hidden="true">→</span>';
    reset.hidden=!approved;
    if(approved)reset.focus({preventScroll:true});else update.focus({preventScroll:true});
  }
  update.addEventListener('click',()=>setTimeline(true));reset.addEventListener('click',()=>setTimeline(false));
})();
