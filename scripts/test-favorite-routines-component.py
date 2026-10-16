#!/usr/bin/env python3
"""Real Chromium component checks with in-memory transport and storage fixtures.

No browser network navigation is needed. These are not daemon or real-speaker
acceptance tests. Requires Python Playwright and an installed Chromium.
"""
from pathlib import Path
import argparse
import shutil
from playwright.sync_api import expect, sync_playwright

ROOT = Path(__file__).resolve().parents[1]
REMOTE = ROOT / "crates/fsonos-api/src/remote"
FIXTURE = r"""
const fixture = {
  groups: [{household:'S2', coordinator_room:'Kitchen', members:['Kitchen','Study']}],
  favorites: [{id:'FV:2/1', title:'Morning Music', kind:'container', description:'Spotify'}],
  scenes:[], rows:[], writes:[], favoriteReads:0, noFavorites:false, mode:'normal', accepted:false, release:null,
};
const fixtureData = new Map();
const fixtureStorage = { getItem: k => fixtureData.get(k) ?? null,
  setItem: (k,v) => fixtureData.set(k,v), removeItem: k => fixtureData.delete(k) };
const copy = v => JSON.parse(JSON.stringify(v));
async function fixtureCall(name, {body, params, query}={}) {
  if (name==='routineZones') return copy(fixture.groups);
  if (name==='routineFavorites') {
    fixture.favoriteReads++;
    if(fixture.noFavorites) throw new Error('Favorites are offline');
    if (query.zone!=='Kitchen@S2') throw new Error('wrong household');
    return copy(fixture.favorites);
  }
  if (name==='routineScenes') return copy(fixture.scenes);
  if (name==='routineSchedules') return copy(fixture.rows);
  fixture.writes.push([name, copy(body)]);
  if (name==='routineSaveScene') {
    const scene = {name:params.name, saved:100, ...copy(body)};
    if (scene.groups[0].source.kind === 'favorite') scene.groups[0].source={kind:'favorite', name:fixture.favorites[0].title, uri:'x-rincon-cpcontainer:fixture'};
    fixture.scenes.push(scene); fixture.accepted=true;
    if (fixture.mode==='hold_put') await new Promise(resolve => {fixture.release=resolve;});
    if (fixture.mode==='lost_put') throw new Error('lost scene reply');
    return copy(scene);
  }
  if (name==='routineAdd') {
    if (fixture.mode==='refuse_post') throw new Error('server error after submission');
    const row={id:fixture.rows.length+1, action:'apply_scene', scene:body.scene,
      when:body.when.startsWith('in ')?'once 2026-10-16T07:30:00+00:00':body.when,
      enabled:true, creator:'loopback-http', summary:'apply scene', next:'2026-10-16T07:30:00+00:00'};
    fixture.rows.push(row);
    if (fixture.mode==='lost_post') throw new Error('lost schedule reply');
    if (fixture.mode==='malformed_post') return {};
    return copy(row);
  }
  throw new Error('Unexpected route '+name);
}
let mounted;
function mountFixture() {
  mounted?.dispose();
  mounted?.node.remove();
  mounted=FavoriteRoutinePanel.panel({core:FavoriteRoutines, call:fixtureCall, el,
    journal:FavoriteRoutinePanel.sessionJournal(()=>fixtureStorage)});
  document.querySelector('#scenes-section').before(mounted.node);
}
mountFixture();
"""


def setup(browser):
    page = browser.new_page()
    page.set_default_timeout(5000)
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.set_content('<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width"></head>'
        '<body><main><details id="scenes-section"><summary>Scenes</summary></details></main></body></html>')
    page.add_style_tag(content=(REMOTE / "favorite-routines.css").read_text())
    # Extract the actual DOM helper without starting the rest of the remote.
    helper = (REMOTE / "remote.js").read_text().split("function el(tag, attrs = {}, ...children) {", 1)[1].split("let toastTimer = 0;", 1)[0]
    page.add_script_tag(content="globalThis.module={exports:{}};function el(tag, attrs = {}, ...children) {" + helper)
    for name in ("favorite-routines.js", "favorite-routines-panel.js"):
        page.add_script_tag(content=(REMOTE / name).read_text())
    page.add_script_tag(content=FIXTURE)
    return page, errors


def choose(page):
    page.locator("#favorite-routines-section > summary").click()
    page.get_by_label("Routine name", exact=True).fill("Morning")
    page.get_by_role("combobox", name="Group to save", exact=True).select_option("Kitchen@S2")
    page.get_by_role("combobox", name="Playable favorite", exact=True).select_option("FV:2/1")


