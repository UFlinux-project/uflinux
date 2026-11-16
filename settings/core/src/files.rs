use crate::Result;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("uflinux")
}
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
/// Same-directory, no-follow temporary file; readers see complete old or new data.
pub fn atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or("Missing parent directory")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = parent.join(format!(".uflinux-{}-{}", std::process::id(), unique()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(data)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
        fs::rename(&temporary, path).map_err(|e| e.to_string())?;
        fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| e.to_string())
    })();
    let _ = fs::remove_file(temporary);
    result
}
pub fn save_json(path: &Path, value: &impl serde::Serialize, mode: u32) -> Result<()> {
    atomic(
        path,
        &serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
        mode,
    )
}
pub fn unique() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Update the HDR node/attribute without dropping unrelated XML or comments.
pub fn set_hdr_xml(source: &str, enabled: bool) -> Result<String> {
    use quick_xml::{
        Reader, Writer,
        events::{BytesEnd, BytesStart, BytesText, Event},
    };
    fn hdr(writer: &mut Writer<Vec<u8>>, value: &str) -> Result<()> {
        writer
            .write_event(Event::Start(BytesStart::new("hdr")))
            .map_err(|e| e.to_string())?;
        writer
            .write_event(Event::Text(BytesText::new(value)))
            .map_err(|e| e.to_string())?;
        writer
            .write_event(Event::End(BytesEnd::new("hdr")))
            .map_err(|e| e.to_string())
    }
    fn core(start: &BytesStart<'_>, value: &str) -> Result<(BytesStart<'static>, bool)> {
        let mut output = start.to_owned();
        output.clear_attributes();
        let mut found = false;
        for attribute in start.attributes() {
            let attribute = attribute.map_err(|e| e.to_string())?;
            if attribute.key.as_ref() == b"hdr" {
                output.push_attribute(("hdr", value));
                found = true;
            } else {
                output.push_attribute(attribute);
            }
        }
        Ok((output, found))
    }
    let mut reader = Reader::from_str(source);
    let mut writer = Writer::new(Vec::new());
    let value = if enabled { "yes" } else { "no" };
    let (mut depth, mut seen_root, mut seen_core, mut in_core, mut found, mut skip) =
        (0, false, false, false, false, false);
    loop {
        let event = reader.read_event().map_err(|e| e.to_string())?;
        match &event {
            Event::Start(start) => {
                depth += 1;
                if depth == 1 {
                    if seen_root || start.name().as_ref() != b"labwc_config" {
                        return Err("Expected a labwc_config XML document".into());
                    }
                    seen_root = true;
                }
                if depth == 2 && start.name().as_ref() == b"core" {
                    seen_core = true;
                    in_core = true;
                    let (start, attribute) = core(start, value)?;
                    found = attribute;
                    writer
                        .write_event(Event::Start(start))
                        .map_err(|e| e.to_string())?;
                    continue;
                }
                if depth == 3 && in_core && start.name().as_ref() == b"hdr" {
                    found = true;
                    skip = true;
                    writer
                        .write_event(event.clone())
                        .map_err(|e| e.to_string())?;
                    writer
                        .write_event(Event::Text(BytesText::new(value)))
                        .map_err(|e| e.to_string())?;
                    continue;
                }
            }
            Event::Empty(start) if depth == 0 && start.name().as_ref() == b"labwc_config" => {
                if seen_root {
                    return Err("Duplicate XML root".into());
                }
                seen_root = true;
                seen_core = true;
                writer
                    .write_event(Event::Start(start.to_owned()))
                    .map_err(|e| e.to_string())?;
                writer
                    .write_event(Event::Start(BytesStart::new("core")))
                    .map_err(|e| e.to_string())?;
                hdr(&mut writer, value)?;
                writer
                    .write_event(Event::End(BytesEnd::new("core")))
                    .map_err(|e| e.to_string())?;
                writer
                    .write_event(Event::End(BytesEnd::new("labwc_config")))
                    .map_err(|e| e.to_string())?;
                continue;
            }
            Event::Empty(start) if depth == 1 && start.name().as_ref() == b"core" => {
                seen_core = true;
                let (start, attribute) = core(start, value)?;
                writer
                    .write_event(Event::Start(start))
                    .map_err(|e| e.to_string())?;
                if !attribute {
                    hdr(&mut writer, value)?;
                }
                writer
                    .write_event(Event::End(BytesEnd::new("core")))
                    .map_err(|e| e.to_string())?;
                continue;
            }
            Event::Empty(start) if depth == 2 && in_core && start.name().as_ref() == b"hdr" => {
                found = true;
                hdr(&mut writer, value)?;
                continue;
            }
            Event::End(end) => {
                if depth == 2 && in_core && end.name().as_ref() == b"core" {
                    if !found {
                        hdr(&mut writer, value)?;
                    }
                    in_core = false;
                }
                if depth == 1 && !seen_core {
                    writer
                        .write_event(Event::Start(BytesStart::new("core")))
                        .map_err(|e| e.to_string())?;
                    hdr(&mut writer, value)?;
                    writer
                        .write_event(Event::End(BytesEnd::new("core")))
                        .map_err(|e| e.to_string())?;
                }
                if depth == 3 && skip && end.name().as_ref() == b"hdr" {
                    skip = false;
                }
                depth -= 1;
            }
            Event::Text(_) | Event::CData(_) if skip => continue,
            Event::Eof => break,
            _ => {}
        }
        writer.write_event(event).map_err(|e| e.to_string())?;
    }
    if depth != 0 || !seen_root {
        return Err("Incomplete labwc configuration".into());
    }
    String::from_utf8(writer.into_inner()).map_err(|e| e.to_string())
}

