//! MCP server over stdio: the same tools as the CLI, for agents.
//!
//! Hand-rolled JSON-RPC 2.0 (newline-delimited, per the MCP stdio
//! transport) instead of an SDK, since the surface is five tools and the
//! rest of the crate is synchronous. Register it with
//! `claude mcp add --transport stdio ilex-vm -- ilex-vmctl mcp`.
//!
//! Screenshots come back downscaled (longer edge 1568 px by default, what
//! image-reading models resample to anyway). `click`/`scroll` take
//! coordinates in the pixels of the last screenshot returned, and scale them
//! back to the framebuffer.

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ilex_devkit::Workspace;
use ilex_devkit::qemu::{self, base64};
use serde_json::{Value, json};

use crate::{Button, Vmctl, WaitCond};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_MAX_EDGE: u32 = 1568;

const INSTRUCTIONS: &str = "Drives the QEMU VM that `cargo xtask vm --disk` (or --gui) or the e2e tests booted for this \
checkout, over QMP. Take a `screenshot` first; `click`/`scroll` coordinates are in the pixels of the \
latest screenshot. It works before login (firmware, GRUB, GDM). Use SSH, not pixels, for assertions.";

/// Serve MCP on stdin/stdout until stdin closes. `ws` is `None` when no
/// checkout could be found; tools then explain how to fix that.
pub fn serve(ws: Option<Workspace>, default_vm: Option<String>) -> Result<()> {
    let mut server = Server { ws, default_vm, last_scale: None };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = server.handle_line(&line) else { continue };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

struct Server {
    ws: Option<Workspace>,
    default_vm: Option<String>,
    /// Screen pixels per screenshot pixel, from the last screenshot returned.
    last_scale: Option<f64>,
}

impl Server {
    fn handle_line(&mut self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_response(Value::Null, -32700, &format!("parse error: {e}")));
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(id) = id else {
            // Notification (e.g. notifications/initialized): no reply.
            return None;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => ok_response(id, initialize_result(&params)),
            "ping" => ok_response(id, json!({})),
            "tools/list" => ok_response(id, json!({ "tools": tool_definitions() })),
            "tools/call" => ok_response(id, self.call_tool(&params)),
            "resources/list" => ok_response(id, json!({ "resources": [] })),
            "prompts/list" => ok_response(id, json!({ "prompts": [] })),
            _ => error_response(id, -32601, &format!("method not found: {method}")),
        })
    }

    fn call_tool(&mut self, params: &Value) -> Value {
        let name = params["name"].as_str().unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        match self.run_tool(name, &args) {
            Ok(content) => json!({ "content": content, "isError": false }),
            Err(e) => json!({ "content": [text(&format!("{e:#}"))], "isError": true }),
        }
    }

    fn workspace(&self, args: &Value) -> Result<Workspace> {
        if let Some(slug) = args["slug"].as_str() {
            return Ok(Workspace { root: Default::default(), slug: slug.to_string(), worktree: None });
        }
        match &self.ws {
            Some(ws) => Ok(ws.clone()),
            None => Workspace::discover()
                .context("no ilex checkout found: start the server inside one, set ILEX_ROOT, or pass `slug`"),
        }
    }

    fn vm(&self, args: &Value) -> Result<Vmctl> {
        let ws = self.workspace(args)?;
        let purpose = args["vm"].as_str().or(self.default_vm.as_deref());
        Vmctl::attach(&ws, purpose)
    }

    /// Screen pixels per screenshot pixel; takes a screenshot to find out
    /// when none was returned yet.
    fn scale(&mut self, vm: &Vmctl) -> Result<f64> {
        if let Some(s) = self.last_scale {
            return Ok(s);
        }
        let f = vm.screenshot()?;
        let s = f.width.max(f.height) as f64 / f.width.max(f.height).min(DEFAULT_MAX_EDGE) as f64;
        self.last_scale = Some(s);
        Ok(s)
    }

    /// Convert tool coordinates to screen pixels.
    fn point(&mut self, vm: &Vmctl, args: &Value) -> Result<(u32, u32)> {
        let x = args["x"].as_f64().context("`x` is required")?;
        let y = args["y"].as_f64().context("`y` is required")?;
        let scale = if args["space"].as_str() == Some("screen") { 1.0 } else { self.scale(vm)? };
        Ok(((x * scale).round().max(0.0) as u32, (y * scale).round().max(0.0) as u32))
    }

    fn run_tool(&mut self, name: &str, args: &Value) -> Result<Vec<Value>> {
        match name {
            "vm_list" => {
                let ws = self.workspace(args)?;
                let vms = qemu::running_vms(&ws);
                if vms.is_empty() {
                    return Ok(vec![text(&format!(
                        "No VM running for `{}`. Boot one with `cargo xtask vm --disk` (headless) or `--gui`.",
                        ws.slug
                    ))]);
                }
                let lines: Vec<String> = vms
                    .iter()
                    .map(|v| {
                        format!(
                            "{} ({}): display {}, configured {}x{}, ssh: {}",
                            v.purpose,
                            v.name,
                            v.display,
                            v.width,
                            v.height,
                            v.ssh_command_line()
                        )
                    })
                    .collect();
                Ok(vec![text(&lines.join("\n"))])
            }
            "screenshot" => {
                let vm = self.vm(args)?;
                let max_edge = if args["full"].as_bool() == Some(true) {
                    0
                } else {
                    args["max_edge"].as_u64().map_or(DEFAULT_MAX_EDGE, |n| n as u32)
                };
                self.screenshot_result(&vm, max_edge)
            }
            "click" => {
                let vm = self.vm(args)?;
                let (x, y) = self.point(&vm, args)?;
                let button = Button::parse(args["button"].as_str().unwrap_or("left"))?;
                let count = if args["double"].as_bool() == Some(true) { 2 } else { 1 };
                vm.click(x, y, button, count)?;
                Ok(vec![text(&format!("clicked {} at screen ({x}, {y})", args["button"].as_str().unwrap_or("left")))])
            }
            "scroll" => {
                let vm = self.vm(args)?;
                let (x, y) = self.point(&vm, args)?;
                let up = match args["direction"].as_str().unwrap_or("down") {
                    "up" => true,
                    "down" => false,
                    other => bail!("direction must be `up` or `down`, not `{other}`"),
                };
                let amount = args["amount"].as_u64().unwrap_or(3) as u32;
                vm.scroll(x, y, up, amount)?;
                Ok(vec![text(&format!("scrolled {amount} at screen ({x}, {y})"))])
            }
            "type_text" => {
                let vm = self.vm(args)?;
                let t = args["text"].as_str().context("`text` is required")?;
                vm.type_text(t)?;
                Ok(vec![text(&format!("typed {} characters", t.chars().count()))])
            }
            "key" => {
                let vm = self.vm(args)?;
                let combo = args["combo"].as_str().context("`combo` is required")?;
                vm.key(combo)?;
                Ok(vec![text(&format!("pressed {combo}"))])
            }
            "wait_screen" => {
                let vm = self.vm(args)?;
                let cond = WaitCond {
                    nonblank: args["nonblank"].as_bool().unwrap_or(false),
                    changed: args["changed"].as_bool().unwrap_or(false),
                    stable: args["stable_secs"].as_f64().map(Duration::from_secs_f64),
                    size: args["size"].as_str().map(crate::parse_size).transpose()?,
                };
                let timeout = Duration::from_secs_f64(args["timeout_secs"].as_f64().unwrap_or(120.0));
                let started = std::time::Instant::now();
                let (png, frame) = vm.wait_screen(&cond, timeout)?;
                let mut out = vec![text(&format!("screen ready after {:.1}s", started.elapsed().as_secs_f64()))];
                out.extend(self.image_content(&frame, &png, DEFAULT_MAX_EDGE)?);
                Ok(out)
            }
            other => bail!("unknown tool `{other}`"),
        }
    }

    fn screenshot_result(&mut self, vm: &Vmctl, max_edge: u32) -> Result<Vec<Value>> {
        let png = vm.screenshot_png()?;
        let frame = crate::Frame::decode(&png)?;
        self.image_content(&frame, &png, max_edge)
    }

    /// Image content for `frame`, shrunk to `max_edge` (0 = as is); records
    /// the scale for later clicks.
    fn image_content(&mut self, frame: &crate::Frame, png: &[u8], max_edge: u32) -> Result<Vec<Value>> {
        let small = frame.downscale(max_edge);
        let scale = frame.width as f64 / small.width as f64;
        self.last_scale = Some(scale);
        let data = if small.width == frame.width { png.to_vec() } else { small.encode_png()? };
        let note = format!(
            "Screen is {}x{}; this image is {}x{}. click/scroll x,y are in this image's pixels{}.",
            frame.width,
            frame.height,
            small.width,
            small.height,
            if frame.is_blank() { ". The screen is blank." } else { "" }
        );
        Ok(vec![
            json!({ "type": "image", "data": base64(&data), "mimeType": "image/png" }),
            text(&note),
        ])
    }
}

