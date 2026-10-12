//! Install / uninstall / control hwcast as a Windows service.
//! Only install and uninstall need admin; the installed service lets interactive
//! users start and stop it, so `hwcast start|stop|status|open` never prompt.

use crate::service::NAME;
use serde_json::Value;
use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::net::UdpSocket;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const DEFAULT_PORT: u16 = 8340;
const FIREWALL_RULE: &str = "hwcast (local network)";
const FIREWALL_REMOTE: &str = "localsubnet,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,100.64.0.0/10";
const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\hwcast";
// Default service rights, plus start/stop for interactive users (IU) so control needs no UAC prompt.
const SERVICE_SDDL: &str =
    "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPDTLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn help() {
    println!(
        "hwcast {} - live hardware dashboard for this PC, served to any browser on your network

Usage:
  hwcast                     Install (asks once for admin), or open the dashboard if installed
  hwcast install [options]   Install or update the background service
      --port <n>             Port for the dashboard (default {DEFAULT_PORT})
      --dir <path>           Install folder (default: Program Files\\hwcast)
      --no-open              Do not open the dashboard afterwards
      --no-pawnio            Do not offer to install the PawnIO sensor driver
      --no-pause             Close the admin window when done (for scripts)
  hwcast uninstall [--keep-config]
  hwcast start | stop | restart
  hwcast status              Service state, sensor driver, dashboard addresses
  hwcast open                Open the dashboard in your browser
  hwcast run                 Run in the foreground instead of as a service
  hwcast mcp [--host <h:p>]  MCP server for AI agents on stdio (reads the running service)
  hwcast --once              Print one JSON snapshot of every sensor (run as admin)
  hwcast --version",
        env!("CARGO_PKG_VERSION")
    );
}

pub fn dispatch(cmd: &str, rest: &[String]) {
    let code = match cmd {
        "install" => install(rest),
        "uninstall" | "remove" => uninstall(rest),
        "start" => control(true),
        "stop" => control(false),
        "restart" => control(false).and_then(|_| control(true)),
        "status" => status(),
        "open" => open_dashboard(),
        _ => {
            eprintln!("Unknown command: {cmd}\n");
            help();
            std::process::exit(2);
        }
    };
    if let Err(e) = code {
        eprintln!("\nError: {e}");
        pause_if(rest);
        std::process::exit(1);
    }
}

/// Double-click behaviour: install if needed, otherwise make sure it runs and open it.
pub fn double_click() {
    if installed_dir().is_some() {
        if !is_running() {
            let _ = control(true);
        }
        let _ = open_dashboard();
        println!("hwcast is running. The dashboard is open in your browser.");
        println!("Run `hwcast help` in a terminal for more options.");
        sleep(Duration::from_secs(3));
        return;
    }
    println!("hwcast {}\n", env!("CARGO_PKG_VERSION"));
    println!("A live hardware dashboard for this PC: CPU, GPUs, memory, temperatures and fans,");
    println!("viewable from any browser on your local network. It sends nothing to the internet.\n");
    println!("Installing makes these changes (all undone by `hwcast uninstall` or Settings > Apps):");
    println!("  - copies hwcast.exe to {}", default_dir().display());
    println!("  - registers the 'hwcast' Windows service (runs as LocalSystem to read sensors, starts at boot)");
    println!("  - adds a Windows Firewall rule so devices on your local network and VPN can open port {DEFAULT_PORT}");
    println!("  - adds hwcast to the system PATH, a Start menu entry and a Settings > Apps entry");
    println!("  - optionally installs the PawnIO sensor driver (you are asked separately)");
    println!("Windows will ask for administrator permission once.\n");
    if !ask_yes("Install hwcast on this PC now?") {
        return;
    }
    if let Err(e) = install(&[]) {
        eprintln!("\nError: {e}");
        wait_enter();
    }
}

// ---------------------------------------------------------------- install

