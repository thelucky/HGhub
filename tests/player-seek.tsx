import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import Feed from "../src/components/Feed";
import { AppProvider } from "../src/store";
import { api } from "../src/api";
import "../src/index.css";
import { flushPlaybackLog, initPlaybackLog, playbackLogApi, setPlaybackLog } from "../src/playbackDiagnostics";
import Settings from "../src/pages/Settings";
import Player from "../src/components/Player";
import Hls from "hls.js";

// 仅将自定义媒体协议映射到本地测试服务器；播放器、HLS 和鼠标事件均使用真实实现。
const proxy = new URLSearchParams(location.search).get("proxy") || "http://127.0.0.1:17779";
const open = XMLHttpRequest.prototype.open;
XMLHttpRequest.prototype.open = function (this: XMLHttpRequest, method: string, target: string | URL, async: boolean = true, user?: string | null, credential?: string | null) {
  const url = new URL(String(target), location.href);
  if (url.hostname === "hg.localhost" || (url.hostname === "localhost" && url.protocol === "hg:")) {
    target = `${proxy}${url.pathname}${url.search}`;
  }
  return open.call(this, method, target, async, user, credential);
};

const fixture = {
  seeks: [] as number[], errors: [] as string[],
  setActive: (_active: boolean) => {},
  logs: [] as { event: string; data: Record<string, unknown> }[],
  setLogging: setPlaybackLog,
  flushLogs: flushPlaybackLog,
  rejectLoggingSave: false,
  playCalls: 0,
  failPlayback: () => testHls?.trigger(Hls.Events.ERROR, {
    type: Hls.ErrorTypes.NETWORK_ERROR, details: Hls.ErrorDetails.FRAG_LOAD_ERROR,
    fatal: true, error: new Error("Media network unavailable"),
    frag: testHls.levels[0]?.details?.fragments[0],
  }),
  pauseBuffering: () => testHls?.pauseBuffering(),
  resumeBuffering: () => testHls?.resumeBuffering(),
  engineState: () => ({ loading: testHls?.loadingEnabled, buffering: testHls?.bufferingEnabled, levels: testHls?.levels.length }),
};
let testHls: Hls | undefined;
const diagnosticTest = new URLSearchParams(location.search).has("diagnostics");
const legacyTest = new URLSearchParams(location.search).has("legacy");
const initiallyInactive = new URLSearchParams(location.search).has("inactive");
if (diagnosticTest) {
  // Test-only handle: exercise the real engine's public API without rewriting production modules.
  const attach = Hls.prototype.attachMedia;
  Hls.prototype.attachMedia = function (...args) { testHls = this; return attach.apply(this, args); };
  let enabled = false;
  playbackLogApi.status = async () => ({ enabled, path: "playback.log", error: null });
  playbackLogApi.set = async (on) => {
    if (fixture.rejectLoggingSave) throw new Error("fixture: disk unavailable");
    return { enabled: enabled = on, path: "playback.log", error: null };
  };
  playbackLogApi.write = async entries => {
    if (!enabled) return;
    fixture.logs.push(...entries);
    const collect = (window as typeof window & { collectPlaybackLogs?: (entries: typeof fixture.logs) => Promise<void> }).collectPlaybackLogs;
    await collect?.(entries);
  };
  playbackLogApi.read = async () => fixture.logs.map(e => JSON.stringify(e)).join("\n");
  api.lineState = async () => ({ base: "https://fixture.invalid", default: "https://fixture.invalid", fixed: [] });
  void initPlaybackLog();
}
Object.assign(window, { playerSeekFixture: fixture });
document.addEventListener("seeking", (e) => {
  if (e.target instanceof HTMLVideoElement) fixture.seeks.push(e.target.currentTime);
}, true);
window.addEventListener("error", (e) => fixture.errors.push(e.message));
api.boot = async () => ({ user: null, logged_in: false, base: "https://fixture.invalid", strict_security: true });
api.play = async () => {
  fixture.playCalls++;
  if (new URLSearchParams(location.search).has("retry") && fixture.playCalls === 1) {
    throw new Error("HTTP 404：播放请求失败");
  }
  return { id: 991, ep_num: 1, title: "Seek fixture", duration: 90, video_url: "https://media.fixture.invalid/list.m3u8" };
};
api.video = async () => ({
  id: 991, title: "Seek fixture", description: "", cover: "", tags: [], score: 0,
  episode_count: 1, total_episodes: 1,
  episodes: [{ ep_num: 1, title: "one", duration: 90, status: 1 }],
  counts: { like: 0, collect: 0, comment: 0, hot: 0 },
  author: { id: 1, name: "Fixture" }, is_finished: true, is_original: 0,
});
function Fixture() {
  const [active, setActive] = useState(!initiallyInactive);
  const [settings, setSettings] = useState(false);
  fixture.setActive = setActive;
  return <AppProvider>
    <div style={{ position: "relative", width: 900, height: 540 }}>
      {legacyTest
        ? <Player src="https://media.fixture.invalid/list.m3u8" autoPlay active={active} className="w-full h-full" diagnosticContext={{ video_id: 991, active }} />
        : <Feed videoIds={[991]} mode="home" active={active} bottomInset={0} />}
    </div>
    <button onClick={() => setActive((v) => !v)}>切换播放页激活状态</button>
    {diagnosticTest && <button onClick={() => setSettings(v => !v)}>诊断设置</button>}
    {settings && <div style={{ position: "relative", width: 900, height: 700 }}><Settings onBack={() => setSettings(false)} wide /></div>}
  </AppProvider>;
}
createRoot(document.querySelector("#root")!).render(<Fixture />);

// 自动检查真正的浏览器命中测试，而非仅匹配 CSS 字符串。
async function checkIdleSeekTarget() {
  const deadline = performance.now() + 20000;
  while (performance.now() < deadline) {
    const video = document.querySelector("video");
    const input = document.querySelector<HTMLInputElement>('input[aria-label="播放进度"]');
    if (video && input && video.currentTime > 0 && document.querySelector(".player-root.is-idle")) {
      const rect = input.getBoundingClientRect();
      if (document.elementFromPoint(rect.x + rect.width * .8, rect.y + rect.height / 2) !== input) {
        throw new Error("闲置进度条点击穿透到视频");
      }
      document.querySelector("#results")!.textContent = JSON.stringify({ passed: 1, failed: 0 });
      return;
    }
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
  }
  throw new Error("等待播放或控制条隐藏超时");
}
if (!legacyTest && !initiallyInactive) void checkIdleSeekTarget().catch((error) => {
  fixture.errors.push(String(error));
  document.querySelector("#results")!.textContent = JSON.stringify({ passed: 0, failed: 1, error: String(error) });
});
