use crate::{
    Control, Result, combined,
    model::{self, Edit, PreviousBatchScope},
    storage,
};
use bajptr_core::{
    database::{self, RowId, TextRow},
    fields,
    rules::{Evidence, Rule, Rules, View},
};
use serde::Serialize;
use serde_json::json;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn review_path(data: &Path, game: &str, version: &str) -> PathBuf {
    review_path_for(data, json!([game, version]))
}

pub(crate) fn read_reviews(data: &Path, game: &str, version: &str) -> Result<BTreeSet<String>> {
    let mut reviewed = read_review(&review_path(data, game, version))?;
    // Older workspaces stored decisions under two separate keys for the same version.
    for legacy in [json!([game, version, true]), json!([game, version, false])] {
        reviewed.extend(read_review(&review_path_for(data, legacy))?);
    }
    Ok(reviewed)
}

fn review_path_for(data: &Path, scope: serde_json::Value) -> PathBuf {
    let key = storage::digest(scope.to_string().as_bytes());
    data.join(".gui").join(format!("batch-review-{key}.json"))
}

pub(crate) fn review_key(old: &str, new: &str, row: &TextRow) -> String {
    storage::digest(json!([old, new, row]).to_string().as_bytes())
}

pub(crate) fn read_review(path: &Path) -> Result<BTreeSet<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) struct TableEvaluator<'a> {
    rules: &'a Rules,
    view: View,
    edits: HashMap<(&'a RowId, usize), &'a Edit>,
    evidence: Evidence,
}
impl<'a> TableEvaluator<'a> {
    pub(crate) fn new(
        table: &str,
        rules: &'a Rules,
        edits: &'a [Edit],
        game: &str,
        version: &str,
    ) -> Self {
        let mut indexed = HashMap::new();
        for edit in edits
            .iter()
            .filter(|edit| edit.game == game && edit.version == version && edit.table == table)
        {
            let previous: &mut &Edit = indexed.entry((&edit.row_id, edit.slot)).or_insert(edit);
            if edit.batch >= previous.batch {
                *previous = edit;
            }
        }
        Self {
            rules,
            view: rules.view(table),
            edits: indexed,
            evidence: Evidence::new(rules.values.len(), false),
        }
    }

    fn apply<'row>(&'row mut self, row: &'row TextRow) -> Result<Cow<'row, str>> {
        if let Some(edit) = self.edits.get(&(&row.1, row.2)) {
            if edit.original != row.3 {
                return Err(crate::invalid("定位记录的底稿原文已变化，请重新查找。"));
            }
            return Ok(Cow::Borrowed(&edit.replacement));
        }
        self.evidence.hits.fill(0);
        Ok(
            match self.view.apply(
                self.rules,
                &row.3,
                &mut self.evidence,
                !fields::has_cjk(&row.3),
            )? {
                Some(text) => Cow::Owned(text),
                None => Cow::Borrowed(&row.3),
            },
        )
    }
}

#[derive(Serialize)]
pub struct Item {
    pub table: String,
    pub label: String,
    pub old: String,
    pub new: String,
    pub count: usize,
    pub reason: String,
    pub missing: usize,
    pub attention: usize,
    pub quiet: bool,
    #[serde(skip)]
    pub previous_scopes: BTreeSet<PreviousBatchScope>,
    #[serde(skip)]
    pub missing_rows: Vec<TextRow>,
}
pub struct Plan {
    pub items: Vec<Item>,
    pub rules: Vec<Rule>,
    pub edits: Vec<Edit>,
    pub relocated: usize,
    pub relocations: Vec<Relocation>,
}

#[derive(Serialize)]
pub struct Relocation {
    pub table: String,
    pub label: String,
    pub original: String,
    pub moved: bool,
    pub reason: String,
}

