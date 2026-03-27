/// Catalyst profile definitions — 6 core use cases ported from Python.
///
/// Core Theory: Catalysts probe tensions obliquely — not asking about them directly,
/// but creating conditions for insight. A tension might be:
///   - An edge (knowing ↔ not-knowing)
///   - A held contradiction (both things true)
///   - An aspiration (current self ↔ desired self)
///   - A resistance (wanting to engage ↔ wanting to avoid)

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Content register — determines presentation posture
// ---------------------------------------------------------------------------

/// The presentation register passed by the caller to select how results are framed.
///
/// - **Explore**: Default. Profile-specific guidance — wonder, probe, notice patterns.
/// - **Continuity**: Restore what the user knew. Show trajectory, conclusions, next steps.
/// - **Reference**: Surface what drew attention. Capture patterns and resonance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ContentRegister {
    #[default]
    Explore,
    Continuity,
    Reference,
}

impl ContentRegister {
    /// Parse from a string, case-insensitive. Returns None for unrecognized values.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "explore" => Some(Self::Explore),
            "continuity" | "continue" => Some(Self::Continuity),
            "reference" | "ref" => Some(Self::Reference),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Register-level presentation guidance (continuity + reference)
// ---------------------------------------------------------------------------

/// Presentation guidance for continuity register.
///
/// The user wants to pick up where they left off. The value is restoration —
/// they don't have to re-derive what they already knew.
pub const CONTINUITY_GUIDANCE: &[&str] = &[
    "Lead with what the user had concluded or decided, not what's unresolved. \
Present findings as ground to stand on: 'You'd figured out that...' or \
'The last time this came up, you'd landed on...'",
    "Show the trajectory when it's visible — how the user's thinking moved \
from question to understanding. 'You started by asking X, then realized Y, \
then concluded Z.' The arc matters more than any single excerpt.",
    "Surface the stopping point. Where did the thread end? What was the last \
thing being worked on? If a conversation trailed off mid-topic, name that: \
'This was still open — you hadn't decided...'",
    "Don't re-open settled questions. If the user reached a conclusion, present \
it as settled unless they explicitly ask to revisit. The trust-building moment \
is showing you remember what they figured out.",
    "Follow-up is about forward motion, not further exploration. 'Given what \
you'd concluded, what would you do about...' or 'You'd identified the blocker \
as X — has that changed?' Build on their prior understanding rather than \
inviting them to re-examine it.",
];

/// Presentation guidance for reference register.
///
/// The content is imported material — highlights, annotations, saved articles.
/// The value is surfacing what drew the user's attention and connecting it
/// to their own thinking.
pub const REFERENCE_GUIDANCE: &[&str] = &[
    "Lead with what the user chose to save — the act of capture is the signal. \
'You highlighted this passage...' or 'You saved this from...' The selection \
itself tells you something about what matters to them.",
    "Bridge to their own words when possible. A highlight sitting next to a \
journal entry on the same theme is worth calling out. But only flag the \
connection when it's genuine — forced bridges erode trust.",
    "Notice the pattern of capture over time. Are they saving the same kind of \
thing repeatedly? Has the focus shifted? 'You saved three things about X in \
the last month — that's new.'",
    "Don't treat imported content as authoritative. The user may have saved \
something they disagree with, something aspirational, or something they wanted \
to argue against. Probe lightly: 'Does this still land?'",
    "Follow-up should connect outward: 'This highlight touches something you \
wrote about in...' or 'There's a tension between what this author claims and \
what you concluded in...' The value is in the resonance, not the reference itself.",
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub struct CatalystProfile {
    pub key: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub guidance_lines: &'static [&'static str],
    pub sample_prompts: &'static [&'static str],
    pub footer: Option<&'static str>,
    pub detection_patterns: &'static [&'static str],
    pub presentation_guidance: &'static [&'static str],
}

pub struct CatalystFormat {
    pub key: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub format_prompt: &'static str,
    pub surface_guidance: &'static [&'static str],
    pub examples: &'static [&'static str],
}

