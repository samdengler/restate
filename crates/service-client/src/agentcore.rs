// Copyright (c) 2023 - 2026 Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Client for Bedrock AgentCore Runtime deployments.
//!
//! This mirrors the Lambda client: the service-protocol request is wrapped in
//! the same buffered request/response payload shape (`ApiGatewayProxyRequest`)
//! and sent through `InvokeAgentRuntime`, which forwards it to the
//! container's `POST /invocations` endpoint where the SDK's request handler
//! unwraps it. The `runtimeSessionId` is derived from the Restate invocation
//! id, pinning retries and resumes of an invocation to the same warm microVM.

use crate::lambda::assume_role::AssumeRoleProvider;
use crate::lambda::{ApiGatewayProxyRequest, ApiGatewayProxyResponse};
use crate::utils::ErrorExt;
use arc_swap::ArcSwap;
use aws_config::BehaviorVersion;
use aws_sdk_bedrockagentcore::config::Region;
use aws_sdk_bedrockagentcore::error::{DisplayErrorContext, SdkError};
use aws_sdk_bedrockagentcore::operation::invoke_agent_runtime::InvokeAgentRuntimeError;
use aws_sdk_bedrockagentcore::primitives::Blob;
use bytes::{Buf, Bytes};
use bytestring::ByteString;
use futures::future::{BoxFuture, FutureExt, Shared};
use http::uri::PathAndQuery;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Body;
use restate_types::config::AwsLambdaOptions;
use restate_types::identifiers::AgentCoreRuntimeArn;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use crate::lambda::AssumeRoleCacheMode;

/// Header carrying the Restate invocation id on invoke requests; used to
/// derive the AgentCore `runtimeSessionId` (invocation ids are 38 chars,
/// above AgentCore's 33-char session id minimum).
const X_RESTATE_INVOCATION_ID: HeaderName = HeaderName::from_static("x-restate-invocation-id");

#[derive(Clone, Debug)]
pub struct AgentCoreClient {
    // Shared so concurrent requests all await the same lazily-created inner
    // client; see LambdaClient for rationale.
    inner: Shared<BoxFuture<'static, Arc<AgentCoreClientInner>>>,
}

#[derive(Debug)]
struct AgentCoreClientInner {
    /// STS client in order to call AssumeRole if needed
    sts_client: aws_sdk_sts::Client,
    /// Client to use if we aren't assuming a role
    no_role_client: aws_sdk_bedrockagentcore::Client,
    /// Client config builder, to create per-assumed-role clients sharing the
    /// underlying connector
    client_builder: aws_sdk_bedrockagentcore::config::Builder,
    /// Map of Role -> Client, same cache semantics as the Lambda client
    role_to_clients: Option<ArcSwap<HashMap<String, aws_sdk_bedrockagentcore::Client>>>,
    /// External id to set on assume role requests
    assume_role_external_id: Option<String>,
}

impl AgentCoreClient {
    pub fn new(
        profile_name: Option<String>,
        assume_role_external_id: Option<String>,
        assume_role_cache_mode: AssumeRoleCacheMode,
    ) -> Self {
        let mut config = aws_config::defaults(BehaviorVersion::latest());
        if let Some(profile_name) = profile_name {
            config = config.profile_name(profile_name);
        };

        let inner = async move {
            let config = config.load().await;

            let sts_conf = aws_sdk_sts::Config::from(&config);
            let sts_client = aws_sdk_sts::Client::from_conf(sts_conf);

            let client_builder = aws_sdk_bedrockagentcore::config::Builder::from(&config)
                // Restate has its own retry mechanisms
                .retry_config(aws_config::retry::RetryConfig::disabled());

            let client =
                aws_sdk_bedrockagentcore::Client::from_conf(client_builder.clone().build());

            let role_to_clients = match assume_role_cache_mode {
                AssumeRoleCacheMode::Unbounded => Some(Default::default()),
                AssumeRoleCacheMode::None => None,
            };

            Arc::new(AgentCoreClientInner {
                sts_client,
                no_role_client: client,
                client_builder,
                role_to_clients,
                assume_role_external_id,
            })
        }
        .boxed()
        .shared();

        Self { inner }
    }

    pub fn from_options(
        options: &AwsLambdaOptions,
        assume_role_cache_mode: AssumeRoleCacheMode,
    ) -> AgentCoreClient {
        AgentCoreClient::new(
            options.aws_profile.clone(),
            options.aws_assume_role_external_id.clone(),
            assume_role_cache_mode,
        )
    }

