use rust_zero_core::{
    MongoStore, MongoStoreConfig, RedisStore, RedisStoreConfig, SqlStoreConfig, SqliteStore,
};
use std::env;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".to_owned());
    let mongo_url =
        env::var("MONGODB_URI").unwrap_or_else(|_| "mongodb://127.0.0.1:27017".to_owned());
    let sqlite_url = env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite::memory:".to_owned());

    let redis = RedisStore::new(RedisStoreConfig::new(redis_url))?;
    let sql = SqliteStore::connect_sqlite(SqlStoreConfig::new(sqlite_url)).await?;
    let mongo = MongoStore::connect(MongoStoreConfig::new(mongo_url, "rust_zero_example")).await?;

    redis.ping().await?;
    sql.health_check().await?;
    mongo.health_check().await?;
    println!("Redis, SQLite, and MongoDB are ready");
    Ok(())
}