fn text(s: &str) -> Value {
    json!({ "type": "text", "text": s })
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize_result(params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str().unwrap_or("");
    let version = if PROTOCOL_VERSIONS.contains(&requested) { requested } else { PROTOCOL_VERSIONS[0] };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "ilex-vmctl", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn tool_definitions() -> Value {
    let vm = json!({ "type": "string", "description": "Which VM when several run (`gui`, `vm`, `graphical`, ...); default: the only running one" });
    let slug = json!({ "type": "string", "description": "Attach to another checkout's VM by worktree slug (default: this checkout)" });
    let point = |extra: Value| {
        let mut props = json!({
            "x": { "type": "number", "description": "X in the last screenshot's pixels" },
            "y": { "type": "number", "description": "Y in the last screenshot's pixels" },
            "space": { "type": "string", "enum": ["screenshot", "screen"], "description": "`screen` = raw framebuffer pixels" },
            "vm": vm.clone(),
            "slug": slug.clone(),
        });
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        props
    };
    json!([
        {
            "name": "vm_list",
            "description": "List the running VMs of this checkout, with their SSH command lines.",
            "inputSchema": { "type": "object", "properties": { "slug": slug } }
        },
        {
            "name": "screenshot",
            "description": "Take a screenshot of the VM's display (works before login: firmware, GRUB, GDM).",
            "inputSchema": { "type": "object", "properties": {
                "max_edge": { "type": "integer", "description": "Longer edge of the returned image in pixels (default 1568)" },
                "full": { "type": "boolean", "description": "Return the framebuffer at full resolution (large)" },
                "vm": vm, "slug": slug
            } }
        },
        {
            "name": "click",
            "description": "Move the pointer to (x, y) and click.",
            "inputSchema": { "type": "object", "required": ["x", "y"], "properties": point(json!({
                "button": { "type": "string", "enum": ["left", "middle", "right"] },
                "double": { "type": "boolean" }
            })) }
        },
        {
            "name": "scroll",
            "description": "Scroll the mouse wheel at (x, y).",
            "inputSchema": { "type": "object", "required": ["x", "y"], "properties": point(json!({
                "direction": { "type": "string", "enum": ["up", "down"] },
                "amount": { "type": "integer", "description": "Wheel notches (default 3)" }
            })) }
        },
        {
            "name": "type_text",
            "description": "Type text on a US keyboard; a newline presses Return.",
            "inputSchema": { "type": "object", "required": ["text"], "properties": {
                "text": { "type": "string" }, "vm": vm, "slug": slug
            } }
        },
        {
            "name": "key",
            "description": "Press a key or combination: `Return`, `Escape`, `Tab`, `ctrl+alt+t`, `super`, `alt+F4`.",
            "inputSchema": { "type": "object", "required": ["combo"], "properties": {
                "combo": { "type": "string" }, "vm": vm, "slug": slug
            } }
        },
        {
            "name": "wait_screen",
            "description": "Wait until the screen is not blank (default), has changed, and/or has been unchanged for `stable_secs`; returns a screenshot.",
            "inputSchema": { "type": "object", "properties": {
                "nonblank": { "type": "boolean" },
                "changed": { "type": "boolean", "description": "Differs from the first picture seen" },
                "stable_secs": { "type": "number", "description": "Unchanged for this many seconds" },
                "size": { "type": "string", "description": "Framebuffer size to wait for, e.g. 3840x2160 (firmware and Plymouth draw at another size during boot)" },
                "timeout_secs": { "type": "number", "description": "Give up after this long (default 120)" },
                "vm": vm, "slug": slug
            } }
        }
    ])
}
