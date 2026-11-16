//! A load generator for `POST /v1/transactions`: transfers of 1 base unit between uniformly chosen
//! user wallets, measured either at full speed (throughput, NFR-04) or at a fixed rate (latency,
//! NFR-05).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// The asset every benchmark transfer moves.
pub const ASSET: &str = "USDC@ethereum/6";
/// What each wallet is seeded with: enough that 1-unit transfers never run dry.
const SEED_AMOUNT: u64 = 1_000_000_000;
/// Postings per seeding transaction (the ledger's limit).
const SEED_BATCH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Base URL of the ledger API, e.g. `http://127.0.0.1:8080`.
    pub url: String,
    /// An API key with the `write` role.
    pub key: String,
    /// User wallets `bench:{i}:wallet` transfers are spread over (≥ 2).
    pub accounts: usize,
    /// Requests in flight at full speed.
    pub concurrency: usize,
    /// How long each measurement runs.
    pub duration: Duration,
    /// Requests per second for the latency run; without it only throughput is measured.
    pub rate: Option<u64>,
}

pub const USAGE: &str = "usage: ledger-bench --url <base-url> --key <write-api-key> \
[--accounts N (10000)] [--concurrency N (64)] [--duration SECONDS (30)] [--rate REQUESTS_PER_SECOND]";

impl Config {
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let (mut url, mut key) = (None, None);
        let mut config = Config {
            url: String::new(),
            key: String::new(),
            accounts: 10_000,
            concurrency: 64,
            duration: Duration::from_secs(30),
            rate: None,
        };
        let number = |flag: &str, value: &str| -> Result<u64, String> {
            value
                .parse::<u64>()
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| format!("{flag} must be a positive number"))
        };
        for pair in args.chunks(2) {
            match pair {
                [flag, value] if flag == "--url" => url = Some(value.clone()),
                [flag, value] if flag == "--key" => key = Some(value.clone()),
                [flag, value] if flag == "--accounts" => {
                    config.accounts = number(flag, value)? as usize;
                }
                [flag, value] if flag == "--concurrency" => {
                    config.concurrency = number(flag, value)? as usize;
                }
                [flag, value] if flag == "--duration" => {
                    config.duration = Duration::from_secs(number(flag, value)?);
                }
                [flag, value] if flag == "--rate" => config.rate = Some(number(flag, value)?),
                _ => return Err(format!("unexpected arguments {pair:?}")),
            }
        }
        config.url = url.ok_or("--url is required")?;
        config.key = key.ok_or("--key is required")?;
        if config.accounts < 2 {
            return Err("--accounts must be at least 2".to_owned());
        }
        Ok(config)
    }
}

/// Latency percentiles (nearest rank).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Latency {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Latency {
    pub fn from_samples(mut samples: Vec<Duration>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        let rank = |p: usize| samples[(samples.len() * p).div_ceil(100).max(1) - 1];
        Self {
            p50: rank(50),
            p95: rank(95),
            p99: rank(99),
            max: samples[samples.len() - 1],
        }
    }
}

/// The outcome of one measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub succeeded: u64,
    pub failed: u64,
    pub elapsed: Duration,
    pub latency: Latency,
}

impl Report {
    /// Successful transfers per second.
    pub fn throughput(&self) -> f64 {
        self.succeeded as f64 / self.elapsed.as_secs_f64()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        write!(
            f,
            "{:.0} transfers/s ({} ok, {} failed in {:.1} s); latency p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
            self.throughput(),
            self.succeeded,
            self.failed,
            self.elapsed.as_secs_f64(),
            ms(self.latency.p50),
            ms(self.latency.p95),
            ms(self.latency.p99),
            ms(self.latency.max),
        )
    }
}

/// Uniformly random `(from, to)` account pairs, never a self-transfer (xorshift64*: fast and
/// reproducible; statistical quality is plenty for spreading load).
#[derive(Debug, Clone)]
pub struct Pairs {
    accounts: usize,
    state: u64,
}

impl Pairs {
    pub fn new(accounts: usize, seed: u64) -> Self {
        Self {
            accounts,
            state: seed.max(1),
        }
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn next_pair(&mut self) -> (usize, usize) {
        let n = self.accounts as u64;
        let from = self.next() % n;
        // Pick among the other n - 1 accounts, so `to` is uniform and never `from`.
        let to = (from + 1 + self.next() % (n - 1)) % n;
        (from as usize, to as usize)
    }
}

fn wallet(i: usize) -> String {
    format!("bench:{i}:wallet")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(1024)
        .build()
        .expect("a default HTTP client")
}

async fn post(client: &reqwest::Client, config: &Config, body: String) -> Result<(), String> {
    let response = client
        .post(format!("{}/v1/transactions", config.url))
        .bearer_auth(&config.key)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!(
            "{status}: {}",
            response.text().await.unwrap_or_default()
        ))
    }
}

