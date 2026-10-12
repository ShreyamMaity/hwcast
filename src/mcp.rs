//! MCP (Model Context Protocol) server so AI agents can read this PC's sensors.
//!
//! The service answers MCP at `POST /mcp` (streamable HTTP, stateless, plain JSON
//! replies). `hwcast mcp` bridges stdio to that endpoint for agents that launch
//! their MCP servers as local commands. Every tool is read-only.

use serde_json::{json, Map, Value};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::time::Instant;

const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Read-only live hardware telemetry from a Windows PC running hwcast: CPU, NVIDIA GPUs, memory, \
motherboard sensors and top processes. Start with get_summary. Before putting work on a GPU, call check_gpu_free. \
Use sample for averages over a few seconds instead of a single reading, and wait_for to wait until a job finishes \
or a part cools down. Metric names for sample and wait_for come from list_metrics.";

/// Where tools get their data. Implemented by the service's sampler hub.
pub trait Source {
    /// The current snapshot (sampled on demand when nobody is watching).
    fn now(&self) -> Option<Value>;
    /// Calls `f` with each new snapshot (about one per second) until it returns
    /// false or `secs` have passed.
    fn watch(&self, secs: f64, f: &mut dyn FnMut(&Value) -> bool);
}

/// Handles one JSON-RPC message or batch. `None` means nothing to send back
/// (notifications and responses).
pub fn handle(msg: &Value, src: &dyn Source) -> Option<Value> {
    if let Some(batch) = msg.as_array() {
        let out: Vec<Value> = batch.iter().filter_map(|m| one(m, src)).collect();
        return (!out.is_empty()).then(|| Value::Array(out));
    }
    one(msg, src)
}

fn one(msg: &Value, src: &dyn Source) -> Option<Value> {
    let id = msg.get("id").filter(|v| !v.is_null())?.clone();
    let Some(method) = msg["method"].as_str() else {
        return None; // a response to something we never send
    };
    let p = &msg["params"];
    let result = match method {
        "initialize" => {
            let asked = p["protocolVersion"].as_str().unwrap_or("");
            let ver = PROTOCOLS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOLS[0]);
            Ok(json!({
                "protocolVersion": ver,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "hwcast", "title": "hwcast", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call(p["name"].as_str().unwrap_or(""), &p["arguments"], src)),
        "resources/list" => Ok(json!({"resources": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, m)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": m}}),
    })
}

pub fn parse_error() -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})
}

// ---------------------------------------------------------------- tools

