//! Deterministic profile assignment for entities.
//!
//! Assigns catalyst profiles without an LLM call using two strategies:
//!
//! 1. **Rules**: keyword matching on entity name/type → profile.
//!    Covers common Obsidian patterns (people → relational, readwise → resonance_trace).
//!
//! 2. **Embedding fallback**: when no keyword matches, embeds the entity's
//!    occurrence context and compares to pre-computed profile exemplar centroids.
//!    Requires an `EmbeddingService`.
//!
//! The default fallback (no embedding service, no keyword match) is `tension_trace`.

use std::collections::HashMap;

use crate::embedding::EmbeddingService;
use crate::llm::DEFAULT_PROFILE_KEY;
use crate::models::Entity;

/// Keyword rules: if any token in `"{type} {name}"` matches a group,
/// the corresponding profile gets a score boost.
const RULE_GROUPS: &[(&[&str], &str, f64)] = &[
    (
        &[
            "people",
            "person",
            "contacts",
            "contact",
            "colleagues",
            "friends",
            "community",
            "network",
            "relationships",
        ],
        "relational",
        2.5,
    ),
    (
        &[
            "preference",
            "preferences",
            "taste",
            "tastes",
            "recipe",
            "recipes",
            "cooking",
            "feedback",
            "activity",
            "activities",
            "trace",
            "traces",
        ],
        "preference_evidence",
        2.5,
    ),
    (
        &[
            "readwise",
            "reading",
            "highlights",
            "highlight",
            "source",
            "sources",
            "pkm",
            "library",
        ],
        "resonance_trace",
        2.5,
    ),
    (
        &[
            "project",
            "projects",
            "work",
            "meeting",
            "meetings",
            "task",
            "tasks",
            "inbox",
            "startup",
            "operations",
        ],
        "operational",
        2.0,
    ),
    (
        &[
            "strategy",
            "strategic",
            "enzyme",
            "storytelling",
            "ux",
            "product",
            "market",
            "decision",
            "decisions",
            "architecture",
            "ai",
        ],
        "decision_trace",
        2.0,
    ),
    (
        &[
            "personal",
            "journal",
            "daily",
            "travel",
            "roots",
            "reflection",
            "reflective",
            "writing",
            "capture",
        ],
        "reflective",
        2.0,
    ),
    (
        &["ethics", "philosophy", "thinking", "concepts", "ideas"],
        "tension_trace",
        1.5,
    ),
];

