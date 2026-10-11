//! hwcast: headless hardware telemetry streamed as Server-Sent Events.
//! Samples only while someone is watching; idle otherwise.
//!
//!   hwcast                  (double-click) install, or open the dashboard if installed
//!   hwcast install|uninstall|start|stop|status|open   manage the Windows service
//!   hwcast run              run the server in the foreground (admin for sensors)
//!   hwcast service          entry point used by the Windows service manager
//!   hwcast --once           print one full JSON snapshot and exit

mod board;
mod cpu;
mod gpu;
mod manage;
mod nvapi;
mod pawnio;
mod service;
mod superio;
mod sys;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INDEX_HTML: &str = include_str!("../web/index.html");
const TICK: Duration = Duration::from_millis(1000);

struct Config {
    bind: Vec<String>,
    /// IPv4 CIDRs allowed to connect. Default: this machine, private LAN ranges,
    /// and 100.64.0.0/10 (used by Tailscale and similar mesh VPNs).
    allow: Vec<(u32, u32)>,
    labels: HashMap<String, String>,
    probes: Option<Vec<String>>,
    hide: Vec<String>,
}

fn parse_cidr(s: &str) -> Option<(u32, u32)> {
    let (ip, bits) = s.split_once('/').unwrap_or((s, "32"));
    let ip: std::net::Ipv4Addr = ip.trim().parse().ok()?;
    let bits: u32 = bits.trim().parse().ok().filter(|b| *b <= 32)?;
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    Some((u32::from(ip) & mask, mask))
}

fn strings(v: &Value) -> Option<Vec<String>> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}

fn load_config() -> Config {
    let mut cfg = Config {
        bind: vec!["0.0.0.0:8340".into()],
        allow: ["127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10"]
            .iter()
            .filter_map(|c| parse_cidr(c))
            .collect(),
        labels: HashMap::new(),
        probes: None,
        hide: Vec::new(),
    };
    let path = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("config.json")));
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return cfg };
    // Windows PowerShell 5 writes UTF-8 with a BOM, which serde_json rejects.
    let v: Value = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("config.json ignored: {e}");
            return cfg;
        }
    };
    if let Some(b) = strings(&v["bind"]) {
        cfg.bind = b;
    }
    if let Some(a) = strings(&v["allow"]) {
        cfg.allow = a.iter().filter_map(|c| parse_cidr(c)).collect();
    }
    if let Some(l) = v["labels"].as_object() {
        for (k, x) in l {
            if let Some(s) = x.as_str() {
                cfg.labels.insert(k.clone(), s.to_string());
            }
        }
    }
    cfg.probes = strings(&v["probes"]);
    cfg.hide = strings(&v["hide"]).unwrap_or_default();
    cfg
}

struct Sampler {
    labels: HashMap<String, String>,
    probes: Vec<String>,
    socket: Option<String>,
    hide: Vec<String>,
    host: String,
    board: String,
    profile: &'static str,
    isa: Option<pawnio::NamedMutex>,
    pci: Option<pawnio::NamedMutex>,
    chips: Vec<superio::Chip>,
    amd: Option<cpu::AmdSensors>,
    cores: cpu::CoreLoad,
    procs: sys::Procs,
    gpus: gpu::Gpus,
    cpu_name: String,
    notes: Vec<String>,
    last_procs: Value,
    tick: u64,
    self_cpu: Option<(f64, Instant)>,
}

