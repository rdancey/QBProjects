// Runs all QuickBooks COM (QBXMLRP2) work on a dedicated STA thread, and provides
// session-scoped discovery/termination of the qbw.exe instance this service auto-launches.
//
// Confining the COM object to a single thread avoids STA/async thread-affinity hazards, and the
// caller drives this under a watchdog timeout so a stalled QuickBooks can never hang the run.

use anyhow::{anyhow, Result};

use crate::file_mode::FileMode;
use crate::qbxml_safe::qbxml_request_processor::QbxmlRequestProcessor;

use winapi::um::combaseapi::{CoInitializeEx, CoUninitialize};
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::objbase::COINIT_APARTMENTTHREADED;
use winapi::um::processthreadsapi::{
    GetCurrentProcessId, OpenProcess, ProcessIdToSessionId, TerminateProcess,
};
use winapi::um::tlhelp32::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use winapi::um::winnt::PROCESS_TERMINATE;

/// Perform the full QuickBooks exchange (connect, begin session, query accounts, end session,
/// close connection) synchronously on the current thread, initializing COM as an STA here.
///
/// Intended to be called from a dedicated `std::thread` so that a stall cannot block the async
/// runtime; the caller applies the watchdog timeout.
pub fn fetch_account_xml(company_file: &str, app_name: &str) -> Result<String> {
    unsafe {
        let hr = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        if hr < 0 {
            return Err(anyhow!(
                "Failed to initialize COM system: HRESULT=0x{:08X}",
                hr
            ));
        }
    }

    // Do the work in an inner closure so we always CoUninitialize afterwards, on every path.
    let result = (|| -> Result<String> {
        let processor = QbxmlRequestProcessor::new()?;

        // QBXML does not use AppID; always pass empty string.
        processor.open_connection("", app_name)?;

        let ticket = processor.begin_session(company_file, FileMode::DoNotCare)?;

        let xml_result = processor.get_account_xml(&ticket);

        // Always attempt to tear the session/connection down, regardless of the query outcome.
        if let Err(e) = processor.end_session(&ticket) {
            log::warn!("end_session errored: {:#}", e);
        }
        if let Err(e) = processor.close_connection() {
            log::warn!("close_connection errored: {:#}", e);
        }

        match xml_result {
            Ok(Some(xml)) => Ok(xml),
            Ok(None) => Err(anyhow!(
                "No response XML received from QuickBooks (ticket likely invalid)"
            )),
            Err(e) => Err(e),
        }
    })();

    unsafe {
        CoUninitialize();
    }

    result
}

/// Return the PIDs of all `qbw.exe` processes running in the current login session.
pub fn qbw_pids_in_current_session() -> Vec<u32> {
    let mut pids = Vec::new();
    unsafe {
        let mut my_session: u32 = 0;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut my_session) == 0 {
            return pids;
        }

        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return pids;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let name_len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);

                if name.eq_ignore_ascii_case("qbw.exe") {
                    let pid = entry.th32ProcessID;
                    let mut session: u32 = 0;
                    if ProcessIdToSessionId(pid, &mut session) != 0 && session == my_session {
                        pids.push(pid);
                    }
                }

                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
    }
    pids
}

/// Force-terminate the given PIDs. Callers must ensure these are qbw.exe instances this service
/// is responsible for (see `qbw_pids_in_current_session` combined with a pre-run snapshot diff).
pub fn terminate_pids(pids: &[u32]) {
    for &pid in pids {
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                log::warn!("Could not open qbw.exe PID {} for termination", pid);
                continue;
            }
            if TerminateProcess(handle, 1) == 0 {
                log::warn!("Failed to terminate qbw.exe PID {}", pid);
            } else {
                log::info!(
                    "Terminated auto-launched qbw.exe PID {} (current session)",
                    pid
                );
            }
            CloseHandle(handle);
        }
    }
}
