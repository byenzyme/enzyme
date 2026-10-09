//! Within-entity temporal evidence selection. Readings that declare no
//! sample/favor policy sample by recency: every represented period keeps
//! evidence, recent periods get more, and the per-period question allocation
//! reaches the prompt. The legacy newest-first folder selector left one excerpt
//! per older period and let the model pile era labels onto the newest quarter.
use super::*;
use chrono::Datelike;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Preference {
    Equal,
    Recent,
}
impl Preference {
    pub(super) fn from_learning(policy: Option<&enzyme_spec::Learning>) -> Option<Self> {
        let Some(policy) = policy.filter(|p| p.sample.is_some() || p.favor.is_some()) else {
            return Some(Self::Recent);
        };
        Some(if policy.favor.as_deref() == Some("recent") {
            Self::Recent
        } else {
            Self::Equal
        })
    }
}

/// The explicit temporal sampling policy: period preference plus the calendar
/// granularity of the periods themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Policy {
    pub(super) preference: Preference,
    pub(super) periods: enzyme_spec::Periods,
}
impl Policy {
    /// `None` keeps the legacy sampler; `periods` alone never opts in.
    pub(super) fn from_learning(policy: Option<&enzyme_spec::Learning>) -> Option<Self> {
        Some(Self {
            preference: Preference::from_learning(policy)?,
            periods: policy
                .and_then(|p| p.periods)
                .unwrap_or(enzyme_spec::Periods::Auto),
        })
    }
}
impl From<Preference> for Policy {
    fn from(preference: Preference) -> Self {
        Self {
            preference,
            periods: enzyme_spec::Periods::Auto,
        }
    }
}

/// Reserve coverage first, then distribute extra slots by weighted fair share.
/// Equal scarcity spans the timeline (one slot chooses the newest period);
/// recent scarcity chooses the newest periods. Saturated periods donate slots.
fn allocate(capacities: &[usize], total: usize, preference: Preference) -> Vec<usize> {
    let active: Vec<usize> = capacities
        .iter()
        .enumerate()
        .filter_map(|(i, &n)| (n > 0).then_some(i))
        .collect();
    let mut result = vec![0; capacities.len()];
    let slots = total.min(
        capacities
            .iter()
            .fold(0usize, |sum, &n| sum.saturating_add(n)),
    );
    if slots == 0 || active.is_empty() {
        return result;
    }
    if slots < active.len() {
        let chosen: Vec<usize> = if preference == Preference::Recent || slots == 1 {
            active[active.len() - slots..].to_vec()
        } else {
            (0..slots)
                .map(|j| active[j * (active.len() - 1) / (slots - 1)])
                .collect()
        };
        for i in chosen {
            result[i] = 1;
        }
        return result;
    }
    for &i in &active {
        result[i] = 1;
    }
    for _ in active.len()..slots {
        let i = active
            .iter()
            .copied()
            .filter(|&i| result[i] < capacities[i])
            .min_by(|&a, &b| {
                let weight = |i| {
                    if preference == Preference::Recent {
                        i + 1
                    } else {
                        1
                    }
                };
                // result[i] is one plus extras already assigned, i.e. the next
                // extra's position in the weighted schedule. No floating point ties.
                (result[a] as u128 * weight(b) as u128)
                    .cmp(&(result[b] as u128 * weight(a) as u128))
                    .then_with(|| {
                        if preference == Preference::Recent {
                            b.cmp(&a)
                        } else {
                            a.cmp(&b)
                        }
                    })
            })
            .unwrap();
        result[i] += 1;
    }
    result
}

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn occurrence_candidates(occurrences: &[EntityOccurrence]) -> Vec<ChunkCandidate> {
    occurrences
        .iter()
        .filter_map(|occ| {
            Some(ChunkCandidate {
                source_ref: occ.file_path.clone(),
                chunk_index: 0,
                content: occ.context.clone()?,
                timestamp: occurrence_timestamp(occ),
                era_index: 0,
                relevance: 0.0,
                vector: None,
                title: None,
            })
        })
        .collect()
}

fn label(c: &ChunkCandidate) -> String {
    excerpt_label(&c.source_ref, c.title.as_deref(), c.timestamp)
}

