# POC: Bedrock AgentCore Runtime as a native Restate deployment type

Branch `agentcore-deployment-type` adds `DeploymentType::AgentCore` to
restate-server, mirroring the AWS Lambda deployment type: the service-protocol
request rides in the same buffered request/response payload envelope
(`ApiGatewayProxyRequest`), but is delivered through `InvokeAgentRuntime`
instead of `lambda:Invoke`. The `runtimeSessionId` is derived from the
`x-restate-invocation-id` header, so every retry/resume of an invocation lands
on the same warm AgentCore microVM.

With this, **no proxy shim is needed**: the topology collapses from

```
Restate -> Lambda proxy -> InvokeAgentRuntime -> ACR   (companion POC 1)
```

to

```
Restate ----------------> InvokeAgentRuntime -> ACR    (this branch)
```

The agent container is unchanged from POC 1
([restate-killswitch-poc](https://github.com/samdengler/restate-killswitch-poc)):
an adapter implementing the ACR contract (`POST /invocations`, `GET /ping`)
that unwraps the envelope into the Restate Python SDK's request handler.

## Change surface (853 lines, 19 files)

| Area | Change |
|---|---|
| `types/identifiers` | `AgentCoreRuntimeArn` (`arn:aws:bedrock-agentcore:<region>:<acct>:runtime/<id>`) |
| `types/schema/deployment` | `DeploymentType::AgentCore { arn, assume_role_arn }`, storage serde, semantic equality |
| `types/deployment` | `AgentCoreDeploymentAddress`, `DeploymentAddress::AgentCore` |
| `service-client` | `AgentCoreClient` modeled on `LambdaClient` (same assume-role client cache); envelope reuse from the Lambda module |
| `service-protocol-v4` | discovery dispatch for AgentCore endpoints |
| `invoker-impl` | endpoint dispatch for the invocation path |
| `admin` / `admin-rest-model` | registration (see below), first-class response variants |
| `storage-query-datafusion` | `sys_deployment.ty = "agentcore"` |
| `cli` | list/describe rendering |

**Registration** reuses the existing `arn` field of the register-deployment
request and dispatches on the ARN's service segment — so *unmodified* HTTP
clients (curl, UI) can register an AgentCore runtime today. The CLI needed a
one-hunk tweak (it validates ARNs client-side before sending):

```sh
restate deployments register arn:aws:bedrock-agentcore:us-east-1:<acct>:runtime/<id> \
  [--assume-role-arn <role>]
```

Not covered yet (follow-ups for a real PR): PATCH address updates for
AgentCore deployments (rejected; re-register instead), zstd compression,
UI affordances, docs, config surface for a dedicated AWS profile.

## Demo

### Prereqs

1. **The POC 1 agent runtime.** From the
   [companion repo](https://github.com/samdengler/restate-killswitch-poc)
   (at commit `cf25c8e` or later — the adapter must be the dual-mode version
   that accepts native Lambda-event payloads), run:

   ```sh
   infra/build-push.sh        # arm64 agent image -> ECR
   infra/create-runtime.sh    # ACR runtime; prints the runtime ARN
   ```

   Nothing else from POC 1 is needed (no proxy, no Gateway, no Restate Cloud).

2. **Ambient AWS credentials** (env/profile/SSO) with
   `bedrock-agentcore:InvokeAgentRuntime` on that runtime — the *server*
   makes the call now. Region is taken from the ARN.

3. **Rust build tools**: `rustup` (the repo pins its toolchain via
   `rust-toolchain.toml`), plus `protoc` and `cmake` on `PATH`
   (`brew install protobuf cmake` on macOS).

### Run

```sh
cargo build -p restate-server -p restate-cli

target/debug/restate-server &           # ingress :8080, admin/UI :9070

# NOTE: use the CLI built above. The wire API accepts agentcore ARNs from any
# client (curl, UI), but released CLIs validate ARNs client-side and reject
# the bedrock-agentcore service (fixed in this branch).
target/debug/restate deployments register --yes \
  arn:aws:bedrock-agentcore:us-east-1:<acct>:runtime/<id>

target/debug/restate deployments list   # TYPE column shows "AgentCore"

# invoke through the ingress; watch journal entries per step in the UI
curl -X POST localhost:8080/AgentService/run/send \
  -H 'content-type: application/json' -d '{"prompt": "native invoke"}'

# the kill switch: cancel mid-run (the 10s sleep after step 5 is the easy
# window); compensation runs durably, done in ~1-2s
target/debug/restate invocations cancel inv_...
target/debug/restate invocations describe inv_...   # [409] cancelled
```

When finished: stop the server (its data lives in `restate-data/` under the
cwd it ran from) and use POC 1's `infra/teardown.sh` to remove the AWS
resources.
