// Local-only headless playback checks. No upstream APIs, credentials or Tauri IPC are used.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createServer as createHttpServer } from "node:http";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { appendFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { chromium } from "playwright-core";
import { createServer as createViteServer } from "vite";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const { values: args } = parseArgs({ options: {
  "media-dir": { type: "string" }, output: { type: "string" }, "browser-executable": { type: "string" },
  scenarios: { type: "string", default: "seek,legacy,stall,lifecycle,mms,retry" }, help: { type: "boolean" },
} });
if (args.help) {
  console.log("pnpm test:playback [--media-dir DIR] [--output NEW_DIR] [--browser-executable PATH] [--scenarios seek,legacy,stall,lifecycle,mms,retry]");
  process.exit(0);
}
const scenarios = args.scenarios.split(",");
assert(scenarios.length && scenarios.every(s => ["seek", "legacy", "stall", "lifecycle", "mms", "retry"].includes(s)), "Unknown scenario");
const output = resolve(args.output || join(root, "test-results", "playback", new Date().toISOString().replace(/[:.]/g, "-")));
await mkdir(dirname(output), { recursive: true });
await mkdir(output); // Refuse to overwrite artifacts from an earlier run.
const jsonl = (file, entry) => appendFileSync(file, JSON.stringify({ ts: new Date().toISOString(), ...entry }) + "\n");
const report = { started: new Date().toISOString(), platform: process.platform, browser: "chromium", transport: "browser fixture; not Tauri/WKWebView", output, cases: [] };
let temporaryMedia, vite, media, browser;
let nextRequest = 0;
const requests = new Set();

