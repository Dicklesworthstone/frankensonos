#!/usr/bin/env python3
"""Chromium component checks with production core/activity/workspace code.

Requires Playwright and Chromium. HTTP, the DOM host, the session-review
collaborator and unrelated queue/library components are fixtures. No speakers.
"""
import asyncio
import json
import os
from pathlib import Path
from playwright.async_api import async_playwright

ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / 'crates/fsonos-api/src/remote'
SETUP = r'''
window.calls = []; window.mode = 'ok'; window.hold = null;
window.row = {id:9,at:1700000000,client:'fixture-browser',surface:'http',intent:'set_volume: Synthetic room',decision:'clamp: quiet hours',result:'Volume is 20',undoable:true};
window.zones = [{household:'S2',coordinator_room:'Synthetic room',members:['Synthetic room'],transport_state:'playing'}, {household:'S1',coordinator_room:'Synthetic room',members:['Synthetic room'],transport_state:'playing'}];
window.data = {rows:[structuredClone(row)], candidate:structuredClone(row)};
window.call = async (url, body) => {
  calls.push({url,body});
  if(url === '/zones') return structuredClone(zones);
  if(url.endsWith('/state')) {const zone=zones.find(z=>url.includes(encodeURIComponent(DashboardCore.key(z))));return {zone,transport_state:'playing',volume:20,track:{uri:'fixture:track',title:'Test song',position_secs:10,duration_secs:300}};}
  if(url.startsWith('/actions?')) return structuredClone(data.rows);
  if(url === '/undo/preview') {
    if(mode==='hold-preview') return new Promise(resolve=>{window.hold=()=>resolve(structuredClone(data.candidate));});
    return structuredClone(data.candidate);
  }
  if(url === '/undo/checked') {
    if(mode==='hold-write') return new Promise(resolve=>{window.hold=()=>{data.candidate=null;resolve({undone:9,intent:row.intent,summary:'Restored'});};});
    data.candidate=null;
    if(mode==='lost') throw new Error('Connection lost after application');
    data.rows.unshift({...row,id:10,undoable:false,undo_of:9,intent:'undo',result:'Restored'});
    return {undone:9,intent:row.intent,summary:'Restoration report',...(mode==='partial'?{failures:['Fixture speaker unavailable','Current cap retained']}: {})};
  }
  if(body !== undefined) return {done:'Accepted',changed:true};
  throw new Error('Unexpected fixture request '+url);
};
window.DashboardQueue = {model(){return {state:{page:null,stale:true,loading:false},observe(){},invalidate(){},open(){},close(){},suspend(){},resume(){},read(){},navigate(){}};},editor(){return {node:document.createElement('div'),resetRows(){},update(){},rowControls(){return document.createElement('span');}};}};
window.session = {state:{pending:null},cancel(){this.state.pending=null;}};
window.ws = null;
window.ctl = DashboardCore.model({call, changed:s=>window.ws?.observe(s)});
window.ws = DashboardWorkspace.create({call,controller:ctl,session,el:(tag,cls,text)=>{const n=document.createElement(tag);if(cls)n.className=cls;if(text!=null)n.textContent=text;return n;},icon:()=>document.createElement('span'),openSearch(){},native:(verb,args)=>ctl.command(verb,args),immerse(){}});
await ctl.refresh(); ctl.connection(true);
window.posts=()=>calls.filter(c=>c.body!==undefined);
'''

async def page_for(browser, width=390):
    page = await browser.new_page(viewport={'width':width,'height':900})
    page.set_default_timeout(5000)
    ids = ['queue-items','queue-status','queue-page','queue-prev','queue-next','queue-refresh','tab-picks','tab-queue','dj-picks-panel','speaker-queue-panel','dock-queue','queue-title','room-filter','room-choices','close-rooms','search-input','search-results','shortcut-help','close-shortcuts','toggle-volume']
    host = '<!doctype html><html><head><style>*{box-sizing:border-box}body{margin:12px;font-family:Arial,sans-serif}.host-controls{display:none}button{font:inherit;padding:8px}p{line-height:1.5}</style></head><body><div class="host-controls">'
    host += ''.join(f'<button id="{i}"></button>' for i in ids)
    host += '<div class="player-dock"></div><dialog id="rooms-dialog"></dialog><dialog id="search-dialog"></dialog><dialog id="shortcuts-dialog"></dialog></div><footer class="page-footer">Fixture footer</footer></body></html>'
    await page.set_content(host)
    await page.add_style_tag(content=(ASSETS/'dashboard-activity.css').read_text())
    for name in ['dashboard-core.js','dashboard-activity.js','dashboard-activity-panel.js','dashboard-workspace.js']:
        await page.add_script_tag(content=(ASSETS/name).read_text())
    await page.evaluate('(async()=>{'+SETUP+'})()')
    return page

