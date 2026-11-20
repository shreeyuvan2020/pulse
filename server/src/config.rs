use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    /// Public address of this server without a trailing slash, such as
    /// `https://pulse.example.org`. Used for login redirects and to check where
    /// browser requests come from.
    pub base_url: String,
    /// Hack Club Auth. The staging instance is the default; production is
    /// `https://auth.hackclub.com`.
    pub hca_base: String,
    pub hca_client_id: Option<String>,
    pub hca_client_secret: Option<String>,
    /// Lets anyone sign in as any name, for development. Only honoured when
    /// `base_url` points at this computer.
    pub dev_login: bool,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let bind = var("PULSE_BIND").unwrap_or_else(|| "127.0.0.1:8080".into());
        Ok(Config {
            bind: bind
                .parse()
                .map_err(|_| format!("PULSE_BIND is not an address like 127.0.0.1:8080: {bind}"))?,
            database_url: var("DATABASE_URL").ok_or("DATABASE_URL is not set")?,
            base_url: var("PULSE_BASE_URL")
                .ok_or("PULSE_BASE_URL is not set (the public address of this server)")?
                .trim_end_matches('/')
                .to_owned(),
            hca_base: var("HCA_BASE_URL")
                .unwrap_or_else(|| "https://hca.dinosaurbbq.org".into())
                .trim_end_matches('/')
                .to_owned(),
            hca_client_id: var("HCA_CLIENT_ID"),
            hca_client_secret: var("HCA_CLIENT_SECRET"),
            dev_login: var("PULSE_DEV_LOGIN").is_some_and(|value| value == "1"),
        })
    }

    pub fn dev_login_allowed(&self) -> bool {
        self.dev_login && points_at_this_computer(&self.base_url)
    }

    pub fn secure_cookies(&self) -> bool {
        self.base_url.starts_with("https://")
    }
}

fn points_at_this_computer(url: &str) -> bool {
    let host = url.split_once("://").map_or(url, |(_, rest)| rest);
    ["localhost", "127.0.0.1", "[::1]"].iter().any(|local| {
        host.strip_prefix(local)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
    })
}

#[cfg(test)]
mod tests {
    use super::points_at_this_computer;

    #[test]
    fn dev_login_needs_a_local_address() {
        for local in [
            "http://localhost:8080",
            "http://127.0.0.1",
            "http://[::1]:3000/",
        ] {
            assert!(points_at_this_computer(local), "{local}");
        }
        // lookalikes must not pass
        for remote in [
            "https://pulse.example.org",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com:80",
            "https://evil.com/localhost",
        ] {
            assert!(!points_at_this_computer(remote), "{remote}");
        }
    }
}
