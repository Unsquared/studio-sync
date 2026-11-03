// The CLI's side of the daemon's HTTP API.

use crate::paths::PORT;
use crate::util::{Error, Res};
use serde_json::Value;
use std::sync::LazyLock;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

// The OS's own TLS and certificates (Security.framework, SChannel): no C to build.
static AGENT: LazyLock<Agent> = LazyLock::new(|| {
	let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
	Agent::new_with_config(Agent::config_builder().http_status_as_error(false).tls_config(tls).build())
});

pub const NOT_RUNNING: &str = "the daemon is not running: `studio-sync install` (or `studio-sync serve` in a terminal)";

fn url(route: &str) -> String {
	format!("http://127.0.0.1:{}{route}", *PORT)
}

fn finish(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Res<(u16, String)> {
	let mut response = response.map_err(|_| Error(NOT_RUNNING.into()))?;
	let status = response.status().as_u16();
	let body = response.body_mut().with_config().limit(u64::MAX).read_to_string().map_err(|e| Error(e.to_string()))?;
	Ok((status, body))
}

pub fn get(route: &str) -> Res<(u16, String)> {
	finish(AGENT.get(url(route)).call())
}

pub fn post(route: &str) -> Res<(u16, String)> {
	finish(AGENT.post(url(route)).send_empty())
}

pub fn status() -> Res<Value> {
	Ok(serde_json::from_str(&get("/status")?.1)?)
}

// A query string from pairs, percent-encoded.
pub fn query(pairs: &[(&str, &str)]) -> String {
	pairs.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect::<Vec<_>>().join("&")
}

pub fn encode(text: &str) -> String {
	text.bytes()
		.map(|b| match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
			_ => format!("%{b:02X}"),
		})
		.collect()
}

// HTTPS, for Roblox's public APIs.
pub fn fetch_json(url: &str) -> Res<(u16, Value)> {
	let mut response = AGENT.get(url).call().map_err(|e| Error(format!("{url}: {e}")))?;
	let status = response.status().as_u16();
	let body = response.body_mut().read_to_string().map_err(|e| Error(e.to_string()))?;
	Ok((status, serde_json::from_str(&body).unwrap_or(Value::Null)))
}

#[cfg(test)]
mod tests {
	// Needs the network: cargo test -- --ignored
	#[test]
	#[ignore]
	fn https_through_the_os() {
		let (status, body) = super::fetch_json("https://apis.roblox.com/universes/v1/places/1818/universe").unwrap();
		assert_eq!(status, 200, "{body}");
		assert!(body["universeId"].as_u64().is_some(), "{body}");
	}
}
