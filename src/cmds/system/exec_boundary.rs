//! Filter the command that runs behind an exec boundary.
//!
//! `docker exec`, `kubectl exec`, `ssh`, `nix shell -c`, and an Ansible ad-hoc
//! command all print the inner command's stdout. The remote argv is left
//! unchanged. When the inner command is one this crate already filters
//! (`git log`, `find`, `grep`, `journalctl`), that filter runs on the captured
//! stdout. Anything we cannot parse is left as a passthrough.

use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;

use crate::core::runner::{self, RunOptions};
use crate::core::tracking::args_display;
use crate::core::utils::{ChildArgExt, resolved_command};

use super::pipe_cmd;

#[derive(Clone, Copy)]
enum ExecKind {
    Docker,
    Compose,
    Podman,
}

/// Run `tool` and filter its stdout when `args` are an exec boundary.
/// Otherwise stream the command unchanged.
pub(crate) fn run_or_passthrough(tool: &str, args: &[OsString], verbose: u8) -> Result<i32> {
    match owned_argv(tool, args).filter(|argv| boundary_filter(argv).is_some()) {
        Some(argv) => run_filtered_tool(tool, args, argv, verbose),
        None => runner::run_passthrough(tool, args, verbose),
    }
}

/// Fallback path for tools that are not clap subcommands (`ssh`, `nix`, `podman`, `ansible`).
/// `None` means the caller should keep its existing filter.
pub(crate) fn run_if_boundary(args: &[String]) -> Result<Option<i32>> {
    if args.is_empty() || boundary_filter(args).is_none() {
        return Ok(None);
    }
    let rest: Vec<OsString> = args[1..].iter().map(OsString::from).collect();
    Ok(Some(run_filtered_tool(&args[0], &rest, args.to_vec(), 0)?))
}

fn run_filtered_tool(tool: &str, args: &[OsString], argv: Vec<String>, verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("rtk: filtering output past an exec boundary ({tool})");
    }
    let mut cmd = resolved_command(tool);
    cmd.child_args(args);
    let args_str = args_display(args);
    runner::run_filtered(
        cmd,
        tool,
        &args_str,
        move |stdout| apply(&argv, stdout).unwrap_or_else(|| stdout.to_string()),
        RunOptions::stdout_only().inherit_stdin(),
    )
}

fn owned_argv(tool: &str, args: &[OsString]) -> Option<Vec<String>> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(tool.to_string());
    for arg in args {
        argv.push(arg.to_str()?.to_string());
    }
    Some(argv)
}

fn boundary_filter(argv: &[String]) -> Option<&'static str> {
    let inner = inner_command(argv)?;
    let name = command_filter(&inner)?;
    pipe_cmd::resolve_filter(name).map(|_| name)
}

fn apply(argv: &[String], stdout: &str) -> Option<String> {
    let inner = inner_command(argv)?;
    let name = command_filter(&inner)?;
    let filter = pipe_cmd::resolve_filter(name)?;
    let tool = program(argv.first()?);
    if tool == "ansible" {
        return Some(filter_ansible(filter, stdout));
    }
    let text = if tool == "ssh" {
        strip_transport_noise(stdout)
    } else {
        stdout.to_string()
    };
    Some(filter(&text))
}

fn inner_command(argv: &[String]) -> Option<Vec<String>> {
    let tool = program(argv.first()?);
    match tool {
        "docker" => docker_inner(argv).map(|cmd| cmd.to_vec()),
        "podman" => podman_inner(argv).map(|cmd| cmd.to_vec()),
        "kubectl" | "oc" => kubectl_inner(argv).map(|cmd| cmd.to_vec()),
        "ssh" => ssh_inner(argv).map(|cmd| cmd.to_vec()),
        "nix" => nix_inner(argv).map(|cmd| cmd.to_vec()),
        "ansible" => ansible_inner(argv),
        _ => None,
    }
}

fn program(argv0: &str) -> &str {
    let base = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(argv0);
    base.strip_suffix(".exe").unwrap_or(base)
}

