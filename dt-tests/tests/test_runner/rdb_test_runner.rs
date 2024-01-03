use std::collections::{HashMap, HashSet};

use dt_common::{
    config::{
        config_enums::DbType, config_token_parser::ConfigTokenParser,
        extractor_config::ExtractorConfig, sinker_config::SinkerConfig, task_config::TaskConfig,
    },
    error::Error,
    utils::{sql_util::SqlUtil, time_util::TimeUtil},
};

use dt_connector::rdb_router::RdbRouter;
use dt_meta::{
    col_value::ColValue, ddl_type::DdlType, mysql::mysql_meta_manager::MysqlMetaManager,
    row_data::RowData, sql_parser::ddl_parser::DdlParser,
};
use dt_task::task_util::TaskUtil;

use sqlx::{MySql, Pool, Postgres};

use crate::test_config_util::TestConfigUtil;

use super::{base_test_runner::BaseTestRunner, rdb_util::RdbUtil};

pub struct RdbTestRunner {
    pub base: BaseTestRunner,
    pub src_conn_pool_mysql: Option<Pool<MySql>>,
    pub dst_conn_pool_mysql: Option<Pool<MySql>>,
    pub src_conn_pool_pg: Option<Pool<Postgres>>,
    pub dst_conn_pool_pg: Option<Pool<Postgres>>,
    pub router: RdbRouter,
}

pub const SRC: &str = "src";
pub const DST: &str = "dst";
pub const PUBLIC: &str = "public";

#[allow(dead_code)]
impl RdbTestRunner {
    pub async fn new(relative_test_dir: &str) -> Result<Self, Error> {
        Self::new_internal(relative_test_dir, "").await
    }

    pub async fn new_internal(
        relative_test_dir: &str,
        config_tmp_relative_dir: &str,
    ) -> Result<Self, Error> {
        let base = BaseTestRunner::new_internal(relative_test_dir, config_tmp_relative_dir)
            .await
            .unwrap();

        // prepare conn pools
        let mut src_conn_pool_mysql = None;
        let mut dst_conn_pool_mysql = None;
        let mut src_conn_pool_pg = None;
        let mut dst_conn_pool_pg = None;

        let (src_db_type, src_url, dst_db_type, dst_url) = Self::parse_conn_info(&base);
        if src_db_type == DbType::Mysql {
            src_conn_pool_mysql = Some(TaskUtil::create_mysql_conn_pool(&src_url, 1, false).await?);
        } else {
            src_conn_pool_pg = Some(TaskUtil::create_pg_conn_pool(&src_url, 1, false).await?);
        }

        if dst_db_type == DbType::Mysql {
            dst_conn_pool_mysql = Some(TaskUtil::create_mysql_conn_pool(&dst_url, 1, false).await?);
        } else {
            dst_conn_pool_pg = Some(TaskUtil::create_pg_conn_pool(&dst_url, 1, false).await?);
        }

        let config = TaskConfig::new(&base.task_config_file);
        let router = RdbRouter::from_config(&config.router, &dst_db_type).unwrap();

        Ok(Self {
            src_conn_pool_mysql,
            dst_conn_pool_mysql,
            src_conn_pool_pg,
            dst_conn_pool_pg,
            router,
            base,
        })
    }

    pub async fn close(&self) -> Result<(), Error> {
        if let Some(pool) = &self.src_conn_pool_mysql {
            pool.close().await;
        }
        if let Some(pool) = &self.dst_conn_pool_mysql {
            pool.close().await;
        }
        if let Some(pool) = &self.src_conn_pool_pg {
            pool.close().await;
        }
        if let Some(pool) = &self.dst_conn_pool_pg {
            pool.close().await;
        }
        Ok(())
    }

    pub async fn get_dst_mysql_version(&self) -> String {
        if let Some(conn_pool) = &self.dst_conn_pool_mysql {
            let meta_manager = MysqlMetaManager::new(conn_pool.clone())
                .init()
                .await
                .unwrap();
            return meta_manager.version.clone();
        }
        String::new()
    }

