//! Offline embedding demo. Model decisions are scripted; business tools really execute.
//! Run: cargo run -p lato-agent --example business_agent
use std::sync::Arc;

use async_trait::async_trait;
use lato_agent::{AcpHost, AgentConfiguration, AgentMode};
use lato_ai::{FakeModelStream, StreamPiece};
use lato_core::{
    SideEffect, Tool, ToolCancellation, ToolConcurrency, ToolContext, ToolDescriptor, ToolError,
    ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use serde_json::{Value, json};

struct BusinessModule(&'static str);

#[async_trait]
impl Tool for BusinessModule {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse(format!("business:{}", self.0)).unwrap(),
            version: "1.0.0".parse().unwrap(),
            description: match self.0 {
                "lookup_customer" => "Look up customer revenue in the demo data module",
                _ => "Format a customer report from verified revenue",
            }
            .into(),
            input_schema: if self.0 == "lookup_customer" {
                json!({"type":"object","properties":{"customer":{"type":"string"}},"required":["customer"],"additionalProperties":false})
            } else {
                json!({"type":"object","properties":{"customer":{"type":"string"},"revenue":{"type":"number"}},"required":["customer","revenue"],"additionalProperties":false})
            },
            capabilities: vec![], // In-memory demo; no file, network or process access.
            side_effect: SideEffect::ReadOnly,
            concurrency: ToolConcurrency::Parallel,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 1_000,
            max_output_bytes: 4_096,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::User,
                id: "business-demo".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(&self, context: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        assert!(context.execution_grant.is_some());
        let customer = args["customer"].as_str().expect("schema validated");
        let content = if self.0 == "lookup_customer" {
            json!({"customer":customer,"revenue":42000,"source":"demo fixture"}).to_string()
        } else {
            format!(
                "# {customer} 客户报告\n\n演示收入：{} 元。",
                args["revenue"]
            )
        };
        println!("[{}] {content}", self.0);
        Ok(ToolOutput {
            content,
            metadata: json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

async fn request(
    host: &mut AcpHost,
    method: &str,
    params: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = host
        .handle(JsonRpcReq {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params: Some(params),
        })
        .await
        .ok_or("missing response")?;
    if let Some(error) = response.get("error") {
        return Err(error.to_string().into());
    }
    Ok(response["result"].clone())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let cwd = std::env::current_dir()?;
    // Replace this fixture with your configured ModelStream for real inference.
    let stream = Arc::new(FakeModelStream::new(vec![
        vec![StreamPiece::ToolCall {
            id: "lookup".into(),
            name: "lookup_customer".into(),
            arguments: json!({"customer":"Acme"}),
        }],
        vec![StreamPiece::ToolCall {
            id: "report".into(),
            name: "format_report".into(),
            arguments: json!({"customer":"Acme","revenue":42000}),
        }],
        vec![StreamPiece::Text(
            "已完成 Acme 客户查询和报告生成（演示数据）。".into(),
        )],
    ]));
    let (tx, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let drain = tokio::spawn(async move { while updates.recv().await.is_some() {} });
    let mut host = AcpHost::new_with_home(
        cwd.clone(),
        SessionTrust::for_headless_prompt(&cwd),
        tx,
        stream,
        home.path().into(),
    );
    host.configure_agent(AgentConfiguration {
        mode: AgentMode::General,
        instructions: "帮助销售团队。先查询客户数据，再使用查询结果生成报告；说明演示数据来源。"
            .into(),
        tools: Some(vec![
            ToolName::parse("business:lookup_customer")?,
            ToolName::parse("business:format_report")?,
        ]),
        workspace_instructions: None,
    })?;
    host.register_business_tool(Arc::new(BusinessModule("lookup_customer")))?;
    host.register_business_tool(Arc::new(BusinessModule("format_report")))?;
    let session = request(&mut host, "session/new", json!({})).await?;
    let sid = session["sessionId"].as_str().ok_or("missing session ID")?;
    let result = request(
        &mut host,
        "session/prompt",
        json!({"sessionId":sid,"text":"查询 Acme 并生成客户报告"}),
    )
    .await?;
    println!("{}", result["text"].as_str().unwrap_or_default());
    request(&mut host, "session/close", json!({"sessionId":sid})).await?;
    drain.abort();
    Ok(())
}