impl Sampler {
    fn new(cfg: &Config) -> Self {
        let isa = pawnio::NamedMutex::open(r"Global\Access_ISABUS.HTP.Method");
        let pci = pawnio::NamedMutex::open(r"Global\Access_PCI");
        let mut notes = Vec::new();
        let chips = match &isa {
            Some(m) => {
                let (c, log) = superio::detect(m);
                notes.extend(log);
                c
            }
            None => {
                notes.push("could not open ISA bus mutex".into());
                Vec::new()
            }
        };
        let amd = match cpu::AmdSensors::new() {
            Ok(a) => Some(a),
            Err(e) => {
                notes.push(e);
                None
            }
        };
        let b = board::detect();
        let keys: Vec<&str> = chips.iter().map(|c| c.key).collect();
        let prof = board::profile(&b, &keys);
        let mut labels = prof.labels;
        labels.extend(cfg.labels.clone());
        Sampler {
            labels,
            probes: cfg.probes.clone().unwrap_or(prof.probes),
            socket: prof.socket,
            hide: cfg.hide.clone(),
            host: std::env::var("COMPUTERNAME").unwrap_or_default(),
            board: format!("{} {}", b.manufacturer, b.product).trim().to_string(),
            profile: prof.name,
            isa,
            pci,
            chips,
            amd,
            cores: cpu::CoreLoad::new(),
            procs: sys::Procs::new(),
            gpus: gpu::Gpus::new(),
            cpu_name: cpu::cpu_id().2,
            notes,
            last_procs: json!({"cpu": [], "mem": []}),
            tick: 0,
            self_cpu: None,
        }
    }

    fn release(&mut self) {
        self.gpus.release();
        self.procs.release();
        self.self_cpu = None;
        sys::trim();
    }

    /// Refresh delta baselines without producing output (after an idle gap).
    fn prime(&mut self) {
        self.cores.sample();
        self.procs.sample();
        if let (Some(a), Some(p)) = (&mut self.amd, &self.pci) {
            a.read(p);
        }
    }

    fn sample(&mut self) -> Value {
        let t0 = Instant::now();
        self.tick += 1;
        let cores = self.cores.sample();
        let load = if cores.is_empty() { 0.0 } else { cores.iter().sum::<f32>() / cores.len() as f32 };
        let amd = match (&mut self.amd, &self.pci) {
            (Some(a), Some(p)) => Some(a.read(p)),
            _ => None,
        };
        let mem = sys::memory();
        let gpus = self.gpus.read();

        let mut temps = Vec::new();
        let mut fans = Vec::new();
        if let Some(isa) = &self.isa {
            if let Some(_g) = isa.lock(50) {
                for c in &self.chips {
                    let r = c.read();
                    for (i, v) in r.temps.iter().enumerate() {
                        let id = format!("{}.t{i}", c.key);
                        if self.hide.contains(&id) {
                            continue;
                        }
                        temps.push(json!({
                            "id": id,
                            "label": self.labels.get(&id).cloned().unwrap_or_else(|| board::generic_label(&id)),
                            "probe": self.probes.contains(&id),
                            "socket": self.socket.as_deref() == Some(id.as_str()),
                            "v": v,
                        }));
                    }
                    for (i, v) in r.fans.iter().enumerate() {
                        let id = format!("{}.f{i}", c.key);
                        if self.hide.contains(&id) {
                            continue;
                        }
                        fans.push(json!({
                            "id": id,
                            "label": self.labels.get(&id).cloned().unwrap_or_else(|| board::generic_label(&id)),
                            "rpm": v.map(|x| x.round()),
                        }));
                    }
                }
            }
        }

        if self.tick % 2 == 1 {
            let mut p = self.procs.sample();
            p.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
            let by_cpu: Vec<Value> = p
                .iter()
                .take(6)
                .map(|s| json!({"name": s.name, "n": s.count, "pct": (s.cpu_pct * 10.0).round() / 10.0}))
                .collect();
            p.sort_by(|a, b| b.mem_gb.total_cmp(&a.mem_gb));
            let by_mem: Vec<Value> = p
                .iter()
                .take(6)
                .map(|s| json!({"name": s.name, "n": s.count, "gb": (s.mem_gb * 100.0).round() / 100.0}))
                .collect();
            self.last_procs = json!({"cpu": by_cpu, "mem": by_mem});
        }

        let (rss_mb, cpu_s) = sys::self_usage();
        let now = Instant::now();
        let self_pct = self.self_cpu.map(|(prev, at)| {
            let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
            (cpu_s - prev) / now.duration_since(at).as_secs_f64() / ncpu * 100.0
        });
        self.self_cpu = Some((cpu_s, now));

        json!({
            // Snapshot schema version for mods (docs/mods.md). Bump only on breaking changes.
            "api": 1,
            "host": self.host,
            "board": {"name": self.board, "profile": self.profile},
            "t": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
            "cpu": {
                "name": self.cpu_name,
                "tctl": amd.as_ref().and_then(|a| a.tctl),
                "ccds": amd.as_ref().map(|a| a.ccds.clone()).unwrap_or_default(),
                "power_w": amd.as_ref().and_then(|a| a.package_w),
                "load": load,
                "cores": cores.iter().map(|c| (c * 10.0).round() / 10.0).collect::<Vec<_>>(),
            },
            "mem": {
                "used_gb": mem.used_gb, "total_gb": mem.total_gb,
                "commit_used_gb": mem.commit_used_gb, "commit_total_gb": mem.commit_total_gb,
                "pagefile_used_gb": mem.pagefile_used_gb, "pagefile_size_gb": mem.pagefile_size_gb,
                "pagefile_peak_gb": mem.pagefile_peak_gb,
            },
            "gpus": gpus,
            "sensors": {"temps": temps, "fans": fans},
            "procs": self.last_procs,
            "self": {"rss_mb": rss_mb, "cpu_pct": self_pct, "sample_ms": t0.elapsed().as_secs_f64() * 1000.0},
            "notes": self.notes,
        })
    }
}