async def opened(page):
    await page.locator('#dashboard-activity > summary').click()
    await page.wait_for_function("document.querySelector('.activity-candidate').textContent.includes('Original result:')")
    await page.wait_for_function("!document.querySelector('[data-activity-focus=review]').disabled")

async def run():
    results=[]
    async with async_playwright() as p:
        browser = await p.chromium.launch(executable_path=os.environ.get('CHROMIUM','/usr/bin/chromium'),args=['--no-sandbox'])
        async def case(name, task):
            page=await page_for(browser)
            errors=[]; page.on('pageerror',lambda e:errors.append(str(e)))
            try:
                await task(page)
                assert not errors, errors
                results.append({'name':name,'passed':True})
                print('PASS',name,flush=True)
            finally: await page.close()
        async def lazy(page):
            assert await page.evaluate("calls.filter(c=>c.url.startsWith('/actions')||c.url==='/undo/preview').length") == 0
            await opened(page)
            assert await page.evaluate('posts().length') == 0
            count=await page.evaluate('calls.length')
            await page.locator('.activity-list summary').click()
            assert await page.locator('.activity-list').inner_text() and 'clamp: quiet hours' in await page.locator('.activity-list').inner_text()
            assert await page.evaluate('calls.length') == count
        await case('lazy real-history inspection and nested details are read-only',lazy)
        async def search(page):
            await opened(page); count=await page.evaluate('calls.length')
            field=page.get_by_label('Find in loaded actions');await field.fill('QUIET');assert await page.locator('.activity-list details').count()==1
            await field.fill('missing');assert await page.locator('.activity-list details').count()==0
            assert await page.evaluate('document.activeElement.getAttribute("aria-label")')=='Find in loaded actions'
            assert await page.evaluate('calls.length')==count
        await case('local filtering preserves focus and sends no reads',search)
        async def cancel(page):
            await opened(page);await page.locator('[data-activity-focus=review]').click()
            assert 'There is no redo' in await page.locator('.activity-review').inner_text()
            assert await page.evaluate('document.activeElement.dataset.activityFocus')=='cancel'
            await page.locator('[data-activity-focus=cancel]').click()
            assert await page.evaluate('document.activeElement.dataset.activityFocus')=='review'
            assert await page.evaluate('posts().length')==0
        await case('exact undo review focuses the non-destructive choice and cancel restores focus',cancel)
        async def success(page):
            await opened(page);await page.locator('[data-activity-focus=review]').click();await page.locator('[data-activity-focus=confirm]').click()
            await page.wait_for_function("document.querySelector('.activity-outcome').textContent.includes('No restoration failures')")
            assert await page.evaluate('posts()')==[{'url':'/undo/checked','body':{'expected_id':9}}]
            assert 'Undo attempt for #9' in await page.locator('.activity-list').text_content()
        await case('confirmation uses the existing mutation lane and exact checked request',success)
        async def changed(page):
            await opened(page);await page.locator('[data-activity-focus=review]').click()
            await page.evaluate('data.candidate={...row,id:11}')
            await page.locator('[data-activity-focus=confirm]').click()
            await page.wait_for_function("document.querySelector('.activity-outcome').textContent.includes('eligible action changed')")
            assert await page.evaluate('posts().length')==0
        await case('a new eligible action is refused instead of substituted',changed)
        async def partial(page):
            await opened(page);await page.evaluate("mode='partial'");await page.locator('[data-activity-focus=review]').click();await page.locator('[data-activity-focus=confirm]').click()
            await page.wait_for_function("document.querySelector('.activity-outcome').textContent.includes('Partial restoration')")
            assert await page.get_by_label('Restoration failures').locator('li').count()==2
            assert await page.evaluate('posts().length')==1
        await case('partial restoration keeps every reported failure visible',partial)
        async def lost(page):
            await opened(page);await page.evaluate("mode='lost'");await page.locator('[data-activity-focus=review]').click();await page.locator('[data-activity-focus=confirm]').click()
            await page.wait_for_function('ctl.state.uncertain')
            await page.locator('[data-activity-focus=refresh]').click()
            await page.evaluate('ctl.refresh()')
            assert await page.evaluate('ctl.state.uncertain')
            assert await page.locator('[data-activity-focus=review]').is_disabled()
            assert await page.evaluate('posts().length')==1
        await case('lost acknowledgement stays locked through passive and activity refresh',lost)
        async def hidden_preflight(page):
            await opened(page);await page.locator('[data-activity-focus=review]').click();await page.evaluate("mode='hold-preview'")
            await page.locator('[data-activity-focus=confirm]').click();await page.wait_for_function('hold!==null')
            await page.evaluate('ws.suspend();ctl.suspend();hold()')
            await page.wait_for_function('!ctl.state.busy')
            assert await page.evaluate('posts().length')==0
        await case('workspace suspension retires an unfinished undo preflight',hidden_preflight)
        async def hidden_receipt(page):
            await opened(page);await page.locator('[data-activity-focus=review]').click();await page.evaluate("mode='hold-write'")
            await page.locator('[data-activity-focus=confirm]').click();await page.wait_for_function('hold!==null')
            assert await page.evaluate("ctl.command('next')") is False
            await page.evaluate('ws.suspend();ctl.suspend();hold()')
            await page.wait_for_function('!ctl.state.busy')
            assert 'No restoration failures' in await page.locator('.activity-outcome').text_content()
            assert await page.evaluate('posts().length')==1
        await case('sent undo survives hiding and excludes simultaneous transport commands',hidden_receipt)
        async def target(page):
            await opened(page);await page.locator('[data-activity-focus=review]').click()
            await page.evaluate("window.oldConfirm=document.querySelector('[data-activity-focus=confirm]');ctl.select('Synthetic room@S1');oldConfirm.click()")
            assert await page.evaluate('posts().length')==0
        await case('retained confirmation cannot cross a household selection change',target)
        async def immersive(page):
            await opened(page);await page.locator('[data-activity-focus=review]').click()
            await page.evaluate('ws.immerse(true)')
            assert await page.locator('.activity-review').is_hidden()
            assert await page.locator('[data-activity-focus=review]').is_disabled()
            await page.evaluate('ws.immerse(false)')
            await page.wait_for_function("!document.querySelector('[data-activity-focus=review]').disabled")
            assert await page.evaluate('posts().length')==0
        await case('immersive mode suspends recovery controls without writing',immersive)
        async def metadata(page):
            await page.evaluate("data.rows=[{...row,intent:'<img src=x onerror=alert(1)>',result:'<script>window.bad=true</script>'}]")
            await opened(page);assert await page.locator('.activity-list img, .activity-list script').count()==0
            assert '<img' in await page.locator('.activity-list summary').text_content()
        await case('action metadata remains literal text, not executable markup',metadata)
        for width in [320,390,768,1440]:
            page=await page_for(browser,width)
            try:
                await page.evaluate("data.rows=[{...row,intent:'Long '+ 'X'.repeat(1000),result:'Y'.repeat(1000)}];data.candidate={...row,intent:'Long '+ 'X'.repeat(1000)}")
                await opened(page);await page.locator('.activity-list summary').click();await page.locator('[data-activity-focus=review]').click()
                assert await page.evaluate('document.documentElement.scrollWidth <= innerWidth'),f'Overflow at {width}'
                print('PASS responsive activity and review',width,flush=True)
                results.append({'name':f'long-metadata layout {width}px','passed':True})
                if width==390 and os.environ.get('SCREENSHOT'): await page.screenshot(path=os.environ['SCREENSHOT'],full_page=True)
            finally: await page.close()
        await browser.close()
    print(json.dumps({'passed':len(results),'cases':results},indent=2))

if __name__=='__main__': asyncio.run(run())
