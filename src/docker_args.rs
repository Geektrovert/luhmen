//! Protect the wrapper's Docker target while retaining container command arguments.

use anyhow::{Result, bail, ensure};

const DIRECT_CLI: &str = "use `docker --context luhmen ...` directly for unsupported option syntax";

#[derive(Clone, Copy)]
enum CommandKind {
    Exec,
    Create,
}

#[derive(Clone, Copy)]
enum Arity {
    Boolean,
    Value,
}

/// Validate arguments passed after `luhmen docker` without modifying them.
/// Long target-selection flags are rejected even inside a command payload.
/// For exec/run/create, short flags stop at the container/image operand, as they
/// do in Docker's non-interspersed flag parser.
pub fn validate(args: &[String]) -> Result<()> {
    let Some((command, remaining)) = args.split_first() else {
        bail!("provide a Docker workload command");
    };
    ensure!(
        !command.is_empty() && !command.starts_with('-') && command != "context",
        "provide a Docker workload command; manage Docker contexts directly with the Docker CLI"
    );
    for arg in args {
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        ensure!(
            !matches!(name, "--context" | "--host" | "--config"),
            "Docker endpoint or configuration overrides are not allowed in the luhmen wrapper"
        );
    }
    let (command, options) = if command == "container" {
        let Some((subcommand, options)) = remaining.split_first() else {
            return Ok(());
        };
        ensure!(
            !subcommand.starts_with('-'),
            "place the container subcommand before its options; {DIRECT_CLI}"
        );
        (subcommand.as_str(), options)
    } else {
        (command.as_str(), remaining)
    };
    match command {
        "exec" => validate_container_options(options, CommandKind::Exec),
        "run" | "create" => validate_container_options(options, CommandKind::Create),
        _ => {
            // Other commands and CLI plugins have different option grammars. Keep
            // ambiguous short target flags conservative instead of guessing arities.
            for arg in options {
                if let Some(short) = arg
                    .strip_prefix('-')
                    .filter(|value| !value.starts_with('-'))
                {
                    ensure!(
                        !short.contains(['c', 'H']),
                        "ambiguous Docker short target option; {DIRECT_CLI}"
                    );
                }
            }
            Ok(())
        }
    }
}

fn validate_container_options(args: &[String], command: CommandKind) -> Result<()> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" || arg == "-" || !arg.starts_with('-') {
            // Docker exec/run/create call SetInterspersed(false). Everything after
            // the container/image belongs to the command, including `sh -c`.
            return Ok(());
        }
        let consumed = if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, false), |(name, _)| (name, true));
            let Some(arity) = long_option(name, command) else {
                bail!("unsupported Docker option --{name}; {DIRECT_CLI}");
            };
            match arity {
                Arity::Boolean => 1,
                Arity::Value if inline => 1,
                Arity::Value => require_value(args, index, name)?,
            }
        } else {
            short_options(args, index, command)?
        };
        index += consumed;
    }
    // Let Docker produce its own missing-operand and help output.
    Ok(())
}

fn require_value(args: &[String], index: usize, option: &str) -> Result<usize> {
    ensure!(
        args.get(index + 1).is_some(),
        "Docker option {option} requires a value; {DIRECT_CLI}"
    );
    Ok(2)
}

fn short_options(args: &[String], index: usize, command: CommandKind) -> Result<usize> {
    let short = &args[index][1..];
    for (offset, name) in short.char_indices() {
        ensure!(
            name != 'H',
            "Docker host overrides are not allowed in the luhmen wrapper"
        );
        ensure!(
            name != 'c' || matches!(command, CommandKind::Create),
            "Docker context overrides are not allowed in the luhmen wrapper"
        );
        let arity = match (command, name) {
            (_, 'd' | 'i' | 't' | 'D') => Arity::Boolean,
            (CommandKind::Exec, 'h' | 'v') => Arity::Boolean,
            (CommandKind::Exec, 'e' | 'u' | 'w' | 'l') => Arity::Value,
            (CommandKind::Create, 'P' | 'q') => Arity::Boolean,
            (CommandKind::Create, 'a' | 'c' | 'e' | 'h' | 'l' | 'm' | 'p' | 'u' | 'v' | 'w') => {
                Arity::Value
            }
            _ => bail!("unsupported Docker short option -{name}; {DIRECT_CLI}"),
        };
        let rest = &short[offset + name.len_utf8()..];
        match arity {
            Arity::Value if rest.is_empty() => {
                return require_value(args, index, &name.to_string());
            }
            Arity::Value => return Ok(1),
            // pflag accepts `-it=false`: i is true and t consumes its inline value.
            Arity::Boolean if rest.starts_with('=') => return Ok(1),
            Arity::Boolean => {}
        }
    }
    Ok(1)
}

