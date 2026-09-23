use std::collections::HashMap;

use chrono::Datelike;

use crate::models::EntityOccurrence;

/// A temporal era within an entity's activity history.
#[derive(Debug, Clone)]
pub struct Era {
    /// Human-readable label, e.g. "2025-Q1", "2025-08 to 2025-10".
    pub label: String,
    /// Inclusive start month key "YYYY-MM".
    pub start: String,
    /// Inclusive end month key "YYYY-MM".
    pub end: String,
    /// Total note count across all months in this era.
    pub count: i64,
    /// Additive evidence weight in this era. Unweighted Markdown gives `1.0`
    /// per occurrence, so this equals `count` for legacy corpora.
    pub weight: f64,
}

/// Cluster an entity's timeline_buckets into contiguous eras.
///
/// Rules:
/// - Consecutive months with activity are grouped together.
/// - A gap of >2 months with zero activity starts a new era.
/// - Eras with fewer than `min_notes` are merged into the nearest neighbor.
pub fn cluster_eras(timeline_buckets: &HashMap<String, i64>, min_notes: i64) -> Vec<Era> {
    if timeline_buckets.is_empty() {
        return Vec::new();
    }

    // Sort month keys chronologically
    let mut months: Vec<(&String, &i64)> = timeline_buckets
        .iter()
        .filter(|&(_, &count)| count > 0)
        .collect();
    months.sort_by_key(|(k, _)| k.as_str());

    if months.is_empty() {
        return Vec::new();
    }

    // Group consecutive months (gap > 2 months triggers new era)
    let mut raw_eras: Vec<Vec<(&str, i64)>> = Vec::new();
    let mut current_group: Vec<(&str, i64)> = vec![(months[0].0.as_str(), *months[0].1)];

    for window in months.windows(2) {
        let prev_key = window[0].0.as_str();
        let curr_key = window[1].0.as_str();
        let curr_count = *window[1].1;

        let gap = month_distance(prev_key, curr_key);
        if gap > 2 {
            raw_eras.push(std::mem::take(&mut current_group));
        }
        current_group.push((curr_key, curr_count));
    }
    if !current_group.is_empty() {
        raw_eras.push(current_group);
    }

    // Build Era structs
    let mut eras: Vec<Era> = raw_eras
        .into_iter()
        .map(|group| {
            let start = group.first().unwrap().0.to_string();
            let end = group.last().unwrap().0.to_string();
            let count: i64 = group.iter().map(|(_, c)| c).sum();
            let label = format_era_label(&start, &end);
            Era {
                label,
                start,
                end,
                count,
                weight: count as f64,
            }
        })
        .collect();

    // Merge thin eras into neighbors
    merge_thin_eras(&mut eras, min_notes);

    eras
}

/// Cluster an entity's timeline_buckets into adaptive eras with budget-aware merging.
///
/// Period size adapts to the entity's total time span:
/// - < 6 months: monthly eras
/// - 6 months – 3 years: quarterly eras
/// - > 3 years: half-year eras for older periods, quarterly for the most recent 2 years
///
/// After initial bucketing, thin eras (below `min_notes`) are merged into neighbors,
/// then the smallest eras are merged until the count fits within `catalyst_budget`.
pub fn cluster_eras_periodic(
    timeline_buckets: &HashMap<String, i64>,
    min_notes: i64,
    catalyst_budget: usize,
) -> Vec<Era> {
    if timeline_buckets.is_empty() {
        return Vec::new();
    }

    // Find the span in months
    let mut month_keys: Vec<&String> = timeline_buckets
        .keys()
        .filter(|k| timeline_buckets.get(*k).copied().unwrap_or(0) > 0)
        .collect();
    month_keys.sort();

    if month_keys.is_empty() {
        return Vec::new();
    }

    let (first_y, first_m) = parse_month_key(month_keys.first().unwrap());
    let (last_y, last_m) = parse_month_key(month_keys.last().unwrap());
    let span_months = (last_y * 12 + last_m) - (first_y * 12 + first_m) + 1;

    let eras = if span_months <= 6 {
        // Monthly eras
        bucket_monthly(timeline_buckets)
    } else if span_months <= 36 {
        // Quarterly eras
        bucket_quarterly(timeline_buckets)
    } else {
        // Hybrid: half-year for old periods, quarterly for recent 2 years
        bucket_hybrid(timeline_buckets)
    };

    let mut eras = eras;

    // Phase 1: merge thin eras (below min_notes)
    merge_thin_eras(&mut eras, min_notes);

    // Phase 2: keep merging smallest era until we fit within budget
    while eras.len() > catalyst_budget && eras.len() > 1 {
        let smallest_idx = eras
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.count)
            .map(|(i, _)| i)
            .unwrap();
        merge_into_neighbor(&mut eras, smallest_idx);
    }

    eras
}

