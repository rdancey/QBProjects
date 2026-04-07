mod file_mode;
mod config;
mod qbxml_safe;

use anyhow::{Result, Context};
use log::info;
use winapi::um::winnt::UpdateBlackBoxRecorder;
use std::env;
use futures::future::join_all; 

use crate::config::{AccountSyncConfig, TimestampConfig, Config};
use crate::file_mode::FileMode;
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

async fn process_sync_blocks(processor: &QbxmlRequestProcessor, response_xml: &str, the_sync_block: &AccountSyncConfig, config: &Config) -> Result<()> {
    let gs_cfg = &config.google_sheets;
    match processor.get_account_balance(&response_xml, &the_sync_block.account_full_name) {
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

async fn process_qbxml(processor: &QbxmlRequestProcessor, response_xml: &str, config: &Config) -> Result<()> {
    // Process sync blocks in parallel
    let sync_futures = config.sync_blocks.iter().map(|sync_block| {
        process_sync_blocks(processor, response_xml, sync_block, config)
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
    unsafe {
        let hr = winapi::um::combaseapi::CoInitializeEx(std::ptr::null_mut(), winapi::um::objbase::COINIT_APARTMENTTHREADED);
        // We can bail out here if there is a failure because nothing will need to be cleaned up
        if hr < 0 {
            return Err(anyhow::anyhow!("Failed to initialize COM system: HRESULT=0x{:08X}", hr));
        }
    }

    let processor = match QbxmlRequestProcessor::new() {
        Ok(processor) => processor,
        Err(e) => {
            eprintln!("[QBXML]: Failed to create QBXML request processor: {:#}", e);
            // YOLO - this is the only cleanup needed at this point in the function
            unsafe { winapi::um::combaseapi::CoUninitialize();  }
            return Err(e);
            },
    };
    
    // AppID isn't used by the QBSDK, if a value is passed in config it is harmless but not used
    let app_id = config.quickbooks.application_id.as_deref().unwrap_or(""); 

    /*  If we ever change the name of the service we register with Quickbooks we'll have 
    to change this default too in order to ensure the program will work even if the config.toml loses this setting
    */
    let app_name = config.quickbooks.application_name.as_deref().unwrap_or("QuickBooks Sync Service"); 
    
    let open_connection_result = processor.open_connection(app_id, app_name);
    if let Err(e) = &open_connection_result {
        eprintln!("[QBXML] open_connection failed: {:#}", e);
    }

    if open_connection_result.is_ok() {

        // sets company_file to AUTO if blank, company file name if provided in config.toml
        let company_file = match config.quickbooks.company_file.as_str() { 
            "AUTO" => "",
            path => {
                println!("[DEBUG] Company file: {}", path);
                path }
            };
    
        // we could try to check to see if we have an apparenlty valid ticket here but ...
        let ticket = processor.begin_session(company_file, crate::FileMode::DoNotCare)?;

        /* 
        ... we'll get the Err and Ok(None) match arms deal with it if the ticket is invalid
        */
        match processor.get_account_xml(&ticket) {
            Ok(Some(response_xml)) => {
                // for debugging this line shows us what we got from the API
                // it outputs more lines than are saved in the console so the output has to be routed somewhere to read it
                // info!("{}", response_xml);
                
                // this is it! This is where all the real processing starts!
                match process_qbxml(&processor, &response_xml, &config).await {
                    Err(e) => eprintln!("[QBXML] Error processing QBXML: {:#}", e),
                    Ok(()) => eprintln!("[QBXML] Processing succeeded")
                };
            },
            Ok(None) => {
                eprintln!("[QBXML] No response_xml received, ticket probably invalid");
            },
            Err(e) => {
                /* 
                we can't exit the function here because it is possible that we have an open connection or have
                initialized the COM system and we need to try to clean Up before we exit
                */
                eprintln!("[QBXML] Error querying Quickbooks: {:#}", e);
            }
        }
        /* 
        The COM system has returned all sorts of values for tickets when the ticket fails to be created
        so we can't just assume that we can detect an invalid ticket; we should attempt to close the
        session regardless of what we got as a ticket.

        We don't want to bail out here in the event of an error because there are still cleanup steps needed
        */
        if let Err(e) = processor.end_session(&ticket) {
            eprintln!("[QBXML] end_session errored: {:#}", e)
        }

    }

    /* 
    Begin cleanup. Because it is hard to test earlier to see if we have a valid state for COM 
    we have to try to clean up everything just in case something managed to open or initialize even if
    running process_qbxml() failed
    */

    /* 
    We want to try to continue clean up even if this fails
    I think this could happen if the connection was not open but the COM system was initialized
    */
    if let Err(e) = processor.close_connection() {
        eprintln!("[QBXML] close_connection errored: {:#}", e);
    }

    /*
    YOLO
    */
    unsafe { winapi::um::combaseapi::CoUninitialize(); }

    /* 
    THis is a pretty unhelpful Ok(()) tbh; it really just means the program didn't crash not that
    it actually achieved its objectives
    */
    Ok(())
}

#[tokio::main]
async fn main() {
    // Parse arguments
    let args: Vec<String> = env::args().collect();
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");

    if verbose {
        print_instructions();
        env_logger::builder().filter_level(log::LevelFilter::Debug).init();
    } else {
        env_logger::builder().filter_level(log::LevelFilter::Info).init();
    }
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
        },
      Ok(()) => {
            // Happy Path!
            // doing nothing
            // will return with exit code 0
        }
    };
}