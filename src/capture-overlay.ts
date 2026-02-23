// Capture overlay window logic.
// Displays screenshot, allows user to select a region (rect/polygon/perspective),
// sends selection coordinates back to Rust backend.

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

const canvas = document.getElementById('capture-canvas') as HTMLCanvasElement;
const ctx = canvas.getContext('2d')!;
const toolbar = document.getElementById('capture-toolbar')!;
const hint = document.getElementById('capture-hint')!;
const cancelBtn = document.getElementById('btn-cancel-capture') as HTMLButtonElement;
const toolBtns = document.querySelectorAll<HTMLButtonElement>('.capture-tool-btn[data-tool]');

type SelectionTool = 'rect' | 'polygon' | 'perspective';
let currentTool: SelectionTool = 'rect';
let screenshotImage: HTMLImageElement | null = null;

// Rectangle selection state
let isDrawing = false;
let startX = 0;
let startY = 0;
let endX = 0;
let endY = 0;

// Polygon selection state
let polygonPoints: Array<{ x: number; y: number }> = [];

// Perspective (4-point) selection state
let perspectivePoints: Array<{ x: number; y: number }> = [];

// Load screenshot from backend
async function loadScreenshot(): Promise<void> {
  try {
    const base64 = (await invoke('get_screenshot_base64')) as string;
    if (!base64) {
      hint.textContent = 'No screenshot available.';
      return;
    }

    screenshotImage = new Image();
    screenshotImage.onload = () => {
      canvas.width = window.innerWidth;
      canvas.height = window.innerHeight;
      drawBackground();
      hint.textContent = getHintText();
    };
    screenshotImage.src = 'data:image/png;base64,' + base64;
  } catch (e) {
    console.error('Failed to load screenshot:', e);
    hint.textContent = 'Failed to load screenshot.';
  }
}

function getHintText(): string {
  switch (currentTool) {
    case 'rect':
      return 'Click and drag to select rectangle. Release to confirm.';
    case 'polygon':
      return 'Click to add points. Double-click to close polygon.';
    case 'perspective':
      return 'Click 4 corners in order (TL, TR, BR, BL).';
  }
}

function drawBackground(): void {
  if (!screenshotImage) return;

  // Draw screenshot scaled to fill canvas
  ctx.drawImage(screenshotImage, 0, 0, canvas.width, canvas.height);

  // Dim overlay
  ctx.fillStyle = 'rgba(0, 0, 0, 0.4)';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
}

function redraw(): void {
  drawBackground();

  ctx.strokeStyle = '#e94560';
  ctx.lineWidth = 2;
  ctx.setLineDash([]);

  switch (currentTool) {
    case 'rect':
      if (isDrawing) {
        const x = Math.min(startX, endX);
        const y = Math.min(startY, endY);
        const w = Math.abs(endX - startX);
        const h = Math.abs(endY - startY);

        // Clear the selected region to show original brightness
        if (screenshotImage) {
          const scaleX = screenshotImage.naturalWidth / canvas.width;
          const scaleY = screenshotImage.naturalHeight / canvas.height;
          ctx.drawImage(
            screenshotImage,
            x * scaleX, y * scaleY, w * scaleX, h * scaleY,
            x, y, w, h
          );
        }

        // Draw selection border
        ctx.strokeRect(x, y, w, h);

        // Draw dimensions
        ctx.fillStyle = 'rgba(233, 69, 96, 0.8)';
        ctx.font = '12px monospace';
        ctx.fillText(`${w}x${h}`, x + 4, y - 4);
      }
      break;

    case 'polygon':
      if (polygonPoints.length > 0) {
        ctx.beginPath();
        ctx.moveTo(polygonPoints[0].x, polygonPoints[0].y);
        for (let i = 1; i < polygonPoints.length; i++) {
          ctx.lineTo(polygonPoints[i].x, polygonPoints[i].y);
        }
        ctx.stroke();

        // Draw points
        for (const pt of polygonPoints) {
          ctx.fillStyle = '#e94560';
          ctx.beginPath();
          ctx.arc(pt.x, pt.y, 4, 0, Math.PI * 2);
          ctx.fill();
        }
      }
      break;

    case 'perspective':
      if (perspectivePoints.length > 0) {
        ctx.beginPath();
        ctx.moveTo(perspectivePoints[0].x, perspectivePoints[0].y);
        for (let i = 1; i < perspectivePoints.length; i++) {
          ctx.lineTo(perspectivePoints[i].x, perspectivePoints[i].y);
        }
        if (perspectivePoints.length === 4) {
          ctx.closePath();
        }
        ctx.stroke();

        // Draw numbered points
        for (let i = 0; i < perspectivePoints.length; i++) {
          const pt = perspectivePoints[i];
          ctx.fillStyle = '#e94560';
          ctx.beginPath();
          ctx.arc(pt.x, pt.y, 6, 0, Math.PI * 2);
          ctx.fill();
          ctx.fillStyle = '#fff';
          ctx.font = '10px monospace';
          ctx.fillText(`${i + 1}`, pt.x - 3, pt.y + 3);
        }
      }
      break;
  }
}