/// Funds every wallet from `world`, 256 postings per transaction.
pub async fn seed(config: &Config) -> Result<(), String> {
    let client = client();
    let accounts: Vec<usize> = (0..config.accounts).collect();
    for batch in accounts.chunks(SEED_BATCH) {
        let postings: Vec<_> = batch
            .iter()
            .map(|&i| json!({"source": "world", "destination": wallet(i), "amount": SEED_AMOUNT.to_string(), "asset": ASSET}))
            .collect();
        post(&client, config, json!({ "postings": postings }).to_string()).await?;
    }
    Ok(())
}

/// Sends transfers for `config.duration`: as fast as `config.concurrency` allows when `rate` is
/// `None`, otherwise at `rate` requests per second (open loop: latency includes queueing).
pub async fn run(config: &Config, rate: Option<u64>) -> Result<Report, String> {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let pairs = Arc::new(Mutex::new(Pairs::new(config.accounts, seed)));
    let samples = Arc::new(Mutex::new(Vec::new()));
    let failed = Arc::new(AtomicU64::new(0));
    let client = client();
    let started = Instant::now();
    let deadline = started + config.duration;

    let transfer = {
        let (pairs, samples, failed, client, config) = (
            pairs.clone(),
            samples.clone(),
            failed.clone(),
            client.clone(),
            config.clone(),
        );
        move || {
            let (pairs, samples, failed, client, config) = (
                pairs.clone(),
                samples.clone(),
                failed.clone(),
                client.clone(),
                config.clone(),
            );
            async move {
                let (from, to) = pairs.lock().await.next_pair();
                let body = json!({"postings": [{"source": wallet(from), "destination": wallet(to), "amount": "1", "asset": ASSET}]});
                let sent = Instant::now();
                match post(&client, &config, body.to_string()).await {
                    Ok(()) => samples.lock().await.push(sent.elapsed()),
                    Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    };

    let mut tasks = JoinSet::new();
    match rate {
        None => {
            for _ in 0..config.concurrency {
                let transfer = transfer.clone();
                tasks.spawn(async move {
                    while Instant::now() < deadline {
                        transfer().await;
                    }
                });
            }
        }
        Some(rate) => {
            let mut ticks = tokio::time::interval(Duration::from_secs_f64(1.0 / rate as f64));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
            loop {
                ticks.tick().await;
                if Instant::now() >= deadline {
                    break;
                }
                tasks.spawn(transfer());
            }
        }
    }
    while let Some(done) = tasks.join_next().await {
        done.map_err(|error| error.to_string())?;
    }
    let elapsed = started.elapsed();
    let samples = std::mem::take(&mut *samples.lock().await);
    Ok(Report {
        succeeded: samples.len() as u64,
        failed: failed.load(Ordering::Relaxed),
        elapsed,
        latency: Latency::from_samples(samples),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let latency = Latency::from_samples(samples);
        assert_eq!(latency.p50, Duration::from_millis(50));
        assert_eq!(latency.p99, Duration::from_millis(99));
        assert_eq!(latency.max, Duration::from_millis(100));
        assert_eq!(Latency::from_samples(Vec::new()).p99, Duration::ZERO);
    }

    #[test]
    fn pairs_are_uniform_and_never_self_transfers() {
        let mut pairs = Pairs::new(10, 7);
        let mut seen = vec![0u32; 10];
        for _ in 0..10_000 {
            let (from, to) = pairs.next_pair();
            assert_ne!(from, to);
            assert!(from < 10 && to < 10);
            seen[from] += 1;
        }
        // Every account is a source roughly 1,000 times out of 10,000.
        assert!(seen.iter().all(|&n| (800..1200).contains(&n)), "{seen:?}");
    }

    #[test]
    fn arguments_have_defaults_and_overrides() {
        let config = Config::from_args(&args(&["--url", "http://x", "--key", "sl_k"])).unwrap();
        assert_eq!(config.url, "http://x");
        assert_eq!(config.accounts, 10_000);
        assert_eq!(config.concurrency, 64);
        assert_eq!(config.duration, Duration::from_secs(30));
        assert_eq!(config.rate, None);

        let config = Config::from_args(&args(&[
            "--url",
            "http://x",
            "--key",
            "sl_k",
            "--accounts",
            "50",
            "--concurrency",
            "8",
            "--duration",
            "5",
            "--rate",
            "1000",
        ]))
        .unwrap();
        assert_eq!(
            (
                config.accounts,
                config.concurrency,
                config.duration,
                config.rate
            ),
            (50, 8, Duration::from_secs(5), Some(1000))
        );

        for bad in [
            &["--key", "sl_k"][..],
            &["--url", "http://x"],
            &["--url", "http://x", "--key", "k", "--accounts", "1"],
            &["--url", "http://x", "--key", "k", "--rate", "0"],
            &["--url", "http://x", "--key", "k", "--colour", "red"],
        ] {
            assert!(Config::from_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_owned()).collect()
    }
}
