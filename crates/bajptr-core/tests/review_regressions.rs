use bajptr_core::{
    Control,
    database::{self, Options, RowId, TextEdit},
    fields,
    rules::{self, Evidence, Rule, Rules},
};
use rusqlite::Connection;
use serde_json::json;
use std::collections::HashMap;

fn ctl() -> Control<'static> {
    Control::new(&|_| true)
}
fn rule(old: &str, new: &str, mode: &str) -> Rule {
    Rule {
        old: old.into(),
        new: new.into(),
        mode: mode.into(),
        source: String::new(),
        table: String::new(),
    }
}
fn options() -> Options {
    serde_json::from_value(json!({"explain":true,"tables":"Localize"})).unwrap()
}
fn blob(strings: &[&str]) -> Vec<u8> {
    let root = 16usize;
    let object = 4 + strings.len() * 4;
    let mut bytes = vec![0; root + object];
    bytes[0..4].copy_from_slice(&(root as u32).to_le_bytes());
    bytes[4..6].copy_from_slice(&((4 + strings.len() * 2) as u16).to_le_bytes());
    bytes[6..8].copy_from_slice(&(object as u16).to_le_bytes());
    bytes[root..root + 4].copy_from_slice(&((root - 4) as i32).to_le_bytes());
    for (slot, text) in strings.iter().enumerate() {
        bytes[8 + slot * 2..10 + slot * 2].copy_from_slice(&((4 + slot * 4) as u16).to_le_bytes());
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        let fpos = root + 4 + slot * 4;
        let start = bytes.len();
        bytes[fpos..fpos + 4].copy_from_slice(&((start - fpos) as u32).to_le_bytes());
        bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
    }
    bytes
}
fn simple_db() -> Connection {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch("CREATE TABLE LocalizeDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB)")
        .unwrap();
    con
}

#[test]
fn overlapping_field_candidates_keep_scope_and_rule_priority() {
    let mut scoped = rule("甲乙", "乙", "sub");
    scoped.table = "LocalizeDBSchema".into();
    let rules = Rules::new(vec![
        rule("甲乙", "错", "sub"),
        rule("乙", "丁", "sub"),
        scoped,
        rule("(?<=丙)甲", "戊", "re"),
        rule("(?:丁|庚)", "辛", "re"),
    ])
    .unwrap();
    let view = rules.view("LocalizeDBSchema");
    let mut evidence = Evidence::new(rules.values.len(), true);
    assert_eq!(
        view.apply(&rules, "甲乙丙甲庚", &mut evidence, false)
            .unwrap()
            .as_deref(),
        Some("乙丙戊辛")
    );
    assert_eq!(evidence.hits, [0, 0, 1, 1, 1]);
    assert!(
        evidence.misses[0]
            .as_ref()
            .unwrap()
            .reason
            .contains("专用规则")
    );
    assert!(evidence.misses[1].as_ref().unwrap().reason.contains("重叠"));
    assert!(std::sync::Arc::ptr_eq(
        &view,
        &rules.view("LocalizeDBSchema")
    ));
}

#[test]
fn blank_table_filter_uses_the_default_scope_for_preview_and_patch() {
    let con = simple_db();
    con.execute_batch(
        "CREATE TABLE SkillDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB);
         CREATE TABLE ItemDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB);
         CREATE TABLE SystemDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB);",
    )
    .unwrap();
    let original = blob(&["攻击"]);
    for table in [
        "LocalizeDBSchema",
        "SkillDBSchema",
        "ItemDBSchema",
        "SystemDBSchema",
    ] {
        con.execute(&format!("INSERT INTO {table} VALUES(1,?)"), [&original])
            .unwrap();
    }
    assert_eq!(database::tables(&con, ".*").unwrap().len(), 4);
    let rules = Rules::new(vec![rule("攻击", "爆炸", "sub")]).unwrap();
    for (pattern, write) in [(" \t", false), ("", true)] {
        let mut options = options();
        options.tables = pattern.into();
        let stats =
            database::patch_database(&con, None, None, &rules, &options, write, &ctl()).unwrap();
        assert_eq!(stats.tables, ["LocalizeDBSchema"]);
        assert_eq!(stats.fields_changed, 1);
    }
    for table in ["SkillDBSchema", "ItemDBSchema", "SystemDBSchema"] {
        let bytes: Vec<u8> = con
            .query_row(&format!("SELECT Bytes FROM {table}"), [], |row| row.get(0))
            .unwrap();
        assert_eq!(bytes, original, "{table}");
    }
}

