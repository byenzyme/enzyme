/// Catalyst profile definitions — reusable retrieval lenses.
///
/// Core Theory: Catalysts ask plainly, in the user's own first-person voice, about
/// what the material marks as decided, changed, or still open: a choice, plan,
/// position, ask, or follow-up. Blind comparisons on a real vault (2026-10-20)
/// ranked this voice above oblique "what does X cost" probing for readability,
/// fit, and usefulness.
// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub struct CatalystProfile {
    pub key: &'static str,
    pub display_name: &'static str,
    /// Public explanation of how this lens changes the way Enzyme reads material.
    /// Suitable for generated configuration, selection UI, and documentation.
    pub user_description: &'static str,
    /// Detailed generation guidance supplied to the model.
    pub description: &'static str,
    pub guidance_lines: &'static [&'static str],
    pub sample_prompts: &'static [&'static str],
    pub footer: Option<&'static str>,
    pub detection_patterns: &'static [&'static str],
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
    "Treat catalysts as handles the user would want to read: each question points at something real in the excerpts that they may want to find again.",
    "Ask about what was decided, what changed and what moved it, or what is still open: a choice, plan, position, ask, or follow-up the excerpts actually mark, not an abstract tension they don't raise.",
    "Write in the user's own voice: first person (I, my, we), plain words, under about 18 words, one question per sentence.",
    "Keep the real names of people, projects, places, and options. Use at most one time anchor, in plain words (\"last spring\", \"after the move\"), never a date stamp or period label.",
    "Ask about the thing itself, not the document: avoid words like note, entry, excerpt, or reflection.",
    "Do not presume an outcome the excerpts don't state.",
    "Return questions only—no exercises, imperatives, or statements passed off as answers.",
    "Prefer questions worth answering: ones that need more than a single line or a date lookup, each leading somewhere different.",
];

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

pub const DEFAULT_PROFILE_KEY: &str = "tension_trace";
pub const DEFAULT_FORMAT_KEY: &str = "question";

// ---------------------------------------------------------------------------
// Catalyst formats
// ---------------------------------------------------------------------------

