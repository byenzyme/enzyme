// Renderer: main render loop orchestrating Canvas2D + WebGL glow (ported from aficionado)

import type { ChunkNode } from "./types";
import {
  COLORS,
  SIMPLE_NODE_ZOOM_THRESHOLD,
  ENTRANCE_DURATION,
  ENTRANCE_STAGGER,
  SYNTHESIS_ANIM_DURATION,
  RESTEER_ANIM_DURATION,
  SEARCH_RESULT_ANIM_DURATION,
} from "./types";
import { Camera, type CameraState } from "./Camera";
import { Quadtree } from "./Quadtree";
import { GlowRenderer } from "./GlowRenderer";
import { NodeRenderer } from "./NodeRenderer";

function easeOutExpo(t: number): number {
  return t === 1 ? 1 : 1 - Math.pow(2, -10 * t);
}

function easeInOutCubic(t: number): number {
  return t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
}

export class Renderer {
  private canvas2d: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private glowRenderer: GlowRenderer;
  private nodeRenderer: NodeRenderer;
  camera: Camera;
  private quadtree: Quadtree<ChunkNode> | null = null;

  private nodes: ChunkNode[] = [];
  private dirty = true;
  private running = false;
  private animFrameId: number | null = null;
  private dpr = 1;

  // Hover state
  private hoveredNode: ChunkNode | null = null;
  private hoverProgress = 0; // 0-1
  private hoverTarget = 0;

  // Entrance animation
  private entranceStartTime: number | null = null;
  private entranceActive = false;

  // Synthesis rearrangement animation
  private synthesisAnimStartTime: number | null = null;
  private synthesisAnimActive = false;
  private synthesisAnimReverse = false;
  private synthesisAnimDuration = SYNTHESIS_ANIM_DURATION;
  private preSynthesisCameraState: CameraState | null = null;

  // Search result animation
  private searchAnimStartTime: number | null = null;
  private searchAnimActive = false;

  // Search settling pulse (dim + pulse irrelevant nodes while branches arrive)
  private currentState: string = "loading";
  private searchSettleStartTime: number | null = null;

  // Exploring breathe (dim + pulse all nodes while waiting for results)
  private exploringStartTime: number | null = null;

  // Source expansion (synthesis panel hover/click)
  private sourceExpandedPaths: Set<string> = new Set();
  private sourceExpandProgress: Map<string, number> = new Map();
  private sourceExpandTargets: Map<string, number> = new Map();

  // Panel inset for viewport-aware centering
  private panelInset = 0;

  // Mouse interaction
  private isDragging = false;
  private lastMouseX = 0;
  private lastMouseY = 0;

  // Pan inertia
  private velocityX = 0;
  private velocityY = 0;
  private lastDragTime = 0;
  private inertiaActive = false;
  private static readonly INERTIA_FRICTION = 0.92;
  private static readonly INERTIA_MIN_VELOCITY = 0.5;

  constructor(canvas2d: HTMLCanvasElement, glowCanvas: HTMLCanvasElement) {
    this.canvas2d = canvas2d;
    this.ctx = canvas2d.getContext("2d")!;
    this.glowRenderer = new GlowRenderer();
    this.nodeRenderer = new NodeRenderer();
    this.camera = new Camera(() => this.markDirty());

    this.glowRenderer.init(glowCanvas);

    this.setupInteraction();
  }

  markDirty(): void {
    this.dirty = true;
  }

  setNodes(nodes: ChunkNode[]): void {
    this.nodes = nodes;
    this.quadtree = Quadtree.fromNodes(nodes);
    this.markDirty();
  }

  getNodes(): ChunkNode[] {
    return this.nodes;
  }

  // Rebuild the quadtree from current node positions (e.g. after
  // positions have been mutated externally without calling setNodes).
  rebuildQuadtree(): void {
    this.quadtree = Quadtree.fromNodes(this.nodes);
    this.markDirty();
  }

  setState(state: string): void {
    const prev = this.currentState;
    this.currentState = state;

    // Start breathe timer when entering exploring
    if (state === "exploring" && prev !== "exploring") {
      this.exploringStartTime = performance.now();
    }
    // Clear exploring breathe when leaving exploring
    if (state !== "exploring") {
      this.exploringStartTime = null;
    }

    // Start pulse timer when entering search_results
    if (state === "search_results" && prev !== "search_results") {
      this.searchSettleStartTime = performance.now();
    }
    // Clear pulse when leaving search_results
    if (state !== "search_results") {
      this.searchSettleStartTime = null;
    }

    this.markDirty();
  }

