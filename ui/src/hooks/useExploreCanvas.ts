// useExploreCanvas: state management, SSE integration, Web Worker orchestration

import React, { useState, useEffect, useRef, useCallback } from "react";
import type {
  ChunkNode,
  ExploreCanvasState,
  LayoutWorkerInput,
  LayoutWorkerOutput,
  SynthesisSegment,
} from "../components/explore/types";
import type { ChunkSummary } from "../types/enzyme";
import * as api from "../lib/api";

export interface SteeringSuggestion {
  topic_name: string;
  bent_prompt: string;
}

interface UseExploreCanvasResult {
  nodes: ChunkNode[];
  state: ExploreCanvasState;
  searchResults: Map<string, { score: number; content: string }>;
  relevantPaths: Set<string> | undefined;
  synthesisText: string;
  synthesisSegments: SynthesisSegment[];
  statusText: string;
  explore: (query: string) => void;
  clearSynthesis: () => void;
  debugTriggerSynthesis: (paths: Set<string>) => void;
  isExploring: boolean;
  // Steering
  steeringSuggestions: SteeringSuggestion[];
  isSteeredPreview: boolean;
  previewBentPrompt: (bentPrompt: string) => void;
  resetToOriginal: () => void;
  originalQuery: string;
}

export function useExploreCanvas(initialQuery?: string): UseExploreCanvasResult {
  const [nodes, setNodes] = useState<ChunkNode[]>([]);
  const [state, setState] = useState<ExploreCanvasState>("loading");
  const [searchResults, setSearchResults] = useState<
    Map<string, { score: number; content: string }>
  >(new Map());
  const [relevantPaths, setRelevantPaths] = useState<Set<string> | undefined>();
  const [synthesisText, setSynthesisText] = useState("");
  const [synthesisSegments, setSynthesisSegments] = useState<SynthesisSegment[]>([]);
  const [statusText, setStatusText] = useState("Loading chunks...");
  const [isExploring, setIsExploring] = useState(false);

  // Steering state
  const [steeringSuggestions, setSteeringSuggestions] = useState<SteeringSuggestion[]>([]);
  const [isSteeredPreview, setIsSteeredPreview] = useState(false);
  const originalQueryRef = useRef<string>("");
  const originalSearchResultsRef = useRef<Map<string, { score: number; content: string }>>(new Map());
  const originalRelevantPathsRef = useRef<Set<string> | undefined>(undefined);

  const workerRef = useRef<Worker | null>(null);
  const chunksRef = useRef<ChunkSummary[]>([]);
  const abortRef = useRef<AbortController | null>(null);

  // Initialize: fetch chunks and compute layout
  useEffect(() => {
    const fetchChunks = async () => {
      try {
        setState("loading");
        setStatusText("Loading chunks...");

        const data = await api.getChunks();
        const chunks: ChunkSummary[] = data.chunks || [];
        chunksRef.current = chunks;

        // Pre-warm LLM prompt cache
        if (chunks.length > 0) {
          api.warm(initialQuery);
        }

        if (chunks.length === 0) {
          setState("idle");
          setStatusText("No chunks found");
          return;
        }

        const uniqueDocs = new Set(chunks.map((c) => c.doc_id)).size;
        setStatusText(
          `Computing layout for ${chunks.length} chunks across ${uniqueDocs} documents...`
        );

        // Post to layout worker
        const worker = new Worker(
          new URL("../workers/explore-layout.worker.ts", import.meta.url),
          { type: "module" }
        );
        workerRef.current = worker;

        worker.onmessage = (e: MessageEvent<LayoutWorkerOutput>) => {
          if (e.data.type !== "layout_complete") return;

          const { positions } = e.data;
          const chunkMap = new Map(chunks.map((c) => [c.chunk_id, c]));

          const chunkNodes: ChunkNode[] = positions.map((pos) => {
            const chunk = chunkMap.get(pos.id)!;
            return {
              id: String(pos.id),
              filePath: pos.filePath,
              title: chunk.title || pos.filePath.split("/").pop() || "Untitled",
              createdAt: chunk.created_at,
              modifiedAt: chunk.modified_at,
              wordCount: chunk.chunk_word_count,
              tags: chunk.tags,
              sourceType: "markdown",
              content: chunk.content || "",
              docId: pos.docId,
              chunkIndex: pos.chunkIndex,
              x: pos.x,
              y: pos.y,
              width: pos.width,
              height: pos.height,
              restingX: pos.x,
              restingY: pos.y,
              prevX: pos.x,
              prevY: pos.y,
              prevOpacity: 1,
              hasSearchResults: false,
              isRelevant: false,
              opacity: 1,
              timelineProgress: (pos as any).timelineProgress ?? 0,
            };
          });

          setNodes(chunkNodes);
          setState("idle");
          setStatusText("");
          worker.terminate();
          workerRef.current = null;
        };

        const input: LayoutWorkerInput = {
          type: "compute_layout",
          docs: chunks.map((c) => ({
            id: c.chunk_id,
            filePath: c.file_path,
            title: c.title,
            createdAt: c.created_at,
            modifiedAt: c.modified_at,
            wordCount: c.chunk_word_count,
            tags: c.tags,
            sourceType: "markdown",
            docId: c.doc_id,
            chunkIndex: c.chunk_index,
          })),
        };
        worker.postMessage(input);
      } catch (err) {
        console.error("Failed to load chunks:", err);
        setState("idle");
        setStatusText("Failed to load chunks");
      }
    };

    fetchChunks();

    return () => {
      workerRef.current?.terminate();
      workerRef.current = null;
    };
  }, []);

  // Explore: SSE stream
  const explore = useCallback(
    (query: string) => {
      if (!query.trim()) return;

      // Abort previous
      abortRef.current?.abort();
      const abort = new AbortController();
      abortRef.current = abort;

      setIsExploring(true);
      setState("exploring");
      setStatusText("Thinking...");
      setSynthesisText("");
      setSynthesisSegments([]);
      setRelevantPaths(undefined);

      // Reset steering state
      setSteeringSuggestions([]);
      setIsSteeredPreview(false);
      originalQueryRef.current = query;

      // Fire steering in parallel (fire-and-forget)
      api.steer(query, abort.signal)
        .then((suggestions) => setSteeringSuggestions(suggestions))
        .catch((err) => {
          if (err.name !== "AbortError") {
            console.warn("[steering] Failed to fetch suggestions:", err);
          }
        });

      // Reset search state on nodes
      setSearchResults(new Map());
      setNodes((prev) =>
        prev.map((n) => ({
          ...n,
          hasSearchResults: false,
          isRelevant: false,
          opacity: 1,
          searchScore: undefined,
          searchContent: undefined,
          x: n.restingX,
          y: n.restingY,
          prevX: n.restingX,
          prevY: n.restingY,
          prevOpacity: 1,
        }))
      );

      const accumulatedResults = new Map<
        string,
        { score: number; content: string }
      >();

      const runExplore = async () => {
        try {
          for await (const { event: eventType, data } of api.explore(query, {
            maxQueries: 4,
            resultsPerQuery: 8,
            signal: abort.signal,
          })) {
            handleSSEEvent(
              eventType,
              data,
              accumulatedResults,
              setStatusText,
              setSearchResults,
              setRelevantPaths,
              setSynthesisText,
              setSynthesisSegments,
              setState
            );
          }

          // Save originals for steering reset
          originalSearchResultsRef.current = new Map(accumulatedResults);
          originalRelevantPathsRef.current = new Set(accumulatedResults.keys());

          setIsExploring(false);
        } catch (err: any) {
          if (err.name === "AbortError") return;
          console.error("Explore error:", err);
          setStatusText("Exploration failed");
          setState("idle");
          setIsExploring(false);
        }
      };

      runExplore();
    },
    []
  );

  const clearSynthesis = useCallback(() => {
    setSynthesisText("");
    setSynthesisSegments([]);
    setRelevantPaths(undefined);
    setStatusText("");
    setSearchResults(new Map());
    setState("idle");
    setSteeringSuggestions([]);
    setIsSteeredPreview(false);
  }, []);

  const debugTriggerSynthesis = useCallback((paths: Set<string>) => {
    setSynthesisText("[Debug] Synthesis triggered from console.");
    setRelevantPaths(paths);
    setStatusText("");
    setState("synthesis_complete");
  }, []);

  // Steering: preview a bent prompt
  const previewBentPrompt = useCallback(
    (bentPrompt: string) => {
      setIsSteeredPreview(true);

      api.preview(bentPrompt)
        .then((results) => {
          const newSearchResults = new Map<string, { score: number; content: string }>();
          for (const r of results) {
            newSearchResults.set(r.file_path, {
              score: r.score,
              content: r.content || "",
            });
          }
          setSearchResults(() => newSearchResults);
          setRelevantPaths(new Set(newSearchResults.keys()));
        })
        .catch(() => {});
    },
    []
  );

  // Steering: reset to original exploration state
  const resetToOriginal = useCallback(() => {
    setIsSteeredPreview(false);
    setSearchResults(() => new Map(originalSearchResultsRef.current));
    setRelevantPaths(
      originalRelevantPathsRef.current
        ? new Set(originalRelevantPathsRef.current)
        : undefined
    );
  }, []);

  return {
    nodes,
    state,
    searchResults,
    relevantPaths,
    synthesisText,
    synthesisSegments,
    statusText,
    explore,
    clearSynthesis,
    debugTriggerSynthesis,
    isExploring,
    // Steering
    steeringSuggestions,
    isSteeredPreview,
    previewBentPrompt,
    resetToOriginal,
    originalQuery: originalQueryRef.current,
  };
}

