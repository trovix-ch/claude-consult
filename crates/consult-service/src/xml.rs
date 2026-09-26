//! The Task Scheduler XML this crate registers, and the parts of a registered
//! task's XML it reads back.

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::{DEFAULT_HOST, ServiceError, ServiceSpec};

/// How the task logs on, which decides whether it can start at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogonKind {
    /// Runs whether or not the user is logged on, without a stored password,
    /// and keeps the user's identity so `~` resolves to the real profile.
    /// Registering it usually needs an elevated shell.
    S4U,
    /// Runs only inside the user's interactive session; the unelevated
    /// fallback, which can therefore only start at logon.
    InteractiveToken,
}

/// Quotes one argument so `CommandLineToArgvW` (and the Rust runtime, which
/// follows the same rules) reads it back unchanged: backslashes are literal
/// except before a quote, where they are doubled, and the quote is escaped.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Before the closing quote every backslash is doubled, or the last one
    // would escape it.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The command line `install_elevated` gives the elevated child:
/// `service install --no-fallback --no-elevate --port N --install-dir D`, plus
/// `--run-as A` when `spec.account` is set (`install_elevated` always sets it).
///
/// `--no-fallback` because an elevated child that quietly registered the
/// logon-only task would report success for exactly the case it exists to
/// fix; `--no-elevate` so it can never ask again; `--run-as` because the
/// child's own environment names whoever answered the administrator prompt.
pub fn elevated_arguments(spec: &ServiceSpec) -> String {
    let dir = spec.working_dir.to_string_lossy();
    let mut args = vec![
        "service".to_owned(),
        "install".to_owned(),
        "--no-fallback".to_owned(),
        "--no-elevate".to_owned(),
        "--port".to_owned(),
        spec.port.to_string(),
        "--install-dir".to_owned(),
        quote_arg(&dir),
    ];
    if let Some(account) = spec.account.as_deref() {
        args.push("--run-as".to_owned());
        args.push(quote_arg(account));
    }
    args.join(" ")
}

impl LogonKind {
    /// The `<LogonType>` value in the task XML.
    pub fn as_xml(self) -> &'static str {
        match self {
            LogonKind::S4U => "S4U",
            LogonKind::InteractiveToken => "InteractiveToken",
        }
    }
}

/// The action's arguments: `serve --http --port N --detached`, with `--host H`
/// only when the host is not the default loopback address.
///
/// `--detached` for both principals: an interactive task would otherwise show
/// the server's console window for as long as it runs, and in the S4U task's
/// session 0 there is no console to lose, so it costs nothing there.
pub fn task_arguments(spec: &ServiceSpec) -> String {
    let host = spec.host.trim();
    if host.is_empty() || host == DEFAULT_HOST {
        format!("serve --http --port {} --detached", spec.port)
    } else {
        format!("serve --http --host {host} --port {} --detached", spec.port)
    }
}

/// The task's description as shown in Task Scheduler.
pub fn task_description(spec: &ServiceSpec) -> String {
    let host = spec.host.trim();
    let shown = if host.is_empty() || host == DEFAULT_HOST {
        "localhost"
    } else {
        host
    };
    format!("Shared OpenRouter MCP server ({shown}:{})", spec.port)
}

/// Escapes text for use in XML element content and attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// The complete task definition for `schtasks /Create /XML`.
///
/// `account` is `DOMAIN\user`. With [`LogonKind::S4U`] the task has a boot
/// trigger and a logon trigger; with [`LogonKind::InteractiveToken`] only the
/// logon trigger, since an interactive task cannot run before anyone logs on.
pub fn task_xml(spec: &ServiceSpec, account: &str, logon: LogonKind) -> String {
    let account = escape(account);
    let mut triggers = String::new();
    // The boot trigger has the service up before anyone logs on (matters on a
    // box reached over RDP, where the first session wants it immediately). The
    // logon trigger is a safety net if the boot run failed; IgnoreNew makes the
    // second trigger a no-op when the first already started it.
    if logon == LogonKind::S4U {
        triggers.push_str("    <BootTrigger>\n      <Enabled>true</Enabled>\n    </BootTrigger>\n");
    }
    let _ = write!(
        triggers,
        "    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{account}</UserId>\n    </LogonTrigger>\n"
    );

    // ExecutionTimeLimit PT0S = never kill a long-running service. Restart on
    // failure, because one shared process means a crash takes the tool away
    // from every session at once. That is the cost of sharing.
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
{triggers}  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{account}</UserId>
      <LogonType>{logon}</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        description = escape(&task_description(spec)),
        logon = logon.as_xml(),
        command = escape(&spec.exe.to_string_lossy()),
        arguments = escape(&task_arguments(spec)),
        working_dir = escape(&spec.working_dir.to_string_lossy()),
    )
}