fn install(args: &[String]) -> Result<(), String> {
    if !is_admin() {
        let mut elevated = vec!["install".to_string()];
        elevated.extend(args.iter().cloned());
        if !args.iter().any(|a| a == "--no-pause") {
            elevated.push("--pause".into());
        }
        let code = run_elevated(&elevated)?;
        if code != 0 {
            return Err("installation did not finish (see the other window)".into());
        }
        if !args.iter().any(|a| a == "--no-open") {
            sleep(Duration::from_millis(800));
            let _ = open_dashboard();
        }
        return Ok(());
    }

    let port: Option<u16> = arg_value(args, "--port").map(|p| p.parse().map_err(|_| format!("bad port: {p}"))).transpose()?;
    let dir = match arg_value(args, "--dir") {
        Some(d) => PathBuf::from(d),
        None => installed_dir().unwrap_or_else(default_dir),
    };
    let exe = dir.join("hwcast.exe");
    println!("Installing hwcast {} to {}", env!("CARGO_PKG_VERSION"), dir.display());

    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(|e| format!("cannot open the service manager: {e}"))?;

    // Stop an existing copy so its exe can be replaced.
    if let Ok(svc) = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) {
        let _ = svc.stop();
        wait_state(&svc, ServiceState::Stopped, 15);
    }

    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if !same_file(&me, &exe) {
        copy_with_retry(&me, &exe)?;
    }
    let port = write_config(&dir, port)?;
    println!("  program copied, dashboard port {port}");

    let info = ServiceInfo {
        name: NAME.into(),
        display_name: "hwcast".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec!["service".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem: needed to read CPU and motherboard sensors
        account_password: None,
    };
    let svc = match mgr.open_service(NAME, ServiceAccess::all()) {
        Ok(s) => {
            s.change_config(&info).map_err(|e| format!("cannot update the service: {e}"))?;
            s
        }
        Err(_) => mgr.create_service(&info, ServiceAccess::all()).map_err(|e| format!("cannot create the service: {e}"))?,
    };
    let _ = svc.set_description("Live hardware dashboard (CPU, GPU, memory, temperatures) served to browsers on your local network.");
    let _ = svc.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(10) };
            3
        ]),
    });
    run_quiet("sc.exe", &["sdset", NAME, SERVICE_SDDL]);
    println!("  service registered (starts automatically at boot)");

    run_quiet_raw("netsh", &format!(r#"advfirewall firewall delete rule name="{FIREWALL_RULE}""#));
    let ok = run_quiet_raw(
        "netsh",
        &format!(
            r#"advfirewall firewall add rule name="{FIREWALL_RULE}" dir=in action=allow program="{}" protocol=TCP localport={port} remoteip={FIREWALL_REMOTE} profile=any"#,
            exe.display()
        ),
    );
    println!("  firewall: {}", if ok { "local network allowed, internet blocked" } else { "could not add the rule (other devices may be blocked)" });

    write_start_menu_link(port);
    write_uninstall_entry(&dir, &exe);
    reg::path_add(&dir.display().to_string());
    println!("  Start menu entry, Settings > Apps entry and `hwcast` command added");

    if !pawnio_installed() && !args.iter().any(|a| a == "--no-pawnio") {
        println!("\nCPU temperature and motherboard sensors need the free PawnIO driver (https://pawnio.eu).");
        println!("Without it you still get CPU load, memory, GPUs and processes.");
        if ask_yes("Install PawnIO now with winget?") {
            let st = Command::new("winget")
                .args(["install", "--id", "namazso.PawnIO", "-e", "--accept-package-agreements", "--accept-source-agreements"])
                .status();
            if !matches!(st, Ok(s) if s.success()) {
                println!("winget could not install it. Download it from https://pawnio.eu, then run `hwcast restart`.");
            }
        }
    }

    svc.start::<&OsStr>(&[]).or_else(|e| if is_running() { Ok(()) } else { Err(e) }).map_err(|e| format!("installed, but the service did not start: {e}"))?;
    let q = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS).map_err(|e| e.to_string())?;
    wait_state(&q, ServiceState::Running, 10);

    println!("\nhwcast is installed and running.");
    print_urls(port);
    pause_if(args);
    Ok(())
}

fn default_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into())).join("hwcast")
}

/// Keeps an existing config.json (labels, probes...) and only sets the port when asked or missing.
fn write_config(dir: &Path, port: Option<u16>) -> Result<u16, String> {
    let path = dir.join("config.json");
    let mut cfg: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let existing = port_from_config(&cfg);
    let port = port.or(existing).unwrap_or(DEFAULT_PORT);
    if existing != Some(port) {
        cfg["bind"] = serde_json::json!([format!("0.0.0.0:{port}")]);
        std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).map_err(|e| format!("cannot write config.json: {e}"))?;
    }
    Ok(port)
}

fn port_from_config(cfg: &Value) -> Option<u16> {
    cfg["bind"].as_array()?.first()?.as_str()?.rsplit(':').next()?.parse().ok()
}