/// Synthetic exemplars per profile. Each reads like a real vault artifact
/// belonging to that profile — preference logs, CRM entries, readwise highlights,
/// sprint reviews, decision memos, journal entries, and conceptual explorations.
const PROFILE_EXEMPLARS: &[(&str, &[&str])] = &[
    (
        "preference_evidence",
        &[
            "Recipe note: reduced the sugar by half and liked the balance much better.",
            "Design trace: user keeps choosing denser canvases with smaller labels and fewer decorative effects.",
            "Feedback log: prefers keyboard shortcuts over modal flows for repeated work.",
            "Agent observation: the user repeatedly rejects verbose summaries and asks for concrete diffs.",
            "Activity log: worked well for a dinner party because it could be made ahead and served at room temperature.",
            "Canvas JSON decision: kept muted color swatches but removed gradients after preview.",
            "Preference note: strong texture contrast matters more than visual polish in this context.",
            "Usage trace: abandoned the flow whenever it required copying data between panes.",
        ],
    ),
    (
        "tension_trace",
        &[
            "I keep going back and forth. The argument for structure makes sense but emergence says the opposite.",
            "The tension between wanting to be known and wanting to stay hidden shows up constantly.",
            "Something unresolved about how community and individualism coexist.",
            "Two frameworks pulling in different directions. Not a problem to solve. A tension to inhabit.",
            "Does technology augment human creativity or replace the conditions that produce it?",
            "How craft and efficiency are at odds. The author does not resolve it. Maybe that is the point.",
            "Both things feel true. Not trying to pick one. Sitting with the dissonance.",
            "Two thinkers who completely disagree and I find both compelling.",
        ],
    ),
    (
        "resonance_trace",
        &[
            "Highlights from a book on cognitive science and hemispheric lateralization.",
            "Saved from podcast: personal knowledge management as building a second brain.",
            "Article clipping on why toolmakers should use their own tools.",
            "Book note: the author argues reading is inherently creative, not passive.",
            "Readwise highlight: great leaders ask questions rather than give answers.",
            "Annotation from a research paper on dimensionality reduction in embeddings.",
            "Clipped from newsletter: information versus knowledge is about integration with experience.",
            "Podcast notes on recommendation systems. The tension between relevance and serendipity.",
        ],
    ),
    (
        "relational",
        &[
            "Contact note for a friend. Met through a local community. Works on design tools. Last caught up in March.",
            "Catch-up notes with an old teammate. We talk less often now but go deeper when we do.",
            "People folder entry for a former colleague. Strong opinions about craft. Potential collaborator on next project.",
            "Group dinner roster and notes. Seating arrangement worked well last time.",
            "Reflecting on which collaborations energize me and which ones drain me.",
            "Mentor check-in log: quarterly call. She always asks about my relationship with ambition.",
            "Network map update: who do I go to for what? Technical questions, emotional support, honest feedback.",
            "CRM entry: followed up with three people from the conference. Two responded. One went quiet.",
        ],
    ),
    (
        "operational",
        &[
            "Sprint review: shipped the new onboarding flow. Retention still lagging. Focus on day-7 next sprint.",
            "Bug triage: the data pipeline is failing silently on large inputs. P1. Fix this week.",
            "TODO: update release checklist, fix CI flakiness, review the open pull request.",
            "Project status: migration is 60 percent done. Blockers: legacy consumers have not upgraded yet.",
            "Action items from standup: finalize copy, set up analytics tracking, schedule user interviews.",
            "Handoff notes for the new contributor. Build system overview, known issues, point of contact.",
            "Quarterly planning: committed to too much last quarter. Three bets maximum this time.",
            "Status update: growth is behind plan and the timeline is tight.",
        ],
    ),
    (
        "decision_trace",
        &[
            "Decision matrix for a launch. Columns: trust, cost, competitive position.",
            "Pricing analysis. Option A targets individuals, option B targets teams. Chose B with documented reasoning.",
            "Committed to a local-first architecture. This forecloses subscription revenue. Trade-off documented.",
            "Wrote down the three conditions that would reverse this bet. Revisit in 90 days.",
            "Go-to-market decision: developer-first or sales-led. Picked developer-first. Consequences listed.",
            "Killed one initiative to focus resources on another. Explicit resource allocation choice.",
            "Competitive positioning memo: our advantage is speed and taste, theirs is distribution. We need to move first.",
            "Scenario analysis: if the market commoditizes in 12 months, what happens to our unit economics?",
        ],
    ),
    (
        "reflective",
        &[
            "Morning pages. Woke up thinking about a conversation with a parent. Something about how they described their own childhood.",
            "Travel journal, day 3 abroad. The old buildings feel like they were built for a different relationship with time.",
            "I keep writing about the same thing without naming it directly.",
            "Draft of an essay. The first paragraph is not working. Too defensive. Need to lead with the question instead.",
            "Walking in the evening. The light reminded me of something from childhood that I cannot quite place.",
            "Processing the end of a season. What I thought I wanted turned out to be a proxy for something else.",
            "Free writing: roots, soil, what it means to belong somewhere without owning it.",
            "Yesterday felt like a turning point but I am not sure toward what. The shift happens before the understanding.",
        ],
    ),
];

/// Assign profiles to a set of entities.
///
/// Returns a map from entity name → profile key.
///
/// Strategy:
/// 1. Try keyword rules on entity name/type.
/// 2. If no keyword match and `embedding_service` is provided, embed the
///    entity's occurrence context and compare to exemplar centroids.
/// 3. Otherwise, fall back to `tension_trace`.
pub fn assign_profiles(
    entities: &[Entity],
    contexts: &HashMap<String, String>,
    embedding_service: Option<&EmbeddingService>,
) -> HashMap<String, String> {
    let exemplar_centroids = embedding_service.and_then(|svc| build_exemplar_centroids(svc).ok());

    let mut profiles = HashMap::new();

    for entity in entities {
        let profile = match rule_assign(entity) {
            Some(p) => p,
            None => match (&exemplar_centroids, contexts.get(&entity.name)) {
                (Some(centroids), Some(context)) => {
                    embedding_assign(embedding_service.unwrap(), context, centroids)
                        .unwrap_or_else(|| DEFAULT_PROFILE_KEY.to_string())
                }
                _ => DEFAULT_PROFILE_KEY.to_string(),
            },
        };
        profiles.insert(entity.name.clone(), profile);
    }

    profiles
}

/// Try to assign a profile from keyword rules. Returns None if no keyword matched.
fn rule_assign(entity: &Entity) -> Option<String> {
    let text = format!("{} {}", entity.entity_type.as_str(), entity.name);
    let tokens: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).collect();

    let mut scores: HashMap<&str, f64> = HashMap::new();

    for (keywords, profile, weight) in RULE_GROUPS {
        for token in &tokens {
            let lower = token.to_lowercase();
            if keywords.contains(&lower.as_str()) {
                *scores.entry(profile).or_default() += weight;
            }
        }
    }

    // Link entities get a small relational nudge
    if entity.entity_type.as_str() == "link" {
        *scores.entry("relational").or_default() += 0.5;
    }

    if scores.is_empty() {
        return None;
    }

    scores
        .into_iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(profile, _)| profile.to_string())
}