    pub fn invoke<B>(
        &self,
        arn: AgentCoreRuntimeArn,
        method: Method,
        assume_role_arn: Option<ByteString>,
        body: B,
        path: PathAndQuery,
        headers: HeaderMap<HeaderValue>,
    ) -> impl Future<Output = Result<Response<Full<Bytes>>, AgentCoreError>> + Send + 'static
    where
        B: Body + Send + Unpin + 'static,
        <B as Body>::Data: Send,
        <B as Body>::Error: Error + Send + Sync + 'static,
    {
        let region = Region::new(arn.region().to_string());
        let runtime_arn = arn.to_string();
        let inner = self.inner.clone();

        // Pin every attempt of the same invocation to the same microVM
        // session; discovery requests have no invocation id and can land on
        // any fresh session.
        let session_id = derive_runtime_session_id(&headers);

        async move {
            let inner = inner.await;

            // Aggregate the request body from upper layer
            let mut aggregated_request_body_buf = body
                .map_err(|e| AgentCoreError::Body(Box::new(e)))
                .collect()
                .await?
                .aggregate();
            let request_body =
                aggregated_request_body_buf.copy_to_bytes(aggregated_request_body_buf.remaining());

            let payload = ApiGatewayProxyRequest {
                path: Some(path.path()),
                http_method: method,
                headers,
                body: request_body,
                is_base64_encoded: true,
            };

            let res = inner
                .build_invoke(assume_role_arn)
                .agent_runtime_arn(runtime_arn)
                .runtime_session_id(session_id)
                .content_type("application/json")
                .accept("application/json")
                .payload(Blob::new(
                    serde_json::to_vec(&payload).map_err(AgentCoreError::SerializationError)?,
                ))
                .customize()
                .config_override(
                    aws_sdk_bedrockagentcore::config::Builder::default().region(region),
                )
                .send()
                .await
                .map_err(Box::new)?;

            let response_bytes = res
                .response
                .collect()
                .await
                .map_err(|e| AgentCoreError::Body(Box::new(e)))?
                .into_bytes();

            let response: ApiGatewayProxyResponse = serde_json::from_slice(&response_bytes)
                .map_err(AgentCoreError::DeserializationError)?;

            response.try_into().map_err(AgentCoreError::Response)
        }
    }
}

impl AgentCoreClientInner {
    fn build_invoke(
        &self,
        assume_role_arn: Option<ByteString>,
    ) -> aws_sdk_bedrockagentcore::operation::invoke_agent_runtime::builders::InvokeAgentRuntimeFluentBuilder
    {
        let assume_role_arn = if let Some(assume_role_arn) = assume_role_arn {
            assume_role_arn
        } else {
            // fastest path; no assumed role
            return self.no_role_client.invoke_agent_runtime();
        };

        if let Some(invoke) = self.role_to_clients.as_ref().and_then(|rlc| {
            rlc.load()
                .get(&*assume_role_arn)
                .map(|client| client.invoke_agent_runtime())
        }) {
            // fast-ish path; we've seen this assumed role before
            return invoke;
        }

        // slow path; create the client for this assumed role
        let conf = self
            .client_builder
            .clone()
            .credentials_provider(AssumeRoleProvider::new(
                self.sts_client.clone(),
                assume_role_arn.to_string(),
                self.assume_role_external_id.clone(),
            ))
            .build();

        let mut client = aws_sdk_bedrockagentcore::Client::from_conf(conf);

        if let Some(rlc) = &self.role_to_clients {
            rlc.rcu(|cache| {
                if let Some(existing_client) = cache.get(&*assume_role_arn) {
                    client = existing_client.clone();
                    return Arc::clone(cache);
                }
                let mut cache = HashMap::clone(cache);
                cache.insert(assume_role_arn.to_string(), client.clone());
                cache.into()
            });
        }

        client.invoke_agent_runtime()
    }
}

/// Derive the AgentCore `runtimeSessionId` for a request: the Restate
/// invocation id verbatim when present (pinning retries/resumes of an
/// invocation to the same warm microVM), otherwise a fresh discovery session.
fn derive_runtime_session_id(headers: &HeaderMap<HeaderValue>) -> String {
    headers
        .get(X_RESTATE_INVOCATION_ID)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("restate-discovery-{}", uuid::Uuid::now_v7()))
}

#[derive(Debug, thiserror::Error)]
pub enum AgentCoreError {
    #[error("problem reading request or response body: {0}")]
    Body(#[from] Box<dyn Error + Send + Sync>),
    #[error("agentcore service returned error: {}", DisplayErrorContext(&.0))]
    SdkError(#[from] Box<SdkError<InvokeAgentRuntimeError>>),
    #[error("request could not be serialized: {0}")]
    SerializationError(serde_json::Error),
    #[error("response could not be deserialized: {0}")]
    DeserializationError(serde_json::Error),
    #[error("response envelope invalid: {0}")]
    Response(#[source] crate::lambda::LambdaError),
}

impl AgentCoreError {
    /// Retryable errors are those which can be caused by transient faults and
    /// where retrying can succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            AgentCoreError::SdkError(err) => err.is_retryable(),
            AgentCoreError::Body(_)
            | AgentCoreError::SerializationError(_)
            | AgentCoreError::DeserializationError(_)
            | AgentCoreError::Response(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_the_invocation_id_verbatim() {
        let mut headers = HeaderMap::new();
        headers.insert(
            X_RESTATE_INVOCATION_ID,
            HeaderValue::from_static("inv_1gOolleakRq544v9Pi3JsL7bEw3M2UtT3Y"),
        );
        assert_eq!(
            derive_runtime_session_id(&headers),
            "inv_1gOolleakRq544v9Pi3JsL7bEw3M2UtT3Y"
        );
    }

    #[test]
    fn session_id_fallback_meets_agentcore_minimum_length() {
        let session_id = derive_runtime_session_id(&HeaderMap::new());
        assert!(session_id.starts_with("restate-discovery-"));
        // AgentCore requires runtimeSessionId to be at least 33 characters
        assert!(session_id.len() >= 33, "too short: {session_id}");
    }
}
