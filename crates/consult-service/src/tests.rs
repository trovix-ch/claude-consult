use std::net::TcpListener;
use std::path::PathBuf;

use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/query_s4u.xml");

fn spec() -> ServiceSpec {
    ServiceSpec {
        exe: PathBuf::from(r"C:\Users\A & B\AppData\Local\claude-consult\bin\claude-consult.exe"),
        working_dir: PathBuf::from(r"C:\Users\A & B\AppData\Local\claude-consult"),
        port: 8765,
        host: DEFAULT_HOST.to_owned(),
        account: None,
    }
}

fn element_names(doc: &roxmltree::Document<'_>, parent: &str) -> Vec<String> {
    doc.descendants()
        .find(|n| n.tag_name().name() == parent)
        .map(|p| {
            p.children()
                .filter(|c| c.is_element())
                .map(|c| c.tag_name().name().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn s4u_xml_is_well_formed_and_has_every_setting() {
    let text = task_xml(&spec(), r"EXAMPLE\someone", LogonKind::S4U);
    let doc = roxmltree::Document::parse(&text).expect("well-formed");
    assert_eq!(doc.root_element().tag_name().name(), "Task");
    assert_eq!(
        doc.root_element().tag_name().namespace(),
        Some("http://schemas.microsoft.com/windows/2004/02/mit/task")
    );
    assert_eq!(
        element_names(&doc, "Triggers"),
        ["BootTrigger", "LogonTrigger"]
    );
    for snippet in [
        "<Description>Shared OpenRouter MCP server (localhost:8765)</Description>",
        "<UserId>EXAMPLE\\someone</UserId>\n      <LogonType>S4U</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>",
        "<LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>EXAMPLE\\someone</UserId>",
        "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
        "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
        "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
        "<AllowStartOnDemand>true</AllowStartOnDemand>",
        "<StartWhenAvailable>true</StartWhenAvailable>",
        "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
        "<RestartOnFailure>\n      <Interval>PT1M</Interval>\n      <Count>3</Count>\n    </RestartOnFailure>",
        "<Arguments>serve --http --port 8765 --detached</Arguments>",
        r"<Command>C:\Users\A &amp; B\AppData\Local\claude-consult\bin\claude-consult.exe</Command>",
        r"<WorkingDirectory>C:\Users\A &amp; B\AppData\Local\claude-consult</WorkingDirectory>",
    ] {
        assert!(text.contains(snippet), "missing {snippet:?} in\n{text}");
    }
}

#[test]
fn interactive_xml_has_only_the_logon_trigger() {
    let text = task_xml(&spec(), r"EXAMPLE\someone", LogonKind::InteractiveToken);
    let doc = roxmltree::Document::parse(&text).expect("well-formed");
    assert_eq!(element_names(&doc, "Triggers"), ["LogonTrigger"]);
    assert!(!text.contains("BootTrigger"));
    assert!(text.contains("<LogonType>InteractiveToken</LogonType>"));
    assert!(text.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    assert!(text.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    // Detached for both principals: the interactive one would show a console.
    assert!(text.contains("<Arguments>serve --http --port 8765 --detached</Arguments>"));
}

#[test]
fn generated_xml_round_trips_through_the_parser() {
    for logon in [LogonKind::S4U, LogonKind::InteractiveToken] {
        let task = parse_task_xml(&task_xml(&spec(), r"EXAMPLE\someone", logon)).expect("parse");
        assert_eq!(task.port(), Some(8765));
        assert_eq!(task.working_dir, Some(spec().working_dir));
        assert_eq!(
            task.command.as_deref(),
            Some(&*spec().exe.to_string_lossy())
        );
        assert_eq!(task.logon_type.as_deref(), Some(logon.as_xml()));
        assert_eq!(task.runs_as().as_deref(), Some(r"EXAMPLE\someone"));
    }
}

#[test]
fn host_is_passed_only_when_not_loopback() {
    let mut s = spec();
    assert_eq!(task_arguments(&s), "serve --http --port 8765 --detached");
    s.host = "0.0.0.0".into();
    assert_eq!(
        task_arguments(&s),
        "serve --http --host 0.0.0.0 --port 8765 --detached"
    );
    assert_eq!(
        task_description(&s),
        "Shared OpenRouter MCP server (0.0.0.0:8765)"
    );
    assert_eq!(
        parse_task_xml(&task_xml(&s, "u", LogonKind::S4U))
            .expect("parse")
            .port(),
        Some(8765)
    );
}

#[test]
fn fixture_query_output_parses() {
    // schtasks emits CR CR LF line ends and declares UTF-16 while writing
    // single bytes; neither may trip the parser.
    let raw = FIXTURE.replace("\r\n", "\n").replace('\n', "\r\r\n");
    let task = parse_task_xml(&raw).expect("parse");
    assert_eq!(task.port(), Some(9123));
    assert_eq!(
        task.working_dir,
        Some(PathBuf::from(
            r"C:\Users\Someone\AppData\Local\claude-consult"
        ))
    );
    assert_eq!(task.logon_type.as_deref(), Some("S4U"));
    assert_eq!(task.triggers, ["Boot", "Logon"]);
    assert!(
        task.user_id
            .as_deref()
            .is_some_and(|u| u.starts_with("S-1-5-21-"))
    );
    // The principal is a SID, so the logon trigger's account name is shown.
    assert_eq!(task.runs_as().as_deref(), Some(r"EXAMPLE-PC\Someone"));
}

#[test]
fn legacy_python_task_parses() {
    let raw = FIXTURE
        .replace(
            "<Arguments>serve --http --port 9123</Arguments>",
            r#"<Arguments>"C:\x\server.py" --http --port 8765</Arguments>"#,
        )
        .replace("<BootTrigger />", "");
    let task = parse_task_xml(&raw).expect("parse");
    assert_eq!(task.port(), Some(8765));
    assert_eq!(task.triggers, ["Logon"]);
}

#[test]
fn garbage_is_a_parse_error() {
    assert!(matches!(
        parse_task_xml("ERROR: nope"),
        Err(ServiceError::Parse(_))
    ));
    assert!(matches!(
        parse_task_xml("<Other/>"),
        Err(ServiceError::Parse(_))
    ));
}

#[test]
fn port_parsing() {
    assert_eq!(port_from_arguments("serve --http --port 1234"), Some(1234));
    assert_eq!(port_from_arguments("serve --http --port=4321"), Some(4321));
    assert_eq!(port_from_arguments("serve --http"), None);
    assert_eq!(port_from_arguments("serve --http --port"), None);
    assert_eq!(port_from_arguments("serve --http --port 99999"), None);
    assert_eq!(
        port_from_arguments("serve --http --port 8765 --detached"),
        Some(8765)
    );
    assert_eq!(
        port_from_arguments("serve --http --detached --port=8766"),
        Some(8766)
    );
    assert_eq!(port_from_arguments(&task_arguments(&spec())), Some(8765));
}

#[test]
fn argument_quoting_round_trips_the_windows_rules() {
    assert_eq!(quote_arg("plain"), "plain");
    assert_eq!(quote_arg(r"C:\no\spaces"), r"C:\no\spaces");
    assert_eq!(quote_arg(""), r#""""#);
    assert_eq!(quote_arg(r"C:\with space"), r#""C:\with space""#);
    // A trailing backslash would escape the closing quote unless doubled.
    assert_eq!(quote_arg(r"C:\with space\"), r#""C:\with space\\""#);
    // A quote is escaped, and backslashes right before it doubled.
    assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
    assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    // Backslashes elsewhere stay single.
    assert_eq!(quote_arg(r"a\\b c"), r#""a\\b c""#);
}

#[test]
fn the_elevated_child_gets_no_fallback_and_no_second_prompt() {
    assert_eq!(
        elevated_arguments(&spec()),
        r#"service install --no-fallback --no-elevate --port 8765 --install-dir "C:\Users\A & B\AppData\Local\claude-consult""#
    );
    let mut s = spec();
    s.working_dir = PathBuf::from(r"C:\x\cc");
    s.port = 9001;
    assert_eq!(
        elevated_arguments(&s),
        r"service install --no-fallback --no-elevate --port 9001 --install-dir C:\x\cc"
    );
}

#[test]
fn the_elevated_child_is_told_whose_task_it_registers() {
    let mut s = spec();
    s.working_dir = PathBuf::from(r"C:\x\cc");
    s.account = Some(r"EXAMPLE\some one".into());
    assert_eq!(
        elevated_arguments(&s),
        r#"service install --no-fallback --no-elevate --port 8765 --install-dir C:\x\cc --run-as "EXAMPLE\some one""#
    );
}

#[test]
fn the_account_given_is_the_principal_and_the_logon_user() {
    // What the elevated child writes with --run-as: that account in both places,
    // whatever its own environment says.
    let mut s = spec();
    s.account = Some(r" EXAMPLE\caller ".into());
    let account = task_account(&s).expect("account");
    assert_eq!(account, r"EXAMPLE\caller");
    for logon in [LogonKind::S4U, LogonKind::InteractiveToken] {
        let task = parse_task_xml(&task_xml(&s, &account, logon)).expect("parse");
        assert_eq!(task.user_id.as_deref(), Some(r"EXAMPLE\caller"));
        assert_eq!(task.logon_trigger_user.as_deref(), Some(r"EXAMPLE\caller"));
    }
}

#[test]
fn is_elevated_answers() {
    // Only that it answers: whether this runner is elevated is not known here.
    let _: bool = is_elevated();
}

#[test]
fn elevated_success_line() {
    assert_eq!(
        elevated_success(r"HOST\me"),
        r"Registered 'OpenRouterMCP' as HOST\me (S4U) - starts at boot and at logon"
    );
}

#[test]
fn output_decoding() {
    assert_eq!(decode_output(b"abc"), "abc");
    assert_eq!(decode_output(&encode_utf16le("h\u{e9}")), "h\u{e9}");
    assert_eq!(decode_output(b"\xEF\xBB\xBFx"), "x");
    assert_eq!(decode_output(&[0xFE, 0xFF, 0x00, 0x41]), "A");
}

#[test]
fn csv_state() {
    assert_eq!(
        parse_csv_state("\r\n\"\\OpenRouterMCP\",\"N/A\",\"Running\"\r\n").as_deref(),
        Some("Running")
    );
    assert_eq!(
        parse_csv_state("\"a,b\",\"x\",\"Ready\"").as_deref(),
        Some("Ready")
    );
    assert_eq!(parse_csv_state(""), None);
}

#[test]
fn serve_process_matching() {
    let dirs = vec![PathBuf::from(r"C:\Users\Me\AppData\Local\claude-consult")];
    let ours = r#""C:\Users\Me\AppData\Local\claude-consult\bin\claude-consult.exe" serve --http --port 8765"#;
    assert!(is_serve_command_line(ours, &dirs));
    // As the task runs it now, detached.
    assert!(is_serve_command_line(
        r#""C:\Users\Me\AppData\Local\claude-consult\bin\claude-consult.exe" serve --http --port 8765 --detached"#,
        &dirs
    ));
    // Case and slash direction do not matter.
    assert!(is_serve_command_line(
        "c:/users/me/appdata/local/CLAUDE-CONSULT/bin/claude-consult.exe serve --http",
        &dirs
    ));
    // The legacy Python service is stopped too.
    assert!(is_serve_command_line(
        r#"C:\Users\Me\AppData\Local\claude-consult\.venv\Scripts\pythonw.exe "C:\Users\Me\AppData\Local\claude-consult\server.py" --http --port 8765"#,
        &dirs
    ));
    // stdio servers, other subcommands, other installs: never.
    assert!(!is_serve_command_line(
        r"C:\Users\Me\AppData\Local\claude-consult\bin\claude-consult.exe serve",
        &dirs
    ));
    assert!(!is_serve_command_line(
        r"C:\Users\Me\AppData\Local\claude-consult\bin\claude-consult.exe hook summary --http",
        &dirs
    ));
    assert!(!is_serve_command_line(
        r"C:\Users\Me\AppData\Local\claude-consult-old\bin\claude-consult.exe serve --http",
        &dirs
    ));
    assert!(!is_serve_command_line(
        r"D:\elsewhere\bin\claude-consult.exe serve --http",
        &dirs
    ));
    // A trailing separator or a verbatim prefix on the dir still matches.
    assert!(is_serve_command_line(
        ours,
        &[PathBuf::from(r"C:\Users\Me\AppData\Local\claude-consult\")]
    ));
    assert!(is_serve_command_line(
        ours,
        &[PathBuf::from(
            r"\\?\C:\Users\Me\AppData\Local\claude-consult"
        )]
    ));
    // No dirs, or only empty ones, match nothing.
    assert!(!is_serve_command_line(ours, &[]));
    assert!(!is_serve_command_line(ours, &[PathBuf::new()]));
}

#[test]
fn listening_check() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    assert!(is_listening(port));
    drop(listener);
    assert!(!is_listening(port));
}

#[test]
fn fallback_explanation() {
    let text = explain_interactive_fallback(" Access is denied.\r\n", Path::new(r"C:\it's\cc"));
    assert!(text.contains("LOGON ONLY"));
    assert!(text.contains("  Reason: Access is denied.\n"));
    assert!(text.contains("Boot-start requires the S4U logon type, which only an elevated"));
    assert!(text.contains("accept the administrator prompt:"));
    assert!(text.contains("it''s"));
    assert!(text.trim_end().ends_with("service install --elevate"));
}

#[test]
fn escaping() {
    assert_eq!(
        escape(r#"<a & 'b' "c">"#),
        "&lt;a &amp; &apos;b&apos; &quot;c&quot;&gt;"
    );
}

#[cfg(not(windows))]
#[test]
fn everything_but_status_is_unsupported_off_windows() {
    assert!(matches!(install(&spec()), Err(ServiceError::Unsupported)));
    assert!(matches!(start(1), Err(ServiceError::Unsupported)));
    assert!(matches!(stop(&[]), Err(ServiceError::Unsupported)));
    assert!(matches!(uninstall(), Err(ServiceError::Unsupported)));
    assert_eq!(registered_port(), None);
    assert_eq!(registered_dir(), None);
    let status = status(Some(1)).expect("status");
    assert!(!status.supported && !status.registered);
}
