#!/usr/bin/env python3
"""Chromium component checks for reviewed source playback across room sets.

Mounts the shipped phone and dashboard source controls, their shared validators,
and their real command-lane implementations with synthetic API observations.
No daemon, Spotify account, speaker, or browser network request is used.
Requires Python Playwright and an installed Chromium.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import shutil

from playwright.sync_api import expect, sync_playwright


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "crates/fsonos-api/src/remote"
TRACK = "spotify:track:1000000000000000000001"

FIXTURE = r"""
window.fixture = {
  calls: [], writes: [], result: 'complete', hold: '', release: null,
  refreshed: [], errors: [], api: null, controller: null,
  groups: [
    {household:'S1', coordinator:'fixture-s1-den', coordinator_room:'Den', members:[
      {household:'S1',room:'Den',player:'fixture-s1-den',coordinator:'fixture-s1-den'},
      {household:'S1',room:'Hall',player:'fixture-s1-hall',coordinator:'fixture-s1-den'}]},
    {household:'S2', coordinator:'fixture-s2-den', coordinator_room:'Den', members:[
      {household:'S2',room:'Den',player:'fixture-s2-den',coordinator:'fixture-s2-den'}]},
    {household:'S2', coordinator:'fixture-s2-study', coordinator_room:'Study', members:[
      {household:'S2',room:'Study',player:'fixture-s2-study',coordinator:'fixture-s2-study'}]},
  ],
  titles: {S1:'Evening mix', S2:'Evening mix'},
  sources: {'Den@S1':'fixture:original-s1','Den@S2':'fixture:original-s2','Study@S2':'fixture:study'},
  revisions: {'Den@S1':'fixture-q-s1','Den@S2':'fixture-q-s2','Study@S2':'fixture-q-study'},
};
const f = fixture, clone = value => structuredClone(value);
f.address = g => `${g.coordinator_room}@${g.household}`;
f.group = target => f.groups.find(g => g.members.some(r => `${r.room}@${r.household}` === target));
f.zone = g => ({household:g.household,coordinator_room:g.coordinator_room,
  members:g.members.map(r=>r.room),transport_state:'paused'});