// Process SSE events
function handleSSEEvent(
  eventType: string,
  data: any,
  accumulatedResults: Map<string, { score: number; content: string }>,
  setStatusText: (s: string) => void,
  setSearchResults: (
    fn: (
      prev: Map<string, { score: number; content: string }>
    ) => Map<string, { score: number; content: string }>
  ) => void,
  setRelevantPaths: (p: Set<string>) => void,
  setSynthesisText: (s: string) => void,
  setSynthesisSegments: React.Dispatch<React.SetStateAction<SynthesisSegment[]>>,
  setState: (s: ExploreCanvasState) => void
): void {
  switch (eventType) {
    case "explore_thinking":
      setStatusText(data.text || data.status || data.message || "Thinking...");

      // After searches complete, the next thinking event signals synthesis start.
      if (accumulatedResults.size > 0) {
        const paths = new Set(accumulatedResults.keys());
        setRelevantPaths(paths);
        setState("synthesis_complete");
      }
      break;

    case "explore_search": {
      const query = data.query || "";
      const results = data.results || [];
      setStatusText(`Searching: ${query}`);

      for (const r of results) {
        const existing = accumulatedResults.get(r.file_path);
        if (!existing || r.aggregated_score > existing.score) {
          accumulatedResults.set(r.file_path, {
            score: r.aggregated_score,
            content: r.content || "",
          });
        }
      }

      const newResults = new Map(accumulatedResults);
      setSearchResults(() => newResults);
      setState("search_results");
      break;
    }

    case "explore_synthesis_segment": {
      const segment: SynthesisSegment = {
        segmentIndex: data.segment_index ?? 0,
        totalSegments: data.total_segments ?? 0,
        explanation: data.explanation ?? "",
        citedDocuments: (data.cited_documents ?? []).map((d: any) => ({
          filePath: d.file_path ?? "",
          relevanceNote: d.relevance_note ?? "",
          excerpt: d.excerpt ?? "",
          heading: d.heading ?? "",
          blockRef: d.block_ref ?? "",
        })),
        theme: data.theme ?? "",
      };

      setSynthesisSegments((prev) => [...prev, segment]);

      const progress = segment.totalSegments > 0
        ? `Synthesizing ${segment.segmentIndex + 1}/${segment.totalSegments}...`
        : `Synthesizing segment ${segment.segmentIndex + 1}...`;
      setStatusText(progress);
      break;
    }

    case "explore_synthesis":
      setSynthesisText(data.text || "");
      setStatusText("Synthesis complete");
      break;

    case "explore_complete":
      setStatusText("");
      break;
  }
}
