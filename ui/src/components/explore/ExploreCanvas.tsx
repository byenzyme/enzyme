// ExploreCanvas: React wrapper for Canvas2D + WebGL glow visualization

import { useRef, useEffect } from "react";
import { Renderer } from "./Renderer";
import type { ChunkNode, ExploreCanvasState } from "./types";

interface ExploreCanvasProps {
  nodes: ChunkNode[];
  state: ExploreCanvasState;
  relevantPaths?: Set<string>;
  searchResults?: Map<string, { score: number; content: string }>;
  sourceExpandedPaths?: Set<string>;
  onFitToContent?: (fit: () => void) => void;
  onExposeZoomToFile?: (fn: (filePath: string) => void) => void;
  onExposeZoomToFilePaths?: (fn: (paths: Set<string>) => void) => void;
  onExposeZoomToRelevant?: (fn: () => void) => void;
  onClearSynthesis?: () => void;
  onDebugTriggerSynthesis?: (paths: Set<string>) => void;
  panelInset?: number;
}

export function ExploreCanvas({
  nodes,
  state,
  relevantPaths,
  searchResults,
  sourceExpandedPaths,
  onFitToContent,
  onExposeZoomToFile,
  onExposeZoomToFilePaths,
  onExposeZoomToRelevant,
  onClearSynthesis,
  onDebugTriggerSynthesis,
  panelInset,
}: ExploreCanvasProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const canvas2dRef = useRef<HTMLCanvasElement>(null);
  const glowCanvasRef = useRef<HTMLCanvasElement>(null);
  const rendererRef = useRef<Renderer | null>(null);
  const prevNodesRef = useRef<ChunkNode[]>([]);
  const prevStateRef = useRef<ExploreCanvasState>("loading");
  const prevSearchResultsRef = useRef<Map<string, { score: number; content: string }> | undefined>(undefined);
  const prevRelevantPathsRef = useRef<Set<string> | undefined>(undefined);

  // Initialize renderer
  useEffect(() => {
    if (!canvas2dRef.current || !glowCanvasRef.current || !containerRef.current) return;

    const renderer = new Renderer(canvas2dRef.current, glowCanvasRef.current);
    rendererRef.current = renderer;

    // Initial resize
    const rect = containerRef.current.getBoundingClientRect();
    renderer.resize(rect.width, rect.height);
    renderer.start();

    // Resize observer
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        renderer.resize(width, height);
      }
    });
    observer.observe(containerRef.current);

    return () => {
      observer.disconnect();
      renderer.destroy();
      rendererRef.current = null;
    };
  }, []);

  // Sync panel inset to renderer
  useEffect(() => {
    if (rendererRef.current) {
      rendererRef.current.setPanelInset(panelInset ?? 0);
    }
  }, [panelInset]);

  // Expose fit-to-content
  useEffect(() => {
    if (onFitToContent && rendererRef.current && containerRef.current) {
      onFitToContent(() => {
        if (rendererRef.current && containerRef.current) {
          const rect = containerRef.current.getBoundingClientRect();
          rendererRef.current.fitToContent(rect.width, rect.height);
        }
      });
    }
  }, [onFitToContent]);

  // Expose zoom-to-file-path (single file, for hover)
  useEffect(() => {
    if (onExposeZoomToFile && rendererRef.current && containerRef.current) {
      onExposeZoomToFile((filePath: string) => {
        if (rendererRef.current && containerRef.current) {
          const rect = containerRef.current.getBoundingClientRect();
          rendererRef.current.zoomToFilePath(filePath, rect.width, rect.height);
        }
      });
    }
  }, [onExposeZoomToFile]);

  // Expose zoom-to-file-paths (multiple files, for selection)
  useEffect(() => {
    if (onExposeZoomToFilePaths && rendererRef.current && containerRef.current) {
      onExposeZoomToFilePaths((paths: Set<string>) => {
        if (rendererRef.current && containerRef.current) {
          const rect = containerRef.current.getBoundingClientRect();
          rendererRef.current.zoomToFilePaths(paths, rect.width, rect.height);
        }
      });
    }
  }, [onExposeZoomToFilePaths]);

  // Expose zoom-to-relevant (all relevant nodes, default synthesis view)
  useEffect(() => {
    if (onExposeZoomToRelevant && rendererRef.current && containerRef.current) {
      onExposeZoomToRelevant(() => {
        if (rendererRef.current && containerRef.current) {
          const rect = containerRef.current.getBoundingClientRect();
          rendererRef.current.zoomToRelevantNodes(rect.width, rect.height);
        }
      });
    }
  }, [onExposeZoomToRelevant]);

  // Update nodes
  useEffect(() => {
    if (!rendererRef.current) return;

    if (nodes !== prevNodesRef.current && nodes.length > 0) {
      rendererRef.current.setNodes(nodes);
      prevNodesRef.current = nodes;

      // Start entrance animation on first load — snap camera to center
      // BEFORE the animation begins so nodes radiate outward from the
      // correct viewport center (no corner → center correction).
      if (state === "idle" && containerRef.current) {
        const rect = containerRef.current.getBoundingClientRect();
        rendererRef.current.fitToContentInstant(rect.width, rect.height);
        rendererRef.current.startEntrance();
      }
    }
  }, [nodes, state]);

  // Update state — detect synthesis→idle to animate nodes back to resting
  useEffect(() => {
    if (!rendererRef.current) return;
    const prevState = prevStateRef.current;
    prevStateRef.current = state;

    rendererRef.current.setState(state);

    if (
      (prevState === "synthesis_complete" || prevState === "search_results") &&
      state === "idle"
    ) {
      const renderer = rendererRef.current;
      const container = containerRef.current;
      if (!container) return;

      const rect = container.getBoundingClientRect();
      renderer.reverseSynthesis(rect.width, rect.height);
    }
  }, [state]);

  // Keep stable refs to callbacks for the debug object
  const onDebugTriggerRef = useRef(onDebugTriggerSynthesis);
  const onClearRef = useRef(onClearSynthesis);
  onDebugTriggerRef.current = onDebugTriggerSynthesis;
  onClearRef.current = onClearSynthesis;

  // Debug: expose synthesis trigger on window for console testing
  useEffect(() => {
    (window as any).__enzyme = {
      // Trigger synthesis via React state (full flow)
      // Usage: __enzyme.triggerSynthesis(0.15) — 15% of nodes
      triggerSynthesis(fraction = 0.2) {
        const renderer = rendererRef.current;
        if (!renderer) return;
        const allNodes = renderer.getNodes();
        if (allNodes.length === 0) return;

        const shuffled = [...allNodes].sort(() => Math.random() - 0.5);
        const count = Math.max(1, Math.round(allNodes.length * fraction));
        const paths = new Set(shuffled.slice(0, count).map((n) => n.filePath));
        console.log(`[enzyme debug] marking ${paths.size}/${allNodes.length} nodes as relevant`);

        onDebugTriggerRef.current?.(paths);
      },
      // Reset via React state (full flow — dismiss synthesis, animate back)
      reset() {
        onClearRef.current?.();
      },
    };
    return () => { delete (window as any).__enzyme; };
  }, []);

  // Apply search results
  useEffect(() => {
    if (!rendererRef.current || !searchResults) return;
    if (searchResults === prevSearchResultsRef.current) return;
    prevSearchResultsRef.current = searchResults;

    rendererRef.current.markSearchResults(searchResults);
  }, [searchResults]);

  // Apply synthesis layout + camera zoom simultaneously
  useEffect(() => {
    if (!rendererRef.current || !relevantPaths || !containerRef.current) return;
    if (relevantPaths === prevRelevantPathsRef.current) return;
    prevRelevantPathsRef.current = relevantPaths;

    const rect = containerRef.current.getBoundingClientRect();
    rendererRef.current.applySynthesisLayout(relevantPaths, rect.width, rect.height);
  }, [relevantPaths]);

  // Forward source-expanded paths to renderer
  const prevSourceExpandedRef = useRef<Set<string> | undefined>(undefined);
  useEffect(() => {
    if (!rendererRef.current) return;
    if (sourceExpandedPaths === prevSourceExpandedRef.current) return;
    prevSourceExpandedRef.current = sourceExpandedPaths;

    rendererRef.current.setSourceExpandedPaths(sourceExpandedPaths ?? new Set());
  }, [sourceExpandedPaths]);

  return (
    <div ref={containerRef} className="relative w-full h-full overflow-hidden">
      {/* WebGL glow layer (behind) */}
      <canvas
        ref={glowCanvasRef}
        className="absolute inset-0 pointer-events-none"
        style={{ mixBlendMode: "screen" }}
      />
      {/* Canvas 2D (interactive) */}
      <canvas ref={canvas2dRef} className="absolute inset-0" />
    </div>
  );
}
