//! Optional source text filters. Templates are bundled data; the configured
//! source chooses whether to apply them after its SQL has decoded text.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::embedding::{get_embedding_service, l2_normalize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Class {
    Meaningful,
    Mundane,
    Borderline,
}

#[derive(Deserialize)]
struct Template {
    class: Class,
    kind: String,
    text: String,
}

#[derive(Deserialize)]
struct TemplateFile {
    version: u32,
    templates: Vec<Template>,
}

struct TemplateVectors {
    classes: Vec<Class>,
    kinds: Vec<String>,
    vectors: Vec<Vec<f32>>,
}

static APPLE_NOTES_V1: OnceLock<TemplateVectors> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct Classification {
    pub(crate) is_mundane: bool,
    pub(crate) mundane_votes: usize,
    pub(crate) nearest_kind: String,
}

/// Classify decoded source text with the built-in ESE backend. A note needs
/// four mundane neighbors, a list-type nearest neighbor, and list-like line
/// structure before exclusion. Ambiguous notes remain indexed.
pub(crate) fn classify_apple_notes_v1(texts: &[String]) -> Result<Vec<Classification>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let service = get_embedding_service(Path::new("."))?;
    let templates = if let Some(templates) = APPLE_NOTES_V1.get() {
        templates
    } else {
        let file: TemplateFile =
            serde_json::from_str(include_str!("apple_notes_templates_v1.json"))
                .context("parse bundled Apple Notes templates")?;
        ensure!(
            file.version == 1,
            "unsupported Apple Notes template version"
        );
        ensure!(
            file.templates.len() >= 5,
            "Apple Notes needs at least five templates"
        );
        let samples: Vec<&str> = file.templates.iter().map(|t| t.text.as_str()).collect();
        let mut vectors = service.embed_documents(&samples)?;
        for vector in &mut vectors {
            l2_normalize(vector);
        }
        let _ = APPLE_NOTES_V1.set(TemplateVectors {
            classes: file.templates.iter().map(|t| t.class).collect(),
            kinds: file.templates.into_iter().map(|t| t.kind).collect(),
            vectors,
        });
        APPLE_NOTES_V1.get().expect("template vectors initialized")
    };

    let samples: Vec<String> = texts
        .iter()
        .map(|text| representative_sample(text))
        .collect();
    let references: Vec<&str> = samples.iter().map(String::as_str).collect();
    let mut vectors = service.embed_documents(&references)?;
    let mut results = Vec::with_capacity(vectors.len());
    for (text, vector) in texts.iter().zip(&mut vectors) {
        l2_normalize(vector);
        let mut nearest: Vec<(usize, f32)> = templates
            .vectors
            .iter()
            .enumerate()
            .map(|(index, template)| (index, vector.iter().zip(template).map(|(a, b)| a * b).sum()))
            .collect();
        nearest.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let top = &nearest[..5];
        let mundane_votes = top
            .iter()
            .filter(|(index, _)| templates.classes[*index] == Class::Mundane)
            .count();
        let nearest_kind = templates.kinds[top[0].0].clone();
        results.push(Classification {
            is_mundane: mundane_votes >= 4
                && matches!(nearest_kind.as_str(), "todo_list" | "shopping_list")
                && looks_like_list(text),
            mundane_votes,
            nearest_kind,
        });
    }
    Ok(results)
}

/// A semantic match alone can confuse short prose with lists. Require the
/// line structure seen in the notes this filter is meant to remove.
fn looks_like_list(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .skip(1) // The first line is the title in the Apple Notes preset.
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let short = lines
        .iter()
        .filter(|line| line.chars().count() <= 120)
        .count();
    lines.len() >= 3 && short >= 3 && short * 10 >= lines.len() * 7
}

fn representative_sample(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 1_000 {
        return text.to_string();
    }
    if chars.len() < 5_000 {
        return format!(
            "{}\n...\n{}",
            chars[..500].iter().collect::<String>(),
            chars[chars.len() - 500..].iter().collect::<String>()
        );
    }
    let middle = chars.len() / 2;
    format!(
        "{}\n...\n{}\n...\n{}",
        chars[..300].iter().collect::<String>(),
        chars[middle - 200..middle + 200].iter().collect::<String>(),
        chars[chars.len() - 300..].iter().collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examples_cover_both_sides_without_excluding_reflection() {
        let texts = vec![
            "Groceries\nMilk\nEggs\nBread\nCoffee\nBananas\nOlive oil".into(),
            "To do today\nCall dentist\nBuy milk\nReturn package\nSchedule oil change".into(),
            "I've been thinking about how conversations change when I listen longer. What surprised me was that the pause created space for a better question, and I want to try that again.".into(),
            "A letter\nHi friend, I hope you are well. I have been thinking about our trip together. We walked for hours, talked about the year ahead, and remembered how much these conversations matter.\nLet's meet again soon.".into(),
        ];
        let labels = classify_apple_notes_v1(&texts).unwrap();
        assert!(labels[0].is_mundane, "grocery vote: {:?}", labels[0]);
        assert!(labels[1].is_mundane, "todo vote: {:?}", labels[1]);
        assert!(!labels[2].is_mundane, "reflection vote: {:?}", labels[2]);
        assert!(!labels[3].is_mundane, "letter vote: {:?}", labels[3]);
    }
}