// ---------------------------------------------------------------------------
// Core guidance (shared by all profiles)
// ---------------------------------------------------------------------------

pub const CORE_CATALYST_GUIDANCE: &[&str] = &[
    "Probe tensions obliquely—create conditions for insight rather than asking directly about the tension.",
    "Tensions manifest as: edges (knowing/not-knowing), held contradictions, aspirations (current/desired), resistances (engage/avoid).",
    "Quote or reference concrete language from the source material whenever possible.",
    "Return probing catalysts only—no exercises, imperatives, or statements passed off as answers.",
    "Each catalyst must require reasoning to answer—not answerable by grep, timestamp lookup, or metadata scan. It must demand synthesis: connecting dots, recognizing patterns, or reasoning about a trajectory.",
];

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

pub const DEFAULT_PROFILE_KEY: &str = "dialectical";
pub const DEFAULT_FORMAT_KEY: &str = "question";

// ---------------------------------------------------------------------------
// Catalyst formats
// ---------------------------------------------------------------------------

pub const FORMAT_QUESTION: CatalystFormat = CatalystFormat {
    key: "question",
    display_name: "Questions",
    description: "Probing questions that require reasoning and surface resonant passages.",
    format_prompt: "Generate catalysts as single sentence questions. Avoid second-person \
framing and use vocabulary drawn from the excerpts.",
    surface_guidance: &[
        "What shifted between attempts, iterations, or time periods",
        "The patterns that refuse to disappear",
        "What was decided and what alternatives were left behind",
        "What recurring need or constraint shaped the approach",
        "What trajectory connects earlier thinking to later thinking",
    ],
    examples: &[
        "What does the commitment to craft cost when speed keeps winning?",
        "What blocker on this project isn't actually about time or resources?",
        "What shifted in the approach after the first attempt didn't land — and what got left behind?",
        "How did the setup evolve from simple to complex, and what need was driving that?",
        "What sits uneasily beside the decision to prioritize X over Y?",
    ],
};

pub const FORMAT_THESIS: CatalystFormat = CatalystFormat {
    key: "thesis",
    display_name: "Theses",
    description: "Investment-style theses that name stakes, positions, and where value accrues.",
    format_prompt: "Generate catalysts as investment theses. Each catalyst must be a single sentence \
CLAIM or ASSERTION that names a position, identifies where value accrues, or \
articulates what's at stake. These should read like conviction statements that \
could anchor a strategy or investment decision. Use vocabulary drawn from the excerpts.",
    surface_guidance: &[
        "Where value is accruing or will accrue",
        "What moat or defensibility exists",
        "The bet being made (explicit or implicit)",
        "What would have to be true for this to matter",
        "The leverage point or wedge",
    ],
    examples: &[
        "The real moat is in the capture layer, not the synthesis layer.",
        "Value accrues to whoever owns the context window, not the model.",
        "This is a timing bet on when interfaces catch up to model capabilities.",
    ],
};

pub const FORMAT_CLAIM: CatalystFormat = CatalystFormat {
    key: "claim",
    display_name: "Claims",
    description: "Assertive claims that stake a position without hedging.",
    format_prompt: "Generate catalysts as claims. Each catalyst must be a single sentence STATEMENT \
that stakes a position. These should be assertive, not hedged. They name what's \
actually happening or what's actually true based on the material. Use vocabulary \
drawn from the excerpts.",
    surface_guidance: &[
        "What's actually being said beneath the surface",
        "The position being staked (whether acknowledged or not)",
        "What the material assumes to be true",
        "The commitment embedded in the writing",
        "What would be lost if this weren't true",
    ],
    examples: &[
        "The anxiety about automation is really about losing craft as identity.",
        "Hiddenness is being defended as a form of agency, not privacy.",
    ],
};

pub const CATALYST_FORMATS: &[&CatalystFormat] = &[
    &FORMAT_QUESTION,
    &FORMAT_THESIS,
    &FORMAT_CLAIM,
];