  setPanelInset(inset: number): void {
    this.panelInset = inset;
  }

  // Start entrance animation
  startEntrance(): void {
    this.entranceStartTime = performance.now();
    this.entranceActive = true;
    this.markDirty();
  }

  // Start synthesis rearrangement animation
  startSynthesisAnimation(reverse = false): void {
    this.synthesisAnimReverse = reverse;
    this.synthesisAnimStartTime = performance.now();
    this.synthesisAnimActive = true;
    this.markDirty();
  }

  // Start search result highlight animation
  startSearchAnimation(): void {
    this.searchAnimStartTime = performance.now();
    this.searchAnimActive = true;
    this.markDirty();
  }

  // Update which file paths have source-expanded nodes
  setSourceExpandedPaths(paths: Set<string>): void {
    // Set targets for newly added paths
    for (const p of paths) {
      if (!this.sourceExpandedPaths.has(p)) {
        this.sourceExpandTargets.set(p, 1);
        if (!this.sourceExpandProgress.has(p)) {
          this.sourceExpandProgress.set(p, 0);
        }
      }
    }
    // Set targets for removed paths (collapse)
    for (const p of this.sourceExpandedPaths) {
      if (!paths.has(p)) {
        this.sourceExpandTargets.set(p, 0);
      }
    }
    this.sourceExpandedPaths = new Set(paths);
    this.markDirty();
  }

  // Fit camera to show all nodes (animated)
  fitToContent(canvasWidth: number, canvasHeight: number): void {
    if (this.nodes.length === 0) return;

    const bounds = this.getNodeBounds();
    this.camera.fitBoundsAnimated(
      bounds,
      canvasWidth,
      canvasHeight,
      100,
      800,
      this.panelInset
    );
  }

  // Snap camera to show all nodes instantly (no animation)
  fitToContentInstant(canvasWidth: number, canvasHeight: number): void {
    if (this.nodes.length === 0) return;

    const bounds = this.getNodeBounds();
    this.camera.fitBoundsInstant(bounds, canvasWidth, canvasHeight, 100, this.panelInset);
  }

  private getNodeBounds(): { minX: number; minY: number; maxX: number; maxY: number } {
    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;

    for (const node of this.nodes) {
      minX = Math.min(minX, node.x);
      minY = Math.min(minY, node.y);
      maxX = Math.max(maxX, node.x + node.width);
      maxY = Math.max(maxY, node.y + node.height);
    }
    return { minX, minY, maxX, maxY };
  }

  // Zoom camera to show nodes matching a file path
  zoomToFilePath(
    filePath: string,
    canvasWidth: number,
    canvasHeight: number
  ): void {
    const matching = this.nodes.filter((n) => n.filePath === filePath);
    if (matching.length === 0) return;

    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;

    for (const node of matching) {
      minX = Math.min(minX, node.x);
      minY = Math.min(minY, node.y);
      maxX = Math.max(maxX, node.x + node.width);
      maxY = Math.max(maxY, node.y + node.height);
    }

    this.camera.fitBoundsAnimated(
      { minX, minY, maxX, maxY },
      canvasWidth,
      canvasHeight,
      160,
      600,
      this.panelInset
    );
  }

  // Zoom camera to show nodes matching any of several file paths
  zoomToFilePaths(
    filePaths: Set<string>,
    canvasWidth: number,
    canvasHeight: number
  ): void {
    const matching = this.nodes.filter((n) => filePaths.has(n.filePath));
    if (matching.length === 0) return;

    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;

    for (const node of matching) {
      minX = Math.min(minX, node.x);
      minY = Math.min(minY, node.y);
      maxX = Math.max(maxX, node.x + node.width);
      maxY = Math.max(maxY, node.y + node.height);
    }

    this.camera.fitBoundsAnimated(
      { minX, minY, maxX, maxY },
      canvasWidth,
      canvasHeight,
      120,
      400,
      this.panelInset
    );
  }

