//! Match saved overrides by original text, keeping uncertain locations explicit.
use crate::{
    Result, invalid,
    model::{self, Edit},
};
use bajptr_core::{
    database::{LocatedText, RowId},
    rules::Rules,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    #[serde(default)]
    pub group_key: String,
    pub indices: Vec<usize>,
    pub table: String,
    pub original: String,
    pub replacement: String,
    pub kind: String,
    pub candidates: Vec<LocatedText>,
    pub suggested: Option<usize>,
    pub proposals: Vec<Option<Proposal>>,
    pub row_id: RowId,
    pub slot: usize,
    pub equivalent_group: bool,
}

impl Item {
    pub(crate) fn add_candidate(
        &mut self,
        candidate: LocatedText,
        layers: &[&Edit],
        rules: &Rules,
        skip: bool,
    ) -> Result<()> {
        let proposal = if candidate.row.3 != self.original {
            Some(rebase::propose(layers, &candidate, rules, skip, None)?)
        } else {
            None
        };
        self.candidates.push(candidate);
        self.proposals.push(proposal);
        if self.candidates.len() == 1 {
            self.kind = "manual".into();
        }
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Proposal {
    pub input: Option<String>,
    pub replacement: Option<String>,
    pub editable: bool,
    pub batch_old: Option<String>,
    pub batch_new: Option<String>,
    pub reason: String,
}

mod rebase;
pub(crate) mod scan;

pub(crate) fn plan_candidates(
    edits: &[Edit],
    game: &str,
    version: &str,
    pool: &scan::Candidates,
    rules: &Rules,
    skip: bool,
    control: &crate::Control<'_>,
) -> Result<Vec<Item>> {
    let occupied: HashSet<_> = edits
        .iter()
        .filter(|e| e.game == game && e.version == version)
        .map(|e| (e.table.as_str(), &e.row_id, e.slot))
        .collect();
    let mut by_original = HashMap::<(&str, &str), Vec<&LocatedText>>::new();
    let mut by_address = HashMap::new();
    let mut by_context = HashMap::<_, Vec<_>>::new();
    for candidate in &pool.locations {
        let (table, row, slot, original) = &candidate.row;
        by_original
            .entry((table, original))
            .or_default()
            .push(candidate);
        by_address.insert((table.as_str(), row, *slot), candidate);
        if let Some(anchor) = &candidate.anchor {
            for (side, value) in [
                (0, &anchor.siblings),
                (1, &anchor.previous),
                (2, &anchor.next),
            ] {
                if let Some(value) = value {
                    by_context
                        .entry((table.as_str(), side, value.as_str()))
                        .or_default()
                        .push(candidate);
                }
            }
        }
    }
    let mut bytes = 0;
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut positions = HashMap::new();
    for (index, edit) in edits
        .iter()
        .enumerate()
        .filter(|(_, e)| e.game == game && e.version != version)
    {
        let key = (&edit.version, &edit.table, &edit.row_id, edit.slot);
        let position = *positions.entry(key).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[position].push(index);
    }
    let mut items = Vec::with_capacity(groups.len());
    for indices in groups {
        control.check()?;
        let layers: Vec<_> = indices.iter().map(|&i| &edits[i]).collect();
        let edit = layers.iter().copied().max_by_key(|e| e.batch).unwrap();
        let table = edit.table.as_str();
        let address = (table, &edit.row_id, edit.slot);
        let original_matches = by_original
            .get(&(table, edit.original.as_str()))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let is_free = |c: &&LocatedText| !occupied.contains(&(c.row.0.as_str(), &c.row.1, c.row.2));
        let mut matches: Vec<_> = original_matches.iter().copied().filter(is_free).collect();
        let was_occupied = matches.len() < original_matches.len() || occupied.contains(&address);
        let anchor = layers.iter().find_map(|e| e.anchor.as_ref());
        let mut corroborated = Vec::new();
        if let Some(anchor) = anchor {
            let mut scores = HashMap::<_, usize>::new();
            for (side, value) in [
                (0, &anchor.siblings),
                (1, &anchor.previous),
                (2, &anchor.next),
            ] {
                if let Some(value) = value {
                    for &target in by_context
                        .get(&(table, side, value.as_str()))
                        .into_iter()
                        .flatten()
                    {
                        let entry = scores.entry((&target.row.1, target.row.2)).or_default();
                        *entry += 1;
                        if *entry == 2 {
                            corroborated.push(target);
                        }
                    }
                }
            }
        }
        let verified = match corroborated.as_slice() {
            [target] if target.row.3 == edit.original && is_free(target) => matches
                .iter()
                .position(|candidate| candidate.row == target.row),
            _ => None,
        };
        let context_mismatch = anchor.is_some_and(|old| {
            matches.len() == 1
                && matches[0].anchor.as_ref().is_some_and(|new| {
                    let pairs = [
                        (&old.siblings, &new.siblings),
                        (&old.previous, &new.previous),
                        (&old.next, &new.next),
                    ];
                    pairs
                        .iter()
                        .filter(|(a, b)| a.is_some() && b.is_some())
                        .count()
                        >= 2
                        && pairs.into_iter().all(|(a, b)| a.is_none() || a != b)
                })
        });
        let anchor_disagrees = verified.is_none() && (!corroborated.is_empty() || context_mismatch);
        let suggested = verified.or_else(|| {
            (original_matches.len() == 1
                && matches.len() == 1
                && !anchor_disagrees
                && matches[0].row.1 == edit.row_id
                && matches[0].row.2 == edit.slot)
                .then_some(0)
        });
        let exact_count = matches.len();
        let mut changed = Vec::new();
        if suggested.is_none() {
            changed.extend(corroborated.iter().copied().take(8));
            if matches.is_empty() || anchor_disagrees {
                changed.extend(by_address.get(&address).copied());
                if let Some(fuzzy) = pool.fuzzy.get(&(edit.table.clone(), edit.original.clone())) {
                    changed.extend(fuzzy.iter());
                }
            }
        }
        let mut seen = HashSet::new();
        for candidate in changed.into_iter().filter(is_free) {
            if candidate.row.3 != edit.original && seen.insert((&candidate.row.1, candidate.row.2))
            {
                matches.push(candidate);
            }
        }
        let table_missing = pool
            .tables
            .as_ref()
            .is_some_and(|tables| !tables.contains(table));
        let kind = if suggested.is_some() {
            "exact"
        } else if exact_count == 0 && !matches.is_empty() {
            "changed"
        } else if matches.is_empty() && was_occupied {
            "occupied"
        } else if matches.is_empty() && table_missing {
            "missing_table"
        } else if matches.is_empty() && pool.skipped_tables.contains(table) {
            "unsupported_table"
        } else if matches.is_empty() {
            "unavailable"
        } else if matches.len() == 1 {
            "relocated"
        } else {
            "ambiguous"
        };
        let (candidates, proposals) = matches
            .into_iter()
            .map(|candidate| {
                bytes += scan::cost(candidate);
                if bytes > scan::MAX_BYTES {
                    return Err(invalid("迁移候选过多，请分批核对旧版修改。"));
                }
                let proposal = if candidate.row.3 != edit.original {
                    let proposal = rebase::propose(&layers, candidate, rules, skip, None)?;
                    bytes += proposal.input.as_ref().map_or(0, String::len)
                        + proposal.replacement.as_ref().map_or(0, String::len);
                    Some(proposal)
                } else {
                    None
                };
                Ok((candidate.clone(), proposal))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unzip();
        items.push(Item {
            // Only interchangeable saved layers may share a review decision.
            // Include the source version so stale revisions are never combined.
            group_key: crate::storage::digest(
                serde_json::to_string(
                    &indices
                        .iter()
                        .map(|&i| {
                            let e = &edits[i];
                            serde_json::json!([
                                e.game,
                                e.version,
                                e.table,
                                e.original,
                                e.replacement,
                                e.batch,
                                e.batch_operation
                                    .as_ref()
                                    .map(|op| serde_json::json!([op.old, op.new, op.before]))
                            ])
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap()
                .as_bytes(),
            ),
            indices,
            table: edit.table.clone(),
            original: edit.original.clone(),
            replacement: edit.replacement.clone(),
            kind: kind.into(),
            candidates,
            proposals,
            suggested,
            row_id: edit.row_id.clone(),
            slot: edit.slot,
            equivalent_group: !was_occupied
                && !anchor_disagrees
                && (anchor.is_none() || original_matches.len() == 1),
        });
    }
    let mut targets = HashMap::new();
    for item in &items {
        let candidates = item.candidates.iter().enumerate().filter(|(i, target)| {
            item.suggested
                .map_or_else(|| target.row.3 == item.original, |index| index == *i)
        });
        for (_, target) in candidates {
            let row = &target.row;
            *targets.entry((&row.0, &row.1, row.2)).or_insert(0) += 1;
        }
    }
    let conflicts: Vec<_> = items
        .iter()
        .map(|item| {
            item.suggested.is_some_and(|index| {
                let row = &item.candidates[index].row;
                targets[&(&row.0, &row.1, row.2)] > 1
            })
        })
        .collect();
    for (item, conflict) in items.iter_mut().zip(conflicts) {
        if conflict {
            item.kind = "ambiguous".into();
            item.suggested = None;
        }
    }
    Ok(items)
}

#[derive(Serialize, Deserialize)]
pub struct Choice {
    pub item: usize,
    pub candidate: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Exact addresses take precedence. Remaining interchangeable records can move
/// as a complete set only when the set has no competing saved modification.
pub fn automatic_choices(items: &[Item]) -> Vec<Choice> {
    type Target = (String, RowId, usize);
    let target = |candidate: &LocatedText| {
        (
            candidate.row.0.clone(),
            candidate.row.1.clone(),
            candidate.row.2,
        )
    };
    let mut choices: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(item, value)| {
            value.suggested.map(|candidate| Choice {
                item,
                candidate,
                text: None,
            })
        })
        .collect();
    let reserved: HashSet<_> = choices
        .iter()
        .map(|c| target(&items[c.item].candidates[c.candidate]))
        .collect();
    let mut groups = HashMap::<(&str, Vec<Target>), Vec<usize>>::new();
    for (index, item) in items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.suggested.is_none())
    {
        let candidates: Vec<_> = item
            .candidates
            .iter()
            .filter(|candidate| candidate.row.0 == item.table && candidate.row.3 == item.original)
            .map(&target)
            .filter(|t| !reserved.contains(t))
            .collect();
        if !candidates.is_empty() {
            groups
                .entry((&item.group_key, candidates))
                .or_default()
                .push(index);
        }
    }
    let mut uses = HashMap::<&Target, usize>::new();
    for (_, targets) in groups.keys() {
        for t in targets {
            *uses.entry(t).or_default() += 1;
        }
    }
    for ((_, targets), members) in &groups {
        if targets.len() != members.len()
            || targets.iter().any(|t| uses[t] > 1)
            || members.iter().any(|&i| !items[i].equivalent_group)
        {
            continue;
        }
        for (&item, t) in members.iter().zip(targets) {
            let candidate = items[item]
                .candidates
                .iter()
                .position(|c| target(c) == *t)
                .unwrap();
            choices.push(Choice {
                item,
                candidate,
                text: None,
            });
        }
    }
    choices.sort_by_key(|c| c.item);
    choices
}

pub(crate) fn apply_choices(
    edits: &[Edit],
    items: &[Item],
    choices: &[Choice],
    game: &str,
    target: (&str, Option<&str>),
    rules: &Rules,
    skip: bool,
) -> Result<Vec<Edit>> {
    let (version, snapshot) = target;
    if choices.is_empty() {
        return Err(invalid("请先选择要迁移的文本。"));
    }
    let mut removed = HashSet::new();
    let mut targets: HashSet<(String, RowId, usize)> = HashSet::new();
    let mut moved = Vec::new();
    let mut batches = HashMap::new();
    for edit in edits
        .iter()
        .filter(|e| e.game == game && e.version == version)
    {
        if let Some(op) = &edit.batch_operation {
            batches.insert((op.old.clone(), op.new.clone()), op.id.clone());
        }
    }
    for choice in choices {
        let item = items
            .get(choice.item)
            .ok_or_else(|| invalid("迁移条目无效，请重新核对。"))?;
        let candidate = item
            .candidates
            .get(choice.candidate)
            .ok_or_else(|| invalid("请选择对应的新文本。"))?;
        if !targets.insert((
            candidate.row.0.clone(),
            candidate.row.1.clone(),
            candidate.row.2,
        )) {
            return Err(invalid("多条旧记录指向同一段新文本，请只保留一项。"));
        }
        if edits.iter().any(|e| {
            e.game == game
                && e.version == version
                && e.table == candidate.row.0
                && e.row_id == candidate.row.1
                && e.slot == candidate.row.2
        }) {
            return Err(invalid("目标文本已有新版本修改，未覆盖。"));
        }
        let layers = item
            .indices
            .iter()
            .map(|&i| {
                edits
                    .get(i)
                    .ok_or_else(|| invalid("旧记录已变化，请重新核对。"))
            })
            .collect::<Result<Vec<_>>>()?;
        let rebased = candidate.row.3 != item.original;
        if !rebased && choice.text.is_some() {
            return Err(invalid("原文未变化，请直接确认对应文本。"));
        }
        let proposal = rebased
            .then(|| rebase::propose(&layers, candidate, rules, skip, choice.text.as_deref()))
            .transpose()?;
        if let Some(proposal) = &proposal
            && proposal.replacement.is_none()
        {
            return Err(invalid(&proposal.reason));
        }
        for &index in &item.indices {
            let mut edit = edits
                .get(index)
                .cloned()
                .ok_or_else(|| invalid("旧记录已变化，请重新核对。"))?;
            if !removed.insert(index)
                || edit.game != game
                || edit.version == version
                || edit.table != item.table
                || edit.original != item.original
                || edit.row_id != item.row_id
                || edit.slot != item.slot
            {
                return Err(invalid("迁移范围已变化，请重新核对。"));
            }
            let source_version = std::mem::replace(&mut edit.version, version.into());
            edit.table.clone_from(&candidate.row.0);
            edit.original.clone_from(&candidate.row.3);
            edit.anchor.clone_from(&candidate.anchor);
            if let Some(proposal) = &proposal {
                if let Some(op) = &mut edit.batch_operation {
                    op.before.clone_from(proposal.input.as_ref().unwrap());
                    edit.replacement
                        .clone_from(proposal.replacement.as_ref().unwrap());
                } else if edit.batch || proposal.editable {
                    edit.replacement
                        .clone_from(proposal.input.as_ref().unwrap());
                }
            }
            edit.row_id = candidate.row.1.clone();
            edit.slot = candidate.row.2;
            if let Some(op) = &mut edit.batch_operation {
                op.previous_scope = snapshot.map(|digest| model::PreviousBatchScope {
                    version: source_version,
                    snapshot: digest.into(),
                });
                op.id = batches
                    .entry((op.old.clone(), op.new.clone()))
                    .or_insert_with(|| uuid::Uuid::new_v4().simple().to_string())
                    .clone();
            }
            moved.push(edit);
        }
    }
    let mut values: Vec<_> = edits
        .iter()
        .enumerate()
        .filter(|(i, _)| !removed.contains(i))
        .map(|(_, e)| e.clone())
        .collect();
    values.extend(moved);
    model::validate_edits(&values)?;
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BatchOperation;
    fn plan(edits: &[Edit], game: &str, version: &str, located: &[LocatedText]) -> Vec<Item> {
        plan_candidates(
            edits,
            game,
            version,
            &scan::Candidates {
                locations: located.to_vec(),
                ..Default::default()
            },
            &Rules::new(vec![]).unwrap(),
            false,
            &crate::Control::new(&|_| true),
        )
        .unwrap()
    }
    fn apply(
        edits: &[Edit],
        items: &[Item],
        choices: &[Choice],
        game: &str,
        version: &str,
    ) -> Result<Vec<Edit>> {
        apply_choices(
            edits,
            items,
            choices,
            game,
            (version, None),
            &Rules::new(vec![]).unwrap(),
            false,
        )
    }
    fn edit(id: i64, original: &str) -> Edit {
        Edit {
            game: "game".into(),
            version: "a".repeat(64),
            table: "Text".into(),
            row_id: RowId::Integer(id),
            slot: 0,
            original: original.into(),
            replacement: format!("{original}改"),
            batch: false,
            batch_operation: None,
            anchor: None,
        }
    }
    fn row(id: i64, original: &str) -> LocatedText {
        LocatedText {
            row: ("Text".into(), RowId::Integer(id), 0, original.into()),
            context: String::new(),
            anchor: None,
        }
    }

    #[test]
    fn reused_address_with_repeated_original_needs_independent_evidence() {
        let items = plan(
            &[edit(1, "重复原文")],
            "game",
            &"b".repeat(64),
            &[row(1, "重复原文"), row(9, "重复原文")],
        );
        assert!(automatic_choices(&items).is_empty());
    }

    #[test]
    fn corroborated_context_finds_the_moved_member_of_a_partial_repeated_group() {
        use bajptr_core::database::LocationAnchor;
        let mut old = edit(1, "重复原文");
        let anchor = LocationAnchor {
            siblings: Some("same siblings".into()),
            previous: Some("before".into()),
            next: Some("after".into()),
        };
        old.anchor = Some(anchor.clone());
        let mut moved = row(9, "重复原文");
        moved.anchor = Some(LocationAnchor {
            next: Some("inserted row".into()),
            ..anchor
        });
        let plan = plan(
            &[old],
            "game",
            &"b".repeat(64),
            &[row(1, "重复原文"), moved],
        );
        let choices = automatic_choices(&plan);
        assert_eq!(choices.len(), 1);
        assert_eq!(
            plan[0].candidates[choices[0].candidate].row.1,
            RowId::Integer(9)
        );
    }

    #[test]
    fn same_text_at_the_old_address_cannot_preempt_a_competing_version() {
        let mut other = edit(9, "唯一原文");
        other.version = "c".repeat(64);
        other.replacement = "另一版结果".into();
        let plan = plan(
            &[edit(1, "唯一原文"), other],
            "game",
            &"b".repeat(64),
            &[row(1, "唯一原文")],
        );
        assert!(automatic_choices(&plan).is_empty());
    }

    #[test]
    fn changed_original_at_old_address_can_be_reviewed() {
        let items = plan(
            &[edit(1, "消耗体力 10 点")],
            "game",
            &"b".repeat(64),
            &[row(1, "消耗体力 15 点")],
        );
        assert!(automatic_choices(&items).is_empty());
        assert_eq!(items[0].candidates[0].row.3, "消耗体力 15 点");
    }

    #[test]
    fn exact_matches_are_selected_while_relocations_duplicates_and_conflicts_need_review() {
        let mut current = edit(5, "已有");
        current.version = "b".repeat(64);
        let edits = vec![
            edit(1, "一致"),
            edit(2, "移位"),
            edit(3, "重复"),
            edit(4, "已变"),
            edit(5, "已有"),
            current,
        ];
        let items = plan(
            &edits,
            "game",
            &"b".repeat(64),
            &[
                row(1, "一致"),
                row(20, "移位"),
                row(30, "重复"),
                row(31, "重复"),
                row(4, "新文"),
                row(5, "已有"),
            ],
        );
        assert_eq!(
            items.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(),
            ["exact", "relocated", "ambiguous", "changed", "occupied"]
        );
        assert_eq!(items.iter().filter(|i| i.suggested.is_some()).count(), 1);
        let updated = apply(
            &edits,
            &items,
            &[
                Choice {
                    item: 0,
                    candidate: 0,
                    text: None,
                },
                Choice {
                    item: 1,
                    candidate: 0,
                    text: None,
                },
            ],
            "game",
            &"b".repeat(64),
        )
        .unwrap();
        assert_eq!(updated.len(), edits.len());
        assert!(
            updated
                .iter()
                .any(|e| e.version == "b".repeat(64) && e.row_id == RowId::Integer(20))
        );
        assert_eq!(
            updated
                .iter()
                .find(|e| e.original == "已有" && e.version == "b".repeat(64))
                .unwrap(),
            &edits[5]
        );
        let mut older = edit(1, "一致");
        older.version = "c".repeat(64);
        older.replacement = "另一版结果".into();
        let conflicts = plan(
            &[edits[0].clone(), older],
            "game",
            &"b".repeat(64),
            &[row(1, "一致")],
        );
        assert!(
            conflicts
                .iter()
                .all(|item| item.suggested.is_none() && item.kind == "ambiguous")
        );
    }

    #[test]
    fn complete_equivalent_groups_move_automatically_but_changed_scope_and_conflicts_do_not() {
        let edits: Vec<_> = (1..=7).map(|id| edit(id, "原文")).collect();
        let moved: Vec<_> = (11..=17).map(|id| row(id, "原文")).collect();
        let items = plan(&edits, "game", &"b".repeat(64), &moved);
        let choices = automatic_choices(&items);
        assert_eq!(choices.len(), 7);
        let updated = apply(&edits, &items, &choices, "game", &"b".repeat(64)).unwrap();
        assert!(
            updated
                .iter()
                .all(|e| e.version == "b".repeat(64) && e.replacement == "原文改")
        );
        assert_eq!(
            updated
                .iter()
                .map(|e| &e.row_id)
                .collect::<HashSet<_>>()
                .len(),
            7
        );
        let mixed: Vec<_> = [row(1, "原文"), row(2, "原文")]
            .into_iter()
            .chain(moved[2..].iter().cloned())
            .collect();
        let mixed_plan = plan(&edits, "game", &"b".repeat(64), &mixed);
        let mixed_choices = automatic_choices(&mixed_plan);
        assert_eq!(mixed_choices.len(), 7);
        assert!(apply(&edits, &mixed_plan, &mixed_choices, "game", &"b".repeat(64)).is_ok());
        for rows in [
            moved[..6].to_vec(),
            moved.iter().cloned().chain([row(18, "原文")]).collect(),
        ] {
            assert!(automatic_choices(&plan(&edits, "game", &"b".repeat(64), &rows)).is_empty());
        }
        let mut conflict = edits.clone();
        conflict[6].replacement = "不同修改".into();
        assert!(automatic_choices(&plan(&conflict, "game", &"b".repeat(64), &moved)).is_empty());
        conflict[6] = edits[6].clone();
        conflict[6].version = "c".repeat(64);
        assert!(automatic_choices(&plan(&conflict, "game", &"b".repeat(64), &moved)).is_empty());
    }

    #[test]
    fn paired_single_and_partial_batch_migrate_together_without_losing_old_members() {
        let single = edit(1, "原文");
        let mut batch = single.clone();
        batch.batch = true;
        batch.replacement = "新文改".into();
        batch.batch_operation = Some(BatchOperation {
            id: "1".repeat(32),
            old: "原文".into(),
            new: "新文".into(),
            before: single.replacement.clone(),
            previous_scope: None,
        });
        let mut remaining = batch.clone();
        remaining.row_id = RowId::Integer(2);
        let edits = vec![single, batch, remaining];
        let items = plan(
            &edits,
            "game",
            &"b".repeat(64),
            &[row(1, "原文"), row(2, "原文")],
        );
        assert_ne!(
            items[0].group_key, items[1].group_key,
            "single+batch and batch-only must stay separate"
        );
        let same = plan(
            &[edits[1].clone(), edits[2].clone()],
            "game",
            &"b".repeat(64),
            &[row(1, "原文"), row(2, "原文")],
        );
        assert_eq!(same[0].group_key, same[1].group_key);
        let updated = apply(
            &edits,
            &items,
            &[Choice {
                item: 0,
                candidate: 0,
                text: None,
            }],
            "game",
            &"b".repeat(64),
        )
        .unwrap();
        assert_eq!(updated.len(), 3);
        model::validate_edits(&updated).unwrap();
        assert_eq!(
            updated
                .iter()
                .filter(|e| e.version == "b".repeat(64))
                .count(),
            2
        );
        let old = updated
            .iter()
            .find(|e| e.version == "a".repeat(64))
            .unwrap();
        let new = updated
            .iter()
            .find(|e| e.version == "b".repeat(64) && e.batch)
            .unwrap();
        assert_ne!(
            old.batch_operation.as_ref().unwrap().id,
            new.batch_operation.as_ref().unwrap().id
        );
        assert_eq!(new.batch_operation.as_ref().unwrap().before, "原文改");
        assert_eq!(
            model::final_edits(&updated, "game", &"b".repeat(64)).unwrap()[0].replacement,
            "新文改"
        );
        let followup = plan(
            &updated,
            "game",
            &"b".repeat(64),
            &[row(1, "原文"), row(2, "原文")],
        );
        let merged = apply(
            &updated,
            &followup,
            &[Choice {
                item: 0,
                candidate: 0,
                text: None,
            }],
            "game",
            &"b".repeat(64),
        )
        .unwrap();
        assert!(merged.iter().all(|e| e.version == "b".repeat(64)));
        assert_eq!(
            merged
                .iter()
                .filter_map(|e| e.batch_operation.as_ref().map(|op| &op.id))
                .collect::<HashSet<_>>()
                .len(),
            1
        );
        let duplicate = vec![
            Choice {
                item: 0,
                candidate: 0,
                text: None,
            },
            Choice {
                item: 1,
                candidate: 0,
                text: None,
            },
        ];
        assert!(apply(&edits, &items, &duplicate, "game", &"b".repeat(64)).is_err());
    }
}
