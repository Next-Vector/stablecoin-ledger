//! Service configuration from the environment, validated at boot: a bad value stops the process
//! with every problem listed, instead of failing on the first request.

use std::fmt;
use std::net::SocketAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub log_format: LogFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub problems: Vec<String>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid configuration: {}", self.problems.join("; "))
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    const DEFAULT_BIND_ADDR: &'static str = "0.0.0.0:8080";

    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|key| std::env::var(key).ok())
    }

    /// Reads configuration through `var`, so tests need not touch the process environment.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut problems = Vec::new();

        let database_url = var("DATABASE_URL").unwrap_or_default();
        if database_url.is_empty() {
            problems.push("DATABASE_URL is required".to_owned());
        } else if !(database_url.starts_with("postgres://")
            || database_url.starts_with("postgresql://"))
        {
            problems.push("DATABASE_URL must be a postgres:// URL".to_owned());
        }

        let raw_bind = var("BIND_ADDR").unwrap_or_else(|| Self::DEFAULT_BIND_ADDR.to_owned());
        let bind_addr = raw_bind.parse().unwrap_or_else(|_| {
            problems.push(format!(
                "BIND_ADDR {raw_bind:?} is not a host:port socket address"
            ));
            Self::DEFAULT_BIND_ADDR.parse().expect("valid default")
        });

        let log_format = match var("LOG_FORMAT").as_deref() {
            None | Some("json") => LogFormat::Json,
            Some("pretty") => LogFormat::Pretty,
            Some(other) => {
                problems.push(format!("LOG_FORMAT {other:?} must be json or pretty"));
                LogFormat::Json
            }
        };

        if problems.is_empty() {
            Ok(Self {
                database_url,
                bind_addr,
                log_format,
            })
        } else {
            Err(ConfigError { problems })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn reads_required_values_and_applies_defaults() {
        let config =
            Config::from_vars(vars(&[("DATABASE_URL", "postgres://u:p@db/ledger")])).unwrap();
        assert_eq!(config.database_url, "postgres://u:p@db/ledger");
        assert_eq!(config.bind_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.log_format, LogFormat::Json);
    }

    #[test]
    fn reads_optional_overrides() {
        let config = Config::from_vars(vars(&[
            ("DATABASE_URL", "postgres://u:p@db/ledger"),
            ("BIND_ADDR", "127.0.0.1:9000"),
            ("LOG_FORMAT", "pretty"),
        ]))
        .unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(config.log_format, LogFormat::Pretty);
    }

    #[test]
    fn reports_every_problem_at_once() {
        let error = Config::from_vars(vars(&[
            ("BIND_ADDR", "not-an-address"),
            ("LOG_FORMAT", "xml"),
        ]))
        .unwrap_err();
        assert_eq!(error.problems.len(), 3, "{error}");
        let message = error.to_string();
        assert!(message.contains("DATABASE_URL is required"), "{message}");
        assert!(message.contains("BIND_ADDR"), "{message}");
        assert!(message.contains("LOG_FORMAT"), "{message}");
    }

    #[test]
    fn rejects_a_database_url_that_is_not_postgres() {
        let error = Config::from_vars(vars(&[("DATABASE_URL", "mysql://db/ledger")])).unwrap_err();
        assert!(error.to_string().contains("DATABASE_URL"), "{error}");
    }
}
