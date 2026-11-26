//! Verified baseline transformations shared by the desktop and command line.
use crate::{
    Control, Result,
    baseline::{self, Baseline, Baselines},
    invalid,
    model::{self, Edit},
    promotion::{self, TableEvaluator},
    region,
    storage::{self, Lock},
};
use bajptr_core::{
    codec, database,
    protocol::{self, Session, execute},
    rules::{Rule, Rules},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque, hash_map::Entry},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Config {
    rules: Vec<Rule>,
    edits: Arc<Vec<Edit>>,
    rules_document: Option<DocumentRevision>,
    edits_document: Option<DocumentRevision>,
    pub ascii_exact: bool,
    pub tables: String,
    pub work_dir: Option<PathBuf>,
    compiled: RefCell<Option<Arc<Rules>>>,
}
struct DocumentRevision {
    path: PathBuf,
    digest: Option<String>,
}
pub struct PatchJob<'a> {
    pub game: &'a str,
    pub data: &'a Path,
    pub backup_root: &'a Path,
    pub output_root: &'a Path,
    pub config: &'a Config,
    pub install: bool,
    pub expected: Option<&'a Value>,
}
impl Config {
    pub fn new(rules: Vec<Rule>, edits: Vec<Edit>) -> Self {
        Self::from_shared_edits(rules, Arc::new(edits))
    }
    pub fn from_compiled_rules(rules: Rules) -> Self {
        let mut config = Self::new(rules.values.clone(), vec![]);
        *config.compiled.get_mut() = Some(Arc::new(rules));
        config
    }
    pub fn from_glossary(path: Option<PathBuf>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::from_compiled_rules(Rules::default()));
        };
        let bytes = fs::read(&path)?;
        let mut config = Self::from_compiled_rules(model::parse_compiled_rules(&bytes)?);
        config.rules_document = Some(DocumentRevision {
            path,
            digest: Some(storage::digest(&bytes)),
        });
        Ok(config)
    }
    pub(crate) fn from_shared_edits(rules: Vec<Rule>, edits: Arc<Vec<Edit>>) -> Self {
        Self {
            rules,
            edits,
            rules_document: None,
            edits_document: None,
            ascii_exact: true,
            tables: database::default_tables(),
            work_dir: None,
            compiled: RefCell::new(None),
        }
    }
    pub(crate) fn with_documents(mut self, store: &storage::Store, compiled: Arc<Rules>) -> Self {
        let document = |name| {
            store
                .expected_revision(name)
                .map(|digest| DocumentRevision {
                    path: store.root.join(name),
                    digest: digest.clone(),
                })
        };
        self.rules_document = document("rules.tsv");
        self.edits_document = document("text-edits.json");
        *self.compiled.get_mut() = Some(compiled);
        self
    }
    pub fn compiled_rules(&self) -> Result<Arc<Rules>> {
        let mut compiled = self.compiled.borrow_mut();
        if let Some(rules) = compiled.as_ref().filter(|rules| rules.values == self.rules) {
            return Ok(rules.clone());
        }
        let rules = Arc::new(Rules::new(self.rules.clone())?);
        *compiled = Some(rules.clone());
        Ok(rules)
    }
    fn options(&self, game: &str, version: &str) -> Result<database::Options> {
        Ok(database::Options {
            ascii_exact: self.ascii_exact,
            tables: self.tables.clone(),
            explain: true,
            edits: model::final_edits(&self.edits, game, version)?,
        })
    }
    pub fn check_documents(&self) -> Result<()> {
        for (document, changed) in [
            (
                &self.rules_document,
                "词表已被其他窗口修改，请重新打开并预览。",
            ),
            (
                &self.edits_document,
                "单条修改记录已变化，请重新打开并预览。",
            ),
        ] {
            if let Some(document) = document
                && storage::revision(&document.path)? != document.digest
            {
                return Err(invalid(changed));
            }
        }
        Ok(())
    }
    pub fn lock_checked_documents(&self) -> Result<Vec<Lock>> {
        let locks = [&self.rules_document, &self.edits_document]
            .into_iter()
            .flatten()
            .map(|document| Lock::acquire(&document.path))
            .collect::<Result<Vec<_>>>()?;
        self.check_documents()?;
        Ok(locks)
    }
}
#[derive(Default)]
pub struct Engine {
    pub baselines: Baselines,
    session: Session,
    source_key: Option<(PathBuf, baseline::Stamp)>,
    identical_live: Option<Value>,
    search_index: Option<(String, database::SearchIndex)>,
    scope_sources: VecDeque<(String, Value)>,
    previews: VecDeque<(String, Value)>,
    prepared: Option<PreparedPatch>,
    preparation_stage: Option<tempfile::TempDir>,
}

#[derive(PartialEq)]
enum PatchMode {
    Generate,
    Prepare,
    ApplyPrepared,
}

