use crate::state::{slice, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub struct ReviewGroup {
    pub members: Vec<usize>,
    pub item: Value,
    pub targets: Vec<String>,
    pub chosen: BTreeSet<usize>,
    pub edits: BTreeMap<usize, String>,
    pub candidate_page: usize,
}
impl ReviewGroup {
    pub fn can_select_all(&self) -> bool {
        self.members.len() > 1
            && self.targets.len() <= self.members.len()
            && self.item["kind"] != "manual"
            && slice(&self.item, "proposals").iter().all(Value::is_null)
    }

    pub fn result_ready(&self, candidate: usize) -> bool {
        let proposal = &self.item["proposals"][candidate];
        if proposal.is_null() {
            return true;
        }
        self.edits.contains_key(&candidate)
            || (proposal["replacement"].is_string() && text(proposal, "reason").is_empty())
    }
}

fn targets(item: &Value) -> Vec<String> {
    slice(item, "candidates").iter().map(target).collect()
}
fn target(candidate: &Value) -> String {
    let row = &candidate["row"];
    json!([row[0], row[1], row[2]]).to_string()
}

pub struct MigrationForm {
    pub open: bool,
    pub plan: Value,
    pub groups: Vec<ReviewGroup>,
    pub selected: Option<usize>,
    pub filter: String,
    pub error: String,
    pub keyword: String,
    pub search_message: String,
    automatic_targets: BTreeSet<String>,
}
impl MigrationForm {
    pub fn new(plan: Value) -> Self {
        let mut groups: Vec<ReviewGroup> = Vec::new();
        let mut positions = HashMap::<(String, Vec<String>), usize>::new();
        let automatic: BTreeMap<usize, usize> = slice(&plan, "automatic")
            .iter()
            .filter_map(|choice| {
                Some((
                    choice["item"].as_u64()? as usize,
                    choice["candidate"].as_u64()? as usize,
                ))
            })
            .collect();
        let automatic_targets = automatic
            .iter()
            .map(|(&item, &candidate)| target(&plan["items"][item]["candidates"][candidate]))
            .collect();
        for (index, item) in slice(&plan, "items").iter().enumerate() {
            if automatic.contains_key(&index) {
                continue;
            }
            let targets = targets(item);
            let key = text(item, "group_key");
            let signature = (
                if key.is_empty()
                    || targets.is_empty()
                    || slice(item, "proposals").iter().any(|p| !p.is_null())
                    || item["equivalent_group"] == false
                {
                    format!("ungrouped-{index}")
                } else {
                    key
                },
                targets.clone(),
            );
            if let Some(&position) = positions.get(&signature) {
                groups[position].members.push(index);
            } else {
                positions.insert(signature, groups.len());
                groups.push(ReviewGroup {
                    members: vec![index],
                    item: item.clone(),
                    targets,
                    chosen: BTreeSet::new(),
                    edits: BTreeMap::new(),
                    candidate_page: 0,
                });
            }
        }
        let selected = (!groups.is_empty()).then_some(0);
        Self {
            open: true,
            plan,
            groups,
            selected,
            filter: String::new(),
            error: String::new(),
            keyword: String::new(),
            search_message: String::new(),
            automatic_targets,
        }
        .select_known_locations()
    }
    fn select_known_locations(mut self) -> Self {
        for index in 0..self.groups.len() {
            let group = &self.groups[index];
            if group.members.len() == 1
                && let Some(candidate) =
                    group.item["suggested"].as_u64().map(|value| value as usize)
                && !self.reserved(index, candidate)
            {
                self.groups[index].chosen.insert(candidate);
            }
        }
        self
    }
    pub fn automatic_count(&self) -> usize {
        slice(&self.plan, "automatic").len()
    }
    pub fn pending_count(&self) -> usize {
        self.groups.iter().map(|group| group.members.len()).sum()
    }
    pub fn automatically_selected(&self, group: usize, candidate: usize) -> bool {
        self.automatic_targets
            .contains(&self.groups[group].targets[candidate])
    }
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        self.groups
            .iter()
            .enumerate()
            .filter(|(_, g)| {
                format!(
                    "{} {} {}",
                    text(&g.item, "original"),
                    text(&g.item, "replacement"),
                    bajptr_project::model::title(&text(&g.item, "table"))
                )
                .to_lowercase()
                .contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }
    pub fn sync_selection(&mut self) {
        let visible = self.visible();
        if !visible.iter().any(|&i| Some(i) == self.selected) {
            self.selected = visible.first().copied();
        }
    }
    pub fn next(&mut self) {
        let visible = self.visible();
        self.selected = self
            .selected
            .and_then(|old| visible.iter().copied().find(|&i| i > old))
            .or_else(|| visible.first().copied());
    }
    pub fn reserved(&self, group: usize, candidate: usize) -> bool {
        let target = &self.groups[group].targets[candidate];
        self.automatic_targets.contains(target)
            || self
                .groups
                .iter()
                .enumerate()
                .any(|(i, g)| i != group && g.chosen.iter().any(|&c| &g.targets[c] == target))
    }
    pub fn selected_count(&self) -> usize {
        self.automatic_count()
            + self
                .groups
                .iter()
                .map(|group| {
                    group
                        .chosen
                        .iter()
                        .filter(|&&c| group.result_ready(c))
                        .count()
                })
                .sum::<usize>()
    }
    pub fn payload(&self) -> Value {
        let mut choices = slice(&self.plan, "automatic").to_vec();
        choices.extend(self.groups.iter().flat_map(|g| {
            g.members
                .iter()
                .zip(g.chosen.iter().filter(|&&c| g.result_ready(c)))
                .map(|(&item, &candidate)| {
                    let mut choice = json!({"item":item,"candidate":candidate});
                    if let Some(edit) = g.edits.get(&candidate) {
                        choice["final_text"] = json!(edit);
                    }
                    choice
                })
        }));
        json!({"nonce":self.plan["nonce"],"revision":self.plan["revision"],"choices":choices})
    }
    pub fn apply_search_result(&mut self, data: &Value) {
        if data["nonce"] != self.plan["nonce"] {
            return;
        }
        let Some(item_index) = data["item"].as_u64().map(|n| n as usize) else {
            return;
        };
        let Some(index) = self
            .groups
            .iter()
            .position(|g| g.members.contains(&item_index))
        else {
            return;
        };
        let added = data["added"].as_u64().unwrap_or_default();
        let total = data["total"].as_u64().unwrap_or_default();
        let occupied = data["occupied"].as_u64().unwrap_or_default();
        self.search_message = if total == 0 {
            "新版底稿中没有匹配文本。".into()
        } else if total > 30 {
            format!("找到 {total} 段，已加入 {added} 个候选。可输入更具体的文字。")
        } else if occupied > 0 {
            format!("已加入 {added} 个候选，{occupied} 段已有新版修改。")
        } else {
            format!("已加入 {added} 个候选。")
        };
        if added == 0 {
            return;
        }
        let item = data["value"].clone();
        self.plan["items"][item_index] = item.clone();
        let group = &mut self.groups[index];
        if group.members.len() > 1 {
            group.members.retain(|&member| member != item_index);
            group.item = self.plan["items"][group.members[0]].clone();
            let mut detached = BTreeSet::new();
            if group.chosen.len() > group.members.len() {
                detached.extend(group.chosen.pop_last());
            }
            self.groups.insert(
                index + 1,
                ReviewGroup {
                    members: vec![item_index],
                    targets: targets(&item),
                    item,
                    chosen: detached,
                    edits: BTreeMap::new(),
                    candidate_page: 0,
                },
            );
            self.selected = Some(index + 1);
        } else {
            group.targets = targets(&item);
            group.item = item;
            group.candidate_page = (group.targets.len().saturating_sub(added as usize)) / 20;
            self.selected = Some(index);
        }
    }
    pub fn toggle_context(&mut self, index: usize, candidates: &[usize], checked: bool) {
        self.error.clear();
        if !checked {
            for c in candidates {
                self.groups[index].chosen.remove(c);
            }
            return;
        }
        let group = &self.groups[index];
        let mut next = if group.members.len() == 1 {
            BTreeSet::new()
        } else {
            group.chosen.clone()
        };
        next.extend(candidates);
        if next.len() > group.members.len() || candidates.iter().any(|&c| self.reserved(index, c)) {
            return;
        }
        self.groups[index].chosen = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    fn item(key: &str, targets: &[i64]) -> Value {
        json!({"group_key":key,"original":"神秘","replacement":"神奇","table":"Text","candidates":targets.iter().map(|id|json!({"row":["Text",id,0,"神秘"],"context":format!("上下文{id}")})).collect::<Vec<_>>()})
    }
    #[test]
    fn checking_targets_updates_the_save_payload_without_expanding_scope() {
        let mut form = MigrationForm::new(
            json!({"items":[item("same",&[1,2]),item("same",&[1,2]),item("single",&[3]),item("subset",&[4,5,6])]}),
        );
        assert_eq!(form.groups.len(), 3);
        assert!(form.groups[0].can_select_all());
        form.toggle_context(0, &[0, 1], true);
        form.toggle_context(1, &[0], true);
        assert_eq!(form.selected_count(), 3);
        let payload = form.payload();
        assert_eq!(slice(&payload, "choices").len(), 3);
        assert_eq!(payload["choices"][0], json!({"item":0,"candidate":0}));
        assert_eq!(payload["choices"][1], json!({"item":1,"candidate":1}));
        form.toggle_context(2, &[0, 1], true);
        assert!(form.groups[2].chosen.is_empty());
        form.toggle_context(2, &[1], true);
        form.toggle_context(2, &[2], true);
        assert_eq!(form.groups[2].chosen, BTreeSet::from([2]));
        assert_eq!(form.selected_count(), 4);
        assert_eq!(
            form.groups
                .iter()
                .flat_map(|g| g.chosen.iter().map(|&c| &g.targets[c]))
                .collect::<HashSet<_>>()
                .len(),
            4
        );
    }
    #[test]
    fn competing_replacements_cannot_select_the_same_target_twice() {
        let mut form = MigrationForm::new(json!({"items":[item("one",&[1]),item("other",&[1])]}));
        form.toggle_context(0, &[0], true);
        form.toggle_context(1, &[0], true);
        assert!(form.groups[1].chosen.is_empty());
        assert_eq!(form.selected_count(), 1);
    }

    #[test]
    fn automatic_choices_and_resolved_conflicts_share_one_save() {
        let mut conflict = item("conflict", &[2]);
        conflict["suggested"] = json!(0);
        conflict["proposals"] = json!([{"replacement":null,"reason":"文字冲突"}]);
        let mut form = MigrationForm::new(json!({
            "items":[item("automatic",&[1]),conflict],
            "automatic":[{"item":0,"candidate":0}]
        }));
        assert_eq!(form.pending_count(), 1);
        assert_eq!(form.selected_count(), 1);
        assert_eq!(form.groups[0].chosen, BTreeSet::from([0]));
        form.groups[0].edits.insert(0, "最终整句".into());
        assert_eq!(form.selected_count(), 2);
        assert_eq!(
            form.payload()["choices"][1],
            json!({"item":1,"candidate":0,"final_text":"最终整句"})
        );
    }
}