#[test]
fn incomplete_string_slots_block_full_coverage_with_precise_diagnostics() {
    let good = blob(&["任务", "待核对"]);
    let field = fields::parse(&good, true).unwrap()[1].clone();
    let end = field.spos + 4 + field.text.len();
    let mut mutations = Vec::new();
    let mut bad = good.clone();
    bad[10..12].copy_from_slice(&10u16.to_le_bytes());
    mutations.push((bad, "字段位置"));
    let mut bad = good.clone();
    bad[field.fpos..field.fpos + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    mutations.push((bad, "字符串位置"));
    let mut bad = good.clone();
    bad[field.spos..field.spos + 4]
        .copy_from_slice(&((fields::MAX_STRING + 1) as u32).to_le_bytes());
    mutations.push((bad, "字符串长度"));
    let mut bad = good.clone();
    bad.truncate(end);
    mutations.push((bad, "字符串结束位置"));
    let mut bad = good.clone();
    bad[end] = 1;
    mutations.push((bad, "结束标记"));
    let mut bad = good.clone();
    bad[field.spos + 4] = 0xff;
    mutations.push((bad, "UTF-8"));

    for (bad, reason) in mutations {
        let con = simple_db();
        con.execute("INSERT INTO LocalizeDBSchema VALUES (1,?)", [&bad])
            .unwrap();
        let error = database::visit_table_with_text_slots(
            &con,
            "LocalizeDBSchema",
            [1],
            &ctl(),
            |_| Ok(()),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("记录 1") && error.contains("字段 1") && error.contains(reason),
            "{error}"
        );
        con.execute("INSERT INTO LocalizeDBSchema VALUES (2,?)", [&good])
            .unwrap();
        let inferred =
            database::visit_table(&con, "LocalizeDBSchema", &ctl(), |_| Ok(())).unwrap_err();
        assert_eq!(inferred.to_string(), error);
    }

    let con = simple_db();
    let mut scalar = good.clone();
    scalar[field.fpos..field.fpos + 4].copy_from_slice(&123456u32.to_le_bytes());
    con.execute("INSERT INTO LocalizeDBSchema VALUES (1,?)", [&scalar])
        .unwrap();
    let mut visited = Vec::new();
    database::visit_table_with_text_slots(&con, "LocalizeDBSchema", [0], &ctl(), |row| {
        visited.push(row.3);
        Ok(())
    })
    .unwrap();
    assert_eq!(visited, ["任务"]);
    con.execute("INSERT INTO LocalizeDBSchema VALUES (2,?)", [&scalar])
        .unwrap();
    con.execute("INSERT INTO LocalizeDBSchema VALUES (3,?)", [&good])
        .unwrap();
    database::visit_table(&con, "LocalizeDBSchema", &ctl(), |_| Ok(())).unwrap();
}

#[test]
fn search_export_and_migration_include_non_cjk_text() {
    let con = simple_db();
    let originals = ["English only", "한국어 문장"];
    con.execute(
        "INSERT INTO LocalizeDBSchema(Bytes) VALUES (?)",
        [blob(&originals)],
    )
    .unwrap();
    let edits = originals
        .iter()
        .enumerate()
        .map(|(slot, original)| TextEdit {
            table: "LocalizeDBSchema".into(),
            row_id: RowId::Integer(1),
            slot,
            original: (*original).into(),
            replacement: "changed".into(),
        })
        .collect::<Vec<_>>();
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        database::export(&con, "Localize", directory.path(), &ctl()).unwrap(),
        2
    );
    let exported = std::fs::read_to_string(directory.path().join("LocalizeDBSchema.tsv")).unwrap();
    let candidates = database::locate_originals(&con, &edits, &ctl()).unwrap();
    assert_eq!(candidates.len(), originals.len());
    for (slot, original) in originals.into_iter().enumerate() {
        let search = database::SearchIndex::build(&con, "Localize", original, &ctl()).unwrap();
        let page = search.page("", 10, 0, &ctl()).unwrap();
        assert_eq!(page.total, 1);
        let row = &page.rows[0];
        assert_eq!((row.2, row.3.as_str()), (slot, original));
        assert!(exported.contains(&format!("1\t{slot}\t{original}\n")));
        let location = database::location_at(&con, row, &ctl()).unwrap().unwrap();
        assert_eq!(&location.row, row);
        assert_eq!(&candidates[slot].row, row);
        assert_eq!(location.context, originals[1 - slot]);
    }
}

