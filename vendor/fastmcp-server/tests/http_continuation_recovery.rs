//! Real OAuth login and TLS reply loss across the PUBLIC managed client and
//! native secured server/MRTR/replay middleware. The bridge does not synthesize
//! MCP challenges, replies or errors: it only forwards native endpoint results
//! or deliberately loses their bytes after server dispatch has settled.
//! Run with proxy,native-tls-roots. The issuer is a local authorization-code
//! fixture, not external-provider qualification. No durable-restart claim.
#![cfg(all(feature = "proxy", feature = "native-tls-roots"))]
#![recursion_limit = "256"]

#[path = "http_continuation_recovery/managed_provider.rs"]
mod managed_provider;
#[path = "http_continuation_recovery/schema_bound.rs"]
mod schema_bound;
#[path = "http_continuation_recovery/schema_driver.rs"]
mod schema_driver;

use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};
use std::task::Poll;
use std::time::{Duration, Instant};

use asupersync::{Cx, Outcome};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::tls::{Certificate, CertificateChain, PrivateKey, TlsAcceptor, TlsAcceptorBuilder, TlsStream};
use fastmcp_client::http_auth::managed::{ManagedOAuthSession, OAuthSessionPolicy};
use fastmcp_client::http_auth::oauth::{OAuthClient, OAuthClientConfiguration, OAuthError};
use fastmcp_client::http_auth::rpc::{ManagedCoreError, ManagedCoreLimits};
use fastmcp_client::http_auth::rpc::interaction::{ManagedInteraction, ManagedInteractionError, ManagedInteractionEvent, ManagedInteractionLimits};
use fastmcp_client::http_auth::rpc::interaction::recovery::{ContinuationReplayContract, ContinuationRecoveryError, RecoverableManagedContinuation};
use fastmcp_core::{AuthContext, CanonicalHttpUrl, McpContext, McpError, McpOutcome, McpRequestCancellation, McpResult};
use fastmcp_core::ingress::{SecurityPartitionDescriptor, VerifiedAudienceBinding, VerifiedIdentityFacts, VerifiedIngressAuthentication};
use fastmcp_core::partition::{ContinuationPartitionKey, DurableOwnerKey, PartitionAuthorization};
use fastmcp_core::runtime::{ProcessGenerationGuard, SnapshotCloneStance};
use fastmcp_protocol::{CompleteResult, Content, CoreRequest, CoreResultDiscriminatorPolicy, DecodedResult,
    FinalCallToolResult, FinalInputResponses, InputRequiredResult, RequestId, ResultMeta, ResultPeerEra,
    Tool, FINAL_PROTOCOL_VERSION, decode_peer_result, protocol_policy::{ProtocolEra, ProtocolPolicy}};
use fastmcp_server::{BoxFuture, FinalToolOutcome, ToolExecutionMode, ToolHandler,
    Server, ServerHttpEndpoint, StaticTokenVerifier, TokenAuthProvider, Middleware,
    ContinuationReplayAuthority, ContinuationReplayLimits, ContinuationReplayMiddleware};
use fastmcp_server::bidirectional::MrtrCompletedInputs;
use fastmcp_server::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
use fastmcp_server::http_admission::security::HttpSecurityPolicy;
use fastmcp_transport::http::{HttpMethod, HttpRequest};
use serde_json::{Value, json};

