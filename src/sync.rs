//! The pure merge behind `lessons sync` (plan step S2): BASE (the lessons at the last synced commit), OURS (the local
//! DB) and THEIRS (the fetched tip) become one set of rows plus a conflict queue. Nothing here touches git, the DB or
//! a clock. Every rule depends on the unordered pair (ours, theirs) plus BASE, never on which side is "ours", so
//! every machine computes the same rows and the same queue.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::store::Lesson;

mod cycle;
mod files;
mod git;
mod run;

pub use cycle::exit_for;
pub use git::redact_credentials;
pub use run::sync_configured;

/// Added to `tags` by a combine in `lessons resolve`, so `lessons list --tag needs-reword` finds the lesson.
pub const NEEDS_REWORD_TAG: &str = "needs-reword";

/// A lesson field merged on its own. `id`, `created_at`, `updated_at` and `hits` are not in it: they have their own
/// rules. The serde names are the `field` values in `<domain>.conflicts.jsonl` and match the `Lesson` field names.
/// The declaration order is the sort order of a queue group's key (`id`, then field), so it decides the order of the
/// committed `<domain>.conflicts.jsonl`: append new fields at the end and never reorder (a test pins it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergedField {
    Title,
    Context,
    Problem,
    RootCause,
    Resolution,
    FailedTries,
    Tags,
    EnvVersions,
    SupersededBy,
}

pub const MERGED_FIELDS: [MergedField; 9] = [
    MergedField::Title,
    MergedField::Context,
    MergedField::Problem,
    MergedField::RootCause,
    MergedField::Resolution,
    MergedField::FailedTries,
    MergedField::Tags,
    MergedField::EnvVersions,
    MergedField::SupersededBy,
];

/// One value of a conflicting field and the `updated_at` of the row it came from. The derived order is the
/// declaration order, (value, updated_at), which is the order a group stores its versions in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub value: Option<String>,
    pub updated_at: String,
}

/// One queue group: every value seen for one field of one lesson. Its key, (id, field), is the same on every machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: String,
    pub field: MergedField,
    pub base: Option<String>,
    pub versions: Vec<Version>,
}

/// A queue group's key: the lesson id and the field.
pub type GroupKey = (String, MergedField);

pub(super) fn group_key(group: &Conflict) -> GroupKey {
    (group.id.clone(), group.field)
}

/// What `merge_lessons` returns. `conflicts` go to the queue; `unqueued` are `superseded_by` conflicts, which keep the
/// default winner and are only logged, because `store::update` cannot set that field.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeOutcome {
    pub merged: Vec<Lesson>,
    pub conflicts: Vec<Conflict>,
    pub unqueued: Vec<Conflict>,
}

/// What `settle_queue` returns. `cleared` are groups whose lesson no longer exists, for the caller to log.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub merged: Vec<Lesson>,
    pub queue: Vec<Conflict>,
    pub cleared: Vec<Conflict>,
}

/// The value of `field` in `lesson`. `title` and `problem` are required, so they are always `Some`.
pub fn field_value(lesson: &Lesson, field: MergedField) -> Option<&str> {
    // Exhaustive on purpose: a new MergedField does not compile until it is read here.
    match field {
        MergedField::Title => Some(lesson.title.as_str()),
        MergedField::Context => lesson.context.as_deref(),
        MergedField::Problem => Some(lesson.problem.as_str()),
        MergedField::RootCause => lesson.root_cause.as_deref(),
        MergedField::Resolution => lesson.resolution.as_deref(),
        MergedField::FailedTries => lesson.failed_tries.as_deref(),
        MergedField::Tags => lesson.tags.as_deref(),
        MergedField::EnvVersions => lesson.env_versions.as_deref(),
        MergedField::SupersededBy => lesson.superseded_by.as_deref(),
    }
}

/// `lesson` with `field` set to `value` and every other field unchanged. A `None` for a required field is the empty
/// string, the same value `field_value` would then read back as `Some("")`.
pub fn with_field(lesson: Lesson, field: MergedField, value: Option<String>) -> Lesson {
    // Struct update keeps every other field; exhaustive, so a new field is never skipped.
    match field {
        MergedField::Title => Lesson {
            title: value.unwrap_or_default(),
            ..lesson
        },
        MergedField::Context => Lesson {
            context: value,
            ..lesson
        },
        MergedField::Problem => Lesson {
            problem: value.unwrap_or_default(),
            ..lesson
        },
        MergedField::RootCause => Lesson {
            root_cause: value,
            ..lesson
        },
        MergedField::Resolution => Lesson {
            resolution: value,
            ..lesson
        },
        MergedField::FailedTries => Lesson {
            failed_tries: value,
            ..lesson
        },
        MergedField::Tags => Lesson {
            tags: value,
            ..lesson
        },
        MergedField::EnvVersions => Lesson {
            env_versions: value,
            ..lesson
        },
        MergedField::SupersededBy => Lesson {
            superseded_by: value,
            ..lesson
        },
    }
}