pub fn hdr_requested(source: &str) -> Option<bool> {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_str(source);
    let mut depth = 0;
    let mut inside = false;
    let mut value = None;
    let boolean = |v: &str| match v.trim().to_lowercase().as_str() {
        "yes" | "true" | "on" | "1" => Some(true),
        "no" | "false" | "off" | "0" => Some(false),
        _ => None,
    };
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => {
                depth += 1;
                if depth == 2 && e.name().as_ref() == b"core" {
                    inside = true;
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == b"hdr" {
                            value = boolean(std::str::from_utf8(&a.value).ok()?);
                        }
                    }
                }
                if depth == 3 && inside && e.name().as_ref() == b"hdr" {
                    value = boolean(&reader.read_text(e.name()).ok()?);
                    depth -= 1;
                }
            }
            Event::Empty(e) if depth == 1 && e.name().as_ref() == b"core" => {
                for a in e.attributes().flatten() {
                    if a.key.as_ref() == b"hdr" {
                        value = boolean(std::str::from_utf8(&a.value).ok()?);
                    }
                }
            }
            Event::End(_) => {
                if depth == 2 {
                    inside = false;
                }
                depth -= 1;
            }
            Event::Eof => return value,
            _ => {}
        }
    }
}

/// Change only the native switcher presentation, preserving personal bindings.
pub fn set_window_previews_xml(source: &str, enabled: bool) -> Result<String> {
    use quick_xml::{
        Reader, Writer,
        events::{BytesEnd, BytesStart, Event},
    };
    let mut reader = Reader::from_str(source);
    let mut events = Vec::new();
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Eof => break,
            Event::Empty(tag) => {
                events.push(Event::Start(tag.to_owned()));
                events.push(Event::End(BytesEnd::new(
                    String::from_utf8_lossy(tag.name().as_ref()).into_owned(),
                )));
            }
            event => events.push(event.into_owned()),
        }
    }
    let mut writer = Writer::new(Vec::new());
    let mut stack = Vec::<Vec<u8>>::new();
    let (mut root, mut switcher, mut osd) = (false, false, false);
    let attributes =
        |tag: &BytesStart<'_>, values: &[(&str, &str)]| -> Result<BytesStart<'static>> {
            let mut out = tag.to_owned();
            out.clear_attributes();
            for attr in tag.attributes() {
                let attr = attr.map_err(|e| e.to_string())?;
                if !values
                    .iter()
                    .any(|(key, _)| key.as_bytes() == attr.key.as_ref())
                {
                    out.push_attribute(attr);
                }
            }
            for value in values {
                out.push_attribute(*value);
            }
            Ok(out)
        };
    let style = if enabled { "thumbnail" } else { "classic" };
    let preview = if enabled { "yes" } else { "no" };
    let osd_tag = || {
        let mut tag = BytesStart::new("osd");
        tag.push_attribute(("show", "yes"));
        tag.push_attribute(("style", style));
        tag.push_attribute(("output", "focused"));
        tag
    };
    for event in events {
        let event = match event {
            Event::Start(tag) => {
                if stack.is_empty() {
                    if root || tag.name().as_ref() != b"labwc_config" {
                        return Err("Expected a labwc_config XML document".into());
                    }
                    root = true;
                }
                let name = tag.name().as_ref().to_vec();
                let tag = if stack.len() == 1 && name == b"windowSwitcher" {
                    if switcher {
                        return Err("Duplicate windowSwitcher configuration".into());
                    }
                    switcher = true;
                    attributes(&tag, &[("preview", preview), ("order", "focus")])?
                } else if stack.len() == 2 && stack[1] == b"windowSwitcher" && name == b"osd" {
                    if osd {
                        return Err("Duplicate switcher OSD configuration".into());
                    }
                    osd = true;
                    attributes(
                        &tag,
                        &[("show", "yes"), ("style", style), ("output", "focused")],
                    )?
                } else {
                    tag
                };
                stack.push(name);
                Event::Start(tag)
            }
            Event::End(tag) => {
                if stack.last().map(Vec::as_slice) != Some(tag.name().as_ref()) {
                    return Err("Malformed labwc XML".into());
                }
                if stack.len() == 2 && stack[1] == b"windowSwitcher" && !osd {
                    writer
                        .write_event(Event::Empty(osd_tag()))
                        .map_err(|e| e.to_string())?;
                }
                if stack.len() == 1 && !switcher {
                    let mut tag = BytesStart::new("windowSwitcher");
                    tag.push_attribute(("preview", preview));
                    tag.push_attribute(("order", "focus"));
                    writer
                        .write_event(Event::Start(tag))
                        .map_err(|e| e.to_string())?;
                    writer
                        .write_event(Event::Empty(osd_tag()))
                        .map_err(|e| e.to_string())?;
                    writer
                        .write_event(Event::End(BytesEnd::new("windowSwitcher")))
                        .map_err(|e| e.to_string())?;
                }
                stack.pop();
                Event::End(tag)
            }
            event => event,
        };
        writer.write_event(event).map_err(|e| e.to_string())?;
    }
    if !root || !stack.is_empty() {
        return Err("Malformed labwc XML".into());
    }
    String::from_utf8(writer.into_inner()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn switcher_edit_preserves_personal_configuration() {
        let input = "<labwc_config><!-- personal --><keyboard><keybind key='A-Tab'/></keyboard><windowSwitcher outlines='no'><osd style='classic'/><fields><field content='title'/></fields></windowSwitcher></labwc_config>";
        let on = set_window_previews_xml(input, true).unwrap();
        assert!(
            on.contains("<!-- personal -->")
                && (on.contains("outlines=\"no\"") || on.contains("outlines='no'"))
        );
        assert!(on.contains("thumbnail") && on.contains("keybind") && on.contains("fields"));
        let off = set_window_previews_xml(&on, false).unwrap();
        assert!(off.contains("style=\"classic\"") && off.contains("preview=\"no\""));
        assert_eq!(set_window_previews_xml(&off, false).unwrap(), off);
        assert!(
            set_window_previews_xml("<labwc_config/>", true)
                .unwrap()
                .contains("thumbnail")
        );
        assert!(set_window_previews_xml("<wrong/>", true).is_err());
        assert!(set_window_previews_xml("<labwc_config>", true).is_err());
    }
    #[test]
    fn existing_appearance_defaults_previews_on() {
        let appearance: crate::model::Appearance =
            serde_json::from_str(r##"{"wallpapers":[],"color":"#000000"}"##).unwrap();
        assert!(appearance.window_previews);
    }
}
