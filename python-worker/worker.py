"""
Ciallo OCR Worker (optional, Phase 3+).
Standalone Python process communicating via Named Pipe (Win) / Unix Socket.
Protocol: MessagePack + raw bytes (no base64 JSON).
Model: PaddleOCR + OpenCV, lazy-loaded, idle>=60s auto-unload.
"""

import sys
import time
import struct
import signal
import threading
import logging

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)
log = logging.getLogger("ocr-worker")

# Lazy-loaded modules (only import when needed)
_paddle_ocr = None
_cv2 = None
_msgpack = None

MODEL_IDLE_TIMEOUT = 60  # seconds before unloading model
HEALTH_CHECK_INTERVAL = 30


def _ensure_msgpack():
    global _msgpack
    if _msgpack is None:
        import msgpack
        _msgpack = msgpack
    return _msgpack


class OcrEngine:
    """Lazy-loading OCR engine with auto-unload on idle."""

    def __init__(self):
        self._model = None
        self._last_used = 0.0
        self._lock = threading.Lock()

    def _load_model(self):
        global _paddle_ocr
        if _paddle_ocr is None:
            from paddleocr import PaddleOCR
            _paddle_ocr = PaddleOCR
        log.info("Loading PaddleOCR model...")
        self._model = _paddle_ocr(use_angle_cls=True, lang="ch", show_log=False)
        self._last_used = time.monotonic()
        log.info("PaddleOCR model loaded.")

    def _ensure_model(self):
        if self._model is None:
            self._load_model()
        self._last_used = time.monotonic()

    def unload_if_idle(self):
        with self._lock:
            if self._model is not None:
                idle = time.monotonic() - self._last_used
                if idle >= MODEL_IDLE_TIMEOUT:
                    log.info(f"Unloading model after {idle:.0f}s idle.")
                    self._model = None

    def run_ocr(self, image_bytes: bytes) -> list:
        """Run OCR on raw image bytes. Returns list of (text, confidence, bbox)."""
        global _cv2
        if _cv2 is None:
            import cv2
            _cv2 = cv2
        import numpy as np

        with self._lock:
            self._ensure_model()
            arr = np.frombuffer(image_bytes, dtype=np.uint8)
            img = _cv2.imdecode(arr, _cv2.IMREAD_COLOR)
            if img is None:
                return []
            result = self._model.ocr(img, cls=True)
            lines = []
            if result and result[0]:
                for line in result[0]:
                    bbox = line[0]
                    text = line[1][0]
                    conf = float(line[1][1])
                    x_min = int(min(p[0] for p in bbox))
                    y_min = int(min(p[1] for p in bbox))
                    x_max = int(max(p[0] for p in bbox))
                    y_max = int(max(p[1] for p in bbox))
                    y_center = (y_min + y_max) // 2
                    lines.append({
                        "text": text,
                        "confidence": conf,
                        "bbox": (x_min, y_min, x_max - x_min, y_max - y_min),
                        "y_center": y_center,
                    })
            return lines


class WorkerServer:
    """IPC server using stdin/stdout with MessagePack framing."""

    def __init__(self):
        self.engine = OcrEngine()
        self._running = True

    def handle_message(self, msg: dict) -> dict:
        msg_type = msg.get("type", "")
        if msg_type == "ping":
            return {"type": "pong"}
        elif msg_type == "ocr":
            request_id = msg.get("request_id", "")
            image_data = msg.get("image_data", b"")
            t0 = time.monotonic()
            lines = self.engine.run_ocr(image_data)
            elapsed_ms = (time.monotonic() - t0) * 1000
            return {
                "type": "ocr_result",
                "request_id": request_id,
                "lines": lines,
                "elapsed_ms": elapsed_ms,
            }
        elif msg_type == "shutdown":
            self._running = False
            return {"type": "ack"}
        else:
            return {"type": "error", "message": f"unknown type: {msg_type}"}

    def run_stdio(self):
        """Simple framed protocol: 4-byte big-endian length prefix + msgpack payload."""
        msgpack = _ensure_msgpack()
        stdin = sys.stdin.buffer
        stdout = sys.stdout.buffer

        # Start idle checker thread
        idle_thread = threading.Thread(target=self._idle_checker, daemon=True)
        idle_thread.start()

        log.info("OCR worker ready (stdio mode).")

        while self._running:
            try:
                # Read length prefix
                len_bytes = stdin.read(4)
                if len(len_bytes) < 4:
                    break
                msg_len = struct.unpack(">I", len_bytes)[0]
                if msg_len > 50 * 1024 * 1024:  # 50MB safety limit
                    log.error(f"Message too large: {msg_len}")
                    break
                payload = stdin.read(msg_len)
                if len(payload) < msg_len:
                    break

                msg = msgpack.unpackb(payload, raw=False)
                response = self.handle_message(msg)
                resp_bytes = msgpack.packb(response, use_bin_type=True)

                stdout.write(struct.pack(">I", len(resp_bytes)))
                stdout.write(resp_bytes)
                stdout.flush()

            except Exception as e:
                log.exception(f"Error processing message: {e}")
                break

        log.info("OCR worker shutting down.")

    def _idle_checker(self):
        while self._running:
            time.sleep(10)
            self.engine.unload_if_idle()


def main():
    signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    server = WorkerServer()
    server.run_stdio()


if __name__ == "__main__":
    main()
