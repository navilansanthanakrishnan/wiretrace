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
    /// Launch Chrome with a DevTools-driven deep interaction observer.
    BrowserDeep(BrowserDeepCommand),
    /// Start the interception proxy and attach already-open macOS apps via the system proxy.
    Attach(AttachCommand),
    /// Run the workflow recording and automation server, or talk to it via CLI helpers.
    Workflow(WorkflowCommand),
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
        default_value_t = false,
        help = "Dangerous: disable redaction and print raw sensitive headers/body fields such as Authorization and Cookie"
    )]
    pub allow_sensitive_output: bool,

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
pub struct BrowserDeepCommand {
    #[arg(long, value_enum, default_value_t = OutputMode::Simple)]
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
        help = "Persist the browser profile at this path instead of using a temporary profile."
    )]
    pub user_data_dir: Option<PathBuf>,

    #[arg(
        long,
        default_value_t = 9223,
        help = "Remote debugging port for the managed browser session"
    )]
    pub remote_debugging_port: u16,

    #[arg(
        long,
        default_value_t = 4000,
        help = "Requests must begin within this many milliseconds of an observed browser interaction to be attributed"
    )]
    pub interaction_window_ms: u64,

    #[arg(
        long,
        default_value_t = false,
        help = "Capture all matching browser requests, even when no interaction attribution is available"
    )]
    pub record_all: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "Do not redact sensitive headers in browser-deep output"
    )]
    pub allow_sensitive_output: bool,
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

#[derive(Debug, Clone, Args)]
pub struct WorkflowCommand {
    #[command(subcommand)]
    pub action: WorkflowAction,
}

#[derive(Debug, Clone, Subcommand)]
pub enum WorkflowAction {
    /// Start the localhost workflow server and UI.
    Serve(WorkflowServeCommand),
    /// Begin a desktop-wide recording session through the local server.
    Begin(WorkflowBeginCommand),
    /// Stop the active recording session and trigger analysis.
    Stop(WorkflowStopCommand),
    /// Show server and active-session status.
    Status(WorkflowStatusCommand),
    /// Ask the analyzer to design and generate an automation from a recorded workflow.
    Ask(WorkflowAskCommand),
}

#[derive(Debug, Clone, Args)]
pub struct WorkflowServeCommand {
    #[arg(
        long,
        default_value = "127.0.0.1:4317",
        help = "Socket address for the workflow UI and API server"
    )]
    pub listen: SocketAddr,
}

#[derive(Debug, Clone, Args)]
pub struct WorkflowBeginCommand {
    #[arg(
        long,
        default_value = "desktop",
        help = "Recording mode: desktop or browser_deep"
    )]
    pub mode: String,

    #[arg(
        long,
        default_value = "Wi-Fi",
        help = "macOS network service for desktop recording mode"
    )]
    pub service: String,

    #[arg(
        long,
        default_value = "https://example.com",
        help = "Initial URL for browser_deep mode"
    )]
    pub open: String,

    #[arg(long, help = "Persistent browser profile directory for browser_deep mode")]
    pub user_data_dir: Option<PathBuf>,

    #[arg(
        long,
        default_value = "127.0.0.1:4317",
        help = "Workflow server address"
    )]
    pub server: SocketAddr,

    #[arg(long, help = "Optional human-friendly name for the recording session")]
    pub name: Option<String>,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Only record requests whose host contains one of these values"
    )]
    pub host_contains: Vec<String>,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Only record requests whose URL contains one of these values"
    )]
    pub url_contains: Vec<String>,

    #[arg(
        long,
        value_delimiter = ',',
        help = "Only record requests whose HTTP method matches one of these values"
    )]
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct WorkflowStopCommand {
    #[arg(
        long,
        default_value = "127.0.0.1:4317",
        help = "Workflow server address"
    )]
    pub server: SocketAddr,
}

#[derive(Debug, Clone, Args)]
pub struct WorkflowStatusCommand {
    #[arg(
        long,
        default_value = "127.0.0.1:4317",
        help = "Workflow server address"
    )]
    pub server: SocketAddr,
}

#[derive(Debug, Clone, Args)]
pub struct WorkflowAskCommand {
    #[arg(long, help = "Workflow session id. Defaults to the latest completed session when omitted.")]
    pub session_id: Option<String>,

    #[arg(
        long,
        default_value = "127.0.0.1:4317",
        help = "Workflow server address"
    )]
    pub server: SocketAddr,

    #[arg(help = "Automation request to generate and implement")]
    pub prompt: String,
}
