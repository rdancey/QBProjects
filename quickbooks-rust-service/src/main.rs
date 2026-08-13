mod file_mode;
mod config;
mod qbxml_safe;
mod qb_worker;

use anyhow::Result;
use log::info;
use std::env;
use std::io::Write;
use std::sync::Mutex;
use futures::future::join_all;

use crate::config::{AccountSyncConfig, TimestampConfig, Config};
use crate::qbxml_safe::qbxml_request_processor::QbxmlRequestProcessor;
mod google_sheets;
use google_sheets::GoogleSheetsClient;

#[derive(Debug, Clone)]
pub struct AccountData {
    pub account_full_name: String,
    pub number: String,
    pub account_type: String,
    pub balance: f64,
}

fn print_instructions() {
    println!("QuickBooks Account Query Service v5");
    println!("===================================");
    println!();
    println!("This service reads configuration from config/config.toml and queries");
    println!("the specified account to retreive its balance from QuickBooks Desktop.");
    println!();
    println!("Prerequisites:");
    println!("   1. QuickBooks Desktop and the QuickBooks SDK v16 (or higher) must be installed and running");
    println!("   2. A company file must be open in QuickBooks");
    println!("   3. The FullName of the account in config.toml must exist in QuickBooks");
    println!();
    println!("Usage: main_account_query [--verbose]");
    println!("All account sync blocks are now read from config/config.toml; no account_full_name, sheet_name, or cell_address parameter is required.");
    println!();
}

async fn process_sync_blocks(response_xml: &str, the_sync_block: &AccountSyncConfig, config: &Config) -> Result<()> {
    let gs_cfg = &config.google_sheets;
    match QbxmlRequestProcessor::get_account_balance(response_xml, &the_sync_block.account_full_name) {
    Ok(Some(account_balance)) => {
        info!("[QBXML] Account '{}' balance is: {:?}", the_sync_block.account_full_name, account_balance);
        let gs_client = GoogleSheetsClient::new(
            gs_cfg.webapp_url.clone(),
            gs_cfg.api_key.clone(),
            the_sync_block.spreadsheet_id.clone(),
            );
        gs_client.send_balance(
            account_balance,
            Some(&the_sync_block.sheet_name),
            Some(&the_sync_block.cell_address),
            ).await?;
            },
        Ok(None) => {
          info!("[QBXML] No valid balance for account '{}'.", the_sync_block.account_full_name);
            },
        Err(e) => {
            eprintln!("[QBXML] Error parsing balance for '{}': {:#}", the_sync_block.account_full_name, e);
            }
    }
    Ok(())
}

async fn process_timestamp_blocks(the_timestamp_block: &TimestampConfig, config: &Config, ) -> Result<()> {
    use chrono::Local;
    let gs_cfg = &config.google_sheets;
    let now = Local::now();
    let formatted_time = now.format("%d-%m-%Y:%H:%M").to_string();
    let gs_client = GoogleSheetsClient::new(
        gs_cfg.webapp_url.clone(),
        gs_cfg.api_key.clone(),
        the_timestamp_block.spreadsheet_id.clone(),
        );
    gs_client.send_timestamp(
        Some(&formatted_time),
        Some(&the_timestamp_block.sheet_name),
        Some(&the_timestamp_block.cell_address),
        ).await?;
    Ok(())
}

async fn process_qbxml(response_xml: &str, config: &Config) -> Result<()> {
    // Process sync blocks in parallel
    let sync_futures = config.sync_blocks.iter().map(|sync_block| {
        process_sync_blocks(response_xml, sync_block, config)
    });
    let sync_results = join_all(sync_futures).await;
    for result in sync_results {
        result?; // Propagate any error
    }

    // Process timestamp blocks in parallel
    let timestamp_futures = config.timestamp_blocks.iter().map(|timestamp_block| {
        process_timestamp_blocks(timestamp_block, config)
    });
    let timestamp_results = join_all(timestamp_futures).await;
    for result in timestamp_results {
        result?; // Propagate any error
    }

    Ok(())
}