#[test]
fn migration_candidates_keep_full_context_and_distinct_addresses() {
    let con = simple_db();
    let prefix = "相同上下文".repeat(30);
    for suffix in ["甲", "乙"] {
        let context = format!("{prefix}{suffix}");
        con.execute(
            "INSERT INTO LocalizeDBSchema(Bytes) VALUES (?)",
            [blob(&["待迁移", &context])],
        )
        .unwrap();
    }
    let edits = vec![TextEdit {
        table: "LocalizeDBSchema".into(),
        row_id: RowId::Integer(999),
        slot: 0,
        original: "待迁移".into(),
        replacement: "修改后".into(),
    }];
    let candidates = database::locate_originals(&con, &edits, &ctl()).unwrap();
    assert_eq!(candidates.len(), 2);
    assert_ne!(candidates[0].row.1, candidates[1].row.1);
    assert_eq!(candidates[0].context, format!("{prefix}甲"));
    assert_eq!(candidates[1].context, format!("{prefix}乙"));
}

#[test]
fn new_source_matches_report_original_text_without_applying_rules() {
    let con = simple_db();
    for text in ["任务开始", "任务", "任务アロナ"] {
        con.execute(
            "INSERT INTO LocalizeDBSchema(Bytes) VALUES (?)",
            [blob(&[text])],
        )
        .unwrap();
    }
    let mut rules = vec![
        rule("任务", "委托", "sub"),
        rule("任务", "工作", "exact"),
        rule("任.开始", "起步", "re"),
    ];
    for rule in &mut rules {
        rule.table = "LocalizeOtherDBSchema".into();
    }
    let compiled = Rules::new(rules).unwrap();
    let report =
        database::source_rule_matches(&con, &["LocalizeDBSchema".into()], &compiled, &ctl())
            .unwrap();
    assert_eq!(
        report
            .iter()
            .map(|m| (m.rule_index, m.fields))
            .collect::<Vec<_>>(),
        vec![(0, 3), (1, 1), (2, 1)]
    );
    assert!(
        report
            .iter()
            .all(|m| m.table == "LocalizeDBSchema" && m.example.contains("任务"))
    );
    let before: Vec<u8> = con
        .query_row(
            "SELECT Bytes FROM LocalizeDBSchema WHERE rowid=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fields::parse(&before, false).unwrap()[0].text, "任务开始");
}

#[test]
fn empty_then_nonempty_matches_agree_in_sample_preview_and_written_blob() {
    let (sample, hits) = rules::regex_sample("a*|b", "X", "b").unwrap();
    assert_eq!((sample.as_str(), hits), ("XXX", 3));
    let rules = Rules::new(vec![rule("a*|b", "X", "re")]).unwrap();
    let mut evidence = Evidence::new(1, true);
    assert_eq!(
        rules
            .view("LocalizeDBSchema")
            .apply(&rules, "b", &mut evidence, false)
            .unwrap()
            .unwrap(),
        sample
    );
    let con = simple_db();
    let original = blob(&["b中"]);
    con.execute("INSERT INTO LocalizeDBSchema VALUES (1,?)", [&original])
        .unwrap();
    let expected = rules::regex_sample("a*|b", "X", "b中").unwrap();
    let preview =
        database::patch_database(&con, None, None, &rules, &options(), false, &ctl()).unwrap();
    let written =
        database::patch_database(&con, None, None, &rules, &options(), true, &ctl()).unwrap();
    assert_eq!(preview.hits, written.hits);
    assert_eq!(written.hits[0], expected.1 as u64);
    let bytes: Vec<u8> = con
        .query_row("SELECT Bytes FROM LocalizeDBSchema", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fields::parse(&bytes, true).unwrap()[0].text, expected.0);
}

#[test]
fn empty_match_does_not_allow_a_lower_priority_rule_to_consume_its_position() {
    let rules = Rules::new(vec![rule("a*", "X", "re"), rule("b", "Y", "re")]).unwrap();
    let mut evidence = Evidence::new(2, true);
    assert_eq!(
        rules
            .view("LocalizeDBSchema")
            .apply(&rules, "b中", &mut evidence, false)
            .unwrap()
            .unwrap(),
        "XbX中X"
    );
    assert_eq!(evidence.hits, vec![3, 0]);
}

#[test]
fn replacement_errors_identify_rule_record_and_field_in_preview_and_write() {
    for write in [false, true] {
        let con = simple_db();
        let first = blob(&["任务"]);
        let bad = blob(&["攻击"]);
        con.execute("INSERT INTO LocalizeDBSchema VALUES (1,?)", [&first])
            .unwrap();
        con.execute("INSERT INTO LocalizeDBSchema VALUES (2,?)", [&bad])
            .unwrap();
        let rules =
            Rules::new(vec![rule("任务", "委托", "sub"), rule("攻击", r"\t", "re")]).unwrap();
        let error = database::patch_database(&con, None, None, &rules, &options(), write, &ctl())
            .unwrap_err()
            .to_string();
        for detail in [
            "LocalizeDBSchema",
            "记录 2",
            "字段 0",
            "第 2 条词条",
            "控制字符",
        ] {
            assert!(error.contains(detail), "missing {detail}: {error}");
        }
        let unchanged: Vec<u8> = con
            .query_row("SELECT Bytes FROM LocalizeDBSchema WHERE Id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            unchanged, first,
            "failed writes must roll back earlier valid rows"
        );
    }
}

#[test]
fn dynamic_output_limit_is_not_reported_as_a_bad_resource() {
    let con = simple_db();
    let text = "攻".repeat(fields::MAX_STRING / 6 + 5);
    let original = blob(&[&text]);
    con.execute("INSERT INTO LocalizeDBSchema VALUES (7,?)", [&original])
        .unwrap();
    let rules = Rules::new(vec![rule("(.+)", r"\1\1", "re")]).unwrap();
    let error = database::patch_database(&con, None, None, &rules, &options(), false, &ctl())
        .unwrap_err()
        .to_string();
    for detail in [
        "LocalizeDBSchema",
        "记录 7",
        "字段 0",
        "第 1 条词条",
        "过长",
    ] {
        assert!(error.contains(detail), "{error}");
    }
}

#[test]
fn malformed_records_have_precise_preview_diagnostics_and_stop_writes() {
    let con = simple_db();
    let mut bad = blob(&["攻击"]);
    bad[4..6].copy_from_slice(&5u16.to_le_bytes());
    con.execute("INSERT INTO LocalizeDBSchema VALUES (9,?)", [&bad])
        .unwrap();
    let rules = Rules::new(vec![rule("攻击", "爆炸", "sub")]).unwrap();
    let preview =
        database::patch_database(&con, None, None, &rules, &options(), false, &ctl()).unwrap();
    assert_eq!(preview.parse_failed, 1);
    let details = serde_json::to_value(preview).unwrap()["diagnostics"].to_string();
    for detail in ["LocalizeDBSchema", "9", "bad vtable length"] {
        assert!(details.contains(detail), "{details}");
    }
    let error = database::patch_database(&con, None, None, &rules, &options(), true, &ctl())
        .unwrap_err()
        .to_string();
    for detail in ["LocalizeDBSchema", "记录 9", "bad vtable length"] {
        assert!(error.contains(detail), "{error}");
    }
    let unchanged: Vec<u8> = con
        .query_row("SELECT Bytes FROM LocalizeDBSchema", [], |r| r.get(0))
        .unwrap();
    assert_eq!(unchanged, bad);
}

#[test]
fn without_rowid_tables_use_their_real_single_primary_key() {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE LocalizeDBSchema(Id TEXT PRIMARY KEY, Bytes BLOB) WITHOUT ROWID",
    )
    .unwrap();
    con.execute(
        "INSERT INTO LocalizeDBSchema VALUES ('text-id',?)",
        [blob(&["攻击"])],
    )
    .unwrap();
    let rules = Rules::new(vec![rule("攻击", "爆炸", "sub")]).unwrap();
    let stats =
        database::patch_database(&con, None, None, &rules, &options(), true, &ctl()).unwrap();
    assert_eq!(stats.parse_failed, 0);
    assert_eq!(stats.rows_changed, 1);
    let (found, total) = database::scan(&con, "Localize", "爆炸", 10, &ctl()).unwrap();
    assert_eq!(total, 1);
    assert_eq!(found[0].1, RowId::Text("text-id".into()));
}

