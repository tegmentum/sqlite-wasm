//! Capability + resource-limit policy for the sqlite-wasm reference
//! host. Mirrors the same types `sqlite-wasm-loader` defines on the
//! Rust side, so consumers can share `Policy` values between the
//! native-loader path and the in-WASM `.load`-driven path. The two
//! definitions live separately for now because the repos are
//! independent; folding them into a shared crate is the natural
//! follow-up.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Spi,
    Prepared,
    Transaction,
    Schema,
    State,
    Cache,
    Random,
    Text,
    Hashing,
    Encoding,
    Http,
}

#[derive(Debug, Clone, Default)]
pub struct HttpPolicy {
    pub allowed_hosts: Vec<String>,
    pub allowed_methods: Option<Vec<String>>,
    pub max_body_bytes: Option<u64>,
    pub timeout_ms: Option<u32>,
}

impl HttpPolicy {
    /// Wildcard-suffix host match. `*.example.com` matches
    /// `api.example.com` but not `example.com` itself.
    pub fn allows(&self, host: &str) -> bool {
        for entry in &self.allowed_hosts {
            if let Some(suffix) = entry.strip_prefix("*.") {
                if host.ends_with(suffix) && host.len() > suffix.len() {
                    return true;
                }
            } else if entry == host {
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    granted: HashSet<Capability>,
    pub http: Option<HttpPolicy>,
    pub fuel_per_call: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub epoch_deadline_ms: Option<u64>,
}

impl Policy {
    pub fn deny_all() -> Self {
        Self::default()
    }

    pub fn with_grants(mut self, caps: impl IntoIterator<Item = Capability>) -> Self {
        self.granted.extend(caps);
        self
    }

    pub fn with_http(mut self, http: HttpPolicy) -> Self {
        self.http = Some(http);
        self
    }

    pub fn with_fuel_per_call(mut self, fuel: u64) -> Self {
        self.fuel_per_call = Some(fuel);
        self
    }

    pub fn with_memory_limit_bytes(mut self, n: u64) -> Self {
        self.memory_limit_bytes = Some(n);
        self
    }

    pub fn with_epoch_deadline_ms(mut self, ms: u64) -> Self {
        self.epoch_deadline_ms = Some(ms);
        self
    }

    pub fn is_granted(&self, cap: Capability) -> bool {
        self.granted.contains(&cap)
    }

    /// Return missing capabilities, if any. Caller decides how to
    /// surface the refusal — extension-loader.load-extension wraps
    /// this in a `loader-error`.
    pub fn missing<'a>(&self, declared: &'a [Capability]) -> Vec<&'a Capability> {
        declared.iter().filter(|c| !self.granted.contains(c)).collect()
    }
}