/// Explicit temporal sampling keeps sparse nonempty periods. Reuse the existing
/// calendar buckets, without merging thin periods or letting question count
/// determine the evidence timeline. `Auto` is monthly through six months,
/// quarterly beyond; an explicit granularity applies regardless of span.
pub(crate) fn sampling_eras(
    timeline_buckets: &HashMap<String, i64>,
    periods: enzyme_spec::Periods,
) -> Vec<Era> {
    match periods {
        enzyme_spec::Periods::Monthly => return bucket_monthly(timeline_buckets),
        enzyme_spec::Periods::Quarterly => return bucket_quarterly(timeline_buckets),
        enzyme_spec::Periods::Auto => {}
    }
    let mut months: Vec<_> = timeline_buckets
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(m, _)| m.as_str())
        .collect();
    months.sort();
    let (Some(first), Some(last)) = (months.first(), months.last()) else {
        return Vec::new();
    };
    let (fy, fm) = parse_month_key(first);
    let (ly, lm) = parse_month_key(last);
    if (ly - fy) * 12 + lm - fm + 1 <= 6 {
        bucket_monthly(timeline_buckets)
    } else {
        bucket_quarterly(timeline_buckets)
    }
}

/// Replace default mention-count weights with configured per-month weights.
pub fn apply_timeline_weights(eras: &mut [Era], timeline_weights: &HashMap<String, f64>) {
    if timeline_weights.is_empty() {
        return;
    }
    for era in eras {
        era.weight = timeline_weights
            .iter()
            .filter(|(month, _)| {
                month.as_str() >= era.start.as_str() && month.as_str() <= era.end.as_str()
            })
            .map(|(_, weight)| *weight)
            .sum();
    }
}

/// Assign occurrences to eras based on their effective_date.
/// Returns a vec parallel to `eras` with the occurrences that fall in each era,
/// plus a final vec for any occurrences that don't match any era.
pub fn assign_occurrences_to_eras<'a>(
    eras: &[Era],
    occurrences: &'a [EntityOccurrence],
) -> Vec<Vec<&'a EntityOccurrence>> {
    let mut buckets: Vec<Vec<&EntityOccurrence>> = vec![Vec::new(); eras.len() + 1];
    let unmatched_idx = eras.len();

    for occ in occurrences {
        let month_key = occurrence_month_key(occ);
        let era_idx = match month_key {
            Some(ref key) => eras.iter().position(|era| {
                key.as_str() >= era.start.as_str() && key.as_str() <= era.end.as_str()
            }),
            None => None,
        };
        match era_idx {
            Some(idx) => buckets[idx].push(occ),
            None => buckets[unmatched_idx].push(occ),
        }
    }

    buckets
}