const CHILD: &str = "FASTMCP_TEST_NATIVE_CONTINUATION_RECOVERY";
// TEST ONLY: same inline localhost identity as existing OAuth TLS fixtures.
// Trust is installed in an isolated child, never the machine's trust store.
const ROOT: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIBgzCCASmgAwIBAgICA+kwCgYIKoZIzj0EAwIwJzElMCMGA1UEAwwcRmFzdE1D\nUCBPQXV0aCBURVNUIE9OTFkgUm9vdDAeFw0yMDAxMDEwMDAwMDBaFw00OTEyMzEw\nMDAwMDBaMCcxJTAjBgNVBAMMHEZhc3RNQ1AgT0F1dGggVEVTVCBPTkxZIFJvb3Qw\nWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAS5t2O8JZ0hNjgI38E9Ov6i6mKoDRGo\nApMsykFkvgb6Zm9/5gCZ90eIKw7aWgK6iNs7lbtVY9mysZBIqm6pKQO2o0UwQzAS\nBgNVHRMBAf8ECDAGAQH/AgEAMA4GA1UdDwEB/wQEAwIBhjAdBgNVHQ4EFgQU6QNI\nrmvMiLoV3jIoCyohXARwI8gwCgYIKoZIzj0EAwIDSAAwRQIgCKOrW3vhzUJ2EyuY\nvQUTdqGFhy0zEHj4ITFLvXPz1X8CIQCLKD4EKCvS/zkBSu/6uee1WV9d97UpK3yW\nX/aCEJ5+hA==\n-----END CERTIFICATE-----\n";
const LEAF: &[u8] = b"-----BEGIN CERTIFICATE-----\nMIIBjjCCATSgAwIBAgICA+owCgYIKoZIzj0EAwIwJzElMCMGA1UEAwwcRmFzdE1D\nUCBPQXV0aCBURVNUIE9OTFkgUm9vdDAeFw0yMDAxMDEwMDAwMDBaFw00OTEyMzEw\nMDAwMDBaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqGSM49\nAwEHA0IABPPKylLna9VpWAlpshHBhSsQHNOv3BaEGX4HSBhHiBVel0ce+qfHF15O\n0T63Zlp7TtxlMdEY+rPpgioSFDQVadijYzBhMAwGA1UdEwEB/wQCMAAwLAYDVR0R\nBCUwI4IJbG9jYWxob3N0hwR/AAABhxAAAAAAAAAAAAAAAAAAAAABMBMGA1UdJQQM\nMAoGCCsGAQUFBwMBMA4GA1UdDwEB/wQEAwIHgDAKBggqhkjOPQQDAgNIADBFAiEA\n6qrAr2qp/t6K62T9Et2mUU/zfd4kJb+ekyoAim1yTFcCICb6SdVY2fg15/SXf0vE\nIvYelqtTk8FQInCEcIxvfF3m\n-----END CERTIFICATE-----\n";
const KEY: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgcCe44IBKhbw+D/s7\nBjDHOOV0g+EoxFno7VJGKhJeer2hRANCAATzyspS52vVaVgJabIRwYUrEBzTr9wW\nhBl+B0gYR4gVXpdHHvqnxxdeTtE+t2Zae07cZTHRGPqz6YIqEhQ0FWnY\n-----END PRIVATE KEY-----\n";

