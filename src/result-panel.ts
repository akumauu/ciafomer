// Result panel window logic.
// Displays source text and streaming translation result.
// Incremental append for streaming chunks (no full re-render).

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

const sourceText = document.getElementById('source-text')!;
const translatedText = document.getElementById('translated-text')!;
const btnCopy = document.getElementById('btn-copy') as HTMLButtonElement;
const btnClose = document.getElementById('btn-close') as HTMLButtonElement;

// Source text display (emitted when capture completes)
listen('capture-complete', (event: { payload: unknown }) => {
  const data = event.payload as { text: string };
  sourceText.textContent = data.text;
  translatedText.textContent = ''; // Clear previous translation
}).catch((e) => console.error('listen capture-complete failed:', e));

// Streaming: append chunks incrementally (no full re-render)
listen('translate-chunk', (event: { payload: unknown }) => {
  const chunk = event.payload as string;
  // Incremental append via textContent concatenation
  translatedText.textContent += chunk;
}).catch((e) => console.error('listen translate-chunk failed:', e));

// Complete translation result (final text, replaces any streaming artifacts)
listen('translate-complete', (event: { payload: unknown }) => {
  const data = event.payload as { request_id: string; translated: string; source: string };
  translatedText.textContent = data.translated;
  if (data.source) {
    sourceText.textContent = data.source;
  }
}).catch((e) => console.error('listen translate-complete failed:', e));

// Copy button
btnCopy.addEventListener('click', async () => {
  const text = translatedText.textContent || '';
  if (text) {
    try {
      await navigator.clipboard.writeText(text);
      btnCopy.textContent = 'Copied!';
      setTimeout(() => { btnCopy.textContent = 'Copy'; }, 1500);
    } catch {
      // Fallback: use Tauri clipboard if available
      console.warn('clipboard write failed');
    }
  }
});

// Close button
btnClose.addEventListener('click', async () => {
  try {
    await invoke('dismiss');
  } catch (e) {
    console.error('dismiss failed:', e);
  }
});

// Reset on force-cancel
listen('force-cancel', () => {
  sourceText.textContent = '';
  translatedText.textContent = '';
}).catch((e) => console.error('listen force-cancel failed:', e));

export {};
