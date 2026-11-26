use crate::{Control, Result, invalid, model, sources::Sources, storage};
use bajptr_core::rules::Rule;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

pub const FILES: [&str; 5] = [
    "rules.tsv",
    "text-edits.json",
    "settings.json",
    "source-history.json",
    "template-drafts.json",
];

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    format: u32,
    kind: String,
    files: BTreeMap<String, String>,
}
pub struct Decoded {
    pub rules: Vec<Rule>,
    pub edits: Vec<model::Edit>,
    pub settings: Value,
    pub sources: Sources,
}
pub struct Snapshot {
    pub files: BTreeMap<String, Option<Vec<u8>>>,
}
pub fn settings(bytes: &[u8]) -> Result<Value> {
    let mut value: Value = serde_json::from_slice(bytes)?;
    if value["format"] != 2 || value.get("game").is_some_and(|game| !game.is_string()) {
        return Err(invalid("设置文件格式无效。"));
    }
    if let Some(sort) = value.get("replacement_sort") {
        serde_json::from_value::<crate::ordering::Sort>(sort.clone())?;
    }
    if let Some(times) = value.get("replacement_times") {
        serde_json::from_value::<crate::ordering::Timeline>(times.clone())?;
    }
    if let Some(settings) = value.as_object_mut() {
        settings.remove("skip_kana");
    }
    Ok(value)
}
pub fn capture(root: &Path, control: &Control<'_>) -> Result<Snapshot> {
    let _locks = FILES
        .iter()
        .map(|name| storage::Lock::acquire(&root.join(name)))
        .collect::<Result<Vec<_>>>()?;
    let mut files = BTreeMap::new();
    for name in FILES {
        control.check()?;
        let bytes = match fs::read(root.join(name)) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        files.insert(name.into(), bytes);
    }
    Ok(Snapshot { files })
}
impl Snapshot {
    pub fn revisions(&self) -> HashMap<String, Option<String>> {
        self.files
            .iter()
            .map(|(name, bytes)| (name.clone(), bytes.as_deref().map(storage::digest)))
            .collect()
    }
    pub fn bundle(&self) -> Result<Bundle> {
        let mut files = BTreeMap::new();
        for name in FILES {
            let bytes = match self.files[name].as_ref() {
                Some(bytes) => bytes.clone(),
                None => match name {
                    "rules.tsv" => vec![],
                    "text-edits.json" => model::edits_bytes(&[])?,
                    "settings.json" => serde_json::to_vec(&json!({"format":2,"game":""}))?,
                    "source-history.json" => serde_json::to_vec(&Sources::default())?,
                    _ => serde_json::to_vec(&json!({"format":2,"drafts":{}}))?,
                },
            };
            files.insert(
                name.into(),
                String::from_utf8(bytes)
                    .map_err(|_| invalid(format!("{name} 不是有效 UTF-8 文本，无法创建备份。")))?,
            );
        }
        let mut result = Bundle {
            format: 1,
            kind: "bajptr-workspace-backup".into(),
            files,
        };
        let decoded = result.decode().map_err(|source| crate::Error::Context {
            message: "工作区记录或设置无效，未创建备份。请修复后重试，详细原因见日志。".into(),
            source: Box::new(source),
        })?;
        result.files.insert(
            "settings.json".into(),
            serde_json::to_string(&decoded.settings)?,
        );
        Ok(result)
    }
    pub(crate) fn ordered_revisions(&self) -> BTreeMap<&str, Option<String>> {
        self.files
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_deref().map(storage::digest)))
            .collect()
    }
    pub fn archive(&self, root: &Path, control: &Control<'_>) -> Result<PathBuf> {
        let id = storage::serialized_digest(&self.ordered_revisions())?;
        let edits = self.files["text-edits.json"]
            .as_deref()
            .and_then(|bytes| model::parse_edits(bytes).ok())
            .unwrap_or_default();
        self.archive_as(
            root,
            "before-backup-restore-",
            &id,
            json!({"kind":"workspace-recovery","migration_snapshots":migration_references(&edits)}),
            control,
        )
    }

    pub(crate) fn archive_as(
        &self,
        root: &Path,
        prefix: &str,
        digest: &str,
        mut info: Value,
        control: &Control<'_>,
    ) -> Result<PathBuf> {
        control.check()?;
        let recovery = root.join("recovery");
        let _lock = storage::Lock::acquire(&recovery.join(".snapshots"))?;
        fs::create_dir_all(&recovery)?;
        let target = recovery.join(format!("{prefix}{digest}"));
        info["format"] = json!(1);
        info["files"] = json!(self.files.iter().map(|(name, bytes)| {
            json!({"name":name,"existed":bytes.is_some(),"digest":bytes.as_ref().map(|bytes|storage::digest(bytes))})
        }).collect::<Vec<_>>());
        if target.exists() {
            if !storage::is_plain_directory(&target)?
                || serde_json::from_slice::<Value>(&fs::read(target.join("info.json"))?)? != info
            {
                return Err(invalid("同名恢复备份已变化，未覆盖。"));
            }
            for (name, bytes) in &self.files {
                if storage::revision(&target.join(name))? != bytes.as_deref().map(storage::digest) {
                    return Err(invalid("同名恢复备份已变化，未覆盖。"));
                }
            }
            return Ok(target);
        }
        let staging = tempfile::Builder::new()
            .prefix("pending-")
            .tempdir_in(&recovery)?;
        for (name, bytes) in &self.files {
            if let Some(bytes) = bytes {
                storage::write_locked(&staging.path().join(name), bytes)?;
            }
        }
        storage::write_locked(
            &staging.path().join("info.json"),
            &serde_json::to_vec_pretty(&info)?,
        )?;
        crate::cleanup::mark_managed(staging.path())?;
        fs::rename(staging.path(), &target)?;
        Ok(target)
    }

    fn read_archive(path: &Path, info: &Value) -> Result<Self> {
        let invalid_archive = || invalid("恢复记录格式无效，或有文件与备份时不一致。");
        let files = info["files"]
            .as_array()
            .filter(|files| files.len() == FILES.len())
            .ok_or_else(invalid_archive)?;
        if info["format"] != 1 {
            return Err(invalid_archive());
        }
        let parent = path.parent().ok_or_else(invalid_archive)?;
        let mut snapshot = BTreeMap::new();
        for entry in files {
            let name = entry["name"]
                .as_str()
                .filter(|name| FILES.contains(name))
                .ok_or_else(invalid_archive)?;
            let bytes = if entry["existed"] == true {
                let bytes = fs::read(parent.join(name))?;
                if entry["digest"] != storage::digest(&bytes) {
                    return Err(invalid_archive());
                }
                Some(bytes)
            } else if entry["existed"] == false && entry["digest"].is_null() {
                None
            } else {
                return Err(invalid_archive());
            };
            if snapshot.insert(name.to_owned(), bytes).is_some() {
                return Err(invalid_archive());
            }
        }
        Ok(Self { files: snapshot })
    }
}

