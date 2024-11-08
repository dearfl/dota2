use std::{collections::HashMap, num::NonZeroU8, time::Duration};

use itertools::Itertools;
use tokio::time::Instant;

use crate::{
    args::Args,
    client::{Client, RequestError},
    database::Database,
    dota2::{full, MatchMask},
};

pub struct RateControl {
    // maybe we could do pwm control?
    // this is way too simple
    interval: Duration,
    min_interval: Duration,
    max_interval: Duration,
    last_timestamp: Instant,
}

impl RateControl {
    pub fn new(min: u64, max: u64) -> Self {
        let min_interval = Duration::from_millis(min);
        let max_interval = Duration::from_millis(max);
        let interval = min_interval * 2;
        let last_timestamp = Instant::now();
        Self {
            interval,
            min_interval,
            max_interval,
            last_timestamp,
        }
    }

    pub async fn wait(&mut self) {
        log::debug!("rate control: waiting {}ms!", self.interval.as_millis());
        tokio::time::sleep_until(self.last_timestamp + self.interval).await;
        self.last_timestamp = Instant::now();
    }

    pub fn speed_up(&mut self) {
        self.interval = std::cmp::max(self.interval * 9 / 10, self.min_interval);
    }

    pub fn slow_down(&mut self) {
        self.interval = std::cmp::min(self.interval * 2, self.max_interval);
    }
}

pub struct Collector {
    match_seq_num: u64,
    rate: RateControl,
    database: Database,
    indices: HashMap<NonZeroU8, Vec<MatchMask>>,
    keys: Vec<String>,
    batch: usize,
    proxy: Option<String>,
}

impl Collector {
    pub async fn new(args: Args) -> anyhow::Result<Self> {
        let match_seq_num = args.start_idx;
        let rate = RateControl::new(args.min_interval, args.max_interval);
        let indices = HashMap::with_capacity(256 * 2);
        let database = Database::new(
            args.clickhouse_server.as_deref(),
            args.clickhouse_database.as_deref(),
            args.clickhouse_user.as_deref(),
            args.clickhouse_password.as_deref(),
        )
        .await?;
        let keys = args.keys;
        let proxy = args.proxy;
        let batch = args.insert_batch_size;

        Ok(Self {
            match_seq_num,
            rate,
            database,
            indices,
            keys,
            proxy,
            batch,
        })
    }

    fn collect(&mut self, matches: &full::MatchHistory) -> u64 {
        // collect a single batch
        // do we want to do anything else?
        matches
            .matches
            .iter()
            .fold(self.match_seq_num, |init, mat| {
                // update indices
                let mask = mat.into();
                mat.players
                    .iter()
                    .filter_map(|p| NonZeroU8::new(p.hero_id))
                    .for_each(|key| self.indices.entry(key).or_default().push(mask));
                // calculate the new match_seq_num
                std::cmp::max(init, mat.match_seq_num + 1)
            })
    }

    pub async fn request(&mut self, client: &Client) -> anyhow::Result<()> {
        // get_match_history is not reliable so we switch back to get_match_history_by_seq_num
        match client.get_match_history_full(self.match_seq_num, 100).await {
            Ok(matches) => {
                // match_seq_num range of current batch: [left, right)
                let (left, right) = (self.match_seq_num, self.collect(&matches));
                let count = matches.matches.len();
                log::debug!("retrived {} matches from [{}, {}).", count, left, right);

                // update match_seq_num
                self.match_seq_num = right;

                self.rate.speed_up();
                if matches.matches.len() < 100 {
                    // this means we're reaching the newest matches, so slowing down a bit
                    self.rate.slow_down();
                }
            }
            Err(RequestError::DecodeError(err, content)) => {
                // maybe valve have changed the json response format
                // this is when things really goes wrong, we need to fix it manually
                log::error!("DecodeError: {}", err);
                log::info!("Saving response to {}-error.json", self.match_seq_num);
                let filename = format!("{}-error.json", self.match_seq_num);
                std::fs::write(filename, content)?;
                // we have to quit or else we'll end in a dead loop
                // we could in theory accept unknown fields so we don't have to quit here
                // but we don't want to
                return Err(err.into());
            }
            Err(error) => {
                // similar connection errors
                log::warn!("RequestError: {}", error);
                self.rate.slow_down();
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        Ok(())
    }

    pub async fn save(&mut self) -> anyhow::Result<()> {
        // saving the full result uses way too much storage space which we can't afford!
        log::debug!("saving indices to database!");
        for (key, masks) in self.indices.iter_mut() {
            self.database.save_indexed_masks(key.get(), masks).await?;
            // clear masks instead of indices so less alloction happens
            masks.clear();
        }
        Ok(())
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        // construct clients here because we don't want to do
        // self-referential stuff
        let clients = self
            .keys
            .iter()
            .map(|key| Client::new(key, self.proxy.as_deref()))
            .collect::<Result<Vec<_>, _>>()?;

        for clts in clients.iter().cycle().chunks(self.batch).into_iter() {
            for clt in clts {
                self.rate.wait().await;
                if let Err(err) = self.request(clt).await {
                    // request will only fail when decode error happened
                    // in case this happens, we still want to save requested matches
                    self.save().await?;
                    return Err(err);
                }
            }
            // save to clickhouse every <batch> requests
            self.save().await?;
        }

        Ok(())
    }
}
