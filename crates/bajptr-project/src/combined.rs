use crate::{
    Result, invalid,
    model::{self, Edit},
};
use bajptr_core::rules::Rule;
use std::collections::BTreeSet;

pub fn rule_matches(rule: &Rule, old: &str, new: &str) -> bool {
    rule.mode == "sub"
        && rule.old == old
        && rule.new == new
        && model::scopes(&rule.table)
            .is_ok_and(|scope| scope.is_some_and(|names| !names.is_empty()))
}
pub fn batch_matches(edit: &Edit, game: &str, version: &str, old: &str, new: &str) -> bool {
    edit.game.as_ref() == game
        && edit.version.as_ref() == version
        && edit.batch
        && edit
            .batch_operation
            .as_ref()
            .is_some_and(|op| op.old == old && op.new == new)
}

pub struct Change<'a> {
    pub game: &'a str,
    pub version: &'a str,
    pub old: &'a str,
    pub new: &'a str,
    pub next_old: &'a str,
    pub next_new: &'a str,
    pub tables: BTreeSet<String>,
    pub removed: BTreeSet<usize>,
}
pub fn update(
    rules: &[Rule],
    edits: &[Edit],
    change: Change<'_>,
) -> Result<(Vec<Rule>, Vec<Edit>)> {
    let Change {
        game,
        version,
        old,
        new,
        next_old,
        next_new,
        tables,
        removed,
    } = change;
    if next_old.is_empty() {
        return Err(invalid("请填写要替换的文字。"));
    }
    let rule_indices = rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule_matches(rule, old, new))
        .map(|(i, _)| i)
        .collect::<BTreeSet<_>>();
    let batch_indices = edits
        .iter()
        .enumerate()
        .filter(|(_, edit)| batch_matches(edit, game, version, old, new))
        .map(|(i, _)| i)
        .collect::<BTreeSet<_>>();
    if rule_indices.is_empty() || batch_indices.is_empty() || !removed.is_subset(&batch_indices) {
        return Err(invalid("合并替换的范围已变化，请重新打开。"));
    }
    let mut remaining = tables;
    let mut values = rules.to_vec();
    for &index in &rule_indices {
        let scope = model::scopes(&rules[index].table)?.unwrap();
        let assigned = scope
            .into_iter()
            .filter(|table| remaining.remove(table))
            .collect::<BTreeSet<_>>();
        values[index].old = next_old.into();
        values[index].new = next_new.into();
        values[index].table = serde_json::to_string(&assigned)?;
    }
    let first = *rule_indices.first().unwrap();
    let mut scope = model::scopes(&values[first].table)?
        .unwrap()
        .into_iter()
        .collect::<BTreeSet<_>>();
    scope.extend(remaining);
    values[first].table = serde_json::to_string(&scope)?;
    let values = values
        .into_iter()
        .enumerate()
        .filter(|(i, rule)| !rule_indices.contains(i) || rule.table != "[]")
        .map(|(_, rule)| rule)
        .collect::<Vec<_>>();
    let mut next = edits
        .iter()
        .enumerate()
        .filter(|(i, _)| !removed.contains(i))
        .map(|(i, edit)| {
            let mut edit = edit.clone();
            if batch_indices.contains(&i) {
                let op = edit.batch_operation.as_mut().unwrap();
                if op.old != next_old || op.new != next_new {
                    op.previous_scope = None;
                }
                op.old = next_old.into();
                op.new = next_new.into();
                edit.replacement = op.before.replace(next_old, next_new);
            }
            edit
        })
        .collect::<Vec<_>>();
    // Reuse an existing batch identity when editing into the same replacement.
    let id = next
        .iter()
        .find(|edit| batch_matches(edit, game, version, next_old, next_new))
        .and_then(|edit| edit.batch_operation.as_ref())
        .map(|op| op.id.clone());
    if let Some(id) = id {
        for edit in &mut next {
            if batch_matches(edit, game, version, next_old, next_new) {
                edit.batch_operation.as_mut().unwrap().id.clone_from(&id);
            }
        }
    }
    model::validate_edits(&next)?;
    Ok((model::normalize(values)?, next))
}