/// Tool definitions live in mcp-tools.json (name, title, description, input
/// properties, required); every tool shares the same schema wrapper and hints.
fn tools() -> Vec<Value> {
    let defs: Vec<Value> = serde_json::from_str(include_str!("mcp-tools.json")).expect("mcp-tools.json");
    defs.into_iter()
        .map(|d| {
            json!({
                "name": d["name"],
                "title": d["title"],
                "description": d["description"],
                "inputSchema": {
                    "type": "object",
                    "properties": d["properties"],
                    "required": d.get("required").cloned().unwrap_or(json!([])),
                    "additionalProperties": false,
                },
                "annotations": {"title": d["title"], "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            })
        })
        .collect()
}

fn call(name: &str, args: &Value, src: &dyn Source) -> Value {
    let out = match name {
        "sample" => sample(args, src),
        "check_gpu_free" => check_gpu_free(args, src),
        "wait_for" => wait_for(args, src),
        _ => match src.now() {
            None => Err("hwcast has no reading yet; try again in a few seconds".into()),
            Some(s) => match name {
                "get_summary" => Ok(summary(&s)),
                "get_cpu" => Ok(cpu(&s)),
                "get_gpus" => gpus(&s, args),
                "get_memory" => Ok(memory(&s)),
                "get_sensors" => Ok(sensors(&s)),
                "get_processes" => Ok(processes(&s)),
                "get_snapshot" => Ok(s),
                "list_metrics" => Ok(list_metrics(&s)),
                _ => Err(format!("unknown tool: {name}")),
            },
        },
    };
    match out {
        Ok(v) => json!({"content": [{"type": "text", "text": strip_nulls(v).to_string()}]}),
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
    }
}

/// Rounds to `d` decimals; drops NaN.
fn r(x: Option<f64>, d: i32) -> Value {
    match x {
        Some(v) if v.is_finite() => {
            let m = 10f64.powi(d);
            json!((v * m).round() / m)
        }
        _ => Value::Null,
    }
}

fn f(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_nulls(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        o => o,
    }
}

fn busiest(s: &Value) -> Option<f64> {
    s["cpu"]["cores"].as_array()?.iter().filter_map(f).reduce(f64::max)
}

fn pct(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) if b > 0.0 => Some(a / b * 100.0),
        _ => None,
    }
}

fn gpu_view(g: &Value) -> Value {
    let used = f(&g["vram_used_gb"]);
    let total = f(&g["vram_total_gb"]);
    json!({
        "index": g["index"],
        "name": g["name"],
        "temp_c": g["temp"],
        "hotspot_c": r(f(&g["hotspot"]), 1),
        "mem_temp_c": r(f(&g["mem_temp"]), 1),
        "load_pct": g["util"],
        "mem_controller_pct": g["mem_util"],
        "vram_used_gb": r(used, 2),
        "vram_free_gb": r(total.zip(used).map(|(t, u)| t - u), 2),
        "vram_total_gb": r(total, 2),
        "power_w": r(f(&g["power_w"]), 0),
        "power_limit_w": r(f(&g["power_limit_w"]), 0),
        "clock_mhz": g["clock_mhz"],
        "fan_pct": g["fan_pct"],
        "fan_rpm": g["fan_rpm"],
    })
}

fn summary(s: &Value) -> Value {
    let c = &s["cpu"];
    let m = &s["mem"];
    let gpus: Vec<Value> = s["gpus"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|g| {
                    let used = f(&g["vram_used_gb"]);
                    let total = f(&g["vram_total_gb"]);
                    json!({
                        "index": g["index"], "name": g["name"],
                        "temp_c": g["temp"], "hotspot_c": r(f(&g["hotspot"]), 0),
                        "load_pct": g["util"],
                        "vram_used_gb": r(used, 1), "vram_total_gb": r(total, 1),
                        "power_w": r(f(&g["power_w"]), 0), "fan_pct": g["fan_pct"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let top = |k: &str, unit: &str| -> Value {
        s["procs"][k].as_array().and_then(|a| a.first()).map_or(Value::Null, |p| json!({"name": p["name"], unit: r(f(&p[unit]), 1)}))
    };
    json!({
        "host": s["host"],
        "cpu": {
            "name": c["name"],
            "temp_c": r(f(&c["tctl"]), 1),
            "load_pct": r(f(&c["load"]), 1),
            "busiest_thread_pct": r(busiest(s), 0),
            "power_w": r(f(&c["power_w"]), 0),
        },
        "gpus": gpus,
        "memory": {
            "used_gb": r(f(&m["used_gb"]), 1),
            "total_gb": r(f(&m["total_gb"]), 1),
            "used_pct": r(pct(f(&m["used_gb"]), f(&m["total_gb"])), 0),
            "pagefile_used_gb": r(f(&m["pagefile_used_gb"]), 1),
        },
        "top_cpu": top("cpu", "pct"),
        "top_memory": top("mem", "gb"),
    })
}

fn cpu(s: &Value) -> Value {
    let c = &s["cpu"];
    let cores: Vec<Value> = c["cores"].as_array().map(|a| a.iter().map(|v| r(f(v), 0)).collect()).unwrap_or_default();
    json!({
        "name": c["name"],
        "temp_c": r(f(&c["tctl"]), 1),
        "ccd_temps_c": c["ccds"].as_array().map(|a| a.iter().map(|v| r(f(v), 1)).collect::<Vec<_>>()),
        "power_w": r(f(&c["power_w"]), 1),
        "load_pct": r(f(&c["load"]), 1),
        "busiest_thread_pct": r(busiest(s), 0),
        "thread_loads_pct": cores,
    })
}

fn gpus(s: &Value, args: &Value) -> Result<Value, String> {
    let all = s["gpus"].as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        return Ok(json!({"gpus": [], "note": "no NVIDIA GPU found"}));
    }
    match args["index"].as_u64() {
        Some(i) => all
            .iter()
            .find(|g| g["index"].as_u64() == Some(i))
            .map(gpu_view)
            .ok_or_else(|| format!("no GPU with index {i}; this PC has {}", all.len())),
        None => Ok(json!({"gpus": all.iter().map(gpu_view).collect::<Vec<_>>()})),
    }
}

fn memory(s: &Value) -> Value {
    let m = &s["mem"];
    json!({
        "ram_used_gb": r(f(&m["used_gb"]), 2),
        "ram_total_gb": r(f(&m["total_gb"]), 2),
        "ram_used_pct": r(pct(f(&m["used_gb"]), f(&m["total_gb"])), 1),
        "commit_used_gb": r(f(&m["commit_used_gb"]), 2),
        "commit_limit_gb": r(f(&m["commit_total_gb"]), 2),
        "pagefile_used_gb": r(f(&m["pagefile_used_gb"]), 2),
        "pagefile_size_gb": r(f(&m["pagefile_size_gb"]), 2),
        "pagefile_peak_gb": r(f(&m["pagefile_peak_gb"]), 2),
    })
}

fn sensors(s: &Value) -> Value {
    let list = |k: &str, unit: &str| -> Vec<Value> {
        s["sensors"][k]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| {
                        let mut o = json!({"id": x["id"], "label": x["label"]});
                        o[unit] = if k == "temps" { x["v"].clone() } else { x["rpm"].clone() };
                        if x["probe"] == true {
                            o["probe"] = json!(true);
                        }
                        o
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let temps = list("temps", "c");
    let fans = list("fans", "rpm");
    let mut out = json!({"board": s["board"]["name"], "temps": temps, "fans": fans});
    if out["temps"].as_array().map_or(true, |a| a.is_empty()) {
        out["note"] = json!("no motherboard sensors: the PawnIO driver is missing or this board's chip is not supported");
    }
    out
}

fn processes(s: &Value) -> Value {
    let list = |k: &str, unit: &str, out: &str| -> Vec<Value> {
        s["procs"][k]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| json!({"name": p["name"], "processes": p["n"], out: r(f(&p[unit]), 1)}))
            .collect()
    };
    json!({"by_cpu": list("cpu", "pct", "cpu_pct"), "by_memory": list("mem", "gb", "memory_gb")})
}

// ---------------------------------------------------------------- metrics

/// Every metric with its unit, read from one snapshot. Names match the
/// hwcast-gcb200 mod where they overlap.
fn metrics(s: &Value) -> Vec<(String, &'static str, Option<f64>)> {
    let mut out = Vec::new();
    let c = &s["cpu"];
    out.push(("cpu.temp".into(), "C", f(&c["tctl"])));
    out.push(("cpu.load".into(), "%", f(&c["load"])));
    out.push(("cpu.maxcore".into(), "%", busiest(s)));
    out.push(("cpu.power_w".into(), "W", f(&c["power_w"])));
    for (i, v) in c["ccds"].as_array().into_iter().flatten().enumerate() {
        out.push((format!("cpu.ccd{i}"), "C", f(v)));
    }
    for g in s["gpus"].as_array().into_iter().flatten() {
        let n = g["index"].as_u64().unwrap_or(0);
        let used = f(&g["vram_used_gb"]);
        let total = f(&g["vram_total_gb"]);
        let p = |k: &str| format!("gpu{n}.{k}");
        out.push((p("temp"), "C", f(&g["temp"])));
        out.push((p("hotspot"), "C", f(&g["hotspot"])));
        out.push((p("memtemp"), "C", f(&g["mem_temp"])));
        out.push((p("load"), "%", f(&g["util"])));
        out.push((p("memload"), "%", f(&g["mem_util"])));
        out.push((p("vram"), "%", pct(used, total)));
        out.push((p("vram_used"), "GB", used));
        out.push((p("vram_free"), "GB", total.zip(used).map(|(t, u)| t - u)));
        out.push((p("power"), "%", pct(f(&g["power_w"]), f(&g["power_limit_w"]))));
        out.push((p("power_w"), "W", f(&g["power_w"])));
        out.push((p("fan"), "%", f(&g["fan_pct"])));
        out.push((p("fan_rpm"), "rpm", f(&g["fan_rpm"])));
        out.push((p("clock"), "MHz", f(&g["clock_mhz"])));
    }
    let m = &s["mem"];
    out.push(("mem.used".into(), "%", pct(f(&m["used_gb"]), f(&m["total_gb"]))));
    out.push(("mem.used_gb".into(), "GB", f(&m["used_gb"])));
    out.push(("mem.commit".into(), "%", pct(f(&m["commit_used_gb"]), f(&m["commit_total_gb"]))));
    out.push(("pagefile.used_gb".into(), "GB", f(&m["pagefile_used_gb"])));
    for (k, unit, field) in [("temps", "C", "v"), ("fans", "rpm", "rpm")] {
        for x in s["sensors"][k].as_array().into_iter().flatten() {
            if let Some(id) = x["id"].as_str() {
                out.push((format!("board.{id}"), unit, f(&x[field])));
            }
        }
    }
    out
}

fn metric(s: &Value, name: &str) -> Option<(&'static str, Option<f64>)> {
    metrics(s).into_iter().find(|(n, _, _)| n == name).map(|(_, u, v)| (u, v))
}

fn list_metrics(s: &Value) -> Value {
    let board: Map<String, Value> = s["sensors"]["temps"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(s["sensors"]["fans"].as_array().into_iter().flatten())
        .filter_map(|x| Some((format!("board.{}", x["id"].as_str()?), x["label"].clone())))
        .collect();
    let list: Vec<Value> = metrics(s)
        .into_iter()
        .filter(|(_, _, v)| v.is_some())
        .map(|(n, u, v)| {
            let mut o = json!({"name": n, "unit": u, "now": r(v, 1)});
            if let Some(l) = board.get(&n) {
                o["label"] = l.clone();
            }
            o
        })
        .collect();
    json!({"metrics": list, "note": "Metrics this PC does not report right now are left out."})
}

fn default_metrics(s: &Value) -> Vec<String> {
    let mut m = vec!["cpu.temp".to_string(), "cpu.load".to_string()];
    for g in s["gpus"].as_array().into_iter().flatten() {
        let n = g["index"].as_u64().unwrap_or(0);
        for k in ["temp", "load", "vram_used", "power_w"] {
            m.push(format!("gpu{n}.{k}"));
        }
    }
    m
}

fn secs(args: &Value, key: &str, default: f64, min: f64, max: f64) -> f64 {
    args[key].as_f64().unwrap_or(default).clamp(min, max)
}

#[derive(Default)]
struct Stat {
    n: u32,
    sum: f64,
    min: f64,
    max: f64,
    last: f64,
}

impl Stat {
    fn add(&mut self, v: f64) {
        if self.n == 0 {
            self.min = v;
            self.max = v;
        }
        self.n += 1;
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.last = v;
    }
    fn json(&self, unit: &str) -> Value {
        if self.n == 0 {
            return json!({"unit": unit, "note": "no readings"});
        }
        json!({"unit": unit, "min": r(Some(self.min), 1), "avg": r(Some(self.sum / self.n as f64), 1), "max": r(Some(self.max), 1), "last": r(Some(self.last), 1)})
    }
}

/// Collects readings of `names` for `secs` seconds.
fn collect(src: &dyn Source, names: &[String], secs: f64) -> (u32, Vec<(&'static str, Stat)>) {
    let mut stats: Vec<(&'static str, Stat)> = names.iter().map(|_| ("", Stat::default())).collect();
    let mut n = 0;
    src.watch(secs, &mut |s| {
        n += 1;
        for (i, name) in names.iter().enumerate() {
            if let Some((u, v)) = metric(s, name) {
                stats[i].0 = u;
                if let Some(v) = v {
                    stats[i].1.add(v);
                }
            }
        }
        true
    });
    (n, stats)
}

fn sample(args: &Value, src: &dyn Source) -> Result<Value, String> {
    let now = src.now().ok_or("hwcast has no reading yet; try again in a few seconds")?;
    let names: Vec<String> = match args["metrics"].as_array() {
        Some(a) if !a.is_empty() => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => default_metrics(&now),
    };
    let unknown: Vec<&String> = names.iter().filter(|n| metric(&now, n).is_none()).collect();
    if !unknown.is_empty() {
        return Err(format!("unknown metric(s): {}. Call list_metrics for the names this PC offers.", unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let t = secs(args, "seconds", 5.0, 1.0, 60.0);
    let (n, stats) = collect(src, &names, t);
    let mut out = Map::new();
    for (name, (u, st)) in names.iter().zip(stats) {
        out.insert(name.clone(), st.json(u));
    }
    Ok(json!({"seconds": t, "readings": n, "metrics": out}))
}

fn check_gpu_free(args: &Value, src: &dyn Source) -> Result<Value, String> {
    let now = src.now().ok_or("hwcast has no reading yet; try again in a few seconds")?;
    let gpus: Vec<(u64, Value)> = now["gpus"].as_array().into_iter().flatten().map(|g| (g["index"].as_u64().unwrap_or(0), g["name"].clone())).collect();
    if gpus.is_empty() {
        return Ok(json!({"free_gpus": [], "gpus": [], "note": "no NVIDIA GPU found"}));
    }
    let need = args["need_vram_gb"].as_f64().unwrap_or(0.0).max(0.0);
    let max_load = args["max_load_pct"].as_f64().unwrap_or(10.0);
    let t = secs(args, "seconds", 3.0, 1.0, 30.0);
    let names: Vec<String> = gpus.iter().flat_map(|(i, _)| [format!("gpu{i}.load"), format!("gpu{i}.vram_free"), format!("gpu{i}.temp")]).collect();
    let (_, stats) = collect(src, &names, t);
    let mut list = Vec::new();
    let mut free = Vec::new();
    let mut best: Option<(u64, f64)> = None;
    for (k, (i, name)) in gpus.iter().enumerate() {
        let (load, vram, temp) = (&stats[k * 3].1, &stats[k * 3 + 1].1, &stats[k * 3 + 2].1);
        let mut reasons = Vec::new();
        if load.n == 0 {
            reasons.push("no readings".to_string());
        } else {
            if load.max > max_load {
                reasons.push(format!("load peaked at {}%", load.max.round()));
            }
            if vram.min < need {
                reasons.push(format!("only {:.1} GB VRAM free, {need:.1} GB needed", vram.min));
            }
        }
        let ok = reasons.is_empty();
        if ok {
            free.push(*i);
            if best.map_or(true, |(_, v)| vram.min > v) {
                best = Some((*i, vram.min));
            }
        }
        list.push(json!({
            "index": i, "name": name, "free": ok,
            "avg_load_pct": r(load.n.gt(&0).then(|| load.sum / load.n as f64), 0),
            "peak_load_pct": r(load.n.gt(&0).then_some(load.max), 0),
            "min_vram_free_gb": r(vram.n.gt(&0).then_some(vram.min), 2),
            "temp_c": r(temp.n.gt(&0).then_some(temp.last), 0),
            "busy_because": if ok { Value::Null } else { json!(reasons.join("; ")) },
        }));
    }
    Ok(json!({
        "free_gpus": free,
        "best": best.map(|(i, _)| i),
        "gpus": list,
        "checked": {"seconds": t, "max_load_pct": max_load, "need_vram_gb": need},
        "note": "Free means idle and enough VRAM now; another program may still start using it.",
    }))
}

fn wait_for(args: &Value, src: &dyn Source) -> Result<Value, String> {
    let name = args["metric"].as_str().ok_or("metric is required")?.to_string();
    let (below, above) = (args["below"].as_f64(), args["above"].as_f64());
    if below.is_none() == above.is_none() {
        return Err("give exactly one of below or above".into());
    }
    let now = src.now().ok_or("hwcast has no reading yet; try again in a few seconds")?;
    let (unit, _) = metric(&now, &name).ok_or_else(|| format!("unknown metric {name}. Call list_metrics for the names this PC offers."))?;
    let hold = secs(args, "hold_seconds", 5.0, 0.0, 300.0);
    let timeout = secs(args, "timeout_seconds", 120.0, 1.0, 600.0);
    let start = Instant::now();
    let mut since: Option<Instant> = None;
    let mut last = None;
    let mut met = false;
    src.watch(timeout, &mut |s| {
        let v = metric(s, &name).and_then(|(_, v)| v);
        last = v;
        let ok = v.map_or(false, |v| below.map_or(true, |b| v < b) && above.map_or(true, |a| v > a));
        if !ok {
            since = None;
            return true;
        }
        let at = *since.get_or_insert_with(Instant::now);
        if at.elapsed().as_secs_f64() + 0.5 >= hold {
            met = true;
            return false;
        }
        true
    });
    Ok(json!({
        "met": met,
        "metric": name,
        "unit": unit,
        "last": r(last, 1),
        "waited_seconds": r(Some(start.elapsed().as_secs_f64()), 0),
        "note": if met { Value::Null } else { json!("timed out before the condition held") },
    }))
}

// ---------------------------------------------------------------- stdio bridge

/// `hwcast mcp [--host <addr:port>]`: speaks MCP over stdin/stdout and forwards
/// each message to the hwcast service's /mcp endpoint.
pub fn stdio(args: &[String]) {
    let host = args
        .iter()
        .position(|a| a == "--host")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| format!("127.0.0.1:{}", crate::manage::configured_port()));
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match post(&host, &line) {
            Ok(Some(body)) => Some(body),
            Ok(None) => None,
            Err(e) => {
                // Answer requests with an error so the agent shows why; ignore notifications.
                let id = serde_json::from_str::<Value>(&line).ok().and_then(|m| m.get("id").cloned()).filter(|v| !v.is_null());
                id.map(|id| {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": format!(
                        "cannot reach the hwcast service at {host} ({e}). Check it is installed and running with `hwcast status`."
                    )}})
                    .to_string()
                })
            }
        };
        if let Some(r) = reply {
            let _ = writeln!(out, "{}", r.trim());
            let _ = out.flush();
        }
    }
}

/// One HTTP POST to /mcp. Ok(None) for 202 Accepted (nothing to relay).
fn post(host: &str, body: &str) -> Result<Option<String>, String> {
    let mut s = TcpStream::connect(host).map_err(|e| e.to_string())?;
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("bad reply")?;
    let status = head.split_whitespace().nth(1).unwrap_or("");
    match status {
        "200" => Ok(Some(body.to_string())),
        "202" => Ok(None),
        "404" => Err("this hwcast is older than 0.2.0 and has no MCP endpoint; update it".into()),
        _ => Err(format!("HTTP {status}: {}", body.trim())),
    }
}

/// True when a browser Origin header may use /mcp: only pages served from this
/// PC. Agents do not send Origin; this blocks DNS-rebinding from web pages.
pub fn origin_ok(origin: Option<&str>) -> bool {
    let Some(o) = origin else { return true };
    let host = o.split("://").nth(1).unwrap_or(o);
    let host = host.rsplit_once(':').filter(|(h, p)| !h.is_empty() && p.chars().all(|c| c.is_ascii_digit())).map_or(host, |(h, _)| h);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake(RefCell<Vec<Value>>);

    fn snap(load0: f64, vram_used0: f64) -> Value {
        json!({
            "api": 1, "host": "PC", "t": 0,
            "board": {"name": "Board", "profile": "p"},
            "cpu": {"name": "CPU", "tctl": 60.4, "ccds": [58.0], "power_w": 50.0, "load": 12.0, "cores": [10.0, 40.0]},
            "mem": {"used_gb": 32.0, "total_gb": 64.0, "commit_used_gb": 40.0, "commit_total_gb": 80.0, "pagefile_used_gb": 1.0, "pagefile_size_gb": 16.0, "pagefile_peak_gb": 2.0},
            "gpus": [
                {"index": 0, "name": "A", "temp": 50, "util": load0, "mem_util": 5, "hotspot": 60.0, "mem_temp": null, "vram_used_gb": vram_used0, "vram_total_gb": 16.0, "power_w": 100.0, "power_limit_w": 300.0, "clock_mhz": 2000, "fan_pct": 30, "fan_rpm": 1000},
                {"index": 1, "name": "B", "temp": 40, "util": 0, "mem_util": 0, "hotspot": null, "mem_temp": null, "vram_used_gb": 1.0, "vram_total_gb": 20.0, "power_w": 20.0, "power_limit_w": 320.0, "clock_mhz": 300, "fan_pct": 0, "fan_rpm": 0}
            ],
            "sensors": {"temps": [{"id": "it.t0", "label": "System", "probe": false, "socket": false, "v": 40.0}], "fans": [{"id": "it.f0", "label": "CPU fan", "rpm": 900.0}]},
            "procs": {"cpu": [{"name": "x", "n": 1, "pct": 5.0}], "mem": [{"name": "y", "n": 1, "gb": 2.0}]},
            "self": {}, "notes": []
        })
    }

    impl Source for Fake {
        fn now(&self) -> Option<Value> {
            self.0.borrow().first().cloned()
        }
        fn watch(&self, _secs: f64, f: &mut dyn FnMut(&Value) -> bool) {
            for s in self.0.borrow().iter() {
                if !f(s) {
                    break;
                }
            }
        }
    }

    fn text(v: &Value) -> Value {
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn rpc(src: &Fake, method: &str, params: Value) -> Value {
        handle(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}), src).unwrap()
    }

    #[test]
    fn initialize_and_list() {
        let f = Fake(RefCell::new(vec![snap(0.0, 1.0)]));
        let r = rpc(&f, "initialize", json!({"protocolVersion": "2025-06-18"}));
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        let r = rpc(&f, "initialize", json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(r["result"]["protocolVersion"], PROTOCOLS[0]);
        let r = rpc(&f, "tools/list", json!({}));
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 11);
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), &f).is_none());
        assert_eq!(rpc(&f, "nope", json!({}))["error"]["code"], -32601);
    }

    #[test]
    fn summary_and_metrics() {
        let f = Fake(RefCell::new(vec![snap(50.0, 4.0)]));
        let s = text(&rpc(&f, "tools/call", json!({"name": "get_summary"})));
        assert_eq!(s["cpu"]["temp_c"], 60.4);
        assert_eq!(s["cpu"]["busiest_thread_pct"], 40.0);
        assert_eq!(s["memory"]["used_pct"], 50.0);
        assert!(s["gpus"][1].get("hotspot_c").is_none());
        let g = text(&rpc(&f, "tools/call", json!({"name": "get_gpus", "arguments": {"index": 0}})));
        assert_eq!(g["vram_free_gb"], 12.0);
        let m = text(&rpc(&f, "tools/call", json!({"name": "list_metrics"})));
        let names: Vec<&str> = m["metrics"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"gpu1.vram_free") && names.contains(&"board.it.t0") && !names.contains(&"gpu0.memtemp"));
    }

    #[test]
    fn sample_and_gpu_free() {
        let f = Fake(RefCell::new(vec![snap(0.0, 2.0), snap(80.0, 10.0), snap(20.0, 2.0)]));
        let s = text(&rpc(&f, "tools/call", json!({"name": "sample", "arguments": {"metrics": ["gpu0.load"]}})));
        assert_eq!(s["metrics"]["gpu0.load"]["max"], 80.0);
        assert_eq!(s["metrics"]["gpu0.load"]["avg"], 33.3);
        let bad = rpc(&f, "tools/call", json!({"name": "sample", "arguments": {"metrics": ["gpu9.load"]}}));
        assert_eq!(bad["result"]["isError"], true);
        let c = text(&rpc(&f, "tools/call", json!({"name": "check_gpu_free", "arguments": {"need_vram_gb": 8}})));
        assert_eq!(c["free_gpus"], json!([1]));
        assert_eq!(c["best"], 1);
        assert_eq!(c["gpus"][0]["free"], false);
    }

    #[test]
    fn wait() {
        let f = Fake(RefCell::new(vec![snap(90.0, 2.0), snap(3.0, 2.0)]));
        let w = text(&rpc(&f, "tools/call", json!({"name": "wait_for", "arguments": {"metric": "gpu0.load", "below": 5, "hold_seconds": 0}})));
        assert_eq!(w["met"], true);
        assert_eq!(w["last"], 3.0);
        let w = text(&rpc(&f, "tools/call", json!({"name": "wait_for", "arguments": {"metric": "gpu0.load", "below": 1}})));
        assert_eq!(w["met"], false);
    }

    #[test]
    fn origins() {
        assert!(origin_ok(None));
        assert!(origin_ok(Some("http://localhost:6274")));
        assert!(origin_ok(Some("http://127.0.0.1")));
        assert!(!origin_ok(Some("http://evil.example:8340")));
        assert!(!origin_ok(Some("null")));
    }
}