fn command_filter(argv: &[String]) -> Option<&'static str> {
    match program(argv.first()?) {
        "git" => git_filter(argv),
        "find" | "fd" => Some("find"),
        "grep" | "rg" => Some("grep"),
        "journalctl" => Some("log"),
        _ => None,
    }
}

fn git_filter(args: &[String]) -> Option<&'static str> {
    const VALUE: &[&str] = &[
        "-c",
        "-C",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--exec-path",
        "--config-env",
        "--super-prefix",
    ];
    const BOOL: &[&str] = &[
        "--no-pager",
        "--paginate",
        "-p",
        "--bare",
        "--literal-pathspecs",
        "--glob-pathspecs",
        "--noglob-pathspecs",
        "--icase-pathspecs",
        "--no-optional-locks",
        "--no-replace-objects",
    ];
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        if VALUE.contains(&arg) {
            i += 2;
            continue;
        }
        if let Some((name, _)) = arg.split_once('=')
            && (VALUE.contains(&name) || BOOL.contains(&name))
        {
            i += 1;
            continue;
        }
        if BOOL.contains(&arg) {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            return None;
        }
        return match arg {
            "log" => Some("git-log"),
            "diff" => Some("git-diff"),
            "status" => Some("git-status"),
            _ => None,
        };
    }
    None
}

fn docker_inner(args: &[String]) -> Option<&[String]> {
    let mut i = 1;
    while args.get(i).is_some_and(|arg| arg.starts_with('-')) {
        i = skip_named(
            args,
            i,
            &[
                "-H",
                "--host",
                "-c",
                "--context",
                "--config",
                "-l",
                "--log-level",
            ],
            &["--debug", "--tls", "--tlsverify"],
        )?;
    }
    match args.get(i).map(String::as_str) {
        Some("exec") => exec_command(&args[i + 1..], ExecKind::Docker),
        Some("compose") => compose_inner(&args[i + 1..]),
        _ => None,
    }
}

fn compose_inner(args: &[String]) -> Option<&[String]> {
    let mut i = 0;
    while args.get(i).is_some_and(|arg| arg.starts_with('-')) {
        i = skip_named(
            args,
            i,
            &[
                "-f",
                "--file",
                "-p",
                "--project-name",
                "--project-directory",
                "--env-file",
                "--profile",
                "--progress",
            ],
            &[],
        )?;
    }
    if args.get(i).map(String::as_str) != Some("exec") {
        return None;
    }
    exec_command(&args[i + 1..], ExecKind::Compose)
}

fn podman_inner(args: &[String]) -> Option<&[String]> {
    let mut i = 1;
    while args.get(i).is_some_and(|arg| arg.starts_with('-')) {
        i = skip_named(
            args,
            i,
            &["--url", "--connection", "-c", "--identity", "--log-level"],
            &["--remote", "--debug"],
        )?;
    }
    if args.get(i).map(String::as_str) != Some("exec") {
        return None;
    }
    exec_command(&args[i + 1..], ExecKind::Podman)
}

fn exec_command(args: &[String], kind: ExecKind) -> Option<&[String]> {
    let mut i = 0;
    while i < args.len() && (args[i] == "--" || args[i].starts_with('-')) {
        if args[i] == "--" {
            i += 1;
            break;
        }
        i = skip_exec_flag(args, i, kind)?;
    }
    let cmd = args.get(i + 1..)?;
    if cmd.is_empty() { None } else { Some(cmd) }
}

fn skip_exec_flag(args: &[String], i: usize, kind: ExecKind) -> Option<usize> {
    let arg = args.get(i)?.as_str();
    if is_exec_bool(arg, kind) {
        return Some(i + 1);
    }
    if is_exec_value(arg, kind) {
        return args.get(i + 1).map(|_| i + 2);
    }
    if let Some((name, _)) = arg.split_once('=')
        && is_exec_value(name, kind)
    {
        return Some(i + 1);
    }
    if let Some(body) = arg.strip_prefix('-')
        && !body.starts_with('-')
        && let Some(first) = body.chars().next()
    {
        let short = format!("-{first}");
        if body.chars().nth(1).is_some() && is_exec_value(&short, kind) {
            return Some(i + 1);
        }
        if body.chars().all(|c| matches!(c, 'i' | 't' | 'd' | 'T')) {
            return Some(i + 1);
        }
    }
    None
}

