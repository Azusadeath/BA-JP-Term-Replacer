//! Fixed source inventories and notices for tables introduced by resource updates.
use crate::{Result, invalid, model};
use bajptr_core::rules::Rule;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Inventory {
    pub known: BTreeSet<String>,
    pub notices: Vec<Notice>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Notice {
    pub table: String,
    pub old: String,
    pub new: String,
    pub mode: String,
    pub fields: u64,
    pub example: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Sources {
    format: u32,
    pub games: BTreeMap<String, Inventory>,
}
impl Default for Sources {
    fn default() -> Self {
        Self {
            format: 1,
            games: BTreeMap::new(),
        }
    }
}
impl Sources {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = serde_json::from_slice(bytes)?;
        if value.format != 1
            || value
                .games
                .values()
                .any(|g| g.known.iter().any(|s| s.is_empty()))
        {
            return Err(invalid("文本表记录格式无效，未重建或覆盖原记录。"));
        }
        Ok(value)
    }
}

/// Preserve table-specific overrides when freezing a legacy global rule.
/// An empty fixed set keeps a fully overridden rule dormant, rather than deleting it.
pub fn freeze(rules: &[Rule], known: &BTreeSet<String>) -> Result<Vec<Rule>> {
    if rules.iter().all(|r| !r.table.is_empty()) {
        return model::normalize_scopes(rules.to_vec());
    }
    let mut overrides = HashMap::<_, BTreeSet<_>>::new();
    for rule in rules.iter().filter(|r| !r.table.is_empty()) {
        if let Some(scope) = model::scopes(&rule.table)? {
            overrides
                .entry((rule.old.as_str(), rule.mode.as_str()))
                .or_default()
                .extend(scope);
        }
    }
    let mut fixed = rules.to_vec();
    for rule in &mut fixed {
        if !rule.table.is_empty() {
            continue;
        }
        let names: Vec<_> = match overrides.get(&(rule.old.as_str(), rule.mode.as_str())) {
            Some(scope) => known.difference(scope).collect(),
            None => known.iter().collect(),
        };
        rule.table = serde_json::to_string(&names)?;
    }
    model::normalize_scopes(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bajptr_core::rules::{Evidence, Rules};
    #[test]
    fn freezing_keeps_overrides_and_does_not_add_new_sources() {
        let rule = |table: &str, new: &str| Rule {
            old: "任务".into(),
            new: new.into(),
            mode: "sub".into(),
            source: String::new(),
            table: table.into(),
        };
        let original = vec![rule("", "委托"), rule("B", "工作")];
        let fixed = freeze(&original, &BTreeSet::from(["A".into(), "B".into()])).unwrap();
        assert_eq!(model::scopes(&fixed[0].table).unwrap().unwrap(), vec!["A"]);
        let compiled = Rules::new(fixed).unwrap();
        let apply = |table| {
            compiled
                .view(table)
                .apply(&compiled, "任务", &mut Evidence::new(2, false), false)
                .unwrap()
        };
        assert_eq!(apply("A"), Some("委托".into()));
        assert_eq!(apply("B"), Some("工作".into()));
        assert_eq!(apply("New"), None);
        let dormant = freeze(&original, &BTreeSet::from(["B".into()])).unwrap();
        assert_eq!(dormant.len(), 2);
        assert_eq!(dormant[0].table, "[]");
    }
}