  // Zoom camera to show all relevant (synthesis-highlighted) nodes
  zoomToRelevantNodes(canvasWidth: number, canvasHeight: number): void {
    const relevant = this.nodes.filter((n) => n.isRelevant);
    if (relevant.length === 0) return;

    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;

    for (const node of relevant) {
      minX = Math.min(minX, node.x);
      minY = Math.min(minY, node.y);
      maxX = Math.max(maxX, node.x + node.width);
      maxY = Math.max(maxY, node.y + node.height);
    }

    this.camera.fitBoundsAnimated(
      { minX, minY, maxX, maxY },
      canvasWidth,
      canvasHeight,
      120,
      400,
      this.panelInset
    );
  }

  resize(width: number, height: number): void {
    this.dpr = window.devicePixelRatio || 1;
    this.canvas2d.width = Math.floor(width * this.dpr);
    this.canvas2d.height = Math.floor(height * this.dpr);
    this.canvas2d.style.width = `${width}px`;
    this.canvas2d.style.height = `${height}px`;
    this.glowRenderer.resize(width, height);
    this.markDirty();
  }

  start(): void {
    if (this.running) return;
    this.running = true;
    this.loop();
  }

  stop(): void {
    this.running = false;
    if (this.animFrameId !== null) {
      cancelAnimationFrame(this.animFrameId);
      this.animFrameId = null;
    }
  }

  destroy(): void {
    this.stop();
    this.camera.destroy();
    this.glowRenderer.destroy();
    this.removeInteraction();
  }

  private loop = (): void => {
    if (!this.running) return;

    // Update animations
    this.updateAnimations();

    if (this.dirty) {
      this.render();
      this.dirty = false;
    }

    this.animFrameId = requestAnimationFrame(this.loop);
  };

  private updateAnimations(): void {
    const now = performance.now();

    // Entrance animation
    if (this.entranceActive && this.entranceStartTime !== null) {
      const elapsed = now - this.entranceStartTime;
      if (elapsed >= ENTRANCE_DURATION + 500) {
        this.entranceActive = false;
        this.entranceStartTime = null;
      }
      this.markDirty();
    }

    // Synthesis rearrangement animation
    if (this.synthesisAnimActive && this.synthesisAnimStartTime !== null) {
      const elapsed = now - this.synthesisAnimStartTime;
      if (elapsed >= this.synthesisAnimDuration) {
        this.synthesisAnimActive = false;
        this.synthesisAnimStartTime = null;

        // If reverse animation completed, reset node state
        if (this.synthesisAnimReverse) {
          this.synthesisAnimReverse = false;
          for (const node of this.nodes) {
            node.x = node.restingX;
            node.y = node.restingY;
            node.opacity = 1;
            node.isRelevant = false;
            node.hasSearchResults = false;
            node.searchScore = undefined;
            node.searchContent = undefined;
          }
          this.quadtree = Quadtree.fromNodes(this.nodes);
          this.preSynthesisCameraState = null;
        }
      }
      this.markDirty();
    }

    // Search result animation
    if (this.searchAnimActive && this.searchAnimStartTime !== null) {
      const elapsed = now - this.searchAnimStartTime;
      if (elapsed >= SEARCH_RESULT_ANIM_DURATION) {
        this.searchAnimActive = false;
        this.searchAnimStartTime = null;
      }
      this.markDirty();
    }

    // Exploring breathe — continuous repaint while waiting for results
    if (this.currentState === "exploring" && this.exploringStartTime !== null) {
      this.markDirty();
    }

    // Search settling pulse — continuous repaint while branches arrive
    if (this.currentState === "search_results" && this.searchSettleStartTime !== null) {
      this.markDirty();
    }

    // Pan inertia
    if (this.inertiaActive) {
      this.camera.pan(this.velocityX, this.velocityY);
      this.velocityX *= Renderer.INERTIA_FRICTION;
      this.velocityY *= Renderer.INERTIA_FRICTION;

      const speed = Math.sqrt(this.velocityX ** 2 + this.velocityY ** 2);
      if (speed < Renderer.INERTIA_MIN_VELOCITY) {
        this.inertiaActive = false;
        this.velocityX = 0;
        this.velocityY = 0;
      }
      this.markDirty();
    }

    // Source expansion animation (per-path progress)
    if (this.sourceExpandTargets.size > 0) {
      const toDelete: string[] = [];
      for (const [path, target] of this.sourceExpandTargets) {
        const current = this.sourceExpandProgress.get(path) ?? 0;
        if (current === target) {
          // Cleanup collapsed entries
          if (target === 0) {
            toDelete.push(path);
          }
          continue;
        }
        const step = target > current ? 0.06 : -0.10;
        let next = current + step;
        if ((step > 0 && next >= target) || (step < 0 && next <= target)) {
          next = target;
        }
        this.sourceExpandProgress.set(path, next);
        this.markDirty();
      }
      for (const path of toDelete) {
        this.sourceExpandTargets.delete(path);
        this.sourceExpandProgress.delete(path);
      }
    }

    // Hover animation
    if (this.hoverTarget !== this.hoverProgress) {
      const delta = this.hoverTarget - this.hoverProgress;
      const step = delta > 0 ? 0.08 : -0.1;
      this.hoverProgress += step;
      if ((delta > 0 && this.hoverProgress >= this.hoverTarget) ||
          (delta < 0 && this.hoverProgress <= this.hoverTarget)) {
        this.hoverProgress = this.hoverTarget;
      }
      this.markDirty();
    }
  }