fn is_exec_bool(arg: &str, kind: ExecKind) -> bool {
    let common = matches!(
        arg,
        "-i" | "-t" | "-d" | "-T" | "--detach" | "--interactive" | "--tty" | "--privileged"
    );
    common || (matches!(kind, ExecKind::Podman) && matches!(arg, "-l" | "--latest"))
}

fn is_exec_value(arg: &str, kind: ExecKind) -> bool {
    let common = matches!(
        arg,
        "-e" | "--env" | "-u" | "--user" | "-w" | "--workdir" | "--env-file" | "--detach-keys"
    );
    if common {
        return true;
    }
    match kind {
        ExecKind::Docker => matches!(arg, "-l" | "--label"),
        ExecKind::Compose => matches!(arg, "--index"),
        ExecKind::Podman => false,
    }
}

fn kubectl_inner(args: &[String]) -> Option<&[String]> {
    let mut i = 1;
    loop {
        let arg = args.get(i)?.as_str();
        if arg == "exec" {
            let rest = &args[i + 1..];
            let sep = rest.iter().position(|token| token == "--")?;
            let cmd = &rest[sep + 1..];
            return if cmd.is_empty() { None } else { Some(cmd) };
        }
        if arg.starts_with('-') {
            i = skip_k8s_global(args, i)?;
            continue;
        }
        return None;
    }
}

fn skip_k8s_global(args: &[String], i: usize) -> Option<usize> {
    const VALUE: &[&str] = &[
        "--context",
        "--cluster",
        "--user",
        "--kubeconfig",
        "--namespace",
        "-n",
        "--token",
        "--as",
        "--as-group",
        "--certificate-authority",
        "--client-certificate",
        "--client-key",
        "--request-timeout",
        "--server",
        "-s",
        "--cache-dir",
        "-v",
        "--v",
    ];
    const BOOL: &[&str] = &["--insecure-skip-tls-verify", "--disable-compression"];
    let arg = args.get(i)?.as_str();
    if BOOL.contains(&arg) {
        return Some(i + 1);
    }
    if VALUE.contains(&arg) {
        return args.get(i + 1).map(|_| i + 2);
    }
    if let Some((name, _)) = arg.split_once('=')
        && VALUE.contains(&name)
    {
        return Some(i + 1);
    }
    if let Some(rest) = arg.strip_prefix("-n")
        && !rest.is_empty()
    {
        return Some(i + 1);
    }
    None
}

fn ssh_inner(args: &[String]) -> Option<&[String]> {
    const VALUE: &[&str] = &[
        "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o", "-p",
        "-Q", "-R", "-S", "-W", "-w",
    ];
    const BOOL_CHARS: &str = "46AaCfGgKkMNnqsTtvXxYy";
    let mut i = 1;
    let mut host_seen = false;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return non_empty(&args[i + 1..]);
        }
        if arg.starts_with("--") {
            return None;
        }
        if let Some(flags) = arg.strip_prefix('-')
            && let Some(first) = flags.chars().next()
        {
            let short = format!("-{first}");
            if VALUE.contains(&short.as_str()) {
                i += if flags.chars().nth(1).is_none() { 2 } else { 1 };
                continue;
            }
            if flags.chars().all(|c| BOOL_CHARS.contains(c)) {
                i += 1;
                continue;
            }
            return None;
        }
        if !host_seen {
            host_seen = true;
            i += 1;
            continue;
        }
        return non_empty(&args[i..]);
    }
    None
}

fn nix_inner(args: &[String]) -> Option<&[String]> {
    let mut i = 1;
    while args.get(i).is_some_and(|arg| arg.starts_with('-')) {
        i = skip_nix_global(args, i)?;
    }
    let sub = args.get(i)?.as_str();
    if !matches!(sub, "shell" | "develop" | "run") {
        return None;
    }
    let rest = &args[i + 1..];
    if let Some(pos) = rest
        .iter()
        .position(|arg| arg == "-c" || arg == "--command")
    {
        return non_empty(&rest[pos + 1..]);
    }
    if sub == "run"
        && let Some(pos) = rest.iter().position(|arg| arg == "--")
    {
        return non_empty(&rest[pos + 1..]);
    }
    None
}

