// SynthesisPanel: overlay panel for synthesis segments or plain text

import { useState, useEffect, useRef } from "react";
import type { SynthesisSegment } from "./types";

type SaveState = "idle" | "saving" | "saved" | "error";

interface SynthesisPanelProps {
  text: string;
  segments: SynthesisSegment[];
  visible: boolean;
  onDismiss?: () => void;
  onDocumentClick?: (filePath: string) => void;
  onDocumentHover?: (filePath: string) => void;
  onDocumentHoverOut?: () => void;
  anchoredPaths?: Set<string>;
  onClearAnchored?: () => void;
  width?: number;
  isLoading?: boolean;
  onSaveNote?: () => void;
  saveState?: SaveState;
  dimmed?: boolean;
}

function SkeletonSegment({ index }: { index: number }) {
  return (
    <div
      className="mb-5 last:mb-0 animate-pulse"
      style={{ animationDelay: `${index * 150}ms` }}
    >
      {/* Theme badge skeleton */}
      <div className="inline-block px-2 py-0.5 mb-2 rounded-md bg-white/6 h-4 w-20" />
      {/* Text lines */}
      <div className="space-y-2 mb-3">
        <div className="h-3 bg-white/5 rounded w-full" />
        <div className="h-3 bg-white/5 rounded w-4/5" />
        <div className="h-3 bg-white/5 rounded w-3/5" />
      </div>
      {/* Doc chip placeholders */}
      <div className="flex gap-1.5">
        <div className="h-6 bg-white/4 rounded-md w-24" />
        <div className="h-6 bg-white/4 rounded-md w-20" />
      </div>
    </div>
  );
}

/** Build an Obsidian-style embed reference from file path + optional heading/block ref. */
function buildEmbedRef(filePath: string, heading?: string, blockRef?: string): string {
  // Strip .md extension for Obsidian-style refs
  const notePath = filePath.replace(/\.md$/, "");
  if (blockRef) {
    // Block reference takes priority — direct embed of a specific block
    const clean = blockRef.startsWith("^") ? blockRef : `^${blockRef}`;
    return `![[${notePath}#${clean}]]`;
  }
  if (heading) {
    // Strip leading # and whitespace from heading
    const clean = heading.replace(/^#+\s*/, "");
    return `![[${notePath}#${clean}]]`;
  }
  return `![[${notePath}]]`;
}

/** Save note button with 4 visual states: idle, saving, saved, error. */
function SaveNoteButton({
  onClick,
  state = "idle",
}: {
  onClick?: () => void;
  state?: SaveState;
}) {
  const disabled = state === "saving" || state === "saved";

  const label =
    state === "saving"
      ? "Saving"
      : state === "saved"
        ? "Saved"
        : state === "error"
          ? "Error"
          : "Save";

  const colorClasses =
    state === "saved"
      ? "text-emerald-400/70 bg-emerald-500/10 border-emerald-500/20"
      : state === "error"
        ? "text-red-400/70 bg-red-500/10 border-red-500/20 hover:bg-red-500/15 hover:text-red-400/90"
        : state === "saving"
          ? "text-white/30 bg-white/5 border-white/8"
          : "text-white/40 bg-white/5 border-white/8 hover:bg-white/10 hover:text-white/60";

  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className={`flex items-center gap-1 px-1.5 py-0.5 rounded-md text-[10px] border transition-colors ${colorClasses} ${disabled ? "cursor-default" : "cursor-pointer"}`}
      title={
        state === "error"
          ? "Save failed — click to retry"
          : state === "saved"
            ? "Note saved to vault"
            : "Save as note in vault"
      }
    >
      {state === "saving" ? (
        /* Spinner */
        <svg
          width="10"
          height="10"
          viewBox="0 0 16 16"
          fill="none"
          className="animate-spin"
        >
          <circle
            cx="8"
            cy="8"
            r="6"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeDasharray="28"
            strokeDashoffset="8"
            strokeLinecap="round"
          />
        </svg>
      ) : state === "saved" ? (
        /* Checkmark */
        <svg width="10" height="10" viewBox="0 0 16 16" fill="none">
          <path
            d="M3 8.5L6.5 12L13 4"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      ) : state === "error" ? (
        /* Warning */
        <svg width="10" height="10" viewBox="0 0 16 16" fill="none">
          <path
            d="M8 4v5M8 11v1"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
        </svg>
      ) : (
        /* Save / download icon */
        <svg width="10" height="10" viewBox="0 0 16 16" fill="none">
          <path
            d="M3 13h10M8 3v7M5 7l3 3 3-3"
            stroke="currentColor"
            strokeWidth="1.2"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      )}
      <span>{label}</span>
    </button>
  );
}