/// Allocate catalyst counts across eras from a total budget.
/// Each era gets at least `min_per_era`, remainder weighted by recency.
pub fn allocate_catalysts_to_eras(eras: &[Era], total: usize, min_per_era: usize) -> Vec<usize> {
    if eras.is_empty() {
        return Vec::new();
    }
    if eras.len() == 1 {
        return vec![total];
    }

    let min_per_era = min_per_era.max(1);
    let max_eras_covered = total / min_per_era;

    // If we can't cover all eras, prioritize recent ones (end of list)
    // but always include the oldest era for temporal contrast.
    if max_eras_covered < eras.len() {
        let mut allocs = vec![0usize; eras.len()];
        if max_eras_covered >= 2 {
            // Oldest era gets 1 slot for contrast
            allocs[0] = 1;
            // Fill remaining from most recent backward
            let mut remaining = total - 1;
            for i in (1..eras.len()).rev() {
                if remaining == 0 {
                    break;
                }
                allocs[i] = min_per_era.min(remaining);
                remaining -= allocs[i];
            }
        } else {
            // Can only cover 1 era — give it all to the most recent
            *allocs.last_mut().unwrap() = total;
        }
        return allocs;
    }

    let floor_total = eras.len() * min_per_era;

    let mut allocs = vec![min_per_era; eras.len()];
    let remaining = total - floor_total;

    // Weight remaining catalysts: more recent eras get higher weight.
    // Simple linear weighting: era 0 gets weight 1, era 1 gets weight 2, etc.
    let weights: Vec<f64> = eras
        .iter()
        .enumerate()
        .map(|(index, era)| (index + 1) as f64 * era.weight.max(0.0))
        .collect();
    let total_weight: f64 = weights.iter().sum();

    for (i, &w) in weights.iter().enumerate() {
        let share = if total_weight > 0.0 {
            (remaining as f64 * w / total_weight).floor() as usize
        } else {
            0
        };
        allocs[i] += share;
    }

    // Distribute rounding remainder to most recent eras
    let distributed: usize = allocs.iter().sum();
    let mut leftover = total.saturating_sub(distributed);
    for i in (0..eras.len()).rev() {
        if leftover == 0 {
            break;
        }
        allocs[i] += 1;
        leftover -= 1;
    }

    allocs
}

/// Sample context strings from era-bucketed occurrences, respecting per-era limits.
/// `max_total` is the overall cap on context samples.
pub fn sample_contexts_by_era(
    eras: &[Era],
    bucketed_occs: &[Vec<&EntityOccurrence>],
    max_total: usize,
) -> Vec<(usize, String)> {
    // (era_index, context_string) pairs
    if eras.is_empty() {
        // No eras — fall back to flat sampling from unmatched bucket
        if let Some(unmatched) = bucketed_occs.last() {
            return unmatched
                .iter()
                .filter_map(|o| o.context.as_ref().map(|c| (0, c.clone())))
                .take(max_total)
                .collect();
        }
        return Vec::new();
    }

    // Allocate sample slots proportionally to era note count, min 1 per era
    let era_counts: Vec<usize> = eras.iter().map(|e| e.count.max(1) as usize).collect();
    let total_count: usize = era_counts.iter().sum();
    let mut slots: Vec<usize> = era_counts
        .iter()
        .map(|&c| (c * max_total / total_count).max(1))
        .collect();

    // Clamp to max_total
    let slot_sum: usize = slots.iter().sum();
    if slot_sum > max_total {
        // Trim from largest eras first
        let mut excess = slot_sum - max_total;
        let mut indices: Vec<usize> = (0..slots.len()).collect();
        indices.sort_by(|a, b| slots[*b].cmp(&slots[*a]));
        for &i in &indices {
            if excess == 0 {
                break;
            }
            let trim = (slots[i] - 1).min(excess);
            slots[i] -= trim;
            excess -= trim;
        }
    }

    let mut results: Vec<(usize, String)> = Vec::new();
    for (era_idx, slot_count) in slots.iter().enumerate() {
        if era_idx >= bucketed_occs.len() {
            break;
        }
        let occs = &bucketed_occs[era_idx];
        let contexts: Vec<_> = occs
            .iter()
            .filter_map(|o| o.context.as_ref().map(|c| (era_idx, c.clone())))
            .take(*slot_count)
            .collect();
        results.extend(contexts);
    }

    // Fill remaining slots from unmatched if we have room
    let unmatched_idx = bucketed_occs.len() - 1;
    if results.len() < max_total && unmatched_idx < bucketed_occs.len() {
        let remaining = max_total - results.len();
        let extra: Vec<_> = bucketed_occs[unmatched_idx]
            .iter()
            .filter_map(|o| o.context.as_ref().map(|c| (eras.len(), c.clone())))
            .take(remaining)
            .collect();
        results.extend(extra);
    }

    results
}

