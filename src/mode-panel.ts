// Mode panel window logic.
// Handles mode selection buttons, sends commands to Rust backend.

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

const modeButtons = document.querySelectorAll<HTMLButtonElement>('.mode-btn[data-mode]');
const dismissBtn = document.getElementById('btn-dismiss') as HTMLButtonElement;

// Mode selection
modeButtons.forEach((btn) => {
  btn.addEventListener('click', async () => {
    const mode = btn.getAttribute('data-mode');
    if (!mode) return;

    try {
      await invoke('select_mode', { mode });
      // Panel will be hidden by the backend after mode is processed
    } catch (e) {
      console.error('select_mode failed:', e);
    }
  });
});

// Dismiss / cancel
dismissBtn.addEventListener('click', async () => {
  try {
    await invoke('dismiss');
  } catch (e) {
    console.error('dismiss failed:', e);
  }
});

// Listen for force-cancel to visually reset
listen('force-cancel', () => {
  // Panel is hidden by backend; nothing to do here
}).catch((e) => console.error('listen force-cancel failed:', e));

export {};
