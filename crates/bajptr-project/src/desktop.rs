//! Desktop's existing action contract; all project mutation stays on its worker.
use crate::{
    Control, Result, baseline, invalid, migration,
    model::{self, Edit},
    ordering::{Sort, Timeline},
    promotion,
    service::{self, Config, Engine, PatchJob},
    sources::{self, Inventory, Notice, Sources},
    storage::{self, Store},
    templates::{self, Templates},
};
use bajptr_core::{
    database::TextRow,
    rules::{Rule, Rules},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};
fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
pub fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid(format!("缺少文字参数：{key}")))
}
fn index(value: &Value, key: &str) -> Result<usize> {
    value[key]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| invalid("记录位置无效，请重新打开。"))
}
#[derive(Clone)]
enum Document {
    Rules(Vec<Rule>),
    Edits(Vec<Edit>),
    Replacements(Vec<Rule>, Vec<Edit>),
}
pub struct Project {
    pub data: PathBuf,
    pub store: Store,
    pub engine: Engine,
    pub rules: Vec<Rule>,
    pub edits: Vec<Edit>,
    pub game: String,
    sort: Sort,
    times: Timeline,
    pub revision: u64,
    errors: Value,
    messages: Vec<String>,
    settings_warning: Option<String>,
    undo: Vec<Document>,
    redo: Vec<Document>,
    pub search: Value,
    searches: VecDeque<Value>,
    search_display: RefCell<Option<(u64, Value)>>,
    preview: Value,
    text_draft: Value,
    batch_draft: Value,
    combined_draft: Value,
    promotion_draft: Value,
    promotion_review: Value,
    backup_draft: Value,
    migration_draft: Value,
    pub version: Option<String>,
    tables: Value,
    pub application: String,
    templates: Templates,
    source_history: Sources,
    source_current: Option<Vec<String>>,
}
impl Project {
    pub fn new(data: PathBuf) -> Self {
        let root = data.join(".gui");
        Self {
            data,
            store: Store::new(root.clone()),
            engine: Engine::default(),
            rules: vec![],
            edits: vec![],
            game: String::new(),
            sort: Sort::default(),
            times: Timeline::default(),
            revision: 0,
            errors: json!({}),
            messages: vec![],
            settings_warning: None,
            undo: vec![],
            redo: vec![],
            search: Value::Null,
            searches: VecDeque::new(),
            search_display: RefCell::new(None),
            preview: Value::Null,
            text_draft: Value::Null,
            batch_draft: Value::Null,
            combined_draft: Value::Null,
            promotion_draft: Value::Null,
            promotion_review: Value::Null,
            backup_draft: Value::Null,
            migration_draft: Value::Null,
            version: None,
            tables: json!([]),
            application: "应用状态待核对，先预览".into(),
            templates: Templates::new(root.join("template-drafts.json")),
            source_history: Sources::default(),
            source_current: None,
        }
    }
    fn config(&self) -> Config {
        let mut c = Config::new(self.rules.clone(), self.edits.clone());
        c.rules_path = Some(self.store.root.join("rules.tsv"));
        c.edits_path = Some(self.store.root.join("text-edits.json"));
        c
    }
    fn load(&mut self, control: &Control<'_>) -> Result<Value> {
        control.check()?;
        storage::validate_data_dir(&self.data)?;
        storage::recover_sources(&self.store.root)?;
        let settings = self.store.root.join("settings.json");
        if settings.exists() {
            match fs::read(&settings)
                .map_err(crate::Error::Io)
                .and_then(|b| crate::backup::settings(&b))
            {
                Ok(v) => {
                    self.game = v["game"].as_str().unwrap_or("").into();
                    self.sort = serde_json::from_value(
                        v.get("replacement_sort").cloned().unwrap_or(json!("added")),
                    )?;
                    self.times = serde_json::from_value(
                        v.get("replacement_times").cloned().unwrap_or(json!({})),
                    )?;
                }
                Err(error) => {
                    let warning = format!("设置读取失败，使用默认设置：{}", error);
                    self.messages.push(warning.clone());
                    self.settings_warning = Some(warning);
                }
            }
        }
        control.check()?;
        match self.store.load("rules.tsv", model::parse_rules, vec![]) {
            Ok(v) => self.rules = v,
            Err(e) => self.errors["rules.tsv"] = json!(e.to_string()),
        }
        control.check()?;
        match self
            .store
            .load("text-edits.json", model::parse_edits, vec![])
        {
            Ok(v) => self.edits = v,
            Err(e) => self.errors["text-edits.json"] = json!(e.to_string()),
        }
        control.check()?;
        match self
            .store
            .load("source-history.json", Sources::parse, Sources::default())
        {
            Ok(value) => self.source_history = value,
            Err(e) => self.errors["source-history.json"] = json!(e.to_string()),
        }
        control.check()?;
        if let Err(error) =
            self.store
                .load("application-state.json", service::parse_receipts, json!({}))
        {
            self.errors["application-state.json"] = json!(error.to_string());
        }
        self.times.observe(&self.rules, &self.edits, 0)?;
        self.state()
    }
    fn require(&self, revision: Option<&Value>) -> Result<()> {
        if storage::pending_transaction(&self.store.root) {
            return Err(invalid(
                "记录保存尚未完成，请重新打开工具恢复后再修改或替换。",
            ));
        }
        if self.errors.as_object().is_some_and(|o| !o.is_empty()) {
            return Err(invalid(
                "记录文件未能完整读取，已停止保存和替换。原文件已保留，请修复后重新打开工具。",
            ));
        }
        if revision
            .filter(|v| !v.is_null())
            .is_some_and(|v| v.as_u64() != Some(self.revision))
        {
            return Err(invalid("修改记录已变化，请重新打开编辑窗口或重新预览。"));
        }
        Ok(())
    }
    fn changed(&mut self) {
        let times = (|| -> Result<()> {
            if self.times.observe(
                &self.rules,
                &self.edits,
                chrono::Utc::now().timestamp_millis().max(1) as u64,
            )? {
                self.save_view_settings(None)?;
            }
            Ok(())
        })();
        if let Err(error) = times {
            let warning = format!("修改已保存，但排序时间未能保存：{error}");
            if !self.messages.contains(&warning) {
                self.messages.push(warning);
            }
        }
        self.migration_draft = Value::Null;
        self.engine.invalidate_prepared();
        self.revision += 1;
        self.preview = Value::Null;
        self.batch_draft = Value::Null;
        self.combined_draft = Value::Null;
        self.promotion_draft = Value::Null;
        self.promotion_review = Value::Null;
        self.backup_draft = Value::Null;
        self.application = "词表已保存，尚未应用".into();
    }
    fn save_view_settings(&self, sort: Option<Sort>) -> Result<()> {
        let path = self.store.root.join("settings.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let expected = bytes.as_deref().map(storage::digest);
        let mut settings = match bytes {
            Some(bytes) => crate::backup::settings(&bytes)?,
            None => json!({"format":2,"game":self.game}),
        };
        if settings["game"].as_str().unwrap_or("") != self.game {
            return Err(invalid("游戏目录已被其他窗口修改，请重新打开工具。"));
        }
        if let Some(sort) = sort {
            settings["replacement_sort"] = json!(sort);
        }
        settings["replacement_times"] = json!(self.times);
        storage::atomic_write(&path, &serde_json::to_vec(&settings)?, Some(&expected))?;
        Ok(())
    }
    fn commit(&mut self, document: Document, record: bool) -> Result<()> {
        self.commit_controlled(document, record, &Control::new(&|_| true))
    }
    fn commit_controlled(
        &mut self,
        document: Document,
        record: bool,
        control: &Control<'_>,
    ) -> Result<()> {
        self.require(None)?;
        let previous = match document {
            Document::Replacements(rules, edits) => {
                let rules = model::normalize(rules)?;
                let rule_bytes = model::rules_bytes(&rules)?;
                let edit_bytes = model::edits_bytes(&edits)?;
                if rules == self.rules && edits == self.edits {
                    return Ok(());
                }
                control.check()?;
                self.store.save_replacements(&rule_bytes, &edit_bytes)?;
                Document::Replacements(
                    std::mem::replace(&mut self.rules, rules),
                    std::mem::replace(&mut self.edits, edits),
                )
            }
            Document::Rules(values) => {
                let values = if let Some(names) = &self.source_current {
                    sources::freeze(&values, &names.iter().cloned().collect())?
                } else {
                    model::normalize(values)?
                };
                let bytes = model::rules_bytes(&values)?;
                control.check()?;
                self.store.save("rules.tsv", &bytes)?;
                if values == self.rules {
                    return Ok(());
                }
                Document::Rules(std::mem::replace(&mut self.rules, values))
            }
            Document::Edits(values) => {
                let bytes = model::edits_bytes(&values)?;
                control.check()?;
                self.store.save("text-edits.json", &bytes)?;
                if values == self.edits {
                    return Ok(());
                }
                Document::Edits(std::mem::replace(&mut self.edits, values))
            }
        };
        if record {
            self.undo.push(previous);
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.changed();
        Ok(())
    }
    fn current_version(&mut self, control: &Control<'_>) -> Result<String> {
        let baseline = self
            .engine
            .baseline(&self.game, &self.data.join("backup"), control)?;
        self.version = Some(baseline.version.clone());
        Ok(baseline.version)
    }
    fn source_notices(&self) -> Result<Vec<Value>> {
        let key = model::game_key(&self.game)?;
        let Some(history) = self.source_history.games.get(&key) else {
            return Ok(vec![]);
        };
        let mut output = Vec::new();
        for notice in &history.notices {
            for (index, rule) in self.rules.iter().enumerate().filter(|(_, r)| {
                (r.old.as_str(), r.new.as_str(), r.mode.as_str())
                    == (
                        notice.old.as_str(),
                        notice.new.as_str(),
                        notice.mode.as_str(),
                    )
            }) {
                if model::scopes(&rule.table)?.is_some_and(|names| names.contains(&notice.table)) {
                    continue;
                }
                output.push(json!({"index":index,"old":notice.old,"new":notice.new,"mode":notice.mode,"table":notice.table,"label":model::title(&notice.table),"fields":notice.fields,"example":notice.example}));
            }
        }
        Ok(output)
    }
    fn synchronize_sources(&mut self, control: &Control<'_>) -> Result<Value> {
        self.require(None)?;
        let version = self.current_version(control)?;
        let key = model::game_key(&self.game)?;
        let metadata = self.store.root.join("tables-cache.json");
        let mut previous = self.source_history.games.get(&key).cloned();
        if previous.is_none() {
            let live = crate::region::locate(&self.game)?;
            let folder = baseline::version_folder(&live, &self.data.join("backup"))?;
            let prefix =
                storage::identity(folder.parent().unwrap())? + std::path::MAIN_SEPARATOR_STR;
            if let Some(cache) = fs::read(&metadata)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                && cache["format"] == 2
                && cache["identity"][0].as_str().is_some_and(|s| {
                    storage::identity(Path::new(s)).is_ok_and(|path| path.starts_with(&prefix))
                })
                && let Some(names) = cache["tables"].as_array().filter(|names| {
                    names
                        .iter()
                        .all(|n| n.as_str().is_some_and(|n| !n.is_empty()))
                })
            {
                previous = Some(Inventory {
                    known: names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    notices: vec![],
                });
            }
        }
        // Do not overwrite the previous cache until migration and notices are durable.
        let names: Vec<String> = serde_json::from_value(self.engine.observed_tables(
            &self.game,
            &self.data.join("backup"),
            &metadata,
            control,
        )?)?;
        let current: BTreeSet<_> = names.iter().cloned().collect();
        if previous.is_none() && self.rules.iter().any(|r| r.table.is_empty()) {
            self.source_current = None;
            return Ok(
                json!({"tables":names.iter().map(|n|model::table_info(n)).collect::<Vec<_>>(),"state":self.state()?,"missing_history":true}),
            );
        }
        let mut history = previous.clone().unwrap_or_else(|| Inventory {
            known: current.clone(),
            notices: vec![],
        });
        let values = sources::freeze(&self.rules, &history.known)?;
        let added: Vec<_> = current.difference(&history.known).cloned().collect();
        if !added.is_empty() && !values.is_empty() {
            let matches = self.engine.source_rule_matches(
                &self.game,
                &self.data.join("backup"),
                &added,
                &values,
                control,
            )?;
            for found in matches {
                let rule = &values[found.rule_index];
                history.notices.push(Notice {
                    table: found.table,
                    old: rule.old.clone(),
                    new: rule.new.clone(),
                    mode: rule.mode.clone(),
                    fields: found.fields,
                    example: found.example,
                });
            }
        }
        history.known.extend(current);
        let mut next = self.source_history.clone();
        next.games.insert(key, history);
        control.check()?;
        let migrated = self.rules.iter().filter(|r| r.table.is_empty()).count();
        if self.current_version(control)? != version {
            return Err(invalid("文本表核对期间资源版本发生变化，请重新检查底稿。"));
        }
        control.check()?;
        let commit_control = control.uncancellable();
        if values != self.rules {
            self.store.check("rules.tsv")?;
            let bytes = fs::read(self.store.root.join("rules.tsv"))?;
            let recovery = self.store.root.join(format!(
                "rules-before-fixed-sources-{}.tsv",
                &storage::digest(&bytes)[..16]
            ));
            if !recovery.exists() {
                storage::atomic_write(&recovery, &bytes, Some(&None))?;
            }
            self.store
                .save_sources(&model::rules_bytes(&values)?, &serde_json::to_vec(&next)?)?;
            self.rules = values;
            // Migration is a compatibility conversion, not an undoable rule edit.
            self.undo.clear();
            self.redo.clear();
            self.changed();
        } else {
            self.store
                .save("source-history.json", &serde_json::to_vec(&next)?)?;
        }
        self.source_history = next;
        self.source_current = Some(names.clone());
        self.tables = json!(names);
        // Cache refresh is optional. Source history above is the authoritative inventory.
        let _ = self.engine.tables_cached(
            &self.game,
            &self.data.join("backup"),
            Some(&metadata),
            &commit_control,
        );
        let _ = control.log("文本表已就绪。");
        Ok(
            json!({"tables":names.iter().map(|n|model::table_info(n)).collect::<Vec<_>>(),"state":self.state()?,"migrated":migrated,"missing_history":false}),
        )
    }
    fn context(&mut self, version: &str, revision: &Value, control: &Control<'_>) -> Result<()> {
        self.require(Some(revision))?;
        if self.current_version(control)? != version {
            return Err(invalid("游戏资源版本已变化，请重新查找对应文本。"));
        }
        Ok(())
    }
    fn search_state(&self) -> Result<Value> {
        if self.search.is_null() || self.search["game"] != model::game_key(&self.game)? {
            return Ok(Value::Null);
        }
        let mut cache = self.search_display.borrow_mut();
        if cache.as_ref().is_some_and(|(revision, value)| {
            *revision != self.revision || value["nonce"] != self.search["nonce"]
        }) {
            *cache = None;
        }
        let mut rendered = cache
            .as_ref()
            .map(|(_, value)| value["rows"].as_array().cloned().unwrap_or_default())
            .unwrap_or_default();
        let rules = Rules::new(self.rules.clone())?;
        let key = model::game_key(&self.game)?;
        let version = text(&self.search, "version")?;
        let rows: Vec<TextRow> = self.search["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .skip(rendered.len())
            .map(|row| Ok(serde_json::from_value(row.clone())?))
            .collect::<Result<_>>()?;
        let rows=rows.into_iter().map(|r|{let (value,explicit)=model::effective(&r,&self.edits,&key,version,&rules)?;Ok(json!({"table":r.0,"label":model::title(&r.0),"row_id":r.1,"slot":r.2,"original":r.3,"state":if explicit{"定点修改"}else if value!=r.3{"词表替换"}else{"原文"},"text":value}))}).collect::<Result<Vec<_>>>()?;
        rendered.extend(rows);
        let source_tables: Vec<_> = self.search["matching_tables"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(model::table_info)
            .collect();
        let value = json!({"rows":rendered,"source_tables":source_tables,"keyword":self.search["keyword"],"source":self.search["source"],"total":self.search["total"],"version":version,"nonce":self.search["nonce"]});
        *cache = Some((self.revision, value.clone()));
        Ok(value)
    }
    fn batch_entries(&self, id: &str) -> Result<Vec<Edit>> {
        let game = model::game_key(&self.game)?;
        let entries: Vec<_> = self
            .edits
            .iter()
            .filter(|e| {
                e.game == game
                    && e.batch_operation
                        .as_ref()
                        .is_some_and(|operation| operation.id == id)
            })
            .cloned()
            .collect();
        if entries.is_empty() {
            return Err(invalid("这项批量修改已变化，请重新打开。"));
        }
        Ok(entries)
    }
    pub fn state(&self) -> Result<Value> {
        let rules=self.rules.iter().enumerate().map(|(index,r)|{let stamp=self.times.rules.get(index).cloned().unwrap_or_default();let (old,new,draft)=if r.mode=="re"{self.templates.display(&r.old,&r.new)}else{(r.old.clone(),r.new.clone(),None)};json!({"rule":r,"scope":if r.table.is_empty(){"待选择文本表".into()}else{model::scope_title(&r.table)},"display_old":old,"display_new":new,"draft":draft,"added":stamp.added,"modified":stamp.modified})}).collect::<Vec<_>>();
        let key = model::game_key(&self.game)?;
        let edits=self.edits.iter().enumerate().filter(|(_,e)|e.game==key).map(|(i,e)|json!({"index":i,"edit":e,"label":model::title(&e.table),"active":self.version.as_ref().is_none_or(|v|v==&e.version)})).collect::<Vec<_>>();
        let mut batches: Vec<Value> = Vec::new();
        let mut positions = std::collections::HashMap::new();
        for (index, edit) in self
            .edits
            .iter()
            .enumerate()
            .filter(|(_, e)| e.game == key && e.batch)
        {
            let stamp = self
                .times
                .batches
                .get(&crate::ordering::batch_key(edit))
                .cloned()
                .unwrap_or_default();
            let active = self.version.as_ref().is_none_or(|v| v == &edit.version);
            if let Some(operation) = &edit.batch_operation {
                if let Some(&position) = positions.get(&operation.id) {
                    let row: &mut Value = &mut batches[position];
                    row["count"] = json!(row["count"].as_u64().unwrap_or(0) + 1);
                    row["active"] = json!(row["active"] == true && active);
                } else {
                    positions.insert(operation.id.clone(), batches.len());
                    batches.push(json!({"id":operation.id,"old":operation.old,"new":operation.new,"count":1,"active":active,"version":edit.version,"added":stamp.added,"modified":stamp.modified}));
                }
            } else {
                batches.push(json!({"index":index,"old":edit.original,"new":edit.replacement,"count":1,"active":active,"label":model::title(&edit.table),"added":stamp.added,"modified":stamp.modified}));
            }
        }
        Ok(
            json!({"game":self.game,"version":self.version,"replacement_sort":self.sort,"revision":self.revision,"rules":rules,"edits":edits,"batches":batches,"errors":self.errors,"messages":self.messages,"settings_warning":self.settings_warning,"can_undo":!self.undo.is_empty(),"can_redo":!self.redo.is_empty(),"templates":templates::metadata(),"search":self.search_state()?,"application":self.application,"source_notices":self.source_notices()?}),
        )
    }
    // Only read-operation state is rolled back by cancellation. Persisted edits are never rolled back.
    pub fn snapshot(&self) -> Value {
        json!({"search":self.search,"preview":self.preview,"tables":self.tables,"version":self.version,"application":self.application})
    }
    pub fn restore_snapshot(&mut self, v: &Value) {
        self.search_display.get_mut().take();
        self.search = v["search"].clone();
        self.preview = v["preview"].clone();
        self.tables = v["tables"].clone();
        self.version = v["version"].as_str().map(str::to_string);
        self.application = v["application"].as_str().unwrap_or("").into();
    }
    fn find_search(&self, p: &Value) -> Result<&Value> {
        let nonce = text(p, "search_nonce")?;
        let game = model::game_key(&self.game)?;
        let matches = |search: &Value| search["nonce"] == nonce && search["game"] == game;
        if matches(&self.search) {
            return Ok(&self.search);
        }
        self.searches
            .iter()
            .rev()
            .find(|search| matches(search))
            .ok_or_else(|| invalid("请先重新查找文本。"))
    }
    pub fn dispatch(&mut self, action: &str, p: &Value, control: &Control<'_>) -> Result<Value> {
        if matches!(
            action,
            "tables"
                | "scope_sources"
                | "sources"
                | "search"
                | "search_more"
                | "text_open"
                | "text_save"
                | "batch_preview"
                | "batch_open"
                | "combined_open"
                | "combined_save"
                | "combined_remove"
                | "promotion_preview"
                | "promotion_start"
                | "promotion_mark_reviewed"
                | "promotion_commit"
                | "promotion_missing_preview"
                | "batch_save"
                | "preview"
                | "prepare_patch"
                | "apply"
                | "baseline"
                | "migration_find"
                | "migration_preview"
                | "migration_start"
                | "migration_commit"
                | "restore"
                | "export_text"
        ) {
            let status = self.engine.baseline_status_for_action(
                &self.game,
                &self.data.join("backup"),
                &self.data,
                matches!(action, "search" | "preview"),
                control,
            )?;
            if status["state"] != "ready" {
                return Err(invalid(
                    "初始底稿尚未就绪或游戏资源已更新，请先检查游戏目录并确认底稿。",
                ));
            }
        }
        match action {
            "backup_create" => {
                self.require(p.get("revision"))?;
                let snapshot = crate::backup::capture(&self.store.root, control)?;
                for name in ["rules.tsv", "text-edits.json", "source-history.json"] {
                    self.store.check(name)?;
                }
                let bundle = snapshot.bundle()?;
                let decoded = bundle.decode()?;
                if decoded.settings["game"].as_str().unwrap_or("") != self.game {
                    return Err(invalid("设置已被其他窗口修改，请重新打开工具后备份。"));
                }
                bundle.export(Path::new(text(p, "path")?), &self.data, control)?;
                Ok(json!({"message":"备份已保存，包含词表、指定文本修改、游戏目录和选项。"}))
            }
            "backup_read" => {
                if storage::pending_transaction(&self.store.root) {
                    return Err(invalid(
                        "记录保存尚未完成，请重新打开工具恢复后再恢复备份。",
                    ));
                }
                if p.get("revision")
                    .is_some_and(|value| value.as_u64() != Some(self.revision))
                {
                    return Err(invalid("当前记录已变化，请重新选择备份。"));
                }
                self.backup_draft = Value::Null;
                control.check()?;
                let bundle = crate::backup::Bundle::read(Path::new(text(p, "path")?))?;
                let decoded = bundle.decode()?;
                let snapshot = crate::backup::capture(&self.store.root, control)?;
                let nonce = nonce();
                self.backup_draft = json!({"nonce":nonce,"revision":self.revision,"bundle":bundle,"expected":snapshot.revisions()});
                Ok(
                    json!({"nonce":nonce,"rule_count":decoded.rules.len(),"edit_count":decoded.edits.len(),"game":decoded.settings["game"].as_str().unwrap_or("")}),
                )
            }
            "backup_restore" => {
                let draft = self.backup_draft.clone();
                if storage::pending_transaction(&self.store.root)
                    || draft.is_null()
                    || draft["nonce"] != p["nonce"]
                    || draft["revision"] != self.revision
                {
                    return Err(invalid("恢复备份的上下文已失效，请重新选择备份。"));
                }
                let bundle: crate::backup::Bundle =
                    serde_json::from_value(draft["bundle"].clone())?;
                let decoded = bundle.decode()?;
                let mut documents = bundle.documents();
                documents.insert(
                    "settings.json".into(),
                    serde_json::to_vec(&decoded.settings)?,
                );
                let templates = Templates::from_bytes(
                    self.store.root.join("template-drafts.json"),
                    &documents["template-drafts.json"],
                )?;
                let expected = serde_json::from_value(draft["expected"].clone())?;
                let current = crate::backup::capture(&self.store.root, control)?;
                if current.revisions() != expected {
                    return Err(invalid("当前数据在选择备份后发生变化，请重新选择备份。"));
                }
                control.check()?;
                let commit = control.uncancellable();
                let recovery = current.archive(&self.store.root, &commit)?;
                self.store.restore_workspace(&documents, &expected)?;
                self.engine.close();
                self.rules = decoded.rules;
                self.edits = decoded.edits;
                self.game = decoded.settings["game"].as_str().unwrap_or("").into();
                self.sort = serde_json::from_value(
                    decoded
                        .settings
                        .get("replacement_sort")
                        .cloned()
                        .unwrap_or(json!("added")),
                )?;
                self.times = serde_json::from_value(
                    decoded
                        .settings
                        .get("replacement_times")
                        .cloned()
                        .unwrap_or(json!({})),
                )?;
                self.times.observe(&self.rules, &self.edits, 0)?;
                self.source_history = decoded.sources;
                self.templates = templates;
                self.settings_warning = None;
                for name in crate::backup::FILES {
                    self.errors.as_object_mut().unwrap().remove(name);
                }
                self.undo.clear();
                self.redo.clear();
                self.search = Value::Null;
                self.searches.clear();
                self.text_draft = Value::Null;
                self.tables = json!([]);
                self.version = None;
                self.source_current = None;
                self.changed();
                self.application = "备份已恢复，请检查游戏目录并预览。".into();
                Ok(json!({"state":self.state()?,"recovery":recovery}))
            }
            "promotion_start" => {
                let plan = self.dispatch("promotion_preview", p, control)?;
                let mut review = self.promotion_draft["review"].take();
                let retained = plan["items"].as_array().unwrap().len() as u64
                    - plan["count"].as_u64().unwrap();
                let mut result = if plan["count"] == 0 && plan["relocated"] == 0 {
                    json!({"state":self.state()?,"count":0,"relocated":0,"backup":null})
                } else {
                    self.dispatch("promotion_commit", &json!({"nonce":plan["nonce"]}), control)?
                };
                result["retained"] = json!(retained);
                result["relocations"] = plan["relocations"].clone();
                let mut items = plan["items"].as_array().cloned().unwrap_or_default();
                for (index, item) in items.iter_mut().enumerate() {
                    item["review_item"] = json!(index);
                }
                items.retain(|item| {
                    item["quiet"] != true
                        && item["reason"]
                            .as_str()
                            .is_some_and(|reason| !reason.is_empty())
                });
                review["revision"] = json!(self.revision);
                result["review_nonce"] = review["nonce"].clone();
                result["retained_items"] = json!(items);
                self.promotion_review = review;
                Ok(result)
            }
            "promotion_preview" => {
                self.require(p.get("revision"))?;
                self.promotion_draft = Value::Null;
                let status = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                let version = self.current_version(control)?;
                let plan =
                    self.engine
                        .promotion_plan(&self.game, &self.data, &self.config(), control)?;
                let current = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                if current["state"] != "ready" || current["token"] != status["token"] {
                    return Err(invalid("底稿或游戏资源在核对期间变化，请重新整理。"));
                }
                let count = plan
                    .items
                    .iter()
                    .filter(|item| item.reason.is_empty())
                    .count();
                let nonce = nonce();
                let review_items: Vec<_> = plan.items.iter().map(|item| {
                    json!({"rows":item.missing_rows,"old":item.old,"new":item.new,"label":item.label})
                }).collect();
                let game = model::game_key(&self.game)?;
                self.promotion_draft = json!({"nonce":nonce,"revision":self.revision,"version":version,"game":game,"token":status["token"],"rules":plan.rules,"edits":plan.edits,"count":count,"relocated":plan.relocated,
                    "review":{"nonce":nonce,"game":game,"version":version,"token":status["token"],"items":review_items}});
                Ok(
                    json!({"nonce":nonce,"items":plan.items,"count":count,"relocated":plan.relocated,"relocations":plan.relocations}),
                )
            }
            "promotion_mark_reviewed" => {
                self.require(p.get("revision"))?;
                let draft = &self.promotion_draft;
                if !p["nonce"].is_string()
                    || draft["nonce"] != p["nonce"]
                    || draft["version"] != p["version"]
                    || draft["revision"] != self.revision
                    || draft["game"] != model::game_key(&self.game)?
                {
                    return Err(invalid("整理结果已失效，请重新核对。"));
                }
                let _rules = storage::Lock::acquire(&self.store.root.join("rules.tsv"))?;
                let _edits = storage::Lock::acquire(&self.store.root.join("text-edits.json"))?;
                self.store.check("rules.tsv")?;
                self.store.check("text-edits.json")?;
                let status = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                if status["state"] != "ready" || status["token"] != draft["token"] {
                    return Err(invalid("底稿或游戏资源已变化，请重新整理。"));
                }
                let path = promotion::review_path(
                    &self.data,
                    text(draft, "game")?,
                    text(draft, "version")?,
                );
                let _lock = storage::Lock::acquire(&path)?;
                let mut reviewed = promotion::read_review(&path)?;
                let before = reviewed.len();
                for item in draft["review"]["items"].as_array().unwrap() {
                    let old = text(item, "old")?;
                    let new = text(item, "new")?;
                    for row in serde_json::from_value::<Vec<TextRow>>(item["rows"].clone())? {
                        control.check()?;
                        reviewed.insert(promotion::review_key(old, new, &row));
                    }
                }
                let count = reviewed.len() - before;
                if count != 0 {
                    storage::write_locked(&path, &serde_json::to_vec_pretty(&reviewed)?)?;
                }
                self.promotion_draft = Value::Null;
                self.promotion_review = Value::Null;
                Ok(json!({"reviewed":count}))
            }
            "promotion_commit" => {
                let draft = self.promotion_draft.clone();
                if draft.is_null()
                    || draft["nonce"] != p["nonce"]
                    || (draft["count"] == 0 && draft["relocated"] == 0)
                    || draft["game"] != model::game_key(&self.game)?
                {
                    return Err(invalid("整理结果已失效，请重新核对。"));
                }
                self.context(text(&draft, "version")?, &draft["revision"], control)?;
                let status = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                if status["state"] != "ready" || status["token"] != draft["token"] {
                    return Err(invalid("底稿或游戏资源已变化，请重新整理。"));
                }
                let rules = serde_json::from_value(draft["rules"].clone())?;
                let edits = serde_json::from_value(draft["edits"].clone())?;
                let mut originals = Vec::new();
                for name in ["rules.tsv", "text-edits.json"] {
                    self.store.check(name)?;
                    let bytes = match fs::read(self.store.root.join(name)) {
                        Ok(bytes) => Some(bytes),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    originals.push((name, bytes));
                }
                control.check()?;
                let commit = control.uncancellable();
                let backup = self
                    .store
                    .root
                    .join("recovery")
                    .join(format!("batch-promotion-{}", nonce()));
                fs::create_dir_all(&backup)?;
                for (name, bytes) in &originals {
                    if let Some(bytes) = bytes {
                        storage::atomic_write(&backup.join(name), bytes, Some(&None))?;
                    }
                }
                storage::atomic_write(
                    &backup.join("info.json"),
                    &serde_json::to_vec_pretty(
                        &json!({"format":1,"game":draft["game"],"version":draft["version"],"files":originals.iter().map(|(name,bytes)|json!({"name":name,"existed":bytes.is_some()})).collect::<Vec<_>>()}),
                    )?,
                    Some(&None),
                )?;
                self.commit_controlled(Document::Replacements(rules, edits), true, &commit)?;
                self.promotion_draft = Value::Null;
                Ok(
                    json!({"state":self.state()?,"count":draft["count"],"relocated":draft["relocated"],"backup":backup}),
                )
            }
            "combined_open" => {
                self.require(p.get("revision"))?;
                let old = text(p, "old")?;
                let new = text(p, "new")?;
                let version = self.current_version(control)?;
                let game = model::game_key(&self.game)?;
                let rules = self
                    .rules
                    .iter()
                    .filter(|rule| crate::combined::rule_matches(rule, old, new))
                    .collect::<Vec<_>>();
                let tables = rules
                    .iter()
                    .map(|rule| model::scopes(&rule.table))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .flatten()
                    .collect::<BTreeSet<_>>();
                let members=self.edits.iter().enumerate().filter(|(_,edit)|crate::combined::batch_matches(edit,&game,&version,old,new)).map(|(index,edit)|json!({"index":index,"edit":edit,"label":model::title(&edit.table)})).collect::<Vec<_>>();
                if rules.is_empty() || members.is_empty() {
                    return Err(invalid("合并替换已变化，请重新打开。"));
                }
                let nonce = nonce();
                self.combined_draft = json!({"nonce":nonce,"old":old,"new":new,"version":version,"revision":self.revision});
                Ok(json!({"nonce":nonce,"old":old,"new":new,"tables":tables,"members":members}))
            }
            "combined_save" => {
                let draft = self.combined_draft.clone();
                if draft.is_null() || p["nonce"] != draft["nonce"] {
                    return Err(invalid("合并编辑上下文已失效，请重新打开。"));
                }
                let version = text(&draft, "version")?;
                self.context(version, &draft["revision"], control)?;
                let tables: BTreeSet<String> = serde_json::from_value(p["tables"].clone())?;
                let available: Vec<String> = serde_json::from_value(self.engine.tables(
                    &self.game,
                    &self.data.join("backup"),
                    control,
                )?)?;
                if tables.iter().any(|table| !available.contains(table)) {
                    return Err(invalid("整表范围包含不存在的文本表，请重新选择。"));
                }
                let removed = serde_json::from_value(p["removed"].clone())?;
                let (rules, edits) = crate::combined::update(
                    &self.rules,
                    &self.edits,
                    crate::combined::Change {
                        game: &model::game_key(&self.game)?,
                        version,
                        old: text(&draft, "old")?,
                        new: text(&draft, "new")?,
                        next_old: text(p, "old")?,
                        next_new: text(p, "new")?,
                        tables,
                        removed,
                    },
                )?;
                self.commit_controlled(Document::Replacements(rules, edits), true, control)?;
                self.combined_draft = Value::Null;
                self.state()
            }
            "combined_remove" => {
                self.require(p.get("revision"))?;
                let version = self.current_version(control)?;
                let game = model::game_key(&self.game)?;
                let old = text(p, "old")?;
                let new = text(p, "new")?;
                let rules = self
                    .rules
                    .iter()
                    .filter(|rule| !crate::combined::rule_matches(rule, old, new))
                    .cloned()
                    .collect::<Vec<_>>();
                let edits = self
                    .edits
                    .iter()
                    .filter(|edit| !crate::combined::batch_matches(edit, &game, &version, old, new))
                    .cloned()
                    .collect::<Vec<_>>();
                if rules.len() == self.rules.len() || edits.len() == self.edits.len() {
                    return Err(invalid("合并替换已变化，请重新选择。"));
                }
                self.commit_controlled(Document::Replacements(rules, edits), true, control)?;
                self.state()
            }
            "baseline_rebuild_preview" => {
                self.engine
                    .rebuild_baseline(&self.game, &self.data.join("backup"), None, control)
            }
            "baseline_rebuild_commit" => {
                let token = p
                    .get("token")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| invalid("请先尝试重建并确认。"))?;
                let result = self.engine.rebuild_baseline(
                    &self.game,
                    &self.data.join("backup"),
                    Some(token),
                    control,
                )?;
                self.version = result["version_id"].as_str().map(str::to_string);
                self.search = Value::Null;
                self.searches.clear();
                self.search_display.get_mut().take();
                self.preview = Value::Null;
                self.batch_draft = Value::Null;
                self.text_draft = Value::Null;
                self.migration_draft = Value::Null;
                self.tables = json!([]);
                self.source_current = None;
                Ok(result)
            }
            "reset_record" => {
                if storage::pending_transaction(&self.store.root) {
                    return Err(invalid(
                        "请先重新打开工具恢复未完成的保存，再处理损坏记录。",
                    ));
                }
                if p["revision"].as_u64() != Some(self.revision) {
                    return Err(invalid("修改记录已变化，请重新检查后重置。"));
                }
                let name = text(p, "name")?;
                if self.errors.get(name).is_none() {
                    return Err(invalid("这份记录没有读取错误。"));
                }
                let archived = self.store.archive_failed(name)?;
                match name {
                    "rules.tsv" => self.rules.clear(),
                    "text-edits.json" => self.edits.clear(),
                    "source-history.json" => {
                        self.source_history = Sources::default();
                        self.source_current = None;
                    }
                    _ => unreachable!(),
                }
                self.errors.as_object_mut().unwrap().remove(name);
                self.undo.clear();
                self.redo.clear();
                self.text_draft = Value::Null;
                self.search = Value::Null;
                self.searches.clear();
                self.changed();
                Ok(json!({"state":self.state()?,"archived":archived}))
            }
            "load" => {
                let mut loaded = Self::new(self.data.clone());
                let state = loaded.load(control)?;
                control.check()?;
                *self = loaded;
                Ok(state)
            }
            "sort_settings" => {
                if storage::pending_transaction(&self.store.root)
                    || p["revision"].as_u64() != Some(self.revision)
                {
                    return Err(invalid("记录正在更新，请稍后重新选择排序。"));
                }
                let sort: Sort = serde_json::from_value(p["value"].clone())?;
                self.save_view_settings(Some(sort))?;
                self.sort = sort;
                self.state()
            }
            "settings" => {
                if storage::pending_transaction(&self.store.root) {
                    return Err(invalid(
                        "记录保存尚未完成，请重新打开工具恢复后再保存设置。",
                    ));
                }
                if p["revision"].as_u64() != Some(self.revision) {
                    return Err(invalid("设置已变化，请重试。"));
                }
                let game = text(p, "game")?.trim().to_string();
                storage::atomic_write(
                    &self.store.root.join("settings.json"),
                    &serde_json::to_vec(
                        &json!({"format":2,"game":game,"replacement_sort":self.sort,"replacement_times":self.times}),
                    )?,
                    None,
                )?;
                self.settings_warning = None;
                if self.game != game {
                    self.engine.close();
                    self.version = None;
                    self.source_current = None;
                    self.search = Value::Null;
                    self.searches.clear();
                    self.game = game;
                    self.changed();
                }
                self.state()
            }
            "check" => {
                let status = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                self.version = status["baseline"]["version_id"]
                    .as_str()
                    .map(str::to_string);
                self.application = service::status(&self.game, &self.data, &self.config())?;
                let key = model::game_key(&self.game)?;
                Ok(
                    json!({"connected":status["state"] == "ready","application":self.application,"baseline_status":status,"migration_count":self.edits.iter().filter(|e|e.game==key && self.version.as_ref().is_some_and(|v|v!=&e.version)).count()}),
                )
            }
            "baseline_commit" => {
                let token = p
                    .get("token")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| invalid("请先检查游戏目录并确认底稿。"))?;
                let game = model::game_key(&self.game)?;
                let keep = self
                    .edits
                    .iter()
                    .filter(|e| {
                        e.game == game && (e.anchor.is_none() || e.batch_operation.is_some())
                    })
                    .map(|e| e.version.clone())
                    .collect();
                let original = self.engine.initialize_for_edits(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    Some(token),
                    &keep,
                    control,
                )?;
                self.version = Some(original.version.clone());
                self.search = Value::Null;
                self.searches.clear();
                self.preview = Value::Null;
                self.batch_draft = Value::Null;
                self.text_draft = Value::Null;
                self.migration_draft = Value::Null;
                self.tables = json!([]);
                self.source_current = None;
                self.application = "底稿已保存，请重新预览".into();
                baseline::info(&original)
            }
            "migration_preview" | "migration_start" => {
                self.require(p.get("revision"))?;
                let version = self.current_version(control)?;
                let game = model::game_key(&self.game)?;
                let before = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                let mut source = self.edits.clone();
                self.engine.migration_source_anchors(
                    &self.game,
                    &self.data.join("backup"),
                    &mut source,
                    control,
                )?;
                let existing: Vec<_> = source
                    .iter()
                    .filter(|e| e.game == game && e.version != version)
                    .cloned()
                    .collect();
                let located = self.engine.migration_candidates(
                    &self.game,
                    &self.data.join("backup"),
                    &existing,
                    control,
                )?;
                let status = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                if status["state"] != "ready"
                    || status["token"] != before["token"]
                    || status["token"]["version"] != version
                {
                    return Err(invalid("底稿已变化，请重新检查后核对迁移。"));
                }
                let rules = Rules::new(self.rules.clone())?;
                let items = migration::plan_candidates(
                    &source, &game, &version, &located, &rules, control,
                )?;
                let draft = json!({"nonce":nonce(),"revision":self.revision,"token":status["token"],"game":game,"version":version,"items":items});
                control.check()?;
                self.migration_draft = draft.clone();
                if action == "migration_preview" {
                    return Ok(draft);
                }
                let automatic = migration::automatic_choices(&items);
                let review = if automatic.is_empty() {
                    items
                } else {
                    let projected = migration::apply_choices(
                        &source,
                        &items,
                        &automatic,
                        &game,
                        (&version, None),
                        &rules,
                    )?;
                    migration::plan_candidates(
                        &projected, &game, &version, &located, &rules, control,
                    )?
                };
                // Prepare the follow-up review before entering the uncancellable commit.
                control.check()?;
                let mut backup = Value::Null;
                if !automatic.is_empty() {
                    backup=self.dispatch("migration_commit",&json!({"nonce":draft["nonce"],"revision":self.revision,"choices":automatic}),&control.uncancellable())?["backup"].clone();
                }
                let unavailable = review
                    .iter()
                    .filter(|item| item.candidates.is_empty())
                    .count();
                let draft = json!({"nonce":nonce(),"revision":self.revision,"token":status["token"],"game":game,"version":version,"items":review});
                self.migration_draft = draft.clone();
                Ok(
                    json!({"state":self.state()?,"plan":draft,"automatic_count":automatic.len(),"unavailable_count":unavailable,"backup":backup}),
                )
            }
            "migration_find" => {
                self.require(p.get("revision"))?;
                let draft = &self.migration_draft;
                let game = model::game_key(&self.game)?;
                if draft.is_null()
                    || p["nonce"] != draft["nonce"]
                    || draft["revision"] != self.revision
                    || draft["game"] != game
                {
                    return Err(invalid("迁移核对已失效，请重新打开。"));
                }
                let at = index(p, "item")?;
                let mut item: migration::Item = serde_json::from_value(
                    draft["items"]
                        .get(at)
                        .ok_or_else(|| invalid("迁移条目已变化。"))?
                        .clone(),
                )?;
                let token = draft["token"].clone();
                let version = text(draft, "version")?.to_owned();
                let root = self.data.join("backup");
                let before = self
                    .engine
                    .baseline_status(&self.game, &root, &self.data, control)?;
                if before["state"] != "ready" || before["token"] != token {
                    return Err(invalid("底稿或游戏资源已变化，请重新核对迁移。"));
                }
                let (found, _) =
                    self.engine
                        .scan(&self.game, &root, text(p, "keyword")?, "", 30, 0, control)?;
                let rows: Vec<TextRow> = serde_json::from_value(found["rows"].clone())?;
                let candidates = self
                    .engine
                    .migration_locations(&self.game, &root, &rows, control)?;
                let status = self
                    .engine
                    .baseline_status(&self.game, &root, &self.data, control)?;
                if status["state"] != "ready" || status["token"] != token {
                    return Err(invalid("底稿或游戏资源已变化，请重新核对迁移。"));
                }
                let rules = Rules::new(self.rules.clone())?;
                let layers: Vec<_> = item.indices.iter().map(|&i| &self.edits[i]).collect();
                let mut added = 0;
                let mut occupied = 0;
                for candidate in candidates.into_iter().flatten() {
                    if self.edits.iter().any(|e| {
                        e.game == game
                            && e.version == version
                            && e.table == candidate.row.0
                            && e.row_id == candidate.row.1
                            && e.slot == candidate.row.2
                    }) {
                        occupied += 1;
                        continue;
                    }
                    if item.candidates.len() >= 100
                        || item.candidates.iter().any(|c| c.row == candidate.row)
                    {
                        continue;
                    }
                    item.add_candidate(candidate, &layers, &rules)?;
                    added += 1;
                }
                control.check()?;
                let item = serde_json::to_value(item)?;
                self.migration_draft["items"][at] = item.clone();
                Ok(
                    json!({"nonce":self.migration_draft["nonce"],"item":at,"value":item,"total":found["total"],"added":added,"occupied":occupied}),
                )
            }
            "migration_commit" => {
                self.require(p.get("revision"))?;
                let draft = self.migration_draft.clone();
                if draft.is_null()
                    || p["nonce"] != draft["nonce"]
                    || draft["revision"] != self.revision
                    || draft["game"] != model::game_key(&self.game)?
                {
                    return Err(invalid("迁移核对已失效，请重新打开。"));
                }
                let status = self.engine.baseline_status(
                    &self.game,
                    &self.data.join("backup"),
                    &self.data,
                    control,
                )?;
                if status["state"] != "ready" || status["token"] != draft["token"] {
                    return Err(invalid("底稿或游戏资源已变化，请重新核对迁移。"));
                }
                let items = serde_json::from_value::<Vec<migration::Item>>(draft["items"].clone())?;
                let choices =
                    serde_json::from_value::<Vec<migration::Choice>>(p["choices"].clone())?;
                self.store.check("rules.tsv")?;
                let rules = Rules::new(self.rules.clone())?;
                self.store.check("text-edits.json")?;
                let bytes = fs::read(self.store.root.join("text-edits.json"))?;
                let digest = storage::digest(&bytes);
                let edits = migration::apply_choices(
                    &self.edits,
                    &items,
                    &choices,
                    text(&draft, "game")?,
                    (text(&draft, "version")?, Some(&digest)),
                    &rules,
                )?;
                let backup = self.store.root.join(format!(
                    "text-edits-before-migration-{}.json",
                    &digest[..16]
                ));
                control.check()?;
                let commit = control.uncancellable();
                if backup.exists() {
                    if fs::read(&backup)? != bytes {
                        return Err(invalid("迁移备份与原记录不一致，未保存。"));
                    }
                } else {
                    storage::atomic_write(&backup, &bytes, Some(&None))?;
                }
                self.commit_controlled(Document::Edits(edits), true, &commit)?;
                Ok(json!({"state":self.state()?,"count":choices.len(),"backup":backup}))
            }
            "sources" => self.synchronize_sources(control),
            "scope_sources" => self.engine.scope_sources(
                &self.game,
                &self.data.join("backup"),
                text(p, "old")?,
                text(p, "mode")?,
                control,
            ),
            "sources_ack" => {
                self.require(p.get("revision"))?;
                let key = model::game_key(&self.game)?;
                let mut next = self.source_history.clone();
                if let Some(game) = next.games.get_mut(&key) {
                    game.notices.clear();
                }
                self.store
                    .save("source-history.json", &serde_json::to_vec(&next)?)?;
                self.source_history = next;
                self.state()
            }
            "tables" => {
                let names: Vec<String> = serde_json::from_value(self.engine.tables_cached(
                    &self.game,
                    &self.data.join("backup"),
                    Some(&self.store.root.join("tables-cache.json")),
                    control,
                )?)?;
                self.tables = json!(names);
                Ok(json!(
                    names
                        .iter()
                        .map(|n| model::table_info(n))
                        .collect::<Vec<_>>()
                ))
            }
            "save_rule" => {
                self.require(p.get("revision"))?;
                let rule: Rule = serde_json::from_value(p["rule"].clone())?;
                let mut values = self.rules.clone();
                if p["index"].is_null() {
                    values.push(rule.clone());
                } else {
                    let i = index(p, "index")?;
                    *values
                        .get_mut(i)
                        .ok_or_else(|| invalid("词条位置已变化，请重新打开。"))? = rule.clone();
                }
                self.commit(Document::Rules(values), true)?;
                if p["draft"].is_object()
                    && let Err(e) = self.templates.remember(&rule.old, &rule.new, &p["draft"])
                {
                    control.warn(&format!("词条已保存；模板表单未保存：{e}"))?;
                }
                self.state()
            }
            "remove_rule" => {
                self.require(p.get("revision"))?;
                let mut values = self.rules.clone();
                let i = index(p, "index")?;
                if i >= values.len() {
                    return Err(invalid("词条位置已变化。"));
                }
                values.remove(i);
                self.commit(Document::Rules(values), true)?;
                self.state()
            }
            "history" => {
                self.require(None)?;
                let undo = p["undo"]
                    .as_bool()
                    .ok_or_else(|| invalid("撤销方向无效。"))?;
                let value = if undo {
                    self.undo.last()
                } else {
                    self.redo.last()
                }
                .cloned();
                if let Some(value) = value {
                    let current = match &value {
                        Document::Rules(_) => Document::Rules(self.rules.clone()),
                        Document::Edits(_) => Document::Edits(self.edits.clone()),
                        Document::Replacements(..) => {
                            Document::Replacements(self.rules.clone(), self.edits.clone())
                        }
                    };
                    self.commit(value, false)?;
                    if undo {
                        self.undo.pop();
                        self.redo.push(current);
                    } else {
                        self.redo.pop();
                        self.undo.push(current);
                    }
                }
                self.state()
            }
            "import_read" => {
                control.check()?;
                let rules = model::parse_rules(&fs::read(text(p, "path")?)?)?;
                control.check()?;
                if rules.is_empty() {
                    return Err(invalid("词表中没有可用词条。"));
                }
                Ok(json!({"count":rules.len(),"rules":rules,"revision":self.revision}))
            }
            "import_commit" => {
                self.require(p.get("revision"))?;
                let values = serde_json::from_value(p["rules"].clone())?;
                control.check()?;
                self.commit_controlled(Document::Rules(values), true, control)?;
                self.state()
            }
            "export_rules" => {
                self.require(None)?;
                let bytes = model::rules_bytes(&self.rules)?;
                control.check()?;
                storage::atomic_write(Path::new(text(p, "path")?), &bytes, None)?;
                Ok(json!({"message":"词表已导出。"}))
            }
            "template" => {
                let key = text(p, "key")?;
                let values: Vec<String> = serde_json::from_value(p["values"].clone())?;
                let (old, new) = templates::build(key, &values)?;
                let sample = p["sample"].as_str().unwrap_or("");
                let (result, count) = bajptr_core::rules::regex_sample(&old, &new, sample)?;
                Ok(
                    json!({"old":old,"new":new,"sample":result,"count":count,"draft":{"key":key,"values":values,"sample":sample}}),
                )
            }
            "search" | "search_more" => {
                let keyword = text(p, "keyword")?.trim();
                let source = p.get("source").and_then(Value::as_str).unwrap_or("");
                let offset = p.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                if action == "search_more"
                    && (self.search.is_null()
                        || self.search["keyword"] != keyword
                        || self.search["source"] != source
                        || self.search["rows"].as_array().map(Vec::len) != Some(offset)
                        || p.get("search_nonce")
                            .is_some_and(|nonce| nonce != &self.search["nonce"]))
                {
                    return Err(invalid("搜索结果已变化，请重新查找后加载更多。"));
                }
                let (result, version) = self.engine.scan(
                    &self.game,
                    &self.data.join("backup"),
                    keyword,
                    source,
                    500,
                    offset,
                    control,
                )?;
                control.check()?;
                self.version = Some(version.clone());
                if action == "search_more"
                    && self.search["keyword"] == keyword
                    && self.search["source"] == source
                    && self.search["version"] == version
                {
                    let mut rows = self.search["rows"].as_array().cloned().unwrap_or_default();
                    rows.extend(result["rows"].as_array().cloned().unwrap_or_default());
                    self.search["rows"] = json!(rows);
                    self.search["total"] = result["total"].clone();
                } else {
                    self.search = json!({"game":model::game_key(&self.game)?,"version":version,"rows":result["rows"],"total":result["total"],"nonce":nonce(),"keyword":keyword,"source":source});
                }
                self.search["matching_tables"] = result["matching_tables"].clone();
                self.searches
                    .retain(|search| search["nonce"] != self.search["nonce"]);
                if self.searches.len() >= 4 {
                    self.searches.pop_front();
                }
                self.searches.push_back(self.search.clone());
                self.search_state()
            }
            "text_open" => {
                self.require(None)?;
                let (version, row, batch) = if p.get("edit_index").is_some() {
                    let e = self
                        .edits
                        .get(index(p, "edit_index")?)
                        .ok_or_else(|| invalid("修改位置已变化。"))?;
                    if e.game != model::game_key(&self.game)? {
                        return Err(invalid("这项修改不属于当前游戏。"));
                    }
                    (
                        e.version.clone(),
                        (
                            e.table.clone(),
                            e.row_id.clone(),
                            e.slot,
                            e.original.clone(),
                        ),
                        e.batch,
                    )
                } else {
                    let search = self.find_search(p)?;
                    let row = search["rows"]
                        .get(index(p, "index")?)
                        .ok_or_else(|| invalid("文本位置已变化。"))?;
                    (
                        text(search, "version")?.to_string(),
                        serde_json::from_value(row.clone())?,
                        false,
                    )
                };
                self.context(&version, &json!(self.revision), control)?;
                let key = model::game_key(&self.game)?;
                let related: Vec<_> = self
                    .edits
                    .iter()
                    .filter(|e| {
                        e.game == key
                            && e.version == version
                            && e.table == row.0
                            && e.row_id == row.1
                            && e.slot == row.2
                    })
                    .collect();
                let grouped_batch = related
                    .iter()
                    .find(|e| e.batch && e.batch_operation.is_some());
                let single = related.iter().find(|e| !e.batch);
                let (mut value, _) = model::effective(
                    &row,
                    &self.edits,
                    &model::game_key(&self.game)?,
                    &version,
                    &Rules::new(self.rules.clone())?,
                )?;
                if !batch && let Some(batch_edit) = grouped_batch {
                    value = single.map(|e| e.replacement.clone()).unwrap_or_else(|| {
                        batch_edit.batch_operation.as_ref().unwrap().before.clone()
                    });
                }
                let nonce = nonce();
                self.text_draft = json!({"nonce":nonce,"row":row,"version":version,"revision":self.revision,"initial":value,"batch":batch});
                Ok(
                    json!({"nonce":nonce,"original":row.3,"text":value,"label":model::title(&row.0),"row_id":row.1,"slot":row.2,"has_batch":!batch && grouped_batch.is_some(),"batch_operation":if batch {None}else{grouped_batch.and_then(|edit|edit.batch_operation.as_ref()).map(|operation|json!({"old":operation.old,"new":operation.new}))}}),
                )
            }
            "text_save" => {
                let draft = self.text_draft.clone();
                if draft.is_null() || p["nonce"] != draft["nonce"] {
                    return Err(invalid("编辑上下文已失效，请重新打开。"));
                }
                let version = text(&draft, "version")?;
                self.context(version, &draft["revision"], control)?;
                let replacement = text(p, "text")?.replace("\r\n", "\n").replace('\r', "\n");
                if json!(replacement) != draft["initial"] {
                    let row: TextRow = serde_json::from_value(draft["row"].clone())?;
                    let anchor = self
                        .engine
                        .migration_locations(
                            &self.game,
                            &self.data.join("backup"),
                            std::slice::from_ref(&row),
                            control,
                        )?
                        .pop()
                        .flatten()
                        .and_then(|entry| entry.anchor);
                    let e = Edit {
                        game: model::game_key(&self.game)?,
                        version: version.into(),
                        table: row.0,
                        row_id: row.1,
                        slot: row.2,
                        original: row.3,
                        replacement,
                        batch: draft["batch"] == true,
                        batch_operation: None,
                        anchor,
                    };
                    self.commit(
                        Document::Edits(model::update_edits(&self.edits, vec![e])?),
                        true,
                    )?;
                }
                self.text_draft = Value::Null;
                self.state()
            }
            "batch_open" => {
                self.require(None)?;
                let entries = self.batch_entries(text(p, "id")?)?;
                self.context(&entries[0].version, &json!(self.revision), control)?;
                let operation = entries[0].batch_operation.as_ref().unwrap();
                Ok(
                    json!({"id":operation.id,"old":operation.old,"new":operation.new,"count":entries.len()}),
                )
            }
            "batch_preview" | "promotion_missing_preview" => {
                self.require(None)?;
                let mut review_item = Value::Null;
                let mut baseline_token = Value::Null;
                let is_supplement = action == "promotion_missing_preview";
                let group_id = if is_supplement {
                    let review = &self.promotion_review;
                    if review.is_null()
                        || review["nonce"] != p["nonce"]
                        || review["revision"] != self.revision
                        || review["game"] != model::game_key(&self.game)?
                    {
                        return Err(invalid("整理结果已失效，请重新整理批量修改。"));
                    }
                    let status = self.engine.baseline_status(
                        &self.game,
                        &self.data.join("backup"),
                        &self.data,
                        control,
                    )?;
                    if status["state"] != "ready" || status["token"] != review["token"] {
                        return Err(invalid("底稿或游戏资源已变化，请重新整理批量修改。"));
                    }
                    baseline_token = status["token"].clone();
                    review_item = review["items"][index(p, "item")?].clone();
                    if !review_item["rows"]
                        .as_array()
                        .is_some_and(|rows| !rows.is_empty())
                    {
                        return Err(invalid("这个范围没有可预览的遗漏文本。"));
                    }
                    ""
                } else {
                    p.get("group_id").and_then(Value::as_str).unwrap_or("")
                };
                let existing = if group_id.is_empty() {
                    None
                } else {
                    Some(self.batch_entries(group_id)?)
                };
                let (version, rows) = if is_supplement {
                    (
                        text(&self.promotion_review, "version")?.to_string(),
                        serde_json::from_value::<Vec<TextRow>>(review_item["rows"].take())?,
                    )
                } else if let Some(entries) = &existing {
                    (
                        entries[0].version.clone(),
                        entries
                            .iter()
                            .map(|e| {
                                (
                                    e.table.clone(),
                                    e.row_id.clone(),
                                    e.slot,
                                    e.original.clone(),
                                )
                            })
                            .collect::<Vec<TextRow>>(),
                    )
                } else {
                    let search = self.find_search(p)?;
                    (
                        text(search, "version")?.to_string(),
                        serde_json::from_value::<Vec<TextRow>>(search["rows"].clone())?,
                    )
                };
                self.context(&version, &json!(self.revision), control)?;
                let terms = if is_supplement { &review_item } else { p };
                let old = text(terms, "old")?;
                let new = text(terms, "new")?;
                if old.is_empty() {
                    return Err(invalid("请填写要替换的文字。"));
                }
                let mut indices: Vec<usize> = if is_supplement {
                    (0..rows.len()).collect()
                } else {
                    serde_json::from_value(p["indices"].clone())?
                };
                indices.sort();
                indices.dedup();
                if existing.is_some() && indices != (0..rows.len()).collect::<Vec<_>>() {
                    return Err(invalid("批量修改的范围已变化，请重新打开。"));
                }
                let rules = Rules::new(self.rules.clone())?;
                let key = model::game_key(&self.game)?;
                let matching: Vec<_> = self
                    .edits
                    .iter()
                    .filter(|e| e.game == key && e.version == version)
                    .filter_map(|e| {
                        e.batch_operation
                            .as_ref()
                            .filter(|operation| operation.old == old && operation.new == new)
                            .map(|operation| (e, operation))
                    })
                    .collect();
                let operation_id = matching
                    .iter()
                    .find(|(edit, _)| !is_supplement || edit.table == rows[0].0)
                    .map(|(_, operation)| operation.id.clone())
                    .unwrap_or_else(|| {
                        if group_id.is_empty() {
                            nonce()
                        } else {
                            group_id.to_string()
                        }
                    });
                let mut changes = Vec::new();
                let mut updates = Vec::new();
                for i in &indices {
                    if is_supplement {
                        control.check()?;
                    }
                    let row = rows
                        .get(*i)
                        .ok_or_else(|| invalid("选中文字位置已变化。"))?;
                    let (before, positioned) =
                        model::effective(row, &self.edits, &key, &version, &rules)?;
                    if is_supplement && (positioned || !before.contains(old)) {
                        continue;
                    }
                    let previous = existing
                        .as_ref()
                        .map(|entries| entries[*i].batch_operation.as_ref().unwrap())
                        .or_else(|| {
                            matching
                                .iter()
                                .find(|(e, _)| {
                                    e.table == row.0 && e.row_id == row.1 && e.slot == row.2
                                })
                                .map(|(_, operation)| *operation)
                        });
                    let base = previous
                        .map(|operation| operation.before.clone())
                        .unwrap_or_else(|| before.clone());
                    let after = base.replace(old, new);
                    if before != after
                        || previous
                            .is_some_and(|operation| operation.old != old || operation.new != new)
                    {
                        changes.push(json!({"label":model::title(&row.0),"row_id":row.1,"slot":row.2,"before":before,"after":after}));
                        updates.push(Edit {
                            game: key.clone(),
                            version: version.clone(),
                            table: row.0.clone(),
                            row_id: row.1.clone(),
                            slot: row.2,
                            original: row.3.clone(),
                            replacement: after,
                            batch: true,
                            anchor: None,
                            batch_operation: Some(model::BatchOperation {
                                id: operation_id.clone(),
                                old: old.into(),
                                new: new.into(),
                                before: base,
                                previous_scope: previous
                                    .filter(|operation| {
                                        operation.old == old && operation.new == new
                                    })
                                    .and_then(|operation| operation.previous_scope.clone()),
                            }),
                        });
                    }
                }
                if is_supplement && changes.is_empty() {
                    self.batch_draft = Value::Null;
                    return Err(invalid("已没有可补改的文本，请重新整理。"));
                }
                let mut edits = model::update_edits(&self.edits, updates)?;
                if !is_supplement {
                    // Saving the same replacement extends its selected scope while retaining each member's base text.
                    for edit in &mut edits {
                        if edit.game == key
                            && edit.version == version
                            && let Some(operation) = &mut edit.batch_operation
                            && operation.old == old
                            && operation.new == new
                        {
                            operation.id.clone_from(&operation_id);
                        }
                    }
                }
                let nonce = nonce();
                self.batch_draft =
                    json!({"nonce":nonce,"edits":edits,"revision":self.revision,"version":version});
                if is_supplement {
                    self.batch_draft["baseline_token"] = baseline_token;
                    return Ok(
                        json!({"nonce":nonce,"changes":changes,"label":review_item["label"],"old":old,"new":new}),
                    );
                }
                Ok(
                    json!({"nonce":nonce,"changes":changes,"draft":{"indices":indices,"old":old,"new":new,"group_id":group_id}}),
                )
            }
            "batch_save" => {
                let draft = self.batch_draft.clone();
                if draft.is_null() || p["nonce"] != draft["nonce"] {
                    return Err(invalid("批量预览已失效，请重新预览。"));
                }
                let version = text(&draft, "version")?;
                self.context(version, &draft["revision"], control)?;
                if !draft["baseline_token"].is_null() {
                    let status = self.engine.baseline_status(
                        &self.game,
                        &self.data.join("backup"),
                        &self.data,
                        control,
                    )?;
                    if status["state"] != "ready" || status["token"] != draft["baseline_token"] {
                        return Err(invalid("底稿或游戏资源已变化，请重新整理批量修改。"));
                    }
                }
                let game = model::game_key(&self.game)?;
                let mut edits: Vec<Edit> = serde_json::from_value(draft["edits"].clone())?;
                let pending: Vec<_> = edits
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.game == game && e.version == version && e.anchor.is_none())
                    .map(|(i, e)| {
                        (
                            i,
                            (
                                e.table.clone(),
                                e.row_id.clone(),
                                e.slot,
                                e.original.clone(),
                            ),
                        )
                    })
                    .collect();
                if !pending.is_empty() {
                    let rows: Vec<_> = pending.iter().map(|(_, row)| row.clone()).collect();
                    let found = self.engine.migration_locations(
                        &self.game,
                        &self.data.join("backup"),
                        &rows,
                        control,
                    )?;
                    for ((i, _), entry) in pending.into_iter().zip(found) {
                        edits[i].anchor = entry.and_then(|entry| entry.anchor);
                    }
                }
                self.commit(Document::Edits(edits), true)?;
                self.state()
            }
            "remove_batch_member" => {
                let revision = p
                    .get("revision")
                    .ok_or_else(|| invalid("请重新打开批量文本范围。"))?;
                self.require(Some(revision))?;
                let id = text(p, "id")?;
                let i = index(p, "index")?;
                let entry = self
                    .edits
                    .get(i)
                    .ok_or_else(|| invalid("该文本段已变化，请重新打开范围。"))?;
                if entry.game != model::game_key(&self.game)?
                    || !entry.batch
                    || !entry
                        .batch_operation
                        .as_ref()
                        .is_some_and(|operation| operation.id == id)
                {
                    return Err(invalid("该文本段不属于这项批量修改，未移除任何记录。"));
                }
                let mut values = self.edits.clone();
                values.remove(i);
                self.commit(Document::Edits(values), true)?;
                self.state()
            }
            "remove_batch" => {
                self.require(p.get("revision"))?;
                let id = text(p, "id")?;
                self.batch_entries(id)?;
                let game = model::game_key(&self.game)?;
                let values = self
                    .edits
                    .iter()
                    .filter(|e| {
                        e.game != game
                            || !e
                                .batch_operation
                                .as_ref()
                                .is_some_and(|operation| operation.id == id)
                    })
                    .cloned()
                    .collect();
                self.commit(Document::Edits(values), true)?;
                self.state()
            }
            "remove_edit" => {
                self.require(p.get("revision"))?;
                let mut values = self.edits.clone();
                let i = index(p, "index")?;
                if i >= values.len() {
                    return Err(invalid("修改位置已变化。"));
                }
                let removed = values.remove(i);
                if !removed.batch {
                    let row = (
                        removed.table.clone(),
                        removed.row_id.clone(),
                        removed.slot,
                        removed.original.clone(),
                    );
                    let base = model::effective(
                        &row,
                        &[],
                        &removed.game,
                        &removed.version,
                        &Rules::new(self.rules.clone())?,
                    )?
                    .0;
                    for batch in values
                        .iter_mut()
                        .filter(|e| e.batch && e.same_address(&removed))
                    {
                        if let Some(operation) = &mut batch.batch_operation {
                            operation.before.clone_from(&base);
                            batch.replacement = base.replace(&operation.old, &operation.new);
                        }
                    }
                }
                self.commit(Document::Edits(values), true)?;
                self.state()
            }
            "preview" => {
                self.require(None)?;
                let stats = self
                    .engine
                    .preview(&self.game, &self.data, &self.config(), control)?;
                self.application = if stats["parse_failed"] != 0 {
                    "预览存在错误，无法核对应用状态"
                } else if service::total(&stats) == 0 {
                    "当前游戏文字与词表一致"
                } else {
                    "词表已保存，尚未应用"
                }
                .into();
                let nonce = nonce();
                self.preview = json!({"nonce":nonce,"stats":stats,"revision":self.revision});
                Ok(
                    json!({"nonce":nonce,"total_changes":service::total(&stats),"stats":stats,"application":self.application}),
                )
            }
            "prepare_patch" | "apply" => {
                self.require(None)?;
                let draft = self.preview.clone();
                if draft.is_null()
                    || p["nonce"] != draft["nonce"]
                    || draft["revision"] != self.revision
                {
                    return Err(invalid("替换设置或预览已变化，请重新预览。"));
                }
                let count = service::total(&draft["stats"]);
                if count == 0 || draft["stats"]["parse_failed"] != 0 {
                    return Err(invalid("当前预览无法应用，请检查结果。"));
                }
                let config = self.config();
                let backup_root = self.data.join("backup");
                let output_root = self.data.join("output");
                let job = PatchJob {
                    game: &self.game,
                    data: &self.data,
                    backup_root: &backup_root,
                    output_root: &output_root,
                    config: &config,
                    install: true,
                    expected: Some(&draft["stats"]["comparison_identity"]),
                };
                if action == "prepare_patch" {
                    let mut result = self.engine.prepare(job, control)?;
                    result["nonce"] = draft["nonce"].clone();
                    return Ok(result);
                }
                let mut result = if p["prepared"] == true {
                    self.engine.apply_prepared(job, control)?
                } else {
                    self.engine.generate(job, control)?
                };
                if result["installed"] != true {
                    self.application = "应用未完成，补丁已保存".into();
                    let output = Path::new(text(&result, "output")?)
                        .parent()
                        .unwrap_or(&self.data)
                        .display()
                        .to_string();
                    result["message"] = json!("应用到游戏失败，已自动保存校验通过的补丁。");
                    result["warning"] = json!(format!(
                        "应用到游戏失败：{}\n\n已自动保存校验通过的补丁：\n{output}\n\n本次修改尚未确认应用成功。请退出游戏并检查目录写入权限后重试。",
                        text(&result, "install_error")?
                    ));
                    result["application"] = json!(self.application);
                    return Ok(result);
                }
                self.preview = Value::Null;
                self.application = "已应用，与当前词表一致".into();
                result["message"] = json!(format!("已应用 {count} 处变化，校验通过。"));
                result["application"] = json!(self.application);
                Ok(result)
            }
            "baseline" => {
                let original =
                    self.engine
                        .baseline(&self.game, &self.data.join("backup"), control)?;
                self.version = Some(original.version.clone());
                baseline::info(&original)
            }
            "restore" => {
                self.engine
                    .restore(&self.game, &self.data, &self.data.join("backup"), control)?;
                self.preview = Value::Null;
                self.application = "已恢复底稿；词表尚未应用".into();
                Ok(
                    json!({"message":"已恢复为当前版本初始底稿，词表、单条及批量修改记录已保留。","application":self.application}),
                )
            }
            "export_text" => {
                let path = text(p, "path")?;
                let count = self.engine.export(
                    &self.game,
                    &self.data.join("backup"),
                    Path::new(path),
                    control,
                )?;
                Ok(
                    json!({"output":path,"message":format!("已导出 {count} 条底稿文本。"),"count":count}),
                )
            }
            _ => Err(invalid("未知界面操作。")),
        }
    }
}