// ---------------------------------------------------------------------------
// Bucketing strategies
// ---------------------------------------------------------------------------

/// Each month is its own era.
fn bucket_monthly(timeline_buckets: &HashMap<String, i64>) -> Vec<Era> {
    let mut months: Vec<(&String, &i64)> = timeline_buckets
        .iter()
        .filter(|&(_, &count)| count > 0)
        .collect();
    months.sort_by_key(|(k, _)| k.as_str());

    months
        .into_iter()
        .map(|(key, &count)| {
            let label = format_month(key);
            Era {
                label,
                start: key.clone(),
                end: key.clone(),
                count,
                weight: count as f64,
            }
        })
        .collect()
}

/// Aggregate months into calendar quarters.
fn bucket_quarterly(timeline_buckets: &HashMap<String, i64>) -> Vec<Era> {
    let mut quarters: HashMap<(i32, i32), i64> = HashMap::new();
    for (key, &count) in timeline_buckets {
        if count <= 0 {
            continue;
        }
        let (y, m) = parse_month_key(key);
        let q = (m - 1) / 3;
        *quarters.entry((y, q)).or_insert(0) += count;
    }

    let mut sorted: Vec<((i32, i32), i64)> = quarters.into_iter().collect();
    sorted.sort_by_key(|&((y, q), _)| (y, q));

    sorted
        .into_iter()
        .map(|((y, q), count)| {
            let start_month = q * 3 + 1;
            let end_month = q * 3 + 3;
            Era {
                label: format!("{}-Q{}", y, q + 1),
                start: format!("{:04}-{:02}", y, start_month),
                end: format!("{:04}-{:02}", y, end_month),
                count,
                weight: count as f64,
            }
        })
        .collect()
}