    fn parse_conn_info(base: &BaseTestRunner) -> (DbType, String, DbType, String) {
        let mut src_db_type = DbType::Mysql;
        let mut src_url = String::new();
        let mut dst_db_type = DbType::Mysql;
        let mut dst_url = String::new();

        let config = TaskConfig::new(&base.task_config_file);
        match config.extractor {
            ExtractorConfig::MysqlCdc { url, .. }
            | ExtractorConfig::MysqlSnapshot { url, .. }
            | ExtractorConfig::MysqlCheck { url, .. }
            | ExtractorConfig::MysqlStruct { url, .. } => {
                src_url = url.clone();
            }

            ExtractorConfig::PgCdc { url, .. }
            | ExtractorConfig::PgSnapshot { url, .. }
            | ExtractorConfig::PgCheck { url, .. }
            | ExtractorConfig::PgStruct { url, .. } => {
                src_db_type = DbType::Pg;
                src_url = url.clone();
            }

            _ => {}
        }

        match config.sinker {
            SinkerConfig::Mysql { url, .. }
            | SinkerConfig::MysqlCheck { url, .. }
            | SinkerConfig::MysqlStruct { url, .. }
            | SinkerConfig::Starrocks { url, .. } => {
                dst_url = url.clone();
            }

            SinkerConfig::Pg { url, .. }
            | SinkerConfig::PgCheck { url, .. }
            | SinkerConfig::PgStruct { url, .. } => {
                dst_db_type = DbType::Pg;
                dst_url = url.clone();
            }

            _ => {}
        }

        (src_db_type, src_url, dst_db_type, dst_url)
    }

    pub async fn run_snapshot_test(&self, compare_data: bool) -> Result<(), Error> {
        // prepare src and dst tables
        self.execute_test_ddl_sqls().await?;
        self.execute_test_dml_sqls().await?;

        // start task
        self.base.start_task().await?;

        // compare data
        let (src_db_tbs, dst_db_tbs) = self.get_compare_db_tbs().await?;
        if compare_data {
            assert!(self.compare_data_for_tbs(&src_db_tbs, &dst_db_tbs).await?)
        }
        Ok(())
    }

    pub async fn run_ddl_test(&self, start_millis: u64, parse_millis: u64) -> Result<(), Error> {
        self.execute_test_ddl_sqls().await?;
        let task = self.base.spawn_task().await?;
        TimeUtil::sleep_millis(start_millis).await;

        self.execute_src_sqls(&self.base.src_dml_sqls).await?;
        TimeUtil::sleep_millis(parse_millis).await;

        let (mut src_db_tbs, mut dst_db_tbs) = self.get_compare_db_tbs().await?;
        let filtered_db_tbs = self.get_filtered_db_tbs();
        for i in (0..src_db_tbs.len()).rev() {
            // do not check filtered tables since they may be dropped
            if filtered_db_tbs.contains(&src_db_tbs[i]) {
                src_db_tbs.remove(i);
                dst_db_tbs.remove(i);
            }
        }

        assert!(self.compare_data_for_tbs(&src_db_tbs, &dst_db_tbs).await?);
        self.base.wait_task_finish(&task).await
    }

    pub async fn run_cdc_test(&self, start_millis: u64, parse_millis: u64) -> Result<(), Error> {
        // prepare src and dst tables
        self.execute_test_ddl_sqls().await?;

        // start task
        let task = self.base.spawn_task().await?;
        TimeUtil::sleep_millis(start_millis).await;

        self.execute_test_sqls_and_compare(parse_millis).await?;

        self.base.wait_task_finish(&task).await
    }