#[derive(Clone)]
struct PreparedPatch {
    key: String,
    output: PathBuf,
    size: u64,
    crc: u32,
    stats: Value,
    seconds: f64,
}
fn same_bundle(left: &Path, right: &Path, control: &Control<'_>) -> Result<bool> {
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut remaining = left.metadata()?.len();
    if remaining != right.metadata()?.len() {
        return Ok(false);
    }
    let mut a = vec![0; 256 * 1024];
    let mut b = vec![0; a.len()];
    while remaining > 0 {
        control.check()?;
        let count = remaining.min(a.len() as u64) as usize;
        left.read_exact(&mut a[..count])?;
        right.read_exact(&mut b[..count])?;
        if a[..count] != b[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(true)
}

impl Engine {
    pub fn with_work(work: PathBuf) -> Self {
        let mut engine = Self::default();
        engine.baselines = Baselines::with_work(work);
        engine
    }
    pub fn rebuild_baseline(
        &mut self,
        game: &str,
        root: &Path,
        expected: Option<&Value>,
        control: &Control<'_>,
    ) -> Result<Value> {
        let live = region::locate(game)?;
        let _lock = Lock::acquire(&live.catalog)?;
        if let Some(expected) = expected {
            self.close();
            let result = crate::recovery::rebuild_baseline(
                &live,
                root,
                &self.baselines.work,
                expected,
                control,
            )?;
            self.baselines = Baselines::with_work(self.baselines.work.clone());
            Ok(result)
        } else {
            crate::recovery::baseline_plan(&live, root, &self.baselines.work, control)
        }
    }
    pub(crate) fn migration_candidates(
        &mut self,
        game: &str,
        root: &Path,
        edits: &[Edit],
        control: &Control<'_>,
    ) -> Result<crate::migration::scan::Candidates> {
        let original = self.baseline(game, root, control)?;
        self.open(&original, control)?;
        crate::migration::scan::collect(
            &self.session.database.as_ref().unwrap().con,
            edits,
            control,
        )
    }
    pub(crate) fn migration_source_anchors(
        &mut self,
        game: &str,
        root: &Path,
        edits: &mut [Edit],
        control: &Control<'_>,
    ) -> Result<()> {
        let live = region::locate(game)?;
        let current = region::version(&live.catalog)?;
        let game = storage::identity(&live.dir)?;
        let mut versions = std::collections::BTreeMap::<String, Vec<&mut Edit>>::new();
        for edit in edits.iter_mut().filter(|edit| {
            edit.anchor.is_none() && edit.game.as_ref() == game && edit.version.as_ref() != current
        }) {
            versions
                .entry(edit.version.to_string())
                .or_default()
                .push(edit);
        }
        for (version, edits) in versions {
            let result = (|| -> Result<()> {
                let Some(source) = self.baselines.read_saved(&live, root, &version, control)?
                else {
                    return Ok(());
                };
                let db = database::Database::open_in(
                    &source.bundle,
                    true,
                    &codec::key(region::KEY)?,
                    control,
                    &self.baselines.work,
                )?;
                for edit in edits {
                    let row = (
                        edit.table.to_string(),
                        edit.row_id.clone(),
                        edit.slot,
                        edit.original.clone(),
                    );
                    edit.anchor =
                        database::location_at(&db.con, &row, control)?.and_then(|row| row.anchor);
                }
                Ok(())
            })();
            if let Err(error) = result {
                if matches!(error, crate::Error::Cancelled) {
                    return Err(error);
                }
                control.warn(&format!("无法读取旧底稿上下文：{error}"))?;
            }
        }
        Ok(())
    }
    pub(crate) fn migration_locations(
        &mut self,
        game: &str,
        root: &Path,
        rows: &[database::TextRow],
        control: &Control<'_>,
    ) -> Result<Vec<Option<database::LocatedText>>> {
        let original = self.baseline(game, root, control)?;
        self.open(&original, control)?;
        let con = &self.session.database.as_ref().unwrap().con;
        rows.iter()
            .map(|row| database::location_at(con, row, control))
            .collect()
    }
    pub fn close(&mut self) {
        self.invalidate_prepared();
        self.close_session();
    }
    fn close_session(&mut self) {
        self.session = Session {
            work_dir: Some(self.baselines.work.clone()),
            ..Session::default()
        };
        self.source_key = None;
        self.identical_live = None;
        self.search_index = None;
        self.scope_sources.clear();
        self.previews.clear();
    }
    fn open(&mut self, original: &Baseline, control: &Control<'_>) -> Result<()> {
        let key = (original.bundle.clone(), baseline::stamp(&original.bundle)?);
        if self.source_key.as_ref() != Some(&key) {
            self.close_session();
            control.log("正在读取初始底稿…")?;
            self.session.database = Some(database::Database::open_in(
                &original.bundle,
                true,
                &codec::key(region::KEY)?,
                control,
                &self.baselines.work,
            )?);
            self.source_key = Some(key);
        }
        Ok(())
    }
    fn open_baseline(
        &mut self,
        game: &region::Game,
        root: &Path,
        control: &Control<'_>,
    ) -> Result<Baseline> {
        let _lock = Lock::acquire(&game.catalog)?;
        let (original, entry) = Baselines::entry_for_open(game, root)?;
        self.open_verified(&original, &entry, control)?;
        Ok(original)
    }
    fn open_verified(
        &mut self,
        original: &Baseline,
        entry: &region::Entry,
        control: &Control<'_>,
    ) -> Result<()> {
        let source_stamp = baseline::stamp(&original.bundle)?;
        let source_key = (original.bundle.clone(), source_stamp);
        if self.source_key.as_ref() == Some(&source_key) {
            self.baselines.checked(&original.bundle, entry, control)?;
        } else {
            self.close_session();
            control.log("正在读取初始底稿…")?;
            let db = database::Database::open_verified_in(
                &original.bundle,
                &codec::key(region::KEY)?,
                control,
                entry.size,
                entry.crc,
                &self.baselines.work,
            )?;
            if baseline::stamp(&original.bundle)? != source_stamp {
                return Err(invalid("数据库在读取期间发生变化，请重试。"));
            }
            self.baselines
                .remember_checked(&original.bundle, source_stamp, entry.crc);
            self.session.database = Some(db);
            self.source_key = Some(source_key);
        }
        Ok(())
    }
    pub fn baseline(&mut self, game: &str, root: &Path, control: &Control<'_>) -> Result<Baseline> {
        let game = region::locate(game)?;
        let _lock = Lock::acquire(&game.catalog)?;
        self.baselines.resolve(&game, root, control)
    }
    pub fn baseline_status(
        &mut self,
        game: &str,
        root: &Path,
        data: &Path,
        control: &Control<'_>,
    ) -> Result<Value> {
        self.baseline_status_for_action(game, root, data, false, control)
    }
    pub(crate) fn baseline_status_for_action(
        &mut self,
        game: &str,
        root: &Path,
        data: &Path,
        open_ready: bool,
        control: &Control<'_>,
    ) -> Result<Value> {
        let live = region::locate(game)?;
        let _lock = Lock::acquire(&live.catalog)?;
        self.baseline_status_locked(&live, root, data, open_ready, control)
    }
    fn baseline_status_locked(
        &mut self,
        live: &region::Game,
        root: &Path,
        data: &Path,
        open_ready: bool,
        control: &Control<'_>,
    ) -> Result<Value> {
        let destination = baseline::location(live, root)?;
        let record = destination.join("baseline.json");
        let identity = region::resource_identity(live)?;
        let version = region::version(&live.catalog)?;
        let key = storage::identity(&live.dir)?;
        let receipts = read_receipts(&data.join(".gui/application-state.json"))?;
        let receipt = &receipts[&key];
        // Touching or copying a file does not turn our own patch into a game update.
        let known = receipt["identity"].is_array()
            && [0, 1, 4, 5]
                .iter()
                .all(|i| receipt["identity"][*i] == identity[*i]);
        let mut saved = Value::Null;
        let state = if destination.exists() {
            let (original, entry) = Baselines::saved_entry(live, root, &version)?
                .ok_or_else(|| Baselines::missing(true))?;
            let ready = (entry.size, entry.crc) == (live.entry.size, live.entry.crc) || known;
            if open_ready && ready {
                self.open_verified(&original, &entry, control)?;
            } else {
                self.baselines.checked(&original.bundle, &entry, control)?;
            }
            saved = baseline::info(&original)?;
            if ready { "ready" } else { "changed" }
        } else if known && receipt["kind"] != "restored" {
            "blocked"
        } else if baseline::has_previous(&destination) {
            "update"
        } else {
            "missing"
        };
        let token = json!({"game":key,"version":version,"identity":identity,"saved":storage::revision(&record)?});
        let marker = fs::read(live.catalog.with_file_name("TableCatalog.hash"))
            .ok()
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .trim()
                    .chars()
                    .take(80)
                    .collect::<String>()
            })
            .unwrap_or_else(|| "当前资源版本".into());
        Ok(json!({"state":state,"token":token,"baseline":saved,"game_version":marker}))
    }
    pub fn initialize(
        &mut self,
        game: &str,
        root: &Path,
        data: &Path,
        expected: Option<&Value>,
        control: &Control<'_>,
    ) -> Result<Baseline> {
        self.initialize_for_edits(
            game,
            root,
            data,
            expected,
            &std::collections::HashSet::new(),
            control,
        )
    }
    pub(crate) fn initialize_for_edits(
        &mut self,
        game: &str,
        root: &Path,
        data: &Path,
        expected: Option<&Value>,
        keep: &std::collections::HashSet<String>,
        control: &Control<'_>,
    ) -> Result<Baseline> {
        let live = region::locate(game)?;
        let _lock = Lock::acquire(&live.catalog)?;
        let status = self.baseline_status_locked(&live, root, data, false, control)?;
        if expected.is_some_and(|v| v != &status["token"]) {
            return Err(invalid(
                "游戏资源或底稿已变化，请重新检查后确认，未替换原底稿。",
            ));
        }
        match status["state"].as_str() {
            Some("ready") => self.baselines.resolve(&live, root, control),
            Some("missing" | "update" | "changed") => {
                self.close();
                self.baselines
                    .capture(&live, root, status["state"] == "changed", keep, control)
            }
            _ => Err(invalid(
                "当前资源是本工具修改后的内容，不能作为初始底稿。请先通过游戏修复资源，或找回原底稿。",
            )),
        }
    }
    pub fn tables(&mut self, game: &str, root: &Path, control: &Control<'_>) -> Result<Value> {
        self.tables_cached(game, root, None, control)
    }
    pub fn source_rule_matches(
        &mut self,
        game: &str,
        root: &Path,
        names: &[String],
        rules: &Rules,
        control: &Control<'_>,
    ) -> Result<Vec<database::SourceRuleMatch>> {
        let original = self.baseline(game, root, control)?;
        self.open(&original, control)?;
        database::source_rule_matches(
            &self
                .session
                .database
                .as_ref()
                .ok_or_else(|| invalid("底稿尚未读取。"))?
                .con,
            names,
            rules,
            control,
        )
    }
    pub fn scope_sources(
        &mut self,
        game: &str,
        root: &Path,
        old: &str,
        mode: &str,
        control: &Control<'_>,
    ) -> Result<Value> {
        let original = self.baseline(game, root, control)?;
        self.open(&original, control)?;
        let key = serde_json::to_string(&(old, mode))?;
        if let Some((_, value)) = self.scope_sources.iter().find(|(k, _)| k == &key) {
            control.check()?;
            return Ok(value.clone());
        }
        let names = database::tables(
            &self.session.database.as_ref().unwrap().con,
            &database::default_tables(),
        )?;
        let names = if old.is_empty() {
            names
        } else {
            self.source_rule_matches(
                game,
                root,
                &names,
                &Rules::new(vec![Rule {
                    old: old.into(),
                    new: String::new(),
                    mode: mode.into(),
                    source: String::new(),
                    table: String::new(),
                }])?,
                control,
            )?
            .into_iter()
            .map(|hit| hit.table)
            .collect()
        };
        let value = json!(
            names
                .iter()
                .map(|name| model::table_info(name))
                .collect::<Vec<_>>()
        );
        remember(&mut self.scope_sources, key, value.clone(), 8);
        Ok(value)
    }
    pub fn tables_cached(
        &mut self,
        game: &str,
        root: &Path,
        metadata: Option<&Path>,
        control: &Control<'_>,
    ) -> Result<Value> {
        self.table_names(game, root, metadata, true, control)
    }
    /// Reuse valid metadata without replacing a previous inventory during migration.
    pub fn observed_tables(
        &mut self,
        game: &str,
        root: &Path,
        metadata: &Path,
        control: &Control<'_>,
    ) -> Result<Value> {
        self.table_names(game, root, Some(metadata), false, control)
    }
    fn table_names(
        &mut self,
        game: &str,
        root: &Path,
        metadata: Option<&Path>,
        publish: bool,
        control: &Control<'_>,
    ) -> Result<Value> {
        let original = self.baseline(game, root, control)?;
        let live = region::locate(game)?;
        let identity = region::source_identity(&live, &original.bundle)?;
        if let Some(path) = metadata
            && let Some(value) = fs::read(path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            && value["format"] == 2
            && value["identity"] == identity
            && value["tables"]
                .as_array()
                .is_some_and(|a| a.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty())))
        {
            return Ok(value["tables"].clone());
        }
        self.open(&original, control)?;
        let tables = execute(&mut self.session, json!({"op":"tables"}), control)?;
        if region::source_identity(&region::locate(game)?, &original.bundle)? != identity {
            return Err(invalid("游戏资源在读取期间发生变化，请重试。"));
        }
        if publish && let Some(path) = metadata {
            let _ = storage::atomic_write(
                path,
                &serde_json::to_vec(&json!({"format":2,"identity":identity,"tables":tables}))?,
                None,
            );
        }
        Ok(tables)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn scan(
        &mut self,
        game: &str,
        root: &Path,
        keyword: &str,
        tables: &str,
        limit: usize,
        offset: usize,
        control: &Control<'_>,
    ) -> Result<(Value, String)> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Err(invalid("请输入要查找的文字。"));
        }
        let original = self.open_baseline(&region::locate(game)?, root, control)?;
        let key = keyword.to_owned();
        if self
            .search_index
            .as_ref()
            .is_none_or(|(cached, _)| cached != &key)
        {
            self.search_index = None;
            let index = database::SearchIndex::build(
                &self.session.database.as_ref().unwrap().con,
                &database::default_tables(),
                keyword,
                control,
            )?;
            self.search_index = Some((key, index));
        }
        let value = serde_json::to_value(
            self.search_index
                .as_ref()
                .unwrap()
                .1
                .page(tables, limit, offset, control)?,
        )?;
        if region::version(&region::locate(game)?.catalog)? != original.version {
            return Err(invalid("游戏资源版本已变化，请重新查找。"));
        }
        Ok((value, original.version))
    }
    pub(crate) fn promotion_plan(
        &mut self,
        game: &str,
        data: &Path,
        config: &Config,
        control: &Control<'_>,
    ) -> Result<promotion::Plan> {
        config.check_documents()?;
        let original = self.baseline(game, &data.join("backup"), control)?;
        self.open(&original, control)?;
        let live = region::locate(game)?;
        let game_key = storage::identity(&live.dir)?;
        let mut plan = promotion::plan(
            &self.session.database.as_ref().unwrap().con,
            &config.rules,
            &config.edits,
            &game_key,
            &original.version,
            control,
        )?;
        let mut databases = HashMap::<String, Option<database::Database>>::new();
        let mut snapshots = HashMap::<String, Vec<Edit>>::new();
        let current: Vec<_> = plan
            .edits
            .iter()
            .filter(|edit| {
                edit.game.as_ref() == game_key && edit.version.as_ref() == original.version
            })
            .cloned()
            .collect();
        let rules = Rules::new(plan.rules.clone())?;
        let reviewed = promotion::read_reviews(data, &game_key, &original.version)?;
        for item in &mut plan.items {
            if item.missing_rows.is_empty() {
                continue;
            }
            let comparison = (|| -> Result<_> {
                let mut previous = Vec::new();
                for source in &item.previous_scopes {
                    control.check()?;
                    if !model::is_version(&source.snapshot) || source.version == original.version {
                        return Ok(None);
                    }
                    let entries = match snapshots.entry(source.snapshot.clone()) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            let gui = data.join(".gui");
                            let path = gui
                                .join("recovery")
                                .join(format!("migration-{}", source.snapshot))
                                .join("text-edits.json");
                            let bytes = match fs::read(path) {
                                Ok(bytes) => bytes,
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    let legacy = gui.join(format!(
                                        "text-edits-before-migration-{}.json",
                                        &source.snapshot[..16]
                                    ));
                                    match fs::read(legacy) {
                                        Ok(bytes) => bytes,
                                        Err(error)
                                            if error.kind() == std::io::ErrorKind::NotFound =>
                                        {
                                            return Ok(None);
                                        }
                                        Err(error) => return Err(error.into()),
                                    }
                                }
                                Err(error) => return Err(error.into()),
                            };
                            if storage::digest(&bytes) != source.snapshot {
                                return Ok(None);
                            }
                            entry.insert(model::parse_edits(&bytes)?)
                        }
                    };
                    let selected: Vec<_> = entries
                        .iter()
                        .filter(|edit| {
                            edit.table.as_ref() == item.table
                                && crate::combined::batch_matches(
                                    edit,
                                    &game_key,
                                    &source.version,
                                    &item.old,
                                    &item.new,
                                )
                        })
                        .collect();
                    if selected.is_empty() {
                        return Ok(None);
                    }
                    let db = match databases.entry(source.version.clone()) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            let baseline = self.baselines.read_saved(
                                &live,
                                &data.join("backup"),
                                &source.version,
                                control,
                            )?;
                            let db = baseline
                                .map(|baseline| {
                                    database::Database::open_in(
                                        &baseline.bundle,
                                        true,
                                        &codec::key(region::KEY)?,
                                        control,
                                        &self.baselines.work,
                                    )
                                })
                                .transpose()?;
                            entry.insert(db)
                        }
                    };
                    let Some(db) = db else {
                        return Ok(None);
                    };
                    previous.push(item.compare_previous(&db.con, &selected, &current, control)?);
                }
                Ok(Some(previous))
            })();
            let previous = match comparison {
                Ok(previous) => previous.unwrap_or_default(),
                Err(crate::Error::Cancelled) => return Err(crate::Error::Cancelled),
                Err(error) => {
                    control.warn(&format!("无法核对批量修改的旧版选区：{error}"))?;
                    Vec::new()
                }
            };
            let evaluator =
                TableEvaluator::new(&item.table, &rules, &current, &game_key, &original.version);
            item.retain_review_rows(&previous, evaluator, &reviewed, control)?;
        }
        Ok(plan)
    }
    pub fn export(
        &mut self,
        game: &str,
        root: &Path,
        target: &Path,
        control: &Control<'_>,
    ) -> Result<u64> {
        let original = self.baseline(game, root, control)?;
        self.open(&original, control)?;
        crate::text_export::export(target, control, |stage| {
            let result = execute(
                &mut self.session,
                json!({"op":"export","destination":stage}),
                control,
            )?;
            Ok(result["count"].as_u64().unwrap_or(0))
        })
    }
    pub fn preview(
        &mut self,
        game: &str,
        data: &Path,
        config: &Config,
        control: &Control<'_>,
    ) -> Result<Value> {
        self.preview_at(game, data, &data.join("backup"), config, control)
    }
    pub fn preview_at(
        &mut self,
        game: &str,
        data: &Path,
        backup_root: &Path,
        config: &Config,
        control: &Control<'_>,
    ) -> Result<Value> {
        let live = region::locate(game)?;
        let identity = region::resource_identity(&live)?;
        let original = self.open_baseline(&live, backup_root, control)?;
        let game_key = storage::identity(&live.dir)?;
        let key = storage::serialized_digest(&(
            &identity,
            &original.version,
            &config.rules,
            &config.edits,
            &config.tables,
            config.ascii_exact,
        ))?;
        let mut stats = if let Some((_, stats)) = self.previews.iter().find(|(k, _)| k == &key) {
            stats.clone()
        } else {
            let mut compare = false;
            if self.identical_live.as_ref() != Some(&identity) {
                let eligible = original.bundle.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .ends_with(&format!("_{}", live.entry.crc))
                });
                if eligible && same_bundle(&live.bundle, &original.bundle, control)? {
                    self.identical_live = Some(identity.clone());
                } else {
                    compare = true;
                }
            }
            let rules = config.compiled_rules()?;
            let options = config.options(&game_key, &original.version)?;
            let comparison_key = compare.then(|| identity.to_string());
            let mut value = serde_json::to_value(protocol::preview(
                &mut self.session,
                &rules,
                options,
                comparison_key
                    .as_deref()
                    .map(|id| (live.bundle.as_path(), Some(id), region::KEY)),
                control,
            )?)?;
            value["comparison_identity"] = identity.clone();
            value["zero_reasons"] = json!(
                value["zero_reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| json!([m["index"], m["reason"], m["example"], m["table"]]))
                    .collect::<Vec<_>>()
            );
            value["inactive_edits"] = json!(
                config
                    .edits
                    .iter()
                    .filter(
                        |e| e.game.as_ref() == game_key && e.version.as_ref() != original.version
                    )
                    .map(Edit::array)
                    .collect::<Vec<_>>()
            );
            remember(&mut self.previews, key, value.clone(), 4);
            value
        };
        if region::resource_identity(&region::locate(game)?)? != identity {
            return Err(invalid("游戏资源在预览期间发生变化，请重新预览。"));
        }
        config.check_documents()?;
        stats["total_changes"] = json!(total(&stats));
        if total(&stats) == 0
            && stats["parse_failed"] == 0
            && let Err(e) = record(game, data, config, "checked", Some(&identity))
        {
            control.warn(&format!("预览完成；应用状态记录未保存：{e}"))?;
        }
        Ok(stats)
    }
    pub fn invalidate_prepared(&mut self) {
        if let Some(patch) = self.prepared.take()
            && let Err(error) = fs::remove_file(&patch.output)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            self.preparation_stage = None;
        }
    }
    fn preparation_key(
        game: &region::Game,
        original: &Baseline,
        config: &Config,
    ) -> Result<String> {
        storage::serialized_digest(&(
            region::resource_identity(game)?,
            storage::identity(&original.bundle)?,
            format!("{:?}", baseline::stamp(&original.bundle)?),
            storage::revision(&original.catalog)?,
            storage::revision(&original.catalog.with_file_name("baseline.json"))?,
            &config.rules,
            &config.edits,
            &config.tables,
            config.ascii_exact,
        ))
    }
    /// Prepare a private, verified patch without publishing anything to the game
    /// or the user's fallback output directory. Closing the preview keeps it.
    pub fn prepare(&mut self, job: PatchJob<'_>, control: &Control<'_>) -> Result<Value> {
        self.generate_inner(job, PatchMode::Prepare, control)
    }
    fn check_prepared(&mut self, patch: &PreparedPatch, control: &Control<'_>) -> Result<()> {
        self.baselines.checked(
            &patch.output,
            &region::Entry {
                size_offset: 0,
                crc_offset: 0,
                size: patch.size,
                crc: patch.crc,
            },
            control,
        )
    }
    pub fn apply_prepared(&mut self, job: PatchJob<'_>, control: &Control<'_>) -> Result<Value> {
        self.generate_inner(job, PatchMode::ApplyPrepared, control)
    }
    pub fn generate(&mut self, job: PatchJob<'_>, control: &Control<'_>) -> Result<Value> {
        self.generate_inner(job, PatchMode::Generate, control)
    }
    fn generate_inner(
        &mut self,
        job: PatchJob<'_>,
        mode: PatchMode,
        control: &Control<'_>,
    ) -> Result<Value> {
        let PatchJob {
            game,
            data,
            backup_root,
            output_root,
            config,
            install,
            expected,
        } = job;
        let live = region::locate(game)?;
        let _game_lock = Lock::acquire(&live.catalog)?;
        let identity = region::resource_identity(&live)?;
        if expected.is_some_and(|e| e != &identity) {
            return Err(invalid("游戏资源已变化，请重新预览。"));
        }
        let original = self.baselines.resolve(&live, backup_root, control)?;
        let source_stat = baseline::stamp(&original.bundle)?;
        let cache_key = Self::preparation_key(&live, &original, config)?;
        let prepare = mode == PatchMode::Prepare;
        if prepare || install {
            if let Some(patch) = self
                .prepared
                .as_ref()
                .filter(|patch| patch.key == cache_key)
                .cloned()
            {
                self.check_prepared(&patch, control)?;
                if prepare {
                    config.check_documents()?;
                    return Ok(json!({
                        "ready":true,"reused":true,"cache_id":patch.key,"prepared_path":patch.output
                    }));
                }
                let _document_locks = config.lock_checked_documents()?;
                if Self::preparation_key(&region::locate(game)?, &original, config)? != cache_key {
                    return Err(invalid("游戏资源或底稿已变化，请重新预览。"));
                }
                control.check()?;
                let mut result = self.publish_patch(
                    &PatchJob {
                        game,
                        data,
                        backup_root,
                        output_root,
                        config,
                        install,
                        expected,
                    },
                    &live,
                    &patch,
                    control,
                )?;
                result["prepared_reused"] = json!(true);
                if result["installed"] == true {
                    self.invalidate_prepared();
                }
                return Ok(result);
            }
            if mode == PatchMode::ApplyPrepared {
                return Err(invalid(
                    "补丁尚未准备完成或已失效，请重新预览并等待准备完成。",
                ));
            }
        }
        let stage_root = config
            .work_dir
            .clone()
            .unwrap_or_else(|| data.join(if prepare { "work/prepared" } else { "work" }));
        fs::create_dir_all(&stage_root)?;
        let scratch;
        let stage = if prepare {
            self.invalidate_prepared();
            if self.preparation_stage.as_ref().is_some_and(|stage| {
                stage.path().parent() != Some(stage_root.as_path()) || !stage.path().is_dir()
            }) {
                self.preparation_stage = None;
            }
            if self.preparation_stage.is_none() {
                self.preparation_stage = Some(
                    tempfile::Builder::new()
                        .prefix("patch-")
                        .tempdir_in(&stage_root)?,
                );
            }
            self.preparation_stage
                .as_ref()
                .unwrap()
                .path()
                .to_path_buf()
        } else {
            scratch = tempfile::Builder::new()
                .prefix("patch-")
                .tempdir_in(&stage_root)?;
            scratch.path().to_path_buf()
        };
        let started = std::time::Instant::now();
        let result = (|| -> Result<Value> {
            let plain = stage.join("working.db");
            control.log("正在从初始底稿重建…")?;
            let key = codec::key(region::KEY)?;
            let cached =
                if self.source_key.as_ref() == Some(&(original.bundle.clone(), source_stat)) {
                    self.session
                        .database
                        .as_ref()
                        .unwrap()
                        .copy_decrypted(&plain)?
                } else {
                    None
                };
            let decrypted;
            let info = if let Some(info) = cached {
                control.check()?;
                info
            } else {
                decrypted = codec::decrypt(&original.bundle, &plain, &key, control)?;
                &decrypted
            };
            let options = config.options(&storage::identity(&live.dir)?, &original.version)?;
            let rules = config.compiled_rules()?;
            let stats = serde_json::to_value(protocol::patch_file(
                &plain, &rules, &options, true, control,
            )?)?;
            let connection = rusqlite::Connection::open_with_flags(
                &plain,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let integrity: String =
                connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            drop(connection);
            if integrity != "ok" {
                return Err(invalid("生成数据库完整性校验失败，未发布补丁。"));
            }
            let encrypted = stage.join("encrypted.db");
            let salt = B64
                .decode(&info.salt)
                .map_err(|_| invalid("底稿 salt 无效。"))?;
            let ivs = info
                .ivs
                .iter()
                .map(|v| B64.decode(v).map_err(|_| invalid("底稿 IV 无效。")))
                .collect::<Result<Vec<_>>>()?;
            control.log("正在加密并校验 HMAC…")?;
            let (_, checked) =
                codec::encrypt_checked(&plain, &encrypted, &key, &salt, &ivs, control)?;
            if checked.ok == 0 || checked.bad > 0 {
                return Err(invalid("生成资源 HMAC 校验失败，未发布补丁。"));
            }
            fs::remove_file(&plain)?;
            let _document_locks = config.lock_checked_documents()?;
            if fs::read(&live.catalog)? != live.bytes
                || region::version(&live.catalog)? != original.version
                || region::resource_identity(&region::locate(game)?)? != identity
                || baseline::stamp(&original.bundle)? != source_stat
            {
                return Err(invalid(
                    "处理期间游戏资源或底稿发生变化，未发布补丁，请重新预览。",
                ));
            }
            let prefix = original
                .bundle
                .file_name()
                .unwrap()
                .to_string_lossy()
                .rsplit_once('_')
                .ok_or_else(|| invalid("底稿文件名无效。"))?
                .0
                .to_string();
            let name = format!("{prefix}_{}", checked.crc);
            let size = encrypted.metadata()?.len();
            // Cancellation is checked before this commit point; publication then finishes.
            control.check()?;
            let commit_control = Control::new(&|_| true);
            let mut patch = PreparedPatch {
                key: cache_key.clone(),
                output: encrypted,
                size,
                crc: checked.crc,
                stats,
                seconds: started.elapsed().as_secs_f64(),
            };
            if prepare {
                let output = stage.join(&name);
                fs::rename(&patch.output, &output)?;
                patch.output = output;
                let result = json!({
                    "ready":true,"reused":false,"cache_id":patch.key,"prepared_path":patch.output
                });
                self.prepared = Some(patch);
                return Ok(result);
            }
            self.publish_patch_named(
                &PatchJob {
                    game,
                    data,
                    backup_root,
                    output_root,
                    config,
                    install,
                    expected,
                },
                &live,
                &patch,
                &name,
                &commit_control,
            )
        })();
        if prepare
            && result.is_err()
            && let Some(stage) = self.preparation_stage.take()
        {
            let path = stage.path().to_path_buf();
            if let Err(error) = stage.close() {
                let _ = control.warn(&format!("临时目录清理失败 {}：{error}", path.display()));
            }
        }
        result
    }
    fn publish_patch(
        &mut self,
        job: &PatchJob<'_>,
        live: &region::Game,
        patch: &PreparedPatch,
        control: &Control<'_>,
    ) -> Result<Value> {
        let name = patch.output.file_name().unwrap().to_string_lossy();
        self.publish_patch_named(job, live, patch, &name, control)
    }
    fn publish_patch_named(
        &mut self,
        job: &PatchJob<'_>,
        live: &region::Game,
        patch: &PreparedPatch,
        name: &str,
        control: &Control<'_>,
    ) -> Result<Value> {
        let PatchJob {
            game,
            data,
            output_root,
            config,
            install,
            ..
        } = job;
        // This is the same final publication path for fresh and prepared patches.
        // Validation and cancellation happen before entry; publication then finishes.
        let commit_control = Control::new(&|_| true);
        let entry = region::Entry {
            size_offset: 0,
            crc_offset: 0,
            size: patch.size,
            crc: patch.crc,
        };
        let mut install_error = None;
        let target = if *install {
            let target = live.dir.join(region::BUNDLES).join(name);
            let mut published = false;
            let installed = (|| -> Result<()> {
                published = publish_resource(
                    &mut self.baselines,
                    &patch.output,
                    &target,
                    patch.size,
                    patch.crc,
                    &commit_control,
                )?;
                region::publish_catalog(live, patch.size, patch.crc)
            })();
            match installed {
                Ok(()) => {
                    if let Err(e) = record(game, data, config, "applied", None) {
                        let _ = control.warn(&format!("游戏已应用；状态记录未保存：{e}"));
                    }
                    target
                }
                Err(error) => {
                    if published
                        && target.is_file()
                        && fs::read(&live.catalog).is_ok_and(|bytes| bytes == live.bytes)
                        && let Err(cleanup) = fs::remove_file(&target)
                    {
                        let _ = control.warn(&format!(
                            "未启用的资源副本暂未移出 {}：{cleanup}",
                            target.display()
                        ));
                    }
                    // Resource changes and failed verification still block publication.
                    // Only filesystem errors while installing fall back to a saved patch.
                    if !matches!(error, crate::Error::Io(_)) {
                        return Err(error);
                    }
                    let _ = control.warn(&format!("安装补丁失败：{error}"));
                    let reason = crate::diagnostics::user_message("apply", &error);
                    let target = match save_patch(
                        &mut self.baselines,
                        &patch.output,
                        output_root,
                        live,
                        name,
                        &entry,
                        true,
                        &commit_control,
                    ) {
                        Ok(target) => target,
                        Err(save_error) => {
                            let saved = save_patch(
                                &mut self.baselines,
                                &patch.output,
                                &data.join("work/recovery"),
                                live,
                                name,
                                &entry,
                                true,
                                &commit_control,
                            );
                            let recovery = match saved {
                                Ok(path) => {
                                    format!("已校验的补丁保留在暂存恢复目录 {}", path.display())
                                }
                                Err(error) => format!("暂存恢复目录写入失败：{error}"),
                            };
                            return Err(invalid(format!(
                                "应用失败：{reason}\n补丁保存失败：{}\n{recovery}",
                                crate::diagnostics::user_message("export_text", &save_error)
                            )));
                        }
                    };
                    install_error = Some(reason);
                    target
                }
            }
        } else {
            save_patch(
                &mut self.baselines,
                &patch.output,
                output_root,
                live,
                name,
                &entry,
                false,
                &commit_control,
            )?
        };
        Ok(
            json!({"output":target,"stats":patch.stats,"verified":true,"installed":*install && install_error.is_none(),"install_error":install_error,"processing_seconds":patch.seconds}),
        )
    }
    pub fn restore(
        &mut self,
        game: &str,
        data: &Path,
        root: &Path,
        control: &Control<'_>,
    ) -> Result<Baseline> {
        let live = region::locate(game)?;
        let _lock = Lock::acquire(&live.catalog)?;
        let identity = region::resource_identity(&live)?;
        let original = self.baselines.resolve(&live, root, control)?;
        self.close();
        let entry = region::parse(&fs::read(&original.catalog)?)?;
        self.baselines.checked(&original.bundle, &entry, control)?;
        let target = live
            .dir
            .join(region::BUNDLES)
            .join(original.bundle.file_name().unwrap());
        if fs::read(&live.catalog)? != live.bytes
            || region::version(&live.catalog)? != original.version
            || region::resource_identity(&region::locate(game)?)? != identity
        {
            return Err(invalid("恢复期间游戏资源发生变化，请重试。"));
        }
        control.check()?;
        let published = publish_resource(
            &mut self.baselines,
            &original.bundle,
            &target,
            entry.size,
            entry.crc,
            &Control::new(&|_| true),
        )?;
        if let Err(error) = region::publish_catalog(&live, entry.size, entry.crc) {
            if published
                && fs::read(&live.catalog).is_ok_and(|bytes| bytes == live.bytes)
                && let Err(cleanup) = fs::remove_file(&target)
            {
                let _ = control.warn(&format!(
                    "未启用的资源副本暂未移出 {}：{cleanup}",
                    target.display()
                ));
            }
            return Err(error);
        }
        if let Err(e) = record(game, data, &Config::new(vec![], vec![]), "restored", None) {
            let _ = control.warn(&format!("底稿已恢复；状态记录未保存：{e}"));
        }
        Ok(original)
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.invalidate_prepared();
    }
}
fn memory(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len() + 32,
        Value::Array(a) => a.iter().map(memory).sum::<usize>() + 32 * a.len(),
        Value::Object(o) => o.iter().map(|(k, v)| k.len() + memory(v) + 64).sum(),
        _ => 32,
    }
}
fn remember(cache: &mut VecDeque<(String, Value)>, key: String, value: Value, limit: usize) {
    const CAP: usize = 12 * 1024 * 1024;
    let size = memory(&value) + key.len();
    if size > CAP {
        return;
    }
    while cache.len() >= limit
        || cache
            .iter()
            .map(|(k, v)| k.len() + memory(v))
            .sum::<usize>()
            + size
            > CAP
    {
        cache.pop_front();
    }
    cache.push_back((key, value));
}
#[allow(clippy::too_many_arguments)]
fn save_patch(
    checks: &mut Baselines,
    source: &Path,
    output_root: &Path,
    game: &region::Game,
    name: &str,
    entry: &region::Entry,
    automatic: bool,
    control: &Control<'_>,
) -> Result<PathBuf> {
    let mut catalog = game.bytes.clone();
    catalog[game.entry.size_offset..game.entry.size_offset + 8]
        .copy_from_slice(&(entry.size as i64).to_le_bytes());
    catalog[game.entry.crc_offset..game.entry.crc_offset + 8]
        .copy_from_slice(&(entry.crc as i64).to_le_bytes());
    let marker = game.catalog.with_file_name("TableCatalog.hash");
    let marker = match fs::read(marker) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let content = storage::file_digest(source)?;
    let id = storage::serialized_digest(&(name, &content, storage::digest(&catalog), &marker))?;
    let root = if automatic {
        let game_id = storage::digest(storage::identity(&game.dir)?.as_bytes());
        output_root.join("automatic").join(game_id)
    } else {
        output_root.to_path_buf()
    };
    let _lock = Lock::acquire(&root.join(".packages"))?;
    fs::create_dir_all(&root)?;
    let folder = root.join(format!("patch-{id}"));
    let target = folder.join(name);
    if folder.exists() {
        if !storage::is_plain_directory(&folder)?
            || storage::file_digest(&target)? != content
            || fs::read(folder.join("TableCatalog.bytes"))? != catalog
            || storage::revision(&folder.join("TableCatalog.hash"))?
                != marker.as_deref().map(storage::digest)
        {
            return Err(invalid("同名补丁内容已变化，请选择其他输出目录。"));
        }
    } else {
        let stage = tempfile::Builder::new()
            .prefix("pending-")
            .tempdir_in(&root)?;
        publish_resource(
            checks,
            source,
            &stage.path().join(name),
            entry.size,
            entry.crc,
            control,
        )?;
        storage::write_locked(&stage.path().join("TableCatalog.bytes"), &catalog)?;
        if let Some(marker) = marker {
            storage::write_locked(&stage.path().join("TableCatalog.hash"), &marker)?;
        }
        if automatic {
            crate::cleanup::mark_managed(stage.path())?;
        }
        fs::rename(stage.path(), &folder)?;
    }
    if automatic {
        crate::cleanup::retain_recent(&root, "patch-", 2, |key| key == id, control);
    }
    Ok(target)
}

