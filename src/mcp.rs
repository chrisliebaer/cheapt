use std::{
	collections::HashMap,
	ops::Deref,
	process::Stdio,
	str::FromStr,
};

use llm::{
	ToolCall,
	builder::FunctionBuilder,
};
use log::{
	debug,
	trace,
};
use miette::{
	IntoDiagnostic,
	Result,
	WrapErr,
};
use reqwest::Client;
use rmcp::{
	RoleClient,
	ServiceError,
	ServiceExt,
	model::{
		CallToolRequestParam,
		CallToolResult,
		ClientInfo,
		Content,
		Implementation,
		InitializeRequestParam,
		ListToolsResult,
	},
	service::RunningService,
	transport::{
		ConfigureCommandExt,
		SseClientTransport,
		StreamableHttpClientTransport,
		TokioChildProcess,
		sse_client::SseClientConfig,
		streamable_http_client::StreamableHttpClientTransportConfig,
	},
};
use serde_json::Value;
use tokio::process::Command;
use tracing::info;

use crate::mcp_config::{
	McpConfig,
	McpServerConfig,
};

/// Convert a ServiceError into a descriptive error string
/// Extracts detailed information especially from the McpError variant
fn service_error_to_description(err: &ServiceError) -> String {
	match err {
		ServiceError::McpError(mcp_error) => {
			let mut description = format!("MCP Error ({}): {}", mcp_error.code.0, mcp_error.message);

			// Add human-readable error type based on error code
			let error_type = match mcp_error.code.0 {
				-32002 => "Resource Not Found",
				-32600 => "Invalid Request",
				-32601 => "Method Not Found",
				-32602 => "Invalid Parameters",
				-32603 => "Internal Error",
				-32700 => "Parse Error",
				_ => "Unknown Error",
			};

			description = format!("{} ({})", error_type, description);

			// Include additional data if present
			if let Some(data) = &mcp_error.data {
				// Try to extract meaningful information from the data field
				if let Some(details) = data.as_str() {
					description.push_str(&format!(" - {}", details));
				} else if data.is_object() || data.is_array() {
					description.push_str(&format!(" - Additional info: {}", data));
				}
			}

			description
		},
		ServiceError::TransportSend(transport_err) => {
			format!("Transport Send Error: {}", transport_err)
		},
		ServiceError::TransportClosed => "Transport connection closed".to_string(),
		ServiceError::UnexpectedResponse => "Received unexpected response type".to_string(),
		ServiceError::Cancelled {
			reason,
		} => match reason {
			Some(reason) => format!("Request cancelled: {}", reason),
			None => "Request cancelled for unknown reason".to_string(),
		},
		ServiceError::Timeout {
			timeout,
		} => {
			format!("Request timed out after {:?}", timeout)
		},
		_ => format!("Service Error: {}", err),
	}
}

/// Extract text content from MCP Content array
/// Concatenates all text content found in the array
fn extract_text_from_content(content: &[Content]) -> String {
	let mut result = String::new();

	for item in content {
		// The Content type is an Annotated<RawContent>, we need to access the inner value
		match item.deref() {
			rmcp::model::RawContent::Text(text_content) => {
				if !result.is_empty() {
					result.push('\n');
				}
				result.push_str(&text_content.text);
			},
			// these should not occur for now
			_ => unimplemented!("Extracting non-text content is not implemented"),
		}
	}

	result
}

/// Create a reqwest HTTP client with the provided headers
/// Common functionality for both SSE and StreamableHttp transports
fn create_http_client_with_headers(headers: &HashMap<String, String>) -> Result<Client> {
	let mut client_builder = Client::builder();

	let mut header_map = reqwest::header::HeaderMap::new();
	for (key, value) in headers {
		if let (Ok(name), Ok(val)) = (
			reqwest::header::HeaderName::from_str(key),
			reqwest::header::HeaderValue::from_str(value),
		) {
			header_map.insert(name, val);
		}
	}
	client_builder = client_builder.default_headers(header_map);

	client_builder
		.build()
		.into_diagnostic()
		.wrap_err("Failed to build reqwest client")
}

/// Common functionality for initializing an MCP client and fetching tools
async fn initialize_mcp_client(
	client: RunningService<RoleClient, InitializeRequestParam>,
	server_name: &str,
) -> Result<McpClientWithTools> {
	McpClientWithTools::new(client)
		.await
		.wrap_err(format!("Failed to fetch tools from MCP server '{}'", server_name))
}

/// Struct that combines an MCP client with its cached tools
pub struct McpClientWithTools {
	client: RunningService<RoleClient, InitializeRequestParam>,
	tools: ListToolsResult,
}

