// SteeringChips: catalyst topic chips for prompt steering

interface SteeringChipsProps {
  suggestions: Array<{ topic_name: string; bent_prompt: string }>;
  onSelectPrompt: (bentPrompt: string) => void;
  visible: boolean;
}

export function SteeringChips({
  suggestions,
  onSelectPrompt,
  visible,
}: SteeringChipsProps) {
  if (!visible || suggestions.length === 0) return null;

  return (
    <div
      className="flex flex-col items-center gap-1.5 mt-3"
      style={{
        animation: "fadeSlideIn 0.35s cubic-bezier(0.16, 1, 0.3, 1) both",
      }}
    >
      {/* Hint label */}
      <span className="text-[10px] text-white/50 uppercase tracking-widest">
        Shift perspective
      </span>

      {/* Chips */}
      <div className="flex gap-2 flex-wrap justify-center">
        {suggestions.map((s, i) => (
          <button
            key={s.topic_name}
            onClick={() => onSelectPrompt(s.bent_prompt)}
            className="group flex items-center gap-1.5 px-3 py-1.5 rounded-full
                       bg-white/[0.07] border border-white/[0.14]
                       text-xs text-white/65
                       hover:text-white/90 hover:bg-white/[0.12] hover:border-white/25
                       transition-all duration-200 cursor-pointer"
            style={{
              animation:
                "fadeSlideIn 0.35s cubic-bezier(0.16, 1, 0.3, 1) both",
              animationDelay: `${(i + 1) * 100}ms`,
            }}
            title={s.bent_prompt}
          >
            {/* Directional arrow icon */}
            <svg
              width="10"
              height="10"
              viewBox="0 0 16 16"
              fill="none"
              className="text-white/45 group-hover:text-white/70 transition-colors flex-shrink-0"
            >
              <path
                d="M3 8h10M9 4l4 4-4 4"
                stroke="currentColor"
                strokeWidth="1.5"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
            <span>{s.topic_name}</span>
          </button>
        ))}
      </div>
    </div>
  );
}