#[test]
fn search_pages_cover_all_matches_and_keep_source_table_filtering_exact() {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE LocalizeDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB); CREATE TABLE SkillDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB)",
    )
    .unwrap();
    for id in 0..3 {
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (?,?)",
            rusqlite::params![id, blob(&["攻击"])],
        )
        .unwrap();
    }
    for id in 0..2 {
        con.execute(
            "INSERT INTO SkillDBSchema VALUES (?,?)",
            rusqlite::params![id, blob(&["攻击"])],
        )
        .unwrap();
    }
    let (first, total) = database::scan_page(&con, "Localize|Skill", "攻击", 2, 0, &ctl()).unwrap();
    let (second, total_again) =
        database::scan_page(&con, "Localize|Skill", "攻击", 2, 2, &ctl()).unwrap();
    assert_eq!(
        (first.len(), second.len(), total, total_again),
        (2, 2, 5, 5)
    );
    let (localize, local_total) =
        database::scan_page(&con, "^(?:LocalizeDBSchema)$", "攻击", 10, 0, &ctl()).unwrap();
    assert_eq!(local_total, 3);
    assert!(localize.iter().all(|row| row.0 == "LocalizeDBSchema"));
}

#[test]
fn search_sources_cover_unloaded_pages_and_exclude_nonmatching_tables() {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE LocalizeDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB);
        CREATE TABLE SkillDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB);
        CREATE TABLE EmptyDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB);
        CREATE TABLE KanaDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB)",
    )
    .unwrap();
    for id in 0..501 {
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (?,?)",
            rusqlite::params![id, blob(&["任务"])],
        )
        .unwrap();
    }
    con.execute(
        "INSERT INTO SkillDBSchema VALUES (1,?)",
        [blob(&["任务说明"])],
    )
    .unwrap();
    con.execute(
        "INSERT INTO EmptyDBSchema VALUES (1,?)",
        [blob(&["无匹配"])],
    )
    .unwrap();
    con.execute("INSERT INTO KanaDBSchema VALUES (1,?)", [blob(&["ア任务"])])
        .unwrap();
    let page = database::scan_page_with_sources(&con, "DBSchema$", "任务", 500, 0, &ctl()).unwrap();
    assert_eq!((page.rows.len(), page.total), (500, 503));
    assert_eq!(page.rows[0].0, "KanaDBSchema");
    assert_eq!(
        page.matching_tables,
        vec!["KanaDBSchema", "LocalizeDBSchema", "SkillDBSchema"]
    );
    let metadata =
        database::scan_page_with_sources(&con, "DBSchema$", "任务", 0, 0, &ctl()).unwrap();
    assert!(metadata.rows.is_empty());
    assert_eq!(metadata.matching_tables, page.matching_tables);
    let empty =
        database::scan_page_with_sources(&con, "DBSchema$", "不存在", 500, 0, &ctl()).unwrap();
    assert!(empty.matching_tables.is_empty());

    let index = database::SearchIndex::build(&con, "DBSchema$", "任务", &ctl()).unwrap();
    con.execute_batch("DROP TABLE LocalizeDBSchema; DROP TABLE SkillDBSchema")
        .unwrap();
    let next = index.page("", 500, 500, &ctl()).unwrap();
    assert_eq!((next.rows.len(), next.total), (3, 503));
    assert_eq!(next.matching_tables, page.matching_tables);
    let filtered = index.page("SkillDBSchema", 500, 0, &ctl()).unwrap();
    assert_eq!((filtered.rows.len(), filtered.total), (1, 1));
    assert_eq!(filtered.rows[0].0, "SkillDBSchema");
    assert_eq!(filtered.matching_tables, page.matching_tables);
    assert!(index.page("", 500, 503, &ctl()).unwrap().rows.is_empty());
    assert!(index.page("", 500, 0, &Control::new(&|_| false)).is_err());
}