pub(crate) fn configured_port() -> u16 {
    installed_dir()
        .and_then(|d| std::fs::read_to_string(d.join("config.json")).ok())
        .and_then(|s| serde_json::from_str::<Value>(s.trim_start_matches('\u{feff}')).ok())
        .and_then(|v| port_from_config(&v))
        .unwrap_or(DEFAULT_PORT)
}

fn write_start_menu_link(port: u16) {
    if let Some(dir) = start_menu_dir() {
        let body = format!("[InternetShortcut]\r\nURL=http://localhost:{port}/\r\n");
        let _ = std::fs::write(dir.join("hwcast.url"), body);
    }
}

fn start_menu_dir() -> Option<PathBuf> {
    let pd = std::env::var("ProgramData").ok()?;
    Some(PathBuf::from(pd).join(r"Microsoft\Windows\Start Menu\Programs"))
}

fn write_uninstall_entry(dir: &Path, exe: &Path) {
    let exe_s = exe.display().to_string();
    let size_kb = std::fs::metadata(exe).map(|m| (m.len() / 1024) as u32).unwrap_or(0);
    reg::set_str(UNINSTALL_KEY, "DisplayName", "hwcast");
    reg::set_str(UNINSTALL_KEY, "DisplayVersion", env!("CARGO_PKG_VERSION"));
    reg::set_str(UNINSTALL_KEY, "Publisher", "hwcast");
    reg::set_str(UNINSTALL_KEY, "DisplayIcon", &exe_s);
    reg::set_str(UNINSTALL_KEY, "InstallLocation", &dir.display().to_string());
    reg::set_str(UNINSTALL_KEY, "UninstallString", &format!("\"{exe_s}\" uninstall --pause"));
    reg::set_str(UNINSTALL_KEY, "QuietUninstallString", &format!("\"{exe_s}\" uninstall"));
    reg::set_dword(UNINSTALL_KEY, "NoModify", 1);
    reg::set_dword(UNINSTALL_KEY, "NoRepair", 1);
    reg::set_dword(UNINSTALL_KEY, "EstimatedSize", size_kb);
}

// ---------------------------------------------------------------- uninstall