async fn run_qbxml(config: &Config) -> Result<()> {
    // Resolve the company file ("AUTO" -> empty string lets QuickBooks use the open/hosted file).
    let company_file = match config.quickbooks.company_file.as_str() {
        "AUTO" => String::new(),
        path => {
            println!("[DEBUG] Company file: {}", path);
            path.to_string()
        }
    };

    let app_name = config
        .quickbooks
        .application_name
        .clone()
        .unwrap_or_else(|| "QuickBooks Sync Service".to_string());

    // Watchdog timeout for the entire QuickBooks exchange, in seconds.
    let timeout_secs = config.quickbooks.connection_timeout.unwrap_or(120) as u64;

    // Snapshot qbw.exe already running in THIS session, so cleanup only ever targets the instance
    // QuickBooks auto-launches for this run -- never another user's interactive QuickBooks.
    let pre_pids = qb_worker::qbw_pids_in_current_session();

    // Run all COM work on a dedicated STA thread and wait on it with a watchdog timeout, so a
    // stalled/unresponsive QuickBooks can never hang this process indefinitely.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cf = company_file.clone();
    let an = app_name.clone();
    std::thread::spawn(move || {
        let result = qb_worker::fetch_account_xml(&cf, &an);
        let _ = tx.send(result);
    });

    let response_xml = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
        Ok(Ok(Ok(xml))) => {
            info!("[QBXML] Retrieved account data from QuickBooks");
            xml
        }
        Ok(Ok(Err(e))) => {
            eprintln!("[QBXML] QuickBooks access failed: {:#}", e);
            cleanup_owned_qbw(&pre_pids);
            return Err(e);
        }
        Ok(Err(_recv)) => {
            cleanup_owned_qbw(&pre_pids);
            return Err(anyhow::anyhow!("QuickBooks worker thread ended unexpectedly"));
        }
        Err(_elapsed) => {
            eprintln!(
                "[QBXML] QuickBooks access timed out after {}s; terminating this run's QuickBooks instance",
                timeout_secs
            );
            log::error!("QuickBooks access timed out after {}s", timeout_secs);
            cleanup_owned_qbw(&pre_pids);
            return Err(anyhow::anyhow!("QuickBooks access timed out after {}s", timeout_secs));
        }
    };

    // The worker already ended the session and closed the connection. As a safety net, make sure
    // the auto-launched QuickBooks instance is not left running before we do any network I/O.
    cleanup_owned_qbw(&pre_pids);

    // QuickBooks is fully released; now push the values to Google Sheets. A slow or hung Apps
    // Script call can no longer hold a QuickBooks session open or orphan a qbw.exe.
    match process_qbxml(&response_xml, config).await {
        Err(e) => eprintln!("[QBXML] Error processing QBXML: {:#}", e),
        Ok(()) => eprintln!("[QBXML] Processing succeeded"),
    };

    Ok(())
}

/// Terminate any qbw.exe in the current session that appeared after `pre_pids` was captured,
/// i.e. the instance QuickBooks auto-launched for this run. Session-scoped so it never touches
/// another user's interactive QuickBooks on a shared/RDS host.
fn cleanup_owned_qbw(pre_pids: &[u32]) {
    let owned: Vec<u32> = qb_worker::qbw_pids_in_current_session()
        .into_iter()
        .filter(|pid| !pre_pids.contains(pid))
        .collect();
    if !owned.is_empty() {
        log::info!(
            "Cleaning up {} auto-launched qbw.exe instance(s) in this session",
            owned.len()
        );
        qb_worker::terminate_pids(&owned);
    }
}

struct DualLogger {
    level: log::LevelFilter,
    file: Option<Mutex<std::fs::File>>,
}

impl log::Log for DualLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} [{}] {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level(),
            record.args()
        );
        eprintln!("{}", line);
        if let Some(file) = &self.file {
            if let Ok(mut handle) = file.lock() {
                let _ = writeln!(handle, "{}", line);
            }
        }
    }

    fn flush(&self) {
        if let Some(file) = &self.file {
            if let Ok(mut handle) = file.lock() {
                let _ = handle.flush();
            }
        }
    }
}

/// Open (append) today's log file in a `logs/` directory next to the executable.
fn open_log_file() -> Option<std::fs::File> {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let path = dir.join(format!(
        "qb_sync_{}.log",
        chrono::Local::now().format("%Y-%m-%d")
    ));
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn setup_logging(verbose: bool) {
    let level = if verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    let logger = DualLogger {
        level,
        file: open_log_file().map(Mutex::new),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
}

#[tokio::main]
async fn main() {
    // Parse arguments
    let args: Vec<String> = env::args().collect();
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");

    if verbose {
        print_instructions();
    }
    setup_logging(verbose);

    // Load configuration
    let config = match Config::load_from_file("config/config.toml") {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Error: {:#}", e);
            // no config.toml? we out!
            std::process::exit(1);
        }
    };
    // Do the work
    match run_qbxml(&config).await {
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::exit(1);
        }
        Ok(()) => {
            // Happy Path! exit code 0
        }
    };
}
