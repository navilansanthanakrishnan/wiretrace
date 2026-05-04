use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "agent-mcp-b",
    version,
    about = "Terminal-first HTTP(S) interception tool with app launch helpers."
)]
pub struct Cli {
    #[arg(
        long,
        env = "AGENT_MCP_B_LOG",
        default_value = "agent_mcp_b=info,proxy=off,hudsucker=off",
        help = "Tracing filter directive, for example info or agent_mcp_b=debug"
    )]
    pub log_filter: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the interception proxy without launching an app.
    Proxy(ProxyCommand),
    /// Launch Chrome through the local interception proxy.
    Chrome(ChromeCommand),
    /// Start the interception proxy and attach already-open macOS apps via the system proxy.
    Attach(AttachCommand),
    /// Install or inspect the local CA used for HTTPS interception.
    Ca(CaCommand),
    /// Print paths used by the application runtime.
    Paths,
}

#[derive(Debug, Clone, Args)]
pub struct ProxyCommand {
    #[arg(
        long,
        default_value = "127.0.0.1:8787",
        help = "Socket address to bind the local proxy to"
    )]
    pub listen: SocketAddr,

    #[arg(long, value_enum, default_value_t = OutputMode::Focused)]
    pub output: OutputMode,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Only emit requests whose host contains one of these values"
    )]
    pub host_contains: Vec<String>,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Only emit requests whose URL contains one of these values"
    )]
    pub url_contains: Vec<String>,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Only emit requests whose HTTP method matches one of these values"
    )]
    pub methods: Vec<String>,

    #[arg(
        long,
        default_value_t = 8192,
        help = "Maximum number of request or response body bytes to print"
    )]
    pub body_preview_bytes: usize,

    #[arg(
        long,
        default_value_t = false,
        help = "Print raw CONNECT tunnel setup requests in addition to intercepted HTTP requests"
    )]
    pub show_connect: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = InteractionMode::Off,
        help = "Capture every matching request, or only requests that begin within a manually armed interaction window"
    )]
    pub interaction_mode: InteractionMode,

    #[arg(
        long,
        default_value_t = 4000,
        help = "When interaction mode is manual, capture requests that begin within this many milliseconds after you arm the window"
    )]
    pub interaction_window_ms: u64,
}

#[derive(Debug, Clone, Args)]
pub struct ChromeCommand {
    #[command(flatten)]
    pub proxy: ProxyCommand,

    #[arg(
        long,
        default_value = "https://example.com",
        help = "Initial URL to open in the managed Chrome session"
    )]
    pub open: String,

    #[arg(
        long,
        help = "Path to the Chrome executable. When omitted, macOS defaults are tried."
    )]
    pub chrome_path: Option<PathBuf>,

    #[arg(
        long,
        help = "Launch Chrome with certificate verification disabled. Useful before the local CA is trusted."
    )]
    pub insecure_ignore_cert_errors: bool,

    #[arg(
        long,
        help = "Persist the browser profile at this path instead of using a temporary profile."
    )]
    pub user_data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct AttachCommand {
    #[command(flatten)]
    pub proxy: ProxyCommand,

    #[arg(
        long,
        default_value = "Wi-Fi",
        help = "macOS network service to attach the system web and secure web proxies to"
    )]
    pub service: String,

    #[arg(
        long,
        default_value_t = false,
        help = "Leave the system proxy enabled when the command exits instead of restoring the previous settings"
    )]
    pub leave_enabled: bool,
}

#[derive(Debug, Clone, Args)]
pub struct CaCommand {
    #[command(subcommand)]
    pub action: CaAction,
}

#[derive(Debug, Clone, Subcommand)]
pub enum CaAction {
    /// Print the CA certificate path and whether the file exists.
    Status,
    /// Trust the CA certificate in the current user's macOS login keychain.
    Trust,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum OutputMode {
    Simple,
    Focused,
    Pretty,
    Json,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum InteractionMode {
    Off,
    Manual,
    Auto,
}