#[derive(Clone)]
struct Probe { starts: Arc<AtomicUsize>, effects: Arc<AtomicUsize>, transforms: Arc<AtomicUsize> }
impl Probe {
    fn new() -> Self { Self { starts: Arc::new(AtomicUsize::new(0)), effects: Arc::new(AtomicUsize::new(0)), transforms: Arc::new(AtomicUsize::new(0)) } }
}
fn challenge() -> InputRequiredResult {
    let (decoded, _) = decode_peer_result(r#"{"resultType":"input_required","requestState":"handler-private-state",
        "inputRequests":{"left":{"method":"roots/list"},"right":{"method":"roots/list"}}}"#,
        ResultPeerEra::Modern, &CoreResultDiscriminatorPolicy).unwrap();
    let DecodedResult::InputRequired(input) = decoded else { panic!("fixture branch"); };
    input
}
impl ToolHandler for Probe {
    fn definition(&self) -> Tool {
        Tool { name: "checkout".to_owned(), description: None, input_schema: json!({"type":"object"}),
            output_schema: None, icon: None, version: None, tags: vec![], annotations: None }
    }
    fn execution_mode(&self) -> ToolExecutionMode { ToolExecutionMode::Async }
    fn declares_final_mrtr(&self) -> bool { true }
    fn call(&self, _: &McpContext, _: Value) -> McpResult<Vec<Content>> { panic!("request-owned hook required") }
    fn call_final_outcome_async_resuming_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, arguments: Value, inputs: Option<&'a MrtrCompletedInputs>,
    ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
        Box::pin(async move {
            cx.checkpoint().unwrap(); ctx.ensure_live().unwrap();
            let Some(inputs) = inputs else {
                self.starts.fetch_add(1, Ordering::SeqCst);
                return Outcome::Ok(FinalToolOutcome::InputRequired(challenge()));
            };
            assert_eq!(inputs.responses().len(), 2);
            let effect = self.effects.fetch_add(1, Ordering::SeqCst) + 1;
            let result: FinalCallToolResult = serde_json::from_value(json!({"content":[],"structuredContent":{
                "quantity":arguments["quantity"],"effect":effect,
                "left":inputs.roots("left").unwrap().unwrap(),"right":inputs.roots("right").unwrap().unwrap(),
                "order":inputs.responses().iter().map(|(key,_)|key).collect::<Vec<_>>()}})).unwrap();
            Outcome::Ok(FinalToolOutcome::Complete(CompleteResult::new(result, ResultMeta::empty())))
        })
    }
}
struct Stamp(Arc<AtomicUsize>);
impl Middleware for Stamp {
    fn on_response(&self, _: &McpContext, _: &fastmcp_protocol::JsonRpcRequest, mut value: Value) -> McpResult<Value> {
        if value["resultType"] == "complete" {
            value["com.example/stamp"] = json!(self.0.fetch_add(1, Ordering::SeqCst) + 1);
        }
        Ok(value)
    }
}
fn authority(ctx: &McpContext, resource: &str, lifetime: &McpRequestCancellation) -> McpResult<ContinuationReplayAuthority> {
    let auth = ctx.auth().ok_or_else(|| McpError::invalid_params("authenticated owner required"))?;
    let subject = auth.subject.as_deref().ok_or_else(|| McpError::invalid_params("subject required"))?;
    if !auth.scopes.iter().any(|scope| scope == "tools:call") { return Err(McpError::invalid_params("permission required")); }
    let ingress = VerifiedIngressAuthentication::from_verified_provider_output(VerifiedIdentityFacts {
        provider:"native-recovery-test", configuration_generation:1, issuer:"https://issuer.example", canonical_resource:resource,
        verified_audience_binding: VerifiedAudienceBinding::OAuth {
            canonical_resource:resource.to_owned(), validated_audience:resource.to_owned(), audience_policy_id:"fixture".to_owned(),
            audience_policy_revision:1, provider:"native-recovery-test".to_owned(), configuration_generation:1,
        },
        tenant:"fixture", subject_or_principal:subject, authorized_party_or_client:"recovery-client",
        verified_claims:&[], auth_policy_revision:1, trust_generation:1,
    }).unwrap();
    let descriptor = SecurityPartitionDescriptor::from_verified_ingress(&ingress).to_partition_descriptor().unwrap();
    let key = ContinuationPartitionKey::derive(&descriptor, &["tools:call"], "checkout-v1", "roots-v1", "policy-v1", "deployment-v1").unwrap();
    let owner = DurableOwnerKey::derive(&descriptor, 1).unwrap();
    Ok(ContinuationReplayAuthority::new(key, PartitionAuthorization::current(&descriptor, &owner), lifetime.clone()))
}