// Scale canvas coordinates to screenshot coordinates
function toScreenshotCoords(
  canvasX: number,
  canvasY: number
): { x: number; y: number } {
  if (!screenshotImage) return { x: canvasX, y: canvasY };
  const scaleX = screenshotImage.naturalWidth / canvas.width;
  const scaleY = screenshotImage.naturalHeight / canvas.height;
  return {
    x: Math.round(canvasX * scaleX),
    y: Math.round(canvasY * scaleY),
  };
}

// Submit selection to backend
async function submitSelection(): Promise<void> {
  try {
    switch (currentTool) {
      case 'rect': {
        const x = Math.min(startX, endX);
        const y = Math.min(startY, endY);
        const w = Math.abs(endX - startX);
        const h = Math.abs(endY - startY);
        if (w < 10 || h < 10) return; // Too small

        const tl = toScreenshotCoords(x, y);
        const br = toScreenshotCoords(x + w, y + h);
        await invoke('submit_ocr_selection', {
          roiType: 'rect',
          roiParams: {
            x: tl.x,
            y: tl.y,
            w: br.x - tl.x,
            h: br.y - tl.y,
          },
        });
        break;
      }
      case 'polygon': {
        if (polygonPoints.length < 3) return;
        const points = polygonPoints.map((pt) => {
          const sc = toScreenshotCoords(pt.x, pt.y);
          return [sc.x, sc.y];
        });
        await invoke('submit_ocr_selection', {
          roiType: 'polygon',
          roiParams: { points },
        });
        break;
      }
      case 'perspective': {
        if (perspectivePoints.length !== 4) return;
        const corners = perspectivePoints.map((pt) => {
          const sc = toScreenshotCoords(pt.x, pt.y);
          return [sc.x, sc.y];
        });
        await invoke('submit_ocr_selection', {
          roiType: 'perspective',
          roiParams: { corners },
        });
        break;
      }
    }
  } catch (e) {
    console.error('submit_ocr_selection failed:', e);
  }
}

async function cancelCapture(): Promise<void> {
  try {
    await invoke('cancel_ocr_capture');
  } catch (e) {
    console.error('cancel_ocr_capture failed:', e);
  }
}

// --- Mouse events ---

canvas.addEventListener('mousedown', (e: MouseEvent) => {
  if (currentTool === 'rect') {
    isDrawing = true;
    startX = e.clientX;
    startY = e.clientY;
    endX = e.clientX;
    endY = e.clientY;
  }
});

canvas.addEventListener('mousemove', (e: MouseEvent) => {
  if (currentTool === 'rect' && isDrawing) {
    endX = e.clientX;
    endY = e.clientY;
    redraw();
  }
});

canvas.addEventListener('mouseup', (e: MouseEvent) => {
  if (currentTool === 'rect' && isDrawing) {
    isDrawing = false;
    endX = e.clientX;
    endY = e.clientY;
    redraw();
    submitSelection();
  }
});

canvas.addEventListener('click', (e: MouseEvent) => {
  if (currentTool === 'polygon') {
    polygonPoints.push({ x: e.clientX, y: e.clientY });
    redraw();
  } else if (currentTool === 'perspective') {
    if (perspectivePoints.length < 4) {
      perspectivePoints.push({ x: e.clientX, y: e.clientY });
      redraw();
      if (perspectivePoints.length === 4) {
        submitSelection();
      }
    }
  }
});

canvas.addEventListener('dblclick', () => {
  if (currentTool === 'polygon' && polygonPoints.length >= 3) {
    redraw();
    submitSelection();
  }
});

// --- Keyboard ---

document.addEventListener('keydown', (e: KeyboardEvent) => {
  if (e.key === 'Escape') {
    cancelCapture();
  }
  // Undo last point for polygon/perspective
  if (e.key === 'z' && e.ctrlKey) {
    if (currentTool === 'polygon' && polygonPoints.length > 0) {
      polygonPoints.pop();
      redraw();
    } else if (currentTool === 'perspective' && perspectivePoints.length > 0) {
      perspectivePoints.pop();
      redraw();
    }
  }
});

// --- Toolbar ---

toolBtns.forEach((btn) => {
  btn.addEventListener('click', (e: MouseEvent) => {
    e.stopPropagation();
    const tool = btn.getAttribute('data-tool') as SelectionTool;
    if (!tool) return;

    currentTool = tool;
    toolBtns.forEach((b) => b.classList.remove('active'));
    btn.classList.add('active');

    // Reset selection state
    isDrawing = false;
    polygonPoints = [];
    perspectivePoints = [];
    redraw();
    hint.textContent = getHintText();
  });
});

cancelBtn.addEventListener('click', (e: MouseEvent) => {
  e.stopPropagation();
  cancelCapture();
});

// --- Window resize ---

window.addEventListener('resize', () => {
  canvas.width = window.innerWidth;
  canvas.height = window.innerHeight;
  redraw();
});

// --- Listen for force-cancel ---

listen('force-cancel', () => {
  // Window will be hidden by backend
}).catch((e) => console.error('listen force-cancel failed:', e));

// --- Init ---

canvas.width = window.innerWidth;
canvas.height = window.innerHeight;
loadScreenshot();

export {};