    pub async fn execute_test_sqls_and_compare(&self, parse_millis: u64) -> Result<(), Error> {
        // load dml sqls
        let mut src_insert_sqls = Vec::new();
        let mut src_update_sqls = Vec::new();
        let mut src_delete_sqls = Vec::new();

        for sql in self.base.src_dml_sqls.iter() {
            if sql.to_lowercase().starts_with("insert") {
                src_insert_sqls.push(sql.clone());
            } else if sql.to_lowercase().starts_with("update") {
                src_update_sqls.push(sql.clone());
            } else {
                src_delete_sqls.push(sql.clone());
            }
        }

        // insert src data
        self.execute_src_sqls(&src_insert_sqls).await?;
        TimeUtil::sleep_millis(parse_millis).await;

        let (src_db_tbs, dst_db_tbs) = self.get_compare_db_tbs().await?;
        self.compare_data_for_tbs(&src_db_tbs, &dst_db_tbs).await?;

        // update src data
        self.execute_src_sqls(&src_update_sqls).await?;
        if !src_update_sqls.is_empty() {
            TimeUtil::sleep_millis(parse_millis).await;
        }
        self.compare_data_for_tbs(&src_db_tbs, &dst_db_tbs).await?;

        // delete src data
        self.execute_src_sqls(&src_delete_sqls).await?;
        if !src_delete_sqls.is_empty() {
            TimeUtil::sleep_millis(parse_millis).await;
        }
        self.compare_data_for_tbs(&src_db_tbs, &dst_db_tbs).await?;
        Ok(())
    }

    pub async fn run_cdc_test_with_different_configs(
        &self,
        start_millis: u64,
        parse_millis: u64,
    ) -> Result<(), Error> {
        let configs = TestConfigUtil::get_default_configs();
        for config in configs {
            TestConfigUtil::update_task_config(
                &self.base.task_config_file,
                &self.base.task_config_file,
                &config,
            );
            println!("----------------------------------------");
            println!("running cdc test with config: {:?}", config);
            self.run_cdc_test(start_millis, parse_millis).await.unwrap();
        }
        Ok(())
    }

    pub async fn run_foxlake_test(
        &self,
        start_millis: u64,
        parse_millis: u64,
    ) -> Result<(), Error> {
        // execute src ddl sqls
        self.execute_test_ddl_sqls().await?;

        let task = self.base.spawn_task().await?;
        TimeUtil::sleep_millis(start_millis).await;

        // execute src dml sqls
        self.execute_src_sqls(&self.base.src_dml_sqls).await?;
        TimeUtil::sleep_millis(parse_millis).await;

        self.base.wait_task_finish(&task).await
    }

    pub async fn execute_test_dml_sqls(&self) -> Result<(), Error> {
        self.execute_src_sqls(&self.base.src_dml_sqls).await?;
        self.execute_dst_sqls(&self.base.dst_dml_sqls).await
    }

    pub async fn execute_test_ddl_sqls(&self) -> Result<(), Error> {
        self.execute_src_sqls(&self.base.src_ddl_sqls).await?;
        self.execute_dst_sqls(&self.base.dst_ddl_sqls).await
    }

    pub async fn execute_clean_sqls(&self) -> Result<(), Error> {
        if TestConfigUtil::should_do_clean_or_not() {
            self.execute_src_sqls(&self.base.src_clean_sqls).await?;
            self.execute_dst_sqls(&self.base.dst_clean_sqls).await
        } else {
            Ok(())
        }
    }

    pub async fn execute_src_sqls(&self, sqls: &Vec<String>) -> Result<(), Error> {
        if let Some(pool) = &self.src_conn_pool_mysql {
            RdbUtil::execute_sqls_mysql(pool, sqls).await?;
        }
        if let Some(pool) = &self.src_conn_pool_pg {
            RdbUtil::execute_sqls_pg(pool, sqls).await?;
        }
        Ok(())
    }

    async fn execute_dst_sqls(&self, sqls: &Vec<String>) -> Result<(), Error> {
        if let Some(pool) = &self.dst_conn_pool_mysql {
            RdbUtil::execute_sqls_mysql(pool, sqls).await?;
        }
        if let Some(pool) = &self.dst_conn_pool_pg {
            RdbUtil::execute_sqls_pg(pool, sqls).await?;
        }
        Ok(())
    }

    pub async fn compare_data_for_tbs(
        &self,
        src_db_tbs: &Vec<(String, String)>,
        dst_db_tbs: &Vec<(String, String)>,
    ) -> Result<bool, Error> {
        let filtered_db_tbs = self.get_filtered_db_tbs();
        for i in 0..src_db_tbs.len() {
            if filtered_db_tbs.contains(&src_db_tbs[i]) {
                let dst_data = self.fetch_data(&dst_db_tbs[i], DST).await?;
                if !dst_data.is_empty() {
                    println!("tb: {:?} is filtered but dst is not emtpy", dst_db_tbs[i]);
                    assert!(false)
                }
            } else {
                assert!(self.compare_tb_data(&src_db_tbs[i], &dst_db_tbs[i]).await?)
            }
        }
        Ok(true)
    }

