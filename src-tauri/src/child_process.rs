use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

/// Creates a child process intended for background discovery work.
///
/// A GUI-subsystem application does not own a console on Windows. Starting a
/// console-subsystem executable from it without `CREATE_NO_WINDOW` briefly
/// creates a visible console window, so every background child process must go
/// through this helper.
pub fn background_command(program: &str) -> Command {
    let mut command = Command::new(program);
    hide_console_window(&mut command);
    command
}

/// Captures a background command while enforcing a real child-process deadline.
pub fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    output_with_input_and_timeout(command, None, timeout)
}

/// Like [`output_with_timeout`], feeding `input` (kept out of the command line, where other
/// processes could see it) on the child's standard input when given.
pub fn output_with_input_and_timeout(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdin_writer = match (input, child.stdin.take()) {
        (Some(input), Some(mut stdin)) => {
            let input = zeroize::Zeroizing::new(input.to_vec());
            Some(spawn_worker(move || {
                stdin.write_all(&input)?;
                stdin.flush()
            }))
        }
        _ => None,
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("child stderr was not captured"))?;
    let stdout_reader = spawn_worker(move || read_all(stdout));
    let stderr_reader = spawn_worker(move || read_all(stderr));
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child timeout is too large"))?;

    // The pipe workers are never joined unconditionally: a grandchild that
    // inherited stdin/stdout/stderr keeps them blocked after the child exits,
    // so they are awaited only until the deadline and otherwise left detached.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                );
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timed_out());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    };

    let written = stdin_writer.map_or(Ok(()), |writer| {
        receive_by(&writer, deadline, "background input writer panicked")?
    });
    let output = Output {
        status,
        stdout: receive_by(
            &stdout_reader,
            deadline,
            "background output reader panicked",
        )??,
        stderr: receive_by(
            &stderr_reader,
            deadline,
            "background output reader panicked",
        )??,
    };
    match written {
        Ok(()) => Ok(output),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe && !output.status.success() => {
            Ok(output)
        }
        Err(error) => Err(error),
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "background command timed out")
}

fn spawn_worker<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver
}

fn receive_by<T>(receiver: &Receiver<T>, deadline: Instant, panicked: &str) -> io::Result<T> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(value) => Ok(value),
        Err(RecvTimeoutError::Timeout) => Err(timed_out()),
        Err(RecvTimeoutError::Disconnected) => Err(io::Error::other(panicked.to_owned())),
    }
}

fn read_all(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Finds an installed command-line tool: on `PATH`, then in `preferred`, then in the usual install
/// directories, because GUI launches may not inherit the terminal's `PATH`. `executable` is the
/// file name, including `.exe` on Windows.
pub fn find_executable(executable: &str, preferred: &[PathBuf]) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    find_executable_in(
        executable,
        std::env::var_os("PATH").as_deref(),
        &home,
        preferred,
    )
}

fn find_executable_in(
    executable: &str,
    path: Option<&std::ffi::OsStr>,
    home: &Path,
    preferred: &[PathBuf],
) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    directories.extend(preferred.iter().cloned());
    directories.push(home.join(".local/bin"));
    #[cfg(unix)]
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/home/linuxbrew/.linuxbrew/bin"),
    ]);
    directories
        .into_iter()
        .map(|directory| directory.join(executable))
        .find(|path| path.is_file())
}

#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_console_window(_command: &mut Command) {}

#[cfg(test)]
mod lookup_tests {
    use std::fs;

    use super::find_executable_in;

    #[test]
    fn executables_are_found_on_path_first_then_in_install_directories() {
        let directory = tempfile::tempdir().unwrap();
        let on_path = directory.path().join("path");
        let home = directory.path().join("home");
        fs::create_dir_all(&on_path).unwrap();
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        fs::write(home.join(".local/bin/tool"), "").unwrap();

        assert_eq!(
            find_executable_in("tool", Some(on_path.as_os_str()), &home, &[]),
            Some(home.join(".local/bin/tool"))
        );
        fs::write(on_path.join("tool"), "").unwrap();
        assert_eq!(
            find_executable_in("tool", Some(on_path.as_os_str()), &home, &[]),
            Some(on_path.join("tool"))
        );
        assert_eq!(
            find_executable_in("missing-openquota-tool", None, &home, &[]),
            None
        );
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use std::time::{Duration, Instant};

    use super::{background_command, output_with_timeout};

    #[test]
    fn background_console_process_has_no_console_window() {
        let script = r#"Add-Type -Name NativeMethods -Namespace OpenQuota -MemberDefinition '[System.Runtime.InteropServices.DllImport("kernel32.dll")] public static extern System.IntPtr GetConsoleWindow();'; [OpenQuota.NativeMethods]::GetConsoleWindow().ToInt64()"#;
        let output = background_command("powershell")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .output()
            .expect("PowerShell should be available on supported Windows installations");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0");
    }

    #[test]
    fn background_command_deadline_terminates_a_slow_process() {
        let started = Instant::now();
        let error = output_with_timeout(
            background_command("powershell").args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "Start-Sleep -Seconds 5",
            ]),
            Duration::from_millis(100),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use std::time::{Duration, Instant};

    use super::{background_command, output_with_input_and_timeout, output_with_timeout};

    #[test]
    fn background_command_deadline_terminates_a_slow_process() {
        let started = Instant::now();
        let error = output_with_timeout(
            background_command("sleep").arg("5"),
            Duration::from_millis(100),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn deadline_holds_when_a_grandchild_keeps_the_output_pipes_open() {
        for script in ["sleep 5 & sleep 5", "sleep 5 & echo done"] {
            let started = Instant::now();
            let error = output_with_timeout(
                background_command("sh").args(["-c", script]),
                Duration::from_millis(300),
            )
            .unwrap_err();

            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{script}");
            assert!(started.elapsed() < Duration::from_secs(2), "{script}");
        }
    }

    #[test]
    fn background_command_input_arrives_on_stdin_and_the_deadline_still_holds() {
        let output = output_with_input_and_timeout(
            &mut background_command("cat"),
            Some(b"secret line\n"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"secret line\n");

        let started = Instant::now();
        let error = output_with_input_and_timeout(
            background_command("sleep").arg("5"),
            Some(b"ignored\n"),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn early_failing_child_preserves_exit_status_and_stderr_on_broken_pipe() {
        let large_input = vec![b'x'; 256 * 1024];
        let output = output_with_input_and_timeout(
            background_command("sh").args(["-c", "printf 'bad input\\n' >&2; exit 23"]),
            Some(&large_input),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stderr, b"bad input\n");
        assert!(output.stdout.is_empty());

        let error = output_with_input_and_timeout(
            background_command("sh").args(["-c", "exit 0"]),
            Some(&large_input),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
}
