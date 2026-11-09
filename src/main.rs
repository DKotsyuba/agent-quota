//! Rust MCP application; protocol, presentation and deployment have separate boundaries.
mod advice;
mod assets;
mod backoff;
mod collector;
mod config;
mod credentials;
mod delegated;
mod forecast;
mod ipc;
mod process;
mod quota;
mod response;
mod service;
mod store;
mod tools;
use clap::{Parser, Subcommand};
use mcp_presentation::Renderer;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, model::*, service::RequestContext,
};
use std::{path::PathBuf, process::ExitCode, sync::Arc};

/// Private catalog cache lifetime in milliseconds for modern MCP requests.
const TOOLS_LIST_TTL_MS: u64 = 60_000;

#[derive(Parser)]
#[command(version, about = concat!(env!("CARGO_PKG_NAME"), " MCP server"))]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Run the protocol-only stdio endpoint.
    Mcp {
        /// Service socket; defaults to AGENT_QUOTA_SOCKET or the service home.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Expected owner UID for a service running under another account.
        #[arg(long)]
        server_uid: Option<u32>,
    },
    /// Run the independent quota collector and local read-only API.
    Serve {
        /// Private service home, defaulting to AGENT_QUOTA_HOME or ~/.agent-quota.
        #[arg(long)]
        home: Option<PathBuf>,
        /// Quota configuration, defaulting to config.toml under the service home.
        #[arg(long)]
        config: Option<PathBuf>,
        /// API socket; defaults to AGENT_QUOTA_SOCKET or the service home.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Additional explicitly authorized local client UIDs.
        #[arg(long = "allow-uid")]
        allowed_uids: Vec<u32>,
    },
    /// Read ready-made quota metrics from the running service as JSON.
    Limits {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        server_uid: Option<u32>,
        /// Restrict the response to the specified account; repeat for multiple accounts.
        #[arg(long = "account")]
        accounts: Vec<String>,
        /// Restrict the response to one configured or native model name.
        #[arg(long)]
        model: Option<String>,
    },
    /// Read the running service's nonsecret collection status as JSON.
    ServiceStatus {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        server_uid: Option<u32>,
    },
    /// Run one configured external probe as the credential-owning OS user.
    Probe {
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        account: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        source: String,
    },
    /// Submit a fenced external-probe result to a service owned by this OS user.
    Ingest {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Export the collector and Linux launcher files embedded in this executable.
    ExportAssets {
        /// New absolute directory under an existing owner-private parent.
        #[arg(long)]
        directory: PathBuf,
    },
    /// Verify an exported directory against the assets embedded in this executable.
    VerifyAssets {
        /// Absolute owner-private export directory.
        #[arg(long)]
        directory: PathBuf,
    },
    /// Check local resources, without credentials, network or mutation.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Service socket; defaults to AGENT_QUOTA_SOCKET or the service home.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Expected owner UID for a service running under another account.
        #[arg(long)]
        server_uid: Option<u32>,
    },
    /// Export the actual registry or inspect unimplemented tool stubs.
    Contract {
        #[command(subcommand)]
        command: ContractCommand,
    },
    /// Install a verified single-binary bundle. Does not restart services.
    SelfInstall {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        bin_dir: PathBuf,
    },
    /// Select a retained compatible installation.
    Releases {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
}
#[derive(Subcommand)]
enum ContractCommand {
    Export,
    Readiness,
}
#[derive(Subcommand)]
enum ReleaseCommand {
    Use {
        version: String,
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        bin_dir: PathBuf,
    },
}
#[derive(Clone)]
struct Handler {
    identity: Arc<Renderer>,
    templates: Arc<response::Templates>,
    catalog: Vec<Tool>,
    socket: Option<PathBuf>,
    server_uid: u32,
}
impl Handler {
    fn new(socket: Option<PathBuf>, server_uid: u32) -> Result<Self, &'static str> {
        let identity = Renderer::new().map_err(|_| "presentation_setup_failed")?;
        let templates = response::Templates::new(&tools::templates())?;
        let catalog = serde_json::from_value(serde_json::Value::Array(tools::definitions()))
            .map_err(|_| "catalog_invalid")?;
        Ok(Self {
            identity: Arc::new(identity),
            templates: Arc::new(templates),
            catalog,
            socket,
            server_uid,
        })
    }
}
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions("Use get_status for identity and get_quota for stored quota observations. Tool descriptions define effects.")
    }
    /// Return the static catalog; modern requests cache it privately for 60 seconds.
    /// Pagination is unused; legacy sessions retain their original wire fields.
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut result = ListToolsResult::with_all_items(self.catalog.clone());
        if context
            .protocol_version()
            .is_some_and(|version| version.as_str() >= ProtocolVersion::V_2026_07_28.as_str())
        {
            result = result
                .with_ttl_ms(TOOLS_LIST_TTL_MS)
                .with_cache_scope(CacheScope::Private);
        }
        Ok(result)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let reply = tools::call(
            &request.name,
            args,
            &self.identity,
            &self.templates,
            self.socket.as_deref(),
            self.server_uid,
        )
        .await
        .ok_or_else(|| McpError::new(ErrorCode::METHOD_NOT_FOUND, "Unknown tool", None))?;
        Ok(reply.into())
    }
}
#[allow(clippy::print_stdout, reason = "Explicit CLI branch, never MCP output")]
fn print_json(value: impl serde::Serialize) -> ExitCode {
    match serde_json::to_string_pretty(&value) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("CLI serialization failed");
            ExitCode::FAILURE
        }
    }
}