    async fn compare_tb_data(
        &self,
        src_db_tb: &(String, String),
        dst_db_tb: &(String, String),
    ) -> Result<bool, Error> {
        let src_data = self.fetch_data(src_db_tb, SRC).await?;
        let dst_data = self.fetch_data(dst_db_tb, DST).await?;
        println!(
            "comparing row data for src_tb: {:?}, src_data count: {}",
            src_db_tb,
            src_data.len()
        );

        let col_map = self.router.get_col_map(&src_db_tb.0, &src_db_tb.1);
        if !self.compare_row_data(&src_data, &dst_data, col_map) {
            println!(
                "compare tb data failed, src_tb: {:?}, dst_tb: {:?}",
                src_db_tb, dst_db_tb,
            );
            return Ok(false);
        }
        Ok(true)
    }

    fn compare_row_data(
        &self,
        src_data: &Vec<RowData>,
        dst_data: &Vec<RowData>,
        col_map: Option<&HashMap<String, String>>,
    ) -> bool {
        assert_eq!(src_data.len(), dst_data.len());
        let src_db_type = self.get_db_type(SRC);
        let dst_db_type = self.get_db_type(DST);

        for i in 0..src_data.len() {
            let src_col_values = src_data[i].after.as_ref().unwrap();
            let dst_col_values = dst_data[i].after.as_ref().unwrap();

            for (src_col, src_col_value) in src_col_values {
                let dst_col = if let Some(col_map) = col_map {
                    if let Some(dst_col) = col_map.get(src_col) {
                        dst_col
                    } else {
                        src_col
                    }
                } else {
                    src_col
                };

                let dst_col_value = dst_col_values.get(dst_col).unwrap();
                if Self::compare_col_value(src_col_value, dst_col_value, &src_db_type, &dst_db_type)
                {
                    continue;
                }
                println!(
                    "row index: {}, col: {}, src_col_value: {:?}, dst_col_value: {:?}",
                    i, src_col, src_col_value, dst_col_value
                );
                return false;
            }
        }
        true
    }

    fn compare_col_value(
        src_col_value: &ColValue,
        dst_col_value: &ColValue,
        src_db_type: &DbType,
        dst_db_type: &DbType,
    ) -> bool {
        if src_col_value != dst_col_value {
            if src_col_value.is_nan() && dst_col_value.is_nan() {
                return true;
            }
            // different databases support different column types,
            // for example: we use Year in mysql, but INT in StarRocks,
            // so try to compare after both converted to string.
            if src_db_type != dst_db_type {
                return src_col_value.to_option_string() == dst_col_value.to_option_string();
            }
            return false;
        }
        true
    }

    pub async fn fetch_data(
        &self,
        db_tb: &(String, String),
        from: &str,
    ) -> Result<Vec<RowData>, Error> {
        let (conn_pool_mysql, conn_pool_pg) = self.get_conn_pool(from);
        let data = if let Some(pool) = conn_pool_mysql {
            let db_type = self.get_db_type(from);
            RdbUtil::fetch_data_mysql_compatible(pool, db_tb, &db_type).await?
        } else if let Some(pool) = conn_pool_pg {
            RdbUtil::fetch_data_pg(pool, db_tb).await?
        } else {
            Vec::new()
        };
        Ok(data)
    }

    pub fn parse_full_tb_name(full_tb_name: &str, db_type: &DbType) -> (String, String) {
        let escape_pairs = SqlUtil::get_escape_pairs(&db_type);
        let tokens = ConfigTokenParser::parse(full_tb_name, &vec!['.'], &escape_pairs);
        let (db, tb) = if tokens.len() > 1 {
            (tokens[0].to_string(), tokens[1].to_string())
        } else {
            ("".to_string(), full_tb_name.to_string())
        };

        (
            SqlUtil::unescape(&db, &escape_pairs[0]),
            SqlUtil::unescape(&tb, &escape_pairs[0]),
        )
    }