fn skip_nix_global(args: &[String], i: usize) -> Option<usize> {
    const BOOL: &[&str] = &[
        "--offline",
        "--refresh",
        "--impure",
        "--no-write-lock-file",
        "--accept-flake-config",
        "--no-accept-flake-config",
        "-L",
        "--print-build-logs",
        "--keep-going",
        "--fallback",
        "--repair",
        "--no-net",
        "--no-eval-cache",
        "--show-trace",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--debug",
        "--ignore-try",
        "--allow-dirty",
        "--no-allow-dirty",
        "--relaxed-sandbox",
        "--no-sandbox",
    ];
    const ONE: &[&str] = &[
        "-I",
        "--store",
        "--experimental-features",
        "--log-format",
        "--access-tokens",
        "--file",
        "-f",
        "--include",
        "--expr",
        "-E",
        "--arg",
        "--argstr",
        "--override-input",
        "--update-input",
        "--max-jobs",
        "-j",
        "--cores",
        "--builders",
        "--system",
        "--extra-experimental-features",
        "--plugin-files",
    ];
    let arg = args.get(i)?.as_str();
    if BOOL.contains(&arg) {
        return Some(i + 1);
    }
    if arg == "--option" {
        return (i + 2 < args.len()).then_some(i + 3);
    }
    if ONE.contains(&arg) {
        return args.get(i + 1).map(|_| i + 2);
    }
    if let Some((name, _)) = arg.split_once('=')
        && (ONE.contains(&name) || BOOL.contains(&name))
    {
        return Some(i + 1);
    }
    None
}

fn ansible_inner(args: &[String]) -> Option<Vec<String>> {
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "-a" || arg == "--args" {
            return split_words(args.get(i + 1)?);
        }
        if let Some(value) = arg.strip_prefix("--args=") {
            return split_words(value);
        }
        i += 1;
    }
    None
}

fn split_words(value: &str) -> Option<Vec<String>> {
    let words: Vec<String> = value.split_whitespace().map(str::to_string).collect();
    if words.is_empty() { None } else { Some(words) }
}

fn non_empty(args: &[String]) -> Option<&[String]> {
    if args.is_empty() { None } else { Some(args) }
}

fn skip_named(args: &[String], i: usize, value: &[&str], flags: &[&str]) -> Option<usize> {
    let arg = args.get(i)?.as_str();
    if flags.contains(&arg) {
        return Some(i + 1);
    }
    if value.contains(&arg) {
        return args.get(i + 1).map(|_| i + 2);
    }
    if let Some((name, _)) = arg.split_once('=')
        && value.contains(&name)
    {
        return Some(i + 1);
    }
    None
}

