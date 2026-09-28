use enzyme_core::db::Database;
use enzyme_core::models::{EntityOccurrence, EntityType};

#[test]
fn arena_partial_remote_upsert_preserves_other_channel_membership() {
    let db = Database::open_in_memory().unwrap();
    let first = occurrence("arena:block:42", 1_700_000_000_000);
    let second = occurrence("arena:block:42", 1_710_000_000_000);
    db.upsert_entity_occurrences_partial(&[
        ("arena:channels/channel/1", &EntityType::Collection, &first),
        ("arena:channels/channel/2", &EntityType::Collection, &second),
    ])
    .unwrap();

    let refreshed = occurrence("arena:block:42", 1_720_000_000_000);
    db.upsert_entity_occurrences_partial(&[(
        "arena:channels/channel/1",
        &EntityType::Collection,
        &refreshed,
    )])
    .unwrap();

    let entities = db
        .get_entities_by_names(&[
            ("arena:channels/channel/1", EntityType::Collection),
            ("arena:channels/channel/2", EntityType::Collection),
        ])
        .unwrap();
    assert_eq!(entities.len(), 2);
    for entity in entities {
        let occurrences = db.get_entity_occurrences(entity.id.unwrap()).unwrap();
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].file_path, "arena:block:42");
        let expected = if entity.name == "arena:channels/channel/1" {
            1_720_000_000_000
        } else {
            1_710_000_000_000
        };
        assert_eq!(occurrences[0].effective_date, Some(expected));
    }
}

fn occurrence(file_path: &str, effective_date: i64) -> EntityOccurrence {
    EntityOccurrence {
        id: None,
        entity_id: 0,
        file_path: file_path.into(),
        context: Some("Evidence".into()),
        created_at: effective_date,
        effective_date: Some(effective_date),
        date_source: "arena_connection".into(),
        weight: 1.0,
    }
}
