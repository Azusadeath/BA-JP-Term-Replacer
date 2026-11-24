use crate::{
    Control, Result, invalid, model,
    region::{self, Entry, Game},
    storage,
};
use bajptr_core::codec;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone)]
pub struct Baseline {
    pub bundle: PathBuf,
    pub catalog: PathBuf,
    pub version: String,
    pub origin: String,
}
pub(crate) type Stamp = (u64, SystemTime, SystemTime);
#[derive(Default)]
pub struct Baselines {
    verified: HashMap<PathBuf, (Stamp, u32)>,
}
pub fn stamp(path: &Path) -> Result<Stamp> {
    let m = path.metadata()?;
    Ok((
        m.len(),
        m.modified()?,
        m.created().unwrap_or(SystemTime::UNIX_EPOCH),
    ))
}
pub fn location(game: &Game, root: &Path) -> Result<PathBuf> {
    active_folder(&version_folder(game, root)?)
}

pub(crate) fn version_folder(game: &Game, root: &Path) -> Result<PathBuf> {
    let version = region::version(&game.catalog)?;
    version_folder_for(game, root, &version)
}

fn version_folder_for(game: &Game, root: &Path, version: &str) -> Result<PathBuf> {
    let game_id = storage::digest(storage::identity(&game.dir)?.as_bytes());
    Ok(root.join("_baselines").join(&game_id[..24]).join(version))
}

const ACTIVE: &str = ".active";
const GENERATION: &str = "generation-";

fn is_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

fn legacy_version(name: &str) -> Option<&str> {
    let (version, suffix) = name.split_once('.')?;
    let (kind, id) = suffix.split_once('-')?;
    (model::is_version(version) && matches!(kind, "superseded" | "damaged") && is_id(id))
        .then_some(version)
}

fn active_folder(destination: &Path) -> Result<PathBuf> {
    match fs::read_to_string(destination.join(ACTIVE)) {
        Ok(name) => {
            if name.strip_prefix(GENERATION).is_none_or(|id| !is_id(id)) {
                return Err(invalid("当前底稿指针无效，请检查 backup/_baselines。"));
            }
            let folder = destination.join(name);
            if !folder.is_dir() {
                return Err(invalid(format!("当前底稿目录丢失：{}", folder.display())));
            }
            return Ok(folder);
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if destination.join("baseline.json").is_file() {
        return Ok(destination.into());
    }

    // Older builds could leave the published directory under either rollback name.
    let entries = match fs::read_dir(destination.parent().unwrap()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(destination.into()),
        Err(error) => return Err(error.into()),
    };
    let version = destination.file_name().and_then(|name| name.to_str());
    let mut previous = None;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir()
            && name
                .to_str()
                .and_then(legacy_version)
                .is_some_and(|v| Some(v) == version)
            && previous.replace(entry.path()).is_some()
        {
            return Err(invalid(format!(
                "发现多个旧底稿副本，无法自动确定原底稿。请检查 {}。",
                destination.parent().unwrap().display()
            )));
        }
    }
    Ok(previous.unwrap_or_else(|| destination.into()))
}

pub(crate) fn has_previous(destination: &Path) -> bool {
    fs::read_dir(destination.parent().unwrap()).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            (model::is_version(&name) || legacy_version(&name).is_some())
                && (entry.path().join("baseline.json").is_file()
                    || entry.path().join(ACTIVE).is_file())
        })
    })
}

