use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use lsp_share::config::{BinaryMatching, ClientRole};
use lsp_share::{control, daemon, mock_lsp, paths, proxy, shim};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "lsp-share",
    version,
    about = "Cross-platform LSP multiplexer for sharing language servers between editors and agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as a transparent stdio LSP proxy connected to the shared daemon
    Proxy(ProxyArgs),
    /// Run the background LSP multiplexer daemon
    Daemon(DaemonArgs),
    /// Show running daemon status, LSP instances, and connected clients
    Status(StatusArgs),
    /// Stop one LSP instance, all instances, or the daemon itself
    Stop(StopArgs),
    /// Restart an LSP instance in-place while preserving open documents
    Restart(RestartArgs),
    /// Manage transparent argv[0] binary shims
    Shim(ShimArgs),
    #[command(name = "__mock-lsp", hide = true)]
    MockLsp(MockLspArgs),
}

#[derive(Args)]
struct ProxyArgs {
    /// Client role hint (auto-detected from initialize.clientInfo if omitted)
    #[arg(long, value_enum, default_value_t = ClientRole::Auto)]
    role: ClientRole,
    /// Override server matching strategy (default: name)
    #[arg(long, value_enum)]
    match_by: Option<BinaryMatching>,
    /// Explicit logical instance key override
    #[arg(long)]
    key: Option<String>,
    /// Explicit workspace root override
    #[arg(long)]
    root: Option<String>,
    /// Optional JSONL wire-tap trace output file
    #[arg(long)]
    trace: Option<PathBuf>,
    /// Optional config.toml path
    #[arg(long)]
    config: Option<PathBuf>,
    /// Real language server command and its arguments (after `--`)
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    target: Vec<String>,
}

#[derive(Args)]
struct DaemonArgs {
    /// Run in the foreground instead of detaching
    #[arg(long)]
    foreground: bool,
    /// Path to config.toml
    #[arg(long)]
    config: Option<PathBuf>,
    /// Idle timeout in seconds before shutting down unused LSP servers
    #[arg(long)]
    idle_timeout: Option<u64>,
    /// Log level (error, warn, info, debug, trace)
    #[arg(long)]
    log_level: Option<String>,
    /// Optional JSONL wire-tap trace file
    #[arg(long)]
    trace: Option<PathBuf>,
}

#[derive(Args)]
struct StatusArgs {
    /// Output machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct StopArgs {
    /// Specific instance ID to stop
    instance_id: Option<String>,
    /// Stop all running LSP instances
    #[arg(long)]
    all: bool,
    /// Stop all LSP instances and shut down the daemon
    #[arg(long)]
    daemon: bool,
}

#[derive(Args)]
struct RestartArgs {
    /// Instance ID to restart
    instance_id: String,
}

#[derive(Args)]
struct ShimArgs {
    #[command(subcommand)]
    action: ShimAction,
}

#[derive(Subcommand)]
enum ShimAction {
    /// Add a transparent argv[0] shim (in ~/.lsp-share/bin by default, or any custom path)
    Add {
        /// Server name (e.g. `clangd`) or destination shim file path (e.g. `D:\shims\clangd.exe`)
        name: String,
        /// Custom output file path or directory where the shim binary should be created
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        #[arg(long)]
        target: Option<PathBuf>,
        #[arg(long, value_enum)]
        role: Option<ClientRole>,
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List configured shims
    List {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Remove a shim
    Remove {
        name: String,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Args)]
struct MockLspArgs {
    #[arg(long)]
    log: PathBuf,
    #[arg(long)]
    label: Option<String>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    _extra: Vec<String>,
}

fn should_dispatch_as_shim(argv0: Option<&str>) -> Option<String> {
    let raw = argv0?;
    let file_name = Path::new(raw).file_name()?.to_str()?;
    let norm = paths::extract_server_name(file_name);
    if norm.is_empty() || norm == "lsp-share" || norm.starts_with("lsp_share-") {
        None
    } else {
        Some(norm)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut raw_args: Vec<String> = std::env::args().collect();
    if std::env::var_os("LSP_SHARE_SPAWNED_BY_DAEMON").is_some()
        && raw_args.get(1).map(String::as_str) != Some("__mock-lsp")
    {
        let argv0 = raw_args.first().map(String::as_str).unwrap_or("lsp-share");
        eprintln!(
            "lsp-share recursive spawn error: target binary '{argv0}' is a copy/shim of lsp-share itself, not the real LSP executable. Pass `--target <real-lsp-path>` to `lsp-share shim add` or point `lsp-share proxy` to the real binary."
        );
        std::process::exit(1);
    }
    let argv0 = raw_args.first().map(String::as_str);
    if let Some(invoked_name) = should_dispatch_as_shim(argv0) {
        let pass_args = if raw_args.len() > 1 {
            raw_args.split_off(1)
        } else {
            Vec::new()
        };
        shim::run_as_shim(&invoked_name, pass_args).await?;
        // Exit immediately so Tokio runtime drop never blocks on tokio::io::stdin() thread.
        std::process::exit(0);
    }

    let cli = Cli::parse();
    match cli.command {
        Commands::Proxy(args) => {
            let mut iter = args.target.into_iter();
            let command = iter.next().expect("clap enforces required target");
            let rest: Vec<String> = iter.collect();
            proxy::run_proxy(proxy::ProxyOptions {
                role: args.role,
                match_by: args.match_by,
                key: args.key,
                root: args.root,
                trace: args.trace,
                config: args.config,
                command,
                args: rest,
            })
            .await?;
            std::process::exit(0);
        }
        Commands::Daemon(args) => {
            daemon::run_daemon(
                args.config.as_deref(),
                args.idle_timeout,
                args.log_level.as_deref(),
                args.trace.as_deref(),
                args.foreground,
            )
            .await
        }
        Commands::Status(args) => control::run_status(args.json).await,
        Commands::Stop(args) => control::run_stop(args.instance_id, args.all, args.daemon).await,
        Commands::Restart(args) => control::run_restart(args.instance_id).await,
        Commands::Shim(args) => match args.action {
            ShimAction::Add {
                name,
                output,
                target,
                role,
                key,
                config,
            } => shim::add_shim(&name, output, target, role, key, config.as_deref()),
            ShimAction::List { config } => shim::list_shims(config.as_deref()),
            ShimAction::Remove { name, config } => shim::remove_shim(&name, config.as_deref()),
        },
        Commands::MockLsp(args) => mock_lsp::run_mock_lsp(args.log, args.label).await,
    }
}
