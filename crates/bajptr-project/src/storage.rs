//! Cross-instance exclusion, compare-and-swap saves and durable atomic publication.
use crate::{Result, invalid};
use base64::{Engine as _, display::Base64Display, engine::general_purpose::STANDARD as B64};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Component, Path, PathBuf},
};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn file_digest(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    std::io::copy(&mut File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}
pub fn is_plain_directory(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Ok(false);
        }
    }
    Ok(metadata.file_type().is_dir())
}
pub(crate) fn serialized_digest(value: &impl Serialize) -> Result<String> {
    let mut hash = Sha256::new();
    serde_json::to_writer(&mut hash, value)?;
    Ok(format!("{:x}", hash.finalize()))
}
pub fn validate_data_dir(data: &Path) -> Result<()> {
    let resolved = resolved(data)?;
    if resolved.is_file() {
        return Err(invalid(format!(
            "--data-dir 必须是数据目录，收到文件：{}",
            data.display()
        )));
    }
    let is_gui = resolved
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case(".gui"));
    let contains_gui_files = !resolved.join(".gui").is_dir()
        && [
            "settings.json",
            "rules.tsv",
            "text-edits.json",
            "source-history.json",
            "template-drafts.json",
            "application-state.json",
            "tables-cache.json",
        ]
        .iter()
        .any(|name| resolved.join(name).is_file());
    if is_gui || contains_gui_files {
        return Err(invalid(format!(
            "--data-dir 应指向包含 .gui 的父目录。当前目录：{}。请将设置、词表等文件放在数据目录下的 .gui 文件夹中，再传入其父目录。",
            data.display()
        )));
    }
    Ok(())
}
pub fn resolved(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let path = if path.exists() {
        fs::canonicalize(&path)?
    } else {
        match (path.parent(), path.components().next_back()) {
            (Some(parent), Some(Component::Normal(name))) => resolved(parent)?.join(name),
            (Some(parent), Some(Component::ParentDir)) => {
                let parent = resolved(parent)?;
                parent.parent().unwrap_or(&parent).to_path_buf()
            }
            // A missing drive or share is still a valid saved setting. The
            // connection check reports it; project loading must remain usable.
            _ => path,
        }
    };
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        let value = if let Some(s) = value.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{s}")
        } else {
            value.strip_prefix(r"\\?\").unwrap_or(&value).to_string()
        };
        Ok(PathBuf::from(value))
    }
    #[cfg(not(windows))]
    {
        Ok(path)
    }
}
pub fn identity(path: &Path) -> Result<String> {
    let value = resolved(path)?.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        Ok(value.replace('/', "\\").to_lowercase())
    }
    #[cfg(not(windows))]
    {
        Ok(value)
    }
}
pub fn revision(path: &Path) -> Result<Option<String>> {
    match file_digest(path) {
        Ok(hash) => Ok(Some(hash)),
        Err(crate::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
pub struct Lock {
    _file: File,
}
impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let root = std::env::temp_dir().join("bajptr-locks");
        fs::create_dir_all(&root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(format!("{}.lock", digest(identity(path)?.as_bytes()))))?;
        file.try_lock_exclusive().map_err(lock_error)?;
        Ok(Self { _file: file })
    }
}
fn lock_error(error: std::io::Error) -> crate::Error {
    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
        invalid("另一个工具实例正在处理这些文件，请稍后重试。")
    } else {
        error.into()
    }
}

