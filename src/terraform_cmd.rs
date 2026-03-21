use crate::tracking;
use crate::utils::{resolved_command, strip_ansi};
use anyhow::{Context, Result};
use regex::Regex;

const MAX_FALLBACK_LINES: usize = 60;

pub fn run(args: &[String], verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = resolved_command("terraform");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: terraform {}", args.join(" "));
    }

    let output = cmd.output().context("Failed to run terraform")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);
    let filtered = filter_terraform_output(&raw);

    println!("{}", filtered);

    timer.track(
        &format!("terraform {}", args.join(" ")),
        &format!("rtk terraform {}", args.join(" ")),
        &raw,
        &filtered,
    );

    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

fn filter_terraform_output(raw: &str) -> String {
    let clean = strip_ansi(raw);
    let mut kept: Vec<String> = Vec::new();
    let mut fallback: Vec<String> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if is_terraform_noise(trimmed) {
            continue;
        }

        if is_high_signal(trimmed) {
            kept.push(trimmed.to_string());
            continue;
        }

        fallback.push(trimmed.to_string());
    }

    if kept.is_empty() {
        kept.extend(fallback.into_iter().take(MAX_FALLBACK_LINES));
    }

    if kept.is_empty() {
        "ok terraform".to_string()
    } else {
        kept.join("\n")
    }
}

fn is_terraform_noise(line: &str) -> bool {
    line.starts_with("Acquiring state lock")
        || line.starts_with("Releasing state lock")
        || line.contains("Refreshing state...")
        || line.starts_with("Reading...")
        || line.starts_with("Read complete after")
        || line.starts_with("Still creating...")
        || line.starts_with("Still modifying...")
        || line.starts_with("Still destroying...")
}

fn is_high_signal(line: &str) -> bool {
    lazy_static::lazy_static! {
        static ref RESOURCE_ACTION_RE: Regex = Regex::new(
            r"^#\s+.+\s+will be\s+(created|destroyed|read during apply|updated in-place|replaced)$"
        ).unwrap();
    }

    line.starts_with("Error:")
        || line.starts_with("Warning:")
        || line.starts_with("No changes.")
        || line.starts_with("Terraform will perform")
        || line.starts_with("Plan:")
        || line.starts_with("Apply complete!")
        || line.starts_with("Destroy complete!")
        || line.starts_with("Changes to Outputs")
        || line.starts_with("Outputs:")
        || RESOURCE_ACTION_RE.is_match(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_terraform_plan_removes_refresh_noise() {
        let raw = r#"
Acquiring state lock. This may take a few moments...
module.app.aws_instance.web: Refreshing state... [id=i-123]
Terraform will perform the following actions:
  # module.app.aws_instance.web will be updated in-place
  ~ resource "aws_instance" "web" {
      instance_type = "t3.micro" -> "t3.small"
    }
Plan: 0 to add, 1 to change, 0 to destroy.
"#;
        let filtered = filter_terraform_output(raw);
        assert!(filtered.contains("Terraform will perform the following actions:"));
        assert!(filtered.contains("# module.app.aws_instance.web will be updated in-place"));
        assert!(filtered.contains("Plan: 0 to add, 1 to change, 0 to destroy."));
        assert!(!filtered.contains("Refreshing state"));
        assert!(!filtered.contains("instance_type"));
        assert!(!filtered.contains("resource \"aws_instance\""));
    }

    #[test]
    fn test_filter_terraform_keeps_errors() {
        let raw = r#"
module.app.aws_s3_bucket.assets: Refreshing state... [id=bucket]
Error: Unsupported argument
  on main.tf line 12, in resource "aws_s3_bucket" "assets":
"#;
        let filtered = filter_terraform_output(raw);
        assert!(filtered.contains("Error: Unsupported argument"));
    }
}
