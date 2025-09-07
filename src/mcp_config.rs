use std::{
	collections::HashMap,
	path::Path,
};

use miette::{
	IntoDiagnostic,
	Result,
	WrapErr,
};
use serde::{
	Deserialize,
	Serialize,
};
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
	pub servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpServerConfig {
	#[serde(rename = "http")]
	Http {
		url: String,
		#[serde(default)]
		headers: HashMap<String, String>,
	},
	#[serde(rename = "sse")]
	Sse {
		url: String,
		#[serde(default)]
		headers: HashMap<String, String>,
	},
	#[serde(rename = "stdio")]
	Stdio {
		command: String,
		args: Option<Vec<String>>,
		#[serde(default)]
		env: HashMap<String, String>,
	},
}

impl McpConfig {
	/// Load MCP configuration from a file
	pub async fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
		let content = fs::read_to_string(path.as_ref())
			.await
			.into_diagnostic()
			.wrap_err_with(|| format!("Failed to read MCP config file: {}", path.as_ref().display()))?;

		let config: McpConfig = serde_json::from_str(&content)
			.into_diagnostic()
			.wrap_err("Failed to parse MCP config JSON")?;

		Ok(config)
	}

	/// Load MCP configuration from default locations
	pub async fn load_default() -> Result<Option<Self>> {
		// Try workspace-specific config first
		if Path::new(".vscode/mcp.json").exists() {
			return Ok(Some(Self::from_file(".vscode/mcp.json").await?));
		}

		// Try root mcp.json
		if Path::new("mcp.json").exists() {
			return Ok(Some(Self::from_file("mcp.json").await?));
		}

		Ok(None)
	}
}

impl McpServerConfig {
	pub fn get_connection_url(&self) -> Option<&str> {
		match self {
			McpServerConfig::Http {
				url, ..
			} => Some(url),
			McpServerConfig::Sse {
				url, ..
			} => Some(url),
			McpServerConfig::Stdio {
				..
			} => None, // stdio connections don't use URLs
		}
	}

