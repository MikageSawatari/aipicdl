//! HTTP 層。サイトに負荷をかけないよう、すべてのリクエストを 1 本のワーカースレッドから
//! 直列に投げ、種類ごと (GraphQL API / ファイル) に最低間隔を空ける。

use std::cell::Cell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::job::Reporter;

pub const API_ENDPOINT: &str = "https://api.aipictors.com/";
pub const USER_AGENT: &str = concat!(
    "aipicdl/",
    env!("CARGO_PKG_VERSION"),
    " (personal archiver; +https://github.com/MikageSawatari/aipicdl)"
);

const MAX_ATTEMPTS: u32 = 6;
const BODY_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

/// ユーザーが停止ボタンを押した。
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("中断されました")
    }
}
impl std::error::Error for Cancelled {}

/// リトライしても意味がない HTTP ステータス (404 など)。
#[derive(Debug)]
pub struct HttpStatus(pub u16);

impl fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}
impl std::error::Error for HttpStatus {}

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Api,
    File,
}

pub struct Client {
    agent: ureq::Agent,
    api_interval: Duration,
    file_interval: Duration,
    last_api: Cell<Option<Instant>>,
    last_file: Cell<Option<Instant>>,
    cancel: Arc<AtomicBool>,
    reporter: Arc<Reporter>,
}

pub struct Downloaded {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

impl Client {
    pub fn new(
        api_interval: Duration,
        file_interval: Duration,
        cancel: Arc<AtomicBool>,
        reporter: Arc<Reporter>,
    ) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
            api_interval,
            file_interval,
            last_api: Cell::new(None),
            last_file: Cell::new(None),
            cancel,
            reporter,
        }
    }

    pub fn reporter(&self) -> &Reporter {
        &self.reporter
    }

    pub fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    }

    /// 中断ボタンに反応できるよう、細切れに眠る。
    pub fn sleep(&self, d: Duration) -> Result<()> {
        let end = Instant::now() + d;
        loop {
            self.check_cancel()?;
            let now = Instant::now();
            if now >= end {
                return Ok(());
            }
            std::thread::sleep((end - now).min(Duration::from_millis(100)));
        }
    }

    fn throttle(&self, kind: Kind) -> Result<()> {
        let (cell, interval) = match kind {
            Kind::Api => (&self.last_api, self.api_interval),
            Kind::File => (&self.last_file, self.file_interval),
        };
        if let Some(last) = cell.get() {
            let elapsed = last.elapsed();
            if elapsed < interval {
                self.sleep(interval - elapsed)?;
            }
        }
        cell.set(Some(Instant::now()));
        Ok(())
    }

    /// 失敗時は待ち時間を伸ばしながらリトライする。429 / 503 の Retry-After も尊重する。
    fn execute(
        &self,
        kind: Kind,
        what: &str,
        send: &dyn Fn(&ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.throttle(kind)?;
            let (retry_after, err) = match send(&self.agent) {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if (200..300).contains(&status) {
                        return Ok(resp);
                    }
                    let retryable = status == 408 || status == 429 || status >= 500;
                    if !retryable {
                        return Err(anyhow::Error::new(HttpStatus(status))
                            .context(format!("{what} の取得に失敗")));
                    }
                    let ra = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    (ra, anyhow!("HTTP {status}"))
                }
                Err(e) => (None, anyhow!(e)),
            };
            if attempt >= MAX_ATTEMPTS {
                return Err(err.context(format!("{what} の取得に {attempt} 回失敗")));
            }
            let backoff =
                Duration::from_secs(10 * 2u64.pow(attempt - 1)).min(Duration::from_secs(300));
            let wait = retry_after.unwrap_or(backoff).max(backoff);
            self.reporter.log(format!(
                "  ! {what}: {err} — {} 秒後に再試行 ({attempt}/{MAX_ATTEMPTS})",
                wait.as_secs()
            ));
            self.sleep(wait)?;
        }
    }

    /// GraphQL クエリを投げて `data` を返す。部分的なエラーはログに出して `data` を優先する。
    pub fn graphql(&self, what: &str, query: &str, variables: Value) -> Result<Value> {
        let body = serde_json::to_string(&serde_json::json!({
            "query": query,
            "variables": variables,
        }))?;
        let mut resp = self.execute(Kind::Api, what, &|agent| {
            agent
                .post(API_ENDPOINT)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .send(body.as_str())
        })?;
        let v: Value = resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_json()?;
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        if let Some(errors) = v.get("errors").and_then(|e| e.as_array())
            && !errors.is_empty()
        {
            let msgs: Vec<String> = errors
                .iter()
                .map(|e| {
                    let m = e.get("message").and_then(|m| m.as_str()).unwrap_or("?");
                    match e.get("path") {
                        Some(p) => format!("{m} at {p}"),
                        None => m.to_string(),
                    }
                })
                .collect();
            if data.is_null() {
                return Err(anyhow!("{what}: GraphQL エラー: {}", msgs.join("; ")));
            }
            self.reporter
                .log(format!("  ! {what}: GraphQL 警告: {}", msgs.join("; ")));
        }
        Ok(data)
    }

    pub fn get(&self, kind: Kind, what: &str, url: &str) -> Result<Downloaded> {
        let mut resp = self.execute(kind, what, &|agent| agent.get(url).call())?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()?;
        Ok(Downloaded {
            bytes,
            content_type,
        })
    }

    pub fn get_text(&self, kind: Kind, what: &str, url: &str) -> Result<String> {
        let d = self.get(kind, what, url)?;
        Ok(String::from_utf8_lossy(&d.bytes).into_owned())
    }
}