fn filter_ansible(filter: fn(&str) -> String, stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_host_header(line))
        .map(|(idx, _)| idx)
        .collect();
    if headers.is_empty() {
        return filter(&strip_transport_noise(stdout));
    }
    let mut out = Vec::new();
    if headers[0] > 0 {
        out.extend(lines[..headers[0]].iter().map(|line| (*line).to_string()));
    }
    for (n, &idx) in headers.iter().enumerate() {
        out.push(lines[idx].to_string());
        let end = headers.get(n + 1).copied().unwrap_or(lines.len());
        let body = strip_transport_noise(&lines[idx + 1..end].join("\n"));
        let filtered = filter(&body);
        let trimmed = filtered.trim_end();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    let mut rendered = out.join("\n");
    if stdout.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

fn is_host_header(line: &str) -> bool {
    const MARKS: [&str; 4] = [
        " | CHANGED |",
        " | SUCCESS |",
        " | FAILED |",
        " | UNREACHABLE |",
    ];
    MARKS.iter().any(|mark| line.contains(mark))
}

fn strip_transport_noise(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if is_transport_noise(line) {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn is_transport_noise(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("Warning: Permanently added")
        || line.starts_with("Shared connection to ")
        || line.starts_with("Authenticated to ")
        || line.starts_with("debug1:")
        || line.starts_with("OpenSSH_")
        || line.starts_with("Pseudo-terminal will not be allocated")
        || (line.starts_with("Connection to ") && line.ends_with("closed."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(cmd: &str) -> Vec<String> {
        cmd.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_inner_filters() {
        let cases = [
            (
                "docker exec -w /work box git -c safe.directory=/work log -n 120",
                Some("git-log"),
            ),
            (
                "docker compose -f compose.yml exec -T web git diff",
                Some("git-diff"),
            ),
            ("podman exec -it web find /bin", Some("find")),
            ("kubectl -n demo exec pod -- find /bin", Some("find")),
            (
                "oc --namespace demo exec pod -- grep -R foo src",
                Some("grep"),
            ),
            (
                "ssh -i /key orangepi5 git -C /repo log -n 20",
                Some("git-log"),
            ),
            (
                "ssh -o BatchMode=yes orangepi5 journalctl -n 80",
                Some("log"),
            ),
            (
                "nix --offline shell nixpkgs#git -c git status",
                Some("git-status"),
            ),
            ("nix run . -- git diff", Some("git-diff")),
        ];
        for (cmd, expected) in cases {
            assert_eq!(boundary_filter(&words(cmd)), expected, "{cmd}");
        }
        let ansible = ["ansible", "all", "-m", "raw", "-a", "find /bin -type f"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(boundary_filter(&ansible), Some("find"));
    }

    #[test]
    fn fails_closed_when_the_inner_command_is_not_parsed() {
        let cmds = [
            "docker exec box bash -lc git log",
            "docker exec --not-a-flag box git log",
            "kubectl exec pod find /bin",
            "ssh orangepi5",
            "ssh --mystery host git log",
            "nix build .",
            "nix shell nixpkgs#git",
            "nix develop",
            "ansible all -m ping",
        ];
        for cmd in cmds {
            assert_eq!(boundary_filter(&words(cmd)), None, "{cmd}");
        }
    }

    #[test]
    fn filters_find_stdout_and_drops_ssh_noise() {
        let argv = words("ssh orangepi5 find /bin");
        let raw = "\
Warning: Permanently added 'orangepi5' (ED25519) to the list of known hosts.
/bin/a
/bin/b
/bin/c
Shared connection to 100.118.28.115 closed.
";
        let out = apply(&argv, raw).unwrap();
        assert!(out.contains("3 files"), "{out}");
        assert!(!out.contains("Permanently added"), "{out}");
        assert!(!out.contains("Shared connection"), "{out}");
    }

    #[test]
    fn filters_each_ansible_host() {
        let argv = ["ansible", "all", "-m", "raw", "-a", "find /bin"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let raw = "\
localhost | CHANGED | rc=0 >>
/bin/a
/bin/b
orangepi5 | CHANGED | rc=0 >>
/usr/bin/a
Shared connection to 100.118.28.115 closed.
";
        let out = apply(&argv, raw).unwrap();
        assert!(out.contains("localhost | CHANGED |"), "{out}");
        assert!(out.contains("orangepi5 | CHANGED |"), "{out}");
        assert!(!out.contains("Shared connection"), "{out}");
        assert_eq!(out.matches("files in").count(), 2, "{out}");
    }

    #[test]
    fn rewrite_rules_reach_the_runner() {
        let cases = [
            "kubectl exec pod -- find /bin",
            "oc exec pod -- find /bin",
            "docker compose exec web git log",
            "podman exec web find /bin",
            "nix shell nixpkgs#git -c git log",
            "nix --offline develop -c git status",
            "nix run . -- git log",
            "ansible all -m raw -a \"find /bin\"",
        ];
        for cmd in cases {
            let rewritten =
                crate::discover::registry::rewrite_command(cmd, &[], &[]).unwrap_or_default();
            assert!(rewritten.starts_with("rtk "), "{cmd} -> {rewritten}");
        }
        assert_eq!(
            crate::discover::registry::rewrite_command("nix build .", &[], &[]),
            None
        );
    }
}