	pub fn is_http_based(&self) -> bool {
		matches!(self, McpServerConfig::Http { .. } | McpServerConfig::Sse { .. })
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use tempfile::NamedTempFile;
	use tokio::fs;

	use super::*;

	/// Test parsing a basic HTTP server configuration
	#[tokio::test]
	async fn test_parse_http_server_config() {
		let json = r#"
        {
            "servers": {
                "web-search": {
                    "type": "http",
                    "url": "http://192.168.200.10:8096/servers/web-search/sse"
                }
            }
        }
        "#;

		let config: McpConfig = serde_json::from_str(json).expect("Failed to parse config");

		assert_eq!(config.servers.len(), 1);

		let server = config.servers.get("web-search").expect("web-search server not found");
		match server {
			McpServerConfig::Http {
				url,
				headers,
			} => {
				assert_eq!(url, "http://192.168.200.10:8096/servers/web-search/sse");
				assert!(headers.is_empty());
			},
			_ => panic!("Expected HTTP server config"),
		}

		assert!(server.is_http_based());
		assert_eq!(
			server.get_connection_url(),
			Some("http://192.168.200.10:8096/servers/web-search/sse")
		);
	}

	/// Test parsing an SSE server configuration
	#[tokio::test]
	async fn test_parse_sse_server_config() {
		let json = r#"
        {
            "servers": {
                "web-fetch": {
                    "type": "sse",
                    "url": "http://192.168.200.10:8096/servers/fetch/sse",
                    "headers": {
                        "Authorization": "Bearer token123",
                        "User-Agent": "MCP-Client/1.0"
                    }
                }
            }
        }
        "#;

		let config: McpConfig = serde_json::from_str(json).expect("Failed to parse config");

		let server = config.servers.get("web-fetch").expect("web-fetch server not found");
		match server {
			McpServerConfig::Sse {
				url,
				headers,
			} => {
				assert_eq!(url, "http://192.168.200.10:8096/servers/fetch/sse");
				assert_eq!(headers.len(), 2);
				assert_eq!(headers.get("Authorization"), Some(&"Bearer token123".to_string()));
				assert_eq!(headers.get("User-Agent"), Some(&"MCP-Client/1.0".to_string()));
			},
			_ => panic!("Expected SSE server config"),
		}

		assert!(server.is_http_based());
	}

	/// Test parsing a stdio server configuration
	#[tokio::test]
	async fn test_parse_stdio_server_config() {
		let json = r#"
        {
            "servers": {
                "local-tool": {
                    "type": "stdio",
                    "command": "/usr/local/bin/mcp-server",
                    "args": ["--config", "/etc/mcp/config.json"],
                    "env": {
                        "MCP_DEBUG": "1",
                        "PATH": "/usr/local/bin:/usr/bin"
                    }
                }
            }
        }
        "#;

		let config: McpConfig = serde_json::from_str(json).expect("Failed to parse config");

		let server = config.servers.get("local-tool").expect("local-tool server not found");
		match server {
			McpServerConfig::Stdio {
				command,
				args,
				env,
			} => {
				assert_eq!(command, "/usr/local/bin/mcp-server");
				assert_eq!(args.as_ref().unwrap().len(), 2);
				assert_eq!(args.as_ref().unwrap()[0], "--config");
				assert_eq!(args.as_ref().unwrap()[1], "/etc/mcp/config.json");
				assert_eq!(env.len(), 2);
				assert_eq!(env.get("MCP_DEBUG"), Some(&"1".to_string()));
			},
			_ => panic!("Expected Stdio server config"),
		}

		assert!(!server.is_http_based());
		assert_eq!(server.get_connection_url(), None);
	}

	/// Test parsing multiple servers with mixed types
	#[tokio::test]
	async fn test_parse_mixed_server_types() {
		let json = r#"
        {
            "servers": {
                "web-search": {
                    "type": "http",
                    "url": "http://192.168.200.10:8096/servers/web-search/sse"
                },
                "web-fetch": {
                    "type": "sse",
                    "url": "http://192.168.200.10:8096/servers/fetch/sse"
                },
                "local-tool": {
                    "type": "stdio",
                    "command": "python",
                    "args": ["-m", "mcp_server"]
                }
            }
        }
        "#;

		let config: McpConfig = serde_json::from_str(json).expect("Failed to parse config");

		assert_eq!(config.servers.len(), 3);
		assert!(config.servers.contains_key("web-search"));
		assert!(config.servers.contains_key("web-fetch"));
		assert!(config.servers.contains_key("local-tool"));

		let http_count = config.servers.values().filter(|s| s.is_http_based()).count();
		let stdio_count = config.servers.values().filter(|s| !s.is_http_based()).count();

		assert_eq!(http_count, 2);
		assert_eq!(stdio_count, 1);
	}

	/// Test parsing config similar to the provided mcp.json
	#[tokio::test]
	async fn test_parse_provided_config_format() {
		let json = r#"
        {
            "servers": {
                "web-search": {
                    "url": "http://192.168.200.10:8096/servers/web-search/sse",
                    "type": "http"
                },
                "web-fetch": {
                    "url": "http://192.168.200.10:8096/servers/fetch/sse",
                    "type": "http"
                }
            }
        }
        "#;

		let config: McpConfig = serde_json::from_str(json).expect("Failed to parse config");

		assert_eq!(config.servers.len(), 2);

		for (name, server) in &config.servers {
			match server {
				McpServerConfig::Http {
					url,
					headers,
				} => {
					assert!(url.starts_with("http://192.168.200.10:8096/servers/"));
					assert!(headers.is_empty());
					if name == "web-search" {
						assert!(url.contains("web-search"));
					} else if name == "web-fetch" {
						assert!(url.contains("fetch"));
					}
				},
				_ => panic!("Expected HTTP server config for {}", name),
			}
		}
	}

	/// Test stdio config with minimal fields
	#[tokio::test]
	async fn test_parse_stdio_minimal() {
		let json = r#"
        {
            "servers": {
                "simple-tool": {
                    "type": "stdio",
                    "command": "node server.js"
                }
            }
        }
        "#;

		let config: McpConfig = serde_json::from_str(json).expect("Failed to parse config");

		let server = config.servers.get("simple-tool").expect("simple-tool server not found");
		match server {
			McpServerConfig::Stdio {
				command,
				args,
				env,
			} => {
				assert_eq!(command, "node server.js");
				assert!(args.is_none());
				assert!(env.is_empty());
			},
			_ => panic!("Expected Stdio server config"),
		}
	}

	/// Test loading config from file
	#[tokio::test]
	async fn test_load_from_file() {
		let json = r#"
        {
            "servers": {
                "test-server": {
                    "type": "http",
                    "url": "http://localhost:8080"
                }
            }
        }
        "#;

		let temp_file = NamedTempFile::new().expect("Failed to create temp file");
		fs::write(temp_file.path(), json).await.expect("Failed to write temp file");

		let config = McpConfig::from_file(temp_file.path()).await.expect("Failed to load config");

		assert_eq!(config.servers.len(), 1);
		assert!(config.servers.contains_key("test-server"));
	}

	/// Test error handling for invalid JSON
	#[tokio::test]
	async fn test_invalid_json() {
		let invalid_json = r#"
        {
            "servers": {
                "broken": {
                    "type": "http"
                    // missing comma and url
                }
            }
        }
        "#;

		let result: Result<McpConfig, _> = serde_json::from_str(invalid_json);
		assert!(result.is_err());
	}

	/// Test error handling for missing required fields
	#[tokio::test]
	async fn test_missing_required_fields() {
		let json_missing_url = r#"
        {
            "servers": {
                "broken-http": {
                    "type": "http"
                }
            }
        }
        "#;

		let result: Result<McpConfig, _> = serde_json::from_str(json_missing_url);
		assert!(result.is_err());

		let json_missing_command = r#"
        {
            "servers": {
                "broken-stdio": {
                    "type": "stdio"
                }
            }
        }
        "#;

		let result: Result<McpConfig, _> = serde_json::from_str(json_missing_command);
		assert!(result.is_err());
	}

	/// Test error handling for unknown server type
	#[tokio::test]
	async fn test_unknown_server_type() {
		let json = r#"
        {
            "servers": {
                "unknown": {
                    "type": "websocket",
                    "url": "ws://localhost:8080"
                }
            }
        }
        "#;

		let result: Result<McpConfig, _> = serde_json::from_str(json);
		assert!(result.is_err());
	}

	/// Test empty servers object
	#[tokio::test]
	async fn test_empty_servers() {
		let json = r#"
        {
            "servers": {}
        }
        "#;

		let config: McpConfig = serde_json::from_str(json).expect("Failed to parse config");
		assert_eq!(config.servers.len(), 0);
	}

	/// Test load_default with no config files present
	#[tokio::test]
	async fn test_load_default_no_files() {
		// This test assumes we're in a directory without .vscode/mcp.json or mcp.json
		let result = McpConfig::load_default().await.expect("load_default should not fail");
		// Should return None when no config files are found
		assert!(result.is_none());
	}

	/// Test serialization round-trip
	#[tokio::test]
	async fn test_serialization_round_trip() {
		let mut servers = HashMap::new();

		let mut headers = HashMap::new();
		headers.insert("Authorization".to_string(), "Bearer test".to_string());

		servers.insert("http-server".to_string(), McpServerConfig::Http {
			url: "http://example.com".to_string(),
			headers,
		});

		let mut env = HashMap::new();
		env.insert("DEBUG".to_string(), "1".to_string());

		servers.insert("stdio-server".to_string(), McpServerConfig::Stdio {
			command: "python".to_string(),
			args: Some(vec!["-m".to_string(), "server".to_string()]),
			env,
		});

		let original_config = McpConfig {
			servers,
		};

		let json = serde_json::to_string(&original_config).expect("Failed to serialize");
		let parsed_config: McpConfig = serde_json::from_str(&json).expect("Failed to deserialize");

		assert_eq!(original_config.servers.len(), parsed_config.servers.len());

		for (name, server) in &original_config.servers {
			let parsed_server = parsed_config.servers.get(name).expect("Server not found after round-trip");

			match (server, parsed_server) {
				(
					McpServerConfig::Http {
						url: url1,
						headers: headers1,
					},
					McpServerConfig::Http {
						url: url2,
						headers: headers2,
					},
				) => {
					assert_eq!(url1, url2);
					assert_eq!(headers1, headers2);
				},
				(
					McpServerConfig::Stdio {
						command: cmd1,
						args: args1,
						env: env1,
					},
					McpServerConfig::Stdio {
						command: cmd2,
						args: args2,
						env: env2,
					},
				) => {
					assert_eq!(cmd1, cmd2);
					assert_eq!(args1, args2);
					assert_eq!(env1, env2);
				},
				_ => panic!("Server type mismatch after round-trip for {}", name),
			}
		}
	}
}