/// The default winner of two versions, side-independent: the newer `updated_at`; on a tie the larger value, `None`
/// read as the empty string; then `Some("")` over `None`, so two different versions never tie.
fn newer_or_larger<'a>(left: &'a Version, right: &'a Version) -> &'a Version {
    // A total order, so the winner never depends on which side is passed first.
    let fn_winner_order = |first: &&Version, second: &&Version| {
        first
            .updated_at
            .cmp(&second.updated_at)
            .then_with(|| {
                first
                    .value
                    .as_deref()
                    .unwrap_or("")
                    .cmp(second.value.as_deref().unwrap_or(""))
            })
            .then_with(|| first.value.cmp(&second.value))
    };
    std::cmp::max_by(left, right, fn_winner_order) // equal only for identical versions, so the tie side never matters
}

/// True when the merged fields of the two rows differ; `hits` and `updated_at` are ignored.
fn fields_differ(left: &Lesson, right: &Lesson) -> bool {
    // Only merged fields count: a hit or a newer timestamp alone is not an edit.
    MERGED_FIELDS
        .iter()
        .any(|&field| field_value(left, field) != field_value(right, field))
}

/// The per-field three-way rule. Returns the value to store and, for a conflict, the group to queue.
fn merge_field(
    base: Option<&Lesson>,
    ours: &Lesson,
    theirs: &Lesson,
    field: MergedField,
) -> (Option<String>, Option<Conflict>) {
    // Values are compared, never timestamps: an older edit still beats an unchanged copy.
    let ours_value: Option<&str> = field_value(ours, field);
    let theirs_value: Option<&str> = field_value(theirs, field);
    // None: no base row at all.
    let base_value: Option<Option<&str>> = base.map(|base_row| field_value(base_row, field));
    let fn_conflict = || -> (Option<String>, Option<Conflict>) {
        let fn_version_of = |value: Option<&str>, row: &Lesson| Version {
            value: value.map(String::from),
            updated_at: row.updated_at.clone(),
        };
        let (ours_version, theirs_version) = (
            fn_version_of(ours_value, ours),
            fn_version_of(theirs_value, theirs),
        );
        let winner: Option<String> = newer_or_larger(&ours_version, &theirs_version)
            .value
            .clone();
        let group = Conflict {
            id: ours.id.clone(),
            field,
            base: base_value.flatten().map(String::from),
            versions: BTreeSet::from([ours_version, theirs_version])
                .into_iter()
                .collect(),
        };
        (winner, Some(group))
    };
    // Some(value): the rule settles the field; None: a conflict.
    let settled: Option<Option<&str>> = match base_value {
        _ if ours_value == theirs_value => Some(ours_value),
        Some(base_text) if base_text == ours_value => Some(theirs_value),
        Some(base_text) if base_text == theirs_value => Some(ours_value),
        Some(_) | None => None, // all three differ, or no base row to tell which side changed
    };
    settled.map_or_else(fn_conflict, |value| (value.map(String::from), None))
}

/// One merged field and the value it settled on.
type FieldValue = (MergedField, Option<String>);

/// Both sides hold the lesson: every merged field by the three-way rule, `hits` and `updated_at` the larger,
/// `created_at` the earlier (all side-independent).
fn merge_both_present(
    base: Option<&Lesson>,
    ours: &Lesson,
    theirs: &Lesson,
) -> (Lesson, Vec<Conflict>) {
    // Each field merges on its own, so edits to different fields on two machines both survive.
    let row_with_counters = Lesson {
        hits: ours.hits.max(theirs.hits),
        updated_at: ours.updated_at.clone().max(theirs.updated_at.clone()),
        created_at: ours.created_at.clone().min(theirs.created_at.clone()),
        ..ours.clone()
    };
    let (field_values, field_groups): (Vec<FieldValue>, Vec<Option<Conflict>>) = MERGED_FIELDS
        .iter()
        .map(|&field| {
            let (value, group) = merge_field(base, ours, theirs, field);
            ((field, value), group)
        })
        .unzip();
    let row: Lesson = field_values
        .into_iter()
        .fold(row_with_counters, |row, (field, value)| {
            with_field(row, field, value)
        });
    (row, field_groups.into_iter().flatten().collect())
}

/// Merges BASE, OURS and THEIRS per lesson id (the merge table in the plan's Design section). Output is sorted by id.
pub fn merge_lessons(base: &[Lesson], ours: &[Lesson], theirs: &[Lesson]) -> MergeOutcome {
    // Visits the ids of either side in id order, so the output is sorted and side-independent.
    // Borrowed, not cloned: only the rows that end up in MERGED are copied.
    fn lessons_by_id(rows: &[Lesson]) -> BTreeMap<&str, &Lesson> {
        rows.iter().map(|row| (row.id.as_str(), row)).collect()
    }
    let (base_by_id, ours_by_id, theirs_by_id) = (
        lessons_by_id(base),
        lessons_by_id(ours),
        lessons_by_id(theirs),
    );
    // An id only in BASE was deleted on both sides, so it is not even visited.
    let all_ids: BTreeSet<&str> = ours_by_id
        .keys()
        .chain(theirs_by_id.keys())
        .copied()
        .collect();
    let (rows, groups): (Vec<Option<Lesson>>, Vec<Vec<Conflict>>) = all_ids
        .into_iter()
        .map(|lesson_id| {
            match (
                base_by_id.get(lesson_id).copied(),
                ours_by_id.get(lesson_id).copied(),
                theirs_by_id.get(lesson_id).copied(),
            ) {
                (base_row, Some(ours_row), Some(theirs_row)) => {
                    let (row, conflicts) = merge_both_present(base_row, ours_row, theirs_row);
                    (Some(row), conflicts)
                }
                (None, Some(added_row), None) | (None, None, Some(added_row)) => {
                    (Some(added_row.clone()), Vec::new())
                }
                // Deleted on one side: kept only when the other side changed it, because an edit beats a delete.
                (Some(base_row), Some(kept_row), None) | (Some(base_row), None, Some(kept_row)) => {
                    (
                        fields_differ(base_row, kept_row).then(|| kept_row.clone()),
                        Vec::new(),
                    )
                }
                (_, None, None) => (None, Vec::new()),
            }
        })
        .unzip();
    let (unqueued, conflicts): (Vec<Conflict>, Vec<Conflict>) = groups
        .into_iter()
        .flatten()
        .partition(|group| group.field == MergedField::SupersededBy);
    MergeOutcome {
        merged: rows.into_iter().flatten().collect(),
        conflicts,
        unqueued,
    }
}