#[test]
fn search_includes_text_containing_kana() {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch("CREATE TABLE LocalizeDBSchema(Id INTEGER PRIMARY KEY, Bytes BLOB)")
        .unwrap();
    con.execute(
        "INSERT INTO LocalizeDBSchema VALUES (1,?)",
        [blob(&["攻撃", "ｱｲﾃﾑ攻撃"])],
    )
    .unwrap();
    let (rows, total) = database::scan_page(&con, "Localize", "攻撃", 10, 0, &ctl()).unwrap();
    assert_eq!(total, 2);
    assert_eq!(rows[1].3, "ｱｲﾃﾑ攻撃");
}

#[test]
fn composite_primary_keys_bind_each_selected_record_in_the_same_table() {
    for columns in ["Id", "Key"] {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch(&format!("CREATE TABLE LocalizeDBSchema({columns} INTEGER, Lang TEXT, Bytes BLOB, PRIMARY KEY({columns},Lang)) WITHOUT ROWID")).unwrap();
        let original = blob(&["攻击"]);
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (1,'cn',?)",
            [&original],
        )
        .unwrap();
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (1,'jp',?)",
            [&original],
        )
        .unwrap();
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (1,'kr',?)",
            [&original],
        )
        .unwrap();
        let (found, total) = database::scan(&con, "Localize", "攻击", 10, &ctl()).unwrap();
        assert_eq!(total, 3, "{columns}");
        assert_ne!(
            found[0].1, found[1].1,
            "a partial Key column must not identify both rows"
        );
        let mut options = options();
        for (row, replacement) in found.iter().zip(["中文替换", "日文替换"]) {
            options.edits.push(TextEdit {
                table: "LocalizeDBSchema".into(),
                row_id: row.1.clone(),
                slot: 0,
                original: "攻击".into(),
                replacement: replacement.into(),
            });
        }
        let rules = Rules::new(vec![]).unwrap();
        let stats =
            database::patch_database(&con, None, None, &rules, &options, true, &ctl()).unwrap();
        assert_eq!(stats.rows_changed, 2);
        assert_eq!(stats.parse_failed, 0);
        for (lang, expected) in [("cn", "中文替换"), ("jp", "日文替换"), ("kr", "攻击")] {
            let actual: Vec<u8> = con
                .query_row(
                    "SELECT Bytes FROM LocalizeDBSchema WHERE Lang=?",
                    [lang],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(fields::parse(&actual, true).unwrap()[0].text, expected);
        }
    }
}