/// What this crate reads back from a registered task's XML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegisteredTask {
    /// The first `Exec` action's program.
    pub command: Option<String>,
    /// The first `Exec` action's arguments.
    pub arguments: Option<String>,
    /// The first `Exec` action's working directory.
    pub working_dir: Option<PathBuf>,
    /// The principal's `UserId`: an account name, or a SID when Task Scheduler
    /// stored it that way.
    pub user_id: Option<String>,
    /// The principal's `LogonType` as written in the XML (`S4U`,
    /// `InteractiveToken`, `Password`, ...).
    pub logon_type: Option<String>,
    /// Trigger kinds in file order, named the way `Get-ScheduledTask` shows
    /// them: `Boot`, `Logon`, `Time`, ...
    pub triggers: Vec<String>,
    /// The first logon trigger's `UserId`, which is an account name even when
    /// the principal is stored as a SID.
    pub logon_trigger_user: Option<String>,
}

impl RegisteredTask {
    /// The `--port` the action passes, if any.
    pub fn port(&self) -> Option<u16> {
        self.arguments.as_deref().and_then(port_from_arguments)
    }

    /// Whether the principal logs on with S4U, and so starts at boot.
    pub fn is_s4u(&self) -> bool {
        self.logon_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(LogonKind::S4U.as_xml()))
    }

    /// Who the task runs as, preferring a readable account name over a SID.
    pub fn runs_as(&self) -> Option<String> {
        match self.user_id.as_deref() {
            Some(id) if !is_sid(id) => Some(id.to_owned()),
            sid => self
                .logon_trigger_user
                .clone()
                .or_else(|| sid.map(str::to_owned)),
        }
    }
}

fn is_sid(id: &str) -> bool {
    id.len() > 4 && id.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("S-1-"))
}

/// Recovers the port from action arguments (`--port N` or `--port=N`).
pub fn port_from_arguments(arguments: &str) -> Option<u16> {
    let mut words = arguments.split_whitespace();
    while let Some(word) = words.next() {
        if word == "--port" {
            return words.next().and_then(|n| n.trim_matches('"').parse().ok());
        }
        if let Some(n) = word.strip_prefix("--port=") {
            return n.trim_matches('"').parse().ok();
        }
    }
    None
}

/// Parses the output of `schtasks /Query /TN <name> /XML`.
pub fn parse_task_xml(text: &str) -> Result<RegisteredTask, ServiceError> {
    let text = text.trim_start_matches('\u{feff}').trim();
    let doc = roxmltree::Document::parse(text).map_err(|e| ServiceError::Parse(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "Task" {
        return Err(ServiceError::Parse(format!(
            "root element is <{}>, not <Task>",
            root.tag_name().name()
        )));
    }

    let child_text = |node: roxmltree::Node<'_, '_>, name: &str| -> Option<String> {
        node.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
            .and_then(|c| c.text())
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    };
    let first = |name: &str| {
        root.descendants()
            .find(|n| n.is_element() && n.tag_name().name() == name)
    };

    let mut task = RegisteredTask::default();
    if let Some(exec) = first("Exec") {
        task.command = child_text(exec, "Command");
        task.arguments = child_text(exec, "Arguments");
        task.working_dir =
            child_text(exec, "WorkingDirectory").map(|d| PathBuf::from(d.trim_matches('"')));
    }
    if let Some(principal) = first("Principal") {
        task.user_id = child_text(principal, "UserId");
        task.logon_type = child_text(principal, "LogonType");
    }
    if let Some(triggers) = first("Triggers") {
        for trigger in triggers.children().filter(|c| c.is_element()) {
            let name = trigger.tag_name().name();
            task.triggers
                .push(name.strip_suffix("Trigger").unwrap_or(name).to_owned());
            if name == "LogonTrigger" && task.logon_trigger_user.is_none() {
                task.logon_trigger_user = child_text(trigger, "UserId");
            }
        }
    }
    Ok(task)
}

/// Decodes schtasks output: UTF-16 when it carries a byte-order mark,
/// otherwise UTF-8, lossily.
///
/// schtasks writes in the console's OEM code page when redirected, so a
/// non-ASCII path can come back mangled; there is no way to ask it for UTF-8.
pub fn decode_output(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                if le {
                    u16::from_le_bytes(*p)
                } else {
                    u16::from_be_bytes(*p)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Encodes the task XML the way `schtasks /Create /XML` reads it most
/// reliably: UTF-16LE with a byte-order mark, matching the declaration.
pub fn encode_utf16le(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + text.len() * 2);
    out.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// The `Status` column of `schtasks /Query /TN <name> /FO CSV /NH`: the
/// third field of the first non-empty line.
///
/// Columns, unlike the verbose list's labels, do not depend on the display
/// language; the value itself (`Ready`, `Running`, ...) does.
pub fn parse_csv_state(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    fields.push(field);
    fields
        .get(2)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