impl McpClientWithTools {
	/// Create a new McpClientWithTools by fetching tools from the client
	async fn new(client: RunningService<RoleClient, InitializeRequestParam>) -> Result<Self> {
		let tools = client
			.list_tools(None)
			.await
			.into_diagnostic()
			.wrap_err("Failed to fetch tools from MCP client")?;

		Ok(McpClientWithTools {
			client,
			tools,
		})
	}

	/// Get a reference to the client
	pub fn client(&self) -> &RunningService<RoleClient, InitializeRequestParam> {
		&self.client
	}

	/// Get a reference to the cached tools
	pub fn tools(&self) -> &ListToolsResult {
		&self.tools
	}
}

/// RAII guard that maintains MCP connections during an LLM session.
pub struct McpConnection {
	clients: HashMap<String, McpClientWithTools>,
}

/// Factory for creating MCP connections from configuration.
/// Holds the configuration but doesn't maintain persistent connections.
pub struct McpManager {
	config: McpConfig,
}

impl McpConnection {
	/// Create a new MCP connection session by connecting to all configured servers
	/// This establishes fresh connections for this session
	pub async fn new(config: &McpConfig) -> Result<Self> {
		let mut clients = HashMap::new();

		// init client info which we need to pass to all servers to introduce ourselves
		let client_info = ClientInfo {
			protocol_version: Default::default(),
			capabilities: Default::default(),
			client_info: Implementation {
				name: env!("CARGO_PKG_NAME").to_string(),
				version: env!("CARGO_PKG_VERSION").to_string(),
			},
		};

		for (server_name, server_config) in &config.servers {
			match server_config {
				McpServerConfig::Http {
					url,
					headers,
				} => {
					info!("Connecting to HTTP MCP server '{}' at {}", server_name, url);

					let http_client = create_http_client_with_headers(headers)
						.wrap_err(format!("Failed to build reqwest client for MCP server '{}'", server_name))?;

					let transport_config = StreamableHttpClientTransportConfig {
						uri: url.clone().into(),
						..Default::default()
					};

					let transport = StreamableHttpClientTransport::with_client(http_client, transport_config);
					let client = client_info
						.clone()
						.serve(transport)
						.await
						.into_diagnostic()
						.wrap_err(format!("Failed to initialize MCP client for server '{}'", server_name))?;

					let client_with_tools = initialize_mcp_client(client, server_name).await?;
					clients.insert(server_name.clone(), client_with_tools);
				},
				McpServerConfig::Sse {
					url,
					headers,
				} => {
					info!("Connecting to SSE MCP server '{}' at {}", server_name, url);

					let http_client = create_http_client_with_headers(headers)
						.wrap_err(format!("Failed to build reqwest client for MCP server '{}'", server_name))?;

					let transport_config = SseClientConfig {
						sse_endpoint: url.clone().into(),
						..Default::default()
					};

					let transport = SseClientTransport::start_with_client(http_client, transport_config)
						.await
						.into_diagnostic()
						.wrap_err(format!("Failed to start SSE transport for MCP server '{}'", server_name))?;

					let client = client_info
						.clone()
						.serve(transport)
						.await
						.into_diagnostic()
						.wrap_err(format!("Failed to initialize MCP client for server '{}'", server_name))?;

					let client_with_tools = initialize_mcp_client(client, server_name).await?;
					clients.insert(server_name.clone(), client_with_tools);
				},
				McpServerConfig::Stdio {
					command,
					args,
					env,
				} => {
					info!("Connecting to Stdio MCP server '{}' with command: {}", server_name, command);

					let mut cmd = Command::new(command);
					if let Some(args) = args {
						cmd.args(args);
					}
					for (key, value) in env {
						cmd.env(key, value);
					}

					// configure stdio - stdout and stdin are piped for communication, stderr inherits for debugging
					cmd = cmd.configure(|c| {
						c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
					});

					let transport = TokioChildProcess::new(cmd)
						.into_diagnostic()
						.wrap_err(format!("Failed to start child process for MCP server '{}'", server_name))?;

					let client = client_info
						.clone()
						.serve(transport)
						.await
						.into_diagnostic()
						.wrap_err(format!("Failed to initialize MCP client for server '{}'", server_name))?;

					let client_with_tools = initialize_mcp_client(client, server_name).await?;
					clients.insert(server_name.clone(), client_with_tools);
				},
			}
		}

		let connection = McpConnection {
			clients,
		};
		connection.dump_available_clients();
		Ok(connection)
	}