#[test]
fn non_text_tables_are_reported_separately_instead_of_blocking_text_updates() {
    let con = simple_db();
    con.execute_batch("CREATE TABLE LocalizeMetadata(Id INTEGER PRIMARY KEY, Value TEXT)")
        .unwrap();
    con.execute(
        "INSERT INTO LocalizeDBSchema VALUES (1,?)",
        [blob(&["攻击"])],
    )
    .unwrap();
    let rules = Rules::new(vec![rule("攻击", "爆炸", "sub")]).unwrap();
    let stats =
        database::patch_database(&con, None, None, &rules, &options(), true, &ctl()).unwrap();
    assert_eq!(stats.parse_failed, 0);
    assert_eq!(stats.rows_changed, 1);
    let stats = serde_json::to_value(stats).unwrap();
    assert_eq!(stats["tables_skipped"], 1);
    assert!(
        stats["diagnostics"]
            .to_string()
            .contains("LocalizeMetadata")
    );
}

#[test]
fn one_invalid_slot_does_not_discard_valid_fields_or_other_records() {
    let con = simple_db();
    let mut original = blob(&["攻击", "其他"]);
    let object = original[6..8].to_vec();
    original[10..12].copy_from_slice(&object);
    con.execute("INSERT INTO LocalizeDBSchema VALUES (1,?)", [&original])
        .unwrap();
    let rules = Rules::new(vec![rule("攻击", "爆炸", "sub")]).unwrap();
    let stats =
        database::patch_database(&con, None, None, &rules, &options(), true, &ctl()).unwrap();
    assert_eq!(stats.rows_changed, 1);
    assert_eq!(stats.parse_failed, 0);
    let details = serde_json::to_value(stats).unwrap();
    assert_eq!(details["fields_skipped"], 1);
    let bytes: Vec<u8> = con
        .query_row("SELECT Bytes FROM LocalizeDBSchema", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        &bytes[..28],
        &original[..28],
        "unrecognized slot and all offsets stay unchanged"
    );
    assert_eq!(fields::parse(&bytes, true).unwrap()[0].text, "爆炸");
    assert!(
        details["diagnostics"]
            .to_string()
            .contains("LocalizeDBSchema")
    );
}

