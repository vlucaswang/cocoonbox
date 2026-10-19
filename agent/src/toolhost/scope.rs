use std::collections::BTreeSet;

use serde_json::Value;
use zbus::Connection;
use zbus::zvariant::Value as Variant;

const DRIVER_CGROUPS: [&str; 2] = ["/cua-driver.service", "/cocoon-shuttle.service"];
const SYSTEMD: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";

pub async fn adopt(launched: &Value) -> Result<(), String> {
    let s = &launched["structuredContent"];
    let roots = roots(s);
    let Some(&pid) = roots.last() else {
        return Ok(());
    };
    let pids = tokio::task::spawn_blocking(move || driver_tree(&roots))
        .await
        .map_err(|e| format!("walk the launch: {e}"))?;
    if pids.is_empty() {
        return Ok(());
    }
    let unit = unit_name(s["name"].as_str().unwrap_or("app"), pid);
    let conn = Connection::session()
        .await
        .map_err(|e| format!("connect the session bus: {e}"))?;
    let properties = [
        ("PIDs", Variant::from(pids)),
        ("CollectMode", Variant::from("inactive-or-failed")),
    ];
    let aux: Vec<(&str, Vec<(&str, Variant<'_>)>)> = Vec::new();
    conn.call_method(
        Some(SYSTEMD),
        SYSTEMD_PATH,
        Some(MANAGER),
        "StartTransientUnit",
        &(unit.as_str(), "fail", &properties[..], aux),
    )
    .await
    .map_err(|e| format!("StartTransientUnit {unit}: {e}"))?;
    Ok(())
}

fn roots(s: &Value) -> Vec<u32> {
    let mut out = Vec::new();
    for v in [&s["launcher_pid"], &s["pid"]] {
        if let Some(p) = v.as_u64().and_then(|p| u32::try_from(p).ok())
            && !out.contains(&p)
        {
            out.push(p);
        }
    }
    out
}

fn driver_tree(roots: &[u32]) -> Vec<u32> {
    let mut seen = BTreeSet::new();
    let mut todo = roots.to_vec();
    while let Some(p) = todo.pop() {
        if seen.insert(p) {
            todo.extend(children(p));
        }
    }
    seen.into_iter().filter(|&p| in_driver(p)).collect()
}

fn children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|t| std::fs::read_to_string(t.path().join("children")).ok())
        .flat_map(|c| {
            c.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}

fn in_driver(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|c| {
        c.lines()
            .any(|l| DRIVER_CGROUPS.iter().any(|g| l.ends_with(g)))
    })
}

fn unit_name(app: &str, pid: u32) -> String {
    let base = app.rsplit('/').next().unwrap_or(app);
    let app: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    format!("app-desk-{app}-{pid}.scope")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unit_names_are_valid_systemd_scopes() {
        assert_eq!(unit_name("mousepad", 42), "app-desk-mousepad-42.scope");
        assert_eq!(
            unit_name("/usr/local/bin/cocoonbox-chrome", 7),
            "app-desk-cocoonbox-chrome-7.scope"
        );
        assert_eq!(
            unit_name("Google Chrome", 1),
            "app-desk-Google_Chrome-1.scope"
        );
    }

    #[test]
    fn roots_are_the_launcher_and_the_app() {
        assert_eq!(roots(&json!({"pid": 5, "launcher_pid": 4})), [4, 5]);
        assert_eq!(roots(&json!({"pid": 4, "launcher_pid": 4})), [4]);
        assert_eq!(roots(&json!({"pid": null, "launcher_pid": 4})), [4]);
        assert!(roots(&json!({})).is_empty());
    }

    #[test]
    fn the_tree_holds_children_and_leaves_out_other_cgroups() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let me = std::process::id();
        assert!(children(me).contains(&child.id()), "children of {me}");
        assert!(driver_tree(&[me]).is_empty());
        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");
    }

    #[tokio::test]
    async fn a_launch_without_a_pid_is_left_alone() {
        adopt(&json!({"structuredContent": {"pid": null}}))
            .await
            .expect("adopt");
        adopt(&json!({"structuredContent": {"pid": std::process::id()}}))
            .await
            .expect("adopt");
    }
}