    /// get compare tbs
    pub async fn get_compare_db_tbs(
        &self,
    ) -> Result<(Vec<(String, String)>, Vec<(String, String)>), Error> {
        let mut src_db_tbs = Self::get_compare_db_tbs_from_sqls(&self.base.src_ddl_sqls)?;
        // since tables may be created/dropped in src_dml.sql for ddl tests,
        // we also need to parse src_dml.sql.
        src_db_tbs.extend_from_slice(&Self::get_compare_db_tbs_from_sqls(
            &self.base.src_dml_sqls,
        )?);

        let mut dst_db_tbs = vec![];
        for (db, tb) in src_db_tbs.iter() {
            let (dst_db, dst_tb) = self.router.get_tb_map(db, tb);
            dst_db_tbs.push((dst_db.into(), dst_tb.into()));
        }

        Ok((src_db_tbs, dst_db_tbs))
    }

    pub fn get_compare_db_tbs_from_sqls(
        sqls: &Vec<String>,
    ) -> Result<Vec<(String, String)>, Error> {
        let mut db_tbs = vec![];

        for sql in sqls.iter() {
            let sql = sql.to_lowercase().trim().to_string();
            if !sql.starts_with("create") || !sql.contains("table") {
                continue;
            }
            let ddl = DdlParser::parse(&sql).unwrap();
            if ddl.0 == DdlType::CreateTable {
                let db = if let Some(db) = ddl.1 {
                    db
                } else {
                    PUBLIC.to_string()
                };
                let tb = ddl.2.unwrap();
                db_tbs.push((db, tb));
            }
        }

        Ok(db_tbs)
    }

    fn get_filtered_db_tbs(&self) -> HashSet<(String, String)> {
        let mut filtered_db_tbs = HashSet::new();
        let db_type = &self.get_db_type(SRC);
        let delimiters = vec!['.'];
        let escape_pairs = SqlUtil::get_escape_pairs(db_type);
        let filtered_tbs_file = format!("{}/filtered_tbs.txt", &self.base.test_dir);

        if BaseTestRunner::check_path_exists(&filtered_tbs_file) {
            let lines = BaseTestRunner::load_file(&filtered_tbs_file);
            for line in lines.iter() {
                let db_tb = ConfigTokenParser::parse(line, &delimiters, &escape_pairs);
                filtered_db_tbs.insert((db_tb[0].clone(), db_tb[1].clone()));
            }
        }
        filtered_db_tbs
    }

    async fn get_tb_cols(
        &self,
        db_tb: &(String, String),
        from: &str,
    ) -> Result<Vec<String>, Error> {
        let db_tb = self.router.get_tb_map(&db_tb.0, &db_tb.1);
        let db_tb = &(db_tb.0.into(), db_tb.1.into());

        let (conn_pool_mysql, conn_pool_pg) = self.get_conn_pool(from);
        let cols = if let Some(conn_pool) = conn_pool_mysql {
            let tb_meta = RdbUtil::get_tb_meta_mysql(conn_pool, db_tb).await?;
            tb_meta.basic.cols.clone()
        } else if let Some(conn_pool) = conn_pool_pg {
            let tb_meta = RdbUtil::get_tb_meta_pg(conn_pool, db_tb).await?;
            tb_meta.basic.cols.clone()
        } else {
            vec![]
        };
        Ok(cols)
    }

    fn get_conn_pool(&self, from: &str) -> (&Option<Pool<MySql>>, &Option<Pool<Postgres>>) {
        if from == SRC {
            (&self.src_conn_pool_mysql, &self.src_conn_pool_pg)
        } else {
            (&self.dst_conn_pool_mysql, &self.dst_conn_pool_pg)
        }
    }

    fn get_db_type(&self, from: &str) -> DbType {
        let config = TaskConfig::new(&self.base.task_config_file);
        if from == SRC {
            config.extractor_basic.db_type
        } else {
            config.sinker_basic.db_type
        }
    }
}
