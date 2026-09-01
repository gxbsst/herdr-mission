use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::Deserialize;

const MISSION_BINDING: &str = concat!(
    "[[keys.command]]\n",
    "key = \"prefix+m\"\n",
    "type = \"shell\"\n",
    "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\"\n",
    "description = \"打开 Mission 看板\"\n",
);
const MISSION_COMMAND: &str =
    "herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus";
const LEGACY_MISSION_COMMANDS: &[&str] = &[
    "weston.herdr-kit.open-mission-center",
    "weston.herdr-mission.mission-new",
];
const MISSION_INLINE_COMMAND_ENTRY: &str = concat!(
    "{ key = \"prefix+m\", type = \"shell\", ",
    "command = \"herdr plugin pane open --plugin weston.herdr-mission --entrypoint dashboard --focus\", ",
    "description = \"打开 Mission 看板\" }",
);

pub(crate) fn default_herdr_config_path() -> Option<PathBuf> {
    non_empty_env("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            non_empty_env("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("herdr/config.toml"))
        })
        .or_else(|| {
            non_empty_env("HOME")
                .map(PathBuf::from)
                .map(|path| path.join(".config/herdr/config.toml"))
        })
}

fn non_empty_env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

pub(crate) fn install_herdr_keybinding(config_path: &Path) -> io::Result<()> {
    let configured_parent = match config_path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Herdr config path has no parent directory",
            ));
        }
    };
    fs::create_dir_all(configured_parent)?;

    let configured_metadata = match fs::symlink_metadata(config_path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if configured_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        match config_path.canonicalize() {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Herdr config path is a dangling symlink",
                ));
            }
            Err(error) => return Err(error),
        }
    }

    let (target, previous, updated) = if configured_metadata.is_some() {
        let target = config_path.canonicalize()?;
        let previous = fs::read(&target)?;
        let content = String::from_utf8(previous.clone()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Herdr config is not valid UTF-8",
            )
        })?;
        let parsed = content.parse::<toml::Value>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Herdr config is not valid TOML: {error}"),
            )
        })?;
        if !parsed.is_table() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Herdr config top level must be a table",
            ));
        }
        validate_keys_shape(&parsed)?;
        let binding_state = mission_binding_state(&parsed);
        let mut updated = content;
        match binding_state {
            MissionBindingState::Installed => return Ok(()),
            MissionBindingState::Conflict => {
                updated = migrate_legacy_mission_binding(&updated, &parsed)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "prefix+m is already assigned to another Herdr command",
                    )
                })?;
            }
            MissionBindingState::Missing => {}
        }
        if binding_state == MissionBindingState::Missing {
            append_mission_binding(&mut updated, &parsed)?;
        }
        (target, Some(previous), updated.into_bytes())
    } else {
        let mut content = String::from("[keys]\nprefix = \"ctrl+a\"\n\n");
        content.push_str(MISSION_BINDING);
        (config_path.to_path_buf(), None, content.into_bytes())
    };

    let generated = std::str::from_utf8(&updated).map_err(io::Error::other)?;
    generated.parse::<toml::Value>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("generated Herdr config is not valid TOML: {error}"),
        )
    })?;
    write_atomic(&target, previous.as_deref(), &updated)
}

fn validate_keys_shape(config: &toml::Value) -> io::Result<()> {
    let Some(keys) = config.get("keys") else {
        return Ok(());
    };
    let keys = keys
        .as_table()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "keys must be a TOML table"))?;
    if let Some(prefix) = keys.get("prefix") {
        let prefix = prefix.as_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "keys.prefix must be a string")
        })?;
        if prefix.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "keys.prefix must not be empty",
            ));
        }
    }
    Ok(())
}

fn append_mission_binding(content: &mut String, config: &toml::Value) -> io::Result<()> {
    if let Some((open, close)) = find_root_inline_keys_range(content) {
        let keys = config
            .get("keys")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "keys must be a TOML table")
            })?;
        let (mut insertion_at, insertion) = if keys.contains_key("command") {
            let inline_keys = &content[open..=close];
            let (array_open, array_close) = find_inline_array_field(inline_keys, "command")
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "inline keys.command array could not be located safely",
                    )
                })?;
            let separator = if inline_keys[array_open + 1..array_close].trim().is_empty() {
                ""
            } else {
                ", "
            };
            (
                open + array_close,
                format!("{separator}{MISSION_INLINE_COMMAND_ENTRY}"),
            )
        } else {
            let field = format!("command = [{MISSION_INLINE_COMMAND_ENTRY}]");
            let insertion = if keys.is_empty() {
                field
            } else {
                format!(", {field}")
            };
            (close, insertion)
        };
        while content.as_bytes()[..insertion_at]
            .last()
            .is_some_and(u8::is_ascii_whitespace)
        {
            insertion_at -= 1;
        }
        content.insert_str(insertion_at, &insertion);
        return Ok(());
    }

    if !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.ends_with("\n\n") {
        content.push('\n');
    }
    content.push_str(MISSION_BINDING);
    Ok(())
}

