//! CPU vulnerability mitigation policy: the `/etc/uflinux/boot.conf` record and
//! the in-place rewrite of the `mitigations=` token in a kernel command line.
//!
//! UFLinux ships mitigations switched off, which is what the boot-entry
//! generator appends when this file is missing or unreadable. Settings only ever
//! writes the two tokens the generator understands, so a hand-edited file with
//! anything else in it is reported as the default rather than being rewritten
//! behind the user's back.
use crate::*;
use std::{fs, path::Path};

pub const POLICY: &str = "/etc/uflinux/boot.conf";

/// Reads the policy, defaulting to the shipped value when the file is absent.
pub fn read_policy(path: &Path) -> BootPolicy {
    BootPolicy {
        mitigations: fs::read_to_string(path)
            .ok()
            .map(|text| parse_policy(&text))
            .unwrap_or_default(),
    }
}
pub fn parse_policy(text: &str) -> Mitigations {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| key.trim() == "mitigations")
        .last()
        .and_then(|(_, value)| Mitigations::parse(value).ok())
        .unwrap_or_default()
}
pub fn render_policy(policy: BootPolicy) -> String {
    format!(
        "# UFLinux boot policy, applied by /usr/libexec/uflinux-boot-menu on the\n\
         # next restart. mitigations=off favours throughput; mitigations=auto lets\n\
         # the kernel enable its CPU vulnerability mitigations.\n\
         mitigations={}\n",
        policy.mitigations.name()
    )
}

/// Rewrites the `mitigations=` token of one kernel command line, keeping every
/// other argument and all of the original spacing. A line that carries the
/// token twice keeps the first occurrence and loses the rest. `None` means the
/// line already asks for `value`.
pub fn rewrite_arguments(line: &str, value: Mitigations) -> Option<String> {
    let token = format!("mitigations={}", value.name());
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut cursor = 0;
    let mut seen = false;
    let mut dropped = false;
    // Start of the whitespace run pushed most recently, so that a dropped
    // duplicate does not leave the gap it was separated from its neighbour by.
    let mut gap = 0;
    while cursor < bytes.len() {
        if !bytes[cursor].is_ascii_whitespace() {
            let end = bytes[cursor..]
                .iter()
                .position(|b| b.is_ascii_whitespace())
                .map_or(bytes.len(), |offset| cursor + offset);
            let word = &line[cursor..end];
            if word.starts_with("mitigations=") {
                if seen {
                    out.truncate(gap);
                    dropped = true;
                } else {
                    seen = true;
                    out.push_str(&token);
                }
                cursor = end;
                continue;
            }
            out.push_str(word);
            cursor = end;
            continue;
        }
        if !out.ends_with(char::is_whitespace) {
            gap = out.len();
        }
        out.push(bytes[cursor] as char);
        cursor += 1;
    }
    if dropped {
        if !line.starts_with(char::is_whitespace) {
            out = out.trim_start().to_string();
        }
        if !line.ends_with(char::is_whitespace) {
            out.truncate(out.trim_end().len());
        }
    }
    if seen {
        return (out != line).then_some(out);
    }
    if line.trim().is_empty() {
        return Some(token);
    }
    Some(format!("{out} {token}"))
}

/// The `mitigations=` token a command line asks for, if it carries one.
pub fn arguments_value(line: &str) -> Option<Mitigations> {
    line.split_whitespace()
        .filter_map(|word| word.strip_prefix("mitigations="))
        .last()
        .and_then(|value| Mitigations::parse(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_files_default_to_off() {
        assert_eq!(parse_policy(""), Mitigations::Off);
        assert_eq!(parse_policy("# only a comment\n"), Mitigations::Off);
        assert_eq!(parse_policy("mitigations=spectre\n"), Mitigations::Off);
        assert_eq!(parse_policy("mitigations=auto\n"), Mitigations::Auto);
        assert_eq!(parse_policy("mitigations = auto \n"), Mitigations::Auto);
        // The last record wins, matching the generator's parser.
        assert_eq!(
            parse_policy("mitigations=auto\nmitigations=off\n"),
            Mitigations::Off
        );
    }

    #[test]
    fn rendered_policy_parses_back() {
        for value in [Mitigations::Off, Mitigations::Auto] {
            let text = render_policy(BootPolicy { mitigations: value });
            assert!(text.starts_with('#'));
            assert_eq!(parse_policy(&text), value);
        }
    }

    #[test]
    fn a_missing_token_is_appended() {
        let line = "root=UUID=1234 rw quiet splash";
        assert_eq!(
            rewrite_arguments(line, Mitigations::Off).as_deref(),
            Some("root=UUID=1234 rw quiet splash mitigations=off")
        );
        assert_eq!(arguments_value(line), None);
    }

    #[test]
    fn an_existing_token_is_replaced_in_place() {
        let line = "root=UUID=1234 mitigations=auto console=ttyS0,115200";
        assert_eq!(
            rewrite_arguments(line, Mitigations::Off).as_deref(),
            Some("root=UUID=1234 mitigations=off console=ttyS0,115200")
        );
        assert_eq!(
            rewrite_arguments(line, Mitigations::Auto),
            None,
            "already asking for the requested value"
        );
        assert_eq!(arguments_value(line), Some(Mitigations::Auto));
    }

    #[test]
    fn spacing_and_tabs_survive_the_rewrite() {
        let line = "\tAPPEND\troot=/dev/mmcblk0p2\tmitigations=off\t";
        assert_eq!(
            rewrite_arguments(line, Mitigations::Auto).as_deref(),
            Some("\tAPPEND\troot=/dev/mmcblk0p2\tmitigations=auto\t")
        );
    }

    #[test]
    fn a_duplicate_token_is_dropped() {
        let line = "mitigations=auto root=/dev/sda2 mitigations=off";
        assert_eq!(
            rewrite_arguments(line, Mitigations::Auto).as_deref(),
            Some("mitigations=auto root=/dev/sda2")
        );
    }

    #[test]
    fn an_unrelated_word_is_not_mistaken_for_the_token() {
        let line = "mitigation_disclosure=auto root=/dev/sda2";
        assert_eq!(arguments_value(line), None);
        assert_eq!(
            rewrite_arguments(line, Mitigations::Off).as_deref(),
            Some("mitigation_disclosure=auto root=/dev/sda2 mitigations=off")
        );
    }

    #[test]
    fn snapshots_without_a_boot_section_are_accepted() {
        // A daemon built without this page still sends a snapshot without it.
        let mut value = serde_json::to_value(Snapshot::default()).unwrap();
        value.as_object_mut().unwrap().remove("boot");
        let snapshot = serde_json::from_value::<Snapshot>(value).unwrap();
        assert!(!snapshot.boot.available);
        assert_eq!(snapshot.boot.configured, Mitigations::Off);
        assert_eq!(snapshot.boot.applied, None);
    }
}
