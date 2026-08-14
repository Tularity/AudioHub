//! Validation of AudioHub's per-user Windows Scheduled Task contract.
//!
//! `schtasks /Query /XML` is machine state, not a trustworthy echo of what we
//! once wrote. A task with the right command but a different principal,
//! trigger, arguments, or enabled state is stale and must be repaired.

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsTaskInspection {
    /// The sole Exec/Command, even when another contract field is stale.
    pub target: Option<String>,
    /// Every security and lifecycle field matches AudioHub's fixed contract.
    pub current: bool,
}

#[derive(Default)]
struct ParsedTask {
    trigger_children: usize,
    logon_triggers: usize,
    principal_children: usize,
    action_children: usize,
    exec_actions: usize,
    logon_enabled: Vec<String>,
    task_enabled: Vec<String>,
    logon_users: Vec<String>,
    principal_users: Vec<String>,
    logon_types: Vec<String>,
    run_levels: Vec<String>,
    commands: Vec<String>,
    arguments: Vec<String>,
}

fn local_name(raw: &[u8]) -> String {
    let raw = std::str::from_utf8(raw).unwrap_or_default();
    raw.rsplit(':').next().unwrap_or(raw).to_string()
}

fn ends_with_path(stack: &[String], suffix: &[&str]) -> bool {
    stack.len() >= suffix.len()
        && stack[stack.len() - suffix.len()..]
            .iter()
            .map(String::as_str)
            .eq(suffix.iter().copied())
}

fn push_text(parsed: &mut ParsedTask, stack: &[String], raw: &str) {
    let value = quick_xml::escape::unescape(raw)
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if value.is_empty() {
        return;
    }
    if ends_with_path(stack, &["Triggers", "LogonTrigger", "Enabled"]) {
        parsed.logon_enabled.push(value);
    } else if ends_with_path(stack, &["Settings", "Enabled"]) {
        parsed.task_enabled.push(value);
    } else if ends_with_path(stack, &["Triggers", "LogonTrigger", "UserId"]) {
        parsed.logon_users.push(value);
    } else if ends_with_path(stack, &["Principals", "Principal", "UserId"]) {
        parsed.principal_users.push(value);
    } else if ends_with_path(stack, &["Principals", "Principal", "LogonType"]) {
        parsed.logon_types.push(value);
    } else if ends_with_path(stack, &["Principals", "Principal", "RunLevel"]) {
        parsed.run_levels.push(value);
    } else if ends_with_path(stack, &["Actions", "Exec", "Command"]) {
        parsed.commands.push(value);
    } else if ends_with_path(stack, &["Actions", "Exec", "Arguments"]) {
        parsed.arguments.push(value);
    }
}

