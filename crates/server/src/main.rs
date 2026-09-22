use clap::Parser;
use git_fight_server::Config;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;

fn sqlite_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("sqlite://") {
        if rest.starts_with('/') || rest == ":memory:" || rest.starts_with(":memory:") {
            return url.to_string();
        }
        return format!("sqlite:{rest}");
    }
    url.to_string()
}

#[derive(Parser, Debug)]
#[command(name = "git-fight-server", about = "Online match rooms for git fight.")]
struct Args {
    /// Fake one-way latency applied to WebSocket messages.
    #[arg(long, default_value_t = 0)]
    lag_ms: u64,
    /// Bind address.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,
    /// SQLite URL, e.g. sqlite://data/git-fight.db
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "sqlite://data/git-fight.db"
    )]
    db: String,
    /// Directory of the Vite build (index.html + assets).
    #[arg(long)]
    r#static: Option<PathBuf>,
    /// Confirm ticks as fast as inputs arrive (lockstep tests, not production).
    #[arg(long, default_value_t = false)]
    instant: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if args.db.starts_with("sqlite://") {
        if let Some(rest) = args.db.strip_prefix("sqlite://") {
            let path = rest.split('?').next().unwrap_or(rest);
            if let Some(dir) = std::path::Path::new(path).parent() {
                if !dir.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(dir);
                }
            }
        }
    }
    let pool = git_fight_server::db_connect(&sqlite_url(&args.db))
        .await
        .unwrap_or_else(|e| {
            eprintln!("database: {e}");
            std::process::exit(1);
        });
    let mut config = Config {
        lag: Duration::from_millis(args.lag_ms),
        instant: args.instant,
        static_dir: args.r#static.or_else(|| {
            let p = PathBuf::from("web/dist");
            p.exists().then_some(p)
        }),
        expire_secs: git_fight_server::protocol::EXPIRE_SECS,
        disconnect: Duration::from_secs(git_fight_server::protocol::DISCONNECT_SECS),
        ..Config::default()
    };
    config.auth.public_url =
        env::var("GIT_FIGHT_PUBLIC_URL").unwrap_or_else(|_| format!("http://{}", args.bind));
    if let Ok(key) = env::var("SESSION_KEY") {
        config.auth.session_key = key.into_bytes();
    }
    config.webhook_secret = env::var("GITHUB_WEBHOOK_SECRET")
        .ok()
        .map(|s| s.into_bytes());
    config.github = match git_fight_server::github_from_env() {
        Ok(gh) => gh,
        Err(name) => {
            eprintln!("{name} is required when GitHub App credentials are set");
            std::process::exit(1);
        }
    };
    if let Err(name) = config.require_live_github_secrets() {
        eprintln!("{name} is required when GitHub App credentials are set");
        std::process::exit(1);
    }
    let listener = TcpListener::bind(args.bind).await.unwrap_or_else(|e| {
        eprintln!("bind: {e}");
        std::process::exit(1);
    });
    let addr = listener.local_addr().unwrap_or(args.bind);
    eprintln!("git-fight-server on http://{addr}");
    if let Err(e) = git_fight_server::serve(listener, pool, config).await {
        eprintln!("server: {e}");
        std::process::exit(1);
    }
}