/// Hybrid: half-year eras for periods older than 2 years, quarterly for recent.
fn bucket_hybrid(timeline_buckets: &HashMap<String, i64>) -> Vec<Era> {
    let now = chrono::Utc::now();
    let cutoff_y = now.year() - 2;
    let cutoff_m = now.month() as i32;
    // Months at or after (cutoff_y, cutoff_m) get quarterly; older get half-year

    let mut old_halves: HashMap<(i32, i32), i64> = HashMap::new(); // (year, half: 0 or 1)
    let mut recent_quarters: HashMap<(i32, i32), i64> = HashMap::new();

    for (key, &count) in timeline_buckets {
        if count <= 0 {
            continue;
        }
        let (y, m) = parse_month_key(key);

        if y * 12 + m < cutoff_y * 12 + cutoff_m {
            let h = if m <= 6 { 0 } else { 1 };
            *old_halves.entry((y, h)).or_insert(0) += count;
        } else {
            let q = (m - 1) / 3;
            *recent_quarters.entry((y, q)).or_insert(0) += count;
        }
    }

    let mut eras: Vec<Era> = Vec::new();

    // Old halves
    let mut sorted_halves: Vec<((i32, i32), i64)> = old_halves.into_iter().collect();
    sorted_halves.sort_by_key(|&((y, h), _)| (y, h));
    for ((y, h), count) in sorted_halves {
        let (start_month, end_month) = if h == 0 { (1, 6) } else { (7, 12) };
        let label = format!("{}-H{}", y, h + 1);
        eras.push(Era {
            label,
            start: format!("{:04}-{:02}", y, start_month),
            end: format!("{:04}-{:02}", y, end_month),
            count,
            weight: count as f64,
        });
    }

    // Recent quarters
    let mut sorted_quarters: Vec<((i32, i32), i64)> = recent_quarters.into_iter().collect();
    sorted_quarters.sort_by_key(|&((y, q), _)| (y, q));
    for ((y, q), count) in sorted_quarters {
        let start_month = q * 3 + 1;
        let end_month = q * 3 + 3;
        eras.push(Era {
            label: format!("{}-Q{}", y, q + 1),
            start: format!("{:04}-{:02}", y, start_month),
            end: format!("{:04}-{:02}", y, end_month),
            count,
            weight: count as f64,
        });
    }

    eras
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Distance in months between two "YYYY-MM" keys.
fn month_distance(a: &str, b: &str) -> i32 {
    let (ay, am) = parse_month_key(a);
    let (by, bm) = parse_month_key(b);
    (by * 12 + bm) - (ay * 12 + am)
}

fn parse_month_key(key: &str) -> (i32, i32) {
    let mut parts = key.splitn(2, '-');
    let year: i32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let month: i32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    (year, month)
}

fn occurrence_month_key(occ: &EntityOccurrence) -> Option<String> {
    let ts = occ.effective_date.unwrap_or(occ.created_at);
    chrono::DateTime::from_timestamp_millis(ts).map(|dt| dt.format("%Y-%m").to_string())
}

/// Build a human-readable label for an era span.
fn format_era_label(start: &str, end: &str) -> String {
    if start == end {
        // Single month
        return format_month(start);
    }

    let (sy, sm) = parse_month_key(start);
    let (ey, em) = parse_month_key(end);

    // Same year, check for quarter alignment
    if sy == ey {
        let sq = (sm - 1) / 3;
        let eq = (em - 1) / 3;
        if sq == eq {
            return format!("{}-Q{}", sy, sq + 1);
        }
        return format!("{} to {}", format_month(start), format_month(end));
    }

    format!("{} to {}", format_month(start), format_month(end))
}

fn format_month(key: &str) -> String {
    let (y, m) = parse_month_key(key);
    let month_name = match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    };
    format!("{} {}", month_name, y)
}

/// Merge era at `idx` into its nearest neighbor.
fn merge_into_neighbor(eras: &mut Vec<Era>, idx: usize) {
    if eras.len() <= 1 {
        return;
    }

    let merge_into = if idx == 0 {
        1
    } else if idx == eras.len() - 1 {
        idx - 1
    } else {
        // Pick the neighbor with fewer notes (to keep sizes balanced)
        if eras[idx - 1].count <= eras[idx + 1].count {
            idx - 1
        } else {
            idx + 1
        }
    };

    let (keep, remove) = if merge_into < idx {
        (merge_into, idx)
    } else {
        (idx, merge_into)
    };

    let removed = eras.remove(remove);
    let kept = &mut eras[keep];
    kept.count += removed.count;
    kept.weight += removed.weight;
    if removed.start < kept.start {
        kept.start = removed.start;
    }
    if removed.end > kept.end {
        kept.end = removed.end;
    }
    kept.label = format_era_label(&kept.start, &kept.end);
}