fn parse_task(xml: &str) -> Option<ParsedTask> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut stack = Vec::<String>::new();
    let mut parsed = ParsedTask::default();
    loop {
        match reader.read_event().ok()? {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if stack.last().is_some_and(|parent| parent == "Triggers") {
                    parsed.trigger_children += 1;
                }
                if stack.last().is_some_and(|parent| parent == "Principals") {
                    parsed.principal_children += 1;
                }
                if stack.last().is_some_and(|parent| parent == "Actions") {
                    parsed.action_children += 1;
                }
                if name == "LogonTrigger" && stack.last().is_some_and(|parent| parent == "Triggers")
                {
                    parsed.logon_triggers += 1;
                }
                if name == "Exec" && stack.last().is_some_and(|parent| parent == "Actions") {
                    parsed.exec_actions += 1;
                }
                stack.push(name);
            }
            Event::Empty(empty) => {
                let name = local_name(empty.name().as_ref());
                if stack.last().is_some_and(|parent| parent == "Triggers") {
                    parsed.trigger_children += 1;
                    if name == "LogonTrigger" {
                        parsed.logon_triggers += 1;
                    }
                }
                if stack.last().is_some_and(|parent| parent == "Principals") {
                    parsed.principal_children += 1;
                }
                if stack.last().is_some_and(|parent| parent == "Actions") {
                    parsed.action_children += 1;
                    if name == "Exec" {
                        parsed.exec_actions += 1;
                    }
                }
            }
            Event::Text(text) => {
                let decoded = text.decode().ok()?;
                push_text(&mut parsed, &stack, &decoded);
            }
            Event::CData(text) => {
                let decoded = text.decode().ok()?;
                push_text(&mut parsed, &stack, &decoded);
            }
            Event::End(_) => {
                stack.pop()?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    stack.is_empty().then_some(parsed)
}

fn sole_eq(values: &[String], expected: &str) -> bool {
    values.len() == 1 && values[0].eq_ignore_ascii_case(expected)
}

fn sole_user_matches(values: &[String], expected_sid: &str) -> bool {
    if values.len() != 1 {
        return false;
    }
    if values[0].eq_ignore_ascii_case(expected_sid) {
        return true;
    }
    #[cfg(windows)]
    {
        return crate::platform::task_user_id_matches(&values[0], expected_sid);
    }
    #[cfg(not(windows))]
    false
}

/// Task Scheduler removes schema-default values from `/Query /XML` output even
/// when they were explicit in the creation XML. Absence therefore means the
/// documented effective default; an explicit different value remains stale.
fn effective_default_eq(values: &[String], expected: &str) -> bool {
    values.is_empty() || sole_eq(values, expected)
}

fn normalized_windows_path(path: &str) -> String {
    let path = path.trim().trim_matches('"').replace('/', "\\");
    path.strip_prefix(r"\\?\")
        .unwrap_or(&path)
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// Validate an exported task against the complete AudioHub startup contract.
pub fn inspect_windows_task_xml(
    xml: &str,
    expected_app: &Path,
    expected_user_sid: &str,
) -> WindowsTaskInspection {
    let Some(parsed) = parse_task(xml) else {
        return WindowsTaskInspection {
            target: None,
            current: false,
        };
    };
    let target = (parsed.commands.len() == 1).then(|| parsed.commands[0].clone());
    let command_matches = target.as_deref().is_some_and(|target| {
        normalized_windows_path(target)
            == normalized_windows_path(&expected_app.display().to_string())
    });
    let current = parsed.trigger_children == 1
        && parsed.logon_triggers == 1
        && parsed.principal_children == 1
        && parsed.action_children == 1
        && parsed.exec_actions == 1
        && effective_default_eq(&parsed.logon_enabled, "true")
        && effective_default_eq(&parsed.task_enabled, "true")
        && sole_user_matches(&parsed.logon_users, expected_user_sid)
        && sole_user_matches(&parsed.principal_users, expected_user_sid)
        && sole_eq(&parsed.logon_types, "InteractiveToken")
        && effective_default_eq(&parsed.run_levels, "LeastPrivilege")
        && command_matches
        && sole_eq(&parsed.arguments, "--background");
    WindowsTaskInspection { target, current }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> String {
        r#"<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>S-1-5-21-42</UserId></LogonTrigger></Triggers>
<Principals><Principal id="Author"><UserId>S-1-5-21-42</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><Enabled>true</Enabled></Settings>
<Actions Context="Author"><Exec><Command>C:\Program Files\AudioHub\audiohub-app.exe</Command><Arguments>--background</Arguments></Exec></Actions>
</Task>"#
            .into()
    }

    #[test]
    fn complete_contract_is_current() {
        let result = inspect_windows_task_xml(
            &valid(),
            Path::new(r"C:\Program Files\AudioHub\audiohub-app.exe"),
            "S-1-5-21-42",
        );
        assert!(result.current);
        assert_eq!(
            result.target.as_deref(),
            Some(r"C:\Program Files\AudioHub\audiohub-app.exe")
        );
    }

    #[test]
    fn every_privilege_and_lifecycle_mutation_is_stale() {
        let valid = valid();
        for changed in [
            valid.replace("<Enabled>true</Enabled>", "<Enabled>false</Enabled>"),
            valid.replace("InteractiveToken", "Password"),
            valid.replace("LeastPrivilege", "HighestAvailable"),
            valid.replace("--background", "--complete-install"),
            valid.replace("S-1-5-21-42", "S-1-5-18"),
            valid.replace("<Exec>", "<Exec/><Exec>"),
            valid.replace("</Triggers>", "<TimeTrigger/></Triggers>"),
        ] {
            assert!(
                !inspect_windows_task_xml(
                    &changed,
                    Path::new(r"C:\Program Files\AudioHub\audiohub-app.exe"),
                    "S-1-5-21-42",
                )
                .current
            );
        }
    }

    #[test]
    fn scheduler_omitted_schema_defaults_are_current() {
        let exported = valid()
            .replace("<Enabled>true</Enabled>", "")
            .replace("<RunLevel>LeastPrivilege</RunLevel>", "");
        let result = inspect_windows_task_xml(
            &exported,
            Path::new(r"C:\Program Files\AudioHub\audiohub-app.exe"),
            "S-1-5-21-42",
        );
        assert!(result.current);
    }
}