pub(crate) fn migration_references(edits: &[model::Edit]) -> std::collections::BTreeSet<&str> {
    edits
        .iter()
        .filter_map(|edit| edit.batch_operation.as_ref()?.previous_scope.as_ref())
        .map(|scope| scope.snapshot.as_str())
        .collect()
}

pub(crate) fn archived_migration_references(
    root: &Path,
) -> Result<std::collections::HashSet<String>> {
    let mut references = std::collections::HashSet::new();
    for entry in fs::read_dir(root.join("recovery"))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if ["before-backup-restore-", "batch-promotion-"]
            .iter()
            .any(|prefix| name.strip_prefix(prefix).is_some_and(model::is_version))
            && storage::is_plain_directory(&entry.path())?
        {
            let info: Value = serde_json::from_slice(&fs::read(entry.path().join("info.json"))?)?;
            if let Some(snapshots) = info["migration_snapshots"].as_array() {
                references.extend(
                    snapshots
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
            }
        }
    }
    Ok(references)
}

pub(crate) fn retain_recent(
    root: &Path,
    prefix: &str,
    protected: impl Fn(&str) -> bool,
    control: &Control<'_>,
) {
    let root = root.join("recovery");
    match storage::Lock::acquire(&root.join(".snapshots")) {
        Ok(_lock) => crate::cleanup::retain_recent(&root, prefix, 4, protected, control),
        Err(error) => {
            let _ = control.warn(&format!("自动备份清理未完成：{error}"));
        }
    }
}