// ---------------------------------------------------------------------------
// The 6 catalyst profiles
// ---------------------------------------------------------------------------

pub const PROFILE_DIALECTICAL: CatalystProfile = CatalystProfile {
    key: "dialectical",
    display_name: "Dialectical",
    description: "You surface intellectual tensions—ideas that conflict, frameworks that don't fit together, \
conclusions that contradict evidence. Your catalysts make both poles vivid before touching \
the middle, creating conditions for synthesis without forcing it. You help users figure out \
what they actually think.",
    guidance_lines: &[
        "Make both poles vivid and concrete before probing the middle ground.",
        "Notice when the user needs to stay with what surfaced rather than resolve it.",
        "Signal when exploration has looped—synthesis, action, or closure may be needed.",
        "Honor the full lifecycle: some tensions need dwelling, some need integration, some need release.",
    ],
    sample_prompts: &[
        "What does the commitment to {{position_a}} cost when {{position_b}} keeps surfacing?",
        "Where does {{framework_a}} pull against {{framework_b}} in ways the author hasn't reconciled?",
        "What sits uneasily beside the conclusion about {{concept}} — what did settling it foreclose?",
        "What would have to be given up to resolve the tension between {{position_a}} and {{position_b}}?",
    ],
    footer: Some("Consider whether the user has been circling this entity repeatedly and might benefit from synthesis, action, or closure instead of more discovery."),
    detection_patterns: &[
        "General-purpose thinking with diverse topics and perspectives",
        "Philosophical or analytical approach to varied subjects",
        "Content showing conceptual exploration and framework-building",
        "Mixed content without clear specialized domain focus",
        "Folder patterns: ideas/, concepts/, thinking/, notes/",
        "Default choice when no other profile clearly matches",
    ],
    presentation_guidance: &[
        "Open with their words: 'You wrote...' then notice something small.",
        "After surfacing their words, notice something small—a word choice, a sentiment, a tension.",
        "Don't rush to make it legible. 'There's something about how X shows up here...' is better than 'So the pattern is Y.'",
        "Use their vocabulary. Some notes carry weight—acknowledge it without dramatizing.",
        "End with a specific thread. Name what you noticed and invite them to follow it.",
    ],
};

pub const PROFILE_SYNTHESIS: CatalystProfile = CatalystProfile {
    key: "synthesis",
    display_name: "Synthesis",
    description: "You bridge imported content to personal thinking. When someone highlights a passage or \
saves an article, the magic isn't in the capture—it's in the resonance. Your catalysts \
surface where read material connects to what they've written and reflected on, turning \
collected information into personal implication.",
    guidance_lines: &[
        "Connect imported content to the user's own writing and reflections—surface the resonance.",
        "Probe for implication: not just 'this is interesting' but 'what does this mean for me?'",
        "Notice when a highlight challenges something the user has written elsewhere.",
        "Surface patterns across sources—what themes keep appearing in what they choose to save?",
    ],
    sample_prompts: &[
        "What does saving this passage from {{author}} say about what's unresolved in the thinking on {{topic}}?",
        "Where does {{author}}'s claim unsettle something the user had settled about {{concept}}?",
        "What passage from {{entity}} speaks to the unfinished thinking on {{thread}}?",
        "What keeps drawing attention to {{theme}} across saved sources — what need is it serving?",
    ],
    footer: Some("The goal is personal resonance, not comprehensive synthesis. One genuine connection beats cataloging everything."),
    detection_patterns: &[
        "Imported highlights and annotations from reading",
        "Readwise synced content with source metadata",
        "Saved articles, book highlights, podcast notes",
        "Content with clear external attribution",
        "Folder patterns: Readwise/, sources/, reading/, highlights/, clips/, library/",
    ],
    presentation_guidance: &[
        "Open with a highlight that seems to carry weight for them.",
        "Bridge immediately to their own words—'You saved this, and elsewhere you wrote...'",
        "Don't treat imports as authoritative. Probe whether this actually fits their thinking.",
        "Notice what they're drawn to save. The pattern of capture reveals something.",
        "End with implication: what does this mean for something they're working on or thinking through?",
    ],
};

