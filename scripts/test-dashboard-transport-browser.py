#!/usr/bin/env python3
"""Playback controls with real dashboard assets and synthetic missing-track state."""
import argparse
import importlib.util
from pathlib import Path

from playwright.sync_api import sync_playwright

spec = importlib.util.spec_from_file_location(
    "dashboard_browser", Path(__file__).with_name("test-dashboard-browser.py")
)
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)
base.FIXTURE = base.FIXTURE.replace("window.fixture = {", "window.fixture = {noTrack:true,").replace(
    "track:{uri:f.track", "track:f.noTrack?null:{uri:f.track"
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--browser-executable", default="/usr/bin/chromium")
    args = parser.parse_args()
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True, executable_path=args.browser_executable)
        for width, height in [(1440, 1000), (390, 844)]:
            page = browser.new_page(viewport={"width": width, "height": height}, reduced_motion="reduce")
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            base.load(page)
            page.wait_for_function("document.getElementById('track-title').textContent==='Music is playing.'")
            assert page.locator("#play").is_enabled()
            assert page.locator("#next").is_disabled()
            assert page.locator("#previous").is_disabled()
            assert page.locator("#dock-title").inner_text() == "Track details unavailable"
            assert "Nothing playing" not in page.locator(".room-card").first.inner_text()
            assert page.evaluate("fixture.writes.length") == 0
            controls = "#play" if width > 600 else '.hero-play[data-command="toggle"]'
            page.locator(controls).click()
            page.wait_for_function("document.getElementById('track-title').textContent==='Playback is paused.'")
            assert page.evaluate("fixture.writes") == [{"url": "/pause", "body": {"zone": "Studio@S2"}}]
            assert page.locator(controls).is_enabled()
            page.locator(controls).click()
            page.wait_for_function("document.getElementById('track-title').textContent==='Music is playing.'")
            assert page.evaluate("fixture.writes[1]") == {"url": "/resume", "body": {"zone": "Studio@S2"}}
            for state in ["stopped", "unknown", "transitioning"]:
                page.evaluate("state => {fixture.states[0]=state;fixture.feeds.at(-1).emit('zone.state');}", state)
                page.wait_for_function("state => document.getElementById('stage-status').textContent===state.toUpperCase()", arg=state)
                assert page.locator(controls).is_disabled()
            assert page.evaluate("fixture.writes.length") == 2
            page.evaluate("fixture.states[0]='playing';fixture.noTrack=false;fixture.feeds.at(-1).emit('zone.state')")
            page.wait_for_function("!document.getElementById('next').disabled")
            assert page.locator("#track-title").inner_text() == "Three rooms"
            assert not errors, errors
            page.close()
            print(f"PASS {width}px: missing track details preserve play/pause and truthful state; unavailable navigation stays disabled")
        browser.close()


if __name__ == "__main__":
    main()
