//! Retention for complete snapshots published by the tool.
use crate::model;
use crate::{Control, Result, invalid, storage};
use std::{fs, path::Path};
const MANAGED: &str = ".bajptr-managed";

pub(crate) fn mark_managed(path: &Path) -> Result<()> {
    let files = managed_contents(path)?
        .into_iter()
        .map(|name| {
            let digest = storage::file_digest(&path.join(&name))?;
            Ok((name, digest))
        })
        .collect::<Result<Vec<_>>>()?;
    storage::write_locked(&path.join(MANAGED), &serde_json::to_vec(&files)?)?;
    Ok(())
}

fn managed_contents(path: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(invalid("自动备份目录包含其他内容，已保留。"));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("文件名无效。"))?;
        if name != MANAGED {
            names.push(name);
        }
    }
    names.sort_unstable();
    Ok(names)
}

pub(crate) fn retain_recent(
    root: &Path,
    prefix: &str,
    keep: usize,
    protected: impl Fn(&str) -> bool,
    control: &Control<'_>,
) {
    let prune = || -> Result<()> {
        let mut candidates = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry.file_name();
            let file_name = name.to_string_lossy();
            let Some(id) = file_name
                .strip_prefix(prefix)
                .filter(|id| model::is_version(id))
            else {
                continue;
            };
            if protected(id) || !storage::is_plain_directory(&entry.path())? {
                continue;
            }
            let path = entry.path();
            let marker = path.join(MANAGED);
            if !fs::symlink_metadata(&marker)
                .is_ok_and(|m| m.file_type().is_file() && m.len() <= 8192)
            {
                continue;
            }
            let Ok(files) = serde_json::from_slice::<Vec<(String, String)>>(&fs::read(&marker)?)
            else {
                continue;
            };
            if managed_contents(&path)
                .is_ok_and(|actual| files.iter().map(|(name, _)| name).eq(actual.iter()))
            {
                candidates.push((entry.metadata()?.modified()?, path, files));
            }
        }
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        let mut removed = 0;
        for (_, path, files) in candidates.into_iter().skip(keep) {
            // Verify an expired directory still contains exactly the files published by the tool.
            if files.iter().all(|(name, digest)| {
                storage::file_digest(&path.join(name)).is_ok_and(|actual| &actual == digest)
            }) {
                fs::remove_dir_all(&path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            let _ = control.log(&format!(
                "已清理 {} 下的 {removed} 份过期自动备份。",
                root.display()
            ));
        }
        Ok(())
    };
    if let Err(error) = prune() {
        let _ = control.warn(&format!("自动备份清理未完成 {}：{error}", root.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_respects_references_and_user_changes() {
        let root = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..5)
            .map(|index| root.path().join(format!("snapshot-{index:064x}")))
            .collect();
        for path in &paths {
            fs::create_dir(path).unwrap();
            fs::write(path.join("data"), b"snapshot").unwrap();
        }
        for path in &paths[..4] {
            mark_managed(path).unwrap();
        }
        fs::write(paths[2].join("data"), b"user change").unwrap();
        fs::write(paths[3].join("notes"), b"added file").unwrap();
        retain_recent(
            root.path(),
            "snapshot-",
            0,
            |id| id.ends_with('1'),
            &Control::new(&|_| true),
        );
        assert!(!paths[0].exists());
        assert!(paths[1..].iter().all(|path| path.is_dir()));
    }
}
