#![allow(dead_code)]

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, Statement};
use std::path::Path;
use std::time::Duration;
use tokio::fs;

use crate::info;

pub mod queries;
pub mod stats;
pub mod utils;

/// 初始化数据库连接
pub async fn init() -> Result<DatabaseConnection, DbErr> {
    if !Path::new("data").exists() {
        let _ = fs::create_dir("data").await;
    }

    // mode=rwc 允许 读/写/创建
    let db_url = "sqlite:data/bot.db?mode=rwc";

    // 配置连接池选项
    // 注意: SQLite 为单写者模型，并发写会串行排队，过大的连接池不仅无用，
    // 还会让每个连接各自持有 page cache / prepared statement 缓存，白占内存。
    // 8 个连接已远够用；idle_timeout 会在空闲时及时回收。
    let mut opt = ConnectOptions::new(db_url);
    opt.max_connections(8)
        .min_connections(1)
        .connect_timeout(Duration::from_secs(8))
        .acquire_timeout(Duration::from_secs(8))
        .idle_timeout(Duration::from_secs(8))
        .max_lifetime(Duration::from_secs(8 * 60)); // 设置连接最大生命周期，防止长时间空闲后连接失效

    // 设置日志级别（可选）
    // opt.sqlx_logging(true)
    //    .sqlx_logging_level(log::LevelFilter::Debug);

    let db = Database::connect(opt).await?;

    // 开启 WAL 模式 (Write-Ahead Logging) 以提高并发性能
    let backend = db.get_database_backend();
    db.execute_raw(Statement::from_string(
        backend,
        "PRAGMA journal_mode=WAL;".to_owned(),
    ))
    .await?;

    // 关闭过于严格的安全检查 (Synchronous NORMAL 足够安全且快)
    db.execute_raw(Statement::from_string(
        backend,
        "PRAGMA synchronous=NORMAL;".to_owned(),
    ))
    .await?;

    // 启用增量自动回收: 删除行后页面进入 freelist，配合每日 incremental_vacuum 回收，
    // 避免全量 VACUUM 需要约 2× 文件大小的临时空间和长时间锁库。
    // (首次从 NONE 切换会对现有库隐式执行一次 VACUUM，之后便为增量模式)
    db.execute_raw(Statement::from_string(
        backend,
        "PRAGMA auto_vacuum=INCREMENTAL;".to_owned(),
    ))
    .await?;

    info!(target: "Database", "连接成功: {} (WAL Mode)", db_url);

    Ok(db)
}