pub const FORMAT_QUESTION: CatalystFormat = CatalystFormat {
    key: "question",
    display_name: "Questions",
    description: "Plain first-person questions about what was decided, what changed, and what is still open.",
    format_prompt: "Generate catalysts as single sentence questions in the user's own first-person \
voice (never \"you\" or \"the writer\"), using vocabulary drawn from the excerpts.",
    surface_guidance: &[
        "What was decided, and what alternatives were left behind",
        "What changed between attempts or seasons, and what moved it",
        "What is still open: a follow-up, an ask, an unsettled choice",
        "What keeps coming back, in the user's own words",
        "Who was involved, and what they asked for or offered",
    ],
    examples: &[
        "Why did I pick the smaller launch over the full rewrite?",
        "Did I ever follow up with Maya after the pricing call?",
        "How did my plan for the studio change after the first client left?",
        "What did I decide about moving, and has it held?",
        "Which of my reasons for leaving the job still feel true?",
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

pub const CATALYST_FORMATS: &[&CatalystFormat] = &[&FORMAT_QUESTION, &FORMAT_THESIS, &FORMAT_CLAIM];

// ---------------------------------------------------------------------------
// The catalyst retrieval lenses
// ---------------------------------------------------------------------------

pub const PROFILE_PREFERENCE_EVIDENCE: CatalystProfile = CatalystProfile {
    key: "preference_evidence",
    display_name: "Preference Evidence",
    user_description: "Read activity and feedback as evidence of stable preferences: what worked or failed, which constraints mattered, and which adjustments keep recurring.",
    description: "You create retrieval handles for stable preference signals across activity logs: what worked, \
what failed, what the user repeated, adapted, avoided, reduced, intensified, or chose under constraint. \
The domain can be cooking, design, product behavior, agent traces, or any log where taste and preference \
are inferred from concrete actions.",
    guidance_lines: &[
        "Surface likes, dislikes, thresholds, constraints, repeated choices, and worked/didn't-work signals.",
        "Name retrieval axes a future assistant would need: occasions, constraints, substitutions, outcomes, tradeoffs, materials, styles, and repeated adjustments.",
        "Prefer questions that retrieve non-obvious evidence across many small entries over questions that restate one entry.",
        "Use metadata only as grounding; the catalyst should point to a latent preference pattern in the body and surrounding entries.",
    ],
    sample_prompts: &[
        "Which {{domain_items}} became repeat-worthy after {{adjustment_or_constraint}} changed?",
        "Where did {{preference_signal}} make something work despite {{constraint}}?",
        "What pattern connects the moments where {{user_or_system}} reduced, omitted, or substituted {{attribute}}?",
        "Which examples show {{style_or_choice}} as a stable preference rather than a one-off outcome?",
    ],
    footer: Some(
        "The goal is preference recall: generate handles that help an agent retrieve evidence of taste, constraints, and repeated behavior.",
    ),
    detection_patterns: &[
        "Activity logs with comments, outcomes, or preference signals",
        "JSON traces describing user choices, edits, rankings, or approvals",
        "Recipe notes, design decisions, product usage logs, saved preferences",
        "Agent observations about what the user tends to prefer or reject",
        "Folder patterns: preferences/, activity/, recipes/, design-decisions/, traces/, feedback/",
    ],
};

pub const PROFILE_TENSION_TRACE: CatalystProfile = CatalystProfile {
    key: "tension_trace",
    display_name: "Tension Trace",
    user_description: "Read this material for unresolved pressure: costs, assumptions, contradictions, avoided tradeoffs, and the positions that keep pulling against each other.",
    description: "You create retrieval handles for costs, assumptions, contradictions, avoided tradeoffs, and live \
frictions. This lens is useful for personal reflection, technical uncertainty, design disagreement, strategy, \
and any material where the important retrieval target is the unresolved pressure rather than the surface topic.",
    guidance_lines: &[
        "Look for choices or positions still pulled in two directions, and name both sides in the user's words.",
        "Ask whether it was settled, how, or what keeps reopening it.",
        "Avoid applying one template to every entity; vary what each question asks.",
        "Notice when the user has been circling something and may be ready to decide or let it go.",
    ],
    sample_prompts: &[
        "Am I still torn between {{position_a}} and {{position_b}}, or did I settle it?",
        "What keeps pulling me back to {{concept}} after I set it aside?",
        "What would I have to give up to commit to {{position_a}}?",
        "Did I ever decide between {{framework_a}} and {{framework_b}} for {{project}}?",
    ],
    footer: Some(
        "Consider whether the user has been circling this entity repeatedly and might benefit from synthesis, action, or closure instead of more discovery.",
    ),
    detection_patterns: &[
        "General-purpose thinking with diverse topics and perspectives",
        "Philosophical or analytical approach to varied subjects",
        "Content showing conceptual exploration and framework-building",
        "Mixed content without clear specialized domain focus",
        "Folder patterns: ideas/, concepts/, thinking/, notes/",
        "Default choice when no other profile clearly matches",
    ],
};

pub const PROFILE_RESONANCE_TRACE: CatalystProfile = CatalystProfile {
    key: "resonance_trace",
    display_name: "Resonance Trace",
    user_description: "Read saved material through its connection to active work: what repeatedly draws attention, challenges settled thinking, or gives language to an unfinished thread.",
    description: "You create retrieval handles for reference material that keeps connecting to the user's own \
thinking. Imported highlights, saved articles, research notes, design references, and inspiration boards matter \
because they show what repeatedly draws attention and where it feeds active work.",
    guidance_lines: &[
        "Connect saved highlights to the user's own plans and writing, in their words.",
        "Ask where an idea was taken up, or was meant to be and wasn't.",
        "Notice when a highlight pushes against something the user believes.",
        "Name the author and the idea plainly.",
    ],
    sample_prompts: &[
        "Which of my plans did {{author}}'s idea about {{concept}} feed into?",
        "Did I ever use what I saved about {{topic}} the way I meant to?",
        "What did {{author}} say that pushes against my view of {{concept}}?",
        "Why do I keep saving ideas about {{theme}}?",
    ],
    footer: Some(
        "The goal is personal resonance, not comprehensive synthesis. One genuine connection beats cataloging everything.",
    ),
    detection_patterns: &[
        "Imported highlights and annotations from reading",
        "Readwise synced content with source metadata",
        "Saved articles, book highlights, podcast notes",
        "Content with clear external attribution",
        "Folder patterns: Readwise/, sources/, reading/, highlights/, clips/, library/",
    ],
};

pub const PROFILE_RELATIONAL: CatalystProfile = CatalystProfile {
    key: "relational",
    display_name: "Relational",
    user_description: "Read people notes for the meaning and movement of relationships: what each connection gives or asks, which dynamics repeat, and what could move it forward.",
    description: "You create retrieval handles for what passes between the user and the people in their \
notes: what was said, offered, or asked, how each relationship changed, and which follow-ups \
were planned. Ask plainly, by name, in the user's own words.",
    guidance_lines: &[
        "Ask what was said, offered, or asked between the user and the person, by name.",
        "Ask how the relationship, or what was on the table, changed and what moved it.",
        "Notice follow-ups that were planned, and whether a later excerpt shows them happening.",
        "Connect people to the projects, places, and communities where they show up.",
    ],
    sample_prompts: &[
        "What did {{person}} and I decide about {{topic}}?",
        "How did things with {{person}} change after {{event}}?",
        "Did I follow up with {{person}} after {{conversation}}?",
        "Where does {{person}} show up in my work on {{project}}?",
    ],
    footer: Some(
        "Some relationships need tending, some need honest evaluation, some need release. Surface which this might be.",
    ),
    detection_patterns: &[
        "Notes about specific people with interaction history",
        "Contact information and relationship context",
        "Meeting notes tied to individuals",
        "Reflection on relationship dynamics",
        "Folder patterns: people/, contacts/, relationships/, network/, CRM/",
    ],
};

pub const PROFILE_OPERATIONAL: CatalystProfile = CatalystProfile {
    key: "operational",
    display_name: "Operational",
    user_description: "Read project material for what affects progress: what is stuck, what keeps returning unresolved, which decisions lack an owner, and what is actually working.",
    description: "You surface what's stuck, what needs attention, and what's actually working. Projects \
accumulate notes but lose clarity. Meetings happen but actions slip. Your catalysts cut \
through the pile to find what's unresolved, what's blocking progress, and what the user \
might be avoiding.",
    guidance_lines: &[
        "Ask what is stuck and who owns it, in plain words.",
        "Separate what is genuinely blocked from what is being put off.",
        "Notice what keeps coming up in meetings without a resolution.",
        "Connect the task to why the project matters.",
    ],
    sample_prompts: &[
        "What is really blocking {{project}}, if it isn't time?",
        "Who owns the open question about {{topic}}?",
        "Did anyone actually decide {{assumption}} for {{project}}?",
        "What keeps coming up about {{topic}} that we never resolve?",
    ],
    footer: Some(
        "Operational clarity often comes from naming what's stuck, not adding more tasks.",
    ),
    detection_patterns: &[
        "Project notes with tasks, timelines, or status updates",
        "Meeting notes with attendees and action items",
        "Work-related documentation and planning",
        "Content with deadlines, milestones, or deliverables",
        "Folder patterns: projects/, work/, meetings/, clients/, sprints/",
    ],
};

pub const PROFILE_DECISION_TRACE: CatalystProfile = CatalystProfile {
    key: "decision_trace",
    display_name: "Decision Trace",
    user_description: "Read this material as a trail of choices: which alternatives were considered, what constraints made one path win, and what evidence could reopen it.",
    description: "You create retrieval handles for bets, options, reversals, rejected alternatives, assumptions, \
constraints, and evidence thresholds. This lens works across technical decisions, product strategy, canvas \
design choices, agent traces, architecture logs, and any domain where the artifact records why one path won.",
    guidance_lines: &[
        "Name the options that were on the table and which one won, in the user's words.",
        "Ask what tipped the choice, or what evidence would reopen it.",
        "Notice when more research has become a way of not deciding.",
        "Retrieve rejected alternatives, constraints, and reversals, not just the final choice.",
    ],
    sample_prompts: &[
        "Why did I pick {{option_a}} over {{option_b}} for {{decision}}?",
        "What would make me reopen {{decision}}?",
        "Am I still gathering information on {{decision}} instead of choosing?",
        "Which options did {{stakeholder}} rule out for {{decision}}?",
    ],
    footer: Some("Decision clarity often comes from naming the trade-off, not from more analysis."),
    detection_patterns: &[
        "Decision logs with options and criteria",
        "Technical, product, design, or strategic planning with trade-off analysis",
        "Notes weighing alternatives or scenarios",
        "Content about bets, risks, or resource allocation",
        "Agent traces explaining why a tool, architecture, canvas shape, or design choice was selected",
        "Folder patterns: decisions/, strategy/, planning/, bets/, architecture/, design-decisions/",
    ],
};

pub const PROFILE_REFLECTIVE: CatalystProfile = CatalystProfile {
    key: "reflective",
    display_name: "Reflective",
    user_description: "Read entries across time for what keeps returning, what is changing, and what may be ready to become a decision, conversation, or piece of work.",
    description: "You surface what's emerging and what's ready. Journals capture the raw material of \
thinking. Creative work develops over time. Your catalysts notice patterns across \
entries, probe what keeps coming back, and sense when something is ready to become \
something else—a piece of writing, a decision, a conversation.",
    guidance_lines: &[
        "Notice what keeps coming back across entries, and ask about it in the user's words.",
        "Ask whether a feeling or intention became a decision, a conversation, or a piece of work.",
        "Ask how a stance shifted between seasons and what moved it.",
        "Not everything needs to ship; leave room for questions about what is still forming.",
    ],
    sample_prompts: &[
        "What keeps coming back when I write about {{topic}}?",
        "Did my intention about {{practice}} hold, or did I drop it?",
        "How did my thinking about {{experience}} shift after {{event}}?",
        "Is {{thread}} ready to become something I share?",
    ],
    footer: Some(
        "Reflective work moves at its own pace. Surface what's ready, don't force production.",
    ),
    detection_patterns: &[
        "Daily notes and journal entries",
        "Timestamped reflections and processing",
        "Creative writing and drafts",
        "Personal narrative and life documentation",
        "Folder patterns: journal/, daily/, reflection/, writing/, drafts/, creative/",
    ],
};

/// All profiles in display order.
pub const CATALYST_PROFILES: &[&CatalystProfile] = &[
    &PROFILE_PREFERENCE_EVIDENCE,
    &PROFILE_TENSION_TRACE,
    &PROFILE_RESONANCE_TRACE,
    &PROFILE_RELATIONAL,
    &PROFILE_OPERATIONAL,
    &PROFILE_DECISION_TRACE,
    &PROFILE_REFLECTIVE,
];

/// Enabled profile keys in order.
pub const ENABLED_PROFILE_KEYS: &[&str] = &[
    "preference_evidence",
    "tension_trace",
    "resonance_trace",
    "relational",
    "operational",
    "decision_trace",
    "reflective",
];

// ---------------------------------------------------------------------------
// Lookup helpers
// ---------------------------------------------------------------------------

/// Look up a profile by key (case-insensitive).
pub fn get_profile(key: &str) -> Option<&'static CatalystProfile> {
    let lower = match key.to_lowercase().as_str() {
        "dialectical" => "tension_trace".to_string(),
        "synthesis" => "resonance_trace".to_string(),
        "strategic" => "decision_trace".to_string(),
        other => other.to_string(),
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_profile_found() {
        let p = get_profile("tension_trace").unwrap();
        assert_eq!(p.key, "tension_trace");
        assert_eq!(p.display_name, "Tension Trace");
    }

    #[test]
    fn test_get_profile_legacy_alias() {
        let p = get_profile("dialectical").unwrap();
        assert_eq!(p.key, "tension_trace");
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
        let lines = full_guidance_lines(&PROFILE_TENSION_TRACE);
        assert_eq!(
            lines.len(),
            CORE_CATALYST_GUIDANCE.len() + PROFILE_TENSION_TRACE.guidance_lines.len()
        );
        // First lines should be core guidance
        assert!(lines[0].starts_with("Treat catalysts as handles the user would want to read"));
        // Last lines should be profile-specific
        assert!(
            lines
                .last()
                .unwrap()
                .starts_with("Notice when the user has been circling")
        );
    }

    #[test]
    fn test_all_profiles_present() {
        assert_eq!(CATALYST_PROFILES.len(), 7);
        assert_eq!(ENABLED_PROFILE_KEYS.len(), 7);
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
