use bajptr_core::{
    Control, Result, codec,
    database::{self, Database, Options},
    invalid,
};
use bajptr_project::{
    baseline, region,
    service::{Config, Engine, PatchJob},
    storage,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(
    name = "bajptr-rs",
    version,
    about = "BA-JP-Term-Replacer（BA-JPTR）· 蔚蓝档案日服译名替换工具"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Args)]
struct Source {
    #[arg(long)]
    game_dir: Option<String>,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long,default_value=region::KEY)]
    key: String,
}
impl Source {
    fn game(&self) -> Result<String> {
        self.game_dir
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid("请用 --game-dir 指定游戏目录。"))
    }
    fn path(&self) -> Result<PathBuf> {
        match &self.db {
            Some(path) => Ok(path.clone()),
            None => Ok(region::locate(&self.game()?)?.bundle),
        }
    }
    fn open(&self, work: &Path, control: &Control<'_>) -> Result<Database> {
        let path = self.path()?;
        Database::open_in(
            &path,
            is_encrypted(&path)?,
            &codec::key(&self.key)?,
            control,
            work,
        )
    }
}
#[derive(Subcommand)]
enum Command {
    /// 显示游戏、资源目录和数据库位置
    Paths {
        #[command(flatten)]
        source: Source,
    },
    /// 建立或检查当前版本唯一底稿
    Backup {
        #[command(flatten)]
        source: Source,
        #[arg(long, default_value = "backup")]
        out: PathBuf,
    },
    /// 恢复底稿，保留词表和单条修改
    Restore {
        #[command(flatten)]
        source: Source,
        #[arg(long, default_value = "backup")]
        backup_dir: PathBuf,
    },
    /// 从指定数据库或当前游戏查找文字
    Scan {
        #[command(flatten)]
        source: Source,
        #[arg(long)]
        keyword: String,
        #[arg(long,default_value_t=database::default_tables())]
        tables: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 导出为每个文本表一个 TSV
    Export {
        #[command(flatten)]
        source: Source,
        #[arg(long,default_value_t=database::default_tables())]
        tables: String,
        #[arg(long, default_value = "output/export")]
        out: PathBuf,
    },
    /// 从初始底稿生成补丁，只有 --install 才写入游戏
    Patch {
        #[command(flatten)]
        source: Source,
        #[arg(long,default_value_t=database::default_tables())]
        tables: String,
        #[arg(long)]
        glossary: Option<PathBuf>,
        #[arg(long, default_value = "output")]
        out: PathBuf,
        #[arg(long)]
        install: bool,
        #[arg(long, default_value = "backup")]
        backup_dir: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_ascii_exact: bool,
        #[arg(long)]
        work_dir: Option<PathBuf>,
    },
}
fn is_encrypted(path: &Path) -> Result<bool> {
    let mut header = [0; 16];
    fs::File::open(path)?.read_exact(&mut header)?;
    Ok(&header != b"SQLite format 3\0")
}
fn oneline(s: &str) -> String {
    s.replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}
pub fn run(control: &Control<'_>) -> Result<()> {
    let cli = Cli::parse();
    let data = std::env::current_dir()?;
    let work = data.join("work");
    let mut engine = Engine::with_work(work.clone());
    match cli.command {
        Command::Paths { source } => {
            let game = region::locate(&source.game()?)?;
            println!(
                "游戏目录：{}\n资源目录：{}\n数据库：{}\n大小：{}\nCRC：{}",
                game.dir.display(),
                game.catalog.display(),
                game.bundle.display(),
                game.entry.size,
                game.entry.crc
            );
        }
        Command::Backup { source, out } => {
            let original = engine.initialize(&source.game()?, &out, &data, None, control)?;
            println!("初始底稿：{}", original.bundle.display());
        }
        Command::Restore { source, backup_dir } => {
            engine.restore(&source.game()?, &data, &backup_dir, control)?;
            println!("已恢复初始底稿，词表和单条修改已保留。");
        }
        Command::Scan {
            source,
            keyword,
            tables,
            limit,
        } => {
            let db = source.open(&work, control)?;
            let (rows, total) = database::scan(&db.con, &tables, &keyword, limit, control)?;
            for (table, rid, slot, text) in &rows {
                println!("{table}\t{}\tfield{slot}\t{}", rid.display(), oneline(text));
            }
            println!("命中 {total} 处（显示前 {} 条）", rows.len());
        }
        Command::Export {
            source,
            tables,
            out,
        } => {
            let db = source.open(&work, control)?;
            let count = bajptr_project::text_export::export(&out, control, |stage| {
                database::export(&db.con, &tables, stage, control)
            })?;
            println!("已导出 {count} 条文本 → {}", out.display());
        }
        Command::Patch {
            source,
            tables,
            glossary,
            out,
            install,
            backup_dir,
            dry_run,

            no_ascii_exact,
            work_dir,
        } => {
            let mut config = Config::from_glossary(glossary)?;
            config.ascii_exact = !no_ascii_exact;
            config.tables = tables;
            config.work_dir = work_dir.or(Some(work));
            engine = Engine::with_work(config.work_dir.clone().unwrap());
            let result = if let Some(path) = &source.db {
                if install {
                    return Err(invalid(
                        "直接指定 --db 时不能 --install；请选择 --game-dir 后安装。",
                    ));
                }
                standalone_patch(path, &source.key, &out, &config, dry_run, control)?
            } else {
                if source.key != region::KEY {
                    return Err(invalid(
                        "游戏底稿只使用当前日服密钥；其他密钥请配合 --db 处理独立资源。",
                    ));
                }
                let game = source.game()?;
                if dry_run {
                    json!({"stats":engine.preview_at(&game,&data,&backup_dir,&config,control)?})
                } else {
                    engine.generate(
                        PatchJob {
                            game: &game,
                            data: &data,
                            backup_root: &backup_dir,
                            output_root: &out,
                            config: &config,
                            install,
                            expected: None,
                        },
                        control,
                    )?
                }
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }
    Ok(())
}
fn standalone_patch(
    input: &Path,
    key: &str,
    out: &Path,
    config: &Config,
    dry_run: bool,
    control: &Control<'_>,
) -> Result<Value> {
    let key = codec::key(key)?;
    let encrypted = is_encrypted(input)?;
    let rules = config.compiled_rules()?;
    let options: Options = serde_json::from_value(
        json!({"ascii_exact":config.ascii_exact,"tables":config.tables,"explain":true}),
    )?;
    let stage_root = config
        .work_dir
        .as_deref()
        .unwrap_or_else(|| Path::new("work"));
    if dry_run {
        let db = Database::open_in(input, encrypted, &key, control, stage_root)?;
        let stats =
            database::patch_database(&db.con, None, None, &rules, &options, false, control)?;
        config.check_documents()?;
        return Ok(json!({"stats":stats}));
    }
    fs::create_dir_all(stage_root)?;
    let stage = tempfile::Builder::new()
        .prefix("bajptr-standalone-")
        .tempdir_in(stage_root)?;
    let plain = stage.path().join("working.db");
    let info = if encrypted {
        Some(codec::decrypt(input, &plain, &key, control)?)
    } else {
        baseline::copy_sync(input, &plain)?;
        None
    };
    let con =
        rusqlite::Connection::open_with_flags(&plain, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let stats = database::patch_database(&con, None, None, &rules, &options, true, control)?;
    let check: String = con.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(invalid("生成数据库完整性校验失败。"));
    }
    drop(con);
    let result = if let Some(info) = info {
        let result = stage.path().join("patched.db");
        let salt = B64.decode(info.salt).map_err(|_| invalid("salt 无效。"))?;
        let ivs = info
            .ivs
            .iter()
            .map(|v| B64.decode(v).map_err(|_| invalid("IV 无效。")))
            .collect::<Result<Vec<_>>>()?;
        let (_, checked) = codec::encrypt_checked(&plain, &result, &key, &salt, &ivs, control)?;
        if checked.ok == 0 || checked.bad > 0 {
            return Err(invalid("输出数据库 HMAC 校验失败。"));
        }
        fs::remove_file(&plain)?;
        result
    } else {
        plain
    };
    let _document_locks = config.lock_checked_documents()?;
    control.check()?;
    fs::create_dir_all(out)?;
    let digest = storage::file_digest(&result)?;
    let folder = out.join(format!("patch-{digest}"));
    let _lock = storage::Lock::acquire(&folder)?;
    let target = folder.join("patched.db");
    if folder.exists() {
        if !storage::is_plain_directory(&folder)? || storage::file_digest(&target)? != digest {
            return Err(invalid("同名补丁内容已变化，请选择其他输出目录。"));
        }
    } else {
        let pending = tempfile::Builder::new()
            .prefix("pending-")
            .tempdir_in(out)?;
        let pending_file = pending.path().join("patched.db");
        baseline::copy_sync(&result, &pending_file)?;
        if storage::file_digest(&pending_file)? != digest {
            return Err(invalid("补丁副本校验失败。"));
        }
        fs::rename(pending.path(), &folder)?;
    }
    Ok(json!({"output":target,"stats":stats,"verified":true}))
}
