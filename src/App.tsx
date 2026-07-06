import { createSignal, For, Show } from "solid-js";
import "./App.css";

type LogLevel = "info" | "success" | "warn" | "error";
interface LogLine {
  level: LogLevel;
  message: string;
  time: string;
}

// TODO: 백엔드 연동 시 실제 파일 메타로 교체
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
  const [upscale, setUpscale] = createSignal("2x");
  const [denoise, setDenoise] = createSignal(true);
  const [sharpen, setSharpen] = createSignal(false);
  const [autoLevel, setAutoLevel] = createSignal(true);
  const [grayscale, setGrayscale] = createSignal(false);

  const [format, setFormat] = createSignal("cbz");
  const [quality, setQuality] = createSignal(85);
  const [outputDir, setOutputDir] = createSignal("");

  // ── 실행 상태 ─────────────────────────────
  const [running, setRunning] = createSignal(false);
  const [progress, setProgress] = createSignal(0);
  const [logs, setLogs] = createSignal<LogLine[]>([]);

  function pushLog(level: LogLevel, message: string) {
    const time = new Date().toLocaleTimeString("ko-KR", { hour12: false });
    setLogs((prev) => [...prev, { level, message, time }]);
  }

  // ── 소스 조작 (임시 목업) ──────────────────
  function addFiles() {
    // TODO: Tauri dialog.open({ multiple: true }) 연동
    const n = sources().length + 1;
    setSources((prev) => [
      ...prev,
      { id: nextId++, name: `page-${String(n).padStart(3, "0")}.png`, path: `C:/comics/vol-01/page-${n}.png` },
    ]);
  }

  function addFolder() {
    // TODO: Tauri dialog.open({ directory: true }) 연동
    setSources((prev) => [
      ...prev,
      { id: nextId++, name: "vol-01/", path: "C:/comics/vol-01" },
    ]);
  }

  function removeSource(id: number) {
    setSources((prev) => prev.filter((s) => s.id !== id));
  }

  function clearSources() {
    setSources([]);
  }

  // ── 시작 (임시 목업 스트리밍) ───────────────
  function start() {
    if (running() || sources().length === 0) return;
    setRunning(true);
    setProgress(0);
    setLogs([]);
    pushLog("info", `${sources().length}개 항목 처리 시작`);

    // TODO: invoke("start_processing", { files, options }) + 이벤트 리스너로 교체
    const items = sources();
    let i = 0;
    const timer = setInterval(() => {
      if (i >= items.length) {
        clearInterval(timer);
        setProgress(100);
        pushLog("success", "모든 작업 완료 🎉");
        setRunning(false);
        return;
      }
      const item = items[i];
      pushLog("info", `[${i + 1}/${items.length}] ${item.name} 이미지 개선 중…`);
      pushLog("info", `  · 업스케일 ${upscale()} / 노이즈제거 ${denoise() ? "on" : "off"}`);
      pushLog("success", `  · ${format().toUpperCase()} 패키징 완료`);
      i++;
      setProgress(Math.round((i / items.length) * 100));
    }, 500);
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
        <button
          class="bg-accent hover:bg-accent-hover disabled:opacity-40 disabled:cursor-not-allowed text-white rounded-lg px-5 py-2.5 text-[15px] font-semibold cursor-pointer transition-colors"
          disabled={running() || sources().length === 0}
          onClick={start}
        >
          {running() ? "처리 중…" : "▶ 시작"}
        </button>
      </header>

      {/* 본문: 사이드바(소스+옵션) + 콘솔 */}
      <div class="grid grid-cols-[300px_1fr] gap-3.5 flex-1 min-h-0">
        <aside class="flex flex-col gap-3.5 min-h-0">
          {/* 소스 패널 */}
          <section class="bg-panel border border-edge rounded-[10px] flex flex-col shrink-0">
            <div class="flex items-center justify-between px-3.5 py-3 border-b border-edge">
              <h2 class="text-sm font-semibold">소스</h2>
              <div class="flex gap-1.5">
                <button class="bg-transparent text-muted border border-edge rounded-md px-2.5 py-1 text-xs cursor-pointer transition-colors hover:text-ink hover:border-accent" onClick={addFiles}>+ 파일</button>
                <button class="bg-transparent text-muted border border-edge rounded-md px-2.5 py-1 text-xs cursor-pointer transition-colors hover:text-ink hover:border-accent" onClick={addFolder}>+ 폴더</button>
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
                    <span class="text-xs opacity-70">이미지 파일 또는 1권 폴더</span>
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
              <h3 class="text-xs font-semibold uppercase tracking-wider text-muted m-0">이미지 개선</h3>

              <label class="flex flex-col gap-1.5 text-[13px]">
                <span class="text-muted">업스케일</span>
                <select
                  class="bg-panel2 border border-edge rounded-md px-2.5 py-2 text-ink text-[13px] focus:outline-none focus:border-accent"
                  value={upscale()}
                  onChange={(e) => setUpscale(e.currentTarget.value)}
                >
                  <option value="none">없음</option>
                  <option value="2x">2x</option>
                  <option value="4x">4x</option>
                </select>
              </label>

              <label class="flex items-center gap-2 text-[13px] cursor-pointer">
                <input type="checkbox" class="w-4 h-4 accent-accent cursor-pointer" checked={denoise()} onChange={(e) => setDenoise(e.currentTarget.checked)} />
                <span>노이즈 제거</span>
              </label>
              <label class="flex items-center gap-2 text-[13px] cursor-pointer">
                <input type="checkbox" class="w-4 h-4 accent-accent cursor-pointer" checked={sharpen()} onChange={(e) => setSharpen(e.currentTarget.checked)} />
                <span>샤픈 (선명하게)</span>
              </label>
              <label class="flex items-center gap-2 text-[13px] cursor-pointer">
                <input type="checkbox" class="w-4 h-4 accent-accent cursor-pointer" checked={autoLevel()} onChange={(e) => setAutoLevel(e.currentTarget.checked)} />
                <span>자동 레벨/대비 보정</span>
              </label>
              <label class="flex items-center gap-2 text-[13px] cursor-pointer">
                <input type="checkbox" class="w-4 h-4 accent-accent cursor-pointer" checked={grayscale()} onChange={(e) => setGrayscale(e.currentTarget.checked)} />
                <span>흑백 변환</span>
              </label>
            </div>

            <div class="p-3.5 flex flex-col gap-3">
              <h3 class="text-xs font-semibold uppercase tracking-wider text-muted m-0">패키징</h3>

              <label class="flex flex-col gap-1.5 text-[13px]">
                <span class="text-muted">출력 포맷</span>
                <select
                  class="bg-panel2 border border-edge rounded-md px-2.5 py-2 text-ink text-[13px] focus:outline-none focus:border-accent"
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
                <span class="text-muted">출력 경로</span>
                <div class="flex gap-1.5">
                  <input
                    type="text"
                    class="flex-1 bg-panel2 border border-edge rounded-md px-2.5 py-2 text-ink text-[13px] focus:outline-none focus:border-accent"
                    placeholder="출력 폴더 선택…"
                    value={outputDir()}
                    onInput={(e) => setOutputDir(e.currentTarget.value)}
                  />
                  {/* TODO: dialog.open({ directory: true }) */}
                  <button class="bg-transparent text-muted border border-edge rounded-md px-2.5 py-1 text-xs cursor-pointer transition-colors hover:text-ink hover:border-accent" onClick={() => setOutputDir("C:/comics/output")}>찾기</button>
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
