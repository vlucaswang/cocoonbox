//! Tool plugins an image installs as JSON files in one directory: script tools that run with
//! `script_run`'s contract and their own program.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Automation;
use crate::tools;

const MAX_DESCRIPTION: usize = 2048;

/// One plugin tool.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Plugin {
    pub name: String,
    pub description: String,
    /// The program that runs the tool's scripts in place of Node.
    pub bin: PathBuf,
    #[serde(default)]
    pub resources: Vec<Resource>,
}

/// A file the plugin serves over MCP.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Resource {
    pub uri: String,
    pub path: PathBuf,
    pub mime: String,
    pub description: String,
}

impl Plugin {
    fn check(&self) -> Result<(), String> {
        let name_ok = self.name.ends_with("_run")
            && self.name.starts_with(|c: char| c.is_ascii_lowercase())
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !name_ok {
            return Err(format!("name {:?} is not ^[a-z][a-z0-9_]*_run$", self.name));
        }
        let builtin = tools::main_tools(true, Automation::Desk)
            .into_iter()
            .chain(tools::remote_tools(Automation::Desk))
            .any(|t| t.name == self.name.as_str());
        if builtin {
            return Err(format!("name {} is a built-in tool", self.name));
        }
        if self.description.chars().count() > MAX_DESCRIPTION {
            return Err(format!("description over {MAX_DESCRIPTION} characters"));
        }
        if !self.bin.is_absolute() {
            return Err(format!("bin {} is not absolute", self.bin.display()));
        }
        Ok(())
    }
}

/// Reads the `*.json` plugins in `dir` in file-name order; a missing directory has none, a service
/// spec, which shuttle runs, is no script tool, and a file that breaks a rule or repeats a name is
/// skipped with one log line.
pub fn load(dir: &Path) -> Vec<Plugin> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let mut plugins: Vec<Plugin> = Vec::new();
    for file in files {
        let raw = std::fs::read(&file).map_err(|e| e.to_string());
        if raw.as_ref().is_ok_and(|raw| {
            serde_json::from_slice::<serde_json::Value>(raw)
                .is_ok_and(|v| v.get("service").is_some())
        }) {
            continue;
        }
        let plugin = raw
            .and_then(|raw| serde_json::from_slice::<Plugin>(&raw).map_err(|e| e.to_string()))
            .and_then(|p| p.check().map(|()| p))
            .and_then(|p| {
                if plugins.iter().any(|q| q.name == p.name) {
                    Err(format!("name {} is taken", p.name))
                } else {
                    Ok(p)
                }
            });
        match plugin {
            Ok(p) => plugins.push(p),
            Err(e) => eprintln!("plugins: skip {}: {e}", file.display()),
        }
    }
    plugins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, file: &str, body: &str) {
        std::fs::write(dir.join(file), body).unwrap();
    }

    #[test]
    fn loads_valid_plugins_in_file_name_order_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(
            d,
            "20-b.json",
            r#"{"name": "beta_run", "description": "b", "bin": "/usr/bin/b"}"#,
        );
        write(
            d,
            "10-a.json",
            r#"{"name": "alpha_run", "description": "a", "bin": "/usr/bin/a", "resources": [{"uri": "cocoonbox://a/GUIDE.md", "path": "/opt/a/GUIDE.md", "mime": "text/markdown", "description": "g"}]}"#,
        );
        write(
            d,
            "30-badname.json",
            r#"{"name": "Gamma", "description": "c", "bin": "/usr/bin/c"}"#,
        );
        write(
            d,
            "31-builtin.json",
            r#"{"name": "script_run", "description": "c", "bin": "/usr/bin/c"}"#,
        );
        write(
            d,
            "32-relative.json",
            r#"{"name": "rel_run", "description": "c", "bin": "bin/c"}"#,
        );
        write(
            d,
            "33-dup.json",
            r#"{"name": "alpha_run", "description": "c", "bin": "/usr/bin/c"}"#,
        );
        write(
            d,
            "34-long.json",
            &format!(
                r#"{{"name": "long_run", "description": "{}", "bin": "/usr/bin/c"}}"#,
                "x".repeat(MAX_DESCRIPTION + 1)
            ),
        );
        write(d, "35-broken.json", "{");
        write(
            d,
            "36-service.json",
            r#"{"service": "cocoonbox", "bin": "/usr/local/bin/cocoonbox-agent", "args": ["toolhost"], "tools": []}"#,
        );
        write(
            d,
            "40-c.txt",
            r#"{"name": "text_run", "description": "c", "bin": "/usr/bin/c"}"#,
        );
        let got = load(d);
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["alpha_run", "beta_run"]);
        assert_eq!(got[0].resources[0].path, PathBuf::from("/opt/a/GUIDE.md"));
        assert!(got[1].resources.is_empty());
    }

    #[test]
    fn a_missing_directory_has_no_plugins() {
        assert!(load(Path::new("/nonexistent/tools.d")).is_empty());
    }
}
