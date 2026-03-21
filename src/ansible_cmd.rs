use crate::tracking;
use crate::utils::{resolved_command, strip_ansi, truncate};
use anyhow::{Context, Result};

pub fn run(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("ansible-playbook");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: ansible-playbook {}", args.join(" "));
    }

    let output = cmd
        .output()
        .context("Failed to run ansible-playbook. Is Ansible installed?")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);
    let filtered = filter_ansible_output(&raw, output.status.success());

    println!("{}", filtered);

    timer.track(
        &format!("ansible-playbook {}", args.join(" ")),
        &format!("rtk ansible-playbook {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

fn filter_ansible_output(raw: &str, success: bool) -> String {
    let clean = strip_ansi(raw);
    let mut out: Vec<String> = Vec::new();
    let mut in_recap = false;
    let mut fallback: Vec<String> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("PLAY RECAP") {
            out.push("PLAY RECAP".to_string());
            in_recap = true;
            continue;
        }

        if in_recap {
            if trimmed.contains("ok=") {
                out.push(trimmed.to_string());
            }
            continue;
        }

        if trimmed.starts_with("PLAY [")
            || trimmed.starts_with("TASK [")
            || trimmed.starts_with("RUNNING HANDLER [")
        {
            out.push(trimmed.to_string());
            continue;
        }

        if trimmed.starts_with("changed:")
            || trimmed.starts_with("fatal:")
            || trimmed.starts_with("failed:")
            || trimmed.starts_with("unreachable:")
            || trimmed.contains("FAILED!")
        {
            out.push(truncate_result_line(trimmed));
            continue;
        }

        let lower = trimmed.to_lowercase();
        if lower.starts_with("error:")
            || lower.contains("no hosts matched")
            || lower.contains("could not match supplied host pattern")
        {
            out.push(trimmed.to_string());
            continue;
        }

        if !trimmed.starts_with("ok:") && !trimmed.starts_with("skipping:") {
            fallback.push(trimmed.to_string());
        }
    }

    if out.is_empty() {
        if success {
            return "ok ansible-playbook".to_string();
        }

        if fallback.is_empty() {
            return "failed ansible-playbook".to_string();
        }

        return fallback.into_iter().take(20).collect::<Vec<_>>().join("\n");
    }

    out.join("\n")
}

fn truncate_result_line(line: &str) -> String {
    if let Some((prefix, _)) = line.split_once(" => ") {
        return prefix.to_string();
    }
    truncate(line, 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_ansible_keeps_task_changed_and_recap() {
        let raw = r#"
PLAY [web] ********************************************************************

TASK [Gathering Facts] ********************************************************
ok: [host1]

TASK [Install nginx] **********************************************************
changed: [host1]

PLAY RECAP ********************************************************************
host1 : ok=2 changed=1 unreachable=0 failed=0 skipped=0 rescued=0 ignored=0
"#;

        let filtered = filter_ansible_output(raw, true);
        assert!(filtered.contains("PLAY [web]"));
        assert!(filtered.contains("TASK [Install nginx]"));
        assert!(filtered.contains("changed: [host1]"));
        assert!(filtered.contains("host1 : ok=2 changed=1"));
        assert!(!filtered.contains("ok: [host1]"));
    }

    #[test]
    fn test_filter_ansible_keeps_failure_signal() {
        let raw = r#"
TASK [Deploy app] *************************************************************
fatal: [host1]: FAILED! => {"msg":"permission denied"}
"#;
        let filtered = filter_ansible_output(raw, false);
        assert!(filtered.contains("TASK [Deploy app]"));
        assert!(filtered.contains("fatal: [host1]: FAILED!"));
        assert!(!filtered.contains("permission denied"));
    }
}