/// Reports an asset result without hiding a completed export if stdout fails.
fn print_asset_report(report: assets::Report, exported: bool) -> ExitCode {
    use std::io::Write;
    let output = serde_json::to_vec_pretty(&report)
        .map_err(std::io::Error::other)
        .and_then(|text| {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&text)?;
            stdout.write_all(b"\n")?;
            stdout.flush()
        });
    if output.is_ok() {
        return ExitCode::SUCCESS;
    }
    let code = if exported {
        "assets_export_succeeded_output_failed"
    } else {
        "assets_output_failed"
    };
    let _ = writeln!(std::io::stderr().lock(), "assets: {code}");
    ExitCode::from(if exported { 3 } else { 1 })
}

/// Encodes a compact probe envelope, including its newline in the byte budget.
fn probe_frame(value: &serde_json::Value) -> Result<Vec<u8>, &'static str> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| "output_invalid")?;
    if bytes.len().saturating_add(1) > delegated::MAX_ENVELOPE_BYTES {
        return Err("output_too_large");
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Emits one bounded compact probe envelope without exposing I/O details.
fn print_probe(value: &serde_json::Value) -> ExitCode {
    use std::io::Write;
    let bytes = match probe_frame(value) {
        Ok(bytes) => bytes,
        Err(code) => {
            eprintln!("quota probe: {code}");
            return ExitCode::FAILURE;
        }
    };
    let mut output = std::io::stdout().lock();
    if output.write_all(&bytes).is_err() {
        eprintln!("quota probe: output_unavailable");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Reads a Unix descriptor with one absolute deadline and a fixed byte ceiling.
fn read_bounded<R: std::io::Read + std::os::fd::AsRawFd>(
    reader: &mut R,
    limit: usize,
    timeout: std::time::Duration,
) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind};
    let deadline = std::time::Instant::now() + timeout;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::new(ErrorKind::TimedOut, "input deadline"));
        }
        let mut descriptor = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = remaining.as_millis().max(1).min(libc::c_int::MAX as u128) as libc::c_int;
        // SAFETY: descriptor points to one valid pollfd for the duration of this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready == 0 {
            return Err(Error::new(ErrorKind::TimedOut, "input deadline"));
        }
        if ready < 0 {
            let error = Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let mut chunk = [0u8; 8192];
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(count) if count <= limit.saturating_sub(bytes.len()) => {
                bytes.extend_from_slice(&chunk[..count])
            }
            Ok(_) => return Err(Error::new(ErrorKind::InvalidData, "input limit")),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}
/// Release qualification recorded in the embedded family manifest, so
/// `doctor` and `get_status` report the same value the release tooling reads.
/// An unreadable manifest fails closed to `not_verified`.
fn qualification() -> &'static str {
    static QUALIFICATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    QUALIFICATION.get_or_init(|| {
        toml::from_str::<toml::Value>(include_str!("../family.toml"))
            .ok()
            .and_then(|family| {
                family
                    .get("qualification")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "not_verified".to_owned())
    })
}

