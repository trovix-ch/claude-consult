//! Registers, inspects and deletes a throwaway scheduled task. It changes the
//! machine, so it is ignored: run it by hand with
//! `cargo test -p consult-service --test live_task -- --ignored`.
//! It never touches the real `OpenRouterMCP` task and never runs the task.

#![cfg(windows)]

use consult_service::{Registration, ServiceSpec, Task};

#[test]
#[ignore = "registers a real scheduled task; run by hand"]
fn install_status_uninstall_round_trip() {
    let task = Task::named(format!("ClaudeConsultTest-{}", std::process::id()));
    let exe = std::env::current_exe().expect("test exe");
    let dir = exe.parent().expect("exe dir").to_path_buf();
    let spec = ServiceSpec {
        exe,
        working_dir: dir.clone(),
        port: 1,
        host: "127.0.0.1".into(),
    };

    let registration = task.install(&spec).expect("install");
    let result = std::panic::catch_unwind(|| {
        let status = task.status(None).expect("status");
        assert!(status.registered);
        assert_eq!(status.port, 1);
        assert_eq!(task.registered_port(), Some(1));
        assert_eq!(status.working_dir.as_deref(), Some(dir.as_path()));
        match &registration {
            Registration::S4U => {
                assert_eq!(status.logon_type.as_deref(), Some("S4U"));
                assert_eq!(status.triggers, ["Boot", "Logon"]);
            }
            Registration::InteractiveLogonOnly { reason } => {
                assert!(!reason.is_empty());
                assert_eq!(status.logon_type.as_deref(), Some("InteractiveToken"));
                assert_eq!(status.triggers, ["Logon"]);
            }
        }
        eprintln!("{registration:?}\n{status:#?}");
    });
    task.uninstall().expect("uninstall");
    assert!(!task.status(None).expect("status").registered);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
