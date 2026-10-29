//! User-scoped MCP configuration. Never runs inside the elevated installer.
use anyhow::{Context, Result, ensure};
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde::Serialize;
use std::{
    fs,
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

#[derive(Clone, Serialize)]
pub(super) struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    pub path: PathBuf,
    pub detected: bool,
}

#[derive(Clone, Serialize)]
pub(super) struct Outcome {
    pub name: String,
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
    pub error: Option<String>,
}

pub(super) fn catalog() -> Result<Vec<Agent>> {
    let dirs = directories::BaseDirs::new().context("无法确定用户目录。")?;
    let home = dirs.home_dir();
    let env_path = |name: &str, fallback: PathBuf| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or(fallback)
    };
    let codex = env_path("CODEX_HOME", home.join(".codex"));
    let devin = if cfg!(windows) {
        dirs.config_dir().join("devin")
    } else {
        env_path("XDG_CONFIG_HOME", home.join(".config")).join("devin")
    };
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(|p| PathBuf::from(p).join(".claude.json"))
        .unwrap_or_else(|| home.join(".claude.json"));
    let entries = vec![
        ("codex", "Codex", codex.join("config.toml")),
        ("claude_code", "Claude Code", claude),
        (
            "claude_desktop",
            "Claude Desktop",
            dirs.config_dir().join("Claude/claude_desktop_config.json"),
        ),
        ("cursor", "Cursor", home.join(".cursor/mcp.json")),
        (
            "windsurf",
            "Windsurf",
            home.join(".codeium/windsurf/mcp_config.json"),
        ),
        (
            "vscode",
            "VS Code",
            dirs.config_dir().join("Code/User/mcp.json"),
        ),
        ("gemini", "Gemini CLI", home.join(".gemini/settings.json")),
        ("devin", "Devin Desktop", devin.join("mcp_config.json")),
    ];
    Ok(entries
        .into_iter()
        .map(|(id, name, path)| Agent {
            id,
            name,
            detected: path.is_file(),
            path,
        })
        .collect())
}

pub(super) fn validate(ids: &[String], agents: &[Agent]) -> Result<()> {
    ensure!(ids.len() <= agents.len(), "客户端选择过多。");
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        ensure!(
            agents.iter().any(|a| a.id == id) && seen.insert(id),
            "无效或重复的客户端选择。"
        );
    }
    Ok(())
}

pub(super) fn configure(
    agents: &[Agent],
    ids: &[String],
    command: &Path,
    endpoint: SocketAddr,
    replace: bool,
) -> Vec<Outcome> {
    agents
        .iter()
        .filter(|agent| ids.iter().any(|id| id == agent.id))
        .map(|agent| {
            let result = write_config(agent, command, endpoint, replace);
            Outcome {
                name: agent.name.into(),
                path: agent.path.clone(),
                backup: result.as_ref().ok().cloned().flatten(),
                // Parser diagnostics can contain existing secrets. Only display our
                // outer, deliberately content-free error message.
                error: result.err().map(|error| error.to_string()),
            }
        })
        .collect()
}

fn read(path: &Path) -> Result<Option<Zeroizing<String>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("无法读取配置文件。"),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "配置路径不是普通文件，请手动配置。"
    );
    let mut value = Zeroizing::new(String::new());
    fs::File::open(path)?
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut value)
        .context("配置必须为 UTF-8 文本。")?;
    ensure!(value.len() <= 2 * 1024 * 1024, "配置文件过大，请手动配置。");
    Ok(Some(value))
}

fn json_root(source: &str) -> Result<CstRootNode> {
    CstRootNode::parse(
        source,
        &jsonc_parser::ParseOptions {
            allow_comments: true,
            allow_trailing_commas: true,
            allow_loose_object_property_names: false,
            allow_missing_commas: false,
            allow_single_quoted_strings: false,
            allow_hexadecimal_numbers: false,
            allow_unary_plus_numbers: false,
            allow_bare_decimal_point_numbers: false,
            allow_non_finite_numbers: false,
            allow_extended_string_escapes: false,
        },
    )
    .context("JSON 配置格式无效，原文件未改动。")
}

fn unique(object: &CstObject) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for property in object.properties() {
        let name = property
            .name()
            .context("配置键缺失。")?
            .decoded_value()
            .context("配置键无效。")?;
        ensure!(names.insert(name), "配置包含重复键，请先手动修正。");
    }
    Ok(())
}