def review(page):
    page.get_by_role("button", name="Review music routine", exact=True).click()
    expect(page.get_by_role("region", name="Review music routine")).to_be_visible()
    expect(page.get_by_role("button", name="Cancel review")).to_be_focused()


def writes(page):
    return page.evaluate("fixture.writes.map(([name])=>name)")


def normal(page):
    assert page.evaluate("mounted.node.tagName") == 'SECTION'
    choose(page)
    review(page)
    assert writes(page) == []
    expect(page.get_by_text("Exact saved group: Kitchen@S2 + Study@S2", exact=True)).to_be_visible()
    page.get_by_role("button", name="Save scene and create routine", exact=True).click()
    expect(page.get_by_role("status")).to_contain_text("acknowledged")
    assert writes(page) == ['routineSaveScene', 'routineAdd']
    assert page.evaluate("fixture.writes[0][1].groups[0].source.name") == 'FV:2/1'
    assert page.evaluate("fixture.writes[0][1].volumes") == {}
    assert page.evaluate("fixture.writes[0][1].mutes") == {}
    page.evaluate("mountFixture()")
    page.locator("#favorite-routines-section > summary").click()
    expect(page.get_by_role("status")).to_contain_text("previous setup")
    page.get_by_role("button", name="Inspect saved state").click()
    expect(page.get_by_role("status")).to_contain_text("observation")
    assert writes(page) == ['routineSaveScene', 'routineAdd']


def cancel_delay(page):
    choose(page)
    review(page)
    page.keyboard.press("Escape")
    expect(page.get_by_role("button", name="Review music routine", exact=True)).to_be_focused()
    page.get_by_role("combobox", name="When", exact=True).select_option("delay")
    page.get_by_label("Delay in minutes").fill("45")
    review(page)
    page.get_by_role("button", name="Save scene and create routine", exact=True).click()
    expect(page.get_by_role("status")).to_contain_text("acknowledged")
    assert page.evaluate("fixture.writes[1][1].when") == 'in 45m'


def lost_put(page):
    page.evaluate("fixture.mode='lost_put'")
    choose(page)
    review(page)
    page.get_by_role("button", name="Save scene and create routine", exact=True).click()
    expect(page.get_by_role("status")).to_contain_text("needs inspection")
    page.evaluate("mountFixture()")
    page.locator("#favorite-routines-section > summary").click()
    page.get_by_role("button", name="Inspect saved state").click()
    expect(page.get_by_role("status")).to_contain_text("routine not submitted")
    assert writes(page) == ['routineSaveScene']
    page.get_by_role("button", name="Review scheduling saved scene").click()
    expect(page.get_by_role("button", name="Cancel review")).to_be_focused()
    page.get_by_role("button", name="Create routine for saved scene").click()
    expect(page.get_by_role("status")).to_contain_text("acknowledged")
    assert writes(page) == ['routineSaveScene', 'routineAdd']


def lost_post(page, mode='lost_post'):
    page.evaluate('(mode)=>{fixture.mode=mode}', mode)
    choose(page)
    review(page)
    page.get_by_role("button", name="Save scene and create routine", exact=True).click()
    expect(page.get_by_role("status")).to_contain_text("needs inspection")
    page.evaluate("mountFixture()")
    page.locator("#favorite-routines-section > summary").click()
    for _ in range(2):
        page.get_by_role("button", name="Inspect saved state").click()
        expect(page.get_by_role("status")).to_contain_text('may still finish' if mode=='refuse_post' else 'observation')
    assert writes(page) == ['routineSaveScene', 'routineAdd']
    if mode=='refuse_post':
        expect(page.get_by_role("button", name="New music routine")).to_have_count(0)


def close_put(page):
    page.evaluate("fixture.mode='hold_put'")
    choose(page)
    review(page)
    page.get_by_role("button", name="Save scene and create routine", exact=True).click()
    page.wait_for_function("fixture.accepted")
    page.locator("#favorite-routines-section > summary").click()
    # Wait for the actual toggle event to invalidate further dispatch.
    page.wait_for_function("!document.querySelector('#favorite-routines-section').open")
    page.evaluate("fixture.release()")
    page.wait_for_function("document.querySelector('[role=status]').textContent.includes('Reopen and inspect')")
    assert writes(page) == ['routineSaveScene']


def stale(page):
    choose(page)
    review(page)
    page.evaluate("fixture.groups[0].members=['Kitchen']")
    page.get_by_role("button", name="Save scene and create routine", exact=True).click()
    expect(page.get_by_role("status")).to_contain_text("group changed")
    assert writes(page) == []