/** Chars per tick / tick interval for streaming text animation. */
const STREAM_CHARS_PER_TICK = 3;
const STREAM_TICK_MS = 10;

function SegmentBlock({
  segment,
  onDocumentClick,
  onDocumentHover,
  onDocumentHoverOut,
  anchoredPaths,
}: {
  segment: SynthesisSegment;
  onDocumentClick?: (filePath: string) => void;
  onDocumentHover?: (filePath: string) => void;
  onDocumentHoverOut?: () => void;
  anchoredPaths?: Set<string>;
}) {
  const fullText = segment.explanation;
  const [displayedLen, setDisplayedLen] = useState(0);
  const [textComplete, setTextComplete] = useState(false);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    if (!fullText) {
      setTextComplete(true);
      return;
    }

    // Reset for this segment's text
    setDisplayedLen(0);
    setTextComplete(false);

    intervalRef.current = setInterval(() => {
      setDisplayedLen((prev) => {
        const next = Math.min(prev + STREAM_CHARS_PER_TICK, fullText.length);
        if (next >= fullText.length) {
          clearInterval(intervalRef.current!);
          intervalRef.current = null;
          setTextComplete(true);
        }
        return next;
      });
    }, STREAM_TICK_MS);

    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [fullText]);

  return (
    <div
      className="mb-5 last:mb-0"
      style={{
        animation: "fadeSlideIn 0.35s cubic-bezier(0.16, 1, 0.3, 1) both",
        animationDelay: `${segment.segmentIndex * 120}ms`,
      }}
    >
      {/* Theme label */}
      <div className="inline-block px-2 py-0.5 mb-2 rounded-md bg-white/8 border border-white/10 text-[11px] font-medium text-white/50 uppercase tracking-wider">
        {segment.theme}
      </div>

      {/* Explanation — streaming text */}
      <div className="text-sm text-white/80 leading-relaxed whitespace-pre-wrap mb-3">
        {fullText.slice(0, displayedLen)}
        {!textComplete && (
          <span className="inline-block w-[2px] h-[1em] bg-white/40 align-text-bottom ml-px animate-pulse" />
        )}
      </div>

      {/* Cited documents — stagger in one-by-one after text finishes */}
      {textComplete && segment.citedDocuments.length > 0 && (
        <div className="flex flex-col gap-1.5">
          {segment.citedDocuments.map((doc, i) => {
            const isAnchored = anchoredPaths?.has(doc.filePath) ?? false;
            const hasExcerpt = !!doc.excerpt;
            const embedRef = buildEmbedRef(doc.filePath, doc.heading, doc.blockRef);

            return (
              <div
                key={`${doc.filePath}-${i}`}
                onClick={() => onDocumentClick?.(doc.filePath)}
                onMouseEnter={() => onDocumentHover?.(doc.filePath)}
                onMouseLeave={() => onDocumentHoverOut?.()}
                style={{
                  animation: "fadeSlideIn 0.3s cubic-bezier(0.16, 1, 0.3, 1) both",
                  animationDelay: `${i * 80}ms`,
                }}
                className={`group rounded-md border transition-colors duration-150 cursor-pointer ${
                  isAnchored
                    ? "bg-white/10 border-white/20"
                    : "bg-white/5 border-white/8 hover:bg-white/8 hover:border-white/15"
                }`}
              >
                {/* Filename row */}
                <div className="flex items-center gap-1.5 px-2.5 py-1.5">
                  <svg
                    width="10"
                    height="10"
                    viewBox="0 0 16 16"
                    fill="none"
                    className={`transition-colors flex-shrink-0 ${
                      isAnchored
                        ? "text-white/60"
                        : "text-white/30 group-hover:text-white/50"
                    }`}
                  >
                    <path
                      d="M4 2h5.5L13 5.5V13a1 1 0 01-1 1H4a1 1 0 01-1-1V3a1 1 0 011-1z"
                      stroke="currentColor"
                      strokeWidth="1.2"
                    />
                  </svg>
                  <span
                    className={`truncate text-[11px] transition-colors ${
                      isAnchored
                        ? "text-white/80"
                        : "text-white/50 group-hover:text-white/70"
                    }`}
                  >
                    {doc.filePath.split("/").pop() || doc.filePath}
                  </span>
                  {hasExcerpt && !isAnchored && (
                    <svg
                      width="10"
                      height="10"
                      viewBox="0 0 12 12"
                      fill="none"
                      className="ml-auto flex-shrink-0 text-white/0 group-hover:text-white/25 transition-colors"
                    >
                      <path
                        d="M3 4.5L6 7.5L9 4.5"
                        stroke="currentColor"
                        strokeWidth="1.2"
                        strokeLinecap="round"
                        strokeLinejoin="round"
                      />
                    </svg>
                  )}
                </div>

                {/* Expandable excerpt — inside the chip */}
                {hasExcerpt && (
                  <div
                    className="grid transition-[grid-template-rows] duration-200 ease-out"
                    style={{
                      gridTemplateRows: isAnchored ? "1fr" : "0fr",
                    }}
                  >
                    <div className="overflow-hidden min-h-0">
                      <div className="px-2.5 pb-2 pt-0.5">
                        <div className="text-[10px] text-white/30 font-mono mb-1 truncate">
                          {embedRef}
                        </div>
                        <div className="text-[12px] text-white/60 leading-relaxed line-clamp-4">
                          {doc.excerpt}
                        </div>
                      </div>
                    </div>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function SynthesisPanel({
  text,
  segments,
  visible,
  onDismiss,
  onDocumentClick,
  onDocumentHover,
  onDocumentHoverOut,
  anchoredPaths,
  onClearAnchored,
  width,
  isLoading,
  onSaveNote,
  saveState,
  dimmed,
}: SynthesisPanelProps) {
  if (!visible) return null;

  const hasSegments = segments.length > 0;
  const hasAnchored = anchoredPaths && anchoredPaths.size > 0;

  return (
    <div
      className="absolute right-4 top-16 bottom-4 pointer-events-auto overflow-hidden rounded-xl border border-white/10 bg-[#0e0e18]/90 backdrop-blur-xl shadow-2xl"
      style={{
        width: width ?? 384,
        animation: "slideInRight 0.4s cubic-bezier(0.16, 1, 0.3, 1)",
      }}
    >
      <div
        className={`p-5 h-full flex flex-col transition-opacity duration-300 ${
          dimmed ? "opacity-40 pointer-events-none" : ""
        }`}
      >
        <div className="flex items-center justify-between mb-3">
          <div className="flex items-center gap-2">
            <h3 className="text-sm font-medium text-white/50 uppercase tracking-wider">
              Synthesis
            </h3>
            {hasAnchored && (
              <button
                onClick={onClearAnchored}
                className="flex items-center gap-1 px-1.5 py-0.5 rounded-md text-[10px] text-white/40 bg-white/5 border border-white/8 hover:bg-white/10 hover:text-white/60 transition-colors"
                title="Clear selected documents"
              >
                <span>{anchoredPaths!.size} selected</span>
                <svg
                  width="10"
                  height="10"
                  viewBox="0 0 16 16"
                  fill="none"
                >
                  <path
                    d="M4 4L12 12M4 12L12 4"
                    stroke="currentColor"
                    strokeWidth="1.5"
                    strokeLinecap="round"
                  />
                </svg>
              </button>
            )}
            {onSaveNote && !isLoading && segments.length > 0 && (
              <SaveNoteButton onClick={onSaveNote} state={saveState} />
            )}
          </div>
          {onDismiss && (
            <button
              onClick={onDismiss}
              className="text-white/50 hover:text-white/80 transition-colors p-1.5 -mr-1.5 rounded-lg hover:bg-white/5"
              title="Dismiss synthesis"
            >
              <svg
                width="16"
                height="16"
                viewBox="0 0 16 16"
                fill="none"
                xmlns="http://www.w3.org/2000/svg"
              >
                <path
                  d="M2 2L14 14M2 14L14 2"
                  stroke="currentColor"
                  strokeWidth="1.5"
                  strokeLinecap="round"
                />
              </svg>
            </button>
          )}
        </div>
        <div className="flex-1 overflow-y-auto pr-2">
          {hasSegments ? (
            segments.map((seg) => (
              <SegmentBlock
                key={seg.segmentIndex}
                segment={seg}
                onDocumentClick={onDocumentClick}
                onDocumentHover={onDocumentHover}
                onDocumentHoverOut={onDocumentHoverOut}
                anchoredPaths={anchoredPaths}
              />
            ))
          ) : isLoading ? (
            <>
              <SkeletonSegment index={0} />
              <SkeletonSegment index={1} />
              <SkeletonSegment index={2} />
            </>
          ) : text ? (
            <div className="text-sm text-white/80 leading-relaxed whitespace-pre-wrap">
              {text}
            </div>
          ) : null}
        </div>
      </div>

      <style>{`
        @keyframes fadeSlideIn {
          from {
            opacity: 0;
            transform: translateY(8px);
          }
          to {
            opacity: 1;
            transform: translateY(0);
          }
        }
      `}</style>
    </div>
  );
}
