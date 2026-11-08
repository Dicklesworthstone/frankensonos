#!/usr/bin/env python3
"""Exercise the real rehearsal component with explicit synthetic API observations."""
import argparse
from pathlib import Path

from playwright.sync_api import sync_playwright

ASSETS = Path(__file__).resolve().parents[1] / "crates/fsonos-api/src/remote"
FIXTURE = r"""
window.fixture = {calls:[], posts:[], gate:null, release:null,
  zones:[{household:'S2',coordinator_room:'Studio',members:['Studio','Kitchen'],transport_state:'playing'},
    {household:'S1',coordinator_room:'Studio',members:['Studio'],transport_state:'paused'}]};
window.previewReport = seed => ({zone:'Studio',from:'2026-11-08T23:40:00-04:00',seed,
  works:[{at:'2026-11-08T23:40:00-04:00',work:{title:'<Whole work & song>',composer:'Fixture artist',
    performers:['Fixture quartet'],album:'Fixture recording',movements:3,minutes:24,
    reason:{summary:'Variety and the owner’s preferences.',relaxed:['keywords'],factors:[{factor:'feedback',weight:1250}]}},
    energy:35,energy_target:40,genre:'jazz',mood:'focus',explicit:false},
    {at:'2026-11-09T00:04:00-04:00',work:{title:'After midnight',composer:'Another artist',movements:1,minutes:5,
    reason:{summary:'Artist spacing.'}},energy:20,explicit:true}],
  summary:{works:2,minutes:29,distinct_artists:2,explicit:1,hours:[
    {from:'2026-11-08T23:40:00-04:00',works:1,energy:35,energy_target:40,artist_entropy:0,genre_entropy:0},
    {from:'2026-11-09T00:04:00-04:00',works:1,energy:20,artist_entropy:0,genre_entropy:0}]}});
window.previewCall = async (url,body) => {
  fixture.calls.push({url,body});
  if(url==='/zones')return structuredClone(fixture.zones);
  if(url.endsWith('/state')){const zone=fixture.zones.find(z=>DashboardCore.key(z)===decodeURIComponent(url.split('/')[2]));return {zone,transport_state:zone.transport_state};}
  if(url==='/dj/moods')return {moods:[{name:'focus'},{name:'bright'}]};
  if(url==='/dj/rehearse'){
    fixture.posts.push({url,body});
    if(fixture.gate)await fixture.gate;
    return previewReport(body.seed);
  }
  throw new Error('Unexpected preview route '+url);
};
window.previewController=DashboardCore.model({call:previewCall,changed:()=>window.previewPanel?.observe()});
window.previewPanel=DashboardRehearsal.panel({call:previewCall,controller:previewController});
window.ready=previewController.refresh().then(()=>previewController.connection(true));
"""


def load(page, theme):
    page.set_content(
        '<!doctype html><html data-theme="' + theme + '"><head><meta charset="UTF-8">'
        '<meta name="viewport" content="width=device-width,initial-scale=1"></head><body>'
        '<main style="padding:30px"><h1>Synthetic rehearsal fixture</h1>'
        '<button id="open-rehearsal" class="primary-button" aria-controls="rehearsal-dialog">Rehearse a set</button></main>'
        '<dialog id="rehearsal-dialog" class="review-dialog rehearsal-dialog" aria-labelledby="rehearsal-title"></dialog></body></html>'
    )
    for name in ["dashboard.css", "dashboard-rehearsal.css"]:
        page.add_style_tag(content=(ASSETS / name).read_text())
    page.add_script_tag(content=(ASSETS / "dashboard-core.js").read_text())
    page.add_script_tag(content=(ASSETS / "dj-session.js").read_text())
    page.add_script_tag(content=(ASSETS / "dashboard-rehearsal.js").read_text())
    page.add_script_tag(content=FIXTURE)
    page.evaluate("ready")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--browser-executable", default="/usr/bin/chromium")
    parser.add_argument("--screenshots", type=Path)
    args = parser.parse_args()
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True, executable_path=args.browser_executable)
        for width, height, theme in [(1440, 1000, "dark"), (390, 844, "dark"), (390, 844, "light")]:
            page = browser.new_page(viewport={"width": width, "height": height}, reduced_motion="reduce")
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            load(page, theme)
            page.locator("#open-rehearsal").click()
            page.wait_for_function("!previewPanel.control.state.loadingMoods")
            assert page.locator("#rehearsal-dialog").is_visible()
            assert page.evaluate("fixture.posts.length") == 0
            assert page.locator("#rehearsal-target").inner_text() == "Studio + Kitchen (S2)"
            page.get_by_label("Plan by", exact=True).select_option("hours")
            assert page.get_by_label("Length", exact=True).input_value() == "2"
            page.get_by_label("Start in house time", exact=True).fill("23:40")
            page.get_by_label("Seed", exact=True).fill("0")
            page.get_by_label("Mood for this preview", exact=True).select_option("focus")
            if args.screenshots:
                page.screenshot(path=str(args.screenshots / f"rehearsal-form-{width}-{theme}.png"))
            page.locator("#rehearsal-generate").click()
            page.wait_for_function("previewPanel.control.state.report!==null")
            assert page.evaluate("fixture.posts") == [{"url": "/dj/rehearse", "body": {
                "zone": "Studio@S2", "hours": 2, "seed": 0, "at": "23:40", "mood": "focus"}}]
            output = page.locator("#rehearsal-output")
            assert "<Whole work & song>" in output.inner_text()
            assert "2026-11-09 00:04:00 -04:00" in output.inner_text()
            assert "3 movements, kept together" in output.inner_text()
            assert "Filters relaxed for this pick: keywords" in output.inner_text()
            assert output.locator("meter").count() == 2
            assert output.locator("meter").first.get_attribute("value") == "35"
            assert output.locator("script, img").count() == 0
            assert page.evaluate("document.querySelector('#rehearsal-dialog').scrollWidth<=document.querySelector('#rehearsal-dialog').clientWidth+1")
            output.scroll_into_view_if_needed()
            if args.screenshots:
                page.screenshot(path=str(args.screenshots / f"rehearsal-result-{width}-{theme}.png"))
            page.locator("#rehearsal-seed").fill("2")
            assert "Settings changed" in page.locator("#rehearsal-snapshot").inner_text()
            page.locator("#close-rehearsal").click()
            assert not page.locator("#rehearsal-dialog").is_visible()
            assert page.locator("#open-rehearsal").evaluate("node=>node===document.activeElement")
            # A pending plan cannot later appear under another household's
            # identically named room, and changing scope never replans it.
            page.locator("#open-rehearsal").click()
            page.evaluate("fixture.gate=new Promise(resolve=>fixture.release=resolve)")
            page.locator("#rehearsal-generate").click()
            page.wait_for_function("fixture.posts.length===2")
            page.evaluate("previewController.select('Studio@S1')")
            assert page.locator("#rehearsal-target").inner_text() == "Studio (S1)"
            page.evaluate("fixture.release(); fixture.gate=null")
            page.wait_for_function("!previewPanel.control.state.busy")
            assert not page.locator("#rehearsal-output").is_visible()
            assert page.evaluate("fixture.posts.length") == 2
            assert not errors, errors
            page.close()
            print(f"PASS {width}px {theme}: whole-work timeline, real values, midnight offsets and fenced read-only planning")
        browser.close()


if __name__ == "__main__":
    main()
