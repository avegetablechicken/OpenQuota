//! The shared login on macOS, kept through `/usr/bin/security` exactly as `agy` keeps it with
//! go-keyring.
//!
//! Each write to a Keychain item replaces its partition list with the writer's. `agy` writes
//! through `/usr/bin/security` (partition `apple-tool:`), so an in-process write by OpenQuota
//! (partition `teamid:…`) would lock `agy` out until someone answered a Keychain prompt, and
//! `agy`'s next write would lock OpenQuota out in turn. Going through the same tool keeps the
//! partition list `apple-tool:` for everyone. The tool cannot be told to fail rather than prompt,
//! so every invocation has a deadline.

use std::{io, time::Duration};

use zeroize::Zeroizing;

/// How long one `security` invocation may wait, typically on a Keychain prompt.
const SECURITY_TIMEOUT: Duration = Duration::from_secs(60);
const SERVICE: &str = "gemini";
const ACCOUNT: &str = "antigravity";
/// `security`'s exit status for errSecItemNotFound.
const ITEM_NOT_FOUND_EXIT: i32 = 44;
/// The longest command go-keyring sends to `security -i`, whose line buffer is this size.
const INTERACTIVE_LINE_LIMIT: usize = 4096;
const GO_KEYRING_PREFIX: &str = "go-keyring-base64:";

pub struct CommandOutput {
    /// The exit status; `None` once the tool was ended by a signal.
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
}

/// Runs `/usr/bin/security`; tests substitute a fake.
pub trait SecurityRunner: Send + Sync {
    /// Runs the tool with `args`, writing `input` to its standard input when given, and gives up
    /// (ending the tool) after `timeout`.
    fn run(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<CommandOutput>;
}

#[cfg(target_os = "macos")]
pub struct SystemSecurity;

#[cfg(target_os = "macos")]
impl SecurityRunner for SystemSecurity {
    fn run(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<CommandOutput> {
        let output = crate::child_process::output_with_input_and_timeout(
            crate::child_process::background_command("/usr/bin/security").args(args),
            input,
            timeout,
        )?;
        Ok(CommandOutput {
            status: output.status.code(),
            stdout: output.stdout,
        })
    }
}

/// The shared login's bytes; `None` when there is no such item.
pub fn read(runner: &dyn SecurityRunner) -> Result<Option<Vec<u8>>, String> {
    let output = runner
        .run(
            &["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"],
            None,
            SECURITY_TIMEOUT,
        )
        .map_err(|_| "The macOS Keychain could not be read in time.".to_owned())?;
    match output.status {
        Some(0) => Ok(Some(password_from_output(output.stdout))),
        Some(ITEM_NOT_FOUND_EXIT) => Ok(None),
        _ => Err("The macOS Keychain could not be read.".into()),
    }
}

/// Replaces the shared login with `value`, then rereads it to confirm the change: `security -i`
/// does not document whether a failed command changes its exit status.
pub fn write(runner: &dyn SecurityRunner, value: &[u8]) -> Result<(), String> {
    let command = add_command(value)?;
    let output = runner
        .run(&["-i"], Some(command.as_bytes()), SECURITY_TIMEOUT)
        .map_err(|_| "The macOS Keychain could not be updated in time.".to_owned())?;
    if output.status != Some(0) {
        return Err("The macOS Keychain could not be updated.".into());
    }
    match read(runner)? {
        Some(saved) if saved == value => Ok(()),
        _ => Err("The macOS Keychain item was not updated.".into()),
    }
}

/// The interactive command saving `value`, as go-keyring words it. A go-keyring wrapper is
/// passed as is: its characters need no quoting. Anything else is passed as hexadecimal with
/// `-X`, which stores the same bytes without depending on `security`'s quoting rules.
fn add_command(value: &[u8]) -> Result<Zeroizing<String>, String> {
    let wrapped = std::str::from_utf8(value).ok().filter(|text| {
        text.strip_prefix(GO_KEYRING_PREFIX).is_some_and(|encoded| {
            !encoded.is_empty()
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
        })
    });
    let password = Zeroizing::new(match wrapped {
        Some(text) => format!("-w {text}"),
        None => format!("-X {}", hex(value).as_str()),
    });
    let command = Zeroizing::new(format!(
        "add-generic-password -U -s {SERVICE} -a {ACCOUNT} {}\n",
        password.as_str()
    ));
    if command.len() > INTERACTIVE_LINE_LIMIT {
        return Err("The credential is too large for the macOS Keychain tool.".into());
    }
    Ok(command)
}

/// `security -w` prints a password followed by a newline, in hexadecimal when it holds a byte
/// that is not printable ASCII. Hexadecimal output is decoded only when the bytes it stands for
/// could not have been printed as they are. A printable password made only of hexadecimal digits
/// would be misread, but no login go-keyring stores (a wrapper, JSON, or an OAuth token) is one.
fn password_from_output(mut stdout: Vec<u8>) -> Vec<u8> {
    if stdout.last() == Some(&b'\n') {
        stdout.pop();
    }
    match unhex(&stdout) {
        Some(bytes) if bytes.iter().any(|byte| !(0x20..=0x7e).contains(byte)) => bytes,
        _ => stdout,
    }
}

fn hex(bytes: &[u8]) -> Zeroizing<String> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for &byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

fn unhex(text: &[u8]) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    text.chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{io, sync::Mutex, time::Duration};