fn render(
    source: &str,
    id: &str,
    command: &Path,
    endpoint: SocketAddr,
    replace: bool,
) -> Result<String> {
    let command = command.to_str().context("程序路径不是 UTF-8。")?;
    let args = ["--endpoint", &endpoint.to_string()];
    // Preserve BOM, comments and unrelated settings instead of round-tripping
    // the entire user's file through a generic JSON/TOML serializer.
    let (bom, source) = source
        .strip_prefix('\u{feff}')
        .map_or(("", source), |s| ("\u{feff}", s));
    let result = if id == "codex" {
        let mut doc = source
            .parse::<toml_edit::DocumentMut>()
            .context("TOML 配置格式无效，原文件未改动。")?;
        if !doc.contains_key("mcp_servers") {
            doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let servers = doc["mcp_servers"]
            .as_table_like_mut()
            .context("mcp_servers 必须是表。")?;
        let mut server = toml_edit::Table::new();
        server["command"] = toml_edit::value(command);
        server["args"] = toml_edit::value(args.iter().copied().collect::<toml_edit::Array>());
        server["startup_timeout_sec"] = toml_edit::value(30);
        server["tool_timeout_sec"] = toml_edit::value(3600);
        ensure!(
            replace || !servers.contains_key("trustd"),
            "已有 trustd 配置；勾选替换后重试。"
        );
        servers.insert("trustd", toml_edit::Item::Table(server));
        doc.to_string()
    } else {
        let root = json_root(if source.trim().is_empty() {
            "{}"
        } else {
            source
        })?;
        let object = root.object_value().context("JSON 配置必须是对象。")?;
        unique(&object)?;
        let key = if id == "vscode" {
            "servers"
        } else {
            "mcpServers"
        };
        let servers = object
            .object_value_or_create(key)
            .context("MCP 配置必须是对象。")?;
        unique(&servers)?;
        let mut fields = vec![
            ("command".into(), CstInputValue::String(command.into())),
            (
                "args".into(),
                CstInputValue::Array(
                    args.iter()
                        .map(|s| CstInputValue::String((*s).into()))
                        .collect(),
                ),
            ),
        ];
        if matches!(id, "vscode" | "claude_code") {
            fields.push(("type".into(), "stdio".into()));
        }
        if id == "gemini" {
            fields.push(("timeout".into(), CstInputValue::Number("3600000".into())));
        }
        let value = CstInputValue::Object(fields);
        if let Some(existing) = servers.get("trustd") {
            ensure!(replace, "已有 trustd 配置；勾选替换后重试。");
            existing.set_value(value);
        } else {
            servers.append("trustd", value);
        }
        root.to_string()
    };
    Ok(format!("{bom}{result}"))
}

fn write_config(
    agent: &Agent,
    command: &Path,
    endpoint: SocketAddr,
    replace: bool,
) -> Result<Option<PathBuf>> {
    ensure!(
        agent.path.is_absolute() && command.is_absolute() && command.is_file(),
        "配置或程序路径不可用。"
    );
    let original = read(&agent.path)?;
    let updated = Zeroizing::new(render(
        original.as_ref().map_or("", |s| s.as_str()),
        agent.id,
        command,
        endpoint,
        replace,
    )?);
    if original
        .as_ref()
        .is_some_and(|s| s.as_str() == updated.as_str())
    {
        return Ok(None);
    }
    let parent = agent.path.parent().context("配置目录缺失。")?;
    fs::create_dir_all(parent).context("无法创建客户端配置目录。")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context("无法创建临时配置。")?;
    super::platform::private(temporary.path(), false)?;
    temporary.write_all(updated.as_bytes())?;
    temporary.as_file().sync_all()?;
    let backup = if let Some(original) = &original {
        let mut backup = tempfile::Builder::new()
            .prefix(&format!(
                "{}.trustd-backup-",
                agent.path.file_name().unwrap().to_string_lossy()
            ))
            .tempfile_in(parent)?;
        super::platform::private(backup.path(), false)?;
        backup.write_all(original.as_bytes())?;
        backup.as_file().sync_all()?;
        Some(backup.keep()?.1)
    } else {
        None
    };
    ensure!(
        read(&agent.path)? == original,
        "客户端配置已变化，请关闭客户端后重试。"
    );
    if original.is_some() {
        temporary
            .persist(&agent.path)
            .context("无法保存配置，请关闭客户端后重试。")?;
    } else {
        temporary
            .persist_noclobber(&agent.path)
            .context("配置已被创建，请刷新后重试。")?;
    }
    Ok(backup)
}

#[cfg(test)]
mod tests;
