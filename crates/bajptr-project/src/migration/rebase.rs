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

enum Merge {
    Clean(String),
    Conflict { mine: String, game: String },
}

fn overlaps(a: &Change, b: &Change) -> bool {
    a != b
        && (a.start < b.end && b.start < a.end
            || a.start == b.start && (a.start == a.end || b.start == b.end))
}

fn merge(original: &str, edited: &str, target: &str) -> Option<Merge> {
    if edited == target || original == target {
        return Some(Merge::Clean(edited.into()));
    }
    let base: Vec<_> = original.chars().collect();
    let game = changes(&base, &target.chars().collect::<Vec<_>>())?;
    let mine = changes(&base, &edited.chars().collect::<Vec<_>>())?;
    let combine = |primary: &[Change], secondary: &[Change]| {
        let mut combined = primary
            .iter()
            .chain(secondary.iter().filter(|change| {
                !primary.contains(change) && primary.iter().all(|other| !overlaps(change, other))
            }))
            .collect::<Vec<_>>();
        combined.sort_by_key(|c| (c.start, c.end));
        let mut result = String::new();
        let mut cursor = 0;
        for change in combined {
            result.extend(base[cursor..change.start].iter());
            result.extend(change.text.iter());
            cursor = change.end;
        }
        result.extend(base[cursor..].iter());
        result
    };
    let result = combine(&game, &mine);
    if mine
        .iter()
        .any(|change| game.iter().any(|other| overlaps(change, other)))
    {
        Some(Merge::Conflict {
            mine: combine(&mine, &game),
            game: result,
        })
    } else {
        Some(Merge::Clean(result))
    }
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
        if let Some(text) = manual {
            proposal.input = Some(text.replace("\r\n", "\n").replace('\r', "\n"));
        } else {
            match merge(&edit.original, &edit.replacement, &row.3) {
                Some(Merge::Clean(text)) => proposal.input = Some(text),
                Some(Merge::Conflict { mine, game }) => {
                    let option = |input: String| {
                        let result = batch
                            .map(|op| input.replace(&op.old, &op.new))
                            .unwrap_or_else(|| input.clone());
                        super::MergeOption { input, result }
                    };
                    proposal.mine = Some(option(mine));
                    proposal.game = Some(option(game));
                }
                None => {}
            }
        }
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
    } else if proposal.mine.is_some() {
        proposal.reason = "文字冲突".into();
    } else {
        proposal.reason = "请核对最终文字。".into();
    }
    if top.batch && top.batch_operation.is_none() && manual.is_none() {
        proposal.replacement = None;
        proposal.reason = "旧记录缺少批量替换参数，请填写迁移后的文字。".into();
    }
    Ok(proposal)
}

#[cfg(test)]
mod tests {
    use super::{Merge, merge};

    #[test]
    fn merges_independent_changes_and_flags_overlaps() {
        assert!(matches!(
            merge("体力 10，金币 20", "行动力 10，金块 20", "体力 15，金币 25"),
            Some(Merge::Clean(text)) if text == "行动力 15，金块 25"
        ));
        assert!(matches!(
            merge("消耗体力 10", "消耗体力 20", "消耗体力 30（每日一次）"),
            Some(Merge::Conflict { mine, game })
                if mine == "消耗体力 20（每日一次）" && game == "消耗体力 30（每日一次）"
        ));
        assert!(
            matches!(merge("abc", "xabc", "abcz"), Some(Merge::Clean(text)) if text == "xabcz")
        );
    }
}