fn relocate(
    con: &rusqlite::Connection,
    edits: &[Edit],
    game: &str,
    version: &str,
    available: &[String],
    control: &Control<'_>,
) -> Result<(Vec<Edit>, Vec<Relocation>)> {
    let mut values = edits.to_vec();
    let mut notes = Vec::new();
    let mut tables = BTreeMap::<&str, Vec<usize>>::new();
    for (index, edit) in edits
        .iter()
        .enumerate()
        .filter(|(_, edit)| edit.game == game && edit.version == version)
    {
        tables.entry(&edit.table).or_default().push(index);
    }
    for (table, indices) in tables {
        control.log(&format!("核对修改位置：{}", model::title(table)))?;
        let mut groups = Vec::<Vec<usize>>::new();
        let mut positions = HashMap::new();
        let mut candidates = HashMap::<&str, Vec<(database::RowId, usize)>>::new();
        for index in indices {
            let edit = &edits[index];
            let position = *positions
                .entry((edit.row_id.clone(), edit.slot))
                .or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
            groups[position].push(index);
            if !edit.original.is_empty() {
                candidates.entry(&edit.original).or_default();
            }
        }
        let mut verified = HashSet::new();
        let exists = available.iter().any(|name| name == table);
        if exists {
            let slots = positions.keys().map(|(_, slot)| *slot);
            database::visit_table_with_text_slots(con, table, slots, control, |row| {
                let address = (row.1, row.2);
                if let Some(&position) = positions.get(&address)
                    && edits[groups[position][0]].original == row.3
                {
                    verified.insert(address.clone());
                }
                if let Some(matches) = candidates.get_mut(row.3.as_str())
                    && matches.len() < 2
                {
                    matches.push(address);
                }
                Ok(())
            })?;
        }
        let mut claims = HashMap::new();
        for group in &groups {
            let edit = &edits[group[0]];
            if !verified.contains(&(edit.row_id.clone(), edit.slot))
                && let Some(matches) = candidates.get(edit.original.as_str())
                && matches.len() == 1
            {
                *claims.entry(matches[0].clone()).or_insert(0) += 1;
            }
        }
        for group in groups {
            control.check()?;
            let edit = &edits[group[0]];
            if verified.contains(&(edit.row_id.clone(), edit.slot)) {
                continue;
            }
            let matches = candidates
                .get(edit.original.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut moved = false;
            let reason = if !exists {
                "当前底稿中没有这张文本表，请检查游戏目录和版本。".into()
            } else if edit.original.is_empty() {
                "保存的原文为空，无法通过全文确定位置。".into()
            } else if matches.is_empty() {
                "未找到完整原文，请在「查找」中核对文字是否已变化。".into()
            } else if matches.len() > 1 {
                "多处文本与完整原文相同，无法唯一确定位置。请在「编辑批量修改」或「单条修改」中移除旧记录，再从「查找」选择所需文本。".into()
            } else if positions.contains_key(&matches[0]) {
                "对应文本已有其他修改，未覆盖。请先核对现有修改和旧记录。".into()
            } else if claims[&matches[0]] > 1 {
                "多条旧记录指向同一段文本，无法确定对应关系。请核对后移除重复旧记录。".into()
            } else {
                let (row_id, slot) = &matches[0];
                let anchor = database::location_at(
                    con,
                    &(table.into(), row_id.clone(), *slot, edit.original.clone()),
                    control,
                )?
                .and_then(|located| located.anchor);
                for index in group {
                    values[index].row_id = row_id.clone();
                    values[index].slot = *slot;
                    values[index].anchor.clone_from(&anchor);
                }
                moved = true;
                format!(
                    "完整原文唯一匹配，已重新定位到记录 {}，字段 {slot}。",
                    row_id.display()
                )
            };
            notes.push(Relocation {
                table: table.into(),
                label: model::title(table),
                original: edit.original.clone(),
                moved,
                reason,
            });
        }
    }
    model::validate_edits(&values)?;
    Ok((values, notes))
}

fn extend_scope(rules: &[Rule], old: &str, new: &str, table: &str) -> Result<Vec<Rule>> {
    let mut rules = rules.to_vec();
    let matching = rules
        .iter()
        .position(|rule| {
            combined::rule_matches(rule, old, new)
                && model::scopes(&rule.table)
                    .unwrap()
                    .unwrap()
                    .iter()
                    .any(|name| name == table)
        })
        .or_else(|| {
            rules
                .iter()
                .position(|rule| combined::rule_matches(rule, old, new))
        });
    if let Some(index) = matching {
        let mut scope = model::scopes(&rules[index].table)?
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>();
        scope.insert(table.into());
        rules[index].table = serde_json::to_string(&scope)?;
    } else {
        rules.push(Rule {
            old: old.into(),
            new: new.into(),
            mode: "sub".into(),
            source: "批量修改整理".into(),
            table: serde_json::to_string(&[table])?,
        });
    }
    model::normalize(rules)
}

pub fn plan(
    con: &rusqlite::Connection,
    rules: &[Rule],
    edits: &[Edit],
    game: &str,
    version: &str,
    control: &Control<'_>,
) -> Result<Plan> {
    model::validate_edits(edits)?;
    let tables = database::tables(con, &database::default_tables())?;
    let (located, relocations) = relocate(con, edits, game, version, &tables, control)?;
    let edits = located.as_slice();
    let single_addresses = edits
        .iter()
        .filter(|edit| edit.game == game && edit.version == version && !edit.batch)
        .map(|edit| (edit.table.as_str(), &edit.row_id, edit.slot))
        .collect::<HashSet<_>>();
    let blocked = relocations
        .iter()
        .filter(|item| !item.moved)
        .map(|item| item.table.as_str())
        .collect::<HashSet<_>>();
    let mut groups = BTreeMap::<_, Vec<&Edit>>::new();
    for edit in edits
        .iter()
        .filter(|edit| edit.game == game && edit.version == version && edit.batch)
    {
        if let Some(op) = &edit.batch_operation {
            groups
                .entry((edit.table.clone(), op.old.clone(), op.new.clone()))
                .or_default()
                .push(edit);
        }
    }
    let mut result = Plan {
        items: vec![],
        rules: rules.to_vec(),
        edits: edits.to_vec(),
        relocated: relocations.iter().filter(|item| item.moved).count(),
        relocations: vec![],
    };
    for ((table, old, new), members) in groups {
        control.log(&format!(
            "核对批量范围：{} · {old} → {new}",
            model::title(&table)
        ))?;
        let mut item = Item {
            label: model::title(&table),
            table: table.clone(),
            old: old.clone(),
            new: new.clone(),
            count: members.len(),
            reason: String::new(),
            missing: 0,
            attention: 0,
            quiet: false,
            previous_scopes: BTreeSet::new(),
            missing_rows: Vec::new(),
        };
        if old == new {
            item.reason = "替换前后文字相同，无需转换。".into();
        } else if !tables.contains(&table) {
            item.reason = "当前底稿中没有这张文本表。请检查游戏目录和版本；旧版记录可通过「迁移旧版修改」重新定位。".into();
        } else if blocked.contains(table.as_str()) {
            item.reason =
                "本表仍有无法确定位置的修改，保留指定文本；具体原因和处理方法见位置核对日志。"
                    .into();
        } else if members.iter().any(|member| {
            single_addresses.contains(&(member.table.as_str(), &member.row_id, member.slot))
        }) {
            item.reason = "包含单条修改，保留现有优先级。".into();
        }
        if !item.reason.is_empty() {
            result.items.push(item);
            continue;
        }
        let next_rules = match extend_scope(&result.rules, &old, &new, &table) {
            Ok(rules) => rules,
            Err(crate::Error::Invalid(_)) => {
                item.reason = "与现有词条范围冲突。".into();
                result.items.push(item);
                continue;
            }
            Err(error) => return Err(error),
        };
        let next_edits = result
            .edits
            .iter()
            .filter(|edit| {
                !(edit.table == table && combined::batch_matches(edit, game, version, &old, &new))
            })
            .cloned()
            .collect::<Vec<_>>();
        let before_rules = Rules::new(result.rules.clone())?;
        let after_rules = Rules::new(next_rules.clone())?;
        let scope_rule = Rules::new(vec![Rule {
            old: old.clone(),
            new: new.clone(),
            mode: "sub".into(),
            source: String::new(),
            table: table.clone(),
        }])?;
        let mut before = TableEvaluator::new(&table, &before_rules, &result.edits, game, version);
        let mut after = TableEvaluator::new(&table, &after_rules, &next_edits, game, version);
        let mut scope = TableEvaluator::new(&table, &scope_rule, &[], game, version);
        let addresses = members
            .iter()
            .map(|edit| ((edit.row_id.clone(), edit.slot), *edit))
            .collect::<HashMap<_, _>>();
        let mut unseen = addresses.clone();
        let mut invalid_originals = Vec::new();
        let mut seen = 0;
        let mut missing_rows = Vec::new();
        let mut outside = 0;
        let mut changed = 0;
        let mut same = true;
        let slots = members.iter().map(|edit| edit.slot);
        database::visit_table_with_text_slots(con, &table, slots, control, |row| {
            let member = addresses.get(&(row.1.clone(), row.2));
            let expected = scope.apply(&row)? != row.3.as_str();
            if expected && member.is_none() {
                missing_rows.push(row.clone());
            }
            if !expected && member.is_some() {
                outside += 1;
            }
            if let Some(edit) = member {
                seen += 1;
                unseen.remove(&(row.1.clone(), row.2));
                let op = edit.batch_operation.as_ref().unwrap();
                if edit.original != row.3 || edit.replacement != op.before.replace(&old, &new) {
                    changed += 1;
                    invalid_originals.push(edit.original.as_str());
                    return Ok(());
                }
            }
            if before.apply(&row)? != after.apply(&row)? {
                same = false;
            }
            Ok(())
        })?;
        if seen != members.len() || changed > 0 {
            invalid_originals.extend(unseen.values().map(|edit| edit.original.as_str()));
            invalid_originals.sort_unstable();
            invalid_originals.dedup();
            let originals = invalid_originals
                .iter()
                .take(3)
                .map(|original| {
                    format!(
                        "「{}」",
                        original
                            .chars()
                            .take(80)
                            .collect::<String>()
                            .replace(['\r', '\n'], " ")
                    )
                })
                .collect::<Vec<_>>()
                .join("、");
            item.reason = format!(
                "{} 段记录无法定位，{changed} 段记录内容不一致。请在「编辑批量修改」中搜索并移除旧记录，再从「查找」重新选择文本进行批量修改。需重选的原文（最多列出 3 条）：{originals}",
                members.len() - seen
            );
        } else if outside > 0 {
            item.reason = format!(
                "{outside} 段指定文本不在整表替换范围内。保留指定文本即可；若需整表替换，请先核对指定文本的原文与词条匹配范围。"
            );
        } else if !missing_rows.is_empty() {
            item.missing = missing_rows.len();
            item.missing_rows = missing_rows;
            item.previous_scopes = members
                .iter()
                .filter_map(|edit| edit.batch_operation.as_ref()?.previous_scope.clone())
                .collect();
            item.reason = format!(
                "本表还有 {} 段匹配文本未包含在本批修改中，已保留指定文本范围。",
                item.missing
            );
        } else if !same {
            item.reason = "转换会改变当前结果，保留原批量修改。若需调整，请先预览并核对同一文本表的其他词条。".into();
        } else {
            result.rules = next_rules;
            result.edits = next_edits;
        }
        result.items.push(item);
    }
    result.relocations = relocations;
    Ok(result)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviousMatch {
    Absent,
    Unselected,
    Uncertain,
}

impl Item {
    pub(crate) fn compare_previous(
        &self,
        con: &rusqlite::Connection,
        selected: &[&Edit],
        current: &[Edit],
        control: &Control<'_>,
    ) -> Result<Vec<PreviousMatch>> {
        use PreviousMatch::*;
        if !database::tables(con, &database::default_tables())?.contains(&self.table) {
            return Ok(vec![Absent; self.missing_rows.len()]);
        }
        let mut hits = HashMap::<&str, HashMap<usize, Vec<RowId>>>::new();
        for row in &self.missing_rows {
            hits.entry(&row.3).or_default().entry(row.2).or_default();
        }
        let slots = selected.iter().map(|edit| edit.slot);
        database::visit_table_with_text_slots(con, &self.table, slots, control, |row| {
            if let Some(addresses) = hits
                .get_mut(row.3.as_str())
                .and_then(|slots| slots.get_mut(&row.2))
                && addresses.len() < 2
            {
                addresses.push(row.1);
            }
            Ok(())
        })?;
        let mut current_counts = HashMap::new();
        for row in &self.missing_rows {
            *current_counts
                .entry((row.2, row.3.as_str()))
                .or_insert(0usize) += 1;
        }
        for edit in current.iter().filter(|edit| edit.table == self.table) {
            if let Some(count) = current_counts.get_mut(&(edit.slot, edit.original.as_str())) {
                *count += 1;
            }
        }
        Ok(self
            .missing_rows
            .iter()
            .map(|row| {
                let key = (row.2, row.3.as_str());
                match hits[row.3.as_str()][&row.2].as_slice() {
                    [] => Absent,
                    [one]
                        if current_counts[&key] == 1
                            && !selected.iter().any(|edit| {
                                edit.slot == row.2
                                    && (&edit.row_id == one || edit.original == row.3)
                            }) =>
                    {
                        Unselected
                    }
                    _ => Uncertain,
                }
            })
            .collect())
    }

    pub(crate) fn retain_review_rows(
        &mut self,
        previous: &[Vec<PreviousMatch>],
        mut current: TableEvaluator<'_>,
        reviewed: &BTreeSet<String>,
        control: &Control<'_>,
    ) -> Result<()> {
        self.attention = 0;
        for (index, row) in std::mem::take(&mut self.missing_rows)
            .into_iter()
            .enumerate()
        {
            control.check()?;
            let all =
                |kind| !previous.is_empty() && previous.iter().all(|source| source[index] == kind);
            if all(PreviousMatch::Unselected)
                || (!reviewed.is_empty()
                    && reviewed.contains(&review_key(&self.old, &self.new, &row)))
                || current.edits.contains_key(&(&row.1, row.2))
                || !current.apply(&row)?.contains(&self.old)
            {
                continue;
            }
            self.attention += usize::from(all(PreviousMatch::Absent));
            self.missing_rows.push(row);
        }
        self.missing = self.missing_rows.len();
        self.quiet = self.missing == 0;
        self.reason = if self.quiet {
            "已保留指定文本范围。".into()
        } else {
            format!(
                "本表还有 {} 段文本可补改，已保留指定文本范围。",
                self.missing
            )
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BatchOperation;
    use database::RowId;
    use rusqlite::Connection;

    fn database(rows: &[(&str, i64, &str)]) -> Connection {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch("CREATE TABLE LocalizeDBSchema(Id INTEGER PRIMARY KEY,Bytes BLOB);CREATE TABLE LocalizeSkillDBSchema(Id INTEGER PRIMARY KEY,Bytes BLOB);").unwrap();
        for &(table, id, text) in rows {
            let mut blob = vec![0u8; 24];
            blob[0..4].copy_from_slice(&16u32.to_le_bytes());
            blob[4..6].copy_from_slice(&6u16.to_le_bytes());
            blob[6..8].copy_from_slice(&8u16.to_le_bytes());
            blob[8..10].copy_from_slice(&4u16.to_le_bytes());
            blob[16..20].copy_from_slice(&12i32.to_le_bytes());
            blob[20..24].copy_from_slice(&4u32.to_le_bytes());
            blob.extend_from_slice(&(text.len() as u32).to_le_bytes());
            blob.extend_from_slice(text.as_bytes());
            blob.push(0);
            con.execute(
                &format!("INSERT INTO {table} VALUES(?1,?2)"),
                rusqlite::params![id, blob],
            )
            .unwrap();
        }
        con
    }
    fn edit(table: &str, id: i64, text: &str) -> Edit {
        Edit {
            game: "game".into(),
            version: "a".repeat(64),
            table: table.into(),
            row_id: RowId::Integer(id),
            slot: 0,
            original: text.into(),
            replacement: text.replace("任务", "委托"),
            batch: true,
            anchor: None,
            batch_operation: Some(BatchOperation {
                id: "1".repeat(32),
                old: "任务".into(),
                new: "委托".into(),
                before: text.into(),
                previous_scope: None,
            }),
        }
    }
    fn rule(old: &str, new: &str) -> Rule {
        Rule {
            old: old.into(),
            new: new.into(),
            mode: "sub".into(),
            source: String::new(),
            table: "LocalizeDBSchema".into(),
        }
    }
    fn check(con: &Connection, rules: &[Rule], edits: &[Edit]) -> Plan {
        plan(
            con,
            rules,
            edits,
            "game",
            &"a".repeat(64),
            &Control::new(&|_| true),
        )
        .unwrap()
    }

    #[test]
    fn cached_table_evaluation_matches_existing_results_and_errors() {
        let version = "a".repeat(64);
        let mut values = vec![rule("任务", "委托")];
        values.push(Rule {
            old: "ASCII".into(),
            new: "ascii-exact".into(),
            mode: "exact".into(),
            source: String::new(),
            table: String::new(),
        });
        values.push(Rule {
            old: "任务(\\d+)".into(),
            new: "编号\\g<1>".into(),
            mode: "re".into(),
            source: String::new(),
            table: "LocalizeSkillDBSchema".into(),
        });
        let rules = Rules::new(values).unwrap();
        let batch = edit("LocalizeDBSchema", 1, "任务あ");
        let mut single = batch.clone();
        single.batch = false;
        single.batch_operation = None;
        single.replacement = "单条覆盖".into();
        let mut text_id = single.clone();
        text_id.row_id = RowId::Text("text-id".into());
        text_id.original = "任务".into();
        let mut other_slot = single.clone();
        other_slot.slot = 1;
        other_slot.original = "任务".into();
        let mut other_game = edit("LocalizeDBSchema", 2, "任务");
        other_game.game = "other-game".into();
        let mut other_version = edit("LocalizeDBSchema", 3, "任务");
        other_version.version = "b".repeat(64);
        for edits in [
            vec![
                batch.clone(),
                single.clone(),
                text_id.clone(),
                other_slot.clone(),
                other_game.clone(),
                other_version.clone(),
            ],
            vec![
                single,
                batch,
                text_id,
                other_slot,
                other_game,
                other_version,
            ],
        ] {
            for table in ["LocalizeDBSchema", "LocalizeSkillDBSchema"] {
                let mut cached = TableEvaluator::new(table, &rules, &edits, "game", &version);
                for _ in 0..3 {
                    for (id, slot, text) in [
                        (RowId::Integer(1), 0, "任务あ"),
                        (RowId::Integer(1), 1, "任务"),
                        (RowId::Text("text-id".into()), 0, "任务"),
                        (RowId::Integer(2), 0, "任务"),
                        (RowId::Integer(3), 0, "任务"),
                        (RowId::Integer(4), 0, "任务12"),
                        (RowId::Integer(5), 0, "ASCII"),
                        (RowId::Integer(1), 0, "原文变化"),
                    ] {
                        let row = (table.into(), id, slot, text.into());
                        let expected = model::effective(&row, &edits, "game", &version, &rules)
                            .map(|value| value.0)
                            .map_err(|error| error.to_string());
                        let actual = cached
                            .apply(&row)
                            .map(Cow::into_owned)
                            .map_err(|error| error.to_string());
                        assert_eq!(actual, expected, "table={table}, row={row:?}");
                    }
                }
            }
        }
        let rules = Rules::new(vec![Rule {
            old: "(任务.*)".into(),
            new: "\\g<1>".into(),
            mode: "re".into(),
            source: String::new(),
            table: String::new(),
        }])
        .unwrap();
        let row = (
            "LocalizeDBSchema".into(),
            RowId::Integer(1),
            0,
            "任务\t".into(),
        );
        let expected = model::effective(&row, &[], "game", &version, &rules)
            .unwrap_err()
            .to_string();
        let mut cached = TableEvaluator::new(&row.0, &rules, &[], "game", &version);
        assert_eq!(cached.apply(&row).unwrap_err().to_string(), expected);
    }

    #[test]
    fn complete_table_becomes_a_rule_while_partial_scope_and_other_versions_remain() {
        let con = database(&[
            ("LocalizeDBSchema", 1, "任务一"),
            ("LocalizeDBSchema", 2, "任务二"),
            ("LocalizeSkillDBSchema", 1, "任务甲"),
            ("LocalizeSkillDBSchema", 2, "任务乙"),
        ]);
        let mut prior = edit("LocalizeDBSchema", 1, "任务一");
        prior.version = "b".repeat(64);
        prior.batch_operation.as_mut().unwrap().id = "2".repeat(32);
        let edits = vec![
            edit("LocalizeDBSchema", 1, "任务一"),
            edit("LocalizeDBSchema", 2, "任务二"),
            edit("LocalizeSkillDBSchema", 1, "任务甲"),
            prior.clone(),
        ];
        let result = check(&con, &[], &edits);
        assert_eq!(result.rules.len(), 1);
        assert_eq!(result.rules[0].table, "LocalizeDBSchema");
        assert_eq!(result.edits, vec![edits[2].clone(), prior]);
        assert!(result.items[0].reason.is_empty());
        assert!(result.items[1].reason.contains("还有 1 段匹配文本"));
        assert_eq!(result.items[1].missing, 1);
        assert_eq!(result.items[1].missing_rows[0].1, RowId::Integer(2));
        assert_eq!(result.items[1].missing_rows[0].3, "任务乙");
    }
    #[test]
    fn equivalence_includes_existing_rules_and_rejects_precedence_changes() {
        let con = database(&[("LocalizeDBSchema", 1, "任务甲")]);
        let mut edit = edit("LocalizeDBSchema", 1, "任务甲");
        edit.batch_operation.as_mut().unwrap().before = "任务乙".into();
        edit.replacement = "委托乙".into();
        let compatible = check(&con, &[rule("甲", "乙")], &[edit.clone()]);
        assert!(compatible.edits.is_empty());
        let conflict = check(&con, &[rule("任务甲", "任务乙")], &[edit.clone()]);
        assert_eq!(conflict.edits, vec![edit]);
        assert!(conflict.items[0].reason.contains("改变当前结果"));
    }
    #[test]
    fn batch_promotion_includes_kana_and_preserves_paired_single_edits() {
        let con = database(&[("LocalizeDBSchema", 1, "任务あ")]);
        let edit = edit("LocalizeDBSchema", 1, "任务あ");
        assert!(
            check(&con, &[], std::slice::from_ref(&edit))
                .edits
                .is_empty()
        );
        let mut single = edit.clone();
        single.batch = false;
        single.batch_operation = None;
        single.replacement = "单条内容".into();
        let paired = vec![edit, single];
        let result = check(&con, &[], &paired);
        assert_eq!(result.edits, paired);
        assert!(result.items[0].reason.contains("单条修改"));
    }
    #[test]
    fn missing_originals_keep_records_and_report_each_searchable_text() {
        let con = database(&[("LocalizeDBSchema", 1, "任务甲")]);
        let stale = edit("LocalizeDBSchema", 2, "任务乙");
        let changed = edit("LocalizeDBSchema", 1, "任务旧文");
        let edits = vec![stale, changed];
        let result = check(&con, &[], &edits);
        assert!(result.rules.is_empty());
        assert_eq!(result.edits, edits);
        assert_eq!(result.relocations.len(), 2);
        assert!(
            result
                .relocations
                .iter()
                .all(|item| !item.moved && item.reason.contains("未找到完整原文"))
        );
        assert!(
            result
                .relocations
                .iter()
                .any(|item| item.original == "任务乙")
        );
        assert!(
            result
                .relocations
                .iter()
                .any(|item| item.original == "任务旧文")
        );
    }
    #[test]
    fn full_original_relocates_unique_current_records_and_preserves_paired_layers() {
        let con = database(&[
            ("LocalizeDBSchema", 5, "任务甲"),
            ("LocalizeDBSchema", 6, "任务乙"),
        ]);
        let batch = edit("LocalizeDBSchema", 1, "任务甲");
        let mut single = batch.clone();
        single.batch = false;
        single.batch_operation = None;
        single.replacement = "特别指定的结果".into();
        let mut other_game = edit("LocalizeDBSchema", 2, "任务乙");
        other_game.game = "other".into();
        other_game.batch_operation.as_mut().unwrap().id = "2".repeat(32);
        let mut other_version = other_game.clone();
        other_version.game = "game".into();
        other_version.version = "b".repeat(64);
        other_version.batch_operation.as_mut().unwrap().id = "3".repeat(32);
        let edits = vec![batch, single, other_game.clone(), other_version.clone()];
        let result = check(&con, &[], &edits);
        assert_eq!(result.relocated, 1);
        assert!(result.rules.is_empty());
        assert_eq!(result.edits.len(), 4);
        for (index, edit) in edits.iter().take(2).enumerate() {
            let mut expected = edit.clone();
            expected.row_id = RowId::Integer(5);
            let saved = &result.edits[index];
            assert!(
                saved
                    .anchor
                    .as_ref()
                    .is_some_and(|anchor| anchor.next.is_some())
            );
            expected.anchor.clone_from(&saved.anchor);
            assert_eq!(saved, &expected);
        }
        assert_eq!(&result.edits[2..], &[other_game, other_version]);
    }
    #[test]
    fn relocation_can_promote_complete_batches_but_never_expands_partial_batches() {
        let con = database(&[
            ("LocalizeDBSchema", 5, "任务甲"),
            ("LocalizeDBSchema", 6, "任务乙"),
        ]);
        let first = edit("LocalizeDBSchema", 1, "任务甲");
        let partial = check(&con, &[], std::slice::from_ref(&first));
        assert_eq!(partial.relocated, 1);
        assert!(partial.rules.is_empty());
        assert_eq!(partial.edits[0].row_id, RowId::Integer(5));
        assert!(partial.items[0].reason.contains("还有 1 段"));
        let second = edit("LocalizeDBSchema", 2, "任务乙");
        let complete = check(&con, &[], &[first, second]);
        assert_eq!(complete.relocated, 2);
        assert_eq!(complete.rules.len(), 1);
        assert!(complete.edits.is_empty());
    }
    #[test]
    fn relocation_keeps_ambiguous_occupied_competing_and_missing_records_unchanged() {
        for (rows, edits, reason) in [
            (
                vec![
                    ("LocalizeDBSchema", 5, "任务甲"),
                    ("LocalizeDBSchema", 6, "任务甲"),
                ],
                vec![edit("LocalizeDBSchema", 1, "任务甲")],
                "多处",
            ),
            (
                vec![("LocalizeDBSchema", 5, "任务甲")],
                vec![
                    edit("LocalizeDBSchema", 1, "任务甲"),
                    edit("LocalizeDBSchema", 5, "任务甲"),
                ],
                "已有",
            ),
            (
                vec![("LocalizeDBSchema", 5, "任务甲")],
                vec![
                    edit("LocalizeDBSchema", 1, "任务甲"),
                    edit("LocalizeDBSchema", 2, "任务甲"),
                ],
                "多条",
            ),
            (
                vec![("LocalizeDBSchema", 5, "任务乙")],
                vec![edit("LocalizeDBSchema", 1, "任务甲")],
                "未找到",
            ),
        ] {
            let con = database(&rows);
            let result = check(&con, &[], &edits);
            assert_eq!(result.relocated, 0);
            assert_eq!(result.edits, edits);
            assert!(result.rules.is_empty());
            assert!(
                result
                    .relocations
                    .iter()
                    .any(|item| item.reason.contains(reason))
            );
        }
    }
    #[test]
    fn conflicting_scope_and_unclassified_legacy_records_are_not_promoted() {
        let con = database(&[("LocalizeDBSchema", 1, "任务甲")]);
        let edit = edit("LocalizeDBSchema", 1, "任务甲");
        let result = check(&con, &[rule("任务", "其他")], std::slice::from_ref(&edit));
        assert_eq!(result.edits, vec![edit.clone()]);
        assert!(!result.items[0].reason.is_empty());
        let mut legacy = edit;
        legacy.batch_operation = None;
        let result = check(&con, &[], &[legacy.clone()]);
        assert!(result.items.is_empty());
        assert_eq!(result.edits, vec![legacy]);
    }
    #[test]
    fn incomplete_text_parsing_refuses_a_promotion_plan() {
        let con = database(&[("LocalizeDBSchema", 1, "任务甲")]);
        con.execute_batch("INSERT INTO LocalizeDBSchema VALUES(2,X'010203');")
            .unwrap();
        assert!(
            plan(
                &con,
                &[],
                &[edit("LocalizeDBSchema", 1, "任务甲")],
                "game",
                &"a".repeat(64),
                &Control::new(&|_| true)
            )
            .is_err()
        );
    }
}