#[derive(Default)]
struct HubState {
    subs: Vec<SyncSender<Arc<String>>>,
    latest: Option<(Instant, Arc<String>)>,
    demand_until: Option<Instant>,
}

struct Hub {
    st: Mutex<HubState>,
    cv: Condvar,
}

impl Hub {
    fn active(s: &HubState) -> bool {
        !s.subs.is_empty() || s.demand_until.map_or(false, |d| Instant::now() < d)
    }
}

fn run_sampler(hub: Arc<Hub>, mut s: Sampler) {
    let mut last = Instant::now() - Duration::from_secs(60);
    loop {
        {
            let mut st = hub.st.lock().unwrap();
            if !Hub::active(&st) {
                drop(st);
                s.release();
                st = hub.st.lock().unwrap();
                while !Hub::active(&st) {
                    st = hub.cv.wait_timeout(st, Duration::from_secs(5)).unwrap().0;
                }
            }
        }
        if last.elapsed() > TICK * 3 {
            s.prime();
            thread::sleep(Duration::from_millis(300));
        }
        let started = Instant::now();
        let msg = Arc::new(s.sample().to_string());
        last = Instant::now();
        {
            let mut st = hub.st.lock().unwrap();
            st.latest = Some((last, msg.clone()));
            st.subs.retain(|tx| !matches!(tx.try_send(msg.clone()), Err(TrySendError::Disconnected(_))));
            hub.cv.notify_all();
        }
        thread::sleep(TICK.saturating_sub(started.elapsed()));
    }
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn allowed(stream: &TcpStream, allow: &[(u32, u32)]) -> bool {
    let ok = |v4: std::net::Ipv4Addr| allow.iter().any(|(net, mask)| u32::from(v4) & mask == *net);
    match stream.peer_addr().map(|a| a.ip()) {
        Ok(std::net::IpAddr::V4(ip)) => ok(ip),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback() || ip.to_ipv4_mapped().map_or(false, ok),
        Err(_) => false,
    }
}

fn handle(mut stream: TcpStream, hub: Arc<Hub>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    // Drain headers.
    let mut h = String::new();
    while reader.read_line(&mut h).map(|n| n > 2).unwrap_or(false) {
        h.clear();
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/");
    match path {
        "/" | "/index.html" => respond(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/manifest.json" => respond(
            &mut stream,
            "200 OK",
            "application/manifest+json",
            br##"{"name":"hwcast","short_name":"hwcast","display":"standalone","background_color":"#0b0d10","theme_color":"#0b0d10","start_url":"/"}"##,
        ),
        "/api/now" => {
            let mut st = hub.st.lock().unwrap();
            let fresh = |s: &HubState| s.latest.as_ref().filter(|(at, _)| at.elapsed() < Duration::from_millis(1500)).map(|(_, m)| m.clone());
            let mut got = fresh(&st);
            if got.is_none() {
                st.demand_until = Some(Instant::now() + Duration::from_secs(4));
                hub.cv.notify_all();
                let deadline = Instant::now() + Duration::from_secs(4);
                while got.is_none() && Instant::now() < deadline {
                    st = hub.cv.wait_timeout(st, Duration::from_millis(200)).unwrap().0;
                    got = fresh(&st);
                }
            }
            drop(st);
            match got {
                Some(m) => respond(&mut stream, "200 OK", "application/json", m.as_bytes()),
                None => respond(&mut stream, "503 Service Unavailable", "text/plain", b"no sample"),
            }
        }
        "/events" => {
            let (tx, rx) = sync_channel::<Arc<String>>(4);
            let first = {
                let mut st = hub.st.lock().unwrap();
                st.subs.push(tx);
                hub.cv.notify_all();
                st.latest.as_ref().map(|(_, m)| m.clone())
            };
            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
            let _ = stream.set_nodelay(true);
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\nretry: 2000\n\n";
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            if let Some(m) = first {
                let _ = stream.write_all(format!("data: {m}\n\n").as_bytes());
            }
            loop {
                match rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(m) => {
                        if stream.write_all(format!("data: {m}\n\n").as_bytes()).is_err() {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if stream.write_all(b": ping\n\n").is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            // Dropping rx makes the sampler prune this subscriber on its next send.
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn serve(addr: String, hub: Arc<Hub>, allow: Arc<Vec<(u32, u32)>>) {
    loop {
        match TcpListener::bind(&addr) {
            Ok(l) => {
                eprintln!("listening on http://{addr}");
                for s in l.incoming().flatten() {
                    if !allowed(&s, &allow) {
                        continue; // not from this PC, the local network, or another allowed range
                    }
                    let hub = hub.clone();
                    thread::spawn(move || handle(s, hub));
                }
            }
            // The address may not exist yet at boot (network or VPN still coming up); keep trying.
            Err(_) => thread::sleep(Duration::from_secs(10)),
        }
    }
}

fn lower_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
        BELOW_NORMAL_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    };
    unsafe {
        let h = GetCurrentProcess();
        SetPriorityClass(h, BELOW_NORMAL_PRIORITY_CLASS);
        // EcoQoS: lets Windows schedule us on efficient, low-clock paths.
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        SetProcessInformation(
            h,
            ProcessPowerThrottling,
            &state as *const _ as _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        None => manage::double_click(),
        Some("--version" | "-V" | "version") => println!("hwcast {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h" | "help" | "/?") => manage::help(),
        Some("--once") => print_snapshot(),
        Some("run") => run_server(),
        Some("service") => service::run(),
        Some(cmd) => manage::dispatch(cmd, rest),
    }
}

fn print_snapshot() {
    let cfg = load_config();
    let mut s = Sampler::new(&cfg);
    s.prime();
    thread::sleep(TICK);
    println!("{}", serde_json::to_string_pretty(&s.sample()).unwrap());
}

/// The dashboard server itself. Runs until the process exits.
pub(crate) fn run_server() {
    let cfg = load_config();
    lower_priority();
    let hub = Arc::new(Hub { st: Mutex::new(HubState::default()), cv: Condvar::new() });
    let sampler = Sampler::new(&cfg);
    for n in &sampler.notes {
        eprintln!("{n}");
    }
    {
        let hub = hub.clone();
        thread::spawn(move || run_sampler(hub, sampler));
    }
    let allow = Arc::new(cfg.allow.clone());
    let mut handles = Vec::new();
    for addr in cfg.bind.clone() {
        let hub = hub.clone();
        let allow = allow.clone();
        handles.push(thread::spawn(move || serve(addr, hub, allow)));
    }
    for h in handles {
        let _ = h.join();
    }
}