fn uninstall(args: &[String]) -> Result<(), String> {
    if !is_admin() {
        let mut elevated = vec!["uninstall".to_string()];
        elevated.extend(args.iter().filter(|a| *a != "--pause").cloned());
        if !args.iter().any(|a| a == "--no-pause") {
            elevated.push("--pause".into());
        }
        run_elevated(&elevated)?;
        return Ok(());
    }
    let keep_config = args.iter().any(|a| a == "--keep-config");
    let dir = installed_dir();
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| e.to_string())?;
    if let Ok(svc) = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE) {
        let _ = svc.stop();
        wait_state(&svc, ServiceState::Stopped, 15);
        svc.delete().map_err(|e| format!("cannot remove the service: {e}"))?;
        println!("  service removed");
    }
    run_quiet_raw("netsh", &format!(r#"advfirewall firewall delete rule name="{FIREWALL_RULE}""#));
    if let Some(sm) = start_menu_dir() {
        let _ = std::fs::remove_file(sm.join("hwcast.url"));
    }
    reg::delete_tree(UNINSTALL_KEY);
    if let Some(d) = &dir {
        reg::path_remove(&d.display().to_string());
    }
    println!("  firewall rule, Start menu, Settings > Apps and PATH entries removed");

    if let Some(dir) = dir {
        let exe = dir.join("hwcast.exe");
        let cfg = dir.join("config.json");
        let me = std::env::current_exe().unwrap_or_default();
        if same_file(&me, &exe) {
            // Cannot delete a running exe: let a short-lived cmd remove it after we exit.
            let mut script = format!(r#"/c ping 127.0.0.1 -n 3 >nul & del /f /q "{}""#, exe.display());
            if !keep_config {
                script.push_str(&format!(r#" & del /f /q "{}""#, cfg.display()));
            }
            script.push_str(&format!(r#" & rmdir "{}""#, dir.display()));
            let _ = Command::new("cmd").raw_arg(script).creation_flags(CREATE_NO_WINDOW).spawn();
        } else {
            let _ = std::fs::remove_file(&exe);
            if !keep_config {
                let _ = std::fs::remove_file(&cfg);
            }
            let _ = std::fs::remove_dir(&dir);
        }
        println!("  files removed from {}{}", dir.display(), if keep_config { " (config.json kept)" } else { "" });
    }
    println!("\nhwcast is uninstalled. The PawnIO driver stays installed; remove it in Settings > Apps if you no longer need it.");
    pause_if(args);
    Ok(())
}

// ---------------------------------------------------------------- control

fn control(start: bool) -> Result<(), String> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| e.to_string())?;
    let svc = mgr
        .open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP)
        .map_err(|_| "hwcast is not installed. Run `hwcast install` first.".to_string())?;
    let r = if start { svc.start::<&OsStr>(&[]) } else { svc.stop().map(|_| ()) };
    if let Err(e) = r {
        let state = svc.query_status().map(|s| s.current_state).ok();
        let already = (start && state == Some(ServiceState::Running)) || (!start && state == Some(ServiceState::Stopped));
        if !already {
            return Err(format!("could not {} the service: {e}", if start { "start" } else { "stop" }));
        }
    }
    let target = if start { ServiceState::Running } else { ServiceState::Stopped };
    wait_state(&svc, target, 15);
    println!("hwcast is {}.", if start { "running" } else { "stopped (it starts again at the next boot)" });
    if start {
        print_urls(configured_port());
    }
    Ok(())
}

fn status() -> Result<(), String> {
    println!("hwcast {}", env!("CARGO_PKG_VERSION"));
    let Some(dir) = installed_dir() else {
        println!("  Not installed. Run `hwcast install` (or double-click hwcast.exe).");
        return Ok(());
    };
    let state = if is_running() { "running" } else { "stopped" };
    println!("  Service:    {state} (starts automatically at boot)");
    println!("  Installed:  {}", dir.display());
    println!(
        "  Sensors:    {}",
        if pawnio_installed() { "PawnIO driver installed (CPU temperature and board sensors available)" } else { "PawnIO driver missing (no CPU temperature or board sensors)" }
    );
    print_urls(configured_port());
    Ok(())
}

fn open_dashboard() -> Result<(), String> {
    let url: Vec<u16> = format!("http://localhost:{}/", configured_port()).encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
    }
    Ok(())
}

fn print_urls(port: u16) {
    println!("  Dashboard:  http://localhost:{port}");
    if let Some(ip) = local_ip("8.8.8.8:80") {
        println!("              http://{ip}:{port}   (other devices on your network)");
    }
    // Tailscale answers on 100.100.100.100; routing to it reveals this PC's VPN address.
    if let Some(ip) = local_ip("100.100.100.100:53").filter(|ip| ip.starts_with("100.")) {
        println!("              http://{ip}:{port}   (Tailscale / VPN)");
    }
    println!("  AI agents:  MCP at /mcp on the same addresses, or run `hwcast mcp`. Claude Code:");
    println!("              claude mcp add --scope user hwcast -- hwcast mcp");
}

/// The local address Windows would use to reach `target` (UDP connect sends nothing).
fn local_ip(target: &str) -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(target).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}

// ---------------------------------------------------------------- helpers

fn installed_dir() -> Option<PathBuf> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let svc = mgr.open_service(NAME, ServiceAccess::QUERY_CONFIG).ok()?;
    let raw = svc.query_config().ok()?.executable_path.display().to_string();
    // The stored command line may be quoted and carry arguments: "C:\...\hwcast.exe" service
    let exe = if let Some(stripped) = raw.strip_prefix('"') {
        stripped.split('"').next().unwrap_or("").to_string()
    } else {
        let lower = raw.to_ascii_lowercase();
        lower.find(".exe").map(|i| raw[..i + 4].to_string()).unwrap_or(raw)
    };
    PathBuf::from(exe).parent().map(Path::to_path_buf)
}

fn is_running() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .ok()
        .and_then(|m| m.open_service(NAME, ServiceAccess::QUERY_STATUS).ok())
        .and_then(|s| s.query_status().ok())
        .map_or(false, |s| s.current_state == ServiceState::Running)
}

fn pawnio_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .ok()
        .and_then(|m| m.open_service("PawnIO", ServiceAccess::QUERY_STATUS).ok())
        .is_some()
}

fn wait_state(svc: &windows_service::service::Service, want: ServiceState, secs: u64) {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if svc.query_status().map(|s| s.current_state == want).unwrap_or(false) {
            return;
        }
        sleep(Duration::from_millis(250));
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn copy_with_retry(from: &Path, to: &Path) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..10 {
        match std::fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        sleep(Duration::from_millis(500));
    }
    Err(format!("cannot copy hwcast.exe to {}: {last}", to.display()))
}

