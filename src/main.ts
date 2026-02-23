// Ciallo main window logic.
// Listens for wake/state events from Rust backend, updates UI.
// Uses window.__TAURI__ (withGlobalTauri: true).

declare global {
  interface Window {
    __TAURI__: {
      core: {
        invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
      };
      event: {
        listen: (event: string, handler: (event: { payload: unknown }) => void) => Promise<() => void>;
      };
    };
  }
}

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const statusIndicator = document.getElementById('status-indicator')!;
const statusText = document.getElementById('status-text')!;
const feedback = document.getElementById('feedback')!;
const feedbackText = document.getElementById('feedback-text')!;

type AppState = 'Sleep' | 'WakeConfirm' | 'ModeSelect' | 'Capture' | 'Ocr' | 'Translate' | 'Render' | 'Idle';

function setStatus(state: AppState): void {
  statusIndicator.className = 'status';

  switch (state) {
    case 'Sleep':
      statusIndicator.classList.add('sleep');
      statusText.textContent = 'Sleep';
      break;
    case 'WakeConfirm':
      statusIndicator.classList.add('wake-confirm');
      statusText.textContent = 'Wake?';
      break;
    case 'ModeSelect':
      statusIndicator.classList.add('mode-select');
      statusText.textContent = 'Ready';
      break;
    default:
      statusIndicator.classList.add('active');
      statusText.textContent = state;
      break;
  }
}

function showFeedback(msg: string, durationMs: number = 2000): void {
  feedbackText.textContent = msg;
  feedback.classList.remove('hidden');
  setTimeout(() => feedback.classList.add('hidden'), durationMs);
}

// Audio context for wake sound feedback
let audioCtx: AudioContext | null = null;

function playWakeSound(): void {
  if (!audioCtx) {
    audioCtx = new AudioContext();
  }
  const osc = audioCtx.createOscillator();
  const gain = audioCtx.createGain();
  osc.connect(gain);
  gain.connect(audioCtx.destination);
  osc.frequency.value = 880;
  osc.type = 'sine';
  gain.gain.setValueAtTime(0.15, audioCtx.currentTime);
  gain.gain.exponentialRampToValueAtTime(0.001, audioCtx.currentTime + 0.15);
  osc.start();
  osc.stop(audioCtx.currentTime + 0.15);
}

function playRejectSound(): void {
  if (!audioCtx) {
    audioCtx = new AudioContext();
  }
  const osc = audioCtx.createOscillator();
  const gain = audioCtx.createGain();
  osc.connect(gain);
  gain.connect(audioCtx.destination);
  osc.frequency.value = 330;
  osc.type = 'sine';
  gain.gain.setValueAtTime(0.1, audioCtx.currentTime);
  gain.gain.exponentialRampToValueAtTime(0.001, audioCtx.currentTime + 0.1);
  osc.start();
  osc.stop(audioCtx.currentTime + 0.1);
}

// Event listeners
async function init(): Promise<void> {
  await listen('wake-detected', (event: { payload: unknown }) => {
    setStatus('WakeConfirm');
    playWakeSound();
    showFeedback('Wake detected...');
  });

  await listen('wake-confirmed', () => {
    setStatus('ModeSelect');
    showFeedback('Wake confirmed! Select mode.');
  });

  await listen('wake-rejected', () => {
    setStatus('Sleep');
    playRejectSound();
    showFeedback('False wake, back to sleep.');
  });

  await listen('play-sound', (event: { payload: unknown }) => {
    const soundId = event.payload as string;
    if (soundId === 'wake') playWakeSound();
    else if (soundId === 'reject') playRejectSound();
  });

  await listen('force-cancel', () => {
    setStatus('Sleep');
    showFeedback('Cancelled.');
  });

  // Phase 2: translation pipeline events
  await listen('capture-complete', () => {
    setStatus('Translate');
    showFeedback('Text captured, translating...');
  });

  await listen('capture-error', (event: { payload: unknown }) => {
    const data = event.payload as { error: string };
    setStatus('Sleep');
    showFeedback('Capture failed: ' + data.error);
  });

  await listen('translate-chunk', () => {
    setStatus('Render');
  });

  await listen('translate-complete', () => {
    setStatus('Idle');
    showFeedback('Translation complete.');
  });

  await listen('translate-error', (event: { payload: unknown }) => {
    const data = event.payload as { error: string };
    setStatus('Sleep');
    showFeedback('Translation failed: ' + data.error);
  });

  // Phase 3: OCR pipeline events
  await listen('ocr-started', () => {
    setStatus('Ocr');
    showFeedback('Running OCR...');
  });

  await listen('ocr-complete', (event: { payload: unknown }) => {
    const data = event.payload as { text: string; lines: number; elapsed_ms: number };
    setStatus('Translate');
    showFeedback(`OCR: ${data.lines} lines in ${Math.round(data.elapsed_ms)}ms. Translating...`);
  });

  await listen('ocr-error', (event: { payload: unknown }) => {
    const data = event.payload as { error: string };
    setStatus('Sleep');
    showFeedback('OCR failed: ' + data.error);
  });

  // Phase 4: Realtime incremental translation events
  await listen('realtime-started', () => {
    setStatus('Capture');
    showFeedback('Realtime mode active. Monitoring region...');
  });

  await listen('realtime-update', (event: { payload: unknown }) => {
    const data = event.payload as {
      lines: number;
      added: number;
      cached: number;
      token_saving_pct: number;
    };
    setStatus('Render');
    showFeedback(
      `Realtime: ${data.lines} lines (${data.added} new, ${data.cached} cached, saved ${Math.round(data.token_saving_pct)}%)`,
      3000
    );
  });

  await listen('realtime-error', (event: { payload: unknown }) => {
    const data = event.payload as { error: string };
    showFeedback('Realtime error: ' + data.error);
  });

  await listen('realtime-stopped', (event: { payload: unknown }) => {
    const data = event.payload as {
      token_saving_pct: number;
      lines_translated_via_api: number;
      lines_from_cache: number;
    };
    setStatus('Sleep');
    showFeedback(
      `Realtime stopped. Token saving: ${Math.round(data.token_saving_pct)}% (${data.lines_from_cache} cached / ${data.lines_translated_via_api} API)`,
      5000
    );
  });

  // Initial state
  const state = await invoke('get_state') as string;
  setStatus(state as AppState);
}

init().catch((e) => console.error('init failed:', e));

export {};
