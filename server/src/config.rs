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
    pub github: GithubConfig,
}

/// A GitHub App: one app gives both "Connect GitHub" (its OAuth client id and
/// secret) and push notifications (its webhook secret). Each part switches on
/// by itself when its values are set.
#[derive(Clone, Debug)]
pub struct GithubConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub webhook_secret: Option<String>,
    /// The app's URL name, for the "install the app" link.
    pub app_slug: Option<String>,
    pub oauth_base: String,
    pub api_base: String,
}

impl Default for GithubConfig {
    fn default() -> GithubConfig {
        GithubConfig {
            client_id: None,
            client_secret: None,
            webhook_secret: None,
            app_slug: None,
            oauth_base: "https://github.com".into(),
            api_base: "https://api.github.com".into(),
        }
    }
}

impl GithubConfig {
    pub fn connect_enabled(&self) -> bool {
        self.client_id.is_some() && self.client_secret.is_some()
    }

    pub fn install_url(&self) -> Option<String> {
        let slug = self.app_slug.as_ref()?;
        // the slug ends up in a link, so it must be a plain GitHub name
        slug.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
            .then(|| format!("https://github.com/apps/{slug}/installations/new"))
    }
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
            github: GithubConfig {
                client_id: var("GITHUB_CLIENT_ID"),
                client_secret: var("GITHUB_CLIENT_SECRET"),
                webhook_secret: var("GITHUB_WEBHOOK_SECRET"),
                app_slug: var("GITHUB_APP_SLUG"),
                ..GithubConfig::default()
            },
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
