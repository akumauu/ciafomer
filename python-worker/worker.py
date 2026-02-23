"""
Ciallo OCR Worker — Phase 3.
Standalone Python process communicating via stdin/stdout.
Protocol: MessagePack framed (4-byte BE length prefix + msgpack payload).
Model: PaddleOCR + OpenCV, lazy-loaded, idle>=60s auto-unload.
Features: ROI crop (rect/polygon/perspective), preprocessing (grayscale,
adaptive threshold, denoise, deskew).
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
_np = None
_msgpack = None

MODEL_IDLE_TIMEOUT = 60  # seconds before unloading model


def _ensure_msgpack():
    global _msgpack
    if _msgpack is None:
        import msgpack
        _msgpack = msgpack
    return _msgpack


def _ensure_cv2():
    global _cv2
    if _cv2 is None:
        import cv2
        _cv2 = cv2
    return _cv2


def _ensure_np():
    global _np
    if _np is None:
        import numpy as np
        _np = np
    return _np


class ImagePreprocessor:
    """ROI extraction and image preprocessing for OCR."""

    @staticmethod
    def decode_image(image_bytes):
        """Decode raw image bytes (PNG/JPEG) to numpy array."""
        np = _ensure_np()
        cv2 = _ensure_cv2()
        arr = np.frombuffer(image_bytes, dtype=np.uint8)
        img = cv2.imdecode(arr, cv2.IMREAD_COLOR)
        if img is None:
            raise ValueError("Failed to decode image")
        return img

    @staticmethod
    def crop_roi(img, roi_type, roi_params):
        """Crop image to region of interest.

        Supports:
        - rect: {x, y, w, h}
        - polygon: {points: [[x1,y1], [x2,y2], ...]}
        - perspective: {corners: [[x1,y1], [x2,y2], [x3,y3], [x4,y4]]}
        - fullframe: no cropping
        """
        np = _ensure_np()
        cv2 = _ensure_cv2()

        if roi_type == "rect":
            x = int(roi_params.get("x", 0))
            y = int(roi_params.get("y", 0))
            w = int(roi_params.get("w", img.shape[1]))
            h = int(roi_params.get("h", img.shape[0]))
            # Clamp to image bounds
            x = max(0, min(x, img.shape[1] - 1))
            y = max(0, min(y, img.shape[0] - 1))
            w = min(w, img.shape[1] - x)
            h = min(h, img.shape[0] - y)
            return img[y:y+h, x:x+w].copy()

        elif roi_type == "polygon":
            points = roi_params.get("points", [])
            if len(points) < 3:
                return img
            pts = np.array(points, dtype=np.int32)
            # Create mask
            mask = np.zeros(img.shape[:2], dtype=np.uint8)
            cv2.fillPoly(mask, [pts], 255)
            # Apply mask
            result = cv2.bitwise_and(img, img, mask=mask)
            # Crop to bounding rect of polygon
            x, y, w, h = cv2.boundingRect(pts)
            return result[y:y+h, x:x+w].copy()

        elif roi_type == "perspective":
            corners = roi_params.get("corners", [])
            if len(corners) != 4:
                return img
            src_pts = np.array(corners, dtype=np.float32)
            # Compute target rect size from perspective corners
            w1 = np.linalg.norm(src_pts[0] - src_pts[1])
            w2 = np.linalg.norm(src_pts[2] - src_pts[3])
            h1 = np.linalg.norm(src_pts[0] - src_pts[3])
            h2 = np.linalg.norm(src_pts[1] - src_pts[2])
            width = int(max(w1, w2))
            height = int(max(h1, h2))
            if width < 1 or height < 1:
                return img
            dst_pts = np.array([
                [0, 0],
                [width - 1, 0],
                [width - 1, height - 1],
                [0, height - 1]
            ], dtype=np.float32)
            M = cv2.getPerspectiveTransform(src_pts, dst_pts)
            return cv2.warpPerspective(img, M, (width, height))

        else:
            # fullframe or unknown — return as-is
            return img

    @staticmethod
    def preprocess(img, config):
        """Apply preprocessing pipeline.

        config keys: grayscale, adaptive_threshold, denoise, deskew
        """
        np = _ensure_np()
        cv2 = _ensure_cv2()
        result = img.copy()

        if config.get("grayscale", True):
            if len(result.shape) == 3:
                result = cv2.cvtColor(result, cv2.COLOR_BGR2GRAY)

        if config.get("denoise", True):
            if len(result.shape) == 2:
                result = cv2.fastNlMeansDenoising(result, h=10)
            else:
                result = cv2.fastNlMeansDenoisingColored(result, h=10)

        if config.get("adaptive_threshold", True):
            if len(result.shape) == 2:
                result = cv2.adaptiveThreshold(
                    result, 255,
                    cv2.ADAPTIVE_THRESH_GAUSSIAN_C,
                    cv2.THRESH_BINARY,
                    blockSize=11,
                    C=2
                )

        if config.get("deskew", False):
            result = ImagePreprocessor._deskew(result)

        # PaddleOCR expects BGR 3-channel, convert back if needed
        if len(result.shape) == 2:
            result = cv2.cvtColor(result, cv2.COLOR_GRAY2BGR)

        return result

    @staticmethod
    def _deskew(img):
        """Correct text skew using minimum area rectangle."""
        np = _ensure_np()
        cv2 = _ensure_cv2()

        # Work on a binary version for angle detection
        if len(img.shape) == 3:
            gray = cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)
        else:
            gray = img.copy()

        _, binary = cv2.threshold(gray, 0, 255, cv2.THRESH_BINARY_INV | cv2.THRESH_OTSU)

        coords = np.column_stack(np.where(binary > 0))
        if len(coords) < 10:
            return img

        angle = cv2.minAreaRect(coords)[-1]

        if angle < -45:
            angle = -(90 + angle)
        else:
            angle = -angle

        if abs(angle) < 0.5:
            return img

        h, w = img.shape[:2]
        center = (w // 2, h // 2)
        M = cv2.getRotationMatrix2D(center, angle, 1.0)
        rotated = cv2.warpAffine(
            img, M, (w, h),
            flags=cv2.INTER_CUBIC,
            borderMode=cv2.BORDER_REPLICATE
        )
        log.info(f"Deskew: rotated by {angle:.1f} degrees")
        return rotated


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

    def run_ocr(self, img):
        """Run OCR on a preprocessed image (numpy array, BGR).
        Returns list of {text, confidence, bbox, y_center}."""
        with self._lock:
            self._ensure_model()
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
        self.preprocessor = ImagePreprocessor()
        self._running = True

    def handle_message(self, msg):
        msg_type = msg.get("type", "")

        if msg_type == "ping":
            return {"type": "pong"}

        elif msg_type == "ocr":
            request_id = msg.get("request_id", "")
            image_data = msg.get("image_data", b"")
            roi_type = msg.get("roi_type", "fullframe")
            roi_params = msg.get("roi_params", {})
            preprocess_config = msg.get("preprocess", {})

            try:
                t0 = time.monotonic()

                # 1. Decode image
                img = self.preprocessor.decode_image(image_data)
                log.info(f"Decoded image: {img.shape[1]}x{img.shape[0]}")

                # 2. Crop ROI (must be ROI-only, not full frame for OCR)
                roi_img = self.preprocessor.crop_roi(img, roi_type, roi_params)
                log.info(f"ROI cropped ({roi_type}): {roi_img.shape[1]}x{roi_img.shape[0]}")

                # 3. Preprocess
                processed = self.preprocessor.preprocess(roi_img, preprocess_config)

                # 4. Run OCR on ROI only
                lines = self.engine.run_ocr(processed)

                elapsed_ms = (time.monotonic() - t0) * 1000
                log.info(
                    f"OCR done: {len(lines)} lines in {elapsed_ms:.1f}ms "
                    f"(request_id={request_id})"
                )

                return {
                    "type": "ocr_result",
                    "request_id": request_id,
                    "lines": lines,
                    "elapsed_ms": elapsed_ms,
                }

            except Exception as e:
                log.exception(f"OCR processing error: {e}")
                return {
                    "type": "error",
                    "request_id": request_id,
                    "message": str(e),
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