  private render(): void {
    const ctx = this.ctx;
    const { width, height } = this.canvas2d;
    const logicalWidth = width / this.dpr;
    const logicalHeight = height / this.dpr;

    // Clear
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    ctx.fillStyle = COLORS.background;
    ctx.fillRect(0, 0, logicalWidth, logicalHeight);

    // Render WebGL glow layer
    if (this.glowRenderer.isSupported() && !this.entranceActive) {
      const glowNodes = this.getVisibleNodes(logicalWidth, logicalHeight);
      this.glowRenderer.render(
        glowNodes,
        this.camera.getState(),
        logicalWidth,
        logicalHeight
      );
    }

    // Apply camera transform
    ctx.setTransform(
      this.dpr * this.camera.zoom,
      0,
      0,
      this.dpr * this.camera.zoom,
      this.dpr * this.camera.x,
      this.dpr * this.camera.y
    );

    // Get visible nodes
    const visibleNodes = this.getVisibleNodes(logicalWidth, logicalHeight);
    const zoom = this.camera.zoom;

    // Sort: non-highlighted first, then highlighted, then source-expanded on top
    const regularNodes: ChunkNode[] = [];
    const highlightedNodes: ChunkNode[] = [];
    const sourceExpandedNodes: ChunkNode[] = [];
    for (const node of visibleNodes) {
      if (node === this.hoveredNode) continue;
      const expandProgress = this.sourceExpandProgress.get(node.filePath);
      if (expandProgress !== undefined && expandProgress > 0) {
        sourceExpandedNodes.push(node);
      } else if (node.hasSearchResults || node.isRelevant) {
        highlightedNodes.push(node);
      } else {
        regularNodes.push(node);
      }
    }

    // Draw nodes
    const now = performance.now();

    for (const node of regularNodes) {
      this.renderNode(ctx, node, zoom, now);
    }
    for (const node of highlightedNodes) {
      this.renderNode(ctx, node, zoom, now);
    }

    // Draw source-expanded nodes (between highlighted and hovered)
    // Skip during entrance/synthesis animation
    if (!this.entranceActive && !this.synthesisAnimActive) {
      for (const node of sourceExpandedNodes) {
        const progress = this.sourceExpandProgress.get(node.filePath) ?? 0;
        if (zoom >= SIMPLE_NODE_ZOOM_THRESHOLD) {
          this.nodeRenderer.drawSourceExpanded(ctx, node, progress);
        } else {
          // Low zoom fallback: draw simple with highlighted border
          this.nodeRenderer.drawSimple(ctx, node);
        }
      }
    }

    // Draw hovered node on top
    if (this.hoveredNode && this.hoverProgress > 0) {
      this.nodeRenderer.drawExpanded(ctx, this.hoveredNode, this.hoverProgress);
    }
  }