/// Merge eras below `min_notes` into their nearest neighbor.
fn merge_thin_eras(eras: &mut Vec<Era>, min_notes: i64) {
    loop {
        if eras.len() <= 1 {
            break;
        }
        let thin_idx = match eras.iter().position(|e| e.count < min_notes) {
            Some(i) => i,
            None => break,
        };
        merge_into_neighbor(eras, thin_idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buckets(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn empty_buckets_produce_no_eras() {
        let eras = cluster_eras(&HashMap::new(), 3);
        assert!(eras.is_empty());
    }

    #[test]
    fn single_month_is_one_era() {
        let b = buckets(&[("2025-03", 5)]);
        let eras = cluster_eras(&b, 3);
        assert_eq!(eras.len(), 1);
        assert_eq!(eras[0].count, 5);
    }

    #[test]
    fn gap_splits_into_two_eras() {
        let b = buckets(&[
            ("2025-01", 5),
            ("2025-02", 3),
            // gap: 2025-03, 2025-04, 2025-05 (3 months)
            ("2025-06", 4),
            ("2025-07", 2),
        ]);
        let eras = cluster_eras(&b, 3);
        assert_eq!(eras.len(), 2);
        assert_eq!(eras[0].count, 8);
        assert_eq!(eras[1].count, 6);
    }

    #[test]
    fn consecutive_months_stay_together() {
        let b = buckets(&[("2025-01", 3), ("2025-02", 4), ("2025-03", 5)]);
        let eras = cluster_eras(&b, 3);
        assert_eq!(eras.len(), 1);
        assert_eq!(eras[0].count, 12);
    }

    #[test]
    fn thin_eras_get_merged() {
        let b = buckets(&[
            ("2025-01", 10),
            ("2025-02", 10),
            // gap
            ("2025-06", 1), // thin — should merge into next
            // gap
            ("2025-10", 8),
            ("2025-11", 7),
        ]);
        let eras = cluster_eras(&b, 3);
        // The single-note era at 2025-06 should merge somewhere
        assert!(eras.iter().all(|e| e.count >= 3));
    }

    #[test]
    fn quarter_label_detection() {
        let label = format_era_label("2025-01", "2025-03");
        assert_eq!(label, "2025-Q1");
    }

    #[test]
    fn catalyst_allocation_respects_minimum() {
        let eras = vec![
            Era {
                label: "A".into(),
                start: "2025-01".into(),
                end: "2025-03".into(),
                count: 20,
                weight: 20.0,
            },
            Era {
                label: "B".into(),
                start: "2025-06".into(),
                end: "2025-08".into(),
                count: 5,
                weight: 5.0,
            },
        ];
        let allocs = allocate_catalysts_to_eras(&eras, 7, 2);
        assert_eq!(allocs.iter().sum::<usize>(), 7);
        assert!(allocs.iter().all(|&a| a >= 2));
        assert!(
            allocs[0] > allocs[1],
            "the older era's larger additive evidence weight should outweigh recency"
        );
    }

    #[test]
    fn catalyst_allocation_single_era() {
        let eras = vec![Era {
            label: "A".into(),
            start: "2025-01".into(),
            end: "2025-03".into(),
            count: 10,
            weight: 10.0,
        }];
        let allocs = allocate_catalysts_to_eras(&eras, 7, 2);
        assert_eq!(allocs, vec![7]);
    }

    #[test]
    fn catalyst_allocation_more_eras_than_budget() {
        // 8 eras, budget of 7 — should still give oldest era 1 for contrast
        let eras: Vec<Era> = (0..8)
            .map(|i| Era {
                label: format!("E{i}"),
                start: format!("2025-{:02}", i + 1),
                end: format!("2025-{:02}", i + 1),
                count: 10,
                weight: 10.0,
            })
            .collect();
        let allocs = allocate_catalysts_to_eras(&eras, 7, 1);
        assert_eq!(allocs.iter().sum::<usize>(), 7);
        // Oldest era should get at least 1
        assert!(
            allocs[0] >= 1,
            "oldest era should get at least 1, got {}",
            allocs[0]
        );
        // No era should get 0 if possible, but with 8 eras and 7 budget, one must
        // The one that gets 0 should NOT be the oldest or most recent
        assert!(
            allocs.last().unwrap() >= &1,
            "most recent era should get at least 1"
        );
    }
}