/// The union of two groups with one key: versions keyed by value (the larger `updated_at` kept for a value seen on
/// both), stored sorted; `base` the larger of the two.
fn union_groups(left: &Conflict, right: &Conflict) -> Conflict {
    // A version set keyed by value: a value seen on both sides keeps its newer updated_at.
    let ordered: Vec<Version> = left
        .versions
        .iter()
        .chain(&right.versions)
        .cloned()
        .collect::<BTreeSet<Version>>()
        .into_iter()
        .collect();
    let versions: Vec<Version> = ordered
        .chunk_by(|first, second| first.value == second.value)
        .filter_map(|same_value| same_value.last().cloned())
        .collect();
    Conflict {
        base: left.base.clone().max(right.base.clone()),
        versions,
        ..left.clone()
    }
}

/// Merges the conflict queues: (1) the three-way over BASE, LIVE and THEIRS, where a BASE group missing on one side was
/// resolved there and is dropped; (2) then this cycle's `new_conflicts` are unioned in, so phase 1 never drops them.
pub fn merge_queue(
    base: &[Conflict],
    live: &[Conflict],
    theirs: &[Conflict],
    new_conflicts: &[Conflict],
) -> Vec<Conflict> {
    // The BASE drop runs before the new conflicts join, so a fresh conflict on a resolved key survives.
    // Borrowed, not cloned: only the groups that survive are copied.
    fn groups_by_key(groups: &[Conflict]) -> BTreeMap<GroupKey, &Conflict> {
        groups
            .iter()
            .map(|group| (group_key(group), group))
            .collect()
    }
    let (base_by_key, live_by_key, theirs_by_key, new_by_key) = (
        groups_by_key(base),
        groups_by_key(live),
        groups_by_key(theirs),
        groups_by_key(new_conflicts),
    );
    let fn_union_of = |left: Option<&Conflict>, right: Option<&Conflict>| -> Option<Conflict> {
        match (left, right) {
            (Some(left_group), Some(right_group)) => Some(union_groups(left_group, right_group)),
            (Some(only_group), None) | (None, Some(only_group)) => Some(only_group.clone()),
            (None, None) => None,
        }
    };
    let phase_one: BTreeMap<GroupKey, Conflict> = live_by_key
        .keys()
        .chain(theirs_by_key.keys())
        .collect::<BTreeSet<&GroupKey>>()
        .into_iter()
        .filter(|key| {
            let resolved_on_one_side: bool = base_by_key.contains_key(*key)
                && !(live_by_key.contains_key(*key) && theirs_by_key.contains_key(*key));
            !resolved_on_one_side
        })
        .filter_map(|key| {
            fn_union_of(
                live_by_key.get(key).copied(),
                theirs_by_key.get(key).copied(),
            )
            .map(|group| (key.clone(), group))
        })
        .collect();
    phase_one
        .keys()
        .chain(new_by_key.keys())
        .collect::<BTreeSet<&GroupKey>>()
        .into_iter()
        .filter_map(|key| fn_union_of(phase_one.get(key), new_by_key.get(key).copied()))
        .collect()
}

/// True when two values of `field` count as the same version. For `tags` the space-separated sets are compared with
/// `NEEDS_REWORD_TAG` left out, so a combine on another field (which appends it) adds no version to a `tags` group.
fn same_field_value(field: MergedField, left: Option<&str>, right: Option<&str>) -> bool {
    // Tags compare as sets without needs-reword, so a combine elsewhere adds no tags version.
    fn tag_set_without_reword(tags: Option<&str>) -> BTreeSet<&str> {
        tags.unwrap_or("")
            .split_whitespace()
            .filter(|tag| *tag != NEEDS_REWORD_TAG)
            .collect()
    }
    match field {
        MergedField::Tags => tag_set_without_reword(left) == tag_set_without_reword(right),
        MergedField::Title
        | MergedField::Context
        | MergedField::Problem
        | MergedField::RootCause
        | MergedField::Resolution
        | MergedField::FailedTries
        | MergedField::EnvVersions
        | MergedField::SupersededBy => left == right,
    }
}