fn arg_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn run_quiet(prog: &str, args: &[&str]) -> bool {
    Command::new(prog).args(args).creation_flags(CREATE_NO_WINDOW).output().map_or(false, |o| o.status.success())
}

/// For tools like netsh that need quoting exactly as written.
fn run_quiet_raw(prog: &str, raw: &str) -> bool {
    Command::new(prog).raw_arg(raw).creation_flags(CREATE_NO_WINDOW).output().map_or(false, |o| o.status.success())
}

fn ask_yes(q: &str) -> bool {
    print!("{q} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    if std::io::stdin().lock().read_line(&mut s).is_err() {
        return false;
    }
    let s = s.trim().to_ascii_lowercase();
    s.is_empty() || s == "y" || s == "yes"
}

fn pause_if(args: &[String]) {
    if args.iter().any(|a| a == "--pause") {
        wait_enter();
    }
}

fn wait_enter() {
    print!("\nPress Enter to close this window.");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().lock().read_line(&mut String::new());
}

fn is_admin() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Re-runs this exe with admin rights (one UAC prompt) and waits for it.
fn run_elevated(args: &[String]) -> Result<u32, String> {
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let file = w(&exe.display().to_string());
    let params = w(&args.iter().map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a.clone() }).collect::<Vec<_>>().join(" "));
    let verb = w("runas");
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 || info.hProcess.is_null() {
            return Err("administrator permission was not granted".into());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        windows_sys::Win32::Foundation::CloseHandle(info.hProcess);
        Ok(code)
    }
}

mod reg {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE,
        REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn open(key: &str) -> Option<HKEY> {
        let mut h: HKEY = std::ptr::null_mut();
        let r = unsafe {
            RegCreateKeyExW(HKEY_LOCAL_MACHINE, w(key).as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut h, std::ptr::null_mut())
        };
        (r == 0).then_some(h)
    }

    pub fn set_str(key: &str, name: &str, value: &str) {
        if let Some(h) = open(key) {
            let v = w(value);
            unsafe {
                RegSetValueExW(h, w(name).as_ptr(), 0, REG_SZ, v.as_ptr() as _, (v.len() * 2) as u32);
                RegCloseKey(h);
            }
        }
    }

    pub fn set_dword(key: &str, name: &str, value: u32) {
        if let Some(h) = open(key) {
            unsafe {
                RegSetValueExW(h, w(name).as_ptr(), 0, REG_DWORD, &value as *const u32 as _, 4);
                RegCloseKey(h);
            }
        }
    }

    pub fn delete_tree(key: &str) {
        unsafe {
            RegDeleteTreeW(HKEY_LOCAL_MACHINE, w(key).as_ptr());
        }
    }

    const ENV_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

    fn machine_path() -> Option<String> {
        use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
        let mut buf = vec![0u16; 32768];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w(ENV_KEY).as_ptr(),
                w("Path").as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as _,
                &mut len,
            )
        };
        (r == 0).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
    }

    fn set_machine_path(value: &str) {
        use windows_sys::Win32::System::Registry::REG_EXPAND_SZ;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};
        if let Some(h) = open(ENV_KEY) {
            let v = w(value);
            unsafe {
                RegSetValueExW(h, w("Path").as_ptr(), 0, REG_EXPAND_SZ, v.as_ptr() as _, (v.len() * 2) as u32);
                RegCloseKey(h);
                // Tell Explorer so new terminals pick up the change without a sign-out.
                let env = w("Environment");
                SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as _, SMTO_ABORTIFHUNG, 2000, std::ptr::null_mut());
            }
        }
    }

    fn same_dir(a: &str, b: &str) -> bool {
        a.trim().trim_end_matches('\\').eq_ignore_ascii_case(b.trim().trim_end_matches('\\'))
    }

    pub fn path_add(dir: &str) {
        let Some(cur) = machine_path() else { return };
        if cur.split(';').any(|p| same_dir(p, dir)) {
            return;
        }
        let sep = if cur.is_empty() || cur.ends_with(';') { "" } else { ";" };
        set_machine_path(&format!("{cur}{sep}{dir}"));
    }

    pub fn path_remove(dir: &str) {
        let Some(cur) = machine_path() else { return };
        let kept: Vec<&str> = cur.split(';').filter(|p| !same_dir(p, dir)).collect();
        let new = kept.join(";");
        if new != cur {
            set_machine_path(&new);
        }
    }
}
