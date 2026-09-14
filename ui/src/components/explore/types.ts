// Canvas types for Explore visualization

export interface ChunkNode {
  id: string;
  filePath: string;
  title: string;
  createdAt: number; // ms timestamp
  modifiedAt: number;
  wordCount: number;
  tags: string[];
  sourceType: string;
  // Chunk-specific fields
  docId: number; // parent document ID for clustering
  chunkIndex: number; // position within parent doc
  // Layout
  x: number;
  y: number;
  width: number;
  height: number;
  // State
  hasSearchResults: boolean;
  isRelevant: boolean; // cited in synthesis
  opacity: number; // 1.0 normal, 0.3 when pushed aside
  // Animation
  timelineProgress?: number; // entrance animation position
  restingX: number;
  restingY: number;
  // Transition origin for smooth re-steers (snapshot of visual position before layout recompute)
  prevX: number;
  prevY: number;
  prevOpacity: number;
  // Chunk content (excerpt from the actual text)
  content: string;
  // Search result data (when highlighted)
  searchScore?: number;
  searchContent?: string;
}

export interface ClusterMetadata {
  filePath: string;
  center: { x: number; y: number };
  radius: number;
}

export type ExploreCanvasState =
  | "loading"
  | "idle"
  | "exploring"
  | "search_results"
  | "synthesis_complete";

// Layout worker messages
export interface LayoutWorkerInput {
  type: "compute_layout";
  docs: Array<{
    id: number;
    filePath: string;
    title: string | null;
    createdAt: number;
    modifiedAt: number;
    wordCount: number;
    tags: string[];
    sourceType: string;
    docId: number; // parent doc ID for clustering
    chunkIndex: number; // position within parent doc
  }>;
}

export interface LayoutWorkerOutput {
  type: "layout_complete";
  positions: Array<{
    id: number;
    filePath: string;
    x: number;
    y: number;
    width: number;
    height: number;
    docId: number;
    chunkIndex: number;
  }>;
}

// Derive a hue from file path for per-cluster color variation
export function pathHue(filePath: string): number {
  let hash = 0;
  for (let i = 0; i < filePath.length; i++) {
    hash = (hash * 31 + filePath.charCodeAt(i)) | 0;
  }
  return (hash & 0x7fffffff) % 360;
}

// Convert HSL (h: 0-360, s: 0-100, l: 0-100) to RGB (0-255)
export function hslToRgb(
  h: number,
  s: number,
  l: number
): { r: number; g: number; b: number } {
  const hN = h / 360;
  const sN = s / 100;
  const lN = l / 100;

  if (sN === 0) {
    const v = Math.round(lN * 255);
    return { r: v, g: v, b: v };
  }

  const hue2rgb = (p: number, q: number, t: number) => {
    if (t < 0) t += 1;
    if (t > 1) t -= 1;
    if (t < 1 / 6) return p + (q - p) * 6 * t;
    if (t < 1 / 2) return q;
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6;
    return p;
  };

  const q = lN < 0.5 ? lN * (1 + sN) : lN + sN - lN * sN;
  const p = 2 * lN - q;
  return {
    r: Math.round(hue2rgb(p, q, hN + 1 / 3) * 255),
    g: Math.round(hue2rgb(p, q, hN) * 255),
    b: Math.round(hue2rgb(p, q, hN - 1 / 3) * 255),
  };
}

// Color constants
export const COLORS = {
  background: "#0a0a0f",
  nodeBg: "#181824",
  nodeBorder: "#666680",
  nodeText: "#d8d8e5",
  nodeTextDim: "#aaaabd",
  titleText: "#f2f2f7",

  // Highlight colors (search results)
  searchGlow: { r: 80, g: 160, b: 255 }, // blue
  searchBorder: "#50a0ff",

  // Dim state
  dimOpacity: 0.32,
} as const;

// Rendering thresholds
export const SIMPLE_NODE_ZOOM_THRESHOLD = 0.5;
export const TEXT_RENDER_ZOOM_THRESHOLD = 0.15;

// Synthesis segment from parallel synthesis pipeline
export interface SynthesisSegment {
  segmentIndex: number;
  totalSegments: number;
  explanation: string;
  citedDocuments: Array<{
    filePath: string;
    docId?: number;
    chunkIndex?: number;
    relevanceNote?: string; // legacy
    excerpt?: string; // verbatim passage from the source
    heading?: string; // nearest section heading in the source
    blockRef?: string; // Readwise block reference (e.g. ^rw733395891)
  }>;
  theme: string;
}

// Animation durations (ms)
export const ENTRANCE_DURATION = 4000;
export const ENTRANCE_STAGGER = 0.6;
export const SYNTHESIS_ANIM_DURATION = 1500;
export const RESTEER_ANIM_DURATION = 800;
export const SEARCH_RESULT_ANIM_DURATION = 1200;
export const SOURCE_EXPAND_ANIM_DURATION = 250;