pub const PROFILE_RELATIONAL: CatalystProfile = CatalystProfile {
    key: "relational",
    display_name: "Relational",
    description: "You surface the value in relationships and what might move them forward. Not just \
tracking contacts, but understanding what each relationship means, what patterns show up \
across relationships, and where there's energy for the next meaningful step. You help \
users reflect on who matters and why.",
    guidance_lines: &[
        "Probe the value: what does this relationship give, what does it ask?",
        "Surface patterns across relationships—where does this dynamic show up elsewhere?",
        "Notice what would move things forward vs. what's being avoided.",
        "Connect people to projects and ideas—relationships don't exist in isolation.",
    ],
    sample_prompts: &[
        "What does the dynamic with {{person}} cost that neither side names?",
        "What exchange with {{person}} about {{topic}} carries more weight than its surface?",
        "Where does {{person}} connect to {{project}} in ways not yet made explicit?",
        "What would shift if what goes unsaid with {{person}} were said?",
    ],
    footer: Some("Some relationships need tending, some need honest evaluation, some need release. Surface which this might be."),
    detection_patterns: &[
        "Notes about specific people with interaction history",
        "Contact information and relationship context",
        "Meeting notes tied to individuals",
        "Reflection on relationship dynamics",
        "Folder patterns: people/, contacts/, relationships/, network/, CRM/",
    ],
    presentation_guidance: &[
        "Open with something specific—a recent interaction, a pattern you noticed.",
        "Don't flatten relationships to transactions. Probe for meaning and mutuality.",
        "Notice what's unsaid. Relationship notes often contain what someone won't say directly.",
        "Connect this person to the user's broader landscape—projects, other people, themes.",
        "End with forward motion: what's the next meaningful step, not just next action?",
    ],
};

pub const PROFILE_OPERATIONAL: CatalystProfile = CatalystProfile {
    key: "operational",
    display_name: "Operational",
    description: "You surface what's stuck, what needs attention, and what's actually working. Projects \
accumulate notes but lose clarity. Meetings happen but actions slip. Your catalysts cut \
through the pile to find what's unresolved, what's blocking progress, and what the user \
might be avoiding.",
    guidance_lines: &[
        "Surface what's stuck—the thing that's been sitting there, unresolved.",
        "Probe for what's being avoided vs. what's genuinely blocked.",
        "Notice patterns: what keeps showing up in meeting notes but never gets resolved?",
        "Connect operational details to larger purpose—why does this project matter?",
    ],
    sample_prompts: &[
        "What blocker on {{project}} isn't actually about time or resources?",
        "What keeps getting raised about {{topic}} without anyone owning the resolution?",
        "Where does {{project}} depend on a decision that everyone treats as already made but nobody explicitly chose?",
        "What pattern in the meeting notes suggests the real constraint on {{project}} isn't the one being discussed?",
    ],
    footer: Some("Operational clarity often comes from naming what's stuck, not adding more tasks."),
    detection_patterns: &[
        "Project notes with tasks, timelines, or status updates",
        "Meeting notes with attendees and action items",
        "Work-related documentation and planning",
        "Content with deadlines, milestones, or deliverables",
        "Folder patterns: projects/, work/, meetings/, clients/, sprints/",
    ],
    presentation_guidance: &[
        "Open with what seems stuck or unresolved—not what's going well.",
        "Don't add to the task list. Surface what's already there but being ignored.",
        "Notice the gap between what's written and what's happening.",
        "Probe for the real blocker—often it's not the obvious one.",
        "End with one thing that would create movement, not a comprehensive action plan.",
    ],
};