pub(super) fn select(
    candidates: &[ChunkCandidate],
    num_questions: usize,
    max_samples: usize,
    max_bytes: usize,
    policy: impl Into<Policy>,
    reference_ms: i64,
) -> SelectedContext {
    let Policy {
        preference,
        periods,
    } = policy.into();
    // Keep raw timestamps. Future and non-calendar dates are ineligible for
    // this historical sampler, not silently clamped into the newest period.
    let mut eligible: Vec<&ChunkCandidate> = candidates
        .iter()
        .filter(|c| {
            !c.content.trim().is_empty()
                && c.timestamp <= reference_ms
                && chrono::DateTime::from_timestamp_millis(c.timestamp)
                    .is_some_and(|d| (1..=9999).contains(&d.year()))
        })
        .collect();
    eligible.sort_by(|a, b| {
        b.relevance
            .total_cmp(&a.relevance)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
            .then_with(|| a.source_ref.cmp(&b.source_ref))
            .then_with(|| a.chunk_index.cmp(&b.chunk_index))
            .then_with(|| a.content.cmp(&b.content))
    });
    let mut seen = HashSet::new();
    eligible.retain(|c| seen.insert((c.source_ref.as_str(), c.timestamp, c.content.as_str())));
    let mut months = HashMap::<String, HashSet<&str>>::new();
    for c in &eligible {
        months
            .entry(month_key_from_timestamp(c.timestamp).unwrap())
            .or_default()
            .insert(&c.source_ref);
    }
    let buckets = months
        .into_iter()
        .map(|(month, paths)| (month, paths.len() as i64))
        .collect();
    let eras = crate::pipeline::eras::sampling_eras(&buckets, periods);
    let mut grouped: Vec<Vec<&ChunkCandidate>> = vec![Vec::new(); eras.len()];
    for c in eligible {
        grouped[era_index_for_timestamp(&eras, c.timestamp)].push(c);
    }
    // One excerpt per source per period before taking additional chunks from
    // that source. Dated entries in the same log can still represent other eras.
    for group in &mut grouped {
        let mut sources = HashSet::new();
        let (mut first, rest): (Vec<_>, Vec<_>) = group
            .drain(..)
            .partition(|c| sources.insert(c.source_ref.as_str()));
        first.extend(rest);
        *group = first;
    }
    let capacities: Vec<_> = grouped.iter().map(Vec::len).collect();
    let mut sample_slots = max_samples.min(capacities.iter().sum());
    let (selected, minimums) = loop {
        let allocation = allocate(&capacities, sample_slots, preference);
        let chosen: Vec<_> = grouped
            .iter()
            .enumerate()
            .flat_map(|(i, group)| group.iter().take(allocation[i]).map(move |c| (i, *c)))
            .collect();
        // Count complete provenance labels (source, date, title) inside the
        // existing byte budget; reserve up to 64 content bytes per excerpt
        // before sharing the rest.
        let minimums: Vec<_> = chosen
            .iter()
            .map(|(_, c)| label(c).len() + c.content.len().min(64))
            .collect();
        let minimum: usize = minimums.iter().sum();
        if max_bytes == 0 || minimum <= max_bytes || sample_slots == 0 {
            break (chosen, minimums);
        }
        sample_slots = ((sample_slots as u128 * max_bytes as u128 / minimum as u128) as usize)
            .min(sample_slots - 1);
    };
    let mut lengths = minimums;
    if max_bytes == 0 {
        for (length, (_, c)) in lengths.iter_mut().zip(&selected) {
            *length = label(c).len() + c.content.len();
        }
    } else {
        let mut remaining = max_bytes - lengths.iter().sum::<usize>();
        loop {
            let active: Vec<_> = selected
                .iter()
                .enumerate()
                .filter_map(|(i, (_, c))| {
                    let full = label(c).len() + c.content.len();
                    (lengths[i] < full).then_some((i, full))
                })
                .collect();
            if remaining == 0 || active.is_empty() {
                break;
            }
            let share = (remaining / active.len()).max(1);
            for (i, full) in active {
                let extra = share.min(full - lengths[i]).min(remaining);
                lengths[i] += extra;
                remaining -= extra;
            }
        }
    }
    let represented: Vec<_> = (0..eras.len())
        .filter(|i| selected.iter().any(|(j, _)| i == j))
        .collect();
    let mut sources = Vec::new();
    let mut samples = Vec::new();
    for ((era, c), length) in selected.iter().zip(lengths) {
        let header = label(c);
        let content = prefix(&c.content, length - header.len());
        sources.push(GenerationContextSource {
            source_ref: c.source_ref.clone(),
            timestamp_ms: c.timestamp,
            title: c.title.clone(),
            excerpt: context_excerpt(content),
        });
        samples.push((
            represented.iter().position(|i| i == era).unwrap(),
            format!("{header}{content}"),
        ));
    }
    let era_allocations = allocate(
        &vec![num_questions; represented.len()],
        num_questions,
        preference,
    );
    SelectedContext {
        era_context: (!samples.is_empty()).then(|| EraContext {
            eras: represented.iter().map(|&i| eras[i].clone()).collect(),
            era_allocations,
            samples,
        }),
        flat_samples: Vec::new(),
        context_sources: sources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enzyme_spec::Periods;
    fn ts(date: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(&format!("{date}T00:00:00Z"))
            .unwrap()
            .timestamp_millis()
    }
    fn fixture(counts: [usize; 3]) -> Vec<ChunkCandidate> {
        ["2024-01-01", "2025-01-01", "2026-01-01"]
            .iter()
            .enumerate()
            .flat_map(|(era, date)| {
                (0..counts[era]).map(move |i| ChunkCandidate {
                    source_ref: format!("period-{era}/doc-{i:02}.md"),
                    chunk_index: 0,
                    content: format!(
                        "PERIOD_{era}_DOC_{i} {}",
                        "Substantive source evidence for a decision and rejected alternative. "
                            .repeat(6)
                    ),
                    timestamp: ts(date),
                    era_index: era,
                    relevance: 0.0,
                    vector: None,
                    title: None,
                })
            })
            .collect()
    }
    fn paths(context: &SelectedContext) -> Vec<String> {
        context
            .context_sources
            .iter()
            .map(|s| s.source_ref.clone())
            .collect()
    }
    fn counts(context: &SelectedContext) -> Vec<usize> {
        let ec = context.era_context.as_ref().unwrap();
        (0..ec.eras.len())
            .map(|i| ec.samples.iter().filter(|(j, _)| *j == i).count())
            .collect()
    }
    #[test]
    fn dense_recent_history_preserves_sparse_old_and_middle_evidence() {
        let candidates = fixture([1, 1, 20]);
        let buckets = HashMap::from([
            ("2024-01".into(), 1),
            ("2025-01".into(), 1),
            ("2026-01".into(), 20),
        ]);
        let eras = cluster_eras_periodic(&buckets, 3, 3);
        let mut legacy_candidates = candidates.clone();
        for c in &mut legacy_candidates {
            c.era_index = era_index_for_timestamp(&eras, c.timestamp);
        }
        let before = select_folder_chunks(&eras, 3, &legacy_candidates, 3, 2000);
        assert!(paths(&before).iter().all(|p| p.starts_with("period-2/")));
        let after = select(&candidates, 3, 3, 2000, Preference::Equal, ts("2026-02-01"));
        assert_eq!(
            paths(&after),
            vec![
                "period-0/doc-00.md",
                "period-1/doc-00.md",
                "period-2/doc-00.md"
            ]
        );
        assert_eq!(counts(&after), vec![1, 1, 1]);
    }
    #[test]
    fn equal_and_recent_allocate_evidence_and_questions_before_rendering() {
        let candidates = fixture([8, 8, 8]);
        for (preference, expected) in [
            (Preference::Equal, vec![3, 3, 3]),
            (Preference::Recent, vec![2, 3, 4]),
        ] {
            let selected = select(&candidates, 9, 9, 0, preference, ts("2026-02-01"));
            assert_eq!(counts(&selected), expected);
            assert_eq!(
                selected.era_context.as_ref().unwrap().era_allocations,
                expected
            );
            let mut shuffled = candidates.clone();
            shuffled.reverse();
            shuffled.rotate_left(5);
            assert_eq!(
                paths(&select(&shuffled, 9, 9, 0, preference, ts("2026-02-01"))),
                paths(&selected)
            );
        }
    }
    #[test]
    fn caps_sparse_redistribution_duplicates_and_source_diversity() {
        assert_eq!(allocate(&[1, 2, 20], 9, Preference::Equal), vec![1, 2, 6]);
        assert_eq!(allocate(&[0, 0], 4, Preference::Equal), vec![0, 0]);
        assert_eq!(allocate(&[4, 4, 4], 2, Preference::Equal), vec![1, 0, 1]);
        assert_eq!(allocate(&[4, 4, 4], 2, Preference::Recent), vec![0, 1, 1]);
        assert_eq!(allocate(&[4, 4, 4], 1, Preference::Equal), vec![0, 0, 1]);
        let mut log = fixture([1, 1, 1]);
        for c in &mut log {
            c.source_ref = "journal.md".into();
        }
        let dated_entries = select(&log, 3, 3, 0, Preference::Equal, ts("2026-02-01"));
        assert_eq!(
            dated_entries.context_sources.len(),
            3,
            "one log can represent three periods"
        );
        assert_eq!(
            dated_entries
                .context_sources
                .iter()
                .map(|s| s.timestamp_ms)
                .collect::<HashSet<_>>()
                .len(),
            3
        );
        let mut candidates = fixture([4, 4, 4]);
        candidates.extend(candidates.clone());
        let mut extra = candidates[0].clone();
        extra.chunk_index = 1;
        extra.content = "Additional material in the same document".repeat(10);
        candidates.push(extra);
        let chosen = select(&candidates, 9, 9, 0, Preference::Equal, ts("2026-02-01"));
        assert_eq!(paths(&chosen).iter().collect::<HashSet<_>>().len(), 9);
        for budget in [1, 70, 250, 400, 1000] {
            let result = select(
                &candidates,
                9,
                9,
                budget,
                Preference::Equal,
                ts("2026-02-01"),
            );
            if let Some(ec) = result.era_context {
                assert!(ec.samples.iter().map(|(_, s)| s.len()).sum::<usize>() <= budget);
                assert_eq!(ec.samples.len(), result.context_sources.len());
                for ((_, text), source) in ec.samples.iter().zip(&result.context_sources) {
                    assert!(text.starts_with(&excerpt_label(
                        &source.source_ref,
                        source.title.as_deref(),
                        source.timestamp_ms
                    )));
                }
                assert_eq!(ec.era_allocations.iter().sum::<usize>(), 9);
            }
        }
        assert!(
            select(&candidates, 9, 0, 0, Preference::Equal, ts("2026-02-01"))
                .context_sources
                .is_empty()
        );
        assert!(
            select(&[], 9, 9, 0, Preference::Equal, ts("2026-02-01"))
                .context_sources
                .is_empty()
        );
    }
    #[test]
    fn clock_eligibility_keeps_raw_timestamps_and_unicode_labels_intact() {
        let mut candidates = fixture([1, 1, 1]);
        let mut future = candidates[0].clone();
        future.timestamp = ts("2100-01-01");
        future.source_ref = "future.md".into();
        candidates.push(future);
        let mut invalid = candidates[0].clone();
        invalid.timestamp = i64::MAX;
        candidates.push(invalid);
        for c in &mut candidates {
            c.content = "🦀 data ".repeat(100);
        }
        let chosen = select(
            &candidates,
            4,
            10,
            500,
            Preference::Recent,
            ts("2025-06-01"),
        );
        assert_eq!(
            paths(&chosen),
            vec!["period-0/doc-00.md", "period-1/doc-00.md"]
        );
        assert_eq!(chosen.context_sources[0].timestamp_ms, ts("2024-01-01"));
        assert!(
            chosen
                .era_context
                .unwrap()
                .samples
                .iter()
                .map(|(_, s)| s.len())
                .sum::<usize>()
                <= 500
        );
        let later = select(&candidates, 4, 10, 0, Preference::Recent, ts("2026-02-01"));
        assert_eq!(later.context_sources.len(), 3);
        assert_eq!(candidates[3].timestamp, ts("2100-01-01"));
    }

    fn populate(db: &Database, with_chunks: bool) -> Vec<Entity> {
        use libsql::Value as V;
        db.execute_write("INSERT INTO entities (id,name,type,frequency,trends) VALUES (1,'topic','link',24,'{\"frequency_recent\":8,\"recency_score\":8.0,\"timeline_buckets\":{}}'), (2,'efforts','folder',1,'{}')", vec![]).unwrap();
        db.execute_write("INSERT INTO docs (source_ref,content,content_hash,created_at,modified_at,indexed_at) VALUES ('efforts/topic.md','identity','id',1,1,1)", vec![]).unwrap();
        for c in fixture([8, 8, 8]) {
            db.execute_write("INSERT INTO docs (source_ref,content,content_hash,created_at,modified_at,indexed_at) VALUES (?1,?2,'hash',?3,?3,?3)", vec![V::Text(c.source_ref.clone()),V::Text(c.content.clone()),V::Integer(c.timestamp)]).unwrap();
            db.execute_write("INSERT INTO entity_occurrences (entity_id,file_path,context,created_at,effective_date) VALUES (1,?1,NULL,?2,?2)", vec![V::Text(c.source_ref.clone()),V::Integer(c.timestamp)]).unwrap();
            // Mix chunk-backed and chunkless documents: the old period must
            // remain represented even though its source has no chunks.
            if with_chunks && !c.source_ref.starts_with("period-0/") {
                db.execute_write("INSERT INTO chunks (doc_id,chunk_index,content,created_at,entry_date) SELECT id,0,?2,?3,?3 FROM docs WHERE source_ref=?1", vec![V::Text(c.source_ref),V::Text(c.content),V::Integer(c.timestamp)]).unwrap();
            }
        }
        db.get_all_entities().unwrap()
    }
    fn spec(source: &str) -> enzyme_spec::Vault {
        enzyme_spec::resolve(
            vec![enzyme_spec::parse(source).unwrap()],
            std::path::Path::new("/tmp"),
        )
        .unwrap()
        .vaults
        .remove(0)
    }
    #[test]
    fn as_of_hides_later_evidence_from_temporal_and_legacy_sampling() {
        for chunks in [false, true] {
            let db = Database::open_in_memory().unwrap();
            let entities = populate(&db, chunks);
            let llm = LlmClient::with_base_url("unused", "http://localhost:0");
            let pipeline = CatalystPipeline::new(&db, &llm, None);
            let periods = |source: &str| {
                let spec = spec(source);
                let config = CatalystConfig {
                    reading_spec: Some(&spec),
                    max_samples: 9,
                    max_context_chars: Some(6000),
                    min_context_tokens: 1,
                    ..Default::default()
                };
                let (_, jobs, _) = pipeline.build_jobs(&entities, &config).unwrap();
                let topic = jobs.into_iter().find(|j| j.entity_name == "topic").unwrap();
                let mut periods: Vec<_> = topic
                    .context_sources
                    .iter()
                    .map(|s| s.source_ref[..8].to_string())
                    .collect();
                periods.sort();
                periods.dedup();
                (periods, topic.context_hash)
            };
            let reading = r#"learn questions from folder "efforts" including linked pages"#;
            let (now, now_hash) =
                periods(&format!(r#"vault "/notes" {{ sample by time {reading} }}"#));
            assert_eq!(now, ["period-0", "period-1", "period-2"]);
            let (past, past_hash) = periods(&format!(
                r#"vault "/notes" {{ as of "2025-06-30" sample by time {reading} }}"#
            ));
            assert_eq!(past, ["period-0", "period-1"]);
            assert_ne!(now_hash, past_hash);
            // The day's end is inclusive: 2025-01-01T00:00 is visible as of that date.
            let (day, _) = periods(&format!(
                r#"vault "/notes" {{ learning {{ as of "2025-01-01" sample by recency }} {reading} }}"#
            ));
            assert_eq!(day, ["period-0", "period-1"]);
            let (legacy, _) = periods(&format!(
                r#"learning {{ as of "2024-06-30" }} vault "/notes" {{ {reading} }}"#
            ));
            assert_eq!(legacy, ["period-0"]);
        }
    }
    #[test]
    fn real_jobs_and_prompt_dump_honor_inheritance_overrides_and_chunkless_fallback() {
        for chunks in [false, true] {
            let db = Database::open_in_memory().unwrap();
            let entities = populate(&db, chunks);
            let llm = LlmClient::with_base_url("unused", "http://localhost:0");
            let pipeline = CatalystPipeline::new(&db, &llm, None);
            let parent = spec(
                r#"learning { sample across time favor recent periods } vault "/notes" { learn questions from folder "efforts" including linked pages sample across time favor all periods equally question budget 9 }"#,
            );
            let config = CatalystConfig {
                reading_spec: Some(&parent),
                max_samples: 9,
                max_context_chars: Some(6000),
                min_context_tokens: 1,
                ..Default::default()
            };
            let (_, jobs, _) = pipeline.build_jobs(&entities, &config).unwrap();
            let topic = jobs.iter().find(|j| j.entity_name == "topic").unwrap();
            assert_eq!(topic.context_sources.len(), 9);
            for era in 0..3 {
                assert_eq!(
                    topic
                        .context_sources
                        .iter()
                        .filter(|s| s.source_ref.starts_with(&format!("period-{era}/")))
                        .count(),
                    3
                );
            }
            assert!(topic.user_prompt.contains("PERIOD_0_DOC_0"));
            assert!(topic.user_prompt.contains("3 questions"));
            let overridden = spec(
                r#"learning { sample across time favor all periods equally } vault "/notes" { learn questions from folder "efforts" including linked pages question budget 9 learn questions from link "topic" favor recent periods question budget 9 }"#,
            );
            let config = CatalystConfig {
                reading_spec: Some(&overridden),
                ..config
            };
            let (_, recent_jobs, _) = pipeline.build_jobs(&entities, &config).unwrap();
            let recent = recent_jobs
                .iter()
                .find(|j| j.entity_name == "topic")
                .unwrap();
            for (era, expected) in [2, 3, 4].iter().enumerate() {
                assert_eq!(
                    recent
                        .context_sources
                        .iter()
                        .filter(|s| s.source_ref.starts_with(&format!("period-{era}/")))
                        .count(),
                    *expected
                );
            }
            assert_ne!(topic.context_hash, recent.context_hash);
            for (source, expected) in [
                (
                    r#"learning { sample by recency } vault "/notes" { learn questions from folder "efforts" including linked pages sample by time question budget 9 }"#,
                    &jobs,
                ),
                (
                    r#"learning { sample by time } vault "/notes" { learn questions from folder "efforts" including linked pages question budget 9 learn questions from link "topic" sample by recency question budget 9 }"#,
                    &recent_jobs,
                ),
            ] {
                let concise = spec(source);
                let concise_config = CatalystConfig {
                    reading_spec: Some(&concise),
                    max_samples: 9,
                    max_context_chars: Some(6000),
                    min_context_tokens: 1,
                    ..Default::default()
                };
                let (_, actual, _) = pipeline.build_jobs(&entities, &concise_config).unwrap();
                assert_eq!(actual.len(), expected.len());
                for (a, b) in actual.iter().zip(expected) {
                    assert_eq!(a.entity_name, b.entity_name);
                    assert_eq!(
                        a.user_prompt, b.user_prompt,
                        "same requested question count and period allocation"
                    );
                    assert_eq!(
                        a.context_hash, b.context_hash,
                        "equivalent modes must not invalidate generation"
                    );
                    assert_eq!(
                        a.context_sources
                            .iter()
                            .map(|s| (&s.source_ref, s.timestamp_ms, &s.excerpt))
                            .collect::<Vec<_>>(),
                        b.context_sources
                            .iter()
                            .map(|s| (&s.source_ref, s.timestamp_ms, &s.excerpt))
                            .collect::<Vec<_>>()
                    );
                }
            }
            let dump = tempfile::NamedTempFile::new().unwrap();
            pipeline
                .dump_prompts(&entities, &config, dump.path())
                .unwrap();
            let rows: Vec<serde_json::Value> = std::fs::read_to_string(dump.path())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert_eq!(
                rows.iter().find(|r| r["entity"] == "topic").unwrap()["user_prompt"],
                recent.user_prompt
            );
            // Both local/hosted formatters consume the exact same era samples.
            let candidates = fixture([1, 1, 1]);
            let selected = select(
                &candidates,
                3,
                resolve_sample_limit(40, true, 3),
                resolve_context_budget(None, true),
                Preference::Equal,
                ts("2026-02-01"),
            );
            let local =
                build_local_user_prompt("topic", &[], 3, None, selected.era_context.as_ref());
            let hosted = build_user_prompt(
                "topic",
                &[],
                3,
                get_profile(DEFAULT_PROFILE_KEY).unwrap(),
                DEFAULT_FORMAT_KEY,
                None,
                selected.era_context.as_ref(),
                ts("2026-02-01"),
            );
            for c in &selected.context_sources {
                assert!(local.contains(&c.source_ref));
                assert!(hosted.contains(&c.source_ref));
            }
        }
    }
    #[test]
    fn custom_reading_jobs_and_dumps_omit_builtin_steering() {
        let db = Database::open_in_memory().unwrap();
        let entities = populate(&db, true);
        // Hosted client: the custom system/user contract is the hosted path.
        let llm = LlmClient::with_base_url("unused", "https://hosted.invalid/v1");
        let pipeline = CatalystPipeline::new(&db, &llm, None);
        let custom = spec(
            r#"profile mine { seek "CUSTOM SEEK" notice { "CUSTOM NOTICE" } } vault "/notes" { learn questions from link "topic" about mine }"#,
        );
        let profiles = HashMap::from([("topic".to_string(), "mine".to_string())]);
        let base = CatalystConfig {
            entity_profiles: &profiles,
            max_samples: 9,
            max_context_chars: Some(6000),
            min_context_tokens: 1,
            ..Default::default()
        };
        let config = CatalystConfig {
            reading_spec: Some(&custom),
            ..base
        };
        let (system, jobs, _) = pipeline.build_jobs(&entities, &config).unwrap();
        assert!(system.contains("SELECTED READING PROFILE"));
        let topic = jobs.iter().find(|j| j.entity_name == "topic").unwrap();
        let prompt = &topic.user_prompt;
        for banned in [
            "<practice_shape>",
            "Engagement shape",
            "Register —",
            "probe what would reopen",
            "tensions, emergence",
            "probe AROUND",
            "what it costs",
            "defined in the system prompt",
        ] {
            assert!(!prompt.contains(banned), "{banned:?} in {prompt}");
        }
        assert_eq!(prompt.matches("SELECTED READING PROFILE: mine").count(), 1);
        assert_eq!(prompt.matches("SELECTED PROFILE").count(), 0);
        for kept in [
            "Seek: CUSTOM SEEK",
            "CUSTOM NOTICE",
            "<vault_context",
            "PERIOD_",
            "\"catalysts\"",
        ] {
            assert!(prompt.contains(kept), "{kept:?} missing from {prompt}");
        }
        // The prompt dump shares build_jobs, so it carries the same text.
        let dump = tempfile::NamedTempFile::new().unwrap();
        pipeline
            .dump_prompts(&entities, &config, dump.path())
            .unwrap();
        let dumped = std::fs::read_to_string(dump.path()).unwrap();
        assert!(!dumped.contains("practice_shape"));

        // Built-in profiles keep their engagement steering.
        let builtin_profiles =
            HashMap::from([("topic".to_string(), DEFAULT_PROFILE_KEY.to_string())]);
        let builtin = CatalystConfig {
            entity_profiles: &builtin_profiles,
            ..base
        };
        let (_, jobs, _) = pipeline.build_jobs(&entities, &builtin).unwrap();
        let topic = jobs.iter().find(|j| j.entity_name == "topic").unwrap();
        assert!(topic.user_prompt.contains("<practice_shape>"));
        assert!(topic.user_prompt.contains("Engagement shape"));
    }
    /// Ten consecutive months (2025-01..2025-10), `per_month` documents each.
    fn ten_months(per_month: usize) -> Vec<ChunkCandidate> {
        (1..=10)
            .flat_map(|m| {
                (0..per_month).map(move |i| ChunkCandidate {
                    source_ref: format!("2025-{m:02}/doc-{i}.md"),
                    chunk_index: 0,
                    content: format!(
                        "MONTH_{m}_DOC_{i} {}",
                        "Substantive source evidence for a decision. ".repeat(4)
                    ),
                    timestamp: ts(&format!("2025-{m:02}-15")),
                    era_index: 0,
                    relevance: 0.0,
                    vector: None,
                    title: None,
                })
            })
            .collect()
    }
    fn labels(context: &SelectedContext) -> Vec<String> {
        let ec = context.era_context.as_ref().unwrap();
        ec.eras.iter().map(|e| e.label.clone()).collect()
    }
    #[test]
    fn periods_choose_monthly_or_quarterly_granularity() {
        let history = ten_months(3);
        let reference = ts("2026-02-01");
        let policy = |periods| Policy {
            preference: Preference::Equal,
            periods,
        };
        let auto = select(&history, 10, 30, 0, Preference::Equal, reference);
        assert_eq!(labels(&auto), ["2025-Q1", "2025-Q2", "2025-Q3", "2025-Q4"]);
        let explicit_auto = select(&history, 10, 30, 0, policy(Periods::Auto), reference);
        let quarterly = select(&history, 10, 30, 0, policy(Periods::Quarterly), reference);
        for same in [&explicit_auto, &quarterly] {
            assert_eq!(labels(same), labels(&auto));
            assert_eq!(paths(same), paths(&auto));
        }
        let monthly = select(&history, 10, 30, 0, policy(Periods::Monthly), reference);
        assert_eq!(labels(&monthly).len(), 10);
        assert_eq!(labels(&monthly)[0], "Jan 2025");
        assert_eq!(labels(&monthly)[9], "Oct 2025");
        assert_eq!(counts(&monthly), vec![3; 10]);
        let ec = monthly.era_context.as_ref().unwrap();
        assert_eq!(ec.era_allocations, vec![1; 10]);
        // Recency preference allocates across the same ten months, newer first.
        let recent = select(
            &history,
            10,
            30,
            0,
            Policy {
                preference: Preference::Recent,
                periods: Periods::Monthly,
            },
            reference,
        );
        let allocations = &recent.era_context.as_ref().unwrap().era_allocations;
        assert_eq!(allocations.len(), 10);
        assert_eq!(allocations.iter().sum::<usize>(), 10);
        assert!(
            allocations.windows(2).all(|w| w[0] <= w[1]),
            "{allocations:?}"
        );
        // Short histories: auto is monthly, explicit quarterly still groups.
        let short: Vec<_> = history
            .iter()
            .filter(|c| c.timestamp < ts("2025-04-01"))
            .cloned()
            .collect();
        assert_eq!(
            labels(&select(&short, 3, 9, 0, Preference::Equal, reference)).len(),
            3
        );
        assert_eq!(
            labels(&select(
                &short,
                3,
                9,
                0,
                policy(Periods::Quarterly),
                reference
            )),
            ["2025-Q1"]
        );
    }
    #[test]
    fn periods_clause_drives_jobs_allocation_and_hash_only_when_present() {
        use libsql::Value as V;
        let db = Database::open_in_memory().unwrap();
        db.execute_write("INSERT INTO entities (id,name,type,frequency,trends) VALUES (1,'topic','link',20,'{}')", vec![]).unwrap();
        for c in ten_months(2) {
            db.execute_write("INSERT INTO docs (source_ref,content,content_hash,created_at,modified_at,indexed_at) VALUES (?1,?2,'hash',?3,?3,?3)", vec![V::Text(c.source_ref.clone()),V::Text(c.content.clone()),V::Integer(c.timestamp)]).unwrap();
            db.execute_write("INSERT INTO entity_occurrences (entity_id,file_path,context,created_at,effective_date) VALUES (1,?1,NULL,?2,?2)", vec![V::Text(c.source_ref),V::Integer(c.timestamp)]).unwrap();
        }
        let entities = db.get_all_entities().unwrap();
        let llm = LlmClient::with_base_url("unused", "http://localhost:0");
        let pipeline = CatalystPipeline::new(&db, &llm, None);
        let job = |source: &str| {
            let spec = spec(source);
            let config = CatalystConfig {
                reading_spec: Some(&spec),
                max_samples: 20,
                max_context_chars: Some(20000),
                min_context_tokens: 1,
                ..Default::default()
            };
            let (_, jobs, _) = pipeline.build_jobs(&entities, &config).unwrap();
            jobs.into_iter().find(|j| j.entity_name == "topic").unwrap()
        };
        let reading = r#"learn questions from link "topic" question budget 10"#;
        let monthly = job(&format!(
            r#"vault "/notes" {{ sample by time periods monthly {reading} }}"#
        ));
        assert!(
            monthly
                .user_prompt
                .contains("Temporal evidence: 10 represented periods"),
            "{}",
            monthly.user_prompt
        );
        assert_eq!(monthly.user_prompt.matches(": 1 questions\n").count(), 10);
        // Catalyst era labels are validated against the periods the prompt offered.
        assert_eq!(monthly.era_labels.len(), 10);
        for label in &monthly.era_labels {
            assert!(
                monthly
                    .user_prompt
                    .contains(&format!("- {label}: 1 questions")),
                "{label}"
            );
        }
        // Reading-level clause overrides the vault; auto is today's rule.
        let auto = job(&format!(r#"vault "/notes" {{ sample by time {reading} }}"#));
        assert!(
            auto.user_prompt
                .contains("Temporal evidence: 4 represented periods")
        );
        for label in ["2025-Q1", "2025-Q2", "2025-Q3", "2025-Q4"] {
            assert!(auto.user_prompt.contains(&format!("- {label}: ")));
        }
        let overridden = job(&format!(
            r#"vault "/notes" {{ sample by time periods monthly {reading} periods auto }}"#
        ));
        assert_eq!(overridden.user_prompt, auto.user_prompt);
        // A declared granularity changes the hash; absence keeps the old one.
        assert_ne!(overridden.context_hash, auto.context_hash);
        assert_ne!(monthly.context_hash, auto.context_hash);
        // Without a declared policy, readings sample by recency, so a
        // granularity clause alone still shapes the periods.
        let default = job(&format!(r#"vault "/notes" {{ {reading} }}"#));
        let recency = job(&format!(
            r#"vault "/notes" {{ sample by recency {reading} }}"#
        ));
        assert_eq!(default.user_prompt, recency.user_prompt);
        assert!(
            default
                .user_prompt
                .contains("Temporal evidence: 4 represented periods")
        );
        assert!(default.user_prompt.contains("Recent priority"));
        let monthly_default = job(&format!(
            r#"vault "/notes" {{ periods monthly {reading} }}"#
        ));
        assert!(
            monthly_default
                .user_prompt
                .contains("Temporal evidence: 10 represented periods")
        );
    }
    #[test]
    fn omitted_temporal_settings_sample_by_recency() {
        assert_eq!(Preference::from_learning(None), Some(Preference::Recent));
        assert_eq!(
            Preference::from_learning(Some(&enzyme_spec::Learning::default())),
            Some(Preference::Recent)
        );
        assert_eq!(
            Preference::from_learning(Some(&enzyme_spec::Learning {
                sample: Some("across_time".into()),
                ..Default::default()
            })),
            Some(Preference::Equal)
        );
        assert_eq!(
            Preference::from_learning(Some(&enzyme_spec::Learning {
                favor: Some("recent".into()),
                ..Default::default()
            })),
            Some(Preference::Recent)
        );
    }
}