    use super::{hex, read, unhex, write, CommandOutput, SecurityRunner, SECURITY_TIMEOUT};

    /// One invocation: its arguments, its standard input and its deadline.
    type Call = (Vec<String>, Option<String>, Duration);

    /// A stand-in for `/usr/bin/security` over one in-memory item. It records every call and
    /// answers as the tool does: `-w` output in hexadecimal for unprintable bytes, exit 44 when
    /// there is no item.
    #[derive(Default)]
    struct FakeSecurity {
        item: Mutex<Option<Vec<u8>>>,
        calls: Mutex<Vec<Call>>,
        timed_out: bool,
        ignore_writes: bool,
    }

    impl FakeSecurity {
        fn holding(item: &[u8]) -> Self {
            Self {
                item: Mutex::new(Some(item.to_vec())),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SecurityRunner for FakeSecurity {
        fn run(
            &self,
            args: &[&str],
            input: Option<&[u8]>,
            timeout: Duration,
        ) -> io::Result<CommandOutput> {
            let input = input.map(|input| String::from_utf8(input.to_vec()).unwrap());
            self.calls.lock().unwrap().push((
                args.iter().map(|arg| (*arg).to_owned()).collect(),
                input.clone(),
                timeout,
            ));
            if self.timed_out {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
            }
            let mut item = self.item.lock().unwrap();
            if args == ["-i"] {
                let command = input.unwrap();
                let line = command
                    .strip_prefix("add-generic-password -U -s gemini -a antigravity ")
                    .and_then(|line| line.strip_suffix('\n'))
                    .unwrap();
                let value = match line.split_once(' ').unwrap() {
                    ("-w", value) => value.as_bytes().to_vec(),
                    ("-X", value) => unhex(value.as_bytes()).unwrap(),
                    other => panic!("unexpected password option {other:?}"),
                };
                if !self.ignore_writes {
                    *item = Some(value);
                }
                return Ok(CommandOutput {
                    status: Some(0),
                    stdout: Vec::new(),
                });
            }
            assert_eq!(
                args,
                [
                    "find-generic-password",
                    "-s",
                    "gemini",
                    "-a",
                    "antigravity",
                    "-w"
                ]
            );
            Ok(match item.as_ref() {
                Some(value) => {
                    let mut stdout = if value.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
                        value.clone()
                    } else {
                        value
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                            .into_bytes()
                    };
                    stdout.push(b'\n');
                    CommandOutput {
                        status: Some(0),
                        stdout,
                    }
                }
                None => CommandOutput {
                    status: Some(44),
                    stdout: Vec::new(),
                },
            })
        }
    }

    fn read_call() -> Vec<String> {
        [
            "find-generic-password",
            "-s",
            "gemini",
            "-a",
            "antigravity",
            "-w",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[test]
    fn reads_the_item_by_name_and_maps_a_missing_item_to_none() {
        let security = FakeSecurity::holding(b"go-keyring-base64:e30=");
        assert_eq!(
            read(&security).unwrap().as_deref(),
            Some(b"go-keyring-base64:e30=".as_slice())
        );
        assert_eq!(security.calls(), [(read_call(), None, SECURITY_TIMEOUT)]);

        assert!(read(&FakeSecurity::default()).unwrap().is_none());
    }

    #[test]
    fn hexadecimal_output_is_decoded_only_for_unprintable_passwords() {
        let unprintable = "{\"email\":\"usér@example.com\"}\n".as_bytes();
        assert_eq!(
            read(&FakeSecurity::holding(unprintable))
                .unwrap()
                .as_deref(),
            Some(unprintable)
        );
        // Digits standing for printable bytes cannot be `security`'s hexadecimal form.
        assert_eq!(
            read(&FakeSecurity::holding(b"4142")).unwrap().as_deref(),
            Some(b"4142".as_slice())
        );
    }

    #[test]
    fn writes_send_the_secret_only_on_stdin_and_reread_the_item() {
        let security = FakeSecurity::holding(b"go-keyring-base64:b2xk");
        write(&security, b"go-keyring-base64:bmV3").unwrap();

        let calls = security.calls();
        assert_eq!(
            calls,
            [
                (
                    vec!["-i".to_owned()],
                    Some(
                        "add-generic-password -U -s gemini -a antigravity -w go-keyring-base64:bmV3\n"
                            .to_owned()
                    ),
                    SECURITY_TIMEOUT
                ),
                (read_call(), None, SECURITY_TIMEOUT),
            ]
        );
        assert!(calls
            .iter()
            .flat_map(|(args, _, _)| args)
            .all(|arg| !arg.contains("bmV3")));
    }

    #[test]
    fn unwrapped_items_are_written_as_hexadecimal_and_round_trip() {
        for value in [
            r#"{"access_token":"new token","refresh_token":"it's \"quoted\""}"#.as_bytes(),
            "{\"email\":\"usér@example.com\"}".as_bytes(),
            b"go-keyring-base64:not base64!".as_slice(),
        ] {
            let security = FakeSecurity::holding(b"old");
            write(&security, value).unwrap();

            let (_, input, _) = &security.calls()[0];
            let input = input.as_deref().unwrap();
            let encoded = input
                .strip_prefix("add-generic-password -U -s gemini -a antigravity -X ")
                .and_then(|line| line.strip_suffix('\n'))
                .unwrap();
            assert!(encoded.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(read(&security).unwrap().as_deref(), Some(value));
        }
    }

    #[test]
    fn timeouts_failures_and_unapplied_writes_are_errors() {
        let stuck = FakeSecurity {
            timed_out: true,
            ..FakeSecurity::holding(b"old")
        };
        assert!(read(&stuck).is_err());
        assert!(write(&stuck, b"go-keyring-base64:bmV3").is_err());

        let ignored = FakeSecurity {
            ignore_writes: true,
            ..FakeSecurity::holding(b"go-keyring-base64:b2xk")
        };
        assert!(write(&ignored, b"go-keyring-base64:bmV3").is_err());

        let oversized = format!("go-keyring-base64:{}", "A".repeat(4_096));
        let security = FakeSecurity::holding(b"old");
        assert!(write(&security, oversized.as_bytes()).is_err());
        assert!(security.calls().is_empty());
    }

    #[test]
    fn hex_encodes_bytes_into_exact_capacity_without_reallocation() {
        assert_eq!(hex(b"").as_str(), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]).as_str(), "000fa5ff");

        let input: Vec<u8> = (0u8..=255).collect();
        let encoded = hex(&input);
        assert_eq!(encoded.len(), input.len() * 2);
        assert_eq!(encoded.capacity(), input.len() * 2);
        assert_eq!(unhex(encoded.as_bytes()).as_deref(), Some(input.as_slice()));
    }
}
