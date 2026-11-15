//! The build writes JSON so the shipped agent needs no TOML dependency.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::{Value, json};

type Result<T> = std::result::Result<T, String>;
const IMAGE: &str = "/usr/share/ilex/image.json";
const MACHINE: &str = "/usr/share/ilex/machine.json";

fn read(path: &str) -> Result<Value> {
    let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    if value["schema"].as_u64() != Some(1) {
        return Err(format!("{path}: `schema` must be 1"));
    }
    Ok(value)
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("`{key}` must be a non-empty string"))
}

fn list<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    value[key]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("`{key}` must be a list"))
}

fn image() -> Result<Value> {
    let value = read(IMAGE)?;
    for (key, default) in [
        ("shells", "default-shell"),
        ("design-systems", "default-design-system"),
    ] {
        let entries = list(&value, key).map_err(|e| format!("{IMAGE}: {e}"))?;
        let names = entries
            .iter()
            .map(|v| string(v, "name"))
            .collect::<Result<Vec<_>>>()?;
        let valid = if names.is_empty() {
            value.get(default).is_some_and(Value::is_null)
        } else {
            value[default]
                .as_str()
                .is_some_and(|name| names.contains(&name))
        };
        if !valid {
            return Err(format!(
                "{IMAGE}: `{default}` must name an entry in `{key}`, or be null when empty"
            ));
        }
    }
    Ok(value)
}

fn machine() -> Result<Value> {
    let value = read(MACHINE)?;
    string(&value, "name").map_err(|e| format!("{MACHINE}: {e}"))?;
    string(&value, "description").map_err(|e| format!("{MACHINE}: {e}"))?;
    if !value["packages"].is_object() {
        return Err(format!("{MACHINE}: `packages` must be an object"));
    }
    Ok(value)
}

pub fn run(json_output: bool) -> ExitCode {
    match print(json_output) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ilex-agent: config: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print(json_output: bool) -> Result<()> {
    let image = image()?;
    let machine = machine()?;
    if json_output {
        println!("{}", json!({"image": image, "machine": machine}));
    } else {
        println!(
            "machine:         {} ({})",
            string(&machine, "name")?,
            string(&machine, "description")?
        );
        for (label, key, default) in [
            ("shells", "shells", "default-shell"),
            ("design systems", "design-systems", "default-design-system"),
        ] {
            let entries = list(&image, key)?
                .iter()
                .map(|entry| {
                    let name = string(entry, "name")?;
                    Ok(format!(
                        "{name}{}",
                        if image[default].as_str() == Some(name) {
                            " (default)"
                        } else {
                            ""
                        }
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            println!(
                "{:<17}{}",
                format!("{label}:"),
                if entries.is_empty() {
                    "(none)".into()
                } else {
                    entries.join(", ")
                }
            );
        }
    }
    Ok(())
}

pub fn check_image() -> Result<()> {
    image().map(|_| ())
}
pub fn check_machine() -> Result<()> {
    machine().map(|_| ())
}

pub fn check_target() -> Result<()> {
    let image = image()?;
    let expected = if list(&image, "shells")?.is_empty() {
        "multi-user.target"
    } else {
        "graphical.target"
    };
    let actual = fs::read_link("/etc/systemd/system/default.target").map_err(|e| e.to_string())?;
    if actual.file_name().is_some_and(|name| name == expected) {
        Ok(())
    } else {
        Err(format!(
            "default.target points to {}, expected {expected}",
            actual.display()
        ))
    }
}

pub fn check_sessions() -> Result<()> {
    for shell in list(&image()?, "shells")? {
        let session = &shell["session"];
        let desktop = string(session, "desktop")?;
        let compositor = string(session, "compositor")?;
        super::exists(&format!("/usr/share/wayland-sessions/{desktop}.desktop"))?;
        super::exists(&format!("/usr/bin/{compositor}"))?;
    }
    Ok(())
}

pub fn check_greeter() -> Result<()> {
    let image = image()?;
    let Some(default) = image["default-shell"].as_str() else {
        return Ok(());
    };
    let shell = list(&image, "shells")?
        .iter()
        .find(|s| s["name"].as_str() == Some(default))
        .ok_or("default shell is missing")?;
    let unit = string(&shell["greeter"], "unit")?;
    let actual =
        fs::read_link("/etc/systemd/system/display-manager.service").map_err(|e| e.to_string())?;
    if actual.file_name().is_some_and(|name| name == unit)
        && Path::new("/etc/systemd/system/display-manager.service").exists()
    {
        Ok(())
    } else {
        Err(format!(
            "display-manager.service points to {}, expected {unit}",
            actual.display()
        ))
    }
}

pub fn check_tokens() -> Result<()> {
    for design in list(&image()?, "design-systems")? {
        let path = string(design, "tokens")?;
        let text = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let tokens: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        if !tokens.is_object() {
            return Err(format!("{path}: tokens must be a JSON object"));
        }
    }
    Ok(())
}

pub fn check_packages() -> Result<()> {
    let machine = machine()?;
    if let Some(install) = machine["packages"].get("install") {
        for package in install
            .as_array()
            .ok_or("machine packages.install must be a list")?
        {
            let name = package
                .as_str()
                .ok_or("machine packages.install must contain strings")?;
            if name.starts_with('@') {
                continue;
            }
            let output = Command::new("rpm")
                .args(["-q", "--", name])
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(format!("machine package `{name}` is not installed"));
            }
        }
    }
    Ok(())
}