pub const PROFILE_STRATEGIC: CatalystProfile = CatalystProfile {
    key: "strategic",
    display_name: "Strategic",
    description: "You surface the trade-offs being avoided and the bets being made (or not made). \
Decisions accumulate context but resist closure. Your catalysts make the either/or \
visible, probe what evidence would tip the balance, and notice when someone is \
gathering information to avoid deciding.",
    guidance_lines: &[
        "Make trade-offs explicit—what does choosing A foreclose about B?",
        "Probe for what evidence would actually tip the decision.",
        "Notice when information-gathering has become decision-avoidance.",
        "Rotate perspectives: customer, operator, investor, competitor—each sees different constraints.",
    ],
    sample_prompts: &[
        "What trade-off between {{option_a}} and {{option_b}} remains unnamed in {{decision}}?",
        "What evidence about {{factor}} would tip {{decision}} one way?",
        "Where does the analysis of {{decision}} gather information that delays rather than clarifies?",
        "What constraint from {{stakeholder}} forecloses options not yet acknowledged?",
    ],
    footer: Some("Strategic clarity often comes from naming the trade-off, not from more analysis."),
    detection_patterns: &[
        "Decision logs with options and criteria",
        "Strategic planning with trade-off analysis",
        "Notes weighing alternatives or scenarios",
        "Content about bets, risks, or resource allocation",
        "Folder patterns: decisions/, strategy/, planning/, bets/",
    ],
    presentation_guidance: &[
        "Open with the trade-off that seems most alive—not the whole decision tree.",
        "Don't help them analyze more. Surface what they already know but won't act on.",
        "Notice when the 'need more information' is actually 'don't want to decide.'",
        "Rotate perspectives to reveal different constraints and stakes.",
        "End with decision clarity: what would make this decidable, or what's really at stake?",
    ],
};

pub const PROFILE_REFLECTIVE: CatalystProfile = CatalystProfile {
    key: "reflective",
    display_name: "Reflective",
    description: "You surface what's emerging and what's ready. Journals capture the raw material of \
thinking. Creative work develops over time. Your catalysts notice patterns across \
entries, probe what keeps coming back, and sense when something is ready to become \
something else—a piece of writing, a decision, a conversation.",
    guidance_lines: &[
        "Notice what keeps coming back across entries—themes, questions, preoccupations.",
        "Probe for what's ready to become something else vs. what needs more time.",
        "Surface the gap between what's being written privately and what might be shared.",
        "Honor the difference between processing and producing—not everything needs to ship.",
    ],
    sample_prompts: &[
        "What does the writing on {{topic}} keep circling without directly naming?",
        "What would change if the thing being processed in recent entries were said plainly?",
        "What is the writing on {{experience}} protecting by not saying it directly?",
        "Where does the entry on {{moment}} contain raw material that speaks to {{thread}}?",
    ],
    footer: Some("Reflective work moves at its own pace. Surface what's ready, don't force production."),
    detection_patterns: &[
        "Daily notes and journal entries",
        "Timestamped reflections and processing",
        "Creative writing and drafts",
        "Personal narrative and life documentation",
        "Folder patterns: journal/, daily/, reflection/, writing/, drafts/, creative/",
    ],
    presentation_guidance: &[
        "Open with a pattern you've noticed across entries—not just the latest one.",
        "Don't push toward production. Some writing is for processing, not publishing.",
        "Notice what's being written around—the thing that keeps almost surfacing.",
        "Honor the rhythm. Reflective work has seasons; not everything is ready.",
        "End with possibility: what might this become, if anything? Let them decide.",
    ],
};

/// All profiles in display order.
pub const CATALYST_PROFILES: &[&CatalystProfile] = &[
    &PROFILE_DIALECTICAL,
    &PROFILE_SYNTHESIS,
    &PROFILE_RELATIONAL,
    &PROFILE_OPERATIONAL,
    &PROFILE_STRATEGIC,
    &PROFILE_REFLECTIVE,
];

/// Enabled profile keys in order.
pub const ENABLED_PROFILE_KEYS: &[&str] = &[
    "dialectical",
    "synthesis",
    "relational",
    "operational",
    "strategic",
    "reflective",
];

// ---------------------------------------------------------------------------
// Lookup helpers
// ---------------------------------------------------------------------------

