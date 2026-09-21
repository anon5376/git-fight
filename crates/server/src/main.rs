use clap::Parser;
use git_fight_server::Config;
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
    #[arg(long, default_value = "sqlite://data/git-fight.db")]
    db: String,
    /// Directory of the Vite build (index.html + assets).
    #[arg(long)]
    r#static: Option<PathBuf>,
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
    let config = Config {
        lag: Duration::from_millis(args.lag_ms),
        instant: false,
        static_dir: args.r#static.or_else(|| {
            let p = PathBuf::from("web/dist");
            p.exists().then_some(p)
        }),
        expire_secs: git_fight_server::protocol::EXPIRE_SECS,
        disconnect: Duration::from_secs(git_fight_server::protocol::DISCONNECT_SECS),
    };
    let listener = TcpListener::bind(args.bind).await.unwrap_or_else(|e| {
        eprintln!("bind: {e}");
        std::process::exit(1);
    });
    eprintln!("git-fight-server on http://{}", args.bind);
    if let Err(e) = git_fight_server::serve(listener, pool, config).await {
        eprintln!("server: {e}");
        std::process::exit(1);
    }
}