fn find_root_inline_keys_range(content: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    for raw_line in content.split_inclusive('\n') {
        let line = raw_line.trim_start();
        if line.starts_with('[') {
            return None;
        }
        let Some(key_end) = toml_key_token_end(line, 0) else {
            offset += raw_line.len();
            continue;
        };
        if !toml_key_token_matches(&line[..key_end], "keys") {
            offset += raw_line.len();
            continue;
        }
        let after_key = &line[key_end..];
        let Some(after_equals) = after_key.trim_start().strip_prefix('=') else {
            offset += raw_line.len();
            continue;
        };
        let value = after_equals.trim_start();
        if !value.starts_with('{') {
            offset += raw_line.len();
            continue;
        }
        let value_offset = raw_line.len() - line.len() + line.len() - value.len();
        let open = offset + value_offset;
        return matching_inline_table_close(value).map(|close| (open, open + close));
    }
    None
}

fn find_inline_array_field(value: &str, expected: &str) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    let mut index = 1;
    let mut brace_depth = 1_usize;
    let mut bracket_depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut field_start = true;

    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active_quote) = quote {
            if active_quote == b'"' && !escaped && byte == b'\\' {
                escaped = true;
                index += 1;
                continue;
            }
            if !escaped && byte == active_quote {
                quote = None;
            }
            escaped = false;
            index += 1;
            continue;
        }

        if brace_depth == 1 && bracket_depth == 0 && field_start {
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if let Some(key_end) = toml_key_token_end(value, index)
                .filter(|key_end| toml_key_token_matches(&value[index..*key_end], expected))
            {
                let mut equals = key_end;
                while bytes.get(equals).is_some_and(u8::is_ascii_whitespace) {
                    equals += 1;
                }
                if bytes.get(equals) == Some(&b'=') {
                    equals += 1;
                    while bytes.get(equals).is_some_and(u8::is_ascii_whitespace) {
                        equals += 1;
                    }
                    if bytes.get(equals) == Some(&b'[') {
                        let close = matching_array_close(&value[equals..])?;
                        return Some((equals, equals + close));
                    }
                }
            }
            field_start = false;
            continue;
        }

        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.checked_sub(1)?,
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.checked_sub(1)?,
            b',' if brace_depth == 1 && bracket_depth == 0 => field_start = true,
            _ => {}
        }
        index += 1;
    }
    None
}

fn toml_key_token_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let first = *bytes.get(start)?;
    match first {
        b'"' => {
            let mut index = start + 1;
            let mut escaped = false;
            while let Some(byte) = bytes.get(index).copied() {
                if !escaped && byte == b'"' {
                    return Some(index + 1);
                }
                escaped = !escaped && byte == b'\\';
                index += 1;
            }
            None
        }
        b'\'' => bytes[start + 1..]
            .iter()
            .position(|byte| *byte == b'\'')
            .map(|index| start + index + 2),
        byte if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') => {
            let mut index = start + 1;
            while bytes
                .get(index)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                index += 1;
            }
            Some(index)
        }
        _ => None,
    }
}

fn toml_key_token_matches(token: &str, expected: &str) -> bool {
    format!("{token} = 0")
        .parse::<toml::Value>()
        .ok()
        .is_some_and(|value| value.get(expected).is_some())
}

