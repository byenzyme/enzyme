// Camera: pan/zoom with coordinate transforms (ported from aficionado)

export interface CameraState {
  x: number;
  y: number;
  zoom: number;
}

export interface Bounds {
  minX: number;
  minY: number;
  maxX: number;
  maxY: number;
}

const MIN_ZOOM = 0.01;
const MAX_ZOOM = 2;

export class Camera {
  x = 0;
  y = 0;
  zoom = 1;

  private animationFrame: number | null = null;
  private onDirty: () => void;

  constructor(onDirty: () => void) {
    this.onDirty = onDirty;
  }

  getState(): CameraState {
    return { x: this.x, y: this.y, zoom: this.zoom };
  }

  // Transform screen coordinates to world coordinates
  screenToWorld(screenX: number, screenY: number): { x: number; y: number } {
    return {
      x: (screenX - this.x) / this.zoom,
      y: (screenY - this.y) / this.zoom,
    };
  }

  // Transform world coordinates to screen coordinates
  worldToScreen(worldX: number, worldY: number): { x: number; y: number } {
    return {
      x: worldX * this.zoom + this.x,
      y: worldY * this.zoom + this.y,
    };
  }

  // Get viewport bounds in world coordinates
  getViewportBounds(canvasWidth: number, canvasHeight: number): Bounds {
    const topLeft = this.screenToWorld(0, 0);
    const bottomRight = this.screenToWorld(canvasWidth, canvasHeight);
    return {
      minX: topLeft.x,
      minY: topLeft.y,
      maxX: bottomRight.x,
      maxY: bottomRight.y,
    };
  }

  pan(deltaX: number, deltaY: number): void {
    this.x += deltaX;
    this.y += deltaY;
    this.onDirty();
  }

  // Zoom toward a specific screen point
  zoomToward(screenX: number, screenY: number, factor: number): void {
    const newZoom = Math.max(MIN_ZOOM, Math.min(MAX_ZOOM, this.zoom * factor));
    const ratio = newZoom / this.zoom;
    this.x = screenX - (screenX - this.x) * ratio;
    this.y = screenY - (screenY - this.y) * ratio;
    this.zoom = newZoom;
    this.onDirty();
  }

  // Handle mouse wheel — always zoom toward cursor
  handleWheel(e: WheelEvent, canvasRect: DOMRect): void {
    const mouseX = e.clientX - canvasRect.left;
    const mouseY = e.clientY - canvasRect.top;

    // ctrlKey is set for trackpad pinch-to-zoom (smaller deltas, needs higher sensitivity)
    const sensitivity = e.ctrlKey ? 0.01 : 0.002;
    const factor = Math.max(0.5, Math.min(2, 1 - e.deltaY * sensitivity));
    this.zoomToward(mouseX, mouseY, factor);
  }

  // Animate camera to fit given bounds with padding.
  // rightInset shifts the visible center leftward (e.g. for an overlay panel).
  fitBoundsAnimated(
    bounds: Bounds,
    canvasWidth: number,
    canvasHeight: number,
    padding = 80,
    duration = 600,
    rightInset = 0
  ): void {
    const boundsWidth = bounds.maxX - bounds.minX;
    const boundsHeight = bounds.maxY - bounds.minY;

    if (boundsWidth <= 0 || boundsHeight <= 0) return;

    const availWidth = canvasWidth - padding * 2 - rightInset;
    const availHeight = canvasHeight - padding * 2;

    const targetZoom = Math.max(
      MIN_ZOOM,
      Math.min(MAX_ZOOM, Math.min(availWidth / boundsWidth, availHeight / boundsHeight))
    );

    const centerX = (bounds.minX + bounds.maxX) / 2;
    const centerY = (bounds.minY + bounds.maxY) / 2;

    const targetX = (canvasWidth - rightInset) / 2 - centerX * targetZoom;
    const targetY = canvasHeight / 2 - centerY * targetZoom;

    this.animateTo(targetX, targetY, targetZoom, duration);
  }

  // Snap camera to fit bounds immediately (no animation)
  fitBoundsInstant(
    bounds: Bounds,
    canvasWidth: number,
    canvasHeight: number,
    padding = 80,
    rightInset = 0
  ): void {
    const boundsWidth = bounds.maxX - bounds.minX;
    const boundsHeight = bounds.maxY - bounds.minY;

    if (boundsWidth <= 0 || boundsHeight <= 0) return;

    const availWidth = canvasWidth - padding * 2 - rightInset;
    const availHeight = canvasHeight - padding * 2;

    this.zoom = Math.max(
      MIN_ZOOM,
      Math.min(MAX_ZOOM, Math.min(availWidth / boundsWidth, availHeight / boundsHeight))
    );

    const centerX = (bounds.minX + bounds.maxX) / 2;
    const centerY = (bounds.minY + bounds.maxY) / 2;

    this.x = (canvasWidth - rightInset) / 2 - centerX * this.zoom;
    this.y = canvasHeight / 2 - centerY * this.zoom;
    this.onDirty();
  }

  // Smooth animation to target camera state
  animateTo(targetX: number, targetY: number, targetZoom: number, duration: number): void {
    if (this.animationFrame !== null) {
      cancelAnimationFrame(this.animationFrame);
      this.animationFrame = null;
    }

    const startX = this.x;
    const startY = this.y;
    const startZoom = this.zoom;
    const startTime = performance.now();

    const step = (now: number) => {
      const elapsed = now - startTime;
      const t = Math.min(1, elapsed / duration);
      // easeInOutCubic
      const ease = t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;

      this.x = startX + (targetX - startX) * ease;
      this.y = startY + (targetY - startY) * ease;
      this.zoom = startZoom + (targetZoom - startZoom) * ease;
      this.onDirty();

      if (t < 1) {
        this.animationFrame = requestAnimationFrame(step);
      } else {
        this.animationFrame = null;
      }
    };

    this.animationFrame = requestAnimationFrame(step);
  }

  cancelAnimation(): void {
    if (this.animationFrame !== null) {
      cancelAnimationFrame(this.animationFrame);
      this.animationFrame = null;
    }
  }

  destroy(): void {
    this.cancelAnimation();
  }
}