async function serveMedia(dir) {
  const manifest = await readFile(join(dir, "list.m3u8"), "utf8");
  assert(manifest.startsWith("#EXTM3U") && manifest.includes("#EXT-X-ENDLIST"), "Expected a local VOD HLS fixture");
  const server = createHttpServer(async (req, res) => {
    const started = performance.now(), request = ++nextRequest;
    const url = new URL(req.url, "http://localhost");
    let status = 200, bytes = 0, fragment;
    res.on("close", () => jsonl(join(output, "media-server.jsonl"), { request, route: url.pathname, fragment, status, bytes, elapsed_ms: Math.round(performance.now() - started), aborted: !res.writableFinished }));
    try {
      let body, kind;
      if (url.pathname === "/hls") {
        body = Buffer.from(manifest.split("\n").map(line => !line || line.startsWith("#") ? line :
          `http://hg.localhost/seg?u=${encodeURIComponent(`https://media.fixture.invalid/${line.trim()}`)}`).join("\n"));
        kind = "application/vnd.apple.mpegurl";
      } else if (url.pathname === "/seg") {
        fragment = basename(new URL(url.searchParams.get("u")).pathname);
        assert(/^seg\d+\.ts$/.test(fragment), "Unexpected fixture segment");
        await new Promise(done => setTimeout(done, 120));
        body = await readFile(join(dir, fragment)); kind = "video/mp2t";
      } else { status = 404; body = Buffer.from("not found"); kind = "text/plain"; }
      bytes = body.length;
      res.writeHead(status, { "Content-Type": kind, "Content-Length": bytes, "Access-Control-Allow-Origin": "*" });
      res.end(body);
    } catch {
      status = 404; res.writeHead(status); res.end("fixture error");
    }
  });
  server.on("connection", socket => { requests.add(socket); socket.on("close", () => requests.delete(socket)); });
  await new Promise((done, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", done); });
  return server;
}

const getLogs = page => page.evaluate(async () => { await window.playerSeekFixture.flushLogs(); return window.playerSeekFixture.logs; });
async function waitLog(page, event, source) {
  await page.waitForFunction(({ event, source }) => window.playerSeekFixture.logs.some(e => e.event === event && (!source || e.data.source === source)), { event, source });
  return getLogs(page);
}
async function clickProgress(page, fraction) {
  const input = page.getByRole("slider", { name: "播放进度" });
  await input.scrollIntoViewIfNeeded();
  const r = await input.boundingBox(); assert(r, "Seekbar missing");
  await page.mouse.click(r.x + r.width * fraction, r.y + r.height / 2);
}
async function loadPlayer(page, legacy = false, extra = "") {
  await page.goto(`${report.ui}/tests/player-seek.html?diagnostics=1&proxy=${encodeURIComponent(report.media)}${legacy ? "&legacy=1" : ""}${extra}`);
  await page.waitForFunction(() => document.querySelector("video")?.currentTime > 1);
}
async function seekChecks(page, legacy) {
  await loadPlayer(page, legacy);
  await clickProgress(page, .1);
  assert.equal((await getLogs(page)).length, 0, "Default-off must not record seeks");
  await page.getByRole("button", { name: "诊断设置", exact: true }).click();
  await page.getByRole("button", { name: "播放诊断日志", exact: true }).click();
  await page.waitForFunction(() => document.querySelector('[aria-label="播放诊断日志"]').getAttribute("aria-pressed") === "true");
  await page.getByRole("button", { name: "诊断设置", exact: true }).click();
  await clickProgress(page, .8);
  let logs = await waitLog(page, "seek.resumed");
  const first = logs.find(e => e.event === "seek.request" && e.data.source === `${legacy ? "legacy" : "desktop"}_click`);
  assert(first, "Actual click must request a seek");
  for (const event of ["seek.input.down", "seek.input.change", "seek.input.up", "seek.assigned", "media.seeking", "media.seeked"]) assert(logs.some(e => e.event === event), event);
  assert(logs.some(e => e.event === "seek.resumed" && e.data.seek === first.data.seek), "Click must recover a frame with matching seek ID");

  const before = logs.length;
  const r = await page.getByRole("slider", { name: "播放进度" }).boundingBox();
  const current = await page.locator("video").evaluate(v => v.currentTime / v.duration);
  await page.mouse.move(r.x + r.width * current, r.y + r.height / 2);
  await page.mouse.down();
  for (let i = 1; i <= 12; i++) {
    await page.mouse.move(r.x + r.width * (current + (.45 - current) * i / 12), r.y + r.height / 2);
    await page.waitForTimeout(30);
  }
  const duringDrag = (await getLogs(page)).slice(before);
  assert.equal(duringDrag.filter(e => e.event === "seek.request").length, 0, "Pointer movement must only preview, without seeking");
  assert(duringDrag.filter(e => e.event === "seek.input.change" && e.data.input === "drag").length > 1, "Drag must keep logging preview positions");
  const preview = await page.getByRole("slider", { name: "播放进度" }).inputValue();
  assert(Math.abs(Number(preview) - 40.5) < .5, "Seekbar must preview the target before release");
  await page.mouse.up();
  await page.waitForFunction(n => window.playerSeekFixture.logs.slice(n).some(e => e.event === "seek.resumed"), before);
  logs = (await getLogs(page)).slice(before);
  const drags = logs.filter(e => e.event === "seek.request" && e.data.source === `${legacy ? "legacy" : "desktop"}_drag`);
  assert.equal(drags.length, 1, "A whole drag must commit exactly one seek on release");
  const last = drags.at(-1);
  assert(logs.some(e => e.event === "seek.input.up" && e.data.input === "drag"));
  assert(logs.some(e => e.event === "seek.resumed" && e.data.seek === last.data.seek), "Final drag target must recover a frame");
  const fragments = (await getLogs(page)).filter(e => e.event === "hlsFragLoading" || e.event === "hlsFragLoaded");
  assert(fragments.some(e => e.event === "hlsFragLoading" && e.data.load_ms === null), "Incomplete downloads must have unknown duration");
  for (const { data } of fragments) {
    for (const key of ["load_ms", "first_byte_ms"]) {
      assert(data[key] === null || data[key] >= 0, "Fragment timings must never be negative");
    }
  }

  await page.getByRole("button", { name: "暂停", exact: true }).click();
  await clickProgress(page, .6);
  logs = await waitLog(page, "seek.ready");
  assert(await page.locator("video").evaluate(v => v.paused), "Paused seek must stay paused");
  const keyboardBefore = logs.length;
  await page.getByRole("slider", { name: "播放进度" }).focus();
  await page.keyboard.press("ArrowRight");
  await page.waitForFunction(n => window.playerSeekFixture.logs.slice(n).some(e => e.event === "seek.ready"), keyboardBefore);
  assert((await getLogs(page)).slice(keyboardBefore).some(e => e.event === "seek.request" && e.data.source.endsWith("_keyboard")), "Native keyboard seek must still work");
  assert(await page.locator("video").evaluate(v => v.paused), "Keyboard seek must preserve pause");

  // Page changes during capture must cancel the preview rather than seek the hidden player.
  const cancelledBefore = (await getLogs(page)).length;
  const cancelBox = await page.getByRole("slider", { name: "播放进度" }).boundingBox();
  await page.mouse.move(cancelBox.x + cancelBox.width * .3, cancelBox.y + cancelBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(cancelBox.x + cancelBox.width * .7, cancelBox.y + cancelBox.height / 2);
  await page.evaluate(() => window.playerSeekFixture.setActive(false));
  await page.waitForFunction(() => document.querySelector('input[aria-label="播放进度"]').disabled);
  await page.mouse.up();
  const cancelled = (await getLogs(page)).slice(cancelledBefore);
  assert(cancelled.some(e => e.event === "seek.input.cancel"), "Deactivation must cancel the drag");
  assert(!cancelled.some(e => e.event === "seek.request"), "Cancelled drag must not commit a seek");
  await page.evaluate(() => window.playerSeekFixture.setActive(true));
  await page.waitForFunction(() => !document.querySelector('input[aria-label="播放进度"]').disabled);
  assert(await page.locator("video").evaluate(v => v.paused), "Returning must preserve manual pause");
  await page.evaluate(() => window.playerSeekFixture.setLogging(false));
  const count = (await getLogs(page)).length;
  await clickProgress(page, .2); await page.waitForTimeout(350);
  assert.equal((await getLogs(page)).length, count, "Disabled logging must stop recording");
  await page.evaluate(() => { window.playerSeekFixture.rejectLoggingSave = true; });
  const rejected = await page.evaluate(() => window.playerSeekFixture.setLogging(true).then(() => false, () => true));
  assert(rejected, "Failed persistence must reject");
  await clickProgress(page, .3);
  assert.equal((await getLogs(page)).length, count, "Failed persistence must keep logging disabled");
  return { click_ms: logs.find(e => e.event === "seek.resumed" && e.data.seek === first.data.seek)?.data.elapsed_ms, drag_requests: drags.length };
}
async function stallCheck(page) {
  await loadPlayer(page);
  await page.waitForFunction(() => { const v = document.querySelector("video"); return v.buffered.length && v.buffered.end(0) >= 30; });
  await page.evaluate(async () => { await window.playerSeekFixture.setLogging(true); window.playerSeekFixture.pauseBuffering(); });
  await clickProgress(page, .8);
  const logs = await waitLog(page, "seek.timeout");
  const timeout = logs.find(e => e.event === "seek.timeout");
  assert.equal(timeout.data.buffering_enabled, false);
  assert(timeout.data.ready < 3 && timeout.data.seeking, "Timeout must capture the stalled media state");
  await page.evaluate(() => window.playerSeekFixture.resumeBuffering());
  await waitLog(page, "seek.resumed");
  return { fault_injected: true, timeout_ms: timeout.data.elapsed_ms };
}
async function managedMediaCheck(page) {
  // Chromium supplies real MSE decoding/buffering; only the MMS constructor and streaming
  // events are simulated. This tests the hls.js workaround, not macOS WebKit itself.
  await page.addInitScript(() => {
    const NativeMediaSource = window.MediaSource;
    window.__managedSources = [];
    class ManagedMediaSource extends NativeMediaSource {
      constructor() { super(); this.streaming = true; window.__managedSources.push(this); }
    }
    Object.defineProperty(window, "ManagedMediaSource", { configurable: true, value: ManagedMediaSource });
    if (new URL(location.href).searchParams.get("mms") === "only") {
      Object.defineProperty(window, "MediaSource", { configurable: true, value: undefined });
    }
  });
  const players = [];
  for (const legacy of [false, true]) {
    await loadPlayer(page, legacy, "&mms=both");
    await page.evaluate(() => window.playerSeekFixture.setLogging(true));
    let logs = await getLogs(page);
    assert(logs.some(e => e.event === "player.snapshot" && e.data.engine === "MediaSource" && e.data.prefer_managed === false), "Standard MediaSource must be preferred when both APIs exist");
    assert.equal(await page.evaluate(() => window.__managedSources.length), 0, "Avoid constructing MMS on standard MSE-capable browsers");
    await loadPlayer(page, legacy, "&mms=only");
    await page.waitForFunction(() => { const v = document.querySelector("video"); return v.buffered.length && v.buffered.end(0) >= 30; });
    await page.evaluate(() => window.playerSeekFixture.setLogging(true));
    logs = await getLogs(page);
    assert(logs.some(e => e.event === "player.snapshot" && e.data.engine === "ManagedMediaSource"), "MMS-only environments must still play");
    await page.evaluate(() => {
      const source = window.__managedSources.find(s => s.readyState === "open");
      source.streaming = false;
      source.dispatchEvent(new Event("endstreaming"));
      // A delayed endstreaming event can also arrive after seeking has resumed loading.
      document.querySelector("video").addEventListener("seeking", () => source.dispatchEvent(new Event("endstreaming")), { once: true });
    });
    await page.waitForFunction(() => !window.playerSeekFixture.engineState().buffering);
    await clickProgress(page, .8);
    logs = await waitLog(page, "seek.resumed");
    assert(logs.some(e => e.event === "mms.endstreaming" && e.data.buffering_enabled === false), "Test must first suspend MMS buffering");
    assert(logs.some(e => e.event === "mms.endstreaming" && e.data.seeking && e.data.buffering_enabled === true), "Late endstreaming during seeking must not suspend buffering again");
    assert(!logs.some(e => e.event === "mms.startstreaming"), "Recovery must not depend on a startstreaming event");
    assert.equal(await page.evaluate(() => window.playerSeekFixture.engineState().buffering), true, "Seeking must automatically resume MMS buffering");
    assert(!logs.some(e => e.event === "seek.timeout"));
    players.push({ player: legacy ? "legacy" : "feed", version: logs.find(e => e.event === "app.snapshot")?.data.hls_version,
      prefer_standard_mse: true, mms_only_recovered_without_startstreaming: true,
      seek_ms: logs.find(e => e.event === "seek.resumed")?.data.elapsed_ms });
  }
  return { simulated_mms: true, players };
}
async function retryCheck(page) {
  await page.goto(`${report.ui}/tests/player-seek.html?diagnostics=1&retry=1&proxy=${encodeURIComponent(report.media)}`);
  await page.getByText("视频暂时无法加载", { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.playerSeekFixture.playCalls), 1);
  await page.getByRole("button", { name: "重试", exact: true }).click();
  await page.waitForFunction(() => document.querySelector("video")?.currentTime > 1);
  assert.equal(await page.evaluate(() => window.playerSeekFixture.playCalls), 2, "Retry must obtain fresh playback information");
  assert.equal(await page.getByText("视频暂时无法加载", { exact: true }).count(), 0);
  await page.waitForFunction(() => {
    const video = document.querySelector("video");
    return video.currentTime > 2 && video.buffered.length && video.buffered.end(0) > 8;
  });
  await page.evaluate(() => { window.keptVideo = document.querySelector("video"); window.keptVideo.pause(); });
  await page.route("**/seg?*", route => route.abort());
  await page.evaluate(() => window.playerSeekFixture.failPlayback());
  await page.getByText("视频暂时无法加载", { exact: true }).waitFor();
  await page.getByRole("button", { name: "关闭", exact: true }).click();
  await page.getByText("视频暂时无法加载", { exact: true }).waitFor({ state: "hidden" });
  await page.evaluate(() => window.playerSeekFixture.failPlayback());
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
  assert.equal(await page.getByText("视频暂时无法加载", { exact: true }).count(), 0, "Dismissed errors must not immediately reopen");
  assert(await page.evaluate(() => document.querySelector("video") === window.keptVideo), "Close must preserve the player");
  assert(await page.locator("video").evaluate(video => video.buffered.length > 0 && video.buffered.start(0) <= 1 && video.buffered.end(0) > 3), "Close must retain buffered content");
  await page.locator("video").evaluate(video => { video.currentTime = 1; return video.play(); });
  await page.waitForFunction(() => document.querySelector("video").currentTime > 2);
  assert.equal(await page.evaluate(() => window.playerSeekFixture.playCalls), 2, "Close must not obtain a new playback source");
  return { playback_requests: 2, recovered_from_api_failure: true, close_preserves_buffer: true, buffered_replay_without_network: true };
}

async function lifecyclePlayerCheck(page, legacy) {
  let release, held, holdDeadline;
  const gate = new Promise(done => { release = done; });
  const manifestHeld = new Promise(done => { held = done; });
  await page.route("**/hls?*", async route => {
    const response = await route.fetch(); held(); await gate;
    await route.fulfill({ response });
  });
  try {
    await page.goto(`${report.ui}/tests/player-seek.html?diagnostics=1&proxy=${encodeURIComponent(report.media)}${legacy ? "&legacy=1" : ""}`);
    await page.waitForFunction(() => window.playerSeekFixture && document.querySelector("video"));
    await Promise.race([manifestHeld, new Promise((_, reject) => { holdDeadline = setTimeout(() => reject(new Error("Manifest interception timed out")), 15000); })]);
    clearTimeout(holdDeadline);
    await page.evaluate(async () => { await window.playerSeekFixture.setLogging(true); window.playerSeekFixture.setActive(false); });
    await page.waitForFunction(() => document.querySelector('input[aria-label="播放进度"]').disabled);
    release();
    await page.waitForFunction(() => window.playerSeekFixture.engineState().levels > 0);
    await page.waitForTimeout(350);
    const state = await page.locator("video").evaluate(v => ({ time: v.currentTime, paused: v.paused, ready: v.readyState }));
    const logs = await getLogs(page);
    assert(state.paused, `BACKGROUND_AUTOPLAY: inactive page resumed after manifest returned (${JSON.stringify(state)})`);
    assert.equal(await page.evaluate(() => window.playerSeekFixture.engineState().loading), false, "Inactive player must not load fragments");
    assert(!logs.some(e => e.event === "play.request" && e.data.active === false), "Autoplay must be gated before calling play()");
    assert(!logs.some(e => e.event === "hlsFragLoading"), "Hidden manifest must not start fragment loading");
    await page.evaluate(() => window.playerSeekFixture.setActive(true));
    await page.waitForFunction(() => document.querySelector("video").currentTime > 1 && !document.querySelector("video").paused);
    await page.getByRole("button", { name: "暂停", exact: true }).click();
    await page.evaluate(() => window.playerSeekFixture.setActive(false));
    await page.waitForFunction(() => !window.playerSeekFixture.engineState().loading);
    await page.evaluate(() => window.playerSeekFixture.setActive(true));
    await page.waitForFunction(() => !document.querySelector('input[aria-label="播放进度"]').disabled);
    assert(await page.locator("video").evaluate(v => v.paused), "Manual pause must survive page reactivation");
    await page.getByRole("button", { name: "播放", exact: true }).first().click();
    await page.waitForFunction(() => !document.querySelector("video").paused);
    await page.locator("video").evaluate(v => v.pause());
    await page.evaluate(() => window.playerSeekFixture.setActive(false));
    await page.waitForFunction(() => !window.playerSeekFixture.engineState().loading);
    await page.evaluate(() => window.playerSeekFixture.setActive(true));
    await page.waitForFunction(() => !document.querySelector('input[aria-label="播放进度"]').disabled);
    assert(await page.locator("video").evaluate(v => v.paused), "Native/PiP pause must survive page reactivation");
    return { inactive_paused: state.paused, inactive_fragment_loading: false, manual_pause_preserved: true, native_pause_preserved: true };
  } finally { clearTimeout(holdDeadline); release(); await page.unroute("**/hls?*"); }
}
async function lifecycleCheck(page) {
  const feed = await lifecyclePlayerCheck(page, false);
  const legacy = await lifecyclePlayerCheck(page, true);
  // The video mounts after the parent's inactive effect, just like a late API response.
  await page.goto(`${report.ui}/tests/player-seek.html?diagnostics=1&inactive=1&proxy=${encodeURIComponent(report.media)}`);
  await page.waitForFunction(() => document.querySelector("video") && window.playerSeekFixture.engineState().levels > 0);
  await page.waitForTimeout(350);
  assert(await page.locator("video").evaluate(v => v.paused), "Late-mounted inactive player must not autoplay");
  assert.equal(await page.evaluate(() => window.playerSeekFixture.engineState().loading), false);
  return { feed, legacy, late_mount_paused: true };
}

try {
  let mediaDir = args["media-dir"] && resolve(args["media-dir"]);
  if (!mediaDir) {
    temporaryMedia = await mkdtemp(join(tmpdir(), "hghub-playback-")); mediaDir = temporaryMedia;
    try {
      execFileSync(process.env.FFMPEG_PATH || "ffmpeg", ["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=15", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100", "-t", "90", "-c:v", "libx264", "-preset", "ultrafast", "-g", "30", "-sc_threshold", "0", "-pix_fmt", "yuv420p", "-c:a", "aac", "-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod", "-hls_segment_filename", join(mediaDir, "seg%03d.ts"), join(mediaDir, "list.m3u8")], { timeout: 60000, stdio: "pipe" });
    } catch { throw new Error("Cannot generate HLS fixture: install FFmpeg (libx264/AAC), set FFMPEG_PATH, or pass --media-dir DIR"); }
  }
  vite = await createViteServer({ root, server: { host: "127.0.0.1", port: 0, strictPort: false, hmr: false, open: false } });
  await vite.listen(); report.ui = `http://127.0.0.1:${vite.httpServer.address().port}`;
  media = await serveMedia(mediaDir); report.media = `http://127.0.0.1:${media.address().port}`;
  browser = await chromium.launch({ headless: true, executablePath: args["browser-executable"], args: ["--autoplay-policy=no-user-gesture-required"] });
  report.browser_version = browser.version();
  for (const name of scenarios) {
    const dir = join(output, name); await mkdir(dir);
    const context = await browser.newContext({ viewport: { width: 1000, height: 900 } });
    await context.exposeBinding("collectPlaybackLogs", (_source, entries) => {
      for (const e of entries) jsonl(join(dir, "playback.jsonl"), { ts: e.data.at, event: `web.${e.event}`, data: e.data, transport: "browser-fixture" });
    });
    await context.tracing.start({ screenshots: true, snapshots: true });
    const page = await context.newPage(), pageErrors = [], externalRequests = [];
    page.setDefaultTimeout(15000);
    // Nothing in these tests should contact an upstream service.
    await context.route("**/*", route => {
      const url = new URL(route.request().url());
      if ([report.ui, report.media].includes(url.origin)) return route.continue();
      externalRequests.push({ protocol: url.protocol, host: url.hostname }); return route.abort("blockedbyclient");
    });
    const ids = new WeakMap(); let requestId = 0;
    page.on("request", req => { ids.set(req, ++requestId); jsonl(join(dir, "network.jsonl"), { request: requestId, event: "request", method: req.method(), path: new URL(req.url()).pathname }); });
    page.on("response", response => jsonl(join(dir, "network.jsonl"), { request: ids.get(response.request()), event: "response", status: response.status() }));
    page.on("requestfailed", req => jsonl(join(dir, "network.jsonl"), { request: ids.get(req), event: "failed", error: req.failure()?.errorText }));
    page.on("console", msg => jsonl(join(dir, "console.jsonl"), { type: msg.type(), text: msg.text() }));
    page.on("pageerror", error => { pageErrors.push(error.message); jsonl(join(dir, "console.jsonl"), { type: "pageerror", text: error.message }); });
    const result = { name, status: "running", started: new Date().toISOString() }; report.cases.push(result);
    let deadline;
    try {
      const action = name === "seek" || name === "legacy" ? seekChecks(page, name === "legacy") : name === "stall" ? stallCheck(page) : name === "mms" ? managedMediaCheck(page) : name === "retry" ? retryCheck(page) : lifecycleCheck(page);
      result.metrics = await Promise.race([action, new Promise((_, reject) => { deadline = setTimeout(() => reject(new Error("Scenario exceeded 45 seconds")), 45000); })]);
      assert.deepEqual(pageErrors, [], "Browser script errors");
      assert.deepEqual(externalRequests, [], "Unexpected external network requests");
      result.status = "passed"; console.log(`PASS ${name}: ${JSON.stringify(result.metrics)}`);
    } catch (error) {
      result.status = "failed"; result.error = error.message;
      console.error(`FAIL ${name}: ${error.message}`);
    } finally {
      clearTimeout(deadline);
      await getLogs(page).catch(() => {});
      result.page_errors = pageErrors; result.external_requests = externalRequests;
      await page.screenshot({ path: join(dir, "final.png") }).catch(() => {});
      await context.tracing.stop({ path: join(dir, "trace.zip") }).catch(() => {});
      await context.close();
      result.finished = new Date().toISOString();
      await writeFile(join(output, "report.json"), JSON.stringify(report, null, 2) + "\n");
    }
  }
} catch (error) {
  report.setup_error = error.message; console.error(error.message);
} finally {
  await browser?.close();
  for (const socket of requests) socket.destroy();
  if (media) await new Promise(done => media.close(done));
  await vite?.close();
  if (temporaryMedia) await rm(temporaryMedia, { recursive: true, force: true });
  report.finished = new Date().toISOString();
  report.passed = report.cases.filter(c => c.status === "passed").length;
  report.failed = report.cases.filter(c => c.status !== "passed").length + (report.setup_error ? 1 : 0);
  await writeFile(join(output, "report.json"), JSON.stringify(report, null, 2) + "\n");
  console.log(`Playback checks: ${report.passed} passed, ${report.failed} failed. Artifacts: ${output}`);
  process.exitCode = report.failed ? 1 : 0;
}