pub(crate) fn new_generation(destination: &Path) -> Result<PathBuf> {
    fs::create_dir_all(destination)?;
    let stage = destination.join(format!("{GENERATION}{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&stage)?;
    Ok(stage)
}

pub(crate) fn publish_generation(destination: &Path, stage: &Path) -> Result<()> {
    let name = stage.file_name().unwrap().to_str().unwrap();
    storage::write_locked(&destination.join(ACTIVE), name.as_bytes()).map_err(|source| {
        crate::Error::Context {
            message: format!(
                "{}\n已校验的新副本保留在 {}。",
                crate::diagnostics::user_message("baseline_commit", &source),
                stage.display()
            ),
            source: Box::new(source),
        }
    })?;
    Ok(())
}
impl Baselines {
    pub fn checked(&mut self, path: &Path, entry: &Entry, control: &Control<'_>) -> Result<()> {
        let stat = stamp(path)?;
        if stat.0 != entry.size {
            return Err(invalid("初始底稿或资源大小不符，未改动游戏。"));
        }
        let crc = if let Some((cached, crc)) = self.verified.get(path).filter(|(s, _)| *s == stat) {
            let _ = cached;
            *crc
        } else {
            let inspected = codec::inspect(path, &codec::key(region::KEY)?, control)?;
            if inspected.ok == 0 || inspected.bad > 0 {
                return Err(invalid("数据库 HMAC 校验失败，未接受此资源。"));
            }
            if stamp(path)? != stat {
                return Err(invalid("数据库在校验期间发生变化，请重试。"));
            }
            self.remember_checked(path, stat, inspected.crc);
            inspected.crc
        };
        if crc != entry.crc {
            return Err(invalid("数据库 CRC 校验失败，未改动游戏。"));
        }
        Ok(())
    }
    pub(crate) fn read_saved(
        &mut self,
        game: &Game,
        root: &Path,
        version: &str,
        control: &Control<'_>,
    ) -> Result<Option<Baseline>> {
        if !model::is_version(version) {
            return Err(invalid("旧底稿版本无效。"));
        }
        let Some((baseline, entry)) = Self::saved_entry(game, root, version)? else {
            return Ok(None);
        };
        self.checked(&baseline.bundle, &entry, control)?;
        Ok(Some(baseline))
    }

    pub(crate) fn saved_entry(
        game: &Game,
        root: &Path,
        version: &str,
    ) -> Result<Option<(Baseline, Entry)>> {
        let destination = active_folder(&version_folder_for(game, root, version)?)?;
        let record_path = destination.join("baseline.json");
        if !record_path.is_file() {
            return Ok(None);
        }
        let record: Value = serde_json::from_slice(&fs::read(&record_path)?)?;
        let name = record["db_file"]
            .as_str()
            .ok_or_else(|| invalid("底稿数据库记录无效。"))?;
        if record["format"] != 2
            || !storage::safe_name(name)
            || record["version"] != version
            || storage::identity(Path::new(record["game_dir"].as_str().unwrap_or("")))?
                != storage::identity(&game.dir)?
        {
            return Err(invalid("底稿记录与当前游戏不匹配。"));
        }
        let catalog = destination.join("TableCatalog.bytes");
        if region::version(&catalog)? != version {
            return Err(invalid("底稿资源版本不匹配。"));
        }
        let entry = region::parse(&fs::read(&catalog)?)?;
        if !name.ends_with(&format!("_{}", entry.crc)) {
            return Err(invalid("底稿记录与数据库名称不匹配。"));
        }
        let bundle = destination.join(name);
        Ok(Some((
            Baseline {
                bundle,
                catalog,
                version: version.into(),
                origin: record["origin"].as_str().unwrap_or("").into(),
            },
            entry,
        )))
    }
    // The caller checks the resource against the catalog while decrypting it.
    pub(crate) fn entry_for_open(game: &Game, root: &Path) -> Result<(Baseline, Entry)> {
        let version = region::version(&game.catalog)?;
        if let Some(saved) = Self::saved_entry(game, root, &version)? {
            return Ok(saved);
        }
        Err(Self::missing(location(game, root)?.exists()))
    }

    pub(crate) fn remember_checked(&mut self, path: &Path, stat: Stamp, crc: u32) {
        if self.verified.len() >= 32 {
            self.verified.clear();
        }
        self.verified.insert(path.into(), (stat, crc));
    }

    /// Requires the live catalog lock. Never creates or retires a baseline.
    pub fn resolve(&mut self, game: &Game, root: &Path, control: &Control<'_>) -> Result<Baseline> {
        self.resolve_locked(game, root, control, false, false, &HashSet::new())
    }
    /// Called only after explicit confirmation. The live catalog lock is held by the caller.
    pub fn capture(
        &mut self,
        game: &Game,
        root: &Path,
        replace: bool,
        keep: &HashSet<String>,
        control: &Control<'_>,
    ) -> Result<Baseline> {
        control.check()?;
        self.resolve_locked(game, root, control, true, replace, keep)
    }
    pub(crate) fn missing(incomplete: bool) -> crate::Error {
        invalid(if incomplete {
            "初始底稿目录不完整，未用当前已修改内容代替。请检查 backup/_baselines。"
        } else {
            "尚未确认当前版本的初始底稿，请先检查游戏目录并确认保存底稿。"
        })
    }
    fn resolve_locked(
        &mut self,
        game: &Game,
        root: &Path,
        control: &Control<'_>,
        create: bool,
        replace: bool,
        keep: &HashSet<String>,
    ) -> Result<Baseline> {
        let version = region::version(&game.catalog)?;
        let destination = version_folder_for(game, root, &version)?;
        let previous = active_folder(&destination)?;
        let record_path = previous.join("baseline.json");
        if !replace && let Some(original) = self.read_saved(game, root, &version, control)? {
            return Ok(original);
        }
        if previous.exists() && (!replace || !record_path.is_file()) {
            return Err(Self::missing(true));
        }
        if !create {
            return Err(Self::missing(false));
        }
        let identity = region::resource_identity(game)?;
        self.checked(&game.bundle, &game.entry, control)?;
        control.log("正在保存本版本初始底稿…")?;
        let (source, catalog, origin) = (
            game.bundle.clone(),
            game.catalog.clone(),
            "用户确认的游戏资源".to_string(),
        );
        let entry = region::parse(&fs::read(&catalog)?)?;
        fs::create_dir_all(destination.parent().unwrap())?;
        let stage = if replace {
            new_generation(&destination)?
        } else {
            let stage = destination.with_file_name(format!(
                "{version}.building-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir(&stage)?;
            stage
        };
        let name = source.file_name().unwrap();
        copy_sync(&source, &stage.join(name))?;
        copy_sync(&catalog, &stage.join("TableCatalog.bytes"))?;
        let marker = catalog.with_file_name("TableCatalog.hash");
        if marker.is_file() {
            copy_sync(&marker, &stage.join("TableCatalog.hash"))?;
        }
        self.checked(&stage.join(name), &entry, control)?;
        control.log("正在验证底稿数据库完整性…")?;
        let db = bajptr_core::database::Database::open(
            &stage.join(name),
            true,
            &codec::key(region::KEY)?,
            control,
        )?;
        let integrity: String = db
            .con
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        drop(db);
        if integrity != "ok" {
            return Err(invalid("底稿数据库完整性校验失败，原底稿未替换。"));
        }
        if region::parse(&fs::read(stage.join("TableCatalog.bytes"))?)? != entry
            || region::version(&stage.join("TableCatalog.bytes"))? != version
            || region::version(&game.catalog)? != version
            || fs::read(&game.catalog)? != game.bytes
            || region::resource_identity(&region::locate(
                game.dir.to_str().ok_or_else(|| invalid("游戏路径无效。"))?,
            )?)? != identity
        {
            return Err(invalid(
                "建立底稿期间资源发生变化，请重试。暂存副本已保留。",
            ));
        }
        storage::write_locked(
            &stage.join("baseline.json"),
            &serde_json::to_vec_pretty(
                &json!({"format":2,"id":uuid::Uuid::new_v4().simple().to_string(),"game_dir":game.dir,"version":version,"created":chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),"db_file":name.to_string_lossy(),"origin":origin}),
            )?,
        )?;
        control.check()?;
        let published = if replace {
            publish_generation(&destination, &stage)?;
            // The original layout stores its files in the version directory itself.
            if previous != destination
                && let Err(error) = storage::retire(&previous)
            {
                let _ = control.warn(&format!(
                    "新底稿已保存，旧副本仍保留在 {}：{error}",
                    previous.display()
                ));
            }
            stage
        } else {
            fs::rename(&stage, &destination)?;
            destination.clone()
        };
        let _ = control.log("初始底稿已保存，之后从这份底稿重建。");
        retire_old(&destination, keep, control);
        Ok(Baseline {
            bundle: published.join(name),
            catalog: published.join("TableCatalog.bytes"),
            version,
            origin,
        })
    }
}
pub fn copy_sync(source: &Path, target: &Path) -> Result<()> {
    fs::copy(source, target)?;
    fs::OpenOptions::new()
        .write(true)
        .open(target)?
        .sync_all()?;
    Ok(())
}
fn retire_old(current: &Path, keep: &HashSet<String>, control: &Control<'_>) {
    if let Ok(entries) = fs::read_dir(current.parent().unwrap()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path == current
                || !model::is_version(&entry.file_name().to_string_lossy())
                || keep.contains(entry.file_name().to_string_lossy().as_ref())
                || !(path.join("baseline.json").is_file() || path.join(ACTIVE).is_file())
            {
                continue;
            }
            match storage::retire(&path) {
                Ok(_) => {
                    let _ = control.log("新版底稿已验证，旧版底稿已移出项目。");
                }
                Err(e) => {
                    let _ = control.warn(&e.to_string());
                }
            }
        }
    }
}
pub fn info(original: &Baseline) -> Result<Value> {
    let record: Value =
        serde_json::from_slice(&fs::read(original.catalog.with_file_name("baseline.json"))?)?;
    let marker = fs::read(original.catalog.with_file_name("TableCatalog.hash"))
        .ok()
        .map(|b| {
            String::from_utf8_lossy(&b)
                .trim()
                .chars()
                .take(80)
                .collect::<String>()
        })
        .unwrap_or("当前资源版本".into());
    Ok(
        json!({"version":marker,"created":record["created"].as_str().unwrap_or("").replace('T'," "),"verified":true,"folder":original.bundle.parent(),"resource":original.bundle.file_name().unwrap().to_string_lossy(),"origin":original.origin,"version_id":original.version}),
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn occupied_pointer_leaves_the_previous_generation_readable() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("version");
        let previous = new_generation(&destination).unwrap();
        let newer = new_generation(&destination).unwrap();
        publish_generation(&destination, &previous).unwrap();
        let pointer = destination.join(ACTIVE);
        let pointer_lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&pointer)
            .unwrap();

        let error = publish_generation(&destination, &newer).unwrap_err();
        let crate::Error::Context { source, .. } = error else {
            panic!("missing publication error context");
        };
        assert!(
            matches!(*source, crate::Error::Io(ref io) if matches!(io.raw_os_error(), Some(5 | 32)))
        );
        assert_eq!(active_folder(&destination).unwrap(), previous);
        assert!(newer.is_dir());

        drop(pointer_lock);
        publish_generation(&destination, &newer).unwrap();
        assert_eq!(active_folder(&destination).unwrap(), newer);
    }
}
