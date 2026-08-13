// Type-safe wrapper for QBXMLRP2.RequestProcessor COM API
// Mirrors the structure of request_processor.rs but uses tickets (strings) instead of pointers

use winapi::shared::guiddef::{CLSID, IID_NULL};
use winapi::um::oaidl::{IDispatch, VARIANT, EXCEPINFO};
use crate::qbxml_safe::qbxml_safe_variant::SafeVariant;
use crate::file_mode::FileMode;

const DISPATCH_METHOD: u16 = 1;

pub struct QbxmlRequestProcessor {
    inner: *mut IDispatch,
}

// Locally define IID_IDispatch for use in CoCreateInstance
#[allow(non_upper_case_globals)]
pub const IID_IDispatch: winapi::shared::guiddef::GUID = winapi::shared::guiddef::GUID {
    Data1: 0x00020400,
    Data2: 0x0000,
    Data3: 0x0000,
    Data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

impl QbxmlRequestProcessor {
    pub fn new() -> Result<Self, anyhow::Error> {
        // Use the single QBXML ProgID for RequestProcessor
        let prog_id = "QBXMLRP2.RequestProcessor";
        log::info!("Trying QBXML ProgID: {}", prog_id);
        let prog_id_wide = widestring::U16CString::from_str(prog_id).unwrap();
        let mut clsid: CLSID = unsafe { std::mem::zeroed() };
        let hr = unsafe {
            winapi::um::combaseapi::CLSIDFromProgID(
                prog_id_wide.as_ptr(),
                &mut clsid as *mut CLSID
            )
        };
        if hr < 0 {
            log::error!("ProgID {} not found or CLSIDFromProgID failed: HRESULT=0x{:08X}", prog_id, hr as u32);
            return Err(anyhow::anyhow!("Failed to find QBXML COM ProgID: {} (HRESULT=0x{:08X})", prog_id, hr as u32));
        }
        let mut dispatch_ptr: *mut IDispatch = std::ptr::null_mut();
        let hr = unsafe {
            winapi::um::combaseapi::CoCreateInstance(
                &clsid,
                std::ptr::null_mut(),
                winapi::shared::wtypesbase::CLSCTX_INPROC_SERVER,
                &IID_IDispatch,
                &mut dispatch_ptr as *mut _ as *mut _
            )
        };
        if hr >= 0 && !dispatch_ptr.is_null() {
            let instance = Self {
                inner: dispatch_ptr,
            };
            Ok(instance)
        } else {
            log::error!("Failed to create COM instance for {}: HRESULT=0x{:08X}", prog_id, hr as u32);
            Err(anyhow::anyhow!("Failed to create QBXML COM instance for ProgID: {} (HRESULT=0x{:08X})", prog_id, hr as u32))
        }
    }

    pub fn open_connection(&self, _app_id: &str, app_name: &str) -> Result<(), anyhow::Error> {
        // Always pass empty string for AppID to avoid accidental registration (QBXML does not use AppID)
        let app_id_var = SafeVariant::from_string("");
        let app_name_var = SafeVariant::from_string(app_name);
        // Parameter order matches QBFC for consistency
        match self.invoke_method("OpenConnection", &[app_name_var, app_id_var]) {
            Ok(_) => {
                Ok(())
            },
            Err(e) => {
                log::error!("OpenConnection failed: {:#}", e);
                for cause in e.chain().skip(1) {
                    log::error!("Caused by: {:#}", cause);
                }
                Err(anyhow::anyhow!("Failed to open QuickBooks connection. See error logs above for HRESULT, EXCEPINFO, and details."))
            }
        }
    }

    pub fn begin_session(&self, company_file: &str, file_mode: FileMode) -> Result<String, anyhow::Error> {
        let file_var = SafeVariant::from_string(company_file);
        let mode_int = match file_mode {
            FileMode::SingleUser => 1,
            FileMode::MultiUser => 2,
            FileMode::DoNotCare => 2, // per IDL: omDontCare = 2
            FileMode::Online => 3,
        };
        let mode_var = SafeVariant::from_i32(mode_int);
        // Correct COM parameter order: [mode_var, file_var]
        let result = self.invoke_method("BeginSession", &[mode_var, file_var])?;
        let vt = unsafe { result.as_variant().n1.n2().vt };
        let ticket = result.to_string().unwrap_or_default();
        if ticket.is_empty() {
            log::warn!("BeginSession returned an empty ticket string!");
        }
        Ok(ticket)
    }

    pub fn process_request(&self, ticket: &str, request: &str) -> Result<String, anyhow::Error> {
        let ticket_var = SafeVariant::from_string(ticket);
        let request_var = SafeVariant::from_string(request);
        // ProcessRequest with parameters in the reverse order works!
        let result = self.invoke_method("ProcessRequest", &[request_var, ticket_var])?;

        result.to_string().ok_or_else(|| anyhow::anyhow!("ProcessRequest did not return a string"))
    }

    pub fn end_session(&self, ticket: &str) -> Result<(), anyhow::Error> {
        let ticket_var = SafeVariant::from_string(ticket);
        self.invoke_method("EndSession", &[ticket_var])?;
        Ok(())
    }

    pub fn close_connection(&self) -> Result<(), anyhow::Error> {
        self.invoke_method("CloseConnection", &[])?;
        Ok(())
    }

    // currently not used but we may need it
    pub fn get_current_company_file_name(&self) -> Result<String, anyhow::Error> {
        let result = self.invoke_method("GetCurrentCompanyFileName", &[])?;
        Ok(result.to_string().unwrap_or_default())
    }

    pub fn get_account_xml(&self, ticket: &str) -> Result<Option<String>, anyhow::Error> {
        // Step 1: Build QBXML request for account query
        // note: use xml version "1.0" and qbxml version "13.0" - changes to those versions generate errors
        let qbxml_request = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<?qbxml version="13.0"?>
<QBXML>
   <QBXMLMsgsRq onError="continueOnError">
      <AccountQueryRq>
        <IncludeRetElement>FullName</IncludeRetElement>
        <IncludeRetElement>Balance</IncludeRetElement>
      </AccountQueryRq>
   </QBXMLMsgsRq>
</QBXML>"#);        
        // Step 2: Send request
        let response_xml = match self.process_request(ticket, &qbxml_request) {
            Ok(xml) => xml,
            Err(e) => return Err(e),
        };
        Ok(Some(response_xml))
    }

    pub fn get_account_balance(response_xml: &str, account_full_name: &str) -> Result<Option<f64>, anyhow::Error> {
        let mut balance = 0.0;
        let mut found = false;
        let mut search_start = 0;
        while let Some(ret_start) = response_xml[search_start..].find("<AccountRet>") {
            let ret_start = ret_start + search_start;
            let ret_end = match response_xml[ret_start..].find("</AccountRet>") {
                Some(e) => ret_start + e + "</AccountRet>".len(),
                None => break,
            };
            let account_block = &response_xml[ret_start..ret_end];
            if let Some(full_name) = Self::extract_xml_field(account_block, "<FullName>", "</FullName>") {
                let decoded_full_name = htmlescape::decode_html(&full_name).unwrap_or(full_name.clone());
                if decoded_full_name == account_full_name {
                    balance = Self::extract_xml_field(account_block, "<Balance>", "</Balance>")
                        .and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                    found = true;
                    break;
                }
            }
            search_start = ret_end;
        }
        if found {
            Ok(Some(balance))
        } else {
            log::warn!("No accounts found with the specified criteria");
            Ok(None)
        }
    }

    fn invoke_method(&self, method_name: &str, params: &[SafeVariant]) -> Result<SafeVariant, anyhow::Error> {
        let method_name_wide = widestring::U16CString::from_str(method_name).unwrap();
        // Instead, use VARIANT zeroed and wrap as needed
        let mut result: VARIANT = unsafe { std::mem::zeroed() };
        let mut excepinfo: EXCEPINFO = unsafe { std::mem::zeroed() };
        let hr = unsafe {
            // Correct COM call signature for Invoke
            let mut dispid = 0i32;
            let names = [method_name_wide.as_ptr()];
            let get_id_hr = ((*(*self.inner).lpVtbl).GetIDsOfNames)(
                self.inner,
                &IID_NULL,
                names.as_ptr() as *mut _,
                1,
                0x0409,
                &mut dispid
            );
            if get_id_hr < 0 {
                return Err(anyhow::anyhow!("GetIDsOfNames failed: HRESULT=0x{:08X}", get_id_hr));
            }
            let mut variants: Vec<VARIANT> = params.iter().map(|v| v.0).collect();
            let mut dispparams = winapi::um::oaidl::DISPPARAMS {
                rgvarg: if variants.is_empty() { std::ptr::null_mut() } else { variants.as_mut_ptr() },
                rgdispidNamedArgs: std::ptr::null_mut(),
                cArgs: variants.len() as u32,
                cNamedArgs: 0,
            };
            let mut arg_err = 0u32;
            ((*(*self.inner).lpVtbl).Invoke)(
                self.inner,
                dispid,
                &IID_NULL,
                0x0409,
                DISPATCH_METHOD,
                &mut dispparams,
                &mut result,
                &mut excepinfo,
                &mut arg_err
            )
        };
        if hr < 0 {
            // Log EXCEPINFO details if available
            unsafe {
                let description = if !excepinfo.bstrDescription.is_null() {
                    let wide = widestring::U16CStr::from_ptr_str(excepinfo.bstrDescription);
                    wide.to_string_lossy()
                } else {
                    "<no description>".to_string()
                };
                let source = if !excepinfo.bstrSource.is_null() {
                    let wide = widestring::U16CStr::from_ptr_str(excepinfo.bstrSource);
                    wide.to_string_lossy()
                } else {
                    "<no source>".to_string()
                };
                let scode = excepinfo.scode;
                log::error!("COM Invoke failed: HRESULT=0x{:08X}, Source: {}, Description: {}, SCODE: 0x{:08X}", hr, source, description, scode);
            }
            return Err(anyhow::anyhow!("Invoke failed: HRESULT=0x{:08X}", hr));
        }
        Ok(SafeVariant(result))
    }

    // Helper function for minimal XML field extraction
    fn extract_xml_field(xml: &str, start_tag: &str, end_tag: &str) -> Option<String> {
        let start = xml.find(start_tag)? + start_tag.len();
        let end = xml[start..].find(end_tag)? + start;
        Some(xml[start..end].trim().to_string())
    }
}
