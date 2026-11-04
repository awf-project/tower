use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use extension_protocol::HostCall;
use semantic_context_extension::{
    DeadlineLayer, EmbeddingProviderError, EmbeddingProviderPort, EmbeddingRequest,
    OperationDeadline, SemanticOperation,
    adapters::embedding::{
        EmbeddingConfigurationError, OpenAiCompatibleEmbeddingProvider, ValidatedEmbeddingConfig,
    },
};

#[derive(Clone)]
struct ResponseSpec {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    delay: Duration,
}

impl ResponseSpec {
    fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

struct FixtureState {
    responses: VecDeque<ResponseSpec>,
    requests: Vec<RecordedRequest>,
    stopped: bool,
}

struct HttpFixture {
    address: SocketAddr,
    state: Arc<(Mutex<FixtureState>, Condvar)>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HttpFixture {
    fn new(responses: impl IntoIterator<Item = ResponseSpec>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        listener
            .set_nonblocking(true)
            .expect("make HTTP fixture nonblocking");
        let address = listener.local_addr().expect("read fixture address");
        let state = Arc::new((
            Mutex::new(FixtureState {
                responses: responses.into_iter().collect(),
                requests: Vec::new(),
                stopped: false,
            }),
            Condvar::new(),
        ));
        let worker_state = Arc::clone(&state);
        let worker = thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if worker_state
                            .0
                            .lock()
                            .expect("fixture state lock poisoned")
                            .stopped
                        {
                            break;
                        }
                        let connection_state = Arc::clone(&worker_state);
                        let _ = thread::spawn(move || {
                            let request = read_request(&mut stream);
                            let response = {
                                let (lock, changed) = &*connection_state;
                                let mut state = lock.lock().expect("fixture state lock poisoned");
                                state.requests.push(request);
                                changed.notify_all();
                                state.responses.pop_front().unwrap_or_else(|| {
                                    ResponseSpec::status(500, "unexpected request")
                                })
                            };
                            if !response.delay.is_zero() {
                                thread::sleep(response.delay);
                            }
                            let _ = write_response(&mut stream, &response);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if worker_state
                            .0
                            .lock()
                            .expect("fixture state lock poisoned")
                            .stopped
                        {
                            break;
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("HTTP fixture accept failed: {error}"),
                }
            }
        });

        Self {
            address,
            state,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn request_count(&self) -> usize {
        self.state
            .0
            .lock()
            .expect("fixture state lock poisoned")
            .requests
            .len()
    }

    fn wait_for_requests(&self, count: usize) -> Vec<RecordedRequest> {
        let (lock, changed) = &*self.state;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = lock.lock().expect("fixture state lock poisoned");
        while state.requests.len() < count {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("fixture did not receive the expected request");
            let (next, timeout) = changed
                .wait_timeout(state, remaining)
                .expect("fixture state lock poisoned while waiting");
            state = next;
            assert!(!timeout.timed_out(), "fixture request deadline expired");
        }
        state.requests.clone()
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.state
            .0
            .lock()
            .expect("fixture state lock poisoned")
            .stopped = true;
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("HTTP fixture worker panicked");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> RecordedRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set fixture read timeout");
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1_024];
    let header_end = loop {
        let read = stream.read(&mut buffer).expect("read fixture request");
        assert!(read > 0, "client closed before sending complete headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).expect("request headers are UTF-8");
    let mut lines = head.split("\r\n");
    let request_line = lines.next().expect("request line").to_owned();
    let headers: Vec<_> = lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once(':').expect("valid request header");
            (name.trim().to_owned(), value.trim().to_owned())
        })
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>().expect("numeric content length"))
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let read = stream.read(&mut buffer).expect("read fixture request body");
        assert!(read > 0, "client closed before sending complete body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    let body = String::from_utf8(bytes[header_end..header_end + content_length].to_vec())
        .expect("request body is UTF-8");
    RecordedRequest {
        request_line,
        headers,
        body,
    }
}

fn write_response(stream: &mut TcpStream, response: &ResponseSpec) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        503 => "Service Unavailable",
        _ => "Fixture Response",
    };
    write!(stream, "HTTP/1.1 {} {}\r\n", response.status, reason)?;
    for (name, value) in &response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.body.len(),
        response.body
    )?;
    stream.flush()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build provider test runtime")
}