/// Look up a profile by key (case-insensitive).
pub fn get_profile(key: &str) -> Option<&'static CatalystProfile> {
    let lower = key.to_lowercase();
    CATALYST_PROFILES.iter().find(|p| p.key == lower).copied()
}

/// Look up a format by key (case-insensitive).
pub fn get_format(key: &str) -> Option<&'static CatalystFormat> {
    let lower = key.to_lowercase();
    CATALYST_FORMATS.iter().find(|f| f.key == lower).copied()
}

/// Return CORE guidance + profile-specific guidance lines combined.
pub fn full_guidance_lines(profile: &CatalystProfile) -> Vec<&'static str> {
    let mut lines = CORE_CATALYST_GUIDANCE.to_vec();
    lines.extend_from_slice(profile.guidance_lines);
    lines
}

/// Look up presentation guidance for an entity based on its profile (explore register).
///
/// Resolution order: entity-specific profile override → vault-wide default → dialectical.
/// Only used in explore mode — continuity and reference have register-level guidance.
pub fn presentation_guidance_for_entity(
    entity_name: &str,
    entity_profiles: &std::collections::HashMap<String, String>,
    default_profile: Option<&str>,
) -> Vec<String> {
    let profile_key = entity_profiles
        .get(entity_name)
        .map(|s| s.as_str())
        .or(default_profile)
        .unwrap_or(DEFAULT_PROFILE_KEY);

    let profile = get_profile(profile_key)
        .unwrap_or_else(|| get_profile(DEFAULT_PROFILE_KEY).unwrap());

    profile.presentation_guidance.iter().map(|s| s.to_string()).collect()
}

/// Return presentation guidance for a non-explore register.
pub fn presentation_guidance_for_register(register: ContentRegister) -> Vec<String> {
    let lines: &[&str] = match register {
        ContentRegister::Continuity => CONTINUITY_GUIDANCE,
        ContentRegister::Reference => REFERENCE_GUIDANCE,
        ContentRegister::Explore => &[], // shouldn't be called for explore — use per-entity
    };
    lines.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_profile_found() {
        let p = get_profile("dialectical").unwrap();
        assert_eq!(p.key, "dialectical");
        assert_eq!(p.display_name, "Dialectical");
    }

    #[test]
    fn test_get_profile_case_insensitive() {
        let p = get_profile("Reflective").unwrap();
        assert_eq!(p.key, "reflective");
    }

    #[test]
    fn test_get_profile_not_found() {
        assert!(get_profile("nonexistent").is_none());
    }

    #[test]
    fn test_get_format_found() {
        let f = get_format("thesis").unwrap();
        assert_eq!(f.key, "thesis");
        assert_eq!(f.display_name, "Theses");
    }

    #[test]
    fn test_get_format_case_insensitive() {
        let f = get_format("Claim").unwrap();
        assert_eq!(f.key, "claim");
    }

    #[test]
    fn test_full_guidance_lines() {
        let lines = full_guidance_lines(&PROFILE_DIALECTICAL);
        assert_eq!(lines.len(), CORE_CATALYST_GUIDANCE.len() + PROFILE_DIALECTICAL.guidance_lines.len());
        // First lines should be core guidance
        assert!(lines[0].starts_with("Probe tensions"));
        // Last lines should be profile-specific
        assert!(lines.last().unwrap().starts_with("Honor the full lifecycle"));
    }

    #[test]
    fn test_all_profiles_present() {
        assert_eq!(CATALYST_PROFILES.len(), 6);
        assert_eq!(ENABLED_PROFILE_KEYS.len(), 6);
        for key in ENABLED_PROFILE_KEYS {
            assert!(get_profile(key).is_some(), "profile '{}' not found", key);
        }
    }

    #[test]
    fn test_all_formats_present() {
        assert_eq!(CATALYST_FORMATS.len(), 3);
        for key in &["question", "thesis", "claim"] {
            assert!(get_format(key).is_some(), "format '{}' not found", key);
        }
    }
}