fn matching_array_close(value: &str) -> Option<usize> {
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if let Some(active_quote) = quote {
            if active_quote == b'"' && !escaped && byte == b'\\' {
                escaped = true;
                continue;
            }
            if !escaped && byte == active_quote {
                quote = None;
            }
            escaped = false;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'[' => depth += 1,
            b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn matching_inline_table_close(value: &str) -> Option<usize> {
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if let Some(active_quote) = quote {
            if active_quote == '"' && !escaped && character == '\\' {
                escaped = true;
                continue;
            }
            if !escaped && character == active_quote {
                quote = None;
            }
            escaped = false;
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MissionBindingState {
    Missing,
    Installed,
    Conflict,
}

#[derive(Deserialize)]
struct SpannedConfig {
    keys: Option<SpannedKeys>,
}

#[derive(Deserialize)]
struct SpannedKeys {
    #[serde(default)]
    command: Vec<SpannedCommand>,
}

#[derive(Deserialize)]
struct SpannedCommand {
    key: Option<toml::Spanned<toml::Value>>,
    #[serde(rename = "type")]
    kind: Option<toml::Spanned<String>>,
    command: Option<toml::Spanned<String>>,
    description: Option<toml::Spanned<String>>,
}

fn migrate_legacy_mission_binding(
    content: &str,
    config: &toml::Value,
) -> io::Result<Option<String>> {
    if config
        .get("keys")
        .and_then(toml::Value::as_table)
        .is_some_and(|keys| {
            keys.iter().any(|(name, value)| {
                !matches!(name.as_str(), "prefix" | "command") && value_uses_prefix_m(value)
            })
        })
    {
        return Ok(None);
    }

    let spanned = toml::from_str::<SpannedConfig>(content).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Herdr config keybindings could not be inspected safely: {error}"),
        )
    })?;
    let Some(keys) = spanned.keys else {
        return Ok(None);
    };
    let mut prefix_bindings = keys.command.iter().filter(|binding| {
        binding.key.as_ref().and_then(|key| key.get_ref().as_str()) == Some("prefix+m")
    });
    let Some(legacy) = prefix_bindings.next() else {
        return Ok(None);
    };
    if prefix_bindings.next().is_some()
        || legacy
            .kind
            .as_ref()
            .map(toml::Spanned::get_ref)
            .map(String::as_str)
            != Some("plugin_action")
        || !legacy
            .command
            .as_ref()
            .is_some_and(|command| LEGACY_MISSION_COMMANDS.contains(&command.get_ref().as_str()))
    {
        return Ok(None);
    }

    let mut replacements = vec![
        (
            legacy.kind.as_ref().unwrap().span(),
            String::from("\"shell\""),
        ),
        (
            legacy.command.as_ref().unwrap().span(),
            format!("\"{MISSION_COMMAND}\""),
        ),
    ];
    if legacy.description.as_ref().is_some_and(|description| {
        matches!(
            description.get_ref().as_str(),
            "打开 Mission 控制中心" | "新建 Team Mission"
        )
    }) {
        replacements.push((
            legacy.description.as_ref().unwrap().span(),
            String::from("\"打开 Mission 看板\""),
        ));
    }
    replacements.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));

    let mut updated = content.to_owned();
    for (range, replacement) in replacements {
        updated.replace_range(range, &replacement);
    }
    let reparsed = updated.parse::<toml::Value>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("migrated Herdr config is not valid TOML: {error}"),
        )
    })?;
    if mission_binding_state(&reparsed) != MissionBindingState::Installed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "migrated Herdr Mission keybinding could not be verified",
        ));
    }
    Ok(Some(updated))
}

fn mission_binding_state(config: &toml::Value) -> MissionBindingState {
    if config
        .get("keys")
        .and_then(toml::Value::as_table)
        .is_some_and(|keys| {
            keys.iter().any(|(name, value)| {
                !matches!(name.as_str(), "prefix" | "command") && value_uses_prefix_m(value)
            })
        })
    {
        return MissionBindingState::Conflict;
    }
    let Some(commands) = config
        .get("keys")
        .and_then(|keys| keys.get("command"))
        .and_then(toml::Value::as_array)
    else {
        return MissionBindingState::Missing;
    };
    let mut installed = false;
    for command in commands {
        if !binding_uses_prefix_m(command.get("key")) {
            continue;
        }
        let exact = command.get("command").and_then(toml::Value::as_str) == Some(MISSION_COMMAND)
            && command
                .get("type")
                .and_then(toml::Value::as_str)
                .is_none_or(|kind| kind == "shell");
        if !exact {
            return MissionBindingState::Conflict;
        }
        installed = true;
    }
    if installed {
        MissionBindingState::Installed
    } else {
        MissionBindingState::Missing
    }
}

fn value_uses_prefix_m(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(value) => value == "prefix+m",
        toml::Value::Array(values) => values.iter().any(value_uses_prefix_m),
        toml::Value::Table(values) => values.values().any(value_uses_prefix_m),
        _ => false,
    }
}

fn binding_uses_prefix_m(value: Option<&toml::Value>) -> bool {
    match value {
        Some(toml::Value::String(value)) => value == "prefix+m",
        Some(toml::Value::Array(values)) => values
            .iter()
            .any(|value| value.as_str() == Some("prefix+m")),
        _ => false,
    }
}

fn write_atomic(target: &Path, previous: Option<&[u8]>, updated: &[u8]) -> io::Result<()> {
    let parent = match target.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Herdr config target has no parent directory",
            ));
        }
    };
    let (temp_path, mut file) = create_temporary_file(target)?;
    let result = (|| {
        if let Ok(metadata) = fs::metadata(target) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(updated)?;
        file.sync_all()?;

        match previous {
            Some(expected) if fs::read(target)? != expected => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Herdr config changed while installing the Mission keybinding",
                ));
            }
            None if target.exists() => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Herdr config was created while installing the Mission keybinding",
                ));
            }
            _ => {}
        }

        fs::rename(&temp_path, target)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn create_temporary_file(config_path: &Path) -> io::Result<(PathBuf, fs::File)> {
    let name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    for sequence in 0..32 {
        let path = config_path.with_file_name(format!(
            ".{name}.herdr-mission-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary Herdr config file",
    ))
}