fn long_option(name: &str, command: CommandKind) -> Option<Arity> {
    use Arity::{Boolean, Value};
    match name {
        "debug" | "help" | "version" => return Some(Boolean),
        "log-level" => return Some(Value),
        "detach" | "interactive" | "tty" | "privileged" => return Some(Boolean),
        "detach-keys" | "env" | "env-file" | "user" | "workdir" => return Some(Value),
        _ => {}
    }
    if matches!(command, CommandKind::Exec) {
        return None;
    }
    // Docker CLI v29.8.0 container/{run,create,opts}.go. Keep value arities explicit:
    // mistaking an option value for the image could hide a later target override.
    match name {
        "disable-content-trust"
        | "init"
        | "no-healthcheck"
        | "oom-kill-disable"
        | "publish-all"
        | "quiet"
        | "read-only"
        | "rm"
        | "sig-proxy"
        | "use-api-socket" => Some(Boolean),
        "add-host"
        | "annotation"
        | "attach"
        | "blkio-weight"
        | "blkio-weight-device"
        | "cap-add"
        | "cap-drop"
        | "cgroup-parent"
        | "cgroupns"
        | "cidfile"
        | "cpu-count"
        | "cpu-percent"
        | "cpu-period"
        | "cpu-quota"
        | "cpu-rt-period"
        | "cpu-rt-runtime"
        | "cpu-shares"
        | "cpus"
        | "cpuset-cpus"
        | "cpuset-mems"
        | "device"
        | "device-cgroup-rule"
        | "device-read-bps"
        | "device-read-iops"
        | "device-write-bps"
        | "device-write-iops"
        | "dns"
        | "dns-option"
        | "dns-search"
        | "domainname"
        | "entrypoint"
        | "expose"
        | "gpus"
        | "group-add"
        | "health-cmd"
        | "health-interval"
        | "health-retries"
        | "health-start-interval"
        | "health-start-period"
        | "health-timeout"
        | "hostname"
        | "io-maxbandwidth"
        | "io-maxiops"
        | "ip"
        | "ip6"
        | "ipc"
        | "isolation"
        | "label"
        | "label-file"
        | "link"
        | "link-local-ip"
        | "log-driver"
        | "log-opt"
        | "mac-address"
        | "memory"
        | "memory-reservation"
        | "memory-swap"
        | "memory-swappiness"
        | "mount"
        | "name"
        | "network"
        | "network-alias"
        | "oom-score-adj"
        | "pid"
        | "pids-limit"
        | "platform"
        | "publish"
        | "pull"
        | "restart"
        | "runtime"
        | "security-opt"
        | "shm-size"
        | "stop-signal"
        | "stop-timeout"
        | "storage-opt"
        | "sysctl"
        | "tmpfs"
        | "ulimit"
        | "userns"
        | "uts"
        | "volume"
        | "volume-driver"
        | "volumes-from" => Some(Value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(args: &[&str]) -> Result<()> {
        validate(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn target_overrides_and_context_management_are_rejected() {
        for args in [
            vec!["--context", "other", "ps"],
            vec!["ps", "--context=other"],
            vec!["ps", "--host", "unix:///other.sock"],
            vec!["ps", "--config=/other"],
            vec!["context", "use", "other"],
            vec!["container", "--context", "other", "exec", "app", "true"],
            vec!["exec", "-cother", "app", "true"],
            vec!["exec", "-itcother", "app", "true"],
            vec!["exec", "-DHunix:///other.sock", "app", "true"],
            vec![
                "run",
                "--name",
                "fixture",
                "-H",
                "unix:///other.sock",
                "image",
            ],
            vec!["container", "run", "--config", "/other", "image"],
            vec!["ps", "-Dcother"],
            vec!["ps", "-DHunix:///other.sock"],
            vec!["exec", "app", "program", "--context=other"],
        ] {
            assert!(check(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn exec_preserves_shell_commands_after_the_container() {
        for args in [
            vec!["exec", "app", "sh", "-c", "echo hello"],
            vec!["container", "exec", "app", "sh", "-c", "echo hello"],
            vec![
                "exec",
                "-it",
                "-u",
                "1000:1000",
                "-e",
                "KEY=value",
                "app",
                "sh",
                "-c",
                "echo hello",
            ],
            vec![
                "exec",
                "-itu1000",
                "-eKEY=value",
                "-w/tmp",
                "app",
                "sh",
                "-c",
                "echo hello",
            ],
            vec![
                "exec",
                "-it=false",
                "-u=1000",
                "-e=KEY=value",
                "app",
                "sh",
                "-c",
                "echo hello",
            ],
            vec![
                "exec",
                "--env-file",
                "fixture.env",
                "--user=1000",
                "--workdir",
                "/tmp",
                "app",
                "curl",
                "-H",
                "X-Fixture: yes",
            ],
            vec![
                "exec",
                "--detach-keys=ctrl-c",
                "--privileged=false",
                "--",
                "app",
                "sh",
                "-c",
                "echo hello",
            ],
            vec!["exec", "-e", "-c", "app", "true"],
        ] {
            assert!(check(&args).is_ok(), "rejected {args:?}");
        }
    }

    #[test]
    fn exec_option_values_cannot_hide_later_target_flags() {
        for args in [
            vec!["exec", "-u", "1000", "-cother", "app", "true"],
            vec!["exec", "-itu", "1000", "-cother", "app", "true"],
            vec!["exec", "-e", "KEY=value", "-Hother", "app", "true"],
            vec![
                "exec",
                "--env-file",
                "fixture.env",
                "-Dcother",
                "app",
                "true",
            ],
            vec!["exec", "--future-option", "value", "app", "true"],
            vec!["exec", "-x", "value", "app", "true"],
            vec!["exec", "--user"],
            vec!["exec", "-e"],
        ] {
            assert!(check(&args).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn run_and_create_keep_cpu_shares_and_command_payloads() {
        for args in [
            vec!["run", "-c", "128", "image", "sh", "-c", "echo hello"],
            vec![
                "container",
                "run",
                "-c128",
                "image",
                "sh",
                "-c",
                "echo hello",
            ],
            vec![
                "container",
                "create",
                "-c=128",
                "image",
                "sh",
                "-c",
                "echo hello",
            ],
            vec![
                "run",
                "--rm",
                "-dit",
                "--name",
                "fixture",
                "-p",
                "8080:80",
                "-eKEY=value",
                "image",
                "curl",
                "-H",
                "X-Fixture: yes",
            ],
            vec![
                "run",
                "--mount",
                "type=bind,src=/tmp,dst=/src",
                "--cpus=2",
                "image",
            ],
            vec!["run", "--", "image", "sh", "-c", "echo hello"],
        ] {
            assert!(check(&args).is_ok(), "rejected {args:?}");
        }
    }
}