  private renderNode(
    ctx: CanvasRenderingContext2D,
    node: ChunkNode,
    zoom: number,
    now: number
  ): void {
    // Apply opacity for synthesis state
    const nodeOpacity = node.opacity;

    // Entrance animation
    if (this.entranceActive && this.entranceStartTime !== null && node.timelineProgress !== undefined) {
      const elapsed = now - this.entranceStartTime;
      const nodeStartTime = node.timelineProgress * ENTRANCE_DURATION * ENTRANCE_STAGGER;
      const nodeProgress = Math.max(0, Math.min(1, (elapsed - nodeStartTime) / (ENTRANCE_DURATION * (1 - ENTRANCE_STAGGER))));

      if (nodeProgress <= 0) return; // Not yet visible

      const ease = easeOutExpo(nodeProgress);
      const opacity = Math.min(1, nodeProgress * 2.5) * nodeOpacity;

      // Animate from center axis toward resting position
      const centerX = this.getCenterX();
      const centerY = this.getCenterY();
      const animX = centerX + (node.restingX - centerX) * ease;
      const animY = centerY + (node.restingY - centerY) * ease;

      this.nodeRenderer.drawAnimated(ctx, node, animX, animY, opacity, zoom);
      return;
    }

    // Synthesis rearrangement animation — lerp from snapshot to target
    if (this.synthesisAnimActive && this.synthesisAnimStartTime !== null) {
      const elapsed = now - this.synthesisAnimStartTime;
      const t = Math.min(1, elapsed / this.synthesisAnimDuration);
      const ease = easeInOutCubic(t);

      // Lerp from snapshot (prevX/prevY) to target (node.x/y)
      const animX = node.prevX + (node.x - node.prevX) * ease;
      const animY = node.prevY + (node.y - node.prevY) * ease;
      const animOpacity = node.prevOpacity + (node.opacity - node.prevOpacity) * ease;

      this.nodeRenderer.drawAnimated(ctx, node, animX, animY, animOpacity, zoom);
      return;
    }

    // Exploring breathe: gentle dim + pulse all nodes while waiting for results.
    // Softer than search pulse — signals "working" without drawing focus to
    // individual nodes.
    if (
      this.currentState === "exploring" &&
      this.exploringStartTime !== null
    ) {
      const elapsed = now - this.exploringStartTime;

      // Fade from full opacity into the breathe over 600ms
      const fadeIn = Math.min(1, elapsed / 600);

      // ~1.6s full cycle, visible but not aggressive
      const pulse = 0.5 + 0.5 * Math.sin(elapsed * 0.004);
      // Keep the trough readable at overview zoom; the previous 0.4 floor
      // made tiny nodes disappear against the near-black background.
      const targetOpacity = 0.78 + 0.2 * pulse; // 0.78 → 0.98
      const opacity = 1 - (1 - targetOpacity) * fadeIn;

      ctx.globalAlpha = opacity;
      if (zoom < SIMPLE_NODE_ZOOM_THRESHOLD) {
        this.nodeRenderer.drawSimple(ctx, node);
      } else {
        this.nodeRenderer.draw(ctx, node, zoom);
      }
      ctx.globalAlpha = 1;
      return;
    }

    // Search settling pulse: breathe non-result nodes while branches arrive.
    // Preserve a visible breathe without letting the overview collapse into
    // black between search branches.
    if (
      this.currentState === "search_results" &&
      this.searchSettleStartTime !== null &&
      !node.hasSearchResults
    ) {
      const elapsed = now - this.searchSettleStartTime;

      // Fade from full opacity into the pulse over 500ms
      const fadeIn = Math.min(1, elapsed / 500);

      // Slow breathe (~2.5s full cycle), with a legible minimum opacity.
      const pulse = 0.5 + 0.5 * Math.sin(elapsed * 0.0025);
      const targetOpacity = 0.65 + 0.3 * pulse; // 0.65 → 0.95
      const opacity = 1 - (1 - targetOpacity) * fadeIn;

      ctx.globalAlpha = opacity;
      if (zoom < SIMPLE_NODE_ZOOM_THRESHOLD) {
        this.nodeRenderer.drawSimple(ctx, node);
      } else {
        this.nodeRenderer.draw(ctx, node, zoom);
      }
      ctx.globalAlpha = 1;
      return;
    }

    // Normal draw
    ctx.globalAlpha = nodeOpacity;

    if (zoom < SIMPLE_NODE_ZOOM_THRESHOLD) {
      this.nodeRenderer.drawSimple(ctx, node);
    } else {
      this.nodeRenderer.draw(ctx, node, zoom);
    }

    ctx.globalAlpha = 1;
  }

