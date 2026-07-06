import { createSignal, For, Show, onMount } from "solid-js";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

// 지원 입력: 이미지 / PDF / CBZ·CBR 아카이브
const IMAGE_EXTS = ["png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff", "avif"];
const ARCHIVE_EXTS = ["cbz", "cbr", "zip"];
const DOC_EXTS = ["pdf"];
const ALL_EXTS = [...IMAGE_EXTS, ...ARCHIVE_EXTS, ...DOC_EXTS];

function baseName(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? p;
}

// 반복되는 Tailwind 클래스 묶음
const BTN_GHOST =
  "bg-transparent text-muted border border-edge rounded-md px-2.5 py-1 text-xs cursor-pointer transition-colors hover:text-ink hover:border-accent";
const FIELD_CONTROL =
  "bg-panel2 border border-edge rounded-md px-2.5 py-2 text-ink text-[13px] focus:outline-none focus:border-accent";
const CHECKBOX = "w-4 h-4 accent-accent cursor-pointer";

type LogLevel = "info" | "success" | "warn" | "error";
interface LogLine {
  level: LogLevel;
  message: string;
  time: string;
}

interface SourceItem {
  id: number;
  name: string;
  path: string;
}

const logColor: Record<LogLevel, string> = {
  info: "text-ink",
  success: "text-ok",
  warn: "text-warn",
  error: "text-bad",
};