/// Resolves the requested service home without creating or modifying it.
fn default_home() -> Option<PathBuf> {
    std::env::var_os("AGENT_QUOTA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".agent-quota"))
        })
}

/// Resolves an absolute socket path; an absent home is an explicit unavailable state.
fn socket_path(explicit: Option<PathBuf>) -> Result<PathBuf, &'static str> {
    let path = explicit
        .or_else(|| {
            std::env::var_os("AGENT_QUOTA_SOCKET")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| default_home().map(|home| home.join("quota.sock")))
        .ok_or("socket_unavailable")?;
    if !path.is_absolute() {
        return Err("invalid_socket_path");
    }
    Ok(path)
}

/// Sends one read-only request and prints the verified service result.
async fn read_service(
    socket: Option<PathBuf>,
    server_uid: Option<u32>,
    method: &str,
    accounts: Vec<String>,
    model: Option<String>,
) -> ExitCode {
    let path = match socket_path(socket) {
        Ok(path) => path,
        Err(code) => {
            eprintln!("quota: {code}");
            return ExitCode::FAILURE;
        }
    };
    let request = ipc::Request {
        version: 1,
        method: method.into(),
        accounts,
        model,
        observation: None,
    };
    match ipc::request(&path, &request, server_uid.unwrap_or_else(ipc::owner_uid)).await {
        Ok(ipc::Reply::Result { result, .. }) => print_json(result),
        Ok(ipc::Reply::Error { error, .. }) | Err(error) => {
            eprintln!("quota: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Reads a bounded worker envelope from stdin and submits it once to the owner-only endpoint.
async fn ingest_service(socket: Option<PathBuf>) -> ExitCode {
    let path = match socket_path(socket) {
        Ok(path) => path,
        Err(code) => {
            eprintln!("quota ingest: {code}");
            return ExitCode::FAILURE;
        }
    };
    let bytes = match read_bounded(
        &mut std::io::stdin().lock(),
        delegated::MAX_ENVELOPE_BYTES,
        std::time::Duration::from_secs(305),
    ) {
        Ok(bytes) => bytes,
        Err(_) => {
            eprintln!("quota ingest: invalid_observation");
            return ExitCode::FAILURE;
        }
    };
    let observation: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("quota ingest: invalid_observation");
            return ExitCode::FAILURE;
        }
    };
    let request = ipc::Request {
        version: 1,
        method: "ingest".into(),
        accounts: Vec::new(),
        model: None,
        observation: Some(observation),
    };
    match ipc::request(&path, &request, ipc::owner_uid()).await {
        Ok(ipc::Reply::Result { result, .. }) => print_json(result),
        Ok(ipc::Reply::Error { error, .. }) | Err(error) => {
            eprintln!("quota ingest: {error}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Commands::SelfInstall {
            bundle,
            home,
            bin_dir,
        } => match family_delivery::install(&bundle, &home, &bin_dir) {
            Ok(m) => print_json(m),
            Err(e) => {
                eprintln!("install: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Releases {
            command:
                ReleaseCommand::Use {
                    version,
                    home,
                    bin_dir,
                },
        } => match family_delivery::use_version(&home, &bin_dir, &version) {
            Ok(m) => print_json(m),
            Err(e) => {
                eprintln!("activate: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Doctor {
            json: _,
            socket,
            server_uid,
        } => {
            let frontend_ready = Handler::new(None, ipc::owner_uid()).is_ok();
            let service_ready = match socket_path(socket) {
                Ok(path) => {
                    let request = ipc::Request {
                        version: 1,
                        method: "status".into(),
                        accounts: Vec::new(),
                        model: None,
                        observation: None,
                    };
                    matches!(ipc::request(&path, &request, server_uid.unwrap_or_else(ipc::owner_uid)).await,
                        Ok(ipc::Reply::Result { result, .. }) if result["status"] == "running")
                }
                Err(_) => false,
            };
            let ready = frontend_ready && service_ready;
            let output = print_json(
                serde_json::json!({"product":env!("CARGO_PKG_NAME"),"version":env!("CARGO_PKG_VERSION"),
                "local_ready":ready,"frontend_ready":frontend_ready,
                "service":if service_ready { "running" } else { "unavailable" },
                "release_qualification":qualification(),"incomplete_tools":tools::incomplete()}),
            );
            if ready { output } else { ExitCode::from(2) }
        }
        Commands::Contract {
            command: ContractCommand::Export,
        } => match Handler::new(None, ipc::owner_uid()) {
            Ok(h) => print_json(h.catalog),
            Err(e) => {
                eprintln!("contract: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Contract {
            command: ContractCommand::Readiness,
        } => print_json(tools::incomplete()),
        Commands::Mcp { socket, server_uid } => {
            let path = match socket_path(socket) {
                Ok(path) => Some(path),
                Err("socket_unavailable") => None,
                Err(code) => {
                    eprintln!("startup: {code}");
                    return ExitCode::FAILURE;
                }
            };
            let h = match Handler::new(path, server_uid.unwrap_or_else(ipc::owner_uid)) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("startup: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match h.serve(rmcp::transport::stdio()).await {
                Ok(service) => match service.waiting().await {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(_) => ExitCode::FAILURE,
                },
                Err(_) => {
                    eprintln!("MCP transport failed");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Serve {
            home,
            config,
            socket,
            allowed_uids,
        } => {
            let home = match home.or_else(default_home) {
                Some(home) if home.is_absolute() => home,
                _ => {
                    eprintln!("startup: home_must_be_absolute");
                    return ExitCode::FAILURE;
                }
            };
            let config = config.unwrap_or_else(|| home.join("config.toml"));
            let socket = match socket.or_else(|| {
                std::env::var_os("AGENT_QUOTA_SOCKET")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            }) {
                Some(path) if path.is_absolute() => path,
                Some(_) => {
                    eprintln!("startup: invalid_socket_path");
                    return ExitCode::FAILURE;
                }
                None => home.join("quota.sock"),
            };
            let engine = match service::Engine::open(&home, &config) {
                Ok(engine) => engine,
                Err(code) => {
                    eprintln!("startup: {code}");
                    return ExitCode::FAILURE;
                }
            };
            match service::serve(engine, socket, allowed_uids).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(code) => {
                    eprintln!("service: {code}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Limits {
            socket,
            server_uid,
            accounts,
            model,
        } => read_service(socket, server_uid, "snapshot", accounts, model).await,
        Commands::ServiceStatus { socket, server_uid } => {
            read_service(socket, server_uid, "status", Vec::new(), None).await
        }
        Commands::Probe {
            home,
            config,
            account,
            provider,
            source,
        } => {
            let home = match service::prepare_home(&home) {
                Ok(home) => home,
                Err(code) => {
                    eprintln!("quota probe: {code}");
                    return ExitCode::FAILURE;
                }
            };
            match delegated::probe(&config, &home, &account, &provider, &source).await {
                Ok(value) => print_probe(&value),
                Err(code) => {
                    eprintln!("quota probe: {code}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Ingest { socket } => ingest_service(socket).await,
        Commands::ExportAssets { directory } => match assets::export(&directory) {
            Ok(report) => print_asset_report(report, true),
            Err(code) => {
                eprintln!("assets: {code}");
                ExitCode::FAILURE
            }
        },
        Commands::VerifyAssets { directory } => match assets::verify(&directory) {
            Ok(report) => print_asset_report(report, false),
            Err(code) => {
                eprintln!("assets: {code}");
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Synthetic I/O assertions")]
mod tests {
    use super::*;
    use std::io::Write;

    /// An open pipe with no EOF cannot keep the ingest reader alive past its deadline.
    #[test]
    fn bounded_input_times_out_with_open_writer() {
        let (mut reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let start = std::time::Instant::now();
        let error =
            read_bounded(&mut reader, 1024, std::time::Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    /// An over-budget stream is rejected rather than truncated to valid JSON.
    #[test]
    fn bounded_input_refuses_oversized_data() {
        let (mut reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        writer.write_all(&[b'x'; 33]).unwrap();
        drop(writer);
        let error = read_bounded(&mut reader, 32, std::time::Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Probe output and ingestion use the same compact byte limit, including newline.
    #[test]
    fn probe_frame_accounts_for_newline() {
        let within = serde_json::json!("x".repeat(delegated::MAX_ENVELOPE_BYTES - 3));
        assert_eq!(
            probe_frame(&within).unwrap().len(),
            delegated::MAX_ENVELOPE_BYTES
        );
        let too_large = serde_json::json!("x".repeat(delegated::MAX_ENVELOPE_BYTES - 2));
        assert!(matches!(probe_frame(&too_large), Err("output_too_large")));
    }
}
