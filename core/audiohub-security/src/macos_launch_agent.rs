//! Validation of AudioHub's per-user macOS LaunchAgent contract.
//!
//! A plist on disk is only evidence that a registration exists.  It is not a
//! trustworthy echo of the payload AudioHub wrote: another build or a manual
//! edit can preserve the App path while changing how or when launchd starts it.

use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacLaunchAgentInspection {
    /// The App argument in the sole `ProgramArguments` array, when unambiguous.
    pub target: Option<String>,
    /// Every field in AudioHub's fixed LaunchAgent contract matches exactly.
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlistValue {
    String(String),
    Bool(bool),
    Array(Vec<PlistValue>),
    Dict(Vec<(String, PlistValue)>),
    Other,
}

struct Parser<'a> {
    reader: Reader<&'a [u8]>,
}

impl<'a> Parser<'a> {
    fn new(xml: &'a str) -> Self {
        let mut reader = Reader::from_str(xml);
        // Whitespace inside a string is significant. Structural callers skip
        // whitespace-only text explicitly instead of asking the reader to trim
        // every text node.
        reader.config_mut().trim_text(false);
        Self { reader }
    }

    fn read(&mut self) -> Option<Event<'static>> {
        self.reader.read_event().ok().map(Event::into_owned)
    }

    fn significant(&mut self) -> Option<Event<'static>> {
        loop {
            match self.read()? {
                Event::Text(text) if decode_text(&text).ok()?.trim().is_empty() => {}
                Event::Comment(_) | Event::Decl(_) | Event::DocType(_) | Event::PI(_) => {}
                event => return Some(event),
            }
        }
    }

    fn text_element(&mut self, expected_end: &str) -> Option<String> {
        let mut value = String::new();
        loop {
            match self.read()? {
                Event::Text(text) => value.push_str(&decode_text(&text).ok()?),
                Event::CData(text) => value.push_str(&text.decode().ok()?),
                Event::GeneralRef(reference) => value.push_str(&decode_reference(&reference).ok()?),
                Event::Comment(_) | Event::PI(_) => {}
                Event::End(end) if name(end.name().as_ref())? == expected_end => {
                    return Some(value)
                }
                _ => return None,
            }
        }
    }

    fn empty_element(&mut self, expected_end: &str) -> Option<()> {
        match self.significant()? {
            Event::End(end) if name(end.name().as_ref())? == expected_end => Some(()),
            _ => None,
        }
    }

    fn value(&mut self, event: Event<'static>) -> Option<PlistValue> {
        match event {
            Event::Empty(empty) => match name(empty.name().as_ref())? {
                "string" => Some(PlistValue::String(String::new())),
                "array" => Some(PlistValue::Array(Vec::new())),
                "dict" => Some(PlistValue::Dict(Vec::new())),
                "true" => Some(PlistValue::Bool(true)),
                "false" => Some(PlistValue::Bool(false)),
                "data" | "date" | "real" | "integer" => Some(PlistValue::Other),
                _ => None,
            },
            Event::Start(start) => {
                let element = name(start.name().as_ref())?.to_string();
                match element.as_str() {
                    "string" => self.text_element("string").map(PlistValue::String),
                    "array" => self.array().map(PlistValue::Array),
                    "dict" => self.dict().map(PlistValue::Dict),
                    "true" => {
                        self.empty_element("true")?;
                        Some(PlistValue::Bool(true))
                    }
                    "false" => {
                        self.empty_element("false")?;
                        Some(PlistValue::Bool(false))
                    }
                    "data" | "date" | "real" | "integer" => {
                        self.text_element(&element)?;
                        Some(PlistValue::Other)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn array(&mut self) -> Option<Vec<PlistValue>> {
        let mut values = Vec::new();
        loop {
            let event = self.significant()?;
            if let Event::End(end) = &event {
                return (name(end.name().as_ref())? == "array").then_some(values);
            }
            values.push(self.value(event)?);
        }
    }

    fn dict(&mut self) -> Option<Vec<(String, PlistValue)>> {
        let mut entries = Vec::new();
        loop {
            let event = self.significant()?;
            if let Event::End(end) = &event {
                return (name(end.name().as_ref())? == "dict").then_some(entries);
            }
            let Event::Start(key) = event else {
                return None;
            };
            if name(key.name().as_ref())? != "key" {
                return None;
            }
            let key = self.text_element("key")?;
            let value_event = self.significant()?;
            let value = self.value(value_event)?;
            entries.push((key, value));
        }
    }

    fn root_dict(mut self) -> Option<Vec<(String, PlistValue)>> {
        let Event::Start(plist) = self.significant()? else {
            return None;
        };
        if name(plist.name().as_ref())? != "plist" {
            return None;
        }
        let Event::Start(dict) = self.significant()? else {
            return None;
        };
        if name(dict.name().as_ref())? != "dict" {
            return None;
        }
        let entries = self.dict()?;
        let Event::End(plist) = self.significant()? else {
            return None;
        };
        if name(plist.name().as_ref())? != "plist" {
            return None;
        }
        matches!(self.significant()?, Event::Eof).then_some(entries)
    }
}

fn name(raw: &[u8]) -> Option<&str> {
    std::str::from_utf8(raw).ok()
}

fn decode_text(text: &quick_xml::events::BytesText<'_>) -> Result<String, ()> {
    text.decode()
        .map(|value| value.into_owned())
        .map_err(|_| ())
}

fn decode_reference(reference: &quick_xml::events::BytesRef<'_>) -> Result<String, ()> {
    if let Some(value) = reference.resolve_char_ref().map_err(|_| ())? {
        return Ok(value.to_string());
    }
    match reference.decode().map_err(|_| ())?.as_ref() {
        "amp" => Ok("&".into()),
        "lt" => Ok("<".into()),
        "gt" => Ok(">".into()),
        "quot" => Ok("\"".into()),
        "apos" => Ok("'".into()),
        _ => Err(()),
    }
}

fn sole_value<'a>(entries: &'a [(String, PlistValue)], key: &str) -> Option<&'a PlistValue> {
    let mut matches = entries
        .iter()
        .filter_map(|(name, value)| (name == key).then_some(value));
    let value = matches.next()?;
    matches.next().is_none().then_some(value)
}

fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Validate an XML LaunchAgent plist against AudioHub's complete contract.
///
/// `expected_app == None` is useful when a bare daemon can still report an
/// orphaned registration: the target is extracted, but `current` is always
/// false because that process has no stable App target with which to compare.
pub fn inspect_macos_launch_agent_plist(
    xml: &str,
    expected_label: &str,
    expected_app: Option<&Path>,
) -> MacLaunchAgentInspection {
    let Some(entries) = Parser::new(xml).root_dict() else {
        return MacLaunchAgentInspection {
            target: None,
            current: false,
        };
    };

    let arguments = match sole_value(&entries, "ProgramArguments") {
        Some(PlistValue::Array(arguments)) => Some(arguments),
        _ => None,
    };
    // Keep the target of either known layout for diagnostics. The old layout
    // is stale; ambiguous or malformed action arrays have no inferred target.
    let target = arguments.and_then(|arguments| match arguments.as_slice() {
        [PlistValue::String(open), PlistValue::String(global), PlistValue::String(new_instance), PlistValue::String(target), PlistValue::String(args), PlistValue::String(background)]
            if open == "/usr/bin/open" && global == "-g" && new_instance == "-n"
                && args == "--args" && background == "--background" => Some(target.clone()),
        [PlistValue::String(open), PlistValue::String(global), PlistValue::String(target), PlistValue::String(args), PlistValue::String(background)]
            if open == "/usr/bin/open" && global == "-g"
                && args == "--args" && background == "--background" => Some(target.clone()),
        _ => None,
    });

    let expected_app = expected_app.map(canonical_or_original);
    let expected_arguments = expected_app.as_ref().map(|app| {
        [
            "/usr/bin/open".to_string(),
            "-g".to_string(),
            "-n".to_string(),
            app.display().to_string(),
            "--args".to_string(),
            "--background".to_string(),
        ]
    });
    let arguments_match = arguments
        .zip(expected_arguments.as_ref())
        .is_some_and(|(actual, expected)| {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| matches!(actual, PlistValue::String(value) if value == expected))
        });
    let label_matches = matches!(
        sole_value(&entries, "Label"),
        Some(PlistValue::String(label)) if label == expected_label
    );
    let run_at_load = matches!(
        sole_value(&entries, "RunAtLoad"),
        Some(PlistValue::Bool(true))
    );
    let process_type = matches!(
        sole_value(&entries, "ProcessType"),
        Some(PlistValue::String(value)) if value == "Interactive"
    );

    // Duplicate contract fields and alternate launch actions (`Program`,
    // `KeepAlive`, interval/watch/socket triggers, and so on) are stale. Benign
    // launchd metadata remains allowed so system/admin-added descriptive keys do
    // not force an otherwise equivalent registration to churn on every probe.
    let forbidden_lifecycle_keys = [
        "Program",
        "KeepAlive",
        "StartInterval",
        "StartCalendarInterval",
        "WatchPaths",
        "QueueDirectories",
        "PathState",
        "Sockets",
        "MachServices",
        "StartOnMount",
        "OtherJobEnabled",
        "Disabled",
    ];
    let no_extra_action = !entries.iter().any(|(key, _)| {
        forbidden_lifecycle_keys
            .iter()
            .any(|forbidden| key == forbidden)
    });
    let current =
        label_matches && arguments_match && run_at_load && process_type && no_extra_action;
    MacLaunchAgentInspection { target, current }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABEL: &str = "com.audiohub.app.autostart";
    const APP: &str = "/Applications/AudioHub.app";

    fn valid() -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LABEL}</string>
<key>ProgramArguments</key><array><string>/usr/bin/open</string><string>-g</string><string>-n</string><string>{APP}</string><string>--args</string><string>--background</string></array>
<key>RunAtLoad</key><true/>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>"#
        )
    }

    #[test]
    fn the_complete_contract_is_current() {
        let result = inspect_macos_launch_agent_plist(&valid(), LABEL, Some(Path::new(APP)));
        assert!(result.current, "{result:?}");
        assert_eq!(result.target.as_deref(), Some(APP));
    }

    #[test]
    fn a_correct_app_path_does_not_hide_a_mutated_contract() {
        let valid = valid();
        for changed in [
            valid.replace(LABEL, "com.example.other"),
            valid.replace(
                "<key>ProgramArguments</key>",
                &format!("<key>Label</key><string>{LABEL}</string><key>ProgramArguments</key>"),
            ),
            valid.replace(
                "<key>RunAtLoad</key>",
                "<key>RunAtLoad</key><true/><key>RunAtLoad</key>",
            ),
            valid.replace(
                "<key>ProcessType</key>",
                "<key>ProcessType</key><string>Interactive</string><key>ProcessType</key>",
            ),
            valid.replace("<string>/usr/bin/open</string>", "<string>/bin/sh</string>"),
            valid.replace("<string>-g</string>", "<string>-a</string>"),
            valid.replace("<string>--args</string>", "<string>--wrong</string>"),
            valid.replace(
                "<string>--background</string>",
                "<string>--foreground</string>",
            ),
            valid.replace("</array>", "<string>--extra</string></array>"),
            valid.replace("<true/>", "<false/>"),
            valid.replace("Interactive", "Background"),
            valid.replace(
                "</dict>",
                "<key>Program</key><string>/usr/bin/true</string></dict>",
            ),
            valid.replace("</dict>", "<key>KeepAlive</key><true/></dict>"),
            valid.replace(
                "</dict>",
                "<key>StartInterval</key><integer>1</integer></dict>",
            ),
        ] {
            let result = inspect_macos_launch_agent_plist(&changed, LABEL, Some(Path::new(APP)));
            assert!(!result.current, "mutation was accepted:\n{changed}");
        }
    }

    #[test]
    fn duplicate_program_arguments_are_ambiguous_and_stale() {
        let duplicate = valid().replace(
            "</dict>",
            &format!(
                "<key>ProgramArguments</key><array><string>/usr/bin/open</string><string>-g</string><string>{APP}</string><string>--args</string><string>--background</string></array></dict>"
            ),
        );
        let result = inspect_macos_launch_agent_plist(&duplicate, LABEL, Some(Path::new(APP)));
        assert!(!result.current);
        assert_eq!(result.target, None, "two actions do not have a sole target");
    }

    #[test]
    fn a_stale_non_action_field_keeps_the_target_for_diagnostics() {
        let changed = valid().replace("<true/>", "<false/>");
        let result = inspect_macos_launch_agent_plist(&changed, LABEL, Some(Path::new(APP)));
        assert!(!result.current);
        assert_eq!(result.target.as_deref(), Some(APP));
    }

    #[test]
    fn the_old_launch_layout_is_stale_but_keeps_its_target_for_diagnostics() {
        let body = valid().replace("<string>-n</string>", "");
        let result = inspect_macos_launch_agent_plist(&body, LABEL, Some(Path::new(APP)));
        assert!(!result.current);
        assert_eq!(result.target.as_deref(), Some(APP));
    }

    #[test]
    fn malformed_argument_layouts_do_not_invent_a_target() {
        for body in [
            valid().replace("<string>-n</string>", "<true/>"),
            valid().replace("<string>--args</string>", "<string>--wrong</string>"),
            valid().replace("</array>", "<string>--extra</string></array>"),
        ] {
            let result = inspect_macos_launch_agent_plist(&body, LABEL, Some(Path::new(APP)));
            assert!(!result.current);
            assert_eq!(result.target, None);
        }
    }

    #[test]
    fn benign_metadata_does_not_change_the_launch_contract() {
        let body = valid().replace(
            "</dict>",
            "<key>LimitLoadToSessionType</key><string>Aqua</string></dict>",
        );
        let result = inspect_macos_launch_agent_plist(&body, LABEL, Some(Path::new(APP)));
        assert!(result.current, "{result:?}");
    }

    #[test]
    fn xml_entities_are_decoded_before_exact_comparison() {
        let body = valid().replace(APP, "/Users/a&amp;b/AudioHub.app");
        let result = inspect_macos_launch_agent_plist(
            &body,
            LABEL,
            Some(Path::new("/Users/a&b/AudioHub.app")),
        );
        assert!(result.current, "{result:?}");
        assert_eq!(result.target.as_deref(), Some("/Users/a&b/AudioHub.app"));
    }

    #[test]
    fn malformed_xml_is_never_current() {
        let result = inspect_macos_launch_agent_plist(
            "<plist><dict><key>Label</key>",
            LABEL,
            Some(Path::new(APP)),
        );
        assert_eq!(
            result,
            MacLaunchAgentInspection {
                target: None,
                current: false,
            }
        );
    }
}