f.resolve = target => {
  let selected, matched;
  if(target==='downstairs') { selected=f.groups.slice(0,2); matched='alias'; }
  else if(target==='single') { selected=[f.groups[0]]; matched='alias'; }
  else if(target==='all@S2') { selected=f.groups.filter(g=>g.household==='S2'); matched='everywhere'; }
  else if(target==='everywhere'||target==='all') { selected=f.groups; matched='everywhere'; }
  else { selected=f.groups.filter(g=>g.members.some(r=>`${r.room}@${r.household}`===target)); matched='room'; }
  if(!selected.length)throw new Error('Unknown fixture target '+target);
  return clone({target,matched,rooms:selected.flatMap(g=>g.members),groups:selected});
};
f.state = target => {
  const g=f.group(target); if(!g)throw new Error('Unknown fixture state '+target);
  const queue=g.household==='S1';
  return {zone:f.zone(g),transport_state:'paused',volume:30,group_volume:30,mute:false,
    track:{uri:f.sources[f.address(g)],title:`Current ${g.household} recording`,
      queue_position:queue?2:null,position_secs:17,duration_secs:120}};
};
f.queue = target => {
  const g=f.group(target), total=g.household==='S1'?3:7;
  return {zone:g.coordinator_room,total,revision:f.revisions[f.address(g)],
    playing:g.household==='S1'?2:null,
    items:[{position:1,uri:`fixture:first-${g.household}`,title:'First retained occurrence'}]};
};
f.catalog = (kind,target) => {
  const g=f.group(target), id=kind==='favorite'
    ? g.household==='S1'?'FV:2/2':'FV:2/9' : g.household==='S1'?'SQ:7':'SQ:42';
  return [{id,title:f.titles[g.household],...(kind==='favorite'?{kind:'track',description:'Fixture native source'}:{})}];
};
f.receipt = body => {
  const groups=f.resolve(body.zone).groups, count=body.playlist?3:1;
  const added=groups.map(g=>({household:g.household,coordinator:g.coordinator,
    queue:{done:`Added ${count} source track(s)`,zone:g.coordinator_room,
      first:body.next&&g.household==='S1'?3:g.household==='S1'?4:8,
      count,playing_queue:g.household==='S1'}}));
  if(groups.length===1)return added[0].queue;
  const failed=[];
  if(f.result==='partial'||f.result==='uncertain') {
    const g=groups.at(-1);added.pop();
    failed.push({household:g.household,coordinator:g.coordinator,zone:g.coordinator_room,
      error:{code:f.result==='uncertain'?'OUTCOME_UNCERTAIN':'UPNP_FAULT',
        detail:f.result==='uncertain'?'The insertion reply was lost.':'Speaker refused this insertion.',
        hint:'Read this group before another targeted add; never repeat the whole batch.',
        suggestions:[],retryable:false}});
  }
  const result={done:`Queued in ${added.length} of ${groups.length} groups`,
    complete:failed.length===0,added,failed,logged:true,undoable:f.result!=='uncertain'};
  if(f.result==='malformed')result.added[1].coordinator=result.added[0].coordinator;
  return result;
};
f.call = async (url,body,options={}) => {
  f.calls.push({url,body:body===undefined?null:clone(body)});
  if(body!==undefined) {
    f.writes.push({url,body:clone(body)});
    if(f.hold==='write')await new Promise(resolve=>{f.release=resolve;});
    if(f.result==='lost')throw new Error('Fixture lost acknowledgement');
    if(url==='/queue')return clone(f.receipt(body));
    if(!['/play','/play/favorite','/sonos/playlists/play','/pause'].includes(url))throw new Error('Unexpected write '+url);
    return {done:'Source accepted once for the selected groups',changed:true,notes:[]};
  }
  const parsed=new URL(url,'http://fixture.invalid'),query=parsed.searchParams;
  if(f.hold==='state'&&parsed.pathname.endsWith('/state'))await new Promise(resolve=>{f.release=resolve;});
  if(parsed.pathname==='/rooms')return f.groups.flatMap(g=>g.members.map(r=>({
    name:r.room,household:r.household,zone:g.coordinator_room,aliases:['downstairs']})));
  if(parsed.pathname==='/rooms/resolve')return f.resolve(query.get('target'));
  if(parsed.pathname==='/zones')return f.groups.map(f.zone);
  if(parsed.pathname==='/schedules')return [];
  if(parsed.pathname==='/favorites')return f.catalog('favorite',query.get('zone'));
  if(parsed.pathname==='/sonos/playlists')return f.catalog('playlist',query.get('zone'));
  const match=/^\/zones\/([^/]+)\/(state|queue)$/.exec(parsed.pathname);
  if(match)return clone(match[2]==='state'?f.state(decodeURIComponent(match[1])):f.queue(decodeURIComponent(match[1])));
  throw new Error('Unexpected fixture read '+url);
};
f.phoneCall = (name,args={}) => {
  const routes={zones:['GET','/zones'],state:['GET','/zones/{room}/state'],favorites:['GET','/favorites'],
    ...RoomSetsRemote.routes,...SourcePlayerRemote.routes,...SourceTargetsRemote.routes};
  const route=routes[name];if(!route)throw new Error('Unregistered fixture route '+name);
  const path=route[1].replace(/\{(\w+)\}/g,(_,key)=>encodeURIComponent(args.params?.[key]));
  return f.call(path+(args.query?'?'+new URLSearchParams(args.query):''),args.body,{signal:args.signal});
};
f.replaceIdentity = () => {
  const g=f.groups[1];g.coordinator='fixture-s2-replaced';
  for(const room of g.members){room.coordinator=g.coordinator;room.player=g.coordinator;}
};
"""


def fragment(source: str, start: str, end: str) -> str:
    """Retain real production helpers without booting unrelated page features."""
    return source[source.index(start):source.index(end, source.index(start))]


def phone_lanes() -> str:
    source = (ASSETS / "remote.js").read_text()
    return "\n".join([
        "const house={cards:new Map(),favorites:new Map([['S1',[{}]],['S2',[{}]]])};"
        "const transportFlights=new Map(),volumeFlights=new Map();",
        fragment(source, "function notifyCard(", "function checkedZones("),
        fragment(source, "function ownsCard(", "function disposeCard("),
        fragment(source, "function transportRooms(", "async function act("),
        "async function refreshZone(zone){fixture.refreshed.push(roomName(zone.coordinator_room,zone.household));}",
    ])


PHONE_MOUNT = r"""
const controls=document.getElementById('fixture-cards');
for(const g of f.groups) {
  const zone=f.zone(g),parts={zone,writing:false,disposed:false,readVersion:0,sliders:new Map(),panels:[]};
  for(const key of ['play','next','previous','favorite','dj','skip','mood','like','dislike']) {
    const button=document.createElement('button');button.type='button';button.textContent=key;
    button.dataset.lane=f.address(g);button.dataset.control=key;parts[key]=button;controls.append(button);
  }
  for(const member of g.members) {
    const input=document.createElement('input'),mute=document.createElement('button');
    input.type='range';input.value='30';mute.textContent='Mute';
    parts.sliders.set(member.room,{room:`${member.room}@${member.household}`,input,mute,
      timer:null,pendingVolume:null,volumeWriting:false,mutedPending:false,version:0});
    controls.append(input,mute);
  }
  house.cards.set(zoneKey(zone),parts);
  parts.play.addEventListener('click',async()=>{
    if(!beginAction(parts))return;
    try{await f.call('/pause',{zone:f.address(g)});}catch(error){f.errors.push(error.message);}
    finally{endAction(parts);}
  });
}
const levels=RoomSetsRemote.levelLanes({cards:()=>house.cards.values(),flights:volumeFlights});
const executeSet=RoomSetsRemote.coordinated({cards:()=>house.cards.values(),refresh:async()=>{},
  begin:beginAction,end:endAction,shape:groupShape,levels});