  private getVisibleNodes(logicalWidth: number, logicalHeight: number): ChunkNode[] {
    // During animation, nodes are at interpolated positions that don't match
    // the quadtree. Return all nodes to avoid culling glitches.
    if (this.synthesisAnimActive || this.entranceActive) return this.nodes;
    if (!this.quadtree) return this.nodes;

    const bounds = this.camera.getViewportBounds(logicalWidth, logicalHeight);
    return this.quadtree.queryBounds(bounds);
  }

  private getCenterX(): number {
    if (this.nodes.length === 0) return 0;
    let sum = 0;
    for (const n of this.nodes) sum += n.restingX + n.width / 2;
    return sum / this.nodes.length;
  }

  private getCenterY(): number {
    if (this.nodes.length === 0) return 0;
    let sum = 0;
    for (const n of this.nodes) sum += n.restingY + n.height / 2;
    return sum / this.nodes.length;
  }

  // Snapshot each node's current visual position into prevX/prevY/prevOpacity.
  // This captures mid-animation interpolated positions so that the next
  // synthesis layout can transition smoothly from wherever nodes are right now.
  private snapshotCurrentPositions(): void {
    if (this.synthesisAnimActive && this.synthesisAnimStartTime !== null) {
      // Mid-animation — compute interpolated positions
      const elapsed = performance.now() - this.synthesisAnimStartTime;
      const t = Math.min(1, elapsed / this.synthesisAnimDuration);
      const ease = easeInOutCubic(t);

      for (const node of this.nodes) {
        node.prevX = node.prevX + (node.x - node.prevX) * ease;
        node.prevY = node.prevY + (node.y - node.prevY) * ease;
        node.prevOpacity = node.prevOpacity + (node.opacity - node.prevOpacity) * ease;
      }
    } else {
      // Not animating — use settled positions
      for (const node of this.nodes) {
        node.prevX = node.x;
        node.prevY = node.y;
        node.prevOpacity = node.opacity;
      }
    }
  }

