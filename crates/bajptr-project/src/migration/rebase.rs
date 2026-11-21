use super::Proposal;
use crate::{
    Result, invalid,
    model::{self, Edit},
};
use bajptr_core::{database::LocatedText, rules::Rules};

const MAX_DIFF_CELLS: usize = 300_000;

#[derive(PartialEq, Eq)]
struct Change {
    start: usize,
    end: usize,
    text: Vec<char>,
}

fn changes(base: &[char], updated: &[char]) -> Option<Vec<Change>> {
    let start = base.iter().zip(updated).take_while(|(a, b)| a == b).count();
    let suffix = base[start..]
        .iter()
        .rev()
        .zip(updated[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &base[start..base.len() - suffix];
    let b = &updated[start..updated.len() - suffix];
    let width = b.len() + 1;
    let cells = (a.len() + 1).checked_mul(width)?;
    if cells > MAX_DIFF_CELLS {
        return None;
    }
    let mut lengths = vec![0u32; cells];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            let at = i * width + j;
            lengths[at] = if a[i] == b[j] {
                1 + lengths[at + width + 1]
            } else {
                lengths[at + width].max(lengths[at + 1])
            };
        }
    }
    let mut result = Vec::new();
    let mut current: Option<Change> = None;
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            result.extend(current.take());
            i += 1;
            j += 1;
            continue;
        }
        let change = current.get_or_insert_with(|| Change {
            start: start + i,
            end: start + i,
            text: Vec::new(),
        });
        if j < b.len()
            && (i == a.len() || lengths[i * width + j + 1] > lengths[(i + 1) * width + j])
        {
            change.text.push(b[j]);
            j += 1;
        } else {
            change.end += 1;
            i += 1;
        }
    }
    result.extend(current);
    Some(result)
}

fn merge(original: &str, edited: &str, target: &str) -> Option<String> {
    if edited == target || original == target {
        return Some(edited.into());
    }
    let base: Vec<_> = original.chars().collect();
    let mut game = changes(&base, &target.chars().collect::<Vec<_>>())?;
    for user in changes(&base, &edited.chars().collect::<Vec<_>>())? {
        if game.contains(&user) {
            continue;
        }
        if game.iter().any(|other| {
            user.start < other.end && other.start < user.end
                || user.start == other.start && (user.start == user.end || other.start == other.end)
        }) {
            return None;
        }
        game.push(user);
    }
    game.sort_by_key(|c| (c.start, c.end));
    let mut result = String::new();
    let mut cursor = 0;
    for change in game {
        result.extend(base[cursor..change.start].iter());
        result.extend(change.text);
        cursor = change.end;
    }
    result.extend(base[cursor..].iter());
    Some(result)
}

pub(super) fn propose(
    layers: &[&Edit],
    candidate: &LocatedText,
    rules: &Rules,
    manual: Option<&str>,
) -> Result<Proposal> {
    let row = &candidate.row;
    let top = layers
        .iter()
        .max_by_key(|edit| edit.batch)
        .ok_or_else(|| invalid("旧修改记录为空。"))?;
    if top.original == row.3 && manual.is_none() {
        return Ok(Proposal {
            replacement: Some(top.replacement.clone()),
            ..Proposal::default()
        });
    }
    let batch = top.batch_operation.as_ref();
    let literal = layers
        .iter()
        .find(|e| !e.batch)
        .copied()
        .or_else(|| batch.is_none().then_some(*top));
    let mut proposal = Proposal {
        editable: literal.is_some(),
        batch_old: batch.map(|op| op.old.clone()),
        batch_new: batch.map(|op| op.new.clone()),
        ..Proposal::default()
    };
    if let Some(edit) = literal {
        proposal.input = manual
            .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
            .or_else(|| merge(&edit.original, &edit.replacement, &row.3));
    } else if manual.is_some() {
        return Err(invalid("这条批量修改需按原操作重放。"));
    } else {
        proposal.input = Some(model::effective(row, &[], &top.game, &top.version, rules)?.0);
    }
    if let Some(input) = &proposal.input {
        if let Some(op) = batch {
            let count = input.matches(&op.old).count();
            if count == 0 {
                proposal.reason = "新版文本中找不到批量修改的原词。".into();
            } else {
                if count != op.before.matches(&op.old).count() {
                    proposal.reason = "新版文本中的替换次数已变化，请核对结果。".into();
                }
                proposal.replacement = Some(input.replace(&op.old, &op.new));
            }
        } else {
            proposal.replacement = Some(input.clone());
        }
    } else {
        proposal.reason = "旧修改和新版原文有重叠改动，请填写迁移后的文字。".into();
    }
    if top.batch && top.batch_operation.is_none() && manual.is_none() {
        proposal.replacement = None;
        proposal.reason = "旧记录缺少批量替换参数，请填写迁移后的文字。".into();
    }
    Ok(proposal)
}

#[cfg(test)]
mod tests {
    use super::merge;

    #[test]
    fn merges_independent_changes_and_flags_overlaps() {
        assert_eq!(
            merge("体力 10，金币 20", "行动力 10，金块 20", "体力 15，金币 25").as_deref(),
            Some("行动力 15，金块 25")
        );
        assert_eq!(merge("消耗体力 10", "消耗体力 20", "消耗体力 30"), None);
        assert_eq!(merge("abc", "xabc", "abcz").as_deref(), Some("xabcz"));
    }
}