f.api=SourcePlayerRemote.panel({el,call:f.phoneCall,executeSet,
  async execute(group,work) {
    const parts=[...house.cards.values()].find(card=>groupShape(card.zone)===group.signature);
    if(!parts||!beginAction(parts))throw new Error('Fixture group action is busy.');
    try{return await work();}finally{endAction(parts);}
  }});
document.getElementById('fixture').prepend(f.api.node);
await f.api.refresh();f.api.node.querySelector('details').open=true;
"""

DASHBOARD_MOUNT = r"""
const transport=document.getElementById('fixture-transport');
f.controller=DashboardCore.model({call:f.call,now:()=>1000,changed:()=>{
  f.api?.observe();transport.disabled=!f.controller?.canControl();
}});
f.api=DashboardSource.panel({call:f.call,controller:f.controller,now:()=>1000});
transport.addEventListener('click',()=>{void f.controller.command('pause');});
await f.controller.refresh();f.controller.connection(true);f.controller.select('Den@S1');
document.getElementById('open-source').click();
"""


def setup(browser, kind: str, width: int = 390):
    page = browser.new_page(viewport={"width": width, "height": 1000}, reduced_motion="reduce")
    page.set_default_timeout(5000)
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    # Any accidental resource fetch fails closed; all API replies are in memory.
    page.route("**/*", lambda route: route.abort())
    dialog = ""
    if kind == "dashboard":
        html = (ASSETS / "dashboard.html").read_text()
        dialog = fragment(html, '<dialog id="source-dialog"', '</dialog>') + '</dialog>'
    page.set_content('<!doctype html><html data-theme="light"><head><meta charset="utf-8">'
                     '<meta name="viewport" content="width=device-width,initial-scale=1"></head>'
                     '<body><main id="fixture" style="max-width:960px;margin:auto;padding:12px">'
                     '<button id="open-source" type="button">Music source</button>'
                     '<button id="fixture-transport" type="button">Pause selected group</button>'
                     '<div id="fixture-cards"></div>' + dialog + '</main></body></html>')
    styles = ["remote.css", "source-player.css"] if kind == "phone" else ["dashboard.css", "dashboard-source.css"]
    for name in styles:
        page.add_style_tag(content=(ASSETS / name).read_text())
    source = (ASSETS / "remote.js").read_text()
    page.add_script_tag(content="globalThis.module={exports:{}};\n" + fragment(source, "function el(", "let toastTimer"))
    for name in ["room-sets.js", "source-targets.js", "source-player.js", "dashboard-core.js", "dashboard-source.js"]:
        page.add_script_tag(content=(ASSETS / name).read_text())
    page.add_script_tag(content=FIXTURE)
    if kind == "phone":
        page.add_script_tag(content=phone_lanes())
    page.evaluate("async()=>{" + (PHONE_MOUNT if kind == "phone" else DASHBOARD_MOUNT) + "}")
    assert not errors, errors
    return page, errors


def writes(page):
    return page.evaluate("fixture.writes")


def scope(page, kind):
    return page.locator(".source-player" if kind == "phone" else "#source-dialog")


def choose(page, kind, mode="play", native=None, target="downstairs"):
    area = scope(page, kind)
    area.get_by_label("Destination selection", exact=True).select_option("set")
    area.get_by_label("Rooms or room set", exact=True).fill(target)
    if native:
        area.get_by_label("Music source", exact=True).select_option(native)
        area.get_by_label("Favorite or playlist title", exact=True).fill("Evening mix")
    else:
        source = area.get_by_label("Spotify link or audio stream address", exact=True) if kind == "phone" else area.locator("#source-input")
        source.fill(TRACK)
    action = area.get_by_label("Play now or add to queue", exact=True) if kind == "phone" else area.locator("#source-mode")
    action.select_option(mode)


def review(page, kind):
    scope(page, kind).get_by_role("button", name=re.compile(r"^Review (playback|queue addition)$")).click()
    page.wait_for_function("fixture.api.state.pending !== null")
    panel = page.locator(".source-review" if kind == "phone" else "#source-preview")
    expect(panel).to_be_visible()
    return panel


def confirm(page, kind):
    panel = page.locator(".source-review" if kind == "phone" else "#source-preview")
    button = panel.get_by_role("button", name=re.compile(r"^(Play now|Queue next|Queue at end|Add to end)$"))
    button.click()


def settled(page):
    page.wait_for_function("!fixture.api.state.writing && fixture.api.state.result !== null")
    return page.evaluate("fixture.api.state.result")


def playback(page, kind):
    choose(page, kind)
    panel = review(page, kind)
    text = panel.inner_text()
    assert "Hall" in text and "S1" in text and "S2" in text, text
    assert "Study" not in text, "Unselected groups must not enter the reviewed audience."
    assert writes(page) == [], "Review must not dispatch playback."
    confirm(page, kind)
    result = settled(page)
    assert result["status"] == "accepted" and not result.get("partial", False), result
    assert writes(page) == [{"url": "/play", "body": {"zone": "downstairs", "source_uri": TRACK}}]


def queue_results(page, kind, result_kind="complete"):
    choose(page, kind, mode="next")
    review(page, kind)
    page.evaluate("value => {fixture.result=value;}", result_kind)
    confirm(page, kind)
    result = settled(page)
    assert result["status"] == "accepted", result
    assert result.get("partial", False) == (result_kind != "complete"), result
    detail = result["detail"]
    assert "Den@S1" in detail and "Den@S2" in detail, detail
    assert "position 3" in detail, detail
    if result_kind == "complete":
        assert "position 8" in detail, detail
        assert "not using its queue" in detail, detail
    else:
        assert ("OUTCOME_UNCERTAIN" if result_kind == "uncertain" else "UPNP_FAULT") in detail, detail
        assert "whole batch" in detail, detail
        if result_kind == "uncertain":
            assert "No safe batch undo" in detail, detail
    assert writes(page) == [{"url": "/queue", "body": {"zone": "downstairs", "source_uri": TRACK, "next": True}}]


def native_titles(page, kind, native="playlist"):
    choose(page, kind, mode="end", native=native)
    panel = review(page, kind)
    text = panel.inner_text()
    expected_ids = ["SQ:7", "SQ:42"] if native == "playlist" else ["FV:2/2", "FV:2/9"]
    for native_id in expected_ids:
        assert native_id in text, text
    assert writes(page) == []
    confirm(page, kind)
    assert settled(page)["status"] == "accepted"
    sent = writes(page)
    assert sent == [{"url": "/queue", "body": {"zone": "downstairs", native: "Evening mix", "next": False}}], sent
    assert not any(native_id in str(sent[0]["body"]) for native_id in expected_ids), "An opaque household ID was transplanted."


def stale_review(page, kind, change="identity"):
    choose(page, kind)
    review(page, kind)
    if change == "identity":
        page.evaluate("fixture.replaceIdentity()")
    else:
        page.evaluate("fixture.sources['Den@S2']='fixture:another-controller'")
    confirm(page, kind)
    result = settled(page)
    assert result["status"] == "not_sent", result
    assert writes(page) == [], writes(page)


def uncertain_reply(page, kind, response="lost"):
    choose(page, kind, mode="end")
    review(page, kind)
    page.evaluate("value => {fixture.result=value;}", response)
    confirm(page, kind)
    result = settled(page)
    assert result["status"] == "uncertain", result
    assert len(writes(page)) == 1, writes(page)
    assert page.evaluate("fixture.api.state.pending === null")
    if kind == "dashboard":
        assert page.evaluate("fixture.controller.state.uncertain")
        assert not page.evaluate("fixture.controller.canControl()")


def shared_lanes(page, kind):
    choose(page, kind)
    review(page, kind)
    page.evaluate("fixture.hold='write'")
    confirm(page, kind)
    page.wait_for_function("fixture.writes.length === 1 && fixture.release !== null")
    assert page.evaluate("fixture.api.state.writing")
    expect(scope(page, kind).get_by_label("Destination selection", exact=True)).to_be_disabled()
    if kind == "phone":
        for target in ["Den@S1", "Den@S2"]:
            expect(page.locator(f'[data-lane="{target}"][data-control="play"]')).to_be_disabled()
        expect(page.locator('[data-lane="Study@S2"][data-control="play"]')).to_be_enabled()
        assert page.evaluate("transportFlights.size") == 3, "All rooms, including the grouped member, must share the source lease."
    else:
        assert page.evaluate("fixture.controller.state.busy")
        expect(page.locator("#fixture-transport")).to_be_disabled()
    page.evaluate("fixture.hold='';fixture.release()")
    assert settled(page)["status"] == "accepted"
    assert len(writes(page)) == 1
    if kind == "phone":
        assert page.evaluate("transportFlights.size") == 0
        assert page.evaluate("volumeFlights.size") == 0
    else:
        assert not page.evaluate("fixture.controller.state.busy")


def already_busy(page, kind):
    choose(page, kind)
    review(page, kind)
    page.evaluate("fixture.hold='write'")
    # Begin an existing card/controller action while the source review waits.
    # A dashboard modal makes the unrelated transport button inert, so trigger
    # its real listener directly to model a previously admitted UI callback.
    selector = '[data-lane="Den@S2"][data-control="play"]' if kind == "phone" else "#fixture-transport"
    page.locator(selector).evaluate("button => button.click()")
    page.wait_for_function("fixture.writes.length === 1 && fixture.release !== null")
    if kind == "phone":
        confirm(page, kind)
        assert settled(page)["status"] == "not_sent"
        assert page.evaluate("transportFlights.size") == 1, "A failed set lease must release earlier acquired groups."
    else:
        expect(page.locator("#source-confirm")).to_be_disabled()
    assert writes(page) == [{"url": "/pause", "body": {"zone": "Den@S2" if kind == "phone" else "Den@S1"}}]
    page.evaluate("fixture.hold='';fixture.release()")
    page.wait_for_function("!fixture.controller?.state.busy" if kind == "dashboard" else "transportFlights.size === 0")


def single_group_alias(page, kind):
    choose(page, kind, mode="end", target="single")
    review(page, kind)
    confirm(page, kind)
    result = settled(page)
    assert result["status"] == "accepted", result
    assert "position 4" in result["detail"], result
    assert writes(page) == [{"url": "/queue", "body": {"zone": "single", "source_uri": TRACK, "next": False}}]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--browser-executable", default=shutil.which("chromium") or shutil.which("chromium-browser"))
    args = parser.parse_args()
    if not args.browser_executable:
        parser.error("Pass --browser-executable pointing to Chromium.")
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True, executable_path=args.browser_executable,
            args=["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"])
        try:
            for kind in ["phone", "dashboard"]:
                cases = [
                    ("reviewed alias playback", playback),
                    ("per-group queue positions", queue_results),
                    ("definite partial receipt", lambda p, k: queue_results(p, k, "partial")),
                    ("uncertain partial receipt", lambda p, k: queue_results(p, k, "uncertain")),
                    ("household playlist titles", native_titles),
                    ("household favorite titles", lambda p, k: native_titles(p, k, "favorite")),
                    ("changed coordinator identity", stale_review),
                    ("changed playback source", lambda p, k: stale_review(p, k, "source")),
                    ("lost reply without replay", uncertain_reply),
                    ("contradictory receipt without replay", lambda p, k: uncertain_reply(p, k, "malformed")),
                    ("source reserves existing lanes", shared_lanes),
                    ("existing lane blocks source", already_busy),
                    ("single-group alias compatibility", single_group_alias),
                ]
                for name, check in cases:
                    page, errors = setup(browser, kind, 390 if kind == "phone" else 1440)
                    try:
                        check(page, kind)
                        assert not errors, errors
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "The controls overflow the viewport."
                    finally:
                        page.close()
                    print(f"PASS {kind}: {name}", flush=True)
        finally:
            browser.close()


if __name__ == "__main__":
    main()
