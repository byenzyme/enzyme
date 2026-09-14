// ExplorePage: query input + canvas + synthesis panel

import { useState, useCallback, useRef, useEffect, useLayoutEffect, useMemo } from "react";
import { useExploreCanvas } from "./hooks/useExploreCanvas";
import { ExploreCanvas } from "./components/explore/ExploreCanvas";
import { SynthesisPanel } from "./components/explore/SynthesisPanel";
import { SteeringChips } from "./components/explore/SteeringChips";
import type { SynthesisSegment } from "./components/explore/types";
import * as api from "./lib/api";

/** Build an Obsidian-style embed reference from file path + optional heading/block ref. */
function buildEmbedRef(filePath: string, heading?: string, blockRef?: string): string {
  const notePath = filePath.replace(/\.md$/, "");
  if (blockRef) {
    const clean = blockRef.startsWith("^") ? blockRef : `^${blockRef}`;
    return `![[${notePath}#${clean}]]`;
  }
  if (heading) {
    const clean = heading.replace(/^#+\s*/, "");
    return `![[${notePath}#${clean}]]`;
  }
  return `![[${notePath}]]`;
}

/** Build markdown content from exploration query + synthesis segments. */
function buildSynthesisMarkdown(query: string, segments: SynthesisSegment[]): string {
  const lines: string[] = [];
  lines.push(`# ${query}`);
  lines.push("");

  for (const seg of segments) {
    lines.push(`## ${seg.theme}`);
    lines.push("");
    if (seg.explanation) {
      lines.push(seg.explanation);
      lines.push("");
    }
    for (const doc of seg.citedDocuments) {
      lines.push(buildEmbedRef(doc.filePath, doc.heading, doc.blockRef));
      lines.push("");
    }
  }

  lines.push("---");
  const date = new Date().toLocaleDateString("en-US", {
    year: "numeric",
    month: "long",
    day: "numeric",
  });
  lines.push(`*Explored with Enzyme — ${date}*`);
  lines.push("");

  return lines.join("\n");
}

export function ExplorePage() {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const submittedQueryRef = useRef<string>("");
  const [anchoredPaths, setAnchoredPaths] = useState<Set<string>>(new Set());
  const [hoveredSourcePath, setHoveredSourcePath] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<"idle" | "saving" | "saved" | "error">("idle");
  const [query, setQuery] = useState("");

  // Auto-resize textarea height to fit content.
  const typewriterActiveRef = useRef(false);
  const [typewriterLock, setTypewriterLock] = useState<{ width: number; height: number } | null>(null);

  const autoResize = useCallback(() => {
    const el = textareaRef.current;
    if (!el || typewriterActiveRef.current) return;
    el.style.height = "0";
    el.style.height = Math.max(el.scrollHeight, 38) + "px";
  }, []);

  useLayoutEffect(() => {
    autoResize();
  }, [query, autoResize]);

  useEffect(() => {
    requestAnimationFrame(() => autoResize());
  }, [autoResize]);

  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    const handler = (e: TransitionEvent) => {
      if (e.propertyName === "width") autoResize();
    };
    el.addEventListener("transitionend", handler);
    return () => el.removeEventListener("transitionend", handler);
  }, [autoResize]);

  const {
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
    originalQuery,
  } = useExploreCanvas(query);

  // Typewriter animation for steering chip selection
  const typewriterRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const cancelTypewriter = useCallback(() => {
    if (typewriterRef.current) {
      clearInterval(typewriterRef.current);
      typewriterRef.current = null;
    }
    typewriterActiveRef.current = false;
    setTypewriterLock(null);
  }, []);

  useEffect(() => () => cancelTypewriter(), [cancelTypewriter]);

  const handleSteerSelect = useCallback(
    (bentPrompt: string) => {
      cancelTypewriter();

      const el = textareaRef.current;
      if (el) {
        typewriterActiveRef.current = true;
        setTypewriterLock({
          width: el.offsetWidth,
          height: el.offsetHeight,
        });
      }

      setQuery("");
      previewBentPrompt(bentPrompt);

      let pos = 0;
      typewriterRef.current = setInterval(() => {
        pos += 2;
        setQuery(bentPrompt.slice(0, pos));
        if (pos >= bentPrompt.length) {
          clearInterval(typewriterRef.current!);
          typewriterRef.current = null;
          typewriterActiveRef.current = false;
          setTypewriterLock(null);
        }
      }, 15);
    },
    [cancelTypewriter, previewBentPrompt]
  );

  const handleReset = useCallback(() => {
    cancelTypewriter();
    setQuery(originalQuery);
    resetToOriginal();
  }, [cancelTypewriter, originalQuery, resetToOriginal]);

  // Responsive panel width
  const [panelWidth, setPanelWidth] = useState(() => {
    const w = window.innerWidth;
    if (w < 900) return Math.min(320, Math.round(w * 0.4));
    if (w <= 1400) return 384;
    return Math.min(480, Math.round(w * 0.3));
  });

  useEffect(() => {
    const onResize = () => {
      const w = window.innerWidth;
      if (w < 900) setPanelWidth(Math.min(320, Math.round(w * 0.4)));
      else if (w <= 1400) setPanelWidth(384);
      else setPanelWidth(Math.min(480, Math.round(w * 0.3)));
    };
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  const synthesisActive = state === "synthesis_complete";
  const panelInset = synthesisActive ? panelWidth + 32 : 0;
  const showSynthesis = synthesisActive;
  const synthesisLoading = synthesisActive && synthesisSegments.length === 0 && !synthesisText;

  const sourceExpandedPaths = useMemo(() => {
    const paths = new Set(anchoredPaths);
    if (hoveredSourcePath) paths.add(hoveredSourcePath);
    return paths.size > 0 ? paths : undefined;
  }, [anchoredPaths, hoveredSourcePath]);

  // Zoom function refs exposed by ExploreCanvas
  const fitRef = useRef<(() => void) | null>(null);
  const zoomToFileRef = useRef<((filePath: string) => void) | null>(null);
  const zoomToFilesRef = useRef<((paths: Set<string>) => void) | null>(null);
  const zoomToRelevantRef = useRef<(() => void) | null>(null);

  const doExplore = useCallback(() => {
    if (query.trim() && !isExploring) {
      submittedQueryRef.current = query.trim();
      setAnchoredPaths(new Set());
      setSaveState("idle");
      explore(query.trim());
    }
  }, [query, isExploring, explore]);

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      doExplore();
    },
    [doExplore]
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        doExplore();
      }
    },
    [doExplore]
  );

  const handleFitToContent = useCallback((fit: () => void) => {
    fitRef.current = fit;
  }, []);

  const handleExposeZoomToFile = useCallback(
    (fn: (filePath: string) => void) => {
      zoomToFileRef.current = fn;
    },
    []
  );

  const handleExposeZoomToFilePaths = useCallback(
    (fn: (paths: Set<string>) => void) => {
      zoomToFilesRef.current = fn;
    },
    []
  );

  const handleExposeZoomToRelevant = useCallback(
    (fn: () => void) => {
      zoomToRelevantRef.current = fn;
    },
    []
  );

  const handleDocumentHover = useCallback((filePath: string) => {
    setHoveredSourcePath(filePath);
    zoomToFileRef.current?.(filePath);
  }, []);

  const handleDocumentHoverOut = useCallback(() => {
    setHoveredSourcePath(null);
    if (anchoredPaths.size > 0) {
      if (anchoredPaths.size === 1) {
        zoomToFileRef.current?.([...anchoredPaths][0]);
      } else {
        zoomToFilesRef.current?.(anchoredPaths);
      }
    } else {
      zoomToRelevantRef.current?.();
    }
  }, [anchoredPaths]);

  const handleDocumentClick = useCallback((filePath: string) => {
    setAnchoredPaths((prev) => {
      const next = new Set(prev);
      if (next.has(filePath)) {
        next.delete(filePath);
      } else {
        next.add(filePath);
      }
      if (next.size === 0) {
        zoomToRelevantRef.current?.();
      } else if (next.size === 1) {
        zoomToFileRef.current?.([...next][0]);
      } else {
        zoomToFilesRef.current?.(next);
      }
      return next;
    });
  }, []);

  const handleClearAnchored = useCallback(() => {
    setAnchoredPaths(new Set());
    zoomToRelevantRef.current?.();
  }, []);

  const handleDismiss = useCallback(() => {
    setAnchoredPaths(new Set());
    setHoveredSourcePath(null);
    clearSynthesis();
  }, [clearSynthesis]);

  const handleSaveNote = useCallback(async () => {
    if (synthesisSegments.length === 0) return;

    setSaveState("saving");
    try {
      const queryText = submittedQueryRef.current || "Exploration";
      const markdown = buildSynthesisMarkdown(queryText, synthesisSegments);

      const result = await api.saveSynthesisNote(queryText, markdown);

      if (!result.success) {
        throw new Error(result.error || "Save failed");
      }

      setSaveState("saved");
      const timer = setTimeout(() => setSaveState("idle"), 3000);
      return () => clearTimeout(timer);
    } catch (err) {
      console.error("[ExplorePage] Save note failed:", err);
      setSaveState("error");
      setTimeout(() => setSaveState("idle"), 4000);
    }
  }, [synthesisSegments]);

  const showQueryCentered = state === "idle" || state === "loading";
  const isStale = !showQueryCentered && query.trim() === submittedQueryRef.current;

  return (
    <div className="relative w-full h-full bg-[#0a0a0f] overflow-hidden">
      {/* Canvas */}
      <ExploreCanvas
        nodes={nodes}
        state={state}
        searchResults={searchResults}
        relevantPaths={relevantPaths}
        sourceExpandedPaths={sourceExpandedPaths}
        onFitToContent={handleFitToContent}
        onExposeZoomToFile={handleExposeZoomToFile}
        onExposeZoomToFilePaths={handleExposeZoomToFilePaths}
        onExposeZoomToRelevant={handleExposeZoomToRelevant}
        onClearSynthesis={handleDismiss}
        onDebugTriggerSynthesis={debugTriggerSynthesis}
        panelInset={panelInset}
      />

      {/* Query input */}
      <div
        className="absolute z-10"
        style={{
          top: "1rem",
          right: "calc(50% - 240px)",
          transform: showQueryCentered
            ? "translateY(calc(50vh - 50% - 1rem))"
            : "translateY(0)",
          transition: "transform 500ms cubic-bezier(0.16, 1, 0.3, 1)",
          willChange: "transform",
        }}
      >
        <form onSubmit={handleSubmit} className="flex items-center gap-2">
          {/* Reset button */}
          {isSteeredPreview && (
            <button
              type="button"
              onClick={handleReset}
              className="p-2 rounded-lg bg-white/8 border border-white/15 text-white/60 hover:bg-white/12 hover:text-white/85 transition-all flex-shrink-0"
              title="Reset to original prompt"
              style={{
                animation: "fadeSlideIn 0.3s cubic-bezier(0.16, 1, 0.3, 1) both",
              }}
            >
              <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
                <path d="M2 8a6 6 0 1 1 1.76 4.24" />
                <path d="M2 12V8h4" />
              </svg>
            </button>
          )}
          <textarea
            ref={textareaRef}
            value={query}
            onChange={(e) => {
              cancelTypewriter();
              setQuery(e.target.value);
            }}
            onKeyDown={handleKeyDown}
            placeholder="What are you exploring?"
            disabled={isExploring}
            rows={1}
            style={
              typewriterLock
                ? { width: typewriterLock.width, height: typewriterLock.height }
                : undefined
            }
            className={`${
              typewriterLock
                ? ""
                : query.length > 40
                  ? "w-[600px]"
                  : showQueryCentered
                    ? "w-96"
                    : "w-80"
            } px-4 py-2.5 rounded-xl border ${
              isStale
                ? "bg-white/5 border-white/10 text-white/55 placeholder-white/30"
                : "bg-white/8 border-white/16 text-white/95 placeholder-white/50"
            } outline-none focus:border-white/30 focus:bg-white/12 transition-[width,background-color,border-color,color] duration-300 backdrop-blur-sm text-sm resize-none overflow-hidden leading-relaxed animate-search-glow`}
          />
          <button
            type="submit"
            disabled={isExploring || !query.trim()}
            className={`px-4 py-2.5 rounded-xl border text-sm disabled:opacity-30 disabled:cursor-not-allowed transition-all flex-shrink-0 ${
              isStale
                ? "bg-white/6 border-white/10 text-white/45 hover:bg-white/10 hover:text-white/65"
                : "bg-white/12 border-white/18 text-white/85 hover:bg-white/16 hover:text-white"
            }`}
          >
            {isExploring ? "..." : "Explore"}
          </button>
        </form>

        {/* Status text */}
        {isExploring && !showSynthesis && statusText && (
          <div
            key={statusText}
            className="text-center text-white/60 text-xs mt-2 animate-fade-in"
          >
            {statusText}
          </div>
        )}

        {/* Steering chips */}
        <SteeringChips
          suggestions={steeringSuggestions}
          onSelectPrompt={handleSteerSelect}
          visible={!showQueryCentered && steeringSuggestions.length > 0}
        />
      </div>

      {/* Fit-to-content button */}
      {nodes.length > 0 && (
        <button
          onClick={() => fitRef.current?.()}
          className="absolute bottom-4 right-4 z-10 px-3 py-1.5 rounded-lg bg-white/8 border border-white/15 text-white/60 text-xs hover:bg-white/12 hover:text-white/85 transition-all"
          title="Fit all nodes in view"
        >
          Fit
        </button>
      )}

      {/* Synthesis panel */}
      <SynthesisPanel
        text={synthesisText}
        segments={synthesisSegments}
        visible={showSynthesis}
        onDismiss={handleDismiss}
        onDocumentClick={handleDocumentClick}
        onDocumentHover={handleDocumentHover}
        onDocumentHoverOut={handleDocumentHoverOut}
        anchoredPaths={anchoredPaths}
        onClearAnchored={handleClearAnchored}
        width={panelWidth}
        isLoading={synthesisLoading}
        onSaveNote={handleSaveNote}
        saveState={saveState}
        dimmed={isSteeredPreview}
      />
    </div>
  );
}