fn publish_resource(
    checks: &mut Baselines,
    source: &Path,
    target: &Path,
    size: u64,
    crc: u32,
    control: &Control<'_>,
) -> Result<bool> {
    let entry = region::Entry {
        size_offset: 0,
        crc_offset: 0,
        size,
        crc,
    };
    if target.exists() {
        checks.checked(target, &entry, control)?;
        return Ok(false);
    }
    let pending = storage::temporary_path(target)?;
    baseline::copy_sync(source, &pending)?;
    // Verify the bytes that reached the destination, including restore copies.
    // The encryption-time digest alone cannot detect a damaged copied file.
    checks.checked(&pending, &entry, control)?;
    // All cooperating writers hold the catalog lock. Never replace an unrelated resource.
    if target.exists() {
        return Err(invalid("目标资源在发布前出现，请重新预览。"));
    }
    fs::rename(&pending, target)?;
    Ok(true)
}
pub fn total(stats: &Value) -> u64 {
    ["added", "updated", "reverted"]
        .iter()
        .map(|k| stats[k].as_u64().unwrap_or(0))
        .sum()
}

pub fn record(
    game: &str,
    data: &Path,
    config: &Config,
    kind: &str,
    expected: Option<&Value>,
) -> Result<()> {
    let live = region::locate(game)?;
    let identity = region::resource_identity(&live)?;
    if expected.is_some_and(|v| v != &identity) {
        return Err(invalid("游戏资源已变化，请重新预览。"));
    }
    let key = storage::identity(&live.dir)?;
    let edits = config
        .edits
        .iter()
        .filter(|e| e.game.as_ref() == key)
        .cloned()
        .collect::<Vec<_>>();
    let path = data.join(".gui/application-state.json");
    let _lock = Lock::acquire(&path)?;
    let mut values = read_receipts(&path)?;
    values.as_object_mut().unwrap().retain(|stored_game, receipt| {
        // Applied receipts protect against adopting our own patch as a new baseline.
        // Only discard restored entries whose directory is missing on an online volume.
        if stored_game == &key || receipt["kind"] != "restored" {
            return true;
        }
        let folder = Path::new(stored_game);
        !folder.is_absolute()
            || !matches!(fs::metadata(folder), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            || !folder.ancestors().last().is_some_and(|root| root.is_dir())
    });
    values[&key] = json!({"identity":identity,"fingerprint":model::fingerprint(&config.rules,&edits)?,"kind":kind});
    storage::write_locked(
        &path,
        &serde_json::to_vec(&json!({"format":2,"games":values}))?,
    )?;
    Ok(())
}
pub(crate) fn parse_receipts(bytes: &[u8]) -> Result<Value> {
    let parsed = (|| -> Result<Value> {
        let value: Value = serde_json::from_slice(bytes)?;
        if value["format"] != 2
            || !value["games"].is_object()
            || value["games"].as_object().unwrap().values().any(|receipt| {
                let identity = receipt["identity"].as_array();
                !receipt.is_object()
                    || !matches!(
                        receipt["kind"].as_str(),
                        Some("applied" | "restored" | "checked")
                    )
                    || identity.is_none_or(|identity| {
                        identity.len() != 6
                            || !identity[0].is_string()
                            || identity[1..5].iter().any(|part| part.as_u64().is_none())
                            || !identity[5].is_string()
                    })
                    || !receipt["fingerprint"].is_string()
            })
        {
            return Err(invalid("应用状态记录格式无效。"));
        }
        Ok(value["games"].clone())
    })();
    parsed.map_err(|source| crate::Error::Context { message:"应用状态文件损坏或格式不受支持，原文件已保留。请检查数据目录中的 application-state.json，修复后重新打开工具。".into(),source:Box::new(source) })
}
fn read_receipts(path: &Path) -> Result<Value> {
    match fs::read(path) {
        Ok(bytes) => parse_receipts(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(crate::Error::Context {
            message: format!(
                "应用状态文件无法读取。请检查 {} 的路径和访问权限，原文件未改动。",
                path.display()
            ),
            source: Box::new(error.into()),
        }),
    }
}
pub fn status(game: &str, data: &Path, config: &Config) -> Result<String> {
    let live = region::locate(game)?;
    let key = storage::identity(&live.dir)?;
    let receipts = read_receipts(&data.join(".gui/application-state.json"))?;
    let receipt = &receipts[&key];
    if !receipt.is_object() {
        return Ok("应用状态待核对，先预览".into());
    }
    if receipt["identity"] != region::resource_identity(&live)? {
        return Ok("游戏资源已变化，请重新预览".into());
    }
    let edits = config
        .edits
        .iter()
        .filter(|e| e.game.as_ref() == key)
        .cloned()
        .collect::<Vec<_>>();
    Ok(if receipt["kind"] == "restored" {
        if config.rules.is_empty() && edits.is_empty() {
            "已恢复底稿，与当前空词表一致"
        } else {
            "已恢复底稿；词表尚未应用"
        }
    } else if receipt["fingerprint"] != model::fingerprint(&config.rules, &edits)? {
        "词表已保存，尚未应用"
    } else if receipt["kind"] == "applied" {
        "已应用，与当前词表一致"
    } else {
        "当前游戏文字与词表一致"
    }
    .into())
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn failed_resource_verification_removes_only_its_pending_copy() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("resource");
        let unrelated = root.path().join("unrelated.pending-keep");
        fs::write(&source, vec![0; 8192]).unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        let control = Control::new(&|_| true);
        assert!(
            publish_resource(
                &mut Baselines::default(),
                &source,
                &target,
                8192,
                1,
                &control
            )
            .is_err()
        );
        assert!(!target.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    }
}