	/// Dump information about all connected MCP clients to the log
	/// Uses cached tools instead of fetching them again
	fn dump_available_clients(&self) {
		for (server_name, client_with_tools) in &self.clients {
			// Get peer info and use cached tools
			let peer_info = client_with_tools.client().peer_info();
			let tools = &client_with_tools.tools().tools;

			info!("Connected to MCP server '{}': {:?}", server_name, peer_info);
			if log::log_enabled!(log::Level::Debug) {
				debug!("Server '{}' provides {} tools", server_name, tools.len());

				for tool in tools {
					debug!(
						"  - Tool: {} - {}",
						tool.name,
						tool.description.as_deref().unwrap_or("No description")
					);
					trace!("    Input Schema: {:?}", tool.input_schema);
					trace!("    Output Schema: {:?}", tool.output_schema);
				}
			}
		}
	}

	/// Get all tools from all connected MCP clients and convert them to llm::chat::Tool
	/// This can be used to register all tools with an LLM that supports function calling
	pub fn get_llm_functions(&self) -> Box<[FunctionBuilder]> {
		let mut all_tools = Vec::new();

		for client_with_tools in self.clients.values() {
			let tools = &client_with_tools.tools().tools;

			// Convert rmcp::model::Tool to llm::chat::Tool
			for tool in tools {
				let json_obj = tool.input_schema.as_ref().clone();
				let mut function = FunctionBuilder::new(tool.name.as_ref()).json_schema(Value::Object(json_obj));

				if let Some(description) = &tool.description {
					function = function.description(description.as_ref());
				}

				all_tools.push(function);
			}
		}

		all_tools.into_boxed_slice()
	}

	pub async fn handle_llm_tool_call(&self, tool_call: &ToolCall) -> Option<Result<Value>> {
		let call = &tool_call.function;

		// figure out which client to use based on tool name
		let find_result = self.clients.iter().find(|(_server_name, client)| {
			let tools = &client.tools().tools;
			tools.iter().any(|tool| tool.name == call.name)
		});

		let (server_name, client_with_tools) = match find_result {
			Some((name, client)) => (name, client),
			None => {
				return Some(Err(miette::miette!("No MCP client found for tool '{}'", call.name)));
			},
		};

		let client = client_with_tools.client();

		// arguments are returned as string and need to be parsed as JSON object so tool can be called
		let arguments = match serde_json::from_str::<Value>(&call.arguments) {
			Ok(Value::Object(map)) => map,
			Ok(_) => {
				return Some(Err(miette::miette!(
					"Tool call arguments for tool '{}' are not a JSON object",
					call.name
				)));
			},
			Err(err) => {
				return Some(Err(miette::miette!(
					"Failed to parse tool call arguments for tool '{}': {}",
					call.name,
					err
				)));
			},
		};

		let result = client
			.call_tool(CallToolRequestParam {
				name: call.name.clone().into(),
				arguments: Some(arguments),
			})
			.await;

		match result {
			Ok(CallToolResult {
				is_error,
				content,
				structured_content,
				..
			}) => {
				// obvious error case, plain and simple
				if is_error.unwrap_or(false) {
					let error_message = if !content.is_empty() {
						extract_text_from_content(&content)
					} else {
						"Tool execution failed without error details".to_string()
					};

					return Some(Err(miette::miette!(
						"Tool '{}' on server '{}' returned an error: {}",
						call.name,
						server_name,
						error_message
					)));
				}

				// Handle successful tool call result
				if let Some(structured) = structured_content {
					// If we have structured content and it's not empty, return it
					if !structured.is_null() {
						debug!("Returning structured content for tool '{}'", call.name);
						return Some(Ok(structured));
					}
				}

				// Fall back to extracting text content if no structured content or if it's empty
				if !content.is_empty() {
					let text_content = extract_text_from_content(&content);
					debug!("Returning text content for tool '{}': {}", call.name, text_content);
					return Some(Ok(Value::String(text_content)));
				}

				// No content at all - return empty string
				debug!("Tool '{}' returned no content, returning empty string", call.name);
				Some(Ok(Value::String(String::new())))
			},

			Err(err) => Some(Err(miette::miette!(
				"Failed to call tool '{}' on MCP server '{}': {}",
				call.name,
				server_name,
				service_error_to_description(&err)
			))),
		}
	}
}

impl McpManager {
	/// Create a new McpManager from configuration
	/// This only stores the configuration - connections are created on demand
	pub fn new(config: McpConfig) -> Self {
		Self {
			config,
		}
	}

	/// Create a new MCP connection session
	/// This establishes connections to all configured servers
	pub async fn create_connection(&self) -> Result<McpConnection> {
		McpConnection::new(&self.config).await
	}
}
