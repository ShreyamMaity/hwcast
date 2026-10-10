//! Windows service entry point. The service manager starts `hwcast.exe service`;
//! the dashboard server runs on its own threads until the service is stopped.

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

pub const NAME: &str = "hwcast";

define_windows_service!(ffi_service_main, service_main);

pub fn run() {
    if let Err(e) = service_dispatcher::start(NAME, ffi_service_main) {
        eprintln!("This mode is only for the Windows service manager ({e}).");
        eprintln!("Use `hwcast run` to run in the foreground, or `hwcast install`.");
        std::process::exit(1);
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

fn service_main(_args: Vec<OsString>) {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(handle) = service_control_handler::register(NAME, handler) else { return };
    let _ = handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ));
    std::thread::spawn(crate::run_server);
    let _ = stop_rx.recv();
    // Returning lets the dispatcher return and the process exit, which ends the server threads.
    let _ = handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()));
}
