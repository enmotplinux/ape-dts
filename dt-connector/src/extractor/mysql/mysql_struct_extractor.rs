use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use concurrent_queue::ConcurrentQueue;
use dt_common::{error::Error, log_info, utils::rdb_filter::RdbFilter};

use dt_meta::{
    ddl_data::DdlData, ddl_type::DdlType, dt_data::DtData, struct_meta::database_model::StructModel,
};
use sqlx::{MySql, Pool};

use crate::{
    extractor::base_extractor::BaseExtractor,
    meta_fetcher::mysql::mysql_struct_fetcher::MysqlStructFetcher, Extractor,
};

pub struct MysqlStructExtractor<'a> {
    pub conn_pool: Pool<MySql>,
    pub buffer: &'a ConcurrentQueue<DtData>,
    pub db: String,
    pub filter: RdbFilter,
    pub shut_down: &'a AtomicBool,
}

#[async_trait]
impl Extractor for MysqlStructExtractor<'_> {
    async fn extract(&mut self) -> Result<(), Error> {
        log_info!("MysqlStructExtractor starts, schema: {}", self.db,);
        self.extract_internal().await
    }

    async fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

impl MysqlStructExtractor<'_> {
    pub async fn extract_internal(&mut self) -> Result<(), Error> {
        let mut mysql_fetcher = MysqlStructFetcher {
            conn_pool: self.conn_pool.to_owned(),
            db: self.db.clone(),
            filter: Some(self.filter.to_owned()),
        };

        for (_, meta) in mysql_fetcher.get_table(&None).await.unwrap() {
            self.push_dt_data(&meta).await;
        }

        for (_, meta) in mysql_fetcher.get_index(&None).await.unwrap() {
            self.push_dt_data(&meta).await;
        }

        for (_, meta) in mysql_fetcher.get_constraint(&None).await.unwrap() {
            self.push_dt_data(&meta).await;
        }

        BaseExtractor::wait_task_finish(self.buffer, self.shut_down).await
    }

    pub async fn push_dt_data(&mut self, meta: &StructModel) {
        let ddl_data = DdlData {
            schema: self.db.clone(),
            query: String::new(),
            meta: Some(meta.to_owned()),
            ddl_type: DdlType::Unknown,
        };
        BaseExtractor::push_dt_data(self.buffer, DtData::Ddl { ddl_data })
            .await
            .unwrap()
    }

    pub fn build_fetcher(&self) -> MysqlStructFetcher {
        MysqlStructFetcher {
            conn_pool: self.conn_pool.to_owned(),
            db: self.db.clone(),
            filter: Some(self.filter.to_owned()),
        }
    }
}
