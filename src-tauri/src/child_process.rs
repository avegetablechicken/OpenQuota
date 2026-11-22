use std::{
    io::{self, Read, Write},
    process::{Command, Output, Stdio},
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
            Some(thread::spawn(move || {
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
    let stdout_reader = thread::spawn(move || read_all(stdout));
    let stderr_reader = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child timeout is too large"))?;

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
                let _ = join_writer(stdin_writer);
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "background command timed out",
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_writer(stdin_writer);
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Err(error);
            }
        }
    };

    let written = join_writer(stdin_writer);
    let output = Output {
        status,
        stdout: join_reader(stdout_reader)?,
        stderr: join_reader(stderr_reader)?,
    };
    match written {
        Ok(()) => Ok(output),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe && !output.status.success() => {
            Ok(output)
        }
        Err(error) => Err(error),
    }
}

fn join_writer(writer: Option<thread::JoinHandle<io::Result<()>>>) -> io::Result<()> {
    writer.map_or(Ok(()), |writer| {
        writer
            .join()
            .map_err(|_| io::Error::other("background input writer panicked"))?
    })
}

fn read_all(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(reader: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other("background output reader panicked"))?
}

#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_console_window(_command: &mut Command) {}

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