/// Pre-computed exemplar centroids for each profile.
struct ExemplarCentroids {
    /// Profile key → normalized centroid vector.
    centroids: HashMap<String, Vec<f32>>,
}

/// Build exemplar centroids by embedding the synthetic exemplar texts.
fn build_exemplar_centroids(service: &EmbeddingService) -> anyhow::Result<ExemplarCentroids> {
    let mut centroids = HashMap::new();

    for (profile, exemplars) in PROFILE_EXEMPLARS {
        let vecs = service.embed_documents(exemplars)?;
        let dim = vecs.first().map(|v| v.len()).unwrap_or(0);
        if dim == 0 {
            continue;
        }

        // Average
        let mut centroid = vec![0.0f32; dim];
        for vec in &vecs {
            for (i, v) in vec.iter().enumerate() {
                centroid[i] += v;
            }
        }
        let n = vecs.len() as f32;
        for v in &mut centroid {
            *v /= n;
        }

        // Normalize
        let norm: f32 = centroid.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut centroid {
                *v /= norm;
            }
        }

        centroids.insert(profile.to_string(), centroid);
    }

    Ok(ExemplarCentroids { centroids })
}

/// Assign a profile by comparing the entity's context embedding to exemplar centroids.
fn embedding_assign(
    service: &EmbeddingService,
    context: &str,
    centroids: &ExemplarCentroids,
) -> Option<String> {
    let vec = service.embed_query(context).ok()?;

    centroids
        .centroids
        .iter()
        .map(|(profile, centroid)| {
            let sim: f32 = vec.iter().zip(centroid).map(|(a, b)| a * b).sum();
            (profile.as_str(), sim)
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(profile, _)| profile.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::EntityType;

    fn make_entity(name: &str, etype: EntityType) -> Entity {
        Entity {
            id: None,
            name: name.to_string(),
            entity_type: etype,
            frequency: 0,
            ranking_weight: 0.0,
            first_seen: None,
            last_seen: None,
            trends: Default::default(),
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn rule_assign_people_is_relational() {
        let e = make_entity("people", EntityType::Folder);
        assert_eq!(rule_assign(&e), Some("relational".to_string()));
    }

    #[test]
    fn rule_assign_readwise_is_resonance_trace() {
        let e = make_entity("readwise", EntityType::Folder);
        assert_eq!(rule_assign(&e), Some("resonance_trace".to_string()));
    }

    #[test]
    fn rule_assign_recipes_are_preference_evidence() {
        let e = make_entity("recipes", EntityType::Folder);
        assert_eq!(rule_assign(&e), Some("preference_evidence".to_string()));
    }

    #[test]
    fn rule_assign_founding_is_operational() {
        let e = make_entity("startup", EntityType::Tag);
        assert_eq!(rule_assign(&e), Some("operational".to_string()));
    }

    #[test]
    fn rule_assign_enzyme_is_decision_trace() {
        let e = make_entity("enzyme", EntityType::Tag);
        assert_eq!(rule_assign(&e), Some("decision_trace".to_string()));
    }

    #[test]
    fn rule_assign_travel_is_reflective() {
        let e = make_entity("travel", EntityType::Tag);
        assert_eq!(rule_assign(&e), Some("reflective".to_string()));
    }

    #[test]
    fn rule_assign_faith_is_tension_trace() {
        let e = make_entity("ethics", EntityType::Tag);
        assert_eq!(rule_assign(&e), Some("tension_trace".to_string()));
    }

    #[test]
    fn rule_assign_unknown_returns_none() {
        let e = make_entity("bestill", EntityType::Tag);
        assert_eq!(rule_assign(&e), None);
    }

    #[test]
    fn rule_assign_link_gets_relational_nudge() {
        let e = make_entity("alice jones", EntityType::Link);
        assert_eq!(rule_assign(&e), Some("relational".to_string()));
    }

    #[test]
    fn assign_profiles_falls_back_to_tension_trace() {
        let entities = vec![make_entity("bestill", EntityType::Tag)];
        let profiles = assign_profiles(&entities, &HashMap::new(), None);
        assert_eq!(profiles.get("bestill").unwrap(), "tension_trace");
    }

    #[test]
    fn assign_profiles_uses_rules_when_available() {
        let entities = vec![
            make_entity("people", EntityType::Folder),
            make_entity("enzyme", EntityType::Tag),
            make_entity("unknown", EntityType::Tag),
        ];
        let profiles = assign_profiles(&entities, &HashMap::new(), None);
        assert_eq!(profiles.get("people").unwrap(), "relational");
        assert_eq!(profiles.get("enzyme").unwrap(), "decision_trace");
        assert_eq!(profiles.get("unknown").unwrap(), "tension_trace");
    }
}