function App() {
  // ── 소스 목록 ─────────────────────────────
  const [sources, setSources] = createSignal<SourceItem[]>([]);
  let nextId = 1;

  // ── 옵션 ─────────────────────────────────
  // waifu2x: 업스케일 배율(-s) + 노이즈 제거 레벨(-n)
  const [useWaifu2x, setUseWaifu2x] = createSignal(true);
  const [upscale, setUpscale] = createSignal("2x");
  const [denoiseLevel, setDenoiseLevel] = createSignal("1");

  // 종이 화이트닝: 스캔 배경색을 흰색으로
  const [whiten, setWhiten] = createSignal(true);
  const [whiteStrength, setWhiteStrength] = createSignal(70);
  const [keepColor, setKeepColor] = createSignal(true);

  const [format, setFormat] = createSignal("cbz");
  const [quality, setQuality] = createSignal(100);
  const [outputDir, setOutputDir] = createSignal("");

  // ── 실행 상태 ─────────────────────────────
  const [running, setRunning] = createSignal(false);
  const [progress, setProgress] = createSignal(0);
  const [logs, setLogs] = createSignal<LogLine[]>([]);

  function pushLog(level: LogLevel, message: string) {
    const time = new Date().toLocaleTimeString("ko-KR", { hour12: false });
    setLogs((prev) => [...prev, { level, message, time }]);
  }

  // ── 백엔드 이벤트 구독 ─────────────────────
  onMount(async () => {
    await listen<{ level: LogLevel; message: string }>("process://log", (e) =>
      pushLog(e.payload.level, e.payload.message),
    );
    await listen<{ percent: number }>("process://progress", (e) => setProgress(e.payload.percent));
    await listen("process://done", () => setRunning(false));
  });

  // ── 소스 조작 ─────────────────────────────
  function addPaths(paths: string[]) {
    setSources((prev) => {
      const existing = new Set(prev.map((s) => s.path));
      const added = paths
        .filter((p) => !existing.has(p))
        .map((p) => ({ id: nextId++, name: baseName(p), path: p }));
      return [...prev, ...added];
    });
  }

  async function addFiles() {
    const selected = await open({
      multiple: true,
      filters: [
        { name: "만화 파일 (이미지·PDF·CBZ)", extensions: ALL_EXTS },
        { name: "이미지", extensions: IMAGE_EXTS },
        { name: "PDF", extensions: DOC_EXTS },
        { name: "아카이브 (CBZ/CBR/ZIP)", extensions: ARCHIVE_EXTS },
      ],
    });
    if (!selected) return;
    addPaths(Array.isArray(selected) ? selected : [selected]);
  }

  async function addFolder() {
    const selected = await open({ directory: true, multiple: true });
    if (!selected) return;
    addPaths(Array.isArray(selected) ? selected : [selected]);
  }

  function removeSource(id: number) {
    setSources((prev) => prev.filter((s) => s.id !== id));
  }

  function clearSources() {
    setSources([]);
  }

  async function pickOutputDir() {
    const dir = await open({ directory: true });
    if (typeof dir === "string") setOutputDir(dir);
  }

  // ── 시작 ─────────────────────────────────
  async function start() {
    if (running() || sources().length === 0) return;
    setRunning(true);
    setProgress(0);
    setLogs([]);
    try {
      await invoke("start_processing", {
        sources: sources().map((s) => s.path),
        options: {
          useWaifu2x: useWaifu2x(),
          upscale: upscale(),
          denoiseLevel: denoiseLevel(),
          whiten: whiten(),
          whiteStrength: whiteStrength(),
          keepColor: keepColor(),
          format: format(),
          quality: quality(),
          outputDir: outputDir(),
        },
      });
    } catch (err) {
      pushLog("error", String(err));
      setRunning(false);
    }
  }

  async function cancel() {
    try {
      await invoke("cancel_processing");
      pushLog("warn", "중지 요청됨 — 현재 페이지까지 마무리 후 정지합니다");
    } catch (err) {
      pushLog("error", String(err));
    }
  }

  return (
    <div class="flex flex-col h-screen p-3.5 gap-3.5">
      {/* 헤더 */}
      <header class="flex items-center justify-between">
        <div class="flex items-center gap-3">
          <span class="text-3xl">📚</span>
          <div>
            <h1 class="text-xl font-bold leading-tight">ComicV</h1>
            <p class="text-xs text-muted">만화 이미지 개선 &amp; 패키징</p>
          </div>
        </div>
        <Show
          when={running()}
          fallback={
            <button
              class="bg-accent hover:bg-accent-hover disabled:opacity-40 disabled:cursor-not-allowed text-white rounded-lg px-5 py-2.5 text-[15px] font-semibold cursor-pointer transition-colors"
              disabled={sources().length === 0}
              onClick={start}
            >
              ▶ 시작
            </button>
          }
        >
          <button
            class="bg-bad hover:brightness-110 text-white rounded-lg px-5 py-2.5 text-[15px] font-semibold cursor-pointer transition-all"
            onClick={cancel}
          >
            ■ 정지
          </button>
        </Show>
      </header>

      {/* 본문: 사이드바(소스+옵션) + 콘솔 */}
      <div class="grid grid-cols-[300px_1fr] gap-3.5 flex-1 min-h-0">
        <aside class="flex flex-col gap-3.5 min-h-0">
          {/* 소스 패널 */}
          <section class="bg-panel border border-edge rounded-[10px] flex flex-col shrink-0">
            <div class="flex items-center justify-between px-3.5 py-3 border-b border-edge">
              <h2 class="text-sm font-semibold">소스</h2>
              <div class="flex gap-1.5">
                <button class={BTN_GHOST} onClick={addFiles}>+ 파일</button>
                <button class={BTN_GHOST} onClick={addFolder}>+ 폴더</button>
                <Show when={sources().length > 0}>
                  <button class="bg-transparent text-muted border border-edge rounded-md px-2.5 py-1 text-xs cursor-pointer transition-colors hover:text-bad hover:border-bad" onClick={clearSources}>비우기</button>
                </Show>
              </div>
            </div>

            <div class="overflow-y-auto p-2 max-h-[200px]">
              <Show
                when={sources().length > 0}
                fallback={
                  <div class="flex flex-col items-center justify-center gap-1.5 text-muted text-center py-8">
                    <p class="text-sm m-0">처리할 파일이나 폴더를 추가하세요</p>
                    <span class="text-xs opacity-70">이미지 · PDF · CBZ/CBR 또는 이미지 폴더</span>
                  </div>
                }
              >
                <For each={sources()}>
                  {(s) => (
                    <div class="flex items-center gap-2.5 px-2.5 py-2 rounded-lg hover:bg-panel2 transition-colors">
                      <span class="text-lg">🗂️</span>
                      <div class="flex flex-col flex-1 min-w-0">
                        <span class="text-[13px] font-medium truncate">{s.name}</span>
                        <span class="text-[11px] text-muted truncate">{s.path}</span>
                      </div>
                      <button class="bg-transparent border-none text-muted cursor-pointer text-[13px] p-1 rounded hover:text-bad" onClick={() => removeSource(s.id)}>✕</button>
                    </div>
                  )}
                </For>
              </Show>
            </div>
          </section>

          {/* 옵션 패널 */}
          <section class="bg-panel border border-edge rounded-[10px] flex flex-col flex-1 overflow-y-auto min-h-0">
            <div class="flex items-center justify-between px-3.5 py-3 border-b border-edge sticky top-0 bg-panel">
              <h2 class="text-sm font-semibold">옵션</h2>
            </div>

            <div class="p-3.5 border-b border-edge flex flex-col gap-3">
              <h3 class="text-xs font-semibold uppercase tracking-wider text-muted m-0">waifu2x · 업스케일 &amp; 노이즈</h3>

              <label class="flex items-center gap-2 text-[13px] cursor-pointer">
                <input type="checkbox" class={CHECKBOX} checked={useWaifu2x()} onChange={(e) => setUseWaifu2x(e.currentTarget.checked)} />
                <span>waifu2x 사용 <span class="text-muted">(끄면 변환만)</span></span>
              </label>

              <label class="flex flex-col gap-1.5 text-[13px]" classList={{ "opacity-40 pointer-events-none": !useWaifu2x() }}>
                <span class="text-muted">업스케일 배율</span>
                <select
                  class={FIELD_CONTROL}
                  value={upscale()}
                  onChange={(e) => setUpscale(e.currentTarget.value)}
                >
                  <option value="none">없음 (1x)</option>
                  <option value="2x">2x</option>
                  <option value="4x">4x</option>
                </select>
              </label>

              <label class="flex flex-col gap-1.5 text-[13px]" classList={{ "opacity-40 pointer-events-none": !useWaifu2x() }}>
                <span class="text-muted">노이즈 제거 레벨</span>
                <select
                  class={FIELD_CONTROL}
                  value={denoiseLevel()}
                  onChange={(e) => setDenoiseLevel(e.currentTarget.value)}
                >
                  <option value="none">없음</option>
                  <option value="0">0 (약)</option>
                  <option value="1">1 (기본)</option>
                  <option value="2">2 (강)</option>
                  <option value="3">3 (최강)</option>
                </select>
              </label>
            </div>

            <div class="p-3.5 border-b border-edge flex flex-col gap-3">
              <h3 class="text-xs font-semibold uppercase tracking-wider text-muted m-0">종이 화이트닝</h3>

              <label class="flex items-center gap-2 text-[13px] cursor-pointer">
                <input type="checkbox" class={CHECKBOX} checked={whiten()} onChange={(e) => setWhiten(e.currentTarget.checked)} />
                <span>스캔 종이색을 흰색으로 보정</span>
              </label>

              <label class="flex flex-col gap-1.5 text-[13px]" classList={{ "opacity-40 pointer-events-none": !whiten() }}>
                <span class="text-muted">강도 ({whiteStrength()})</span>
                <input
                  type="range"
                  class="accent-accent"
                  min="0"
                  max="100"
                  value={whiteStrength()}
                  onInput={(e) => setWhiteStrength(+e.currentTarget.value)}
                />
              </label>

              <label class="flex items-center gap-2 text-[13px] cursor-pointer" classList={{ "opacity-40 pointer-events-none": !whiten() }}>
                <input type="checkbox" class={CHECKBOX} checked={keepColor()} onChange={(e) => setKeepColor(e.currentTarget.checked)} />
                <span>컬러 페이지 색감 보존</span>
              </label>
            </div>

            <div class="p-3.5 flex flex-col gap-3">
              <h3 class="text-xs font-semibold uppercase tracking-wider text-muted m-0">패키징</h3>

              <label class="flex flex-col gap-1.5 text-[13px]">
                <span class="text-muted">출력 포맷</span>
                <select
                  class={FIELD_CONTROL}
                  value={format()}
                  onChange={(e) => setFormat(e.currentTarget.value)}
                >
                  <option value="cbz">CBZ</option>
                  <option value="pdf">PDF</option>
                  <option value="folder">폴더(이미지)</option>
                </select>
              </label>

              <label class="flex flex-col gap-1.5 text-[13px]">
                <span class="text-muted">품질 ({quality()})</span>
                <input
                  type="range"
                  class="accent-accent"
                  min="40"
                  max="100"
                  value={quality()}
                  onInput={(e) => setQuality(+e.currentTarget.value)}
                />
              </label>

              <label class="flex flex-col gap-1.5 text-[13px]">
                <span class="text-muted">출력 경로 <span class="opacity-70">(선택 · 비우면 입력 폴더 기준)</span></span>
                <div class="flex gap-1.5">
                  <input
                    type="text"
                    class={`flex-1 ${FIELD_CONTROL}`}
                    placeholder="비우면 입력 폴더에 저장…"
                    value={outputDir()}
                    onInput={(e) => setOutputDir(e.currentTarget.value)}
                  />
                  <button class={BTN_GHOST} onClick={pickOutputDir}>찾기</button>
                </div>
              </label>
            </div>
          </section>
        </aside>

        {/* 진행 + 로그 (메인) */}
        <section class="bg-panel border border-edge rounded-[10px] flex flex-col min-h-0">
          <div class="flex items-center justify-between px-3.5 py-2.5 border-b border-edge">
            <h2 class="text-sm font-semibold">진행 로그</h2>
            <div class="flex items-center gap-2.5 w-[45%]">
              <div class="flex-1 h-1.5 bg-panel2 rounded-full overflow-hidden">
                <div class="h-full bg-accent transition-[width] duration-300" style={{ width: `${progress()}%` }} />
              </div>
              <span class="text-xs text-muted min-w-[34px] text-right">{progress()}%</span>
            </div>
          </div>

          <div class="flex-1 overflow-y-auto px-3.5 py-2.5 font-mono text-xs">
            <Show when={logs().length > 0} fallback={<div class="text-muted opacity-60">시작하면 여기에 로그가 표시됩니다.</div>}>
              <For each={logs()}>
                {(l) => (
                  <div class="flex gap-2.5 py-px">
                    <span class="text-muted shrink-0">{l.time}</span>
                    <span class={logColor[l.level]}>{l.message}</span>
                  </div>
                )}
              </For>
            </Show>
          </div>
        </section>
      </div>
    </div>
  );
}

export default App;
