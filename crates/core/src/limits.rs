//! Rate limits, in memory (one server process), in fixed windows. They slow
//! guessing down and never lock anyone out for good: a lockout would let
//! anyone lock a victim out. Limits are settings (`rate_limit_<name>`, "N/secs").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::time::Instant;

use crate::settings::Settings;

/// What is limited, and by what key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Limit {
    /// Login attempts from an address.
    LoginIp,
    /// Failed logins on an account (only failures: a victim keeps logging in).
    LoginAccount,
    /// Failed TOTP or recovery codes on an account.
    Totp,
    /// Emails asked for one address (registration, verification, reset).
    EmailAddress,
    /// Emails asked from one address (registration, verification, reset).
    EmailIp,
    /// Setup attempts from an address.
    SetupIp,
    /// Members added to an organization.
    Invitations,
}

impl Limit {
    pub const ALL: [Limit; 7] = [
        Limit::LoginIp,
        Limit::LoginAccount,
        Limit::Totp,
        Limit::EmailAddress,
        Limit::EmailIp,
        Limit::SetupIp,
        Limit::Invitations,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Limit::LoginIp => "login_ip",
            Limit::LoginAccount => "login_account",
            Limit::Totp => "totp",
            Limit::EmailAddress => "email_address",
            Limit::EmailIp => "email_ip",
            Limit::SetupIp => "setup_ip",
            Limit::Invitations => "invitations",
        }
    }
}

/// The counters, shared by every request.
#[derive(Debug, Clone, Default)]
pub struct Limits(Arc<Mutex<HashMap<(Limit, String), Window>>>);

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

/// Counters kept at most; beyond, finished windows are dropped.
const MAX_KEYS: usize = 100_000;

impl Limits {
    /// Counts one, then refuses beyond the limit: `Err(retry after)`.
    pub fn hit(&self, settings: &Settings, limit: Limit, key: &str) -> Result<(), Duration> {
        self.count(settings, limit, key, true)
    }

    /// Refuses if already beyond the limit, without counting (limits counting
    /// failures check first, and [`Self::fail`] on a failure).
    pub fn check(&self, settings: &Settings, limit: Limit, key: &str) -> Result<(), Duration> {
        self.count(settings, limit, key, false)
    }

    /// Counts a failure.
    pub fn fail(&self, settings: &Settings, limit: Limit, key: &str) {
        let _ = self.count(settings, limit, key, true);
    }

    fn count(
        &self,
        settings: &Settings,
        limit: Limit,
        key: &str,
        add: bool,
    ) -> Result<(), Duration> {
        let Some(&(max, window)) = settings.rate_limits.get(limit.name()) else {
            return Ok(());
        };
        let now = Instant::now();
        let mut windows = self.0.lock().expect("not poisoned");
        if windows.len() > MAX_KEYS {
            windows.retain(|_, w| now.duration_since(w.start) < window);
        }
        let entry = windows
            .entry((limit, key.to_lowercase()))
            .or_insert(Window {
                start: now,
                count: 0,
            });
        if now.duration_since(entry.start) >= window {
            *entry = Window {
                start: now,
                count: 0,
            };
        }
        if entry.count >= max {
            metrics::counter!("theca_rate_limited_total", "limit" => limit.name()).increment(1);
            return Err(window - now.duration_since(entry.start));
        }
        if add {
            entry.count += 1;
        }
        Ok(())
    }
}
