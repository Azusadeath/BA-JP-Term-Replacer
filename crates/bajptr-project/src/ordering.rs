use crate::{Result, model::Edit, storage};
use bajptr_core::rules::Rule;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    Alphabetical,
    Modified,
    #[default]
    Added,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Stamp {
    signature: String,
    pub added: u64,
    pub modified: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Timeline {
    pub rules: Vec<Stamp>,
    pub batches: BTreeMap<String, Stamp>,
    retired_rules: Vec<Stamp>,
    tick: u64,
}

fn signature<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(storage::digest(&serde_json::to_vec(value)?))
}

pub fn batch_key(edit: &Edit) -> String {
    let identity = if let Some(op) = &edit.batch_operation {
        serde_json::json!([edit.game, edit.version, op.id])
    } else {
        serde_json::json!([edit.game, edit.version, edit.table, edit.row_id, edit.slot])
    };
    storage::digest(identity.to_string().as_bytes())
}

impl Timeline {
    pub fn observe(&mut self, rules: &[Rule], edits: &[Edit], time: u64) -> Result<bool> {
        let now = if time == 0 {
            0
        } else {
            time.max(self.tick.saturating_add(1))
        };
        let signatures = rules.iter().map(signature).collect::<Result<Vec<_>>>()?;
        let mut old: Vec<_> = self.rules.iter().cloned().map(Some).collect();
        let mut next = vec![None; rules.len()];
        for (index, signature) in signatures.iter().enumerate() {
            if let Some(previous) = old
                .iter()
                .position(|s| s.as_ref().is_some_and(|s| s.signature == *signature))
            {
                next[index] = old[previous].take();
            } else if let Some(previous) = self
                .retired_rules
                .iter()
                .position(|s| s.signature == *signature)
            {
                let mut stamp = self.retired_rules.remove(previous);
                if now > 0 {
                    stamp.modified = now;
                }
                next[index] = Some(stamp);
            }
        }
        let same_length = old.len() == next.len();
        let mut changed = old.iter().any(Option::is_some) || self.rules.len() != rules.len();
        for (index, signature) in signatures.into_iter().enumerate() {
            if next[index].is_some() {
                continue;
            }
            changed = true;
            let previous = if same_length {
                old.get_mut(index)
                    .and_then(Option::take)
                    .or_else(|| old.iter_mut().find_map(Option::take))
            } else {
                None
            };
            let added = previous.as_ref().map_or(now, |stamp| stamp.added);
            next[index] = Some(Stamp {
                signature,
                added,
                modified: now,
            });
        }
        self.retired_rules.extend(old.into_iter().flatten());
        if self.retired_rules.len() > 100 {
            self.retired_rules.drain(..self.retired_rules.len() - 100);
        }
        self.rules = next.into_iter().flatten().collect();
        let mut groups = BTreeMap::<String, Vec<&Edit>>::new();
        for edit in edits.iter().filter(|edit| edit.batch) {
            groups.entry(batch_key(edit)).or_default().push(edit);
        }
        let active = groups.keys().cloned().collect::<BTreeSet<_>>();
        for (key, mut group) in groups {
            group.sort_by_key(|edit| serde_json::to_string(edit).unwrap_or_default());
            let signature = signature(&group)?;
            if self
                .batches
                .get(&key)
                .is_some_and(|stamp| stamp.signature == signature)
            {
                continue;
            }
            let added = self.batches.get(&key).map_or(now, |stamp| stamp.added);
            self.batches.insert(
                key,
                Stamp {
                    signature,
                    added,
                    modified: now,
                },
            );
            changed = true;
        }
        let mut retired = self
            .batches
            .iter()
            .filter(|(key, _)| !active.contains(*key))
            .map(|(key, stamp)| (key.clone(), stamp.modified))
            .collect::<Vec<_>>();
        retired.sort_by_key(|(_, time)| std::cmp::Reverse(*time));
        for (key, _) in retired.into_iter().skip(100) {
            self.batches.remove(&key);
        }
        if changed && now > 0 {
            self.tick = now;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(old: &str, new: &str) -> Rule {
        Rule {
            old: old.into(),
            new: new.into(),
            mode: "sub".into(),
            source: String::new(),
            table: "A".into(),
        }
    }

    #[test]
    fn timestamps_follow_edits_insertion_deletion_undo_and_restart() -> Result<()> {
        let a = rule("体力", "行动力");
        let b = rule("任务", "委托");
        let mut timeline = Timeline::default();
        timeline.observe(std::slice::from_ref(&a), &[], 0)?;
        assert_eq!(timeline.rules[0].added, 0);
        timeline.observe(&[a.clone(), b.clone()], &[], 100)?;
        assert_eq!(timeline.rules[1].added, 100);
        let edited = rule("任务", "差事");
        timeline.observe(&[a.clone(), edited.clone()], &[], 200)?;
        assert_eq!(
            (timeline.rules[1].added, timeline.rules[1].modified),
            (100, 200)
        );
        timeline.observe(std::slice::from_ref(&edited), &[], 300)?;
        assert_eq!(timeline.rules[0].added, 100);
        timeline.observe(&[a.clone(), edited.clone()], &[], 400)?;
        assert_eq!(timeline.rules[0].added, 0);
        let mut restored: Timeline = serde_json::from_slice(&serde_json::to_vec(&timeline)?)?;
        assert!(!restored.observe(&[a, edited], &[], 0)?);
        assert_eq!(restored.rules[1].modified, 200);
        Ok(())
    }
}