impl Bundle {
    pub fn read(path: &Path) -> Result<Self> {
        #[derive(Deserialize)]
        struct Header<'a> {
            #[serde(borrow)]
            kind: std::borrow::Cow<'a, str>,
        }
        let bytes = fs::read(path)?;
        let header: Header<'_> = serde_json::from_slice(&bytes)?;
        if header.kind == "workspace-recovery" {
            let value: Value = serde_json::from_slice(&bytes)?;
            return Snapshot::read_archive(path, &value)?.bundle();
        }
        let value: Self = serde_json::from_slice(&bytes)?;
        value.decode()?;
        Ok(value)
    }
    pub fn documents(&self) -> BTreeMap<String, Vec<u8>> {
        self.files
            .iter()
            .map(|(name, text)| (name.clone(), text.as_bytes().to_vec()))
            .collect()
    }
    pub fn decode(&self) -> Result<Decoded> {
        if self.format != 1
            || self.kind != "bajptr-workspace-backup"
            || self.files.len() != FILES.len()
            || FILES.iter().any(|name| !self.files.contains_key(*name))
        {
            return Err(invalid("这不是完整的 BA-JPTR 备份，或备份版本不受支持。"));
        }
        let check = || -> Result<Decoded> {
            let rules = model::parse_rules(self.files["rules.tsv"].as_bytes())?;
            let edits = model::parse_edits(self.files["text-edits.json"].as_bytes())?;
            let settings = settings(self.files["settings.json"].as_bytes())?;
            let sources = Sources::parse(self.files["source-history.json"].as_bytes())?;
            crate::templates::parse_document(self.files["template-drafts.json"].as_bytes())?;
            Ok(Decoded {
                rules,
                edits,
                settings,
                sources,
            })
        };
        check().map_err(|source| crate::Error::Context {
            message: "备份中的记录或设置无效，未恢复。请检查备份文件，详细原因见日志。".into(),
            source: Box::new(source),
        })
    }
    pub fn export(&self, path: &Path, data: &Path, control: &Control<'_>) -> Result<()> {
        let reject = || invalid("工具数据目录中的 .gui、backup 和 work 受保护。请选择其他位置。");
        let protected = [".gui", "backup", "work"]
            .map(|name| storage::identity(&data.join(name)))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let absolute = std::path::absolute(path)?;
        let target = storage::resolved(&absolute)?;
        for ancestor in std::iter::once(target.as_path()).chain(absolute.ancestors()) {
            let identity = storage::identity(ancestor)?;
            if protected
                .iter()
                .any(|folder| Path::new(&identity).starts_with(folder))
            {
                return Err(reject());
            }
        }
        self.write(&target, control)
    }

    fn write(&self, path: &Path, control: &Control<'_>) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        control.check()?;
        storage::atomic_write(path, &bytes, None)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_restore_snapshots_share_storage_and_distinct_snapshots_rotate() {
        let root = tempfile::tempdir().unwrap();
        let control = Control::new(&|_| true);
        let mut last = None;
        for index in 0..7 {
            let mut files = FILES
                .into_iter()
                .map(|name| (name.to_owned(), None))
                .collect::<BTreeMap<_, _>>();
            files.insert(
                "rules.tsv".into(),
                Some(format!("任务\t委托{index}\tsub\t\t\n").into_bytes()),
            );
            let snapshot = Snapshot { files };
            let archived = snapshot.archive(root.path(), &control).unwrap();
            assert_eq!(snapshot.archive(root.path(), &control).unwrap(), archived);
            let current = archived.file_name().unwrap().to_string_lossy();
            retain_recent(
                root.path(),
                "before-backup-restore-",
                |id| current.ends_with(id),
                &control,
            );
            last = Some(archived);
        }
        assert_eq!(
            fs::read_dir(root.path().join("recovery")).unwrap().count(),
            5
        );
        let latest = last.unwrap();
        assert!(Bundle::read(&latest.join("info.json")).is_ok());
    }
}
