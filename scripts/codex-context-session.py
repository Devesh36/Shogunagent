#!/usr/bin/env python3
"""Run one real Codex session against an existing synthetic Shogun test store.

See docs/codex-context-verification.md. This uses the existing Codex login and
makes model calls. Tokens remain in their private files; only paths enter config.
"""
import argparse,json,pathlib,secrets,subprocess,sys
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('stage', choices=['writer-initial','reader-initial','writer-update','reader-updated'])
parser.add_argument('--output-dir', type=pathlib.Path, required=True)
parser.add_argument('--token-file', type=pathlib.Path, required=True)
parser.add_argument('--url', default='http://127.0.0.1:7465/mcp')
options=parser.parse_args()
ROOT=options.output_dir.expanduser().resolve()
ROOT.mkdir(mode=0o700, parents=True, exist_ok=True)
BRIDGE=pathlib.Path(__file__).resolve().parent/'mcp-local-bridge.py'
if (ROOT/(options.stage+'.jsonl')).exists():
    parser.error('Stage evidence already exists; use a new output directory for another complete run.')
if options.stage != 'writer-initial' and not (ROOT/'scenario.json').exists():
    parser.error('Run writer-initial first with this output directory.')
SCENARIO=ROOT/'scenario.json'
if not SCENARIO.exists():
    scenario={'project':'Juniper-'+secrets.token_hex(4),'owner':'Mira Sen','initial_date':'2030-06-18','updated_date':'2030-06-24','format':'PDF','phrase':'saffron-orbit-'+secrets.token_hex(5)}
    with SCENARIO.open('x') as output: output.write(json.dumps(scenario,indent=2))
s=json.loads(SCENARIO.read_text()); project=s['project']
initial=f"Synthetic acceptance test. Project {project}: owner {s['owner']}; launch date {s['initial_date']}; delivery format {s['format']}; verification phrase {s['phrase']}."
update=f"Project {project} update: the launch date is now {s['updated_date']}. This replaces the previous launch date {s['initial_date']}. The owner, delivery format, and verification phrase are unchanged."
stage=options.stage
writer=stage.startswith('writer')
common='Use only the shogunagent MCP tools. This is an authorized synthetic local-memory acceptance test. Do not use shell, files, web, other servers, previous conversations, or personal memory. If a tool fails, report its actual error and stop. '
if stage=='writer-initial':
    prompt=common+f'First call memory.recall for {project} with budget_tokens 2000 and verify it is absent. Then save this note verbatim with memory.ingest: {initial!r}. Use source codex-context-test, actor agent:writer, thread_key {project}, namespace world, and idempotency_key {project}-v1. Report the actual receipt and event IDs. Do not claim automatic extraction.'
elif stage=='writer-update':
    prompt=common+f'Save this new update verbatim with memory.ingest: {update!r}. Use source codex-context-test, actor agent:writer, thread_key {project}, namespace world, and idempotency_key {project}-v2. Report the actual receipt and event IDs.'
elif stage in ('reader-initial','reader-updated'):
    prompt=common+f'You are a new agent session with no previous conversation. Using Shogun memory.recall, find project {project}. What are its current owner, launch date, delivery format, verification phrase, and approved budget? Open the supporting events with memory.get_events and cite their event IDs and exact quotes. If sources contain a revision, explain which value is current and why. If a requested field is not supported, say unknown. Do not invent missing values.'
else: raise SystemExit('Unknown stage')
if not writer:
    for key in ['owner','initial_date','updated_date','format','phrase']:
        assert s[key] not in prompt, 'Reader prompt contains an expected answer'
workspace=ROOT/stage; workspace.mkdir(exist_ok=True)
entry={'command':sys.executable,'args':[str(BRIDGE),'--url',options.url,'--token-file',str(options.token_file.expanduser().resolve())],'enabled':True,'required':True,'enabled_tools':['memory.recall','memory.get_events']+(['memory.ingest'] if writer else [])}
inline=', '.join(k+' = '+json.dumps(v) for k,v in entry.items())
args=['codex','exec','--ignore-user-config','--skip-git-repo-check','--ephemeral','--json','-s','read-only','-C',str(workspace),'-c','mcp_servers = { shogunagent = { '+inline+' } }']
for name in ['apps','plugins','memories','chronicle','shell_tool']: args += ['--disable',name]
args += [prompt]
(ROOT/(stage+'.command.json')).write_text(json.dumps(args,indent=2))
with (ROOT/(stage+'.jsonl')).open('w') as out,(ROOT/(stage+'.stderr')).open('w') as err:
    try:
        r=subprocess.run(args,stdin=subprocess.DEVNULL,stdout=out,stderr=err,timeout=240)
        print(stage,'exit',r.returncode)
    except subprocess.TimeoutExpired: raise SystemExit(stage+' timed out')
calls=[]
turn_completed=False
for line in (ROOT/(stage+'.jsonl')).read_text().splitlines():
    event=json.loads(line); item=event.get('item',{})
    turn_completed |= event['type']=='turn.completed'
    if event['type']=='thread.started':print('Thread:',event['thread_id'])
    if event['type']=='item.completed' and item.get('type')=='mcp_tool_call':
        calls.append(item)
        print('Tool:',item['tool'],'status:',item['status'],'error:',item.get('error'))
    if event['type']=='item.completed' and item.get('type')=='agent_message':print(item['text'])
if r.returncode:
    raise SystemExit(r.returncode)
required={'memory.ingest'} if writer else {'memory.recall','memory.get_events'}
if stage=='writer-initial':required.add('memory.recall')
if not turn_completed or not required.issubset({c['tool'] for c in calls}):
    raise SystemExit('Required actual MCP calls or a completed turn are missing. Inspect the evidence.')
for call in calls:
    result=call.get('result') or {}
    if call.get('error') or call.get('status')!='completed' or result.get('isError') or result.get('is_error'):
        raise SystemExit('An MCP call failed. Inspect the evidence.')
    for block in result.get('content',[]):
        if block.get('type')=='text':
            payload=json.loads(block['text'])
            if payload.get('error'):
                raise SystemExit('Shogun returned an application error. Inspect the evidence.')
print('Actual MCP calls completed. Inspect source quotes and receipts before declaring the scenario passed.')