  // Apply synthesis layout: contract relevant clusters toward their centroid
  // while preserving each cluster's angular position from the resting layout.
  // Tag-similar docs stay angularly close; movement is radial contraction.
  // Camera targets FINAL positions directly — single-phase animation.
  applySynthesisLayout(
    relevantPaths: Set<string>,
    canvasWidth: number,
    canvasHeight: number
  ): void {
    // Detect if we're re-steering within an existing synthesis.
    // Use preSynthesisCameraState as the primary signal — it's explicitly
    // nulled on reverse-animation completion, so it accurately reflects
    // "we have a saved state to return to" regardless of animation timing.
    const isResteer =
      this.preSynthesisCameraState !== null || this.synthesisAnimActive;

    // Only save camera state on first entry — preserve the original for dismiss
    if (!isResteer) {
      this.preSynthesisCameraState = this.camera.getState();
    }

    // Snapshot current visual positions as the animation origin
    this.snapshotCurrentPositions();

    const relevant: ChunkNode[] = [];
    const irrelevant: ChunkNode[] = [];

    for (const node of this.nodes) {
      if (relevantPaths.has(node.filePath)) {
        relevant.push(node);
      } else {
        irrelevant.push(node);
      }
    }

    if (relevant.length === 0) return;

    // Global centroid of relevant nodes
    let gcx = 0,
      gcy = 0;
    for (const n of relevant) {
      gcx += n.restingX + n.width / 2;
      gcy += n.restingY + n.height / 2;
    }
    gcx /= relevant.length;
    gcy /= relevant.length;

    // Group relevant nodes by docId
    const clusterMap = new Map<number, ChunkNode[]>();
    for (const node of relevant) {
      let list = clusterMap.get(node.docId);
      if (!list) {
        list = [];
        clusterMap.set(node.docId, list);
      }
      list.push(node);
    }

    // Compute per-cluster centroid, bounding radius, and polar coords from centroid
    const clusters: Array<{
      nodes: ChunkNode[];
      cx: number;
      cy: number;
      radius: number;
      angle: number;
      dist: number;
    }> = [];

    for (const [, nodes] of clusterMap) {
      let cx = 0,
        cy = 0;
      for (const n of nodes) {
        cx += n.restingX + n.width / 2;
        cy += n.restingY + n.height / 2;
      }
      cx /= nodes.length;
      cy /= nodes.length;

      let maxR = 0;
      for (const n of nodes) {
        const dx = n.restingX + n.width / 2 - cx;
        const dy = n.restingY + n.height / 2 - cy;
        const r =
          Math.sqrt(dx * dx + dy * dy) + Math.max(n.width, n.height) / 2;
        maxR = Math.max(maxR, r);
      }

      const dx = cx - gcx;
      const dy = cy - gcy;
      clusters.push({
        nodes,
        cx,
        cy,
        radius: maxR,
        angle: Math.atan2(dy, dx),
        dist: Math.sqrt(dx * dx + dy * dy),
      });
    }

    // Sort by distance from centroid — place innermost first for collision greedy
    clusters.sort((a, b) => a.dist - b.dist);

    // Contract clusters radially inward: each slides along its existing
    // radial line toward the centroid, then gets pushed back out just
    // enough to avoid overlapping already-placed clusters.
    const CONTRACTION = 0.4; // pull to 40% of original distance
    const gap = 40;
    const placedClusters: Array<{ cx: number; cy: number; radius: number }> =
      [];

    for (const cluster of clusters) {
      let targetR = cluster.dist * CONTRACTION;
      const angle = cluster.angle;

      // Greedy collision resolution: push outward along radial line
      for (let attempt = 0; attempt < 50; attempt++) {
        const tcx = gcx + targetR * Math.cos(angle);
        const tcy = gcy + targetR * Math.sin(angle);

        let collision = false;
        for (const placed of placedClusters) {
          const pdx = tcx - placed.cx;
          const pdy = tcy - placed.cy;
          const dist = Math.sqrt(pdx * pdx + pdy * pdy);
          const minDist = cluster.radius + placed.radius + gap;
          if (dist < minDist) {
            targetR += minDist - dist + 1;
            collision = true;
            break;
          }
        }
        if (!collision) break;
      }

      const targetCx = gcx + targetR * Math.cos(angle);
      const targetCy = gcy + targetR * Math.sin(angle);
      placedClusters.push({
        cx: targetCx,
        cy: targetCy,
        radius: cluster.radius,
      });

      // Rigid-body translation — preserves internal distribution exactly
      const dx = targetCx - cluster.cx;
      const dy = targetCy - cluster.cy;

      for (const node of cluster.nodes) {
        node.isRelevant = true;
        node.opacity = 1;
        node.x = node.restingX + dx;
        node.y = node.restingY + dy;
      }
    }

    // Push irrelevant nodes 40% further from relevant centroid
    for (const node of irrelevant) {
      node.isRelevant = false;
      node.opacity = COLORS.dimOpacity;
      const dx = node.restingX + node.width / 2 - gcx;
      const dy = node.restingY + node.height / 2 - gcy;
      node.x = node.restingX + dx * 0.4;
      node.y = node.restingY + dy * 0.4;
    }

    this.quadtree = Quadtree.fromNodes(this.nodes);

    // Use shorter duration for re-steers (document swap, not mode transition)
    this.synthesisAnimDuration = isResteer
      ? RESTEER_ANIM_DURATION
      : SYNTHESIS_ANIM_DURATION;

    // Camera targets FINAL positions directly — no secondary zoom needed
    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;
    for (const node of relevant) {
      minX = Math.min(minX, node.x);
      minY = Math.min(minY, node.y);
      maxX = Math.max(maxX, node.x + node.width);
      maxY = Math.max(maxY, node.y + node.height);
    }
    this.camera.fitBoundsAnimated(
      { minX, minY, maxX, maxY },
      canvasWidth,
      canvasHeight,
      120,
      this.synthesisAnimDuration,
      this.panelInset
    );

    this.startSynthesisAnimation();
  }

  // Animate nodes and camera back to pre-synthesis state
  reverseSynthesis(canvasWidth: number, canvasHeight: number): void {
    // Snapshot current positions as animation origin
    this.snapshotCurrentPositions();

    // Set targets to resting positions
    for (const node of this.nodes) {
      node.x = node.restingX;
      node.y = node.restingY;
      node.opacity = 1;
    }

    this.synthesisAnimDuration = SYNTHESIS_ANIM_DURATION;

    if (this.preSynthesisCameraState) {
      const { x, y, zoom } = this.preSynthesisCameraState;
      this.camera.animateTo(x, y, zoom, SYNTHESIS_ANIM_DURATION);
    } else {
      this.fitToContent(canvasWidth, canvasHeight);
    }

    this.startSynthesisAnimation(true);
  }