#[test]
fn kana_text_supports_substring_rules_and_explicit_edit_override() {
    for prefix in ["ｱｲﾃﾑ", "ㇰ", "アイテム", "ひらがな"] {
        let text = format!("{prefix}かな");
        let rules = Rules::new(vec![rule("かな", "中文名称", "sub")]).unwrap();
        let original = blob(&[&text]);
        let mut evidence = Evidence::new(1, true);
        let (changed, _) = database::patch_blob(
            &original,
            &rules,
            &rules.view("LocalizeDBSchema"),
            &mut evidence,
            &options(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            fields::parse(changed.as_ref().unwrap(), true).unwrap()[0].text,
            format!("{prefix}中文名称")
        );
        let (explicit, _) = database::patch_blob(
            &original,
            &rules,
            &rules.view("LocalizeDBSchema"),
            &mut evidence,
            &options(),
            &HashMap::from([(0, "指定修改".into())]),
        )
        .unwrap();
        assert_eq!(
            fields::parse(explicit.as_ref().unwrap(), true).unwrap()[0].text,
            "指定修改"
        );
    }
}

#[test]
fn a_shadowed_rowid_column_is_not_treated_as_a_unique_record_id() {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch("CREATE TABLE LocalizeDBSchema(rowid INTEGER, Bytes BLOB)")
        .unwrap();
    for _ in 0..2 {
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (5,?)",
            [blob(&["攻击"])],
        )
        .unwrap();
    }
    let (rows, total) = database::scan(&con, "Localize", "攻击", 10, &ctl()).unwrap();
    assert_eq!(total, 2);
    assert_ne!(rows[0].1, rows[1].1);
}

#[test]
fn comparison_cache_handles_composite_keys_and_non_text_tables() {
    let original = Connection::open_in_memory().unwrap();
    let current = Connection::open_in_memory().unwrap();
    for con in [&original, &current] {
        con.execute_batch("CREATE TABLE LocalizeDBSchema(Id INTEGER, Lang TEXT, Bytes BLOB, PRIMARY KEY(Id,Lang)) WITHOUT ROWID; CREATE TABLE LocalizeMetadata(Value TEXT)").unwrap();
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (1,'cn',?)",
            [blob(&["攻击"])],
        )
        .unwrap();
        con.execute(
            "INSERT INTO LocalizeDBSchema VALUES (1,'jp',?)",
            [blob(&["攻击"])],
        )
        .unwrap();
    }
    current
        .execute(
            "UPDATE LocalizeDBSchema SET Bytes=? WHERE Lang='cn'",
            [blob(&["当前文字"])],
        )
        .unwrap();
    let cached = database::current_overrides(&original, &current, "Localize", 1024 * 1024, &ctl())
        .unwrap()
        .unwrap();
    let rules = Rules::new(vec![rule("攻击", "爆炸", "sub")]).unwrap();
    let streamed = database::patch_database(
        &original,
        Some(&current),
        None,
        &rules,
        &options(),
        false,
        &ctl(),
    )
    .unwrap();
    let cached = database::patch_database(
        &original,
        None,
        Some(&cached),
        &rules,
        &options(),
        false,
        &ctl(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(streamed).unwrap(),
        serde_json::to_value(cached).unwrap()
    );
}

#[test]
fn scanning_bad_records_emits_a_location_and_the_actual_parse_reason() {
    let con = simple_db();
    let mut bad = blob(&["攻击"]);
    bad[4..6].copy_from_slice(&5u16.to_le_bytes());
    con.execute("INSERT INTO LocalizeDBSchema VALUES (12,?)", [bad])
        .unwrap();
    let logs = std::sync::Mutex::new(Vec::new());
    let callback = |message: Option<&str>| {
        if let Some(message) = message {
            logs.lock().unwrap().push(message.to_owned());
        }
        true
    };
    let (_, total) =
        database::scan(&con, "Localize", "攻击", 10, &Control::new(&callback)).unwrap();
    assert_eq!(total, 0);
    let logs = logs.lock().unwrap().join("\n");
    for detail in ["LocalizeDBSchema", "记录 12", "bad vtable length"] {
        assert!(logs.contains(detail), "{logs}");
    }
}