struct Peer {
    listener: TcpListener, tls: TlsAcceptor, endpoint: ServerHttpEndpoint, policy: HttpSecurityPolicy,
    journal: Arc<ContinuationReplayMiddleware>, probe: Probe, token: String,
    seen: Mutex<Vec<Value>>, grants: AtomicUsize,
}
#[derive(Clone, Copy)]
enum Delivery { Complete, LoseHead, TruncateBody, StallBody }
impl Peer {
    async fn new(cx: &Cx, journal_enabled: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("https://{}", listener.local_addr().unwrap());
        let resource = format!("{origin}/mcp");
        let probe = Probe::new();
        let token = format!("native-recovery-{}", listener.local_addr().unwrap().port());
        let mut auth = AuthContext::with_subject("alice".to_owned());
        auth.scopes = vec!["tools:call".to_owned()];
        let provider = TokenAuthProvider::new(StaticTokenVerifier::new([(token.clone(), auth)]).unwrap());
        let lifetime = McpRequestCancellation::new();
        let journal = Arc::new(ContinuationReplayMiddleware::new_with_successor_recovery(cx,
            ProcessGenerationGuard::install().unwrap(), SnapshotCloneStance::NoLiveMemoryCloning,
            ContinuationReplayLimits::default(),
            move |ctx: &McpContext, _: &fastmcp_protocol::JsonRpcRequest| authority(ctx, &resource, &lifetime)).unwrap());
        let mut builder = Server::new("native-recovery", "1").protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
            .auth_provider(provider).tool(probe.clone());
        if journal_enabled { builder = builder.middleware(journal.clone()); }
        builder = builder.middleware(Stamp(probe.transforms.clone()));
        #[cfg(feature = "legacy-2024-11-05")]
        let endpoint = builder.build_http_endpoint(&origin).unwrap();
        #[cfg(not(feature = "legacy-2024-11-05"))]
        let endpoint = builder.build_http_endpoint().unwrap();
        Self { listener, endpoint, probe, token, journal, seen:Mutex::new(vec![]), grants:AtomicUsize::new(0),
            tls:TlsAcceptorBuilder::new(CertificateChain::from_pem(LEAF).unwrap(),PrivateKey::from_pem(KEY).unwrap())
                .alpn_protocols(vec![b"http/1.1".to_vec()]).build().unwrap(),
            policy:HttpSecurityPolicy::new(HttpEndpointConfig::new("/mcp",HttpAdmissionLimits::new(32,8192,65536).unwrap()).unwrap(),
                &origin,vec![]).unwrap() }
    }
    fn resource(&self) -> CanonicalHttpUrl { url(&format!("https://{}/mcp",self.listener.local_addr().unwrap())) }
    fn client(&self) -> OAuthClient {
        OAuthClient::new(OAuthClientConfiguration::from_trusted_endpoints("https://issuer.example",
            url("https://issuer.example/authorize"), url(&format!("https://{}/token",self.listener.local_addr().unwrap())),
            self.resource(), "recovery-client", vec!["tools:call".to_owned()]).unwrap()
            .with_extra_root_certificate(Certificate::from_pem(ROOT).unwrap().remove(0)).unwrap())
    }
    async fn receive(&self) -> (TlsStream<TcpStream>, String, BTreeMap<String,String>, Vec<u8>) {
        let (socket,_) = self.listener.accept().await.unwrap();
        let mut socket = self.tls.accept(socket).await.unwrap();
        let mut bytes = Vec::new(); let mut chunk = [0;2048];
        let end = loop {
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0 && bytes.len()+count <= 65536);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(offset) = bytes.windows(4).position(|part|part==b"\r\n\r\n") { break offset+4; }
        };
        let head = std::str::from_utf8(&bytes[..end]).unwrap();
        let start = head.lines().next().unwrap().to_owned();
        let mut headers = BTreeMap::new();
        for line in head.lines().skip(1).filter(|line|!line.is_empty()) {
            let (name,value) = line.split_once(':').unwrap();
            assert!(headers.insert(name.to_ascii_lowercase(),value.trim().to_owned()).is_none());
        }
        assert!(!headers.contains_key("transfer-encoding"));
        let size:usize = headers["content-length"].parse().unwrap();
        assert!(end+size <= 65536);
        while bytes.len()<end+size {
            let count=socket.read(&mut chunk).await.unwrap();
            assert!(count>0 && bytes.len()+count<=65536); bytes.extend_from_slice(&chunk[..count]);
        }
        assert_eq!(bytes.len(),end+size);
        (socket,start,headers,bytes[end..].to_vec())
    }
    async fn login(&self) {
        let (mut socket,start,headers,body)=self.receive().await;
        assert_eq!(start,"POST /token HTTP/1.1"); assert!(!headers.contains_key("authorization"));
        let fields=form(std::str::from_utf8(&body).unwrap());
        assert_eq!(fields["grant_type"],"authorization_code"); assert_eq!(fields["resource"],self.resource().as_str());
        self.grants.fetch_add(1,Ordering::SeqCst);
        let body=serde_json::to_vec(&json!({"access_token":self.token,"token_type":"Bearer","expires_in":300})).unwrap();
        let headers=std::collections::HashMap::from([("content-type".to_owned(),"application/json".to_owned())]);
        write_reply(&mut socket,200,&headers,&body,Delivery::Complete).await;
    }
    async fn dispatch(&self,cx:&Cx,delivery:Delivery) -> Value {
        let (mut socket,start,headers,body)=self.receive().await;
        assert_eq!(start,"POST /mcp HTTP/1.1");
        assert_eq!(headers["authorization"],format!("Bearer {}",self.token));
        assert!(!headers.contains_key("mcp-session-id") && !headers.contains_key("last-event-id"));
        let wire:Value=serde_json::from_slice(&body).unwrap();
        self.seen.lock().unwrap().push(wire);
        let mut request=HttpRequest::new(HttpMethod::Post,"/mcp");
        for (key,value) in headers { request=request.with_header(key,value); }
        request=request.with_body(body);
        let reply=Box::pin(self.endpoint.handle_secured_async(cx,&self.policy,request)).await.unwrap();
        assert!(!reply.is_streaming(),"fixture emits finite JSON, not a replayable stream");
        let (response,stream)=reply.into_parts();
        assert!(stream.is_none());drop(stream);
        let status=response.status.0;
        let value=serde_json::from_slice(&response.body).unwrap();
        // Injection is strictly AFTER native authentication, dispatch, effect,
        // result transformation and replay-journal completion.
        write_reply(&mut socket,status,&response.headers,&response.body,delivery).await;
        value
    }
    fn quiet(&self) {
        let mut cx=std::task::Context::from_waker(std::task::Waker::noop());
        assert!(self.listener.poll_accept(&mut cx).is_pending(),"no implicit recovery, replay or refresh POST");
    }
}
async fn write_reply(socket:&mut TlsStream<TcpStream>,status:u16,headers:&std::collections::HashMap<String,String>,body:&[u8],delivery:Delivery) {
    if matches!(delivery,Delivery::LoseHead) { let _=socket.shutdown().await; return; }
    let mut head=format!("HTTP/1.1 {status} OK\r\n");
    for (name,value) in headers {
        assert!(!name.eq_ignore_ascii_case("transfer-encoding"),"native response must be finite JSON");
        if !name.eq_ignore_ascii_case("content-length") && !name.eq_ignore_ascii_case("connection") {
            head.push_str(name);head.push_str(": ");head.push_str(value);head.push_str("\r\n");
        }
    }
    head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n",body.len()));
    socket.write_all(head.as_bytes()).await.unwrap();
    if matches!(delivery,Delivery::StallBody) {
        socket.flush().await.unwrap();
        let mut byte=[0]; assert!(!matches!(socket.read(&mut byte).await,Ok(n) if n>0));
        return;
    }
    let body=if matches!(delivery,Delivery::TruncateBody) { &body[..1] } else { body };
    socket.write_all(body).await.unwrap(); socket.flush().await.unwrap();
    let _=socket.shutdown().await;
}
fn url(value:&str)->CanonicalHttpUrl { CanonicalHttpUrl::parse(value).unwrap() }
fn form(value:&str)->BTreeMap<String,String> {
    fn decode(text:&str)->String {
        let mut bytes=text.bytes(); let mut out=Vec::new();
        while let Some(byte)=bytes.next() { out.push(match byte {
            b'+'=>b' ', b'%'=>((char::from(bytes.next().unwrap()).to_digit(16).unwrap()*16)+char::from(bytes.next().unwrap()).to_digit(16).unwrap()) as u8,
            byte=>byte,
        }); } String::from_utf8(out).unwrap()
    }
    value.split('&').map(|field|{let (key,value)=field.split_once('=').unwrap();(decode(key),decode(value))}).collect()
}
async fn browser(authorization:CanonicalHttpUrl)->Result<(),OAuthError> {
    let fields=form(authorization.query().unwrap());
    let address:std::net::SocketAddr=fields["redirect_uri"].strip_prefix("http://").unwrap().split('/').next().unwrap().parse().unwrap();
    assert!(address.ip().is_loopback()); assert_eq!(fields["code_challenge_method"],"S256");
    let mut socket=TcpStream::connect(address).await.map_err(|_|OAuthError::CallbackRejected)?;
    socket.write_all(format!("GET /oauth/callback?code=native-recovery-code&iss=https%3A%2F%2Fissuer.example&state={} HTTP/1.1\r\nHost: {address}\r\n\r\n",fields["state"]).as_bytes())
        .await.map_err(|_|OAuthError::CallbackRejected)
}
async fn pair<L:Future,R:Future>(left:L,right:R)->(L::Output,R::Output) {
    let mut left=Box::pin(left);let mut right=Box::pin(right);let(mut one,mut two)=(None,None);
    poll_fn(|cx|{
        if one.is_none(){if let Poll::Ready(value)=left.as_mut().poll(cx){one=Some(value);}}
        if two.is_none(){if let Poll::Ready(value)=right.as_mut().poll(cx){two=Some(value);}}
        if one.is_some()&&two.is_some(){Poll::Ready((one.take().unwrap(),two.take().unwrap()))}else{Poll::Pending}
    }).await
}
fn original()->CoreRequest {
    CoreRequest::decode(ProtocolEra::Modern2026,"tools/call",Some(&json!({"name":"checkout","arguments":{"quantity":1},
        "_meta":{"io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,"io.modelcontextprotocol/clientCapabilities":{"roots":{}}}}))).unwrap()
}
fn answers(keys:&[&str],effects:&AtomicUsize)->FinalInputResponses {
    let mut result=serde_json::Map::new();
    for key in keys {
        effects.fetch_add(1,Ordering::SeqCst);
        result.insert((*key).to_owned(),json!({"roots":[{"uri":format!("file:///{key}/approved")}]}));
    }
    serde_json::from_value(Value::Object(result)).unwrap()
}
async fn lose(peer:&Peer,cx:&Cx,pending:&mut RecoverableManagedContinuation,id:i64,delivery:Delivery,recovery:bool)->Value {
    let application=async {
        let sent=if recovery {pending.recover(cx,RequestId::Number(id)).await}else{pending.send(cx,RequestId::Number(id)).await};
        let error=match sent {
            Err(error)=>error,
            Ok(())=>pending.next_event(cx).await.err().expect("native committed reply bytes were lost"),
        };
        assert!(matches!(error,ContinuationRecoveryError::Interrupted),"{error}");
        assert!(pending.is_recovery_pending());
    };
    let (reply,())=pair(peer.dispatch(cx,delivery),application).await;
    peer.quiet(); reply
}

#[derive(Clone,Copy)]
enum Case { Head, Body, Successor, NoJournal, Cancel, CloseOwner, Bytes, Attempts, Abandon, Endpoint, Provider(managed_provider::Case), SchemaBound(schema_bound::Case), SchemaDriver(schema_driver::Case) }
fn isolated(name:&str,case:Case) {
    if let Ok(selected)=std::env::var(CHILD) {assert_eq!(selected,name);run(case);return;}
    struct Root(std::path::PathBuf);
    impl Drop for Root {fn drop(&mut self){let _=std::fs::remove_file(&self.0);}}
    struct Child(std::process::Child);
    impl Drop for Child {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}
    let root=Root(std::env::temp_dir().join(format!("fastmcp-native-recovery-{}-{name}.pem",std::process::id())));
    let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(&root.0).unwrap();
    file.write_all(ROOT).unwrap();drop(file);
    let mut child=Child(Command::new(std::env::current_exe().unwrap()).args(["--exact",name,"--nocapture","--test-threads=1"])
        .env(CHILD,name).env("SSL_CERT_FILE",&root.0).env_remove("SSL_CERT_DIR")
        .stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap());
    let end=Instant::now()+Duration::from_secs(30);
    loop {if let Some(status)=child.0.try_wait().unwrap(){assert!(status.success());return;}
        assert!(Instant::now()<end,"native recovery child exceeded its bound");std::thread::sleep(Duration::from_millis(10));}
}
fn run(case:Case) {
    asupersync::runtime::RuntimeBuilder::current_thread().with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .blocking_threads(1,4).build().unwrap().block_on(async move {
            let parent=Cx::current().unwrap();
            let mut task=parent.spawn(move |cx|async move {
                let end=cx.now().saturating_add_nanos(20_000_000_000);
                asupersync::time::timeout_at(end,Box::pin(scenario(cx,case))).await.unwrap();
            }).unwrap(); task.join(&parent).await.unwrap();
        });
}
async fn scenario(cx:Cx,case:Case) {
    if let Case::SchemaDriver(selected) = case {
        Box::pin(schema_driver::scenario(cx, selected)).await;
        return;
    }
    if let Case::SchemaBound(selected) = case {
        Box::pin(schema_bound::scenario(cx, selected)).await;
        return;
    }
    if let Case::Provider(selected) = case {
        managed_provider::scenario(cx, selected).await;
        return;
    }
    let peer=Peer::new(&cx,!matches!(case,Case::NoJournal)).await;
    let ((),session)=pair(peer.login(),ManagedOAuthSession::authorize(&cx,peer.client(),OAuthSessionPolicy::default(),browser)).await;
    let session=session.unwrap();
    let cancel=McpRequestCancellation::new();
    let limits=ManagedInteractionLimits::new(ManagedCoreLimits::new(4096,4096,
        if matches!(case,Case::Bytes){8192}else{65536},0,Duration::from_secs(15)).unwrap(),2,2).unwrap();
    let (initial,operation)=pair(peer.dispatch(&cx,Delivery::Complete),session.start_core_interaction_with_cancellation(
        &cx,&cancel,original(),RequestId::Number(1),limits)).await;
    let mut operation:ManagedInteraction=operation.unwrap();
    assert!(matches!(operation.next_event(&cx).await.unwrap(),Some(ManagedInteractionEvent::InputRequired(_))));
    assert_ne!(initial["result"]["requestState"],"handler-private-state");
    assert_eq!(peer.probe.starts.load(Ordering::SeqCst),1);
    let host_effects=AtomicUsize::new(0);
    let selected=if matches!(case,Case::Successor){vec!["left"]}else{vec!["left","right"]};
    let contract=ContinuationReplayContract::for_configured_endpoint(
        if matches!(case,Case::Endpoint){url("https://other.example/mcp")}else{peer.resource()},1).unwrap();
    let prepared=operation.prepare_recoverable_continuation(&cx,Some(answers(&selected,&host_effects)),contract);
    if matches!(case,Case::Endpoint) {
        assert!(matches!(prepared,Err(ContinuationRecoveryError::EndpointMismatch)));
        peer.quiet();session.close();return;
    }
    let mut pending=prepared.unwrap();
    assert!(matches!(pending.send(&cx,RequestId::Number(1)).await,
        Err(ContinuationRecoveryError::Interaction(ManagedInteractionError::RepeatedRequestId))));
    assert_eq!(pending.attempts(),0);peer.quiet();
    let lost=if matches!(case,Case::Abandon) {
        let application=async {
            pending.send(&cx,RequestId::Number(2)).await.unwrap();
            let mut read=Box::pin(pending.next_event(&cx));
            poll_fn(|cx|{assert!(read.as_mut().poll(cx).is_pending());Poll::Ready(())}).await;
            drop(read);assert!(pending.is_recovery_pending());
        };
        pair(peer.dispatch(&cx,Delivery::StallBody),application).await.0
    }else{
        lose(&peer,&cx,&mut pending,2,if matches!(case,Case::Body){Delivery::TruncateBody}else{Delivery::LoseHead},false).await
    };
    assert!(lost.get("error").is_none(),"native dispatch must complete before injected loss: {lost}");
    assert_eq!(peer.probe.effects.load(Ordering::SeqCst),usize::from(!matches!(case,Case::Successor)));
    assert_eq!(host_effects.load(Ordering::SeqCst),selected.len());
    assert!(matches!(pending.recover(&cx,RequestId::Number(2)).await,
        Err(ContinuationRecoveryError::Interaction(ManagedInteractionError::RepeatedRequestId))));
    assert_eq!(pending.attempts(),1);peer.quiet();
    match case {
        Case::Cancel|Case::CloseOwner=>{
            if matches!(case,Case::Cancel){cancel.cancel();}else{session.close();}
            assert!(pending.recover(&cx,RequestId::Number(3)).await.is_err());
            assert!(!pending.is_recovery_pending());peer.quiet();
        }
        Case::Bytes=>{
            assert!(matches!(pending.recover(&cx,RequestId::Number(3)).await,
                Err(ContinuationRecoveryError::Interaction(ManagedInteractionError::Core(ManagedCoreError::ResponseByteLimit)))));
            assert_eq!(pending.attempts(),1);peer.quiet();
        }
        Case::Attempts=>{
            let second=lose(&peer,&cx,&mut pending,3,Delivery::LoseHead,true).await;
            assert_eq!(second["result"],lost["result"]);
            assert!(matches!(pending.recover(&cx,RequestId::Number(4)).await,Err(ContinuationRecoveryError::RecoveryLimit)));
            assert_eq!(pending.attempts(),2);peer.quiet();
        }
        _=>{
            let (replayed,result)=pair(peer.dispatch(&cx,Delivery::Complete),async {
                pending.recover(&cx,RequestId::Number(3)).await.unwrap();pending.next_event(&cx).await
            }).await;
            assert_eq!(replayed["id"],3);
            if matches!(case,Case::NoJournal) {
                assert!(replayed.get("error").is_some());assert!(result.is_err());
                assert!(!pending.is_recovery_pending());
            }else{
                assert_eq!(replayed["result"],lost["result"]);
                let event=result.unwrap();
                if matches!(case,Case::Successor){
                    let ManagedInteractionEvent::InputRequired(input)=event else{panic!("recover the exact successor");};
                    assert_eq!(input.request_state(),lost["result"]["requestState"].as_str());
                    assert_eq!(input.input_requests().unwrap().members().len(),1);
                    let operation=pending.into_interaction().unwrap();
                    assert_eq!(operation.continuation_count(),1);
                    let contract=ContinuationReplayContract::for_configured_endpoint(peer.resource(),1).unwrap();
                    let mut pending=operation.prepare_recoverable_continuation(&cx,Some(answers(&["right"],&host_effects)),contract).unwrap();
                    let (final_wire,event)=pair(peer.dispatch(&cx,Delivery::Complete),async {
                        pending.send(&cx,RequestId::Number(4)).await.unwrap();pending.next_event(&cx).await.unwrap()
                    }).await;
                    assert!(matches!(event,ManagedInteractionEvent::Complete(_)));
                    assert_eq!(pending.into_interaction().unwrap().continuation_count(),2);
                    assert_eq!(final_wire["result"]["structuredContent"]["order"],json!(["left","right"]));
                }else{
                    assert!(matches!(event,ManagedInteractionEvent::Complete(_)));
                    assert_eq!(pending.into_interaction().unwrap().continuation_count(),1);
                }
            }
        }
    }
    let requests=peer.seen.lock().unwrap();
    if requests.len()>=3 {
        assert_eq!(requests[1]["params"],requests[2]["params"],"recovery changes only correlation ID");
        assert_ne!(requests[1]["id"],requests[2]["id"]);
    }
    assert_eq!(peer.probe.starts.load(Ordering::SeqCst),1);
    assert_eq!(peer.probe.effects.load(Ordering::SeqCst),1);
    assert_eq!(peer.probe.transforms.load(Ordering::SeqCst),1);
    assert_eq!(host_effects.load(Ordering::SeqCst),2,"answers are computed once per input, not per transmission");
    assert_eq!(peer.grants.load(Ordering::SeqCst),1);
    drop(requests);peer.quiet();session.close();peer.journal.close().unwrap();
}

#[test] fn native_terminal_head_loss_recovers_without_reexecuting(){isolated("native_terminal_head_loss_recovers_without_reexecuting",Case::Head);}
#[test] fn native_terminal_body_truncation_recovers_without_reexecuting(){isolated("native_terminal_body_truncation_recovers_without_reexecuting",Case::Body);}
#[test] fn native_partial_reply_recovers_exact_successor_and_both_answers(){isolated("native_partial_reply_recovers_exact_successor_and_both_answers",Case::Successor);}
#[test] fn native_one_use_registry_without_journal_refuses_recovery(){isolated("native_one_use_registry_without_journal_refuses_recovery",Case::NoJournal);}
#[test] fn native_lost_reply_cancellation_prevents_recovery_post(){isolated("native_lost_reply_cancellation_prevents_recovery_post",Case::Cancel);}
#[test] fn native_lost_reply_owner_close_prevents_recovery_post(){isolated("native_lost_reply_owner_close_prevents_recovery_post",Case::CloseOwner);}
#[test] fn native_lost_reply_full_frame_charge_bounds_recovery(){isolated("native_lost_reply_full_frame_charge_bounds_recovery",Case::Bytes);}
#[test] fn native_repeated_reply_loss_exhausts_explicit_attempt_budget(){isolated("native_repeated_reply_loss_exhausts_explicit_attempt_budget",Case::Attempts);}
#[test] fn native_abandoned_body_can_only_recover_through_the_journal(){isolated("native_abandoned_body_can_only_recover_through_the_journal",Case::Abandon);}
#[test] fn native_replay_contract_must_match_the_authenticated_endpoint(){isolated("native_replay_contract_must_match_the_authenticated_endpoint",Case::Endpoint);}