/// Applies the queue to the merged rows: a field conflicted this cycle (`conflicted`) takes the default winner over ALL
/// versions of its group; a group whose lesson is gone is cleared; a merged value missing from its group is added as a
/// version with the row's `updated_at`. Every step reads only MERGED and the queue, so every machine agrees.
pub fn settle_queue(merged: Vec<Lesson>, queue: Vec<Conflict>, conflicted: &[GroupKey]) -> Settled {
    // The winner first, then the add-a-version check, so a default winner never adds a version.
    let lesson_ids: BTreeSet<String> = merged.iter().map(|row| row.id.clone()).collect();
    let (kept, cleared): (Vec<Conflict>, Vec<Conflict>) = queue
        .into_iter()
        .partition(|group| lesson_ids.contains(&group.id));
    let kept_by_key: BTreeMap<GroupKey, Conflict> = kept
        .into_iter()
        .map(|group| (group_key(&group), group))
        .collect();
    let settled_rows: Vec<Lesson> = merged
        .into_iter()
        .map(|row| {
            MERGED_FIELDS.iter().fold(row, |row, &field| {
                let key: GroupKey = (row.id.clone(), field);
                let all_versions_winner: Option<Option<String>> = kept_by_key
                    .get(&key)
                    .filter(|_| conflicted.contains(&key))
                    .and_then(|group| group.versions.iter().reduce(newer_or_larger))
                    .map(|winner| winner.value.clone());
                match all_versions_winner {
                    Some(winner_value) => with_field(row, field, winner_value),
                    None => row,
                }
            })
        })
        .collect();
    let rows_by_id: BTreeMap<&str, &Lesson> = settled_rows
        .iter()
        .map(|row| (row.id.as_str(), row))
        .collect();
    let settled_queue: Vec<Conflict> = kept_by_key
        .into_values()
        .filter_map(|group| {
            rows_by_id.get(group.id.as_str()).map(|row| {
                let merged_value: Option<&str> = field_value(row, group.field);
                let already_a_version: bool = group.versions.iter().any(|version| {
                    same_field_value(group.field, version.value.as_deref(), merged_value)
                });
                if already_a_version {
                    group
                } else {
                    let added = Conflict {
                        versions: vec![Version {
                            value: merged_value.map(String::from),
                            updated_at: row.updated_at.clone(),
                        }],
                        ..group.clone()
                    };
                    union_groups(&group, &added)
                }
            })
        })
        .collect();
    Settled {
        merged: settled_rows,
        queue: settled_queue,
        cleared,
    }
}

// Unit tests only: every function here is pure, with no shared state, clock or I/O, so no race or back-to-back case
// applies at this layer. The idempotency a back-to-back sync needs is pinned by
// `merging_a_result_again_changes_nothing`; the races (two machines, push rejected, crash points) are E2E rows of
// later steps.
#[cfg(test)]
#[expect(
    clippy::cloned_ref_to_slice_refs,
    reason = "a one-row `&[row.clone()]` reads as the slice merge_lessons takes"
)]
mod tests {
    use super::*;

    const EARLY: &str = "2026-11-01T00:00:00Z";
    const MIDDLE: &str = "2026-11-02T00:00:00Z";
    const LATE: &str = "2026-11-03T00:00:00Z";
    const LESSON_ID: &str = "01KAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER_ID: &str = "01KBBBBBBBBBBBBBBBBBBBBBBB";
    const FEW_HITS: i64 = 2;
    const MORE_HITS: i64 = 5;

    /// A lesson every field of which differs from the values the tests set, so no assertion passes on the pre-state.
    fn sample_lesson(lesson_id: &str, updated_at: &str) -> Lesson {
        Lesson {
            id: lesson_id.to_string(),
            created_at: EARLY.to_string(),
            updated_at: updated_at.to_string(),
            title: "base title".to_string(),
            context: Some("base context".to_string()),
            problem: "base problem".to_string(),
            root_cause: Some("base root cause".to_string()),
            resolution: Some("base resolution".to_string()),
            failed_tries: Some("base failed tries".to_string()),
            tags: Some("rust base".to_string()),
            env_versions: Some("base env".to_string()),
            superseded_by: None,
            hits: 0,
        }
    }

    fn with_resolution(lesson: Lesson, resolution: &str) -> Lesson {
        Lesson {
            resolution: Some(resolution.to_string()),
            ..lesson
        }
    }

    fn version(value: &str, updated_at: &str) -> Version {
        Version {
            value: Some(value.to_string()),
            updated_at: updated_at.to_string(),
        }
    }

    fn group(lesson_id: &str, field: MergedField, versions: Vec<Version>) -> Conflict {
        Conflict {
            id: lesson_id.to_string(),
            field,
            base: None,
            versions,
        }
    }

    /// The derived order is the declaration order, and a queue group's sort key uses it: a reordered enum would reorder
    /// every committed `<domain>.conflicts.jsonl`, so `MERGED_FIELDS` must stay in declaration order.
    #[test]
    fn merged_fields_are_listed_in_declaration_order() {
        assert!(MERGED_FIELDS.is_sorted(), "{MERGED_FIELDS:?}");
        assert_eq!(MERGED_FIELDS.first(), Some(&MergedField::Title));
        assert_eq!(
            MERGED_FIELDS.last(),
            Some(&MergedField::SupersededBy),
            "new fields are appended at the end"
        );
    }