def safe_text(page):
    page.evaluate("fixture.favorites[0].title='<img src=x onerror=alert(1)>'+ 'W'.repeat(500)")
    choose(page)
    review(page)
    assert page.locator('#favorite-routines-section img').count() == 0
    for width in [1440, 768, 390, 320]:
        page.set_viewport_size({'width':width, 'height':900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), width
    assert writes(page) == []


def storage_block(page):
    page.evaluate("fixtureStorage.setItem('fsonos.favorite-routine.v1', '{broken'); mountFixture()")
    page.locator("#favorite-routines-section > summary").click()
    expect(page.get_by_role('status')).to_contain_text('Setup blocked')
    expect(page.get_by_role('button', name='Review music routine', exact=True)).to_have_count(0)
    assert writes(page) == []


def visibility(page):
    choose(page)
    review(page)
    page.evaluate("Object.defineProperty(document,'hidden',{configurable:true,value:true}); document.dispatchEvent(new Event('visibilitychange'))")
    expect(page.locator('#favorite-routines-section')).not_to_have_attribute('open', '')
    page.evaluate("Object.defineProperty(document,'hidden',{configurable:true,value:false}); document.dispatchEvent(new Event('visibilitychange'))")
    page.locator("#favorite-routines-section > summary").click()
    expect(page.get_by_role('button', name='Save scene and create routine', exact=True)).to_have_count(0)
    assert writes(page) == []



def workspace_hide(page):
    page.evaluate("fixture.mode='hold_put'")
    choose(page)
    review(page)
    page.get_by_role("button", name="Save scene and create routine", exact=True).click()
    page.wait_for_function("fixture.accepted")
    # Exercise both the synchronous hidden-ancestor dispatch fence and the
    # Studio event that discards dormant reviews during workspace navigation.
    page.evaluate("mounted.node.hidden=true; fixture.release()")
    page.wait_for_function("document.querySelector('[role=status]').textContent.includes('Reopen and inspect')")
    assert writes(page) == ['routineSaveScene']
    page.evaluate("document.dispatchEvent(new CustomEvent('fsonos:studio-selection')); mounted.node.hidden=false")
    expect(page.locator('#favorite-routines-section')).not_to_have_attribute('open', '')
    page.locator("#favorite-routines-section > summary").click()
    expect(page.get_by_role('button', name='Save scene and create routine', exact=True)).to_have_count(0)
    assert writes(page) == ['routineSaveScene']


SPOTIFY_ID = '0123456789ABCDEFGHIJKL'


def choose_uri(page, kind='spotify', uri=None):
    page.evaluate("fixture.noFavorites=true")
    page.locator("#favorite-routines-section > summary").click()
    page.get_by_label("Routine name", exact=True).fill("Morning music")
    page.get_by_role("combobox", name="Music source", exact=True).select_option(kind)
    page.get_by_role("combobox", name="Group to save", exact=True).select_option("Kitchen@S2")
    page.get_by_label('Spotify URI or full share link' if kind == 'spotify' else 'Radio stream URL', exact=True).fill(
        uri or f'https://open.spotify.com/album/{SPOTIFY_ID}?si=discarded')
    expect(page.get_by_role('combobox', name='Playable favorite', exact=True)).to_have_count(0)
    assert page.evaluate('fixture.favoriteReads') == 0


def spotify_levels(page, kind='album'):
    choose_uri(page, uri=f'https://open.spotify.com/{kind}/{SPOTIFY_ID}?si=discarded')
    page.get_by_label('Volume for each saved room (optional)', exact=True).fill('23')
    page.get_by_role('checkbox', name='Unmute the saved rooms when this routine runs', exact=True).check()
    review(page)
    region = page.get_by_role('region', name='Review music routine')
    expect(region).to_contain_text(f'spotify:{kind}:{SPOTIFY_ID}')
    expect(region).not_to_contain_text('discarded')
    expect(region).to_contain_text('Volume: request 23 for each saved room')
    expect(region).to_contain_text('Mute: explicitly unmute every saved room')
    expect(region).to_contain_text('replaces the group’s queue' if kind != 'track' else 'switches the group to this track or stream')
    page.keyboard.press('Escape')
    expect(page.get_by_role('button', name='Review music routine', exact=True)).to_be_focused()
    expect(page.get_by_label('Volume for each saved room (optional)', exact=True)).to_have_value('23')
    expect(page.get_by_role('checkbox')).to_be_checked()
    review(page)
    page.get_by_role('button', name='Save scene and create routine', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('acknowledged')
    assert writes(page) == ['routineSaveScene', 'routineAdd']
    assert page.evaluate('fixture.scenes[0].groups[0].source') == {'kind': 'uri', 'uri': f'spotify:{kind}:{SPOTIFY_ID}'}
    assert page.evaluate('fixture.scenes[0].volumes') == {'Kitchen@S2': 23, 'Study@S2': 23}
    assert page.evaluate('fixture.scenes[0].mutes') == {'Kitchen@S2': False, 'Study@S2': False}
    assert page.evaluate('fixture.favoriteReads') == 0


def radio_defaults(page):
    uri = 'https://radio.example.invalid/live?channel=' + 'r' * 1200
    choose_uri(page, 'stream', uri)
    for width in [1440, 768, 390, 320]:
        page.set_viewport_size({'width': width, 'height': 900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), width
    review(page)
    for width in [1440, 768, 390, 320]:
        page.set_viewport_size({'width': width, 'height': 900})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), width
    expect(page.get_by_role('region', name='Review music routine')).to_contain_text('Volume: keep current levels')
    expect(page.get_by_role('region', name='Review music routine')).to_contain_text('Mute: keep current mute settings')
    assert page.locator('#favorite-routines-section a').count() == 0
    page.get_by_role('button', name='Save scene and create routine', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('acknowledged')
    assert page.evaluate('fixture.scenes[0].groups[0].source.uri') == uri
    assert page.evaluate('fixture.scenes[0].volumes') == {}
    assert page.evaluate('fixture.scenes[0].mutes') == {}


def mode_switch(page):
    choose_uri(page)
    page.get_by_label('Volume for each saved room (optional)', exact=True).fill('17')
    page.evaluate('fixture.noFavorites=false')
    page.get_by_role('combobox', name='Music source', exact=True).select_option('favorite')
    page.get_by_role('combobox', name='Playable favorite', exact=True).select_option('FV:2/1')
    expect(page.get_by_label('Volume for each saved room (optional)', exact=True)).to_have_value('17')
    review(page)
    expect(page.get_by_role('region', name='Review music routine')).to_contain_text('Morning Music · container')
    page.get_by_role('button', name='Save scene and create routine', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('acknowledged')
    assert page.evaluate('fixture.scenes[0].groups[0].source.kind') == 'favorite'
    assert writes(page) == ['routineSaveScene', 'routineAdd']
    page.get_by_role('button', name='New music routine', exact=True).click()
    expect(page.get_by_role('combobox', name='Music source', exact=True)).to_have_value('favorite')
    expect(page.get_by_label('Volume for each saved room (optional)', exact=True)).to_have_value('')
    expect(page.get_by_role('checkbox')).not_to_be_checked()


def invalid_uri(page):
    choose_uri(page, uri='spotify:album:short')
    page.get_by_role('button', name='Review music routine', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('22-character ID')
    expect(page.get_by_role('region', name='Review music routine')).to_have_count(0)
    assert writes(page) == []
    page.get_by_label('Spotify URI or full share link', exact=True).fill(f'spotify:track:{SPOTIFY_ID}')
    page.get_by_label('Volume for each saved room (optional)', exact=True).fill('0')
    review(page)
    expect(page.get_by_role('region', name='Review music routine')).to_contain_text('Zero is silent')
    page.get_by_role('button', name='Save scene and create routine', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('acknowledged')
    assert page.evaluate('fixture.scenes[0].volumes') == {'Kitchen@S2': 0, 'Study@S2': 0}
    assert page.evaluate('fixture.scenes[0].mutes') == {}


def uri_recovery(page, mode='lost_put'):
    page.evaluate('(mode)=>{fixture.mode=mode}', mode)
    choose_uri(page)
    page.get_by_label('Volume for each saved room (optional)', exact=True).fill('18')
    page.get_by_role('checkbox').check()
    review(page)
    page.get_by_role('button', name='Save scene and create routine', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('needs inspection')
    sent = writes(page)
    page.evaluate('mountFixture()')
    page.locator('#favorite-routines-section > summary').click()
    page.get_by_role('button', name='Inspect saved state', exact=True).click()
    assert writes(page) == sent
    if mode == 'lost_put':
        expect(page.get_by_role('status')).to_contain_text('routine not submitted')
        page.get_by_role('button', name='Review scheduling saved scene', exact=True).click()
        expect(page.get_by_role('region', name='Review music routine')).to_contain_text('Volume: request 18')
        expect(page.get_by_role('region', name='Review music routine')).to_contain_text('explicitly unmute')
        page.get_by_role('button', name='Create routine for saved scene', exact=True).click()
        expect(page.get_by_role('status')).to_contain_text('acknowledged')
    else:
        expect(page.get_by_role('status')).to_contain_text('observation')
    assert writes(page) == ['routineSaveScene', 'routineAdd']
    assert page.evaluate('fixture.favoriteReads') == 0


def legacy_journal(page):
    # Build a genuine v1-shaped review and evidence, then inspect it with v2 UI.
    page.evaluate("""() => {
      const group = FavoriteRoutines.groupsOf(fixture.groups)[0];
      const favorite = FavoriteRoutines.favoritesOf(fixture.favorites)[0];
      const plan = copy(FavoriteRoutines.planOf(group, favorite,
        {label:'Legacy morning',cadence:'daily',time:'07:30'},'a'.repeat(32)));
      delete plan.source; delete plan.settings;
      const scene = {name:plan.name,saved:100,...copy(plan.definition)};
      scene.groups[0].source={kind:'favorite',name:favorite.title,uri:'x-rincon-cpcontainer:fixture'};
      fixture.scenes.push(scene);
      const attempt = {version:1,plan,sceneSent:true,scheduleSent:false,scene:FavoriteRoutines.sceneOf(scene,plan)};
      fixtureStorage.setItem(FavoriteRoutinePanel.JOURNAL_KEY,JSON.stringify(attempt));
      mountFixture();
    }""")
    page.locator('#favorite-routines-section > summary').click()
    page.get_by_role('button', name='Inspect saved state', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('routine not submitted')
    page.get_by_role('button', name='Review scheduling saved scene', exact=True).click()
    expect(page.get_by_role('region', name='Review music routine')).to_contain_text('Morning Music')
    expect(page.get_by_role('region', name='Review music routine')).to_contain_text('keep current levels')
    page.get_by_role('button', name='Create routine for saved scene', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('acknowledged')
    assert writes(page) == ['routineAdd']


def edited_levels(page):
    page.evaluate("fixture.mode='lost_put'")
    choose_uri(page)
    page.get_by_label('Volume for each saved room (optional)', exact=True).fill('18')
    review(page)
    page.get_by_role('button', name='Save scene and create routine', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('needs inspection')
    page.evaluate("fixture.scenes[0].volumes['Kitchen@S2']=90; mountFixture()")
    page.locator('#favorite-routines-section > summary').click()
    page.get_by_role('button', name='Inspect saved state', exact=True).click()
    expect(page.get_by_role('status')).to_contain_text('does not match')
    expect(page.get_by_role('button', name='Review scheduling saved scene', exact=True)).to_have_count(0)
    assert writes(page) == ['routineSaveScene']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--chromium', default=shutil.which('chromium') or shutil.which('chromium-browser'))
    args=parser.parse_args()
    cases=[('creation and journal remount',normal),('Escape and relative timing',cancel_delay),
        ('lost PUT and continuation',lost_put),('lost POST',lost_post),
        ('malformed POST',lambda page:lost_post(page,'malformed_post')),
        ('uncertain absent POST',lambda page:lost_post(page,'refuse_post')),
        ('close during PUT',close_put),('stale membership',stale),('safe text and four widths',safe_text),
        ('corrupt journal',storage_block),('visibility invalidation',visibility),('hidden Studio workspace',workspace_hide),
        ('Spotify album and exact levels',spotify_levels),
        ('Spotify track and exact levels',lambda page:spotify_levels(page,'track')),
        ('Spotify playlist and exact levels',lambda page:spotify_levels(page,'playlist')),
        ('radio defaults and four widths',radio_defaults),('source mode switch and new routine reset',mode_switch),
        ('invalid source and deliberate zero',invalid_uri),('URI lost PUT with settings',uri_recovery),
        ('URI lost POST with settings',lambda page:uri_recovery(page,'lost_post')),
        ('legacy journal in expanded panel',legacy_journal),('edited levels block scheduling',edited_levels)]
    with sync_playwright() as p:
        browser=p.chromium.launch(executable_path=args.chromium,headless=True,args=['--no-sandbox'])
        try:
            for name,fn in cases:
                page,errors=setup(browser)
                try:
                    fn(page)
                    assert not errors,errors
                    print(f'PASS {name}',flush=True)
                finally:
                    page.close()
        finally:
            browser.close()
    print(f'{len(cases)} Chromium component cases passed; transport and storage are fixtures.')


if __name__=='__main__':
    main()