  // Mark nodes with search results (clears stale results from previous searches)
  markSearchResults(resultPaths: Map<string, { score: number; content: string }>): void {
    for (const node of this.nodes) {
      const result = resultPaths.get(node.filePath);
      if (result) {
        node.hasSearchResults = true;
        node.searchScore = result.score;
        node.searchContent = result.content;
      } else {
        node.hasSearchResults = false;
        node.searchScore = undefined;
        node.searchContent = undefined;
      }
    }
    this.startSearchAnimation();
  }

  // Interaction setup
  private onMouseDown = (e: MouseEvent): void => {
    this.isDragging = true;
    this.inertiaActive = false;
    this.velocityX = 0;
    this.velocityY = 0;
    this.lastMouseX = e.clientX;
    this.lastMouseY = e.clientY;
    this.lastDragTime = performance.now();
    this.canvas2d.style.cursor = "grabbing";
  };

  private onMouseMove = (e: MouseEvent): void => {
    if (this.isDragging) {
      const now = performance.now();
      const dx = e.clientX - this.lastMouseX;
      const dy = e.clientY - this.lastMouseY;
      const dt = now - this.lastDragTime;

      // Track velocity with exponential smoothing (in pixels per frame at ~60fps)
      if (dt > 0) {
        const timeFactor = Math.min(dt / 16, 2);
        const instantVx = dx / timeFactor;
        const instantVy = dy / timeFactor;
        this.velocityX = 0.4 * instantVx + 0.6 * this.velocityX;
        this.velocityY = 0.4 * instantVy + 0.6 * this.velocityY;
      }

      this.camera.pan(dx, dy);
      this.lastMouseX = e.clientX;
      this.lastMouseY = e.clientY;
      this.lastDragTime = now;
      return;
    }

    // Hit detection for hover — skip during animation since rendered
    // positions are interpolated and don't match the quadtree
    if (this.synthesisAnimActive || this.entranceActive) return;

    const rect = this.canvas2d.getBoundingClientRect();
    const screenX = e.clientX - rect.left;
    const screenY = e.clientY - rect.top;
    const world = this.camera.screenToWorld(screenX, screenY);

    const hit = this.quadtree?.findAtPoint(world.x, world.y) ?? null;

    if (hit !== this.hoveredNode) {
      this.hoveredNode = hit;
      this.hoverTarget = hit ? 1 : 0;
      if (!hit) {
        this.hoverProgress = Math.min(this.hoverProgress, 1);
      }
      this.canvas2d.style.cursor = hit ? "pointer" : "grab";
      this.markDirty();
    }
  };

  private onMouseUp = (): void => {
    if (this.isDragging) {
      this.isDragging = false;

      // If mouse was stationary for >60ms before release, kill velocity
      const timeSinceLastMove = performance.now() - this.lastDragTime;
      if (timeSinceLastMove > 60) {
        this.velocityX = 0;
        this.velocityY = 0;
      }

      // Start inertia if velocity is above threshold
      const speed = Math.sqrt(this.velocityX ** 2 + this.velocityY ** 2);
      if (speed > Renderer.INERTIA_MIN_VELOCITY) {
        this.inertiaActive = true;
        this.markDirty();
      }
    }
    this.canvas2d.style.cursor = this.hoveredNode ? "pointer" : "grab";
  };

  private onWheel = (e: WheelEvent): void => {
    e.preventDefault();
    this.inertiaActive = false;
    const rect = this.canvas2d.getBoundingClientRect();
    this.camera.handleWheel(e, rect);
  };

  private setupInteraction(): void {
    this.canvas2d.addEventListener("mousedown", this.onMouseDown);
    window.addEventListener("mousemove", this.onMouseMove);
    window.addEventListener("mouseup", this.onMouseUp);
    this.canvas2d.addEventListener("wheel", this.onWheel, { passive: false });
    this.canvas2d.style.cursor = "grab";
  }

  private removeInteraction(): void {
    this.canvas2d.removeEventListener("mousedown", this.onMouseDown);
    window.removeEventListener("mousemove", this.onMouseMove);
    window.removeEventListener("mouseup", this.onMouseUp);
    this.canvas2d.removeEventListener("wheel", this.onWheel);
  }
}
