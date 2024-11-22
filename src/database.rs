use clickhouse::{error::Error, Client};
use itertools::Itertools;

use crate::dota2::MatchDraft;

pub struct Database {
    database: String,
    client: Client,
}

impl Database {
    pub async fn new(
        server: &str,
        database: &str,
        user: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self, Error> {
        let database = database.to_string();
        let client = Client::default().with_url(server);

        let client = match user {
            Some(user) => client.with_user(user),
            _ => client,
        };

        let client = match password {
            Some(password) => client.with_password(password),
            _ => client,
        };

        // create database if not exists
        let query = format!("CREATE DATABASE IF NOT EXISTS {};", database);
        client.query(&query).execute().await?;

        let client = client.with_database(&database);

        let query = format!(
            "CREATE TABLE IF NOT EXISTS {}.drafts (
                match_id UInt64,
                radiant Tuple(UInt8, UInt8, UInt8, UInt8, UInt8),
                dire Tuple(UInt8, UInt8, UInt8, UInt8, UInt8),
            )
            ENGINE = MergeTree()
            ORDER BY match_id
            PARTITION BY intDiv(match_id, 10000000)
            PRIMARY KEY match_id;",
            &database
        );
        client.query(&query).execute().await?;

        Ok(Self { database, client })
    }

    pub async fn query_matches(
        &self,
        team1: &[u8],
        team2: &[u8],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MatchDraft>, Error> {
        let side_check = |side: &str, heroes: &[u8]| {
            format!(
                "(bitmapHasAll(bitmapBuild(array(untuple({}))), bitmapBuild([{}])))",
                side,
                heroes.iter().format(","),
            )
        };

        let (cond1, cond2) = match (team1.is_empty(), team2.is_empty()) {
            (true, true) => return Ok(vec![]),
            (true, false) => (side_check("radiant", team2), side_check("dire", team2)),
            (false, true) => (side_check("radiant", team1), side_check("dire", team1)),
            (false, false) => (
                format!(
                    "({} AND {})",
                    side_check("radiant", team1),
                    side_check("dire", team2)
                ),
                format!(
                    "({} AND {})",
                    side_check("radiant", team2),
                    side_check("dire", team1)
                ),
            ),
        };

        let query = format!(
            "SELECT ?fields FROM {}.drafts WHERE ({} OR {}) ORDER BY match_id DESC LIMIT {} OFFSET {}",
            self.database, cond1, cond2, limit, offset
        );
        self.client.query(&query).fetch_all().await
    }

    pub async fn save_match_masks(&self, drafts: &[MatchDraft]) -> Result<(), Error> {
        let mut insert = self.client.insert("drafts")?;
        for draft in drafts {
            insert.write(draft).await?;
        }
        insert.end().await?;
        Ok(())
    }
}
