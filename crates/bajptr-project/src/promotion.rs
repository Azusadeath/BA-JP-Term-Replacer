use crate::{
    Control, Result, combined,
    model::{self, Edit},
};
use bajptr_core::{
    database,
    rules::{Rule, Rules},
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Serialize)]
pub struct Item {
    pub table: String,
    pub label: String,
    pub old: String,
    pub new: String,
    pub count: usize,
    pub reason: String,
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
            database::visit_table(con, table, control, |row| {
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
                for index in group {
                    values[index].row_id = row_id.clone();
                    values[index].slot = *slot;
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
    skip: bool,
    control: &Control<'_>,
) -> Result<Plan> {
    model::validate_edits(edits)?;
    let tables = database::tables(con, &database::default_tables())?;
    let (located, relocations) = relocate(con, edits, game, version, &tables, control)?;
    let edits = located.as_slice();
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
            edits
                .iter()
                .any(|edit| !edit.batch && edit.same_address(member))
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
        let addresses = members
            .iter()
            .map(|edit| ((edit.row_id.clone(), edit.slot), *edit))
            .collect::<HashMap<_, _>>();
        let mut unseen = addresses.clone();
        let mut invalid_originals = Vec::new();
        let mut seen = 0;
        let mut missing = 0;
        let mut outside = 0;
        let mut changed = 0;
        let mut same = true;
        database::visit_table(con, &table, control, |row| {
            let member = addresses.get(&(row.1.clone(), row.2));
            let expected =
                model::effective(&row, &[], game, version, &scope_rule, skip)?.0 != row.3;
            if expected && member.is_none() {
                missing += 1;
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
            if model::effective(&row, &result.edits, game, version, &before_rules, skip)?.0
                != model::effective(&row, &next_edits, game, version, &after_rules, skip)?.0
            {
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
                "{outside} 段指定文本不在整表替换范围内（含假名的文本会受跳过设置影响）。保留指定文本即可；若需整表替换，请先核对「跳过日文假名的文本」设置和原文字。"
            );
        } else if missing > 0 {
            item.reason = format!(
                "本表还有 {missing} 段匹配文本未包含在本批修改中。仅修改部分文本时无需处理；若需整表替换，请在「查找」中选中遗漏文本，以相同内容批量修改后再次整理。"
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
            batch_operation: Some(BatchOperation {
                id: "1".repeat(32),
                old: "任务".into(),
                new: "委托".into(),
                before: text.into(),
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
    fn check(con: &Connection, rules: &[Rule], edits: &[Edit], skip: bool) -> Plan {
        plan(
            con,
            rules,
            edits,
            "game",
            &"a".repeat(64),
            skip,
            &Control::new(&|_| true),
        )
        .unwrap()
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
        let result = check(&con, &[], &edits, true);
        assert_eq!(result.rules.len(), 1);
        assert_eq!(result.rules[0].table, "LocalizeDBSchema");
        assert_eq!(result.edits, vec![edits[2].clone(), prior]);
        assert!(result.items[0].reason.is_empty());
        assert!(result.items[1].reason.contains("还有 1 段匹配文本"));
        assert!(result.items[1].reason.contains("无需处理"));
        assert!(result.items[1].reason.contains("再次整理"));
    }
    #[test]
    fn equivalence_includes_existing_rules_and_rejects_precedence_changes() {
        let con = database(&[("LocalizeDBSchema", 1, "任务甲")]);
        let mut edit = edit("LocalizeDBSchema", 1, "任务甲");
        edit.batch_operation.as_mut().unwrap().before = "任务乙".into();
        edit.replacement = "委托乙".into();
        let compatible = check(&con, &[rule("甲", "乙")], &[edit.clone()], true);
        assert!(compatible.edits.is_empty());
        let conflict = check(&con, &[rule("任务甲", "任务乙")], &[edit.clone()], true);
        assert_eq!(conflict.edits, vec![edit]);
        assert!(conflict.items[0].reason.contains("改变当前结果"));
    }
    #[test]
    fn paired_single_edits_and_kana_overrides_keep_their_existing_behavior() {
        let con = database(&[("LocalizeDBSchema", 1, "任务あ")]);
        let edit = edit("LocalizeDBSchema", 1, "任务あ");
        let skipped = check(&con, &[], std::slice::from_ref(&edit), true);
        assert_eq!(skipped.edits, vec![edit.clone()]);
        assert!(
            skipped.items[0]
                .reason
                .contains("1 段指定文本不在整表替换范围内")
        );
        assert!(skipped.items[0].reason.contains("跳过日文假名"));
        assert!(
            check(&con, &[], std::slice::from_ref(&edit), false)
                .edits
                .is_empty()
        );
        let mut single = edit.clone();
        single.batch = false;
        single.batch_operation = None;
        single.replacement = "单条内容".into();
        let paired = vec![edit, single];
        let result = check(&con, &[], &paired, false);
        assert_eq!(result.edits, paired);
        assert!(result.items[0].reason.contains("单条修改"));
    }
    #[test]
    fn missing_originals_keep_records_and_report_each_searchable_text() {
        let con = database(&[("LocalizeDBSchema", 1, "任务甲")]);
        let stale = edit("LocalizeDBSchema", 2, "任务乙");
        let changed = edit("LocalizeDBSchema", 1, "任务旧文");
        let edits = vec![stale, changed];
        let result = check(&con, &[], &edits, true);
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
        let result = check(&con, &[], &edits, true);
        assert_eq!(result.relocated, 1);
        assert!(result.rules.is_empty());
        assert_eq!(result.edits.len(), 4);
        for (index, edit) in edits.iter().take(2).enumerate() {
            let mut expected = edit.clone();
            expected.row_id = RowId::Integer(5);
            assert_eq!(result.edits[index], expected);
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
        let partial = check(&con, &[], std::slice::from_ref(&first), true);
        assert_eq!(partial.relocated, 1);
        assert!(partial.rules.is_empty());
        assert_eq!(partial.edits[0].row_id, RowId::Integer(5));
        assert!(partial.items[0].reason.contains("还有 1 段"));
        let second = edit("LocalizeDBSchema", 2, "任务乙");
        let complete = check(&con, &[], &[first, second], true);
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
            let result = check(&con, &[], &edits, true);
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
        let result = check(
            &con,
            &[rule("任务", "其他")],
            std::slice::from_ref(&edit),
            true,
        );
        assert_eq!(result.edits, vec![edit.clone()]);
        assert!(!result.items[0].reason.is_empty());
        let mut legacy = edit;
        legacy.batch_operation = None;
        let result = check(&con, &[], &[legacy.clone()], true);
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
                true,
                &Control::new(&|_| true)
            )
            .is_err()
        );
    }
}