    /// The pin test for the wire tokens: the `field` values on disk in `<domain>.conflicts.jsonl`.
    #[test]
    fn merged_field_wire_tokens_are_the_lesson_field_names() {
        let wire_tokens: Vec<String> = MERGED_FIELDS
            .iter()
            .map(|field| serde_json::to_string(field).unwrap())
            .collect();
        assert_eq!(
            wire_tokens,
            [
                "\"title\"",
                "\"context\"",
                "\"problem\"",
                "\"root_cause\"",
                "\"resolution\"",
                "\"failed_tries\"",
                "\"tags\"",
                "\"env_versions\"",
                "\"superseded_by\""
            ]
        );
    }

    #[test]
    fn with_field_sets_only_that_field_and_field_value_reads_it_back() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        MERGED_FIELDS.iter().for_each(|&field| {
            let new_value = format!("new value of {field:?}");
            let changed = with_field(base_row.clone(), field, Some(new_value.clone()));
            assert_eq!(
                field_value(&changed, field),
                Some(new_value.as_str()),
                "{field:?}"
            );
            MERGED_FIELDS
                .iter()
                .filter(|other| **other != field)
                .for_each(|&other| {
                    assert_eq!(
                        field_value(&changed, other),
                        field_value(&base_row, other),
                        "{field:?} changed {other:?}"
                    );
                });
            assert_eq!(
                (
                    changed.id.as_str(),
                    changed.hits,
                    changed.updated_at.as_str()
                ),
                (LESSON_ID, 0, EARLY)
            );
        });
    }

    #[test]
    fn with_field_none_on_a_required_field_is_the_empty_string() {
        let cleared = with_field(sample_lesson(LESSON_ID, EARLY), MergedField::Title, None);
        assert_eq!(field_value(&cleared, MergedField::Title), Some(""));
    }

    /// The per-field rule for every merged field: equal, ours == base, theirs == base, all differ, no base row.
    #[test]
    fn per_field_rule_for_every_field() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        MERGED_FIELDS
            .iter()
            .filter(|field| **field != MergedField::SupersededBy)
            .for_each(|&field| {
                let fn_set = |value: &str, updated_at: &str| {
                    with_field(
                        Lesson {
                            updated_at: updated_at.to_string(),
                            ..base_row.clone()
                        },
                        field,
                        Some(value.to_string()),
                    )
                };
                let fn_merge_one = |base: &[Lesson], ours: Lesson, theirs: Lesson| {
                    merge_lessons(base, &[ours], &[theirs])
                };
                let unchanged_ours = Lesson {
                    updated_at: MIDDLE.to_string(),
                    ..base_row.clone()
                };

                let equal = fn_merge_one(
                    &[base_row.clone()],
                    fn_set("same", MIDDLE),
                    fn_set("same", LATE),
                );
                assert_eq!(
                    field_value(&equal.merged[0], field),
                    Some("same"),
                    "{field:?} equal"
                );
                assert!(equal.conflicts.is_empty(), "{field:?} equal");

                let theirs_changed = fn_merge_one(
                    &[base_row.clone()],
                    unchanged_ours.clone(),
                    fn_set("theirs", EARLY),
                );
                assert_eq!(
                    field_value(&theirs_changed.merged[0], field),
                    Some("theirs"),
                    "{field:?} ours == base"
                );
                assert!(
                    theirs_changed.conflicts.is_empty(),
                    "{field:?} ours == base"
                );

                let ours_changed =
                    fn_merge_one(&[base_row.clone()], fn_set("ours", EARLY), unchanged_ours);
                assert_eq!(
                    field_value(&ours_changed.merged[0], field),
                    Some("ours"),
                    "{field:?} theirs == base"
                );
                assert!(
                    ours_changed.conflicts.is_empty(),
                    "{field:?} theirs == base"
                );

                let all_differ = fn_merge_one(
                    &[base_row.clone()],
                    fn_set("ours", LATE),
                    fn_set("theirs", MIDDLE),
                );
                assert_eq!(
                    field_value(&all_differ.merged[0], field),
                    Some("ours"),
                    "{field:?} default winner is the newer"
                );
                assert_eq!(
                    all_differ.conflicts,
                    vec![Conflict {
                        id: LESSON_ID.to_string(),
                        field,
                        base: field_value(&base_row, field).map(String::from),
                        versions: vec![version("ours", LATE), version("theirs", MIDDLE)],
                    }],
                    "{field:?} all differ"
                );

                let no_base_row = fn_merge_one(&[], fn_set("ours", EARLY), fn_set("theirs", EARLY));
                assert_eq!(no_base_row.conflicts.len(), 1, "{field:?} no base row");
                assert_eq!(no_base_row.conflicts[0].base, None, "{field:?} no base row");
            });
    }

    #[test]
    fn hits_and_updated_at_take_the_larger_and_created_at_the_earlier() {
        let ours = Lesson {
            hits: MORE_HITS,
            updated_at: EARLY.to_string(),
            created_at: MIDDLE.to_string(),
            ..sample_lesson(LESSON_ID, EARLY)
        };
        let theirs = Lesson {
            hits: FEW_HITS,
            updated_at: LATE.to_string(),
            created_at: EARLY.to_string(),
            ..sample_lesson(LESSON_ID, EARLY)
        };
        let outcome = merge_lessons(&[], &[ours], &[theirs]);
        let merged = &outcome.merged[0];
        assert_eq!(
            (
                merged.hits,
                merged.updated_at.as_str(),
                merged.created_at.as_str()
            ),
            (MORE_HITS, LATE, EARLY)
        );
    }

    /// The old defect: an edit older than (or as old as) an unchanged copy must still win.
    #[test]
    fn an_older_edit_beats_a_newer_unchanged_copy() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        [EARLY, MIDDLE].iter().for_each(|edit_time| {
            let edited = with_resolution(
                Lesson {
                    updated_at: edit_time.to_string(),
                    ..base_row.clone()
                },
                "edited",
            );
            let unchanged = Lesson {
                updated_at: MIDDLE.to_string(),
                ..base_row.clone()
            };
            let outcome = merge_lessons(&[base_row.clone()], &[unchanged], &[edited]);
            assert_eq!(
                outcome.merged[0].resolution.as_deref(),
                Some("edited"),
                "edit at {edit_time}"
            );
            assert!(outcome.conflicts.is_empty());
        });
    }

    #[test]
    fn a_tie_on_updated_at_picks_the_larger_value_none_read_as_empty() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        let larger = with_resolution(
            Lesson {
                updated_at: MIDDLE.to_string(),
                ..base_row.clone()
            },
            "zebra",
        );
        let smaller = with_resolution(
            Lesson {
                updated_at: MIDDLE.to_string(),
                ..base_row.clone()
            },
            "apple",
        );
        assert_eq!(
            merge_lessons(&[base_row.clone()], &[smaller], &[larger]).merged[0]
                .resolution
                .as_deref(),
            Some("zebra")
        );
        let emptied = Lesson {
            resolution: None,
            updated_at: MIDDLE.to_string(),
            ..base_row.clone()
        };
        let filled = with_resolution(
            Lesson {
                updated_at: MIDDLE.to_string(),
                ..base_row.clone()
            },
            "any text",
        );
        assert_eq!(
            merge_lessons(&[base_row], &[emptied], &[filled]).merged[0]
                .resolution
                .as_deref(),
            Some("any text")
        );
    }

    #[test]
    fn a_superseded_by_conflict_keeps_the_default_winner_and_is_not_queued() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        let ours = Lesson {
            superseded_by: Some("01KOURS".to_string()),
            updated_at: LATE.to_string(),
            ..base_row.clone()
        };
        let theirs = Lesson {
            superseded_by: Some("01KTHEIRS".to_string()),
            updated_at: MIDDLE.to_string(),
            ..base_row.clone()
        };
        let outcome = merge_lessons(&[base_row], &[ours], &[theirs]);
        assert_eq!(outcome.merged[0].superseded_by.as_deref(), Some("01KOURS"));
        assert!(outcome.conflicts.is_empty());
        assert_eq!(
            outcome
                .unqueued
                .iter()
                .map(|group| group.field)
                .collect::<Vec<_>>(),
            vec![MergedField::SupersededBy]
        );
    }

    /// Every row of the merge table, by (base, ours, theirs) presence.
    #[test]
    fn merge_table_rows() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        let changed = with_resolution(
            Lesson {
                updated_at: MIDDLE.to_string(),
                ..base_row.clone()
            },
            "changed",
        );
        let only_hits_bumped = Lesson {
            hits: MORE_HITS,
            updated_at: MIDDLE.to_string(),
            ..base_row.clone()
        };
        let fn_ids = |outcome: MergeOutcome| -> Vec<(String, Option<String>)> {
            outcome
                .merged
                .into_iter()
                .map(|row| (row.id, row.resolution))
                .collect()
        };
        let kept_changed = vec![(LESSON_ID.to_string(), Some("changed".to_string()))];
        let kept_base = vec![(LESSON_ID.to_string(), Some("base resolution".to_string()))];
        assert_eq!(
            fn_ids(merge_lessons(&[], &[base_row.clone()], &[])),
            kept_base,
            "added here"
        );
        assert_eq!(
            fn_ids(merge_lessons(&[], &[], &[base_row.clone()])),
            kept_base,
            "added there"
        );
        assert_eq!(
            fn_ids(merge_lessons(&[base_row.clone()], &[base_row.clone()], &[])),
            vec![],
            "theirs deleted, ours unchanged"
        );
        assert_eq!(
            fn_ids(merge_lessons(&[base_row.clone()], &[changed.clone()], &[])),
            kept_changed,
            "theirs deleted, ours changed"
        );
        assert_eq!(
            fn_ids(merge_lessons(&[base_row.clone()], &[], &[base_row.clone()])),
            vec![],
            "ours deleted, theirs unchanged"
        );
        assert_eq!(
            fn_ids(merge_lessons(&[base_row.clone()], &[], &[changed.clone()])),
            kept_changed,
            "ours deleted, theirs changed"
        );
        assert_eq!(
            fn_ids(merge_lessons(&[base_row.clone()], &[], &[])),
            vec![],
            "deleted on both"
        );
        assert_eq!(
            fn_ids(merge_lessons(&[base_row.clone()], &[only_hits_bumped], &[])),
            vec![],
            "unchanged ignores hits"
        );
    }

    #[test]
    fn merged_rows_are_sorted_by_id() {
        let outcome = merge_lessons(
            &[],
            &[sample_lesson(OTHER_ID, EARLY)],
            &[sample_lesson(LESSON_ID, EARLY)],
        );
        assert_eq!(
            outcome
                .merged
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec![LESSON_ID, OTHER_ID]
        );
    }

    /// merge(base, x, y) == merge(base, y, x), over inputs that differ on every side (a symmetric input proves
    /// nothing).
    #[test]
    fn merge_is_symmetric() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        let ours_edit = Lesson {
            hits: FEW_HITS,
            ..with_resolution(
                Lesson {
                    updated_at: LATE.to_string(),
                    ..base_row.clone()
                },
                "ours",
            )
        };
        let theirs_edit = Lesson {
            tags: Some("theirs tags".to_string()),
            hits: MORE_HITS,
            ..with_resolution(
                Lesson {
                    updated_at: MIDDLE.to_string(),
                    ..base_row.clone()
                },
                "theirs",
            )
        };
        let tie_edit = with_resolution(
            Lesson {
                updated_at: LATE.to_string(),
                ..base_row.clone()
            },
            "tie",
        );
        let other_lesson = sample_lesson(OTHER_ID, MIDDLE);
        let cases: Vec<(Vec<Lesson>, Vec<Lesson>, Vec<Lesson>)> = vec![
            (
                vec![base_row.clone()],
                vec![ours_edit.clone()],
                vec![theirs_edit.clone()],
            ),
            (vec![], vec![ours_edit.clone()], vec![theirs_edit.clone()]),
            (
                vec![base_row.clone()],
                vec![ours_edit.clone()],
                vec![tie_edit],
            ),
            (
                vec![base_row.clone()],
                vec![ours_edit.clone(), other_lesson.clone()],
                vec![],
            ),
            (
                vec![base_row.clone()],
                vec![base_row.clone()],
                vec![theirs_edit.clone(), other_lesson],
            ),
        ];
        cases.iter().for_each(|(base, left, right)| {
            assert_eq!(
                merge_lessons(base, left, right),
                merge_lessons(base, right, left),
                "base {base:?}"
            );
        });
    }

    /// A back-to-back sync: merging the result with itself (BASE = OURS = THEIRS = MERGED) changes nothing.
    #[test]
    fn merging_a_result_again_changes_nothing() {
        let base_row = sample_lesson(LESSON_ID, EARLY);
        let first = merge_lessons(
            &[base_row.clone()],
            &[with_resolution(
                Lesson {
                    updated_at: LATE.to_string(),
                    ..base_row.clone()
                },
                "ours",
            )],
            &[with_resolution(
                Lesson {
                    updated_at: MIDDLE.to_string(),
                    ..base_row
                },
                "theirs",
            )],
        );
        let again = merge_lessons(&first.merged, &first.merged, &first.merged);
        assert_eq!((again.merged, again.conflicts.len()), (first.merged, 0));
    }

    /// (base, live, theirs, new conflicts): one input set of `merge_queue`.
    type QueueInputs = (Vec<Conflict>, Vec<Conflict>, Vec<Conflict>, Vec<Conflict>);

    #[test]
    fn queue_merge_three_way() {
        let resolved = group(
            LESSON_ID,
            MergedField::Resolution,
            vec![version("a", EARLY), version("b", MIDDLE)],
        );
        let tags_group = group(
            LESSON_ID,
            MergedField::Tags,
            vec![version("x", EARLY), version("y", MIDDLE)],
        );
        assert_eq!(
            merge_queue(&[resolved.clone()], &[], &[resolved.clone()], &[]),
            vec![],
            "resolved here"
        );
        assert_eq!(
            merge_queue(&[resolved.clone()], &[resolved.clone()], &[], &[]),
            vec![],
            "resolved there"
        );
        assert_eq!(
            merge_queue(&[], &[tags_group.clone()], &[], &[]),
            vec![tags_group.clone()],
            "added here"
        );
        assert_eq!(
            merge_queue(&[], &[], &[tags_group.clone()], &[]),
            vec![tags_group.clone()],
            "added there"
        );
        let other_side = group(
            LESSON_ID,
            MergedField::Tags,
            vec![version("y", LATE), version("z", EARLY)],
        );
        let union_expected = group(
            LESSON_ID,
            MergedField::Tags,
            vec![version("x", EARLY), version("y", LATE), version("z", EARLY)],
        );
        assert_eq!(
            merge_queue(&[], &[tags_group.clone()], &[other_side.clone()], &[]),
            vec![union_expected.clone()],
            "added on both"
        );
        assert_eq!(
            merge_queue(
                &[tags_group.clone()],
                &[tags_group.clone()],
                &[other_side.clone()],
                &[]
            ),
            vec![union_expected],
            "in base, on both"
        );
        // Symmetric over every phase-one case and with new conflicts, on inputs that differ on each side.
        let resolution_group = group(
            OTHER_ID,
            MergedField::Resolution,
            vec![version("p", EARLY), version("q", MIDDLE)],
        );
        let fresh = group(
            OTHER_ID,
            MergedField::Resolution,
            vec![version("r", LATE), version("s", LATE)],
        );
        let symmetry_cases: Vec<QueueInputs> = vec![
            (
                vec![],
                vec![tags_group.clone()],
                vec![other_side.clone()],
                vec![],
            ),
            (
                vec![tags_group.clone()],
                vec![tags_group.clone()],
                vec![other_side.clone()],
                vec![],
            ),
            (
                vec![tags_group.clone()],
                vec![tags_group.clone()],
                vec![],
                vec![],
            ),
            (
                vec![resolution_group.clone()],
                vec![resolution_group.clone(), tags_group.clone()],
                vec![other_side.clone()],
                vec![fresh.clone()],
            ),
            (vec![], vec![resolution_group], vec![], vec![fresh]),
        ];
        symmetry_cases
            .iter()
            .for_each(|(base, live, theirs, new_conflicts)| {
                assert_eq!(
                    merge_queue(base, live, theirs, new_conflicts),
                    merge_queue(base, theirs, live, new_conflicts),
                    "symmetric for base {base:?}"
                );
            });
    }

    /// A group resolved on one side while a new conflict appears on the same key: only the new versions remain.
    #[test]
    fn a_new_conflict_after_a_resolve_starts_a_fresh_group() {
        let resolved = group(
            LESSON_ID,
            MergedField::Resolution,
            vec![version("a", EARLY), version("b", MIDDLE)],
        );
        let fresh = group(
            LESSON_ID,
            MergedField::Resolution,
            vec![version("c", LATE), version("d", LATE)],
        );
        assert_eq!(
            merge_queue(&[resolved.clone()], &[], &[resolved], &[fresh.clone()]),
            vec![fresh]
        );
    }

    #[test]
    fn settle_applies_the_all_versions_winner_only_to_a_field_conflicted_this_cycle() {
        let row = with_resolution(sample_lesson(LESSON_ID, MIDDLE), "theirs");
        let three_versions = group(
            LESSON_ID,
            MergedField::Resolution,
            vec![
                version("theirs", MIDDLE),
                version("newest", LATE),
                version("old", EARLY),
            ],
        );
        let key: GroupKey = (LESSON_ID.to_string(), MergedField::Resolution);
        let not_conflicted = settle_queue(vec![row.clone()], vec![three_versions.clone()], &[]);
        assert_eq!(
            not_conflicted.merged[0].resolution.as_deref(),
            Some("theirs"),
            "the per-field result stands"
        );
        let conflicted = settle_queue(vec![row], vec![three_versions.clone()], &[key]);
        assert_eq!(
            conflicted.merged[0].resolution.as_deref(),
            Some("newest"),
            "all-versions default winner"
        );
        assert_eq!(
            conflicted.queue,
            vec![three_versions],
            "a default winner adds no version"
        );
    }

    #[test]
    fn settle_adds_a_merged_value_missing_from_its_group() {
        let row = with_resolution(sample_lesson(LESSON_ID, LATE), "local edit");
        let pending = group(
            LESSON_ID,
            MergedField::Resolution,
            vec![version("a", EARLY), version("b", MIDDLE)],
        );
        let settled = settle_queue(vec![row], vec![pending], &[]);
        assert_eq!(
            settled.queue[0].versions,
            vec![
                version("a", EARLY),
                version("b", MIDDLE),
                version("local edit", LATE)
            ]
        );
    }

    #[test]
    fn settle_clears_a_group_whose_lesson_is_gone() {
        let orphan = group(
            OTHER_ID,
            MergedField::Resolution,
            vec![version("a", EARLY), version("b", MIDDLE)],
        );
        let settled = settle_queue(
            vec![sample_lesson(LESSON_ID, EARLY)],
            vec![orphan.clone()],
            &[],
        );
        assert_eq!((settled.queue, settled.cleared), (vec![], vec![orphan]));
    }

    /// TAGS COMPARISON: a combine elsewhere appends `needs-reword` to `tags`; that adds no version to a `tags` group.
    #[test]
    fn settle_compares_tags_without_needs_reword() {
        let row = Lesson {
            tags: Some(format!("b a {NEEDS_REWORD_TAG}")),
            ..sample_lesson(LESSON_ID, LATE)
        };
        let tags_group = group(
            LESSON_ID,
            MergedField::Tags,
            vec![version("a b", EARLY), version("c", MIDDLE)],
        );
        assert_eq!(
            settle_queue(vec![row.clone()], vec![tags_group.clone()], &[]).queue,
            vec![tags_group.clone()]
        );
        let resolution_group = group(
            LESSON_ID,
            MergedField::Resolution,
            vec![version("a b", EARLY), version("c", MIDDLE)],
        );
        let other_field = Lesson {
            resolution: Some("b a".to_string()),
            ..row
        };
        assert!(
            settle_queue(vec![other_field], vec![resolution_group], &[]).queue[0]
                .versions
                .contains(&version("b a", LATE)),
            "only tags ignore order and the tag"
        );
    }
}
