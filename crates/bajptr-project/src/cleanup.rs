//! Successful scratch work is deleted independently of the interactive process.
//! Failed work stays in place for diagnosis. The helper accepts only marked scratch.
use crate::{Control, Result, invalid, storage};
use std::{fs, path::Path};
const MARKER: &str = ".bajptr-scratch";
pub fn mark(path: &Path) -> Result<()> {
    storage::write_locked(
        &path.join(MARKER),
        uuid::Uuid::new_v4().simple().to_string().as_bytes(),
    )?;
    Ok(())
}
pub fn handle_mode() -> Option<i32> {
    let args: Vec<_> = std::env::args_os().collect();
    if !args.get(1).is_some_and(|s| s == "--retire-scratch") {
        return None;
    }
    let result = (|| -> Result<()> {
        if args.len() != 4 {
            return Err(invalid("清理参数无效。"));
        }
        remove_marked(Path::new(&args[2]), &args[3].to_string_lossy())
    })();
    if let Err(error) = &result {
        eprintln!("{error}");
    }
    Some(if result.is_ok() { 0 } else { 1 })
}
fn remove_marked(path: &Path, token: &str) -> Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if !path.is_absolute()
        || !(name.starts_with("patch-") || name.starts_with("bajptr-standalone-"))
        || token.len() != 32
        || !token.bytes().all(|c| c.is_ascii_hexdigit())
        || !fs::symlink_metadata(path)?.file_type().is_dir()
        || fs::read(path.join(MARKER))? != token.as_bytes()
    {
        return Err(invalid("未接受未标记的清理目录。"));
    }
    fs::remove_dir_all(path)?;
    Ok(())
}

pub fn remove_success(path: &Path, control: &Control<'_>) {
    let remove = || -> Result<()> {
        let path = std::path::absolute(path)?;
        let token = fs::read_to_string(path.join(MARKER))?;
        remove_marked(&path, &token)
    };
    let spawn = (|| -> Result<bool> {
        let exe = std::env::current_exe()?;
        // Test harnesses clean up synchronously.
        if !matches!(
            exe.file_stem().and_then(|s| s.to_str()),
            Some("bajptr-desktop" | "bajptr-rs")
        ) {
            return Ok(false);
        }
        let token = fs::read_to_string(path.join(MARKER))?;
        let mut command = std::process::Command::new(exe);
        command
            .arg("--retire-scratch")
            .arg(std::path::absolute(path)?)
            .arg(token)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        command.spawn()?;
        Ok(true)
    })();
    if !matches!(spawn, Ok(true))
        && let Err(error) = remove()
    {
        let _ = control.warn(&format!("临时目录清理失败 {}：{error}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_cleanup_requires_the_matching_marker() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("patch-test");
        fs::create_dir(&scratch).unwrap();
        fs::write(scratch.join("working.db"), b"temporary").unwrap();
        assert!(remove_marked(&scratch, &"0".repeat(32)).is_err());
        assert!(scratch.join("working.db").is_file());

        mark(&scratch).unwrap();
        let token = fs::read_to_string(scratch.join(MARKER)).unwrap();
        remove_marked(&scratch, &token).unwrap();
        assert!(!scratch.exists());
    }
}