pub fn temporary_path(target: &Path) -> Result<tempfile::TempPath> {
    let parent = target.parent().ok_or_else(|| invalid("保存路径无效。"))?;
    Ok(tempfile::Builder::new()
        .prefix(&format!(
            "{}.pending-",
            target.file_name().unwrap_or_default().to_string_lossy()
        ))
        .tempfile_in(parent)?
        .into_temp_path())
}
pub fn replace(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let extended = |path: &Path| -> Result<Vec<u16>> {
            let absolute = resolved(path)?.to_string_lossy().into_owned();
            let name = if let Some(unc) = absolute.strip_prefix(r"\\") {
                format!(r"\\?\UNC\{unc}")
            } else {
                format!(r"\\?\{absolute}")
            };
            Ok(std::ffi::OsStr::new(&name)
                .encode_wide()
                .chain(Some(0))
                .collect())
        };
        let a = extended(source)?;
        let b = extended(destination)?;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(a.as_ptr(), b.as_ptr(), 1 | 8)
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)?;
        Ok(())
    }
}
pub fn check_revision(path: &Path, expected: &Option<String>) -> Result<()> {
    if &revision(path)? == expected {
        Ok(())
    } else {
        Err(revision_conflict(path))
    }
}
fn revision_conflict(path: &Path) -> crate::Error {
    invalid(format!(
        "{} 已被其他程序修改，未覆盖原文件。请重新打开工具。",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}
/// Caller holds the corresponding file lock across validation and publication.
pub fn write_locked(path: &Path, bytes: &[u8]) -> Result<String> {
    write_pending(path, |stream| Ok(stream.write_all(bytes)?))?;
    Ok(digest(bytes))
}
fn write_pending(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or_else(|| invalid("保存路径无效。"))?)?;
    let pending = temporary_path(path)?;
    let mut stream = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&pending)?;
    write(&mut stream)?;
    stream.sync_all()?;
    drop(stream);
    replace(&pending, path)
}
pub fn atomic_write(
    path: &Path,
    bytes: &[u8],
    expected: Option<&Option<String>>,
) -> Result<String> {
    let _lock = Lock::acquire(path)?;
    if let Some(expected) = expected {
        check_revision(path, expected)?;
    }
    write_locked(path, bytes)
}
#[derive(Default)]
pub struct Store {
    pub root: PathBuf,
    revisions: HashMap<String, Option<String>>,
    failed: Vec<String>,
}
const SOURCES_TRANSACTION: &str = "sources-transaction.json";
#[derive(Serialize, Deserialize)]
struct SourceWrite<'a> {
    name: String,
    before: Option<JournalBytes<'a>>,
    after: JournalBytes<'a>,
}
struct JournalBytes<'a>(Cow<'a, [u8]>);
impl Serialize for JournalBytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&Base64Display::new(&self.0, &B64))
    }
}
impl<'de> Deserialize<'de> for JournalBytes<'_> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> de::Visitor<'de> for BytesVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("base64 data or a legacy byte array")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Vec<u8>, E> {
                B64.decode(value).map_err(E::custom)
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Vec<u8>, A::Error> {
                let mut bytes = Vec::new();
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                Ok(bytes)
            }
        }
        deserializer
            .deserialize_any(BytesVisitor)
            .map(|bytes| Self(Cow::Owned(bytes)))
    }
}
impl SourceWrite<'_> {
    fn before(&self) -> Option<&[u8]> {
        self.before.as_ref().map(|bytes| bytes.0.as_ref())
    }
}
fn document_locks(root: &Path, names: &[&str]) -> Result<Vec<Lock>> {
    names
        .iter()
        .map(|name| Lock::acquire(&root.join(name)))
        .collect()
}
fn rollback_documents(root: &Path, journal: &str, writes: &[SourceWrite<'_>]) -> Result<()> {
    for write in writes {
        let current = revision(&root.join(&write.name))?;
        let before = write.before().map(digest);
        if current != before && current != Some(digest(&write.after.0)) {
            return Err(invalid(
                "成组保存中断后文件又被修改，请保留事务记录并检查数据目录，未覆盖文件。",
            ));
        }
    }
    for write in writes {
        let path = root.join(&write.name);
        if revision(&path)? == write.before().map(digest) {
            continue;
        }
        if let Some(bytes) = write.before() {
            write_locked(&path, bytes)?;
        } else {
            let archived = root.join(format!(
                "source-sync-aborted-{}-{}",
                uuid::Uuid::new_v4().simple(),
                write.name
            ));
            fs::rename(&path, archived)?;
        }
    }
    fs::remove_file(root.join(journal))?;
    Ok(())
}
pub fn recover_sources(root: &Path) -> Result<()> {
    recover_documents(
        root,
        SOURCES_TRANSACTION,
        &["rules.tsv", "source-history.json"],
    )?;
    recover_documents(
        root,
        "replacements-transaction.json",
        &["rules.tsv", "text-edits.json"],
    )?;
    recover_documents(root, "workspace-transaction.json", &crate::backup::FILES)
}
pub fn pending_transaction(root: &Path) -> bool {
    [
        SOURCES_TRANSACTION,
        "replacements-transaction.json",
        "workspace-transaction.json",
    ]
    .iter()
    .any(|name| root.join(name).exists())
}
fn recover_documents(root: &Path, journal: &str, names: &[&str]) -> Result<()> {
    let _locks = document_locks(root, names)?;
    let bytes = match fs::read(root.join(journal)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let writes: Vec<SourceWrite<'_>> = serde_json::from_slice(&bytes)?;
    if writes.len() != names.len()
        || writes
            .iter()
            .zip(names)
            .any(|(write, name)| write.name != *name)
    {
        return Err(invalid("成组保存的事务记录格式无效，未覆盖原文件。"));
    }
    rollback_documents(root, journal, &writes)
}
impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }
    pub fn load<T>(
        &mut self,
        name: &str,
        parse: impl FnOnce(&[u8]) -> Result<T>,
        default: T,
    ) -> Result<T> {
        let path = self.root.join(name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                self.failed.push(name.into());
                return Err(e.into());
            }
        };
        self.revisions
            .insert(name.into(), bytes.as_ref().map(|b| digest(b)));
        match bytes {
            None => Ok(default),
            Some(bytes) => parse(&bytes).inspect_err(|_| self.failed.push(name.into())),
        }
    }
    pub fn save(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        if self.failed.iter().any(|n| n == name) {
            return Err(invalid(
                "原文件未能完整读取，已停止保存。请修复后重新打开。",
            ));
        }
        let expected = self
            .revisions
            .get(name)
            .ok_or_else(|| invalid("请先读取修改记录。"))?;
        let revision = atomic_write(&self.root.join(name), bytes, Some(expected))?;
        self.revisions.insert(name.into(), Some(revision));
        Ok(())
    }
    pub fn check(&self, name: &str) -> Result<()> {
        check_revision(
            &self.root.join(name),
            self.revisions
                .get(name)
                .ok_or_else(|| invalid("修改记录尚未读取。"))?,
        )
    }
    pub(crate) fn expected_revision(&self, name: &str) -> Option<&Option<String>> {
        self.revisions.get(name)
    }
    pub fn save_sources(&mut self, rules: &[u8], history: &[u8]) -> Result<()> {
        self.save_documents(
            &[("rules.tsv", rules), ("source-history.json", history)],
            SOURCES_TRANSACTION,
        )
    }
    pub fn save_replacements(&mut self, rules: &[u8], edits: &[u8]) -> Result<()> {
        self.save_documents(
            &[("rules.tsv", rules), ("text-edits.json", edits)],
            "replacements-transaction.json",
        )
    }
    fn save_documents(&mut self, documents: &[(&str, &[u8])], journal: &str) -> Result<()> {
        let mut expected = HashMap::new();
        for (name, _) in documents {
            if self.failed.iter().any(|failed| failed == name) {
                return Err(invalid("记录文件读取失败，未保存任何变更。"));
            }
            expected.insert(
                (*name).to_string(),
                self.revisions
                    .get(*name)
                    .ok_or_else(|| invalid("修改记录尚未读取。"))?
                    .clone(),
            );
        }
        self.save_checked(documents, journal, &expected)
    }
    pub fn restore_workspace(
        &mut self,
        documents: &std::collections::BTreeMap<String, Vec<u8>>,
        expected: &HashMap<String, Option<String>>,
    ) -> Result<()> {
        let documents = crate::backup::FILES
            .iter()
            .map(|name| {
                documents
                    .get(*name)
                    .map(|bytes| (*name, bytes.as_slice()))
                    .ok_or_else(|| invalid("备份缺少工作区记录，未恢复。"))
            })
            .collect::<Result<Vec<_>>>()?;
        self.save_checked(&documents, "workspace-transaction.json", expected)?;
        self.failed
            .retain(|name| !crate::backup::FILES.contains(&name.as_str()));
        Ok(())
    }
    fn save_checked(
        &mut self,
        documents: &[(&str, &[u8])],
        journal: &str,
        expected: &HashMap<String, Option<String>>,
    ) -> Result<()> {
        let names = documents.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        let _locks = document_locks(&self.root, &names)?;
        if pending_transaction(&self.root) {
            return Err(invalid(
                "记录存在未完成的成组保存，请重新打开工具恢复后重试。",
            ));
        }
        let mut writes = Vec::new();
        for &(name, bytes) in documents {
            let path = self.root.join(name);
            let revision = expected
                .get(name)
                .ok_or_else(|| invalid("缺少恢复前的文件修订，未保存。"))?;
            let before = match fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if &before.as_deref().map(digest) != revision {
                return Err(revision_conflict(&path));
            }
            writes.push(SourceWrite {
                name: name.into(),
                before: before.map(|bytes| JournalBytes(Cow::Owned(bytes))),
                after: JournalBytes(Cow::Borrowed(bytes)),
            });
        }
        write_pending(&self.root.join(journal), |stream| {
            let mut writer = BufWriter::new(stream);
            serde_json::to_writer(&mut writer, &writes)?;
            Ok(writer.flush()?)
        })?;
        let result = (|| -> Result<Vec<String>> {
            let mut revisions = Vec::with_capacity(writes.len());
            for write in &writes {
                revisions.push(write_locked(&self.root.join(&write.name), &write.after.0)?);
            }
            fs::remove_file(self.root.join(journal))?;
            Ok(revisions)
        })();
        let revisions = result.map_err(|error| {
            if let Err(rollback) = rollback_documents(&self.root, journal, &writes) {
                self.failed
                    .extend(writes.iter().map(|write| write.name.clone()));
                return crate::Error::Context {
                    message:
                        "成组保存失败，原记录尚未完整恢复。请重新打开工具恢复；详细原因见日志。"
                            .into(),
                    source: Box::new(invalid(format!("保存：{error}；回滚：{rollback}"))),
                };
            }
            error
        })?;
        for (write, revision) in writes.into_iter().zip(revisions) {
            self.revisions.insert(write.name, Some(revision));
        }
        Ok(())
    }
    pub fn archive_failed(&mut self, name: &str) -> Result<PathBuf> {
        if !["rules.tsv", "text-edits.json", "source-history.json"].contains(&name) {
            return Err(invalid("这类记录不支持重置，请修复原文件后重新打开工具。"));
        }
        if !self.failed.iter().any(|failed| failed == name)
            || !["rules.tsv", "text-edits.json", "source-history.json"].contains(&name)
        {
            return Err(invalid("这份记录没有读取错误，未进行重置。"));
        }
        let path = self.root.join(name);
        let _lock = Lock::acquire(&path)?;
        self.check(name)?;
        let recovery = self
            .root
            .join("recovery")
            .join(uuid::Uuid::new_v4().simple().to_string());
        fs::create_dir_all(&recovery)?;
        let archived = recovery.join(name);
        fs::rename(&path, &archived)?;
        self.failed.retain(|failed| failed != name);
        self.revisions.insert(name.into(), None);
        Ok(archived)
    }
}
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| "<>:\"/\\|?*".contains(c) || c.is_control())
        && Path::new(name)
            .components()
            .all(|p| matches!(p, Component::Normal(_)))
        && name != "."
        && name != ".."
}
pub fn retire(source: &Path) -> Result<()> {
    retire_to(
        source,
        std::env::var_os("BAJPTR_TRASH_DIR")
            .as_deref()
            .map(Path::new),
    )
}
/// An optional explicit destination is useful for automated tests. Ordinary
/// releases use the OS Recycle Bin and never assume a user's drive letters.
pub fn retire_to(source: &Path, destination: Option<&Path>) -> Result<()> {
    let source = resolved(source)?;
    let moved = (|| -> Result<()> {
        let root = destination.ok_or_else(|| invalid("没有指定移出目录。"))?;
        fs::create_dir_all(root)?;
        let root = resolved(root)?;
        let target = root.join(format!(
            "bajptr-{}-{}",
            source.file_name().unwrap_or_default().to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        ));
        if fs::rename(&source, &target).is_err() {
            #[cfg(windows)]
            {
                shell_file_operation(&source, Some(&target), 1)?;
            }
            #[cfg(not(windows))]
            {
                fs::rename(&source, &target)?;
            }
        }
        if source.exists()
            || !target.exists()
            || resolved(&target)?.parent() != Some(root.as_path())
        {
            return Err(invalid("移出后路径检查失败。"));
        }
        Ok(())
    })();
    if moved.is_ok() {
        return moved;
    }
    #[cfg(windows)]
    {
        shell_file_operation(&source, None, 3)?;
        if source.exists() {
            return Err(invalid("移到回收站后检查失败。"));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        moved
    }
}
#[cfg(windows)]
fn shell_file_operation(source: &Path, target: Option<&Path>, operation: u32) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let from: Vec<u16> = source.as_os_str().encode_wide().chain([0, 0]).collect();
    let to: Vec<u16> = target
        .map(|p| p.as_os_str().encode_wide().chain([0, 0]).collect())
        .unwrap_or_else(|| vec![0, 0]);
    let mut args = windows_sys::Win32::UI::Shell::SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: operation,
        pFrom: from.as_ptr(),
        pTo: to.as_ptr(),
        fFlags: 0x10 | 0x4 | 0x400 | 0x200 | if operation == 3 { 0x40 } else { 0 },
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let result = unsafe { windows_sys::Win32::UI::Shell::SHFileOperationW(&mut args) };
    if result != 0 || args.fAnyOperationsAborted != 0 {
        return Err(invalid(format!(
            "临时内容暂未移出：{}（系统返回 {result}）",
            source.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    #[test]
    fn lock_errors_distinguish_contention_from_permission_and_other_failures() {
        assert!(
            lock_error(fs2::lock_contended_error())
                .to_string()
                .contains("另一个工具实例")
        );
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidInput,
        ] {
            let error = lock_error(std::io::Error::new(kind, "underlying lock failure"));
            assert!(matches!(error, crate::Error::Io(_)));
            assert!(
                !crate::diagnostics::user_message("save_rule", &error).contains("另一个工具实例")
            );
        }
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    #[test]
    fn compact_journal_recovers_binary_files_and_records_that_were_missing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let before: Vec<u8> = (0..=255).cycle().take(4096).collect();
        let after: &[u8] = b"first file written before interruption";
        let created: &[u8] = b"new file written before interruption";
        fs::write(root.join("rules.tsv"), after).unwrap();
        fs::write(root.join("source-history.json"), created).unwrap();

        let writes = [
            SourceWrite {
                name: "rules.tsv".into(),
                before: Some(JournalBytes(Cow::Borrowed(&before))),
                after: JournalBytes(Cow::Borrowed(after)),
            },
            SourceWrite {
                name: "source-history.json".into(),
                before: None,
                after: JournalBytes(Cow::Borrowed(created)),
            },
        ];
        let bytes = serde_json::to_vec(&writes).unwrap();
        let legacy = serde_json::to_vec(&serde_json::json!([
            {"name":"rules.tsv","before":before,"after":after},
            {"name":"source-history.json","before":null,"after":created},
        ]))
        .unwrap();
        assert!(bytes.len() * 2 < legacy.len());
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value[0]["before"], B64.encode(&before));
        assert!(value[1]["before"].is_null());
        fs::write(root.join(SOURCES_TRANSACTION), bytes).unwrap();

        recover_sources(root).unwrap();
        assert_eq!(fs::read(root.join("rules.tsv")).unwrap(), before);
        assert!(!root.join("source-history.json").exists());
        assert!(!root.join(SOURCES_TRANSACTION).exists());
        let archived = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("source-sync-aborted-")
            })
            .unwrap();
        assert_eq!(fs::read(archived).unwrap(), created);
    }
}