fn config(base_url: String) -> ValidatedEmbeddingConfig {
    ValidatedEmbeddingConfig {
        base_url,
        model: "fixture-model".to_owned(),
        timeout_ms: 100,
        batch_size: 2,
        dimensions: Some(3),
        allow_remote: false,
        api_key_env: None,
    }
}

fn request(inputs: &[&str]) -> EmbeddingRequest {
    EmbeddingRequest {
        inputs: inputs.iter().map(|input| (*input).to_owned()).collect(),
    }
}

fn provider_deadline(budget_ms: u64) -> OperationDeadline {
    OperationDeadline::for_provider(
        Instant::now() + Duration::from_millis(budget_ms),
        SemanticOperation::Embedding,
        budget_ms,
    )
}

fn success(indices: &[usize]) -> String {
    let data = indices
        .iter()
        .map(|index| format!(r#"{{"index":{index},"embedding":[1.0,2.0,3.0]}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"data":[{data}]}}"#)
}

fn assert_no_request(fixture: &HttpFixture) {
    thread::sleep(Duration::from_millis(20));
    assert_eq!(fixture.request_count(), 0);
}

#[test]
fn remote_embedding_endpoints_require_explicit_opt_in() {
    let fixture = HttpFixture::new([]);
    let mut candidate = config("http://192.0.2.1/v1".to_owned());
    candidate.allow_remote = false;

    assert!(matches!(
        OpenAiCompatibleEmbeddingProvider::new(candidate),
        Err(EmbeddingConfigurationError::RemoteOptInRequired)
    ));
    assert_no_request(&fixture);

    assert!(matches!(
        OpenAiCompatibleEmbeddingProvider::new(config("not a URL".to_owned())),
        Err(EmbeddingConfigurationError::InvalidEndpoint)
    ));
    let mut empty_model = config("http://embedded:secret@127.0.0.1:9/v1".to_owned());
    empty_model.model.clear();
    assert!(
        matches!(
            OpenAiCompatibleEmbeddingProvider::new(empty_model),
            Err(EmbeddingConfigurationError::EmptyModel)
        ),
        "model validation precedes URL credential policy"
    );
    let mut invalid_numeric = config("http://embedded:secret@127.0.0.1:9/v1".to_owned());
    invalid_numeric.timeout_ms = 0;
    assert!(
        matches!(
            OpenAiCompatibleEmbeddingProvider::new(invalid_numeric),
            Err(EmbeddingConfigurationError::InvalidNumericValue)
        ),
        "numeric validation precedes URL credential policy"
    );
    for mut invalid_numeric in [
        {
            let mut candidate = config("http://127.0.0.1:9/v1".to_owned());
            candidate.timeout_ms = 3_600_001;
            candidate
        },
        {
            let mut candidate = config("http://127.0.0.1:9/v1".to_owned());
            candidate.batch_size = 257;
            candidate
        },
        {
            let mut candidate = config("http://127.0.0.1:9/v1".to_owned());
            candidate.dimensions = Some(65_537);
            candidate
        },
    ] {
        assert!(matches!(
            OpenAiCompatibleEmbeddingProvider::new(invalid_numeric.clone()),
            Err(EmbeddingConfigurationError::InvalidNumericValue)
        ));
        invalid_numeric.base_url = "http://embedded:secret@127.0.0.1:9/v1".to_owned();
        assert!(
            matches!(
                OpenAiCompatibleEmbeddingProvider::new(invalid_numeric),
                Err(EmbeddingConfigurationError::InvalidNumericValue)
            ),
            "numeric validation precedes URL credential policy"
        );
    }
    for endpoint in [
        "http://127.0.0.1:9/v1?forbidden=1",
        "http://127.0.0.1:9/v1#forbidden",
    ] {
        assert!(matches!(
            OpenAiCompatibleEmbeddingProvider::new(config(endpoint.to_owned())),
            Err(EmbeddingConfigurationError::InvalidEndpoint)
        ));
    }
    let mut missing_key = config("http://127.0.0.1:9/v1".to_owned());
    missing_key.api_key_env = Some("TOWER_F016_MISSING_EMBEDDING_API_KEY".to_owned());
    assert!(matches!(
        OpenAiCompatibleEmbeddingProvider::new(missing_key),
        Err(EmbeddingConfigurationError::MissingApiKey)
    ));
}

#[test]
fn remote_embedding_endpoints_require_https() {
    let fixture = HttpFixture::new([]);
    let mut candidate = config("http://192.0.2.1/v1".to_owned());
    candidate.allow_remote = true;

    assert!(matches!(
        OpenAiCompatibleEmbeddingProvider::new(candidate),
        Err(EmbeddingConfigurationError::RemoteHttpsRequired)
    ));
    assert_no_request(&fixture);
}

#[test]
fn embedding_endpoint_urls_reject_credentials() {
    let fixture = HttpFixture::new([]);
    let candidate = config("http://embedded:secret@127.0.0.1:9/v1".to_owned());

    assert!(matches!(
        OpenAiCompatibleEmbeddingProvider::new(candidate),
        Err(EmbeddingConfigurationError::UrlCredentialsForbidden)
    ));
    assert_no_request(&fixture);
}

#[test]
fn embedding_provider_rejects_redirect_responses() {
    let forbidden = HttpFixture::new([]);
    let redirect = ResponseSpec {
        status: 302,
        headers: vec![(
            "Location".to_owned(),
            format!("{}/embeddings", forbidden.base_url()),
        )],
        body: String::new(),
        delay: Duration::ZERO,
    };
    let fixture = HttpFixture::new([redirect]);
    let provider = OpenAiCompatibleEmbeddingProvider::new(config(fixture.base_url()))
        .expect("loopback fixture configuration must be valid");

    let error = runtime()
        .block_on(provider.embed(&request(&["redirect source"]), provider_deadline(100)))
        .expect_err("redirects must be rejected");

    assert_eq!(error, EmbeddingProviderError::Unavailable);
    assert_eq!(fixture.wait_for_requests(1).len(), 1);
    assert_no_request(&forbidden);
}

#[test]
fn provider_failures_return_stable_errors_and_preserve_liveness() {
    let fixture = HttpFixture::new([
        ResponseSpec::status(503, "unavailable detail"),
        ResponseSpec::json("not-json"),
        ResponseSpec::json(
            r#"{"data":[{"index":0,"embedding":[1.0,2.0,3.0]},{"index":0,"embedding":[1.0,2.0,3.0]}]}"#,
        ),
        ResponseSpec::json(r#"{"data":[{"index":1,"embedding":[1.0,2.0,3.0]}]}"#),
        ResponseSpec::json(
            r#"{"data":[{"index":1,"embedding":[1.0,2.0,3.0]},{"index":0,"embedding":[1.0,2.0,3.0]}]}"#,
        ),
        ResponseSpec::json(r#"{"data":[{"index":2,"embedding":[1.0,2.0,3.0]}]}"#),
        ResponseSpec::json(r#"{"data":[{"index":0,"embedding":[1e39,2.0,3.0]}]}"#),
        ResponseSpec::json(r#"{"data":[{"index":0,"embedding":[0.0,0.0,0.0]}]}"#),
        ResponseSpec::json(r#"{"data":[{"index":0,"embedding":[1.0,2.0]}]}"#),
        ResponseSpec::json(success(&[0])),
    ]);
    let provider = OpenAiCompatibleEmbeddingProvider::new(config(fixture.base_url()))
        .expect("loopback fixture configuration must be valid");
    let runtime = runtime();
    let one = request(&["one"]);
    let two = request(&["one", "two"]);
    let cases = [
        (&one, EmbeddingProviderError::Unavailable),
        (&one, EmbeddingProviderError::MalformedResponse),
        (&two, EmbeddingProviderError::DuplicateIndex { index: 0 }),
        (&two, EmbeddingProviderError::MissingIndex { index: 0 }),
        (
            &two,
            EmbeddingProviderError::UnorderedIndex {
                previous: 1,
                current: 0,
            },
        ),
        (
            &two,
            EmbeddingProviderError::OutOfRangeIndex {
                index: 2,
                batch_len: 2,
            },
        ),
        (&one, EmbeddingProviderError::NonFiniteVector { index: 0 }),
        (&one, EmbeddingProviderError::ZeroNormVector { index: 0 }),
        (
            &one,
            EmbeddingProviderError::DimensionMismatch {
                expected: 3,
                actual: 2,
            },
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            runtime.block_on(provider.embed(input, provider_deadline(100))),
            Err(expected)
        );
    }
    assert!(
        runtime
            .block_on(provider.embed(&one, provider_deadline(100)))
            .is_ok(),
        "a later independent call must remain usable"
    );

    let timeout_fixture = HttpFixture::new([
        ResponseSpec::json(success(&[0])).delayed(Duration::from_millis(80)),
        ResponseSpec::json(success(&[0])),
    ]);
    let timeout_provider =
        OpenAiCompatibleEmbeddingProvider::new(config(timeout_fixture.base_url()))
            .expect("loopback fixture configuration must be valid");
    assert_eq!(
        runtime.block_on(timeout_provider.embed(&one, provider_deadline(30))),
        Err(EmbeddingProviderError::TimedOut {
            operation: SemanticOperation::Embedding,
            layer: DeadlineLayer::Provider,
            budget_ms: 30,
        })
    );
    assert!(
        runtime
            .block_on(timeout_provider.embed(&one, provider_deadline(100)))
            .is_ok(),
        "a timed-out call must not poison the provider"
    );
    assert_eq!(
        timeout_fixture.wait_for_requests(2).len(),
        2,
        "the later independent call must be accepted while the timed-out handler is delayed"
    );

    let batched_fixture = HttpFixture::new([
        ResponseSpec::json(success(&[0])).delayed(Duration::from_millis(20)),
        ResponseSpec::json(success(&[0])).delayed(Duration::from_millis(20)),
        ResponseSpec::json(success(&[0])),
    ]);
    let mut batched_config = config(batched_fixture.base_url());
    batched_config.batch_size = 1;
    let batched_provider = OpenAiCompatibleEmbeddingProvider::new(batched_config)
        .expect("loopback fixture configuration must be valid");
    assert_eq!(
        runtime.block_on(batched_provider.embed(&two, provider_deadline(30))),
        Err(EmbeddingProviderError::TimedOut {
            operation: SemanticOperation::Embedding,
            layer: DeadlineLayer::Provider,
            budget_ms: 30,
        }),
        "all batches in one call must share one absolute provider deadline"
    );
    assert!(
        runtime
            .block_on(batched_provider.embed(&one, provider_deadline(100)))
            .is_ok(),
        "a later call receives a fresh provider deadline"
    );

    let outer_fixture =
        HttpFixture::new([ResponseSpec::json(success(&[0])).delayed(Duration::from_millis(30))]);
    let outer_provider = OpenAiCompatibleEmbeddingProvider::new(config(outer_fixture.base_url()))
        .expect("loopback fixture configuration must be valid");
    let outer_deadline = OperationDeadline::for_operation(
        Some(Instant::now() + Duration::from_millis(10)),
        SemanticOperation::Search,
        10,
    );
    assert_eq!(
        runtime.block_on(outer_provider.embed(&one, outer_deadline)),
        Err(EmbeddingProviderError::TimedOut {
            operation: SemanticOperation::Search,
            layer: DeadlineLayer::Semantic,
            budget_ms: 10,
        }),
        "an earlier enclosing operation deadline retains ownership"
    );

    let provider_limit_fixture =
        HttpFixture::new([ResponseSpec::json(success(&[0])).delayed(Duration::from_millis(30))]);
    let mut provider_limit_config = config(provider_limit_fixture.base_url());
    provider_limit_config.timeout_ms = 10;
    let provider_limit = OpenAiCompatibleEmbeddingProvider::new(provider_limit_config)
        .expect("loopback fixture configuration must be valid");
    let later_outer_deadline = OperationDeadline::for_operation(
        Some(Instant::now() + Duration::from_millis(100)),
        SemanticOperation::Search,
        100,
    );
    assert_eq!(
        runtime.block_on(provider_limit.embed(&one, later_outer_deadline)),
        Err(EmbeddingProviderError::TimedOut {
            operation: SemanticOperation::Embedding,
            layer: DeadlineLayer::Provider,
            budget_ms: 10,
        }),
        "an earlier adapter limit retains provider timeout ownership"
    );
}

#[test]
fn provider_errors_and_logs_do_not_expose_confidential_inputs() {
    const SOURCE_SENTINEL: &str = "SOURCE_SECRET_7a19";
    const RESPONSE_SENTINEL: &str = "RESPONSE_SECRET_82bc";
    const PRIVATE_PATH_SENTINEL: &str = "/private/workspace/secret.rs";
    const API_KEY_SENTINEL: &str = "API_KEY_SECRET_f91e";
    const API_KEY_ENV: &str = "TOWER_F016_EMBEDDING_API_KEY";

    let fixture = HttpFixture::new([ResponseSpec::status(
        503,
        format!("{RESPONSE_SENTINEL} {PRIVATE_PATH_SENTINEL}"),
    )]);
    let mut candidate = config(fixture.base_url());
    candidate.api_key_env = Some(API_KEY_ENV.to_owned());
    // SAFETY: this test uses a task-specific variable and restores it before returning.
    unsafe { std::env::set_var(API_KEY_ENV, API_KEY_SENTINEL) };
    let captured_logs = Arc::new(Mutex::new(Vec::new()));
    let log_capture = Arc::clone(&captured_logs);
    let provider = OpenAiCompatibleEmbeddingProvider::new(candidate)
        .expect("configured API key must be available")
        .with_log_host_call_sink(move |call| {
            log_capture
                .lock()
                .expect("log capture lock poisoned")
                .push(call);
        });
    let error = runtime()
        .block_on(provider.embed(&request(&[SOURCE_SENTINEL]), provider_deadline(100)))
        .expect_err("fixture deliberately returns an unavailable response");
    // SAFETY: this removes only the task-specific variable installed above.
    unsafe { std::env::remove_var(API_KEY_ENV) };

    assert_eq!(error, EmbeddingProviderError::Unavailable);
    let public = format!("{error:?}");
    for sentinel in [
        SOURCE_SENTINEL,
        RESPONSE_SENTINEL,
        PRIVATE_PATH_SENTINEL,
        API_KEY_SENTINEL,
    ] {
        assert!(!public.contains(sentinel));
    }
    let logs = captured_logs.lock().expect("log capture lock poisoned");
    assert_eq!(
        logs.as_slice(),
        [HostCall::Log {
            level: "warn".to_owned(),
            msg: "Embedding provider request failed".to_owned(),
        }]
    );
    let rendered_logs = format!("{logs:?}");
    for sentinel in [
        SOURCE_SENTINEL,
        RESPONSE_SENTINEL,
        PRIVATE_PATH_SENTINEL,
        API_KEY_SENTINEL,
        "URL_USERINFO_SECRET_11",
    ] {
        assert!(
            !rendered_logs.contains(sentinel),
            "log HostCall leaked {sentinel}"
        );
    }
    drop(logs);

    let url_userinfo = config("http://URL_USERINFO_SECRET_11@127.0.0.1:9/v1".to_owned());
    let credential_error = match OpenAiCompatibleEmbeddingProvider::new(url_userinfo) {
        Err(error) => error,
        Ok(_) => panic!("URL credentials must be rejected before a request"),
    };
    assert!(!format!("{credential_error:?}").contains("URL_USERINFO_SECRET_11"));

    let requests = fixture.wait_for_requests(1);
    let observed = &requests[0];
    assert_eq!(observed.request_line, "POST /v1/embeddings HTTP/1.1");
    assert_eq!(
        observed.body,
        format!(
            r#"{{"model":"fixture-model","input":["{SOURCE_SENTINEL}"],"encoding_format":"float","dimensions":3}}"#
        )
    );
    let authorization: Vec<_> = observed
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .collect();
    assert_eq!(authorization.len(), 1);
    assert_eq!(authorization[0].1, format!("Bearer {API_KEY_SENTINEL}"));

    let no_auth_fixture = HttpFixture::new([ResponseSpec::json(success(&[0, 1]))]);
    let mut without_dimensions = config(no_auth_fixture.base_url());
    without_dimensions.dimensions = None;
    let unauthenticated = OpenAiCompatibleEmbeddingProvider::new(without_dimensions)
        .expect("loopback fixture configuration must be valid");
    runtime()
        .block_on(unauthenticated.embed(
            &request(&["ordered first", "ordered second"]),
            provider_deadline(100),
        ))
        .expect("fixture response must be accepted");
    let unauthenticated_request = &no_auth_fixture.wait_for_requests(1)[0];
    assert_eq!(
        unauthenticated_request.body,
        r#"{"model":"fixture-model","input":["ordered first","ordered second"],"encoding_format":"float"}"#
    );
    assert!(
        unauthenticated_request
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
    );
}
